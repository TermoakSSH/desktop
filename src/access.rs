//! Teams security on the desktop (0.7 phase 4c, docs-next/TEAMS-SECURITY.md
//! §13.1): the rules the views share and the access state of every signed-in
//! account.
//!
//! - Roles of a vault member as the role picker offers them (Connect only,
//!   Use, Edit, Admin) with their help texts.
//! - What a vault's team policy hides ([`gates`]): reveal/copy/export, the
//!   AI, the private network picker; whether sessions are recorded.
//! - The access window of a host (approved requests): the countdown texts.
//! - The error codes and session close reasons of teams security with their
//!   texts, and [`AccessNeed`]: what an `access_approval_required` error
//!   says (to offer "Request access" instead of an error).
//! - [`AccessBook`]: shared by every window, per account: the requests you
//!   can decide (the approvals inbox and its badge), your own requests and
//!   the server's answer about each host (`GET /hosts/{id}/access`). The
//!   events socket's `access_request` messages keep it current.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::SharedString;
use serde::Deserialize;
use serde_json::{Value, json};
use termoak_client::{ApiClient, ClientError};
use termoak_core::access::{
    AccessRequest, AccessRequestStatus, AccessTargetKind, HostAccess, VaultPolicy,
};
use termoak_core::error::CoreError;
use termoak_core::model::VaultRole;
use termoak_core::{Id, error::codes};

use crate::state::ToastKind;

// ----- Roles -----

/// The roles a vault manager can give, lowest first (wire names
/// `connect_only`, `use_only`, `editor`, `manager`).
pub const GRANTABLE_ROLES: [VaultRole; 4] = [
    VaultRole::ConnectOnly,
    VaultRole::UseOnly,
    VaultRole::Editor,
    VaultRole::Manager,
];

/// Roles offered by the picker: the four with a server that has teams
/// security (`roles_v2`), else the two every server knows.
pub fn grantable_roles(teams_security: bool) -> Vec<VaultRole> {
    if teams_security {
        GRANTABLE_ROLES.to_vec()
    } else {
        vec![VaultRole::UseOnly, VaultRole::Editor]
    }
}

/// One line saying what a role allows (under the picker's entries).
pub fn role_hint(role: VaultRole) -> SharedString {
    match role {
        VaultRole::ConnectOnly => t!("vaults.role_hint.connect_only"),
        VaultRole::UseOnly => t!("vaults.role_hint.use_only"),
        VaultRole::Editor => t!("vaults.role_hint.editor"),
        VaultRole::Manager => t!("vaults.role_hint.manager"),
        VaultRole::Unknown => t!("vaults.role_hint.unknown"),
    }
}

/// The member can be removed or changed in the members list, and whether
/// "Delete vault" is offered: an explicit Admin (a `manager` grant) manages
/// the vault but cannot delete it; only implicit managers (the owner of a
/// shared vault, the team's owners and admins) can.
pub fn can_delete_vault(role: VaultRole, implicit_manager: bool) -> bool {
    role == VaultRole::Manager && implicit_manager
}

// ----- Policy gates -----

/// What a vault's team policy lets this member do (and what to hide).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    /// Reveal or copy secrets, export private keys.
    pub reveal: bool,
    /// AI actions on its hosts, and their place in the AI's inventory.
    pub ai: bool,
    /// The private network picker.
    pub networks: bool,
    /// Every session on its hosts is recorded ("Recorded" badge).
    pub recorded: bool,
    /// Some hosts have access policies (ask the server before connecting).
    pub access_rules: bool,
    /// The vault's owner has an SSH CA ("Team certificate" works).
    pub certificates: bool,
}

