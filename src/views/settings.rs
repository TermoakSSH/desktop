//! Settings: appearance, language, terminal (with autocomplete), account and
//! sync with the server (with two-step verification, invitations and the
//! email verification code) and updates. The AI has its own page (where it runs, API keys, AI credit).

use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, IntoElement, ParentElement, Render,
    Styled, Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::{Select, SelectEvent};
use gpui_component::switch::Switch;
use gpui_component::tab::{Tab, TabBar};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use serde_json::Value;
use termoak_client::ApiClient;

use super::OpenRequest;
use super::ai_settings::AiSettingsView;
use super::two_factor::TwoFactorPanel;
use crate::i18n;
use crate::runtime;
use crate::state::{
    AppModel, LoginError, LoginOutcome, LoginRequest, PendingVerification, ToastKind, api_error,
    clean_email_code,
};
use crate::theme;
use crate::ui::{self, IconName};
use crate::update::{self, UpdateModel, UpdateStatus};

/// Wait after typing the invitation code before checking it.
const INVITE_CHECK_DELAY: Duration = Duration::from_millis(500);

/// Result of checking an invitation code.
#[derive(Clone)]
enum InviteCheck {
    Checking,
    Valid(String),
    Invalid(String),
}

/// Pages of the settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    General,
    Ai,
}

pub struct SettingsView {
    model: Entity<AppModel>,
    page: SettingsPage,
    /// Settings → AI.
    ai: Entity<AiSettingsView>,
    updates: Entity<UpdateModel>,
    server_url: Entity<InputState>,
    email: Entity<InputState>,
    password: Entity<InputState>,
    name: Entity<InputState>,
    /// Two-step verification code (or recovery code).
    totp: Entity<InputState>,
    /// Invitation code (optional) when creating the account.
    invite: Entity<InputState>,
    /// Six-digit code from the verification email.
    email_code: Entity<InputState>,
    font_family: Entity<InputState>,
    /// Interface language (`None` = the system language).
    language: ui::ChoiceState<Option<String>>,
    two_factor: Entity<TwoFactorPanel>,
    register: bool,
    /// The account asked for the two-step verification code.
    totp_step: bool,
    /// Verifying the email also asked for the two-step verification code.
    verify_totp: bool,
    busy: bool,
    /// Asking for another verification email.
    resending: bool,
    /// The "Resend in N s" countdown is ticking.
    countdown: bool,
    invite_check: Option<InviteCheck>,
    _invite_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for SettingsView {}

/// Data of a `termoak://invite?server=...&token=...` link.
pub fn parse_invite_link(text: &str) -> Option<(String, String)> {
    let url = url::Url::parse(text.trim()).ok()?;
    // `aceitunoak://` is the scheme used before the rename to Termoak.
    if !matches!(url.scheme(), "termoak" | "aceitunoak") {
        return None;
    }
    let mut server = None;
    let mut token = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "server" => server = Some(v.trim().to_string()),
            "token" => token = Some(v.trim().to_string()),
            _ => {}
        }
    }
    // Link half pasted or half typed: not yet.
    Some((
        server.filter(|s| !s.is_empty())?,
        token.filter(|t| !t.is_empty())?,
    ))
}

/// Text for a valid invitation (`GET /api/v1/invites/{code}`).
pub fn describe_invite(v: &Value) -> String {
    let mut parts = Vec::new();
    match v["team"].as_str() {
        Some(team) => parts.push(t!("settings.invite.join_team", team = team).to_string()),
        None => parts.push(t!("settings.invite.valid").to_string()),
    }
    if let Some(email) = v["email"].as_str() {
        parts.push(t!("settings.invite.only_for", email = email).to_string());
    }
    if let Some(exp) = v["expires_at"].as_i64() {
        parts.push(t!("settings.invite.expires", date = ui::format_ms(exp)).to_string());
    }
    parts.join(" ")
}

/// Choices of the language selector: the system language and every
/// available translation, by its own name.
fn language_items() -> Vec<ui::Choice<Option<String>>> {
    let system = i18n::system().unwrap_or(i18n::DEFAULT);
    let mut items = vec![ui::Choice::new(
        t!(
            "settings.language.system",
            language = i18n::language_name(system)
        ),
        None,
    )];
    items.extend(
        i18n::available()
            .into_iter()
            .map(|l| ui::Choice::new(i18n::language_name(l), Some(l.to_string()))),
    );
    items
}

