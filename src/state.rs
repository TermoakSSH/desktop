//! App model: the local workspace (This device and one store per signed-in
//! account), the accounts with their sync and events, the cached data
//! (hosts, keychain, snippets, tunnels...) of the accounts in sight and the
//! background tasks.
//!
//! Lists hold [`Item`]s: a record plus where it lives ([`Scope`]: This
//! device or an account) and what the user can do with it
//! ([`ItemAccess`]). Saving and deleting go to the item's own store; new
//! items go to the vault chosen in the picker, the last used vault or the
//! personal vault of the current account (This device without accounts).

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{Context, EventEmitter, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use termoak_client::sync::SyncReport;
use termoak_client::{
    Account, AccountInfo, AccountStatus, AccountView, ApiClient, ClientError, ItemAccess,
    ItemFilter, ItemRef, LocalTransfer, SaveTarget, Scope, Scoped, ServerChoice, SignOutReport,
    Workspace,
};
use termoak_core::Id;
use termoak_core::model::{
    Entity as CoreEntity, EntityKind, Group, Host, HostSecret, Identity, KnownHost, PortForward,
    Record, SecretUpdate, Snippet, SshKey, SyncMode, Team, TeamRole, User,
};
use termoak_core::transfer::{Dependencies, TransferMode, TransferResult};
use termoak_ssh::{Connection, ForwardHandle, ForwardSpec, ForwardStats};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::accounts::{self, Caps, NoticeLevel, VaultEntry, VaultFilter, ViewMode};
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
/// Meta key of the device store set by the data layout migration (0.3 → 0.4).
const LAYOUT_BACKUP_KEY: &str = "layout.backup_at";

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
    /// The switcher shows only This device items (the account or "all
    /// accounts" choice is kept by the workspace).
    pub device_view: bool,
    /// Last vault used for new items, per account.
    pub last_vaults: BTreeMap<Id, Id>,
    /// The notice about the data moved to one store per account was shown.
    pub layout_notice_seen: bool,
    /// How many server sessions were running when the Home notice about
    /// them was closed (it comes back when there are more).
    pub cloud_notice_dismissed: usize,
    /// The hosts list shows whether each host answers (a TCP check of its
    /// SSH port every minute while the list is on screen).
    pub host_status: bool,
    /// Hosts whose status is not checked (on this device).
    pub host_status_off: Vec<Id>,
    /// The tabs of the previous session open again at start.
    pub reopen_tabs: bool,
    /// Touch ID / Windows Hello to open the app (Settings → General).
    pub lock: crate::app_lock::LockSettings,
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
            device_view: false,
            last_vaults: BTreeMap::new(),
            layout_notice_seen: false,
            cloud_notice_dismissed: 0,
            host_status: true,
            host_status_off: Vec::new(),
            reopen_tabs: true,
            lock: crate::app_lock::LockSettings::default(),
        }
    }
}

/// The Home notice about your sessions running on the server is shown:
/// there are some, and more than when it was closed.
pub fn cloud_notice_visible(running: usize, dismissed: usize) -> bool {
    running > 0 && running > dismissed
}

/// The count to keep after the server says `running` sessions are running,
/// if it changes: when sessions end it goes down with them, so the notice
/// comes back as soon as a new one starts (closed with 3, down to 1, a new
/// one makes 2: shown).
pub fn cloud_notice_lowered(running: usize, dismissed: usize) -> Option<usize> {
    (running < dismissed).then_some(running)
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
    /// Server WebSocket event (`/api/v1/events/ws`), with `"account_id"`.
    Server(Value),
    /// The current account changed (sign-in, sign-out, switcher).
    SessionChanged,
    /// First start after the data moved to one store per account
    /// ([`AppModel::take_layout_notice`]).
    LayoutMigrated,
}

/// An item with where it lives and what the user can do with it. It
/// dereferences to its record (`item.data`, `item.meta`).
#[derive(Debug, Clone)]
pub struct Item<T> {
    pub scope: Scope,
    pub access: ItemAccess,
    pub rec: Record<T>,
}

impl<T> Deref for Item<T> {
    type Target = Record<T>;

    fn deref(&self) -> &Record<T> {
        &self.rec
    }
}

impl<T: CoreEntity> Item<T> {
    /// A This device item.
    #[cfg(test)]
    pub fn device(rec: Record<T>) -> Self {
        Self {
            scope: Scope::Device,
            access: ItemAccess::Device,
            rec,
        }
    }

    pub fn item_ref(&self) -> ItemRef {
        ItemRef {
            scope: self.scope,
            id: self.rec.data.id(),
        }
    }

    /// Account of the item (`None`: This device).
    pub fn account(&self) -> Option<Id> {
        self.scope.account()
    }
}

impl<T> From<Scoped<T>> for Item<T> {
    fn from(s: Scoped<T>) -> Self {
        Self {
            scope: s.scope,
            access: s.access,
            rec: s.record,
        }
    }
}

/// An account that has to confirm its email with the six-digit code from the
/// verification email before using its server (docs/API.md, "Email
/// verification").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVerification {
    pub account: Id,
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

/// How a successful sign-in ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// Signed in: the account is current and its first sync started.
    /// `first`: it is the only account (offer to upload This device items).
    SignedIn { account: Id, first: bool },
    /// The server requires a verified email and this account has not
    /// verified it: the code from the email is needed
    /// ([`AppModel::pending_verification`]).
    VerifyEmail(Id),
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

/// Stable code of a client error: the server's (`Api`) or a vault rule of
/// the client (`use_only_needs_server`, `vault_read_only`...).
pub fn error_code(e: &ClientError) -> Option<String> {
    match e {
        ClientError::Api { code, .. } => Some(code.clone()),
        ClientError::Core(c) => c.vault_code().map(str::to_string),
        _ => None,
    }
}

/// Text of a client error for the interface. Server errors show only their
/// explanation (without the HTTP status): the translation of their stable
/// `code` (`error.<code>`, see docs/API.md) or, in English or for codes
/// without one, the server's own English message, which may be more
/// specific (for example, the limit of the plan). Vault rules checked on
/// this device (Use-only, read-only vaults) are always translated.
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
        ClientError::Core(c) => match c.vault_code().and_then(crate::i18n::api_error_text) {
            Some(text) => text,
            None => c.to_string(),
        },
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

/// A failed transfer: the text, and whether it was refused because items
/// that stay still use what is moved (`still_referenced`: "Move anyway"
/// detaches them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferError {
    pub text: String,
    pub still_referenced: bool,
}

impl From<ClientError> for TransferError {
    fn from(e: ClientError) -> Self {
        let still_referenced =
            error_code(&e).as_deref() == Some(termoak_core::error::codes::STILL_REFERENCED);
        Self {
            text: api_error(e),
            still_referenced,
        }
    }
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

/// A signed-in account as the interface shows it.
#[derive(Debug, Clone)]
pub struct AccountModel {
    pub info: AccountInfo,
    /// It has a session with its server.
    pub signed_in: bool,
    /// Its vaults (last sync), personal first.
    pub vaults: Vec<VaultEntry>,
    pub syncing: bool,
    /// Result of the last sync in this run.
    pub last_sync: Option<Result<SyncSummary, String>>,
    /// Its events WebSocket is connected.
    pub events_online: bool,
    /// Local changes not uploaded yet.
    pub unsynced: usize,
}

impl AccountModel {
    pub fn id(&self) -> Id {
        self.info.id
    }

    /// Signed in and verified: it syncs and its server can be used.
    pub fn active(&self) -> bool {
        self.signed_in && self.info.status == AccountStatus::Active
    }

    pub fn vaults_supported(&self) -> bool {
        self.info.vaults_supported()
    }

    /// Id of its personal vault (the user's id on the server).
    pub fn personal(&self) -> Option<Id> {
        self.info.user_id
    }