/// [`Gates`] of a member with `role` in a vault with `policy` (`None`: no
/// team policy, everything as the role allows).
pub fn gates(role: VaultRole, policy: Option<&VaultPolicy>) -> Gates {
    let can_read = role.rank() >= VaultRole::Editor.rank();
    Gates {
        reveal: can_read && (role == VaultRole::Manager || policy.is_none_or(|p| p.reveal_allowed)),
        ai: policy.is_none_or(|p| p.ai_allowed),
        networks: policy.is_none_or(|p| p.networks_allowed),
        recorded: policy.is_some_and(|p| p.record_sessions),
        access_rules: policy.is_some_and(|p| p.access_policies),
        certificates: policy.is_some_and(|p| p.certificates),
    }
}

// ----- Access windows -----

/// Seconds left until `ends_at` (ms), never negative.
pub fn remaining_secs(ends_at: i64, now: i64) -> i64 {
    ((ends_at - now).max(0) + 999) / 1000
}

/// "1 h 05 min", "42 min", "45 s": how long is left, short.
pub fn short_remaining(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return t!("access.time.seconds", n = secs).to_string();
    }
    let minutes = (secs + 59) / 60;
    if minutes < 60 {
        return t!("access.time.minutes", n = minutes).to_string();
    }
    let h = minutes / 60;
    let m = minutes % 60;
    t!("access.time.hours", h = h, m = format!("{m:02}")).to_string()
}

/// Local "HH:MM" of a time (ms).
pub fn clock(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// "Access until 15:42 · 42 min left".
pub fn window_label(ends_at: i64, now: i64) -> String {
    t!(
        "access.window.until",
        time = clock(ends_at),
        left = short_remaining(remaining_secs(ends_at, now))
    )
    .to_string()
}

/// How often a countdown must repaint: every second in the last two
/// minutes, else every 15 seconds.
pub fn tick_every(secs_left: i64) -> Duration {
    if secs_left <= 120 {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(15)
    }
}

/// The window is about to end (the countdown turns to a warning color).
pub fn ending_soon(secs_left: i64) -> bool {
    secs_left <= 300
}

/// Durations offered in the request dialog (minutes), up to `max`; `max`
/// itself is always offered.
pub fn duration_choices(max: u32) -> Vec<u32> {
    let mut v: Vec<u32> = [15, 30, 60, 120, 240, 480, 1440, 4320, 10080]
        .into_iter()
        .filter(|m| *m < max)
        .collect();
    if max > 0 {
        v.push(max);
    }
    v
}

/// "15 min", "2 h", "3 days" for a duration in minutes.
pub fn duration_label(minutes: u32) -> String {
    if minutes < 60 {
        t!("access.duration.minutes", n = minutes).to_string()
    } else if minutes % 1440 == 0 {
        tn!("access.duration.days", minutes / 1440).to_string()
    } else if minutes % 60 == 0 {
        t!("access.duration.hours", n = minutes / 60).to_string()
    } else {
        t!(
            "access.duration.hours_minutes",
            h = minutes / 60,
            m = minutes % 60
        )
        .to_string()
    }
}

// ----- Error codes and close reasons -----

/// Error codes of teams security (TEAMS-SECURITY.md §7). Each has an
/// `error.<code>` text in every language, always used (the server's English
/// message is for developers).
pub const ERROR_CODES: [&str; 22] = [
    codes::CONNECT_ONLY,
    "secret_reveal_forbidden",
    codes::ACCESS_APPROVAL_REQUIRED,
    codes::ACCESS_NEEDS_SERVER,
    "access_request_exists",
    codes::ACCESS_REQUEST_NOT_NEEDED,
    "access_request_closed",
    codes::ACCESS_DURATION_TOO_LONG,
    "self_approval_forbidden",
    "not_an_approver",
    codes::CA_NOT_CONFIGURED,
    "ca_exists",
    "certificate_key_not_owned",
    "certificate_key_revoked",
    "certificate_proof_invalid",
    "certificate_no_principals",
    codes::TEAM_2FA_REQUIRED,
    "policy_requires_2fa",
    "ai_forbidden_by_policy",
    "networks_forbidden_by_policy",
    codes::RECORDING_REQUIRED,
    "invalid_reason",
];

/// Why the server closed a session (`session_closed.reason`, a terminal's
/// closed status) that the app explains in words.
#[cfg_attr(not(test), allow(dead_code))]
pub const CLOSE_REASONS: [&str; 3] = ["access_expired", "access_revoked", "vault_access_revoked"];

/// A teams security code (its text is always the app's).
pub fn is_teams_code(code: &str) -> bool {
    ERROR_CODES.contains(&code)
}

/// The text of a close reason (`None`: not one of them).
pub fn close_reason_text(reason: &str) -> Option<String> {
    match reason {
        "access_expired" => Some(t!("access.closed.access_expired").to_string()),
        "access_revoked" => Some(t!("access.closed.access_revoked").to_string()),
        "vault_access_revoked" => Some(t!("access.closed.vault_access_revoked").to_string()),
        _ => None,
    }
}

// ----- "Request access" -----

/// What an `access_approval_required` error says: the host, the policy's
/// target and the longest window that can be asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct AccessNeed {
    pub host_id: Option<Id>,
    pub vault_id: Option<Id>,
    pub target_kind: AccessTargetKind,
    pub target_id: Option<Id>,
    pub max_minutes: u32,
    /// A request of yours already waits.
    pub pending: Option<Id>,
    pub can_request: bool,
}

