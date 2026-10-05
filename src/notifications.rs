//! Notifications of the operating system (Notification Center on macOS,
//! toasts on Windows, the freedesktop notification server on Linux) for the
//! notices that also show up as toasts in the window: requests on your
//! shared sessions, sessions shared with you and the AI tasks.
//!
//! They are only posted when the window is not focused (in the background
//! or minimized); with the window in front, the toast in the window is
//! enough. A click brings the window to the front and opens what the notice
//! is about (the tab, the session or the task). Do Not Disturb and the
//! per-app settings of the system apply as usual: the system decides
//! whether to show them.
//!
//! The delivery is GPUI's (`App::show_system_notification`): it uses
//! `UNUserNotificationCenter` on macOS, WinRT toasts on Windows and
//! `notify-rust` over the pure-Rust D-Bus of `zbus` on Linux. This module
//! decides when to post (preferences, focus, repeated notices) and where a
//! click leads.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use gpui::{EntityId, SharedString, SystemNotification};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use termoak_core::Id;

use crate::sharing::SessionNotice;
use crate::state::ToastKind;

/// Identity of the app for the system: the AppUserModelID of the Windows
/// toasts (an unpackaged app needs one) and the app name on Linux. The same
/// identifier as the macOS bundle and the Linux `app_id`.
pub const APP_ID: &str = "com.termoak.Termoak";
pub const APP_NAME: &str = "Termoak";

/// The same notice again within this time is not posted again (it can come
/// from the terminal and from the server's events, or be repeated).
const REPEAT_WINDOW: Duration = Duration::from_secs(15);
/// Remembered notices are forgotten after this time.
const FORGET_AFTER: Duration = Duration::from_secs(120);
/// Where the last notifications lead (older ones just bring the window up).
const MAX_TARGETS: usize = 64;
/// Longest text of a notification (AI errors can be long).
const MAX_BODY: usize = 240;

/// Settings → Notifications (all on by default).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationPrefs {
    /// Desktop notifications at all.
    pub enabled: bool,
    /// Someone wants to join or asks for the keyboard of your sessions, or
    /// one of them waits for an answer.
    pub sharing: bool,
    /// An AI task needs your approval, finished or failed.
    pub ai: bool,
    /// A session was shared with you, or you got or lost its keyboard.
    pub shared_with_me: bool,
}

impl Default for NotificationPrefs {
    fn default() -> Self {
        Self {
            enabled: true,
            sharing: true,
            ai: true,
            shared_with_me: true,
        }
    }
}

impl NotificationPrefs {
    pub fn allows(&self, category: Category) -> bool {
        self.enabled
            && match category {
                Category::Sharing => self.sharing,
                Category::Ai => self.ai,
                Category::SharedWithMe => self.shared_with_me,
            }
    }
}

/// What a notice is about (each one has its switch in Settings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Sharing,
    Ai,
    SharedWithMe,
}

/// Where a click on the notification leads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A terminal of a tab (a request on a shared terminal, your keyboard).
    Terminal(EntityId),
    /// A server session (yours or shared with you): its tab, opened if
    /// needed.
    Session { session_id: Id, title: String },
    /// An AI task, in the AI section.
    AiTask(Id),
}

/// A notice to post as a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub category: Category,
    /// Identity of the notice: the same key is the same notice (repeats are
    /// not posted and a newer one replaces the old one in the system).
    pub key: String,
    pub title: String,
    pub body: String,
    pub target: Target,
}

/// Post a notification? Only with the window in the background (or
/// minimized) and with its kind turned on.
pub fn should_notify(prefs: &NotificationPrefs, category: Category, window_active: bool) -> bool {
    !window_active && prefs.allows(category)
}

/// What was posted: repeats and where each one leads.
#[derive(Default)]
pub struct Notifier {
    recent: HashMap<String, Instant>,
    targets: VecDeque<(String, Target)>,
}

