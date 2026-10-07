//! Snippets: reusable command fragments with `{{name}}` variables and
//! "Run on…" several hosts at once (local connections, no PTY), showing the
//! output of each one.

use std::collections::BTreeMap;

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{SecretUpdate, Snippet};
use termoak_ssh::{ExecOptions, ExecOutput};

use super::OpenRequest;
use crate::runtime;
use crate::state::{AppModel, Item};
use crate::ui::{self, IconName};

enum RunState {
    Running,
    Done(ExecOutput),
    Failed(String),
}

struct RunResult {
    host: String,
    state: RunState,
}

pub struct SnippetsView {
    model: Entity<AppModel>,
    run_title: Option<String>,
    runs: Vec<RunResult>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for SnippetsView {}

impl SnippetsView {
    pub fn new(model: Entity<AppModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&model, |_, _, cx| cx.notify())];
        Self {
            model,
            run_title: None,
            runs: Vec::new(),
            _subs: subs,
        }
    }

    pub(crate) fn edit(
        &mut self,
        rec: Option<Item<Snippet>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        open_snippet_form(self.model.clone(), rec.map(|r| r.rec.data), window, cx);
    }
    fn delete(&mut self, rec: Item<Snippet>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("snippets.delete.title"),
            t!("snippets.delete.message", name = rec.data.name),
            t!("snippets.delete.ok"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<Snippet>(rec.data.id, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
            },
        );
    }

    /// "Run on…" dialog: target hosts and values of the variables.
    fn run_dialog(&mut self, snippet: Snippet, window: &mut Window, cx: &mut Context<Self>) {
        let hosts: Vec<(Id, String)> = self
            .model
            .read(cx)
            .hosts
            .iter()
            .map(|h| (h.data.id, h.data.label.clone()))
            .collect();
        if hosts.is_empty() {
            ui::error(window, cx, t!("snippets.error.no_hosts"));
            return;
        }
        let weak = cx.entity().downgrade();
        let dialog = cx.new(|cx| RunDialog::new(snippet, hosts, weak, window, cx));
        window.open_dialog(cx, move |d, _, _| {
            let dialog_ok = dialog.clone();
            d.title(t!("snippets.run.title"))
                .w(px(560.))
                .child(dialog.clone())
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("run-cancel")
                                .label(t!("common.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("run-go")
                                .primary()
                                .icon(ui::icon(IconName::Play))
                                .label(t!("snippets.run.ok"))
                                .on_click(move |_, window, cx| {
                                    let ok = dialog_ok.update(cx, |d, cx| d.submit(window, cx));
                                    if ok {
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                )
        });
    }

    /// Runs the script on the chosen hosts.
    fn run(
        &mut self,
        name: String,
        script: String,
        targets: Vec<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = self.model.read(cx);
        let ws = m.ws.clone();
        let prompter = m.prompter.clone();
        let use_agent = m.settings.use_agent;
        let labels: Vec<String> = targets.iter().map(|id| m.host_label(*id)).collect();
        self.run_title = Some(name);
        self.runs = labels
            .into_iter()
            .map(|host| RunResult {
                host,
                state: RunState::Running,
            })
            .collect();
        cx.notify();
        for (ix, host_id) in targets.into_iter().enumerate() {
            let ws = ws.clone();
            let prompter = prompter.clone();
            let script = script.clone();
            runtime::run_in(
                cx,
                window,
                async move {
                    let conn = ws
                        .connect(host_id, prompter, use_agent)
                        .await
                        .map_err(crate::state::api_error)?;
                    let out = conn
                        .exec(&script, &ExecOptions::default())
                        .await
                        .map_err(|e| {
                            crate::state::api_error(termoak_client::ClientError::from(e))
                        })?;
                    conn.disconnect().await;
                    Ok::<_, String>(out)
                },
                move |this, res, _, cx| {
                    if let Some(r) = this.runs.get_mut(ix) {
                        r.state = match res {
                            Ok(out) => RunState::Done(out),
                            Err(e) => RunState::Failed(e),
                        };
                    }
                    cx.notify();
                },
            );
        }
    }

    fn render_results(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let Some(title) = &self.run_title else {
            return div().into_any_element();
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::Play)
                            .size(px(16.))
                            .text_color(theme.primary),
                    )
                    .child(
                        div()
                            .font_semibold()
                            .child(t!("snippets.results.title", name = title)),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("clear-results")
                            .small()
                            .ghost()
                            .label(t!("snippets.results.clear"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.run_title = None;
                                this.runs.clear();
                                cx.notify();
                            })),
                    ),
            )
            .children(self.runs.iter().enumerate().map(|(i, r)| {
                let (status, color, text): (SharedString, gpui::Hsla, String) = match &r.state {
                    RunState::Running => {
                        (t!("snippets.results.running"), theme.warning, String::new())
                    }
                    RunState::Failed(e) => (t!("snippets.results.error"), theme.danger, e.clone()),
                    RunState::Done(out) => {
                        let mut text = out.stdout_text();
                        let err = out.stderr_text();
                        if !err.trim().is_empty() {
                            if !text.is_empty() && !text.ends_with('\n') {
                                text.push('\n');
                            }
                            text.push_str(&err);
                        }
                        if out.truncated {
                            text.push('\n');
                            text.push_str(&t!("snippets.results.truncated"));
                        }
                        let status = if out.timed_out {
                            t!("snippets.results.timed_out")
                        } else {
                            match out.exit_code {
                                Some(0) => t!("snippets.results.ok", ms = out.duration_ms),
                                Some(c) => {
                                    t!("snippets.results.exit_code", code = c, ms = out.duration_ms)
                                }
                                None => out
                                    .exit_signal
                                    .clone()
                                    .map(|s| t!("snippets.results.signal", signal = s))
                                    .unwrap_or_else(|| t!("snippets.results.finished")),
                            }
                        };
                        let color = if out.success() {
                            theme.success
                        } else {
                            theme.danger
                        };
                        (status, color, text)
                    }
                };
                let running = matches!(r.state, RunState::Running);
                let copy_text = text.clone();
                ui::card(cx)
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(ui::icon(IconName::Server).size(px(14.)))
                            .child(div().font_medium().text_sm().child(r.host.clone()))
                            .child(ui::pill(status, color))
                            .when(running, |this| this.child(Spinner::new().small()))
                            .child(div().flex_1())
                            .when(!text.is_empty(), |this| {
                                this.child(
                                    Button::new(("copy-out", i))
                                        .xsmall()
                                        .ghost()
                                        .icon(ui::icon(IconName::Copy))
                                        .on_click(move |_, window, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy_text.clone(),
                                            ));
                                            ui::success(
                                                window,
                                                cx,
                                                t!("snippets.results.output_copied"),
                                            );
                                        }),
                                )
                            }),
                    )
                    .when(!text.is_empty(), |this| {
                        this.child(
                            div()
                                .id(("out", i))
                                .max_h(px(260.))
                                .overflow_y_scroll()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.background)
                                .font_family(ui::mono_family(cx))
                                .text_xs()
                                .child(text),
                        )
                    })
            }))
            .into_any_element()
    }
}