    pub fn vault(&self, id: Id) -> Option<&VaultEntry> {
        self.vaults.iter().find(|v| v.id() == id)
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

/// Messages from the background tasks (per account).
enum BgMsg {
    Synced(Id, Result<SyncReport, ClientError>),
    Event(Id, Value),
    EventsOnline(Id, bool),
    /// The server answered `email_not_verified` to the account's session.
    EmailNotVerified(Id),
}

/// Data loaded from the stores in sight.
#[derive(Default)]
struct Data {
    hosts: Vec<Item<Host>>,
    groups: Vec<Item<Group>>,
    identities: Vec<Item<Identity>>,
    keys: Vec<Item<SshKey>>,
    snippets: Vec<Item<Snippet>>,
    forwards: Vec<Item<PortForward>>,
    known_hosts: Vec<Item<KnownHost>>,
    /// Per account: its vaults and its pending changes.
    accounts: Vec<(Id, Vec<termoak_core::model::Vault>, usize)>,
}

async fn list<T: CoreEntity>(
    ws: &Workspace,
    filter: &ItemFilter,
    first: Option<Id>,
) -> Result<Vec<Item<T>>, ClientError> {
    let items = ws
        .list_items::<T>(filter)
        .await?
        .into_iter()
        .map(Item::from)
        .collect();
    Ok(dedupe(items, first))
}

/// The same item seen through two accounts (both members of one vault)
/// is listed once: the copy of `first` (the current account) wins, then
/// the first account in order. The interface finds items by id, so two
/// rows with one id would act on the wrong copy.
pub fn dedupe<T: CoreEntity>(items: Vec<Item<T>>, first: Option<Id>) -> Vec<Item<T>> {
    let mut out: Vec<Item<T>> = Vec::with_capacity(items.len());
    for item in items {
        let id = item.data.id();
        match out.iter().position(|o| o.data.id() == id) {
            None => out.push(item),
            Some(ix) if item.account().is_some() && item.account() == first => out[ix] = item,
            Some(_) => {}
        }
    }
    out
}

/// Changes some fields of a stored item and leaves the rest as they are
/// now: it is read again from its store first, because the lists in memory
/// can be behind (the operating system detected when connecting is saved
/// straight to the store, for example). It stays in the same place.
pub async fn update_stored<T: CoreEntity>(
    ws: &Workspace,
    item: ItemRef,
    change: impl FnOnce(&mut T),
) -> Result<Scoped<T>, ClientError> {
    let current = ws.get_item::<T>(item).await?;
    let target = match current.scope {
        Scope::Device => SaveTarget::Device,
        Scope::Account(account) => SaveTarget::Account {
            account,
            vault: None,
        },
    };
    let mut data = current.record.data;
    change(&mut data);
    ws.save_item(target, data, SecretUpdate::Keep, None).await
}

/// Global model (one entity shared by every view).
pub struct AppModel {
    pub ws: Workspace,
    pub settings: Settings,
    pub prompter: Arc<DesktopPrompter>,
    /// Server client of the current account, when signed in.
    pub api: Option<ApiClient>,
    /// Email and server of the current account.
    pub server_user: Option<String>,
    pub server_url: Option<String>,
    /// The current account (the one of the view, or the first active one in
    /// "all accounts").
    pub current_account: Option<Id>,
    /// An account created or signed in that still has to confirm its email.
    pub pending_verification: Option<PendingVerification>,
    /// The current account (`GET /api/v1/me`): id, name, whether it is a
    /// server administrator and whether it has two-step verification.
    pub me: Option<User>,
    /// Teams of the current account (to share and manage).
    pub teams: Vec<Team>,
    /// Every account signed in on this device, in order.
    pub accounts: Vec<AccountModel>,
    /// What the switcher shows.
    pub view: ViewMode,
    /// The vault picker.
    pub vault_filter: VaultFilter,
    /// Account whose server AI the AI panel uses (`None`: the current one).
    pub ai_account: Option<Id>,
    /// The notice about the data moved to one store per account is waiting
    /// to be shown.
    layout_notice: bool,
    /// Number of the latest reload.
    reload_gen: u64,
    /// Accounts to sync again when their running sync ends (a change was
    /// made meanwhile).
    sync_again: std::collections::HashSet<Id>,
    pub hosts: Vec<Item<Host>>,
    pub groups: Vec<Item<Group>>,
    pub identities: Vec<Item<Identity>>,
    pub keys: Vec<Item<SshKey>>,
    pub snippets: Vec<Item<Snippet>>,
    pub forwards: Vec<Item<PortForward>>,
    pub known_hosts: Vec<Item<KnownHost>>,
    pub loaded: bool,
    /// The current account is syncing.
    pub syncing: bool,
    /// Result of the last sync of the current account.
    pub last_sync: Option<Result<SyncSummary, String>>,
    /// The events WebSocket of the current account is connected.
    pub events_online: bool,
    pub running_forwards: HashMap<Id, RunningForward>,
    /// Tunnels starting (to disable their button).
    pub starting_forwards: Vec<Id>,
    /// Server sessions with something waiting for you that you are not
    /// watching (someone wants in or asks for the keyboard, a prompt): the
    /// badge of Sessions.
    pub session_alerts: HashMap<Id, usize>,
    bg_tx: mpsc::UnboundedSender<BgMsg>,
    /// Sync and events tasks of each account.
    background: HashMap<Id, Vec<tokio::task::JoinHandle<()>>>,
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
        let view = if settings.device_view {
            ViewMode::Device
        } else {
            match ws.view() {
                AccountView::One(id) => ViewMode::Account(id),
                AccountView::All => ViewMode::All,
            }
        };
        let mut model = Self {
            ws,
            settings,
            prompter,
            api: None,
            server_user: None,
            server_url: None,
            current_account: None,
            pending_verification: None,
            me: None,
            teams: Vec::new(),
            accounts: Vec::new(),
            view,
            vault_filter: VaultFilter::All,
            ai_account: None,
            layout_notice: false,
            reload_gen: 0,
            sync_again: Default::default(),
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
            background: HashMap::new(),
            _bg_task: bg_task,
        };
        model.refresh_accounts();
        model.view = accounts::normalize_view(&model.account_infos(), model.view);
        for acc in model.ws.account_list() {
            if acc.is_signed_in() && acc.status() == AccountStatus::Active {
                model.start_account(&acc, cx);
            }
        }
        model.update_current(cx);
        model.reload(cx);
        model.check_layout_notice(cx);
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

    /// The layout notice, if it is waiting (only once).
    pub fn take_layout_notice(&mut self) -> bool {
        std::mem::take(&mut self.layout_notice)
    }

    /// First start after the 0.3 data moved to one store per account: a
    /// notice, once.
    fn check_layout_notice(&mut self, cx: &mut Context<Self>) {
        if self.settings.layout_notice_seen {
            return;
        }
        let store = self.ws.store.clone();
        runtime::run(
            cx,
            async move { store.meta_get(LAYOUT_BACKUP_KEY).await },
            |m, res, cx| {
                let mut settings = m.settings.clone();
                settings.layout_notice_seen = true;
                m.save_settings(settings, cx);
                if matches!(res, Ok(Some(_))) {
                    m.layout_notice = true;
                    cx.emit(ModelEvent::LayoutMigrated);
                }
            },
        );
    }

    // ----- Accounts and views -----

    /// The registry of accounts, in order.
    pub fn account_infos(&self) -> Vec<AccountInfo> {
        self.accounts.iter().map(|a| a.info.clone()).collect()
    }

    pub fn account(&self, id: Id) -> Option<&AccountModel> {
        self.accounts.iter().find(|a| a.id() == id)
    }

    fn account_mut(&mut self, id: Id) -> Option<&mut AccountModel> {
        self.accounts.iter_mut().find(|a| a.id() == id)
    }

    /// Accounts in sight in the current view.
    pub fn accounts_in_view(&self) -> Vec<&AccountModel> {
        let ids = accounts::accounts_in_view(&self.account_infos(), self.view);
        self.accounts
            .iter()
            .filter(|a| ids.contains(&a.id()))
            .collect()
    }

    /// Vaults of the accounts in sight.
    pub fn vaults_in_view(&self) -> Vec<VaultEntry> {
        self.accounts_in_view()
            .into_iter()
            .flat_map(|a| a.vaults.iter().cloned())
            .collect()
    }

    /// Every vault of every account.
    pub fn all_vaults(&self) -> Vec<VaultEntry> {
        self.accounts
            .iter()
            .flat_map(|a| a.vaults.iter().cloned())
            .collect()
    }

    pub fn vault_entry(&self, account: Id, vault: Id) -> Option<&VaultEntry> {
        self.account(account).and_then(|a| a.vault(vault))
    }

    /// Re-reads the registry, keeping what this run knows of each account.
    fn refresh_accounts(&mut self) {
        let old = std::mem::take(&mut self.accounts);
        self.accounts = self
            .ws
            .account_list()
            .into_iter()
            .map(|a| {
                let prev = old.iter().find(|o| o.id() == a.id);
                AccountModel {
                    info: a.info(),
                    signed_in: a.is_signed_in(),
                    vaults: prev.map(|p| p.vaults.clone()).unwrap_or_default(),
                    syncing: prev.is_some_and(|p| p.syncing),
                    last_sync: prev.and_then(|p| p.last_sync.clone()),
                    events_online: prev.is_some_and(|p| p.events_online),
                    unsynced: prev.map_or(0, |p| p.unsynced),
                }
            })
            .collect();
    }

    /// Works out the current account and its server client; when it
    /// changes, the views that depend on the server reload.
    fn update_current(&mut self, cx: &mut Context<Self>) {
        let current = match self.view {
            ViewMode::Account(id) => self.ws.account(id),
            _ => self.ws.current(),
        };
        let api = current
            .as_ref()
            .filter(|a| a.is_signed_in() && a.status() == AccountStatus::Active)
            .map(|a| a.api.clone());
        let id = current.as_ref().map(|a| a.id);
        let changed = id != self.current_account || api.is_some() != self.api.is_some();
        self.current_account = id;
        self.api = api;
        self.server_user = current.as_ref().map(|a| a.info().email);
        self.server_url = current.as_ref().map(|a| a.info().server_url);
        let model = id.and_then(|id| self.account(id)).cloned();
        self.syncing = model.as_ref().is_some_and(|a| a.syncing);
        self.last_sync = model.as_ref().and_then(|a| a.last_sync.clone());
        self.events_online = model.as_ref().is_some_and(|a| a.events_online);
        if changed {
            self.me = None;
            self.teams.clear();
            self.session_alerts.clear();
            self.refresh_me(cx);
            self.refresh_teams(cx);
            cx.emit(ModelEvent::SessionChanged);
        }
        cx.notify();
    }

    /// Shows one account, all of them or This device only (switcher).
    pub fn set_view(&mut self, view: ViewMode, cx: &mut Context<Self>) {
        let view = accounts::normalize_view(&self.account_infos(), view);
        if view == self.view {
            return;
        }
        self.view = view;
        self.vault_filter = VaultFilter::All;
        let device = view == ViewMode::Device;
        if self.settings.device_view != device {
            let mut s = self.settings.clone();
            s.device_view = device;
            self.save_settings(s, cx);
        }
        let ws_view = match view {
            ViewMode::Account(id) => Some(AccountView::One(id)),
            ViewMode::All => Some(AccountView::All),
            ViewMode::Device => None,
        };
        if let Some(v) = ws_view {
            let ws = self.ws.clone();
            runtime::run(cx, async move { ws.set_view(v).await }, |m, res, cx| {
                if let Err(e) = res {
                    tracing::warn!(error = %e, "could not save the account view");
                }
                m.update_current(cx);
            });
        }
        self.update_current(cx);
        // The lists that fan out over the accounts reload too.
        cx.emit(ModelEvent::SessionChanged);
        self.reload(cx);
    }

    /// Shows the items of one vault, of This device, or all of them.
    pub fn set_vault_filter(&mut self, filter: VaultFilter, cx: &mut Context<Self>) {
        if self.vault_filter != filter {
            self.vault_filter = filter;
            cx.notify();
        }
    }

    /// Stores of the current view.
    fn item_filter(&self) -> ItemFilter {
        match self.view {
            ViewMode::Account(id) => ItemFilter {
                accounts: Some(vec![id]),
                vaults: None,
                include_device: true,
            },
            ViewMode::All => ItemFilter::all(),
            ViewMode::Device => ItemFilter::device_only(),
        }
    }

    // ----- Local data -----

    /// Reloads every list from the stores in sight.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let ws = self.ws.clone();
        let filter = self.item_filter();
        let first = self.current_account;
        // Only the latest reload is applied (an older one may end later).
        self.reload_gen += 1;
        let generation = self.reload_gen;
        runtime::run(
            cx,
            async move {
                let mut accounts = Vec::new();
                for acc in ws.account_list() {
                    let vaults = acc.store.local_vault_list().await?;
                    let unsynced = acc.store.dirty_summary().await?.total;
                    accounts.push((acc.id, vaults, unsynced));
                }
                Ok::<_, ClientError>(Data {
                    hosts: list(&ws, &filter, first).await?,
                    groups: list(&ws, &filter, first).await?,
                    identities: list(&ws, &filter, first).await?,
                    keys: list(&ws, &filter, first).await?,
                    snippets: list(&ws, &filter, first).await?,
                    forwards: list(&ws, &filter, first).await?,
                    known_hosts: list(&ws, &filter, first).await?,
                    accounts,
                })
            },
            move |m, res, cx| match res {
                Ok(_) if generation != m.reload_gen => {}
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
                    m.refresh_accounts();
                    for (id, vaults, unsynced) in d.accounts {
                        if let Some(a) = m.account_mut(id) {
                            a.vaults = vaults
                                .into_iter()
                                .map(|vault| VaultEntry { account: id, vault })
                                .collect();
                            a.unsynced = unsynced;
                        }
                    }
                    // A vault that is gone leaves the picker.
                    if let VaultFilter::Vault { account, vault } = m.vault_filter
                        && m.vault_entry(account, vault).is_none()
                    {
                        m.vault_filter = VaultFilter::All;
                    }
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

    /// Some list has This device items.
    pub fn has_device_items(&self) -> bool {
        fn any<T>(list: &[Item<T>]) -> bool {
            list.iter().any(|i| i.scope == Scope::Device)
        }
        any(&self.hosts)
            || any(&self.groups)
            || any(&self.keys)
            || any(&self.identities)
            || any(&self.snippets)
            || any(&self.forwards)
    }

    /// Scope of a loaded item.
    pub fn scope_of(&self, kind: EntityKind, id: Id) -> Option<Scope> {
        fn find<T: CoreEntity>(list: &[Item<T>], id: Id) -> Option<Scope> {
            list.iter().find(|i| i.data.id() == id).map(|i| i.scope)
        }
        match kind {
            EntityKind::Host => find(&self.hosts, id),
            EntityKind::Group => find(&self.groups, id),
            EntityKind::Identity => find(&self.identities, id),
            EntityKind::Key => find(&self.keys, id),
            EntityKind::Snippet => find(&self.snippets, id),
            EntityKind::Forward => find(&self.forwards, id),
            EntityKind::KnownHost => find(&self.known_hosts, id),
            EntityKind::Memory => None,
        }
    }

    /// Kind, place and vault of a loaded item, whatever its kind.
    pub fn locate_loaded(&self, id: Id) -> Option<(EntityKind, Scope, Option<Id>)> {
        fn find<T: CoreEntity>(
            m: &AppModel,
            list: &[Item<T>],
            id: Id,
        ) -> Option<(EntityKind, Scope, Option<Id>)> {
            list.iter()
                .find(|i| i.data.id() == id)
                .map(|i| (T::KIND, i.scope, m.vault_of(i)))
        }
        find(self, &self.hosts, id)
            .or_else(|| find(self, &self.groups, id))
            .or_else(|| find(self, &self.keys, id))
            .or_else(|| find(self, &self.identities, id))
            .or_else(|| find(self, &self.snippets, id))
            .or_else(|| find(self, &self.forwards, id))
            .or_else(|| find(self, &self.known_hosts, id))
    }

    /// The vault an item is in (rows without one are the personal vault).
    pub fn vault_of<T>(&self, item: &Item<T>) -> Option<Id> {
        let personal = item
            .scope
            .account()
            .and_then(|a| self.account(a))
            .and_then(AccountModel::personal);
        accounts::effective_vault(item.scope, item.meta.vault_id, personal)
    }

    /// Whether an item passes the vault picker.
    pub fn in_filter<T>(&self, item: &Item<T>) -> bool {
        accounts::filter_matches(self.vault_filter, item.scope, self.vault_of(item))
    }

    /// The vault of an item, if known.
    pub fn vault_entry_of<T>(&self, item: &Item<T>) -> Option<&VaultEntry> {
        let account = item.scope.account()?;
        self.vault_entry(account, self.vault_of(item)?)
    }

    /// What the user can do with an item (Use-only rules).
    pub fn caps_of<T: CoreEntity>(&self, item: &Item<T>) -> Caps {
        let strict = self.vault_entry_of(item).is_some_and(VaultEntry::strict);
        accounts::caps(item.access, strict, T::KIND)
    }

    /// What the user can do with a host (everything when it is not loaded).
    pub fn host_caps(&self, id: Id) -> Caps {
        match self.host_record(id) {
            Some(h) => self.caps_of(h),
            None => accounts::caps(ItemAccess::Device, false, EntityKind::Host),
        }
    }

    /// Account a host belongs to (`None`: This device or not loaded).
    pub fn account_of_host(&self, id: Id) -> Option<Id> {
        self.host_record(id).and_then(Item::account)
    }

    /// Server client of an account (`None`: the current one).
    pub fn api_of(&self, account: Option<Id>) -> Option<ApiClient> {
        match account {
            None => self.api.clone(),
            Some(id) => self
                .ws
                .account(id)
                .filter(|a| a.is_signed_in() && a.status() == AccountStatus::Active)
                .map(|a| a.api.clone()),
        }
    }

    /// Server client to use for a host: its account's (This device hosts:
    /// the current account).
    pub fn api_for_host(&self, id: Id) -> Option<ApiClient> {
        match self.account_of_host(id) {
            Some(a) => self.api_of(Some(a)),
            None => self.api.clone(),
        }
    }

    /// Server client of the AI panel (the account chosen there, or the
    /// current one).
    pub fn ai_api(&self) -> Option<ApiClient> {
        self.ai_account
            .and_then(|a| self.api_of(Some(a)))
            .or_else(|| self.api.clone())
    }

    /// Chooses the account of the server AI.
    pub fn set_ai_account(&mut self, account: Option<Id>, cx: &mut Context<Self>) {
        if self.ai_account != account {
            self.ai_account = account;
            cx.emit(ModelEvent::SessionChanged);
            cx.notify();
        }
    }

    /// Name of where an item lives: "Personal", "Ops", "This device" (with
    /// the account's email when several accounts are in sight).
    pub fn place_label(&self, scope: Scope, vault: Option<Id>) -> String {
        let Scope::Account(a) = scope else {
            return t!("accounts.switcher.device").to_string();
        };
        let Some(acc) = self.account(a) else {
            return String::new();
        };
        let email = acc.info.email.clone();
        match vault.and_then(|v| acc.vault(v)).map(VaultEntry::label) {
            Some(vault) if accounts::show_account_badges(self.accounts_in_view().len()) => {
                format!("{email} · {vault}")
            }
            Some(vault) => vault,
            // A server without vaults: the account is the place.
            None => email,
        }
    }

    /// Where a new item goes: the vault of the picker, This device, the last
    /// vault used in the current account or its personal vault.
    pub fn new_item_target(&self) -> SaveTarget {
        let writable = |a: Id, v: Id| self.vault_entry(a, v).is_some_and(VaultEntry::can_write);
        match (self.vault_filter, self.view) {
            (VaultFilter::Device, _) | (_, ViewMode::Device) => SaveTarget::Device,
            (VaultFilter::Vault { account, vault }, _) if writable(account, vault) => {
                SaveTarget::Account {
                    account,
                    vault: Some(vault),
                }
            }
            _ => match self.current_account {
                Some(account) => SaveTarget::Account {
                    account,
                    vault: self
                        .settings
                        .last_vaults
                        .get(&account)
                        .copied()
                        .filter(|v| writable(account, *v)),
                },
                None => SaveTarget::Device,
            },
        }
    }

    /// Scope and vault a save target puts a new item in.
    pub fn place_of_target(&self, target: SaveTarget) -> (Scope, Option<Id>) {
        match target {
            SaveTarget::Account { account, vault } => {
                let personal = self.account(account).and_then(AccountModel::personal);
                let scope = Scope::Account(account);
                (scope, accounts::effective_vault(scope, vault, personal))
            }
            _ => (Scope::Device, None),
        }
    }

    /// An item another item at `place` may reference: one of the same
    /// vault, or a This device item.
    pub fn fits_place<T>(&self, item: &Item<T>, place: (Scope, Option<Id>)) -> bool {
        item.scope == Scope::Device || (item.scope == place.0 && self.vault_of(item) == place.1)
    }

    /// Save target of an item that exists (or of a new one next to it).
    pub fn target_of<T>(&self, item: &Item<T>) -> SaveTarget {
        match item.scope {
            Scope::Device => SaveTarget::Device,
            Scope::Account(account) => SaveTarget::Account {
                account,
                vault: item.meta.vault_id,
            },
        }
    }

    /// Where saving an item goes: its own store when it exists, otherwise
    /// [`new_item_target`](Self::new_item_target).
    fn save_target(&self, kind: EntityKind, id: Id) -> SaveTarget {
        if id.is_nil() {
            return self.new_item_target();
        }
        match self.scope_of(kind, id) {
            Some(Scope::Device) => SaveTarget::Device,
            Some(Scope::Account(account)) => SaveTarget::Account {
                account,
                vault: None,
            },
            None => SaveTarget::Auto,
        }
    }

    fn item_ref(&self, kind: EntityKind, id: Id) -> Option<ItemRef> {
        self.scope_of(kind, id).map(|scope| ItemRef { scope, id })
    }

    /// Saves (creates or updates) an entity and reloads.
    pub fn save<T: CoreEntity>(
        &mut self,
        data: T,
        secret: SecretUpdate<T::Secret>,
        mode: Option<SyncMode>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Item<T>, String>> {
        let target = self.save_target(T::KIND, data.id());
        self.save_to(target, data, secret, mode, cx)
    }

    /// Saves an entity in a given place (new items: a vault or This device).
    pub fn save_to<T: CoreEntity>(
        &mut self,
        target: SaveTarget,
        data: T,
        secret: SecretUpdate<T::Secret>,
        mode: Option<SyncMode>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Item<T>, String>> {
        self.remember_vault(target, cx);
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            ws.save_item(target, data, secret, mode)
                .await
                .map(Item::from)
                .map_err(api_error)
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| m.data_changed(cx));
            }
            res
        })
    }

    /// Changes some fields of an existing item (see [`update_stored`]): for
    /// quick actions like a favourite or a rename, which must not save the
    /// copy of a list over newer data.
    pub fn update_item<T: CoreEntity>(
        &mut self,
        id: Id,
        change: impl FnOnce(&mut T) + Send + 'static,
        cx: &mut Context<Self>,
    ) -> Task<Result<Item<T>, String>> {
        let item = self.item_ref(T::KIND, id);
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            let item = match item {
                Some(i) => i,
                None => ws.locate(id).await.map_err(api_error)?,
            };
            update_stored(&ws, item, change)
                .await
                .map(Item::from)
                .map_err(api_error)
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| m.data_changed(cx));
            }
            res
        })
    }

    /// Remembers the vault chosen for new items of an account.
    fn remember_vault(&mut self, target: SaveTarget, cx: &mut Context<Self>) {
        if let SaveTarget::Account {
            account,
            vault: Some(v),
        } = target
            && self.settings.last_vaults.get(&account) != Some(&v)
        {
            let mut s = self.settings.clone();
            s.last_vaults.insert(account, v);
            self.save_settings(s, cx);
        }
    }

    /// Saves a host changing only the given secrets (`None`: keep;
    /// `Some(None)`: delete). The SSH and proxy passwords share one secret,
    /// so they are merged with what is stored. `target` places a new host
    /// (`None`: [`new_item_target`](Self::new_item_target)).
    pub fn save_host(
        &mut self,
        host: Host,
        password: Option<Option<String>>,
        proxy_password: Option<Option<String>>,
        mode: Option<SyncMode>,
        target: Option<SaveTarget>,
        cx: &mut Context<Self>,
    ) -> Task<Result<Item<Host>, String>> {
        let target = target
            .filter(|_| host.id.is_nil())
            .unwrap_or_else(|| self.save_target(EntityKind::Host, host.id));
        let existing = self.item_ref(EntityKind::Host, host.id);
        let use_only = self
            .host_record(host.id)
            .is_some_and(|h| !h.access.can_read_secrets());
        if use_only && (password.is_some() || proxy_password.is_some()) {
            return Task::ready(Err(t!("error.vault_read_only").to_string()));
        }
        self.remember_vault(target, cx);
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            let secret = if password.is_none() && proxy_password.is_none() {
                SecretUpdate::Keep
            } else {
                let mut s = match existing {
                    Some(item) => ws.item_secret::<Host>(item).await.unwrap_or_default(),
                    None => HostSecret::default(),
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
            ws.save_item(target, host, secret, mode)
                .await
                .map(Item::from)
                .map_err(api_error)
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| m.data_changed(cx));
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
        let item = self.item_ref(T::KIND, id);
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            let item = match item {
                Some(i) => i,
                None => ws.locate(id).await?,
            };
            ws.delete_item::<T>(item).await
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| m.data_changed(cx));
            }
            res
        })
    }

    /// Copies a host (with its password, in the same place and sync mode)
    /// as "<label> (copy)". Not for Use-only hosts.
    pub fn duplicate_host(
        &mut self,
        id: Id,
        cx: &mut Context<Self>,
    ) -> Task<Result<Item<Host>, String>> {
        let Some(item) = self.host_record(id).cloned() else {
            return Task::ready(Err(t!("hosts.error.not_found").to_string()));
        };
        if !self.caps_of(&item).duplicate {
            return Task::ready(Err(t!("error.secret_hidden").to_string()));
        }
        let target = match item.scope {
            Scope::Device => SaveTarget::Device,
            Scope::Account(account) => SaveTarget::Account {
                account,
                vault: item.meta.vault_id,
            },
        };
        let ws = self.ws.clone();
        let label = t!("hosts.copy_label", name = item.data.label).to_string();
        let fut = runtime::spawn(cx, async move {
            let secret = if item.meta.has_secret {
                SecretUpdate::Set(ws.item_secret::<Host>(item.item_ref()).await?)
            } else {
                SecretUpdate::Keep
            };
            let mut host = item.data.clone();
            host.id = Id::nil();
            host.label = label;
            host.favorite = false;
            ws.save_item(target, host, secret, Some(item.meta.sync_mode))
                .await
                .map(Item::from)
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            if res.is_ok() {
                let _ = this.update(cx, |m, cx| m.data_changed(cx));
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
        let items: Vec<ItemRef> = self
            .hosts
            .iter()
            .filter(|h| ids.contains(&h.data.id) && h.data.group_id != group)
            .filter(|h| h.access.can_write())
            .map(Item::item_ref)
            .collect();
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            for item in items {
                update_stored::<Host>(&ws, item, |h| h.group_id = group).await?;
            }
            Ok::<_, ClientError>(())
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |m, cx| m.data_changed(cx));
            res
        })
    }

    /// Deletes several hosts.
    pub fn delete_hosts(
        &mut self,
        ids: Vec<Id>,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        // Use-only hosts cannot be deleted: they are left out.
        let items: Vec<ItemRef> = self
            .hosts
            .iter()
            .filter(|h| ids.contains(&h.data.id) && h.access.can_write())
            .map(Item::item_ref)
            .collect();
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            for item in items {
                ws.delete_item::<Host>(item).await.map_err(api_error)?;
            }
            Ok::<_, String>(())
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |m, cx| m.data_changed(cx));
            res
        })
    }

    /// Moves or copies items (with a dry run first for the confirmation).
    pub fn transfer(
        &mut self,
        req: LocalTransfer,
        cx: &mut Context<Self>,
    ) -> Task<Result<TransferResult, TransferError>> {
        let ws = self.ws.clone();
        let dry_run = req.dry_run;
        let task = runtime::handle(cx).spawn(async move { ws.transfer(req).await });
        cx.spawn(async move |this, cx| {
            let res = match task.await {
                Ok(r) => r.map_err(TransferError::from),
                Err(e) => Err(TransferError {
                    text: t!("common.task_interrupted", error = e).to_string(),
                    still_referenced: false,
                }),
            };
            if !dry_run {
                let _ = this.update(cx, |m, cx| m.data_changed(cx));
            }
            res
        })
    }

    /// This device items, to offer uploading them to an account.
    pub fn device_items(&self, cx: &mut Context<Self>) -> Task<Result<Vec<DeviceItem>, String>> {
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            let f = ItemFilter::device_only();
            let mut out = Vec::new();
            fn push<T: CoreEntity>(
                out: &mut Vec<DeviceItem>,
                list: Vec<Scoped<T>>,
                label: impl Fn(&T) -> String,
            ) {
                for s in list {
                    out.push(DeviceItem {
                        item: s.item(),
                        kind: T::KIND,
                        label: label(&s.record.data),
                        device_only: s.record.meta.sync_mode == SyncMode::DeviceOnly,
                    });
                }
            }
            push(&mut out, ws.list_items::<Host>(&f).await?, |h| {
                h.label.clone()
            });
            push(&mut out, ws.list_items::<Group>(&f).await?, |g| {
                g.name.clone()
            });
            push(&mut out, ws.list_items::<SshKey>(&f).await?, |k| {
                k.label.clone()
            });
            push(&mut out, ws.list_items::<Identity>(&f).await?, |i| {
                i.label.clone()
            });
            push(&mut out, ws.list_items::<Snippet>(&f).await?, |s| {
                s.name.clone()
            });
            push(&mut out, ws.list_items::<PortForward>(&f).await?, |p| {
                p.label.clone()
            });
            Ok::<_, ClientError>(out)
        });
        cx.spawn(async move |_, _| fut.await)
    }

    /// Uploads This device items to the personal vault of an account
    /// (they leave this device's store).
    pub fn upload_device_items(
        &mut self,
        account: Id,
        items: Vec<ItemRef>,
        cx: &mut Context<Self>,
    ) -> Task<Result<TransferResult, TransferError>> {
        self.transfer(
            LocalTransfer {
                items,
                to: Scope::Account(account),
                vault: None,
                mode: TransferMode::Move,
                dependencies: Dependencies::Auto,
                dry_run: false,
                force: false,
            },
            cx,
        )
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
    /// the choice, re-renders the windows in the new language and saves it
    /// in every signed-in account too.
    pub fn set_language(&mut self, choice: Option<String>, cx: &mut Context<Self>) {
        crate::i18n::apply(choice.as_deref());
        let mut settings = self.settings.clone();
        settings.language = choice;
        self.save_settings(settings, cx);
        crate::app::set_menus(cx);
        cx.refresh_windows();
        self.sync_locale(cx);
    }

    /// Saves the interface language in the accounts (`PATCH /api/v1/me`),
    /// which the servers use for emails.
    pub fn sync_locale(&mut self, cx: &mut Context<Self>) {
        let locale = crate::i18n::current();
        for acc in self.ws.account_list() {
            if !acc.is_signed_in() || acc.status() != AccountStatus::Active {
                continue;
            }
            if Some(acc.id) == self.current_account
                && self.me.as_ref().is_some_and(|u| u.locale == locale)
            {
                continue;
            }
            let api = acc.api.clone();
            let id = acc.id;
            let locale = locale.clone();
            runtime::run(
                cx,
                async move { api.set_locale(&locale).await.map_err(api_error) },
                move |m, res, cx| match res {
                    Ok(user) if m.current_account == Some(id) && m.api.is_some() => {
                        m.me = Some(user);
                        cx.notify();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "could not save the language in the account")
                    }
                },
            );
        }
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

    pub fn host_record(&self, id: Id) -> Option<&Item<Host>> {
        self.hosts.iter().find(|h| h.data.id == id)
    }

    /// Label of a host (or its id if it no longer exists).
    pub fn host_label(&self, id: Id) -> String {
        self.host(id)
            .map(|h| h.label.clone())
            .unwrap_or_else(|| id.to_string())
    }

    // ----- Server -----

    /// The current account is signed in.
    pub fn logged_in(&self) -> bool {
        self.api.is_some()
    }

    /// Is the current account a server administrator?
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

    /// Reads the current account again.
    pub fn refresh_me(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let current = self.current_account;
        runtime::run(
            cx,
            async move { api.get::<Value>("/api/v1/me").await.map_err(api_error) },
            move |m, res, cx| match res.and_then(|v| {
                serde_json::from_value::<User>(v["user"].clone()).map_err(|e| e.to_string())
            }) {
                Ok(user) if m.api.is_some() && m.current_account == current => {
                    m.me = Some(user);
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not read the server account"),
            },
        );
    }

    /// Reads the teams of the current account again.
    pub fn refresh_teams(&mut self, cx: &mut Context<Self>) {
        let Some(api) = self.api.clone() else {
            return;
        };
        let current = self.current_account;
        runtime::run(
            cx,
            async move {
                api.get::<Vec<Team>>("/api/v1/teams")
                    .await
                    .map_err(api_error)
            },
            move |m, res, cx| match res {
                Ok(teams) if m.api.is_some() && m.current_account == current => {
                    m.teams = teams;
                    cx.notify();
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "could not read the teams"),
            },
        );
    }

    // ----- Background (sync and events per account) -----

    /// Starts the periodic sync and the events WebSocket of an account.
    fn start_account(&mut self, acc: &Arc<Account>, cx: &mut Context<Self>) {
        self.stop_account(acc.id);
        let handle = runtime::handle(cx);
        let id = acc.id;
        let weak = Arc::downgrade(acc);
        let tx = self.bg_tx.clone();
        let sync = handle.spawn(async move {
            let mut tick = tokio::time::interval(SYNC_EVERY);
            loop {
                tick.tick().await;
                let Some(acc) = weak.upgrade() else { break };
                if !acc.is_signed_in() {
                    let _ = tx.send(BgMsg::Synced(id, Err(ClientError::NotLoggedIn)));
                    break;
                }
                let res = acc.sync_once().await;
                drop(acc);
                if tx.send(BgMsg::Synced(id, res)).is_err() {
                    break;
                }
            }
        });
        let tx = self.bg_tx.clone();
        let api = acc.api.clone();
        let events = handle.spawn(async move { events_loop(id, api, tx).await });
        self.background.insert(id, vec![sync, events]);
        if let Some(a) = self.account_mut(id) {
            a.syncing = true;
        }
    }

    fn stop_account(&mut self, id: Id) {
        for task in self.background.remove(&id).unwrap_or_default() {
            task.abort();
        }
        if let Some(a) = self.account_mut(id) {
            a.events_online = false;
            a.syncing = false;
        }
    }

    fn stop_background(&mut self) {
        for (_, tasks) in self.background.drain() {
            for t in tasks {
                t.abort();
            }
        }
    }

    fn on_background(&mut self, msg: BgMsg, cx: &mut Context<Self>) {
        match msg {
            BgMsg::Synced(id, res) => self.synced(id, res, cx),
            BgMsg::Event(id, v) => self.server_event(id, v, cx),
            BgMsg::EventsOnline(id, online) => {
                if let Some(a) = self.account_mut(id) {
                    a.events_online = online;
                }
                if self.current_account == Some(id) {
                    self.events_online = online;
                }
                cx.notify();
            }
            BgMsg::EmailNotVerified(id) => self.email_not_verified(id, cx),
        }
    }

    fn synced(&mut self, id: Id, res: Result<SyncReport, ClientError>, cx: &mut Context<Self>) {
        let email = self
            .account(id)
            .map(|a| a.info.email.clone())
            .unwrap_or_default();
        let summary = match res {
            Ok(report) => {
                for (level, text) in accounts::sync_notices(&report) {
                    let kind = match level {
                        NoticeLevel::Info => ToastKind::Info,
                        NoticeLevel::Warning => ToastKind::Warning,
                    };
                    let text = if self.accounts.len() > 1 {
                        format!("{email}: {text}")
                    } else {
                        text
                    };
                    self.toast(kind, text, cx);
                }
                Ok(SyncSummary::new(report.pushed, report.pulled))
            }
            Err(e) if e.is_email_not_verified() => {
                self.email_not_verified(id, cx);
                return;
            }
            Err(e) => {
                tracing::warn!(account = %id, error = %e, "sync failed");
                if matches!(e, ClientError::SessionExpired) {
                    self.stop_account(id);
                    self.toast(
                        ToastKind::Warning,
                        t!("accounts.notice.session_expired", email = email).to_string(),
                        cx,
                    );
                }
                Err(api_error(e))
            }
        };
        let ok = summary.is_ok();
        if let Some(a) = self.account_mut(id) {
            a.syncing = false;
            a.last_sync = Some(summary);
        }
        self.refresh_accounts();
        self.update_current(cx);
        if ok {
            self.reload(cx);
        }
        if self.sync_again.remove(&id) {
            self.sync_account(id, cx);
        }
        cx.notify();
    }

    /// An event of an account's server: vault changes sync that account;
    /// the rest goes to the views.
    fn server_event(&mut self, id: Id, mut v: Value, cx: &mut Context<Self>) {
        if let Some(obj) = v.as_object_mut() {
            obj.insert("account_id".into(), Value::String(id.to_string()));
        }
        if v["type"] == "vault" {
            if v["event"] == "access" {
                let name = v["vault_id"]
                    .as_str()
                    .and_then(|s| s.parse::<Id>().ok())
                    .and_then(|vault| self.vault_entry(id, vault))
                    .map(VaultEntry::label);
                if let Some(name) = name {
                    let lost = v["role"].is_null();
                    let text = if lost {
                        t!("accounts.notice.vault_lost", vault = name)
                    } else {
                        t!("accounts.notice.vault_access", vault = name)
                    };
                    self.toast(
                        if lost {
                            ToastKind::Warning
                        } else {
                            ToastKind::Info
                        },
                        text.to_string(),
                        cx,
                    );
                }
            }
            self.sync_account(id, cx);
        }
        cx.emit(ModelEvent::Server(v));
    }

    /// The server says the account has not confirmed its email: its sync
    /// stops and Settings → Accounts asks for the code.
    fn email_not_verified(&mut self, id: Id, cx: &mut Context<Self>) {
        self.stop_account(id);
        let Some(acc) = self.account(id).cloned() else {
            return;
        };
        tracing::info!(account = %id, "the account has to confirm its email");
        if self.pending_verification.as_ref().map(|p| p.account) != Some(id) {
            self.pending_verification = Some(PendingVerification {
                account: id,
                url: acc.info.server_url.clone(),
                email: acc.info.email.clone(),
                resend_at: None,
            });
        }
        self.toast(ToastKind::Warning, t!("state.verify_email"), cx);
        cx.notify();
    }

    /// Syncs one account now (if it is not syncing already).
    pub fn sync_account(&mut self, id: Id, cx: &mut Context<Self>) {
        let Some(acc) = self.ws.account(id) else {
            return;
        };
        if !acc.is_signed_in() || acc.status() != AccountStatus::Active {
            return;
        }
        if let Some(a) = self.account_mut(id) {
            if a.syncing {
                self.sync_again.insert(id);
                return;
            }
            a.syncing = true;
        }
        if self.current_account == Some(id) {
            self.syncing = true;
        }
        cx.notify();
        let tx = self.bg_tx.clone();
        runtime::handle(cx).spawn(async move {
            let res = acc.sync_once().await;
            let _ = tx.send(BgMsg::Synced(id, res));
        });
    }

    /// Syncs every signed-in account now.
    pub fn sync_now(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<Id> = self.accounts.iter().map(AccountModel::id).collect();
        for id in ids {
            self.sync_account(id, cx);
        }
    }

    /// Reloads and syncs after a change (also made outside the model, e.g.
    /// when importing an `ssh_config`).
    pub fn data_changed(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        self.sync_now(cx);
    }

    // ----- Signing in, accounts -----

    /// Signs in to a server: a new account, or the same one again. If the
    /// account has two-step verification and `totp` is missing, returns
    /// [`LoginError::TotpRequired`]. An account that still has to confirm
    /// its email returns [`LoginOutcome::VerifyEmail`].
    pub fn sign_in(
        &mut self,
        server: ServerChoice,
        email: String,
        password: String,
        totp: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<LoginOutcome, LoginError>> {
        let ws = self.ws.clone();
        let task = runtime::handle(cx).spawn(async move {
            ws.sign_in(server, &email, &password, totp.as_deref())
                .await
                .map_err(LoginError::from)
        });
        self.adopt(task, false, cx)
    }

    /// Creates an account on a server (with an invitation code when its
    /// registration is closed). `accept_terms` records that the person
    /// accepted the server's terms of use and privacy policy.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_up(
        &mut self,
        server: ServerChoice,
        email: String,
        name: String,
        password: String,
        invite: Option<String>,
        accept_terms: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<LoginOutcome, LoginError>> {
        let ws = self.ws.clone();
        let task = runtime::handle(cx).spawn(async move {
            let terms = accept_terms.then_some(None);
            ws.sign_up_accepting(server, &email, &name, &password, invite.as_deref(), terms)
                .await
                .map_err(LoginError::from)
        });
        self.adopt(task, true, cx)
    }

    /// The result of a sign-in or sign-up: the account becomes current, or
    /// waits for its email code.
    fn adopt(
        &mut self,
        task: tokio::task::JoinHandle<Result<Arc<Account>, LoginError>>,
        registering: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<LoginOutcome, LoginError>> {
        cx.spawn(async move |this, cx| {
            let acc = task.await.map_err(|e| {
                LoginError::Failed(t!("common.task_interrupted", error = e).to_string())
            })??;
            let outcome = this
                .update(cx, |m, cx| m.account_added(acc, registering, cx))
                .map_err(|e| LoginError::Failed(e.to_string()))?;
            Ok(outcome)
        })
    }

    fn account_added(
        &mut self,
        acc: Arc<Account>,
        registering: bool,
        cx: &mut Context<Self>,
    ) -> LoginOutcome {
        let id = acc.id;
        // A new account and the only one (a sign-up waiting for its code
        // is already listed; signing in again is not new).
        let first = self
            .accounts
            .iter()
            .all(|a| a.id() == id && a.info.status == AccountStatus::Unverified);
        self.refresh_accounts();
        if acc.status() == AccountStatus::Unverified {
            self.stop_account(id);
            // Creating the account sent the first code just now.
            self.pending_verification = Some(PendingVerification {
                account: id,
                url: acc.info().server_url,
                email: acc.info().email,
                resend_at: registering.then(|| Instant::now() + RESEND_WAIT),
            });
            cx.notify();
            return LoginOutcome::VerifyEmail(id);
        }
        if self.pending_verification.as_ref().map(|p| p.account) == Some(id) {
            self.pending_verification = None;
        }
        self.view = ViewMode::Account(id);
        self.vault_filter = VaultFilter::All;
        if self.settings.device_view {
            let mut s = self.settings.clone();
            s.device_view = false;
            self.save_settings(s, cx);
        }
        self.start_account(&acc, cx);
        self.update_current(cx);
        self.reload(cx);
        self.toast(
            ToastKind::Success,
            t!("state.signed_in", email = acc.info().email).to_string(),
            cx,
        );
        // A language picked explicitly goes to the account (emails).
        if self.settings.language.is_some() {
            self.sync_locale(cx);
        }
        LoginOutcome::SignedIn { account: id, first }
    }

    /// Verifies an account's email with the code from the verification email
    /// and signs in. If the account has two-step verification and `totp` is
    /// missing, returns [`LoginError::TotpRequired`].
    pub fn verify_account(
        &mut self,
        account: Id,
        code: String,
        totp: Option<String>,
        cx: &mut Context<Self>,
    ) -> Task<Result<LoginOutcome, LoginError>> {
        let ws = self.ws.clone();
        let code = clean_email_code(&code);
        let task = runtime::handle(cx).spawn(async move {
            ws.verify_account(account, &code, totp.as_deref())
                .await
                .map_err(LoginError::from)
        });
        self.adopt(task, false, cx)
    }

    /// Asks the server to email a new verification code. Afterwards
    /// "Resend" waits a minute.
    pub fn resend_account_code(
        &mut self,
        account: Id,
        cx: &mut Context<Self>,
    ) -> Task<Result<(), String>> {
        let ws = self.ws.clone();
        let task = runtime::handle(cx).spawn(async move { ws.resend_account_code(account).await });
        cx.spawn(async move |this, cx| {
            let res = task
                .await
                .map_err(|e| t!("common.task_interrupted", error = e).to_string())?;
            let wait = match &res {
                Ok(()) => RESEND_WAIT,
                // Asked too often: wait the usual minute before offering it
                // again.
                Err(e) if e.api_code() == Some("too_many_attempts") => RESEND_WAIT,
                Err(_) => Duration::ZERO,
            };
            let _ = this.update(cx, |m, cx| {
                if let Some(p) = m
                    .pending_verification
                    .as_mut()
                    .filter(|p| p.account == account)
                {
                    p.resend_at = Some(Instant::now() + wait);
                    cx.notify();
                }
            });
            res.map_err(api_error)
        })
    }

    /// Waits for the email code of an account (Settings → Accounts).
    pub fn resume_verification(&mut self, account: Id, cx: &mut Context<Self>) {
        let Some(acc) = self.account(account).cloned() else {
            return;
        };
        if self.pending_verification.as_ref().map(|p| p.account) != Some(account) {
            self.pending_verification = Some(PendingVerification {
                account,
                url: acc.info.server_url,
                email: acc.info.email,
                resend_at: None,
            });
            cx.notify();
        }
    }

    /// Signs out of an account and deletes its local data. With unsynced
    /// changes and `discard = false` nothing happens and the report says how
    /// many there are (ask "Sync now / Discard").
    pub fn sign_out_account(
        &mut self,
        account: Id,
        discard: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<SignOutReport, String>> {
        let restart = self.background.contains_key(&account);
        self.stop_account(account);
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move {
            ws.sign_out(account, discard)
                .await
                .map_err(|e| api_error(e))
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |m, cx| {
                match &res {
                    Ok(report) if report.signed_out => {
                        m.signed_out(account, cx);
                    }
                    _ => {
                        // Not signed out: it keeps syncing.
                        if restart && let Some(acc) = m.ws.account(account) {
                            m.start_account(&acc, cx);
                        }
                    }
                }
            });
            res
        })
    }

    /// Signs out of every account (their data is deleted; This device items
    /// stay).
    pub fn sign_out_all(&mut self, cx: &mut Context<Self>) -> Task<Result<(), String>> {
        self.stop_background();
        let ws = self.ws.clone();
        let fut = runtime::spawn(cx, async move { ws.sign_out_all().await.map(|_| ()) });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |m, cx| {
                m.pending_verification = None;
                m.ai_account = None;
                m.vault_filter = VaultFilter::All;
                m.refresh_accounts();
                // Whatever could not be removed is still there (and syncs).
                for acc in m.ws.account_list() {
                    if acc.is_signed_in() && acc.status() == AccountStatus::Active {
                        m.start_account(&acc, cx);
                    }
                }
                let infos = m.account_infos();
                m.view = accounts::normalize_view(&infos, m.view);
                m.update_current(cx);
                m.reload(cx);
                if res.is_ok() {
                    m.toast(ToastKind::Info, t!("state.signed_out"), cx);
                }
                cx.emit(ModelEvent::SessionChanged);
            });
            res
        })
    }

    fn signed_out(&mut self, account: Id, cx: &mut Context<Self>) {
        self.stop_account(account);
        if self.pending_verification.as_ref().map(|p| p.account) == Some(account) {
            self.pending_verification = None;
        }
        if self.ai_account == Some(account) {
            self.ai_account = None;
        }
        if self.settings.last_vaults.contains_key(&account) {
            let mut s = self.settings.clone();
            s.last_vaults.remove(&account);
            self.save_settings(s, cx);
        }
        self.refresh_accounts();
        let infos = self.account_infos();
        let view = match self.view {
            ViewMode::Account(id) if id == account => infos
                .first()
                .map_or(ViewMode::Device, |a| ViewMode::Account(a.id)),
            other => other,
        };
        self.view = accounts::normalize_view(&infos, view);
        if matches!(self.vault_filter, VaultFilter::Vault { account: a, .. } if a == account) {
            self.vault_filter = VaultFilter::All;
        }
        self.update_current(cx);
        self.reload(cx);
        self.toast(ToastKind::Info, t!("state.signed_out"), cx);
        cx.emit(ModelEvent::SessionChanged);
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
                    None => ws
                        .connect(forward.host_id, prompter, use_agent)
                        .await
                        .map_err(api_error)?,
                };
                let handle = conn
                    .start_forward(ForwardSpec::from(&forward))
                    .await
                    .map_err(|e| api_error(termoak_client::ClientError::from(e)))?;
                Ok::<_, String>((conn, handle))
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

