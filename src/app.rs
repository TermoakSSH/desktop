//! Main window, in the style of Termius: title bar with tabs (terminals and
//! SFTP), sidebar with the sections and the content of the active section or
//! tab. To the right of the terminal, the AI copilot. At startup, the running
//! server sessions show up as dormant tabs, which attach when clicked.
//!
//! A terminal tab is a workspace: one terminal or several in a grid (split
//! view), with focus mode (one big, the others small) and broadcast input
//! (what is typed in one pane goes to all of them). See `panes.rs`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    AnyElement, App, AppContext, Bounds, ClickEvent, Context, DragMoveEvent, Entity, EntityId,
    FocusHandle, Focusable, InteractiveElement, IntoElement, KeyBinding, MouseButton,
    ParentElement, Pixels, Point, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, actions, div, prelude::FluentBuilder, px, relative,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{ContextMenuExt, DropdownMenu, PopupMenu, PopupMenuItem};
use gpui_component::notification::Notification;
use gpui_component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_component::{ActiveTheme, Root, Sizable, StyledExt, TitleBar, WindowExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_ssh::Connection;

use crate::accounts::{self, SwitcherEntry, VaultFilter, ViewMode};
use crate::app_lock::{self, AppLock};
use crate::drag::{self, DragPreview, DraggedPane, DraggedTab};
use crate::local_ai::copilot::{LocalAi, LocalAiGlobal};
use crate::local_ai::terminals::LocalTerminals;
use crate::menus::{self, MenuState};
use crate::notifications::{self, AiNotice, Category, Notice, Target};
use crate::panes::{self, Dir, MAX_PANES, Zone};
use crate::prompts::PromptRequest;
use crate::runtime;
use crate::sharing::{self, SessionNotice};
use crate::state::{self as app_state, AppModel, ModelEvent, ToastKind};
use crate::terminal::{
    CopilotRequest, PaneAction, PaneChrome, RequestKind, ShareRequest, TermKind, TermState,
    TerminalEvent, TerminalView, serial::SerialParams,
};
use crate::ui::{self, IconName};
use crate::update::{self, UpdateEvent, UpdateModel, UpdateStatus};
use crate::views::OpenRequest;
use crate::views::accounts::avatar;
use crate::views::add_account;
use crate::views::admin::AdminView;
use crate::views::ai::AiView;
use crate::views::ai_chat::{AiChat, AiChatEvent};
use crate::views::forwards::ForwardsView;
use crate::views::host_picker::{HostPicker, PickMode};
use crate::views::hosts::HostsView;
use crate::views::keychain::KeychainView;
use crate::views::known_hosts::KnownHostsView;
use crate::views::server_sessions::ServerSessionsView;
use crate::views::settings::{SettingsPage, SettingsView};
use crate::views::sftp::SftpView;
use crate::views::snippets::SnippetsView;
use crate::views::teams::TeamsView;
use crate::views::vaults;
use crate::windows;
use crate::workspaces::{
    self as saved, Layout, Mode, PlanPane, PlanTab, SavedPane, SavedTab, Workspaces,
};

mod command_notice;
mod palette;

actions!(
    termoak,
    [
        NewTab,
        /// Another window on the same data, with its own tabs.
        NewWindow,
        NewLocalTerminal,
        CloseTab,
        NextTab,
        PrevTab,
        GoHome,
        ToggleCopilot,
        Quit,
        About,
        OpenSettings,
        CheckForUpdates,
        Hide,
        HideOthers,
        ShowAll,
        NewHost,
        QuickConnect,
        /// Closes the focused pane of a split view.
        ClosePane,
        ToggleSidebar,
        /// Adds a terminal to the current tab (split view).
        AddPane,
        /// Focus mode: the focused pane big and the others small.
        ToggleFocusMode,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        ToggleFullScreen,
        /// Sends what is typed in the focused pane to every pane.
        ToggleBroadcast,
        SendSnippet,
        Reconnect,
        OpenSftp,
        DuplicateSession,
        Minimize,
        OpenDocs,
        ReportIssue,
        ShowShortcuts,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        /// Moves the active tab one place to the left.
        MoveTabLeft,
        /// Moves the active tab one place to the right.
        MoveTabRight,
        /// Takes the focused pane out of the split view into a tab of its own.
        PaneToNewTab,
        /// Command palette: search hosts, tabs, snippets, sessions and actions.
        CommandPalette,
        /// Saves the tabs of the window as a named workspace.
        SaveWorkspace,
        /// Chooses a saved workspace to open.
        OpenWorkspace
    ]
);

const CONTEXT: &str = "Workspace";

/// Toasts of requests from a shared terminal tab (one per request).
struct ShareRequestToast;

/// Toasts of notices about server sessions you are not watching.
struct SessionNoticeToast;

/// Key of the toast of a request.
fn request_key(kind: RequestKind, participant: Id) -> SharedString {
    format!("{kind:?}-{participant}").into()
}

/// Default terminal font size (View → Actual size).
const DEFAULT_FONT_SIZE: f32 = 14.;

/// Global shortcuts of the window.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-k", CommandPalette, Some(CONTEXT)),
        KeyBinding::new("cmd-t", NewTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-t", NewLocalTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("cmd-alt-w", ClosePane, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-]", NextTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-[", PrevTab, Some(CONTEXT)),
        KeyBinding::new("cmd-1", GoHome, Some(CONTEXT)),
        KeyBinding::new("cmd-i", ToggleCopilot, Some(CONTEXT)),
        KeyBinding::new("cmd-,", OpenSettings, Some(CONTEXT)),
        KeyBinding::new("cmd-n", NewWindow, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-n", NewHost, Some(CONTEXT)),
        KeyBinding::new("cmd-d", AddPane, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-m", ToggleFocusMode, Some(CONTEXT)),
        KeyBinding::new("cmd-b", ToggleBroadcast, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-s", SendSnippet, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-r", Reconnect, Some(CONTEXT)),
        KeyBinding::new("cmd-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("cmd-+", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("cmd--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("cmd-0", ZoomReset, Some(CONTEXT)),
        KeyBinding::new("ctrl-cmd-f", ToggleFullScreen, Some(CONTEXT)),
        KeyBinding::new("cmd-alt-left", FocusPaneLeft, Some(CONTEXT)),
        KeyBinding::new("cmd-alt-right", FocusPaneRight, Some(CONTEXT)),
        KeyBinding::new("cmd-alt-up", FocusPaneUp, Some(CONTEXT)),
        KeyBinding::new("cmd-alt-down", FocusPaneDown, Some(CONTEXT)),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-/", ShowShortcuts, Some(CONTEXT)),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    #[cfg(not(target_os = "macos"))]
    cx.bind_keys([
        // In the terminal Ctrl+K belongs to the shell: there the palette
        // opens with Ctrl+Shift+P (see `terminal::init`).
        KeyBinding::new("ctrl-k", CommandPalette, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-p", CommandPalette, Some(CONTEXT)),
        KeyBinding::new("ctrl-t", NewTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-t", NewLocalTerminal, Some(CONTEXT)),
        // In the terminal, Ctrl+W belongs to the shell: there tabs close with Ctrl+Shift+W.
        KeyBinding::new("ctrl-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-h", GoHome, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-i", ToggleCopilot, Some(CONTEXT)),
        KeyBinding::new("ctrl-,", OpenSettings, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-n", NewHost, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-d", AddPane, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-m", ToggleFocusMode, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-b", ToggleBroadcast, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-s", SendSnippet, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-r", Reconnect, Some(CONTEXT)),
        KeyBinding::new("ctrl-=", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl-+", ZoomIn, Some(CONTEXT)),
        KeyBinding::new("ctrl--", ZoomOut, Some(CONTEXT)),
        KeyBinding::new("ctrl-0", ZoomReset, Some(CONTEXT)),
        KeyBinding::new("f11", ToggleFullScreen, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-left", FocusPaneLeft, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-right", FocusPaneRight, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-up", FocusPaneUp, Some(CONTEXT)),
        KeyBinding::new("ctrl-alt-down", FocusPaneDown, Some(CONTEXT)),
    ]);
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-pageup", MoveTabLeft, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-pagedown", MoveTabRight, Some(CONTEXT)),
    ]);
    crate::views::hosts::init(cx);
    palette::init(cx);
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    // Deferred: the action arrives while the window in front is being
    // updated, and the new one is placed from its bounds.
    cx.on_action(|_: &NewWindow, cx: &mut App| cx.defer(windows::open_new));
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
    cx.on_action(|_: &OpenDocs, cx: &mut App| cx.open_url(menus::DOCS_URL));
    cx.on_action(|_: &ReportIssue, cx: &mut App| cx.open_url(menus::ISSUES_URL));
    crate::dock::init(cx);
    set_menus(cx);
}

/// Application menu (and the Dock menu on macOS), in the current language
/// (call it again after changing the language).
pub fn set_menus(cx: &mut App) {
    menus::set_menus(cx);
    crate::dock::set_menu(cx);
}

/// Sections of the sidebar.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Hosts,
    Keychain,
    Snippets,
    Forwards,
    KnownHosts,
    Ai,
    ServerSessions,
    Teams,
    Admin,
    Settings,
}

impl Section {
    fn label(self) -> SharedString {
        match self {
            Section::Hosts => t!("app.section.hosts"),
            Section::Keychain => t!("app.section.keychain"),
            Section::Snippets => t!("app.section.snippets"),
            Section::Forwards => t!("app.section.forwards"),
            Section::KnownHosts => t!("app.section.known_hosts"),
            Section::Ai => t!("app.section.ai"),
            Section::ServerSessions => t!("app.section.server_sessions"),
            Section::Teams => t!("app.section.teams"),
            Section::Admin => t!("app.section.admin"),
            Section::Settings => t!("app.section.settings"),
        }
    }

    fn icon(self) -> IconName {
        match self {
            Section::Hosts => IconName::Server,
            Section::Keychain => IconName::KeyRound,
            Section::Snippets => IconName::SquareTerminal,
            Section::Forwards => IconName::ArrowLeftRight,
            Section::KnownHosts => IconName::ShieldCheck,
            Section::Ai => IconName::Sparkles,
            Section::ServerSessions => IconName::Cloud,
            Section::Teams => IconName::Users,
            Section::Admin => IconName::ShieldUser,
            Section::Settings => IconName::Settings,
        }
    }
}

/// Where a dragged tab or pane would land now (the drop indicator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropHint {
    /// Before or after a tab of the title bar (by id).
    Tab { id: usize, before: bool },
    /// After the last tab (the empty end of the tab bar).
    End,
    /// On a side of a terminal of the active tab.
    Pane { id: EntityId, zone: Zone },
}

/// What was dropped on a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DropSource {
    /// A tab of the title bar (by id).
    Tab(usize),
    /// A pane of a split view (its terminal).
    Pane(EntityId),
}

/// A terminal of a workspace.
struct Pane {
    view: Entity<TerminalView>,
    /// Its events and focus tracking.
    _subs: Vec<Subscription>,
}

/// Terminals of a tab: one, or several in a grid (split view).
struct Panes {
    items: Vec<Pane>,
    focused: usize,
    /// Focus mode: the focused pane big, the others small.
    maximized: bool,
    /// What is typed in the focused pane goes to every included pane.
    broadcast: bool,
    /// Panes left out of the broadcast.
    excluded: HashSet<EntityId>,
}

impl Panes {
    fn new(items: Vec<Pane>) -> Self {
        Self {
            items,
            focused: 0,
            maximized: false,
            broadcast: false,
            excluded: HashSet::new(),
        }
    }

    fn focused(&self) -> &Entity<TerminalView> {
        &self.items[self.focused.min(self.items.len() - 1)].view
    }

    fn ids(&self) -> Vec<EntityId> {
        self.items.iter().map(|p| p.view.entity_id()).collect()
    }

    fn position(&self, id: EntityId) -> Option<usize> {
        self.items.iter().position(|p| p.view.entity_id() == id)
    }

    fn is_split(&self) -> bool {
        self.items.len() > 1
    }

    fn views(&self) -> impl Iterator<Item = &Entity<TerminalView>> {
        self.items.iter().map(|p| &p.view)
    }
}

enum TabContent {
    Terminal(Panes),
    Sftp(Entity<SftpView>),
    /// Server session not looked at yet: it attaches when clicked.
    Dormant {
        session_id: Id,
        title: String,
        /// Account whose server has it.
        account: Id,
    },
}

struct Tab {
    id: usize,
    content: TabContent,
    /// Name given with "Rename" (instead of the automatic one).
    custom_title: Option<String>,
}

impl Tab {
    fn title(&self, cx: &App) -> SharedString {
        if let Some(t) = &self.custom_title {
            return t.clone().into();
        }
        match &self.content {
            TabContent::Terminal(p) if p.is_split() => t!(
                "app.split.tab_title",
                name = p.items[0].view.read(cx).title(cx),
                more = p.items.len() - 1
            ),
            TabContent::Terminal(p) => p.focused().read(cx).title(cx),
            TabContent::Sftp(s) => s.read(cx).title(),
            TabContent::Dormant { title, .. } => title.clone().into(),
        }
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.content {
            TabContent::Terminal(p) => Some(p.focused().focus_handle(cx)),
            TabContent::Sftp(_) | TabContent::Dormant { .. } => None,
        }
    }

    /// Server sessions of the tab (attached or dormant).
    fn server_sessions(&self, cx: &App) -> Vec<Id> {
        match &self.content {
            TabContent::Terminal(p) => p
                .views()
                .filter_map(|v| match v.read(cx).kind() {
                    TermKind::Server { session_id, .. } => *session_id,
                    _ => None,
                })
                .collect(),
            TabContent::Dormant { session_id, .. } => vec![*session_id],
            TabContent::Sftp(_) => Vec::new(),
        }
    }

    fn panes(&self) -> Option<&Panes> {
        match &self.content {
            TabContent::Terminal(p) => Some(p),
            _ => None,
        }
    }
}

/// How to open a terminal like an existing one ("Duplicate session").
fn duplicate_request(kind: &TermKind) -> Option<OpenRequest> {
    match kind {
        TermKind::Local { host_id } => Some(OpenRequest::Local { host_id: *host_id }),
        TermKind::Server {
            host_id: Some(h), ..
        } => Some(OpenRequest::Server { host_id: *h }),
        TermKind::Server { host_id: None, .. } => None,
        TermKind::Shell => Some(OpenRequest::Shell),
        TermKind::Serial { path, baud } => Some(OpenRequest::Serial(SerialParams {
            path: path.clone(),
            baud: *baud,
        })),
    }
}

/// Root view (inside `gpui_component::Root`).
pub struct AppView {
    model: Entity<AppModel>,
    updates: Entity<UpdateModel>,
    section: Section,
    tabs: Vec<Tab>,
    /// Active tab (`None` = home with the sidebar).
    active: Option<usize>,
    /// Last terminal tab that was active (by id): "Connect in split view"
    /// adds to it when the hosts are on screen.
    last_terminal: Option<usize>,
    next_id: usize,
    sidebar_collapsed: bool,
    hosts: Entity<HostsView>,
    keychain: Entity<KeychainView>,
    snippets: Entity<SnippetsView>,
    forwards: Entity<ForwardsView>,
    known_hosts: Entity<KnownHostsView>,
    ai: Entity<AiView>,
    server_sessions: Entity<ServerSessionsView>,
    teams: Entity<TeamsView>,
    admin: Entity<AdminView>,
    settings: Entity<SettingsView>,
    /// Copilot visible (to the right of the terminals).
    copilot_open: bool,
    /// Copilot conversation of each terminal (by its entity).
    copilots: HashMap<EntityId, (Entity<AiChat>, Subscription)>,
    /// Own sessions running on the server (for the Home notice).
    cloud_sessions: usize,
    /// Where what is being dragged would land (only drawn while dragging).
    drop_hint: Option<DropHint>,
    /// Saved workspaces and the last session's tabs (shared by the windows).
    workspaces: Entity<Workspaces>,
    /// Whether this window still has to reopen the last session's tabs.
    restore: Restore,
    /// The first window of the app (it opens the others of the last session).
    primary: bool,
    /// Your server sessions known to be running (last time they were asked).
    running_sessions: Option<Vec<Id>>,
    /// Touch ID / Windows Hello lock (shared by the windows).
    lock: Entity<AppLock>,
    /// The lock was on at the last render (to give the focus back).
    was_locked: bool,
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

/// Reopening the last session's tabs in a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Restore {
    /// When the hosts and the saved session are read.
    Waiting,
    Done,
}

impl AppView {
    pub fn new(
        model: Entity<AppModel>,
        updates: Entity<UpdateModel>,
        prompts_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PromptRequest>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let windows::Registered {
            first,
            start_services,
        } = windows::register(&model, &updates, cx.weak_entity(), window, cx);
        if start_services {
            Self::start_app_services(prompts_rx, model.clone(), cx);
        }
        // macOS: the red button of the last window hides the app instead
        // (the Dock brings it back with its tabs).
        #[cfg(target_os = "macos")]
        window.on_window_should_close(cx, windows::should_close);

        let hosts = cx.new(|cx| HostsView::new(model.clone(), window, cx));
        let keychain = cx.new(|cx| KeychainView::new(model.clone(), window, cx));
        let snippets = cx.new(|cx| SnippetsView::new(model.clone(), window, cx));
        let forwards = cx.new(|cx| ForwardsView::new(model.clone(), window, cx));
        let known_hosts = cx.new(|cx| KnownHostsView::new(model.clone(), window, cx));
        let ai = cx.new(|cx| AiView::new(model.clone(), window, cx));
        let server_sessions = cx.new(|cx| ServerSessionsView::new(model.clone(), window, cx));
        let teams = cx.new(|cx| TeamsView::new(model.clone(), window, cx));
        let admin = cx.new(|cx| AdminView::new(model.clone(), window, cx));
        let settings = cx.new(|cx| SettingsView::new(model.clone(), updates.clone(), window, cx));
        let workspaces = Workspaces::global(&model, cx);
        let lock = AppLock::global(&model, cx);

        let mut subs = vec![
            cx.observe_in(&model, window, |this, model, window, cx| {
                // The administration section disappears if you are no longer
                // an administrator (or you sign out).
                if this.section == Section::Admin && !model.read(cx).is_admin() {
                    this.select_section(Section::Hosts, window, cx);
                }
                this.try_restore(window, cx);
                cx.notify();
            }),
            cx.observe_in(&workspaces, window, |this, _, window, cx| {
                this.try_restore(window, cx);
                cx.notify();
            }),
            cx.observe_in(&lock, window, |this, lock, window, cx| {
                let locked = lock.read(cx).locked();
                if this.was_locked && !locked {
                    // Open again: the focus goes back to the tab in view.
                    this.activate(this.active, window, cx);
                }
                this.was_locked = locked;
                cx.notify();
            }),
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                // Notices show up in one window only (the one used last).
                let notices = windows::is_notice_window(window, cx);
                match ev {
                    ModelEvent::Toast(kind, msg) if notices => {
                        ui::notify(window, cx, *kind, msg.clone())
                    }
                    ModelEvent::LayoutMigrated if notices => this.layout_notice(window, cx),
                    ModelEvent::Server(v) if v["type"] == "ai" && notices => {
                        this.on_ai_event(v, window, cx)
                    }
                    // When starting signed in or when signing in: the running
                    // sessions, as dormant tabs (in the first window).
                    ModelEvent::SessionChanged => {
                        let add_tabs = windows::is_first_window(window, cx);
                        this.restore_cloud_tabs(add_tabs, window, cx)
                    }
                    ModelEvent::Server(v) if v["type"] == "session" => {
                        this.restore_cloud_tabs(false, window, cx);
                        if !notices {
                            return;
                        }
                        if v["notice"]["type"] == "session_closed"
                            && let Some(id) = v["notice"]["session_id"]
                                .as_str()
                                .and_then(|s| s.parse::<Id>().ok())
                        {
                            this.model.update(cx, |m, cx| m.clear_session_alert(id, cx));
                        }
                        if let Some(notice) = SessionNotice::from_event(v) {
                            let account = v["account_id"].as_str().and_then(|a| a.parse().ok());
                            this.on_session_notice(notice, account, window, cx);
                        }
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(&updates, window, |this, _, ev: &UpdateEvent, window, cx| {
                if windows::is_notice_window(window, cx) {
                    this.on_update_event(ev, window, cx);
                }
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                let active = window.is_window_active();
                windows::set_active(window, active, cx);
                let any = windows::any_active(cx);
                this.lock.update(cx, |l, cx| {
                    l.set_active(any);
                    // Locked: the prompt opens by itself (once per lock).
                    if active {
                        l.auto_unlock(window, cx);
                    }
                });
            }),
        ];
        // A window that closes leaves the saved session (unless it is the
        // last one: its tabs reopen next time).
        let me = cx.entity_id();
        let closing = workspaces.clone();
        subs.push(cx.on_release(move |_, cx| {
            let others = !windows::views(cx).is_empty();
            closing.update(cx, |w, _| w.window_closed(me, others));
        }));
        subs.push(cx.subscribe_in(&hosts, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&keychain, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&snippets, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&forwards, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&known_hosts, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&ai, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&server_sessions, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&settings, window, Self::on_open_request));

        let logged_in = model.read(cx).logged_in();
        // The first window reopens the last session's tabs (and opens the
        // other windows it had); those windows take theirs.
        let restore = if !model.read(cx).settings.reopen_tabs {
            Restore::Done
        } else if start_services || workspaces.update(cx, |w, _| w.claim_session_window()) {
            Restore::Waiting
        } else {
            Restore::Done
        };
        let was_locked = lock.read(cx).locked();
        let mut view = Self {
            model,
            updates,
            section: Section::Hosts,
            tabs: Vec::new(),
            active: None,
            last_terminal: None,
            next_id: 1,
            sidebar_collapsed: false,
            hosts,
            keychain,
            snippets,
            forwards,
            known_hosts,
            ai,
            server_sessions,
            teams,
            admin,
            settings,
            copilot_open: false,
            copilots: HashMap::new(),
            cloud_sessions: 0,
            drop_hint: None,
            workspaces,
            restore,
            primary: start_services,
            running_sessions: None,
            lock,
            was_locked,
            focus: cx.focus_handle(),
            _subs: subs,
        };
        // macOS asks for permission once; before the first notice, so that
        // one is not lost.
        if start_services && view.model.read(cx).settings.notifications.enabled {
            notifications::request_authorization();
        }
        // If the server session was already restored, the event will not
        // come. Only the first window gets the dormant tabs; a new one starts
        // empty.
        if logged_in {
            view.restore_cloud_tabs(first, window, cx);
        }
        view.try_restore(window, cx);
        // Locked at start: the system prompt opens by itself.
        if was_locked {
            let lock = view.lock.clone();
            window.defer(cx, move |window, cx| {
                lock.update(cx, |l, cx| l.auto_unlock(window, cx));
            });
        }
        // The layout notice may have come before this window existed.
        if start_services {
            let app = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                if let Some(app) = app.upgrade() {
                    app.update(cx, |app, cx| app.layout_notice(window, cx));
                }
            });
        }
        view
    }

    /// What exists once for the whole app, set up by the first window and
    /// shared by every window (see `windows.rs`): the authentication
    /// questions, the links opened from outside, the clicks on
    /// notifications of the system and the AI that runs on this computer.
    fn start_app_services(
        prompts_rx: Option<tokio::sync::mpsc::UnboundedReceiver<PromptRequest>>,
        model: Entity<AppModel>,
        cx: &mut Context<Self>,
    ) {
        // Questions of the SSH engine: in the window in use.
        if let Some(mut rx) = prompts_rx {
            cx.spawn(async move |_, cx| {
                while let Some(req) = rx.recv().await {
                    cx.update(|cx| windows::show_prompt(req, cx));
                }
            })
            .detach();
        }
        // `termoak://` links opened from outside (at startup or later).
        if let Some(mut links) = crate::links::take_receiver() {
            cx.spawn(async move |_, cx| {
                while let Some(link) = links.recv().await {
                    // With every window closed (macOS), one opens for it.
                    cx.update(|cx| {
                        windows::with_front_window(cx, move |app, window, cx| {
                            app.open_link(&link, window, cx)
                        })
                    });
                }
            })
            .detach();
        }
        // A click on a notification of the system: the window comes to the
        // front with what it was about.
        cx.on_system_notification_response(|response, cx| {
            windows::notification_clicked(&response.tag, cx)
        });

        // The AI that runs on this computer: its tools reach the terminals
        // of every window through these requests.
        let (terminals, mut term_rx) = LocalTerminals::new();
        let store = model.read(cx).ws.store.clone();
        let rt = runtime::handle(cx);
        let local_ai = {
            let _rt = rt.enter();
            Arc::new(LocalAi::new(store, Arc::new(terminals)))
        };
        cx.set_global(LocalAiGlobal(local_ai.clone()));
        Self::start_local_engine(local_ai, model, cx);
        cx.spawn(async move |_, cx| {
            while let Some(req) = term_rx.recv().await {
                cx.update(|cx| windows::answer_terminal(req, cx));
            }
        })
        .detach();
    }

    /// A `termoak://` (or web join) link: join a shared session or sign up
    /// with an invitation.
    pub fn open_link(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        windows::unhide_app();
        window.activate_window();
        if sharing::parse_join_link(link).is_some() {
            self.open_join_dialog(Some(link.to_string()), window, cx);
        } else if add_account::parse_invite_link(link).is_some() {
            self.select_section(Section::Settings, window, cx);
            self.settings
                .update(cx, |s, cx| s.open_invite_link(link, window, cx));
        } else {
            ui::notify(window, cx, ToastKind::Warning, t!("join.unknown_link"));
        }
    }

    /// "Join with link".
    pub fn open_join_dialog(
        &mut self,
        link: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.entity().downgrade();
        crate::views::join::open(
            self.model.clone(),
            link,
            std::rc::Rc::new(move |req, window, cx| {
                if let Some(app) = weak.upgrade() {
                    app.update(cx, |app, cx| app.open(req, window, cx));
                }
            }),
            window,
            cx,
        );
    }

    /// Shows the tab (and pane) of a terminal.
    fn show_terminal(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((tab, pane)) = self.find_pane(id) {
            self.activate(Some(tab), window, cx);
            self.focus_pane(tab, pane, window, cx);
        }
    }

    /// A terminal is in view: its tab is the active one and the window has
    /// the focus.
    fn terminal_in_view(&self, id: EntityId, window: &Window) -> bool {
        window.is_window_active()
            && self
                .find_pane(id)
                .is_some_and(|(tab, _)| self.active == Some(tab))
    }

    /// A request from a shared terminal: if its tab is not in view, a toast
    /// to answer it from anywhere.
    fn on_share_request(
        &mut self,
        id: EntityId,
        req: &ShareRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.notify_system(
            Notice {
                category: Category::Sharing,
                key: notifications::request_key(req.kind == RequestKind::Join, req.participant),
                title: req.title.clone(),
                body: req.text().to_string(),
                target: Target::Terminal(id),
            },
            window,
            cx,
        );
        if self.terminal_in_view(id, window) {
            return;
        }
        let Some(view) = self
            .find_pane(id)
            .and_then(|(tab, _)| self.tabs[tab].panes())
            .and_then(|p| p.views().find(|v| v.entity_id() == id).cloned())
        else {
            return;
        };
        let terminal = view.downgrade();
        let app = cx.entity().downgrade();
        let (kind, participant) = (req.kind, req.participant);
        let yes_label = match kind {
            RequestKind::Join => t!("share.allow"),
            RequestKind::Control => t!("share.give"),
        };
        window.push_notification(
            Notification::new()
                .id1::<ShareRequestToast>(request_key(kind, participant))
                .icon(ui::icon(match kind {
                    RequestKind::Join => IconName::DoorOpen,
                    RequestKind::Control => IconName::Keyboard,
                }))
                .title(req.title.clone())
                .message(req.text())
                .autohide(false)
                .content(move |_, _, cx| {
                    let answer = |yes: bool| {
                        let terminal = terminal.clone();
                        cx.listener(move |note: &mut Notification, _: &ClickEvent, window, cx| {
                            if let Some(t) = terminal.upgrade() {
                                t.update(cx, |t, cx| t.answer_request(kind, participant, yes, cx));
                            }
                            note.dismiss(window, cx);
                        })
                    };
                    let app = app.clone();
                    h_flex()
                        .pt_2()
                        .gap_2()
                        .child(
                            Button::new("request-yes")
                                .small()
                                .primary()
                                .label(yes_label.clone())
                                .on_click(answer(true)),
                        )
                        .child(
                            Button::new("request-no")
                                .small()
                                .label(t!("share.deny"))
                                .on_click(answer(false)),
                        )
                        .child(
                            Button::new("request-show")
                                .small()
                                .ghost()
                                .label(t!("share.show"))
                                .on_click(cx.listener(
                                    move |note: &mut Notification, _: &ClickEvent, window, cx| {
                                        if let Some(a) = app.upgrade() {
                                            a.update(cx, |a, cx| a.show_terminal(id, window, cx));
                                        }
                                        note.dismiss(window, cx);
                                    },
                                )),
                        )
                        .into_any_element()
                }),
            cx,
        );
    }

    /// A notice of the events WebSocket about a shared session. Sessions
    /// open in a tab show it themselves.
    fn on_session_notice(
        &mut self,
        notice: SessionNotice,
        account: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = notice.session_id();
        if self.has_session(session_id, cx)
            || windows::session_open_elsewhere(session_id, cx.entity_id(), cx)
        {
            return;
        }
        if notice.needs_owner() {
            self.model
                .update(cx, |m, cx| m.add_session_alert(session_id, cx));
        }
        let title = match &notice {
            SessionNotice::JoinRequest { title, .. }
            | SessionNotice::ControlRequest { title, .. }
            | SessionNotice::SessionShared { title, .. }
                if !title.is_empty() =>
            {
                title.clone()
            }
            _ => t!("server_sessions.default_title").to_string(),
        };
        let shared = matches!(notice, SessionNotice::SessionShared { .. });
        let heading = if shared {
            t!("share.notice.shared_title")
        } else {
            t!("share.notice.title")
        };
        let (category, key) = notifications::session_notice_key(&notice);
        self.notify_system(
            Notice {
                category,
                key,
                title: heading.to_string(),
                body: notice.text(),
                target: Target::Session {
                    session_id,
                    title: title.clone(),
                    account,
                },
            },
            window,
            cx,
        );
        let mut note = Notification::info(notice.text())
            .id1::<SessionNoticeToast>(SharedString::from(session_id.to_string()))
            .title(heading);
        if notice.needs_owner() || shared {
            let app = cx.entity().downgrade();
            note = note.action(move |_, _, _| {
                let app = app.clone();
                let title = title.clone();
                Button::new("notice-open")
                    .small()
                    .primary()
                    .label(t!("server_sessions.open"))
                    .on_click(move |_, window, cx| {
                        if let Some(a) = app.upgrade() {
                            let req = OpenRequest::Attach {
                                session_id,
                                title: title.clone(),
                                account,
                            };
                            a.update(cx, |a, cx| a.open(req, window, cx));
                        }
                    })
            });
        }
        window.push_notification(note, cx);
    }

    /// An event of an AI task: a task on this computer needing approval or
    /// ending shows a toast (the server's ones are in the AI section); both
    /// become a notification of the system in the background.
    fn on_ai_event(&mut self, v: &serde_json::Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(notice) = AiNotice::from_event(v) else {
            return;
        };
        let local = v["local"] == true;
        if local {
            let (kind, text) = notice.toast(true);
            ui::notify(window, cx, kind, text);
        }
        self.notify_system(notice.notice(local), window, cx);
    }

    /// Posts a notice as a notification of the system if the window is in
    /// the background and its kind is on in Settings (see `notifications`).
    /// (Any window of the app in front counts: the toast is enough.)
    fn notify_system(&mut self, notice: Notice, window: &Window, cx: &mut Context<Self>) {
        let prefs = self.model.read(cx).settings.notifications;
        let active = window.is_window_active() || windows::any_active(cx);
        let admitted = cx.try_global::<windows::AppWindows>().is_some().then(|| {
            cx.global_mut::<windows::AppWindows>().notifier.admit(
                &prefs,
                notice,
                active,
                Instant::now(),
            )
        });
        if let Some(n) = admitted.flatten() {
            cx.show_system_notification(n);
        }
    }

    /// A notification of the system was clicked (`windows.rs` picks the
    /// window): it comes to the front and opens what it was about.
    pub fn open_notification_target(
        &mut self,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.activate(true);
        window.activate_window();
        match target {
            Target::Terminal(id) => self.show_terminal(id, window, cx),
            Target::Session {
                session_id,
                title,
                account,
            } => self.open(
                OpenRequest::Attach {
                    session_id,
                    title,
                    account,
                },
                window,
                cx,
            ),
            Target::AiTask(task_id) => {
                self.select_section(Section::Ai, window, cx);
                self.ai.update(cx, |v, cx| v.open_task(task_id, window, cx));
            }
        }
    }

    /// Starts the engine of the AI tasks on this computer and passes its
    /// events on like the server's (marked `"local"`).
    fn start_local_engine(local_ai: Arc<LocalAi>, model: Entity<AppModel>, cx: &mut Context<Self>) {
        let started = runtime::spawn(cx, async move { local_ai.start_engine().await });
        // Not tied to a window: it keeps going while any window is open.
        let model = model.downgrade();
        cx.spawn(async move |_, cx| {
            let engine = match started.await {
                Ok(e) => e,
                Err(e) => {
                    tracing::error!(error = %e, "the local AI engine could not start");
                    return;
                }
            };
            let mut events = engine.subscribe();
            // The views load the local tasks now.
            let _ = model.update(cx, |_, cx| {
                cx.emit(ModelEvent::Server(serde_json::json!({"type": "hello"})))
            });
            loop {
                let ev = match events.recv().await {
                    Ok(ev) => ev,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let mut v = serde_json::to_value(&ev).unwrap_or_default();
                v["type"] = "ai".into();
                // The window shows its notices (`on_ai_event`).
                v["local"] = true.into();
                // The model goes away with the last window.
                let alive = model
                    .update(cx, |_, cx| cx.emit(ModelEvent::Server(v)))
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Every open terminal of this window (all the panes of every tab).
    pub fn terminals(&self) -> Vec<Entity<TerminalView>> {
        self.tabs
            .iter()
            .filter_map(Tab::panes)
            .flat_map(|p| p.views().cloned().collect::<Vec<_>>())
            .collect()
    }

    /// The terminal `id` is in a tab of this window.
    pub fn has_terminal(&self, id: EntityId) -> bool {
        self.find_pane(id).is_some()
    }

    /// The server session is open in a terminal of this window.
    pub fn has_session(&self, session_id: Id, cx: &App) -> bool {
        self.tabs.iter().any(|t| {
            matches!(t.content, TabContent::Terminal(_))
                && t.server_sessions(cx).contains(&session_id)
        })
    }

    /// Fetches the server sessions. With `add_tabs`, own running sessions
    /// that are not open are added as dormant tabs (without changing screen);
    /// without it, only the Home notice is updated.
    fn restore_cloud_tabs(&mut self, add_tabs: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Every signed-in account (not only the one in sight): dormant tabs
        // of the others stay.
        let sources: Vec<(Id, termoak_client::ApiClient)> = {
            let m = self.model.read(cx);
            m.accounts
                .iter()
                .filter_map(|a| m.api_of(Some(a.id())).map(|api| (a.id(), api)))
                .collect()
        };
        if sources.is_empty() {
            // Signed out, dormant tabs can no longer attach.
            self.cloud_sessions = 0;
            let dormant: Vec<usize> = self
                .tabs
                .iter()
                .filter(|t| matches!(t.content, TabContent::Dormant { .. }))
                .map(|t| t.id)
                .collect();
            for id in dormant {
                self.drop_dormant(id);
            }
            cx.notify();
            return;
        }
        runtime::run_in(
            cx,
            window,
            async move {
                let mut out = Vec::new();
                let mut failed = false;
                for (account, api) in sources {
                    match api.get::<serde_json::Value>("/api/v1/sessions").await {
                        Ok(v) => out.push((account, v)),
                        Err(_) => failed = true,
                    }
                }
                // Without every answer, nothing is dropped as "ended".
                if failed {
                    return Err("incomplete".to_string());
                }
                Ok::<_, String>(out)
            },
            move |this, res, _, cx| {
                let Ok(all) = res else { return };
                let running: Vec<(Id, String, Id)> = all
                    .iter()
                    .flat_map(|(account, v)| {
                        v["active"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|s| s["kind"] == "server" && s["state"]["state"] != "closed")
                            .filter(|s| s["access"].as_str().unwrap_or("owner") == "owner")
                            .filter_map(move |s| {
                                Some((
                                    s["id"].as_str()?.parse().ok()?,
                                    s["title"].as_str().map(str::to_string).unwrap_or_else(|| {
                                        t!("app.session_fallback_title").to_string()
                                    }),
                                    *account,
                                ))
                            })
                    })
                    .collect();
                this.cloud_sessions = running.len();
                this.running_sessions = Some(running.iter().map(|(id, _, _)| *id).collect());
                // Sessions that ended lower the count the Home notice was
                // closed with: a new one shows it again.
                let dismissed = this.model.read(cx).settings.cloud_notice_dismissed;
                if let Some(d) = app_state::cloud_notice_lowered(running.len(), dismissed) {
                    this.model.update(cx, |m, cx| {
                        let mut s = m.settings.clone();
                        s.cloud_notice_dismissed = d;
                        m.save_settings(s, cx);
                    });
                }
                // Dormant tabs of sessions that already ended are dropped.
                let ended: Vec<usize> = this
                    .tabs
                    .iter()
                    .filter(|t| match &t.content {
                        TabContent::Dormant { session_id, .. } => {
                            !running.iter().any(|(id, _, _)| id == session_id)
                        }
                        _ => false,
                    })
                    .map(|t| t.id)
                    .collect();
                for id in ended {
                    this.drop_dormant(id);
                }
                // While the last session's tabs are still to reopen, they
                // come first; this runs again after them.
                if add_tabs && this.restore == Restore::Done {
                    let me = cx.entity_id();
                    for (session_id, title, account) in running {
                        let open = this
                            .tabs
                            .iter()
                            .any(|t| t.server_sessions(cx).contains(&session_id))
                            || windows::session_open_elsewhere(session_id, me, cx);
                        if !open {
                            let id = this.next_id;
                            this.next_id += 1;
                            this.tabs.push(Tab {
                                id,
                                content: TabContent::Dormant {
                                    session_id,
                                    title,
                                    account,
                                },
                                custom_title: None,
                            });
                        }
                    }
                }
                cx.notify();
            },
        );
    }

    /// Removes a dormant tab without touching the active one or the focus (it
    /// is never active: activating it wakes it up).
    fn drop_dormant(&mut self, id: usize) {
        let Some(ix) = self.tab_index(id) else { return };
        if !matches!(self.tabs[ix].content, TabContent::Dormant { .. }) {
            return;
        }
        self.tabs.remove(ix);
        if let Some(a) = self.active
            && a > ix
        {
            self.active = Some(a - 1);
        }
    }

    /// Attaches a dormant tab (when clicked).
    fn wake(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(TabContent::Dormant {
            session_id,
            title,
            account,
        }) = self.tabs.get(ix).map(|t| &t.content)
        else {
            return;
        };
        let (session_id, title, account) = (*session_id, title.clone(), *account);
        let model = self.model.clone();
        let view = cx.new(|cx| {
            TerminalView::server(
                model,
                None,
                Some(session_id),
                Some(account),
                title,
                window,
                cx,
            )
        });
        let pane = self.new_pane(view, window, cx);
        self.tabs[ix].content = TabContent::Terminal(Panes::new(vec![pane]));
    }

    /// Copilot of the focused terminal of the active tab (created the first
    /// time), if the tab is a terminal.
    fn active_copilot(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<AiChat>> {
        let term = self.focused_terminal()?;
        Some(self.copilot_of(term, window, cx))
    }

    /// Copilot of a terminal (created the first time).
    fn copilot_of(
        &mut self,
        term: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<AiChat> {
        let key = term.entity_id();
        if let Some((chat, _)) = self.copilots.get(&key) {
            return chat.clone();
        }
        let model = self.model.clone();
        let weak = term.downgrade();
        let chat = cx.new(|cx| AiChat::copilot(model, weak, window, cx));
        let sub = cx.subscribe_in(
            &chat,
            window,
            |this, _, ev: &AiChatEvent, window, cx| match ev {
                AiChatEvent::Close => this.set_copilot(false, window, cx),
                AiChatEvent::OpenAiSettings => this.open(OpenRequest::AiSettings, window, cx),
                AiChatEvent::Summary(_) => {}
            },
        );
        self.copilots.insert(key, (chat.clone(), sub));
        chat
    }

    fn set_copilot(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        let opening = open && !self.copilot_open;
        self.copilot_open = open;
        if open {
            if let Some(chat) = self.active_copilot(window, cx) {
                chat.update(cx, |c, cx| {
                    // The terminal's context, as removable chips.
                    if opening {
                        c.load_terminal_context(cx);
                    }
                    c.focus_input(window, cx)
                });
            }
        } else {
            // Once closed, the AI stops working and loses access to the terminal.
            if let Some(chat) = self.active_copilot(window, cx) {
                chat.update(cx, |c, cx| c.release(window, cx));
            }
            // The focus goes back to the terminal.
            self.activate(self.active, window, cx);
        }
        cx.notify();
    }

    /// A terminal asks its copilot for something (explain a failed command
    /// or the selection, ask about the selection): it opens, focused on that
    /// terminal, with its context.
    fn copilot_request(
        &mut self,
        id: EntityId,
        req: CopilotRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((tab, pane)) = self.find_pane(id) else {
            return;
        };
        let Some(term) = self.tabs[tab]
            .panes()
            .and_then(|p| p.views().find(|v| v.entity_id() == id).cloned())
        else {
            return;
        };
        if self.active != Some(tab) {
            self.activate(Some(tab), window, cx);
        }
        if let TabContent::Terminal(p) = &mut self.tabs[tab].content
            && p.focused != pane
        {
            p.focused = pane;
            self.sync_panes(tab, cx);
        }
        self.copilot_open = true;
        let chat = self.copilot_of(term, window, cx);
        chat.update(cx, |c, cx| {
            c.load_terminal_context(cx);
            match req {
                CopilotRequest::Ask => c.focus_input(window, cx),
                CopilotRequest::Explain(req) => c.quick_explain(req, window, cx),
            }
        });
        cx.notify();
    }

    /// Lets go of the copilot of a terminal that is closing.
    fn release_copilot(&mut self, id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((chat, _)) = self.copilots.remove(&id) {
            chat.update(cx, |c, cx| c.release(window, cx));
        }
    }

    fn on_update_event(&mut self, ev: &UpdateEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            UpdateEvent::Ready(version) => {
                let updates = self.updates.clone();
                window.push_notification(
                    Notification::success(t!("app.update.ready", version = version))
                        .title("Termoak")
                        .action(move |_, _, _| {
                            let updates = updates.clone();
                            Button::new("restart-now")
                                .primary()
                                .small()
                                .label(t!("app.update.restart_now"))
                                .on_click(move |_, _, cx| updates.read(cx).restart_now())
                        }),
                    cx,
                );
            }
            UpdateEvent::Available(version) => {
                window.push_notification(
                    Notification::info(t!("app.update.available", version = version))
                        .title("Termoak")
                        .action(|_, _, _| {
                            Button::new("open-releases")
                                .small()
                                .label(t!("app.update.download"))
                                .on_click(|_, _, cx| cx.open_url(update::RELEASES_PAGE))
                        }),
                    cx,
                );
            }
        }
    }

    fn on_open_request<T>(
        &mut self,
        _: &Entity<T>,
        req: &OpenRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(req.clone(), window, cx);
    }

    /// The terminal for a request, if it opens one (new entity, connecting).
    fn make_terminal(
        &mut self,
        req: &OpenRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TerminalView>> {
        let model = self.model.clone();
        Some(match req {
            OpenRequest::Local { host_id } => {
                let host_id = *host_id;
                cx.new(|cx| TerminalView::local(model, host_id, window, cx))
            }
            // Telnet hosts only open from this computer (a saved workspace
            // may still ask for a server session).
            OpenRequest::Server { host_id } if self.model.read(cx).is_telnet(*host_id) => {
                let host_id = *host_id;
                cx.new(|cx| TerminalView::local(model, host_id, window, cx))
            }
            OpenRequest::Server { host_id } => {
                let host_id = *host_id;
                let title = self.model.read(cx).host_label(host_id);
                cx.new(|cx| {
                    TerminalView::server(model, Some(host_id), None, None, title, window, cx)
                })
            }
            OpenRequest::Attach {
                session_id,
                title,
                account,
            } => {
                let (session_id, title, account) = (*session_id, title.clone(), *account);
                cx.new(|cx| {
                    TerminalView::server(model, None, Some(session_id), account, title, window, cx)
                })
            }
            OpenRequest::JoinLink {
                session_id,
                title,
                link,
            } => {
                let (session_id, title, link) = (*session_id, title.clone(), link.clone());
                cx.new(|cx| TerminalView::join_link(model, session_id, link, title, window, cx))
            }
            OpenRequest::Shell => cx.new(|cx| TerminalView::shell(model, window, cx)),
            OpenRequest::Serial(params) => {
                let params = params.clone();
                cx.new(|cx| TerminalView::serial(model, params, window, cx))
            }
            _ => return None,
        })
    }

    /// How a host opens from this device: Use-only hosts of Strict vaults
    /// open a server session instead, and Use-only hosts need their server
    /// for just-in-time credentials. `None`: it cannot open (already said).
    fn route_local(
        &self,
        host_id: Id,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<OpenRequest> {
        let m = self.model.read(cx);
        let caps = m.host_caps(host_id);
        let signed_in = m.api_for_host(host_id).is_some();
        let telnet = m.is_telnet(host_id);
        match accounts::connect_route(caps.connect, signed_in) {
            Ok(false) => Some(OpenRequest::Local { host_id }),
            // The server has no Telnet sessions.
            Ok(true) if telnet => {
                ui::error(window, cx, t!("telnet.strict_vault"));
                None
            }
            Ok(true) => {
                ui::notify(window, cx, ToastKind::Info, t!("vaults.strict_connect"));
                Some(OpenRequest::Server { host_id })
            }
            Err(code) => {
                ui::error(
                    window,
                    cx,
                    crate::i18n::api_error_text(code).unwrap_or_else(|| code.to_string()),
                );
                None
            }
        }
    }

    /// Opens a new tab.
    pub fn open(&mut self, req: OpenRequest, window: &mut Window, cx: &mut Context<Self>) {
        let req = match req {
            OpenRequest::Local { host_id } => match self.route_local(host_id, window, cx) {
                Some(r) => r,
                None => return,
            },
            other => other,
        };
        if let OpenRequest::Attach { session_id, .. } = &req {
            let id = *session_id;
            self.model.update(cx, |m, cx| m.clear_session_alert(id, cx));
            window
                .remove_notification1::<SessionNoticeToast>(SharedString::from(id.to_string()), cx);
        }
        match req {
            OpenRequest::Attach { session_id, .. }
                if let Some(ix) = self
                    .tabs
                    .iter()
                    .position(|t| t.server_sessions(cx).contains(&session_id)) =>
            {
                // Already open: activate it.
                self.activate(Some(ix), window, cx);
            }
            OpenRequest::Sftp { host_id, .. } if self.model.read(cx).is_telnet(host_id) => {
                ui::notify(window, cx, ToastKind::Info, t!("telnet.no_sftp"));
            }
            OpenRequest::Sftp { host_id, conn } => self.open_sftp(host_id, conn, window, cx),
            OpenRequest::AiSettings => {
                self.select_section(Section::Settings, window, cx);
                self.settings
                    .update(cx, |s, cx| s.show_page(SettingsPage::Ai, window, cx));
            }
            OpenRequest::AiTask { host_ids } => {
                self.select_section(Section::Ai, window, cx);
                self.ai
                    .update(cx, |v, cx| v.start_with_hosts(host_ids, window, cx));
            }
            OpenRequest::Split { hosts, current } => {
                let reqs: Vec<OpenRequest> = hosts
                    .into_iter()
                    .filter_map(|host_id| self.route_local(host_id, window, cx))
                    .collect();
                self.open_split(reqs, current, window, cx);
            }
            req => {
                if let Some(view) = self.make_terminal(&req, window, cx) {
                    self.push_terminal(view, window, cx);
                }
            }
        }
    }

    /// Opens terminals in a split view: added to the current workspace
    /// (`current`, if there is room) or in a new tab. Beyond the limit of a
    /// grid, the rest go to another tab.
    pub fn open_split(
        &mut self,
        reqs: Vec<OpenRequest>,
        current: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let views: Vec<Entity<TerminalView>> = reqs
            .iter()
            .filter_map(|r| self.make_terminal(r, window, cx))
            .collect();
        if views.is_empty() {
            return;
        }
        let mut views = views.into_iter().peekable();
        if current && let Some(ix) = self.current_workspace() {
            let room = MAX_PANES.saturating_sub(self.tabs[ix].panes().map_or(0, |p| p.items.len()));
            let batch: Vec<_> = views.by_ref().take(room).collect();
            if !batch.is_empty() {
                let panes: Vec<Pane> = batch
                    .into_iter()
                    .map(|v| self.new_pane(v, window, cx))
                    .collect();
                if let TabContent::Terminal(p) = &mut self.tabs[ix].content {
                    p.items.extend(panes);
                    p.focused = p.items.len() - 1;
                }
                self.sync_panes(ix, cx);
                self.activate(Some(ix), window, cx);
            }
        }
        while views.peek().is_some() {
            let batch: Vec<_> = views.by_ref().take(MAX_PANES).collect();
            let panes: Vec<Pane> = batch
                .into_iter()
                .map(|v| self.new_pane(v, window, cx))
                .collect();
            self.push_tab(TabContent::Terminal(Panes::new(panes)), window, cx);
            let ix = self.tabs.len() - 1;
            self.sync_panes(ix, cx);
        }
    }

    /// Opens a request as a new pane of the current tab (split view), or as
    /// a tab if it is not a terminal.
    pub(crate) fn open_as_pane(
        &mut self,
        req: OpenRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let req = match req {
            // Use-only hosts of Strict vaults open through the server.
            OpenRequest::Local { host_id } => match self.route_local(host_id, window, cx) {
                Some(r) => r,
                None => return,
            },
            other => other,
        };
        match req {
            OpenRequest::Local { .. }
            | OpenRequest::Server { .. }
            | OpenRequest::Shell
            | OpenRequest::Serial(_) => self.open_split(vec![req], true, window, cx),
            other => self.open(other, window, cx),
        }
    }

    /// Workspace that "Connect in split view" adds to: the active terminal
    /// tab or, from the home screen, the last one used.
    fn current_workspace(&self) -> Option<usize> {
        if let Some(ix) = self.active
            && matches!(
                self.tabs.get(ix).map(|t| &t.content),
                Some(TabContent::Terminal(_))
            )
        {
            return Some(ix);
        }
        self.last_terminal
            .and_then(|id| self.tab_index(id))
            .filter(|ix| matches!(self.tabs[*ix].content, TabContent::Terminal(_)))
    }

    fn new_pane(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Pane {
        let id = view.entity_id();
        let events = cx.subscribe_in(
            &view,
            window,
            move |this, _, ev: &TerminalEvent, window, cx| {
                this.on_terminal_event(id, ev, window, cx)
            },
        );
        let handle = view.focus_handle(cx);
        let focus = cx.on_focus_in(&handle, window, move |this, _, cx| {
            this.pane_focused(id, cx)
        });
        Pane {
            view,
            _subs: vec![events, focus],
        }
    }

    fn push_terminal(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let pane = self.new_pane(view, window, cx);
        self.push_tab(TabContent::Terminal(Panes::new(vec![pane])), window, cx);
    }

    /// Tab and position of a terminal.
    fn find_pane(&self, id: EntityId) -> Option<(usize, usize)> {
        self.tabs.iter().enumerate().find_map(|(ix, t)| {
            t.panes()
                .and_then(|p| p.position(id))
                .map(|pane| (ix, pane))
        })
    }

    fn on_terminal_event(
        &mut self,
        id: EntityId,
        ev: &TerminalEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match ev {
            TerminalEvent::TitleChanged => cx.notify(),
            TerminalEvent::CommandFinished(done) => self.on_command_finished(id, done, window, cx),
            TerminalEvent::Copilot(req) => self.copilot_request(id, req.clone(), window, cx),
            TerminalEvent::ShareRequest(req) => self.on_share_request(id, req, window, cx),
            TerminalEvent::ShareRequestDone { kind, participant } => {
                window.remove_notification1::<ShareRequestToast>(
                    request_key(*kind, *participant),
                    cx,
                );
                let key = notifications::request_key(*kind == RequestKind::Join, *participant);
                let tag = cx
                    .try_global::<windows::AppWindows>()
                    .is_some()
                    .then(|| cx.global_mut::<windows::AppWindows>().notifier.forget(&key));
                if let Some(tag) = tag.flatten() {
                    cx.dismiss_system_notification(&tag);
                }
            }
            TerminalEvent::KeyboardChanged { you_drive, text } => {
                let title = self
                    .find_pane(id)
                    .and_then(|(tab, _)| self.tabs[tab].panes())
                    .and_then(|p| p.views().find(|v| v.entity_id() == id).cloned())
                    .map(|v| v.read(cx).title(cx).to_string())
                    .unwrap_or_else(|| t!("share.notice.title").to_string());
                self.notify_system(
                    Notice {
                        category: Category::SharedWithMe,
                        key: format!("keyboard:{id}:{you_drive}"),
                        title,
                        body: text.to_string(),
                        target: Target::Terminal(id),
                    },
                    window,
                    cx,
                );
            }
            TerminalEvent::OpenSftp { host_id, conn } => {
                self.open_sftp(*host_id, conn.clone(), window, cx);
            }
            TerminalEvent::Broadcast(input) => {
                let Some((tab, _)) = self.find_pane(id) else {
                    return;
                };
                let Some(p) = self.tabs[tab].panes() else {
                    return;
                };
                let targets = panes::broadcast_targets(&p.ids(), id, p.broadcast, &p.excluded);
                let views: Vec<Entity<TerminalView>> = p
                    .views()
                    .filter(|v| targets.contains(&v.entity_id()))
                    .cloned()
                    .collect();
                for v in views {
                    v.update(cx, |v, cx| v.apply_broadcast(input, cx));
                }
            }
            TerminalEvent::Pane(action) => {
                let Some((tab, pane)) = self.find_pane(id) else {
                    return;
                };
                match action {
                    PaneAction::Close => self.close_pane(tab, pane, window, cx),
                    PaneAction::ToggleInclude => {
                        if let TabContent::Terminal(p) = &mut self.tabs[tab].content
                            && !p.excluded.remove(&id)
                        {
                            p.excluded.insert(id);
                        }
                        self.sync_panes(tab, cx);
                    }
                    PaneAction::ToggleMaximize => {
                        if let TabContent::Terminal(p) = &mut self.tabs[tab].content {
                            p.maximized = !(p.maximized && p.focused == pane);
                            p.focused = pane;
                        }
                        self.sync_panes(tab, cx);
                        self.focus_pane(tab, pane, window, cx);
                    }
                }
            }
        }
    }

    /// A terminal got the focus (click, keyboard): it becomes the focused
    /// pane of its tab.
    fn pane_focused(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let Some((tab, pane)) = self.find_pane(id) else {
            return;
        };
        if let TabContent::Terminal(p) = &mut self.tabs[tab].content
            && p.focused != pane
        {
            p.focused = pane;
            self.sync_panes(tab, cx);
        }
    }

    /// Tells every terminal of a tab its pane controls and whether it
    /// broadcasts.
    fn sync_panes(&self, tab: usize, cx: &mut Context<Self>) {
        let Some(p) = self.tabs.get(tab).and_then(Tab::panes) else {
            return;
        };
        let split = p.is_split();
        for (i, pane) in p.items.iter().enumerate() {
            let included = !p.excluded.contains(&pane.view.entity_id());
            let chrome = split.then_some(PaneChrome {
                maximized: p.maximized && i == p.focused,
                broadcasting: p.broadcast,
                included,
            });
            let broadcasting = split && p.broadcast && included;
            pane.view.update(cx, |v, cx| {
                v.set_pane(chrome, cx);
                v.set_broadcasting(broadcasting, cx);
            });
        }
        cx.notify();
    }

    fn focus_pane(&mut self, tab: usize, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(view) = self
            .tabs
            .get(tab)
            .and_then(Tab::panes)
            .and_then(|p| p.items.get(pane))
            .map(|p| p.view.clone())
        else {
            return;
        };
        if let TabContent::Terminal(p) = &mut self.tabs[tab].content {
            p.focused = pane;
        }
        view.focus_handle(cx).focus(window, cx);
        self.sync_panes(tab, cx);
    }

    /// Closes one terminal of a split view (the tab, if it was the last).
    fn close_pane(&mut self, tab: usize, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(n) = self
            .tabs
            .get(tab)
            .and_then(Tab::panes)
            .map(|p| p.items.len())
        else {
            return;
        };
        if n <= 1 {
            self.close_tab(tab, window, cx);
            return;
        }
        let TabContent::Terminal(p) = &mut self.tabs[tab].content else {
            return;
        };
        let next = panes::focus_after_close(n, p.focused, pane).unwrap_or(0);
        let removed = p.items.remove(pane);
        p.excluded.remove(&removed.view.entity_id());
        p.focused = next;
        if p.items.len() < 2 {
            p.maximized = false;
            p.broadcast = false;
            p.excluded.clear();
        }
        let id = removed.view.entity_id();
        removed.view.update(cx, |t, cx| t.shutdown(cx));
        drop(removed);
        self.release_copilot(id, window, cx);
        self.sync_panes(tab, cx);
        if self.active == Some(tab) {
            self.focus_pane(tab, next, window, cx);
        }
    }

    fn open_sftp(
        &mut self,
        host_id: Id,
        conn: Option<Arc<Connection>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        let view = cx.new(|cx| SftpView::new(model, host_id, conn, window, cx));
        self.push_tab(TabContent::Sftp(view), window, cx);
    }

    fn push_tab(&mut self, content: TabContent, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            content,
            custom_title: None,
        });
        self.activate(Some(self.tabs.len() - 1), window, cx);
    }

    fn activate(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = ix.filter(|i| *i < self.tabs.len());
        if let Some(i) = self.active {
            self.wake(i, window, cx);
            if matches!(self.tabs[i].content, TabContent::Terminal(_)) {
                self.last_terminal = Some(self.tabs[i].id);
            }
        }
        match self.active.and_then(|i| self.tabs[i].focus_handle(cx)) {
            Some(handle) => handle.focus(window, cx),
            None => self.focus.focus(window, cx),
        }
        cx.notify();
    }

    fn tab_index(&self, id: usize) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    /// Files dropped on a tab: the terminal on screen in it uploads them,
    /// an SFTP browser too (to its server folder).
    fn drop_files_on_tab(
        &mut self,
        tab_id: usize,
        paths: Vec<std::path::PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        match &self.tabs[ix].content {
            TabContent::Terminal(p) => {
                let t = p.focused().clone();
                t.update(cx, |t, cx| t.drop_paths(paths, window, cx));
            }
            TabContent::Sftp(s) => {
                let s = s.clone();
                s.update(cx, |s, cx| s.drop_paths(paths, cx));
            }
            TabContent::Dormant { .. } => {}
        }
    }

    fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        let tab = self.tabs.remove(ix);
        match &tab.content {
            TabContent::Terminal(p) => {
                for v in p.views() {
                    let id = v.entity_id();
                    v.update(cx, |t, cx| t.shutdown(cx));
                    self.release_copilot(id, window, cx);
                }
            }
            TabContent::Sftp(s) => s.update(cx, |s, cx| s.shutdown(cx)),
            TabContent::Dormant { .. } => {}
        }
        let next = match self.active {
            _ if self.tabs.is_empty() => None,
            Some(a) if a > ix => Some(a - 1),
            Some(a) if a == ix => Some(ix.min(self.tabs.len() - 1)),
            other => other,
        };
        self.activate(next, window, cx);
    }

    /// Closes every tab but one.
    fn close_other_tabs(&mut self, keep: usize, window: &mut Window, cx: &mut Context<Self>) {
        let others: Vec<usize> = self
            .tabs
            .iter()
            .map(|t| t.id)
            .filter(|id| *id != keep)
            .collect();
        for id in others {
            if let Some(ix) = self.tab_index(id) {
                self.close_tab(ix, window, cx);
            }
        }
        if let Some(ix) = self.tab_index(keep) {
            self.activate(Some(ix), window, cx);
        }
    }

    /// Moves the terminals of tab `from` into tab `into` (as many as fit).
    fn merge_tabs(
        &mut self,
        from: usize,
        into: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (Some(from_ix), Some(into_ix)) = (self.tab_index(from), self.tab_index(into)) else {
            return;
        };
        let room = MAX_PANES.saturating_sub(
            self.tabs[into_ix]
                .panes()
                .map_or(MAX_PANES, |p| p.items.len()),
        );
        let from_len = self.tabs[from_ix].panes().map_or(0, |p| p.items.len());
        if room == 0 || from_len == 0 {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                t!("app.split.full", max = MAX_PANES),
            );
            return;
        }
        let moved: Vec<Pane> = match &mut self.tabs[from_ix].content {
            TabContent::Terminal(p) => p.items.drain(..room.min(from_len)).collect(),
            _ => return,
        };
        let emptied = self.tabs[from_ix]
            .panes()
            .is_some_and(|p| p.items.is_empty());
        if let TabContent::Terminal(p) = &mut self.tabs[from_ix].content
            && !emptied
        {
            p.focused = 0;
            p.maximized = false;
        }
        if let TabContent::Terminal(p) = &mut self.tabs[into_ix].content {
            p.items.extend(moved);
            p.focused = p.items.len() - 1;
        }
        if emptied {
            // Its terminals live on in the other tab: removed without closing them.
            self.tabs.remove(from_ix);
            if let Some(a) = self.active
                && a > from_ix
            {
                self.active = Some(a - 1);
            }
        } else if let Some(ix) = self.tab_index(from) {
            self.sync_panes(ix, cx);
        }
        if let Some(ix) = self.tab_index(into) {
            self.sync_panes(ix, cx);
            self.activate(Some(ix), window, cx);
        }
    }

    // ----- Moving tabs and panes (drag and drop, keyboard) -----

    /// Moves the tab at `from` into `slot` of the tab bar (a gap: 0 before
    /// the first tab, `len` after the last). The active tab stays active.
    fn move_tab(&mut self, from: usize, slot: usize, cx: &mut Context<Self>) {
        if let Some(to) = drag::move_item(&mut self.tabs, from, slot) {
            self.active = self.active.map(|a| drag::index_after_move(a, from, to));
        }
        cx.notify();
    }

    /// "Move tab left/right": the active tab, one place.
    fn step_active_tab(&mut self, right: bool, cx: &mut Context<Self>) {
        let Some(ix) = self.active else { return };
        if let Some(slot) = drag::step_slot(self.tabs.len(), ix, right) {
            self.move_tab(ix, slot, cx);
        }
    }

    /// Takes pane `pane` out of tab `tab` without closing its terminal. A
    /// tab left empty is removed; one left with a single pane is no longer
    /// a split view (as when closing a pane).
    fn take_pane(&mut self, tab: usize, pane: usize, cx: &mut Context<Self>) -> Option<Pane> {
        let TabContent::Terminal(p) = &mut self.tabs.get_mut(tab)?.content else {
            return None;
        };
        if pane >= p.items.len() {
            return None;
        }
        let n = p.items.len();
        let next = panes::focus_after_close(n, p.focused, pane).unwrap_or(0);
        let taken = p.items.remove(pane);
        p.excluded.remove(&taken.view.entity_id());
        p.focused = next;
        if p.items.len() < 2 {
            p.maximized = false;
            p.broadcast = false;
            p.excluded.clear();
        }
        if p.items.is_empty() {
            // Its terminal lives on elsewhere: removed without closing it.
            self.tabs.remove(tab);
            self.active = match self.active {
                Some(a) if a > tab => Some(a - 1),
                Some(a) if a == tab => None,
                other => other,
            };
        } else {
            self.sync_panes(tab, cx);
        }
        Some(taken)
    }

    /// A pane of a split view dropped on the tab bar: it becomes a tab of
    /// its own in that place.
    fn pane_to_tab(
        &mut self,
        terminal: EntityId,
        slot: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((tab, pane)) = self.find_pane(terminal) else {
            return;
        };
        let alone = self.tabs[tab].panes().is_some_and(|p| p.items.len() < 2);
        if alone {
            // Nothing to take out: the tab itself moves.
            self.move_tab(tab, slot, cx);
            return;
        }
        // The tab the slot is in front of (ids survive the change).
        let before = self.tabs.get(slot).map(|t| t.id);
        let Some(taken) = self.take_pane(tab, pane, cx) else {
            return;
        };
        let at = before
            .and_then(|id| self.tab_index(id))
            .unwrap_or(self.tabs.len());
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.insert(
            at,
            Tab {
                id,
                content: TabContent::Terminal(Panes::new(vec![taken])),
                custom_title: None,
            },
        );
        if let Some(a) = self.active
            && a >= at
        {
            self.active = Some(a + 1);
        }
        self.sync_panes(at, cx);
        self.activate(Some(at), window, cx);
    }

    /// "Move pane to a new tab": the focused pane of the active split view,
    /// next to it.
    fn on_pane_to_new_tab(
        &mut self,
        _: &PaneToNewTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active else { return };
        let Some(id) = self.focused_terminal().map(|t| t.entity_id()) else {
            return;
        };
        self.pane_to_tab(id, tab + 1, window, cx);
    }

    /// A tab (`Some(id)`) or a pane (`terminal`) dropped on side `zone` of
    /// the terminal `target`: its terminals join that split view there
    /// (as many as fit). A tab dropped on itself, or a pane on itself, does
    /// nothing.
    fn drop_on_pane(
        &mut self,
        source: DropSource,
        target: EntityId,
        zone: Zone,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((dst, _)) = self.find_pane(target) else {
            return;
        };
        let dst_id = self.tabs[dst].id;
        let dst_len = self.tabs[dst].panes().map_or(0, |p| p.items.len());
        let moved: Vec<Pane> = match source {
            DropSource::Pane(id) if id == target => return,
            DropSource::Pane(id) => {
                let Some((src, pane)) = self.find_pane(id) else {
                    return;
                };
                if src == dst {
                    // Rearranged inside the same split view: its broadcast
                    // and focus mode stay as they are.
                    let TabContent::Terminal(p) = &mut self.tabs[dst].content else {
                        return;
                    };
                    let moving = p.items.remove(pane);
                    let at = p
                        .position(target)
                        .map_or(p.items.len(), |t| panes::drop_index(p.items.len(), t, zone));
                    p.items.insert(at, moving);
                    self.sync_panes(dst, cx);
                    self.focus_pane(dst, at, window, cx);
                    return;
                }
                if dst_len >= MAX_PANES {
                    ui::notify(
                        window,
                        cx,
                        ToastKind::Warning,
                        t!("app.split.full", max = MAX_PANES),
                    );
                    return;
                }
                self.take_pane(src, pane, cx).into_iter().collect()
            }
            DropSource::Tab(id) if id == dst_id => return,
            DropSource::Tab(id) => {
                let Some(src) = self.tab_index(id) else {
                    return;
                };
                // A dormant session attaches first.
                self.wake(src, window, cx);
                let Some(src_len) = self.tabs[src].panes().map(|p| p.items.len()) else {
                    // SFTP: not a terminal.
                    return;
                };
                let room = MAX_PANES.saturating_sub(dst_len);
                if room == 0 || src_len == 0 {
                    ui::notify(
                        window,
                        cx,
                        ToastKind::Warning,
                        t!("app.split.full", max = MAX_PANES),
                    );
                    return;
                }
                // Taken from the end so the indexes still to take hold; the
                // first ones that fit go, in their order.
                let mut taken: Vec<Pane> = Vec::new();
                for pane in (0..room.min(src_len)).rev() {
                    let Some(src) = self.tab_index(id) else { break };
                    if let Some(p) = self.take_pane(src, pane, cx) {
                        taken.push(p);
                    }
                }
                taken.reverse();
                taken
            }
        };
        if moved.is_empty() {
            return;
        }
        let Some(dst) = self.tab_index(dst_id) else {
            return;
        };
        let TabContent::Terminal(p) = &mut self.tabs[dst].content else {
            return;
        };
        let Some(target_ix) = p.position(target) else {
            return;
        };
        let at = panes::drop_index(p.items.len(), target_ix, zone);
        p.items.splice(at..at, moved);
        p.focused = at;
        self.sync_panes(dst, cx);
        self.activate(Some(dst), window, cx);
        self.focus_pane(dst, at, window, cx);
    }

    /// The pointer moves over a tab while dragging: before or after it.
    /// `moving`: the tab that would move (none for a pane that becomes a
    /// new tab); no indicator where it would stay in place.
    fn hover_tab(
        &mut self,
        id: usize,
        accepts: bool,
        moving: Option<usize>,
        bounds: Bounds<Pixels>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let before = at.x < bounds.center().x;
        let moves = self
            .tab_index(id)
            .is_some_and(|ix| self.drop_moves(moving, drag::slot_of(ix, before)));
        let hint =
            (accepts && moves && bounds.contains(&at)).then_some(DropHint::Tab { id, before });
        self.set_hint(
            hint,
            |h| matches!(h, DropHint::Tab { id: i, .. } if i == id),
            cx,
        );
    }

    /// The pointer moves over the empty end of the tab bar while dragging:
    /// after the last tab.
    fn hover_strip_end(
        &mut self,
        accepts: bool,
        moving: Option<usize>,
        bounds: Bounds<Pixels>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let moves = self.drop_moves(moving, self.tabs.len());
        let hint = (accepts && moves && bounds.contains(&at)).then_some(DropHint::End);
        self.set_hint(hint, |h| h == DropHint::End, cx);
    }

    /// The zone of terminal `id` something was just dropped on (and the
    /// hint is over).
    fn take_pane_zone(&mut self, id: EntityId) -> Option<Zone> {
        match self.drop_hint.take() {
            Some(DropHint::Pane { id: h, zone }) if h == id => Some(zone),
            _ => None,
        }
    }

    /// A dragged pane comes from this window.
    fn owns_pane(&self, d: &DraggedPane) -> bool {
        self.find_pane(d.terminal).is_some()
    }

    /// Dropping in `slot` of the tab bar changes something: a new tab, or
    /// the tab at `moving` going somewhere else.
    fn drop_moves(&self, moving: Option<usize>, slot: usize) -> bool {
        moving.is_none_or(|from| drag::move_target(self.tabs.len(), from, slot).is_some())
    }

    /// What a drag over the tab bar is: whether this window takes it and
    /// the tab it would move (`None`: a pane that becomes a new tab).
    fn strip_drag_tab(&self, d: Option<&DraggedTab>, me: EntityId) -> (bool, Option<usize>) {
        match d.filter(|d| d.app == me) {
            Some(d) => (true, self.tab_index(d.tab)),
            None => (false, None),
        }
    }

    /// The same for a dragged pane: alone in its tab, that tab moves.
    fn strip_drag_pane(&self, d: Option<&DraggedPane>) -> (bool, Option<usize>) {
        match d.and_then(|d| self.find_pane(d.terminal)) {
            Some((tab, _)) => {
                let alone = self.tabs[tab].panes().is_some_and(|p| p.items.len() < 2);
                (true, alone.then_some(tab))
            }
            None => (false, None),
        }
    }

    /// The pointer moves over a terminal of the active tab while dragging:
    /// the side it is closest to.
    fn hover_pane(
        &mut self,
        id: EntityId,
        accepts: bool,
        bounds: Bounds<Pixels>,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hint = (accepts && bounds.contains(&at)).then(|| {
            let rel = at - bounds.origin;
            DropHint::Pane {
                id,
                zone: panes::zone_at(
                    f32::from(bounds.size.width),
                    f32::from(bounds.size.height),
                    f32::from(rel.x),
                    f32::from(rel.y),
                ),
            }
        });
        self.set_hint(
            hint,
            |h| matches!(h, DropHint::Pane { id: i, .. } if i == id),
            cx,
        );
    }

    /// Sets the drop hint (`Some`), or clears it if `owned` says the current
    /// one is this target's (`None`): every target sees every move.
    fn set_hint(
        &mut self,
        hint: Option<DropHint>,
        owned: impl Fn(DropHint) -> bool,
        cx: &mut Context<Self>,
    ) {
        let next = match hint {
            Some(h) => Some(h),
            None if self.drop_hint.is_some_and(&owned) => None,
            None => self.drop_hint,
        };
        if next != self.drop_hint {
            self.drop_hint = next;
            cx.notify();
        }
    }

    /// The slot of the tab bar a drop on tab `id` goes to (from the hint:
    /// before or after it).
    fn slot_for_tab(&self, id: usize) -> Option<usize> {
        let ix = self.tab_index(id)?;
        let before = match self.drop_hint {
            Some(DropHint::Tab { id: h, before }) if h == id => before,
            _ => true,
        };
        Some(drag::slot_of(ix, before))
    }

    /// A tab of this window that can be dropped on the terminals of the
    /// active tab (another terminal tab, or a dormant session).
    fn tab_joins_split(&self, d: &DraggedTab, me: EntityId) -> bool {
        d.app == me
            && self.tab_index(d.tab).is_some_and(|ix| {
                Some(ix) != self.active
                    && matches!(
                        self.tabs[ix].content,
                        TabContent::Terminal(_) | TabContent::Dormant { .. }
                    )
            })
    }

    /// Asks for a new name for a tab (empty: the automatic one again).
    fn rename_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(id) else {
            return;
        };
        let current = self.tabs[ix].title(cx).to_string();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("app.tab.rename_placeholder"))
                .default_value(current)
        });
        ui::focus_later(&input, window, cx);
        let weak = cx.entity().downgrade();
        let field = input.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("app.tab.rename_title"),
            t!("common.save"),
            420.,
            move |_, cx| {
                v_flex()
                    .gap_2()
                    .child(Input::new(&input))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("app.tab.rename_hint")),
                    )
                    .into_any_element()
            },
            move |_, cx| {
                let name = field.read(cx).value().trim().to_string();
                if let Some(app) = weak.upgrade() {
                    app.update(cx, |this, cx| {
                        if let Some(ix) = this.tab_index(id) {
                            this.tabs[ix].custom_title = (!name.is_empty()).then_some(name);
                            cx.notify();
                        }
                    });
                }
                true
            },
        );
    }

    /// Opens the same thing as tab `id` (its focused terminal) in a new tab.
    fn duplicate_tab(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(id) else {
            return;
        };
        let req = match &self.tabs[ix].content {
            TabContent::Terminal(p) => duplicate_request(p.focused().read(cx).kind()),
            TabContent::Sftp(s) => Some(s.read(cx).duplicate_request()),
            TabContent::Dormant { .. } => None,
        };
        match req {
            Some(req) => self.open(req, window, cx),
            None => ui::notify(window, cx, ToastKind::Info, t!("app.tab.cannot_duplicate")),
        }
    }

    pub(crate) fn select_section(
        &mut self,
        section: Section,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.section = section;
        // The sections that depend on the server refresh when entered.
        match section {
            Section::ServerSessions => self
                .server_sessions
                .update(cx, |v, cx| v.refresh(window, cx)),
            Section::Ai => self.ai.update(cx, |v, cx| v.refresh(window, cx)),
            Section::Teams => self.teams.update(cx, |v, cx| v.refresh(window, cx)),
            Section::Admin => self.admin.update(cx, |v, cx| v.refresh(window, cx)),
            Section::Settings => self.settings.update(cx, |v, cx| v.refresh(window, cx)),
            _ => {}
        }
        self.activate(None, window, cx);
    }

    // ----- State for the menus -----

    fn active_panes(&self) -> Option<&Panes> {
        self.active
            .and_then(|i| self.tabs.get(i))
            .and_then(Tab::panes)
    }

    /// Focused terminal of the active tab.
    fn focused_terminal(&self) -> Option<Entity<TerminalView>> {
        self.active_panes().map(|p| p.focused().clone())
    }

    fn menu_state(&self, cx: &App) -> MenuState {
        let panes = self.active_panes();
        let term = panes.map(|p| p.focused().read(cx));
        MenuState {
            terminal: term.is_some(),
            split: panes.is_some_and(Panes::is_split),
            writable: term.is_some_and(|t| t.writable()),
            selection: term.is_some_and(|t| t.has_selection()),
            ended: term.is_some_and(|t| t.ended()),
            host: term.is_some_and(|t| t.host().is_some()),
            duplicable: term.is_some_and(|t| duplicate_request(t.kind()).is_some())
                || self
                    .active
                    .and_then(|i| self.tabs.get(i))
                    .is_some_and(|t| matches!(t.content, TabContent::Sftp(_))),
            any_tab: !self.tabs.is_empty(),
            active_tab: self.active.is_some(),
            updates: self.updates.read(cx).status != UpdateStatus::Disabled,
        }
    }

    // ----- Actions -----

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_host_picker(PickMode::Tab, window, cx);
    }

    fn on_new_local_terminal(
        &mut self,
        _: &NewLocalTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open(OpenRequest::Shell, window, cx);
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.active {
            self.close_tab(ix, window, cx);
        }
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        let next = match self.active {
            None if !self.tabs.is_empty() => Some(0),
            Some(i) if i + 1 < self.tabs.len() => Some(i + 1),
            _ => None,
        };
        self.activate(next, window, cx);
    }

    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        let prev = match self.active {
            None if !self.tabs.is_empty() => Some(self.tabs.len() - 1),
            Some(0) => None,
            Some(i) => Some(i - 1),
            None => None,
        };
        self.activate(prev, window, cx);
    }

    fn on_move_tab_left(&mut self, _: &MoveTabLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.step_active_tab(false, cx);
    }

    fn on_move_tab_right(&mut self, _: &MoveTabRight, _: &mut Window, cx: &mut Context<Self>) {
        self.step_active_tab(true, cx);
    }

    fn on_go_home(&mut self, _: &GoHome, window: &mut Window, cx: &mut Context<Self>) {
        self.activate(None, window, cx);
    }

    fn on_toggle_copilot(
        &mut self,
        _: &ToggleCopilot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_copilot(!self.copilot_open, window, cx);
    }

    fn on_about(&mut self, _: &About, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, |d, _, cx| {
            let theme = cx.theme();
            d.w(px(420.)).child(
                v_flex()
                    .items_center()
                    .gap_2()
                    .py_2()
                    .child(
                        div()
                            .size(px(56.))
                            .rounded(px(14.))
                            .bg(theme.primary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                ui::icon(IconName::Terminal)
                                    .size(px(30.))
                                    .text_color(theme.primary_foreground),
                            ),
                    )
                    .child(div().text_xl().font_semibold().child("Termoak"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("app.about.version", version = update::current_version())),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .child(t!("settings.about.description")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("app.about.license")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .pt_2()
                            .child(
                                Button::new("about-source")
                                    .small()
                                    .icon(ui::icon(IconName::Github))
                                    .label(t!("settings.about.source_code"))
                                    .on_click(|_, _, cx| cx.open_url(menus::DOCS_URL)),
                            )
                            .child(
                                Button::new("about-close")
                                    .small()
                                    .primary()
                                    .label(t!("common.close"))
                                    .on_click(|_, window, cx| window.close_dialog(cx)),
                            ),
                    ),
            )
        });
    }

    fn on_open_settings(&mut self, _: &OpenSettings, window: &mut Window, cx: &mut Context<Self>) {
        self.select_section(Section::Settings, window, cx);
    }

    fn on_check_updates(
        &mut self,
        _: &CheckForUpdates,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.updates.read(cx).status == UpdateStatus::Disabled {
            ui::notify(window, cx, ToastKind::Info, t!("app.update.disabled"));
            return;
        }
        self.updates.update(cx, |u, cx| u.check_now(cx));
        ui::notify(window, cx, ToastKind::Info, t!("app.update.checking"));
    }

    fn on_new_host(&mut self, _: &NewHost, window: &mut Window, cx: &mut Context<Self>) {
        self.select_section(Section::Hosts, window, cx);
        self.hosts.update(cx, |h, cx| h.edit(None, window, cx));
    }

    fn on_quick_connect(&mut self, _: &QuickConnect, window: &mut Window, cx: &mut Context<Self>) {
        self.open_host_picker(PickMode::Quick, window, cx);
    }

    fn on_close_pane(&mut self, _: &ClosePane, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(tab) = self.active
            && let Some(pane) = self.active_panes().map(|p| p.focused)
        {
            self.close_pane(tab, pane, window, cx);
        }
    }

    fn on_toggle_sidebar(
        &mut self,
        _: &ToggleSidebar,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        // The sidebar is on the home screen.
        if self.active.is_some() {
            self.activate(None, window, cx);
        }
        cx.notify();
    }

    fn on_add_pane(&mut self, _: &AddPane, window: &mut Window, cx: &mut Context<Self>) {
        if self
            .active_panes()
            .is_some_and(|p| p.items.len() >= MAX_PANES)
        {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                t!("app.split.full", max = MAX_PANES),
            );
            return;
        }
        self.open_host_picker(PickMode::Pane, window, cx);
    }

    fn on_toggle_focus_mode(
        &mut self,
        _: &ToggleFocusMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active else { return };
        if let TabContent::Terminal(p) = &mut self.tabs[tab].content
            && p.is_split()
        {
            p.maximized = !p.maximized;
            let focused = p.focused;
            self.sync_panes(tab, cx);
            self.focus_pane(tab, focused, window, cx);
        }
    }

    fn on_toggle_broadcast(
        &mut self,
        _: &ToggleBroadcast,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.active else { return };
        self.toggle_broadcast(tab, window, cx);
    }

    fn toggle_broadcast(&mut self, tab: usize, window: &mut Window, cx: &mut Context<Self>) {
        let mut note = None;
        if let Some(TabContent::Terminal(p)) = self.tabs.get_mut(tab).map(|t| &mut t.content)
            && p.is_split()
        {
            p.broadcast = !p.broadcast;
            if p.broadcast {
                note = Some(panes::broadcast_count(&p.ids(), &p.excluded));
            }
        }
        self.sync_panes(tab, cx);
        if let Some(n) = note {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                tn!("app.broadcast.started", n),
            );
        }
        if let Some(focused) = self.tabs.get(tab).and_then(Tab::panes).map(|p| p.focused) {
            self.focus_pane(tab, focused, window, cx);
        }
    }

    fn change_font_size(&mut self, size: Option<f32>, delta: f32, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.font_size = size.unwrap_or(s.font_size + delta).clamp(9., 28.);
            m.save_settings(s, cx);
        });
    }

    fn on_zoom_in(&mut self, _: &ZoomIn, _: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(None, 1., cx);
    }

    fn on_zoom_out(&mut self, _: &ZoomOut, _: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(None, -1., cx);
    }

    fn on_zoom_reset(&mut self, _: &ZoomReset, _: &mut Window, cx: &mut Context<Self>) {
        self.change_font_size(Some(DEFAULT_FONT_SIZE), 0., cx);
    }

    fn on_full_screen(&mut self, _: &ToggleFullScreen, window: &mut Window, _: &mut Context<Self>) {
        window.toggle_fullscreen();
    }

    fn on_minimize(&mut self, _: &Minimize, window: &mut Window, _: &mut Context<Self>) {
        window.minimize_window();
    }

    fn on_send_snippet(&mut self, _: &SendSnippet, window: &mut Window, cx: &mut Context<Self>) {
        self.open_send_snippet(None, window, cx);
    }

    /// "Send snippet" for the active terminal (with `preselect` chosen).
    fn open_send_snippet(
        &mut self,
        preselect: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.active_panes() else {
            return;
        };
        let split = p.is_split();
        let broadcast = p.broadcast;
        let weak = cx.entity().downgrade();
        crate::views::snippets::open_send_dialog(
            self.model.clone(),
            split,
            broadcast,
            preselect,
            window,
            cx,
            move |script, all, window, cx| {
                if let Some(app) = weak.upgrade() {
                    app.update(cx, |this, cx| this.send_snippet(&script, all, window, cx));
                }
            },
        );
    }

    /// Types a snippet into the focused terminal or into every included
    /// pane of the split view.
    fn send_snippet(
        &mut self,
        script: &str,
        all: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(p) = self.active_panes() else {
            return;
        };
        let targets: Vec<Entity<TerminalView>> = if all {
            p.views()
                .filter(|v| !p.excluded.contains(&v.entity_id()))
                .cloned()
                .collect()
        } else {
            vec![p.focused().clone()]
        };
        for t in &targets {
            t.update(cx, |t, cx| t.send_snippet(script, cx));
        }
        if let Some(tab) = self.active
            && let Some(focused) = self.active_panes().map(|p| p.focused)
        {
            self.focus_pane(tab, focused, window, cx);
        }
    }

    fn on_reconnect(&mut self, _: &Reconnect, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = self.focused_terminal() {
            t.update(cx, |t, cx| t.connect(window, cx));
        }
    }

    fn on_open_sftp(&mut self, _: &OpenSftp, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.focused_terminal() else {
            return;
        };
        let (host, conn) = {
            let t = t.read(cx);
            (t.host(), t.ssh_connection())
        };
        if let Some(host_id) = host {
            self.open_sftp(host_id, conn, window, cx);
        }
    }

    fn on_duplicate(&mut self, _: &DuplicateSession, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active.map(|i| self.tabs[i].id) {
            self.duplicate_tab(id, window, cx);
        }
    }

    fn on_show_shortcuts(
        &mut self,
        _: &ShowShortcuts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mac = cfg!(target_os = "macos");
        window.open_dialog(cx, move |d, _, cx| {
            let theme = cx.theme();
            d.title(t!("app.shortcuts.title"))
                .w(px(560.))
                .child(
                    v_flex()
                        .id("shortcuts-list")
                        .max_h(px(460.))
                        .overflow_y_scroll()
                        .children(menus::shortcuts().into_iter().map(|(key, m, other)| {
                            h_flex()
                                .py_1()
                                .gap_3()
                                .border_b_1()
                                .border_color(theme.border)
                                .child(div().flex_1().min_w_0().text_sm().child(t!(key)))
                                .child(
                                    div()
                                        .flex_shrink_0()
                                        .px_2()
                                        .rounded(theme.radius)
                                        .bg(theme.muted)
                                        .font_family(ui::mono_family(cx))
                                        .text_xs()
                                        .child(if mac { m } else { other }),
                                )
                        })),
                )
                .footer(
                    h_flex().w_full().justify_end().child(
                        Button::new("shortcuts-close")
                            .label(t!("common.close"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    fn move_pane_focus(&mut self, dir: Dir, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active else { return };
        let Some(p) = self.active_panes() else { return };
        if let Some(next) = panes::neighbor(p.items.len(), p.focused, dir) {
            self.focus_pane(tab, next, window, cx);
        }
    }

    fn on_pane_left(&mut self, _: &FocusPaneLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(Dir::Left, window, cx);
    }

    fn on_pane_right(&mut self, _: &FocusPaneRight, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(Dir::Right, window, cx);
    }

    fn on_pane_up(&mut self, _: &FocusPaneUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(Dir::Up, window, cx);
    }

    fn on_pane_down(&mut self, _: &FocusPaneDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_pane_focus(Dir::Down, window, cx);
    }

    /// Host picker for a new tab (Ctrl/Cmd+T), a new pane or quick connect.
    pub(crate) fn open_host_picker(
        &mut self,
        mode: PickMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        let weak = cx.entity().downgrade();
        let picker = cx.new(|cx| HostPicker::new(model, weak, mode, window, cx));
        window.open_dialog(cx, move |d, _, _| {
            let first = picker.clone();
            d.title(match mode {
                PickMode::Tab => t!("app.picker.title"),
                PickMode::Pane => t!("app.picker.title_pane"),
                PickMode::Quick => t!("app.picker.title_quick"),
            })
            .w(px(560.))
            // Enter in the search opens the first entry (the dialog gets it
            // as "OK"; the choice itself closes it).
            .on_ok(move |_, window, cx| {
                first.update(cx, |p, cx| p.pick_first(window, cx));
                false
            })
            .child(picker.clone())
            .footer(
                h_flex().w_full().justify_end().child(
                    Button::new("picker-close")
                        .label(t!("common.cancel"))
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                ),
            )
        });
    }

    // ----- Rendering -----

    /// Right-click menu of a tab.
    fn tab_menu(
        menu: PopupMenu,
        weak: gpui::WeakEntity<Self>,
        tab_id: usize,
        is_terminal: bool,
        can_duplicate: bool,
        others: Vec<(usize, SharedString)>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let (w1, w2, w3, w4) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
        let (w5, w6) = (weak.clone(), weak.clone());
        // Where the tab is, to enable moving it left or right.
        let (ix, len) = weak
            .upgrade()
            .map(|a| {
                let a = a.read(cx);
                (a.tab_index(tab_id), a.tabs.len())
            })
            .unwrap_or((None, 0));
        let left = ix.and_then(|i| drag::step_slot(len, i, false));
        let right = ix.and_then(|i| drag::step_slot(len, i, true));
        let menu = menu
            .item(
                PopupMenuItem::new(t!("app.tab.rename"))
                    .icon(ui::icon(IconName::Pencil))
                    .on_click(move |_, window, cx| {
                        if let Some(app) = w1.upgrade() {
                            app.update(cx, |this, cx| this.rename_tab(tab_id, window, cx));
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t!("app.tab.duplicate"))
                    .icon(ui::icon(IconName::CopyPlus))
                    .disabled(!can_duplicate)
                    .on_click(move |_, window, cx| {
                        if let Some(app) = w2.upgrade() {
                            app.update(cx, |this, cx| this.duplicate_tab(tab_id, window, cx));
                        }
                    }),
            );
        let menu = if is_terminal && !others.is_empty() {
            menu.separator().submenu_with_icon(
                Some(ui::icon(IconName::LayoutGrid)),
                t!("app.tab.merge_into"),
                window,
                cx,
                move |mut sub, _, _| {
                    for (other, title) in others.clone() {
                        let w = weak.clone();
                        sub = sub.item(PopupMenuItem::new(title).on_click(move |_, window, cx| {
                            if let Some(app) = w.upgrade() {
                                app.update(cx, |this, cx| {
                                    this.merge_tabs(tab_id, other, window, cx)
                                });
                            }
                        }));
                    }
                    sub
                },
            )
        } else {
            menu
        };
        menu.separator()
            .item(
                PopupMenuItem::new(t!("app.tab.move_left"))
                    .icon(ui::icon(IconName::ArrowLeft))
                    .disabled(left.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(app) = w5.upgrade() {
                            app.update(cx, |this, cx| {
                                if let (Some(ix), Some(slot)) = (this.tab_index(tab_id), left) {
                                    this.move_tab(ix, slot, cx);
                                }
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t!("app.tab.move_right"))
                    .icon(ui::icon(IconName::ArrowRight))
                    .disabled(right.is_none())
                    .on_click(move |_, _, cx| {
                        if let Some(app) = w6.upgrade() {
                            app.update(cx, |this, cx| {
                                if let (Some(ix), Some(slot)) = (this.tab_index(tab_id), right) {
                                    this.move_tab(ix, slot, cx);
                                }
                            });
                        }
                    }),
            )
            .separator()
            .item(
                PopupMenuItem::new(t!("app.tab.close"))
                    .icon(ui::icon(IconName::X))
                    .on_click(move |_, window, cx| {
                        if let Some(app) = w3.upgrade() {
                            app.update(cx, |this, cx| {
                                if let Some(ix) = this.tab_index(tab_id) {
                                    this.close_tab(ix, window, cx);
                                }
                            });
                        }
                    }),
            )
            .item(
                PopupMenuItem::new(t!("app.tab.close_others")).on_click(move |_, window, cx| {
                    if let Some(app) = w4.upgrade() {
                        app.update(cx, |this, cx| this.close_other_tabs(tab_id, window, cx));
                    }
                }),
            )
    }

    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let dragging = cx.has_active_drag();
        let hint = if dragging { self.drop_hint } else { None };
        let me = cx.entity_id();
        let last_tab = self.tabs.last().map(|t| t.id);
        let theme = cx.theme();
        let primary = theme.primary;
        let home_active = self.active.is_none();
        let weak = cx.entity().downgrade();
        let terminal_tabs: Vec<(usize, SharedString)> = self
            .tabs
            .iter()
            .filter(|t| matches!(t.content, TabContent::Terminal(_)))
            .map(|t| (t.id, t.title(cx)))
            .collect();
        let mut strip = h_flex()
            .id("tab-strip")
            .h_full()
            .flex_1()
            .min_w_0()
            .gap_1()
            .items_center()
            .overflow_x_scroll()
            .child(
                h_flex()
                    .id("tab-home")
                    // The tabs are in the title bar: without this, on Windows
                    // the window drag area takes the click and it never
                    // reaches the tab.
                    .block_mouse_except_scroll()
                    // And without passing the press to the title bar: it uses
                    // it to start moving the window at the first pixel of
                    // movement, and on Windows that swallows the click.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .h(px(28.))
                    .px_3()
                    .gap_2()
                    .items_center()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(home_active, |this| {
                        this.bg(theme.tab_active)
                            .text_color(theme.tab_active_foreground)
                    })
                    .when(!home_active, |this| {
                        this.text_color(theme.tab_foreground)
                            .hover(|s| s.bg(theme.secondary_hover))
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.activate(None, window, cx)
                    }))
                    .child(ui::icon(IconName::LayoutDashboard).size(px(14.)))
                    .child(div().text_sm().font_medium().child(t!("app.home"))),
            );
        for (ix, tab) in self.tabs.iter().enumerate() {
            let active = self.active == Some(ix);
            let title = tab.title(cx);
            let mut logo = None;
            let (icon, dot, count) = match &tab.content {
                TabContent::Terminal(p) => {
                    let t = p.focused().read(cx);
                    // A single host terminal shows the host's logo.
                    if !p.is_split() {
                        logo = t.host_logo(cx);
                    }
                    (
                        if p.is_split() {
                            IconName::LayoutGrid
                        } else if matches!(t.kind(), TermKind::Shell) {
                            IconName::Laptop
                        } else {
                            IconName::SquareTerminal
                        },
                        Some(match t.state() {
                            TermState::Running => theme.success,
                            TermState::Connecting(_) => theme.warning,
                            TermState::Failed(_) => theme.danger,
                            TermState::Closed(_) => theme.muted_foreground,
                        }),
                        p.is_split().then_some((p.items.len(), p.broadcast)),
                    )
                }
                TabContent::Sftp(_) => (IconName::FolderOpen, None, None),
                TabContent::Dormant { .. } => (IconName::Cloud, None, None),
            };
            let tab_id = tab.id;
            let is_terminal = matches!(tab.content, TabContent::Terminal(_));
            let can_duplicate = !matches!(tab.content, TabContent::Dormant { .. });
            let others: Vec<(usize, SharedString)> = terminal_tabs
                .iter()
                .filter(|(id, _)| *id != tab_id)
                .cloned()
                .collect();
            let weak = weak.clone();
            let dragged = DraggedTab {
                app: me,
                tab: tab_id,
                title: title.clone(),
                icon,
            };
            // Drop indicator: a bar in the gap before or after the tab.
            let bar_before = hint
                == Some(DropHint::Tab {
                    id: tab_id,
                    before: true,
                });
            let bar_after =
                hint == Some(DropHint::Tab {
                    id: tab_id,
                    before: false,
                }) || (hint == Some(DropHint::End) && last_tab == Some(tab_id));
            let bar = move || {
                div()
                    .absolute()
                    .top(px(2.))
                    .bottom(px(2.))
                    .w(px(2.))
                    .rounded_full()
                    .bg(primary)
            };
            strip = strip.child(
                h_flex()
                    .id(("tab", tab.id))
                    .relative()
                    .block_mouse_except_scroll()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .h(px(28.))
                    .max_w(px(240.))
                    .pl_3()
                    .pr_1()
                    .gap_2()
                    .items_center()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .when(active, |this| {
                        this.bg(theme.tab_active)
                            .text_color(theme.tab_active_foreground)
                    })
                    .when(!active, |this| {
                        this.text_color(theme.tab_foreground)
                            .hover(|s| s.bg(theme.secondary_hover))
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let ix = this.tab_index(tab_id);
                        if ix.is_some() {
                            this.activate(ix, window, cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, window, cx| {
                            if let Some(ix) = this.tab_index(tab_id) {
                                this.close_tab(ix, window, cx);
                            }
                        }),
                    )
                    .child(logo.unwrap_or_else(|| ui::icon(icon)).size(px(14.)))
                    .when_some(dot, |this, c| {
                        this.child(div().size(px(6.)).rounded_full().bg(c))
                    })
                    .child(
                        div()
                            .text_sm()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(title),
                    )
                    .when_some(count, |this, (n, broadcast)| {
                        this.child(
                            div()
                                .flex_shrink_0()
                                .px_1()
                                .rounded(theme.radius)
                                .text_xs()
                                .bg(if broadcast {
                                    theme.warning
                                } else {
                                    theme.muted
                                })
                                .text_color(if broadcast {
                                    theme.warning_foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .child(n.to_string()),
                        )
                    })
                    .child(
                        Button::new(("tab-close", tab.id))
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::X))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                cx.stop_propagation();
                                if let Some(ix) = this.tab_index(tab_id) {
                                    this.close_tab(ix, window, cx);
                                }
                            })),
                    )
                    // Dragged to another place of the bar, or onto a
                    // terminal to make a split view.
                    .on_drag(dragged, |d, _, _, cx| {
                        cx.new(|_| DragPreview::new(d.title.clone(), d.icon))
                    })
                    .on_drag_move(
                        cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                            let d = e.dragged_item().downcast_ref::<DraggedTab>();
                            let (ok, moving) = this.strip_drag_tab(d, cx.entity_id());
                            this.hover_tab(tab_id, ok, moving, e.bounds, e.event.position, cx);
                        }),
                    )
                    .on_drag_move(cx.listener(
                        move |this, e: &DragMoveEvent<DraggedPane>, _, cx| {
                            let d = e.dragged_item().downcast_ref::<DraggedPane>();
                            let (ok, moving) = this.strip_drag_pane(d);
                            this.hover_tab(tab_id, ok, moving, e.bounds, e.event.position, cx);
                        },
                    ))
                    .on_drop(cx.listener(move |this, d: &DraggedTab, _, cx| {
                        let slot = this.slot_for_tab(tab_id);
                        this.drop_hint = None;
                        if d.app == cx.entity_id()
                            && let (Some(from), Some(slot)) = (this.tab_index(d.tab), slot)
                        {
                            this.move_tab(from, slot, cx);
                        }
                        cx.notify();
                    }))
                    .on_drop(cx.listener(move |this, d: &DraggedPane, window, cx| {
                        let slot = this.slot_for_tab(tab_id);
                        this.drop_hint = None;
                        if let Some(slot) = slot {
                            this.pane_to_tab(d.terminal, slot, window, cx);
                        }
                        cx.notify();
                    }))
                    // Files from the system file manager: to its terminal
                    // (upload) or SFTP browser.
                    .on_drop(
                        cx.listener(move |this, paths: &gpui::ExternalPaths, window, cx| {
                            this.drop_files_on_tab(tab_id, paths.paths().to_vec(), window, cx);
                        }),
                    )
                    .when(bar_before, |this| this.child(bar().left(px(-3.))))
                    .when(bar_after, |this| this.child(bar().right(px(-3.))))
                    .context_menu(move |menu, window, cx| {
                        Self::tab_menu(
                            menu,
                            weak.clone(),
                            tab_id,
                            is_terminal,
                            can_duplicate,
                            others.clone(),
                            window,
                            cx,
                        )
                    }),
            );
        }
        // The + and the empty rest of the bar: a drop there goes after the
        // last tab. While dragging it takes the mouse (otherwise the empty
        // bar moves the window).
        strip.child(
            h_flex()
                .id("tab-strip-end")
                .flex_1()
                .h_full()
                .min_w(px(56.))
                .items_center()
                .when(dragging, |this| this.block_mouse_except_scroll())
                .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                    let d = e.dragged_item().downcast_ref::<DraggedTab>();
                    let (ok, moving) = this.strip_drag_tab(d, cx.entity_id());
                    this.hover_strip_end(ok, moving, e.bounds, e.event.position, cx);
                }))
                .on_drag_move(cx.listener(|this, e: &DragMoveEvent<DraggedPane>, _, cx| {
                    let d = e.dragged_item().downcast_ref::<DraggedPane>();
                    let (ok, moving) = this.strip_drag_pane(d);
                    this.hover_strip_end(ok, moving, e.bounds, e.event.position, cx);
                }))
                .on_drop(cx.listener(|this, d: &DraggedTab, _, cx| {
                    this.drop_hint = None;
                    if d.app == cx.entity_id()
                        && let Some(from) = this.tab_index(d.tab)
                    {
                        let len = this.tabs.len();
                        this.move_tab(from, len, cx);
                    }
                    cx.notify();
                }))
                .on_drop(cx.listener(|this, d: &DraggedPane, window, cx| {
                    this.drop_hint = None;
                    let len = this.tabs.len();
                    this.pane_to_tab(d.terminal, len, window, cx);
                    cx.notify();
                }))
                .child(
                    div()
                        .id("tab-new-area")
                        .block_mouse_except_scroll()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(
                            Button::new("tab-new")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::Plus))
                                .tooltip(t!("app.new_tab"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.open_host_picker(PickMode::Tab, window, cx)
                                })),
                        ),
                ),
        )
    }

    /// ☰ button with the application menus (Windows and Linux; macOS has
    /// the menu bar).
    fn render_app_menu(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let weak = cx.entity().downgrade();
        div()
            .id("app-menu-area")
            .block_mouse_except_scroll()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .pl_1()
            .child(
                Button::new("app-menu")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Menu))
                    .tooltip(t!("app.menu_button"))
                    .dropdown_menu(move |mut menu, window, cx| {
                        let Some(app) = weak.upgrade() else {
                            return menu;
                        };
                        let (state, context) = {
                            let a = app.read(cx);
                            let context = a
                                .focused_terminal()
                                .map(|t| t.focus_handle(cx))
                                .unwrap_or_else(|| a.focus.clone());
                            (a.menu_state(cx), context)
                        };
                        menu = menu.action_context(context.clone());
                        for (i, spec) in menus::spec(false).iter().enumerate() {
                            let context = context.clone();
                            menu = menu.submenu(
                                menus::title(spec),
                                window,
                                cx,
                                move |mut sub, _, _| {
                                    sub = sub.action_context(context.clone()).min_w(px(240.));
                                    let specs = menus::spec(false);
                                    for entry in &specs[i].entries {
                                        sub = match entry {
                                            menus::Entry::Separator => sub.separator(),
                                            menus::Entry::Item {
                                                label,
                                                action,
                                                need,
                                            } => sub.menu_with_disabled(
                                                t!(*label),
                                                action.boxed_clone(),
                                                !state.allows(*need),
                                            ),
                                        };
                                    }
                                    sub
                                },
                            );
                        }
                        menu
                    }),
            )
    }

    /// Workspace buttons of the title bar: add a terminal, focus mode and
    /// broadcast.
    fn render_workspace_buttons(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let panes = self.active_panes();
        let split = panes.is_some_and(Panes::is_split);
        let maximized = panes.is_some_and(|p| p.maximized);
        let broadcast = panes.is_some_and(|p| p.broadcast);
        let on_terminal = panes.is_some();
        let mac = cfg!(target_os = "macos");
        h_flex()
            .id("workspace-buttons")
            .block_mouse_except_scroll()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .gap_1()
            .when(on_terminal, |this| {
                this.child(
                    Button::new("ws-add-pane")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::LayoutGrid))
                        .tooltip(t!(
                            "app.split.add_tooltip",
                            shortcut = if mac { "⌘D" } else { "Ctrl+Shift+D" }
                        ))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.on_add_pane(&AddPane, window, cx)
                        })),
                )
            })
            .when(split, |this| {
                this.child(
                    Button::new("ws-focus-mode")
                        .small()
                        .map(|b| if maximized { b.primary() } else { b.ghost() })
                        .icon(ui::icon(if maximized {
                            IconName::Minimize2
                        } else {
                            IconName::Maximize2
                        }))
                        .tooltip(t!(
                            "app.split.focus_tooltip",
                            shortcut = if mac { "⌘⇧M" } else { "Ctrl+Shift+M" }
                        ))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.on_toggle_focus_mode(&ToggleFocusMode, window, cx)
                        })),
                )
                .child(
                    Button::new("ws-broadcast")
                        .small()
                        .map(|b| if broadcast { b.warning() } else { b.ghost() })
                        .icon(ui::icon(IconName::RadioTower))
                        .label(t!("app.broadcast.button"))
                        .tooltip(t!(
                            "app.broadcast.tooltip",
                            shortcut = if mac { "⌘B" } else { "Ctrl+Alt+B" }
                        ))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.on_toggle_broadcast(&ToggleBroadcast, window, cx)
                        })),
                )
            })
    }

    /// Banner over a split view while broadcasting.
    fn render_broadcast_banner(&self, tab: usize, p: &Panes, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let n = panes::broadcast_count(&p.ids(), &p.excluded);
        h_flex()
            .w_full()
            .px_3()
            .py_1()
            .gap_2()
            .items_center()
            .bg(theme.warning)
            .text_color(theme.warning_foreground)
            .child(ui::icon(IconName::RadioTower).size(px(14.)))
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(tn!("app.broadcast.banner", n)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .child(t!("app.broadcast.banner_hint")),
            )
            .child(
                Button::new("broadcast-stop")
                    .xsmall()
                    .label(t!("app.broadcast.stop"))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.toggle_broadcast(tab, window, cx)
                    })),
            )
            .into_any_element()
    }

    /// A terminal as a drop target: a tab dropped on one of its sides joins
    /// it in a split view there, and so does a pane of the split view
    /// (which moves). While dragging, the side the pointer is closest to is
    /// highlighted.
    fn pane_drop_target(
        &self,
        el: gpui::Stateful<gpui::Div>,
        id: EntityId,
        hint: Option<DropHint>,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let zone = match hint {
            Some(DropHint::Pane { id: h, zone }) if h == id => Some(zone),
            _ => None,
        };
        let color = cx.theme().primary;
        el.on_drag_move(
            cx.listener(move |this, e: &DragMoveEvent<DraggedTab>, _, cx| {
                let me = cx.entity_id();
                let ok = e
                    .dragged_item()
                    .downcast_ref::<DraggedTab>()
                    .is_some_and(|d| this.tab_joins_split(d, me));
                this.hover_pane(id, ok, e.bounds, e.event.position, cx);
            }),
        )
        .on_drag_move(
            cx.listener(move |this, e: &DragMoveEvent<DraggedPane>, _, cx| {
                let ok = e
                    .dragged_item()
                    .downcast_ref::<DraggedPane>()
                    .is_some_and(|d| d.terminal != id && this.owns_pane(d));
                this.hover_pane(id, ok, e.bounds, e.event.position, cx);
            }),
        )
        .on_drop(cx.listener(move |this, d: &DraggedTab, window, cx| {
            let zone = this.take_pane_zone(id);
            if let Some(zone) = zone
                && this.tab_joins_split(d, cx.entity_id())
            {
                this.drop_on_pane(DropSource::Tab(d.tab), id, zone, window, cx);
            }
            cx.notify();
        }))
        .on_drop(cx.listener(move |this, d: &DraggedPane, window, cx| {
            let zone = this.take_pane_zone(id);
            if let Some(zone) = zone
                && this.owns_pane(d)
            {
                this.drop_on_pane(DropSource::Pane(d.terminal), id, zone, window, cx);
            }
            cx.notify();
        }))
        .when_some(zone, |el, zone| {
            // Half of the terminal on that side, where the new one will be.
            let area = div()
                .absolute()
                .rounded(px(4.))
                .border_2()
                .border_color(color)
                .bg(color.opacity(0.18));
            el.child(match zone {
                Zone::Left => area.top_0().bottom_0().left_0().w(relative(0.5)),
                Zone::Right => area.top_0().bottom_0().right_0().w(relative(0.5)),
                Zone::Top => area.left_0().right_0().top_0().h(relative(0.5)),
                Zone::Bottom => area.left_0().right_0().bottom_0().h(relative(0.5)),
            })
        })
    }

    /// Terminals of a tab: alone, in a grid or in focus mode.
    fn render_panes(&self, tab: usize, p: &Panes, cx: &Context<Self>) -> AnyElement {
        let hint = if cx.has_active_drag() {
            self.drop_hint
        } else {
            None
        };
        if !p.is_split() {
            let view = p.focused().clone();
            let id = view.entity_id();
            let single = div().id("pane-single").relative().size_full().child(view);
            return self
                .pane_drop_target(single, id, hint, cx)
                .into_any_element();
        }
        let theme = cx.theme();
        let pane_box = |i: usize| {
            let pane = &p.items[i];
            let id = pane.view.entity_id();
            let included = p.broadcast && !p.excluded.contains(&id);
            let color = if i == p.focused {
                theme.primary
            } else if included {
                theme.warning
            } else {
                theme.border
            };
            let el = div()
                .id(("pane", i))
                .relative()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .rounded(theme.radius)
                .border_2()
                .border_color(color)
                .child(pane.view.clone());
            self.pane_drop_target(el, id, hint, cx)
        };
        let body: AnyElement = if p.maximized {
            let focused = p.focused.min(p.items.len() - 1);
            h_flex()
                .size_full()
                .gap_1()
                .child(pane_box(focused).flex_1().h_full())
                .child(
                    v_flex()
                        .w(relative(0.28))
                        .flex_shrink_0()
                        .h_full()
                        .gap_1()
                        .children(
                            (0..p.items.len())
                                .filter(|i| *i != focused)
                                .map(|i| pane_box(i).flex_1().w_full()),
                        ),
                )
                .into_any_element()
        } else {
            let mut start = 0;
            v_flex()
                .size_full()
                .gap_1()
                .children(panes::grid_rows(p.items.len()).into_iter().map(|count| {
                    let row =
                        h_flex().flex_1().min_h_0().w_full().gap_1().children(
                            (start..start + count).map(|i| pane_box(i).flex_1().h_full()),
                        );
                    start += count;
                    row
                }))
                .into_any_element()
        };
        v_flex()
            .size_full()
            .when(p.broadcast, |this| {
                this.child(self.render_broadcast_banner(tab, p, cx))
            })
            .child(div().flex_1().min_h_0().w_full().p_1().child(body))
            .into_any_element()
    }

    // ----- Workspaces and the last session -----

    /// Reopens the last session's tabs once the hosts and the saved session
    /// are read.
    fn try_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restore != Restore::Waiting {
            return;
        }
        if !self.model.read(cx).loaded || !self.workspaces.read(cx).session_ready() {
            return;
        }
        self.restore = Restore::Done;
        let (layout, more) = self.workspaces.update(cx, |w, _| w.take_session_window());
        // The other windows the last session had.
        if self.primary {
            for _ in 0..more {
                self.workspaces.update(cx, |w, _| w.expect_session_window());
                cx.defer(windows::open_new);
            }
        }
        match layout {
            Some(layout) if !layout.is_empty() => {
                self.open_layout(layout, Mode::Restore, window, cx)
            }
            // Nothing to reopen: the running server sessions, as usual.
            _ => self.cloud_tabs_after_restore(window, cx),
        }
    }

    /// After reopening the last session, the running server sessions that
    /// are not open yet come as dormant tabs (first window).
    fn cloud_tabs_after_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.read(cx).logged_in() {
            let add = windows::is_first_window(window, cx);
            self.restore_cloud_tabs(add, window, cx);
        }
    }

    /// Opens the tabs of a layout (the last session or a workspace): first
    /// it finds out which of its hosts still exist on this device (some may
    /// be in accounts or vaults out of sight).
    fn open_layout(
        &mut self,
        layout: Layout,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (ws, loaded) = {
            let m = self.model.read(cx);
            let loaded: HashSet<Id> = m.hosts.iter().map(|h| h.data.id).collect();
            (m.ws.clone(), loaded)
        };
        let unknown: Vec<Id> = layout
            .host_ids()
            .into_iter()
            .filter(|h| !loaded.contains(h))
            .collect();
        runtime::run_in(
            cx,
            window,
            async move {
                let mut found = Vec::new();
                for h in unknown {
                    if ws.locate(h).await.is_ok() {
                        found.push(h);
                    }
                }
                Ok::<_, String>(found)
            },
            move |this, res, window, cx| {
                let mut exists = loaded;
                exists.extend(res.unwrap_or_default());
                this.apply_layout(&layout, mode, &exists, window, cx);
            },
        );
    }

    /// How a planned terminal opens (`None`: it cannot, already said).
    fn request_of(
        &self,
        pane: PlanPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<OpenRequest> {
        Some(match pane {
            PlanPane::Ssh(host_id) => return self.route_local(host_id, window, cx),
            PlanPane::Attach {
                session_id,
                account,
                title,
            } => OpenRequest::Attach {
                session_id,
                title,
                account,
            },
            PlanPane::NewServer(host_id) => OpenRequest::Server { host_id },
            PlanPane::Shell => OpenRequest::Shell,
            PlanPane::Serial { path, baud } => OpenRequest::Serial(SerialParams { path, baud }),
        })
    }

    fn apply_layout(
        &mut self,
        layout: &Layout,
        mode: Mode,
        exists: &HashSet<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open_sessions: Vec<Id> = self
            .tabs
            .iter()
            .flat_map(|t| t.server_sessions(cx))
            .collect();
        let plan = {
            let m = self.model.read(cx);
            let host_exists = |h: Id| exists.contains(&h);
            let signed_in = |a: Option<Id>| m.api_of(a).is_some();
            saved::plan(
                layout,
                mode,
                &saved::Env {
                    host_exists: &host_exists,
                    signed_in: &signed_in,
                    running: self.running_sessions.as_deref(),
                    open_sessions: &open_sessions,
                },
            )
        };
        // Planned tab → index of the tab opened for it.
        let mut opened: Vec<Option<usize>> = Vec::with_capacity(plan.tabs.len());
        for tab in plan.tabs {
            let content = match tab {
                PlanTab::Sftp(host_id) => {
                    let model = self.model.clone();
                    let view = cx.new(|cx| SftpView::new(model, host_id, None, window, cx));
                    Some((TabContent::Sftp(view), None))
                }
                PlanTab::Dormant {
                    session_id,
                    account,
                    title,
                } => Some((
                    TabContent::Dormant {
                        session_id,
                        title,
                        account,
                    },
                    None,
                )),
                PlanTab::Terminal {
                    panes,
                    focused,
                    maximized,
                    title,
                } => {
                    let reqs: Vec<OpenRequest> = panes
                        .into_iter()
                        .filter_map(|p| self.request_of(p, window, cx))
                        .collect();
                    let views: Vec<Entity<TerminalView>> = reqs
                        .iter()
                        .filter_map(|r| self.make_terminal(r, window, cx))
                        .collect();
                    if views.is_empty() {
                        None
                    } else {
                        let items: Vec<Pane> = views
                            .into_iter()
                            .map(|v| self.new_pane(v, window, cx))
                            .collect();
                        let mut p = Panes::new(items);
                        p.focused = focused.min(p.items.len() - 1);
                        p.maximized = maximized && p.is_split();
                        Some((TabContent::Terminal(p), title))
                    }
                }
            };
            match content {
                Some((content, custom_title)) => {
                    let id = self.next_id;
                    self.next_id += 1;
                    self.tabs.push(Tab {
                        id,
                        content,
                        custom_title,
                    });
                    let ix = self.tabs.len() - 1;
                    self.sync_panes(ix, cx);
                    opened.push(Some(ix));
                }
                None => opened.push(None),
            }
        }
        if let Some(ix) = plan.active.and_then(|a| opened.get(a).copied().flatten()) {
            self.activate(Some(ix), window, cx);
        }
        if plan.skipped > 0 {
            ui::notify(
                window,
                cx,
                ToastKind::Warning,
                match mode {
                    Mode::Restore => tn!("workspaces.restore_skipped", plan.skipped),
                    Mode::Open => tn!("workspaces.open_skipped", plan.skipped),
                },
            );
        }
        if mode == Mode::Restore {
            self.cloud_tabs_after_restore(window, cx);
        }
        cx.notify();
    }

    /// The tabs of the window as they would be saved.
    fn layout_snapshot(&self, cx: &App) -> Layout {
        let m = self.model.read(cx);
        let current = m.current_account;
        let mut tabs = Vec::new();
        let mut active = None;
        for (i, tab) in self.tabs.iter().enumerate() {
            let saved = match &tab.content {
                TabContent::Terminal(p) => {
                    let mut panes = Vec::new();
                    let mut focused = 0;
                    for (j, pane) in p.items.iter().enumerate() {
                        let v = pane.view.read(cx);
                        if let Some(sp) = saved::pane_of(v.kind(), v.opened_as(), current) {
                            if j == p.focused {
                                focused = panes.len();
                            }
                            panes.push(sp);
                        }
                    }
                    (!panes.is_empty()).then(|| SavedTab::Terminal {
                        panes,
                        focused,
                        maximized: p.maximized,
                        title: tab.custom_title.clone(),
                    })
                }
                TabContent::Sftp(s) => match s.read(cx).duplicate_request() {
                    OpenRequest::Sftp { host_id, .. } => Some(SavedTab::Sftp { host_id }),
                    _ => None,
                },
                TabContent::Dormant {
                    session_id,
                    title,
                    account,
                } => Some(SavedTab::Terminal {
                    panes: vec![SavedPane::Server {
                        host_id: None,
                        session_id: Some(*session_id),
                        account: Some(*account),
                        title: title.clone(),
                    }],
                    focused: 0,
                    maximized: false,
                    title: tab.custom_title.clone(),
                }),
            };
            if let Some(t) = saved {
                if self.active == Some(i) {
                    active = Some(tabs.len());
                }
                tabs.push(t);
            }
        }
        Layout { tabs, active }
    }

    /// Keeps the saved session up to date with the tabs (when they change).
    fn persist_session(&self, cx: &mut Context<Self>) {
        if self.restore != Restore::Done || !self.model.read(cx).settings.reopen_tabs {
            return;
        }
        let layout = self.layout_snapshot(cx);
        let me = cx.entity_id();
        self.workspaces
            .update(cx, |w, _| w.window_changed(me, layout));
    }

    /// Opens a saved workspace (its tabs are added to the window).
    pub(crate) fn open_workspace(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = self.workspaces.read(cx).get(id).map(|w| w.layout.clone()) else {
            return;
        };
        if layout.is_empty() {
            ui::notify(window, cx, ToastKind::Info, t!("workspaces.empty"));
            return;
        }
        self.open_layout(layout, Mode::Open, window, cx);
    }

    /// "Save tabs as workspace…": asks for a name.
    fn save_workspace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let layout = self.layout_snapshot(cx);
        if layout.is_empty() {
            ui::notify(
                window,
                cx,
                ToastKind::Info,
                t!("workspaces.nothing_to_save"),
            );
            return;
        }
        let default = saved::next_name(
            &t!("workspaces.default_name"),
            &self.workspaces.read(cx).list,
        );
        let workspaces = self.workspaces.clone();
        let count = layout.tabs.len();
        saved::ask_name(
            window,
            cx,
            t!("workspaces.save_title"),
            default,
            Some(tn!("workspaces.save_hint", count)),
            move |name, window, cx| {
                let w = saved::SavedWorkspace::new(name.clone(), layout.clone());
                workspaces.update(cx, |list, cx| list.add(w, cx));
                ui::success(window, cx, t!("workspaces.saved", name = name));
            },
        );
    }

    /// "Replace with the current tabs".
    fn replace_workspace(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let layout = self.layout_snapshot(cx);
        if layout.is_empty() {
            ui::notify(
                window,
                cx,
                ToastKind::Info,
                t!("workspaces.nothing_to_save"),
            );
            return;
        }
        let name = self
            .workspaces
            .read(cx)
            .get(id)
            .map(|w| w.name.clone())
            .unwrap_or_default();
        self.workspaces
            .update(cx, |w, cx| w.set_layout(id, layout, cx));
        ui::success(window, cx, t!("workspaces.saved", name = name));
    }

    fn rename_workspace(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspaces.read(cx).get(id).map(|w| w.name.clone()) else {
            return;
        };
        let workspaces = self.workspaces.clone();
        saved::ask_name(
            window,
            cx,
            t!("workspaces.rename_title"),
            name,
            None,
            move |name, _, cx| workspaces.update(cx, |w, cx| w.rename(id, name, cx)),
        );
    }

    fn delete_workspace(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.workspaces.read(cx).get(id).map(|w| w.name.clone()) else {
            return;
        };
        let workspaces = self.workspaces.clone();
        ui::confirm(
            window,
            cx,
            t!("workspaces.delete_title"),
            t!("workspaces.delete_message", name = name),
            t!("common.delete"),
            true,
            move |_, cx| workspaces.update(cx, |w, cx| w.remove(id, cx)),
        );
    }

    /// Menu of a workspace (right click in the sidebar).
    fn workspace_menu(
        menu: PopupMenu,
        weak: gpui::WeakEntity<Self>,
        id: Id,
        has_tabs: bool,
    ) -> PopupMenu {
        let act = move |f: fn(&mut AppView, Id, &mut Window, &mut Context<AppView>)| {
            let weak = weak.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                if let Some(app) = weak.upgrade() {
                    app.update(cx, |this, cx| f(this, id, window, cx));
                }
            }
        };
        menu.item(
            PopupMenuItem::new(t!("workspaces.menu.open"))
                .icon(ui::icon(IconName::SquareTerminal))
                .on_click(act(Self::open_workspace)),
        )
        .item(
            PopupMenuItem::new(t!("workspaces.menu.replace"))
                .icon(ui::icon(IconName::Save))
                .disabled(!has_tabs)
                .on_click(act(Self::replace_workspace)),
        )
        .item(
            PopupMenuItem::new(t!("workspaces.menu.rename"))
                .icon(ui::icon(IconName::Pencil))
                .on_click(act(Self::rename_workspace)),
        )
        .separator()
        .item(
            PopupMenuItem::new(t!("workspaces.menu.delete"))
                .icon(ui::icon(IconName::Trash))
                .on_click(act(Self::delete_workspace)),
        )
    }

    /// File → Open workspace…: the saved workspaces in a dialog.
    fn open_workspace_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list: Vec<(Id, String, usize)> = self
            .workspaces
            .read(cx)
            .list
            .iter()
            .map(|w| (w.id, w.name.clone(), w.layout.tabs.len()))
            .collect();
        if list.is_empty() {
            ui::notify(window, cx, ToastKind::Info, t!("workspaces.none_yet"));
            return;
        }
        let weak = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            dialog
                .title(t!("workspaces.open_title"))
                .w(px(420.))
                .child(v_flex().gap_1().children(list.iter().enumerate().map(
                    |(i, (id, name, tabs))| {
                        let (id, weak) = (*id, weak.clone());
                        h_flex()
                            .id(("open-workspace", i))
                            .w_full()
                            .px_3()
                            .py_2()
                            .gap_3()
                            .items_center()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().secondary_hover))
                            .child(ui::icon(IconName::LayoutPanelLeft).size(px(16.)))
                            .child(div().flex_1().min_w_0().child(name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(tn!("workspaces.tab_count", *tabs)),
                            )
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                if let Some(app) = weak.upgrade() {
                                    app.update(cx, |this, cx| this.open_workspace(id, window, cx));
                                }
                            })
                    },
                )))
        });
    }

    fn on_save_workspace(
        &mut self,
        _: &SaveWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.save_workspace(window, cx);
    }

    fn on_open_workspace(
        &mut self,
        _: &OpenWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_workspace_picker(window, cx);
    }

    /// The Workspaces group of the sidebar: one click opens one.
    fn render_workspaces_group(&self, cx: &mut Context<Self>) -> SidebarGroup<SidebarMenu> {
        let has_tabs = !self.tabs.is_empty();
        let weak = cx.entity().downgrade();
        let mut menu = SidebarMenu::new();
        for w in &self.workspaces.read(cx).list {
            let id = w.id;
            let (w_click, w_menu) = (weak.clone(), weak.clone());
            menu = menu.child(
                SidebarMenuItem::new(w.name.clone())
                    .icon(ui::icon(IconName::LayoutPanelLeft).size(px(18.)))
                    .min_h(px(34.))
                    .px_3()
                    .gap_x_3()
                    .on_click(move |_, window, cx| {
                        if let Some(app) = w_click.upgrade() {
                            app.update(cx, |this, cx| this.open_workspace(id, window, cx));
                        }
                    })
                    .context_menu(move |menu, _, _| {
                        Self::workspace_menu(menu, w_menu.clone(), id, has_tabs)
                    }),
            );
        }
        menu = menu.child(
            SidebarMenuItem::new(t!("workspaces.save_current"))
                .icon(ui::icon(IconName::BookmarkPlus).size(px(18.)))
                .min_h(px(34.))
                .px_3()
                .gap_x_3()
                .disable(!has_tabs)
                .on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.save_workspace(window, cx)),
                ),
        );
        SidebarGroup::new(t!("app.group.workspaces")).child(menu)
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let collapsed = self.sidebar_collapsed;
        let model = self.model.read(cx);
        let logged_in = model.logged_in();
        let is_admin = model.is_admin();
        let user = model.current_account_name();
        let syncing = model.syncing;
        let events_online = model.events_online;
        // Requests waiting in sessions you are not watching.
        let alerts = model.session_alert_count();
        // Your sessions running on the server.
        let running = self.cloud_sessions;
        let section = self.section;
        let item = |s: Section, cx: &mut Context<Self>| {
            let badge = (s == Section::ServerSessions && alerts > 0).then_some(alerts);
            let running =
                (s == Section::ServerSessions && alerts == 0 && running > 0).then_some(running);
            // Taller and with bigger text and icons than the stock ones (28 px):
            // the fixed height of the component is applied later, but the
            // minimum wins.
            let item = SidebarMenuItem::new(s.label())
                .icon(ui::icon(s.icon()).size(px(18.)))
                .min_h(px(38.))
                .px_3()
                .gap_x_3()
                .text_size(px(15.))
                .active(section == s)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_section(s, window, cx)
                }));
            match (badge, running) {
                (Some(n), _) => item.suffix(move |_, cx| {
                    div()
                        .min_w(px(18.))
                        .h(px(18.))
                        .px_1()
                        .rounded_full()
                        .bg(cx.theme().warning)
                        .text_color(cx.theme().warning_foreground)
                        .text_xs()
                        .font_semibold()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(n.to_string())
                }),
                // Running sessions: a green dot with how many.
                (None, Some(n)) => item.suffix(move |_, cx| {
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(div().size(px(7.)).rounded_full().bg(cx.theme().success))
                        .child(n.to_string())
                }),
                (None, None) => item,
            }
        };
        let theme = cx.theme();
        let account = h_flex()
            .gap_2()
            .items_center()
            .w_full()
            .child(
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(if logged_in && events_online {
                        theme.success
                    } else if logged_in {
                        theme.warning
                    } else {
                        theme.muted_foreground
                    }),
            )
            .when(!collapsed, |this| {
                this.child(
                    v_flex()
                        .min_w_0()
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(if logged_in {
                                    user.map(SharedString::from)
                                        .unwrap_or_else(|| t!("app.account.connected"))
                                } else {
                                    t!("app.account.no_server")
                                }),
                        )
                        .child(div().text_xs().text_color(theme.muted_foreground).child(
                            if !logged_in {
                                t!("app.account.this_device_only")
                            } else if syncing {
                                t!("app.account.syncing")
                            } else {
                                t!("app.account.synced")
                            },
                        )),
                )
            });
        let switcher = self.render_switcher(collapsed, cx);
        let picker = self.render_vault_picker(collapsed, cx);
        let workspaces = self.render_workspaces_group(cx);
        let theme = cx.theme();
        Sidebar::new("sidebar")
            .w(px(232.))
            .collapsed(collapsed)
            .header(
                v_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        SidebarHeader::new().child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .size(px(28.))
                                        .rounded(px(7.))
                                        .bg(theme.primary)
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .child(
                                            ui::icon(IconName::Terminal)
                                                .size(px(16.))
                                                .text_color(theme.primary_foreground),
                                        ),
                                )
                                .when(!collapsed, |this| {
                                    this.child(
                                        v_flex()
                                            .child(div().text_sm().font_semibold().child("Termoak"))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(format!(
                                                        "v{}",
                                                        update::current_version()
                                                    )),
                                            ),
                                    )
                                }),
                        ),
                    )
                    .child(switcher)
                    .children(picker),
            )
            .child(
                SidebarGroup::new(t!("app.group.vault")).child(
                    SidebarMenu::new()
                        .child(item(Section::Hosts, cx))
                        .child(item(Section::Keychain, cx))
                        .child(item(Section::Snippets, cx))
                        .child(item(Section::Forwards, cx))
                        .child(item(Section::KnownHosts, cx)),
                ),
            )
            .child(workspaces)
            .child(SidebarGroup::new(t!("app.group.server")).child({
                let menu = SidebarMenu::new()
                    .child(item(Section::Ai, cx))
                    .child(item(Section::ServerSessions, cx))
                    .child(item(Section::Teams, cx));
                if is_admin {
                    menu.child(item(Section::Admin, cx))
                } else {
                    menu
                }
            }))
            .child(
                SidebarGroup::new(t!("app.group.app"))
                    .child(SidebarMenu::new().child(item(Section::Settings, cx))),
            )
            .footer(
                SidebarFooter::new().child(account.id("account").cursor_pointer().on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.select_section(Section::Settings, window, cx);
                    }),
                )),
            )
    }

    /// Settings → Accounts.
    fn open_accounts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_section(Section::Settings, window, cx);
        self.settings
            .update(cx, |s, cx| s.show_page(SettingsPage::Accounts, window, cx));
    }

    /// First start after the data moved to one store per account.
    fn layout_notice(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.model.update(cx, |m, _| m.take_layout_notice()) {
            return;
        }
        let accounts = self.model.read(cx).accounts.len();
        window.open_dialog(cx, move |d, _, _| {
            d.title(t!("accounts.layout_notice.title"))
                .w(px(480.))
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().child(if accounts > 0 {
                            t!("accounts.layout_notice.with_account")
                        } else {
                            t!("accounts.layout_notice.device_only")
                        }))
                        .child(div().text_sm().child(t!("accounts.layout_notice.backup"))),
                )
                .footer(
                    h_flex().w_full().justify_end().child(
                        Button::new("layout-notice-ok")
                            .primary()
                            .label(t!("common.ok"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }

    /// The account switcher at the top of the sidebar: each account, all of
    /// them, This device only, Add account and Manage accounts.
    fn render_switcher(&mut self, collapsed: bool, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.read(cx);
        let infos = m.account_infos();
        let view = accounts::normalize_view(&infos, m.view);
        let (title, subtitle) = accounts::switcher_title(&infos, view);
        let theme = cx.theme();
        let badge: AnyElement = match view {
            ViewMode::Account(id) => infos
                .iter()
                .find(|a| a.id == id)
                .map(|a| avatar(&accounts::AccountRow::new(a), 26.).into_any_element())
                .unwrap_or_else(|| div().into_any_element()),
            ViewMode::All => ui::icon(IconName::Users)
                .size(px(18.))
                .text_color(theme.primary)
                .into_any_element(),
            ViewMode::Device => ui::icon(IconName::Laptop)
                .size(px(18.))
                .text_color(theme.muted_foreground)
                .into_any_element(),
        };
        let model = self.model.clone();
        let menu_model = self.model.clone();
        let app = cx.entity().downgrade();
        let menu_app = app.clone();
        let muted = theme.muted_foreground;
        let switcher = Button::new("account-switcher")
            .ghost()
            .w_full()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .size(px(26.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(badge),
                    )
                    .when(!collapsed, |this| {
                        this.child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .items_start()
                                .child(
                                    div()
                                        .w_full()
                                        .text_sm()
                                        .font_medium()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .text_xs()
                                        .text_color(muted)
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(subtitle),
                                ),
                        )
                        .child(
                            ui::icon(IconName::ChevronsUpDown)
                                .size(px(14.))
                                .text_color(muted),
                        )
                    }),
            )
            .dropdown_menu(move |mut menu, _, cx| {
                let (infos, view) = {
                    let m = model.read(cx);
                    (m.account_infos(), m.view)
                };
                let set = |view: ViewMode, model: Entity<AppModel>| {
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        model.update(cx, |m, cx| m.set_view(view, cx))
                    }
                };
                for entry in accounts::switcher_entries(&infos, view) {
                    menu = match entry {
                        SwitcherEntry::Account { row, selected } => {
                            let id = row.id;
                            let status = row.status;
                            menu.item(
                                PopupMenuItem::element(move |_, cx| {
                                    let theme = cx.theme();
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(avatar(&row, 22.))
                                        .child(
                                            v_flex()
                                                .child(div().text_sm().child(row.title.clone()))
                                                .when_some(row.server.clone(), |this, s| {
                                                    this.child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(theme.muted_foreground)
                                                            .child(s),
                                                    )
                                                })
                                                .when(
                                                    status != termoak_client::AccountStatus::Active,
                                                    |this| {
                                                        this.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(theme.warning)
                                                                .child(accounts::status_text(
                                                                    status, false,
                                                                )),
                                                        )
                                                    },
                                                ),
                                        )
                                })
                                .checked(selected)
                                .on_click(set(ViewMode::Account(id), model.clone())),
                            )
                        }
                        SwitcherEntry::All { selected } => menu.item(
                            PopupMenuItem::new(t!("accounts.switcher.all"))
                                .icon(ui::icon(IconName::Users))
                                .checked(selected)
                                .on_click(set(ViewMode::All, model.clone())),
                        ),
                        SwitcherEntry::Device { selected } => menu.item(
                            PopupMenuItem::new(t!("accounts.switcher.device"))
                                .icon(ui::icon(IconName::Laptop))
                                .checked(selected)
                                .on_click(set(ViewMode::Device, model.clone())),
                        ),
                        SwitcherEntry::Add => {
                            let model = model.clone();
                            menu.separator().item(
                                PopupMenuItem::new(t!("accounts.add_menu"))
                                    .icon(ui::icon(IconName::UserPlus))
                                    .on_click(move |_, window, cx| {
                                        add_account::open(
                                            model.clone(),
                                            add_account::Start::Choose,
                                            window,
                                            cx,
                                        )
                                    }),
                            )
                        }
                        SwitcherEntry::Manage => {
                            let app = app.clone();
                            menu.item(
                                PopupMenuItem::new(t!("accounts.manage_menu"))
                                    .icon(ui::icon(IconName::Settings))
                                    .on_click(move |_, window, cx| {
                                        if let Some(app) = app.upgrade() {
                                            app.update(cx, |app, cx| app.open_accounts(window, cx));
                                        }
                                    }),
                            )
                        }
                    };
                }
                menu
            });
        // Right click: the alias of the accounts in sight and hiding the
        // emails (before a screenshot or sharing the screen).
        div()
            .id("account-switcher-menu")
            .w_full()
            .child(switcher)
            .context_menu(move |mut menu, _, cx| {
                let m = menu_model.read(cx);
                let infos = m.account_infos();
                let view = accounts::normalize_view(&infos, m.view);
                let hide = m.settings.hide_emails;
                let names = accounts::names();
                let in_sight: Vec<_> = accounts::accounts_in_view(&infos, view)
                    .into_iter()
                    .filter_map(|id| infos.iter().find(|a| a.id == id))
                    .map(|a| (a.id, names.name(a.id, &a.email)))
                    .collect();
                let one = in_sight.len() == 1;
                for (id, title) in in_sight {
                    let model = menu_model.clone();
                    menu = menu.item(
                        PopupMenuItem::new(if one {
                            t!("accounts.alias.menu")
                        } else {
                            t!("accounts.alias.menu_for", account = title)
                        })
                        .icon(ui::icon(IconName::Pencil))
                        .on_click(move |_, window, cx| {
                            crate::views::accounts::edit_alias(model.clone(), id, window, cx)
                        }),
                    );
                }
                if !infos.is_empty() {
                    let model = menu_model.clone();
                    let app = menu_app.clone();
                    menu = menu
                        .item(
                            PopupMenuItem::new(t!("settings.privacy.hide_emails"))
                                .checked(hide)
                                .on_click(move |_, _, cx| {
                                    model.update(cx, |m, cx| {
                                        let mut s = m.settings.clone();
                                        s.hide_emails = !s.hide_emails;
                                        m.save_settings(s, cx);
                                    })
                                }),
                        )
                        .separator()
                        .item(
                            PopupMenuItem::new(t!("accounts.manage_menu"))
                                .icon(ui::icon(IconName::Settings))
                                .on_click(move |_, window, cx| {
                                    if let Some(app) = app.upgrade() {
                                        app.update(cx, |app, cx| app.open_accounts(window, cx));
                                    }
                                }),
                        );
                } else {
                    let model = menu_model.clone();
                    menu = menu.item(
                        PopupMenuItem::new(t!("accounts.add_menu"))
                            .icon(ui::icon(IconName::UserPlus))
                            .on_click(move |_, window, cx| {
                                add_account::open(
                                    model.clone(),
                                    add_account::Start::Choose,
                                    window,
                                    cx,
                                )
                            }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    /// The vault picker under the switcher (only with more than one place
    /// to choose from): all vaults, each vault, This device.
    fn render_vault_picker(
        &mut self,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if collapsed {
            return None;
        }
        let m = self.model.read(cx);
        let vaults = m.vaults_in_view();
        let device_items = m.has_device_items();
        if !accounts::show_vault_picker(vaults.len(), device_items) {
            return None;
        }
        let filter = m.vault_filter;
        let (icon, label) = match filter {
            VaultFilter::All => (IconName::Layers, t!("vaults.picker.all").to_string()),
            VaultFilter::Device => (IconName::Laptop, t!("accounts.switcher.device").to_string()),
            VaultFilter::Vault { account, vault } => match m.vault_entry(account, vault) {
                Some(v) => (vaults::vault_icon(&v.vault), v.label()),
                None => (IconName::Vault, String::new()),
            },
        };
        let current = m.current_account.filter(|a| {
            m.account(*a)
                .is_some_and(|a| a.active() && a.vaults_supported())
        });
        let model = self.model.clone();
        let muted = cx.theme().muted_foreground;
        Some(
            Button::new("vault-picker")
                .small()
                .ghost()
                .w_full()
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_center()
                        .child(ui::icon(icon).size(px(14.)).text_color(muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(label),
                        )
                        .child(
                            ui::icon(IconName::ChevronDown)
                                .size(px(12.))
                                .text_color(muted),
                        ),
                )
                .dropdown_menu(move |mut menu, _, cx| {
                    let (vaults, filter, device_items, labels) = {
                        let m = model.read(cx);
                        let vaults = m.vaults_in_view();
                        let labels: Vec<String> = vaults
                            .iter()
                            .map(|v| {
                                m.place_label(
                                    termoak_client::Scope::Account(v.account),
                                    Some(v.id()),
                                )
                            })
                            .collect();
                        (vaults, m.vault_filter, m.has_device_items(), labels)
                    };
                    let set = |f: VaultFilter, model: Entity<AppModel>| {
                        move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                            model.update(cx, |m, cx| m.set_vault_filter(f, cx))
                        }
                    };
                    menu = menu.item(
                        PopupMenuItem::new(t!("vaults.picker.all"))
                            .icon(ui::icon(IconName::Layers))
                            .checked(filter == VaultFilter::All)
                            .on_click(set(VaultFilter::All, model.clone())),
                    );
                    for (v, label) in vaults.iter().zip(labels) {
                        let f = VaultFilter::Vault {
                            account: v.account,
                            vault: v.id(),
                        };
                        menu = menu.item(
                            PopupMenuItem::new(label)
                                .icon(ui::icon(vaults::vault_icon(&v.vault)))
                                .checked(filter == f)
                                .on_click(set(f, model.clone())),
                        );
                    }
                    if device_items {
                        menu = menu.item(
                            PopupMenuItem::new(t!("accounts.switcher.device"))
                                .icon(ui::icon(IconName::Laptop))
                                .checked(filter == VaultFilter::Device)
                                .on_click(set(VaultFilter::Device, model.clone())),
                        );
                    }
                    if let Some(account) = current {
                        let model = model.clone();
                        menu = menu.separator().item(
                            PopupMenuItem::new(t!("vaults.new_menu"))
                                .icon(ui::icon(IconName::Plus))
                                .on_click(move |_, window, cx| {
                                    vaults::open_create(model.clone(), account, None, window, cx)
                                }),
                        );
                    }
                    if let VaultFilter::Vault { account, vault } = filter {
                        let model = model.clone();
                        menu = menu.item(
                            PopupMenuItem::new(t!("vaults.manage_menu"))
                                .icon(ui::icon(IconName::Settings))
                                .on_click(move |_, window, cx| {
                                    vaults::open_manage(model.clone(), account, vault, window, cx)
                                }),
                        );
                    }
                    menu
                })
                .into_any_element(),
        )
    }

    /// Home notice: sessions running on the server (they are already at the
    /// top, as tabs).
    fn render_cloud_notice(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let n = self.cloud_sessions;
        h_flex()
            .mx_4()
            .mt_3()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(
                ui::icon(IconName::Cloud)
                    .size(px(16.))
                    .text_color(theme.info),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .font_medium()
                            .child(tn!("app.cloud_notice.title", n)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("app.cloud_notice.detail")),
                    ),
            )
            .child(
                Button::new("cloud-notice-open")
                    .small()
                    .label(t!("app.cloud_notice.open"))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.select_section(Section::ServerSessions, window, cx)
                    })),
            )
            .child(
                Button::new("cloud-notice-dismiss")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::X))
                    .tooltip(t!("app.cloud_notice.dismiss"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.dismiss_cloud_notice(cx)),
                    ),
            )
    }

    /// The Home notice about running sessions is closed: hidden until there
    /// are more of them (saved, for every window).
    fn dismiss_cloud_notice(&mut self, cx: &mut Context<Self>) {
        let n = self.cloud_sessions;
        self.model.update(cx, |m, cx| {
            let mut s = m.settings.clone();
            s.cloud_notice_dismissed = n;
            m.save_settings(s, cx);
        });
        cx.notify();
    }

    /// The Home notice about running sessions is on screen.
    fn cloud_notice_shown(&self, cx: &App) -> bool {
        self.section != Section::ServerSessions
            && app_state::cloud_notice_visible(
                self.cloud_sessions,
                self.model.read(cx).settings.cloud_notice_dismissed,
            )
    }

    fn render_section(&self) -> AnyElement {
        match self.section {
            Section::Hosts => self.hosts.clone().into_any_element(),
            Section::Keychain => self.keychain.clone().into_any_element(),
            Section::Snippets => self.snippets.clone().into_any_element(),
            Section::Forwards => self.forwards.clone().into_any_element(),
            Section::KnownHosts => self.known_hosts.clone().into_any_element(),
            Section::Ai => self.ai.clone().into_any_element(),
            Section::ServerSessions => self.server_sessions.clone().into_any_element(),
            Section::Teams => self.teams.clone().into_any_element(),
            Section::Admin => self.admin.clone().into_any_element(),
            Section::Settings => self.settings.clone().into_any_element(),
        }
    }
}

