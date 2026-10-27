//! The extended audit in the interface (0.7 phase 5b, docs-next/E2EE-SSO-
//! AUDIT.md §4 and §10): your activity (Settings → Security) and a team's
//! (Teams → Audit, owners and admins): filters (action presets Access,
//! Certificates, Policy, E2EE, SSO, Sign-ins, more actions by hand, who,
//! when, text), pages of 100 with "Load more" (`before`), export to CSV,
//! JSON or NDJSON; for a team also the retention and the SIEM sinks
//! (webhooks with their signing secret shown once, syslog over TLS or TCP;
//! test, pause, failures and the last error).

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::{Select, SelectEvent};
use gpui_component::spinner::Spinner;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde::Deserialize;
use serde_json::{Value, json};
use termoak_client::Account;
use termoak_client::audit::AuditOf;
use termoak_core::Id;
use termoak_core::model::AuditEntry;
use termoak_core::store::audit::{AuditSink, AuditSinkKind};

use crate::audit::{self, ActionPreset, Filters, Range, SinkForm};
use crate::runtime;
use crate::state::{AppModel, api_error};
use crate::ui::{self, Choice, ChoiceState, IconName};

fn muted(text: impl Into<SharedString>, cx: &App) -> gpui::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

/// `GET /teams/{id}/audit/settings`.
#[derive(Debug, Clone, Deserialize)]
struct SettingsView {
    retention_days: Option<u32>,
    default_retention_days: u32,
    max_retention_days: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct CreatedSink {
    sink: AuditSink,
    secret: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct Member {
    user_id: Id,
    email: String,
}

pub struct AuditPanel {
    model: Entity<AppModel>,
    account: Id,
    of: AuditOf,
    /// The team's name (team scope).
    scope_name: String,
    entries: Vec<AuditEntry>,
    loading: bool,
    error: Option<String>,
    next: Option<i64>,
    preset: ChoiceState<ActionPreset>,
    range: ChoiceState<Range>,
    actor: Entity<InputState>,
    text: Entity<InputState>,
    actions: Entity<InputState>,
    /// User ids → emails (team members, you).
    names: HashMap<String, String>,
    settings: Option<SettingsView>,
    retention: Entity<InputState>,
    sinks: Vec<AuditSink>,
    sinks_supported: bool,
    busy: bool,
    _subs: Vec<Subscription>,
}

impl AuditPanel {
    pub fn new(
        model: Entity<AppModel>,
        account: Id,
        of: AuditOf,
        scope_name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let preset = ui::choice_state(
            audit::PRESETS
                .iter()
                .map(|p| Choice::new(p.label(), *p))
                .collect(),
            Some(&ActionPreset::All),
            window,
            cx,
        );
        let range = ui::choice_state(
            audit::RANGES
                .iter()
                .map(|r| Choice::new(r.label(), *r))
                .collect(),
            Some(&Range::Any),
            window,
            cx,
        );
        let input = |p: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(p))
        };
        let actor = input(t!("audit.filter.actor_placeholder"), window, cx);
        let text = input(t!("audit.filter.text_placeholder"), window, cx);
        let actions = input(t!("audit.filter.actions_placeholder"), window, cx);
        let retention = input(SharedString::from(""), window, cx);
        let sinks_supported = model
            .read(cx)
            .audit_v2(account)
            .is_some_and(|f| f["sinks"].as_array().is_some_and(|a| !a.is_empty()));
        let mut subs = vec![
            cx.subscribe_in(
                &preset,
                window,
                |this, _, _: &SelectEvent<Vec<Choice<ActionPreset>>>, window, cx| {
                    this.reload(window, cx)
                },
            ),
            cx.subscribe_in(
                &range,
                window,
                |this, _, _: &SelectEvent<Vec<Choice<Range>>>, window, cx| this.reload(window, cx),
            ),
        ];
        for i in [&actor, &text, &actions] {
            subs.push(
                cx.subscribe_in(i, window, |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.reload(window, cx);
                    }
                }),
            );
        }
        let mut names = HashMap::new();
        if let Some(me) = model.read(cx).me.as_ref() {
            names.insert(me.id.to_string(), me.email.clone());
        }
        let mut this = Self {
            model,
            account,
            of,
            scope_name,
            entries: Vec::new(),
            loading: false,
            error: None,
            next: None,
            preset,
            range,
            actor,
            text,
            actions,
            names,
            settings: None,
            retention,
            sinks: Vec::new(),
            sinks_supported,
            busy: false,
            _subs: subs,
        };
        this.reload(window, cx);
        if let AuditOf::Team(_) = of {
            this.load_team(window, cx);
        }
        this
    }

    fn acc(&self, cx: &App) -> Option<Arc<Account>> {
        self.model
            .read(cx)
            .ws
            .account(self.account)
            .filter(|a| a.is_signed_in())
    }

    fn filters(&self, cx: &App) -> Filters {
        let v = |i: &Entity<InputState>| i.read(cx).value().to_string();
        let mut actor = v(&self.actor);
        // An email of a known person stands for their user id.
        if let Some((id, _)) = self
            .names
            .iter()
            .find(|(_, email)| email.eq_ignore_ascii_case(actor.trim()))
        {
            actor = format!("user:{id}");
        }
        Filters {
            preset: ui::chosen(&self.preset, cx).unwrap_or(ActionPreset::All),
            actions: v(&self.actions),
            actor,
            vault: None,
            range: ui::chosen(&self.range, cx).unwrap_or(Range::Any),
            text: v(&self.text),
        }
    }

    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.entries.clear();
        self.next = None;
        self.load(None, window, cx);
    }

    fn load(&mut self, before: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let filter = self
            .filters(cx)
            .to_filter(termoak_core::time::now_ms(), before);
        let of = self.of;
        self.loading = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { acc.audit_search(of, &filter).await.map_err(api_error) },
            move |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok(page) => {
                        let n = page.len();
                        if before.is_none() {
                            this.entries.clear();
                        }
                        this.entries.extend(page);
                        this.next = audit::next_before(&this.entries, n);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn load_team(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (AuditOf::Team(team), Some(acc)) = (self.of, self.acc(cx)) else {
            return;
        };
        let sinks = self.sinks_supported;
        runtime::run_in(
            cx,
            window,
            async move {
                let s: SettingsView = acc
                    .api
                    .get(&format!("/api/v1/teams/{team}/audit/settings"))
                    .await
                    .map_err(api_error)?;
                let list = if sinks {
                    acc.audit_sinks(AuditOf::Team(team))
                        .await
                        .map_err(api_error)?
                } else {
                    Vec::new()
                };
                let members: Vec<Member> = acc
                    .api
                    .get(&format!("/api/v1/teams/{team}/members"))
                    .await
                    .unwrap_or_default();
                Ok::<_, String>((s, list, members))
            },
            |this, res, window, cx| {
                match res {
                    Ok((s, list, members)) => {
                        this.retention.update(cx, |i, cx| {
                            i.set_value(
                                s.retention_days.map(|d| d.to_string()).unwrap_or_default(),
                                window,
                                cx,
                            );
                            i.set_placeholder(
                                t!("audit.retention.default", days = s.default_retention_days),
                                window,
                                cx,
                            );
                        });
                        this.settings = Some(s);
                        this.sinks = list;
                        for m in members {
                            this.names.insert(m.user_id.to_string(), m.email);
                        }
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn export(&mut self, format: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(acc) = self.acc(cx) else {
            return;
        };
        let mut filter = self
            .filters(cx)
            .to_filter(termoak_core::time::now_ms(), None);
        filter.limit = None;
        let of = self.of;
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let name = audit::export_name(
            if self.scope_name.is_empty() {
                "account"
            } else {
                &self.scope_name
            },
            format,
            &date,
        );
        let dir = directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(std::path::Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        let rt = runtime::handle(cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let p = path.clone();
            let task = rt.spawn(async move {
                acc.audit_export(of, &filter, format, &p)
                    .await
                    .map_err(api_error)
            });
            let res = match task.await {
                Ok(r) => r,
                Err(e) => Err(e.to_string()),
            };
            let _ = this.update_in(cx, |_, window, cx| match res {
                Ok(bytes) => ui::success(
                    window,
                    cx,
                    t!(
                        "audit.exported",
                        path = path.display().to_string(),
                        size = ui::format_bytes(bytes)
                    ),
                ),
                Err(e) => ui::error(window, cx, e),
            });
        })
        .detach();
    }

    fn save_retention(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (AuditOf::Team(team), Some(acc), Some(s)) =
            (self.of, self.acc(cx), self.settings.clone())
        else {
            return;
        };
        let days =
            match audit::parse_retention(&self.retention.read(cx).value(), s.max_retention_days) {
                Ok(d) => d,
                Err(e) => return ui::error(window, cx, e),
            };
        runtime::run_in(
            cx,
            window,
            async move {
                acc.api
                    .put::<Value>(
                        &format!("/api/v1/teams/{team}/audit/settings"),
                        &json!({"retention_days": days}),
                    )
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| match res {
                Ok(_) => {
                    ui::success(window, cx, t!("audit.retention.saved"));
                    this.load_team(window, cx);
                }
                Err(e) => ui::error(window, cx, e),
            },
        );
    }

    fn sink_op(&mut self, id: Id, op: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(acc), of) = (self.acc(cx), self.of) else {
            return;
        };
        let enabled = self.sinks.iter().find(|s| s.id == id).map(|s| s.enabled);
        self.busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                match op {
                    "test" => acc.test_audit_sink(of, id).await.map_err(api_error),
                    "toggle" => acc
                        .update_audit_sink(of, id, &json!({"enabled": !enabled.unwrap_or(true)}))
                        .await
                        .map(|_| json!({"ok": true}))
                        .map_err(api_error),
                    _ => acc
                        .delete_audit_sink(of, id)
                        .await
                        .map(|_| json!({"ok": true}))
                        .map_err(api_error),
                }
            },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) if op == "test" => {
                        if v["ok"].as_bool().unwrap_or(false) {
                            ui::success(window, cx, t!("audit.sink.test_ok"));
                        } else {
                            ui::error(
                                window,
                                cx,
                                t!(
                                    "audit.sink.test_failed",
                                    error = v["error"].as_str().unwrap_or_default()
                                ),
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => ui::error(window, cx, e),
                }
                this.load_team(window, cx);
                cx.notify();
            },
        );
    }

    fn delete_sink(&mut self, id: Id, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("audit.sink.delete_title", name = name),
            t!("audit.sink.delete_message"),
            t!("common.delete"),
            true,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |this, cx| this.sink_op(id, "delete", window, cx));
                }
            },
        );
    }

    fn add_sink(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(acc), of) = (self.acc(cx), self.of) else {
            return;
        };
        let weak = cx.entity().downgrade();
        let form = cx.new(|cx| SinkFormView::new(window, cx));
        let f2 = form.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("audit.sink.add_title"),
            t!("audit.sink.add"),
            560.,
            move |_, _| form.clone().into_any_element(),
            move |window, cx| {
                let values = f2.read(cx).values(cx);
                let (name, config, actions) = match values.to_parts() {
                    Ok(p) => p,
                    Err(e) => {
                        ui::error(window, cx, e);
                        return false;
                    }
                };
                let kind = values.kind;
                let acc = acc.clone();
                let weak = weak.clone();
                let task = runtime::spawn(cx, async move {
                    acc.create_audit_sink(of, kind, &name, &config, &actions)
                        .await
                        .map_err(api_error)
                });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(v) => {
                                if let Ok(c) = serde_json::from_value::<CreatedSink>(v)
                                    && let Some(secret) = c.secret
                                {
                                    show_secret(c.sink.name, secret, window, cx);
                                } else {
                                    ui::success(window, cx, t!("audit.sink.added"));
                                }
                                if let Some(v) = weak.upgrade() {
                                    v.update(cx, |this, cx| this.load_team(window, cx));
                                }
                            }
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
                true
            },
        );
    }

    fn actor_text(&self, actor: &str) -> String {
        match actor.split_once(':') {
            Some(("user", id)) => self
                .names
                .get(id)
                .cloned()
                .unwrap_or_else(|| format!("user {}", id.get(..8).unwrap_or(id))),
            _ => actor.to_string(),
        }
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        h_flex()
            .gap_2()
            .flex_wrap()
            .items_end()
            .child(div().w(px(170.)).child(ui::field(
                t!("audit.filter.preset"),
                Select::new(&self.preset),
                cx,
            )))
            .child(div().w(px(170.)).child(ui::field(
                t!("audit.filter.range"),
                Select::new(&self.range),
                cx,
            )))
            .child(div().w(px(200.)).child(ui::field(
                t!("audit.filter.actor"),
                Input::new(&self.actor),
                cx,
            )))
            .child(div().w(px(180.)).child(ui::field(
                t!("audit.filter.text"),
                Input::new(&self.text),
                cx,
            )))
            .child(div().w(px(200.)).child(ui::field(
                t!("audit.filter.actions"),
                Input::new(&self.actions),
                cx,
            )))
            .child(
                Button::new("audit-search")
                    .icon(ui::icon(IconName::Search))
                    .label(t!("audit.filter.apply"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.reload(window, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_entries(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mono = ui::mono_family(cx);
        let mut col = v_flex().gap_0p5();
        if self.entries.is_empty() && !self.loading {
            col = col.child(muted(t!("audit.empty"), cx));
        }
        for (i, e) in self.entries.iter().enumerate() {
            let detail = audit::detail_summary(&e.detail, 160);
            let target = e.target.clone().unwrap_or_default();
            col = col.child(
                h_flex()
                    .id(("audit-entry", i))
                    .px_2()
                    .py_1p5()
                    .gap_3()
                    .items_start()
                    .rounded(theme.radius)
                    .hover(|s| s.bg(theme.secondary_hover))
                    .child(
                        div()
                            .w(px(130.))
                            .flex_shrink_0()
                            .text_xs()
                            .font_family(mono.clone())
                            .text_color(theme.muted_foreground)
                            .child(ui::format_ms(e.created_at)),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_medium()
                                            .child(audit::action_text(&e.action)),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_family(mono.clone())
                                            .text_color(theme.muted_foreground)
                                            .child(e.action.clone()),
                                    ),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                if target.is_empty() {
                                    self.actor_text(&e.actor)
                                } else {
                                    format!(
                                        "{} → {}",
                                        self.actor_text(&e.actor),
                                        self.names.get(&target).cloned().unwrap_or(target)
                                    )
                                },
                            ))
                            .when(!detail.is_empty(), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .font_family(mono.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(detail),
                                )
                            }),
                    ),
            );
        }
        col.child(
            h_flex()
                .pt_2()
                .gap_2()
                .when(self.loading, |this| this.child(Spinner::new().small()))
                .when_some(self.next.filter(|_| !self.loading), |this, before| {
                    this.child(
                        Button::new("audit-more")
                            .small()
                            .label(t!("audit.load_more"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.load(Some(before), window, cx)
                            })),
                    )
                })
                .when(self.next.is_none() && !self.entries.is_empty(), |this| {
                    this.child(muted(tn!("audit.count", self.entries.len()), cx))
                }),
        )
        .into_any_element()
    }

    fn render_team(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let Some(s) = &self.settings else {
            return div().into_any_element();
        };
        let retention = v_flex()
            .gap_2()
            .child(div().font_semibold().child(t!("audit.retention.title")))
            .child(muted(
                t!(
                    "audit.retention.hint",
                    default = s.default_retention_days,
                    max = s.max_retention_days
                ),
                cx,
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(160.)).child(Input::new(&self.retention)))
                    .child(muted(t!("audit.retention.days"), cx))
                    .child(
                        Button::new("audit-retention-save")
                            .small()
                            .label(t!("common.save"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save_retention(window, cx)
                            })),
                    ),
            );
        let mut sinks = v_flex()
            .gap_2()
            .child(div().font_semibold().child(t!("audit.sinks.title")))
            .child(muted(t!("audit.sinks.hint"), cx));
        if self.sinks_supported {
            if self.sinks.is_empty() {
                sinks = sinks.child(muted(t!("audit.sinks.none"), cx));
            }
            for (i, s) in self.sinks.iter().enumerate() {
                let (state, ok) = audit::sink_state(s);
                let (id, id2, id3, name) = (s.id, s.id, s.id, s.name.clone());
                sinks = sinks.child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .p_2()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(if ok { theme.border } else { theme.danger })
                        .child(ui::icon(match s.kind {
                            AuditSinkKind::Webhook => IconName::Webhook,
                            AuditSinkKind::Syslog => IconName::ScrollText,
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_medium().child(s.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(ui::mono_family(cx))
                                        .text_color(theme.muted_foreground)
                                        .child(audit::sink_target(s)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if ok {
                                            theme.muted_foreground
                                        } else {
                                            theme.danger
                                        })
                                        .child(state),
                                )
                                .when(!s.actions.is_empty(), |this| {
                                    this.child(muted(s.actions.join(", "), cx).text_xs())
                                }),
                        )
                        .child(
                            Switch::new(("sink-on", i))
                                .checked(s.enabled)
                                .tooltip(t!("audit.sink.enabled"))
                                .on_click(cx.listener(move |this, _: &bool, window, cx| {
                                    this.sink_op(id, "toggle", window, cx)
                                })),
                        )
                        .child(
                            Button::new(("sink-test", i))
                                .small()
                                .label(t!("audit.sink.test"))
                                .disabled(self.busy)
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.sink_op(id2, "test", window, cx)
                                })),
                        )
                        .child(
                            Button::new(("sink-del", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .tooltip(t!("common.delete"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.delete_sink(id3, name.clone(), window, cx)
                                })),
                        ),
                );
            }
            sinks = sinks.child(
                h_flex().child(
                    Button::new("sink-add")
                        .icon(ui::icon(IconName::Plus))
                        .label(t!("audit.sink.add"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.add_sink(window, cx)
                        })),
                ),
            );
        }
        v_flex()
            .gap_5()
            .child(retention)
            .child(sinks)
            .into_any_element()
    }
}

