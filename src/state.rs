//! App model: local workspace, session with the server, cached data (hosts,
//! keychain, snippets, tunnels...) and background tasks (periodic sync and
//! server events).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{Context, EventEmitter, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use termoak_client::{ApiClient, ClientError, LOCAL_OWNER, SyncEngine, Workspace};
use termoak_core::model::{
    Entity as CoreEntity, Group, Host, HostSecret, Identity, KnownHost, PortForward, Record,
    SecretUpdate, Snippet, SshKey, SyncMode, Team, TeamRole, User,
};
use termoak_core::{CoreError, Id};
use termoak_ssh::{Connection, ForwardHandle, ForwardSpec, ForwardStats};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::local_ai::{AiSettings, RunOn};
use crate::notifications::NotificationPrefs;
use crate::prompts::DesktopPrompter;
use crate::runtime;
use crate::terminal::paste::RightClick;

const SETTINGS_KEY: &str = "desktop.settings";
const SYNC_EVERY: Duration = Duration::from_secs(60);
/// Wait before asking for another verification email when the server does
/// not say (it allows one a minute).
const RESEND_WAIT: Duration = Duration::from_secs(60);

/// Desktop app preferences (stored in the local database).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Dark theme (the default) or light.
    pub dark: bool,
    /// Try the keys of the system SSH agent.
    pub use_agent: bool,
    /// Terminal font size.
    pub font_size: f32,
    /// Terminal font family (empty = the system monospace font).
    pub font_family: String,
    /// Terminal scrollback lines.
    pub scrollback: usize,
    /// Suggest how to finish the command while typing in the terminal (and
    /// keep each host's commands on this device).
    pub autocomplete: bool,
    /// Interface language (BCP 47 code); `None` follows the system language.
    pub language: Option<String>,
    /// Where the AI runs on this device and with what (Settings → AI).
    pub ai: AiSettings,
    /// A plain Ctrl+V pastes in the terminal (off macOS; off by default
    /// because Ctrl+V is a control character there). Ctrl+Shift+V and
    /// Shift+Insert always paste.
    pub ctrl_v_pastes: bool,
    /// What the right mouse button does in the terminal.
    pub right_click: RightClick,
    /// Selecting text in the terminal copies it.
    pub copy_on_select: bool,
    /// Ask before pasting more than one line.
    pub confirm_multiline_paste: bool,
    /// Notifications of the system while the window is in the background.
    pub notifications: NotificationPrefs,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            dark: true,
            use_agent: true,
            font_size: 14.0,
            font_family: String::new(),
            scrollback: 10_000,
            autocomplete: true,
            language: None,
            ai: AiSettings::default(),
            ctrl_v_pastes: false,
            right_click: RightClick::Menu,
            copy_on_select: false,
            confirm_multiline_paste: true,
            notifications: NotificationPrefs::default(),
        }
    }
}

impl Settings {
    /// Reads the saved preferences.
    pub async fn load(store: &termoak_core::Store) -> Self {
        store
            .meta_get(SETTINGS_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Reads the saved preferences (blocking; only at startup).
    pub fn load_blocking(ws: &Workspace, rt: &tokio::runtime::Runtime) -> Self {
        rt.block_on(ws.store.meta_get(SETTINGS_KEY))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
}

/// Kind of notification for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

/// Model events for the views.
#[derive(Debug, Clone)]
pub enum ModelEvent {
    /// Notification to show in the window.
    Toast(ToastKind, String),
    /// Server WebSocket event (`/api/v1/events/ws`).
    Server(Value),
    /// The session with the server changed (sign-in or sign-out).
    SessionChanged,
}

/// How to get into the server.
#[derive(Debug, Clone)]
pub enum LoginRequest {
    /// Sign in, with the two-step verification code if the account is
    /// already known to ask for it.
    Login { totp: Option<String> },
    /// Create the account, with an invitation code if there is one.
    Register {
        name: String,
        invite: Option<String>,
    },
}

/// How a successful sign-in ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// Signed in: sync has started.
    SignedIn,
    /// The server requires a verified email and this account has not
    /// verified it: the code from the email is needed
    /// ([`AppModel::pending_verification`]).
    VerifyEmail,
}

/// An account that has to confirm its email with the six-digit code from the
/// verification email before using the server (docs/API.md, "Email
/// verification"). The restricted tokens of that sign-in are discarded:
/// [`AppModel::verify_email_code`] signs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVerification {
    pub url: String,
    pub email: String,
    /// When "Resend" can be used again (`None`: now).
    pub resend_at: Option<Instant>,
}

impl PendingVerification {
    /// Whole seconds left before "Resend" can be used again.
    pub fn resend_wait(&self) -> u64 {
        self.resend_at
            .map(|at| at.saturating_duration_since(Instant::now()))
            .filter(|left| !left.is_zero())
            .map_or(0, |left| {
                left.as_secs() + u64::from(left.subsec_nanos() > 0)
            })
    }
}

/// Keeps only the digits of a verification code as typed or pasted
/// ("123 456", "123-456"), at most six.
pub fn clean_email_code(text: &str) -> String {
    text.chars().filter(char::is_ascii_digit).take(6).collect()
}

/// Why signing in failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    /// The account has two-step verification and the code is missing.
    TotpRequired,
    /// The verification code is not correct.
    TotpInvalid,
    Failed(String),
}

