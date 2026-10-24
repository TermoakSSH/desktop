//! Snippets: reusable command fragments with variables (built-ins such as
//! `{{host}}`, prompts with defaults, selects and secrets; see
//! `snippet_vars.rs`) and "Run on…" several hosts at once, from this
//! computer (local connections, no PTY) or from the server (`/exec`, for
//! hosts of one account), showing the output of each one.

use std::collections::{BTreeMap, HashMap};

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, EntityId, EventEmitter,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use termoak_client::Scope;
use termoak_core::Id;
use termoak_core::model::{SecretUpdate, Snippet};
use termoak_core::template::TemplateContext;
use termoak_ssh::{ExecOptions, ExecOutput};

use super::OpenRequest;
use super::snippet_vars::{self, SnippetVars};
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

    /// "Run on…" dialog: target hosts, where it runs and the values of the
    /// variables (prefilled with the ones remembered for this snippet).
    fn run_dialog(&mut self, item: Item<Snippet>, window: &mut Window, cx: &mut Context<Self>) {
        let hosts: Vec<(Id, String, Option<Id>)> = {
            let m = self.model.read(cx);
            m.hosts
                .iter()
                .map(|h| (h.data.id, h.data.label.clone(), h.account()))
                .collect()
        };
        if hosts.is_empty() {
            ui::error(window, cx, t!("snippets.error.no_hosts"));
            return;
        }
        let ws = self.model.read(cx).ws.clone();
        let id = item.data.id;
        let remembered = runtime::spawn(cx, async move { ws.snippet_values(id, None).await });
        let weak = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let remembered = remembered.await.unwrap_or_default();
            let _ = cx.update(|window, cx| {
                let dialog =
                    cx.new(|cx| RunDialog::new(item, hosts, &remembered, weak, window, cx));
                if let Some(first) = dialog.read(cx).vars.first_input() {
                    ui::focus_later(&first, window, cx);
                }
                window.open_dialog(cx, move |d, _, _| {
                    let dialog_ok = dialog.clone();
                    d.title(t!("snippets.run.title"))
                        .w(px(580.))
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
                                            let ok =
                                                dialog_ok.update(cx, |d, cx| d.submit(window, cx));
                                            if ok {
                                                window.close_dialog(cx);
                                            }
                                        }),
                                ),
                        )
                });
            });
        })
        .detach();
    }

    fn start_results(&mut self, name: String, targets: &[Id], cx: &mut Context<Self>) {
        let m = self.model.read(cx);
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
    }

    /// Runs the snippet on the chosen hosts from this computer: each host
    /// gets its own built-ins.
    fn run(
        &mut self,
        name: String,
        script: String,
        values: BTreeMap<String, String>,
        targets: Vec<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let m = self.model.read(cx);
        let ws = m.ws.clone();
        let prompter = m.prompter.clone();
        let use_agent = m.settings.use_agent;
        self.start_results(name, &targets, cx);
        for (ix, host_id) in targets.into_iter().enumerate() {
            let ws = ws.clone();
            let prompter = prompter.clone();
            let script = script.clone();
            let values = values.clone();
            runtime::run_in(
                cx,
                window,
                async move {
                    let ctx = ws
                        .snippet_context(host_id)
                        .await
                        .map_err(crate::state::api_error)?;
                    let command = snippet_vars::render_targets(&script, &values, &[ctx])?.remove(0);
                    let conn = ws
                        .connect(host_id, prompter, use_agent)
                        .await
                        .map_err(crate::state::api_error)?;
                    let out = conn
                        .exec(&command, &ExecOptions::default())
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

    /// Runs the snippet from the server (`POST /exec` with `snippet_id`):
    /// the server renders it per host and connects (a security key on
    /// those hosts is asked on your devices).
    fn run_on_server(
        &mut self,
        snippet: Snippet,
        values: BTreeMap<String, String>,
        targets: Vec<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = targets
            .first()
            .and_then(|h| self.model.read(cx).api_for_host(*h))
        else {
            ui::error(window, cx, t!("error.not_logged_in"));
            return;
        };
        self.start_results(snippet.name.clone(), &targets, cx);
        let body = serde_json::json!({
            "host_ids": targets,
            "snippet_id": snippet.id,
            "variables": values,
        });
        runtime::run_in(
            cx,
            window,
            async move {
                api.post::<Vec<ServerExec>>("/api/v1/exec", &body)
                    .await
                    .map_err(crate::state::api_error)
            },
            move |this, res, _, cx| {
                match res {
                    Ok(list) => {
                        for (ix, host) in targets.iter().enumerate() {
                            let state = match list.iter().find(|r| r.host_id == *host) {
                                Some(r) => r.state(),
                                None => RunState::Failed(t!("snippets.results.no_result").into()),
                            };
                            if let Some(slot) = this.runs.get_mut(ix) {
                                slot.state = state;
                            }
                        }
                    }
                    Err(e) => {
                        for r in &mut this.runs {
                            r.state = RunState::Failed(e.clone());
                        }
                    }
                }
                cx.notify();
            },
        );
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
                .items_start()
                .gap_3()
                .children(
                    snippets
                        .into_iter()
                        .enumerate()
                        .map(|(i, (rec, editable))| {
                            let s = rec.data.clone();
                            let (s_run, r_edit, r_del) = (rec.clone(), rec.clone(), rec.clone());
                            let nvars = termoak_core::template::parse(&s.script).variables.len();
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
                                        .when(nvars > 0, |this| {
                                            this.child(ui::pill(
                                                tn!("snippets.vars.count", nvars),
                                                theme.muted_foreground,
                                            ))
                                        })
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

/// One host's result of `POST /exec`.
#[derive(serde::Deserialize)]
pub(crate) struct ServerExec {
    host_id: Id,
    #[serde(default)]
    exit_code: Option<u32>,
    #[serde(default)]
    stdout: String,
    #[serde(default)]
    stderr: String,
    #[serde(default)]
    duration_ms: u64,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    error: Option<String>,
}

impl ServerExec {
    /// The output when it ran (tests).
    #[cfg(test)]
    pub fn output(&self) -> Option<String> {
        match self.state() {
            RunState::Done(o) => Some(o.stdout_text()),
            _ => None,
        }
    }

    fn state(&self) -> RunState {
        match &self.error {
            Some(e) => RunState::Failed(crate::security_keys::server_error_text(e)),
            None => RunState::Done(ExecOutput {
                exit_code: self.exit_code,
                exit_signal: None,
                stdout: self.stdout.clone().into_bytes(),
                stderr: self.stderr.clone().into_bytes(),
                truncated: false,
                timed_out: self.timed_out,
                duration_ms: self.duration_ms,
            }),
        }
    }
}

/// Content of the "Run on…" dialog.
struct RunDialog {
    item: Item<Snippet>,
    /// (id, label, account) of every host.
    hosts: Vec<(Id, String, Option<Id>)>,
    selected: Vec<Id>,
    vars: SnippetVars,
    /// Run from the server (`/exec`) instead of this computer.
    on_server: bool,
    view: gpui::WeakEntity<SnippetsView>,
}

impl RunDialog {
    fn new(
        item: Item<Snippet>,
        hosts: Vec<(Id, String, Option<Id>)>,
        remembered: &BTreeMap<String, String>,
        view: gpui::WeakEntity<SnippetsView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Every target is a host: built-ins are never asked.
        let vars = SnippetVars::new(
            snippet_vars::prompts(&item.data.script, true),
            remembered,
            window,
            cx,
        );
        Self {
            item,
            hosts,
            selected: Vec::new(),
            vars,
            on_server: false,
            view,
        }
    }

    /// The account whose server can run it: the snippet's, when every
    /// chosen host is in it too.
    fn server_account(&self) -> Option<Id> {
        let Scope::Account(a) = self.item.scope else {
            return None;
        };
        (!self.selected.is_empty()
            && self.selected.iter().all(|id| {
                self.hosts
                    .iter()
                    .any(|(h, _, acc)| h == id && *acc == Some(a))
            }))
        .then_some(a)
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.selected.is_empty() {
            ui::error(window, cx, t!("snippets.run.choose_host"));
            return false;
        }
        let values = match self.vars.values(cx) {
            Ok(v) => v,
            Err(missing) => {
                ui::error(window, cx, t!("snippets.run.missing_value", name = missing));
                return false;
            }
        };
        let snippet = self.item.data.clone();
        // Remembered on this computer for the next run (never secrets).
        if let Some(view) = self.view.upgrade() {
            let ws = view.read(cx).model.read(cx).ws.clone();
            let (id, script, v) = (snippet.id, snippet.script.clone(), values.clone());
            runtime::handle(cx).spawn(async move {
                let _ = ws.remember_snippet_values(id, None, &script, &v).await;
            });
        }
        let targets = self.selected.clone();
        let on_server = self.on_server && self.server_account().is_some();
        if let Some(view) = self.view.upgrade() {
            view.update(cx, |v, cx| {
                if on_server {
                    v.run_on_server(snippet, values, targets, window, cx)
                } else {
                    v.run(
                        snippet.name.clone(),
                        snippet.script.clone(),
                        values,
                        targets,
                        window,
                        cx,
                    )
                }
            });
        }
        true
    }
}

impl Render for RunDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let all = self.selected.len() == self.hosts.len();
        let server = self.server_account().is_some();
        let where_button = |id: &'static str,
                            label: SharedString,
                            server_side: bool,
                            on: bool,
                            cx: &mut Context<Self>| {
            Button::new(id)
                .small()
                .label(label)
                .map(|b| if on { b.primary() } else { b.ghost() })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.on_server = server_side;
                    cx.notify();
                }))
        };
        let on_server = self.on_server && server;
        let muted = theme.muted_foreground;
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("snippets.run.explanation", name = self.item.data.name)),
            )
            .when(!self.vars.is_empty(), |this| {
                this.child(self.vars.render(cx))
            })
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
                                    this.selected =
                                        this.hosts.iter().map(|(id, _, _)| *id).collect();
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("run-hosts")
                    .max_h(px(220.))
                    .gap_1()
                    .overflow_y_scrollbar()
                    .children(self.hosts.iter().enumerate().map(|(i, (id, label, _))| {
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
            .when(server, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(div().text_sm().font_medium().child(t!("snippets.run.from")))
                                .child(where_button(
                                    "run-here",
                                    t!("snippets.run.from_here"),
                                    false,
                                    !on_server,
                                    cx,
                                ))
                                .child(where_button(
                                    "run-server",
                                    t!("snippets.run.from_server"),
                                    true,
                                    on_server,
                                    cx,
                                )),
                        )
                        .child(div().text_xs().text_color(muted).child(if on_server {
                            t!("snippets.run.from_server_hint")
                        } else {
                            t!("snippets.run.from_here_hint")
                        })),
                )
            })
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

/// A terminal the "Send snippet" dialog may type into.
#[derive(Debug, Clone)]
pub struct SendTarget {
    pub pane: EntityId,
    /// Its host (`None`: a local shell or serial port: the host built-ins
    /// are asked).
    pub host: Option<Id>,
    pub focused: bool,
    /// Included in "Run in all panes" (not left out of the broadcast).
    pub included: bool,
}

/// Terminal → Send snippet: picks a snippet, fills its variables and types
/// it into the focused terminal or, in a split view, into every included
/// pane ("Run in all panes", checked while broadcasting), each one with its
/// own built-ins (`{{host}}`, `{{user}}`...). `on_send` gets the text of
/// each pane. `preselect` chooses a snippet first (the command palette, for
/// one with variables to fill in).
pub fn open_send_dialog(
    model: Entity<AppModel>,
    targets: Vec<SendTarget>,
    broadcast: bool,
    preselect: Option<Id>,
    window: &mut Window,
    cx: &mut gpui::App,
    on_send: impl Fn(Vec<(EntityId, String)>, &mut Window, &mut gpui::App) + 'static,
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
    let dialog = cx.new(|cx| SendDialog::new(model, targets, broadcast, preselect, window, cx));
    let on_send = std::rc::Rc::new(on_send);
    window.open_dialog(cx, move |d, _, _| {
        let (paste, run, enter) = (dialog.clone(), dialog.clone(), dialog.clone());
        let (send_paste, send_run, send_enter) =
            (on_send.clone(), on_send.clone(), on_send.clone());
        d.title(t!("snippets.send.title"))
            .w(px(580.))
            .on_ok(move |_, window, cx| {
                match enter.update(cx, |d, cx| d.finish(true, window, cx)) {
                    Some(texts) => {
                        send_enter(texts, window, cx);
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
                                if let Some(texts) =
                                    paste.update(cx, |d, cx| d.finish(false, window, cx))
                                {
                                    window.close_dialog(cx);
                                    send_paste(texts, window, cx);
                                }
                            }),
                    )
                    .child(
                        Button::new("send-run")
                            .primary()
                            .icon(ui::icon(IconName::Play))
                            .label(t!("snippets.send.run"))
                            .on_click(move |_, window, cx| {
                                if let Some(texts) =
                                    run.update(cx, |d, cx| d.finish(true, window, cx))
                                {
                                    window.close_dialog(cx);
                                    send_run(texts, window, cx);
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
    vars: SnippetVars,
    targets: Vec<SendTarget>,
    all_panes: bool,
    /// Built-ins of the targets' hosts (loaded when the dialog opens).
    contexts: HashMap<Id, TemplateContext>,
    /// Which load of remembered values is current.
    load: u64,
    _sub: Subscription,
}

impl SendDialog {
    fn new(
        model: Entity<AppModel>,
        targets: Vec<SendTarget>,
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
        // The built-ins of every host among the targets.
        let ws = model.read(cx).ws.clone();
        let mut hosts: Vec<Id> = targets.iter().filter_map(|t| t.host).collect();
        hosts.sort();
        hosts.dedup();
        for host in hosts {
            let ws = ws.clone();
            runtime::run_in(
                cx,
                window,
                async move { ws.snippet_context(host).await },
                move |this: &mut Self, res, _, cx| {
                    if let Ok(ctx) = res {
                        this.contexts.insert(host, ctx);
                        cx.notify();
                    }
                },
            );
        }
        let split = targets.len() > 1;
        let mut dialog = Self {
            model,
            search,
            selected: None,
            vars: SnippetVars::new(Vec::new(), &BTreeMap::new(), window, cx),
            targets,
            all_panes: split && broadcast,
            contexts: HashMap::new(),
            load: 0,
            _sub: sub,
        };
        dialog.select(selected, window, cx);
        dialog
    }

    fn split(&self) -> bool {
        self.targets.len() > 1
    }

    /// The terminals it goes to.
    fn chosen_targets(&self) -> Vec<SendTarget> {
        self.targets
            .iter()
            .filter(|t| {
                if self.split() && self.all_panes {
                    t.included || t.focused
                } else {
                    t.focused
                }
            })
            .cloned()
            .collect()
    }

    /// The host its values are remembered for: the focused terminal's
    /// (runs in several panes: none).
    fn memory_host(&self) -> Option<Id> {
        let chosen = self.chosen_targets();
        match chosen.as_slice() {
            [one] => one.host,
            _ => None,
        }
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

    /// Chooses a snippet: its fields now, prefilled with the values
    /// remembered for it once they are read.
    fn select(&mut self, id: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id;
        self.rebuild_vars(&BTreeMap::new(), window, cx);
        self.load += 1;
        let (load, host) = (self.load, self.memory_host());
        if let Some(id) = id {
            let ws = self.model.read(cx).ws.clone();
            runtime::run_in(
                cx,
                window,
                async move { ws.snippet_values(id, host).await },
                move |this: &mut Self, res, window, cx| {
                    if this.load == load
                        && let Ok(values) = res
                        && !values.is_empty()
                    {
                        this.rebuild_vars(&values, window, cx);
                    }
                },
            );
        }
        cx.notify();
    }

    fn rebuild_vars(
        &mut self,
        remembered: &BTreeMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let all_hosts = self.chosen_targets().iter().all(|t| t.host.is_some());
        let vars = self
            .snippet(cx)
            .map(|s| snippet_vars::prompts(&s.script, all_hosts))
            .unwrap_or_default();
        self.vars = SnippetVars::new(vars, remembered, window, cx);
        cx.notify();
    }

    /// The text for each terminal, or `None` (with an error shown) if
    /// something is missing.
    fn finish(
        &mut self,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Vec<(EntityId, String)>> {
        let Some(snippet) = self.snippet(cx) else {
            ui::error(window, cx, t!("snippets.send.choose"));
            return None;
        };
        let values = match self.vars.values(cx) {
            Ok(v) => v,
            Err(missing) => {
                ui::error(window, cx, t!("snippets.run.missing_value", name = missing));
                return None;
            }
        };
        let targets = self.chosen_targets();
        let mut contexts = Vec::new();
        for t in &targets {
            match t.host {
                None => contexts.push(TemplateContext::default()),
                Some(h) => match self.contexts.get(&h) {
                    Some(c) => contexts.push(c.clone()),
                    None => {
                        ui::error(window, cx, t!("snippets.send.loading"));
                        return None;
                    }
                },
            }
        }
        let texts = match snippet_vars::render_targets(&snippet.script, &values, &contexts) {
            Ok(t) => t,
            Err(e) => {
                ui::error(window, cx, e);
                return None;
            }
        };
        // Remembered for next time (never secrets).
        let ws = self.model.read(cx).ws.clone();
        let host = self.memory_host();
        runtime::handle(cx).spawn(async move {
            let _ = ws
                .remember_snippet_values(snippet.id, host, &snippet.script, &values)
                .await;
        });
        Some(
            targets
                .iter()
                .zip(texts)
                .map(|(t, text)| (t.pane, snippet_input(&text, run)))
                .collect(),
        )
    }
}

impl Render for SendDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let items = self.matches(cx);
        let selected = self.selected;
        let split = self.split();
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
            .when(!self.vars.is_empty(), |this| {
                this.child(self.vars.render(cx))
            })
            .when(split, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("send-all-panes")
                                .label(t!("snippets.send.all_panes"))
                                .checked(self.all_panes)
                                .on_click(cx.listener(|this, v: &bool, window, cx| {
                                    this.all_panes = *v;
                                    // The host built-ins may need asking now.
                                    let typed = this.vars.values(cx).unwrap_or_default();
                                    this.rebuild_vars(&typed, window, cx);
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
                .children(snippet_vars::editor_summary(
                    &script.read(cx).value().to_string(),
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
            if !snippet_vars::issues(&snippet.script).is_empty() {
                ui::error(window, cx, t!("snippets.issue.fix_first"));
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
