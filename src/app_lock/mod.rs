//! App lock: Settings → General → "Lock Termoak" asks the system to verify
//! the user (Touch ID on macOS, Windows Hello on Windows) to open the app,
//! after some minutes idle or in the background, and optionally before
//! copying a saved password or private key.
//!
//! - macOS: LocalAuthentication with `LAPolicyDeviceOwnerAuthentication`:
//!   Touch ID (or an Apple Watch) and, when there is no biometry or it
//!   fails, the Mac's password in the same system prompt (`macos.rs`).
//! - Windows: `UserConsentVerifier` (Windows Hello: face, fingerprint or the
//!   PIN), shown over the window (`hello.rs`).
//! - Linux has no such system API (polkit would need an agent and an
//!   action installed system-wide): the option is not offered there.
//!
//! Nobody is locked out: turning the lock on verifies once first (so it
//! is known to work); cancelling leaves the lock screen with "Try again";
//! if the system says it can no longer verify anyone (Windows Hello was
//! removed, no password set), the app opens with a warning. Quitting
//! (⌘Q, the window's close button) always works.
//!
//! The lock hides the window's content (tabs, sidebar, dialogs); terminals
//! keep running behind it.

#[cfg(target_os = "windows")]
mod hello;
#[cfg(target_os = "macos")]
mod macos;

use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, Global, IntoElement, ParentElement,
    SharedString, Styled, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use serde::{Deserialize, Serialize};

use crate::state::{AppModel, ToastKind};
use crate::ui::{self, IconName};

/// Preferences of the lock (this device).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LockSettings {
    /// Lock Termoak (off by default).
    pub enabled: bool,
    /// Minutes idle or in the background before it locks (0: as soon as
    /// it goes to the background).
    pub after_minutes: u32,
    /// Also ask before copying a saved password or private key.
    pub protect_secrets: bool,
}

impl Default for LockSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            after_minutes: 5,
            protect_secrets: true,
        }
    }
}

/// Choices of "Lock after" (minutes; 0 = when in the background).
pub const AFTER_CHOICES: [u32; 6] = [0, 1, 5, 15, 30, 60];
/// After verifying, secrets can be copied without asking again for this long.
pub const SECRET_GRACE: Duration = Duration::from_secs(60);
/// How often the idle time is looked at.
const TICK: Duration = Duration::from_secs(5);

/// The lock's state over time (pure, for the tests).
#[derive(Debug, Clone)]
pub struct Clock {
    locked: bool,
    last_activity: Instant,
    /// Since when no window of the app has the focus.
    background_since: Option<Instant>,
    /// Last successful verification.
    verified_at: Option<Instant>,
    /// The system prompt was shown by itself for this lock (not again
    /// until it locks again: a cancelled prompt does not come back in a
    /// loop each time the window gets the focus).
    auto_prompted: bool,
}