impl From<ClientError> for LoginError {
    fn from(e: ClientError) -> Self {
        if e.is_totp_required() {
            LoginError::TotpRequired
        } else if e.is_totp_invalid() {
            LoginError::TotpInvalid
        } else {
            LoginError::Failed(api_error(e))
        }
    }
}

/// Text of a client error for the interface. Server errors show only their
/// explanation (without the HTTP status): the translation of their stable
/// `code` (`error.<code>`, see docs/API.md) or, in English or for codes
/// without one, the server's own English message, which may be more
/// specific (for example, the limit of the plan).
pub fn api_error(e: ClientError) -> String {
    match e {
        ClientError::Api { code, message, .. } => {
            let translated = (!crate::i18n::current().starts_with("en"))
                .then(|| translated_api_code(&code))
                .flatten();
            match translated {
                Some(text) => text,
                None if !message.trim().is_empty() => message,
                None => crate::i18n::api_error_text(&code).unwrap_or(code),
            }
        }
        ClientError::Network(detail) => {
            tracing::warn!(%detail, "network error");
            t!("error.network").to_string()
        }
        ClientError::SessionExpired => t!("error.session_expired").to_string(),
        ClientError::NotLoggedIn => t!("error.not_logged_in").to_string(),
        other => other.to_string(),
    }
}

/// A failed AI request (tasks, messages): the text to show and whether
/// Settings → AI is where it gets fixed (`ai_key_required`: no usable API
/// key; `ai_budget_exceeded`: this month's AI credit is spent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiFailure {
    pub text: String,
    pub fix_in_settings: bool,
}

impl AiFailure {
    /// Any other failure (network, interrupted task...).
    pub fn other(text: String) -> Self {
        Self {
            text,
            fix_in_settings: false,
        }
    }
}

impl From<ClientError> for AiFailure {
    fn from(e: ClientError) -> Self {
        let fix_in_settings = e.is_ai_key_required() || e.is_ai_budget_exceeded();
        Self {
            text: api_error(e),
            fix_in_settings,
        }
    }
}

/// Translation of a server error code. The generic codes (`bad_request`,
/// `not_found`...) are left out: their English message says more than a
/// generic translation.
fn translated_api_code(code: &str) -> Option<String> {
    const GENERIC: [&str; 4] = ["bad_request", "forbidden", "not_found", "conflict"];
    if GENERIC.contains(&code) {
        return None;
    }
    crate::i18n::api_error_text(code)
}

/// Result of a successful sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSummary {
    /// Changes sent to the server.
    pub pushed: usize,
    /// Changes received from the server.
    pub pulled: usize,
    /// When it finished.
    pub at: chrono::DateTime<chrono::Local>,
}

impl SyncSummary {
    fn new(pushed: usize, pulled: usize) -> Self {
        Self {
            pushed,
            pulled,
            at: chrono::Local::now(),
        }
    }
}

/// Running tunnel.
pub struct RunningForward {
    pub handle: Arc<ForwardHandle>,
    pub bound_port: u16,
    /// Connection that carries it.
    pub conn: Arc<Connection>,
}

impl RunningForward {
    pub fn stats(&self) -> ForwardStats {
        self.handle.stats()
    }
}

/// Messages from the background tasks.
enum BgMsg {
    Synced(Result<SyncSummary, String>),
    Event(Value),
    EventsOnline(bool),
    /// The server answered `email_not_verified`: the account has to confirm
    /// its email before using the server.
    EmailNotVerified,
}

/// Message for the result of a sync.
fn synced(res: termoak_client::Result<(usize, usize)>) -> BgMsg {
    match res {
        Err(e) if e.is_email_not_verified() => BgMsg::EmailNotVerified,
        res => BgMsg::Synced(
            res.map(|(pushed, pulled)| SyncSummary::new(pushed, pulled))
                .map_err(|e| e.to_string()),
        ),
    }
}

/// Data loaded from the local database.
#[derive(Default)]
struct Data {
    hosts: Vec<Record<Host>>,
    groups: Vec<Record<Group>>,
    identities: Vec<Record<Identity>>,
    keys: Vec<Record<SshKey>>,
    snippets: Vec<Record<Snippet>>,
    forwards: Vec<Record<PortForward>>,
    known_hosts: Vec<Record<KnownHost>>,
}