impl Render for SnippetsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Only what the vault picker lets through; Use-only snippets can be
        // used (and copied) but not changed.
        let snippets: Vec<(Item<Snippet>, bool)> = {
            let m = self.model.read(cx);
            m.snippets
                .iter()
                .filter(|s| m.in_filter(s))
                .map(|s| (s.clone(), m.caps_of(s).edit))
                .collect()
        };
        let results = self.render_results(cx);
        let theme = cx.theme();
        let list: gpui::AnyElement = if snippets.is_empty() {
            ui::empty_state(
                IconName::SquareTerminal,
                t!("snippets.empty.title"),
                t!("snippets.empty.detail"),
                cx,
            )
            .into_any_element()
        } else {
            h_flex()
                .flex_wrap()
                .gap_3()
                .children(
                    snippets
                        .into_iter()
                        .enumerate()
                        .map(|(i, (rec, editable))| {
                            let s = rec.data.clone();
                            let (s_run, r_edit, r_del) = (s.clone(), rec.clone(), rec.clone());
                            let script_copy = s.script.clone();
                            let preview: String =
                                s.script.lines().take(3).collect::<Vec<_>>().join("\n");
                            v_flex()
                                .id(("snippet", i))
                                .w(px(340.))
                                .p_3()
                                .gap_2()
                                .rounded(theme.radius_lg)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.secondary)
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            ui::icon(IconName::SquareTerminal)
                                                .size(px(16.))
                                                .text_color(theme.primary),
                                        )
                                        .child(
                                            div()
                                                .flex_1()
                                                .font_semibold()
                                                .text_sm()
                                                .child(s.name.clone()),
                                        )
                                        .when(!editable, |this| {
                                            this.child(
                                                ui::icon(IconName::Lock)
                                                    .size(px(12.))
                                                    .text_color(theme.warning),
                                            )
                                        })
                                        .when(editable, |this| {
                                            this.child(
                                                Button::new(("edit-snippet", i))
                                                    .xsmall()
                                                    .ghost()
                                                    .icon(ui::icon(IconName::Pencil))
                                                    .on_click(cx.listener(
                                                        move |this, _: &ClickEvent, window, cx| {
                                                            this.edit(
                                                                Some(r_edit.clone()),
                                                                window,
                                                                cx,
                                                            )
                                                        },
                                                    )),
                                            )
                                            .child(
                                                Button::new(("delete-snippet", i))
                                                    .xsmall()
                                                    .ghost()
                                                    .icon(ui::icon(IconName::Trash))
                                                    .on_click(cx.listener(
                                                        move |this, _: &ClickEvent, window, cx| {
                                                            this.delete(r_del.clone(), window, cx)
                                                        },
                                                    )),
                                            )
                                        }),
                                )
                                .when(!s.description.is_empty(), |this| {
                                    this.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(s.description.clone()),
                                    )
                                })
                                .child(
                                    div()
                                        .p_2()
                                        .rounded(theme.radius)
                                        .bg(theme.background)
                                        .font_family(ui::mono_family(cx))
                                        .text_xs()
                                        .max_h(px(64.))
                                        .overflow_hidden()
                                        .child(preview),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .flex_wrap()
                                        .children(
                                            s.tags.iter().map(|t| {
                                                ui::pill(t.clone(), theme.muted_foreground)
                                            }),
                                        )
                                        .child(div().flex_1())
                                        .child(
                                            Button::new(("copy-snippet", i))
                                                .xsmall()
                                                .ghost()
                                                .icon(ui::icon(IconName::Copy))
                                                .tooltip(t!("common.copy"))
                                                .on_click(move |_, window, cx| {
                                                    cx.write_to_clipboard(
                                                        ClipboardItem::new_string(
                                                            script_copy.clone(),
                                                        ),
                                                    );
                                                    ui::success(window, cx, t!("snippets.copied"));
                                                }),
                                        )
                                        .child(
                                            Button::new(("run-snippet", i))
                                                .xsmall()
                                                .primary()
                                                .icon(ui::icon(IconName::Play))
                                                .label(t!("snippets.run.title"))
                                                .on_click(cx.listener(
                                                    move |this, _: &ClickEvent, window, cx| {
                                                        this.run_dialog(s_run.clone(), window, cx)
                                                    },
                                                )),
                                        ),
                                )
                        }),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("snippets.title"),
                t!("snippets.subtitle"),
                Button::new("new-snippet")
                    .primary()
                    .icon(ui::icon(IconName::Plus))
                    .label(t!("snippets.form.new_title"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.edit(None, window, cx)),
                    ),
                cx,
            ))
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(v_flex().p_6().gap_6().child(list).child(results)),
                ),
            )
    }
}