impl AccessNeed {
    /// From the error's details (the server's or the engine's own check).
    pub fn from_detail(d: &Value) -> Self {
        let id = |k: &str| d[k].as_str().and_then(|s| s.parse::<Id>().ok());
        Self {
            host_id: id("host_id"),
            vault_id: id("vault_id"),
            target_kind: d["target_kind"]
                .as_str()
                .map(AccessTargetKind::parse)
                .unwrap_or(AccessTargetKind::Host),
            target_id: id("target_id"),
            max_minutes: d["max_minutes"]
                .as_u64()
                .and_then(|m| u32::try_from(m).ok())
                .filter(|m| *m > 0)
                .unwrap_or(60),
            pending: id("pending"),
            can_request: d["can_request"].as_bool().unwrap_or(true),
        }
    }

    /// The need of an error, if it is `access_approval_required`.
    pub fn from_error(e: &ClientError) -> Option<Self> {
        match e {
            ClientError::Api { code, details, .. } if code == codes::ACCESS_APPROVAL_REQUIRED => {
                Some(Self::from_detail(details))
            }
            ClientError::Core(CoreError::Vault { code, detail, .. })
                if *code == codes::ACCESS_APPROVAL_REQUIRED =>
            {
                Some(Self::from_detail(detail.as_ref().unwrap_or(&Value::Null)))
            }
            _ => None,
        }
    }

    /// The need from a host's access answer (the host list, before
    /// connecting).
    pub fn from_access(a: &HostAccess) -> Self {
        Self {
            host_id: Some(a.host_id),
            vault_id: Some(a.vault_id),
            target_kind: a
                .policy
                .as_ref()
                .map_or(AccessTargetKind::Host, |p| p.target_kind),
            target_id: a.policy.as_ref().map(|p| p.target_id),
            max_minutes: a.max_minutes.max(1),
            pending: a.pending.as_ref().map(|p| p.id),
            can_request: a.can_request,
        }
    }
}

/// The body of `POST /access-requests` for one target.
pub fn request_body(kind: AccessTargetKind, target: Id, minutes: u32, reason: &str) -> Value {
    let key = match kind {
        AccessTargetKind::Group => "group_id",
        AccessTargetKind::Vault => "vault_id",
        _ => "host_id",
    };
    json!({key: target, "minutes": minutes, "reason": reason.trim()})
}

/// The request form is complete: a reason (1–500 characters) and a
/// duration within the policy's maximum.
pub fn request_valid(minutes: u32, max: u32, reason: &str) -> Result<(), &'static str> {
    let n = reason.trim().chars().count();
    if n == 0 {
        return Err("access.request.error.reason");
    }
    if n > 500 {
        return Err("access.request.error.reason_long");
    }
    if minutes == 0 || minutes > max {
        return Err("access.request.error.duration");
    }
    Ok(())
}

