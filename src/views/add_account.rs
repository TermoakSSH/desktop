//! "Add account": signing in or creating an account, on the official server
//! (one big button, no address to type) or on a server of your own (its
//! address is checked first with `/info`: name, version, whether anyone can
//! sign up and the test-environment banner). Then email and password, the
//! two-step code when the account has it, and for new accounts the terms
//! and the six-digit code from the verification email.
//!
//! The same dialog signs an account in again (same server and email) and
//! opens `termoak://invite` links (sign-up form with the server and code).

use std::time::Duration;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde_json::Value;
use termoak_client::ApiClient;
use termoak_client::servers::{self, OFFICIAL_SERVER, ServerChoice};
use termoak_core::Id;

use crate::runtime;
use crate::state::{
    AppModel, LoginError, LoginOutcome, PendingVerification, ToastKind, api_error, clean_email_code,
};
use crate::ui::{self, IconName};

/// Wait after typing the invitation code before checking it.
const INVITE_CHECK_DELAY: Duration = Duration::from_millis(500);

/// How the dialog starts.
#[derive(Debug, Clone)]
pub enum Start {
    /// The welcome choices.
    Choose,
    /// Sign in again to an account (prefilled).
    SignInAgain { url: String, email: String },
    /// An invitation link: the sign-up form of that server.
    Invite { server: String, token: String },
    /// The email code of an account that is not verified yet.
    Verify(Id),
}

/// Opens "Add account".
pub fn open(model: Entity<AppModel>, start: Start, window: &mut Window, cx: &mut App) {
    let view = cx.new(|cx| AddAccountDialog::new(model, start, window, cx));
    window.open_dialog(cx, move |d, _, _| {
        d.title(t!("add_account.title"))
            .w(px(500.))
            .overlay_closable(false)
            .child(view.clone())
    });
}

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

/// Link to a page of the server website (`None` without a valid URL).
pub fn web_link(server: &str, path: &str) -> Option<String> {
    let base = server.trim().trim_end_matches('/');
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return None;
    }
    Some(format!("{base}{path}"))
}

/// What `/info` says about a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// Canonical URL.
    pub url: String,
    /// Host to show ("ssh.example.com").
    pub host: String,
    pub name: String,
    pub version: String,
    /// Anyone can create an account (otherwise an invitation is needed).
    pub registration_open: bool,
    /// A test server (`preprod`...): show a banner.
    pub environment: Option<String>,
    pub terms_url: Option<String>,
    pub privacy_url: Option<String>,
    /// New accounts confirm their email with a six-digit code.
    pub email_code: bool,
    /// Plain `http://`: allowed for your own servers, with a warning.
    pub insecure: bool,
    pub official: bool,
    /// The server has vaults (older ones: one personal vault, no sharing).
    pub vaults: bool,
}

/// Reads `/info`. `None` if it is not a Termoak server.
pub fn parse_server_info(url: &str, v: &Value) -> Option<ServerInfo> {
    if v["api"].as_str().is_none() && v["version"].as_str().is_none() {
        return None;
    }
    let text = |key: &str| {
        v[key]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(ServerInfo {
        url: url.to_string(),
        host: servers::display_host(url),
        name: text("name").unwrap_or_else(|| "Termoak".into()),
        version: text("version").unwrap_or_default(),
        registration_open: v["registration"].as_str() != Some("closed"),
        environment: text("environment"),
        terms_url: text("terms_url"),
        privacy_url: text("privacy_url"),
        email_code: v["features"]["email_verification_code"]
            .as_bool()
            .unwrap_or(false),
        insecure: servers::is_insecure(url),
        official: servers::is_official(url),
        vaults: v["features"]["vaults"].as_bool().unwrap_or(false),
    })
}

/// Result of checking an invitation code.
#[derive(Clone)]
enum InviteCheck {
    Checking,
    Valid(String),
    Invalid(String),
}

/// Steps of the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Choose,
    /// The address of your own server.
    CustomUrl,
    SignIn,
    SignUp,
    /// The six-digit code from the verification email.
    Verify,
}