impl Notifier {
    /// Decides whether to post a notice and, if so, remembers it and
    /// returns the notification to post.
    pub fn admit(
        &mut self,
        prefs: &NotificationPrefs,
        notice: Notice,
        window_active: bool,
        now: Instant,
    ) -> Option<SystemNotification> {
        if !should_notify(prefs, notice.category, window_active) {
            return None;
        }
        self.recent
            .retain(|_, at| now.saturating_duration_since(*at) < FORGET_AFTER);
        if self
            .recent
            .get(&notice.key)
            .is_some_and(|at| now.saturating_duration_since(*at) < REPEAT_WINDOW)
        {
            return None;
        }
        self.recent.insert(notice.key.clone(), now);
        self.targets.retain(|(k, _)| *k != notice.key);
        if self.targets.len() >= MAX_TARGETS {
            self.targets.pop_front();
        }
        self.targets.push_back((notice.key.clone(), notice.target));
        Some(SystemNotification {
            tag: notice.key.into(),
            title: clip(&notice.title, MAX_BODY).into(),
            body: clip(&notice.body, MAX_BODY).into(),
            actions: Vec::new(),
        })
    }

    /// Where the clicked notification leads (`None`: just the window).
    pub fn clicked(&mut self, tag: &str) -> Option<Target> {
        let ix = self.targets.iter().position(|(k, _)| k == tag)?;
        self.targets.remove(ix).map(|(_, t)| t)
    }

    /// The notice was answered or is no longer valid: returns its tag if it
    /// was posted (to remove it from the system).
    pub fn forget(&mut self, key: &str) -> Option<SharedString> {
        let ix = self.targets.iter().position(|(k, _)| k == key)?;
        self.targets.remove(ix);
        Some(SharedString::from(key.to_string()))
    }
}

/// Key of a request on a shared session: the same whether it comes from
/// the open terminal or from the server's events.
pub fn request_key(join: bool, participant: Id) -> String {
    if join {
        format!("join:{participant}")
    } else {
        format!("control:{participant}")
    }
}

/// Kind and key of a notice of the server about a shared session.
pub fn session_notice_key(notice: &SessionNotice) -> (Category, String) {
    match notice {
        SessionNotice::JoinRequest {
            session_id,
            participant,
            name,
            ..
        } => (
            Category::Sharing,
            participant
                .map(|p| request_key(true, p))
                .unwrap_or_else(|| format!("join:{session_id}:{name}")),
        ),
        SessionNotice::ControlRequest {
            session_id,
            participant,
            name,
            ..
        } => (
            Category::Sharing,
            participant
                .map(|p| request_key(false, p))
                .unwrap_or_else(|| format!("control:{session_id}:{name}")),
        ),
        SessionNotice::PromptPending { session_id, host } => {
            (Category::Sharing, format!("prompt:{session_id}:{host}"))
        }
        SessionNotice::ControlGranted { session_id } => (
            Category::SharedWithMe,
            format!("control-granted:{session_id}"),
        ),
        SessionNotice::ControlRevoked { session_id } => (
            Category::SharedWithMe,
            format!("control-revoked:{session_id}"),
        ),
        SessionNotice::SessionShared { session_id, .. } => {
            (Category::SharedWithMe, format!("shared:{session_id}"))
        }
    }
}

/// An event of an AI task that deserves a notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiNotice {
    Approval {
        task_id: Id,
        approval_id: Id,
        summary: String,
    },
    Finished {
        task_id: Id,
    },
    Failed {
        task_id: Id,
        error: String,
    },
}

impl AiNotice {
    /// From an `{"type":"ai","task_id":…,"event":{…}}` event (of the
    /// server or of the engine on this computer).
    pub fn from_event(v: &Value) -> Option<Self> {
        use termoak_ai::{TaskEvent, TaskStatus};
        if v["type"] != "ai" {
            return None;
        }
        let task_id: Id = v["task_id"].as_str()?.parse().ok()?;
        let event: TaskEvent = serde_json::from_value(v["event"].clone()).ok()?;
        Some(match event {
            TaskEvent::ApprovalRequested {
                approval_id,
                summary,
                ..
            } => AiNotice::Approval {
                task_id,
                approval_id,
                summary,
            },
            TaskEvent::Finished {
                status: TaskStatus::Completed,
                ..
            } => AiNotice::Finished { task_id },
            TaskEvent::Finished {
                status: TaskStatus::Failed,
                error,
                ..
            } => AiNotice::Failed {
                task_id,
                error: error.unwrap_or_default(),
            },
            _ => return None,
        })
    }

