//! SFTP: two panes (this computer ↔ server), browsing, uploading and
//! downloading files and folders through the transfers queue
//! (`crate::transfers`), creating folders, renaming and deleting.
//!
//! - Files and folders dropped from the system file manager go to the
//!   server folder on screen (or to the folder row they are dropped on).
//! - Rows can be dragged between the panes; a server file dragged out of
//!   the window goes to the file manager where the platform supports it
//!   (macOS, Wayland): it is fetched to a temporary file as the drag
//!   starts. Elsewhere there is "Download to…".
//! - "Edit on this computer" downloads a file to a temporary folder, opens
//!   it with the system's default app and uploads every save, checking
//!   first that nobody changed it on the server (size and date).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui::{
    AppContext, ClickEvent, Context, Entity, EntityId, ExternalDragPayload, ExternalPaths,
    FileDragPaths, InteractiveElement, IntoElement, ParentElement, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use parking_lot::Mutex;
use termoak_core::Id;
use termoak_ssh::{Connection, FileEntry, FileKind, Sftp};

use crate::drag::DragPreview;
use crate::runtime;
use crate::state::AppModel;
use crate::transfers::{self, Direction, Finished, JobSpec, Queue};
use crate::ui::{self, IconName};

/// Largest file "Edit on this computer" opens.
const EDIT_MAX: u64 = 64 * 1024 * 1024;
/// Largest server file fetched when it is dragged (to drop it outside).
const DRAG_OUT_MAX: u64 = 256 * 1024 * 1024;
/// How often edited copies are checked for saves.
const EDIT_POLL: Duration = Duration::from_millis(1000);

/// Entry of a listing (local or remote).
#[derive(Clone, Debug)]
struct Entry {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    modified: Option<i64>,
    mode: String,
}

impl From<FileEntry> for Entry {
    fn from(e: FileEntry) -> Self {
        Self {
            is_dir: e.kind == FileKind::Dir,
            name: e.name,
            path: e.path,
            size: e.size,
            modified: e.modified,
            mode: e.mode_string,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Local,
    Remote,
}

enum Conn {
    Connecting,
    Ready(Arc<Sftp>),
    Failed(String),
}

/// Rows dragged from a pane.
#[derive(Clone)]
struct DraggedFiles {
    view: EntityId,
    side: Side,
    entry: Entry,
    /// Server file fetched for a drag out of the window.
    fetched: Arc<Mutex<Option<PathBuf>>>,
}

/// State of a file being edited on this computer.
#[derive(Clone, Debug, PartialEq)]
enum EditState {
    Opening,
    Watching,
    Uploading,
    /// Uploaded at this local time (`HH:MM:SS`).
    Saved(String),
    /// It changed on the server since it was opened.
    Conflict,
    Failed(String),
}

/// A server file open in a local app.
struct EditSession {
    id: u64,
    name: String,
    remote: String,
    /// Temporary folder (deleted when the session ends).
    dir: PathBuf,
    local: PathBuf,
    /// Server date and size when it was last downloaded or uploaded.
    remote_stamp: (Option<i64>, u64),
    /// Local date and size when it was last in sync.
    local_stamp: Option<(SystemTime, u64)>,
    /// A local change waiting to settle (editors save in several steps).
    pending: Option<(SystemTime, u64)>,
    state: EditState,
}

/// SFTP tab.
pub struct SftpView {
    model: Entity<AppModel>,
    host_id: Id,
    title: SharedString,
    conn: Conn,
    /// SSH connection (kept to reconnect the subsystem).
    ssh: Option<Arc<Connection>>,
    local_dir: PathBuf,
    local: Vec<Entry>,
    local_sel: Option<usize>,
    local_input: Entity<InputState>,
    remote_dir: String,
    remote: Vec<Entry>,
    remote_sel: Option<usize>,
    remote_input: Entity<InputState>,
    remote_loading: bool,
    queue: Entity<Queue>,
    edits: Vec<EditSession>,
    next_edit: u64,
    _edit_watch: Option<Task<()>>,
    /// Temporary folder of files fetched for drags out of the window.
    drag_dir: PathBuf,
    _subs: Vec<Subscription>,
}

/// Temporary folder of this app for a purpose (`edit`, `drag`), unique.
fn temp_dir(purpose: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("termoak-{purpose}"))
        .join(uuid::Uuid::new_v4().simple().to_string())
}

/// Local date and size of a file.
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

impl SftpView {
    pub fn new(
        model: Entity<AppModel>,
        host_id: Id,
        conn: Option<Arc<Connection>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let title = format!("SFTP · {}", model.read(cx).host_label(host_id)).into();
        let home = directories::BaseDirs::new()
            .map(|d| d.home_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let local_input =
            cx.new(|cx| InputState::new(window, cx).default_value(home.display().to_string()));
        let remote_input = cx.new(|cx| InputState::new(window, cx).placeholder("/"));
        let queue = transfers::queue(cx);
        let store = model.read(cx).ws.store.clone();
        queue.update(cx, |q, cx| q.load_limit(store, cx));
        let subs = vec![
            cx.subscribe_in(
                &local_input,
                window,
                |this, input, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        let path = PathBuf::from(input.read(cx).value().trim().to_string());
                        this.list_local(path, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &remote_input,
                window,
                |this, input, ev: &InputEvent, window, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        let path = input.read(cx).value().trim().to_string();
                        this.list_remote(path, window, cx);
                    }
                },
            ),
            cx.observe(&queue, |_, _, cx| cx.notify()),
            cx.subscribe_in(&queue, window, |this, _, ev: &Finished, window, cx| {
                this.transfer_finished(ev, window, cx)
            }),
        ];
        let mut view = Self {
            model,
            host_id,
            title,
            conn: Conn::Connecting,
            ssh: conn,
            local_dir: home.clone(),
            local: Vec::new(),
            local_sel: None,
            local_input,
            remote_dir: String::new(),
            remote: Vec::new(),
            remote_sel: None,
            remote_input,
            remote_loading: false,
            queue,
            edits: Vec::new(),
            next_edit: 1,
            _edit_watch: None,
            drag_dir: temp_dir("drag"),
            _subs: subs,
        };
        view.list_local(home, window, cx);
        view.connect(window, cx);
        view
    }