impl Focusable for AppView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl AppView {
    /// The window while the app is locked: nothing of its content, only
    /// the way to open it (quitting still works: ⌘Q, the close button).
    fn render_locked(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let notification_layer = Root::render_notification_layer(window, cx);
        let theme = cx.theme();
        v_flex()
            .id("termoak")
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(TitleBar::new().child(div().pl_2().text_sm().child("Termoak")))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .child(app_lock::render_lock_screen(&self.lock, cx)),
            )
            .children(notification_layer)
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.persist_session(cx);
        if self.lock.read(cx).locked() {
            return self.render_locked(window, cx);
        }
        let tab_strip = self.render_tab_strip(cx);
        let copilot = if self.copilot_open {
            self.active_copilot(window, cx)
        } else {
            None
        };
        let on_terminal = self.active_panes().is_some();
        let state = self.menu_state(cx);
        let main: AnyElement = match self.active.and_then(|i| self.tabs.get(i).map(|t| (i, t))) {
            Some((ix, tab)) => match &tab.content {
                TabContent::Terminal(p) => h_flex()
                    .size_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_panes(ix, p, cx)),
                    )
                    .when_some(copilot, |this, chat| {
                        this.child(
                            div()
                                .w(px(380.))
                                .flex_shrink_0()
                                .h_full()
                                .border_l_1()
                                .border_color(cx.theme().border)
                                .bg(cx.theme().background)
                                .child(chat),
                        )
                    })
                    .into_any_element(),
                TabContent::Sftp(s) => s.clone().into_any_element(),
                // It wakes up when activated; it never gets here.
                TabContent::Dormant { .. } => div().into_any_element(),
            },
            None => h_flex()
                .size_full()
                .child(self.render_sidebar(cx))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .when(self.cloud_notice_shown(cx), |this| {
                            this.child(self.render_cloud_notice(cx))
                        })
                        .child(div().flex_1().min_h_0().child(self.render_section())),
                )
                .into_any_element(),
        };
        let workspace_buttons = self.render_workspace_buttons(cx);
        let copilot_button = div()
            .id("copilot-toggle-area")
            .block_mouse_except_scroll()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .px_2()
            .when(on_terminal, |this| {
                this.child(
                    Button::new("copilot-toggle")
                        .small()
                        .map(|b| {
                            if self.copilot_open {
                                b.primary()
                            } else {
                                b.ghost()
                            }
                        })
                        .icon(ui::icon(IconName::Sparkles))
                        .label(t!("app.copilot.label"))
                        .tooltip(t!(
                            "app.copilot.tooltip",
                            shortcut = if cfg!(target_os = "macos") {
                                SharedString::from("⌘I")
                            } else {
                                t!("app.shortcut.copilot")
                            }
                        ))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.set_copilot(!this.copilot_open, window, cx)
                        })),
                )
            });
        let app_menu = (!cfg!(target_os = "macos")).then(|| self.render_app_menu(cx));
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let sheet_layer = Root::render_sheet_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);
        let theme = cx.theme();

        v_flex()
            .id("termoak")
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_new_local_terminal))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_go_home))
            .on_action(cx.listener(Self::on_about))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_new_host))
            .on_action(cx.listener(Self::on_quick_connect))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_zoom_reset))
            .on_action(cx.listener(Self::on_full_screen))
            .on_action(cx.listener(Self::on_minimize))
            .on_action(cx.listener(Self::on_show_shortcuts))
            .on_action(cx.listener(Self::on_command_palette))
            .on_action(cx.listener(Self::on_open_workspace))
            // Input resets the idle time of the lock.
            .capture_key_down(
                cx.listener(|this, _, _, cx| this.lock.update(cx, |l, _| l.activity())),
            )
            .capture_any_mouse_down(
                cx.listener(|this, _, _, cx| this.lock.update(cx, |l, _| l.activity())),
            )
            .on_mouse_move(cx.listener(|this, _, _, cx| this.lock.update(cx, |l, _| l.activity())))
            // The rest only when they have something to act on: on macOS
            // the menu bar disables the items whose action is not handled.
            .when(state.updates, |this| {
                this.on_action(cx.listener(Self::on_check_updates))
            })
            .when(state.any_tab, |this| {
                this.on_action(cx.listener(Self::on_close_tab))
                    .on_action(cx.listener(Self::on_save_workspace))
            })
            .when(state.active_tab, |this| {
                this.on_action(cx.listener(Self::on_move_tab_left))
                    .on_action(cx.listener(Self::on_move_tab_right))
            })
            .when(state.terminal, |this| {
                this.on_action(cx.listener(Self::on_toggle_copilot))
                    .on_action(cx.listener(Self::on_add_pane))
            })
            .when(state.writable, |this| {
                this.on_action(cx.listener(Self::on_send_snippet))
            })
            .when(state.ended, |this| {
                this.on_action(cx.listener(Self::on_reconnect))
            })
            .when(state.host, |this| {
                this.on_action(cx.listener(Self::on_open_sftp))
            })
            .when(state.duplicable, |this| {
                this.on_action(cx.listener(Self::on_duplicate))
            })
            .when(state.split, |this| {
                this.on_action(cx.listener(Self::on_close_pane))
                    .on_action(cx.listener(Self::on_toggle_focus_mode))
                    .on_action(cx.listener(Self::on_toggle_broadcast))
                    .on_action(cx.listener(Self::on_pane_left))
                    .on_action(cx.listener(Self::on_pane_right))
                    .on_action(cx.listener(Self::on_pane_up))
                    .on_action(cx.listener(Self::on_pane_down))
                    .on_action(cx.listener(Self::on_pane_to_new_tab))
            })
            .child(
                TitleBar::new()
                    .children(app_menu)
                    .child(tab_strip)
                    .child(workspace_buttons)
                    .child(copilot_button),
            )
            .child(div().flex_1().min_h_0().w_full().child(main))
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
            .into_any_element()
    }
}
