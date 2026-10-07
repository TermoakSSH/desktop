//! Terminal tab: emulation (alacritty_terminal), painting with GPUI, keyboard
//! (with dead keys and IME) and mouse (with reports to programs), history,
//! selection, copy/paste, session sharing, command autocompletion and quick
//! AI actions. Used for SSH, server sessions and the local terminal.

pub mod backend;
pub mod complete;
mod element;
pub mod find;
pub mod input;
pub mod latency;
pub mod model;
pub mod mouse;
pub mod paste;
pub mod serial;
mod share_ui;
pub mod shell;

use std::ops::Range;
use std::sync::Arc;

use alacritty_terminal::term::TermMode;
use gpui::{
    Action, App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, CursorStyle, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, Font, FontFeatures, FontStyle,
    FontWeight, InteractiveElement, IntoElement, KeyBinding, KeyDownEvent, Modifiers, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Point, Render,
    ScrollWheelEvent, SharedString, StatefulInteractiveElement, Styled, Task, UTF16Selection,
    WeakEntity, Window, div, point, prelude::FluentBuilder, px, size,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::text::TextView;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, WindowExt, h_flex, v_flex};
use serde::Deserialize;
use serde_json::{Value, json};
use termoak_client::relay::{LocalTerm, RelayShare};
use termoak_core::Id;
use termoak_ssh::terminal::OutputHub;
use termoak_ssh::{Connection, TerminalSession};

use self::backend::{Backend, Cmd, LinkJoin, LocalParams, Out, ServerTarget};
use self::complete::{LineTracker, Suggestions};
use self::element::{PADDING, TerminalElement};
use self::model::{Snapshot, TermEvent, TermModel};
use self::share_ui::ShareState;
pub use self::share_ui::{RequestKind, ShareRequest};
use crate::drag::{DragPreview, DraggedPane};
use crate::runtime;
use crate::state::{AppModel, ToastKind, api_error};
use crate::theme::TermPalette;
use crate::ui::{self, IconName};

const CONTEXT: &str = "Terminal";

/// Sends text as is to the terminal (for shortcuts the app must not capture).
#[derive(Clone, PartialEq, Eq, Deserialize, Action)]
#[action(namespace = terminal, no_json)]
pub struct SendText {
    pub text: String,
}

gpui::actions!(
    terminal,
    [
        Copy,
        Paste,
        /// Pastes the terminal's own selection (like the middle button).
        PasteSelection,
        SelectAll,
        /// Find in the terminal (screen and history).
        Find,
        /// Clears the history and asks the shell to clear the screen.
        ClearTerminal,
        ScrollPageUp,
        ScrollPageDown,
        ScrollToBottom,
        /// Closes the find bar.
        Dismiss,
        /// Next match of the find bar (older, upwards).
        FindNext,
        /// Previous match of the find bar (newer, downwards).
        FindPrevious,
        /// Find bar: upper and lower case are different (or not).
        ToggleFindCase,
        /// Find bar: the text is a regular expression (or not).
        ToggleFindRegex
    ]
);

/// Terminal shortcuts.
pub fn init(cx: &mut App) {
    let send = |text: &str| SendText {
        text: text.to_string(),
    };
    cx.bind_keys([
        KeyBinding::new("tab", send("\t"), Some(CONTEXT)),
        KeyBinding::new("shift-tab", send("\x1b[Z"), Some(CONTEXT)),
        KeyBinding::new("shift-pageup", ScrollPageUp, Some(CONTEXT)),
        KeyBinding::new("shift-pagedown", ScrollPageDown, Some(CONTEXT)),
        KeyBinding::new("shift-end", ScrollToBottom, Some(CONTEXT)),
        KeyBinding::new("escape", Dismiss, Some(FIND_CONTEXT)),
        KeyBinding::new("f3", FindNext, Some(FIND_CONTEXT)),
        KeyBinding::new("shift-f3", FindPrevious, Some(FIND_CONTEXT)),
    ]);
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("cmd-f", Find, Some(CONTEXT)),
        KeyBinding::new("cmd-f", Find, Some(FIND_CONTEXT)),
        KeyBinding::new("cmd-g", FindNext, Some(CONTEXT)),
        KeyBinding::new("cmd-g", FindNext, Some(FIND_CONTEXT)),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-g", FindPrevious, Some(FIND_CONTEXT)),
        KeyBinding::new("cmd-alt-c", ToggleFindCase, Some(FIND_CONTEXT)),
        KeyBinding::new("cmd-alt-r", ToggleFindRegex, Some(FIND_CONTEXT)),
        KeyBinding::new("cmd-k", ClearTerminal, Some(CONTEXT)),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        // In the terminal Ctrl+C and Ctrl+W belong to the shell (interrupt, delete word).
        KeyBinding::new("ctrl-c", send("\x03"), Some(CONTEXT)),
        KeyBinding::new("ctrl-w", send("\x17"), Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-c", Copy, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-v", Paste, Some(CONTEXT)),
        KeyBinding::new("shift-insert", Paste, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-a", SelectAll, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-f", Find, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-f", Find, Some(FIND_CONTEXT)),
        KeyBinding::new("alt-c", ToggleFindCase, Some(FIND_CONTEXT)),
        KeyBinding::new("alt-r", ToggleFindRegex, Some(FIND_CONTEXT)),
        KeyBinding::new("ctrl-shift-k", ClearTerminal, Some(CONTEXT)),
    ]);
}

/// Key context of the find bar.
const FIND_CONTEXT: &str = "TerminalFind";

/// Source of the terminal.
#[derive(Debug, Clone)]
pub enum TermKind {
    /// SSH from this computer.
    Local { host_id: Id },
    /// Session that lives on the server.
    Server {
        host_id: Option<Id>,
        session_id: Option<Id>,
        /// Account whose server has it (`None`: the host's account, or the
        /// current one).
        account: Option<Id>,
        /// Joined with a link (maybe on another server, maybe as a guest).
        link: Option<LinkJoin>,
    },
    /// Shell of this computer (local terminal, no SSH).
    Shell,
    /// Serial port of this computer.
    Serial { path: String, baud: u32 },
}

/// Context of a terminal for the AI.
#[derive(Debug, Clone, PartialEq)]
pub struct AiContext {
    pub host_id: Option<Id>,
    /// Server session (the AI can read it and write to it).
    pub session_id: Option<Id>,
    /// Name of the host or the terminal.
    pub label: String,
    /// Terminal of this computer (shell or serial): the AI cannot reach it.
    pub device: bool,
}

/// Connection state.
#[derive(Debug, Clone)]
pub enum TermState {
    Connecting(String),
    Running,
    Closed(Option<String>),
    Failed(String),
}

/// Notifications for the main window.
pub enum TerminalEvent {
    TitleChanged,
    /// Open SFTP over the same connection.
    OpenSftp {
        host_id: Id,
        conn: Option<Arc<Connection>>,
    },
    /// Input typed in this terminal while broadcasting: the workspace sends
    /// it to the other panes.
    Broadcast(BroadcastInput),
    /// A button of the pane controls (split view) was pressed.
    Pane(PaneAction),
    /// Someone waits to be let in or asks for the keyboard (owner).
    ShareRequest(ShareRequest),
    /// That request was answered (here, from another device or by leaving).
    ShareRequestDone {
        kind: RequestKind,
        participant: Id,
    },
    /// You got or lost the keyboard of a session shared with you (the
    /// toast is already shown; the window may also notify the system).
    KeyboardChanged {
        you_drive: bool,
        text: SharedString,
    },
}

/// User input repeated in the other panes while broadcasting. It is kept as
/// what the user did (a key, text, a paste) and not as bytes, so each
/// terminal encodes it for its own mode (application cursor keys, bracketed
/// paste...).
#[derive(Debug, Clone)]
pub enum BroadcastInput {
    Key {
        keystroke: gpui::Keystroke,
        prefer_character_input: bool,
    },
    Text(String),
    Paste(String),
    Bytes(Vec<u8>),
}

/// Pane controls shown in the toolbar of a terminal in a split view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneAction {
    ToggleMaximize,
    ToggleInclude,
    Close,
}

/// State of the pane controls, set by the workspace (`None` outside a
/// split view).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneChrome {
    pub maximized: bool,
    pub broadcasting: bool,
    /// Receives the broadcast input (not excluded).
    pub included: bool,
}

/// Find bar of the terminal.
struct FindBar {
    input: Entity<InputState>,
    options: find::FindOptions,
    /// Search of the current text (`None`: empty, or not a valid regex).
    regex: Option<alacritty_terminal::term::search::RegexSearch>,
    /// The text is not a valid regular expression.
    invalid: bool,
    /// Every match, from the top down (see `find`).
    matches: Vec<model::FindMatch>,
    /// There were more than `find::MAX_MATCHES`.
    capped: bool,
    /// Index of the current match in `matches`.
    current: Option<usize>,
    /// Matches on screen, for painting (refreshed when rendering).
    visible: Vec<find::Highlight>,
    /// Counting again after new output (at most every so often).
    refresh: Option<Task<()>>,
    _sub: gpui::Subscription,
}

/// Sharing of a local terminal through the server.
type SharedRelay = Arc<tokio::sync::Mutex<Option<RelayShare>>>;

/// View of a terminal.
pub struct TerminalView {
    app: Entity<AppModel>,
    kind: TermKind,
    host_label: String,
    /// Title set by the remote program (OSC 0/2).
    osc_title: Option<String>,
    focus: FocusHandle,
    model: TermModel,
    backend: Option<Backend>,
    state: TermState,
    local: Option<Arc<TerminalSession>>,
    can_write: bool,
    viewers: usize,
    relay: Option<SharedRelay>,
    /// Server session it is shared through (relay or server session).
    share_session: Option<Id>,
    /// Shared shell or serial port: copy of its output for the server and
    /// close notification.
    local_feed: Option<(Arc<OutputHub>, tokio::sync::watch::Sender<bool>)>,
    /// Shared by the copilot (not the user): sharing stops when the copilot
    /// is closed or stopped.
    copilot_shared: bool,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    selecting: bool,
    /// Cell where a click without drag started (to open links on release).
    press_cell: Option<(usize, usize)>,
    /// Link under the mouse (hand cursor).
    hover_link: Option<String>,
    scroll_rest: f32,
    /// Composition text of the IME or a dead key (not sent yet).
    ime_marked: Option<String>,
    /// Button whose press was reported to the program (for dragging and to
    /// report its release).
    mouse_held: Option<(MouseButton, mouse::Button)>,
    /// Last reported cell (reports happen only when the cell changes).
    mouse_cell: Option<(usize, usize)>,
    ai_busy: bool,
    /// Line being typed (autocompletion).
    line: LineTracker,
    /// Suggestions for the current line.
    suggestions: Option<Suggestions>,
    /// Suggestion lookup in progress (cancelled when more is typed).
    complete_task: Option<Task<()>>,
    /// Was on the alternate screen (vim, less...) when the last output was
    /// processed.
    was_alt: bool,
    /// Id of this terminal for the AI that runs on this computer.
    ai_id: Id,
    /// Copy of the output for the AI on this computer (while it waits for
    /// what a command prints).
    ai_tap: Option<tokio::sync::broadcast::Sender<bytes::Bytes>>,
    /// Part of a split view that is broadcasting: user input is also
    /// emitted as [`TerminalEvent::Broadcast`].
    broadcasting: bool,
    /// Pane controls (split view).
    pane: Option<PaneChrome>,
    /// Right-click menu open, where it was opened.
    context_menu: Option<(Entity<PopupMenu>, Point<Pixels>, gpui::Subscription)>,
    find: Option<FindBar>,
    /// Live sharing: participants, keyboard, requests.
    share: ShareState,
    /// Latency shown in the toolbar (SSH from here and server sessions).
    latency: latency::Probe,
    /// Measurement in progress, then the wait for the next one.
    latency_task: Option<Task<()>>,
    /// Termoak server of a server session (for the latency tooltip).
    latency_server: Option<String>,
    _reader: Option<Task<()>>,
}

/// Maximum suggestions in the autocompletion list.
const MAX_SUGGESTIONS: usize = 5;
/// Height of each row of the suggestion list.
const SUGGESTION_ROW: f32 = 24.;