impl Clock {
    pub fn new(locked: bool, now: Instant) -> Self {
        Self {
            locked,
            last_activity: now,
            background_since: None,
            verified_at: None,
            auto_prompted: false,
        }
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// The user typed, clicked or moved the mouse in a window.
    pub fn activity(&mut self, now: Instant) {
        if !self.locked {
            self.last_activity = now;
        }
    }

    /// Some window of the app has the focus (or none).
    pub fn set_active(&mut self, active: bool, now: Instant) {
        match (active, self.background_since) {
            (true, _) => {
                self.background_since = None;
                self.activity(now);
            }
            (false, None) => self.background_since = Some(now),
            (false, Some(_)) => {}
        }
    }

    /// It is time to lock.
    pub fn should_lock(&self, s: &LockSettings, now: Instant) -> bool {
        if !s.enabled || self.locked {
            return false;
        }
        if s.after_minutes == 0 {
            return self.background_since.is_some();
        }
        let limit = Duration::from_secs(u64::from(s.after_minutes) * 60);
        now.saturating_duration_since(self.last_activity) >= limit
            || self
                .background_since
                .is_some_and(|b| now.saturating_duration_since(b) >= limit)
    }

    pub fn lock(&mut self) {
        self.locked = true;
        self.auto_prompted = false;
        self.verified_at = None;
    }

    /// Verified: open, and the idle time starts again.
    pub fn unlocked(&mut self, now: Instant) {
        self.locked = false;
        self.last_activity = now;
        self.verified_at = Some(now);
    }

    /// Whether the prompt may open by itself now (once per lock).
    pub fn take_auto_prompt(&mut self) -> bool {
        if self.locked && !self.auto_prompted {
            self.auto_prompted = true;
            true
        } else {
            false
        }
    }

    /// Copying a secret needs a verification first.
    pub fn secret_needs_check(&self, s: &LockSettings, now: Instant) -> bool {
        s.enabled
            && s.protect_secrets
            && self
                .verified_at
                .is_none_or(|v| now.saturating_duration_since(v) > SECRET_GRACE)
    }
}

/// Result of asking the system.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
pub enum Outcome {
    Verified,
    /// The user cancelled the prompt.
    Canceled,
    /// The system cannot verify anyone here (no Touch ID nor password,
    /// Windows Hello not set up or turned off by policy).
    Unavailable(String),
    Failed(String),
}

/// The platform has a way to verify the user.
pub const fn supported() -> bool {
    cfg!(any(target_os = "macos", target_os = "windows"))
}

/// Name of the system's verification ("Touch ID", "Windows Hello").
pub fn method_name() -> SharedString {
    if cfg!(target_os = "macos") {
        "Touch ID".into()
    } else {
        "Windows Hello".into()
    }
}

/// Asks the system to verify the user (`reason` is shown in the prompt).
fn verify(reason: &str, window: &Window) -> futures::channel::oneshot::Receiver<Outcome> {
    #[cfg(target_os = "macos")]
    {
        let _ = window;
        macos::verify(reason)
    }
    #[cfg(target_os = "windows")]
    {
        hello::verify(reason, hello::hwnd(window))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (reason, window);
        let (tx, rx) = futures::channel::oneshot::channel();
        let _ = tx.send(Outcome::Unavailable(String::new()));
        rx
    }
}

/// Asks the system and gives the outcome on the interface thread.
pub fn verify_then(
    reason: SharedString,
    window: &mut Window,
    cx: &mut App,
    done: impl FnOnce(Outcome, &mut Window, &mut App) + 'static,
) {
    let rx = verify(&reason, window);
    window
        .spawn(cx, async move |cx| {
            let outcome = rx.await.unwrap_or_else(|_| Outcome::Failed(String::new()));
            let _ = cx.update(|window, cx| done(outcome, window, cx));
        })
        .detach();
}

/// Text for an outcome that did not verify.
fn outcome_text(outcome: &Outcome) -> SharedString {
    match outcome {
        Outcome::Verified => SharedString::default(),
        Outcome::Canceled => t!("lock.canceled"),
        Outcome::Unavailable(detail) | Outcome::Failed(detail) if !detail.is_empty() => {
            t!("lock.failed_detail", detail = detail)
        }
        Outcome::Unavailable(_) => t!("lock.unavailable", method = method_name()),
        Outcome::Failed(_) => t!("lock.failed"),
    }
}

/// The lock of the app, shared by every window.
pub struct AppLock {
    model: Entity<AppModel>,
    clock: Clock,
    /// The system prompt is open.
    busy: bool,
    /// Why the last try did not open.
    message: Option<SharedString>,
    _tick: Task<()>,
}

struct AppLockGlobal(Entity<AppLock>);

impl Global for AppLockGlobal {}

impl AppLock {
    /// The one of the app (created by the first window: locked at start
    /// when the lock is on).
    pub fn global(model: &Entity<AppModel>, cx: &mut App) -> Entity<Self> {
        if let Some(g) = cx.try_global::<AppLockGlobal>() {
            return g.0.clone();
        }
        let enabled = supported() && model.read(cx).settings.lock.enabled;
        let model = model.clone();
        let entity = cx.new(|cx: &mut Context<Self>| {
            let tick = cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(TICK).await;
                    if this.update(cx, |l, cx| l.check(cx)).is_err() {
                        break;
                    }
                }
            });
            AppLock {
                model,
                clock: Clock::new(enabled, Instant::now()),
                busy: false,
                message: None,
                _tick: tick,
            }
        });
        cx.set_global(AppLockGlobal(entity.clone()));
        entity
    }

    /// The lock of the app, if a window created it.
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<AppLockGlobal>().map(|g| g.0.clone())
    }

    fn settings(&self, cx: &App) -> LockSettings {
        let s = self.model.read(cx).settings.lock;
        LockSettings {
            enabled: s.enabled && supported(),
            ..s
        }
    }

    pub fn locked(&self) -> bool {
        self.clock.locked()
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    pub fn message(&self) -> Option<SharedString> {
        self.message.clone()
    }

    /// Input in a window (no repaint).
    pub fn activity(&mut self) {
        self.clock.activity(Instant::now());
    }

    /// Some window has the focus, or none. It locks at the next tick, not
    /// at once: moving between two Termoak windows also passes through
    /// "none".
    pub fn set_active(&mut self, active: bool) {
        self.clock.set_active(active, Instant::now());
    }

    fn check(&mut self, cx: &mut Context<Self>) {
        let s = self.settings(cx);
        if self.clock.should_lock(&s, Instant::now()) {
            self.lock_now(cx);
        }
    }

    /// Locks every window now.
    pub fn lock_now(&mut self, cx: &mut Context<Self>) {
        if !self.settings(cx).enabled || self.locked() {
            return;
        }
        self.clock.lock();
        self.message = None;
        cx.notify();
    }

    /// The lock was just turned on (after verifying): it counts as a
    /// verification, so it does not lock at once.
    pub fn turned_on(&mut self) {
        self.clock.unlocked(Instant::now());
    }

    /// The window got the focus while locked: the prompt opens by itself
    /// once per lock.
    pub fn auto_unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.locked() && !self.busy && self.clock.take_auto_prompt() {
            self.unlock(window, cx);
        }
    }

    /// "Unlock" / "Try again".
    pub fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.locked() || self.busy {
            return;
        }
        self.busy = true;
        self.message = None;
        cx.notify();
        let me = cx.entity().downgrade();
        verify_then(
            t!("lock.reason_open"),
            window,
            cx,
            move |outcome, window, cx| {
                let Some(me) = me.upgrade() else { return };
                me.update(cx, |l, cx| {
                    l.busy = false;
                    match &outcome {
                        Outcome::Verified => {
                            l.clock.unlocked(Instant::now());
                        }
                        // Never locked out: the system can no longer verify
                        // anyone here.
                        Outcome::Unavailable(_) => {
                            l.clock.unlocked(Instant::now());
                            ui::notify(
                                window,
                                cx,
                                ToastKind::Warning,
                                t!("lock.opened_unavailable", method = method_name()),
                            );
                        }
                        Outcome::Canceled | Outcome::Failed(_) => {
                            l.message = Some(outcome_text(&outcome));
                        }
                    }
                    cx.notify();
                });
            },
        );
    }
}