/// Global model (one entity shared by every view).
pub struct AppModel {
    pub ws: Workspace,
    pub settings: Settings,
    pub prompter: Arc<DesktopPrompter>,
    /// Server client, when signed in.
    pub api: Option<ApiClient>,
    pub server_user: Option<String>,
    pub server_url: Option<String>,
    /// Signed in (or registered) to an account that still has to confirm
    /// its email: Settings → Account asks for the code.
    pub pending_verification: Option<PendingVerification>,
    /// Signed-in account (`GET /api/v1/me`): id, name, whether it is a server
    /// administrator and whether it has two-step verification.
    pub me: Option<User>,
    /// Teams you belong to (to share and manage).
    pub teams: Vec<Team>,
    pub hosts: Vec<Record<Host>>,
    pub groups: Vec<Record<Group>>,
    pub identities: Vec<Record<Identity>>,
    pub keys: Vec<Record<SshKey>>,
    pub snippets: Vec<Record<Snippet>>,
    pub forwards: Vec<Record<PortForward>>,
    pub known_hosts: Vec<Record<KnownHost>>,
    pub loaded: bool,
    pub syncing: bool,
    /// Result of the last sync.
    pub last_sync: Option<Result<SyncSummary, String>>,
    /// The server events WebSocket is connected.
    pub events_online: bool,
    pub running_forwards: HashMap<Id, RunningForward>,
    /// Tunnels starting (to disable their button).
    pub starting_forwards: Vec<Id>,
    /// Server sessions with something waiting for you that you are not
    /// watching (someone wants in or asks for the keyboard, a prompt): the
    /// badge of Sessions.
    pub session_alerts: HashMap<Id, usize>,
    bg_tx: mpsc::UnboundedSender<BgMsg>,
    background: Vec<tokio::task::JoinHandle<()>>,
    _bg_task: Task<()>,
}

impl EventEmitter<ModelEvent> for AppModel {}

impl AppModel {
    pub fn new(
        ws: Workspace,
        settings: Settings,
        prompter: Arc<DesktopPrompter>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<BgMsg>();
        let bg_task = cx.spawn(async move |this, cx| {
            while let Some(msg) = bg_rx.recv().await {
                let alive = this.update(cx, |m, cx| m.on_background(msg, cx)).is_ok();
                if !alive {
                    break;
                }
            }
        });
        let mut model = Self {
            ws,
            settings,
            prompter,
            api: None,
            server_user: None,
            server_url: None,
            pending_verification: None,
            me: None,
            teams: Vec::new(),
            hosts: Vec::new(),
            groups: Vec::new(),
            identities: Vec::new(),
            keys: Vec::new(),
            snippets: Vec::new(),
            forwards: Vec::new(),
            known_hosts: Vec::new(),
            loaded: false,
            syncing: false,
            last_sync: None,
            events_online: false,
            running_forwards: HashMap::new(),
            starting_forwards: Vec::new(),
            session_alerts: HashMap::new(),
            bg_tx,
            background: Vec::new(),
            _bg_task: bg_task,
        };
        model.reload(cx);
        model.restore_server(cx);
        model
    }

    // ----- Notifications -----

    /// Something waits for you in a server session you are not watching.
    pub fn add_session_alert(&mut self, session_id: Id, cx: &mut Context<Self>) {
        *self.session_alerts.entry(session_id).or_default() += 1;
        cx.notify();
    }

    /// The session was opened (or ended): nothing waits there any more.
    pub fn clear_session_alert(&mut self, session_id: Id, cx: &mut Context<Self>) {
        if self.session_alerts.remove(&session_id).is_some() {
            cx.notify();
        }
    }

    /// Total for the badge.
    pub fn session_alert_count(&self) -> usize {
        self.session_alerts.values().sum()
    }

    pub fn toast(&self, kind: ToastKind, msg: impl Into<String>, cx: &mut Context<Self>) {
        cx.emit(ModelEvent::Toast(kind, msg.into()));
    }

    // ----- Local data -----