impl EventEmitter<TerminalEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TerminalView {
    fn base(app: Entity<AppModel>, kind: TermKind, label: String, cx: &mut Context<Self>) -> Self {
        let scrollback = app.read(cx).settings.scrollback;
        Self {
            app,
            kind,
            host_label: label,
            osc_title: None,
            focus: cx.focus_handle(),
            model: TermModel::new(80, 24, scrollback),
            backend: None,
            state: TermState::Connecting(t!("terminal.status.preparing").to_string()),
            local: None,
            can_write: false,
            viewers: 0,
            relay: None,
            share_session: None,
            local_feed: None,
            copilot_shared: false,
            origin: Point::default(),
            cell_width: px(8.),
            line_height: px(18.),
            selecting: false,
            press_cell: None,
            hover_link: None,
            scroll_rest: 0.,
            ime_marked: None,
            mouse_held: None,
            mouse_cell: None,
            ai_busy: false,
            line: LineTracker::default(),
            suggestions: None,
            complete_task: None,
            was_alt: false,
            ai_id: termoak_core::new_id(),
            ai_tap: None,
            broadcasting: false,
            pane: None,
            context_menu: None,
            find: None,
            share: ShareState::default(),
            latency: latency::Probe::default(),
            latency_task: None,
            latency_server: None,
            _reader: None,
        }
    }

    /// Local SSH terminal.
    pub fn local(
        app: Entity<AppModel>,
        host_id: Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let label = app.read(cx).host_label(host_id);
        let mut view = Self::base(app, TermKind::Local { host_id }, label, cx);
        view.connect(window, cx);
        view
    }

    /// Terminal on the server: a new session for a host or attaching to an existing one.
    pub fn server(
        app: Entity<AppModel>,
        host_id: Option<Id>,
        session_id: Option<Id>,
        account: Option<Id>,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let account = account.or_else(|| host_id.and_then(|h| app.read(cx).account_of_host(h)));
        let mut view = Self::base(
            app,
            TermKind::Server {
                host_id,
                session_id,
                account,
                link: None,
            },
            title,
            cx,
        );
        view.connect(window, cx);
        view
    }

    /// Session shared with a link: joins it (as a guest if not signed in
    /// to that server).
    pub fn join_link(
        app: Entity<AppModel>,
        session_id: Id,
        link: LinkJoin,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::base(
            app,
            TermKind::Server {
                host_id: None,
                session_id: Some(session_id),
                account: None,
                link: Some(link),
            },
            title,
            cx,
        );
        view.connect(window, cx);
        view
    }

    /// Local terminal: a shell of this computer.
    pub fn shell(app: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let label = t!("terminal.shell_label").to_string();
        let mut view = Self::base(app, TermKind::Shell, label, cx);
        view.connect(window, cx);
        view
    }

    /// Serial terminal: a port of this computer.
    pub fn serial(
        app: Entity<AppModel>,
        params: serial::SerialParams,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let label = format!(
            "{} · {}",
            params
                .path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(&params.path),
            params.baud
        );
        let kind = TermKind::Serial {
            path: params.path,
            baud: params.baud,
        };
        let mut view = Self::base(app, kind, label, cx);
        view.connect(window, cx);
        view
    }

    /// Terminal of this computer (shell or serial port), without a host.
    fn is_local_device(&self) -> bool {
        matches!(self.kind, TermKind::Shell | TermKind::Serial { .. })
    }

    pub fn kind(&self) -> &TermKind {
        &self.kind
    }

    /// What the AI (copilot) needs to know about this terminal.
    pub fn copilot_context(&self, cx: &App) -> AiContext {
        let (host_id, session_id) = match &self.kind {
            TermKind::Local { host_id } => (Some(*host_id), self.share_session),
            // A session joined with a link may be on another server: the
            // AI (on your server) cannot reach it.
            TermKind::Server { link: Some(_), .. } => (None, None),
            TermKind::Server {
                host_id,
                session_id,
                ..
            } => (*host_id, *session_id),
            TermKind::Shell | TermKind::Serial { .. } => (None, self.share_session),
        };
        AiContext {
            host_id,
            session_id,
            label: self.label(cx),
            device: self.is_local_device(),
        }
    }

    /// Text of the visible screen.
    pub fn screen_text(&self) -> String {
        self.model.screen_text()
    }

    /// Id of this terminal for the AI that runs on this computer.
    pub fn ai_id(&self) -> Id {
        self.ai_id
    }