    pub fn title(&self) -> SharedString {
        self.title.clone()
    }

    /// What opens another browser of the same host ("Duplicate session"),
    /// reusing its SSH connection while it is open (no new login).
    pub fn duplicate_request(&self) -> crate::views::OpenRequest {
        crate::views::OpenRequest::Sftp {
            host_id: self.host_id,
            conn: self.ssh.clone().filter(|c| !c.is_closed()),
        }
    }

    /// Closes the SFTP session: cancels its transfers and deletes the
    /// temporary copies (edited files, files fetched for drags).
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        let owner = cx.entity_id();
        self.queue.update(cx, |q, cx| q.cancel_owner(owner, cx));
        let mut dirs: Vec<PathBuf> = self.edits.drain(..).map(|e| e.dir).collect();
        dirs.push(self.drag_dir.clone());
        runtime::handle(cx).spawn(async move {
            for d in dirs {
                let _ = tokio::fs::remove_dir_all(d).await;
            }
        });
        self._edit_watch = None;
        if let Conn::Ready(sftp) =
            std::mem::replace(&mut self.conn, Conn::Failed(t!("sftp.closed").to_string()))
        {
            runtime::handle(cx).spawn(async move { sftp.close().await });
        }
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conn = Conn::Connecting;
        cx.notify();
        let m = self.model.read(cx);
        let ws = m.ws.clone();
        let prompter = m.prompter.clone();
        let use_agent = m.settings.use_agent;
        let host_id = self.host_id;
        let existing = self.ssh.clone().filter(|c| !c.is_closed());
        runtime::run_in(
            cx,
            window,
            async move {
                let conn = match existing {
                    Some(c) => c,
                    None => ws
                        .connect(host_id, prompter, use_agent)
                        .await
                        .map_err(crate::state::api_error)?,
                };
                let sftp = conn
                    .sftp()
                    .await
                    .map_err(|e| crate::state::api_error(termoak_client::ClientError::from(e)))?;
                let home = sftp.home().await.unwrap_or_else(|_| "/".into());
                Ok::<_, String>((conn, Arc::new(sftp), home))
            },
            |this, res, window, cx| {
                match res {
                    Ok((conn, sftp, home)) => {
                        this.ssh = Some(conn);
                        this.conn = Conn::Ready(sftp);
                        this.list_remote(home, window, cx);
                    }
                    Err(e) => this.conn = Conn::Failed(e),
                }
                cx.notify();
            },
        );
    }

    fn sftp(&self) -> Option<Arc<Sftp>> {
        match &self.conn {
            Conn::Ready(s) => Some(s.clone()),
            _ => None,
        }
    }

    // ----- Listings -----

    fn list_local(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let dir_task = dir.clone();
        runtime::run_in(
            cx,
            window,
            async move { read_local(&dir_task).await },
            move |this, res, window, cx| {
                match res {
                    Ok(entries) => {
                        if this.local_dir != dir {
                            this.local_sel = None;
                        }
                        this.local_dir = dir.clone();
                        this.local = entries;
                        this.local_sel = this.local_sel.filter(|i| *i < this.local.len());
                        this.local_input.update(cx, |i, cx| {
                            i.set_value(dir.display().to_string(), window, cx)
                        });
                    }
                    Err(e) => ui::error(
                        window,
                        cx,
                        t!("sftp.open_failed", path = dir.display(), error = e),
                    ),
                }
                cx.notify();
            },
        );
    }

