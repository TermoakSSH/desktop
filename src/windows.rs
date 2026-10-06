//! Several windows of the app (File → New window). Every window is an
//! [`AppView`] with its own tabs, sidebar and dialogs, on the same
//! [`AppModel`] (vault, account, sync, tunnels) and [`UpdateModel`].
//!
//! What exists once for the whole app lives here and goes to one window:
//! - authentication questions of the SSH engine (host keys, passwords),
//!   links opened from outside and the notices of the model (toasts, the
//!   server's events, updates) go to the *notice window*: the one used
//!   last, or the first one;
//! - the notifications of the system share one [`Notifier`] (repeats, where
//!   a click leads);
//! - the local AI (engine and terminal tools) is started by the first
//!   window and sees the terminals of every window;
//! - dormant tabs of your running server sessions are added to the first
//!   window only.
//!
//! The model lives as long as the app, not as a window: on macOS the app
//! stays in the Dock with no window and opens one again on the same data
//! (click on the Dock icon, its menu, a link, a notification).
//!
//! Closing the last window: on Windows and Linux the app quits (GPUI's
//! default). On macOS the red button of the last window hides the app
//! instead of closing it, so its tabs and connections are still there when
//! it comes back from the Dock; ⌘Q quits.

use gpui::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, EntityId, Focusable, Global, Point,
    WeakEntity, Window, WindowBounds, WindowId, px,
};
use gpui_component::Root;
use termoak_ai::SessionSummary;

use crate::app::AppView;
use crate::local_ai::terminals::{TermRequest, screen_tail};
use crate::notifications::Notifier;
use crate::prompts::{self, PromptRequest};
use crate::state::AppModel;
use crate::terminal::{TermKind, TerminalView};
use crate::update::UpdateModel;

/// How far a new window is moved from the one in front.
const CASCADE: f32 = 28.;

/// The windows of the app.
pub struct AppWindows {
    /// Held here so they outlive the windows (macOS keeps the app running
    /// with none).
    model: Entity<AppModel>,
    updates: Entity<UpdateModel>,
    views: Vec<(AnyWindowHandle, WeakEntity<AppView>)>,
    /// What exists once for the whole app was set up (by the first window
    /// ever opened).
    services: bool,
    /// The window used last (it gets the notices).
    last_active: Option<WindowId>,
    /// One of the windows has the focus now.
    any_active: bool,
    /// Notifications of the system (shared, to see repeats coming from
    /// different windows).
    pub notifier: Notifier,
}

impl Global for AppWindows {}

/// What a window that just opened has to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registered {
    /// No other window is open: it gets the dormant tabs of your server
    /// sessions and the notices.
    pub first: bool,
    /// The first window since the app started: it sets up what exists once
    /// (see `AppView::start_app_services`).
    pub start_services: bool,
}

/// Adds a window.
pub fn register(
    model: &Entity<AppModel>,
    updates: &Entity<UpdateModel>,
    view: WeakEntity<AppView>,
    window: &Window,
    cx: &mut App,
) -> Registered {
    let handle = window.window_handle();
    if !cx.has_global::<AppWindows>() {
        cx.set_global(AppWindows {
            model: model.clone(),
            updates: updates.clone(),
            views: Vec::new(),
            services: false,
            last_active: None,
            any_active: false,
            notifier: Notifier::default(),
        });
    }
    let first = views(cx).is_empty();
    let w = cx.global_mut::<AppWindows>();
    w.views.retain(|(_, v)| v.upgrade().is_some());
    w.views.push((handle, view));
    if first {
        w.last_active = Some(handle.window_id());
    }
    let start_services = !std::mem::replace(&mut w.services, true);
    Registered {
        first,
        start_services,
    }
}

/// The window gained or lost the focus.
pub fn set_active(window: &Window, active: bool, cx: &mut App) {
    let Some(w) = cx.try_global::<AppWindows>() else {
        return;
    };
    let id = window.window_handle().window_id();
    let known = w.views.iter().any(|(h, _)| h.window_id() == id);
    let w = cx.global_mut::<AppWindows>();
    if active {
        w.any_active = true;
        if known {
            w.last_active = Some(id);
        }
    } else if w.last_active == Some(id) {
        w.any_active = false;
    }
}

/// One of the app's windows has the focus.
pub fn any_active(cx: &App) -> bool {
    // A window that closed in front may not say it lost the focus.
    cx.try_global::<AppWindows>().is_some_and(|w| {
        w.any_active
            && views(cx)
                .iter()
                .any(|(h, _)| Some(h.window_id()) == w.last_active)
    })
}

