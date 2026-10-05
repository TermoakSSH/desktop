//! Live session sharing, the parts without interface: the share form and
//! its request bodies, the shares as listed by the server, the people in a
//! session as shown in the participants bar, links to join a session and
//! the messages shown when the server sends someone away.
//!
//! Protocol: `WEBSOCKET-PROTOCOL.md` and `API.md` of the server.

use gpui::SharedString;
use serde_json::{Value, json};
use termoak_client::remote::Participant;
use termoak_core::Id;

// ----- Share form -----

/// Expiry of a new share (or the new one of an existing share).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    Never,
    Hour,
    EightHours,
    Day,
    Week,
    Month,
}

impl Expiry {
    pub const ALL: [Expiry; 6] = [
        Expiry::Never,
        Expiry::Hour,
        Expiry::EightHours,
        Expiry::Day,
        Expiry::Week,
        Expiry::Month,
    ];

    /// Minutes from now (`None`: it does not expire).
    pub fn minutes(self) -> Option<i64> {
        match self {
            Expiry::Never => None,
            Expiry::Hour => Some(60),
            Expiry::EightHours => Some(8 * 60),
            Expiry::Day => Some(24 * 60),
            Expiry::Week => Some(7 * 24 * 60),
            Expiry::Month => Some(30 * 24 * 60),
        }
    }

    pub fn label(self) -> SharedString {
        match self {
            Expiry::Never => t!("share.expiry.never"),
            Expiry::Hour => t!("share.expiry.hour"),
            Expiry::EightHours => t!("share.expiry.eight_hours"),
            Expiry::Day => t!("share.expiry.day"),
            Expiry::Week => t!("share.expiry.week"),
            Expiry::Month => t!("share.expiry.month"),
        }
    }
}

/// Who a new share is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A user of the server, by email.
    User(String),
    /// Every member of one of your teams (`None`: none chosen yet).
    Team(Option<Id>),
    /// Anyone with the link (no account needed).
    Link,
}

/// What the share dialog asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareForm {
    pub target: Target,
    /// `control`: can ask for the keyboard; otherwise only watch.
    pub control: bool,
    pub expiry: Expiry,
    /// Whoever joins waits until the owner lets them in.
    pub require_approval: bool,
    /// Requests for the keyboard are granted without asking.
    pub auto_grant: bool,
}

/// Why a share form cannot be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormError {
    InvalidEmail,
    NoTeam,
}

impl FormError {
    pub fn message(self) -> SharedString {
        match self {
            FormError::InvalidEmail => t!("share.invalid_email"),
            FormError::NoTeam => t!("share.choose_team"),
        }
    }
}

/// A plausible email address: something before and after a single `@`, a
/// dot in the domain and no spaces. The server has the last word.
pub fn valid_email(email: &str) -> bool {
    let email = email.trim();
    let Some((user, domain)) = email.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !email.chars().any(char::is_whitespace)
}

/// Default of "Ask me before letting people in" for a target: on for links
/// (anyone could have them), off for people and teams you chose.
pub fn approval_default(target: &Target) -> bool {
    matches!(target, Target::Link)
}

impl ShareForm {
    pub fn new(target: Target) -> Self {
        Self {
            require_approval: approval_default(&target),
            target,
            control: false,
            expiry: Expiry::Never,
            auto_grant: false,
        }
    }

    /// Body of `POST /sessions/{id}/shares`.
    pub fn body(&self) -> Result<Value, FormError> {
        let mut body = match &self.target {
            Target::User(email) => {
                let email = email.trim();
                if !valid_email(email) {
                    return Err(FormError::InvalidEmail);
                }
                json!({"email": email})
            }
            Target::Team(Some(team)) => json!({"team_id": team}),
            Target::Team(None) => return Err(FormError::NoTeam),
            Target::Link => json!({"link": true}),
        };
        body["permission"] = json!(permission(self.control));
        body["require_approval"] = json!(self.require_approval);
        // Without the keyboard there is nothing to grant.
        body["auto_grant"] = json!(self.control && self.auto_grant);
        if let Some(m) = self.expiry.minutes() {
            body["expires_in_minutes"] = json!(m);
        }
        Ok(body)
    }
}

