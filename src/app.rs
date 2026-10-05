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

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EntityId, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, actions, div, prelude::FluentBuilder,
    px, relative,
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

use crate::local_ai::copilot::{LocalAi, LocalAiGlobal};
use crate::local_ai::terminals::{LocalTerminals, TermRequest, screen_tail};
use crate::menus::{self, MenuState};
use crate::panes::{self, Dir, MAX_PANES};
use crate::prompts::{self, PromptRequest};
use crate::runtime;
use crate::sharing::{self, SessionNotice};
use crate::state::{AppModel, ModelEvent, ToastKind};
use crate::terminal::{
    PaneAction, PaneChrome, RequestKind, ShareRequest, TermKind, TermState, TerminalEvent,
    TerminalView, serial::SerialParams,
};
use crate::ui::{self, IconName};
use crate::update::{self, UpdateEvent, UpdateModel, UpdateStatus};
use crate::views::OpenRequest;
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

actions!(
    termoak,
    [
        NewTab,
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
        FocusPaneDown
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
        KeyBinding::new("cmd-t", NewTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-t", NewLocalTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("cmd-alt-w", ClosePane, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-]", NextTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-[", PrevTab, Some(CONTEXT)),
        KeyBinding::new("cmd-1", GoHome, Some(CONTEXT)),
        KeyBinding::new("cmd-i", ToggleCopilot, Some(CONTEXT)),
        KeyBinding::new("cmd-,", OpenSettings, Some(CONTEXT)),
        KeyBinding::new("cmd-n", NewHost, Some(CONTEXT)),
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
    ]);
    crate::views::hosts::init(cx);
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    cx.on_action(|_: &Hide, cx: &mut App| cx.hide());
    cx.on_action(|_: &HideOthers, cx: &mut App| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx: &mut App| cx.unhide_other_apps());
    cx.on_action(|_: &OpenDocs, cx: &mut App| cx.open_url(menus::DOCS_URL));
    cx.on_action(|_: &ReportIssue, cx: &mut App| cx.open_url(menus::ISSUES_URL));
    set_menus(cx);
}

/// Application menu, in the current language (call it again after changing
/// the language).
pub fn set_menus(cx: &mut App) {
    menus::set_menus(cx);
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
    focus: FocusHandle,
    _subs: Vec<Subscription>,
}

impl AppView {
    pub fn new(
        model: Entity<AppModel>,
        updates: Entity<UpdateModel>,
        prompts_rx: tokio::sync::mpsc::UnboundedReceiver<PromptRequest>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        prompts::listen(prompts_rx, window, cx);

        // The AI that runs on this computer: its tools reach the terminals
        // of this window through these requests.
        let (terminals, mut term_rx) = LocalTerminals::new();
        let store = model.read(cx).ws.store.clone();
        let rt = runtime::handle(cx);
        let local_ai = {
            let _rt = rt.enter();
            Arc::new(LocalAi::new(store, Arc::new(terminals)))
        };
        cx.set_global(LocalAiGlobal(local_ai.clone()));
        Self::start_local_engine(local_ai, model.clone(), window, cx);
        cx.spawn_in(window, async move |this, cx| {
            while let Some(req) = term_rx.recv().await {
                if this
                    .update(cx, |app, cx| app.answer_terminal(req, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

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

        let mut subs = vec![
            cx.observe_in(&model, window, |this, model, window, cx| {
                // The administration section disappears if you are no longer
                // an administrator (or you sign out).
                if this.section == Section::Admin && !model.read(cx).is_admin() {
                    this.select_section(Section::Hosts, window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                match ev {
                    ModelEvent::Toast(kind, msg) => ui::notify(window, cx, *kind, msg.clone()),
                    // When starting signed in or when signing in: the running
                    // sessions, as dormant tabs.
                    ModelEvent::SessionChanged => this.restore_cloud_tabs(true, window, cx),
                    ModelEvent::Server(v) if v["type"] == "session" => {
                        this.restore_cloud_tabs(false, window, cx);
                        if v["notice"]["type"] == "session_closed"
                            && let Some(id) = v["notice"]["session_id"]
                                .as_str()
                                .and_then(|s| s.parse::<Id>().ok())
                        {
                            this.model.update(cx, |m, cx| m.clear_session_alert(id, cx));
                        }
                        if let Some(notice) = SessionNotice::from_event(v) {
                            this.on_session_notice(notice, window, cx);
                        }
                    }
                    _ => {}
                }
            }),
            cx.subscribe_in(&updates, window, |this, _, ev: &UpdateEvent, window, cx| {
                this.on_update_event(ev, window, cx);
            }),
        ];
        subs.push(cx.subscribe_in(&hosts, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&keychain, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&snippets, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&forwards, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&known_hosts, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&ai, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&server_sessions, window, Self::on_open_request));
        subs.push(cx.subscribe_in(&settings, window, Self::on_open_request));

        let logged_in = model.read(cx).logged_in();
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
            focus: cx.focus_handle(),
            _subs: subs,
        };
        // If the server session was already restored, the event will not come.
        if logged_in {
            view.restore_cloud_tabs(true, window, cx);
        }
        // `termoak://` links opened from outside (at startup or later).
        if let Some(mut links) = crate::links::take_receiver() {
            cx.spawn_in(window, async move |this, cx| {
                while let Some(link) = links.recv().await {
                    if this
                        .update_in(cx, |app, window, cx| app.open_link(&link, window, cx))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .detach();
        }
        view
    }

    /// A `termoak://` (or web join) link: join a shared session or sign up
    /// with an invitation.
    pub fn open_link(&mut self, link: &str, window: &mut Window, cx: &mut Context<Self>) {
        window.activate_window();
        if sharing::parse_join_link(link).is_some() {
            self.open_join_dialog(Some(link.to_string()), window, cx);
        } else if crate::views::settings::parse_invite_link(link).is_some() {
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let session_id = notice.session_id();
        let watching = self.tabs.iter().any(|t| {
            matches!(t.content, TabContent::Terminal(_))
                && t.server_sessions(cx).contains(&session_id)
        });
        if watching {
            return;
        }
        if notice.needs_owner() {
            self.model
                .update(cx, |m, cx| m.add_session_alert(session_id, cx));
        }
        let title = match &notice {
            SessionNotice::JoinRequest { title, .. }
            | SessionNotice::ControlRequest { title, .. }
                if !title.is_empty() =>
            {
                title.clone()
            }
            _ => t!("server_sessions.default_title").to_string(),
        };
        let mut note = Notification::info(notice.text())
            .id1::<SessionNoticeToast>(SharedString::from(session_id.to_string()))
            .title(t!("share.notice.title"));
        if notice.needs_owner() {
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
                            };
                            a.update(cx, |a, cx| a.open(req, window, cx));
                        }
                    })
            });
        }
        window.push_notification(note, cx);
    }

    /// Starts the engine of the AI tasks on this computer and passes its
    /// events on like the server's (with a notification when a task needs
    /// approval or ends).
    fn start_local_engine(
        local_ai: Arc<LocalAi>,
        model: Entity<AppModel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let started = runtime::spawn(cx, async move { local_ai.start_engine().await });
        cx.spawn_in(window, async move |_, cx| {
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
                let note = local_task_note(&ev.event);
                let mut v = serde_json::to_value(&ev).unwrap_or_default();
                v["type"] = "ai".into();
                let alive = cx
                    .update(|window, cx| {
                        model.update(cx, |_, cx| cx.emit(ModelEvent::Server(v)));
                        if let Some((kind, text)) = note {
                            ui::notify(window, cx, kind, text);
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Every open terminal (all the panes of every tab).
    fn all_terminals(&self) -> Vec<Entity<TerminalView>> {
        self.tabs
            .iter()
            .filter_map(Tab::panes)
            .flat_map(|p| p.views().cloned().collect::<Vec<_>>())
            .collect()
    }

    /// Answers the local AI about the open terminals.
    fn answer_terminal(&mut self, req: TermRequest, cx: &mut Context<Self>) {
        let terms = self.all_terminals();
        let find = |id: Id, cx: &App| terms.iter().find(|t| t.read(cx).ai_id() == id).cloned();
        let missing = |id: Id| format!("there is no open terminal {id}; use list_sessions");
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
                        termoak_ai::SessionSummary {
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

    /// Fetches the server sessions. With `add_tabs`, own running sessions
    /// that are not open are added as dormant tabs (without changing screen);
    /// without it, only the Home notice is updated.
    fn restore_cloud_tabs(&mut self, add_tabs: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
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
        };
        runtime::run_in(
            cx,
            window,
            async move { api.get::<serde_json::Value>("/api/v1/sessions").await },
            move |this, res, _, cx| {
                let Ok(v) = res else { return };
                let running: Vec<(Id, String)> = v["active"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|s| s["kind"] == "server" && s["state"]["state"] != "closed")
                    .filter(|s| s["access"].as_str().unwrap_or("owner") == "owner")
                    .filter_map(|s| {
                        Some((
                            s["id"].as_str()?.parse().ok()?,
                            s["title"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| t!("app.session_fallback_title").to_string()),
                        ))
                    })
                    .collect();
                this.cloud_sessions = running.len();
                // Dormant tabs of sessions that already ended are dropped.
                let ended: Vec<usize> = this
                    .tabs
                    .iter()
                    .filter(|t| match &t.content {
                        TabContent::Dormant { session_id, .. } => {
                            !running.iter().any(|(id, _)| id == session_id)
                        }
                        _ => false,
                    })
                    .map(|t| t.id)
                    .collect();
                for id in ended {
                    this.drop_dormant(id);
                }
                if add_tabs {
                    for (session_id, title) in running {
                        let open = this
                            .tabs
                            .iter()
                            .any(|t| t.server_sessions(cx).contains(&session_id));
                        if !open {
                            let id = this.next_id;
                            this.next_id += 1;
                            this.tabs.push(Tab {
                                id,
                                content: TabContent::Dormant { session_id, title },
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
        let Some(TabContent::Dormant { session_id, title }) = self.tabs.get(ix).map(|t| &t.content)
        else {
            return;
        };
        let (session_id, title) = (*session_id, title.clone());
        let model = self.model.clone();
        let view =
            cx.new(|cx| TerminalView::server(model, None, Some(session_id), title, window, cx));
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
        let key = term.entity_id();
        if let Some((chat, _)) = self.copilots.get(&key) {
            return Some(chat.clone());
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
        Some(chat)
    }

    fn set_copilot(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.copilot_open = open;
        if open {
            if let Some(chat) = self.active_copilot(window, cx) {
                chat.update(cx, |c, cx| c.focus_input(window, cx));
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
            OpenRequest::Server { host_id } => {
                let host_id = *host_id;
                let title = self.model.read(cx).host_label(host_id);
                cx.new(|cx| TerminalView::server(model, Some(host_id), None, title, window, cx))
            }
            OpenRequest::Attach { session_id, title } => {
                let (session_id, title) = (*session_id, title.clone());
                cx.new(|cx| TerminalView::server(model, None, Some(session_id), title, window, cx))
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

    /// Opens a new tab.
    pub fn open(&mut self, req: OpenRequest, window: &mut Window, cx: &mut Context<Self>) {
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
            OpenRequest::Sftp { host_id, conn } => self.open_sftp(host_id, conn, window, cx),
            OpenRequest::AiSettings => {
                self.select_section(Section::Settings, window, cx);
                self.settings
                    .update(cx, |s, cx| s.show_page(SettingsPage::Ai, window, cx));
            }
            OpenRequest::Split { hosts, current } => {
                let reqs: Vec<OpenRequest> = hosts
                    .into_iter()
                    .map(|host_id| OpenRequest::Local { host_id })
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
            TerminalEvent::ShareRequest(req) => self.on_share_request(id, req, window, cx),
            TerminalEvent::ShareRequestDone { kind, participant } => {
                window.remove_notification1::<ShareRequestToast>(
                    request_key(*kind, *participant),
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
            TabContent::Dormant { .. } | TabContent::Sftp(_) => None,
        };
        match req {
            Some(req) => self.open(req, window, cx),
            None => ui::notify(window, cx, ToastKind::Info, t!("app.tab.cannot_duplicate")),
        }
    }

    fn select_section(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
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
            duplicable: term.is_some_and(|t| duplicate_request(t.kind()).is_some()),
            any_tab: !self.tabs.is_empty(),
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
    fn open_host_picker(&mut self, mode: PickMode, window: &mut Window, cx: &mut Context<Self>) {
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
        others: Vec<(usize, SharedString)>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let (w1, w2, w3, w4) = (weak.clone(), weak.clone(), weak.clone(), weak.clone());
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
                    .disabled(!is_terminal)
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
        let theme = cx.theme();
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
            let (icon, dot, count) = match &tab.content {
                TabContent::Terminal(p) => {
                    let t = p.focused().read(cx);
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
            let others: Vec<(usize, SharedString)> = terminal_tabs
                .iter()
                .filter(|(id, _)| *id != tab_id)
                .cloned()
                .collect();
            let weak = weak.clone();
            strip = strip.child(
                h_flex()
                    .id(("tab", tab.id))
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
                    .child(ui::icon(icon).size(px(14.)))
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
                    .context_menu(move |menu, window, cx| {
                        Self::tab_menu(
                            menu,
                            weak.clone(),
                            tab_id,
                            is_terminal,
                            others.clone(),
                            window,
                            cx,
                        )
                    }),
            );
        }
        strip.child(
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

    /// Terminals of a tab: alone, in a grid or in focus mode.
    fn render_panes(&self, tab: usize, p: &Panes, cx: &Context<Self>) -> AnyElement {
        if !p.is_split() {
            return p.focused().clone().into_any_element();
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
            div()
                .id(("pane", i))
                .relative()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .rounded(theme.radius)
                .border_2()
                .border_color(color)
                .child(pane.view.clone())
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

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let collapsed = self.sidebar_collapsed;
        let model = self.model.read(cx);
        let logged_in = model.logged_in();
        let is_admin = model.is_admin();
        let user = model.server_user.clone();
        let syncing = model.syncing;
        let events_online = model.events_online;
        // Requests waiting in sessions you are not watching.
        let alerts = model.session_alert_count();
        let section = self.section;
        let item = |s: Section, cx: &mut Context<Self>| {
            let badge = (s == Section::ServerSessions && alerts > 0).then_some(alerts);
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
            match badge {
                Some(n) => item.suffix(move |_, cx| {
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
                None => item,
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
        Sidebar::new("sidebar")
            .w(px(232.))
            .collapsed(collapsed)
            .header(
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
                                            .child(format!("v{}", update::current_version())),
                                    ),
                            )
                        }),
                ),
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

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                        .when(
                            self.cloud_sessions > 0 && self.section != Section::ServerSessions,
                            |this| this.child(self.render_cloud_notice(cx)),
                        )
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
            // The rest only when they have something to act on: on macOS
            // the menu bar disables the items whose action is not handled.
            .when(state.updates, |this| {
                this.on_action(cx.listener(Self::on_check_updates))
            })
            .when(state.any_tab, |this| {
                this.on_action(cx.listener(Self::on_close_tab))
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
    }
}

/// Notification for an event of a task on this computer: it needs approval
/// or it ended.
fn local_task_note(ev: &termoak_ai::TaskEvent) -> Option<(crate::state::ToastKind, SharedString)> {
    use crate::state::ToastKind;
    use termoak_ai::{TaskEvent, TaskStatus};
    match ev {
        TaskEvent::ApprovalRequested { summary, .. } => Some((
            ToastKind::Warning,
            t!("ai.local.approval_needed", action = summary.clone()),
        )),
        TaskEvent::Finished {
            status: TaskStatus::Completed,
            ..
        } => Some((ToastKind::Success, t!("ai.local.finished"))),
        TaskEvent::Finished {
            status: TaskStatus::Failed,
            error,
            ..
        } => Some((
            ToastKind::Error,
            t!("ai.local.failed", error = error.clone().unwrap_or_default()),
        )),
        _ => None,
    }
}
