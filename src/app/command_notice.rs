//! Long commands that ended in a terminal out of sight: a toast with a
//! button to go there when the window is in front (another tab is
//! showing), a notification of the system when it is in the background
//! (a click on it brings the tab back). Nothing for the tab in view.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use gpui::{Context, EntityId, SharedString, Window};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::notification::Notification;
use gpui_component::{Sizable, WindowExt};

use super::AppView;
use crate::notifications::CommandDone;
use crate::windows;

/// Toasts of commands that ended (one per command).
struct CommandToast;

/// Makes each notice a new one (two commands may end alike).
static SEQ: AtomicU64 = AtomicU64::new(1);

impl AppView {
    pub(super) fn on_command_finished(
        &mut self,
        id: EntityId,
        done: &CommandDone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Looking at it (its tab in view, the window in front).
        if self.terminal_in_view(id, window) {
            return;
        }
        let notice = done.notice(id, SEQ.fetch_add(1, Ordering::Relaxed));
        if window.is_window_active() {
            let app = cx.entity().downgrade();
            let note = if done.failed() {
                Notification::error(notice.body.clone())
            } else {
                Notification::success(notice.body.clone())
            };
            let note = note
                .id1::<CommandToast>(SharedString::from(notice.key.clone()))
                .title(notice.title.clone())
                .action(move |_, _, _| {
                    let app = app.clone();
                    Button::new("command-show")
                        .small()
                        .primary()
                        .label(t!("notifications.command_show"))
                        .on_click(move |_, window, cx| {
                            if let Some(a) = app.upgrade() {
                                a.update(cx, |a, cx| a.show_terminal(id, window, cx));
                            }
                        })
                });
            window.push_notification(note, cx);
            return;
        }
        let prefs = self.model.read(cx).settings.notifications;
        let posted = cx.try_global::<windows::AppWindows>().is_some().then(|| {
            cx.global_mut::<windows::AppWindows>().notifier.admit(
                &prefs,
                notice,
                false,
                Instant::now(),
            )
        });
        if let Some(n) = posted.flatten() {
            cx.show_system_notification(n);
        }
    }
}