/// Content of the "Run on…" dialog.
struct RunDialog {
    snippet: Snippet,
    hosts: Vec<(Id, String)>,
    selected: Vec<Id>,
    vars: Vec<(String, Entity<InputState>)>,
    view: gpui::WeakEntity<SnippetsView>,
}

impl RunDialog {
    fn new(
        snippet: Snippet,
        hosts: Vec<(Id, String)>,
        view: gpui::WeakEntity<SnippetsView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let vars = snippet
            .variables()
            .into_iter()
            .map(|name| {
                let state = cx.new(|cx| InputState::new(window, cx).placeholder(name.clone()));
                (name, state)
            })
            .collect();
        Self {
            snippet,
            hosts,
            selected: Vec::new(),
            vars,
            view,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.selected.is_empty() {
            ui::error(window, cx, t!("snippets.run.choose_host"));
            return false;
        }
        let values: BTreeMap<String, String> = self
            .vars
            .iter()
            .map(|(n, s)| (n.clone(), s.read(cx).value().to_string()))
            .collect();
        if let Some((missing, _)) = values.iter().find(|(_, v)| v.trim().is_empty()) {
            ui::error(window, cx, t!("snippets.run.missing_value", name = missing));
            return false;
        }
        let script = match self.snippet.render(&values) {
            Ok(s) => s,
            Err(e) => {
                ui::error(window, cx, e.to_string());
                return false;
            }
        };
        let targets = self.selected.clone();
        let name = self.snippet.name.clone();
        if let Some(view) = self.view.upgrade() {
            view.update(cx, |v, cx| v.run(name, script, targets, window, cx));
        }
        true
    }
}

impl Render for RunDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let all = self.selected.len() == self.hosts.len();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("snippets.run.explanation", name = self.snippet.name)),
            )
            .children(
                self.vars.iter().map(|(name, state)| {
                    ui::field(format!("{{{{{name}}}}}"), Input::new(state), cx)
                }),
            )
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(t!("snippets.run.hosts")),
                    )
                    .child(
                        Button::new("select-all-hosts")
                            .xsmall()
                            .ghost()
                            .label(if all {
                                t!("common.none")
                            } else {
                                t!("snippets.run.all")
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if all {
                                    this.selected.clear();
                                } else {
                                    this.selected = this.hosts.iter().map(|(id, _)| *id).collect();
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("run-hosts")
                    .max_h(px(260.))
                    .gap_1()
                    .overflow_y_scrollbar()
                    .children(self.hosts.iter().enumerate().map(|(i, (id, label))| {
                        let id = *id;
                        Checkbox::new(("run-host", i))
                            .label(label.clone())
                            .checked(self.selected.contains(&id))
                            .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                if *v {
                                    if !this.selected.contains(&id) {
                                        this.selected.push(id);
                                    }
                                } else {
                                    this.selected.retain(|s| *s != id);
                                }
                                cx.notify();
                            }))
                    })),
            )
    }
}