    fn list_remote(&mut self, dir: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp() else {
            return;
        };
        self.remote_loading = true;
        cx.notify();
        let dir_task = dir.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                let canonical = sftp
                    .canonicalize(&dir_task)
                    .await
                    .unwrap_or(dir_task.clone());
                let mut list: Vec<Entry> = sftp
                    .list(&canonical)
                    .await?
                    .into_iter()
                    .filter(|e| e.name != "." && e.name != "..")
                    .map(Entry::from)
                    .collect();
                sort_entries(&mut list);
                Ok::<_, termoak_ssh::SshError>((canonical, list))
            },
            move |this, res, window, cx| {
                this.remote_loading = false;
                match res {
                    Ok((canonical, list)) => {
                        if this.remote_dir != canonical {
                            this.remote_sel = None;
                        }
                        this.remote_dir = canonical.clone();
                        this.remote = list;
                        this.remote_sel = this.remote_sel.filter(|i| *i < this.remote.len());
                        this.remote_input
                            .update(cx, |i, cx| i.set_value(canonical, window, cx));
                    }
                    Err(e) => ui::error(window, cx, t!("sftp.open_failed", path = dir, error = e)),
                }
                cx.notify();
            },
        );
    }

    fn refresh(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        match side {
            Side::Local => self.list_local(self.local_dir.clone(), window, cx),
            Side::Remote => self.list_remote(self.remote_dir.clone(), window, cx),
        }
    }

    fn go_up(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        match side {
            Side::Local => {
                if let Some(parent) = self.local_dir.parent() {
                    self.list_local(parent.to_path_buf(), window, cx);
                }
            }
            Side::Remote => {
                let parent = remote_parent(&self.remote_dir);
                self.list_remote(parent, window, cx);
            }
        }
    }

    fn open_entry(&mut self, side: Side, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match side {
            Side::Local => {
                if let Some(e) = self.local.get(ix).cloned()
                    && e.is_dir
                {
                    self.list_local(PathBuf::from(e.path), window, cx);
                }
            }
            Side::Remote => {
                if let Some(e) = self.remote.get(ix).cloned() {
                    if e.is_dir {
                        self.list_remote(e.path, window, cx);
                    } else {
                        self.download(window, cx);
                    }
                }
            }
        }
    }

    // ----- Transfers -----

    fn enqueue(
        &mut self,
        direction: Direction,
        local: PathBuf,
        remote: String,
        is_dir: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(sftp) = self.sftp() else {
            return;
        };
        let spec = JobSpec {
            direction,
            local,
            remote,
            is_dir,
            host: self.model.read(cx).host_label(self.host_id),
            sftp,
            owner: Some(cx.entity_id()),
        };
        self.queue.update(cx, |q, cx| {
            q.add(spec, cx);
        });
    }

    /// Uploads files and folders of this computer to a server folder.
    pub fn upload_paths(&mut self, paths: Vec<PathBuf>, dir: String, cx: &mut Context<Self>) {
        for p in paths {
            let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            let is_dir = p.is_dir();
            let remote = termoak_ssh::sftp::join(&dir, &name);
            self.enqueue(Direction::Upload, p, remote, is_dir, cx);
        }
    }

    /// Files dropped from the system file manager.
    pub fn drop_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        if self.sftp().is_none() {
            return;
        }
        let dir = self.remote_dir.clone();
        self.upload_paths(paths, dir, cx);
    }

    fn upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.local_sel.and_then(|i| self.local.get(i)).cloned() else {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.select_local_file"),
            );
            return;
        };
        let dir = self.remote_dir.clone();
        self.upload_paths(vec![PathBuf::from(entry.path)], dir, cx);
    }

    /// Downloads a server entry to a local folder.
    fn download_entry(&mut self, entry: &Entry, dir: &Path, cx: &mut Context<Self>) {
        self.enqueue(
            Direction::Download,
            dir.join(&entry.name),
            entry.path.clone(),
            entry.is_dir,
            cx,
        );
    }

    fn selected_remote(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<Entry> {
        let entry = self.remote_sel.and_then(|i| self.remote.get(i)).cloned();
        if entry.is_none() {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.select_remote_file"),
            );
        }
        entry
    }

    fn download(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_remote(window, cx) else {
            return;
        };
        let dir = self.local_dir.clone();
        self.download_entry(&entry, &dir, cx);
    }

    /// "Download to…": a folder chosen in the system dialog.
    fn download_to(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_remote(window, cx) else {
            return;
        };
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t!("sftp.download_here")),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this, cx| this.download_entry(&entry, &dir, cx));
        })
        .detach();
    }

    fn transfer_finished(&mut self, ev: &Finished, window: &mut Window, cx: &mut Context<Self>) {
        match ev.direction {
            Direction::Upload => {
                if remote_parent(&ev.remote) == self.remote_dir.trim_end_matches('/')
                    || remote_parent(&ev.remote) == self.remote_dir
                {
                    self.refresh(Side::Remote, window, cx);
                }
            }
            Direction::Download => {
                if ev.local.parent() == Some(self.local_dir.as_path()) {
                    self.refresh(Side::Local, window, cx);
                }
            }
        }
    }

    /// What a drag out of the window needs to fetch a server file.
    fn drag_source(&self) -> Option<(Arc<Sftp>, PathBuf)> {
        Some((self.sftp()?, self.drag_dir.clone()))
    }

    /// Rows dragged from the other pane were dropped on `side`.
    fn drop_rows(
        &mut self,
        d: &DraggedFiles,
        side: Side,
        into: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if d.view != cx.entity_id() || d.side == side {
            return;
        }
        match side {
            Side::Remote => {
                let dir = into.unwrap_or_else(|| self.remote_dir.clone());
                self.upload_paths(vec![PathBuf::from(&d.entry.path)], dir, cx);
            }
            Side::Local => {
                let dir = into
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.local_dir.clone());
                self.download_entry(&d.entry, &dir, cx);
            }
        }
    }

    // ----- Edit on this computer -----

    fn edit_locally(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_remote(window, cx) else {
            return;
        };
        let Some(sftp) = self.sftp() else {
            return;
        };
        if entry.is_dir {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.edit_files_only"),
            );
            return;
        }
        if let Some(e) = self.edits.iter().find(|e| e.remote == entry.path) {
            // Already open: just bring it up again.
            cx.open_with_system(&e.local);
            return;
        }
        if entry.size > EDIT_MAX {
            ui::error(
                window,
                cx,
                t!("sftp.edit_too_big", size = ui::format_bytes(EDIT_MAX)),
            );
            return;
        }
        let id = self.next_edit;
        self.next_edit += 1;
        let dir = temp_dir("edit");
        let local = dir.join(&entry.name);
        self.edits.push(EditSession {
            id,
            name: entry.name.clone(),
            remote: entry.path.clone(),
            dir: dir.clone(),
            local: local.clone(),
            remote_stamp: (entry.modified, entry.size),
            local_stamp: None,
            pending: None,
            state: EditState::Opening,
        });
        cx.notify();
        let remote = entry.path.clone();
        let target = local.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                tokio::fs::create_dir_all(&dir)
                    .await
                    .map_err(|e| e.to_string())?;
                let file = tokio::fs::File::create(&target)
                    .await
                    .map_err(|e| e.to_string())?;
                sftp.download(&remote, file, None)
                    .await
                    .map_err(|e| e.to_string())?;
                let st = sftp.stat(&remote).await.map_err(|e| e.to_string())?;
                Ok::<_, String>((st.modified, st.size))
            },
            move |this, res, window, cx| {
                let Some(e) = this.edits.iter_mut().find(|e| e.id == id) else {
                    return;
                };
                match res {
                    Ok(remote_stamp) => {
                        e.remote_stamp = remote_stamp;
                        e.local_stamp = stamp(&e.local);
                        e.state = EditState::Watching;
                        cx.open_with_system(&local);
                        this.start_edit_watch(cx);
                    }
                    Err(err) => {
                        e.state = EditState::Failed(err.clone());
                        ui::error(window, cx, t!("sftp.edit_open_failed", error = err));
                    }
                }
                cx.notify();
            },
        );
    }

    fn start_edit_watch(&mut self, cx: &mut Context<Self>) {
        if self._edit_watch.is_some() {
            return;
        }
        self._edit_watch = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EDIT_POLL).await;
                let go_on = this
                    .update(cx, |this, cx| {
                        this.check_edits(cx);
                        !this.edits.is_empty()
                    })
                    .unwrap_or(false);
                if !go_on {
                    let _ = this.update(cx, |this, _| this._edit_watch = None);
                    break;
                }
            }
        }));
    }

    /// Uploads the copies that were saved (once their date and size stop
    /// changing).
    fn check_edits(&mut self, cx: &mut Context<Self>) {
        let mut ready = Vec::new();
        for e in &mut self.edits {
            if !matches!(
                e.state,
                EditState::Watching | EditState::Saved(_) | EditState::Failed(_)
            ) {
                continue;
            }
            // Never synced (the download failed): nothing to upload.
            if e.local_stamp.is_none() {
                continue;
            }
            let now = stamp(&e.local);
            if now.is_none() || now == e.local_stamp {
                e.pending = None;
                continue;
            }
            if e.pending == now {
                e.pending = None;
                ready.push(e.id);
            } else {
                e.pending = now;
            }
        }
        for id in ready {
            self.upload_edit(id, false, cx);
        }
    }

    /// Uploads an edited copy. Without `force`, it first checks that the
    /// server file is as it was when downloaded.
    fn upload_edit(&mut self, id: u64, force: bool, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp() else {
            return;
        };
        let Some(e) = self.edits.iter_mut().find(|e| e.id == id) else {
            return;
        };
        e.state = EditState::Uploading;
        let local = e.local.clone();
        let remote = e.remote.clone();
        let expected = e.remote_stamp;
        let sent = stamp(&local);
        cx.notify();
        runtime::run(
            cx,
            async move {
                if !force {
                    let st = sftp.stat(&remote).await.map_err(|e| e.to_string())?;
                    if (st.modified, st.size) != expected {
                        return Ok(None);
                    }
                }
                let file = tokio::fs::File::open(&local)
                    .await
                    .map_err(|e| e.to_string())?;
                sftp.upload(file, &remote, None)
                    .await
                    .map_err(|e| e.to_string())?;
                let st = sftp.stat(&remote).await.map_err(|e| e.to_string())?;
                Ok::<_, String>(Some((st.modified, st.size)))
            },
            move |this, res, cx| {
                let Some(e) = this.edits.iter_mut().find(|e| e.id == id) else {
                    return;
                };
                match res {
                    Ok(Some(remote_stamp)) => {
                        e.remote_stamp = remote_stamp;
                        e.local_stamp = sent;
                        e.state =
                            EditState::Saved(chrono::Local::now().format("%H:%M:%S").to_string());
                    }
                    Ok(None) => e.state = EditState::Conflict,
                    Err(err) => {
                        // Tried again on the next save.
                        e.local_stamp = sent;
                        e.state = EditState::Failed(err);
                    }
                }
                cx.notify();
            },
        );
    }

    /// Conflict: replaces the local copy with the server's version.
    fn reload_edit(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(sftp) = self.sftp() else {
            return;
        };
        let Some(e) = self.edits.iter_mut().find(|e| e.id == id) else {
            return;
        };
        e.state = EditState::Opening;
        let local = e.local.clone();
        let remote = e.remote.clone();
        cx.notify();
        runtime::run(
            cx,
            async move {
                let file = tokio::fs::File::create(&local)
                    .await
                    .map_err(|e| e.to_string())?;
                sftp.download(&remote, file, None)
                    .await
                    .map_err(|e| e.to_string())?;
                let st = sftp.stat(&remote).await.map_err(|e| e.to_string())?;
                Ok::<_, String>((st.modified, st.size))
            },
            move |this, res, cx| {
                let Some(e) = this.edits.iter_mut().find(|e| e.id == id) else {
                    return;
                };
                match res {
                    Ok(remote_stamp) => {
                        e.remote_stamp = remote_stamp;
                        e.local_stamp = stamp(&e.local);
                        e.state = EditState::Watching;
                    }
                    Err(err) => e.state = EditState::Failed(err),
                }
                cx.notify();
            },
        );
    }

    /// Stops editing a file and deletes its local copy.
    fn close_edit(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(ix) = self.edits.iter().position(|e| e.id == id) {
            let e = self.edits.remove(ix);
            runtime::handle(cx).spawn(async move {
                let _ = tokio::fs::remove_dir_all(e.dir).await;
            });
        }
        cx.notify();
    }

    // ----- Operations -----

    fn mkdir(&mut self, side: Side, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("sftp.new_folder_placeholder")));
        ui::focus_later(&input, window, cx);
        let weak = cx.entity().downgrade();
        let input_ok = input.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("sftp.new_folder"),
            t!("common.create"),
            400.,
            move |_, cx| ui::field(t!("common.name"), Input::new(&input), cx).into_any_element(),
            move |window, cx| {
                let name = input_ok.read(cx).value().trim().to_string();
                if name.is_empty() || name.contains('/') {
                    ui::error(window, cx, t!("sftp.invalid_folder_name"));
                    return false;
                }
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| v.do_mkdir(side, name, window, cx));
                }
                true
            },
        );
    }

    fn do_mkdir(&mut self, side: Side, name: String, window: &mut Window, cx: &mut Context<Self>) {
        match side {
            Side::Local => {
                let path = self.local_dir.join(&name);
                runtime::run_in(
                    cx,
                    window,
                    async move { tokio::fs::create_dir(&path).await },
                    |this, res, window, cx| match res {
                        Ok(()) => this.refresh(Side::Local, window, cx),
                        Err(e) => ui::error(window, cx, t!("sftp.mkdir_failed", error = e)),
                    },
                );
            }
            Side::Remote => {
                let Conn::Ready(sftp) = &self.conn else {
                    return;
                };
                let sftp = sftp.clone();
                let path = termoak_ssh::sftp::join(&self.remote_dir, &name);
                runtime::run_in(
                    cx,
                    window,
                    async move { sftp.mkdir(&path).await },
                    |this, res, window, cx| match res {
                        Ok(()) => this.refresh(Side::Remote, window, cx),
                        Err(e) => ui::error(window, cx, t!("sftp.mkdir_failed", error = e)),
                    },
                );
            }
        }
    }

    fn rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.remote_sel.and_then(|i| self.remote.get(i)).cloned() else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(entry.name.clone()));
        ui::focus_later(&input, window, cx);
        let weak = cx.entity().downgrade();
        let input_ok = input.clone();
        ui::open_form_dialog(
            window,
            cx,
            t!("sftp.rename"),
            t!("sftp.rename"),
            400.,
            move |_, cx| ui::field(t!("sftp.new_name"), Input::new(&input), cx).into_any_element(),
            move |window, cx| {
                let name = input_ok.read(cx).value().trim().to_string();
                if name.is_empty() || name.contains('/') {
                    ui::error(window, cx, t!("sftp.invalid_name"));
                    return false;
                }
                let from = entry.path.clone();
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| {
                        let Conn::Ready(sftp) = &v.conn else { return };
                        let sftp = sftp.clone();
                        let to = termoak_ssh::sftp::join(&v.remote_dir, &name);
                        runtime::run_in(
                            cx,
                            window,
                            async move { sftp.rename(&from, &to).await },
                            |this, res, window, cx| match res {
                                Ok(()) => this.refresh(Side::Remote, window, cx),
                                Err(e) => {
                                    ui::error(window, cx, t!("sftp.rename_failed", error = e))
                                }
                            },
                        );
                    });
                }
                true
            },
        );
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.remote_sel.and_then(|i| self.remote.get(i)).cloned() else {
            return;
        };
        let weak = cx.entity().downgrade();
        let message = if entry.is_dir {
            t!("sftp.delete_dir_message", name = entry.name)
        } else {
            t!("sftp.delete_file_message", name = entry.name)
        };
        ui::confirm(
            window,
            cx,
            t!("sftp.delete_title"),
            message,
            t!("sftp.delete"),
            true,
            move |window, cx| {
                let entry = entry.clone();
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| {
                        let Conn::Ready(sftp) = &v.conn else { return };
                        let sftp = sftp.clone();
                        runtime::run_in(
                            cx,
                            window,
                            async move {
                                if entry.is_dir {
                                    sftp.remove_dir(&entry.path, true).await
                                } else {
                                    sftp.remove_file(&entry.path).await
                                }
                            },
                            |this, res, window, cx| match res {
                                Ok(()) => this.refresh(Side::Remote, window, cx),
                                Err(e) => {
                                    ui::error(window, cx, t!("sftp.delete_failed", error = e))
                                }
                            },
                        );
                    });
                }
            },
        );
    }

    // ----- Rendering -----

    fn render_pane(&self, side: Side, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let (entries, selected, input, title, icon) = match side {
            Side::Local => (
                &self.local,
                self.local_sel,
                &self.local_input,
                t!("sftp.this_computer"),
                IconName::Monitor,
            ),
            Side::Remote => (
                &self.remote,
                self.remote_sel,
                &self.remote_input,
                SharedString::from(self.model.read(cx).host_label(self.host_id)),
                IconName::Server,
            ),
        };
        let ready = matches!(self.conn, Conn::Ready(_));
        let has_sel = selected.is_some();
        let sel_is_file = selected
            .and_then(|i| entries.get(i))
            .is_some_and(|e| !e.is_dir);
        let id_prefix = if side == Side::Local {
            "local"
        } else {
            "remote"
        };
        let view_id = cx.entity_id();

        let mut actions = h_flex().gap_1().child(
            Button::new((id_prefix, 0usize))
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::ArrowUp))
                .tooltip(t!("sftp.parent_folder"))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.go_up(side, window, cx)
                })),
        );
        actions = actions
            .child(
                Button::new((id_prefix, 1usize))
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::RefreshCw))
                    .tooltip(t!("common.refresh"))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.refresh(side, window, cx)
                    })),
            )
            .child(
                Button::new((id_prefix, 2usize))
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::FolderPlus))
                    .tooltip(t!("sftp.new_folder"))
                    .disabled(side == Side::Remote && !ready)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.mkdir(side, window, cx)
                    })),
            );
        if side == Side::Remote {
            actions = actions
                .child(
                    Button::new((id_prefix, 3usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::Pencil))
                        .tooltip(t!("sftp.rename"))
                        .disabled(!has_sel)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.rename(window, cx)),
                        ),
                )
                .child(
                    Button::new((id_prefix, 4usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::Trash))
                        .tooltip(t!("sftp.delete"))
                        .disabled(!has_sel)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.delete(window, cx)),
                        ),
                )
                .child(
                    Button::new((id_prefix, 6usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::FilePen))
                        .tooltip(t!("sftp.edit_locally"))
                        .disabled(!sel_is_file || !ready)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.edit_locally(window, cx)
                        })),
                )
                .child(
                    Button::new((id_prefix, 7usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::FolderDown))
                        .tooltip(t!("sftp.download_to"))
                        .disabled(!has_sel || !ready)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.download_to(window, cx)
                        })),
                )
                .child(
                    Button::new((id_prefix, 5usize))
                        .xsmall()
                        .primary()
                        .icon(ui::icon(IconName::Download))
                        .label(t!("sftp.download"))
                        .disabled(!has_sel || !ready)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.download(window, cx)
                        })),
                );
        } else {
            actions = actions.child(
                Button::new((id_prefix, 5usize))
                    .xsmall()
                    .primary()
                    .icon(ui::icon(IconName::Upload))
                    .label(t!("sftp.upload"))
                    .disabled(!has_sel || !ready)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.upload(window, cx)),
                    ),
            );
        }

        let body: gpui::AnyElement = match (&self.conn, side) {
            (Conn::Connecting, Side::Remote) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new())
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("sftp.connecting")),
                )
                .into_any_element(),
            (Conn::Failed(e), Side::Remote) => {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .p_6()
                    .child(
                        div()
                            .text_sm()
                            .text_center()
                            .text_color(theme.danger)
                            .child(e.clone()),
                    )
                    .child(
                        Button::new("sftp-retry")
                            .primary()
                            .label(t!("common.retry"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.connect(window, cx)
                            })),
                    )
                    .into_any_element()
            }
            _ => {
                let rows = entries.iter().take(3000).enumerate().map(|(i, e)| {
                    let active = selected == Some(i);
                    let dragged = DraggedFiles {
                        view: view_id,
                        side,
                        entry: e.clone(),
                        fetched: Arc::new(Mutex::new(None)),
                    };
                    let weak = cx.entity().downgrade();
                    let folder = e.is_dir.then(|| e.path.clone());
                    let folder_rows = folder.clone();
                    let drop_target = theme.drop_target;
                    h_flex()
                        .id((id_prefix, 100 + i))
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .text_sm()
                        .cursor_pointer()
                        .when(active, |this| this.bg(theme.list_active))
                        .when(!active, |this| this.hover(|s| s.bg(theme.list_hover)))
                        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                            if ev.click_count() >= 2 {
                                this.open_entry(side, i, window, cx);
                            } else {
                                match side {
                                    Side::Local => this.local_sel = Some(i),
                                    Side::Remote => this.remote_sel = Some(i),
                                }
                                cx.notify();
                            }
                        }))
                        .on_drag(dragged, move |d, _, _, cx| {
                            if let Some(source) =
                                weak.upgrade().and_then(|v| v.read(cx).drag_source())
                            {
                                fetch_for_drag(d, source, cx);
                            }
                            let icon = if d.entry.is_dir {
                                IconName::Folder
                            } else {
                                IconName::File
                            };
                            cx.new(|_| DragPreview::new(d.entry.name.clone().into(), icon))
                        })
                        .external_drag_payload(|d: &DraggedFiles, _, _| {
                            let path = match d.side {
                                Side::Local => Some(PathBuf::from(&d.entry.path)),
                                Side::Remote => d.fetched.lock().clone(),
                            }?;
                            Some(ExternalDragPayload::Files(FileDragPaths::new([(
                                path,
                                d.entry.is_dir && d.side == Side::Local,
                            )])))
                        })
                        // A folder row takes what is dropped on it.
                        .when_some(folder, |this, path| {
                            let p2 = path.clone();
                            this.drag_over::<ExternalPaths>(move |s, _, _, _| s.bg(drop_target))
                                .drag_over::<DraggedFiles>(move |s, _, _, _| s.bg(drop_target))
                                .on_drop(cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                                    cx.stop_propagation();
                                    if side == Side::Remote {
                                        this.upload_paths(paths.paths().to_vec(), path.clone(), cx);
                                    }
                                }))
                                .on_drop(cx.listener(move |this, d: &DraggedFiles, _, cx| {
                                    cx.stop_propagation();
                                    if d.entry.path != p2 {
                                        this.drop_rows(d, side, folder_rows.clone(), cx);
                                    }
                                }))
                        })
                        .child(
                            ui::icon(if e.is_dir {
                                IconName::Folder
                            } else {
                                IconName::File
                            })
                            .size(px(14.))
                            .text_color(if e.is_dir {
                                theme.primary
                            } else {
                                theme.muted_foreground
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(e.name.clone()),
                        )
                        .child(
                            div()
                                .w(px(80.))
                                .text_right()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(if e.is_dir {
                                    String::new()
                                } else {
                                    ui::format_bytes(e.size)
                                }),
                        )
                        .child(
                            div()
                                .w(px(120.))
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(e.modified.map(ui::format_secs).unwrap_or_default()),
                        )
                        .when(side == Side::Remote, |this| {
                            this.child(
                                div()
                                    .w(px(84.))
                                    .text_xs()
                                    .font_family(ui::mono_family(cx))
                                    .text_color(theme.muted_foreground)
                                    .child(e.mode.clone()),
                            )
                        })
                });
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id((id_prefix, 99usize))
                            .size_full()
                            .overflow_y_scrollbar()
                            // Without their own id, both panes (and those of
                            // other SFTP tabs) share the scroll position: the
                            // default id is the source line.
                            .id(gpui::ElementId::Name(
                                format!("sftp-{}-{id_prefix}", cx.entity_id()).into(),
                            ))
                            .children(rows)
                            .when(entries.is_empty(), |this| {
                                this.child(
                                    div()
                                        .p_4()
                                        .text_sm()
                                        .text_color(theme.muted_foreground)
                                        .child(t!("sftp.empty_folder")),
                                )
                            }),
                    )
                    .into_any_element()
            }
        };

        let drop_border = theme.drop_target;
        let accepts = move |d: &DraggedFiles| d.view == view_id && d.side != side;
        v_flex()
            .id(("sftp-pane", side as usize))
            .flex_1()
            .min_w_0()
            .h_full()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .overflow_hidden()
            .when(side == Side::Remote && ready, |this| {
                this.drag_over::<ExternalPaths>(move |s, _, _, _| s.border_color(drop_border))
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                        this.drop_paths(paths.paths().to_vec(), cx)
                    }))
            })
            .drag_over::<DraggedFiles>(move |s, d, _, _| {
                if accepts(d) {
                    s.border_color(drop_border)
                } else {
                    s
                }
            })
            .on_drop(
                cx.listener(move |this, d: &DraggedFiles, _, cx| this.drop_rows(d, side, None, cx)),
            )
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(ui::icon(icon).size(px(16.)))
                    .child(div().font_semibold().text_sm().child(title))
                    .child(div().flex_1())
                    .child(actions),
            )
            .child(div().px_3().py_2().child(Input::new(input).small()))
            .child(body)
    }

    /// Chips of the files being edited on this computer.
    fn render_edits(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if self.edits.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let chips = self.edits.iter().map(|e| {
            let id = e.id;
            let (text, color): (SharedString, gpui::Hsla) = match &e.state {
                EditState::Opening => (t!("sftp.edit.opening"), theme.muted_foreground),
                EditState::Watching => (t!("sftp.edit.watching"), theme.muted_foreground),
                EditState::Uploading => (t!("sftp.edit.uploading"), theme.info),
                EditState::Saved(at) => (t!("sftp.edit.saved", time = at.clone()), theme.success),
                EditState::Conflict => (t!("sftp.edit.conflict"), theme.warning),
                EditState::Failed(err) => {
                    (t!("sftp.edit.failed", error = err.clone()), theme.danger)
                }
            };
            let conflict = e.state == EditState::Conflict;
            let local = e.local.clone();
            h_flex()
                .id(("sftp-edit", id as usize))
                .gap_1p5()
                .pl_2()
                .pr_1()
                .py_0p5()
                .items_center()
                .rounded(theme.radius)
                .border_1()
                .border_color(if conflict {
                    theme.warning
                } else {
                    theme.border
                })
                .bg(theme.secondary)
                .text_xs()
                .child(ui::icon(IconName::FilePen).size(px(12.)))
                .child(div().font_medium().child(e.name.clone()))
                .child(
                    div()
                        .max_w(px(320.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(color)
                        .child(text),
                )
                .when(conflict, |this| {
                    this.child(
                        Button::new(("sftp-edit-overwrite", id as usize))
                            .xsmall()
                            .warning()
                            .label(t!("sftp.edit.overwrite"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.upload_edit(id, true, cx)
                            })),
                    )
                    .child(
                        Button::new(("sftp-edit-reload", id as usize))
                            .xsmall()
                            .ghost()
                            .label(t!("sftp.edit.reload"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.reload_edit(id, cx)
                            })),
                    )
                })
                .child(
                    Button::new(("sftp-edit-open", id as usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::ExternalLink))
                        .tooltip(t!("sftp.edit.open_again"))
                        .on_click(move |_: &ClickEvent, _, cx| cx.open_with_system(&local)),
                )
                .child(
                    Button::new(("sftp-edit-close", id as usize))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::X))
                        .tooltip(t!("sftp.edit.close"))
                        .on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| this.close_edit(id, cx)),
                        ),
                )
        });
        Some(
            h_flex()
                .flex_wrap()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("sftp.edit.title")),
                )
                .children(chips)
                .into_any_element(),
        )
    }
}