fn permission(control: bool) -> &'static str {
    if control { "control" } else { "view" }
}

/// One change to an existing share, applied at once (`PATCH
/// /sessions/{id}/shares/{share_id}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareChange {
    Control(bool),
    Expiry(Expiry),
    RequireApproval(bool),
    AutoGrant(bool),
}

impl ShareChange {
    pub fn body(self) -> Value {
        match self {
            ShareChange::Control(c) => {
                let mut v = json!({"permission": permission(c)});
                if !c {
                    v["auto_grant"] = json!(false);
                }
                v
            }
            ShareChange::Expiry(e) => match e.minutes() {
                Some(m) => json!({"expires_in_minutes": m}),
                None => json!({"no_expiry": true}),
            },
            ShareChange::RequireApproval(r) => json!({"require_approval": r}),
            ShareChange::AutoGrant(a) => json!({"auto_grant": a}),
        }
    }
}

/// Who a share is for, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShareWho {
    User { email: String, name: Option<String> },
    Team(String),
    Link,
}

/// A share as listed by `GET /sessions/{id}/shares`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareRow {
    pub id: Id,
    pub who: ShareWho,
    pub control: bool,
    pub expires_at: Option<i64>,
    /// Not revoked nor expired.
    pub active: bool,
    /// People inside with it now.
    pub participants: usize,
    pub require_approval: bool,
    pub auto_grant: bool,
    pub created_at: i64,
}

impl ShareRow {
    pub fn from_json(v: &Value) -> Option<Self> {
        let who = if v["is_link"].as_bool() == Some(true) {
            ShareWho::Link
        } else if let Some(email) = v["user_email"].as_str() {
            ShareWho::User {
                email: email.to_string(),
                name: v["user_name"]
                    .as_str()
                    .filter(|n| !n.trim().is_empty())
                    .map(str::to_string),
            }
        } else if v["team_id"].is_string() {
            ShareWho::Team(v["team_name"].as_str().unwrap_or("").to_string())
        } else {
            ShareWho::User {
                email: String::new(),
                name: None,
            }
        };
        let revoked = v["revoked"].as_bool().unwrap_or(false);
        Some(Self {
            id: v["id"].as_str()?.parse().ok()?,
            who,
            control: v["permission"] == "control",
            expires_at: v["expires_at"].as_i64(),
            active: v["active"].as_bool().unwrap_or(!revoked),
            participants: v["participants"].as_u64().unwrap_or(0) as usize,
            require_approval: v["require_approval"].as_bool().unwrap_or(false),
            auto_grant: v["auto_grant"].as_bool().unwrap_or(false),
            created_at: v["created_at"].as_i64().unwrap_or(0),
        })
    }

    /// The active shares of a list, newest first.
    pub fn active_list(v: &Value) -> Vec<ShareRow> {
        let mut rows: Vec<ShareRow> = v
            .as_array()
            .map(|a| a.iter().filter_map(ShareRow::from_json).collect())
            .unwrap_or_default();
        rows.retain(|r| r.active);
        rows.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        rows
    }

    /// Who it is for, in words.
    pub fn label(&self) -> String {
        match &self.who {
            ShareWho::User { email, name } => match name {
                Some(n) if !email.is_empty() => format!("{n} · {email}"),
                Some(n) => n.clone(),
                None => email.clone(),
            },
            ShareWho::Team(name) if name.is_empty() => t!("share.team_fallback").to_string(),
            ShareWho::Team(name) => t!("share.row.team", name = name).to_string(),
            ShareWho::Link => t!("share.row.link").to_string(),
        }
    }
}

/// The links of a link share, shown once (`{link, app_link}` of the
/// creation). The app link is built from the web one when the server does
/// not send it.
pub fn created_links(v: &Value, server: &str) -> Option<(String, String)> {
    let token = v["token"].as_str().filter(|t| !t.is_empty());
    let web = v["link"]
        .as_str()
        .map(str::to_string)
        .or_else(|| token.map(|t| format!("{}/join/{t}", server.trim_end_matches('/'))))?;
    let app = v["app_link"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            let parsed = parse_join_link(&web)?;
            Some(parsed.app_link())
        })
        .unwrap_or_default();
    Some((web, app))
}

// ----- Joining with a link -----