impl Render for AuditPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (primary, danger) = (cx.theme().primary, cx.theme().danger);
        let exports = h_flex()
            .gap_1()
            .child(muted(t!("audit.export"), cx))
            .children(["csv", "json", "ndjson"].into_iter().map(|f| {
                Button::new(SharedString::from(format!("audit-export-{f}")))
                    .xsmall()
                    .label(f.to_uppercase())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.export(f, window, cx)
                    }))
            }));
        let team = matches!(self.of, AuditOf::Team(_));
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(IconName::ScrollText).text_color(primary))
                    .child(div().font_semibold().child(if team {
                        t!("audit.team_title")
                    } else {
                        t!("audit.account_title")
                    }))
                    .child(div().flex_1())
                    .child(exports),
            )
            .child(self.render_filters(cx))
            .when_some(self.error.clone(), |this, e| {
                this.child(div().text_sm().text_color(danger).child(e))
            })
            .child(self.render_entries(cx))
            .when(team, |this| this.child(self.render_team(cx)))
    }
}

/// The webhook's signing secret, shown once.
fn show_secret(name: String, secret: String, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |d, _, cx| {
        let s2 = secret.clone();
        let theme = cx.theme();
        d.title(t!("audit.sink.secret_title", name = name.clone()))
            .w(px(520.))
            .overlay_closable(false)
            .child(
                v_flex()
                    .gap_3()
                    .child(div().text_sm().child(t!("audit.sink.secret_message")))
                    .child(
                        h_flex()
                            .gap_2()
                            .p_2()
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .font_family(ui::mono_family(cx))
                                    .child(secret.clone()),
                            )
                            .child(
                                Button::new("sink-secret-copy")
                                    .small()
                                    .icon(ui::icon(IconName::Copy))
                                    .label(t!("common.copy"))
                                    .on_click(move |_: &ClickEvent, window, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            s2.clone(),
                                        ));
                                        ui::success(window, cx, t!("audit.sink.secret_copied"));
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("audit.sink.secret_hint")),
                    ),
            )
            .footer(
                h_flex().w_full().justify_end().child(
                    Button::new("sink-secret-done")
                        .primary()
                        .label(t!("common.close"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    });
}

/// The fields of a new sink.
struct SinkFormView {
    kind: ChoiceState<AuditSinkKind>,
    tls: bool,
    name: Entity<InputState>,
    url: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    ca_pem: Entity<TextareaState>,
    actions: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl SinkFormView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let kind = ui::choice_state(
            vec![
                Choice::new(t!("audit.sink.kind_webhook"), AuditSinkKind::Webhook),
                Choice::new(t!("audit.sink.kind_syslog"), AuditSinkKind::Syslog),
            ],
            Some(&AuditSinkKind::Webhook),
            window,
            cx,
        );
        let input = |p: SharedString, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(p))
        };
        let subs = vec![cx.subscribe_in(
            &kind,
            window,
            |_, _, _: &SelectEvent<Vec<Choice<AuditSinkKind>>>, _, cx| cx.notify(),
        )];
        Self {
            kind,
            tls: true,
            name: input(t!("audit.sink.name_placeholder"), window, cx),
            url: input("https://siem.example.com/termoak".into(), window, cx),
            host: input("logs.example.com".into(), window, cx),
            port: input("6514".into(), window, cx),
            ca_pem: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 6)
                    .placeholder("-----BEGIN CERTIFICATE-----")
            }),
            actions: input("access.*, e2ee.*, sso.*".into(), window, cx),
            _subs: subs,
        }
    }

    fn values(&self, cx: &App) -> SinkForm {
        let v = |i: &Entity<InputState>| i.read(cx).value().to_string();
        SinkForm {
            kind: ui::chosen(&self.kind, cx).unwrap_or(AuditSinkKind::Webhook),
            name: v(&self.name),
            url: v(&self.url),
            host: v(&self.host),
            port: v(&self.port),
            tls: self.tls,
            ca_pem: self.ca_pem.read(cx).value().to_string(),
            actions: v(&self.actions),
        }
    }
}