impl Render for SftpView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let local = self.render_pane(Side::Local, cx);
        let remote = self.render_pane(Side::Remote, cx);
        let edits = self.render_edits(cx);
        let store = self.model.read(cx).ws.store.clone();
        let transfers = transfers::render_panel(&self.queue.clone(), store, cx);
        let theme = cx.theme();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(theme.background)
            .children(edits)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(local)
                    .child(remote),
            )
            .children(transfers)
    }
}

/// Starts fetching a dragged server file, so that it can be dropped
/// outside the window (the platform asks for it when the pointer leaves).
fn fetch_for_drag(d: &DraggedFiles, (sftp, drag_dir): (Arc<Sftp>, PathBuf), cx: &mut gpui::App) {
    if d.side != Side::Remote || d.entry.is_dir || d.entry.size > DRAG_OUT_MAX {
        return;
    }
    let dir = drag_dir.join(uuid::Uuid::new_v4().simple().to_string());
    let target = dir.join(&d.entry.name);
    let remote = d.entry.path.clone();
    let fetched = d.fetched.clone();
    runtime::handle(cx).spawn(async move {
        if tokio::fs::create_dir_all(&dir).await.is_err() {
            return;
        }
        let Ok(file) = tokio::fs::File::create(&target).await else {
            return;
        };
        if sftp.download(&remote, file, None).await.is_ok() {
            *fetched.lock() = Some(target);
        }
    });
}

/// Lists a local folder (folders first).
async fn read_local(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut out = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
        // Downloads in progress.
        if name.ends_with(".termoak-part") {
            continue;
        }
        let meta = e.metadata().await.ok();
        out.push(Entry {
            path: e.path().display().to_string(),
            is_dir: meta.as_ref().is_some_and(|m| m.is_dir()),
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64),
            mode: String::new(),
            name,
        });
    }
    sort_entries(&mut out);
    Ok(out)
}

fn sort_entries(list: &mut [Entry]) {
    list.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// Parent folder of a remote path (POSIX).
fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parents_and_stamps() {
        assert_eq!(remote_parent("/home/ana/file.txt"), "/home/ana");
        assert_eq!(remote_parent("/etc/"), "/");
        assert_eq!(remote_parent("/"), "/");
        let dir = temp_dir("test");
        assert!(dir.starts_with(std::env::temp_dir().join("termoak-test")));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.txt");
        std::fs::write(&f, b"abc").unwrap();
        assert_eq!(stamp(&f).map(|s| s.1), Some(3));
        assert_eq!(stamp(&dir.join("missing")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
