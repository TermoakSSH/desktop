//! Queue of SFTP transfers shared by every window: uploads and downloads
//! of files and folders with progress, speed, cancel, retry and a limit of
//! how many run at once (saved in the local store).
//!
//! The SFTP browser shows it ([`render_panel`]); files dropped on a
//! terminal are queued here too. Downloads are written next to their
//! target as a hidden `.part` file and renamed when complete, so a
//! cancelled or failed download leaves nothing half written.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, EntityId, EventEmitter, Global,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, Task, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::progress::Progress;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_ssh::{FileKind, Sftp};
use tokio_util::sync::CancellationToken;

use crate::runtime;
use crate::ui::{self, IconName};

/// Key of the limit in the local store.
const LIMIT_KEY: &str = "desktop.sftp.max_transfers";
/// Limit until the saved one is read.
pub const DEFAULT_LIMIT: usize = 3;
/// What a cancelled job returns (never shown).
const CANCELLED: &str = "\u{0}cancelled";
/// Finished jobs kept in the list.
const KEEP_FINISHED: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

/// What to transfer.
#[derive(Clone)]
pub struct JobSpec {
    pub direction: Direction,
    /// File or folder on this computer (source of an upload, target of a
    /// download).
    pub local: PathBuf,
    /// Path on the server (target of an upload, source of a download).
    pub remote: String,
    pub is_dir: bool,
    /// Host label (shown in the list).
    pub host: String,
    pub sftp: Arc<Sftp>,
    /// The view that queued it: its jobs are cancelled when it closes.
    pub owner: Option<EntityId>,
}

