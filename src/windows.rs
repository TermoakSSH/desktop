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
//! Closing the last window quits as before (GPUI's default: on macOS the
//! app stays in the dock).

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
    model: WeakEntity<AppModel>,
    updates: WeakEntity<UpdateModel>,
    views: Vec<(AnyWindowHandle, WeakEntity<AppView>)>,
    /// The window used last (it gets the notices).
    last_active: Option<WindowId>,
    /// One of the windows has the focus now.
    any_active: bool,
    /// Notifications of the system (shared, to see repeats coming from
    /// different windows).
    pub notifier: Notifier,
}

impl Global for AppWindows {}

/// Adds a window. Returns `true` for the first one (it sets up what exists
/// once).
pub fn register(
    model: &Entity<AppModel>,
    updates: &Entity<UpdateModel>,
    view: WeakEntity<AppView>,
    window: &Window,
    cx: &mut App,
) -> bool {
    let handle = window.window_handle();
    if !cx.has_global::<AppWindows>() {
        cx.set_global(AppWindows {
            model: model.downgrade(),
            updates: updates.downgrade(),
            views: Vec::new(),
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
    first
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
    let Some(w) = cx.try_global::<AppWindows>() else {
        return;
    };
    // The model lives while a window does (on macOS, with every window
    // closed, the app has to be opened again).
    let (Some(model), Some(updates)) = (w.model.upgrade(), w.updates.upgrade()) else {
        return;
    };
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
        Ok(_) => cx.activate(true),
        Err(e) => tracing::error!(error = %e, "could not open a new window"),
    }
}

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
        return;
    };
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