// ----- Requests as the server lists them -----

/// An access request with whether you may decide it (`can_decide`, from the
/// server's listings).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Request {
    #[serde(flatten)]
    pub r: AccessRequest,
    #[serde(default)]
    pub can_decide: bool,
}

impl std::ops::Deref for Request {
    type Target = AccessRequest;
    fn deref(&self) -> &AccessRequest {
        &self.r
    }
}

/// Label of a request's state.
pub fn status_label(r: &AccessRequest, now: i64) -> SharedString {
    match r.status {
        AccessRequestStatus::Pending => t!("access.status.pending"),
        AccessRequestStatus::Approved if r.is_active(now) => t!("access.status.active"),
        AccessRequestStatus::Approved => t!("access.status.ended"),
        AccessRequestStatus::Denied => t!("access.status.denied"),
        AccessRequestStatus::Expired => t!("access.status.expired"),
        AccessRequestStatus::Cancelled => t!("access.status.cancelled"),
        AccessRequestStatus::Revoked => t!("access.status.revoked"),
        AccessRequestStatus::Unknown => t!("access.status.unknown"),
    }
}

/// What to tell the user about an `access_request` event (`me`: the
/// account's user id). `None`: nothing to say.
pub fn event_notice(me: Option<Id>, v: &Value) -> Option<(ToastKind, String)> {
    if v["type"] != "access_request" {
        return None;
    }
    let r: AccessRequest = serde_json::from_value(v["request"].clone()).ok()?;
    let mine = me == Some(r.requester_id);
    let target = r.target_label.clone();
    let who = if r.requester_name.trim().is_empty() {
        r.requester_email.clone()
    } else {
        r.requester_name.clone()
    };
    let by = r.decided_by_name.clone().unwrap_or_default();
    Some(match (v["event"].as_str()?, mine) {
        ("created", false) => (
            ToastKind::Info,
            t!(
                "access.notice.created",
                name = who,
                target = target,
                time = duration_label(r.minutes)
            )
            .to_string(),
        ),
        ("approved", true) => (
            ToastKind::Success,
            t!(
                "access.notice.approved",
                target = target,
                time = r.ends_at.map(clock).unwrap_or_default()
            )
            .to_string(),
        ),
        ("denied", true) => (
            ToastKind::Warning,
            match r
                .decision_reason
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            {
                Some(reason) => t!(
                    "access.notice.denied_reason",
                    target = target,
                    reason = reason
                )
                .to_string(),
                None => t!("access.notice.denied", target = target).to_string(),
            },
        ),
        ("revoked", true) => (
            ToastKind::Warning,
            t!("access.notice.revoked", target = target, name = by).to_string(),
        ),
        ("expired", true) => (
            ToastKind::Info,
            t!("access.notice.expired", target = target).to_string(),
        ),
        ("ended", true) => (
            ToastKind::Info,
            t!("access.notice.ended", target = target).to_string(),
        ),
        _ => return None,
    })
}

// ----- The shared book -----

/// Answers about hosts younger than this are reused.
const HOST_FRESH: Duration = Duration::from_secs(30);

/// What is known of one account.
#[derive(Default)]
pub struct AccountAccess {
    /// Requests you can see as an approver (pending and recent), newest
    /// first.
    pub approvals: Vec<Request>,
    /// Your own requests, newest first.
    pub mine: Vec<Request>,
    pub loaded: bool,
    hosts: HashMap<Id, (HostAccess, Instant)>,
    loading_hosts: HashSet<Id>,
    loading: bool,
}

impl AccountAccess {
    /// Requests waiting for your decision.
    pub fn waiting(&self) -> impl Iterator<Item = &Request> {
        self.approvals
            .iter()
            .filter(|r| r.status == AccessRequestStatus::Pending && r.can_decide)
    }
}

/// The access state of every account, shared by every window.
#[derive(Default)]
pub struct AccessBook {
    accounts: HashMap<Id, AccountAccess>,
}