/// Runs `f` (which copies a password or private key) after verifying the
/// user if the lock asks for it; right away otherwise.
pub fn guard_secret(
    model: &Entity<AppModel>,
    window: &mut Window,
    cx: &mut App,
    f: impl FnOnce(&mut Window, &mut App) + 'static,
) {
    let settings = model.read(cx).settings.lock;
    let lock = AppLock::try_global(cx);
    let needs = supported()
        && lock.as_ref().is_none_or(|l| {
            l.read(cx)
                .clock
                .secret_needs_check(&settings, Instant::now())
        });
    if !needs || !settings.enabled || !settings.protect_secrets {
        f(window, cx);
        return;
    }
    verify_then(
        t!("lock.reason_secret"),
        window,
        cx,
        move |outcome, window, cx| match outcome {
            Outcome::Verified => {
                if let Some(l) = lock {
                    l.update(cx, |l, _| l.clock.verified_at = Some(Instant::now()));
                }
                f(window, cx);
            }
            // Never locked out of one's own data.
            Outcome::Unavailable(_) => f(window, cx),
            other => ui::notify(window, cx, ToastKind::Info, outcome_text(&other)),
        },
    );
}

/// Turns the lock on after one successful verification (so it is known to
/// work on this device), or off.
pub fn set_enabled(model: &Entity<AppModel>, on: bool, window: &mut Window, cx: &mut App) {
    let save = |model: &Entity<AppModel>, on: bool, cx: &mut App| {
        model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.lock.enabled = on;
            m.save_settings(s, cx);
        });
    };
    if !on {
        save(model, false, cx);
        return;
    }
    let model = model.clone();
    verify_then(
        t!("lock.reason_enable"),
        window,
        cx,
        move |outcome, window, cx| match outcome {
            Outcome::Verified => {
                let lock = AppLock::global(&model, cx);
                lock.update(cx, |l, _| l.turned_on());
                save(&model, true, cx);
                ui::success(window, cx, t!("lock.enabled", method = method_name()));
            }
            other => ui::error(window, cx, outcome_text(&other)),
        },
    );
}

