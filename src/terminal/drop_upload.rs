//! Files dropped on a terminal (or on its tab).
//!
//! - SSH terminals (from this computer or server sessions of a host):
//!   after a confirmation, the files and folders are uploaded through the
//!   transfers queue to the shell's working directory when the shell
//!   reports it (OSC 7), otherwise to the home folder.
//! - The shell of this computer gets their paths typed, quoted.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Context, Subscription, Window};
use termoak_core::Id;

use super::{TermKind, TerminalView};
use crate::runtime;
use crate::state::ToastKind;
use crate::transfers::{self, Direction, Finished, JobSpec};
use crate::ui;

/// Uploads started from a terminal, to tell when they end.
#[derive(Default)]
pub struct Uploads {
    batches: Vec<Batch>,
    sub: Option<Subscription>,
}

struct Batch {
    jobs: Vec<u64>,
    dir: String,
    ok: usize,
    failed: Vec<String>,
}

/// A path as typed in a shell: POSIX single quotes, or double quotes for
/// the Windows shells.
pub fn quote_path(path: &str, windows: bool) -> String {
    if windows {
        if path.contains([' ', '&', '(', ')', '\'', ';', ',', '%', '^', '$', '`']) {
            format!("\"{path}\"")
        } else {
            path.to_string()
        }
    } else if !path.is_empty()
        && path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%=".contains(c))
    {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

impl TerminalView {
    /// Files and folders dropped from the system file manager.
    pub fn drop_paths(&mut self, paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            return;
        }
        match self.kind.clone() {
            TermKind::Shell => {
                let text = paths
                    .iter()
                    .map(|p| quote_path(&p.display().to_string(), cfg!(windows)))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.insert_text(&format!("{text} "), cx);
            }
            TermKind::Serial { .. } => {}
            TermKind::Local { host_id } => self.confirm_upload(host_id, paths, window, cx),
            TermKind::Server {
                host_id: Some(host_id),
                link: None,
                ..
            } => self.confirm_upload(host_id, paths, window, cx),
            TermKind::Server { .. } => ui::notify(
                window,
                cx,
                ToastKind::Info,
                t!("terminal.drop.not_available"),
            ),
        }
    }

    fn confirm_upload(
        &mut self,
        host_id: Id,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.model.cwd().map(str::to_string);
        let place = match &cwd {
            Some(dir) => dir.clone(),
            None => t!("terminal.drop.home").to_string(),
        };
        let names = paths
            .iter()
            .take(3)
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect::<Vec<_>>()
            .join(", ");
        let more = paths.len().saturating_sub(3);
        let names = if more > 0 {
            t!("terminal.drop.names_more", names = names, more = more).to_string()
        } else {
            names
        };
        let message = tn!(
            "terminal.drop.confirm",
            paths.len(),
            names = names,
            host = self.host_label.clone(),
            dir = place
        );
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("terminal.drop.title"),
            message,
            t!("terminal.drop.upload"),
            false,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    let paths = paths.clone();
                    let cwd = cwd.clone();
                    v.update(cx, |v, cx| v.start_upload(host_id, paths, cwd, window, cx));
                }
            },
        );
    }

    fn start_upload(
        &mut self,
        host_id: Id,
        paths: Vec<PathBuf>,
        cwd: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (ws, prompter, use_agent) = {
            let m = self.app.read(cx);
            (m.ws.clone(), m.prompter.clone(), m.settings.use_agent)
        };
        let existing = self.ssh_connection().filter(|c| !c.is_closed());
        let host = self.host_label.clone();
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
                let dir = match cwd {
                    Some(d) => d,
                    None => sftp.home().await.map_err(|e| e.to_string())?,
                };
                Ok::<_, String>((Arc::new(sftp), dir))
            },
            move |this, res, window, cx| match res {
                Ok((sftp, dir)) => {
                    let queue = transfers::queue(cx);
                    if this.uploads.sub.is_none() {
                        this.uploads.sub = Some(cx.subscribe_in(
                            &queue,
                            window,
                            |this, _, ev: &Finished, window, cx| {
                                this.upload_finished(ev, window, cx)
                            },
                        ));
                    }
                    let mut jobs = Vec::new();
                    for p in &paths {
                        let Some(name) = p.file_name().map(|n| n.to_string_lossy().to_string())
                        else {
                            continue;
                        };
                        let spec = JobSpec {
                            direction: Direction::Upload,
                            local: p.clone(),
                            remote: termoak_ssh::sftp::join(&dir, &name),
                            is_dir: p.is_dir(),
                            host: host.clone(),
                            sftp: sftp.clone(),
                            owner: None,
                        };
                        jobs.push(queue.update(cx, |q, cx| q.add(spec, cx)));
                    }
                    ui::notify(
                        window,
                        cx,
                        ToastKind::Info,
                        tn!("terminal.drop.started", jobs.len(), dir = dir.clone()),
                    );
                    this.uploads.batches.push(Batch {
                        jobs,
                        dir,
                        ok: 0,
                        failed: Vec::new(),
                    });
                }
                Err(e) => ui::error(window, cx, t!("terminal.drop.failed", error = e)),
            },
        );
    }

    fn upload_finished(&mut self, ev: &Finished, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self
            .uploads
            .batches
            .iter()
            .position(|b| b.jobs.contains(&ev.id))
        else {
            return;
        };
        let b = &mut self.uploads.batches[ix];
        b.jobs.retain(|j| *j != ev.id);
        match &ev.error {
            None => b.ok += 1,
            Some(e) => b
                .failed
                .push(format!("{}: {e}", transfers::remote_name(&ev.remote))),
        }
        if !b.jobs.is_empty() {
            return;
        }
        let b = self.uploads.batches.remove(ix);
        if b.failed.is_empty() {
            ui::success(window, cx, tn!("terminal.drop.done", b.ok, dir = b.dir));
        } else {
            ui::error(
                window,
                cx,
                t!("terminal.drop.failed", error = b.failed.join("; ")),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::quote_path;

    #[test]
    fn quotes_paths_for_shells() {
        assert_eq!(
            quote_path("/home/ana/file.txt", false),
            "/home/ana/file.txt"
        );
        assert_eq!(quote_path("/tmp/My docs", false), "'/tmp/My docs'");
        assert_eq!(quote_path("/tmp/it's", false), "'/tmp/it'\\''s'");
        assert_eq!(
            quote_path(r"C:\Users\Ana\a b.txt", true),
            r#""C:\Users\Ana\a b.txt""#
        );
        assert_eq!(quote_path(r"C:\x.txt", true), r"C:\x.txt");
    }
}