/// A link to join a shared session: the server and the share token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinLink {
    /// Server URL, without a trailing slash.
    pub server: String,
    pub token: String,
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 256
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Reads a link to join a session:
/// - `termoak://join?server=<url>&token=<token>` (also `aceitunoak://`, the
///   scheme before the rename),
/// - `https://<server>/join/<token>` (the web page),
/// - `https://<server>/api/v1/join/<token>` (servers without the web app).
pub fn parse_join_link(text: &str) -> Option<JoinLink> {
    let url = url::Url::parse(text.trim()).ok()?;
    match url.scheme() {
        "termoak" | "aceitunoak" => {
            // `termoak://join?...`: the host is the action.
            if url.host_str() != Some("join") {
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
            let server = server.filter(|s| !s.is_empty())?;
            let parsed = url::Url::parse(&server).ok()?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return None;
            }
            let token = token.filter(|t| valid_token(t))?;
            Some(JoinLink {
                server: server.trim_end_matches('/').to_string(),
                token,
            })
        }
        "http" | "https" => {
            let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
            let at = segments.iter().rposition(|s| *s == "join")?;
            // Exactly one segment after `join`.
            if at + 2 != segments.len() {
                return None;
            }
            let token = segments[at + 1].to_string();
            if !valid_token(&token) {
                return None;
            }
            // Whatever comes before `/join` (or `/api/v1/join`) is the server
            // (it may live under a path).
            let mut prefix = &segments[..at];
            if prefix.ends_with(&["api", "v1"]) {
                prefix = &prefix[..prefix.len() - 2];
            }
            let mut server = format!("{}://{}", url.scheme(), url.host_str()?);
            if let Some(port) = url.port() {
                server.push_str(&format!(":{port}"));
            }
            for s in prefix {
                server.push('/');
                server.push_str(s);
            }
            Some(JoinLink { server, token })
        }
        _ => None,
    }
}

impl JoinLink {
    /// `termoak://join?server=…&token=…`.
    pub fn app_link(&self) -> String {
        let server: String = url::form_urlencoded::byte_serialize(self.server.as_bytes()).collect();
        format!("termoak://join?server={server}&token={}", self.token)
    }

    /// Path of the public data of the link (`GET`, no account needed).
    pub fn info_path(&self) -> String {
        format!("/api/v1/join/{}", self.token)
    }

    /// The same server as `other` (ignoring case and a trailing slash).
    pub fn same_server(&self, other: &str) -> bool {
        normalize_server(&self.server) == normalize_server(other)
    }
}