struct AccessBookGlobal(gpui::Entity<AccessBook>);

impl gpui::Global for AccessBookGlobal {}

/// The window of a host as the host list shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostWindow {
    /// Open until (ms).
    Open(i64),
    /// A request of yours waits.
    Pending,
    /// It needs an approved window (you have none).
    Needed,
}

impl AccessBook {
    pub fn global(cx: &mut gpui::App) -> gpui::Entity<Self> {
        use gpui::AppContext;
        if let Some(g) = cx.try_global::<AccessBookGlobal>() {
            return g.0.clone();
        }
        let entity = cx.new(|_| AccessBook::default());
        cx.set_global(AccessBookGlobal(entity.clone()));
        entity
    }

    /// The global book if it exists (read-only places).
    pub fn try_global(cx: &gpui::App) -> Option<gpui::Entity<Self>> {
        cx.try_global::<AccessBookGlobal>().map(|g| g.0.clone())
    }

    pub fn account(&self, id: Id) -> Option<&AccountAccess> {
        self.accounts.get(&id)
    }

    /// Requests waiting for your decision in every account (the badge).
    pub fn waiting_count(&self) -> usize {
        self.accounts.values().map(|a| a.waiting().count()).sum()
    }

    /// Forgets an account (signed out).
    pub fn forget(&mut self, account: Id, cx: &mut gpui::Context<Self>) {
        if self.accounts.remove(&account).is_some() {
            cx.notify();
        }
    }

    /// Forgets the host answers of an account (a request changed, a policy
    /// changed).
    pub fn forget_hosts(&mut self, account: Id, cx: &mut gpui::Context<Self>) {
        if let Some(a) = self.accounts.get_mut(&account) {
            a.hosts.clear();
            cx.notify();
        }
    }

    /// Loads the requests of an account (approvals and yours).
    pub fn refresh(&mut self, account: Id, api: ApiClient, cx: &mut gpui::Context<Self>) {
        let entry = self.accounts.entry(account).or_default();
        if entry.loading {
            return;
        }
        entry.loading = true;
        crate::runtime::run(
            cx,
            async move {
                let approvals: Vec<Request> = api
                    .get("/api/v1/access-requests?scope=approvals&limit=100")
                    .await?;
                let mine: Vec<Request> = api
                    .get("/api/v1/access-requests?scope=mine&limit=50")
                    .await?;
                Ok::<_, ClientError>((approvals, mine))
            },
            move |this: &mut Self, res, cx| {
                let entry = this.accounts.entry(account).or_default();
                entry.loading = false;
                match res {
                    Ok((approvals, mine)) => {
                        entry.approvals = approvals;
                        entry.mine = mine;
                        entry.loaded = true;
                    }
                    Err(e) => tracing::info!(error = %e, "could not load the access requests"),
                }
                cx.notify();
            },
        );
    }

    /// An `access_request` event of an account: the lists and host answers
    /// are read again.
    pub fn on_event(&mut self, account: Id, api: ApiClient, cx: &mut gpui::Context<Self>) {
        // The hosts already known are asked again (their chips and
        // terminals follow), the rest when they are next shown.
        let known: Vec<Id> = self
            .accounts
            .get(&account)
            .map(|a| a.hosts.keys().copied().collect())
            .unwrap_or_default();
        for host in known {
            self.ensure_host(account, host, api.clone(), true, cx);
        }
        self.refresh(account, api, cx);
    }

    /// The server's answer about a host, if known.
    pub fn host(&self, account: Id, host: Id) -> Option<&HostAccess> {
        self.accounts
            .get(&account)
            .and_then(|a| a.hosts.get(&host))
            .map(|(h, _)| h)
    }

