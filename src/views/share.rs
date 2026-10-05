//! Dialog to share a session through the server: invite a user by email,
//! share with one of your teams or create a link for guests without an
//! account. Each share says how far guests can go (only watch, or ask for
//! the keyboard), when it expires, whether you let people in yourself and
//! whether the keyboard is handed over without asking (and for how long at
//! most). The shares in use
//! are listed below: they can be changed live or revoked, and "Stop
//! sharing" revokes them all. It works for the terminals of this computer
//! (relay) and for server sessions.

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, IntoElement, ParentElement,
    Render, SharedString, Styled, Subscription, WeakEntity, Window, div, prelude::FluentBuilder,
    px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::select::{Select, SelectEvent};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::Value;
use termoak_client::ApiClient;
use termoak_core::Id;

use crate::runtime;
use crate::sharing::{Expiry, ShareChange, ShareForm, ShareRow, Target};
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
    window.open_dialog(cx, move |d, _, cx| {
        let can_stop = dialog.read(cx).can_stop();
        let stop = dialog.downgrade();
        d.title(t!("share.title"))
            .w(px(600.))
            .child(dialog.clone())
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .when(can_stop, |this| {
                        this.child(
                            Button::new("share-stop-all")
                                .danger()
                                .icon(ui::icon(IconName::CircleStop))
                                .label(t!("share.stop_all"))
                                .on_click(move |_: &ClickEvent, window, cx| {
                                    if let Some(d) = stop.upgrade() {
                                        d.update(cx, |d, cx| d.stop_all(window, cx));
                                    }
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("share-close")
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
            )
    });
}

/// Who the next share is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    User,
    Team,
    Link,
}

/// Content of the share dialog.
pub struct ShareDialog {
    model: Entity<AppModel>,
    api: ApiClient,
    session_id: Id,
    terminal: Option<WeakEntity<TerminalView>>,
    is_relay: bool,
    mode: Mode,
    email: Entity<InputState>,
    team: ChoiceState<Id>,
    /// Teams in the dropdown (to rebuild it if they change).
    team_ids: Vec<Id>,
    control: bool,
    expiry: ChoiceState<Expiry>,
    require_approval: bool,
    /// The user changed "Ask me before letting people in" (its default
    /// follows the kind of share until then).
    approval_touched: bool,
    auto_grant: bool,
    /// Limit of the automatic grants (minutes).
    control_limit: ChoiceState<Option<u32>>,
    busy: bool,
    /// Link just created: web link and app link (shown only now).
    created: Option<(String, String)>,
    shares: Vec<ShareRow>,
    loading: bool,
    load_error: Option<String>,
    /// Share being edited, its expiry dropdown (`None`: unchanged) and the
    /// limit of its automatic grants.
    editing: Option<(Id, ChoiceState<Option<Expiry>>, ChoiceState<Option<u32>>)>,
    _subs: Vec<Subscription>,
    _edit_subs: Vec<Subscription>,
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

fn expiry_choices() -> Vec<Choice<Expiry>> {
    Expiry::ALL
        .iter()
        .map(|e| Choice::new(e.label(), *e))
        .collect()
}

/// Limits of the automatic grants (`None`: no limit).
fn control_limit_choices() -> Vec<Choice<Option<u32>>> {
    std::iter::once(Choice::new(t!("share.control_limit_none"), None))
        .chain(
            crate::sharing::CONTROL_MINUTES
                .iter()
                .map(|&m| Choice::new(tn!("share.control_limit_minutes", m), Some(m))),
        )
        .collect()
}

/// Text of a share's expiry.
fn expiry_text(at: Option<i64>) -> SharedString {
    match at {
        Some(ms) => t!("share.row.expires", date = ui::format_ms(ms)),
        None => t!("share.row.no_expiry"),
    }
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
        let expiry = ui::choice_state(expiry_choices(), Some(&Expiry::Never), window, cx);
        let control_limit = ui::choice_state(control_limit_choices(), Some(&None), window, cx);
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
        let mut this = Self {
            model,
            api,
            session_id,
            terminal,
            is_relay,
            mode: Mode::User,
            email,
            team,
            team_ids,
            control: false,
            expiry,
            require_approval: false,
            approval_touched: false,
            auto_grant: false,
            control_limit,
            busy: false,
            created: None,
            shares: Vec::new(),
            loading: false,
            load_error: None,
            editing: None,
            _subs: vec![sub],
            _edit_subs: Vec::new(),
        };
        this.load(window, cx);
        this
    }

    /// "Stop sharing" makes sense: there are shares, or this computer's
    /// terminal is being shared.
    fn can_stop(&self) -> bool {
        !self.shares.is_empty() || (self.is_relay && self.terminal.is_some())
    }

    fn shares_path(&self) -> String {
        format!("/api/v1/sessions/{}/shares", self.session_id)
    }

    /// Reads the shares in use.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.loading = true;
        let api = self.api.clone();
        let path = self.shares_path();
        runtime::run_in(
            cx,
            window,
            async move { api.get::<Value>(&path).await.map_err(api_error) },
            |this, res, _, cx| {
                this.loading = false;
                match res {
                    Ok(v) => {
                        this.shares = ShareRow::active_list(&v);
                        this.load_error = None;
                        if let Some((id, _, _)) = &this.editing
                            && !this.shares.iter().any(|s| s.id == *id)
                        {
                            this.editing = None;
                            this._edit_subs.clear();
                        }
                    }
                    Err(e) => this.load_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn set_mode(&mut self, mode: Mode, cx: &mut Context<Self>) {
        self.mode = mode;
        if !self.approval_touched {
            self.require_approval = crate::sharing::approval_default(&self.target(mode));
        }
        cx.notify();
    }

    /// The form as it is now.
    fn form(&self, cx: &App) -> ShareForm {
        let target = match self.mode {
            Mode::User => Target::User(self.email.read(cx).value().to_string()),
            Mode::Team => Target::Team(ui::chosen(&self.team, cx)),
            Mode::Link => Target::Link,
        };
        ShareForm {
            control: self.control,
            expiry: ui::chosen(&self.expiry, cx).unwrap_or(Expiry::Never),
            require_approval: self.require_approval,
            auto_grant: self.auto_grant,
            control_minutes: ui::chosen(&self.control_limit, cx).flatten(),
            ..ShareForm::new(target)
        }
    }

    /// Target of a mode, only to know its defaults.
    fn target(&self, mode: Mode) -> Target {
        match mode {
            Mode::User => Target::User(String::new()),
            Mode::Team => Target::Team(None),
            Mode::Link => Target::Link,
        }
    }

    /// Creates the share.
    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.form(cx);
        let body = match form.body() {
            Ok(b) => b,
            Err(e) => {
                ui::error(window, cx, e.message());
                return;
            }
        };
        self.busy = true;
        cx.notify();
        let api = self.api.clone();
        let path = self.shares_path();
        let server = api.base_url().to_string();
        runtime::run_in(
            cx,
            window,
            async move { api.post::<Value>(&path, &body).await.map_err(api_error) },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(v) => {
                        match &form.target {
                            Target::User(email) => {
                                this.email.update(cx, |i, cx| i.set_value("", window, cx));
                                ui::success(window, cx, t!("share.invited", email = email.trim()));
                            }
                            Target::Team(team) => {
                                let name = this
                                    .model
                                    .read(cx)
                                    .teams
                                    .iter()
                                    .find(|t| Some(t.id) == *team)
                                    .map(|t| t.name.clone())
                                    .unwrap_or_else(|| t!("share.team_fallback").to_string());
                                ui::success(window, cx, t!("share.shared_with_team", name = name));
                            }
                            Target::Link => {
                                if let Some((web, app)) = crate::sharing::created_links(&v, &server)
                                {
                                    cx.write_to_clipboard(ClipboardItem::new_string(web.clone()));
                                    ui::success(window, cx, t!("share.link_copied"));
                                    this.created = Some((web, app));
                                }
                            }
                        }
                        this.load(window, cx);
                    }
                    Err(e) => ui::error(window, cx, t!("share.error", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn start_edit(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.as_ref().is_some_and(|(e, _, _)| *e == id) {
            self.editing = None;
            self._edit_subs.clear();
            cx.notify();
            return;
        }
        let mut choices = vec![Choice::new(t!("share.expiry.keep"), None)];
        choices.extend(Expiry::ALL.iter().map(|e| Choice::new(e.label(), Some(*e))));
        let state = ui::choice_state(choices, Some(&None), window, cx);
        let current = self
            .shares
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.control_minutes);
        let limit = ui::choice_state(control_limit_choices(), Some(&current), window, cx);
        self._edit_subs = vec![
            cx.subscribe_in(
                &state,
                window,
                move |this, _, ev: &SelectEvent<Vec<Choice<Option<Expiry>>>>, window, cx| {
                    if let SelectEvent::Confirm(Some(Some(e))) = ev {
                        this.change(id, ShareChange::Expiry(*e), window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &limit,
                window,
                move |this, _, ev: &SelectEvent<Vec<Choice<Option<u32>>>>, window, cx| {
                    if let SelectEvent::Confirm(Some(m)) = ev {
                        this.change(id, ShareChange::ControlMinutes(*m), window, cx);
                    }
                },
            ),
        ];
        self.editing = Some((id, state, limit));
        cx.notify();
    }

    /// Changes a share live.
    fn change(&mut self, id: Id, change: ShareChange, window: &mut Window, cx: &mut Context<Self>) {
        // Shown at once.
        if let Some(row) = self.shares.iter_mut().find(|r| r.id == id) {
            match change {
                ShareChange::Control(c) => {
                    row.control = c;
                    if !c {
                        row.auto_grant = false;
                    }
                }
                ShareChange::RequireApproval(r) => row.require_approval = r,
                ShareChange::AutoGrant(a) => row.auto_grant = a,
                ShareChange::ControlMinutes(m) => row.control_minutes = m,
                ShareChange::Expiry(_) => {}
            }
        }
        cx.notify();
        let api = self.api.clone();
        let path = format!("{}/{id}", self.shares_path());
        let body = change.body();
        runtime::run_in(
            cx,
            window,
            async move { api.patch::<Value>(&path, &body).await.map_err(api_error) },
            move |this, res, window, cx| {
                match res {
                    Ok(v) => match ShareRow::from_json(&v) {
                        Some(row) if row.active => {
                            if let Some(r) = this.shares.iter_mut().find(|r| r.id == id) {
                                *r = row;
                            }
                        }
                        _ => this.shares.retain(|r| r.id != id),
                    },
                    Err(e) => {
                        ui::error(window, cx, t!("share.change_failed", error = e));
                        this.load(window, cx);
                    }
                }
                cx.notify();
            },
        );
    }

    fn revoke(&mut self, row: ShareRow, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("share.revoke_title"),
            t!("share.revoke_message", who = row.label()),
            t!("share.revoke"),
            true,
            move |window, cx| {
                let Some(this) = weak.upgrade() else { return };
                let id = row.id;
                this.update(cx, |this, cx| {
                    this.shares.retain(|r| r.id != id);
                    let api = this.api.clone();
                    let path = format!("{}/{id}", this.shares_path());
                    runtime::run_in(
                        cx,
                        window,
                        async move { api.delete(&path).await.map_err(api_error) },
                        |this, res, window, cx| {
                            if let Err(e) = res {
                                ui::error(window, cx, t!("share.revoke_failed", error = e));
                            }
                            this.load(window, cx);
                        },
                    );
                    cx.notify();
                });
            },
        );
    }

    /// Revokes every share and sends everyone away (and, for a terminal of
    /// this computer, stops sharing it).
    fn stop_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("share.stop_all_title"),
            t!("share.stop_all_message"),
            t!("share.stop_all"),
            true,
            move |window, cx| {
                let Some(this) = weak.upgrade() else { return };
                this.update(cx, |this, cx| {
                    let api = this.api.clone();
                    let path = this.shares_path();
                    runtime::run_in(
                        cx,
                        window,
                        async move { api.delete(&path).await.map_err(api_error) },
                        |this, res, window, cx| {
                            if let Err(e) = res {
                                ui::error(window, cx, t!("share.revoke_failed", error = e));
                                this.load(window, cx);
                                return;
                            }
                            if this.is_relay
                                && let Some(t) = this.terminal.as_ref().and_then(|t| t.upgrade())
                            {
                                t.update(cx, |t, cx| {
                                    t.stop_sharing(cx);
                                    cx.notify();
                                });
                            }
                            window.close_dialog(cx);
                            ui::notify(window, cx, ToastKind::Info, t!("share.stopped"));
                        },
                    );
                });
            },
        );
    }

    fn render_form(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let mode = self.mode;
        let control = self.control;
        let has_teams = !self.team_ids.is_empty();
        let mode_button = |id: &'static str, m: Mode, label: SharedString, icon: IconName| {
            Button::new(id)
                .small()
                .icon(ui::icon(icon))
                .label(label)
                .map(|b| if mode == m { b.primary() } else { b.ghost() })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.set_mode(m, cx)))
        };
        let target: gpui::AnyElement = match mode {
            Mode::User => {
                ui::field(t!("share.invite_user"), Input::new(&self.email), cx).into_any_element()
            }
            Mode::Team if has_teams => {
                ui::field(t!("share.share_with_team"), Select::new(&self.team), cx)
                    .into_any_element()
            }
            Mode::Team => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("share.no_teams"))
                .into_any_element(),
            Mode::Link => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("share.link_hint"))
                .into_any_element(),
        };
        let create_label = match mode {
            Mode::User => t!("share.invite"),
            Mode::Team => t!("share.share"),
            Mode::Link => t!("share.create_link"),
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_1()
                    .child(mode_button(
                        "mode-user",
                        Mode::User,
                        t!("share.mode.user"),
                        IconName::Mail,
                    ))
                    .child(mode_button(
                        "mode-team",
                        Mode::Team,
                        t!("share.mode.team"),
                        IconName::Users,
                    ))
                    .child(mode_button(
                        "mode-link",
                        Mode::Link,
                        t!("share.mode.link"),
                        IconName::Link,
                    )),
            )
            .child(target)
            .child(
                h_flex()
                    .gap_3()
                    .items_end()
                    .child(ui::field(
                        t!("share.permission"),
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("perm-view")
                                    .small()
                                    .icon(ui::icon(IconName::Eye))
                                    .label(t!("share.view_only"))
                                    .map(|b| if !control { b.primary() } else { b.ghost() })
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.control = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("perm-control")
                                    .small()
                                    .icon(ui::icon(IconName::Keyboard))
                                    .label(t!("share.can_request_control"))
                                    .map(|b| if control { b.primary() } else { b.ghost() })
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.control = true;
                                        cx.notify();
                                    })),
                            ),
                        cx,
                    ))
                    .child(div().w(px(170.)).child(ui::field(
                        t!("share.expires"),
                        Select::new(&self.expiry).small(),
                        cx,
                    ))),
            )
            .child(
                Checkbox::new("share-approval")
                    .label(t!("share.ask_before_join"))
                    .checked(self.require_approval)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.require_approval = *v;
                        this.approval_touched = true;
                        cx.notify();
                    })),
            )
            .child(
                Checkbox::new("share-auto-grant")
                    .label(t!("share.auto_grant"))
                    .checked(control && self.auto_grant)
                    .disabled(!control)
                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                        this.auto_grant = *v;
                        cx.notify();
                    })),
            )
            .when(control && self.auto_grant, |this| {
                this.child(control_limit_row(&self.control_limit, false, cx))
            })
            .child(
                h_flex().justify_end().child(
                    Button::new("share-create")
                        .primary()
                        .icon(ui::icon(match mode {
                            Mode::User => IconName::Mail,
                            Mode::Team => IconName::Users,
                            Mode::Link => IconName::Link,
                        }))
                        .label(create_label)
                        .loading(self.busy)
                        .disabled(mode == Mode::Team && !has_teams)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.create(window, cx)),
                        ),
                ),
            )
            .into_any_element()
    }

    fn render_created(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (web, app) = self.created.clone()?;
        let theme = cx.theme();
        let line = |id: &'static str, label: SharedString, value: String| {
            let copy = value.clone();
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .w(px(70.))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .p_1p5()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .font_family(ui::mono_family(cx))
                        .text_xs()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(value),
                )
                .child(
                    Button::new(id)
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::Copy))
                        .tooltip(t!("share.copy"))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                            ui::success(window, cx, t!("share.copied"));
                        }),
                )
        };
        Some(
            v_flex()
                .gap_2()
                .p_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.success)
                .child(
                    div()
                        .text_xs()
                        .font_medium()
                        .text_color(theme.success)
                        .child(t!("share.link_once")),
                )
                .child(line("copy-web", t!("share.web_link"), web))
                .when(!app.is_empty(), |this| {
                    this.child(line("copy-app", t!("share.app_link"), app))
                })
                .into_any_element(),
        )
    }

    fn render_shares(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_sm().font_semibold().child(t!("share.active")))
            .when(self.loading, |this| this.child(Spinner::new().small()));
        let body: gpui::AnyElement = if let Some(e) = &self.load_error {
            div()
                .text_xs()
                .text_color(theme.danger)
                .child(t!("share.load_failed", error = e.clone()))
                .into_any_element()
        } else if self.shares.is_empty() && !self.loading {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(t!("share.none_active"))
                .into_any_element()
        } else {
            v_flex()
                .gap_2()
                .children(
                    self.shares
                        .iter()
                        .enumerate()
                        .map(|(i, r)| self.render_row(i, r, cx)),
                )
                .into_any_element()
        };
        v_flex()
            .gap_2()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_row(&self, i: usize, r: &ShareRow, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let editing = self
            .editing
            .as_ref()
            .filter(|(id, _, _)| *id == r.id)
            .map(|(_, s, l)| (s.clone(), l.clone()));
        let id = r.id;
        let row = r.clone();
        let mut facts = vec![
            if r.control {
                t!("share.can_request_control")
            } else {
                t!("share.view_only")
            },
            expiry_text(r.expires_at),
        ];
        if r.require_approval {
            facts.push(t!("share.row.asks"));
        }
        if r.auto_grant {
            facts.push(t!("share.row.auto_grant"));
            if let Some(m) = r.control_minutes {
                facts.push(tn!("share.row.control_minutes", m));
            }
        }
        if r.participants > 0 {
            facts.push(tn!("share.row.inside", r.participants));
        }
        let facts: Vec<String> = facts.into_iter().map(|f| f.to_string()).collect();
        let icon = match r.who {
            crate::sharing::ShareWho::User { .. } => IconName::User,
            crate::sharing::ShareWho::Team(_) => IconName::Users,
            crate::sharing::ShareWho::Link => IconName::Link,
        };
        v_flex()
            .p_2()
            .gap_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(icon)
                            .size(px(16.))
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(r.label()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(facts.join(" · ")),
                            ),
                    )
                    .child(
                        Button::new(("share-edit", i))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::Pencil))
                            .tooltip(t!("share.edit"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.start_edit(id, window, cx)
                            })),
                    )
                    .child(
                        Button::new(("share-revoke", i))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::X))
                            .tooltip(t!("share.revoke"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.revoke(row.clone(), window, cx)
                            })),
                    ),
            )
            .when_some(editing, |this, (expiry, limit)| {
                let control = r.control;
                let auto_grant = r.auto_grant;
                this.child(
                    v_flex()
                        .gap_2()
                        .pl(px(24.))
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new(("edit-view", i))
                                        .xsmall()
                                        .label(t!("share.view_only"))
                                        .map(|b| if !control { b.primary() } else { b.ghost() })
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.change(
                                                    id,
                                                    ShareChange::Control(false),
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )),
                                )
                                .child(
                                    Button::new(("edit-control", i))
                                        .xsmall()
                                        .label(t!("share.can_request_control"))
                                        .map(|b| if control { b.primary() } else { b.ghost() })
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.change(
                                                    id,
                                                    ShareChange::Control(true),
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("share.new_expiry")),
                                )
                                .child(div().w(px(170.)).child(Select::new(&expiry).xsmall())),
                        )
                        .child(
                            Checkbox::new(("edit-approval", i))
                                .label(t!("share.ask_before_join"))
                                .checked(r.require_approval)
                                .on_click(cx.listener(move |this, v: &bool, window, cx| {
                                    this.change(id, ShareChange::RequireApproval(*v), window, cx)
                                })),
                        )
                        .child(
                            Checkbox::new(("edit-auto-grant", i))
                                .label(t!("share.auto_grant"))
                                .checked(r.auto_grant)
                                .disabled(!control)
                                .on_click(cx.listener(move |this, v: &bool, window, cx| {
                                    this.change(id, ShareChange::AutoGrant(*v), window, cx)
                                })),
                        )
                        .when(control && auto_grant, |this| {
                            this.child(control_limit_row(&limit, true, cx))
                        }),
                )
            })
            .into_any_element()
    }
}

/// "Limit automatic control to [N minutes]", under "Give control
/// automatically".
fn control_limit_row(state: &ChoiceState<Option<u32>>, compact: bool, cx: &App) -> gpui::Div {
    h_flex()
        .gap_2()
        .items_center()
        .pl(px(24.))
        .child(
            div()
                .text_sm()
                .when(compact, |d| d.text_xs())
                .text_color(cx.theme().muted_foreground)
                .child(t!("share.control_limit")),
        )
        .child(div().w(px(150.)).child(if compact {
            Select::new(state).xsmall()
        } else {
            Select::new(state).small()
        }))
}

impl Render for ShareDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = if self.is_relay {
            t!("share.relay_hint")
        } else {
            t!("share.server_hint")
        };
        let form = self.render_form(cx);
        let created = self.render_created(cx);
        let shares = self.render_shares(cx);
        let theme_border = cx.theme().border;
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
            .child(form)
            .children(created)
            .child(div().h(px(1.)).bg(theme_border))
            .child(shares)
    }
}