impl SettingsView {
    pub fn new(
        model: Entity<AppModel>,
        updates: Entity<UpdateModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let m = model.read(cx);
        let url = m.server_url.clone().unwrap_or_default();
        let user = m.server_user.clone().unwrap_or_default();
        let family = m.settings.font_family.clone();
        let language_choice = m.settings.language.clone();
        let server_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://ssh.example.com")
                .default_value(url)
        });
        let email = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.placeholder.email"))
                .default_value(user)
        });
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("settings.placeholder.password"))
        });
        let name =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("settings.placeholder.name")));
        let totp = cx.new(|cx| InputState::new(window, cx).placeholder("123456"));
        let invite =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("settings.placeholder.invite")));
        let email_code = cx.new(|cx| InputState::new(window, cx).placeholder("123456"));
        let font_family = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("settings.placeholder.font_family"))
                .default_value(family)
        });
        let language = ui::choice_state(language_items(), Some(&language_choice), window, cx);
        let two_factor = cx.new(|cx| TwoFactorPanel::new(model.clone(), window, cx));
        let ai = cx.new(|cx| AiSettingsView::new(model.clone(), window, cx));
        let subs = vec![
            cx.subscribe_in(
                &language,
                window,
                |this, _, ev: &SelectEvent<Vec<ui::Choice<Option<String>>>>, window, cx| {
                    let SelectEvent::Confirm(Some(choice)) = ev else {
                        return;
                    };
                    this.set_language(choice.clone(), window, cx);
                },
            ),
            cx.observe_in(&model, window, |this, model, window, cx| {
                // Fills in the URL and email when the saved session is restored.
                let m = model.read(cx);
                let (url, user) = (m.server_url.clone(), m.server_user.clone());
                if let Some(url) = url
                    && this.server_url.read(cx).value().is_empty()
                {
                    this.server_url
                        .update(cx, |i, cx| i.set_value(url, window, cx));
                }
                if let Some(user) = user
                    && this.email.read(cx).value().is_empty()
                {
                    this.email.update(cx, |i, cx| i.set_value(user, window, cx));
                }
                cx.notify();
            }),
            cx.observe(&updates, |_, _, cx| cx.notify()),
            cx.observe(&two_factor, |_, _, cx| cx.notify()),
            // Enter in the password or the code signs in.
            cx.subscribe_in(&password, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.login(window, cx);
                }
            }),
            cx.subscribe_in(&totp, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    if this.model.read(cx).pending_verification.is_some() {
                        this.verify_email(window, cx);
                    } else {
                        this.login(window, cx);
                    }
                }
            }),
            // The email code keeps only its digits (pasted as "123 456" or
            // "123-456") and is checked as soon as it is complete.
            cx.subscribe_in(
                &email_code,
                window,
                |this, input, ev: &InputEvent, window, cx| match ev {
                    InputEvent::Change => {
                        let value = input.read(cx).value().to_string();
                        let code = clean_email_code(&value);
                        if code != value {
                            input.update(cx, |i, cx| i.set_value(code.clone(), window, cx));
                        }
                        if code.len() == 6 && !this.verify_totp {
                            this.verify_email(window, cx);
                        }
                    }
                    InputEvent::PressEnter { .. } => this.verify_email(window, cx),
                    _ => {}
                },
            ),
            // Another email or server: the previous code no longer applies.
            cx.subscribe_in(&email, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::Change = ev {
                    this.cancel_totp(window, cx);
                }
            }),
            cx.subscribe_in(
                &server_url,
                window,
                |this, _, ev: &InputEvent, window, cx| {
                    if let InputEvent::Change = ev {
                        this.cancel_totp(window, cx);
                        this.schedule_invite_check(window, cx);
                    }
                },
            ),
            cx.subscribe_in(&invite, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::Change = ev {
                    this.on_invite_changed(window, cx);
                }
            }),
        ];
        Self {
            model,
            page: SettingsPage::General,
            ai,
            updates,
            server_url,
            email,
            password,
            name,
            totp,
            invite,
            email_code,
            font_family,
            language,
            two_factor,
            register: false,
            totp_step: false,
            verify_totp: false,
            busy: false,
            resending: false,
            countdown: false,
            invite_check: None,
            _invite_task: None,
            _subs: subs,
        }
    }

    /// Reloads what depends on the server (when the section is entered).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == SettingsPage::Ai {
            self.ai.update(cx, |v, cx| v.refresh(window, cx));
        }
    }

    /// Shows a page (the AI one reloads its data when it appears).
    pub fn show_page(&mut self, page: SettingsPage, window: &mut Window, cx: &mut Context<Self>) {
        if self.page == page {
            return;
        }
        self.page = page;
        self.refresh(window, cx);
        cx.notify();
    }

    /// A `termoak://invite?...` link opened from outside the app: the sign
    /// up form, filled in.
    pub fn open_invite_link(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((server, token)) = parse_invite_link(link) else {
            return;
        };
        self.show_page(SettingsPage::General, window, cx);
        if self.model.read(cx).logged_in() {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("settings.invite.already_signed_in"),
            );
            return;
        }
        self.register = true;
        self.server_url
            .update(cx, |i, cx| i.set_value(server, window, cx));
        self.invite
            .update(cx, |i, cx| i.set_value(token, window, cx));
        self.schedule_invite_check(window, cx);
        cx.notify();
    }

    fn set_dark(&mut self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        theme::apply(dark, Some(window), cx);
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.dark = dark;
            m.save_settings(s, cx);
        });
    }

    fn set_language(
        &mut self,
        choice: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.model.read(cx).settings.language == choice {
            return;
        }
        self.model
            .update(cx, |m, cx| m.set_language(choice.clone(), cx));
        // "System default" names the system language in the new language.
        self.language.update(cx, |state, cx| {
            state.set_items(language_items(), window, cx);
            state.set_selected_value(&choice, window, cx);
        });
    }

    fn change_font_size(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.font_size = (s.font_size + delta).clamp(9., 28.);
            m.save_settings(s, cx);
        });
    }

    fn apply_font_family(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let family = self.font_family.read(cx).value().trim().to_string();
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.font_family = family;
            m.save_settings(s, cx);
        });
        ui::success(window, cx, t!("settings.font.updated"));
    }

    fn set_agent(&mut self, on: bool, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.use_agent = on;
            m.save_settings(s, cx);
        });
    }

    fn set_autocomplete(&mut self, on: bool, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.autocomplete = on;
            m.save_settings(s, cx);
        });
    }

    /// Changes the copy and paste preferences.
    fn set_paste(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut crate::state::Settings)) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            f(&mut s);
            m.save_settings(s, cx);
        });
    }

    fn render_paste(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.read(cx).settings.clone();
        let muted = cx.theme().muted_foreground;
        let hint = |text: gpui::SharedString| div().text_xs().text_color(muted).child(text);
        let mac = cfg!(target_os = "macos");
        let right_click = settings.right_click;
        self.render_card(t!("settings.paste.title"), IconName::ClipboardPaste, cx)
            .child(hint(if mac {
                t!("settings.paste.shortcuts_macos")
            } else {
                t!("settings.paste.shortcuts")
            }))
            .when(!mac, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Switch::new("ctrl-v-pastes")
                                .label(t!("settings.paste.ctrl_v"))
                                .checked(settings.ctrl_v_pastes)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    let v = *v;
                                    this.set_paste(cx, |s| s.ctrl_v_pastes = v)
                                })),
                        )
                        .child(hint(t!("settings.paste.ctrl_v_hint"))),
                )
            })
            .child(ui::field_with_hint(
                t!("settings.paste.right_click"),
                h_flex().gap_2().flex_wrap().children(
                    crate::terminal::paste::RightClick::ALL
                        .into_iter()
                        .enumerate()
                        .map(|(i, choice)| {
                            Button::new(("right-click", i))
                                .small()
                                .label(match choice {
                                    crate::terminal::paste::RightClick::Menu => {
                                        t!("settings.paste.right_click_menu")
                                    }
                                    crate::terminal::paste::RightClick::Paste => {
                                        t!("settings.paste.right_click_paste")
                                    }
                                    crate::terminal::paste::RightClick::CopyOrPaste => {
                                        t!("settings.paste.right_click_copy_paste")
                                    }
                                })
                                .when(right_click == choice, |b| b.primary())
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.set_paste(cx, |s| s.right_click = choice)
                                }))
                        }),
                ),
                t!("settings.paste.right_click_hint"),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("copy-on-select")
                            .label(t!("settings.paste.copy_on_select"))
                            .checked(settings.copy_on_select)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                let v = *v;
                                this.set_paste(cx, |s| s.copy_on_select = v)
                            })),
                    )
                    .child(hint(t!("settings.paste.copy_on_select_hint"))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("confirm-multiline")
                            .label(t!("settings.paste.confirm_multiline"))
                            .checked(settings.confirm_multiline_paste)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                let v = *v;
                                this.set_paste(cx, |s| s.confirm_multiline_paste = v)
                            })),
                    )
                    .child(hint(t!("settings.paste.confirm_multiline_hint"))),
            )
    }

    fn clear_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ws = self.model.read(cx).ws.clone();
        ui::confirm(
            window,
            cx,
            t!("settings.history.clear_title"),
            t!("settings.history.clear_message"),
            t!("common.delete"),
            true,
            move |window, cx| {
                let ws = ws.clone();
                let task = runtime::spawn(cx, async move { ws.clear_history(None).await });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(()) => ui::success(window, cx, t!("settings.history.cleared")),
                            Err(e) => ui::error(
                                window,
                                cx,
                                t!("settings.history.clear_failed", error = e),
                            ),
                        });
                    })
                    .detach();
            },
        );
    }

    fn server_url_value(&self, cx: &Context<Self>) -> String {
        let url = self.server_url.read(cx).value().trim().to_string();
        if url.is_empty() || url.starts_with("http://") || url.starts_with("https://") {
            url
        } else {
            format!("https://{url}")
        }
    }

    /// Goes back to the form without a code (another email, server...).
    fn cancel_totp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.totp_step {
            self.totp_step = false;
            self.totp.update(cx, |i, cx| i.set_value("", window, cx));
            cx.notify();
        }
    }

    /// A pasted `termoak://invite?...` link is split into the server URL and
    /// the code. Then the code is checked.
    fn on_invite_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.invite.read(cx).value().to_string();
        if let Some((server, token)) = parse_invite_link(&value) {
            self.server_url
                .update(cx, |i, cx| i.set_value(server, window, cx));
            self.invite
                .update(cx, |i, cx| i.set_value(token, window, cx));
        }
        self.schedule_invite_check(window, cx);
    }

    fn schedule_invite_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.invite.read(cx).value().trim().to_string();
        let url = self.server_url_value(cx);
        if !self.register || token.is_empty() || url.is_empty() {
            self.invite_check = None;
            self._invite_task = None;
            cx.notify();
            return;
        }
        self.invite_check = Some(InviteCheck::Checking);
        cx.notify();
        self._invite_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(INVITE_CHECK_DELAY).await;
            let Ok(rt) = this.update(cx, |_, cx| runtime::handle(cx)) else {
                return;
            };
            let res = rt
                .spawn(async move {
                    let api = ApiClient::new(&url).map_err(api_error)?;
                    api.invite_info(&token).await.map_err(api_error)
                })
                .await
                .unwrap_or_else(|e| Err(t!("common.task_interrupted", error = e).to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.invite_check = Some(match res {
                    Ok(v) => {
                        // If the invitation is for an email address, fill it in.
                        if let Some(email) = v["email"].as_str()
                            && this.email.read(cx).value().trim().is_empty()
                        {
                            let email = email.to_string();
                            this.email
                                .update(cx, |i, cx| i.set_value(email, window, cx));
                        }
                        InviteCheck::Valid(describe_invite(&v))
                    }
                    Err(e) => InviteCheck::Invalid(ui::capitalize(&e)),
                });
                cx.notify();
            });
        }));
    }

    fn login(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let url = self.server_url_value(cx);
        let email = self.email.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        let name = self.name.read(cx).value().trim().to_string();
        let code: String = self
            .totp
            .read(cx)
            .value()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let invite = self.invite.read(cx).value().trim().to_string();
        if url.is_empty() || email.is_empty() || password.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_fields"));
            return;
        }
        if self.register && name.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_name"));
            return;
        }
        let request = if self.register {
            LoginRequest::Register {
                name,
                invite: Some(invite).filter(|i| !i.is_empty()),
            }
        } else {
            if self.totp_step && code.is_empty() {
                ui::error(window, cx, t!("settings.login.missing_code"));
                return;
            }
            LoginRequest::Login {
                totp: self.totp_step.then_some(code),
            }
        };
        let register = self.register;
        self.busy = true;
        cx.notify();
        let task = self
            .model
            .update(cx, |m, cx| m.login(url, email, password, request, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match res {
                    Ok(LoginOutcome::VerifyEmail) => {
                        // The password stays in case "Use a different email"
                        // brings the form back.
                        this.totp_step = false;
                        this.verify_totp = false;
                        for input in [&this.totp, &this.email_code] {
                            input.update(cx, |i, cx| i.set_value("", window, cx));
                        }
                        ui::focus_later(&this.email_code, window, cx);
                        this.start_countdown(cx);
                    }
                    Ok(LoginOutcome::SignedIn) => {
                        // After signing out, "Sign in" is offered again.
                        this.register = false;
                        this.totp_step = false;
                        for input in [&this.password, &this.totp, &this.invite] {
                            input.update(cx, |i, cx| i.set_value("", window, cx));
                        }
                        this.invite_check = None;
                    }
                    Err(LoginError::TotpRequired) => {
                        this.totp_step = true;
                        ui::focus_later(&this.totp, window, cx);
                        ui::notify(
                            window,
                            cx,
                            crate::state::ToastKind::Info,
                            t!("settings.login.totp_required"),
                        );
                    }
                    Err(LoginError::TotpInvalid) => {
                        this.totp_step = true;
                        this.totp.update(cx, |i, cx| i.set_value("", window, cx));
                        ui::focus_later(&this.totp, window, cx);
                        ui::error(window, cx, t!("settings.login.totp_invalid"));
                    }
                    Err(LoginError::Failed(e)) => ui::error(
                        window,
                        cx,
                        if register {
                            t!("settings.login.register_failed", error = e)
                        } else {
                            t!("settings.login.failed", error = e)
                        },
                    ),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Checks the code from the verification email and signs in.
    fn verify_email(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let code = clean_email_code(&self.email_code.read(cx).value());
        if code.len() != 6 {
            ui::error(window, cx, t!("settings.verify.missing_code"));
            return;
        }
        let totp: String = self
            .totp
            .read(cx)
            .value()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if self.verify_totp && totp.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_code"));
            return;
        }
        let totp = self.verify_totp.then_some(totp);
        self.busy = true;
        cx.notify();
        let task = self
            .model
            .update(cx, |m, cx| m.verify_email_code(code, totp, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match res {
                    Ok(()) => {
                        this.register = false;
                        this.totp_step = false;
                        this.verify_totp = false;
                        for input in [&this.password, &this.totp, &this.invite, &this.email_code] {
                            input.update(cx, |i, cx| i.set_value("", window, cx));
                        }
                        this.invite_check = None;
                    }
                    Err(LoginError::TotpRequired) => {
                        this.verify_totp = true;
                        ui::focus_later(&this.totp, window, cx);
                        ui::notify(
                            window,
                            cx,
                            ToastKind::Info,
                            t!("settings.login.totp_required"),
                        );
                    }
                    Err(LoginError::TotpInvalid) => {
                        this.verify_totp = true;
                        this.totp.update(cx, |i, cx| i.set_value("", window, cx));
                        ui::focus_later(&this.totp, window, cx);
                        ui::error(window, cx, t!("settings.login.totp_invalid"));
                    }
                    Err(LoginError::Failed(e)) => {
                        this.email_code
                            .update(cx, |i, cx| i.set_value("", window, cx));
                        ui::focus_later(&this.email_code, window, cx);
                        ui::error(window, cx, t!("settings.verify.failed", error = e));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Asks for another verification email.
    fn resend_email_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.resending {
            return;
        }
        let Some(email) = self
            .model
            .read(cx)
            .pending_verification
            .as_ref()
            .map(|p| p.email.clone())
        else {
            return;
        };
        self.resending = true;
        cx.notify();
        let task = self.model.update(cx, |m, cx| m.resend_email_code(cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.resending = false;
                match res {
                    Ok(()) => {
                        ui::notify(
                            window,
                            cx,
                            ToastKind::Success,
                            t!("settings.verify.resent", email = email),
                        );
                        ui::focus_later(&this.email_code, window, cx);
                    }
                    Err(e) => ui::error(window, cx, t!("settings.verify.resend_failed", error = e)),
                }
                this.start_countdown(cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Back to the sign-in form to use another email.
    fn use_another_email(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.verify_totp = false;
        for input in [&self.totp, &self.email_code] {
            input.update(cx, |i, cx| i.set_value("", window, cx));
        }
        self.model.update(cx, |m, cx| m.cancel_verification(cx));
        ui::focus_later(&self.email, window, cx);
        cx.notify();
    }

    /// Seconds left before "Resend" can be used again.
    fn resend_wait(&self, cx: &Context<Self>) -> u64 {
        self.model
            .read(cx)
            .pending_verification
            .as_ref()
            .map_or(0, PendingVerification::resend_wait)
    }

    /// Re-renders every half second while "Resend" is waiting.
    fn start_countdown(&mut self, cx: &mut Context<Self>) {
        if self.countdown || self.resend_wait(cx) == 0 {
            return;
        }
        self.countdown = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let waiting = this.update(cx, |this, cx| {
                    cx.notify();
                    let waiting = this.resend_wait(cx) > 0;
                    this.countdown = waiting;
                    waiting
                });
                if !matches!(waiting, Ok(true)) {
                    return;
                }
            }
        })
        .detach();
    }

    /// Two-step verification code box (signing in or verifying the email).
    fn render_totp_box(&self, cx: &Context<Self>) -> gpui::Div {
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.primary)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::ShieldCheck)
                            .size(px(16.))
                            .text_color(theme.primary),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .child(t!("settings.totp.title")),
                    ),
            )
            .child(ui::field_with_hint(
                t!("settings.totp.code"),
                div().w(px(240.)).child(Input::new(&self.totp)),
                t!("settings.totp.hint"),
                cx,
            ))
    }

    /// "Check your email": the code from the verification email, which
    /// signs in.
    fn render_verify_email(
        &self,
        pending: PendingVerification,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = cx.theme();
        let wait = pending.resend_wait();
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::Mail)
                            .size(px(16.))
                            .text_color(theme.primary),
                    )
                    .child(div().font_semibold().child(t!("settings.verify.title"))),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .child(t!("settings.verify.sent", email = pending.email)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(pending.url.clone()),
                    ),
            )
            .child(ui::field_with_hint(
                t!("settings.verify.code"),
                div().w(px(240.)).child(Input::new(&self.email_code)),
                t!("settings.verify.hint"),
                cx,
            ))
            .when(self.verify_totp, |this| {
                this.child(self.render_totp_box(cx))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        Button::new("verify-email")
                            .primary()
                            .icon(ui::icon(IconName::ShieldCheck))
                            .label(t!("settings.login.verify"))
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.verify_email(window, cx)
                            })),
                    )
                    .child(
                        Button::new("resend-code")
                            .ghost()
                            .icon(ui::icon(IconName::Send))
                            .label(if wait > 0 {
                                t!("settings.verify.resend_in", seconds = wait)
                            } else {
                                t!("settings.verify.resend")
                            })
                            .loading(self.resending)
                            .disabled(wait > 0)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.resend_email_code(window, cx)
                            })),
                    )
                    .child(
                        Button::new("use-another-email")
                            .ghost()
                            .label(t!("settings.verify.another_email"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.use_another_email(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_card(
        &self,
        title: impl Into<gpui::SharedString>,
        icon: IconName,
        cx: &Context<Self>,
    ) -> gpui::Div {
        ui::card(cx).p_5().gap_4().w_full().max_w(px(760.)).child(
            h_flex()
                .gap_2()
                .items_center()
                .child(ui::icon(icon).size(px(18.)).text_color(cx.theme().primary))
                .child(div().text_lg().font_semibold().child(title.into())),
        )
    }

    fn render_appearance(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let settings = self.model.read(cx).settings.clone();
        let dark = cx.theme().is_dark();
        self.render_card(t!("settings.appearance.title"), IconName::Palette, cx)
            .child(ui::field_with_hint(
                t!("settings.language.label"),
                div().w(px(280.)).child(Select::new(&self.language)),
                t!("settings.language.hint"),
                cx,
            ))
            .child(ui::field(
                t!("settings.appearance.theme"),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("theme-dark")
                            .icon(ui::icon(IconName::Moon))
                            .label(t!("settings.appearance.dark"))
                            .when(dark, |b| b.primary())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.set_dark(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("theme-light")
                            .icon(ui::icon(IconName::Sun))
                            .label(t!("settings.appearance.light"))
                            .when(!dark, |b| b.primary())
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.set_dark(false, window, cx)
                            })),
                    ),
                cx,
            ))
            .child(ui::field(
                t!("settings.font.size"),
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("font-smaller")
                            .small()
                            .icon(ui::icon(IconName::Minus))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.change_font_size(-1., cx)
                            })),
                    )
                    .child(
                        div()
                            .w(px(48.))
                            .text_center()
                            .child(format!("{:.0} px", settings.font_size)),
                    )
                    .child(
                        Button::new("font-bigger")
                            .small()
                            .icon(ui::icon(IconName::Plus))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.change_font_size(1., cx)
                            })),
                    ),
                cx,
            ))
            .child(ui::field_with_hint(
                t!("settings.font.family"),
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.font_family)))
                    .child(
                        Button::new("font-apply")
                            .label(t!("settings.font.apply"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.apply_font_family(window, cx)
                            })),
                    ),
                t!("settings.font.family_hint"),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Switch::new("use-agent")
                            .label(t!("settings.agent.label"))
                            .checked(settings.use_agent)
                            .on_click(cx.listener(|this, v: &bool, _, cx| this.set_agent(*v, cx))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("settings.agent.hint")),
                    ),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_3()
                            .items_center()
                            .child(
                                Switch::new("autocomplete")
                                    .label(t!("settings.autocomplete.label"))
                                    .checked(settings.autocomplete)
                                    .on_click(cx.listener(|this, v: &bool, _, cx| {
                                        this.set_autocomplete(*v, cx)
                                    })),
                            )
                            .child(
                                Button::new("clear-history")
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::Trash))
                                    .label(t!("settings.history.clear"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.clear_history(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("settings.autocomplete.hint")),
                    ),
            )
    }

    fn render_login_form(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let register = self.register;
        let totp_step = self.totp_step && !register;
        let invite_line = self.invite_check.clone().map(|c| {
            let (icon, color, text) = match c {
                InviteCheck::Checking => (
                    IconName::Loader,
                    theme.muted_foreground,
                    t!("settings.invite.checking").to_string(),
                ),
                InviteCheck::Valid(t) => (IconName::CircleCheck, theme.success, t),
                InviteCheck::Invalid(e) => (IconName::CircleX, theme.danger, e),
            };
            h_flex()
                .gap_1p5()
                .items_center()
                .text_xs()
                .text_color(color)
                .child(ui::icon(icon).size(px(14.)))
                .child(text)
        });
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("settings.login.intro")),
            )
            .child(ui::field(
                t!("settings.login.server_url"),
                Input::new(&self.server_url),
                cx,
            ))
            .when(register, |this| {
                this.child(ui::field(t!("common.name"), Input::new(&self.name), cx))
            })
            .child(ui::field(
                t!("settings.login.email"),
                Input::new(&self.email),
                cx,
            ))
            .child(ui::field(
                t!("settings.login.password"),
                Input::new(&self.password).mask_toggle(),
                cx,
            ))
            .when(register, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(ui::field_with_hint(
                            t!("settings.invite.label"),
                            Input::new(&self.invite).cleanable(true),
                            t!("settings.invite.hint"),
                            cx,
                        ))
                        .children(invite_line),
                )
            })
            .when(totp_step, |this| this.child(self.render_totp_box(cx)))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("login")
                            .primary()
                            .icon(ui::icon(if totp_step {
                                IconName::ShieldCheck
                            } else {
                                IconName::LogIn
                            }))
                            .label(if register {
                                t!("settings.login.create_account")
                            } else if totp_step {
                                t!("settings.login.verify")
                            } else {
                                t!("settings.login.sign_in")
                            })
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.login(window, cx)
                            })),
                    )
                    .when(totp_step, |this| {
                        this.child(
                            Button::new("cancel-totp")
                                .ghost()
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.cancel_totp(window, cx)
                                })),
                        )
                    })
                    .when(!totp_step && !register, |this| {
                        // The server website sends the link to set a new
                        // password.
                        this.child(
                            Button::new("forgot-password")
                                .ghost()
                                .label(t!("settings.login.forgot_password"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    let url = this.server_url.read(cx).value().trim().to_string();
                                    if let Some(link) = web_link(&url, "/forgot-password") {
                                        cx.open_url(&link);
                                    }
                                })),
                        )
                    })
                    .when(!totp_step, |this| {
                        this.child(
                            Button::new("toggle-register")
                                .ghost()
                                .label(if register {
                                    t!("settings.login.have_account")
                                } else {
                                    t!("settings.login.new_account")
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.register = !this.register;
                                    this.schedule_invite_check(window, cx);
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_account(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let m = self.model.read(cx);
        let logged_in = m.logged_in();
        let user = m.server_user.clone().unwrap_or_default();
        let display_name =
            m.me.as_ref()
                .map(|u| u.name.clone())
                .filter(|n| !n.is_empty());
        let is_admin = m.is_admin();
        let url = m.server_url.clone().unwrap_or_default();
        let syncing = m.syncing;
        let last = m.last_sync.clone();
        let online = m.events_online;
        let pending = m.pending_verification.clone();
        let theme = cx.theme();
        let card = self.render_card(t!("settings.account.title"), IconName::Cloud, cx);
        if logged_in {
            let (last_text, last_color) = match &last {
                Some(Ok(sync)) => (
                    t!(
                        "settings.account.last_sync",
                        changes = t!(
                            "settings.account.sync_changes",
                            pushed = sync.pushed,
                            pulled = sync.pulled
                        ),
                        time = sync.at.format("%H:%M:%S")
                    ),
                    theme.muted_foreground,
                ),
                Some(Err(e)) => (t!("settings.account.sync_error", error = e), theme.danger),
                None => (t!("settings.account.never_synced"), theme.muted_foreground),
            };
            card.child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .when_some(display_name, |this, n| {
                                this.child(div().font_semibold().child(n))
                            })
                            .child(div().font_medium().child(user))
                            .child(ui::pill(
                                if online {
                                    t!("settings.account.online")
                                } else {
                                    t!("settings.account.offline")
                                },
                                if online { theme.success } else { theme.warning },
                            ))
                            .when(is_admin, |this| {
                                this.child(ui::pill(
                                    t!("settings.account.server_admin"),
                                    theme.primary,
                                ))
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(url),
                    )
                    .child(div().text_sm().text_color(last_color).child(last_text)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("settings.account.sync_hint")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("sync-now")
                            .primary()
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("settings.account.sync_now"))
                            .loading(syncing)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.model.update(cx, |m, cx| m.sync_now(cx));
                            })),
                    )
                    .child(
                        Button::new("web-account")
                            .icon(ui::icon(IconName::ExternalLink))
                            .label(t!("settings.account.web_account"))
                            .tooltip(t!("settings.account.web_account_tooltip"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                let url =
                                    this.model.read(cx).server_url.clone().unwrap_or_default();
                                if let Some(link) = web_link(&url, "/app/account") {
                                    cx.open_url(&link);
                                }
                            })),
                    )
                    .child(
                        Button::new("logout")
                            .icon(ui::icon(IconName::LogOut))
                            .label(t!("settings.account.sign_out"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.model.update(cx, |m, cx| m.logout(cx));
                            })),
                    ),
            )
            .child(self.two_factor.clone())
        } else if let Some(pending) = pending {
            card.child(self.render_verify_email(pending, cx))
        } else {
            card.child(self.render_login_form(cx))
        }
    }

    fn render_updates(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let u = self.updates.read(cx);
        let status = u.status.clone();
        let text = u.status_text();
        let enabled = status != UpdateStatus::Disabled;
        let ready = u.ready_version().is_some();
        let busy = matches!(
            status,
            UpdateStatus::Checking | UpdateStatus::Downloading(_)
        );
        let available = matches!(status, UpdateStatus::Available(_));
        self.render_card(t!("settings.updates.title"), IconName::Download, cx)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().font_medium().child(t!(
                        "settings.updates.installed",
                        version = update::current_version()
                    )))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(text),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("check-updates")
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("settings.updates.check"))
                            .loading(busy)
                            .disabled(!enabled || ready)
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.updates.update(cx, |u, cx| u.check_now(cx));
                            })),
                    )
                    .when(ready, |this| {
                        this.child(
                            Button::new("restart-update")
                                .primary()
                                .label(t!("settings.updates.restart"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.updates.read(cx).restart_now();
                                })),
                        )
                    })
                    .when(available, |this| {
                        this.child(
                            Button::new("open-releases")
                                .icon(ui::icon(IconName::ExternalLink))
                                .label(t!("settings.updates.download"))
                                .on_click(|_: &ClickEvent, _, cx| {
                                    cx.open_url(update::RELEASES_PAGE)
                                }),
                        )
                    }),
            )
    }

    fn render_about(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let dir = self.model.read(cx).ws.dir.display().to_string();
        self.render_card(t!("settings.about.title"), IconName::Info, cx)
            .child(div().text_sm().child(t!("settings.about.description")))
            .child(ui::field(
                t!("settings.about.local_data"),
                div().text_sm().font_family(ui::mono_family(cx)).child(dir),
                cx,
            ))
            .child(
                Button::new("repo")
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::Github))
                    .label(t!("settings.about.source_code"))
                    .on_click(|_: &ClickEvent, _, cx| {
                        cx.open_url("https://github.com/TermoakSSH/desktop")
                    }),
            )
    }
}

