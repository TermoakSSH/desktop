//! Server sessions: terminals that live on the Termoak server (they stay
//! open even if you close the app) and sessions others have shared with
//! you. From here you get back into them.

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use serde_json::Value;
use termoak_core::Id;

use super::OpenRequest;
use crate::runtime;
use crate::state::{AppModel, ModelEvent};
use crate::ui::{self, IconName};

/// A session as described by the API.
#[derive(Clone)]
struct SessionRow {
    id: Id,
    title: String,
    kind: String,
    state: String,
    detail: String,
    /// People inside (or sockets, for servers before live sharing).
    viewers: usize,
    created_at: i64,
    access: String,
    /// Who shared it with you.
    owner_name: Option<String>,
}

/// People inside a session as listed (`participants`, without those in the
/// waiting room; servers before live sharing only have `viewers`).
fn people_inside(v: &Value) -> usize {
    match v["participants"].as_array() {
        Some(list) if !list.is_empty() => list.iter().filter(|p| p["waiting"] != true).count(),
        _ => v["viewers"].as_array().map(|a| a.len()).unwrap_or(0),
    }
}

impl SessionRow {
    fn from_view(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_str()?.parse().ok()?,
            title: v["title"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| t!("server_sessions.default_title").to_string()),
            kind: v["kind"].as_str().unwrap_or("server").to_string(),
            state: v["state"]["state"].as_str().unwrap_or("").to_string(),
            detail: v["state"]["message"]
                .as_str()
                .or_else(|| v["state"]["reason"].as_str())
                .unwrap_or("")
                .to_string(),
            viewers: people_inside(v),
            created_at: v["created_at"].as_i64().unwrap_or(0),
            access: v["access"].as_str().unwrap_or("owner").to_string(),
            owner_name: v["owner_name"]
                .as_str()
                .filter(|n| !n.trim().is_empty())
                .map(str::to_string),
        })
    }

    fn from_info(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_str()?.parse().ok()?,
            title: v["title"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| t!("server_sessions.default_title").to_string()),
            kind: v["kind"].as_str().unwrap_or("server").to_string(),
            state: v["status"].as_str().unwrap_or("closed").to_string(),
            detail: v["error"].as_str().unwrap_or("").to_string(),
            viewers: 0,
            created_at: v["ended_at"]
                .as_i64()
                .or(v["created_at"].as_i64())
                .unwrap_or(0),
            access: "owner".into(),
            owner_name: None,
        })
    }
}