    /// State for the AI's terminal list.
    pub fn ai_status(&self) -> &'static str {
        match &self.state {
            TermState::Connecting(_) => "connecting",
            TermState::Running if self.can_write => "running",
            TermState::Running => "read_only",
            TermState::Closed(_) => "closed",
            TermState::Failed(_) => "failed",
        }
    }

    /// For the AI on this computer: types `input` (as the keyboard would)
    /// and returns a subscription to what the terminal prints next.
    pub fn ai_type(
        &mut self,
        input: &str,
        cx: &mut Context<Self>,
    ) -> Result<tokio::sync::broadcast::Receiver<bytes::Bytes>, String> {
        if !matches!(self.state, TermState::Running) || self.backend.is_none() {
            return Err("the terminal is not connected".into());
        }
        if !self.can_write {
            return Err("the terminal is read-only".into());
        }
        let tap = self
            .ai_tap
            .get_or_insert_with(|| tokio::sync::broadcast::channel(1024).0);
        let rx = tap.subscribe();
        // Whatever the user had half typed is not tracked any more.
        self.clear_line();
        self.write(input.as_bytes().to_vec(), cx);
        Ok(rx)
    }

    pub fn state(&self) -> &TermState {
        &self.state
    }

    /// Name of the host (read again from the model in case it changed or was
    /// not loaded yet).
    fn label(&self, cx: &App) -> String {
        if matches!(self.kind, TermKind::Shell) {
            return t!("terminal.shell_label").to_string();
        }
        let host_id = match &self.kind {
            TermKind::Local { host_id } => Some(*host_id),
            TermKind::Server { host_id, .. } => *host_id,
            TermKind::Shell | TermKind::Serial { .. } => None,
        };
        host_id
            .and_then(|id| self.app.read(cx).host(id).map(|h| h.label.clone()))
            .unwrap_or_else(|| self.host_label.clone())
    }

    /// Title of the tab.
    pub fn title(&self, cx: &App) -> SharedString {
        let label = self.label(cx);
        match &self.osc_title {
            Some(t) if !t.trim().is_empty() && t.len() < 60 => {
                format!("{label} · {}", t.trim()).into()
            }
            _ => label.into(),
        }
    }

    /// (Re)connects.
    pub fn connect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(old) = self.backend.take() {
            old.send(Cmd::Close);
        }
        self.local = None;
        self.can_write = false;
        self.ime_marked = None;
        self.mouse_held = None;
        self.clear_line();
        self.model.reset();
        self.reset_share();
        self.latency.stop();
        self.latency_task = None;
        let (cols, rows) = (self.model.cols(), self.model.rows());
        let app = self.app.read(cx);
        let rt = runtime::handle(cx);
        self.latency_server = match &self.kind {
            TermKind::Server { link: Some(l), .. } => Some(latency::server_name(l.api.base_url())),
            TermKind::Server { account, .. } => app
                .api_of(*account)
                .map(|a| latency::server_name(a.base_url())),
            _ => None,
        };
        let started = match &self.kind {
            TermKind::Local { host_id } => Some(backend::start_local(
                &rt,
                LocalParams {
                    ws: app.ws.clone(),
                    host_id: *host_id,
                    prompter: app.prompter.clone(),
                    use_agent: app.settings.use_agent,
                    cols,
                    rows,
                    conn: None,
                },
            )),
            TermKind::Server {
                session_id: Some(session_id),
                link: Some(link),
                ..
            } => Some(backend::start_server(
                &rt,
                link.api.clone(),
                ServerTarget::Link {
                    session_id: *session_id,
                    link: link.clone(),
                },
                app.prompter.clone(),
                cols,
                rows,
            )),
            TermKind::Server {
                host_id,
                session_id,
                account,
                ..
            } => match app.api_of(*account) {
                Some(api) => {
                    let target = match (session_id, host_id) {
                        (Some(s), _) => Some(ServerTarget::Attach { session_id: *s }),
                        (None, Some(h)) => Some(ServerTarget::New { host_id: *h }),
                        (None, None) => None,
                    };
                    target.map(|t| {
                        backend::start_server(&rt, api, t, app.prompter.clone(), cols, rows)
                    })
                }
                None => None,
            },
            TermKind::Shell => Some(shell::start(cols, rows)),
            TermKind::Serial { path, baud } => Some(serial::start(serial::SerialParams {
                path: path.clone(),
                baud: *baud,
            })),
        };
        let Some((backend, mut rx)) = started else {
            self.state = TermState::Failed(t!("terminal.not_signed_in").to_string());
            cx.notify();
            return;
        };
        self.backend = Some(backend);
        self.state = TermState::Connecting(if self.is_local_device() {
            t!("terminal.status.opening_terminal").to_string()
        } else {
            t!("terminal.status.connecting").to_string()
        });
        self._reader = Some(cx.spawn(async move |this, cx| {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < 512 {
                    match rx.try_recv() {
                        Ok(more) => batch.push(more),
                        Err(_) => break,
                    }
                }
                if this.update(cx, |v, cx| v.handle_out(batch, cx)).is_err() {
                    break;
                }
                // With continuous output (`yes`, a huge `cat`) there is always
                // more waiting: the UI thread is yielded so it can paint and
                // handle the keyboard (Ctrl+C) before the next batch.
                if !rx.is_empty() {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(4))
                        .await;
                }
            }
        }));
        cx.notify();
    }

    fn handle_out(&mut self, batch: Vec<Out>, cx: &mut Context<Self>) {
        for out in batch {
            match out {
                Out::Status(s) => {
                    if s.is_empty() {
                        if !matches!(self.state, TermState::Closed(_) | TermState::Failed(_)) {
                            self.state = TermState::Running;
                            // The server ignores size changes until the terminal starts.
                            self.sync_size();
                        }
                    } else {
                        self.state = TermState::Connecting(s);
                    }
                }
                Out::Local(term) => {
                    if let TermKind::Local { host_id } = self.kind {
                        let conn = term.connection().clone();
                        self.app
                            .update(cx, |m, cx| m.start_auto_forwards(host_id, conn, cx));
                    }
                    self.local = Some(term);
                    self.can_write = true;
                    self.state = TermState::Running;
                    self.sync_size();
                }
                Out::Shell => {
                    self.can_write = true;
                    self.state = TermState::Running;
                    self.sync_size();
                }
                Out::Remote(seat) => {
                    if let TermKind::Server { session_id: s, .. } = &mut self.kind {
                        *s = Some(seat.session_id);
                    }
                    self.share_session = Some(seat.session_id);
                    self.on_remote_seat(seat, cx);
                    self.state = TermState::Running;
                    self.sync_size();
                }
                Out::Control {
                    driver,
                    driver_name,
                    can_write,
                    until,
                } => self.on_control(driver, driver_name, Some(can_write), until, cx),
                Out::ControlExpired(participant) => self.on_control_expired(participant, cx),
                Out::Participants { list, driver } => self.on_participants(list, driver, cx),
                Out::Waiting { owner, title } => {
                    self.share.waiting = Some((owner, title));
                    self.can_write = false;
                }
                Out::JoinRequest(p) => self.on_share_request(RequestKind::Join, p, cx),
                Out::ControlRequest(p) => self.on_share_request(RequestKind::Control, p, cx),
                Out::ControlDenied => {
                    self.app.update(cx, |m, cx| {
                        m.toast(
                            ToastKind::Info,
                            t!("share.toast.control_denied").to_string(),
                            cx,
                        )
                    });
                }
                Out::Ended(code) => self.on_ended(code),
                Out::Data(bytes) => {
                    if matches!(self.state, TermState::Connecting(_)) {
                        self.state = TermState::Running;
                    }
                    if let Some((hub, _)) = &self.local_feed {
                        hub.push(bytes.clone());
                    }
                    if let Some(tap) = &self.ai_tap
                        && tap.receiver_count() > 0
                    {
                        let _ = tap.send(bytes.clone());
                    }
                    let events = self.model.feed(&bytes);
                    if let Some(b) = &self.backend {
                        b.consumed(bytes.len());
                    }
                    self.schedule_find_refresh(cx);
                    self.handle_term_events(events, cx);
                    // When leaving vim, less... the shell paints a new line.
                    let alt = self.model.mode().contains(TermMode::ALT_SCREEN);
                    if self.was_alt && !alt {
                        self.clear_line();
                    }
                    self.was_alt = alt;
                }
                Out::Reset => self.model.reset(),
                Out::Notice(msg) => {
                    self.app
                        .update(cx, |m, cx| m.toast(ToastKind::Warning, msg, cx));
                }
                Out::Presence(n) => self.viewers = n,
                Out::Closed(reason) => {
                    if self.is_local_device() {
                        // Leave a note on the screen itself, like other terminals.
                        let text = match &reason {
                            Some(r) => r.clone(),
                            None if matches!(self.kind, TermKind::Shell) => {
                                t!("terminal.shell.exited").to_string()
                            }
                            None => t!("terminal.serial.port_closed").to_string(),
                        };
                        let line = format!("\r\n\x1b[0;2m[{text}]\x1b[0m\r\n");
                        self.model.feed(line.as_bytes());
                    }
                    self.ime_marked = None;
                    self.mouse_held = None;
                    self.share.waiting = None;
                    // The connection closing right after the reason (status,
                    // end code) does not erase it.
                    let keep =
                        matches!(&self.state, TermState::Closed(Some(_))) && reason.is_none();
                    if !keep {
                        self.state = TermState::Closed(reason);
                    }
                    self.stop_relay(false, cx);
                }
                Out::Failed(e) => self.state = TermState::Failed(e),
            }
        }
        cx.notify();
    }

    fn handle_term_events(&mut self, events: Vec<TermEvent>, cx: &mut Context<Self>) {
        for ev in events {
            match ev {
                TermEvent::Write(bytes) => {
                    if let Some(b) = &self.backend {
                        b.input(bytes);
                    }
                }
                TermEvent::Title(t) => {
                    self.osc_title = Some(t);
                    cx.emit(TerminalEvent::TitleChanged);
                }
                TermEvent::ResetTitle => {
                    self.osc_title = None;
                    cx.emit(TerminalEvent::TitleChanged);
                }
                TermEvent::Copy(text) => cx.write_to_clipboard(ClipboardItem::new_string(text)),
                TermEvent::Bell => {}
            }
        }
    }

    /// Sends the current size to the other end.
    fn sync_size(&self) {
        if let Some(b) = &self.backend {
            b.send(Cmd::Resize(self.model.cols(), self.model.rows()));
        }
    }

    /// Closes the terminal (or detaches from the server session).
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        self.stop_relay(false, cx);
        if let Some(b) = self.backend.take() {
            b.send(Cmd::Close);
        }
        self._reader = None;
    }

    /// Called by the element when it knows the available space.
    pub(crate) fn set_geometry(
        &mut self,
        origin: Point<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
        cols: u16,
        rows: u16,
        cx: &mut Context<Self>,
    ) {
        self.origin = origin;
        self.cell_width = cell_width;
        self.line_height = line_height;
        if (cols, rows) != (self.model.cols(), self.model.rows()) {
            self.model.resize(cols, rows);
            self.schedule_find_refresh(cx);
            if let Some(b) = &self.backend {
                b.send(Cmd::Resize(cols, rows));
            }
            if let Some(relay) = self.relay.clone() {
                runtime::handle(cx).spawn(async move {
                    if let Some(r) = relay.lock().await.as_ref() {
                        r.resize(cols, rows).await;
                    }
                });
            }
        }
    }

    pub(crate) fn snapshot(&self, palette: &TermPalette, focused: bool) -> Snapshot {
        let mut snap = self.model.snapshot(palette, focused);
        if let Some(bar) = &self.find {
            snap.highlights = bar.visible.clone();
        }
        if !matches!(self.state, TermState::Running) {
            snap.cursor = None;
        }
        snap
    }

    // ----- Input -----

    fn write(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if !self.can_write || !matches!(self.state, TermState::Running) {
            return;
        }
        if let Some(b) = &self.backend {
            self.model.scroll_to_bottom();
            b.input(bytes);
            cx.notify();
        }
    }

    /// User input (keyboard, IME, paste): besides sending it, tracks the
    /// line being typed for autocompletion.
    fn write_input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if !self.can_write || !matches!(self.state, TermState::Running) || self.backend.is_none() {
            return;
        }
        self.track_input(&bytes, cx);
        self.write(bytes, cx);
    }

    /// Special keys, Ctrl and Alt. Normal text is not handled here: it goes
    /// on to the input handler (`EntityInputHandler`), which receives it
    /// already composed (dead keys, AltGr, IME) and only once.
    fn on_key_down(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // Keys typed in the find bar are not for the terminal.
        if !self.focus.is_focused(window) {
            return;
        }
        if self.suggestion_key(ev, cx) {
            cx.stop_propagation();
            return;
        }
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        if paste::ctrl_v_pastes(
            cfg!(target_os = "macos"),
            self.app.read(cx).settings.ctrl_v_pastes,
            &ks.key,
            m.control,
            m.shift,
            m.alt,
            m.platform,
        ) {
            cx.stop_propagation();
            self.paste(window, cx);
            return;
        }
        if let Some(bytes) = input::to_bytes(ks, ev.prefer_character_input, self.model.mode()) {
            cx.stop_propagation();
            if self.model.has_selection() {
                self.model.clear_selection();
            }
            self.emit_broadcast(
                BroadcastInput::Key {
                    keystroke: ks.clone(),
                    prefer_character_input: ev.prefer_character_input,
                },
                cx,
            );
            self.write_input(bytes, cx);
        }
    }

    /// Text committed by the keyboard or the IME.
    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.model.has_selection() {
            self.model.clear_selection();
        }
        self.emit_broadcast(BroadcastInput::Text(text.to_string()), cx);
        self.write_input(input::text_bytes(text), cx);
    }

    // ----- Split view -----

    /// Part of a split view that broadcasts (or not any more).
    pub fn set_broadcasting(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.broadcasting != on {
            self.broadcasting = on;
            cx.notify();
        }
    }

    /// Pane controls of the split view (`None` for a terminal alone).
    pub fn set_pane(&mut self, pane: Option<PaneChrome>, cx: &mut Context<Self>) {
        if self.pane != pane {
            self.pane = pane;
            cx.notify();
        }
    }

    /// Starts a latency measurement if it is due (called when rendering,
    /// so only terminals on screen measure). Paused while not connected.
    fn poll_latency(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.kind, TermKind::Local { .. } | TermKind::Server { .. }) {
            return;
        }
        let connected = matches!(self.state, TermState::Running) && self.backend.is_some();
        if !connected {
            self.latency.stop();
            self.latency_task = None;
            return;
        }
        let now = std::time::Instant::now();
        let Some(backend) = &self.backend else {
            return;
        };
        if !self.latency.due(now, connected) {
            return;
        }
        let (reply, answer) = tokio::sync::oneshot::channel();
        backend.send(Cmd::Latency(reply));
        let generation = self.latency.start();
        self.latency_task = Some(cx.spawn(async move |this, cx| {
            let rtt = answer.await.ok().flatten();
            let updated = this.update(cx, |v, cx| {
                if v.latency.finish(generation, rtt, std::time::Instant::now()) {
                    cx.notify();
                }
            });
            if updated.is_err() {
                return;
            }
            // Renders again when the next one is due (if still on screen).
            cx.background_executor().timer(latency::INTERVAL).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        }));
    }

    /// Latency badge: text, color and tooltip (`None`: not shown).
    fn latency_badge(&self, cx: &App) -> Option<(String, gpui::Hsla, SharedString)> {
        if !matches!(self.state, TermState::Running) {
            return None;
        }
        let tooltip = match &self.kind {
            TermKind::Local { .. } => t!("terminal.latency.tooltip_host", host = self.label(cx)),
            TermKind::Server { .. } => t!(
                "terminal.latency.tooltip_server",
                server = self
                    .latency_server
                    .clone()
                    .unwrap_or_else(|| self.label(cx))
            ),
            TermKind::Shell | TermKind::Serial { .. } => return None,
        };
        let rtt = self.latency.value();
        let theme = cx.theme();
        let color = match latency::level(rtt) {
            latency::Level::Unknown | latency::Level::Good => theme.muted_foreground,
            latency::Level::Fair => theme.warning,
            latency::Level::Poor => theme.danger,
        };
        Some((latency::format(rtt), color, tooltip.to_string().into()))
    }

    /// Repeats user input for the other panes while broadcasting.
    fn emit_broadcast(&self, input: BroadcastInput, cx: &mut Context<Self>) {
        if self.broadcasting {
            cx.emit(TerminalEvent::Broadcast(input));
        }
    }

    /// Input typed in another pane of the split view: encoded for this
    /// terminal's own mode and sent as if typed here.
    pub fn apply_broadcast(&mut self, input: &BroadcastInput, cx: &mut Context<Self>) {
        if self.model.has_selection() {
            self.model.clear_selection();
        }
        let mode = self.model.mode();
        let bytes = match input {
            BroadcastInput::Key {
                keystroke,
                prefer_character_input,
            } => input::to_bytes(keystroke, *prefer_character_input, mode),
            BroadcastInput::Text(text) => Some(input::text_bytes(text)),
            BroadcastInput::Paste(text) => Some(input::paste_bytes(text, mode)),
            BroadcastInput::Bytes(bytes) => Some(bytes.clone()),
        };
        if let Some(bytes) = bytes {
            self.write_input(bytes, cx);
        }
    }

    /// Types a snippet: "Run" (text ending with a line break) is typed as
    /// is, so each line runs; "Paste" goes as a paste (bracketed if the
    /// program asks for it), so nothing runs until Enter.
    pub fn send_snippet(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = if text.ends_with('\n') {
            input::text_bytes(text)
        } else {
            input::paste_bytes(text, self.model.mode())
        };
        self.write_input(bytes, cx);
    }

    /// Whether some text is selected.
    pub fn has_selection(&self) -> bool {
        self.model.has_selection()
    }

    /// Whether the terminal can be written to now.
    pub fn writable(&self) -> bool {
        self.can_write && matches!(self.state, TermState::Running)
    }

    /// Host of the terminal, if it has one (SSH or server session).
    pub fn host(&self) -> Option<Id> {
        self.host_id()
    }

    /// Connection of an SSH terminal from this computer (to reuse it for
    /// SFTP).
    pub fn ssh_connection(&self) -> Option<Arc<Connection>> {
        self.local.as_ref().map(|t| t.connection().clone())
    }

    /// Whether the session ended or failed (reconnect makes sense).
    pub fn ended(&self) -> bool {
        matches!(self.state, TermState::Closed(_) | TermState::Failed(_))
    }

    // ----- Autocompletion -----

    /// Forgets the line and the suggestions (new connection, vim exited...).
    fn clear_line(&mut self) {
        self.line.reset();
        self.suggestions = None;
        self.complete_task = None;
    }

    fn autocomplete_enabled(&self, cx: &App) -> bool {
        self.app.read(cx).settings.autocomplete
    }

    /// Server client of the account of this terminal: its session's, its
    /// host's, or the current account (This device hosts, shell, serial).
    pub fn account_api(&self, cx: &App) -> Option<termoak_client::ApiClient> {
        let app = self.app.read(cx);
        match &self.kind {
            TermKind::Server {
                account: Some(a), ..
            } => app.api_of(Some(*a)),
            TermKind::Local { host_id }
            | TermKind::Server {
                host_id: Some(host_id),
                ..
            } => app.api_for_host(*host_id),
            _ => app.api.clone(),
        }
    }

    /// Host of the terminal (for the history and the system).
    fn host_id(&self) -> Option<Id> {
        match &self.kind {
            TermKind::Local { host_id } => Some(*host_id),
            TermKind::Server { host_id, .. } => *host_id,
            TermKind::Shell | TermKind::Serial { .. } => None,
        }
    }

    /// Is what is on screen before the cursor the typed line, with nothing to
    /// its right? If not (questions without echo such as passwords, lines
    /// the shell changed...), nothing is suggested or saved.
    fn line_on_screen(&self, line: &str) -> bool {
        let (before, after_blank) = self.model.text_around_cursor(line.chars().count());
        complete::screen_matches(line, &before, after_blank)
    }

    /// Tracks what is typed: saves the line sent with Enter in the history
    /// and looks for suggestions for the new one.
    fn track_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.model.mode().contains(TermMode::ALT_SCREEN) {
            // Inside vim, less... there is no shell line.
            self.line.forget();
            self.suggestions = None;
            return;
        }
        let pending = self.line.current();
        let echoed = pending.as_deref().is_some_and(|l| self.line_on_screen(l));
        if let Some(sent) = self.line.feed(bytes)
            && echoed
            && pending.as_deref() == Some(sent.as_str())
        {
            self.record_command(sent, cx);
        }
        self.refresh_suggestions(cx);
    }

    /// Saves a command in the host history (on this device only).
    fn record_command(&self, command: String, cx: &mut Context<Self>) {
        let Some(host) = self.host_id() else {
            return;
        };
        if !self.autocomplete_enabled(cx) {
            return;
        }
        let ws = self.app.read(cx).ws.clone();
        runtime::handle(cx).spawn(async move {
            if let Err(e) = ws.record_command(host, &command).await {
                tracing::debug!(error = %e, "could not save the command in the history");
            }
        });
    }

    /// Asks for suggestions for the current line (while they arrive, the
    /// previous ones that still match stay, so they do not flicker).
    fn refresh_suggestions(&mut self, cx: &mut Context<Self>) {
        let line = self
            .line
            .current()
            .filter(|l| self.line.at_end() && !l.trim().is_empty());
        let Some(line) = line.filter(|_| self.autocomplete_enabled(cx)) else {
            self.suggestions = None;
            self.complete_task = None;
            return;
        };
        if self.suggestions.as_ref().is_some_and(|s| s.line == line) {
            return;
        }
        self.suggestions = self.suggestions.as_ref().and_then(|s| s.narrow(&line));
        let app = self.app.read(cx);
        let ws = app.ws.clone();
        let host = self.host_id();
        let os = match &self.kind {
            TermKind::Shell => Some(std::env::consts::OS.to_string()),
            _ => host.and_then(|h| app.host(h)).and_then(|h| h.os.clone()),
        };
        let query = line.clone();
        let task = runtime::spawn(cx, async move {
            ws.complete(host, os.as_deref(), &query, MAX_SUGGESTIONS)
                .await
                .map_err(api_error)
        });
        self.complete_task = Some(cx.spawn(async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |v, cx| {
                // Only if the line has not changed in the meantime.
                if v.line.current().as_deref() != Some(line.as_str()) {
                    return;
                }
                match res {
                    Ok(items) => {
                        let items: Vec<_> = items
                            .into_iter()
                            .filter(|s| {
                                !s.insert.is_empty() && !s.insert.chars().any(char::is_control)
                            })
                            .collect();
                        v.suggestions = (!items.is_empty()).then(|| Suggestions {
                            line: line.clone(),
                            items,
                            selected: 0,
                        });
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "autocompletion not available");
                        v.suggestions = None;
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Visible suggestions (the selected one is painted as ghost text after
    /// the cursor), if any. Only with autocompletion on, outside the
    /// alternate screen, not looking at the history nor composing text, with
    /// the cursor at the end of what was typed and the screen matching the
    /// line.
    pub(crate) fn visible_suggestion(&self, cx: &App) -> Option<&Suggestions> {
        let s = self.suggestions.as_ref()?;
        s.current()?;
        if !self.autocomplete_enabled(cx)
            || !self.can_write
            || !matches!(self.state, TermState::Running)
            || self.ime_marked.is_some()
            || self.model.has_selection()
            || self.model.display_offset() != 0
            || self.model.mode().contains(TermMode::ALT_SCREEN)
            || !self.line.at_end()
        {
            return None;
        }
        let line = self.line.current()?;
        (line == s.line && self.line_on_screen(&line)).then_some(s)
    }

    /// Ghost text and the cursor cell where it starts (for the element).
    pub(crate) fn ghost_text(&self, cx: &App) -> Option<(String, usize, usize)> {
        let insert = self.visible_suggestion(cx)?.current()?.insert.clone();
        let (row, col) = self.model.cursor_cell()?;
        Some((insert, row, col))
    }

    /// Accepts the visible suggestion: types what is missing.
    fn accept_suggestion(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(insert) = self
            .visible_suggestion(cx)
            .and_then(|s| s.current())
            .map(|s| s.insert.clone())
        else {
            return false;
        };
        self.emit_broadcast(BroadcastInput::Text(insert.clone()), cx);
        self.write_input(input::text_bytes(&insert), cx);
        true
    }

    /// Autocompletion keys: → accepts and Alt+↑/↓ chooses in the list. Only
    /// if a suggestion is visible; otherwise they go to the terminal as usual.
    fn suggestion_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let ks = &ev.keystroke;
        let m = &ks.modifiers;
        let plain = !m.control && !m.alt && !m.shift && !m.platform && !m.function;
        let only_alt = m.alt && !m.control && !m.shift && !m.platform;
        match ks.key.as_str() {
            "right" if plain => self.accept_suggestion(cx),
            "up" | "down" if only_alt => {
                let listed = self
                    .visible_suggestion(cx)
                    .is_some_and(|s| s.items.len() > 1);
                if listed && let Some(s) = self.suggestions.as_mut() {
                    s.step(if ks.key == "up" { -1 } else { 1 });
                    cx.notify();
                }
                listed
            }
            _ => false,
        }
    }

    /// Suggestion list below (or above) the cursor.
    fn render_suggestions(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let s = self.visible_suggestion(cx)?;
        if s.items.len() < 2 {
            return None;
        }
        let (row, col) = self.model.cursor_cell()?;
        let items = s.items.clone();
        let selected = s.selected;
        let typed = s.line.chars().count();
        let theme = cx.theme();
        let mono = ui::mono_family(cx);
        let width = 460f32;
        let height = SUGGESTION_ROW * items.len() as f32 + 30.;
        let area_w = PADDING * 2. + self.cell_width * self.model.cols() as f32;
        let x = (PADDING + self.cell_width * col as f32)
            .min(area_w - px(width) - PADDING)
            .max(px(4.));
        let fits_below = row + 1 + items.len() + 2 <= self.model.rows() as usize;
        let y = if fits_below {
            PADDING / 2. + self.line_height * (row + 1) as f32
        } else {
            (PADDING / 2. + self.line_height * row as f32 - px(height)).max(px(0.))
        };
        Some(
            v_flex()
                .id("suggestions")
                .absolute()
                .left(x)
                .top(y)
                .w(px(width))
                .p_1()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.border)
                .bg(theme.popover)
                .shadow_md()
                .children(items.into_iter().enumerate().map(|(i, item)| {
                    let (head, tail): (String, String) = {
                        let chars: Vec<char> = item.text.chars().collect();
                        let cut = typed.min(chars.len());
                        (chars[..cut].iter().collect(), chars[cut..].iter().collect())
                    };
                    h_flex()
                        .id(("suggestion", i))
                        .h(px(SUGGESTION_ROW))
                        .px_2()
                        .gap_3()
                        .items_center()
                        .rounded(theme.radius)
                        .cursor_pointer()
                        .when(i == selected, |this| this.bg(theme.list_active))
                        .when(i != selected, |this| {
                            this.hover(|s| s.bg(theme.secondary_hover))
                        })
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                if let Some(s) = this.suggestions.as_mut() {
                                    s.selected = i;
                                }
                                this.accept_suggestion(cx);
                                this.focus.focus(window, cx);
                            }),
                        )
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .font_family(mono.clone())
                                .text_sm()
                                .child(div().text_color(theme.muted_foreground).child(head))
                                .child(div().text_color(theme.popover_foreground).child(tail)),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .max_w(px(180.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(item.description.clone()),
                        )
                }))
                .child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("terminal.suggestions.hint")),
                )
                .into_any_element(),
        )
    }

    /// Composition text and the cursor cell where it is painted.
    pub(crate) fn ime_preedit(&self) -> Option<(String, usize, usize)> {
        let text = self.ime_marked.clone()?;
        let (row, col) = self.model.cursor_cell()?;
        Some((text, row, col))
    }

    fn on_send_text(&mut self, action: &SendText, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            cx.propagate();
            return;
        }
        // Tab accepts the visible suggestion; if there is none, the shell completes.
        if action.text == "\t" && self.accept_suggestion(cx) {
            return;
        }
        self.emit_broadcast(BroadcastInput::Bytes(action.text.clone().into_bytes()), cx);
        self.write_input(action.text.clone().into_bytes(), cx);
    }

    fn on_copy(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        self.copy_selection(window, cx);
    }

    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        self.paste(window, cx);
    }

    fn on_paste_selection(
        &mut self,
        _: &PasteSelection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = self.model.selection_text() {
            self.paste_text(text, window, cx);
        }
    }

    fn on_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        self.open_find(window, cx);
    }

    fn on_clear(&mut self, _: &ClearTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_terminal(cx);
    }

    fn on_dismiss(&mut self, _: &Dismiss, window: &mut Window, cx: &mut Context<Self>) {
        self.close_find(window, cx);
    }

    /// Clears the history and, outside full-screen programs, asks the shell
    /// to clear the screen (Ctrl+L), so its idea of the cursor stays right.
    pub fn clear_terminal(&mut self, cx: &mut Context<Self>) {
        self.model.clear_history();
        self.schedule_find_refresh(cx);
        if !self.model.mode().contains(TermMode::ALT_SCREEN) {
            self.write(vec![0x0c], cx);
        }
        cx.notify();
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.model.select_all();
        cx.notify();
    }

    fn on_page_up(&mut self, _: &ScrollPageUp, _: &mut Window, cx: &mut Context<Self>) {
        self.model.scroll_page(true);
        cx.notify();
    }

    fn on_page_down(&mut self, _: &ScrollPageDown, _: &mut Window, cx: &mut Context<Self>) {
        self.model.scroll_page(false);
        cx.notify();
    }

    fn on_bottom(&mut self, _: &ScrollToBottom, _: &mut Window, cx: &mut Context<Self>) {
        self.model.scroll_to_bottom();
        cx.notify();
    }

    pub fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.model.selection_text() {
            Some(text) => {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
                ui::notify(window, cx, ToastKind::Info, t!("common.copied"));
            }
            None => ui::notify(window, cx, ToastKind::Info, t!("terminal.no_selection")),
        }
    }

    /// Pastes the clipboard.
    pub fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) {
            self.paste_text(text, window, cx);
        }
    }

    /// Pastes text, asking first if it has several lines (and the option is
    /// on and the program does not use bracketed paste).
    pub fn paste_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if text.is_empty() || !self.writable() {
            return;
        }
        let bracketed = self.model.mode().contains(TermMode::BRACKETED_PASTE);
        let confirm = self.app.read(cx).settings.confirm_multiline_paste;
        if !paste::needs_confirmation(&text, confirm, bracketed) {
            self.paste_now(&text, cx);
            return;
        }
        let lines = paste::line_count(&text);
        let preview: SharedString = paste::preview(&text, 12).into();
        let weak = cx.entity().downgrade();
        let app = self.app.clone();
        let dont_ask = std::rc::Rc::new(std::cell::Cell::new(false));
        window.open_dialog(cx, move |d, _, cx| {
            let (weak_ok, weak_enter) = (weak.clone(), weak.clone());
            let (text_ok, text_enter) = (text.clone(), text.clone());
            let (app_ok, app_enter) = (app.clone(), app.clone());
            let (ask_ok, ask_enter, ask_box) =
                (dont_ask.clone(), dont_ask.clone(), dont_ask.clone());
            let checked = dont_ask.get();
            let accept = move |weak: &WeakEntity<TerminalView>,
                               app: &Entity<AppModel>,
                               text: &str,
                               dont_ask: bool,
                               window: &mut Window,
                               cx: &mut App| {
                if dont_ask {
                    app.update(cx, |m, cx| {
                        let mut s = m.settings.clone();
                        s.confirm_multiline_paste = false;
                        m.save_settings(s, cx);
                    });
                }
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |v, cx| {
                        v.paste_now(text, cx);
                        v.focus.focus(window, cx);
                    });
                }
            };
            let accept_enter = accept.clone();
            d.title(tn!("terminal.paste_confirm.title", lines))
                .w(px(560.))
                .on_ok(move |_, window, cx| {
                    accept_enter(
                        &weak_enter,
                        &app_enter,
                        &text_enter,
                        ask_enter.get(),
                        window,
                        cx,
                    );
                    true
                })
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("terminal.paste_confirm.message")),
                        )
                        .child(
                            div()
                                .id("paste-preview")
                                .p_3()
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted)
                                .font_family(ui::mono_family(cx))
                                .text_xs()
                                .whitespace_normal()
                                .child(preview.clone()),
                        )
                        .child(
                            Checkbox::new("paste-dont-ask")
                                .label(t!("terminal.paste_confirm.dont_ask"))
                                .checked(checked)
                                .on_click(move |v: &bool, window, _| {
                                    ask_box.set(*v);
                                    window.refresh();
                                }),
                        ),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("paste-cancel")
                                .label(t!("common.cancel"))
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("paste-ok")
                                .primary()
                                .icon(ui::icon(IconName::ClipboardPaste))
                                .label(t!("terminal.paste_confirm.ok"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    accept(&weak_ok, &app_ok, &text_ok, ask_ok.get(), window, cx);
                                }),
                        ),
                )
        });
    }

    /// Pastes without asking (and repeats it in the other panes while
    /// broadcasting).
    fn paste_now(&mut self, text: &str, cx: &mut Context<Self>) {
        self.emit_broadcast(BroadcastInput::Paste(text.to_string()), cx);
        let bytes = input::paste_bytes(text, self.model.mode());
        self.write_input(bytes, cx);
    }

    // ----- Find -----

    fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(find) = &self.find {
            let input = find.input.clone();
            input.update(cx, |i, cx| {
                i.focus(window, cx);
                i.select_all(window, cx);
            });
            return;
        }
        let initial = self
            .model
            .selection_text()
            .filter(|t| !t.contains('\n') && t.chars().count() <= 200)
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("terminal.find.placeholder"))
                .default_value(initial)
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &gpui_component::input::InputEvent, _, cx| {
                use gpui_component::input::InputEvent;
                match ev {
                    InputEvent::Change => this.find_changed(cx),
                    // Enter: the next (older) match; Shift+Enter: back down.
                    InputEvent::PressEnter { shift, .. } => this.find_step(!*shift, cx),
                    _ => {}
                }
            },
        );
        input.update(cx, |i, cx| i.focus(window, cx));
        self.find = Some(FindBar {
            input,
            options: find::FindOptions::default(),
            regex: None,
            invalid: false,
            matches: Vec::new(),
            capped: false,
            current: None,
            visible: Vec::new(),
            refresh: None,
            _sub: sub,
        });
        self.find_changed(cx);
        cx.notify();
    }

    fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.find.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// The text or a toggle changed: searches again and goes to the match
    /// nearest to the current one (or to the bottom of what is on screen).
    fn find_changed(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        let query = bar.input.read(cx).value().to_string();
        let pattern = find::pattern(&query, bar.options);
        bar.regex = pattern
            .as_deref()
            .and_then(|p| alacritty_terminal::term::search::RegexSearch::new(p).ok());
        bar.invalid = pattern.is_some() && bar.regex.is_none();
        let anchor = self
            .model
            .selection_range()
            .map(|m| *m.end())
            .unwrap_or_else(|| self.model.view_bottom());
        self.recount_matches();
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        bar.current = find::nearest(&bar.matches, anchor);
        match bar.current.and_then(|i| bar.matches.get(i)).cloned() {
            Some(m) => self.model.select_match(&m),
            None => self.model.clear_selection(),
        }
        cx.notify();
    }

    /// Lists the matches again (the text changed or scrolled) and finds the
    /// current one: the selected match, which moves with the text.
    fn recount_matches(&mut self) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        let (matches, capped) = match bar.regex.as_mut() {
            Some(regex) => self.model.find_all(regex, find::MAX_MATCHES),
            None => (Vec::new(), false),
        };
        bar.matches = matches;
        bar.capped = capped;
        bar.current = self
            .model
            .selection_range()
            .and_then(|sel| find::position(&bar.matches, &sel));
    }

    /// Goes to the next match: `older` upwards (Enter, F3), otherwise
    /// downwards (Shift+Enter, Shift+F3). Both wrap around.
    fn find_step(&mut self, older: bool, cx: &mut Context<Self>) {
        if self.find.is_none() {
            return;
        }
        self.recount_matches();
        let bottom = self.model.view_bottom();
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        bar.current = match bar.current {
            Some(i) => find::step(bar.matches.len(), Some(i), older),
            // Nothing selected: the first step lands on the match nearest
            // to the bottom of the screen.
            None => find::nearest(&bar.matches, bottom),
        };
        if let Some(m) = bar.current.and_then(|i| bar.matches.get(i)).cloned() {
            self.model.select_match(&m);
        }
        cx.notify();
    }

    fn toggle_find_option(&mut self, regex: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        if regex {
            bar.options.regex = !bar.options.regex;
        } else {
            bar.options.case_sensitive = !bar.options.case_sensitive;
        }
        let input = bar.input.clone();
        input.update(cx, |i, cx| i.focus(window, cx));
        self.find_changed(cx);
    }

    /// New output while the find bar is open: the count is redone soon
    /// (not for every piece of output). The highlights on screen and the
    /// current match follow the text on their own.
    fn schedule_find_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        if bar.refresh.is_some() || bar.regex.is_none() {
            return;
        }
        bar.refresh = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |this, cx| {
                if let Some(bar) = this.find.as_mut() {
                    bar.refresh = None;
                }
                this.recount_matches();
                cx.notify();
            });
        }));
    }

    /// Matches on screen, for the next frame.
    fn update_find_highlights(&mut self) {
        let Some(bar) = self.find.as_mut() else {
            return;
        };
        bar.visible.clear();
        let Some(regex) = bar.regex.as_mut() else {
            return;
        };
        let current = self.model.selection_range();
        let offset = self.model.display_offset();
        let (lines, cols) = (self.model.rows() as usize, self.model.cols() as usize);
        for m in self.model.visible_matches(regex) {
            let is_current = current.as_ref() == Some(&m);
            bar.visible
                .extend(find::highlights(&m, offset, lines, cols, is_current));
        }
    }

    fn on_find_next(&mut self, _: &FindNext, _: &mut Window, cx: &mut Context<Self>) {
        self.find_step(true, cx);
    }

    fn on_find_previous(&mut self, _: &FindPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.find_step(false, cx);
    }

    fn on_toggle_find_case(
        &mut self,
        _: &ToggleFindCase,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_find_option(false, window, cx);
    }

    fn on_toggle_find_regex(
        &mut self,
        _: &ToggleFindRegex,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_find_option(true, window, cx);
    }

    fn render_find(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let find = self.find.as_ref()?;
        let theme = cx.theme();
        let mac = cfg!(target_os = "macos");
        let empty = find.input.read(cx).value().is_empty();
        let none = !empty && !find.invalid && find.matches.is_empty();
        let status: Option<(SharedString, gpui::Hsla)> = if find.invalid {
            Some((t!("terminal.find.invalid_regex"), theme.danger))
        } else if none {
            Some((t!("terminal.find.none"), theme.danger))
        } else if empty {
            None
        } else {
            let total = if find.capped {
                format!("{}+", find.matches.len())
            } else {
                find.matches.len().to_string()
            };
            let current = find
                .current
                .map(|i| find::ordinal(find.matches.len(), i).to_string())
                .unwrap_or_else(|| "–".into());
            Some((
                t!("terminal.find.count", current = current, total = total),
                theme.muted_foreground,
            ))
        };
        let toggle = |id: &'static str, label: &'static str, on: bool, tip: SharedString| {
            Button::new(id)
                .xsmall()
                .map(|b| if on { b.primary() } else { b.ghost() })
                .child(
                    div()
                        .font_family(ui::mono_family(cx))
                        .text_xs()
                        .child(label),
                )
                .tooltip(tip)
        };
        Some(
            h_flex()
                .id("terminal-find")
                .key_context(FIND_CONTEXT)
                .on_action(cx.listener(Self::on_dismiss))
                .on_action(cx.listener(Self::on_find))
                .on_action(cx.listener(Self::on_find_next))
                .on_action(cx.listener(Self::on_find_previous))
                .on_action(cx.listener(Self::on_toggle_find_case))
                .on_action(cx.listener(Self::on_toggle_find_regex))
                .absolute()
                .top_2()
                .right_4()
                .w(px(460.))
                .p_1()
                .gap_1()
                .items_center()
                .rounded(theme.radius)
                .border_1()
                .border_color(if find.invalid || none {
                    theme.danger
                } else {
                    theme.border
                })
                .bg(theme.popover)
                .shadow_md()
                // Clicks here are not terminal clicks.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&find.input)
                            .small()
                            .prefix(ui::icon(IconName::Search).size(px(14.))),
                    ),
                )
                .when_some(status, |this, (text, color)| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(color)
                            .whitespace_nowrap()
                            .child(text),
                    )
                })
                .child(
                    toggle(
                        "find-case",
                        "Aa",
                        find.options.case_sensitive,
                        t!(
                            "terminal.find.case_sensitive",
                            shortcut = if mac { "⌥⌘C" } else { "Alt+C" }
                        ),
                    )
                    .on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.toggle_find_option(false, window, cx)
                        },
                    )),
                )
                .child(
                    toggle(
                        "find-regex",
                        ".*",
                        find.options.regex,
                        t!(
                            "terminal.find.regex",
                            shortcut = if mac { "⌥⌘R" } else { "Alt+R" }
                        ),
                    )
                    .on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| {
                            this.toggle_find_option(true, window, cx)
                        },
                    )),
                )
                .child(
                    Button::new("find-older")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ChevronUp))
                        .tooltip(t!("terminal.find.next"))
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.find_step(true, cx)),
                        ),
                )
                .child(
                    Button::new("find-newer")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ChevronDown))
                        .tooltip(t!("terminal.find.previous"))
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.find_step(false, cx)),
                        ),
                )
                .child(
                    Button::new("find-close")
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::X))
                        .tooltip(t!("terminal.find.close"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.close_find(window, cx)
                        })),
                )
                .into_any_element(),
        )
    }

    // ----- Context menu -----

    /// Right-click menu at `position`.
    fn open_context_menu(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let has_selection = self.model.has_selection();
        let writable = self.writable();
        let split = self.pane.is_some();
        let broadcasting = self.pane.is_some_and(|p| p.broadcasting);
        let focus = self.focus.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            menu.action_context(focus.clone())
                .min_w(px(220.))
                .menu_with_icon_and_disabled(
                    t!("terminal.menu.copy"),
                    IconName::Copy,
                    Box::new(Copy),
                    !has_selection,
                )
                .menu_with_icon_and_disabled(
                    t!("terminal.menu.paste"),
                    IconName::ClipboardPaste,
                    Box::new(Paste),
                    !writable,
                )
                .menu_with_disabled(
                    t!("terminal.menu.paste_selection"),
                    Box::new(PasteSelection),
                    !has_selection || !writable,
                )
                .separator()
                .menu(t!("terminal.menu.select_all"), Box::new(SelectAll))
                .menu_with_icon(t!("terminal.menu.find"), IconName::Search, Box::new(Find))
                .menu_with_icon(
                    t!("terminal.menu.clear"),
                    IconName::Eraser,
                    Box::new(ClearTerminal),
                )
                .separator()
                .menu_with_icon(
                    t!("terminal.menu.send_snippet"),
                    IconName::SquareTerminal,
                    Box::new(crate::app::SendSnippet),
                )
                .menu_with_icon(
                    t!("terminal.menu.add_pane"),
                    IconName::LayoutGrid,
                    Box::new(crate::app::AddPane),
                )
                .when(split, |menu| {
                    menu.menu_with_check(
                        t!("terminal.menu.broadcast"),
                        broadcasting,
                        Box::new(crate::app::ToggleBroadcast),
                    )
                    .menu(
                        t!("terminal.menu.focus_mode"),
                        Box::new(crate::app::ToggleFocusMode),
                    )
                    .menu(
                        t!("terminal.menu.close_pane"),
                        Box::new(crate::app::ClosePane),
                    )
                })
        });
        let sub = cx.subscribe_in(&menu, window, |this, _, _: &gpui::DismissEvent, _, cx| {
            this.context_menu = None;
            cx.notify();
        });
        let handle = menu.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
        self.context_menu = Some((menu, position, sub));
        cx.notify();
    }

    /// Right click (when the program does not use the mouse): the menu,
    /// paste or copy, as chosen in Settings.
    fn right_click(
        &mut self,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let setting = self.app.read(cx).settings.right_click;
        match paste::right_click_action(setting, self.model.has_selection()) {
            paste::RightClickAction::ShowMenu => self.open_context_menu(position, window, cx),
            paste::RightClickAction::Paste => self.paste(window, cx),
            paste::RightClickAction::Copy => {
                if let Some(text) = self.model.selection_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
                self.model.clear_selection();
                cx.notify();
            }
        }
    }

    /// Writes text in the terminal without pressing Enter (AI suggestions).
    pub fn insert_text(&mut self, text: &str, cx: &mut Context<Self>) {
        // (Not repeated in the other panes: it comes from the AI.)
        let bytes = input::paste_bytes(text, self.model.mode());
        self.write_input(bytes, cx);
    }

    // ----- Mouse -----

    fn cell_at(&self, pos: Point<Pixels>) -> (usize, usize, bool) {
        let x = (pos.x - self.origin.x).max(px(0.));
        let y = (pos.y - self.origin.y).max(px(0.));
        let fx = x / self.cell_width;
        let col = fx.floor() as usize;
        let row = (y / self.line_height).floor() as usize;
        (row, col, fx.fract() > 0.5)
    }

    /// Cell (column, row) for the reports, inside the grid.
    fn report_cell(&self, pos: Point<Pixels>) -> (usize, usize) {
        let (row, col, _) = self.cell_at(pos);
        (
            col.min(self.model.cols().saturating_sub(1) as usize),
            row.min(self.model.rows().saturating_sub(1) as usize),
        )
    }

    /// Do mouse events go to the program? Only if it asked for them, Shift is
    /// not held (it forces local selection) and the history is not being
    /// looked at.
    fn mouse_reporting(&self, modifiers: &Modifiers) -> bool {
        !modifiers.shift
            && mouse::reporting(self.model.mode())
            && self.model.display_offset() == 0
            && self.can_write
            && matches!(self.state, TermState::Running)
    }

    /// Sends a mouse report if the current mode allows it.
    fn report_mouse(
        &mut self,
        button: mouse::Button,
        action: mouse::Action,
        pos: Point<Pixels>,
        modifiers: &Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        let (col, row) = self.report_cell(pos);
        let report = mouse::Report {
            button,
            action,
            col,
            row,
            mods: mouse::Mods {
                shift: modifiers.shift,
                alt: modifiers.alt,
                ctrl: modifiers.control,
            },
        };
        match mouse::encode(&report, self.model.mode()) {
            Some(bytes) => {
                self.mouse_cell = Some((col, row));
                self.write(bytes, cx);
                true
            }
            None => false,
        }
    }

    fn mouse_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let button = match ev.button {
            MouseButton::Left => mouse::Button::Left,
            MouseButton::Middle => mouse::Button::Middle,
            MouseButton::Right => mouse::Button::Right,
            MouseButton::Navigate(_) => return,
        };
        // Ctrl+click (⌘+click on Mac) always opens the link, also in
        // programs that use the mouse (htop, vim...).
        if ev.button == MouseButton::Left && (ev.modifiers.platform || ev.modifiers.control) {
            let (row, col, _) = self.cell_at(ev.position);
            if let Some(url) = self.model.link_at(row, col) {
                cx.open_url(&url);
                return;
            }
        }
        if self.mouse_reporting(&ev.modifiers) {
            if self.report_mouse(button, mouse::Action::Press, ev.position, &ev.modifiers, cx) {
                self.mouse_held = Some((ev.button, button));
                if self.model.has_selection() {
                    self.model.clear_selection();
                    cx.notify();
                }
            }
            return;
        }
        match ev.button {
            MouseButton::Left => {
                let (row, col, right) = self.cell_at(ev.position);
                self.model.start_selection(row, col, right, ev.click_count);
                self.selecting = true;
                self.press_cell = (ev.click_count == 1).then_some((row, col));
                cx.notify();
            }
            MouseButton::Middle => self.paste(window, cx),
            MouseButton::Right => self.right_click(ev.position, window, cx),
            _ => {}
        }
    }

    /// Mouse motion anywhere in the window (registered by the element):
    /// `hovered` says whether it is over the terminal.
    pub(crate) fn mouse_move(
        &mut self,
        ev: &MouseMoveEvent,
        hovered: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some((_, button)) = self.mouse_held {
            // Drag reported to the program (modes 1002 and 1003).
            if self.mouse_cell != Some(self.report_cell(ev.position)) {
                self.report_mouse(
                    button,
                    mouse::Action::Motion,
                    ev.position,
                    &ev.modifiers,
                    cx,
                );
            }
            return;
        }
        if self.selecting {
            if ev.pressed_button == Some(MouseButton::Left) {
                let (row, col, right) = self.cell_at(ev.position);
                if self.press_cell != Some((row, col)) {
                    // It is a drag now: selection, not a click.
                    self.press_cell = None;
                }
                self.model.update_selection(row, col, right);
                cx.notify();
            }
            return;
        }
        // Link under the mouse: hand cursor.
        let link = if hovered {
            let (row, col, _) = self.cell_at(ev.position);
            self.model.link_at(row, col)
        } else {
            None
        };
        if link != self.hover_link {
            self.hover_link = link;
            cx.notify();
        }
        // Motion without a button (mode 1003).
        if hovered
            && ev.pressed_button.is_none()
            && self.mouse_reporting(&ev.modifiers)
            && self.mouse_cell != Some(self.report_cell(ev.position))
        {
            self.report_mouse(
                mouse::Button::None,
                mouse::Action::Motion,
                ev.position,
                &ev.modifiers,
                cx,
            );
        }
    }

    /// Button released anywhere in the window (registered by the element).
    pub(crate) fn mouse_up(&mut self, ev: &MouseUpEvent, cx: &mut Context<Self>) {
        if let Some((held, button)) = self.mouse_held
            && held == ev.button
        {
            self.mouse_held = None;
            self.report_mouse(
                button,
                mouse::Action::Release,
                ev.position,
                &ev.modifiers,
                cx,
            );
            return;
        }
        if self.selecting && ev.button == MouseButton::Left {
            self.selecting = false;
            // Click without drag on a link: it is opened.
            if let Some((row, col)) = self.press_cell.take()
                && self.cell_at(ev.position).0 == row
                && self.cell_at(ev.position).1 == col
                && let Some(url) = self.model.link_at(row, col)
            {
                self.model.clear_selection();
                cx.open_url(&url);
                cx.notify();
                return;
            }
            if !self.model.has_selection() {
                self.model.clear_selection();
            } else if self.app.read(cx).settings.copy_on_select
                && let Some(text) = self.model.selection_text()
            {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            }
            cx.notify();
        }
    }

    fn scroll_wheel(&mut self, ev: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = ev.delta.pixel_delta(self.line_height);
        let lines = delta.y / self.line_height + self.scroll_rest;
        let whole = lines.trunc();
        self.scroll_rest = lines - whole;
        let n = whole as i32;
        if n == 0 {
            return;
        }
        let mode = self.model.mode();
        if self.mouse_reporting(&ev.modifiers) {
            // Wheel for the program: one report per line.
            let button = if n > 0 {
                mouse::Button::WheelUp
            } else {
                mouse::Button::WheelDown
            };
            for _ in 0..n.unsigned_abs() {
                self.report_mouse(button, mouse::Action::Press, ev.position, &ev.modifiers, cx);
            }
        } else if !ev.modifiers.shift
            && mode.contains(TermMode::ALT_SCREEN)
            && mode.contains(TermMode::ALTERNATE_SCROLL)
        {
            // Full screen without mouse (less, man...): arrows, like xterm.
            let seq: &[u8] = if n > 0 { b"\x1bOA" } else { b"\x1bOB" };
            let bytes: Vec<u8> = std::iter::repeat_n(seq, n.unsigned_abs() as usize)
                .flatten()
                .copied()
                .collect();
            self.write(bytes, cx);
        } else {
            self.model.scroll(n);
            cx.notify();
        }
    }

    // ----- Sharing -----

    /// Stops sharing the terminal of this computer (`revoke`: revoking
    /// every share first, so guests read "sharing stopped").
    fn stop_relay(&mut self, revoke: bool, cx: &mut Context<Self>) {
        self.copilot_shared = false;
        if let Some((_, closed)) = self.local_feed.take() {
            let _ = closed.send(true);
        }
        if let Some(relay) = self.relay.take() {
            if !matches!(self.kind, TermKind::Server { .. }) {
                self.share_session = None;
                let done: Vec<_> = self.share.announced.drain().collect();
                for (kind, participant) in done {
                    cx.emit(TerminalEvent::ShareRequestDone { kind, participant });
                }
                self.share = ShareState::default();
            }
            runtime::handle(cx).spawn(async move {
                if let Some(r) = relay.lock().await.take() {
                    if revoke {
                        r.stop_guests().await;
                    }
                    r.stop().await;
                }
            });
        }
    }

    /// Shares the terminal through the server.
    pub fn share(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.account_api(cx) else {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                t!("terminal.share.not_signed_in"),
            );
            return;
        };
        if self.share_session.is_some() {
            // Only the owner shares (a guest sees the participants instead).
            if self.is_share_owner() {
                self.open_share_dialog(window, cx);
            }
            return;
        }
        let Some(term) = self.local.clone() else {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                t!("terminal.share.not_connected"),
            );
            return;
        };
        let title = self.label(cx);
        runtime::run_in(
            cx,
            window,
            async move {
                RelayShare::start(&api, term, &title)
                    .await
                    .map_err(api_error)
            },
            |this, res, window, cx| match res {
                Ok(share) => {
                    this.share_session = Some(share.session_id);
                    this.watch_relay(&share, cx);
                    this.relay = Some(Arc::new(tokio::sync::Mutex::new(Some(share))));
                    cx.notify();
                    this.open_share_dialog(window, cx);
                }
                Err(e) => ui::error(window, cx, t!("terminal.share.failed", error = e)),
            },
        );
    }

    fn open_share_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session_id) = self.share_session else {
            return;
        };
        let weak = cx.entity().downgrade();
        let is_relay = self.relay.is_some();
        let api = self.account_api(cx);
        crate::views::share::open(
            self.app.clone(),
            api,
            session_id,
            Some(weak),
            is_relay,
            window,
            cx,
        );
    }

    /// For the copilot: shares the terminal with the server (only for you) so
    /// the AI can read it and write to it. `None` if it is not needed (server
    /// session or already shared) or not possible (not signed in or not
    /// connected).
    pub fn copilot_share(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<impl std::future::Future<Output = termoak_client::Result<RelayShare>> + use<>> {
        if self.share_session.is_some() || matches!(self.kind, TermKind::Server { .. }) {
            return None;
        }
        if !matches!(self.state, TermState::Running) {
            return None;
        }
        let api = self.account_api(cx)?;
        let title = t!("terminal.copilot_session_title", host = self.label(cx)).to_string();
        let source = match &self.kind {
            TermKind::Local { .. } => Err(self.local.clone()?),
            _ => {
                // Shell or serial: the output is copied here when painted
                // (starting with what is already on screen) and what comes
                // from the server is typed into the terminal.
                let backend = self.backend.clone()?;
                let hub = Arc::new(OutputHub::new(256 * 1024));
                hub.push(bytes::Bytes::from(
                    self.model.screen_text().replace('\n', "\r\n") + "\r\n",
                ));
                let (input, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
                runtime::handle(cx).spawn(async move {
                    while let Some(data) = input_rx.recv().await {
                        backend.input(data);
                    }
                });
                let (closed_tx, closed) = tokio::sync::watch::channel(false);
                self.local_feed = Some((hub.clone(), closed_tx));
                Ok(LocalTerm {
                    hub,
                    input,
                    size: (self.model.cols(), self.model.rows()),
                    closed,
                })
            }
        };
        Some(async move {
            match source {
                Err(term) => RelayShare::start(&api, term, &title).await,
                Ok(local) => RelayShare::start_local(&api, local, &title).await,
            }
        })
    }

    /// The copilot has shared the terminal.
    pub fn adopt_share(&mut self, share: RelayShare, cx: &mut Context<Self>) {
        self.share_session = Some(share.session_id);
        self.watch_relay(&share, cx);
        self.relay = Some(Arc::new(tokio::sync::Mutex::new(Some(share))));
        self.copilot_shared = true;
        cx.notify();
    }

    /// The copilot no longer needs it (it was closed or stopped): sharing
    /// stops if the copilot shared it. What the user shared stays.
    pub fn stop_copilot_share(&mut self, cx: &mut Context<Self>) {
        if self.copilot_shared {
            self.stop_relay(false, cx);
            cx.notify();
        }
    }

    /// Could not share for the copilot: left as it was.
    pub fn forget_share(&mut self, cx: &mut Context<Self>) {
        if self.relay.is_none() {
            self.local_feed = None;
        }
        cx.notify();
    }

    /// Stops sharing the terminal (asked by the share dialog).
    pub(crate) fn stop_sharing(&mut self, cx: &mut Context<Self>) {
        self.stop_relay(false, cx);
    }

    // ----- AI -----

    /// With "This computer" in Settings → AI, what the quick assistant runs
    /// with here (the vault and the AI settings); `None` = the server.
    fn ai_local(&self, cx: &App) -> Option<(termoak_core::Store, crate::local_ai::AiSettings)> {
        let app = self.app.read(cx);
        (app.ai_run_on() == crate::local_ai::RunOn::Local)
            .then(|| (app.ws.store.clone(), app.settings.ai.clone()))
    }

    fn ai_context(&self, cx: &App) -> Value {
        let os = match &self.kind {
            TermKind::Local { host_id } => {
                self.app.read(cx).host(*host_id).and_then(|h| h.os.clone())
            }
            TermKind::Server {
                host_id: Some(h), ..
            } => self.app.read(cx).host(*h).and_then(|h| h.os.clone()),
            TermKind::Shell => Some(local_os()),
            _ => None,
        };
        let screen = self.model.screen_text();
        let tail: String = {
            let chars: Vec<char> = screen.chars().collect();
            chars[chars.len().saturating_sub(3000)..].iter().collect()
        };
        json!({"os": os, "screen": tail})
    }

    /// Explains the selection (or the visible screen) with the server AI.
    pub fn ai_explain(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let local = self.ai_local(cx);
        let api = self.account_api(cx);
        if local.is_none() && api.is_none() {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                t!("terminal.ai.not_signed_in"),
            );
            return;
        }
        let text = self
            .model
            .selection_text()
            .unwrap_or_else(|| self.model.screen_text());
        if text.trim().is_empty() {
            ui::notify(
                window,
                cx,
                ToastKind::Info,
                t!("terminal.ai.nothing_to_explain"),
            );
            return;
        }
        let text: String = {
            let chars: Vec<char> = text.chars().collect();
            chars[chars.len().saturating_sub(6000)..].iter().collect()
        };
        let context = self.ai_context(cx);
        self.ai_busy = true;
        cx.notify();
        ui::notify(window, cx, ToastKind::Info, t!("terminal.ai.asking"));
        runtime::run_in(
            cx,
            window,
            async move {
                match (local, api) {
                    (Some((store, settings)), _) => {
                        crate::local_ai::copilot::explain(&store, &settings, &text, context).await
                    }
                    (None, Some(api)) => api
                        .post::<Value>(
                            "/api/v1/ai/explain",
                            &json!({"text": text, "context": context}),
                        )
                        .await
                        .map_err(api_error),
                    (None, None) => Err(t!("terminal.ai.not_signed_in").to_string()),
                }
            },
            |this, res, window, cx| {
                this.ai_busy = false;
                cx.notify();
                match res {
                    Ok(v) => {
                        let answer: SharedString = v["answer"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| t!("terminal.ai.no_answer").to_string())
                            .into();
                        let provider = v["provider"].as_str().unwrap_or("").to_string();
                        window.open_dialog(cx, move |d, _, cx| {
                            d.title(t!("terminal.ai.explanation_title"))
                                .w(px(640.))
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .max_h(px(460.))
                                        .child(
                                            TextView::markdown("ai-explain", answer.clone())
                                                .selectable(true),
                                        )
                                        .when(!provider.is_empty(), |this| {
                                            this.child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(t!(
                                                        "terminal.ai.provider",
                                                        provider = provider
                                                    )),
                                            )
                                        }),
                                )
                                .footer(
                                    h_flex().w_full().justify_end().child(
                                        Button::new("ai-close")
                                            .label(t!("common.close"))
                                            .on_click(|_, window, cx| window.close_dialog(cx)),
                                    ),
                                )
                        });
                    }
                    Err(e) => ui::error(window, cx, t!("terminal.ai.failed", error = e)),
                }
            },
        );
    }

    /// Asks the AI for a command and inserts it without running it.
    pub fn ai_suggest(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ai_local(cx).is_none() && self.account_api(cx).is_none() {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                t!("terminal.ai.not_signed_in"),
            );
            return;
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("terminal.ai.suggest_placeholder"))
        });
        ui::focus_later(&input, window, cx);
        let weak = cx.entity().downgrade();
        let input_ok = input.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("terminal.ai.which_command"),
            t!("terminal.ai.suggest"),
            520.,
            move |_, cx| {
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("terminal.ai.suggest_hint")),
                    )
                    .child(Input::new(&input))
                    .into_any_element()
            },
            move |window, cx| {
                let request = input_ok.read(cx).value().trim().to_string();
                if request.is_empty() {
                    return false;
                }
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |v, cx| v.request_suggestion(request, window, cx));
                }
                true
            },
        );
    }

    fn request_suggestion(&mut self, request: String, window: &mut Window, cx: &mut Context<Self>) {
        let local = self.ai_local(cx);
        let api = self.account_api(cx);
        if local.is_none() && api.is_none() {
            return;
        }
        let context = self.ai_context(cx);
        self.ai_busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                match (local, api) {
                    (Some((store, settings)), _) => {
                        crate::local_ai::copilot::suggest(&store, &settings, &request, context)
                            .await
                    }
                    (None, Some(api)) => api
                        .post::<Value>(
                            "/api/v1/ai/suggest",
                            &json!({"request": request, "context": context}),
                        )
                        .await
                        .map_err(api_error),
                    (None, None) => Err(t!("terminal.ai.not_signed_in").to_string()),
                }
            },
            |this, res, window, cx| {
                this.ai_busy = false;
                cx.notify();
                match res {
                    Ok(v) => this.show_suggestion(v, window, cx),
                    Err(e) => ui::error(window, cx, t!("terminal.ai.failed", error = e)),
                }
            },
        );
    }

    fn show_suggestion(&mut self, v: Value, window: &mut Window, cx: &mut Context<Self>) {
        let command: SharedString = v["command"].as_str().unwrap_or("").to_string().into();
        if command.trim().is_empty() {
            ui::notify(window, cx, ToastKind::Warning, t!("terminal.ai.no_command"));
            return;
        }
        let explanation: SharedString = v["explanation"].as_str().unwrap_or("").to_string().into();
        let risk = v["risk"].as_str().unwrap_or("read").to_string();
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |d, _, cx| {
            let (risk_label, risk_color) = match risk.as_str() {
                "dangerous" => (t!("terminal.ai.risk_dangerous"), cx.theme().danger),
                "write" => (t!("terminal.ai.risk_write"), cx.theme().warning),
                _ => (t!("terminal.ai.risk_read"), cx.theme().success),
            };
            let insert_cmd = command.clone();
            let copy_cmd = command.clone();
            let weak = weak.clone();
            d.title(t!("terminal.ai.suggested_title"))
                .w(px(600.))
                .child(
                    v_flex()
                        .gap_3()
                        .child(
                            div()
                                .p_3()
                                .rounded(cx.theme().radius)
                                .bg(cx.theme().muted)
                                .font_family(ui::mono_family(cx))
                                .text_sm()
                                .child(command.clone()),
                        )
                        .child(h_flex().gap_2().child(ui::pill(risk_label, risk_color)))
                        .when(!explanation.is_empty(), |this| {
                            this.child(explanation.clone())
                        }),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("sugg-copy").label(t!("common.copy")).on_click(
                            move |_, window, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_cmd.to_string(),
                                ));
                                window.close_dialog(cx);
                            },
                        ))
                        .child(
                            Button::new("sugg-insert")
                                .primary()
                                .label(t!("terminal.ai.insert"))
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    if let Some(view) = weak.upgrade() {
                                        view.update(cx, |v, cx| {
                                            v.insert_text(&insert_cmd, cx);
                                            v.focus.focus(window, cx);
                                        });
                                    }
                                }),
                        ),
                )
        });
    }

    // ----- Painting -----

    /// Dark or light terminal colors: the host's choice (host editor →
    /// terminal theme) or the app theme.
    fn dark_palette(&self, cx: &App) -> bool {
        let host_theme = self
            .host_id()
            .and_then(|id| self.app.read(cx).host(id))
            .and_then(|h| h.settings.theme.clone());
        match host_theme.as_deref() {
            Some("dark") => true,
            Some("light") => false,
            _ => cx.theme().is_dark(),
        }
    }

    fn terminal_font(&self, cx: &App) -> (Font, Pixels) {
        let settings = &self.app.read(cx).settings;
        let family: SharedString = if settings.font_family.trim().is_empty() {
            cx.theme().mono_font_family.clone()
        } else {
            settings.font_family.trim().to_string().into()
        };
        (
            Font {
                family,
                features: FontFeatures::disable_ligatures(),
                fallbacks: None,
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
            },
            px(settings.font_size.clamp(8., 32.)),
        )
    }

    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (dot, status): (gpui::Hsla, String) = match &self.state {
            TermState::Connecting(m) => (theme.warning, m.clone()),
            TermState::Running => (theme.success, String::new()),
            TermState::Closed(r) => (
                theme.muted_foreground,
                r.clone()
                    .unwrap_or_else(|| t!("terminal.status.session_closed").to_string()),
            ),
            TermState::Failed(e) => (theme.danger, e.clone()),
        };
        let running = matches!(self.state, TermState::Running);
        let ended = matches!(self.state, TermState::Closed(_) | TermState::Failed(_));
        let logged_in = self.app.read(cx).logged_in();
        // The quick assistant runs on the server or on this computer.
        let ai_ready = logged_in || self.ai_local(cx).is_some();
        let is_server = matches!(self.kind, TermKind::Server { .. });
        let is_shell = matches!(self.kind, TermKind::Shell);
        let local_host = match &self.kind {
            TermKind::Local { host_id } => Some(*host_id),
            _ => None,
        };
        let weak: WeakEntity<Self> = cx.entity().downgrade();
        // In a split view the panes are narrow: icons only, and copy/paste
        // stay in the right-click menu.
        let pane = self.pane;
        let compact = pane.is_some();
        let participants = self.render_participants(cx);
        let latency_badge = self.latency_badge(cx);
        let theme = cx.theme();
        // You own what this tab shows: sharing and ending it are yours.
        let owner = !is_server || self.is_share_owner();
        // Sent away for good: coming back makes no sense.
        let can_reconnect = self.share.ended.is_none() || self.share.owner;
        let (kind_label, kind_color) = match self.kind {
            TermKind::Local { .. } => (t!("terminal.kind.local"), theme.primary),
            TermKind::Server { .. } => (t!("terminal.kind.server"), theme.info),
            TermKind::Shell => (t!("terminal.kind.shell"), theme.success),
            TermKind::Serial { .. } => (t!("terminal.kind.serial"), theme.warning),
        };

        h_flex()
            .h(px(34.))
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            .child(
                // In a split view the name is a handle: dragged to the tab
                // bar it becomes a tab of its own, onto another terminal it
                // moves there.
                h_flex()
                    .id("pane-handle")
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .when(compact, |this| {
                        let dragged = DraggedPane {
                            terminal: cx.entity_id(),
                            title: self.label(cx).into(),
                        };
                        this.cursor_grab()
                            .child(
                                ui::icon(IconName::GripVertical)
                                    .size(px(14.))
                                    .text_color(theme.muted_foreground),
                            )
                            .on_drag(dragged, |d, _, _, cx| {
                                cx.new(|_| {
                                    DragPreview::new(d.title.clone(), IconName::SquareTerminal)
                                })
                            })
                    })
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(self.label(cx)),
                    ),
            )
            .when(!compact, |this| {
                this.child(ui::pill(kind_label, kind_color))
            })
            // Also in a split view: it is short.
            .when_some(latency_badge, |this, (text, color, tooltip)| {
                this.child(
                    ui::pill(text, color)
                        .id("latency")
                        .flex_none()
                        .whitespace_nowrap()
                        .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx)),
                )
            })
            .when(
                self.share_session.is_some() && self.relay.is_some() && !compact,
                |this| this.child(ui::pill(t!("terminal.shared"), theme.success)),
            )
            .when(self.share.relay_offline, |this| {
                this.child(ui::pill(t!("share.relay_reconnecting"), theme.warning))
            })
            // Servers before live sharing only say how many are connected.
            .when(participants.is_none() && self.viewers > 1, |this| {
                this.child(ui::pill(tn!("terminal.viewers", self.viewers), theme.info))
            })
            .when(
                !self.can_write && running && is_server && !self.share.known,
                |this| this.child(ui::pill(t!("terminal.read_only"), theme.warning)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .whitespace_nowrap()
                    .child(status),
            )
            .children(participants)
            .when(ended && can_reconnect, |this| {
                this.child(
                    Button::new("reconnect")
                        .small()
                        .primary()
                        .icon(ui::icon(IconName::RefreshCw))
                        .when(!compact, |b| {
                            b.label(if is_shell {
                                t!("terminal.reopen")
                            } else {
                                t!("terminal.reconnect")
                            })
                        })
                        .tooltip(if is_shell {
                            t!("terminal.reopen")
                        } else {
                            t!("terminal.reconnect")
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.connect(window, cx);
                        })),
                )
            })
            .when_some(local_host, |this, host_id| {
                let conn = self.local.as_ref().map(|t| t.connection().clone());
                this.child(
                    Button::new("sftp")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::FolderOpen))
                        .when(!compact, |b| b.label("SFTP"))
                        .tooltip(t!("terminal.open_sftp"))
                        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                            cx.emit(TerminalEvent::OpenSftp {
                                host_id,
                                conn: conn.clone(),
                            });
                        })),
                )
            })
            .when(!compact, |this| {
                this.child(
                    Button::new("copy")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::Copy))
                        .tooltip(t!("terminal.copy_selection"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.copy_selection(window, cx);
                        })),
                )
                .child(
                    Button::new("paste")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::ClipboardPaste))
                        .tooltip(t!("terminal.paste"))
                        .disabled(!running)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.paste(window, cx);
                            this.focus.focus(window, cx);
                        })),
                )
            })
            .child({
                let w1 = weak.clone();
                let w2 = weak.clone();
                Button::new("ai")
                    .small()
                    .ghost()
                    .icon(ui::icon(IconName::Sparkles))
                    .when(!compact, |b| b.label(t!("terminal.ai.button")))
                    .loading(self.ai_busy)
                    .disabled(!ai_ready)
                    .tooltip(if ai_ready {
                        t!("terminal.ai.tooltip")
                    } else {
                        t!("terminal.ai.tooltip_signed_out")
                    })
                    .dropdown_menu(move |menu, _, _| {
                        let w1 = w1.clone();
                        let w2 = w2.clone();
                        menu.item(
                            PopupMenuItem::new(t!("terminal.ai.explain"))
                                .icon(ui::icon(IconName::BookOpen))
                                .on_click(move |_, window, cx| {
                                    if let Some(v) = w1.upgrade() {
                                        v.update(cx, |v, cx| v.ai_explain(window, cx));
                                    }
                                }),
                        )
                        .item(
                            PopupMenuItem::new(t!("terminal.ai.which_command"))
                                .icon(ui::icon(IconName::Wand))
                                .on_click(move |_, window, cx| {
                                    if let Some(v) = w2.upgrade() {
                                        v.update(cx, |v, cx| v.ai_suggest(window, cx));
                                    }
                                }),
                        )
                    })
            })
            // Sharing goes through the SSH session (relay) or the server one;
            // the local shell is not shared.
            .when(!is_shell && owner, |this| {
                this.child(
                    Button::new("share")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::Share2))
                        .when(!compact, |b| b.label(t!("terminal.share.button")))
                        .disabled(!running || !logged_in)
                        .tooltip(if logged_in {
                            t!("terminal.share.tooltip")
                        } else {
                            t!("terminal.share.tooltip_signed_out")
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.share(window, cx);
                        })),
                )
            })
            .when(is_server && running && owner, |this| {
                this.child(
                    Button::new("close-server-session")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::CircleStop))
                        .tooltip(t!("terminal.end_session.tooltip"))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            let weak = cx.entity().downgrade();
                            ui::confirm(
                                window,
                                cx,
                                t!("terminal.end_session.title"),
                                t!("terminal.end_session.message"),
                                t!("terminal.end_session.confirm"),
                                true,
                                move |_, cx| {
                                    if let Some(v) = weak.upgrade() {
                                        v.update(cx, |v, _| {
                                            if let Some(b) = &v.backend {
                                                b.send(Cmd::CloseSession);
                                            }
                                        });
                                    }
                                },
                            );
                            let _ = this;
                        })),
                )
            })
            .when_some(pane, |this, pane| {
                this.child(div().w(px(1.)).h(px(16.)).bg(theme.border))
                    .when(pane.broadcasting, |this| {
                        this.child(
                            Button::new("pane-include")
                                .small()
                                .map(|b| {
                                    if pane.included {
                                        b.warning()
                                    } else {
                                        b.ghost()
                                    }
                                })
                                .icon(ui::icon(IconName::RadioTower))
                                .tooltip(if pane.included {
                                    t!("terminal.pane.exclude")
                                } else {
                                    t!("terminal.pane.include")
                                })
                                .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                    cx.emit(TerminalEvent::Pane(PaneAction::ToggleInclude))
                                })),
                        )
                    })
                    .child(
                        Button::new("pane-maximize")
                            .small()
                            .ghost()
                            .icon(ui::icon(if pane.maximized {
                                IconName::Minimize2
                            } else {
                                IconName::Maximize2
                            }))
                            .tooltip(if pane.maximized {
                                t!("terminal.pane.restore")
                            } else {
                                t!("terminal.pane.maximize")
                            })
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(TerminalEvent::Pane(PaneAction::ToggleMaximize))
                            })),
                    )
                    .child(
                        Button::new("pane-close")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::X))
                            .tooltip(t!("terminal.pane.close"))
                            .on_click(cx.listener(|_, _: &ClickEvent, _, cx| {
                                cx.emit(TerminalEvent::Pane(PaneAction::Close))
                            })),
                    )
            })
    }

    fn render_overlay(&mut self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let theme = cx.theme();
        let failed_title = if matches!(self.kind, TermKind::Serial { .. }) {
            t!("terminal.failed.serial")
        } else if matches!(self.kind, TermKind::Shell) {
            t!("terminal.failed.shell")
        } else {
            t!("terminal.failed.connect")
        };
        match &self.state {
            TermState::Connecting(msg) => Some(
                v_flex()
                    .absolute()
                    .inset_0()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(Spinner::new().large())
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(msg.clone()),
                    )
                    .into_any_element(),
            ),
            TermState::Failed(e) => Some(
                v_flex()
                    .absolute()
                    .inset_0()
                    .items_center()
                    .justify_center()
                    .child(
                        ui::card(cx)
                            .p_6()
                            .gap_3()
                            .max_w(px(560.))
                            .items_center()
                            .child(
                                ui::icon(IconName::TriangleAlert)
                                    .size(px(32.))
                                    .text_color(theme.danger),
                            )
                            .child(div().text_lg().font_semibold().child(failed_title))
                            .child(
                                div()
                                    .text_sm()
                                    .text_center()
                                    .text_color(theme.muted_foreground)
                                    .child(e.clone()),
                            )
                            .child(
                                Button::new("retry")
                                    .primary()
                                    .icon(ui::icon(IconName::RefreshCw))
                                    .label(t!("common.retry"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.connect(window, cx);
                                    })),
                            ),
                    )
                    .into_any_element(),
            ),
            _ => None,
        }
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = TermPalette::for_mode(self.dark_palette(cx));
        let (font, font_size) = self.terminal_font(cx);
        let _ = window;
        self.poll_latency(cx);
        let toolbar = self.render_toolbar(cx);
        let banners = self.render_share_banners(cx);
        let overlay = self
            .render_waiting(cx)
            .or_else(|| self.render_ended(cx))
            .or_else(|| self.render_overlay(cx));
        let suggestions = self.render_suggestions(cx);
        self.update_find_highlights();
        let find = self.render_find(cx);
        let context_menu = self.context_menu.as_ref().map(|(menu, position, _)| {
            gpui::deferred(
                gpui::anchored()
                    .position(*position)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu.clone()),
            )
            .with_priority(1)
        });
        let entity = cx.entity();
        v_flex()
            .size_full()
            .bg(palette.background)
            .child(toolbar)
            .children(banners)
            .child(
                div()
                    .id("terminal-area")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_hidden()
                    .key_context(CONTEXT)
                    .track_focus(&self.focus)
                    .cursor(if self.hover_link.is_some() {
                        CursorStyle::PointingHand
                    } else {
                        CursorStyle::IBeam
                    })
                    .on_key_down(cx.listener(Self::on_key_down))
                    .on_action(cx.listener(Self::on_send_text))
                    .on_action(cx.listener(Self::on_copy))
                    .on_action(cx.listener(Self::on_paste))
                    .on_action(cx.listener(Self::on_select_all))
                    .on_action(cx.listener(Self::on_page_up))
                    .on_action(cx.listener(Self::on_page_down))
                    .on_action(cx.listener(Self::on_bottom))
                    .on_action(cx.listener(Self::on_paste_selection))
                    .on_action(cx.listener(Self::on_find))
                    .on_action(cx.listener(Self::on_find_next))
                    .on_action(cx.listener(Self::on_find_previous))
                    .on_action(cx.listener(Self::on_clear))
                    // Motion and release are registered by the element for the whole window.
                    .on_any_mouse_down(cx.listener(Self::mouse_down))
                    .on_scroll_wheel(cx.listener(Self::scroll_wheel))
                    .child(TerminalElement::new(
                        entity,
                        self.focus.clone(),
                        font,
                        font_size,
                        palette,
                    ))
                    .children(suggestions)
                    .children(overlay)
                    .children(find)
                    .children(context_menu),
            )
    }
}