    /// Reloads every list from the local database.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let store = self.ws.store.clone();
        runtime::run(
            cx,
            async move {
                Ok::<_, CoreError>(Data {
                    hosts: store.list::<Host>(LOCAL_OWNER).await?,
                    groups: store.list::<Group>(LOCAL_OWNER).await?,
                    identities: store.list::<Identity>(LOCAL_OWNER).await?,
                    keys: store.list::<SshKey>(LOCAL_OWNER).await?,
                    snippets: store.list::<Snippet>(LOCAL_OWNER).await?,
                    forwards: store.list::<PortForward>(LOCAL_OWNER).await?,
                    known_hosts: store.list::<KnownHost>(LOCAL_OWNER).await?,
                })
            },
            |m, res, cx| match res {
                Ok(d) => {
                    m.hosts = d.hosts;
                    m.groups = d.groups;
                    m.identities = d.identities;
                    m.keys = d.keys;
                    m.snippets = d.snippets;
                    m.forwards = d.forwards;
                    m.known_hosts = d.known_hosts;
                    m.hosts.sort_by_key(|h| h.data.label.to_lowercase());
                    m.groups.sort_by_key(|g| g.data.name.to_lowercase());
                    m.loaded = true;
                    cx.notify();
                }
                Err(e) => m.toast(
                    ToastKind::Error,
                    t!("state.load_failed", error = e).to_string(),
                    cx,
                ),
            },
        );
    }

    /// Saves (creates or updates) an entity and reloads.
    pub fn save<T: CoreEntity>(
        &mut self,
        data: T,
        secret: SecretUpdate<T::Secret>,
        mode: Option<SyncMode>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Record<T>, String>> {
        let store = self.ws.store.clone();
        let fut = runtime::spawn(cx, async move {
            store.save(LOCAL_OWNER, data, secret, mode).await
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| {
                    m.reload(cx);
                    m.sync_soon(cx);
                });
            }
            res
        })
    }

    /// Saves a host changing only the given secrets (`None`: keep;
    /// `Some(None)`: delete). The SSH and proxy passwords share one secret,
    /// so they are merged with what is stored.
    pub fn save_host(
        &mut self,
        host: Host,
        password: Option<Option<String>>,
        proxy_password: Option<Option<String>>,
        mode: Option<SyncMode>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Record<Host>, String>> {
        let store = self.ws.store.clone();
        let fut = runtime::spawn(cx, async move {
            let secret = if password.is_none() && proxy_password.is_none() {
                SecretUpdate::Keep
            } else {
                let mut s = if host.id.is_nil() {
                    HostSecret::default()
                } else {
                    store
                        .secret::<Host>(LOCAL_OWNER, host.id)
                        .await
                        .unwrap_or_default()
                };
                if let Some(p) = password {
                    s.password = p;
                }
                if let Some(p) = proxy_password {
                    s.proxy_password = p;
                }
                if s.password.is_none() && s.proxy_password.is_none() {
                    SecretUpdate::Clear
                } else {
                    SecretUpdate::Set(s)
                }
            };
            store.save(LOCAL_OWNER, host, secret, mode).await
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| {
                    m.reload(cx);
                    m.sync_soon(cx);
                });
            }
            res
        })
    }

    /// Deletes an entity and reloads.
    pub fn delete<T: CoreEntity>(
        &mut self,
        id: Id,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let store = self.ws.store.clone();
        let fut = runtime::spawn(cx, async move { store.delete::<T>(LOCAL_OWNER, id).await });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| {
                    m.reload(cx);
                    m.sync_soon(cx);
                });
            }
            res
        })
    }

    /// Copies a host (with its password, in the same sync mode) as
    /// "<label> (copy)".
    pub fn duplicate_host(
        &mut self,
        id: Id,
        cx: &mut Context<Self>,
    ) -> Task<Result<Record<Host>, String>> {
        let Some(rec) = self.host_record(id).cloned() else {
            return Task::ready(Err(t!("hosts.error.not_found").to_string()));
        };
        let store = self.ws.store.clone();
        let label = t!("hosts.copy_label", name = rec.data.label).to_string();
        let fut = runtime::spawn(cx, async move {
            let secret = if rec.meta.has_secret {
                SecretUpdate::Set(store.secret::<Host>(LOCAL_OWNER, rec.data.id).await?)
            } else {
                SecretUpdate::Keep
            };
            let mut host = rec.data.clone();
            host.id = Id::nil();
            host.label = label;
            host.favorite = false;
            store
                .save(LOCAL_OWNER, host, secret, Some(rec.meta.sync_mode))
                .await
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| {
                    m.reload(cx);
                    m.sync_soon(cx);
                });
            }
            res
        })
    }

    /// Moves hosts to a group (`None`: no group).
    pub fn move_hosts(
        &mut self,
        ids: Vec<Id>,
        group: Option<Id>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let hosts: Vec<Host> = self
            .hosts
            .iter()
            .filter(|h| ids.contains(&h.data.id) && h.data.group_id != group)
            .map(|h| {
                let mut host = h.data.clone();
                host.group_id = group;
                host
            })
            .collect();
        let store = self.ws.store.clone();
        let fut = runtime::spawn(cx, async move {
            for host in hosts {
                store
                    .save(LOCAL_OWNER, host, SecretUpdate::Keep, None)
                    .await?;
            }
            Ok::<_, CoreError>(())
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |m, cx| {
                m.reload(cx);
                m.sync_soon(cx);
            });
            res
        })
    }

    /// Deletes several hosts.
    pub fn delete_hosts(
        &mut self,
        ids: Vec<Id>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let store = self.ws.store.clone();
        let fut = runtime::spawn(cx, async move {
            for id in ids {
                store.delete::<Host>(LOCAL_OWNER, id).await?;
            }
            Ok::<_, CoreError>(())
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |m, cx| {
                m.reload(cx);
                m.sync_soon(cx);
            });
            res
        })
    }

    /// Effective SSH user of a host: its own, its group's or its identity's.
    pub fn effective_user(&self, host: &Host) -> Option<String> {
        host.settings
            .username
            .clone()
            .or_else(|| {
                host.group_id
                    .and_then(|g| self.groups.iter().find(|x| x.data.id == g))
                    .and_then(|g| g.data.settings.username.clone())
            })
            .or_else(|| {
                host.settings
                    .identity_id
                    .and_then(|i| self.identities.iter().find(|x| x.data.id == i))
                    .map(|i| i.data.username.clone())
            })
    }

    /// Effective SSH port of a host: its own, its group's or 22.
    pub fn effective_port(&self, host: &Host) -> u16 {
        host.settings
            .port
            .or_else(|| {
                host.group_id
                    .and_then(|g| self.groups.iter().find(|x| x.data.id == g))
                    .and_then(|g| g.data.settings.port)
            })
            .unwrap_or(22)
    }

    /// Saves the preferences.
    pub fn save_settings(&mut self, settings: Settings, cx: &mut Context<Self>) {
        self.settings = settings.clone();
        cx.notify();
        let store = self.ws.store.clone();
        let json = serde_json::to_string(&settings).unwrap_or_default();
        runtime::run(
            cx,
            async move { store.meta_set(SETTINGS_KEY, &json).await },
            |m, res, cx| {
                if let Err(e) = res {
                    m.toast(
                        ToastKind::Error,
                        t!("state.settings_save_failed", error = e).to_string(),
                        cx,
                    );
                }
            },
        );
    }

    /// Changes the interface language (`None` = the system language): saves
    /// the choice, re-renders the windows in the new language and, when
    /// signed in, saves it in the account too.
    pub fn set_language(&mut self, choice: Option<String>, cx: &mut Context<Self>) {
        crate::i18n::apply(choice.as_deref());
        let mut settings = self.settings.clone();
        settings.language = choice;
        self.save_settings(settings, cx);
        crate::app::set_menus(cx);
        cx.refresh_windows();
        self.sync_locale(cx);
    }

    /// Saves the interface language in the server account
    /// (`PATCH /api/v1/me`), which the server uses for emails.
    pub fn sync_locale(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let locale = crate::i18n::current();
        if self.me.as_ref().is_some_and(|u| u.locale == locale) {
            return;
        }
        runtime::run(
            cx,
            async move { api.set_locale(&locale).await.map_err(api_error) },
            |m, res, cx| match res {
                Ok(user) if m.api.is_some() => {
                    m.me = Some(user);
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not save the language in the account"),
            },
        );
    }

    /// Where the AI runs now (the server only when signed in).
    pub fn ai_run_on(&self) -> RunOn {
        self.settings.ai.run_on(self.logged_in())
    }

    /// Saves the AI preferences of this device.
    pub fn set_ai_settings(&mut self, ai: AiSettings, cx: &mut Context<Self>) {
        let mut settings = self.settings.clone();
        settings.ai = ai;
        self.save_settings(settings, cx);
    }

    pub fn host(&self, id: Id) -> Option<&Host> {
        self.hosts.iter().find(|h| h.data.id == id).map(|h| &h.data)
    }

    pub fn host_record(&self, id: Id) -> Option<&Record<Host>> {
        self.hosts.iter().find(|h| h.data.id == id)
    }

    /// Label of a host (or its id if it no longer exists).
    pub fn host_label(&self, id: Id) -> String {
        self.host(id)
            .map(|h| h.label.clone())
            .unwrap_or_else(|| id.to_string())
    }

    // ----- Server -----

    pub fn logged_in(&self) -> bool {
        self.api.is_some()
    }

    /// Is the account a server administrator?
    pub fn is_admin(&self) -> bool {
        self.api.is_some() && self.me.as_ref().is_some_and(|u| u.is_admin)
    }

    /// Your effective role in a team: yours or, if you administer the
    /// server, owner (the server lets you do the same).
    pub fn team_role(&self, team: &Team) -> Option<TeamRole> {
        if self.is_admin() {
            Some(TeamRole::Owner)
        } else {
            team.role
        }
    }

    /// Reads the signed-in account again.
    pub fn refresh_me(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        runtime::run(
            cx,
            async move { api.get::<Value>("/api/v1/me").await.map_err(api_error) },
            |m, res, cx| match res.and_then(|v| {
                serde_json::from_value::<User>(v["user"].clone()).map_err(|e| e.to_string())
            }) {
                Ok(user) if m.api.is_some() => {
                    m.me = Some(user);
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not read the server account"),
            },
        );
    }

    /// Reads your teams again.
    pub fn refresh_teams(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        runtime::run(
            cx,
            async move {
                api.get::<Vec<Team>>("/api/v1/teams")
                    .await
                    .map_err(api_error)
            },
            |m, res, cx| match res {
                Ok(teams) if m.api.is_some() => {
                    m.teams = teams;
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not read the teams"),
            },
        );
    }

    /// Restores the saved session with the server at startup.
    fn restore_server(&mut self, cx: &mut Context<Self>) {
        let ws = self.ws.clone();
        runtime::run(
            cx,
            async move {
                let api = ws.server().await?;
                let user = ws.server_user().await?;
                let url = ws.store.meta_get("server.url").await?;
                Ok::<_, termoak_client::ClientError>((api, user, url))
            },
            |m, res, cx| match res {
                Ok((api, user, url)) => {
                    m.server_url = url;
                    m.server_user = user;
                    if let Some(api) = api {
                        m.set_api(api, cx);
                    }
                    cx.notify();
                }
                Err(e) => tracing::warn!(error = %e, "could not restore the server session"),
            },
        );
    }

    fn set_api(&mut self, api: ApiClient, cx: &mut Context<Self>) {
        self.stop_background();
        self.api = Some(api.clone());
        self.server_url = Some(api.base_url().to_string());
        let handle = runtime::handle(cx);
        // Periodic sync.
        let engine = SyncEngine::new(self.ws.store.clone(), api.clone());
        let tx = self.bg_tx.clone();
        self.background.push(handle.spawn(async move {
            let mut tick = tokio::time::interval(SYNC_EVERY);
            loop {
                tick.tick().await;
                let res = engine.sync_once().await.map(|r| (r.pushed, r.pulled));
                if tx.send(synced(res)).is_err() {
                    break;
                }
            }
        }));
        // Server events (AI, shared sessions...), reconnecting.
        let tx = self.bg_tx.clone();
        self.background.push(handle.spawn(async move {
            events_loop(api, tx).await;
        }));
        self.syncing = true;
        self.refresh_me(cx);
        self.refresh_teams(cx);
        cx.emit(ModelEvent::SessionChanged);
        cx.notify();
    }

    fn stop_background(&mut self) {
        for task in self.background.drain(..) {
            task.abort();
        }
        self.events_online = false;
    }

    fn on_background(&mut self, msg: BgMsg, cx: &mut Context<Self>) {
        match msg {
            BgMsg::Synced(res) => {
                self.syncing = false;
                if let Err(e) = &res {
                    tracing::warn!(error = %e, "sync failed");
                }
                let ok = res.is_ok();
                self.last_sync = Some(res);
                if ok {
                    self.reload(cx);
                }
                cx.notify();
            }
            BgMsg::Event(v) => cx.emit(ModelEvent::Server(v)),
            BgMsg::EventsOnline(online) => {
                self.events_online = online;
                cx.notify();
            }
            BgMsg::EmailNotVerified => self.email_not_verified(cx),
        }
    }

    /// The server says that the signed-in account has not confirmed its
    /// email (e.g. a session from before the server required it): the
    /// session is left and Settings → Account asks for the code.
    fn email_not_verified(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let Some(email) = self
            .server_user
            .clone()
            .or_else(|| self.me.as_ref().map(|u| u.email.clone()))
        else {
            return;
        };
        tracing::info!("the account has to confirm its email");
        self.start_verification(api.base_url().to_string(), email, None, cx);
        // The restricted tokens are useless: the code signs in again.
        let ws = self.ws.clone();
        runtime::run(cx, async move { ws.logout().await }, |_, res, _| {
            if let Err(e) = res {
                tracing::warn!(error = %e, "could not discard the restricted session");
            }
        });
        self.toast(ToastKind::Warning, t!("state.verify_email"), cx);
        cx.emit(ModelEvent::SessionChanged);
    }

    /// Leaves the session (if any) and waits for the email verification
    /// code of `email` on `url`.
    fn start_verification(
        &mut self,
        url: String,
        email: String,
        resend_wait: Option<Duration>,
        cx: &mut Context<Self>,
    ) {
        self.stop_background();
        self.api = None;
        self.me = None;
        self.teams.clear();
        self.syncing = false;
        self.last_sync = None;
        self.server_url = Some(url.clone());
        self.server_user = Some(email.clone());
        self.pending_verification = Some(PendingVerification {
            url,
            email,
            resend_at: resend_wait.map(|wait| Instant::now() + wait),
        });
        cx.notify();
    }

    /// Verifies the email with the code from the verification email and
    /// signs in (sync starts). If the account has two-step verification and
    /// `totp` is missing, returns [`LoginError::TotpRequired`].
    pub fn verify_email_code(
        &mut self,
        code: String,
        totp: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), LoginError>> {
        let Some(pending) = self.pending_verification.clone() else {
            return Task::ready(Err(LoginError::Failed(
                t!("error.not_logged_in").to_string(),
            )));
        };
        let ws = self.ws.clone();
        let code = clean_email_code(&code);
        let (url, email) = (pending.url.clone(), pending.email.clone());
        let fut = runtime::spawn(cx, async move {
            let res = ws
                .verify_code(&url, &email, &code, totp.as_deref())
                .await
                .map_err(LoginError::from);
            Ok::<_, std::convert::Infallible>(res)
        });
        cx.spawn(async move |this, cx| {
            let api = fut.await.map_err(LoginError::Failed)??;
            let _ = this.update(cx, |m, cx| {
                m.pending_verification = None;
                m.signed_in(api, pending.email, cx);
            });
            Ok(())
        })
    }

    /// Asks the server to email a new verification code. On success,
    /// "Resend" waits as long as the server says.
    pub fn resend_email_code(&mut self, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        let Some(pending) = self.pending_verification.clone() else {
            return Task::ready(Err(t!("error.not_logged_in").to_string()));
        };
        let fut = runtime::spawn(cx, async move {
            let api = ApiClient::new(&pending.url)?;
            let res = api
                .post_public::<Value>(
                    "/api/v1/auth/resend-code",
                    &serde_json::json!({ "email": pending.email }),
                )
                .await;
            Ok::<_, ClientError>(res)
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await?;
            let wait = match &res {
                Ok(v) => v["resend_after"]
                    .as_u64()
                    .map_or(RESEND_WAIT, Duration::from_secs),
                // Asked too often: the client does not get `retry_after`, so
                // wait the usual minute before offering it again.
                Err(e) if e.api_code() == Some("too_many_attempts") => RESEND_WAIT,
                Err(_) => Duration::ZERO,
            };
            let _ = this.update(cx, |m, cx| {
                if let Some(p) = m.pending_verification.as_mut() {
                    p.resend_at = Some(Instant::now() + wait);
                    cx.notify();
                }
            });
            res.map(|_| ()).map_err(api_error)
        })
    }

    /// Gives up the pending verification ("Use a different email"): back to
    /// the sign-in form.
    pub fn cancel_verification(&mut self, cx: &mut Context<Self>) {
        if self.pending_verification.take().is_some() {
            cx.notify();
        }
    }

    /// Signs in or creates the account. If the account has two-step
    /// verification and the code is missing, returns
    /// [`LoginError::TotpRequired`]. If the server requires a verified email
    /// that the account has not confirmed yet, returns
    /// [`LoginOutcome::VerifyEmail`] without signing in.
    pub fn login(
        &mut self,
        url: String,
        email: String,
        password: String,
        request: LoginRequest,
        cx: &mut Context<Self>,
    ) -> Task<Result<LoginOutcome, LoginError>> {
        let ws = self.ws.clone();
        let email_task = email.clone();
        let registering = matches!(request, LoginRequest::Register { .. });
        let fut = runtime::spawn(cx, async move {
            let email = email_task;
            let res = match request {
                LoginRequest::Login { totp } => {
                    ws.login_with_code(&url, &email, &password, totp.as_deref())
                        .await
                }
                LoginRequest::Register { name, invite } => {
                    ws.register_with_invite(&url, &email, &name, &password, invite.as_deref())
                        .await
                }
            };
            let api = match res {
                Ok(api) => api,
                // The error keeps its type (to know whether the code is missing).
                Err(e) => return Ok(Err(LoginError::from(e))),
            };
            // Until the email is confirmed the tokens only reach the account
            // itself: drop them and sign in with the code instead.
            let verify = match api.verification_required().await {
                Ok(verify) => verify,
                Err(e) => {
                    tracing::warn!(error = %e, "could not check the email verification");
                    false
                }
            };
            if verify && let Err(e) = ws.logout().await {
                tracing::warn!(error = %e, "could not discard the restricted session");
            }
            Ok::<_, std::convert::Infallible>(Ok((api, verify)))
        });
        cx.spawn(async move |this, cx| {
            let (api, verify) = fut.await.map_err(LoginError::Failed)??;
            let _ = this.update(cx, |m, cx| {
                if verify {
                    // Creating the account sent the first code just now.
                    let wait = registering.then_some(RESEND_WAIT);
                    m.start_verification(api.base_url().to_string(), email, wait, cx);
                } else {
                    m.pending_verification = None;
                    m.signed_in(api, email, cx);
                }
            });
            Ok(if verify {
                LoginOutcome::VerifyEmail
            } else {
                LoginOutcome::SignedIn
            })
        })
    }

    /// Starts the session with the server (and its sync) after signing in.
    fn signed_in(&mut self, api: ApiClient, email: String, cx: &mut Context<Self>) {
        self.server_user = Some(email.clone());
        self.set_api(api, cx);
        // Periodic sync starts right away with `set_api`.
        self.toast(
            ToastKind::Success,
            t!("state.signed_in", email = email).to_string(),
            cx,
        );
        // A language picked explicitly goes to the account (emails).
        if self.settings.language.is_some() {
            self.sync_locale(cx);
        }
    }

    /// Signs out of the server (local data is kept).
    pub fn logout(&mut self, cx: &mut Context<Self>) {
        self.stop_background();
        self.api = None;
        self.pending_verification = None;
        self.me = None;
        self.teams.clear();
        self.last_sync = None;
        self.session_alerts.clear();
        let ws = self.ws.clone();
        runtime::run(cx, async move { ws.logout().await }, |m, res, cx| {
            match res {
                Ok(()) => m.toast(ToastKind::Info, t!("state.signed_out"), cx),
                Err(e) => m.toast(
                    ToastKind::Warning,
                    t!("state.signed_out_with_warnings", error = e).to_string(),
                    cx,
                ),
            }
            cx.emit(ModelEvent::SessionChanged);
            cx.notify();
        });
        cx.emit(ModelEvent::SessionChanged);
        cx.notify();
    }

    /// Syncs with the server now.
    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        self.syncing = true;
        cx.notify();
        let engine = SyncEngine::new(self.ws.store.clone(), api);
        let tx = self.bg_tx.clone();
        runtime::handle(cx).spawn(async move {
            let res = engine.sync_once().await.map(|r| (r.pushed, r.pulled));
            let _ = tx.send(synced(res));
        });
    }

    /// Reloads and syncs after a change made outside the model (e.g. when
    /// importing an `ssh_config`).
    pub fn data_changed(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        self.sync_soon(cx);
    }

    /// Syncs shortly after a local change (if there is a server).
    fn sync_soon(&mut self, cx: &mut Context<Self>) {
        if self.api.is_some() && !self.syncing {
            self.sync_now(cx);
        }
    }

    // ----- Tunnels -----

    /// Starts a saved tunnel. Reuses `conn` if given (e.g. that of a terminal
    /// open to that host); otherwise it connects.
    pub fn start_forward(
        &mut self,
        forward: PortForward,
        conn: Option<Arc<Connection>>,
        cx: &mut Context<Self>,
    ) {
        if self.running_forwards.contains_key(&forward.id)
            || self.starting_forwards.contains(&forward.id)
        {
            return;
        }
        self.starting_forwards.push(forward.id);
        cx.notify();
        let ws = self.ws.clone();
        let prompter = self.prompter.clone();
        let use_agent = self.settings.use_agent;
        let id = forward.id;
        let label = forward.label.clone();
        let conn = conn.filter(|c| !c.is_closed());
        runtime::run(
            cx,
            async move {
                let conn = match conn {
                    Some(c) => c,
                    None => ws.connect(forward.host_id, prompter, use_agent).await?,
                };
                let handle = conn
                    .start_forward(ForwardSpec::from(&forward))
                    .await
                    .map_err(termoak_client::ClientError::from)?;
                Ok::<_, termoak_client::ClientError>((conn, handle))
            },
            move |m, res, cx| {
                m.starting_forwards.retain(|f| *f != id);
                match res {
                    Ok((conn, handle)) => {
                        let bound_port = handle.bound_port;
                        m.running_forwards.insert(
                            id,
                            RunningForward {
                                handle: Arc::new(handle),
                                bound_port,
                                conn,
                            },
                        );
                        m.toast(
                            ToastKind::Success,
                            t!("state.forward_started", label = label, port = bound_port)
                                .to_string(),
                            cx,
                        );
                    }
                    Err(e) => m.toast(
                        ToastKind::Error,
                        t!("state.forward_failed", label = label, error = e).to_string(),
                        cx,
                    ),
                }
                cx.notify();
            },
        );
    }

    /// Starts a host's automatic tunnels over an already open connection.
    pub fn start_auto_forwards(
        &mut self,
        host_id: Id,
        conn: Arc<Connection>,
        cx: &mut Context<Self>,
    ) {
        let pending: Vec<PortForward> = self
            .forwards
            .iter()
            .filter(|f| f.data.host_id == host_id && f.data.auto_start)
            .filter(|f| !self.running_forwards.contains_key(&f.data.id))
            .map(|f| f.data.clone())
            .collect();
        for f in pending {
            self.start_forward(f, Some(conn.clone()), cx);
        }
    }

    /// Stops a running tunnel.
    pub fn stop_forward(&mut self, id: Id, cx: &mut Context<Self>) {
        if let Some(running) = self.running_forwards.remove(&id) {
            // Dropped inside tokio: closing a remote tunnel needs it.
            runtime::handle(cx).spawn(async move {
                let RunningForward { handle, conn, .. } = running;
                if let Ok(handle) = Arc::try_unwrap(handle) {
                    handle.stop().await;
                }
                drop(conn);
            });
            cx.notify();
        }
    }
}

impl Drop for AppModel {
    fn drop(&mut self) {
        self.stop_background();
    }
}

/// Keeps the events WebSocket open and forwards what arrives.
async fn events_loop(api: ApiClient, tx: mpsc::UnboundedSender<BgMsg>) {
    let mut backoff = 2u64;
    loop {
        match api.websocket("/api/v1/events/ws").await {
            Ok(ws) => {
                backoff = 2;
                let _ = tx.send(BgMsg::EventsOnline(true));
                let (_sink, mut stream) = ws.split();
                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&text)
                                && tx.send(BgMsg::Event(v)).is_err()
                            {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let _ = tx.send(BgMsg::EventsOnline(false));
            }
            Err(e) => {
                tracing::debug!(error = %e, "events WebSocket not available");
                if e.is_email_not_verified() {
                    let _ = tx.send(BgMsg::EmailNotVerified);
                    return;
                }
                if matches!(
                    e,
                    termoak_client::ClientError::NotLoggedIn
                        | termoak_client::ClientError::SessionExpired
                ) {
                    return;
                }
            }
        }
        if tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_codes_keep_only_six_digits() {
        assert_eq!(clean_email_code("123456"), "123456");
        assert_eq!(clean_email_code(" 123 456 "), "123456");
        assert_eq!(clean_email_code("123-456"), "123456");
        assert_eq!(clean_email_code("Code: 1234567"), "123456");
        assert_eq!(clean_email_code("abc"), "");
    }

    #[test]
    fn resend_countdown() {
        let mut p = PendingVerification {
            url: "https://termoak.com".into(),
            email: "ana@example.com".into(),
            resend_at: None,
        };
        assert_eq!(p.resend_wait(), 0);
        p.resend_at = Some(Instant::now() + Duration::from_millis(59_500));
        assert_eq!(p.resend_wait(), 60);
        p.resend_at = Some(Instant::now() - Duration::from_secs(1));
        assert_eq!(p.resend_wait(), 0);
    }

    #[test]
    fn old_settings_files_keep_loading() {
        // Saved before the AI section existed.
        let s: Settings = serde_json::from_str(
            r#"{"dark":false,"use_agent":true,"font_size":15.0,"font_family":"","scrollback":5000,"autocomplete":true,"language":"es"}"#,
        )
        .unwrap();
        assert!(!s.dark);
        assert_eq!(s.language.as_deref(), Some("es"));
        assert_eq!(s.ai, AiSettings::default());
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.font_size, 14.0);
        assert_eq!(s.ai, AiSettings::default());

        let mut s = Settings::default();
        s.ai.run_on = Some(RunOn::Local);
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.ai.run_on, Some(RunOn::Local));
    }
}