/// Text a snippet types into a terminal: with "Run" it ends with Enter (one
/// line break), with "Paste" it never does.
pub fn snippet_input(script: &str, run: bool) -> String {
    let body = script.trim_end_matches(['\r', '\n']);
    if run {
        format!("{body}\n")
    } else {
        body.to_string()
    }
}

/// Terminal → Send snippet: picks a snippet, fills its variables and types
/// it into the focused terminal or, in a split view, into every pane ("Run
/// in all panes", checked while broadcasting). `on_send` gets the text and
/// whether it goes to all the panes. `preselect` chooses a snippet first
/// (the command palette, for one with variables to fill in).
pub fn open_send_dialog(
    model: Entity<AppModel>,
    split: bool,
    broadcast: bool,
    preselect: Option<Id>,
    window: &mut Window,
    cx: &mut gpui::App,
    on_send: impl Fn(String, bool, &mut Window, &mut gpui::App) + 'static,
) {
    if model.read(cx).snippets.is_empty() {
        ui::notify(
            window,
            cx,
            crate::state::ToastKind::Info,
            t!("snippets.send.none"),
        );
        return;
    }
    let dialog = cx.new(|cx| SendDialog::new(model, split, broadcast, preselect, window, cx));
    let on_send = std::rc::Rc::new(on_send);
    window.open_dialog(cx, move |d, _, _| {
        let (paste, run, enter) = (dialog.clone(), dialog.clone(), dialog.clone());
        let (send_paste, send_run, send_enter) =
            (on_send.clone(), on_send.clone(), on_send.clone());
        d.title(t!("snippets.send.title"))
            .w(px(560.))
            .on_ok(move |_, window, cx| {
                match enter.update(cx, |d, cx| d.finish(true, window, cx)) {
                    Some((text, all)) => {
                        send_enter(text, all, window, cx);
                        true
                    }
                    None => false,
                }
            })
            .child(dialog.clone())
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("send-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("send-paste")
                            .icon(ui::icon(IconName::ClipboardPaste))
                            .label(t!("snippets.send.paste"))
                            .tooltip(t!("snippets.send.paste_tooltip"))
                            .on_click(move |_, window, cx| {
                                if let Some((text, all)) =
                                    paste.update(cx, |d, cx| d.finish(false, window, cx))
                                {
                                    window.close_dialog(cx);
                                    send_paste(text, all, window, cx);
                                }
                            }),
                    )
                    .child(
                        Button::new("send-run")
                            .primary()
                            .icon(ui::icon(IconName::Play))
                            .label(t!("snippets.send.run"))
                            .on_click(move |_, window, cx| {
                                if let Some((text, all)) =
                                    run.update(cx, |d, cx| d.finish(true, window, cx))
                                {
                                    window.close_dialog(cx);
                                    send_run(text, all, window, cx);
                                }
                            }),
                    ),
            )
    });
}