    pub fn task_id(&self) -> Id {
        match self {
            AiNotice::Approval { task_id, .. }
            | AiNotice::Finished { task_id }
            | AiNotice::Failed { task_id, .. } => *task_id,
        }
    }

    pub fn key(&self) -> String {
        match self {
            AiNotice::Approval { approval_id, .. } => format!("ai-approval:{approval_id}"),
            AiNotice::Finished { task_id } | AiNotice::Failed { task_id, .. } => {
                format!("ai-finished:{task_id}")
            }
        }
    }

    /// Toast of the window (a task on this computer or on the server).
    pub fn toast(&self, local: bool) -> (ToastKind, String) {
        match (self, local) {
            (AiNotice::Approval { summary, .. }, true) => (
                ToastKind::Warning,
                t!("ai.local.approval_needed", action = summary).to_string(),
            ),
            (AiNotice::Approval { summary, .. }, false) => (
                ToastKind::Warning,
                t!("ai.server.approval_needed", action = summary).to_string(),
            ),
            (AiNotice::Finished { .. }, true) => {
                (ToastKind::Success, t!("ai.local.finished").to_string())
            }
            (AiNotice::Finished { .. }, false) => {
                (ToastKind::Success, t!("ai.server.finished").to_string())
            }
            (AiNotice::Failed { error, .. }, true) => (
                ToastKind::Error,
                t!("ai.local.failed", error = error).to_string(),
            ),
            (AiNotice::Failed { error, .. }, false) => (
                ToastKind::Error,
                t!("ai.server.failed", error = error).to_string(),
            ),
        }
    }

    /// Title of the notification.
    pub fn title(&self) -> String {
        match self {
            AiNotice::Approval { .. } => t!("notifications.ai_approval_title"),
            AiNotice::Finished { .. } => t!("notifications.ai_finished_title"),
            AiNotice::Failed { .. } => t!("notifications.ai_failed_title"),
        }
        .to_string()
    }

    pub fn notice(&self, local: bool) -> Notice {
        Notice {
            category: Category::Ai,
            key: self.key(),
            title: self.title(),
            body: self.toast(local).1,
            target: Target::AiTask(self.task_id()),
        }
    }
}