    /// Asks the server about a host when the answer is missing or old
    /// (`force`: always).
    pub fn ensure_host(
        &mut self,
        account: Id,
        host: Id,
        api: ApiClient,
        force: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let entry = self.accounts.entry(account).or_default();
        let fresh = entry
            .hosts
            .get(&host)
            .is_some_and(|(_, at)| at.elapsed() < HOST_FRESH);
        if (fresh && !force) || entry.loading_hosts.contains(&host) {
            return;
        }
        entry.loading_hosts.insert(host);
        crate::runtime::run(
            cx,
            async move {
                api.get::<HostAccess>(&format!("/api/v1/hosts/{host}/access"))
                    .await
            },
            move |this: &mut Self, res, cx| {
                let entry = this.accounts.entry(account).or_default();
                entry.loading_hosts.remove(&host);
                match res {
                    Ok(a) => {
                        entry.hosts.insert(host, (a, Instant::now()));
                    }
                    Err(e) => {
                        // Keep the old answer, but ask again next time.
                        if let Some((_, at)) = entry.hosts.get_mut(&host) {
                            *at = Instant::now() - HOST_FRESH;
                        }
                        tracing::debug!(error = %e, "host access");
                    }
                }
                cx.notify();
            },
        );
    }

    /// The window of a host (`None`: no rules, exempt, or not known yet).
    pub fn window(&self, account: Id, host: Id, now: i64) -> Option<HostWindow> {
        let a = self.host(account, host)?;
        host_window(a, now)
    }
}

