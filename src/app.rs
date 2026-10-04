//! Main window, in the style of Termius: title bar with tabs (terminals and
//! SFTP), sidebar with the sections and the content of the active section or
//! tab. To the right of the terminal, the AI copilot. At startup, the running
//! server sessions show up as dormant tabs, which attach when clicked.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyBinding, Menu, MenuItem, MouseButton, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window, actions, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::scroll::ScrollableElement;
use gpui_component::sidebar::{
    Sidebar, SidebarFooter, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_component::{ActiveTheme, Root, Sizable, StyledExt, TitleBar, WindowExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_ssh::Connection;

use crate::local_ai::copilot::{LocalAi, LocalAiGlobal};
use crate::local_ai::terminals::{LocalTerminals, TermRequest, screen_tail};
use crate::prompts::{self, PromptRequest};
use crate::runtime;
use crate::state::{AppModel, ModelEvent};
use crate::terminal::{TermKind, TermState, TerminalEvent, TerminalView};
use crate::ui::{self, IconName};
use crate::update::{self, UpdateEvent, UpdateModel};
use crate::views::OpenRequest;
use crate::views::admin::AdminView;
use crate::views::ai::AiView;
use crate::views::ai_chat::{AiChat, AiChatEvent};
use crate::views::forwards::ForwardsView;
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
        Quit
    ]
);

const CONTEXT: &str = "Workspace";

