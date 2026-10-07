//! Workspaces: a saved arrangement of tabs (which hosts, server sessions and
//! local terminals, and how the panes of each split view are laid out) that
//! opens again in one click; and the tabs of the previous session, opened
//! again at start ("Reopen tabs on start").
//!
//! Where they are kept: in the device store (`meta`), not in the synced
//! vaults. The synced data model has a fixed set of item kinds (hosts,
//! groups, keys, identities, snippets, tunnels, known hosts), each living in
//! exactly one vault of one account and checked by the server; a workspace
//! is a view over items of *several* vaults and accounts (and This device)
//! plus terminals of this computer (shells, serial ports), so it is not an
//! item of any one vault. Making it one would need a new kind in the core,
//! the server and every client. So workspaces are device-local, like the
//! window's other preferences, and a workspace that references hosts the
//! device no longer has simply skips them (with a notice).
//!
//! Nothing secret is stored: only ids of hosts, sessions and accounts,
//! serial port paths, tab names and the layout. Sessions joined with a share
//! link are left out (the link carries a token).
//!
//! The pure parts live here (what is saved, how it is read back and what
//! opening it does); the window side is in `app.rs`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use termoak_core::Id;

use crate::panes::MAX_PANES;
use crate::terminal::TermKind;

/// Meta key of the device store with the saved workspaces.
pub const WORKSPACES_KEY: &str = "desktop.workspaces";
/// Meta key of the device store with the tabs of the last session.
pub const SESSION_KEY: &str = "desktop.session";

/// A terminal of a saved tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedPane {
    /// SSH from this computer to a host.
    Ssh { host_id: Id },
    /// A session that lives on a Termoak server: attached again if it is
    /// still running, otherwise (opening a workspace) a new one on its host.
    Server {
        #[serde(default)]
        host_id: Option<Id>,
        #[serde(default)]
        session_id: Option<Id>,
        /// Account whose server has it.
        #[serde(default)]
        account: Option<Id>,
        #[serde(default)]
        title: String,
    },
    /// A shell of this computer.
    Shell,
    /// A serial port of this computer.
    Serial { path: String, baud: u32 },
}

/// A saved tab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedTab {
    /// One terminal, or several in a split view (in grid order).
    Terminal {
        panes: Vec<SavedPane>,
        /// The focused pane.
        #[serde(default)]
        focused: usize,
        /// Focus mode (the focused pane big).
        #[serde(default)]
        maximized: bool,
        /// Name given with "Rename".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// SFTP browser of a host.
    Sftp { host_id: Id },
}

impl SavedTab {
    /// A tab with one SSH terminal.
    pub fn ssh(host_id: Id) -> Self {
        SavedTab::Terminal {
            panes: vec![SavedPane::Ssh { host_id }],
            focused: 0,
            maximized: false,
            title: None,
        }
    }
}

/// The tabs of a window. Broadcast input is never saved (it opens off).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    /// Tabs whose kind this version does not know are skipped when read.
    #[serde(default, deserialize_with = "lenient_tabs")]
    pub tabs: Vec<SavedTab>,
    /// The tab in view (`None`: the home screen).
    #[serde(default)]
    pub active: Option<usize>,
}

impl Layout {
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    /// Adds a host as a new tab ("Add to workspace").
    pub fn add_host(&mut self, host_id: Id) {
        self.tabs.push(SavedTab::ssh(host_id));
    }