/// A This device item that can be uploaded to an account.
#[derive(Debug, Clone)]
pub struct DeviceItem {
    pub item: ItemRef,
    pub kind: EntityKind,
    pub label: String,
    /// Marked "this device only": not offered by default.
    pub device_only: bool,
}

impl Drop for AppModel {
    fn drop(&mut self) {
        self.stop_background();
    }
}

/// Keeps the events WebSocket of an account open and forwards what arrives.
async fn events_loop(account: Id, api: ApiClient, tx: mpsc::UnboundedSender<BgMsg>) {
    let mut backoff = 2u64;
    loop {
        match api.websocket("/api/v1/events/ws").await {
            Ok(ws) => {
                backoff = 2;
                let _ = tx.send(BgMsg::EventsOnline(account, true));
                let (_sink, mut stream) = ws.split();
                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            if let Ok(v) = serde_json::from_str::<Value>(&text)
                                && tx.send(BgMsg::Event(account, v)).is_err()
                            {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                }
                let _ = tx.send(BgMsg::EventsOnline(account, false));
            }
            Err(e) => {
                tracing::debug!(error = %e, "events WebSocket not available");
                if e.is_email_not_verified() {
                    let _ = tx.send(BgMsg::EmailNotVerified(account));
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
    fn cloud_notice_dismissal() {
        // Never closed: shown while there are sessions.
        assert!(!cloud_notice_visible(0, 0));
        assert!(cloud_notice_visible(1, 0));
        // Closed with 2: hidden while there are 2 or fewer...
        assert!(!cloud_notice_visible(2, 2));
        assert!(!cloud_notice_visible(1, 2));
        // ...and back with a third one.
        assert!(cloud_notice_visible(3, 2));
        // Sessions that end lower the count; then a new one shows it again.
        assert_eq!(cloud_notice_lowered(1, 3), Some(1));
        assert_eq!(cloud_notice_lowered(3, 3), None);
        assert_eq!(cloud_notice_lowered(5, 3), None);
        let mut dismissed = 3;
        for running in [1, 2] {
            if let Some(d) = cloud_notice_lowered(running, dismissed) {
                dismissed = d;
            }
        }
        assert_eq!(dismissed, 1);
        assert!(cloud_notice_visible(2, dismissed));
        // An old settings file without the field: never closed.
        let s: Settings = serde_json::from_str(r#"{"dark": false}"#).unwrap();
        assert_eq!(s.cloud_notice_dismissed, 0);
    }
    use termoak_core::CoreError;

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
            account: Id::nil(),
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
        assert!(!s.device_view && s.last_vaults.is_empty() && !s.layout_notice_seen);
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.font_size, 14.0);
        assert_eq!(s.ai, AiSettings::default());

        let mut s = Settings::default();
        s.ai.run_on = Some(RunOn::Local);
        s.last_vaults.insert(Id::nil(), Id::nil());
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.ai.run_on, Some(RunOn::Local));
        assert_eq!(back.last_vaults.len(), 1);
    }

    #[test]
    fn vault_rules_of_this_device_are_translated() {
        let e: ClientError = CoreError::vault(
            termoak_core::error::codes::USE_ONLY_NEEDS_SERVER,
            "this host can only be used while connected to its server",
        )
        .into();
        assert_eq!(error_code(&e).as_deref(), Some("use_only_needs_server"));
        assert_eq!(
            api_error(e),
            "this host can only be used while signed in to its server"
        );
        let e: ClientError =
            CoreError::vault(termoak_core::error::codes::STILL_REFERENCED, "x").into();
        assert!(TransferError::from(e).still_referenced);
    }

    #[test]
    fn the_same_item_through_two_accounts_is_listed_once() {
        let rec = |name: &str| -> Record<Snippet> {
            serde_json::from_value(serde_json::json!({
                "id": Id::from_u128(7), "name": name, "script": "ls",
                "owner_id": Id::nil(), "sync_mode": "synced", "rev": 1, "updated_at": 0,
                "deleted": false, "has_secret": false
            }))
            .unwrap()
        };
        let (a, b) = (Id::from_u128(1), Id::from_u128(2));
        let item = |acc: Id, name: &str| Item {
            scope: Scope::Account(acc),
            access: ItemAccess::Editor,
            rec: rec(name),
        };
        let list = vec![item(a, "seen by a"), item(b, "seen by b")];
        assert_eq!(dedupe(list.clone(), Some(b))[0].data.name, "seen by b");
        assert_eq!(dedupe(list.clone(), None)[0].data.name, "seen by a");
        assert_eq!(dedupe(list, Some(a)).len(), 1);
    }

    #[tokio::test]
    async fn quick_changes_keep_what_was_saved_meanwhile() {
        let dir = std::env::temp_dir().join(format!("termoak-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::open(&dir, termoak_core::crypto::MasterKey::generate()).unwrap();
        let host = Host {
            id: Id::nil(),
            label: "web".into(),
            address: "web.example.com".into(),
            group_id: None,
            tags: Vec::new(),
            settings: Default::default(),
            notes: String::new(),
            color: None,
            os: None,
            os_version: None,
            favorite: false,
        };
        let saved = ws
            .save_item(SaveTarget::Device, host, SecretUpdate::Keep, None)
            .await
            .unwrap();
        // The list keeps this copy while connecting detects the OS.
        let listed = Item::from(saved);
        update_stored::<Host>(&ws, listed.item_ref(), |h| {
            h.os = Some("debian".into());
            h.os_version = Some("Debian 13".into());
        })
        .await
        .unwrap();
        // Marking it as a favourite from the list keeps the OS.
        let fav = update_stored::<Host>(&ws, listed.item_ref(), |h| h.favorite = true)
            .await
            .unwrap();
        assert!(fav.record.data.favorite);
        let stored = ws.get_item::<Host>(listed.item_ref()).await.unwrap();
        assert!(stored.record.data.favorite);
        assert_eq!(stored.record.data.os.as_deref(), Some("debian"));
        assert_eq!(stored.record.data.os_version.as_deref(), Some("Debian 13"));
        assert_eq!(stored.record.data.label, "web");
        assert_eq!(stored.scope, Scope::Device);
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn items_deref_to_their_record() {
        let rec: Record<Snippet> = serde_json::from_value(serde_json::json!({
            "id": Id::nil(), "name": "ls", "script": "ls -la",
            "owner_id": Id::nil(), "sync_mode": "synced", "rev": 1, "updated_at": 0,
            "deleted": false, "has_secret": false
        }))
        .unwrap();
        let item = Item::device(rec);
        assert_eq!(item.data.name, "ls");
        assert_eq!(item.account(), None);
        assert_eq!(item.item_ref().scope, Scope::Device);
    }
}