/// Up to `max` characters (with an ellipsis when cut).
fn clip(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// macOS: asks for permission to notify (only the first time does the
/// system ask; later it answers with what the user chose). GPUI would ask
/// when posting the first notification, which then is not shown; asking
/// before keeps the first one.
#[cfg(target_os = "macos")]
pub fn request_authorization() {
    use block2::RcBlock;
    use objc2::runtime::Bool;
    use objc2_foundation::{NSBundle, NSError};
    use objc2_user_notifications::{UNAuthorizationOptions, UNUserNotificationCenter};

    // Outside an app bundle (`cargo run`) the notification center aborts
    // the process.
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return;
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let completion = RcBlock::new(|granted: Bool, error: *mut NSError| {
        // SAFETY: when non-null, `error` is a valid `NSError` for the
        // duration of the callback.
        if let Some(error) = unsafe { error.as_ref() } {
            tracing::warn!(
                error = %error.localizedDescription(),
                "permission to notify could not be asked"
            );
        } else if !granted.as_bool() {
            tracing::info!("notifications are not allowed in System Settings");
        }
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &completion,
    );
}

/// Elsewhere there is nothing to ask.
#[cfg(not(target_os = "macos"))]
pub fn request_authorization() {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn notice(key: &str, category: Category) -> Notice {
        Notice {
            category,
            key: key.into(),
            title: "Shared session".into(),
            body: "Ana wants to join “prod”".into(),
            target: Target::AiTask(termoak_core::new_id()),
        }
    }

    #[test]
    fn only_in_the_background_and_when_turned_on() {
        let all = NotificationPrefs::default();
        assert!(should_notify(&all, Category::Sharing, false));
        assert!(!should_notify(&all, Category::Sharing, true));

        let off = NotificationPrefs {
            enabled: false,
            ..all
        };
        for c in [Category::Sharing, Category::Ai, Category::SharedWithMe] {
            assert!(!should_notify(&off, c, false));
        }

        let no_ai = NotificationPrefs { ai: false, ..all };
        assert!(!should_notify(&no_ai, Category::Ai, false));
        assert!(should_notify(&no_ai, Category::Sharing, false));
        assert!(should_notify(&no_ai, Category::SharedWithMe, false));

        let no_shared = NotificationPrefs {
            shared_with_me: false,
            ..all
        };
        assert!(!should_notify(&no_shared, Category::SharedWithMe, false));
        let no_sharing = NotificationPrefs {
            sharing: false,
            ..all
        };
        assert!(!should_notify(&no_sharing, Category::Sharing, false));
        assert!(should_notify(&no_sharing, Category::Ai, false));
    }

    #[test]
    fn prefs_default_on_and_old_settings_load() {
        assert!(NotificationPrefs::default().enabled);
        let p: NotificationPrefs = serde_json::from_value(json!({"ai": false})).unwrap();
        assert!(p.enabled && p.sharing && p.shared_with_me && !p.ai);
    }

    #[test]
    fn repeats_are_not_posted() {
        let prefs = NotificationPrefs::default();
        let mut n = Notifier::default();
        let t0 = Instant::now();
        let first = n.admit(&prefs, notice("join:1", Category::Sharing), false, t0);
        assert_eq!(first.unwrap().tag, "join:1");
        // The same request from the other source, right after.
        let again = n.admit(
            &prefs,
            notice("join:1", Category::Sharing),
            false,
            t0 + Duration::from_secs(3),
        );
        assert!(again.is_none());
        // Another one is posted.
        assert!(
            n.admit(&prefs, notice("join:2", Category::Sharing), false, t0)
                .is_some()
        );
        // Later, the same key again is a new notice.
        assert!(
            n.admit(
                &prefs,
                notice("join:1", Category::Sharing),
                false,
                t0 + REPEAT_WINDOW + Duration::from_secs(1),
            )
            .is_some()
        );
    }

    #[test]
    fn suppressed_notices_do_not_count_as_posted() {
        let prefs = NotificationPrefs::default();
        let mut n = Notifier::default();
        let t0 = Instant::now();
        // With the window in front only the toast shows...
        assert!(
            n.admit(&prefs, notice("k", Category::Ai), true, t0)
                .is_none()
        );
        // ...so the same notice in the background is posted.
        assert!(
            n.admit(&prefs, notice("k", Category::Ai), false, t0)
                .is_some()
        );
        assert!(n.clicked("k").is_some());
    }

    #[test]
    fn clicks_lead_to_the_target_once() {
        let prefs = NotificationPrefs::default();
        let mut n = Notifier::default();
        let task = termoak_core::new_id();
        let mut a = notice("ai-finished:x", Category::Ai);
        a.target = Target::AiTask(task);
        n.admit(&prefs, a, false, Instant::now()).unwrap();
        assert_eq!(n.clicked("ai-finished:x"), Some(Target::AiTask(task)));
        assert_eq!(n.clicked("ai-finished:x"), None);
        assert_eq!(n.clicked("unknown"), None);
    }

    #[test]
    fn answered_requests_are_forgotten() {
        let prefs = NotificationPrefs::default();
        let mut n = Notifier::default();
        let p = termoak_core::new_id();
        let key = request_key(true, p);
        n.admit(
            &prefs,
            notice(&key, Category::Sharing),
            false,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(n.forget(&key).as_deref(), Some(key.as_str()));
        assert_eq!(n.forget(&key), None);
        assert_eq!(n.clicked(&key), None);
    }

    #[test]
    fn targets_are_bounded() {
        let prefs = NotificationPrefs::default();
        let mut n = Notifier::default();
        let t0 = Instant::now();
        for i in 0..(MAX_TARGETS + 10) {
            n.admit(
                &prefs,
                notice(&format!("k{i}"), Category::Sharing),
                false,
                t0,
            )
            .unwrap();
        }
        assert_eq!(n.targets.len(), MAX_TARGETS);
        assert!(n.clicked("k0").is_none());
        assert!(n.clicked(&format!("k{}", MAX_TARGETS + 9)).is_some());
    }

    #[test]
    fn requests_have_the_same_key_from_both_sources() {
        let sid = termoak_core::new_id();
        let p = termoak_core::new_id();
        let from_server = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "join_request", "session_id": sid, "title": "prod",
            "participant": {"id": p, "name": "Ana"}}}))
        .unwrap();
        assert_eq!(
            session_notice_key(&from_server),
            (Category::Sharing, request_key(true, p))
        );
        let control = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "control_request", "session_id": sid, "title": "prod",
            "participant": {"id": p, "name": "Ana"}}}))
        .unwrap();
        assert_eq!(
            session_notice_key(&control),
            (Category::Sharing, request_key(false, p))
        );
        let granted = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "control_granted", "session_id": sid}}))
        .unwrap();
        assert_eq!(session_notice_key(&granted).0, Category::SharedWithMe);
        let shared = SessionNotice::from_event(&json!({"type": "session", "notice": {
            "type": "session_shared", "by": "Ana",
            "session": {"id": sid, "title": "prod"}}}))
        .unwrap();
        assert_eq!(
            session_notice_key(&shared),
            (Category::SharedWithMe, format!("shared:{sid}"))
        );
    }

    #[test]
    fn ai_events() {
        let task = termoak_core::new_id();
        let approval = termoak_core::new_id();
        let a = AiNotice::from_event(&json!({"type": "ai", "task_id": task, "seq": 3,
            "event": {"type": "approval_requested", "approval_id": approval, "call_id": "c",
                      "tool": "run", "summary": "rm -rf /tmp/x", "input": {}}}))
        .unwrap();
        assert_eq!(
            a,
            AiNotice::Approval {
                task_id: task,
                approval_id: approval,
                summary: "rm -rf /tmp/x".into()
            }
        );
        let n = a.notice(false);
        assert_eq!(n.category, Category::Ai);
        assert_eq!(n.target, Target::AiTask(task));
        assert!(n.body.contains("rm -rf /tmp/x"));

        let done = AiNotice::from_event(&json!({"type": "ai", "task_id": task,
            "event": {"type": "finished", "status": "completed", "result": "ok", "error": null}}))
        .unwrap();
        assert_eq!(done, AiNotice::Finished { task_id: task });
        let failed = AiNotice::from_event(&json!({"type": "ai", "task_id": task,
            "event": {"type": "finished", "status": "failed", "result": null, "error": "boom"}}))
        .unwrap();
        assert_eq!(done.key(), failed.key());
        assert!(failed.toast(true).1.contains("boom"));
        assert_eq!(failed.toast(true).0, ToastKind::Error);

        // Cancelled tasks and the rest of the events are not notices.
        assert!(
            AiNotice::from_event(&json!({"type": "ai", "task_id": task,
            "event": {"type": "finished", "status": "cancelled", "result": null, "error": null}}))
            .is_none()
        );
        assert!(
            AiNotice::from_event(&json!({"type": "ai", "task_id": task,
            "event": {"type": "text", "delta": "hi"}}))
            .is_none()
        );
        assert!(AiNotice::from_event(&json!({"type": "session"})).is_none());
    }

    #[test]
    fn long_texts_are_clipped() {
        assert_eq!(clip("  short ", 10), "short");
        let long = "é".repeat(300);
        let c = clip(&long, MAX_BODY);
        assert_eq!(c.chars().count(), MAX_BODY);
        assert!(c.ends_with('…'));
    }
}