/// Content of the "Send snippet" dialog.
struct SendDialog {
    model: Entity<AppModel>,
    search: Entity<InputState>,
    selected: Option<Id>,
    vars: Vec<(String, Entity<InputState>)>,
    split: bool,
    all_panes: bool,
    _sub: Subscription,
}

impl SendDialog {
    fn new(
        model: Entity<AppModel>,
        split: bool,
        broadcast: bool,
        preselect: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("snippets.send.search_placeholder"))
        });
        ui::focus_later(&search, window, cx);
        let sub = cx.subscribe_in(
            &search,
            window,
            |this, _, ev: &gpui_component::input::InputEvent, window, cx| {
                if matches!(ev, gpui_component::input::InputEvent::Change) {
                    // While typing, the first match is chosen if the chosen
                    // one is filtered out.
                    let matches = this.matches(cx);
                    if !matches.iter().any(|s| Some(s.data.id) == this.selected) {
                        let first = matches.first().map(|s| s.data.id);
                        this.select(first, window, cx);
                    }
                    cx.notify();
                }
            },
        );
        let selected = preselect
            .filter(|id| model.read(cx).snippets.iter().any(|s| s.data.id == *id))
            .or_else(|| model.read(cx).snippets.first().map(|s| s.data.id));
        let mut dialog = Self {
            model,
            search,
            selected: None,
            vars: Vec::new(),
            split,
            all_panes: split && broadcast,
            _sub: sub,
        };
        dialog.select(selected, window, cx);
        dialog
    }

    fn matches(&self, cx: &gpui::App) -> Vec<Item<Snippet>> {
        let q = self.search.read(cx).value().trim().to_lowercase();
        let m = self.model.read(cx);
        m.snippets
            .iter()
            .filter(|s| m.in_filter(s))
            .filter(|s| {
                q.is_empty()
                    || s.data.name.to_lowercase().contains(&q)
                    || s.data.script.to_lowercase().contains(&q)
                    || s.data.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .cloned()
            .collect()
    }

    fn snippet(&self, cx: &gpui::App) -> Option<Snippet> {
        let id = self.selected?;
        self.model
            .read(cx)
            .snippets
            .iter()
            .find(|s| s.data.id == id)
            .map(|s| s.data.clone())
    }

    fn select(&mut self, id: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id;
        self.vars = self
            .snippet(cx)
            .map(|s| s.variables())
            .unwrap_or_default()
            .into_iter()
            .map(|name| {
                let state = cx.new(|cx| InputState::new(window, cx).placeholder(name.clone()));
                (name, state)
            })
            .collect();
        cx.notify();
    }

    /// The text to type and whether it goes to every pane, or `None` (with
    /// an error shown) if something is missing.
    fn finish(
        &mut self,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<(String, bool)> {
        let Some(snippet) = self.snippet(cx) else {
            ui::error(window, cx, t!("snippets.send.choose"));
            return None;
        };
        let values: BTreeMap<String, String> = self
            .vars
            .iter()
            .map(|(n, s)| (n.clone(), s.read(cx).value().to_string()))
            .collect();
        if let Some((missing, _)) = values.iter().find(|(_, v)| v.trim().is_empty()) {
            ui::error(window, cx, t!("snippets.run.missing_value", name = missing));
            return None;
        }
        match snippet.render(&values) {
            Ok(script) => Some((snippet_input(&script, run), self.split && self.all_panes)),
            Err(e) => {
                ui::error(window, cx, e.to_string());
                None
            }
        }
    }
}

impl Render for SendDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = self.matches(cx);
        let selected = self.selected;
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(Input::new(&self.search).prefix(ui::icon(IconName::Search).size(px(14.))))
            .child(
                v_flex()
                    .id("send-snippets")
                    .max_h(px(220.))
                    .gap_1()
                    .overflow_y_scrollbar()
                    .when(items.is_empty(), |this| {
                        this.child(
                            div()
                                .p_3()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("snippets.send.no_matches")),
                        )
                    })
                    .children(items.into_iter().enumerate().map(|(i, rec)| {
                        let id = rec.data.id;
                        let active = selected == Some(id);
                        let first_line = rec.data.script.lines().next().unwrap_or("").to_string();
                        h_flex()
                            .id(("send-snippet", i))
                            .px_3()
                            .py_2()
                            .gap_3()
                            .items_center()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .when(active, |this| this.bg(theme.list_active))
                            .when(!active, |this| this.hover(|s| s.bg(theme.secondary_hover)))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.select(Some(id), window, cx)
                            }))
                            .child(
                                ui::icon(IconName::SquareTerminal)
                                    .size(px(16.))
                                    .text_color(theme.primary),
                            )
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div().text_sm().font_medium().child(rec.data.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_family(ui::mono_family(cx))
                                            .text_color(theme.muted_foreground)
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_ellipsis()
                                            .child(first_line),
                                    ),
                            )
                    })),
            )
            .children(
                self.vars.iter().map(|(name, state)| {
                    ui::field(format!("{{{{{name}}}}}"), Input::new(state), cx)
                }),
            )
            .when(self.split, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("send-all-panes")
                                .label(t!("snippets.send.all_panes"))
                                .checked(self.all_panes)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    this.all_panes = *v;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("snippets.send.all_panes_hint")),
                        ),
                )
            })
    }
}