pub struct AddAccountDialog {
    model: Entity<AppModel>,
    step: Step,
    /// The step "Back" goes to.
    back: Option<Step>,
    /// Chosen server: official, or your own.
    official: bool,
    /// What `/info` said (while checking: `None`).
    info: Option<ServerInfo>,
    checking: bool,
    url: Entity<InputState>,
    email: Entity<InputState>,
    password: Entity<InputState>,
    name: Entity<InputState>,
    /// Two-step verification code (or recovery code).
    totp: Entity<InputState>,
    /// Invitation code (optional) when creating the account.
    invite: Entity<InputState>,
    /// Six-digit code from the verification email.
    email_code: Entity<InputState>,
    terms: bool,
    /// The account asked for the two-step verification code.
    totp_step: bool,
    /// Verifying the email also asked for the two-step code.
    verify_totp: bool,
    busy: bool,
    resending: bool,
    countdown: bool,
    invite_check: Option<InviteCheck>,
    _invite_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

fn input(
    window: &mut Window,
    cx: &mut Context<AddAccountDialog>,
    placeholder: impl Into<gpui::SharedString>,
) -> Entity<InputState> {
    let placeholder = placeholder.into();
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

impl AddAccountDialog {
    fn new(
        model: Entity<AppModel>,
        start: Start,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let url = input(window, cx, "ssh.example.com");
        let email = input(window, cx, t!("settings.placeholder.email"));
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("settings.placeholder.password"))
        });
        let name = input(window, cx, t!("settings.placeholder.name"));
        let totp = input(window, cx, "123456");
        let invite = input(window, cx, t!("settings.placeholder.invite"));
        let email_code = input(window, cx, "123456");
        let subs = vec![
            cx.subscribe_in(&url, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.check_custom(window, cx);
                }
            }),
            cx.subscribe_in(&password, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit(window, cx);
                }
            }),
            cx.subscribe_in(&totp, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit(window, cx);
                }
            }),
            // Another email: the previous two-step code no longer applies.
            cx.subscribe_in(&email, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::Change = ev {
                    this.cancel_totp(window, cx);
                }
            }),
            cx.subscribe_in(&invite, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::Change = ev {
                    this.on_invite_changed(window, cx);
                }
            }),
            // The email code keeps only its digits and is checked as soon as
            // it is complete.
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
                            this.verify(window, cx);
                        }
                    }
                    InputEvent::PressEnter { .. } => this.verify(window, cx),
                    _ => {}
                },
            ),
            cx.observe(&model, |_, _, cx| cx.notify()),
        ];
        let mut this = Self {
            model,
            step: Step::Choose,
            back: None,
            official: true,
            info: None,
            checking: false,
            url,
            email,
            password,
            name,
            totp,
            invite,
            email_code,
            terms: false,
            totp_step: false,
            verify_totp: false,
            busy: false,
            resending: false,
            countdown: false,
            invite_check: None,
            _invite_task: None,
            _subs: subs,
        };
        match start {
            Start::Choose => {}
            Start::SignInAgain { url, email } => {
                this.official = servers::is_official(&url);
                this.url
                    .update(cx, |i, cx| i.set_value(url.clone(), window, cx));
                this.email
                    .update(cx, |i, cx| i.set_value(email, window, cx));
                this.fetch_info(url, Step::SignIn, window, cx);
                this.go(Step::SignIn, None, window, cx);
            }
            Start::Invite { server, token } => {
                this.official = servers::is_official(&server);
                this.url
                    .update(cx, |i, cx| i.set_value(server.clone(), window, cx));
                this.invite
                    .update(cx, |i, cx| i.set_value(token, window, cx));
                this.fetch_info(server, Step::SignUp, window, cx);
                this.go(Step::SignUp, Some(Step::Choose), window, cx);
                this.schedule_invite_check(window, cx);
            }
            Start::Verify(account) => {
                this.model
                    .update(cx, |m, cx| m.resume_verification(account, cx));
                this.go(Step::Verify, None, window, cx);
            }
        }
        this
    }

    fn go(&mut self, step: Step, back: Option<Step>, window: &mut Window, cx: &mut Context<Self>) {
        self.step = step;
        self.back = back;
        self.totp_step = false;
        match step {
            Step::CustomUrl => ui::focus_later(&self.url, window, cx),
            Step::SignIn => {
                if self.email.read(cx).value().is_empty() {
                    ui::focus_later(&self.email, window, cx)
                } else {
                    ui::focus_later(&self.password, window, cx)
                }
            }
            Step::SignUp => ui::focus_later(&self.name, window, cx),
            Step::Verify => {
                ui::focus_later(&self.email_code, window, cx);
                self.start_countdown(cx);
            }
            Step::Choose => {}
        }
        cx.notify();
    }

    /// The URL of the chosen server.
    fn server_url(&self, cx: &App) -> Result<String, String> {
        if self.official {
            Ok(servers::official_server())
        } else {
            servers::canonical(self.url.read(cx).value().trim()).map_err(api_error)
        }
    }

    fn server_choice(&self, cx: &App) -> Result<ServerChoice, String> {
        if self.official {
            Ok(ServerChoice::Official)
        } else {
            self.server_url(cx).map(ServerChoice::Custom)
        }
    }

    /// Official server: sign in or sign up (its `/info` loads meanwhile).
    fn choose_official(&mut self, sign_up: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.official = true;
        self.info = None;
        self.fetch_info(servers::official_server(), Step::SignIn, window, cx);
        let step = if sign_up { Step::SignUp } else { Step::SignIn };
        self.go(step, Some(Step::Choose), window, cx);
    }

    /// Your own server: check its address with `/info`.
    fn check_custom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        self.official = false;
        match self.server_url(cx) {
            Ok(url) => self.fetch_info(url, Step::CustomUrl, window, cx),
            Err(e) => ui::error(window, cx, e),
        }
    }

    /// Reads `/info` of `url`. From the address step, a valid answer moves
    /// on to signing in.
    fn fetch_info(&mut self, url: String, from: Step, window: &mut Window, cx: &mut Context<Self>) {
        let Ok(url) = servers::canonical(&url) else {
            return;
        };
        self.checking = true;
        self.info = None;
        cx.notify();
        let u = url.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                let api = ApiClient::new(&u).map_err(api_error)?;
                api.info().await.map_err(api_error)
            },
            move |this, res, window, cx| {
                this.checking = false;
                match res.and_then(|v| {
                    parse_server_info(&url, &v)
                        .ok_or_else(|| t!("add_account.not_termoak").to_string())
                }) {
                    Ok(info) => {
                        this.info = Some(info);
                        if from == Step::CustomUrl && this.step == Step::CustomUrl {
                            this.go(Step::SignIn, Some(Step::CustomUrl), window, cx);
                        }
                    }
                    Err(e) if from == Step::CustomUrl => {
                        ui::error(window, cx, t!("add_account.check_failed", error = e))
                    }
                    Err(e) => tracing::warn!(error = %e, "could not read the server information"),
                }
                cx.notify();
            },
        );
    }

    /// Back to the form without a code (another email...).
    fn cancel_totp(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.totp_step {
            self.totp_step = false;
            self.totp.update(cx, |i, cx| i.set_value("", window, cx));
            cx.notify();
        }
    }

    /// A pasted `termoak://invite?...` link is split into the server and the
    /// code. Then the code is checked.
    fn on_invite_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.invite.read(cx).value().to_string();
        if let Some((server, token)) = parse_invite_link(&value) {
            self.official = servers::is_official(&server);
            self.url
                .update(cx, |i, cx| i.set_value(server.clone(), window, cx));
            self.invite
                .update(cx, |i, cx| i.set_value(token, window, cx));
            self.fetch_info(server, Step::SignUp, window, cx);
        }
        self.schedule_invite_check(window, cx);
    }

    fn schedule_invite_check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let token = self.invite.read(cx).value().trim().to_string();
        let url = self.server_url(cx).unwrap_or_default();
        if self.step != Step::SignUp || token.is_empty() || url.is_empty() {
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
                        // An invitation for an email address fills it in.
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

    /// Enter in a field: the action of the step.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.step {
            Step::SignIn => self.sign_in(window, cx),
            Step::SignUp => self.sign_up(window, cx),
            Step::Verify => self.verify(window, cx),
            Step::CustomUrl => self.check_custom(window, cx),
            Step::Choose => {}
        }
    }

    fn code(input: &Entity<InputState>, cx: &App) -> String {
        input
            .read(cx)
            .value()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect()
    }

    fn sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let server = match self.server_choice(cx) {
            Ok(s) => s,
            Err(e) => return ui::error(window, cx, e),
        };
        let email = self.email.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        if email.is_empty() || password.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_fields"));
            return;
        }
        let code = Self::code(&self.totp, cx);
        if self.totp_step && code.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_code"));
            return;
        }
        let totp = self.totp_step.then_some(code);
        self.busy = true;
        cx.notify();
        let task = self
            .model
            .update(cx, |m, cx| m.sign_in(server, email, password, totp, cx));
        self.after_login(task, false, window, cx);
    }

    fn sign_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let server = match self.server_choice(cx) {
            Ok(s) => s,
            Err(e) => return ui::error(window, cx, e),
        };
        let email = self.email.read(cx).value().trim().to_string();
        let password = self.password.read(cx).value().to_string();
        let name = self.name.read(cx).value().trim().to_string();
        let invite =
            Some(self.invite.read(cx).value().trim().to_string()).filter(|i| !i.is_empty());
        if email.is_empty() || password.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_fields"));
            return;
        }
        if name.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_name"));
            return;
        }
        if self.needs_terms() && !self.terms {
            ui::error(window, cx, t!("add_account.terms_required"));
            return;
        }
        self.busy = true;
        cx.notify();
        let task = self.model.update(cx, |m, cx| {
            m.sign_up(server, email, name, password, invite, cx)
        });
        self.after_login(task, true, window, cx);
    }

    fn after_login(
        &mut self,
        task: Task<Result<LoginOutcome, LoginError>>,
        register: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match res {
                    Ok(LoginOutcome::VerifyEmail(_)) => {
                        this.verify_totp = false;
                        for input in [&this.totp, &this.email_code] {
                            input.update(cx, |i, cx| i.set_value("", window, cx));
                        }
                        this.go(Step::Verify, None, window, cx);
                    }
                    Ok(LoginOutcome::SignedIn { account, first }) => {
                        this.done(account, first, window, cx)
                    }
                    Err(LoginError::TotpRequired) => {
                        this.totp_step = true;
                        ui::focus_later(&this.totp, window, cx);
                        ui::notify(
                            window,
                            cx,
                            ToastKind::Info,
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

    /// Signed in: the dialog closes; with the first account, This device
    /// items are offered for upload.
    fn done(&mut self, account: Id, first: bool, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        if first {
            super::upload::offer(self.model.clone(), account, window, cx);
        }
    }

    fn pending(&self, cx: &App) -> Option<PendingVerification> {
        self.model.read(cx).pending_verification.clone()
    }

    /// Checks the code from the verification email and signs in.
    fn verify(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(pending) = self.pending(cx) else {
            return;
        };
        let code = clean_email_code(&self.email_code.read(cx).value());
        if code.len() != 6 {
            ui::error(window, cx, t!("settings.verify.missing_code"));
            return;
        }
        let totp = Self::code(&self.totp, cx);
        if self.verify_totp && totp.is_empty() {
            ui::error(window, cx, t!("settings.login.missing_code"));
            return;
        }
        let totp = self.verify_totp.then_some(totp);
        self.busy = true;
        cx.notify();
        let task = self.model.update(cx, |m, cx| {
            m.verify_account(pending.account, code, totp, cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match res {
                    Ok(LoginOutcome::SignedIn { account, first }) => {
                        this.done(account, first, window, cx)
                    }
                    Ok(LoginOutcome::VerifyEmail(_)) => {
                        ui::error(window, cx, t!("error.email_not_verified"))
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

    fn resend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.resending {
            return;
        }
        let Some(pending) = self.pending(cx) else {
            return;
        };
        self.resending = true;
        cx.notify();
        let task = self
            .model
            .update(cx, |m, cx| m.resend_account_code(pending.account, cx));
        let email = pending.email.clone();
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

    fn resend_wait(&self, cx: &App) -> u64 {
        self.pending(cx)
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

    /// The server shows terms of use: they must be accepted to sign up.
    fn needs_terms(&self) -> bool {
        self.info.as_ref().is_some_and(|i| i.terms_url.is_some())
    }

    /// "Create account" is offered on servers open to anyone, or with an
    /// invitation.
    fn can_sign_up(&self, cx: &App) -> bool {
        self.official
            || self.info.as_ref().is_none_or(|i| i.registration_open)
            || !self.invite.read(cx).value().trim().is_empty()
    }

    // ----- Rendering -----

    fn render_choose(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let first = self.model.read(cx).accounts.is_empty();
        let host = servers::display_host(OFFICIAL_SERVER);
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("add_account.intro")),
            )
            .child(
                Button::new("official-sign-in")
                    .primary()
                    .large()
                    .w_full()
                    .icon(ui::icon(IconName::LogIn))
                    .label(t!("add_account.official_sign_in", host = host))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.choose_official(false, window, cx)
                    })),
            )
            .child(
                Button::new("official-sign-up")
                    .large()
                    .w_full()
                    .icon(ui::icon(IconName::UserPlus))
                    .label(t!("add_account.official_sign_up"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.choose_official(true, window, cx)
                    })),
            )
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        Button::new("custom-server")
                            .link()
                            .icon(ui::icon(IconName::Server))
                            .label(t!("add_account.custom_server"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.official = false;
                                this.info = None;
                                this.go(Step::CustomUrl, Some(Step::Choose), window, cx);
                            })),
                    )
                    .when(first, |this| {
                        this.child(
                            Button::new("no-account")
                                .ghost()
                                .label(t!("add_account.no_account"))
                                .on_click(|_: &ClickEvent, window, cx| window.close_dialog(cx)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The chosen server: name, address, version and the test banner.
    fn render_server(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
        let url = self.server_url(cx).unwrap_or_default();
        let host = servers::display_host(&url);
        let info = self.info.clone();
        let mut card = ui::card(cx).p_3().gap_1().child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    ui::icon(if self.official {
                        IconName::Cloud
                    } else {
                        IconName::Server
                    })
                    .size(px(16.))
                    .text_color(theme.primary),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_semibold()
                        .text_sm()
                        .child(match &info {
                            Some(i) if !i.official => format!("{} · {host}", i.name),
                            _ => host.clone(),
                        }),
                )
                .when(self.checking, |this| {
                    this.child(
                        ui::icon(IconName::Loader)
                            .size(px(14.))
                            .text_color(theme.muted_foreground),
                    )
                })
                .when(!self.official && self.back.is_some(), |this| {
                    this.child(
                        Button::new("change-server")
                            .xsmall()
                            .ghost()
                            .label(t!("add_account.change_server"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.go(Step::CustomUrl, Some(Step::Choose), window, cx)
                            })),
                    )
                }),
        );
        if let Some(i) = &info {
            let mut details = Vec::new();
            if !i.version.is_empty() {
                details.push(t!("add_account.version", version = i.version.clone()).to_string());
            }
            details.push(if i.registration_open {
                t!("add_account.registration_open").to_string()
            } else {
                t!("add_account.registration_closed").to_string()
            });
            if !i.vaults {
                details.push(t!("add_account.no_vaults").to_string());
            }
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(details.join(" · ")),
            );
            if let Some(env) = &i.environment {
                card = card.child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .px_2()
                        .py_1()
                        .rounded(theme.radius)
                        .bg({
                            let mut bg = theme.warning;
                            bg.a = 0.15;
                            bg
                        })
                        .text_xs()
                        .text_color(theme.warning)
                        .child(ui::icon(IconName::TriangleAlert).size(px(12.)))
                        .child(t!("add_account.environment", environment = env.clone())),
                );
            }
            if i.insecure {
                card = card.child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .text_color(theme.danger)
                        .child(ui::icon(IconName::LockOpen).size(px(12.)))
                        .child(t!("add_account.insecure")),
                );
            }
        }
        card.into_any_element()
    }

    fn render_custom_url(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("add_account.custom_intro")),
            )
            .child(ui::field(
                t!("settings.login.server_url"),
                Input::new(&self.url).prefix(ui::icon(IconName::Server).size(px(14.))),
                cx,
            ))
            .child(
                h_flex().justify_end().child(
                    Button::new("check-server")
                        .primary()
                        .label(t!("add_account.continue"))
                        .loading(self.checking)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.check_custom(window, cx)
                        })),
                ),
            )
            .into_any_element()
    }

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

    fn render_sign_in(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let totp_step = self.totp_step;
        let can_sign_up = self.can_sign_up(cx);
        let server = self.render_server(cx);
        v_flex()
            .gap_3()
            .child(server)
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
            .when(totp_step, |this| this.child(self.render_totp_box(cx)))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(
                        Button::new("sign-in")
                            .primary()
                            .icon(ui::icon(if totp_step {
                                IconName::ShieldCheck
                            } else {
                                IconName::LogIn
                            }))
                            .label(if totp_step {
                                t!("settings.login.verify")
                            } else {
                                t!("settings.login.sign_in")
                            })
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.sign_in(window, cx)
                            })),
                    )
                    .when(!totp_step, |this| {
                        this.child(
                            Button::new("forgot-password")
                                .ghost()
                                .label(t!("settings.login.forgot_password"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    let url = this.server_url(cx).unwrap_or_default();
                                    if let Some(link) = web_link(&url, "/forgot-password") {
                                        cx.open_url(&link);
                                    }
                                })),
                        )
                    })
                    .when(!totp_step && can_sign_up, |this| {
                        this.child(
                            Button::new("to-sign-up")
                                .ghost()
                                .label(t!("settings.login.new_account"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    let back = this.back;
                                    this.go(Step::SignUp, back, window, cx);
                                    this.schedule_invite_check(window, cx);
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_sign_up(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = cx.theme();
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
        let terms = self
            .info
            .as_ref()
            .and_then(|i| i.terms_url.clone().map(|t| (t, i.privacy_url.clone())));
        let show_invite = !self.official
            || !self.invite.read(cx).value().trim().is_empty()
            || self.info.as_ref().is_some_and(|i| !i.registration_open);
        let server = self.render_server(cx);
        v_flex()
            .gap_3()
            .child(server)
            .child(ui::field(t!("common.name"), Input::new(&self.name), cx))
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
            .when(show_invite, |this| {
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
            .when_some(terms, |this, (terms, privacy)| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("accept-terms")
                                .label(t!("add_account.accept_terms"))
                                .checked(self.terms)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    this.terms = *v;
                                    cx.notify();
                                })),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    Button::new("open-terms")
                                        .xsmall()
                                        .link()
                                        .label(t!("add_account.terms"))
                                        .on_click(move |_: &ClickEvent, _, cx| cx.open_url(&terms)),
                                )
                                .when_some(privacy, |this, privacy| {
                                    this.child(
                                        Button::new("open-privacy")
                                            .xsmall()
                                            .link()
                                            .label(t!("add_account.privacy"))
                                            .on_click(move |_: &ClickEvent, _, cx| {
                                                cx.open_url(&privacy)
                                            }),
                                    )
                                }),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("sign-up")
                            .primary()
                            .icon(ui::icon(IconName::UserPlus))
                            .label(t!("settings.login.create_account"))
                            .loading(self.busy)
                            .disabled(self.needs_terms() && !self.terms)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.sign_up(window, cx)
                            })),
                    )
                    .child(
                        Button::new("to-sign-in")
                            .ghost()
                            .label(t!("settings.login.have_account"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                let back = this.back;
                                this.go(Step::SignIn, back, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_verify(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(pending) = self.pending(cx) else {
            return div()
                .text_sm()
                .child(t!("add_account.verified"))
                .into_any_element();
        };
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
                            .child(servers::display_host(&pending.url)),
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
                                this.verify(window, cx)
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
                                this.resend(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("add_account.verify_later")),
            )
            .into_any_element()
    }
}

impl Render for AddAccountDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match self.step {
            Step::Choose => self.render_choose(cx),
            Step::CustomUrl => self.render_custom_url(cx),
            Step::SignIn => self.render_sign_in(cx),
            Step::SignUp => self.render_sign_up(cx),
            Step::Verify => self.render_verify(cx),
        };
        v_flex()
            .gap_3()
            .when_some(self.back, |this, back| {
                this.child(
                    Button::new("add-account-back")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ArrowLeft))
                        .label(t!("common.back"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            let back = if back == Step::CustomUrl {
                                Some(Step::Choose)
                            } else {
                                None
                            };
                            let to = this.back.unwrap_or(Step::Choose);
                            this.go(to, back, window, cx)
                        })),
                )
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn web_links() {
        assert_eq!(
            web_link("https://ssh.example.com/", "/forgot-password").as_deref(),
            Some("https://ssh.example.com/forgot-password")
        );
        assert_eq!(web_link("", "/app"), None);
        assert_eq!(web_link("javascript:alert(1)", "/app"), None);
    }

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

    #[test]
    fn server_info_from_info() {
        let v = json!({
            "api": "v1", "environment": "preprod", "name": "Termoak", "registration": "open",
            "terms_url": "https://next.termoak.com/terms", "privacy_url": null, "version": "0.4.0",
            "features": {"email_verification_code": true, "vaults": true}
        });
        let i = parse_server_info("https://next.termoak.com", &v).unwrap();
        assert_eq!(i.host, "next.termoak.com");
        assert_eq!(i.environment.as_deref(), Some("preprod"));
        assert!(i.registration_open && i.email_code && i.vaults && !i.insecure);
        assert_eq!(
            i.terms_url.as_deref(),
            Some("https://next.termoak.com/terms")
        );
        assert_eq!(i.privacy_url, None);

        let v = json!({"version": "0.2.0", "registration": "closed", "environment": null});
        let i = parse_server_info("http://10.0.0.5:7733", &v).unwrap();
        assert!(!i.registration_open && i.insecure && !i.vaults && !i.official);
        assert_eq!(i.environment, None);
        assert_eq!(i.name, "Termoak");

        // Not a Termoak server.
        assert!(parse_server_info("https://example.com", &json!({"hello": 1})).is_none());
    }
}