/// The window's content while locked.
pub fn render_lock_screen(lock: &Entity<AppLock>, cx: &App) -> AnyElement {
    let l = lock.read(cx);
    let busy = l.busy();
    let message = l.message();
    let theme = cx.theme();
    let weak = lock.downgrade();
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_4()
        .p_8()
        .bg(theme.background)
        .child(
            div()
                .size(px(64.))
                .rounded_full()
                .bg(theme.secondary)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    ui::icon(IconName::LockKeyhole)
                        .size(px(30.))
                        .text_color(theme.primary),
                ),
        )
        .child(div().text_xl().font_semibold().child(t!("lock.title")))
        .child(
            div()
                .max_w(px(420.))
                .text_center()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("lock.detail", method = method_name())),
        )
        .when_some(message.clone(), |this, m| {
            this.child(div().text_sm().text_color(theme.warning).child(m))
        })
        .child(
            h_flex().gap_2().child(
                Button::new("app-unlock")
                    .primary()
                    .large()
                    .icon(ui::icon(IconName::LockKeyholeOpen))
                    .label(if message.is_some() {
                        t!("lock.try_again")
                    } else {
                        t!("lock.unlock", method = method_name())
                    })
                    .loading(busy)
                    .disabled(busy)
                    .on_click(move |_: &ClickEvent, window, cx| {
                        if let Some(l) = weak.upgrade() {
                            l.update(cx, |l, cx| l.unlock(window, cx));
                        }
                    }),
            ),
        )
        .child(div().text_xs().text_color(theme.muted_foreground).child(
            if cfg!(target_os = "macos") {
                t!("lock.quit_hint_macos")
            } else {
                t!("lock.quit_hint")
            },
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(after: u32) -> LockSettings {
        LockSettings {
            enabled: true,
            after_minutes: after,
            protect_secrets: true,
        }
    }

    fn mins(n: u64) -> Duration {
        Duration::from_secs(n * 60)
    }

    #[test]
    fn locks_after_idle_time() {
        let t0 = Instant::now();
        let mut c = Clock::new(false, t0);
        let s = on(5);
        assert!(!c.should_lock(&s, t0 + mins(4)));
        assert!(c.should_lock(&s, t0 + mins(5)));
        // Input pushes it back.
        c.activity(t0 + mins(4));
        assert!(!c.should_lock(&s, t0 + mins(8)));
        assert!(c.should_lock(&s, t0 + mins(9)));
        // Off: never.
        assert!(!c.should_lock(&LockSettings::default(), t0 + mins(60)));
    }

    #[test]
    fn locks_in_the_background() {
        let t0 = Instant::now();
        let mut c = Clock::new(false, t0);
        // "When in the background": as soon as no window has the focus.
        let now = on(0);
        assert!(!c.should_lock(&now, t0));
        c.set_active(false, t0);
        assert!(c.should_lock(&now, t0));
        // Back in front before the time is up.
        let s = on(1);
        c.set_active(true, t0 + Duration::from_secs(30));
        assert!(!c.should_lock(&now, t0 + Duration::from_secs(31)));
        c.set_active(false, t0 + Duration::from_secs(40));
        // A second focus loss does not restart the count.
        c.set_active(false, t0 + Duration::from_secs(80));
        assert!(c.should_lock(&s, t0 + Duration::from_secs(100)));
    }

    #[test]
    fn unlocking_and_the_prompt_once_per_lock() {
        let t0 = Instant::now();
        let mut c = Clock::new(true, t0);
        let s = on(5);
        // Locked: nothing to lock and input does not count.
        assert!(!c.should_lock(&s, t0 + mins(60)));
        c.activity(t0 + mins(1));
        assert!(c.take_auto_prompt());
        // Cancelled: the window getting the focus does not ask again.
        assert!(!c.take_auto_prompt());
        c.unlocked(t0 + mins(2));
        assert!(!c.locked());
        assert!(!c.take_auto_prompt());
        assert!(!c.should_lock(&s, t0 + mins(6)));
        assert!(c.should_lock(&s, t0 + mins(7)));
        c.lock();
        assert!(c.locked());
        assert!(c.take_auto_prompt());
    }

    #[test]
    fn secrets_ask_unless_just_verified() {
        let t0 = Instant::now();
        let mut c = Clock::new(false, t0);
        let s = on(5);
        assert!(c.secret_needs_check(&s, t0));
        c.unlocked(t0);
        assert!(!c.secret_needs_check(&s, t0 + Duration::from_secs(30)));
        assert!(c.secret_needs_check(&s, t0 + SECRET_GRACE + Duration::from_secs(1)));
        let no = LockSettings {
            protect_secrets: false,
            ..s
        };
        assert!(!c.secret_needs_check(&no, t0 + mins(10)));
        assert!(!c.secret_needs_check(&LockSettings::default(), t0 + mins(10)));
    }

    #[test]
    fn settings_default_off_and_read_old_files() {
        let s: LockSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, LockSettings::default());
        assert!(!s.enabled);
        assert!(AFTER_CHOICES.contains(&s.after_minutes));
    }
}