/// [`HostWindow`] of a host access answer.
pub fn host_window(a: &HostAccess, now: i64) -> Option<HostWindow> {
    if let Some(g) = &a.grant
        && let Some(end) = g.ends_at
        && end > now
    {
        return Some(HostWindow::Open(end));
    }
    if a.exempt || !a.mode.requires_approval() {
        return None;
    }
    if a.pending.is_some() {
        return Some(HostWindow::Pending);
    }
    (!a.allowed).then_some(HostWindow::Needed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_core::access::AccessMode;

    fn policy(f: impl FnOnce(&mut VaultPolicy)) -> VaultPolicy {
        let mut p = VaultPolicy::default();
        f(&mut p);
        p
    }

    #[test]
    fn role_picker_maps_the_four_roles_in_order() {
        assert_eq!(
            grantable_roles(true),
            vec![
                VaultRole::ConnectOnly,
                VaultRole::UseOnly,
                VaultRole::Editor,
                VaultRole::Manager
            ]
        );
        // The wire names the server expects.
        let wire: Vec<String> = GRANTABLE_ROLES
            .iter()
            .map(|r| {
                serde_json::to_value(r)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(wire, ["connect_only", "use_only", "editor", "manager"]);
        // Older servers: only the roles they know.
        assert_eq!(
            grantable_roles(false),
            vec![VaultRole::UseOnly, VaultRole::Editor]
        );
        for r in GRANTABLE_ROLES {
            assert!(!role_hint(r).is_empty());
            assert_ne!(
                role_hint(r).as_ref(),
                format!("vaults.role_hint.{}", r.as_str())
            );
        }
    }

    #[test]
    fn explicit_admins_cannot_delete() {
        assert!(can_delete_vault(VaultRole::Manager, true));
        assert!(!can_delete_vault(VaultRole::Manager, false));
        assert!(!can_delete_vault(VaultRole::Editor, true));
    }

    #[test]
    fn policy_hides_reveal_ai_and_networks() {
        // No policy: the role decides.
        let g = gates(VaultRole::Editor, None);
        assert!(g.reveal && g.ai && g.networks && !g.recorded && !g.access_rules);
        assert!(!gates(VaultRole::UseOnly, None).reveal);
        assert!(!gates(VaultRole::ConnectOnly, None).reveal);
        // Forbid reveal: Editors lose it, managers keep it.
        let p = policy(|p| p.reveal_allowed = false);
        assert!(!gates(VaultRole::Editor, Some(&p)).reveal);
        assert!(gates(VaultRole::Manager, Some(&p)).reveal);
        // AI, networks, recording, rules, certificates.
        let p = policy(|p| {
            p.ai_allowed = false;
            p.networks_allowed = false;
            p.record_sessions = true;
            p.access_policies = true;
            p.certificates = true;
        });
        let g = gates(VaultRole::Manager, Some(&p));
        assert!(!g.ai && !g.networks && g.recorded && g.access_rules && g.certificates);
    }

    #[test]
    fn countdown() {
        let now = 1_000_000_000_000;
        assert_eq!(remaining_secs(now + 42_000, now), 42);
        assert_eq!(remaining_secs(now + 41_001, now), 42);
        assert_eq!(remaining_secs(now - 5_000, now), 0);
        assert_eq!(tick_every(42), Duration::from_secs(1));
        assert_eq!(tick_every(3600), Duration::from_secs(15));
        assert!(ending_soon(299) && !ending_soon(301));
        rust_i18n::set_locale("en");
        assert_eq!(short_remaining(45), "45 s");
        assert_eq!(short_remaining(60), "1 min");
        assert_eq!(short_remaining(61), "2 min");
        assert_eq!(short_remaining(42 * 60), "42 min");
        assert_eq!(short_remaining(3600 + 5 * 60), "1 h 05 min");
        assert!(window_label(now + 600_000, now).contains("10 min"));
        rust_i18n::set_locale("es");
        assert_eq!(short_remaining(3600 + 5 * 60), "1 h 05 min");
        assert!(window_label(now + 600_000, now).starts_with("Acceso hasta"));
        rust_i18n::set_locale("en");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_choices(60), vec![15, 30, 60]);
        assert_eq!(duration_choices(90), vec![15, 30, 60, 90]);
        assert_eq!(duration_choices(10), vec![10]);
        rust_i18n::set_locale("en");
        assert_eq!(duration_label(15), "15 min");
        assert_eq!(duration_label(120), "2 h");
        assert_eq!(duration_label(90), "1 h 30 min");
        assert_eq!(duration_label(1440), "1 day");
        assert_eq!(duration_label(4320), "3 days");
    }

    #[test]
    fn request_form_rules() {
        assert_eq!(request_valid(30, 60, "deploy"), Ok(()));
        assert_eq!(
            request_valid(30, 60, "  "),
            Err("access.request.error.reason")
        );
        assert_eq!(
            request_valid(30, 60, &"x".repeat(501)),
            Err("access.request.error.reason_long")
        );
        assert_eq!(
            request_valid(90, 60, "x"),
            Err("access.request.error.duration")
        );
        let id = Id::nil();
        assert_eq!(
            request_body(AccessTargetKind::Group, id, 30, " fix "),
            json!({"group_id": id, "minutes": 30, "reason": "fix"})
        );
        assert_eq!(
            request_body(AccessTargetKind::Host, id, 5, "x")["host_id"],
            json!(id)
        );
    }

    #[test]
    fn access_need_from_errors() {
        let host = termoak_core::new_id();
        let group = termoak_core::new_id();
        let api = ClientError::Api {
            status: 403,
            code: "access_approval_required".into(),
            message: "needs a window".into(),
            details: json!({"host_id": host, "target_kind": "group", "target_id": group, "max_minutes": 120, "policy_id": null}),
        };
        let need = AccessNeed::from_error(&api).unwrap();
        assert_eq!(need.host_id, Some(host));
        assert_eq!(need.target_kind, AccessTargetKind::Group);
        assert_eq!(need.target_id, Some(group));
        assert_eq!(need.max_minutes, 120);
        assert!(need.can_request && need.pending.is_none());
        // The engine's own check (before a local connection).
        let core: ClientError = CoreError::vault_detail(
            codes::ACCESS_APPROVAL_REQUIRED,
            "x",
            json!({"host_id": host, "max_minutes": 30, "pending": group, "can_request": false}),
        )
        .into();
        let need = AccessNeed::from_error(&core).unwrap();
        assert_eq!(need.pending, Some(group));
        assert!(!need.can_request);
        assert_eq!(need.target_kind, AccessTargetKind::Host);
        // Other errors are not.
        assert!(AccessNeed::from_error(&ClientError::api(403, "connect_only", "x")).is_none());
    }

    #[test]
    fn every_code_and_reason_has_texts() {
        for lang in ["en", "es"] {
            rust_i18n::set_locale(lang);
            for code in ERROR_CODES {
                let text = crate::i18n::api_error_text(code)
                    .unwrap_or_else(|| panic!("{lang}: error.{code} is missing"));
                assert!(!text.starts_with("error."), "{lang}: {code}");
            }
            for reason in CLOSE_REASONS {
                let text = close_reason_text(reason).unwrap();
                assert!(!text.starts_with("access."), "{lang}: {reason}");
            }
        }
        rust_i18n::set_locale("en");
        assert!(close_reason_text("other").is_none());
        assert!(is_teams_code("access_approval_required") && !is_teams_code("not_found"));
    }

    fn request(status: &str, requester: Id, ends: Option<i64>) -> Value {
        json!({
            "id": Id::nil(), "vault_id": Id::nil(), "target_kind": "host", "target_id": Id::nil(),
            "target_label": "db-1", "requester_id": requester, "requester_name": "Ana",
            "requester_email": "ana@example.com", "reason": "deploy", "minutes": 60,
            "status": status, "created_at": 0, "expires_at": 0, "ends_at": ends,
            "decided_by_name": "Bea", "decision_reason": null
        })
    }

    #[test]
    fn notices_of_events() {
        rust_i18n::set_locale("en");
        let me = termoak_core::new_id();
        let other = termoak_core::new_id();
        // An approver hears about a new request; the requester does not.
        let ev = json!({"type": "access_request", "event": "created", "request": request("pending", other, None)});
        let (kind, text) = event_notice(Some(me), &ev).unwrap();
        assert_eq!(kind, ToastKind::Info);
        assert!(text.contains("Ana") && text.contains("db-1") && text.contains("1 h"));
        let ev = json!({"type": "access_request", "event": "created", "request": request("pending", me, None)});
        assert!(event_notice(Some(me), &ev).is_none());
        // The requester hears the decision.
        let ev = json!({"type": "access_request", "event": "approved", "request": request("approved", me, Some(1_700_000_000_000i64))});
        assert_eq!(event_notice(Some(me), &ev).unwrap().0, ToastKind::Success);
        let ev = json!({"type": "access_request", "event": "denied", "request": request("denied", me, None)});
        assert_eq!(event_notice(Some(me), &ev).unwrap().0, ToastKind::Warning);
        let ev = json!({"type": "access_request", "event": "revoked", "request": request("revoked", me, None)});
        assert!(event_notice(Some(me), &ev).unwrap().1.contains("Bea"));
        assert!(event_notice(Some(me), &json!({"type": "vault"})).is_none());
    }

    #[test]
    fn host_windows() {
        let now = 1_000_000;
        let req: AccessRequest =
            serde_json::from_value(request("approved", Id::nil(), Some(now + 60_000))).unwrap();
        let mut a = HostAccess {
            host_id: Id::nil(),
            vault_id: Id::nil(),
            mode: AccessMode::Approval,
            policy: None,
            exempt: false,
            grant: Some(req.clone()),
            pending: None,
            can_request: true,
            allowed: true,
            certificates: false,
            team_certificate: false,
            record_required: false,
            ai_allowed: true,
            reveal_allowed: true,
            max_minutes: 60,
        };
        assert_eq!(host_window(&a, now), Some(HostWindow::Open(now + 60_000)));
        // Ended: needs a new one.
        assert_eq!(host_window(&a, now + 61_000), None);
        a.grant = None;
        a.allowed = false;
        assert_eq!(host_window(&a, now), Some(HostWindow::Needed));
        a.pending = Some(req);
        assert_eq!(host_window(&a, now), Some(HostWindow::Pending));
        a.exempt = true;
        a.pending = None;
        assert_eq!(host_window(&a, now), None);
        a.exempt = false;
        a.mode = AccessMode::Direct;
        assert_eq!(host_window(&a, now), None);
    }
}