pub struct ServerSessionsView {
    model: Entity<AppModel>,
    active: Vec<SessionRow>,
    shared: Vec<SessionRow>,
    recent: Vec<SessionRow>,
    loading: bool,
    error: Option<String>,
    /// "Join with link" field.
    join_link: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for ServerSessionsView {}

impl ServerSessionsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let join_link =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("join.link_placeholder")));
        let subs = vec![
            cx.subscribe_in(
                &model,
                window,
                |this, _, ev: &ModelEvent, window, cx| match ev {
                    ModelEvent::SessionChanged => this.refresh(window, cx),
                    ModelEvent::Server(v) if v["type"] == "session" => this.refresh(window, cx),
                    _ => {}
                },
            ),
            // The badges of the rows.
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &join_link,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.join(window, cx);
                    }
                },
            ),
        ];
        let mut view = Self {
            model,
            active: Vec::new(),
            shared: Vec::new(),
            recent: Vec::new(),
            loading: false,
            error: None,
            join_link,
            _subs: subs,
        };
        view.refresh(window, cx);
        view
    }

    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            self.active.clear();
            self.shared.clear();
            self.recent.clear();
            cx.notify();
            return;
        };
        self.loading = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { api.get::<Value>("/api/v1/sessions").await },
            |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok(v) => {
                        let list =
                            |key: &str, f: fn(&Value) -> Option<SessionRow>| -> Vec<SessionRow> {
                                v[key]
                                    .as_array()
                                    .map(|a| a.iter().filter_map(f).collect())
                                    .unwrap_or_default()
                            };
                        this.active = list("active", SessionRow::from_view);
                        this.shared = list("shared", SessionRow::from_view);
                        this.recent = list("recent", SessionRow::from_info);
                        this.error = None;
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    /// Opens the "Join with link" dialog with what was pasted.
    fn join(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.join_link.read(cx).value().trim().to_string();
        if !text.is_empty() && crate::sharing::parse_join_link(&text).is_none() {
            ui::error(window, cx, t!("join.invalid_link"));
            return;
        }
        self.join_link
            .update(cx, |i, cx| i.set_value("", window, cx));
        let weak = cx.entity().downgrade();
        super::join::open(
            self.model.clone(),
            Some(text).filter(|t| !t.is_empty()),
            std::rc::Rc::new(move |req, _, cx| {
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |_, cx| cx.emit(req));
                }
            }),
            window,
            cx,
        );
    }

    fn render_join(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        ui::card(cx)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(IconName::Link).size(px(16.)))
                    .child(div().font_semibold().text_sm().child(t!("join.title")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("join.section_hint")),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.join_link)))
                    .child(
                        Button::new("join-with-link")
                            .primary()
                            .icon(ui::icon(IconName::LogIn))
                            .label(t!("join.join"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.join(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn terminate(&mut self, row: SessionRow, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("server_sessions.terminate_title"),
            t!("server_sessions.terminate_message", title = row.title),
            t!("server_sessions.terminate"),
            true,
            move |window, cx| {
                let api = api.clone();
                let id = row.id;
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |_, cx| {
                        runtime::run_in(
                            cx,
                            window,
                            async move { api.delete(&format!("/api/v1/sessions/{id}")).await },
                            |this, res, window, cx| {
                                if let Err(e) = res {
                                    ui::error(
                                        window,
                                        cx,
                                        t!("server_sessions.terminate_failed", error = e),
                                    );
                                }
                                this.refresh(window, cx);
                            },
                        );
                    });
                }
            },
        );
    }

    fn render_rows(
        &self,
        id: &'static str,
        title: SharedString,
        rows: &[SessionRow],
        closed: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let alerts = self.model.read(cx).session_alerts.clone();
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().font_semibold().child(title))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(rows.len().to_string()),
                    ),
            )
            .when(rows.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("server_sessions.none")),
                )
            })
            .children(rows.iter().enumerate().map(|(i, r)| {
                let (state_label, color) = match r.state.as_str() {
                    "running" => (t!("server_sessions.state.running"), theme.success),
                    "connecting" => (t!("server_sessions.state.connecting"), theme.warning),
                    "host_offline" => (t!("server_sessions.state.host_offline"), theme.warning),
                    "failed" => (t!("server_sessions.state.failed"), theme.danger),
                    _ => (t!("server_sessions.state.closed"), theme.muted_foreground),
                };
                let open_req = OpenRequest::Attach {
                    session_id: r.id,
                    title: r.title.clone(),
                };
                let row = r.clone();
                let key: SharedString = format!("{id}-{i}").into();
                h_flex()
                    .id(key)
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        ui::icon(if r.kind == "relay" {
                            IconName::Share2
                        } else {
                            IconName::Cloud
                        })
                        .size(px(18.))
                        .text_color(color),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().font_semibold().text_sm().child(r.title.clone()))
                                    .child(ui::pill(state_label, color))
                                    // A terminal of someone's computer, shared through
                                    // the server (not necessarily with a team).
                                    .when(r.kind == "relay", |this| {
                                        this.child(ui::pill(
                                            t!("server_sessions.kind_relay"),
                                            theme.info,
                                        ))
                                    })
                                    .when_some(
                                        r.owner_name.clone().filter(|_| r.access != "owner"),
                                        |this, owner| {
                                            this.child(ui::pill(
                                                t!("server_sessions.shared_by", name = owner),
                                                theme.muted_foreground,
                                            ))
                                        },
                                    )
                                    .when_some(alerts.get(&r.id).copied(), |this, n| {
                                        this.child(ui::pill(
                                            tn!("server_sessions.waiting_for_you", n),
                                            theme.warning,
                                        ))
                                    })
                                    .when(r.access == "view", |this| {
                                        this.child(ui::pill(
                                            t!("server_sessions.view_only"),
                                            theme.muted_foreground,
                                        ))
                                    })
                                    .when(r.access == "control", |this| {
                                        this.child(ui::pill(
                                            t!("server_sessions.control"),
                                            theme.primary,
                                        ))
                                    }),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                format!(
                                    "{}{}{}",
                                    ui::format_ms(r.created_at),
                                    if r.viewers > 0 {
                                        format!(" · {}", tn!("server_sessions.viewers", r.viewers))
                                    } else {
                                        String::new()
                                    },
                                    if r.detail.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" · {}", r.detail)
                                    }
                                ),
                            )),
                    )
                    .when(!closed, |this| {
                        this.child(
                            Button::new(("attach", i))
                                .small()
                                .primary()
                                .icon(ui::icon(IconName::SquareTerminal))
                                .label(t!("server_sessions.open"))
                                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                    cx.emit(open_req.clone())
                                })),
                        )
                    })
                    .when(!closed && r.access == "owner", |this| {
                        let model = self.model.clone();
                        let (session_id, relay) = (r.id, r.kind == "relay");
                        this.child(
                            Button::new(("share", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Share2))
                                .tooltip(t!("server_sessions.share_tooltip"))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    super::share::open(
                                        model.clone(),
                                        session_id,
                                        None,
                                        relay,
                                        window,
                                        cx,
                                    )
                                }),
                        )
                        .child(
                            Button::new(("terminate", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::CircleStop))
                                .tooltip(t!("server_sessions.terminate_tooltip"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.terminate(row.clone(), window, cx)
                                })),
                        )
                    })
            }))
            .into_any_element()
    }
}

impl Render for ServerSessionsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let logged_in = self.model.read(cx).logged_in();
        let body: gpui::AnyElement = if !logged_in {
            // Links work without an account.
            v_flex()
                .gap_6()
                .child(self.render_join(cx))
                .child(ui::empty_state(
                    IconName::Cloud,
                    t!("server_sessions.no_server_title"),
                    t!("server_sessions.no_server_detail"),
                    cx,
                ))
                .into_any_element()
        } else {
            let active = self.active.clone();
            let shared = self.shared.clone();
            let recent: Vec<SessionRow> = self.recent.iter().take(20).cloned().collect();
            v_flex()
                .gap_6()
                .when_some(self.error.clone(), |this, e| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(t!("server_sessions.load_failed", error = e)),
                    )
                })
                .child(self.render_join(cx))
                .child(self.render_rows("active", t!("server_sessions.active"), &active, false, cx))
                .child(self.render_rows("shared", t!("server_sessions.shared"), &shared, false, cx))
                .child(self.render_rows("recent", t!("server_sessions.recent"), &recent, true, cx))
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("server_sessions.title"),
                t!("server_sessions.subtitle"),
                Button::new("refresh-sessions")
                    .icon(ui::icon(IconName::RefreshCw))
                    .label(t!("common.refresh"))
                    .loading(self.loading)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.refresh(window, cx)),
                    ),
                cx,
            ))
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().p_6().child(body)),
                ),
            )
    }
}