/// The snippet form: a new snippet (`data` with a nil id, maybe filled in,
/// as "Save as runbook" of an AI task does) or an existing one.
pub(crate) fn open_snippet_form(
    model: Entity<AppModel>,
    data: Option<Snippet>,
    window: &mut Window,
    cx: &mut gpui::App,
) {
    let editing = data.as_ref().is_some_and(|d| !d.id.is_nil());
    let name = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t!("snippets.form.name_placeholder"))
            .default_value(data.as_ref().map(|d| d.name.clone()).unwrap_or_default())
    });
    let description = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t!("snippets.form.optional"))
            .default_value(
                data.as_ref()
                    .map(|d| d.description.clone())
                    .unwrap_or_default(),
            )
    });
    let script = cx.new(|cx| {
        TextareaState::new(window, cx)
            .auto_grow(8, 14)
            .placeholder(t!("snippets.form.script_placeholder"))
            .default_value(data.as_ref().map(|d| d.script.clone()).unwrap_or_default())
    });
    let tags = cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(t!("snippets.form.tags_placeholder"))
            .default_value(data.as_ref().map(|d| d.tags.join(", ")).unwrap_or_default())
    });
    ui::focus_later(&name, window, cx);
    let model = model.clone();
    let (n2, d2, s2, t2) = (
        name.clone(),
        description.clone(),
        script.clone(),
        tags.clone(),
    );
    ui::open_form_dialog(
        window,
        cx,
        if editing {
            t!("snippets.form.edit_title")
        } else {
            t!("snippets.form.new_title")
        },
        t!("common.save"),
        560.,
        move |_, cx| {
            v_flex()
                .gap_3()
                .child(ui::field(t!("common.name"), Input::new(&name), cx))
                .child(ui::field(
                    t!("snippets.form.description"),
                    Input::new(&description),
                    cx,
                ))
                .child(ui::field_with_hint(
                    t!("snippets.form.script"),
                    Textarea::new(&script),
                    t!("snippets.form.script_hint"),
                    cx,
                ))
                .child(ui::field(t!("snippets.form.tags"), Input::new(&tags), cx))
                .into_any_element()
        },
        move |window, cx| {
            let snippet = Snippet {
                id: data.as_ref().map(|d| d.id).unwrap_or(Id::nil()),
                name: n2.read(cx).value().trim().to_string(),
                script: s2.read(cx).value().to_string(),
                description: d2.read(cx).value().trim().to_string(),
                tags: t2
                    .read(cx)
                    .value()
                    .split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect(),
            };
            if snippet.name.is_empty() || snippet.script.trim().is_empty() {
                ui::error(window, cx, t!("snippets.error.name_and_script"));
                return false;
            }
            let task = model.update(cx, |m, cx| m.save(snippet, SecretUpdate::Keep, None, cx));
            window
                .spawn(cx, async move |cx| {
                    if let Err(e) = task.await {
                        let _ = cx.update(|window, cx| ui::error(window, cx, e));
                    }
                })
                .detach();
            true
        },
    );
}

#[cfg(test)]
mod tests {
    use super::snippet_input;

    #[test]
    fn run_ends_with_one_enter_and_paste_with_none() {
        assert_eq!(snippet_input("uptime", true), "uptime\n");
        assert_eq!(snippet_input("uptime\n\n", true), "uptime\n");
        assert_eq!(snippet_input("a\nb\n", false), "a\nb");
        assert_eq!(snippet_input("a\r\n", false), "a");
    }
}
