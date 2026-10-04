//! Dialog to share a session through the server: invite a user by email,
//! share with one of your teams or create a link for guests without an
//! account, with permission to view or to view and control. It works for the
//! SSH terminals of this computer (relay) and for server sessions.

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement,
    Render, Styled, Subscription, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Disableable, Sizable, WindowExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_client::ApiClient;
use termoak_core::Id;

use crate::runtime;
use crate::state::{AppModel, ToastKind, api_error};
use crate::terminal::TerminalView;
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Opens the dialog to share the session `session_id`. `terminal` is the
/// tab shared from this computer (for "Stop sharing").
pub fn open(
    model: Entity<AppModel>,
    session_id: Id,
    terminal: Option<WeakEntity<TerminalView>>,
    is_relay: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(api) = model.read(cx).api.clone() else {
        ui::notify(window, cx, ToastKind::Warning, t!("share.need_login"));
        return;
    };
    let dialog =
        cx.new(|cx| ShareDialog::new(model, api, session_id, terminal, is_relay, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!("share.title"))
            .w(px(560.))
            .child(dialog.clone())
            .footer(
                h_flex().w_full().justify_end().child(
                    Button::new("share-close")
                        .label(t!("common.close"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
    });
}

/// Content of the share dialog.
pub struct ShareDialog {
    model: Entity<AppModel>,
    api: ApiClient,
    session_id: Id,
    terminal: Option<WeakEntity<TerminalView>>,
    is_relay: bool,
    email: Entity<InputState>,
    team: ChoiceState<Id>,
    /// Teams in the dropdown (to rebuild it if they change).
    team_ids: Vec<Id>,
    control: bool,
    busy: bool,
    link: Option<String>,
    invited: Vec<String>,
    _sub: Subscription,
}

fn team_choices(model: &AppModel) -> Vec<Choice<Id>> {
    model
        .teams
        .iter()
        .map(|t| {
            Choice::new(
                format!("{} · {}", t.name, tn!("share.team_members", t.member_count)),
                t.id,
            )
        })
        .collect()
}

impl ShareDialog {
    fn new(
        model: Entity<AppModel>,
        api: ApiClient,
        session_id: Id,
        terminal: Option<WeakEntity<TerminalView>>,
        is_relay: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let email =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("share.email_placeholder")));
        let choices = team_choices(model.read(cx));
        let team_ids: Vec<Id> = choices.iter().map(|c| c.value).collect();
        let first = team_ids.first().copied();
        let team = ui::choice_state(choices, first.as_ref(), window, cx);
        // The teams may have changed from another device.
        model.update(cx, |m, cx| m.refresh_teams(cx));
        let sub = cx.observe_in(&model, window, |this, model, window, cx| {
            let choices = team_choices(model.read(cx));
            let ids: Vec<Id> = choices.iter().map(|c| c.value).collect();
            if ids != this.team_ids {
                let keep = ui::chosen(&this.team, cx).filter(|id| ids.contains(id));
                this.team_ids = ids.clone();
                this.team.update(cx, |s, cx| {
                    s.set_items(choices, window, cx);
                    if let Some(id) = keep.or(ids.first().copied()) {
                        s.set_selected_value(&id, window, cx);
                    }
                });
                cx.notify();
            }
        });
        Self {
            model,
            api,
            session_id,
            terminal,
            is_relay,
            email,
            team,
            team_ids,
            control: false,
            busy: false,
            link: None,
            invited: Vec::new(),
            _sub: sub,
        }
    }

    fn permission(&self) -> &'static str {
        if self.control { "control" } else { "view" }
    }

    /// Creates an invitation with the body `body` (email, team or link).
    fn share(
        &mut self,
        mut body: Value,
        window: &mut Window,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Value, &mut Window, &mut Context<Self>) + 'static,
    ) {
        body["permission"] = json!(self.permission());
        self.busy = true;
        cx.notify();
        let api = self.api.clone();
        let path = format!("/api/v1/sessions/{}/shares", self.session_id);
        runtime::run_in(
            cx,
            window,
            async move { api.post::<Value>(&path, &body).await.map_err(api_error) },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => done(this, v, window, cx),
                    Err(e) => ui::error(window, cx, t!("share.error", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn invite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let email = self.email.read(cx).value().trim().to_string();
        if !email.contains('@') {
            ui::error(window, cx, t!("share.invalid_email"));
            return;
        }
        self.share(
            json!({"email": email}),
            window,
            cx,
            move |this, _, window, cx| {
                this.invited.push(email.clone());
                this.email.update(cx, |i, cx| i.set_value("", window, cx));
                ui::success(window, cx, t!("share.invited", email = email));
            },
        );
    }

    fn share_team(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(team_id) = ui::chosen(&self.team, cx) else {
            ui::error(window, cx, t!("share.choose_team"));
            return;
        };
        let name = self
            .model
            .read(cx)
            .teams
            .iter()
            .find(|t| t.id == team_id)
            .map(|t| t.name.clone())
            .unwrap_or_else(|| t!("share.team_fallback").to_string());
        self.share(
            json!({"team_id": team_id}),
            window,
            cx,
            move |this, _, window, cx| {
                this.invited
                    .push(t!("share.invited_team", name = name).to_string());
                ui::success(window, cx, t!("share.shared_with_team", name = name));
            },
        );
    }

    fn create_link(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.share(json!({"link": true}), window, cx, |this, v, window, cx| {
            let link = v["link"]
                .as_str()
                .or_else(|| v["app_link"].as_str())
                .unwrap_or("")
                .to_string();
            if !link.is_empty() {
                cx.write_to_clipboard(ClipboardItem::new_string(link.clone()));
                ui::success(window, cx, t!("share.link_copied"));
                this.link = Some(link);
            }
        });
    }
}

impl Render for ShareDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let control = self.control;
        let has_teams = !self.team_ids.is_empty();
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(if self.is_relay {
                        t!("share.relay_hint")
                    } else {
                        t!("share.server_hint")
                    }),
            )
            .child(ui::field(
                t!("share.permission"),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("perm-view")
                            .small()
                            .label(t!("share.view_only"))
                            .when(!control, |b| b.primary())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.control = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("perm-control")
                            .small()
                            .label(t!("share.view_control"))
                            .when(control, |b| b.primary())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.control = true;
                                cx.notify();
                            })),
                    ),
                cx,
            ))
            .child(ui::field(
                t!("share.invite_user"),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.email)))
                    .child(
                        Button::new("invite")
                            .primary()
                            .icon(ui::icon(IconName::Mail))
                            .label(t!("share.invite"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.invite(window, cx);
                            })),
                    ),
                cx,
            ))
            .child(ui::field(
                t!("share.share_with_team"),
                if has_teams {
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(Select::new(&self.team)))
                        .child(
                            Button::new("share-team")
                                .icon(ui::icon(IconName::Users))
                                .label(t!("share.share"))
                                .loading(self.busy)
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.share_team(window, cx);
                                })),
                        )
                        .into_any_element()
                } else {
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("share.no_teams"))
                        .into_any_element()
                },
                cx,
            ))
            .when(!self.invited.is_empty(), |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("share.shared_with", names = self.invited.join(", "))),
                )
            })
            .child(ui::field(
                t!("share.guest_link"),
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .p_2()
                            .rounded(theme.radius)
                            .bg(theme.muted)
                            .font_family(ui::mono_family(cx))
                            .text_xs()
                            .whitespace_nowrap()
                            .child(
                                self.link
                                    .clone()
                                    .unwrap_or_else(|| t!("share.no_link").to_string()),
                            ),
                    )
                    .child(
                        Button::new("link")
                            .icon(ui::icon(IconName::Link))
                            .label(t!("share.create_link"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.create_link(window, cx);
                            })),
                    ),
                cx,
            ))
            .when(self.is_relay && self.terminal.is_some(), |this| {
                this.child(
                    h_flex().justify_end().child(
                        Button::new("stop-share")
                            .danger()
                            .small()
                            .label(t!("share.stop"))
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                if let Some(t) = this.terminal.as_ref().and_then(|t| t.upgrade()) {
                                    t.update(cx, |t, cx| {
                                        t.stop_sharing(cx);
                                        cx.notify();
                                    });
                                }
                                window.close_dialog(cx);
                                ui::notify(window, cx, ToastKind::Info, t!("share.stopped"));
                            })),
                    ),
                )
            })
    }
}