fn normalize_server(s: &str) -> String {
    s.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Public data of a link (`GET /api/v1/join/{token}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinInfo {
    pub session_id: Id,
    pub title: String,
    pub owner: String,
    /// Can ask for the keyboard.
    pub control: bool,
    pub require_approval: bool,
    pub expires_at: Option<i64>,
    /// People inside now.
    pub participants: usize,
    pub ws_path: String,
}

impl JoinInfo {
    pub fn from_json(v: &Value) -> Option<Self> {
        let session = &v["session"];
        let session_id: Id = session["id"].as_str()?.parse().ok()?;
        Some(Self {
            session_id,
            title: session["title"]
                .as_str()
                .filter(|t| !t.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| t!("server_sessions.default_title").to_string()),
            owner: v["owner"].as_str().unwrap_or("").to_string(),
            control: v["permission"] == "control",
            require_approval: v["require_approval"].as_bool().unwrap_or(false),
            expires_at: v["expires_at"].as_i64(),
            participants: session["participants"].as_u64().unwrap_or(0) as usize,
            ws_path: v["ws_path"].as_str()?.to_string(),
        })
    }
}

/// A display name for a guest: trimmed, without control characters, at
/// most 40 characters (the server does the same).
pub fn clean_guest_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    cleaned
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(40)
        .collect()
}

// ----- End of a shared session -----

/// Title and explanation when the server sends you away for good (`code`
/// of the `error` before the close, see `END_CODES`).
pub fn end_message(code: &str) -> (SharedString, SharedString) {
    match code {
        "revoked" => (
            t!("share.end.revoked.title"),
            t!("share.end.revoked.detail"),
        ),
        "kicked" => (t!("share.end.kicked.title"), t!("share.end.kicked.detail")),
        "expired" => (
            t!("share.end.expired.title"),
            t!("share.end.expired.detail"),
        ),
        "session_ended" => (
            t!("share.end.session_ended.title"),
            t!("share.end.session_ended.detail"),
        ),
        "join_denied" => (
            t!("share.end.join_denied.title"),
            t!("share.end.join_denied.detail"),
        ),
        _ => (
            t!("share.end.forbidden.title"),
            t!("share.end.forbidden.detail"),
        ),
    }
}

// ----- Participants -----

/// Role of a participant, as shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Owner,
    /// Account on the server.
    User,
    /// Link guest, no account.
    Guest,
}

impl Role {
    pub fn label(self) -> SharedString {
        match self {
            Role::Owner => t!("share.role.owner"),
            Role::User => t!("share.role.user"),
            Role::Guest => t!("share.role.guest"),
        }
    }
}

/// A participant as shown in the participants bar and popover.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantRow {
    pub id: Id,
    pub name: String,
    pub initials: String,
    /// Index in the avatar palette (stable for the same participant).
    pub color: usize,
    pub role: Role,
    pub devices: u32,
    pub is_driver: bool,
    pub requested_control: bool,
    pub waiting: bool,
    pub you: bool,
    /// Their share lets them have the keyboard.
    pub can_control: bool,
    pub since: i64,
}

/// Number of avatar colors.
pub const AVATAR_COLORS: usize = 8;

/// Up to two initials of a name ("Ana María" → "AM"; "zoe" → "Z").
pub fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters
    }
}

/// Avatar color of a participant: the same on every device and every
/// render.
pub fn avatar_color(id: &Id) -> usize {
    // FNV-1a over the bytes of the id.
    let mut h: u32 = 0x811c_9dc5;
    for b in id.as_bytes() {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    (h as usize) % AVATAR_COLORS
}

impl ParticipantRow {
    pub fn new(p: &Participant, driver: Option<Id>) -> Self {
        let role = match p.kind.as_str() {
            "owner" => Role::Owner,
            "guest" => Role::Guest,
            _ if p.access == "owner" => Role::Owner,
            _ => Role::User,
        };
        // The owner drives when nobody else does.
        let is_driver = p.is_driver
            || match driver {
                Some(d) => d == p.id,
                None => role == Role::Owner,
            };
        let name = if p.name.trim().is_empty() {
            t!("share.role.guest").to_string()
        } else {
            p.name.clone()
        };
        Self {
            id: p.id,
            initials: initials(&name),
            name,
            color: avatar_color(&p.id),
            role,
            devices: p.devices,
            is_driver,
            requested_control: p.requested_control,
            waiting: p.waiting,
            you: p.you,
            can_control: p.access == "control",
            since: p.since,
        }
    }
}

/// The people in a session as shown: those inside (owner first, then
/// whoever drives, then by arrival) and, apart, those in the waiting room.
pub fn participant_rows(
    list: &[Participant],
    driver: Option<Id>,
) -> (Vec<ParticipantRow>, Vec<ParticipantRow>) {
    let rows: Vec<ParticipantRow> = list
        .iter()
        .map(|p| ParticipantRow::new(p, driver))
        .collect();
    let (mut waiting, mut inside): (Vec<_>, Vec<_>) = rows.into_iter().partition(|r| r.waiting);
    inside.sort_by_key(|r| (r.role != Role::Owner, !r.is_driver, r.since));
    waiting.sort_by_key(|r| r.since);
    (inside, waiting)
}

/// Who has the keyboard, if it is someone other than you (for "Ana is
/// driving").
pub fn other_driver(rows: &[ParticipantRow]) -> Option<&ParticipantRow> {
    rows.iter().find(|r| r.is_driver && !r.you)
}

/// The owner of the session, from the list.
pub fn owner_name(rows: &[ParticipantRow]) -> Option<&str> {
    rows.iter()
        .find(|r| r.role == Role::Owner)
        .map(|r| r.name.as_str())
}

// ----- Notices of the events WebSocket -----

/// A notice about a shared session (`{"type":"session","notice":{…}}` of
/// `/api/v1/events/ws`) that deserves a toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionNotice {
    JoinRequest {
        session_id: Id,
        title: String,
        name: String,
        /// Who asks (the same id the open terminal sees).
        participant: Option<Id>,
    },
    ControlRequest {
        session_id: Id,
        title: String,
        name: String,
        participant: Option<Id>,
    },
    ControlGranted {
        session_id: Id,
    },
    ControlRevoked {
        session_id: Id,
    },
    PromptPending {
        session_id: Id,
        host: String,
    },
    /// Someone shared a session with you (directly or through a team).
    SessionShared {
        session_id: Id,
        title: String,
        by: String,
        team: Option<String>,
    },
}

impl SessionNotice {
    pub fn from_event(v: &Value) -> Option<Self> {
        if v["type"] != "session" {
            return None;
        }
        let n = &v["notice"];
        if n["type"] == "session_shared" {
            let s = &n["session"];
            return Some(SessionNotice::SessionShared {
                session_id: s["id"].as_str()?.parse().ok()?,
                title: s["title"].as_str().unwrap_or("").to_string(),
                by: n["by"]
                    .as_str()
                    .filter(|b| !b.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| t!("share.role.guest").to_string()),
                team: n["team"]
                    .as_str()
                    .filter(|t| !t.trim().is_empty())
                    .map(str::to_string),
            });
        }
        let session_id: Id = n["session_id"].as_str()?.parse().ok()?;
        let participant = n["participant"]["id"].as_str().and_then(|s| s.parse().ok());
        let title = || n["title"].as_str().unwrap_or("").to_string();
        let name = || {
            n["participant"]["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| t!("share.role.guest").to_string())
        };
        Some(match n["type"].as_str()? {
            "join_request" => SessionNotice::JoinRequest {
                session_id,
                title: title(),
                name: name(),
                participant,
            },
            "control_request" => SessionNotice::ControlRequest {
                session_id,
                title: title(),
                name: name(),
                participant,
            },
            "control_granted" => SessionNotice::ControlGranted { session_id },
            "control_revoked" => SessionNotice::ControlRevoked { session_id },
            "prompt_pending" => SessionNotice::PromptPending {
                session_id,
                host: n["prompt"]["host"].as_str().unwrap_or("").to_string(),
            },
            _ => return None,
        })
    }

    pub fn session_id(&self) -> Id {
        match self {
            SessionNotice::JoinRequest { session_id, .. }
            | SessionNotice::ControlRequest { session_id, .. }
            | SessionNotice::ControlGranted { session_id }
            | SessionNotice::ControlRevoked { session_id }
            | SessionNotice::PromptPending { session_id, .. }
            | SessionNotice::SessionShared { session_id, .. } => *session_id,
        }
    }

    /// The owner has to answer something (it counts in the Sessions badge).
    pub fn needs_owner(&self) -> bool {
        matches!(
            self,
            SessionNotice::JoinRequest { .. }
                | SessionNotice::ControlRequest { .. }
                | SessionNotice::PromptPending { .. }
        )
    }

    /// Text of the toast.
    pub fn text(&self) -> String {
        let with_title = |t: &str| {
            if t.is_empty() {
                t!("server_sessions.default_title").to_string()
            } else {
                t.to_string()
            }
        };
        match self {
            SessionNotice::JoinRequest { title, name, .. } => t!(
                "share.notice.join_request",
                name = name,
                title = with_title(title)
            )
            .to_string(),
            SessionNotice::ControlRequest { title, name, .. } => t!(
                "share.notice.control_request",
                name = name,
                title = with_title(title)
            )
            .to_string(),
            SessionNotice::ControlGranted { .. } => t!("share.notice.control_granted").to_string(),
            SessionNotice::ControlRevoked { .. } => t!("share.notice.control_revoked").to_string(),
            SessionNotice::PromptPending { host, .. } if host.is_empty() => {
                t!("share.notice.prompt_pending_any").to_string()
            }
            SessionNotice::PromptPending { host, .. } => {
                t!("share.notice.prompt_pending", host = host).to_string()
            }
            SessionNotice::SessionShared {
                title,
                by,
                team: Some(team),
                ..
            } => t!(
                "share.notice.session_shared_team",
                name = by,
                team = team,
                title = with_title(title)
            )
            .to_string(),
            SessionNotice::SessionShared { title, by, .. } => t!(
                "share.notice.session_shared",
                name = by,
                title = with_title(title)
            )
            .to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn participant(name: &str, kind: &str, access: &str) -> Participant {
        Participant {
            id: termoak_core::new_id(),
            name: name.into(),
            kind: kind.into(),
            access: access.into(),
            is_driver: false,
            since: 0,
            devices: 1,
            requested_control: false,
            waiting: false,
            you: false,
            user_id: None,
            share_id: None,
        }
    }

    #[test]
    fn form_bodies() {
        let mut f = ShareForm::new(Target::User(" ana@example.com ".into()));
        assert!(!f.require_approval);
        f.control = true;
        f.auto_grant = true;
        f.expiry = Expiry::EightHours;
        let b = f.body().unwrap();
        assert_eq!(b["email"], "ana@example.com");
        assert_eq!(b["permission"], "control");
        assert_eq!(b["auto_grant"], true);
        assert_eq!(b["require_approval"], false);
        assert_eq!(b["expires_in_minutes"], 480);

        // Links wait for approval by default and never expire unless asked.
        let f = ShareForm::new(Target::Link);
        assert!(f.require_approval);
        let b = f.body().unwrap();
        assert_eq!(b["link"], true);
        assert_eq!(b["permission"], "view");
        assert!(b.get("expires_in_minutes").is_none());

        // View only: nothing to grant automatically.
        let mut f = ShareForm::new(Target::Link);
        f.auto_grant = true;
        assert_eq!(f.body().unwrap()["auto_grant"], false);

        let team = termoak_core::new_id();
        let b = ShareForm::new(Target::Team(Some(team))).body().unwrap();
        assert_eq!(b["team_id"], team.to_string());
    }

    #[test]
    fn form_validation() {
        for bad in [
            "",
            "ana",
            "ana@",
            "@example.com",
            "ana@example",
            "a b@c.d",
            "a@b@c.d",
        ] {
            assert_eq!(
                ShareForm::new(Target::User(bad.into())).body(),
                Err(FormError::InvalidEmail),
                "{bad}"
            );
        }
        assert!(valid_email("ana.maria+x@sub.example.com"));
        assert_eq!(
            ShareForm::new(Target::Team(None)).body(),
            Err(FormError::NoTeam)
        );
    }

    #[test]
    fn expiry_minutes() {
        assert_eq!(Expiry::Never.minutes(), None);
        assert_eq!(Expiry::Hour.minutes(), Some(60));
        assert_eq!(Expiry::Day.minutes(), Some(1440));
        assert_eq!(Expiry::Week.minutes(), Some(10080));
        assert_eq!(Expiry::Month.minutes(), Some(43200));
    }

    #[test]
    fn changes() {
        assert_eq!(
            ShareChange::Control(false).body(),
            json!({"permission": "view", "auto_grant": false})
        );
        assert_eq!(
            ShareChange::Control(true).body(),
            json!({"permission": "control"})
        );
        assert_eq!(
            ShareChange::Expiry(Expiry::Never).body(),
            json!({"no_expiry": true})
        );
        assert_eq!(
            ShareChange::Expiry(Expiry::Hour).body(),
            json!({"expires_in_minutes": 60})
        );
        assert_eq!(
            ShareChange::RequireApproval(false).body(),
            json!({"require_approval": false})
        );
    }

    #[test]
    fn share_rows() {
        let id = termoak_core::new_id();
        let list = json!([
            {"id": id, "is_link": false, "user_id": termoak_core::new_id(), "user_email": "ana@x.com",
             "user_name": "Ana", "permission": "control", "active": true, "participants": 2,
             "require_approval": false, "auto_grant": true, "created_at": 5, "revoked": false},
            {"id": termoak_core::new_id(), "is_link": true, "permission": "view", "active": true,
             "require_approval": true, "created_at": 9, "expires_at": 100, "revoked": false},
            {"id": termoak_core::new_id(), "is_link": false, "team_id": termoak_core::new_id(),
             "team_name": "Ops", "permission": "view", "active": false, "revoked": true, "created_at": 7},
        ]);
        let rows = ShareRow::active_list(&list);
        assert_eq!(rows.len(), 2);
        // Newest first.
        assert_eq!(rows[0].who, ShareWho::Link);
        assert!(rows[0].require_approval && !rows[0].control);
        assert_eq!(rows[0].expires_at, Some(100));
        assert_eq!(rows[1].id, id);
        assert_eq!(rows[1].label(), "Ana · ana@x.com");
        assert!(rows[1].control && rows[1].auto_grant);
        assert_eq!(rows[1].participants, 2);
    }

    #[test]
    fn join_links() {
        let want = JoinLink {
            server: "https://ssh.example.com".into(),
            token: "abc_DEF-123".into(),
        };
        assert_eq!(
            parse_join_link(
                "termoak://join?server=https%3A%2F%2Fssh.example.com%2F&token=abc_DEF-123"
            ),
            Some(want.clone())
        );
        assert_eq!(
            parse_join_link(
                " aceitunoak://join?server=https%3A%2F%2Fssh.example.com&token=abc_DEF-123 "
            ),
            Some(want.clone())
        );
        assert_eq!(
            parse_join_link("https://ssh.example.com/join/abc_DEF-123"),
            Some(want.clone())
        );
        assert_eq!(
            parse_join_link("https://ssh.example.com/api/v1/join/abc_DEF-123"),
            Some(want.clone())
        );
        assert_eq!(
            want.app_link(),
            "termoak://join?server=https%3A%2F%2Fssh.example.com&token=abc_DEF-123"
        );
        assert_eq!(parse_join_link(&want.app_link()), Some(want.clone()));
        // Port and a path prefix are part of the server.
        assert_eq!(
            parse_join_link("http://localhost:7722/termoak/join/tok"),
            Some(JoinLink {
                server: "http://localhost:7722/termoak".into(),
                token: "tok".into()
            })
        );
        for bad in [
            "",
            "hello",
            "termoak://invite?server=https%3A%2F%2Fa.b&token=x",
            "termoak://join?token=x",
            "termoak://join?server=https%3A%2F%2Fa.b&token=",
            "termoak://join?server=ftp%3A%2F%2Fa.b&token=x",
            "termoak://join?server=https%3A%2F%2Fa.b&token=a%20b",
            "https://ssh.example.com/join/",
            "https://ssh.example.com/join/a/b",
            "https://ssh.example.com/sessions/abc",
            "mailto:join@example.com",
        ] {
            assert_eq!(parse_join_link(bad), None, "{bad}");
        }
        assert!(want.same_server("HTTPS://ssh.example.com/"));
        assert!(!want.same_server("https://other.example.com"));
    }

    #[test]
    fn created_link_pair() {
        let v = json!({"token": "tok", "link": "https://a.b/join/tok", "app_link": "termoak://join?server=https%3A%2F%2Fa.b&token=tok"});
        assert_eq!(
            created_links(&v, "https://a.b"),
            Some((
                "https://a.b/join/tok".into(),
                "termoak://join?server=https%3A%2F%2Fa.b&token=tok".into()
            ))
        );
        // Older servers: no app link.
        let v = json!({"token": "tok", "link": "https://a.b/api/v1/join/tok"});
        assert_eq!(
            created_links(&v, "https://a.b").unwrap().1,
            "termoak://join?server=https%3A%2F%2Fa.b&token=tok"
        );
        assert_eq!(created_links(&json!({}), "https://a.b"), None);
    }

    #[test]
    fn join_info() {
        let id = termoak_core::new_id();
        let v = json!({"session": {"id": id, "title": "prod", "participants": 3}, "owner": "Ana",
                       "permission": "control", "require_approval": true, "expires_at": null,
                       "ws_path": format!("/api/v1/sessions/{id}/ws?share_token=t")});
        let info = JoinInfo::from_json(&v).unwrap();
        assert_eq!(info.session_id, id);
        assert_eq!((info.title.as_str(), info.owner.as_str()), ("prod", "Ana"));
        assert!(info.control && info.require_approval);
        assert_eq!(info.participants, 3);
        assert!(JoinInfo::from_json(&json!({"session": {"id": id}})).is_none());
    }

    #[test]
    fn guest_names() {
        assert_eq!(clean_guest_name("  Ana \t María\n"), "Ana María");
        assert_eq!(clean_guest_name("a\u{7}b"), "a b");
        assert_eq!(clean_guest_name(&"x".repeat(50)).len(), 40);
        assert_eq!(clean_guest_name("   "), "");
    }

    #[test]
    fn end_messages() {
        let codes = [
            "revoked",
            "kicked",
            "expired",
            "session_ended",
            "join_denied",
            "forbidden",
        ];
        let mut titles = std::collections::BTreeSet::new();
        for code in codes {
            let (title, detail) = end_message(code);
            assert!(!title.is_empty() && !detail.is_empty());
            // Each one says something different.
            assert!(titles.insert(title.to_string()), "{code}");
        }
        assert_eq!(end_message("kicked").0, "You were removed from the session");
        // Unknown codes read as "no access".
        assert_eq!(end_message("whatever"), end_message("forbidden"));
    }

    #[test]
    fn participants_view_model() {
        let owner = participant("Oihana Etxe", "owner", "owner");
        let mut ana = participant("ana", "user", "control");
        ana.since = 10;
        ana.requested_control = true;
        let mut zoe = participant("Zoe", "guest", "view");
        zoe.since = 5;
        zoe.devices = 2;
        zoe.you = true;
        let mut waiting = participant("Guest 1", "guest", "control");
        waiting.waiting = true;

        // Nobody else drives: the owner does.
        let list = vec![ana.clone(), zoe.clone(), owner.clone(), waiting.clone()];
        let (inside, queue) = participant_rows(&list, None);
        assert_eq!(
            inside.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["Oihana Etxe", "Zoe", "ana"]
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].name, "Guest 1");
        assert!(inside[0].is_driver && inside[0].role == Role::Owner);
        assert_eq!(inside[0].initials, "OE");
        assert_eq!(inside[1].role, Role::Guest);
        assert_eq!(inside[1].devices, 2);
        assert!(inside[2].requested_control && inside[2].can_control);
        assert_eq!(owner_name(&inside), Some("Oihana Etxe"));
        // I am Zoe and the owner drives.
        assert_eq!(other_driver(&inside).unwrap().name, "Oihana Etxe");

        // Ana drives: she comes right after the owner, who no longer drives.
        let (inside, _) = participant_rows(&list, Some(ana.id));
        assert_eq!(inside[1].name, "ana");
        assert!(inside[1].is_driver && !inside[0].is_driver);
        assert_eq!(inside[1].initials, "A");
        assert_eq!(other_driver(&inside).unwrap().name, "ana");

        // The color is stable and within the palette.
        assert_eq!(avatar_color(&ana.id), avatar_color(&ana.id));
        assert!(avatar_color(&ana.id) < AVATAR_COLORS);
        assert_eq!(initials("  "), "?");
        assert_eq!(initials("ñandú rápido veloz"), "ÑR");
    }

    #[test]
    fn notices() {
        let sid = termoak_core::new_id();
        let ana = termoak_core::new_id();
        let n = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "join_request", "session_id": sid, "title": "prod",
            "participant": {"id": ana, "name": "Ana"}}}))
        .unwrap();
        assert_eq!(
            n,
            SessionNotice::JoinRequest {
                session_id: sid,
                title: "prod".into(),
                name: "Ana".into(),
                participant: Some(ana),
            }
        );
        assert!(n.needs_owner());
        assert_eq!(n.text(), "Ana wants to join “prod”");
        let g = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "control_granted", "session_id": sid}}))
        .unwrap();
        assert!(!g.needs_owner());
        assert_eq!(g.session_id(), sid);
        assert!(
            SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "session_opened", "session_id": sid}}))
            .is_none()
        );
        assert!(SessionNotice::from_event(&json!({"type": "ai"})).is_none());

        let shared = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "session_shared", "by": "Ana",
            "session": {"id": sid, "title": "prod", "kind": "server"}}}))
        .unwrap();
        assert_eq!(shared.session_id(), sid);
        assert!(!shared.needs_owner());
        assert_eq!(shared.text(), "Ana shared “prod” with you");
        let team = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "session_shared", "by": "Ana", "team": "Ops",
            "session": {"id": sid, "title": ""}}}))
        .unwrap();
        assert_eq!(team.text(), "Ana (team “Ops”) shared “Session” with you");
    }
}