impl JobSpec {
    pub fn name(&self) -> String {
        match self.direction {
            Direction::Upload => file_name(&self.local),
            Direction::Download => remote_name(&self.remote),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed(String),
    Cancelled,
}

impl JobState {
    pub fn finished(&self) -> bool {
        matches!(
            self,
            JobState::Done | JobState::Failed(_) | JobState::Cancelled
        )
    }
}

pub struct Job {
    pub id: u64,
    pub spec: JobSpec,
    pub state: JobState,
    pub done: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    cancel: CancellationToken,
    started: Option<Instant>,
    finished_in: Option<Duration>,
    /// Last speed sample (when, bytes).
    sample: (Instant, u64),
    /// Bytes per second (smoothed).
    pub speed: f64,
}

/// A job ended.
#[derive(Debug, Clone)]
pub struct Finished {
    pub id: u64,
    pub direction: Direction,
    pub remote: String,
    pub local: PathBuf,
    /// `None`: done; `Some(error)`; cancelled jobs emit nothing.
    pub error: Option<String>,
}

pub struct Queue {
    jobs: VecDeque<Job>,
    next_id: u64,
    limit: usize,
    limit_loaded: bool,
    ticker: Option<Task<()>>,
}

struct QueueHandle(Entity<Queue>);

impl Global for QueueHandle {}

impl EventEmitter<Finished> for Queue {}

/// The queue of the app (created the first time).
pub fn queue(cx: &mut App) -> Entity<Queue> {
    if let Some(h) = cx.try_global::<QueueHandle>() {
        return h.0.clone();
    }
    let q = cx.new(|_| Queue {
        jobs: VecDeque::new(),
        next_id: 1,
        limit: DEFAULT_LIMIT,
        limit_loaded: false,
        ticker: None,
    });
    cx.set_global(QueueHandle(q.clone()));
    q
}

impl Queue {
    pub fn jobs(&self) -> impl DoubleEndedIterator<Item = &Job> {
        self.jobs.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Jobs running or waiting.
    pub fn pending(&self) -> usize {
        self.jobs.iter().filter(|j| !j.state.finished()).count()
    }

    /// Reads the saved limit (once).
    pub fn load_limit(&mut self, store: termoak_core::Store, cx: &mut Context<Self>) {
        if self.limit_loaded {
            return;
        }
        self.limit_loaded = true;
        runtime::run(
            cx,
            async move { store.meta_get(LIMIT_KEY).await },
            |q, res, cx| {
                if let Ok(Some(v)) = res
                    && let Ok(n) = v.trim().parse::<usize>()
                {
                    q.limit = n.clamp(1, 8);
                    q.pump(cx);
                    cx.notify();
                }
            },
        );
    }

    pub fn set_limit(&mut self, n: usize, store: termoak_core::Store, cx: &mut Context<Self>) {
        self.limit = n.clamp(1, 8);
        self.limit_loaded = true;
        let value = self.limit.to_string();
        runtime::handle(cx).spawn(async move {
            let _ = store.meta_set(LIMIT_KEY, &value).await;
        });
        self.pump(cx);
        cx.notify();
    }

    /// Queues a transfer and returns its id.
    pub fn add(&mut self, spec: JobSpec, cx: &mut Context<Self>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.push_back(Job {
            id,
            spec,
            state: JobState::Queued,
            done: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            cancel: CancellationToken::new(),
            started: None,
            finished_in: None,
            sample: (Instant::now(), 0),
            speed: 0.,
        });
        self.trim();
        self.pump(cx);
        cx.notify();
        id
    }

    pub fn cancel(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id) {
            match j.state {
                JobState::Queued => j.state = JobState::Cancelled,
                JobState::Running => j.cancel.cancel(),
                _ => {}
            }
        }
        self.pump(cx);
        cx.notify();
    }

    /// Cancels the jobs of a view that closes.
    pub fn cancel_owner(&mut self, owner: EntityId, cx: &mut Context<Self>) {
        let ids: Vec<u64> = self
            .jobs
            .iter()
            .filter(|j| j.spec.owner == Some(owner) && !j.state.finished())
            .map(|j| j.id)
            .collect();
        for id in ids {
            self.cancel(id, cx);
        }
    }

    /// Queues a failed or cancelled job again.
    pub fn retry(&mut self, id: u64, cx: &mut Context<Self>) {
        if let Some(j) = self.jobs.iter_mut().find(|j| j.id == id)
            && matches!(j.state, JobState::Failed(_) | JobState::Cancelled)
        {
            j.state = JobState::Queued;
            j.done.store(0, Ordering::Relaxed);
            j.total.store(0, Ordering::Relaxed);
            j.cancel = CancellationToken::new();
            j.speed = 0.;
            j.started = None;
            j.finished_in = None;
        }
        self.pump(cx);
        cx.notify();
    }

    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.jobs.retain(|j| !j.state.finished());
        cx.notify();
    }

    /// Drops the oldest finished jobs over the limit of the list.
    fn trim(&mut self) {
        let finished = self.jobs.iter().filter(|j| j.state.finished()).count();
        let mut extra = finished.saturating_sub(KEEP_FINISHED);
        self.jobs.retain(|j| {
            if extra > 0 && j.state.finished() {
                extra -= 1;
                false
            } else {
                true
            }
        });
    }

    /// Starts queued jobs up to the limit.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut running = self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .count();
        let ids: Vec<u64> = self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Queued)
            .map(|j| j.id)
            .collect();
        for id in ids {
            if running >= self.limit {
                break;
            }
            self.start(id, cx);
            running += 1;
        }
        self.start_ticker(cx);
    }

    fn start(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        let now = Instant::now();
        job.state = JobState::Running;
        job.started = Some(now);
        job.sample = (now, 0);
        let spec = job.spec.clone();
        let done = job.done.clone();
        let total = job.total.clone();
        let cancel = job.cancel.clone();
        let fut = runtime::spawn(cx, async move {
            let part = part_path(&spec);
            tokio::select! {
                _ = cancel.cancelled() => {
                    if let Some(p) = part {
                        let _ = tokio::fs::remove_file(p).await;
                    }
                    Err(CANCELLED.to_string())
                }
                r = run(&spec, &done, &total) => r,
            }
        });
        cx.spawn(async move |this, cx| {
            let res = fut.await;
            let _ = this.update(cx, |q, cx| q.finish(id, res, cx));
        })
        .detach();
    }

    fn finish(&mut self, id: u64, res: Result<u64, String>, cx: &mut Context<Self>) {
        let Some(job) = self.jobs.iter_mut().find(|j| j.id == id) else {
            return;
        };
        job.finished_in = job.started.map(|s| s.elapsed());
        let error = match res {
            Ok(_) => {
                job.state = JobState::Done;
                let t = job.total.load(Ordering::Relaxed);
                job.done.store(t, Ordering::Relaxed);
                Some(None)
            }
            Err(e) if e == CANCELLED => {
                job.state = JobState::Cancelled;
                None
            }
            Err(e) => {
                job.state = JobState::Failed(e.clone());
                Some(Some(e))
            }
        };
        if let Some(error) = error {
            let ev = Finished {
                id,
                direction: job.spec.direction,
                remote: job.spec.remote.clone(),
                local: job.spec.local.clone(),
                error,
            };
            cx.emit(ev);
        }
        self.trim();
        self.pump(cx);
        cx.notify();
    }

    /// Refreshes progress and speeds while something runs.
    fn start_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker.is_some() || !self.jobs.iter().any(|j| j.state == JobState::Running) {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let running = this
                    .update(cx, |q, cx| {
                        q.sample_speeds();
                        cx.notify();
                        q.jobs.iter().any(|j| j.state == JobState::Running)
                    })
                    .unwrap_or(false);
                if !running {
                    let _ = this.update(cx, |q, _| q.ticker = None);
                    break;
                }
            }
        }));
    }

    fn sample_speeds(&mut self) {
        let now = Instant::now();
        for j in self
            .jobs
            .iter_mut()
            .filter(|j| j.state == JobState::Running)
        {
            let bytes = j.done.load(Ordering::Relaxed);
            let dt = now.duration_since(j.sample.0).as_secs_f64();
            if dt < 0.2 {
                continue;
            }
            let instant = bytes.saturating_sub(j.sample.1) as f64 / dt;
            j.speed = if j.speed == 0. {
                instant
            } else {
                j.speed * 0.7 + instant * 0.3
            };
            j.sample = (now, bytes);
        }
    }
}