/// Platform text input: dead keys, compose sequences, AltGr and IME (Chinese,
/// Japanese, Korean...). Committed text goes to the terminal; text being
/// composed is painted underlined at the cursor until it is committed or
/// cancelled.
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        // Only the composition text is known.
        let units: Vec<u16> = self.ime_marked.as_deref()?.encode_utf16().collect();
        let start = range.start.min(units.len());
        let end = range.end.clamp(start, units.len());
        *adjusted_range = Some(start..end);
        Some(String::from_utf16_lossy(&units[start..end]))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        // The IME "cursor" is at the end of the composition text.
        let caret = self.ime_marked.as_deref().map_or(0, input::utf16_len);
        Some(UTF16Selection {
            range: caret..caret,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.ime_marked
            .as_deref()
            .map(|text| 0..input::utf16_len(text))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.ime_marked.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_marked = None;
        if !text.is_empty() {
            self.commit_text(text, cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ime_marked = (!new_text.is_empty()).then(|| new_text.to_string());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Cursor cell, moved to the requested position inside the composition
        // text: the IME candidate window appears there.
        let (row, col) = self.model.cursor_cell()?;
        let offset = self
            .ime_marked
            .as_deref()
            .map_or(0, |text| input::cells_before(text, range_utf16.start));
        Some(Bounds::new(
            point(
                self.origin.x + self.cell_width * (col + offset) as f32,
                self.origin.y + self.line_height * row as f32,
            ),
            size(self.cell_width, self.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Operating system of this computer (context for the AI).
fn local_os() -> String {
    match std::env::consts::OS {
        "linux" => "Linux".into(),
        "macos" => "macOS".into(),
        "windows" => "Windows".into(),
        other => other.to_string(),
    }
}