/// The open windows, in the order they were opened.
pub fn views(cx: &App) -> Vec<(AnyWindowHandle, Entity<AppView>)> {
    let Some(w) = cx.try_global::<AppWindows>() else {
        return Vec::new();
    };
    let open: Vec<WindowId> = cx.windows().iter().map(|h| h.window_id()).collect();
    w.views
        .iter()
        .filter(|(h, _)| open.contains(&h.window_id()))
        .filter_map(|(h, v)| Some((*h, v.upgrade()?)))
        .collect()
}

/// The window that gets the notices: the one used last, or the first one.
pub fn notice_window(cx: &App) -> Option<(AnyWindowHandle, Entity<AppView>)> {
    let views = views(cx);
    let last = cx.try_global::<AppWindows>().and_then(|w| w.last_active);
    let ix = views
        .iter()
        .position(|(h, _)| Some(h.window_id()) == last)
        .unwrap_or(0);
    views.into_iter().nth(ix)
}

/// `window` is the one that gets the notices.
pub fn is_notice_window(window: &Window, cx: &App) -> bool {
    notice_window(cx).is_none_or(|(h, _)| h.window_id() == window.window_handle().window_id())
}

/// `window` is the first of the open windows (it keeps the dormant tabs of
/// your server sessions).
pub fn is_first_window(window: &Window, cx: &App) -> bool {
    views(cx)
        .first()
        .is_none_or(|(h, _)| h.window_id() == window.window_handle().window_id())
}

/// Runs `f` on the notice window. `false` if there is no window.
pub fn with_notice_window(
    cx: &mut App,
    f: impl FnOnce(&mut AppView, &mut Window, &mut gpui::Context<AppView>),
) -> bool {
    let Some((handle, view)) = notice_window(cx) else {
        return false;
    };
    handle
        .update(cx, |_, window, cx| {
            view.update(cx, |v, cx| f(v, window, cx))
        })
        .is_ok()
}

/// File → New window: another window on the same model, with no tabs.
pub fn open_new(cx: &mut App) {
    unhide_app();
    open_window(cx);
}

/// Opens a window on the model (the first one again if every window was
/// closed). `false` if it could not (the vault never opened).
fn open_window(cx: &mut App) -> bool {
    let Some(w) = cx.try_global::<AppWindows>() else {
        return false;
    };
    let (model, updates) = (w.model.clone(), w.updates.clone());
    // A little below and to the right of the window in front.
    let front =
        notice_window(cx).and_then(|(h, _)| h.update(cx, |_, window, _| window.bounds()).ok());
    let mut options = crate::window_options(cx);
    if let Some(b) = front {
        let origin = Point::new(b.origin.x + px(CASCADE), b.origin.y + px(CASCADE));
        options.window_bounds = Some(WindowBounds::Windowed(Bounds::new(origin, b.size)));
    }
    let opened = cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| AppView::new(model, updates, None, window, cx));
        window.focus(&view.focus_handle(cx), cx);
        cx.new(|cx| Root::new(view, window, cx))
    });
    match opened {
        Ok(_) => {
            cx.activate(true);
            true
        }
        Err(e) => {
            tracing::error!(error = %e, "could not open a new window");
            false
        }
    }
}

/// Brings the app to the front: the notice window, or a new one if every
/// window was closed (macOS keeps the app in the Dock). `false` if there
/// is no window to show.
pub fn bring_to_front(cx: &mut App) -> bool {
    unhide_app();
    if let Some((handle, _)) = notice_window(cx) {
        cx.activate(true);
        return handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok();
    }
    if open_window(cx) {
        return true;
    }
    // The vault never opened: the error window, if it is still there.
    let Some(handle) = cx.windows().into_iter().next() else {
        return false;
    };
    cx.activate(true);
    handle
        .update(cx, |_, window, _| window.activate_window())
        .is_ok()
}

/// Runs `f` on the notice window after bringing the app to the front (and
/// opening a window if there was none). Deferred: it may come while a
/// window is being updated (an action, a menu).
pub fn with_front_window(
    cx: &mut App,
    f: impl FnOnce(&mut AppView, &mut Window, &mut gpui::Context<AppView>) + 'static,
) {
    cx.defer(move |cx| {
        if bring_to_front(cx) {
            with_notice_window(cx, f);
        }
    });
}

/// macOS: the app was opened again (Dock icon, Finder) with no window on
/// screen: the window comes back, or a new one opens on the same data.
pub fn reopen(cx: &mut App) {
    cx.defer(|cx| {
        bring_to_front(cx);
    });
}