impl Render for SettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = TabBar::new("settings-pages")
            .segmented()
            .selected_index(match self.page {
                SettingsPage::General => 0,
                SettingsPage::Ai => 1,
            })
            .child(
                Tab::new()
                    .icon(ui::icon(IconName::Settings))
                    .label(t!("settings.page.general")),
            )
            .child(
                Tab::new()
                    .icon(ui::icon(IconName::Sparkles))
                    .label(t!("settings.page.ai")),
            )
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                let page = if *ix == 1 {
                    SettingsPage::Ai
                } else {
                    SettingsPage::General
                };
                this.show_page(page, window, cx);
            }));
        let header = ui::section_header(
            t!("settings.title"),
            match self.page {
                SettingsPage::General => t!("settings.subtitle"),
                SettingsPage::Ai => t!("settings.subtitle_ai"),
            },
            pages,
            cx,
        );
        if self.page == SettingsPage::Ai {
            return v_flex()
                .size_full()
                .child(header)
                .child(div().flex_1().min_h_0().child(self.ai.clone()));
        }
        // A pending verification may come from the background (sync).
        self.start_countdown(cx);
        let account = self.render_account(cx);
        let appearance = self.render_appearance(cx);
        let paste = self.render_paste(cx);
        let updates = self.render_updates(cx);
        let about = self.render_about(cx);
        v_flex().size_full().child(header).child(
            div().flex_1().min_h_0().child(
                v_flex().size_full().overflow_y_scrollbar().child(
                    v_flex()
                        .p_6()
                        .gap_5()
                        .child(account)
                        .child(appearance)
                        .child(paste)
                        .child(updates)
                        .child(about),
                ),
            ),
        )
    }
}

