//! SFTP: two panes (this computer ↔ server), browsing, uploading and
//! downloading files with progress, creating folders, renaming and deleting.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::progress::Progress;
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_ssh::{Connection, FileEntry, FileKind, Sftp};

use crate::runtime;
use crate::state::AppModel;
use crate::ui::{self, IconName};

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

enum TransferState {
    Running,
    Done,
    Failed(String),
}

struct Transfer {
    name: String,
    upload: bool,
    total: u64,
    done: Arc<AtomicU64>,
    state: TransferState,
}

enum Conn {
    Connecting,
    Ready(Arc<Sftp>),
    Failed(String),
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
    transfers: Vec<Transfer>,
    _ticker: Option<Task<()>>,
    _subs: Vec<Subscription>,
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
            transfers: Vec::new(),
            _ticker: None,
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

    /// Closes the SFTP session.
    pub fn shutdown(&mut self, cx: &mut Context<Self>) {
        if let Conn::Ready(sftp) =
            std::mem::replace(&mut self.conn, Conn::Failed(t!("sftp.closed").to_string()))
        {
            runtime::handle(cx).spawn(async move { sftp.close().await });
        }
        self._ticker = None;
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
                    None => ws.connect(host_id, prompter, use_agent).await?,
                };
                let sftp = conn
                    .sftp()
                    .await
                    .map_err(termoak_client::ClientError::from)?;
                let home = sftp.home().await.unwrap_or_else(|_| "/".into());
                Ok::<_, termoak_client::ClientError>((conn, Arc::new(sftp), home))
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
                        this.local_dir = dir.clone();
                        this.local = entries;
                        this.local_sel = None;
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
        let Conn::Ready(sftp) = &self.conn else {
            return;
        };
        let sftp = sftp.clone();
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
                        this.remote_dir = canonical.clone();
                        this.remote = list;
                        this.remote_sel = None;
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

    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self._ticker.is_some() {
            return;
        }
        self._ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        cx.notify();
                        this.transfers
                            .iter()
                            .any(|t| matches!(t.state, TransferState::Running))
                    })
                    .unwrap_or(false);
                if !running {
                    let _ = this.update(cx, |this, _| this._ticker = None);
                    break;
                }
            }
        }));
    }

    fn upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Conn::Ready(sftp) = &self.conn else {
            return;
        };
        let Some(entry) = self.local_sel.and_then(|i| self.local.get(i)).cloned() else {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.select_local_file"),
            );
            return;
        };
        if entry.is_dir {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.upload_files_only"),
            );
            return;
        }
        let sftp = sftp.clone();
        let remote_path = termoak_ssh::sftp::join(&self.remote_dir, &entry.name);
        let done = Arc::new(AtomicU64::new(0));
        let ix = self.transfers.len();
        self.transfers.push(Transfer {
            name: entry.name.clone(),
            upload: true,
            total: entry.size,
            done: done.clone(),
            state: TransferState::Running,
        });
        self.start_ticker(cx);
        let local_path = entry.path.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                let file = tokio::fs::File::open(&local_path)
                    .await
                    .map_err(termoak_ssh::SshError::from)?;
                let d = done.clone();
                let progress = move |n: u64| d.store(n, Ordering::Relaxed);
                sftp.upload(file, &remote_path, Some(&progress)).await
            },
            move |this, res, window, cx| {
                if let Some(t) = this.transfers.get_mut(ix) {
                    t.state = match res {
                        Ok(_) => TransferState::Done,
                        Err(e) => TransferState::Failed(e),
                    };
                }
                this.refresh(Side::Remote, window, cx);
                cx.notify();
            },
        );
    }

    fn download(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Conn::Ready(sftp) = &self.conn else {
            return;
        };
        let Some(entry) = self.remote_sel.and_then(|i| self.remote.get(i)).cloned() else {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.select_remote_file"),
            );
            return;
        };
        if entry.is_dir {
            ui::notify(
                window,
                cx,
                crate::state::ToastKind::Info,
                t!("sftp.download_files_only"),
            );
            return;
        }
        let sftp = sftp.clone();
        let local_path = self.local_dir.join(&entry.name);
        let done = Arc::new(AtomicU64::new(0));
        let ix = self.transfers.len();
        self.transfers.push(Transfer {
            name: entry.name.clone(),
            upload: false,
            total: entry.size,
            done: done.clone(),
            state: TransferState::Running,
        });
        self.start_ticker(cx);
        let remote_path = entry.path.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                let file = tokio::fs::File::create(&local_path)
                    .await
                    .map_err(termoak_ssh::SshError::from)?;
                let d = done.clone();
                let progress = move |n: u64| d.store(n, Ordering::Relaxed);
                sftp.download(&remote_path, file, Some(&progress)).await
            },
            move |this, res, window, cx| {
                if let Some(t) = this.transfers.get_mut(ix) {
                    t.state = match res {
                        Ok(_) => TransferState::Done,
                        Err(e) => TransferState::Failed(e),
                    };
                }
                this.refresh(Side::Local, window, cx);
                cx.notify();
            },
        );
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
        let id_prefix = if side == Side::Local {
            "local"
        } else {
            "remote"
        };

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

        let body: gpui::AnyElement =
            match (&self.conn, side) {
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
                (Conn::Failed(e), Side::Remote) => v_flex()
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
                    .into_any_element(),
                _ => {
                    let rows = entries.iter().take(3000).enumerate().map(|(i, e)| {
                        let active = selected == Some(i);
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

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius_lg)
            .bg(theme.secondary)
            .overflow_hidden()
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

    fn render_transfers(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        v_flex().max_h(px(160.)).gap_1().children(
            self.transfers
                .iter()
                .rev()
                .take(6)
                .enumerate()
                .map(|(i, t)| {
                    let done = t.done.load(Ordering::Relaxed);
                    let pct = if t.total > 0 {
                        (done as f32 / t.total as f32 * 100.).min(100.)
                    } else if matches!(t.state, TransferState::Done) {
                        100.
                    } else {
                        0.
                    };
                    let (status, color) = match &t.state {
                        TransferState::Running => (
                            t!(
                                "sftp.transfer_progress",
                                done = ui::format_bytes(done),
                                total = ui::format_bytes(t.total)
                            ),
                            theme.muted_foreground,
                        ),
                        TransferState::Done => (t!("sftp.transfer_done"), theme.success),
                        TransferState::Failed(e) => (e.clone().into(), theme.danger),
                    };
                    h_flex()
                        .gap_3()
                        .items_center()
                        .text_sm()
                        .child(
                            ui::icon(if t.upload {
                                IconName::Upload
                            } else {
                                IconName::Download
                            })
                            .size(px(14.)),
                        )
                        .child(
                            div()
                                .w(px(220.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(t.name.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Progress::new(("transfer", i)).value(pct)),
                        )
                        .child(
                            div()
                                .w(px(220.))
                                .text_xs()
                                .text_color(color)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(status),
                        )
                }),
        )
    }
}

impl Render for SftpView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let local = self.render_pane(Side::Local, cx);
        let remote = self.render_pane(Side::Remote, cx);
        let transfers = self.render_transfers(cx);
        let has_transfers = !self.transfers.is_empty();
        let theme = cx.theme();
        v_flex()
            .size_full()
            .p_4()
            .gap_3()
            .bg(theme.background)
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(local)
                    .child(remote),
            )
            .when(has_transfers, |this| {
                this.child(
                    ui::card(cx)
                        .p_3()
                        .gap_2()
                        .child(div().text_sm().font_semibold().child(t!("sftp.transfers")))
                        .child(transfers),
                )
            })
    }
}

/// Lists a local folder (folders first).
async fn read_local(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut out = Vec::new();
    while let Some(e) = rd.next_entry().await? {
        let name = e.file_name().to_string_lossy().to_string();
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