impl Render for SinkFormView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = ui::chosen(&self.kind, cx).unwrap_or(AuditSinkKind::Webhook);
        v_flex()
            .gap_3()
            .child(ui::field(
                t!("audit.sink.kind"),
                Select::new(&self.kind),
                cx,
            ))
            .child(ui::field(t!("audit.sink.name"), Input::new(&self.name), cx))
            .map(|this| match kind {
                AuditSinkKind::Webhook => this
                    .child(ui::field(t!("audit.sink.url"), Input::new(&self.url), cx))
                    .child(muted(t!("audit.sink.webhook_hint"), cx)),
                AuditSinkKind::Syslog => this
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(ui::field(
                                t!("audit.sink.host"),
                                Input::new(&self.host),
                                cx,
                            )))
                            .child(div().w(px(110.)).child(ui::field(
                                t!("audit.sink.port"),
                                Input::new(&self.port),
                                cx,
                            ))),
                    )
                    .child(
                        Switch::new("sink-tls")
                            .label(t!("audit.sink.tls"))
                            .checked(self.tls)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.tls = *v;
                                cx.notify();
                            })),
                    )
                    .when(self.tls, |this| {
                        this.child(ui::field_with_hint(
                            t!("audit.sink.ca_pem"),
                            Textarea::new(&self.ca_pem),
                            t!("audit.sink.ca_pem_hint"),
                            cx,
                        ))
                    })
                    .when(!self.tls, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().warning)
                                .child(t!("audit.sink.tcp_warning")),
                        )
                    }),
            })
            .child(ui::field_with_hint(
                t!("audit.sink.actions"),
                Input::new(&self.actions),
                t!("audit.sink.actions_hint"),
                cx,
            ))
    }
}