/// Link to a page of the server website (`None` without a valid URL).
fn web_link(server: &str, path: &str) -> Option<String> {
    let base = server.trim().trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return None;
    }
    Some(format!("{base}{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_links() {
        assert_eq!(
            web_link("https://ssh.example.com/", "/forgot-password").as_deref(),
            Some("https://ssh.example.com/forgot-password")
        );
        assert_eq!(web_link("", "/app"), None);
        assert_eq!(web_link("javascript:alert(1)", "/app"), None);
    }
    use serde_json::json;

    #[test]
    fn invite_links_are_split() {
        assert_eq!(
            parse_invite_link(
                "termoak://invite?server=https%3A%2F%2Fssh.example.com&token=abc_DEF-123"
            ),
            Some(("https://ssh.example.com".into(), "abc_DEF-123".into()))
        );
        assert_eq!(parse_invite_link("abc_DEF-123"), None);
        assert_eq!(parse_invite_link("https://example.com/?token=x"), None);
        assert_eq!(parse_invite_link("termoak://invite?token=x"), None);
        // Half typed.
        assert_eq!(
            parse_invite_link("termoak://invite?server=https%3A%2F%2Fa.b&token"),
            None
        );
        assert_eq!(
            parse_invite_link("termoak://invite?server=&token=abc"),
            None
        );
    }

    #[test]
    fn invite_description() {
        assert_eq!(
            describe_invite(&json!({"team": "Ops", "email": null, "expires_at": null})),
            "You will join the team “Ops”."
        );
        assert_eq!(
            describe_invite(&json!({"team": null, "email": "ana@example.com"})),
            "Valid invitation. Only for ana@example.com."
        );
    }
}