    /// Hosts it uses (to check they still exist).
    pub fn host_ids(&self) -> Vec<Id> {
        let mut ids = Vec::new();
        for tab in &self.tabs {
            match tab {
                SavedTab::Terminal { panes, .. } => {
                    for p in panes {
                        match p {
                            SavedPane::Ssh { host_id } => ids.push(*host_id),
                            SavedPane::Server {
                                host_id: Some(h), ..
                            } => ids.push(*h),
                            _ => {}
                        }
                    }
                }
                SavedTab::Sftp { host_id } => ids.push(*host_id),
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// Reads the tabs one by one, leaving out the ones that do not parse (a
/// newer version may save kinds this one does not know).
fn lenient_tabs<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SavedTab>, D::Error> {
    let raw: Vec<Value> = Vec::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

/// A named workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub id: Id,
    pub name: String,
    pub layout: Layout,
    #[serde(default = "Utc::now")]
    pub updated_at: DateTime<Utc>,
}

impl SavedWorkspace {
    pub fn new(name: impl Into<String>, layout: Layout) -> Self {
        Self {
            id: termoak_core::new_id(),
            name: name.into(),
            layout,
            updated_at: Utc::now(),
        }
    }
}

/// The tabs of every window of the last session, in the order the windows
/// were opened.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFile {
    #[serde(default)]
    pub windows: Vec<Layout>,
}

/// Reads the saved workspaces (an unreadable value is an empty list, never
/// an error at start).
pub fn parse_workspaces(json: Option<&str>) -> Vec<SavedWorkspace> {
    json.and_then(|s| serde_json::from_str::<Vec<Value>>(s).ok())
        .map(|list| {
            list.into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Reads the last session (unreadable: nothing to reopen).
pub fn parse_session(json: Option<&str>) -> SessionFile {
    json.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default()
}

/// A name for a new workspace that no other one has: "Workspace 3".
pub fn next_name(base: &str, existing: &[SavedWorkspace]) -> String {
    (1..)
        .map(|n| format!("{base} {n}"))
        .find(|name| !existing.iter().any(|w| w.name.eq_ignore_ascii_case(name)))
        .unwrap_or_else(|| base.to_string())
}

/// How a terminal is saved (`None`: it is not, e.g. joined with a link).
/// `current` is the account of the window, for server sessions that do
/// not name one.
pub fn pane_of(kind: &TermKind, title: &str, current: Option<Id>) -> Option<SavedPane> {
    match kind {
        TermKind::Local { host_id } => Some(SavedPane::Ssh { host_id: *host_id }),
        TermKind::Server { link: Some(_), .. } => None,
        TermKind::Server {
            host_id,
            session_id,
            account,
            link: None,
        } => {
            if host_id.is_none() && session_id.is_none() {
                return None;
            }
            Some(SavedPane::Server {
                host_id: *host_id,
                session_id: *session_id,
                account: account.or(current),
                title: title.to_string(),
            })
        }
        TermKind::Shell => Some(SavedPane::Shell),
        TermKind::Serial { path, baud } => Some(SavedPane::Serial {
            path: path.clone(),
            baud: *baud,
        }),
    }
}

/// What opening a layout does with server sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The previous session at start: a tab that was one server session
    /// comes back dormant (it attaches when clicked); one that ended is
    /// left out; nothing new starts on the server.
    Restore,
    /// A named workspace: sessions still running are attached, the ones
    /// that ended start again on their host.
    Open,
}

/// What the window knows when opening a layout.
pub struct Env<'a> {
    /// The host exists on this device (some store has it).
    pub host_exists: &'a dyn Fn(Id) -> bool,
    /// The account (`None`: the current one) is signed in and active.
    pub signed_in: &'a dyn Fn(Option<Id>) -> bool,
    /// Server sessions known to be running, if the window asked the
    /// servers already (`None`: unknown, they are tried).
    pub running: Option<&'a [Id]>,
    /// Server sessions already open in the window (not opened twice).
    pub open_sessions: &'a [Id],
}

/// A terminal to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanPane {
    Ssh(Id),
    /// Attach to a running server session.
    Attach {
        session_id: Id,
        account: Option<Id>,
        title: String,
    },
    /// A new server session on a host.
    NewServer(Id),
    Shell,
    Serial {
        path: String,
        baud: u32,
    },
}

/// A tab to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanTab {
    Terminal {
        panes: Vec<PlanPane>,
        focused: usize,
        maximized: bool,
        title: Option<String>,
    },
    /// A server session that attaches when clicked.
    Dormant {
        session_id: Id,
        account: Id,
        title: String,
    },
    Sftp(Id),
}

/// What opening a layout does.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub tabs: Vec<PlanTab>,
    /// Index in `tabs` of the tab to show (`None`: leave the screen as is).
    pub active: Option<usize>,
    /// Terminals and browsers left out: their host no longer exists, their
    /// account is signed out or their session ended.
    pub skipped: usize,
}

/// Works out what opening `layout` does.
pub fn plan(layout: &Layout, mode: Mode, env: &Env) -> Plan {
    let mut out = Plan::default();
    // Saved tab index → planned tab index.
    let mut mapped: Vec<Option<usize>> = Vec::with_capacity(layout.tabs.len());
    for tab in &layout.tabs {
        let planned = match tab {
            SavedTab::Sftp { host_id } => {
                if (env.host_exists)(*host_id) {
                    Some(PlanTab::Sftp(*host_id))
                } else {
                    out.skipped += 1;
                    None
                }
            }
            SavedTab::Terminal {
                panes,
                focused,
                maximized,
                title,
            } => {
                let mut kept: Vec<PlanPane> = Vec::new();
                // Where the focused pane ends up after skipping some.
                let mut new_focus = 0;
                for (i, pane) in panes.iter().enumerate() {
                    if kept.len() >= MAX_PANES {
                        out.skipped += 1;
                        continue;
                    }
                    match plan_pane(pane, mode, env) {
                        Some(p) => {
                            if i == *focused {
                                new_focus = kept.len();
                            }
                            kept.push(p)
                        }
                        // Already open in the window: not a loss.
                        None if already_open(pane, env) => {}
                        None => out.skipped += 1,
                    }
                }
                match kept.as_slice() {
                    [] => None,
                    // Restoring, a tab that was one server session comes
                    // back dormant.
                    [
                        PlanPane::Attach {
                            session_id,
                            account: Some(account),
                            title: session_title,
                        },
                    ] if mode == Mode::Restore => Some(PlanTab::Dormant {
                        session_id: *session_id,
                        account: *account,
                        title: title.clone().unwrap_or_else(|| session_title.clone()),
                    }),
                    _ => {
                        let n = kept.len();
                        Some(PlanTab::Terminal {
                            panes: kept,
                            focused: new_focus.min(n - 1),
                            maximized: *maximized && n > 1,
                            title: title.clone(),
                        })
                    }
                }
            }
        };
        match planned {
            Some(t) => {
                mapped.push(Some(out.tabs.len()));
                out.tabs.push(t);
            }
            None => mapped.push(None),
        }
    }
    out.active = match mode {
        // The tab that was in view, or the nearest one kept before it.
        Mode::Restore => layout.active.and_then(|a| {
            mapped
                .iter()
                .take(a + 1)
                .rev()
                .find_map(|m| *m)
                .or_else(|| mapped.iter().find_map(|m| *m))
        }),
        // Opening a workspace shows its first tab.
        Mode::Open => (!out.tabs.is_empty()).then_some(0),
    };
    out
}

fn already_open(pane: &SavedPane, env: &Env) -> bool {
    matches!(pane, SavedPane::Server { session_id: Some(s), .. } if env.open_sessions.contains(s))
}

fn plan_pane(pane: &SavedPane, mode: Mode, env: &Env) -> Option<PlanPane> {
    match pane {
        SavedPane::Ssh { host_id } => {
            (env.host_exists)(*host_id).then_some(PlanPane::Ssh(*host_id))
        }
        SavedPane::Shell => Some(PlanPane::Shell),
        SavedPane::Serial { path, baud } => Some(PlanPane::Serial {
            path: path.clone(),
            baud: *baud,
        }),
        SavedPane::Server {
            host_id,
            session_id,
            account,
            title,
        } => {
            if !(env.signed_in)(*account) {
                return None;
            }
            if let Some(s) = session_id {
                if env.open_sessions.contains(s) {
                    return None;
                }
                let running = env.running.is_none_or(|r| r.contains(s));
                if running {
                    return Some(PlanPane::Attach {
                        session_id: *s,
                        account: *account,
                        title: title.clone(),
                    });
                }
            }
            match (mode, host_id) {
                (Mode::Open, Some(h)) if (env.host_exists)(*h) => Some(PlanPane::NewServer(*h)),
                _ => None,
            }
        }
    }
}

/// Dialog that asks for the name of a workspace (to save or rename it).
pub fn ask_name(
    window: &mut gpui::Window,
    cx: &mut gpui::App,
    title: gpui::SharedString,
    default: String,
    hint: Option<gpui::SharedString>,
    on_ok: impl Fn(String, &mut gpui::Window, &mut gpui::App) + 'static,
) {
    use gpui::{AppContext, IntoElement};
    use gpui_component::input::{Input, InputState};
    let input = cx.new(|cx| InputState::new(window, cx).default_value(default));
    crate::ui::focus_later(&input, window, cx);
    let field = input.clone();
    crate::ui::open_form_dialog(
        window,
        cx,
        title,
        t!("common.save"),
        420.,
        move |_, cx| match &hint {
            Some(h) => {
                crate::ui::field_with_hint(t!("common.name"), Input::new(&input), h.clone(), cx)
                    .into_any_element()
            }
            None => crate::ui::field(t!("common.name"), Input::new(&input), cx).into_any_element(),
        },
        move |window, cx| {
            let name = field.read(cx).value().trim().to_string();
            if name.is_empty() {
                crate::ui::error(window, cx, t!("workspaces.name_required"));
                return false;
            }
            on_ok(name, window, cx);
            true
        },
    );
}

// ----- Model -----

/// The saved workspaces and the last session's tabs, shared by every
/// window, kept in the device store.
pub struct Workspaces {
    pub list: Vec<SavedWorkspace>,
    /// The list was read from the store.
    pub loaded: bool,
    /// The last session's windows still to reopen (`None`: not read yet).
    session: Option<Vec<Layout>>,
    /// Windows the session asked to open that have not taken their tabs.
    pending_windows: usize,
    /// Tabs of each open window, in the order they were opened.
    windows: Vec<(gpui::EntityId, Layout)>,
    /// What was written last as the session.
    written: Option<String>,
    /// The app is quitting: closing windows does not drop their tabs.
    quitting: bool,
    writer: tokio::sync::mpsc::UnboundedSender<(&'static str, String)>,
    _quit: gpui::Subscription,
}

struct WorkspacesGlobal(gpui::Entity<Workspaces>);

impl gpui::Global for WorkspacesGlobal {}

impl Workspaces {
    /// The one of the app (read from the store the first time).
    pub fn global(
        model: &gpui::Entity<crate::state::AppModel>,
        cx: &mut gpui::App,
    ) -> gpui::Entity<Self> {
        use gpui::AppContext;
        if let Some(g) = cx.try_global::<WorkspacesGlobal>() {
            return g.0.clone();
        }
        let store = model.read(cx).ws.store.clone();
        let rt = crate::runtime::handle(cx);
        // One writer, in order; only the newest value of each key.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(&'static str, String)>();
        let writer_store = store.clone();
        rt.spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while let Ok((key, value)) = rx.try_recv() {
                    match batch.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => batch.push((key, value)),
                    }
                }
                for (key, value) in batch {
                    if let Err(e) = writer_store.meta_set(key, &value).await {
                        tracing::warn!(error = %e, key, "could not save the tabs");
                    }
                }
            }
        });
        let entity = cx.new(|cx: &mut gpui::Context<Self>| {
            let quit = cx.on_app_quit(|this, _| {
                this.quitting = true;
                async {}
            });
            crate::runtime::run(
                cx,
                async move {
                    let list = store.meta_get(WORKSPACES_KEY).await?;
                    let session = store.meta_get(SESSION_KEY).await?;
                    Ok::<_, termoak_core::CoreError>((list, session))
                },
                |this: &mut Self, res, cx| {
                    let (list, session) = res.unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "could not read the workspaces");
                        (None, None)
                    });
                    this.list = parse_workspaces(list.as_deref());
                    this.loaded = true;
                    this.session = Some(parse_session(session.as_deref()).windows);
                    this.written = session;
                    cx.notify();
                },
            );
            Workspaces {
                list: Vec::new(),
                loaded: false,
                session: None,
                pending_windows: 0,
                windows: Vec::new(),
                written: None,
                quitting: false,
                writer: tx,
                _quit: quit,
            }
        });
        cx.set_global(WorkspacesGlobal(entity.clone()));
        entity
    }

    fn save_list(&mut self, cx: &mut gpui::Context<Self>) {
        let json = serde_json::to_string(&self.list).unwrap_or_else(|_| "[]".into());
        let _ = self.writer.send((WORKSPACES_KEY, json));
        cx.notify();
    }

    pub fn get(&self, id: Id) -> Option<&SavedWorkspace> {
        self.list.iter().find(|w| w.id == id)
    }

    /// Adds a workspace (at the end).
    pub fn add(&mut self, w: SavedWorkspace, cx: &mut gpui::Context<Self>) {
        self.list.push(w);
        self.save_list(cx);
    }

    pub fn rename(&mut self, id: Id, name: String, cx: &mut gpui::Context<Self>) {
        if let Some(w) = self.list.iter_mut().find(|w| w.id == id) {
            w.name = name;
            w.updated_at = Utc::now();
            self.save_list(cx);
        }
    }

    /// "Replace with the current tabs".
    pub fn set_layout(&mut self, id: Id, layout: Layout, cx: &mut gpui::Context<Self>) {
        if let Some(w) = self.list.iter_mut().find(|w| w.id == id) {
            w.layout = layout;
            w.updated_at = Utc::now();
            self.save_list(cx);
        }
    }

    /// "Add to workspace" from the host menu.
    pub fn add_hosts(&mut self, id: Id, hosts: &[Id], cx: &mut gpui::Context<Self>) {
        if let Some(w) = self.list.iter_mut().find(|w| w.id == id) {
            for h in hosts {
                w.layout.add_host(*h);
            }
            w.updated_at = Utc::now();
            self.save_list(cx);
        }
    }

    pub fn remove(&mut self, id: Id, cx: &mut gpui::Context<Self>) {
        let before = self.list.len();
        self.list.retain(|w| w.id != id);
        if self.list.len() != before {
            self.save_list(cx);
        }
    }

    // ----- Last session -----

    /// The last session was read.
    pub fn session_ready(&self) -> bool {
        self.session.is_some()
    }

    /// The tabs a window that opens now should reopen (the next window of
    /// the last session), and how many more windows it had.
    pub fn take_session_window(&mut self) -> (Option<Layout>, usize) {
        let Some(windows) = self.session.as_mut() else {
            return (None, 0);
        };
        if windows.is_empty() {
            return (None, 0);
        }
        let first = windows.remove(0);
        (Some(first), windows.len())
    }

    /// A window opened to take the next window of the last session.
    pub fn expect_session_window(&mut self) {
        self.pending_windows += 1;
    }

    /// A window that just opened takes one of the windows the session asked
    /// for.
    pub fn claim_session_window(&mut self) -> bool {
        if self.pending_windows > 0 {
            self.pending_windows -= 1;
            true
        } else {
            false
        }
    }

    /// The tabs of a window changed: the session is saved.
    pub fn window_changed(&mut self, window: gpui::EntityId, layout: Layout) {
        match self.windows.iter_mut().find(|(w, _)| *w == window) {
            Some(slot) if slot.1 == layout => return,
            Some(slot) => slot.1 = layout,
            None => self.windows.push((window, layout)),
        }
        self.write_session();
    }

    /// A window closed: its tabs leave the session, unless it was the last
    /// one or the app is quitting (they reopen next time).
    pub fn window_closed(&mut self, window: gpui::EntityId, others_open: bool) {
        if self.quitting || !others_open {
            return;
        }
        let before = self.windows.len();
        self.windows.retain(|(w, _)| *w != window);
        if self.windows.len() != before {
            self.write_session();
        }
    }

    /// "Reopen tabs on start" was turned off: nothing is kept.
    pub fn clear_session(&mut self) {
        self.windows.clear();
        self.write_session();
    }

    fn write_session(&mut self) {
        // Before the last session is read, it is not overwritten.
        if self.session.is_none() {
            return;
        }
        let file = SessionFile {
            windows: self
                .windows
                .iter()
                .map(|(_, l)| l.clone())
                .filter(|l| !l.is_empty())
                .collect(),
        };
        let json = serde_json::to_string(&file).unwrap_or_default();
        if self.written.as_deref() == Some(json.as_str()) {
            return;
        }
        self.written = Some(json.clone());
        let _ = self.writer.send((SESSION_KEY, json));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_core::new_id;

    fn env<'a>(
        running: Option<&'a [Id]>,
        open: &'a [Id],
        exists: &'a dyn Fn(Id) -> bool,
        signed: &'a dyn Fn(Option<Id>) -> bool,
    ) -> Env<'a> {
        Env {
            host_exists: exists,
            signed_in: signed,
            running,
            open_sessions: open,
        }
    }

    #[test]
    fn round_trip_and_format() {
        let (h1, h2, s, a) = (new_id(), new_id(), new_id(), new_id());
        let layout = Layout {
            tabs: vec![
                SavedTab::Terminal {
                    panes: vec![
                        SavedPane::Ssh { host_id: h1 },
                        SavedPane::Server {
                            host_id: Some(h2),
                            session_id: Some(s),
                            account: Some(a),
                            title: "db".into(),
                        },
                        SavedPane::Shell,
                        SavedPane::Serial {
                            path: "/dev/ttyUSB0".into(),
                            baud: 115_200,
                        },
                    ],
                    focused: 2,
                    maximized: true,
                    title: Some("Prod".into()),
                },
                SavedTab::Sftp { host_id: h1 },
            ],
            active: Some(1),
        };
        let ws = SavedWorkspace::new("Prod", layout.clone());
        let json = serde_json::to_string(&vec![ws.clone()]).unwrap();
        assert!(json.contains(r#""kind":"ssh""#));
        assert!(json.contains(r#""kind":"sftp""#));
        assert_eq!(parse_workspaces(Some(&json)), vec![ws]);
        let session = SessionFile {
            windows: vec![layout.clone(), Layout::default()],
        };
        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(parse_session(Some(&json)), session);
        assert_eq!(layout.host_ids().len(), 2);
    }

    #[test]
    fn unknown_or_broken_data_is_skipped() {
        let h = new_id();
        let json = format!(
            r#"{{"windows":[{{"tabs":[{{"kind":"ssh_future"}},{{"kind":"sftp","host_id":"{h}"}},{{"kind":"terminal","panes":[{{"kind":"telnet"}}]}}],"active":1}}]}}"#
        );
        let s = parse_session(Some(&json));
        // A tab with a pane of an unknown kind is left out whole; the SFTP
        // one stays.
        assert_eq!(s.windows[0].tabs, vec![SavedTab::Sftp { host_id: h }]);
        assert_eq!(parse_session(Some("not json")), SessionFile::default());
        assert_eq!(parse_session(None), SessionFile::default());
        assert!(parse_workspaces(Some("{")).is_empty());
        // A broken workspace does not hide the others.
        let good = SavedWorkspace::new("A", Layout::default());
        let json = format!("[{{\"name\":1}},{}]", serde_json::to_string(&good).unwrap());
        assert_eq!(parse_workspaces(Some(&json)), vec![good]);
    }

    #[test]
    fn panes_from_terminals() {
        let (h, s, a, cur) = (new_id(), new_id(), new_id(), new_id());
        assert_eq!(
            pane_of(&TermKind::Local { host_id: h }, "x", None),
            Some(SavedPane::Ssh { host_id: h })
        );
        assert_eq!(
            pane_of(
                &TermKind::Server {
                    host_id: Some(h),
                    session_id: Some(s),
                    account: None,
                    link: None
                },
                "web",
                Some(cur)
            ),
            Some(SavedPane::Server {
                host_id: Some(h),
                session_id: Some(s),
                account: Some(cur),
                title: "web".into()
            })
        );
        let with_account = pane_of(
            &TermKind::Server {
                host_id: None,
                session_id: Some(s),
                account: Some(a),
                link: None,
            },
            "t",
            Some(cur),
        );
        assert!(matches!(with_account, Some(SavedPane::Server { account: Some(x), .. }) if x == a));
        // Nothing to come back to.
        assert_eq!(
            pane_of(
                &TermKind::Server {
                    host_id: None,
                    session_id: None,
                    account: None,
                    link: None
                },
                "t",
                None
            ),
            None
        );
        assert_eq!(pane_of(&TermKind::Shell, "", None), Some(SavedPane::Shell));
    }

    #[test]
    fn restore_skips_missing_and_makes_dormant_tabs() {
        let (h1, gone, s_run, s_end, acc) = (new_id(), new_id(), new_id(), new_id(), new_id());
        let layout = Layout {
            tabs: vec![
                // 0: a split with a deleted host.
                SavedTab::Terminal {
                    panes: vec![
                        SavedPane::Ssh { host_id: gone },
                        SavedPane::Ssh { host_id: h1 },
                        SavedPane::Shell,
                    ],
                    focused: 2,
                    maximized: true,
                    title: None,
                },
                // 1: a running server session alone → dormant.
                SavedTab::Terminal {
                    panes: vec![SavedPane::Server {
                        host_id: Some(h1),
                        session_id: Some(s_run),
                        account: Some(acc),
                        title: "web".into(),
                    }],
                    focused: 0,
                    maximized: false,
                    title: Some("Renamed".into()),
                },
                // 2: one that ended → left out (restoring starts nothing).
                SavedTab::Terminal {
                    panes: vec![SavedPane::Server {
                        host_id: Some(h1),
                        session_id: Some(s_end),
                        account: Some(acc),
                        title: "old".into(),
                    }],
                    focused: 0,
                    maximized: false,
                    title: None,
                },
                // 3: SFTP of the deleted host.
                SavedTab::Sftp { host_id: gone },
            ],
            active: Some(3),
        };
        let exists = |h: Id| h == h1;
        let signed = |_: Option<Id>| true;
        let running = [s_run];
        let p = plan(
            &layout,
            Mode::Restore,
            &env(Some(&running), &[], &exists, &signed),
        );
        assert_eq!(p.tabs.len(), 2);
        assert_eq!(
            p.tabs[0],
            PlanTab::Terminal {
                panes: vec![PlanPane::Ssh(h1), PlanPane::Shell],
                focused: 1,
                maximized: true,
                title: None
            }
        );
        assert_eq!(
            p.tabs[1],
            PlanTab::Dormant {
                session_id: s_run,
                account: acc,
                title: "Renamed".into()
            }
        );
        // gone (pane) + ended session + gone (SFTP).
        assert_eq!(p.skipped, 3);
        // The active tab (SFTP) is gone: the nearest one before it.
        assert_eq!(p.active, Some(1));
    }

    #[test]
    fn unknown_running_sessions_are_tried() {
        let (s, acc) = (new_id(), new_id());
        let layout = Layout {
            tabs: vec![SavedTab::Terminal {
                panes: vec![SavedPane::Server {
                    host_id: None,
                    session_id: Some(s),
                    account: Some(acc),
                    title: "t".into(),
                }],
                focused: 0,
                maximized: false,
                title: None,
            }],
            active: None,
        };
        let exists = |_: Id| true;
        let signed = |_: Option<Id>| true;
        let p = plan(&layout, Mode::Restore, &env(None, &[], &exists, &signed));
        assert!(matches!(p.tabs[0], PlanTab::Dormant { .. }));
        assert_eq!(p.active, None);
        // Signed out: left out.
        let out = |_: Option<Id>| false;
        let p = plan(&layout, Mode::Restore, &env(None, &[], &exists, &out));
        assert!(p.tabs.is_empty());
        assert_eq!(p.skipped, 1);
    }

    #[test]
    fn opening_a_workspace_restarts_ended_sessions() {
        let (h, s_end, s_open, acc) = (new_id(), new_id(), new_id(), new_id());
        let layout = Layout {
            tabs: vec![SavedTab::Terminal {
                panes: vec![
                    SavedPane::Server {
                        host_id: Some(h),
                        session_id: Some(s_end),
                        account: Some(acc),
                        title: "a".into(),
                    },
                    SavedPane::Server {
                        host_id: Some(h),
                        session_id: Some(s_open),
                        account: Some(acc),
                        title: "b".into(),
                    },
                    SavedPane::Ssh { host_id: h },
                ],
                focused: 0,
                maximized: false,
                title: None,
            }],
            active: None,
        };
        let exists = |_: Id| true;
        let signed = |_: Option<Id>| true;
        let running = [s_open];
        let open = [s_open];
        let p = plan(
            &layout,
            Mode::Open,
            &env(Some(&running), &open, &exists, &signed),
        );
        assert_eq!(
            p.tabs,
            vec![PlanTab::Terminal {
                panes: vec![PlanPane::NewServer(h), PlanPane::Ssh(h)],
                focused: 0,
                maximized: false,
                title: None
            }]
        );
        // The session already open is not a loss.
        assert_eq!(p.skipped, 0);
        assert_eq!(p.active, Some(0));
        // A single running session opens attached (not dormant) when
        // opening a workspace.
        let single = Layout {
            tabs: vec![SavedTab::Terminal {
                panes: vec![SavedPane::Server {
                    host_id: None,
                    session_id: Some(s_end),
                    account: Some(acc),
                    title: "x".into(),
                }],
                focused: 0,
                maximized: false,
                title: None,
            }],
            active: None,
        };
        let running = [s_end];
        let p = plan(
            &single,
            Mode::Open,
            &env(Some(&running), &[], &exists, &signed),
        );
        assert!(
            matches!(p.tabs[0], PlanTab::Terminal { ref panes, .. } if matches!(panes[0], PlanPane::Attach { .. }))
        );
    }

    #[test]
    fn too_many_panes_are_cut() {
        let h = new_id();
        let layout = Layout {
            tabs: vec![SavedTab::Terminal {
                panes: vec![SavedPane::Ssh { host_id: h }; MAX_PANES + 2],
                focused: MAX_PANES + 1,
                maximized: false,
                title: None,
            }],
            active: Some(0),
        };
        let exists = |_: Id| true;
        let signed = |_: Option<Id>| true;
        let p = plan(&layout, Mode::Restore, &env(None, &[], &exists, &signed));
        match &p.tabs[0] {
            PlanTab::Terminal { panes, focused, .. } => {
                assert_eq!(panes.len(), MAX_PANES);
                assert!(*focused < MAX_PANES);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(p.skipped, 2);
    }

    #[test]
    fn names_and_adding_hosts() {
        let a = SavedWorkspace::new("Workspace 1", Layout::default());
        assert_eq!(next_name("Workspace", &[]), "Workspace 1");
        assert_eq!(next_name("Workspace", &[a]), "Workspace 2");
        let mut l = Layout::default();
        let h = new_id();
        l.add_host(h);
        assert_eq!(l.tabs, vec![SavedTab::ssh(h)]);
    }
}