/// Global shortcuts of the window.
pub fn init(cx: &mut App) {
    #[cfg(target_os = "macos")]
    cx.bind_keys([
        KeyBinding::new("cmd-t", NewTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-t", NewLocalTerminal, Some(CONTEXT)),
        KeyBinding::new("cmd-w", CloseTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-]", NextTab, Some(CONTEXT)),
        KeyBinding::new("cmd-shift-[", PrevTab, Some(CONTEXT)),
        KeyBinding::new("cmd-1", GoHome, Some(CONTEXT)),
        KeyBinding::new("cmd-i", ToggleCopilot, Some(CONTEXT)),
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
    ]);
    cx.bind_keys([
        KeyBinding::new("ctrl-tab", NextTab, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some(CONTEXT)),
    ]);
    cx.on_action(|_: &Quit, cx: &mut App| cx.quit());
    set_menus(cx);
}

/// Application menu, in the current language (call it again after changing
/// the language).
pub fn set_menus(cx: &mut App) {
    cx.set_menus([Menu::new("Termoak").items([
        MenuItem::action(t!("app.menu.new_tab"), NewTab),
        MenuItem::action(t!("app.menu.new_local_terminal"), NewLocalTerminal),
        MenuItem::action(t!("app.menu.close_tab"), CloseTab),
        MenuItem::action(t!("app.menu.copilot"), ToggleCopilot),
        MenuItem::separator(),
        MenuItem::action(t!("app.menu.quit"), Quit),
    ])]);
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

enum TabContent {
    Terminal(Entity<TerminalView>),
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
    _sub: Option<Subscription>,
}

impl Tab {
    fn title(&self, cx: &App) -> SharedString {
        match &self.content {
            TabContent::Terminal(t) => t.read(cx).title(cx),
            TabContent::Sftp(s) => s.read(cx).title(),
            TabContent::Dormant { title, .. } => title.clone().into(),
        }
    }

    fn focus_handle(&self, cx: &App) -> Option<FocusHandle> {
        match &self.content {
            TabContent::Terminal(t) => Some(t.focus_handle(cx)),
            TabContent::Sftp(_) | TabContent::Dormant { .. } => None,
        }
    }

    /// Server session of the tab (attached or dormant).
    fn server_session(&self, cx: &App) -> Option<Id> {
        match &self.content {
            TabContent::Terminal(v) => match v.read(cx).kind() {
                TermKind::Server { session_id, .. } => *session_id,
                _ => None,
            },
            TabContent::Dormant { session_id, .. } => Some(*session_id),
            TabContent::Sftp(_) => None,
        }
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
    next_id: usize,
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
    /// Copilot conversation of each tab (by tab id).
    copilots: HashMap<usize, (Entity<AiChat>, Subscription)>,
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
                        this.restore_cloud_tabs(false, window, cx)
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
            next_id: 1,
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
        view
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

    /// Answers the local AI about the open terminals.
    fn answer_terminal(&mut self, req: TermRequest, cx: &mut Context<Self>) {
        let terms: Vec<Entity<TerminalView>> = self
            .tabs
            .iter()
            .filter_map(|t| match &t.content {
                TabContent::Terminal(v) => Some(v.clone()),
                _ => None,
            })
            .collect();
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
                            .any(|t| t.server_session(cx) == Some(session_id));
                        if !open {
                            let id = this.next_id;
                            this.next_id += 1;
                            this.tabs.push(Tab {
                                id,
                                content: TabContent::Dormant { session_id, title },
                                _sub: None,
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
        let sub = self.terminal_events(&view, window, cx);
        let tab = &mut self.tabs[ix];
        tab.content = TabContent::Terminal(view);
        tab._sub = Some(sub);
    }

    /// Copilot of the active tab (created the first time), if it is a terminal.
    fn active_copilot(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<AiChat>> {
        let tab = self.tabs.get(self.active?)?;
        let TabContent::Terminal(term) = &tab.content else {
            return None;
        };
        let tab_id = tab.id;
        if let Some((chat, _)) = self.copilots.get(&tab_id) {
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
        self.copilots.insert(tab_id, (chat.clone(), sub));
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

    /// Opens a new tab.
    pub fn open(&mut self, req: OpenRequest, window: &mut Window, cx: &mut Context<Self>) {
        match req {
            OpenRequest::Local { host_id } => {
                let model = self.model.clone();
                let view = cx.new(|cx| TerminalView::local(model, host_id, window, cx));
                self.push_terminal(view, window, cx);
            }
            OpenRequest::Server { host_id } => {
                let model = self.model.clone();
                let title = self.model.read(cx).host_label(host_id);
                let view = cx
                    .new(|cx| TerminalView::server(model, Some(host_id), None, title, window, cx));
                self.push_terminal(view, window, cx);
            }
            OpenRequest::Attach { session_id, title } => {
                // If it is already open, activate it.
                if let Some(ix) = self
                    .tabs
                    .iter()
                    .position(|t| t.server_session(cx) == Some(session_id))
                {
                    self.activate(Some(ix), window, cx);
                    return;
                }
                let model = self.model.clone();
                let view = cx.new(|cx| {
                    TerminalView::server(model, None, Some(session_id), title, window, cx)
                });
                self.push_terminal(view, window, cx);
            }
            OpenRequest::Sftp { host_id, conn } => self.open_sftp(host_id, conn, window, cx),
            OpenRequest::Shell => {
                let model = self.model.clone();
                let view = cx.new(|cx| TerminalView::shell(model, window, cx));
                self.push_terminal(view, window, cx);
            }
            OpenRequest::Serial(params) => {
                let model = self.model.clone();
                let view = cx.new(|cx| TerminalView::serial(model, params, window, cx));
                self.push_terminal(view, window, cx);
            }
            OpenRequest::AiSettings => {
                self.select_section(Section::Settings, window, cx);
                self.settings
                    .update(cx, |s, cx| s.show_page(SettingsPage::Ai, window, cx));
            }
        }
    }

    fn push_terminal(
        &mut self,
        view: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sub = self.terminal_events(&view, window, cx);
        self.push_tab(TabContent::Terminal(view), Some(sub), window, cx);
    }

    fn terminal_events(
        &mut self,
        view: &Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe_in(
            view,
            window,
            |this, _, ev: &TerminalEvent, window, cx| match ev {
                TerminalEvent::TitleChanged => cx.notify(),
                TerminalEvent::OpenSftp { host_id, conn } => {
                    this.open_sftp(*host_id, conn.clone(), window, cx);
                }
            },
        )
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
        self.push_tab(TabContent::Sftp(view), None, window, cx);
    }

    fn push_tab(
        &mut self,
        content: TabContent,
        sub: Option<Subscription>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        self.tabs.push(Tab {
            id,
            content,
            _sub: sub,
        });
        self.activate(Some(self.tabs.len() - 1), window, cx);
    }

    fn activate(&mut self, ix: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        self.active = ix.filter(|i| *i < self.tabs.len());
        if let Some(i) = self.active {
            self.wake(i, window, cx);
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
        if let Some((chat, _)) = self.copilots.remove(&tab.id) {
            chat.update(cx, |c, cx| c.release(window, cx));
        }
        match &tab.content {
            TabContent::Terminal(t) => t.update(cx, |t, cx| t.shutdown(cx)),
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

    // ----- Actions -----

    fn on_new_tab(&mut self, _: &NewTab, window: &mut Window, cx: &mut Context<Self>) {
        self.open_host_picker(window, cx);
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

    /// Host picker for a new tab (Ctrl/Cmd+T).
    fn open_host_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        let weak = cx.entity().downgrade();
        let picker = cx.new(|cx| HostPicker::new(model, weak, window, cx));
        window.open_dialog(cx, move |d, _, _| {
            let first = picker.clone();
            d.title(t!("app.picker.title"))
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

    fn render_tab_strip(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let home_active = self.active.is_none();
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
            let (icon, dot) = match &tab.content {
                TabContent::Terminal(t) => (
                    if matches!(t.read(cx).kind(), TermKind::Shell) {
                        IconName::Laptop
                    } else {
                        IconName::SquareTerminal
                    },
                    Some(match t.read(cx).state() {
                        TermState::Running => theme.success,
                        TermState::Connecting(_) => theme.warning,
                        TermState::Failed(_) => theme.danger,
                        TermState::Closed(_) => theme.muted_foreground,
                    }),
                ),
                TabContent::Sftp(_) => (IconName::FolderOpen, None),
                TabContent::Dormant { .. } => (IconName::Cloud, None),
            };
            let tab_id = tab.id;
            strip = strip.child(
                h_flex()
                    .id(("tab", tab.id))
                    .block_mouse_except_scroll()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .h(px(28.))
                    .max_w(px(220.))
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
                    ),
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
                            this.open_host_picker(window, cx)
                        })),
                ),
        )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let model = self.model.read(cx);
        let logged_in = model.logged_in();
        let is_admin = model.is_admin();
        let user = model.server_user.clone();
        let syncing = model.syncing;
        let events_online = model.events_online;
        let section = self.section;
        let item = |s: Section, cx: &mut Context<Self>| {
            // Taller and with bigger text and icons than the stock ones (28 px):
            // the fixed height of the component is applied later, but the
            // minimum wins.
            SidebarMenuItem::new(s.label())
                .icon(ui::icon(s.icon()).size(px(18.)))
                .min_h(px(38.))
                .px_3()
                .gap_x_3()
                .text_size(px(15.))
                .active(section == s)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_section(s, window, cx)
                }))
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
            .child(
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
            );
        Sidebar::new("sidebar")
            .w(px(232.))
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
                        .child(
                            v_flex()
                                .child(div().text_sm().font_semibold().child("Termoak"))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("v{}", update::current_version())),
                                ),
                        ),
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
        let on_terminal = matches!(
            self.active
                .and_then(|i| self.tabs.get(i))
                .map(|t| &t.content),
            Some(TabContent::Terminal(_))
        );
        let main: AnyElement = match self.active.and_then(|i| self.tabs.get(i)) {
            Some(tab) => match &tab.content {
                TabContent::Terminal(t) => h_flex()
                    .size_full()
                    .child(div().flex_1().min_w_0().h_full().child(t.clone()))
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
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_go_home))
            .on_action(cx.listener(Self::on_toggle_copilot))
            .child(TitleBar::new().child(tab_strip).child(copilot_button))
            .child(div().flex_1().min_h_0().w_full().child(main))
            .children(sheet_layer)
            .children(dialog_layer)
            .children(notification_layer)
    }
}

/// Host picker of Ctrl/Cmd+T.
struct HostPicker {
    model: Entity<AppModel>,
    app: gpui::WeakEntity<AppView>,
    search: Entity<InputState>,
    _sub: Subscription,
}

impl HostPicker {
    fn new(
        model: Entity<AppModel>,
        app: gpui::WeakEntity<AppView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("app.picker.search")));
        ui::focus_later(&search, window, cx);
        let sub = cx.subscribe_in(
            &search,
            window,
            // Enter is handled by the dialog (see `open_host_picker`).
            |_, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            },
        );
        Self {
            model,
            app,
            search,
            _sub: sub,
        }
    }

    /// Whether the "Local terminal" entry is shown: always without a search,
    /// or when the search matches one of its names (in English and in the
    /// current language).
    fn shows_shell(&self, cx: &App) -> bool {
        let q = self.search.read(cx).value().trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        let terms = t!("app.picker.shell_search_terms").to_lowercase();
        terms
            .split(',')
            .chain(["local terminal", "local", "shell", "this computer"])
            .any(|name| name.trim().contains(q.as_str()))
    }

    /// Opens the first entry of the list (Enter).
    fn pick_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.shows_shell(cx) {
            self.pick_shell(window, cx);
        } else if let Some(id) = self.matches(cx).first().map(|(id, _, _)| *id) {
            self.pick(id, false, window, cx);
        }
    }

    fn pick_shell(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        if let Some(app) = self.app.upgrade() {
            app.update(cx, |app, cx| app.open(OpenRequest::Shell, window, cx));
        }
    }

    fn pick_serial(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        let app = self.app.clone();
        crate::views::serial::open(window, cx, move |params, window, cx| {
            if let Some(app) = app.upgrade() {
                app.update(cx, |app, cx| {
                    app.open(OpenRequest::Serial(params), window, cx)
                });
            }
        });
    }

    fn matches(&self, cx: &App) -> Vec<(Id, String, String)> {
        let q = self.search.read(cx).value().trim().to_lowercase();
        self.model
            .read(cx)
            .hosts
            .iter()
            .filter(|h| {
                q.is_empty()
                    || h.data.label.to_lowercase().contains(&q)
                    || h.data.address.to_lowercase().contains(&q)
                    || h.data.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .map(|h| (h.data.id, h.data.label.clone(), h.data.address.clone()))
            .collect()
    }

    fn pick(&mut self, host_id: Id, server: bool, window: &mut Window, cx: &mut Context<Self>) {
        window.close_dialog(cx);
        if let Some(app) = self.app.upgrade() {
            let req = if server {
                OpenRequest::Server { host_id }
            } else {
                OpenRequest::Local { host_id }
            };
            app.update(cx, |app, cx| app.open(req, window, cx));
        }
    }
}

impl Render for HostPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let logged_in = self.model.read(cx).logged_in();
        let items = self.matches(cx);
        let shows_shell = self.shows_shell(cx);
        let shell_shortcut = if cfg!(target_os = "macos") {
            SharedString::from("⌘⇧T")
        } else {
            t!("app.shortcut.new_local_terminal")
        };
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(Input::new(&self.search).prefix(ui::icon(IconName::Search).size(px(14.))))
            .child(
                v_flex()
                    .id("picker-list")
                    .max_h(px(360.))
                    .gap_1()
                    .overflow_y_scrollbar()
                    .when(shows_shell, |this| {
                        this.child(
                            h_flex()
                                .id("pick-shell")
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.pick_shell(window, cx)
                                }))
                                .child(
                                    ui::icon(IconName::Laptop)
                                        .size(px(16.))
                                        .text_color(theme.success),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_medium()
                                                .child(t!("app.picker.shell_title")),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("app.picker.shell_detail")),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(shell_shortcut),
                                ),
                        )
                    })
                    .when(shows_shell, |this| {
                        this.child(
                            h_flex()
                                .id("pick-serial")
                                .px_3()
                                .py_2()
                                .gap_3()
                                .items_center()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .hover(|s| s.bg(theme.secondary_hover))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.pick_serial(window, cx)
                                }))
                                .child(
                                    ui::icon(IconName::ArrowLeftRight)
                                        .size(px(16.))
                                        .text_color(theme.warning),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .text_sm()
                                                .font_medium()
                                                .child(t!("app.picker.serial_title")),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("app.picker.serial_detail")),
                                        ),
                                ),
                        )
                    })
                    .when(items.is_empty() && !shows_shell, |this| {
                        this.child(
                            div()
                                .p_4()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(t!("app.picker.no_matches")),
                        )
                    })
                    .children(
                        items
                            .into_iter()
                            .enumerate()
                            .map(|(i, (id, label, address))| {
                                h_flex()
                                    .id(("pick", i))
                                    .px_3()
                                    .py_2()
                                    .gap_3()
                                    .items_center()
                                    .rounded(theme.radius)
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme.secondary_hover))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.pick(id, false, window, cx)
                                        },
                                    ))
                                    .child(
                                        ui::icon(IconName::Server)
                                            .size(px(16.))
                                            .text_color(theme.primary),
                                    )
                                    .child(
                                        v_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .child(div().text_sm().font_medium().child(label))
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(theme.muted_foreground)
                                                    .child(address),
                                            ),
                                    )
                                    .when(logged_in, |this| {
                                        this.child(
                                            Button::new(("pick-server", i))
                                                .xsmall()
                                                .ghost()
                                                .icon(ui::icon(IconName::Cloud))
                                                .label(t!("app.picker.on_server"))
                                                .on_click(cx.listener(
                                                    move |this, _: &ClickEvent, window, cx| {
                                                        cx.stop_propagation();
                                                        this.pick(id, true, window, cx);
                                                    },
                                                )),
                                        )
                                    })
                            }),
                    ),
            )
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