// ---------------------------------------------------------------------------
// Running a job
// ---------------------------------------------------------------------------

/// Last part of a remote path.
pub fn remote_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

/// Hidden file a download is written to before it is complete.
pub fn part_file(target: &Path) -> PathBuf {
    let name = file_name(target);
    target.with_file_name(format!(".{name}.termoak-part"))
}

fn part_path(spec: &JobSpec) -> Option<PathBuf> {
    (spec.direction == Direction::Download && !spec.is_dir).then(|| part_file(&spec.local))
}

/// Files and folders under a local folder: (path relative to it with `/`,
/// is a folder, size). Symbolic links to folders are not followed.
pub fn walk_local(root: &Path) -> std::io::Result<Vec<(String, bool, u64)>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            let rel_path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let ft = entry.file_type()?;
            if ft.is_dir() {
                out.push((rel_path.clone(), true, 0));
                stack.push((entry.path(), rel_path));
            } else if ft.is_file() || ft.is_symlink() {
                match std::fs::metadata(entry.path()) {
                    Ok(m) if m.is_file() => out.push((rel_path, false, m.len())),
                    _ => {}
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

async fn walk_remote(sftp: &Sftp, root: &str) -> Result<Vec<(String, bool, u64)>, String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_string(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        for e in sftp.list(&dir).await.map_err(|e| e.to_string())? {
            let rel_path = if rel.is_empty() {
                e.name.clone()
            } else {
                format!("{rel}/{}", e.name)
            };
            match e.kind {
                FileKind::Dir => {
                    out.push((rel_path.clone(), true, 0));
                    stack.push((e.path, rel_path));
                }
                FileKind::File => out.push((rel_path, false, e.size)),
                // Links and others: copied as files when they can be read.
                _ => out.push((rel_path, false, e.size)),
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn local_join(root: &Path, rel: &str) -> PathBuf {
    rel.split('/')
        .fold(root.to_path_buf(), |p, part| p.join(part))
}

async fn upload_file(
    sftp: &Sftp,
    local: &Path,
    remote: &str,
    base: u64,
    done: &Arc<AtomicU64>,
) -> Result<u64, String> {
    let file = tokio::fs::File::open(local)
        .await
        .map_err(|e| format!("{}: {e}", local.display()))?;
    let d = done.clone();
    let progress = move |n: u64| d.store(base + n, Ordering::Relaxed);
    sftp.upload(file, remote, Some(&progress))
        .await
        .map_err(|e| e.to_string())
}

async fn download_file(
    sftp: &Sftp,
    remote: &str,
    local: &Path,
    base: u64,
    done: &Arc<AtomicU64>,
) -> Result<u64, String> {
    let part = part_file(local);
    let file = tokio::fs::File::create(&part)
        .await
        .map_err(|e| format!("{}: {e}", part.display()))?;
    let d = done.clone();
    let progress = move |n: u64| d.store(base + n, Ordering::Relaxed);
    match sftp.download(remote, file, Some(&progress)).await {
        Ok(n) => {
            tokio::fs::rename(&part, local)
                .await
                .map_err(|e| format!("{}: {e}", local.display()))?;
            Ok(n)
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&part).await;
            Err(e.to_string())
        }
    }
}

async fn run(spec: &JobSpec, done: &Arc<AtomicU64>, total: &Arc<AtomicU64>) -> Result<u64, String> {
    let sftp = &spec.sftp;
    match (spec.direction, spec.is_dir) {
        (Direction::Upload, false) => {
            let len = tokio::fs::metadata(&spec.local)
                .await
                .map_err(|e| e.to_string())?
                .len();
            total.store(len, Ordering::Relaxed);
            upload_file(sftp, &spec.local, &spec.remote, 0, done).await
        }
        (Direction::Upload, true) => {
            let root = spec.local.clone();
            let items = tokio::task::spawn_blocking(move || walk_local(&root))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            total.store(items.iter().map(|i| i.2).sum(), Ordering::Relaxed);
            sftp.mkdir_all(&spec.remote)
                .await
                .map_err(|e| e.to_string())?;
            let mut sent = 0u64;
            for (rel, is_dir, size) in items {
                let remote = termoak_ssh::sftp::join(&spec.remote, &rel);
                if is_dir {
                    sftp.mkdir_all(&remote).await.map_err(|e| e.to_string())?;
                } else {
                    upload_file(sftp, &local_join(&spec.local, &rel), &remote, sent, done).await?;
                    sent += size;
                }
            }
            Ok(sent)
        }
        (Direction::Download, false) => {
            let size = sftp
                .stat(&spec.remote)
                .await
                .map_err(|e| e.to_string())?
                .size;
            total.store(size, Ordering::Relaxed);
            download_file(sftp, &spec.remote, &spec.local, 0, done).await
        }
        (Direction::Download, true) => {
            let items = walk_remote(sftp, &spec.remote).await?;
            total.store(items.iter().map(|i| i.2).sum(), Ordering::Relaxed);
            tokio::fs::create_dir_all(&spec.local)
                .await
                .map_err(|e| e.to_string())?;
            let mut got = 0u64;
            for (rel, is_dir, size) in items {
                let local = local_join(&spec.local, &rel);
                if is_dir {
                    tokio::fs::create_dir_all(&local)
                        .await
                        .map_err(|e| e.to_string())?;
                } else {
                    let remote = termoak_ssh::sftp::join(&spec.remote, &rel);
                    download_file(sftp, &remote, &local, got, done).await?;
                    got += size;
                }
            }
            Ok(got)
        }
    }
}

// ---------------------------------------------------------------------------
// Panel
// ---------------------------------------------------------------------------

/// `1:05` / `12 s`.
fn format_eta(secs: f64) -> String {
    let s = secs.round().max(0.) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else if s >= 60 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{s} s")
    }
}

fn status_text(j: &Job) -> SharedString {
    let done = j.done.load(Ordering::Relaxed);
    let total = j.total.load(Ordering::Relaxed);
    match &j.state {
        JobState::Queued => t!("transfers.queued"),
        JobState::Running if total == 0 => t!("transfers.preparing"),
        JobState::Running => {
            let mut s = t!(
                "transfers.progress",
                done = ui::format_bytes(done),
                total = ui::format_bytes(total)
            )
            .to_string();
            if j.speed > 1. {
                s.push_str(&format!(" · {}/s", ui::format_bytes(j.speed as u64)));
                if total > done {
                    s.push_str(&format!(
                        " · {}",
                        t!(
                            "transfers.left",
                            time = format_eta((total - done) as f64 / j.speed)
                        )
                    ));
                }
            }
            s.into()
        }
        JobState::Done => {
            let secs = j.finished_in.map(|d| d.as_secs_f64()).unwrap_or(0.);
            if secs > 0.5 {
                t!(
                    "transfers.done_in",
                    size = ui::format_bytes(total),
                    time = format_eta(secs)
                )
            } else {
                t!("transfers.done", size = ui::format_bytes(total))
            }
        }
        JobState::Failed(e) => e.clone().into(),
        JobState::Cancelled => t!("transfers.cancelled"),
    }
}

/// The transfers panel (inside a view that observes the queue).
pub fn render_panel<V: 'static>(
    queue: &Entity<Queue>,
    store: termoak_core::Store,
    cx: &mut Context<V>,
) -> Option<gpui::AnyElement> {
    let q = queue.read(cx);
    if q.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let limit = q.limit();
    let pending = q.pending();
    let rows: Vec<gpui::AnyElement> = q
        .jobs()
        .rev()
        .take(30)
        .map(|j| {
            let id = j.id;
            let done = j.done.load(Ordering::Relaxed);
            let total = j.total.load(Ordering::Relaxed);
            let pct = match &j.state {
                JobState::Done => 100.,
                _ if total > 0 => (done as f32 / total as f32 * 100.).min(100.),
                _ => 0.,
            };
            let color = match &j.state {
                JobState::Done => theme.success,
                JobState::Failed(_) => theme.danger,
                _ => theme.muted_foreground,
            };
            let upload = j.spec.direction == Direction::Upload;
            let place = if upload {
                format!("{} · {}", j.spec.host, j.spec.remote)
            } else {
                format!("{} → {}", j.spec.host, j.spec.local.display())
            };
            let can_cancel = !j.state.finished();
            let can_retry = matches!(j.state, JobState::Failed(_) | JobState::Cancelled);
            let can_reveal = j.state == JobState::Done && !upload;
            let local = j.spec.local.clone();
            let qc = queue.clone();
            h_flex()
                .gap_3()
                .items_center()
                .text_sm()
                .child(
                    ui::icon(match (upload, j.spec.is_dir) {
                        (true, false) => IconName::Upload,
                        (false, false) => IconName::Download,
                        (true, true) => IconName::FolderUp,
                        (false, true) => IconName::FolderDown,
                    })
                    .size(px(14.)),
                )
                .child(
                    v_flex()
                        .w(px(240.))
                        .min_w_0()
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(j.spec.name()),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(place),
                        ),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Progress::new(("transfer-job", id as usize)).value(pct)),
                )
                .child(
                    div()
                        .w(px(260.))
                        .text_xs()
                        .text_color(color)
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(status_text(j)),
                )
                .child(
                    h_flex()
                        .w(px(56.))
                        .justify_end()
                        .gap_1()
                        .when(can_cancel, |this| {
                            let qc = qc.clone();
                            this.child(
                                Button::new(("transfer-cancel", id as usize))
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::X))
                                    .tooltip(t!("transfers.cancel"))
                                    .on_click(move |_: &ClickEvent, _, cx| {
                                        qc.update(cx, |q, cx| q.cancel(id, cx))
                                    }),
                            )
                        })
                        .when(can_retry, |this| {
                            let qc = qc.clone();
                            this.child(
                                Button::new(("transfer-retry", id as usize))
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::RotateCw))
                                    .tooltip(t!("common.retry"))
                                    .on_click(move |_: &ClickEvent, _, cx| {
                                        qc.update(cx, |q, cx| q.retry(id, cx))
                                    }),
                            )
                        })
                        .when(can_reveal, |this| {
                            this.child(
                                Button::new(("transfer-reveal", id as usize))
                                    .xsmall()
                                    .ghost()
                                    .icon(ui::icon(IconName::FolderOpen))
                                    .tooltip(t!("transfers.reveal"))
                                    .on_click(move |_: &ClickEvent, _, cx| cx.reveal_path(&local)),
                            )
                        }),
                )
                .into_any_element()
        })
        .collect();
    let limit_buttons = (1..=5usize).map(|n| {
        let qc = queue.clone();
        let store = store.clone();
        Button::new(("transfer-limit", n))
            .xsmall()
            .map(|b| if n == limit { b.primary() } else { b.ghost() })
            .label(n.to_string())
            .on_click(move |_: &ClickEvent, _, cx| {
                let store = store.clone();
                qc.update(cx, |q, cx| q.set_limit(n, store, cx))
            })
    });
    let qc = queue.clone();
    Some(
        ui::card(cx)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().font_semibold().child(t!("transfers.title")))
                    .when(pending > 0, |this| {
                        this.child(ui::pill(tn!("transfers.pending", pending), theme.primary))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("transfers.at_once")),
                    )
                    .child(h_flex().gap_0p5().children(limit_buttons))
                    .child(
                        Button::new("transfers-clear")
                            .xsmall()
                            .ghost()
                            .label(t!("transfers.clear"))
                            .on_click(move |_: &ClickEvent, _, cx| {
                                qc.update(cx, |q, cx| q.clear_finished(cx))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .id("transfer-rows")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .gap_1()
                    .children(rows),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_parts() {
        assert_eq!(remote_name("/home/ana/logs/"), "logs");
        assert_eq!(remote_name("/etc/hosts"), "hosts");
        assert_eq!(
            part_file(Path::new("/tmp/x/report.pdf")),
            PathBuf::from("/tmp/x/.report.pdf.termoak-part")
        );
        assert_eq!(format_eta(5.2), "5 s");
        assert_eq!(format_eta(65.), "1:05");
        assert_eq!(format_eta(3725.), "1:02:05");
    }

    #[test]
    fn walks_local_folders() {
        let dir = std::env::temp_dir().join(format!("termoak-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/c.txt"), b"hello").unwrap();
        std::fs::write(dir.join("top.txt"), b"hi").unwrap();
        let items = walk_local(&dir).unwrap();
        assert_eq!(
            items,
            vec![
                ("a".to_string(), true, 0),
                ("a/b".to_string(), true, 0),
                ("a/b/c.txt".to_string(), false, 5),
                ("top.txt".to_string(), false, 2),
            ]
        );
        assert_eq!(
            local_join(&dir, "a/b/c.txt"),
            dir.join("a").join("b").join("c.txt")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