/// macOS: the red button of a window. The last one hides the app instead
/// of closing (its tabs and connections stay for when it comes back from
/// the Dock); with other windows open it closes as usual.
#[cfg(target_os = "macos")]
pub fn should_close(window: &mut Window, cx: &mut App) -> bool {
    let id = window.window_handle().window_id();
    if views(cx).iter().any(|(h, _)| h.window_id() != id) {
        return true;
    }
    cx.hide();
    false
}

/// macOS: shows the app again if it was hidden (⌘H, or the red button of
/// the last window).
#[cfg(target_os = "macos")]
pub fn unhide_app() {
    use objc2::runtime::{AnyClass, AnyObject};
    use objc2::{class, msg_send};
    // SAFETY: plain AppKit calls on the main thread, where GPUI runs:
    // `+[NSApplication sharedApplication]` and `-[NSApplication unhide:]`
    // (its sender may be nil).
    unsafe {
        let class: &AnyClass = class!(NSApplication);
        let app: *mut AnyObject = msg_send![class, sharedApplication];
        if !app.is_null() {
            let _: () = msg_send![app, unhide: std::ptr::null_mut::<AnyObject>()];
        }
    }
}

/// Only macOS hides the app.
#[cfg(not(target_os = "macos"))]
pub fn unhide_app() {}

/// Shows an authentication question in the notice window. Without a
/// window the question is dropped, which is the same as cancelling it.
pub fn show_prompt(req: PromptRequest, cx: &mut App) {
    if let Some((handle, _)) = notice_window(cx) {
        let _ = handle.update(cx, |_, window, cx| prompts::show(req, window, cx));
    }
}

/// A notification of the system was clicked: the window that has what it
/// is about comes to the front.
pub fn notification_clicked(tag: &str, cx: &mut App) {
    if !cx.has_global::<AppWindows>() {
        return;
    }
    let Some(target) = cx.global_mut::<AppWindows>().notifier.clicked(tag) else {
        return;
    };
    // A terminal: its window. The rest: the notice window.
    let owner = match &target {
        crate::notifications::Target::Terminal(id) => views(cx)
            .into_iter()
            .find(|(_, v)| v.read(cx).has_terminal(*id)),
        _ => None,
    };
    let Some((handle, view)) = owner.or_else(|| notice_window(cx)) else {
        // Every window closed (macOS): a new one, which shows what it can.
        with_front_window(cx, move |app, window, cx| {
            app.open_notification_target(target, window, cx)
        });
        return;
    };
    unhide_app();
    let _ = handle.update(cx, |_, window, cx| {
        view.update(cx, |v, cx| v.open_notification_target(target, window, cx))
    });
}

/// Every terminal of every window.
fn all_terminals(cx: &App) -> Vec<Entity<TerminalView>> {
    views(cx)
        .into_iter()
        .flat_map(|(_, v)| v.read(cx).terminals())
        .collect()
}

/// A server session is open in a terminal of a window other than `except`
/// (the window asking, which is being updated and cannot be read here).
pub fn session_open_elsewhere(session_id: termoak_core::Id, except: EntityId, cx: &App) -> bool {
    views(cx)
        .iter()
        .filter(|(_, v)| v.entity_id() != except)
        .any(|(_, v)| v.read(cx).has_session(session_id, cx))
}

/// Answers the local AI about the open terminals (of every window).
pub fn answer_terminal(req: TermRequest, cx: &mut App) {
    let terms = all_terminals(cx);
    let find =
        |id: termoak_core::Id, cx: &App| terms.iter().find(|t| t.read(cx).ai_id() == id).cloned();
    let missing = |id| format!("there is no open terminal {id}; use list_sessions");
    match req {
        TermRequest::List(reply) => {
            let list = terms
                .iter()
                .map(|t| {
                    let v = t.read(cx);
                    let host_id = match v.kind() {
                        TermKind::Local { host_id } => Some(*host_id),
                        TermKind::Server { host_id, .. } => *host_id,
                        _ => None,
                    };
                    SessionSummary {
                        id: v.ai_id(),
                        title: v.title(cx).to_string(),
                        host_id,
                        status: v.ai_status().into(),
                        viewers: 1,
                    }
                })
                .collect();
            let _ = reply.send(list);
        }
        TermRequest::Read {
            id,
            max_chars,
            reply,
        } => {
            let res = find(id, cx)
                .map(|t| screen_tail(&t.read(cx).screen_text(), max_chars))
                .ok_or_else(|| missing(id));
            let _ = reply.send(res);
        }
        TermRequest::Type { id, input, reply } => {
            let res = match find(id, cx) {
                Some(t) => t.update(cx, |t, cx| t.ai_type(&input, cx)),
                None => Err(missing(id)),
            };
            let _ = reply.send(res);
        }
    }
}
