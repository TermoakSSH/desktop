//! The terminals open in this app, for the AI that runs on this computer
//! (`list_sessions`, `read_terminal`, `send_to_terminal`).
//!
//! The terminals live on the interface thread; the AI's tools run on tokio.
//! Each request goes to the main window through a channel and comes back with
//! a oneshot. To see what a command prints, the terminal hands out a
//! subscription to its output before typing (so nothing is lost).

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use termoak_ai::{SessionAccess, SessionSummary, TerminalOutput};
use termoak_core::Id;
use tokio::sync::{broadcast, mpsc, oneshot};

/// A request for the main window.
pub enum TermRequest {
    List(oneshot::Sender<Vec<SessionSummary>>),
    Read {
        id: Id,
        max_chars: usize,
        reply: oneshot::Sender<Result<String, String>>,
    },
    /// Types into the terminal and returns a subscription to what it prints
    /// next.
    Type {
        id: Id,
        input: String,
        reply: oneshot::Sender<Result<broadcast::Receiver<Bytes>, String>>,
    },
}

/// Access to this app's terminals (a [`SessionAccess`] for the AI tools).
pub struct LocalTerminals {
    tx: mpsc::UnboundedSender<TermRequest>,
}

impl LocalTerminals {
    /// The access and the requests the main window must answer.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<TermRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }

    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> TermRequest,
    ) -> Result<T, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .map_err(|_| "the app is closing".to_string())?;
        rx.await.map_err(|_| "the terminal is gone".to_string())
    }
}

#[async_trait]
impl SessionAccess for LocalTerminals {
    async fn list(&self, _owner: Id) -> Vec<SessionSummary> {
        self.ask(TermRequest::List).await.unwrap_or_default()
    }

    async fn read(&self, _owner: Id, session: Id, max_chars: usize) -> Result<String, String> {
        self.ask(|reply| TermRequest::Read {
            id: session,
            max_chars,
            reply,
        })
        .await?
    }

    async fn send(&self, _owner: Id, session: Id, input: &str) -> Result<(), String> {
        self.ask(|reply| TermRequest::Type {
            id: session,
            input: input.to_string(),
            reply,
        })
        .await?
        .map(|_| ())
    }

    async fn send_and_collect(
        &self,
        _owner: Id,
        session: Id,
        input: &str,
        quiet: Duration,
        max: Duration,
    ) -> Result<Option<TerminalOutput>, String> {
        let rx = self
            .ask(|reply| TermRequest::Type {
                id: session,
                input: input.to_string(),
                reply,
            })
            .await??;
        Ok(Some(collect_output(rx, quiet, max).await))
    }
}

/// Collects what the terminal prints until it stays quiet for `quiet` or
/// `max` passes (then it is "still running"), as plain text (its end).
pub async fn collect_output(
    mut rx: broadcast::Receiver<Bytes>,
    quiet: Duration,
    max: Duration,
) -> TerminalOutput {
    let deadline = tokio::time::Instant::now() + max;
    let mut out = Vec::new();
    let mut still_running = false;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            still_running = true;
            break;
        }
        match tokio::time::timeout(quiet.min(left), rx.recv()).await {
            Ok(Ok(chunk)) => out.extend_from_slice(&chunk),
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) => break,
            // Quiet for the requested time: it finished (or waits for input).
            Err(_) if left > quiet => break,
            Err(_) => {
                still_running = true;
                break;
            }
        }
    }
    let text = termoak_ssh::ansi::strip(&String::from_utf8_lossy(&out));
    TerminalOutput {
        // Obvious secrets are hidden before the AI gets it.
        text: crate::terminal::redact::redact(termoak_ssh::ansi::tail(&text, 8000)),
        still_running,
    }
}

/// The end of a screen's text, at most `max_chars`, with its obvious
/// secrets hidden (it goes to the AI).
pub fn screen_tail(screen: &str, max_chars: usize) -> String {
    let trimmed = screen.trim_end();
    let n = trimmed.chars().count();
    let tail: String = trimmed.chars().skip(n.saturating_sub(max_chars)).collect();
    crate::terminal::redact::redact(&tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_is_collected_until_quiet() {
        let (tx, rx) = broadcast::channel(16);
        let feed = tokio::spawn(async move {
            tx.send(Bytes::from_static(b"\x1b[32mtotal 4\x1b[0m\r\n"))
                .unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send(Bytes::from_static(b"file.txt\r\n$ ")).unwrap();
            // Then silence (the sender stays alive).
            tokio::time::sleep(Duration::from_millis(500)).await;
            drop(tx);
        });
        let out = collect_output(rx, Duration::from_millis(150), Duration::from_secs(5)).await;
        assert_eq!(out.text, "total 4\nfile.txt\n$ ");
        assert!(!out.still_running);
        feed.abort();
    }

    #[tokio::test]
    async fn endless_output_is_still_running() {
        let (tx, rx) = broadcast::channel(1024);
        let feed = tokio::spawn(async move {
            loop {
                if tx.send(Bytes::from_static(b"y\n")).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let out = collect_output(rx, Duration::from_millis(200), Duration::from_millis(300)).await;
        assert!(out.still_running);
        assert!(out.text.starts_with("y\n"));
        feed.abort();
    }

    #[tokio::test]
    async fn requests_reach_the_window() {
        let (access, mut rx) = LocalTerminals::new();
        let id = termoak_core::new_id();
        let window = tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                match req {
                    TermRequest::List(reply) => {
                        let _ = reply.send(vec![SessionSummary {
                            id,
                            title: "web1".into(),
                            host_id: None,
                            status: "running".into(),
                            viewers: 1,
                        }]);
                    }
                    TermRequest::Read { reply, .. } => {
                        let _ = reply.send(Ok("$ ".into()));
                    }
                    TermRequest::Type { reply, .. } => {
                        let (tx, rx) = broadcast::channel(4);
                        let _ = reply.send(Ok(rx));
                        tx.send(Bytes::from_static(b"ok\n")).unwrap();
                    }
                }
            }
        });
        let owner = termoak_core::new_id();
        assert_eq!(access.list(owner).await[0].title, "web1");
        assert_eq!(access.read(owner, id, 100).await.unwrap(), "$ ");
        let out = access
            .send_and_collect(
                owner,
                id,
                "ls\r",
                Duration::from_millis(50),
                Duration::from_secs(1),
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out.text, "ok\n");
        window.abort();
    }

    #[test]
    fn tails() {
        assert_eq!(screen_tail("abc\ndef\n\n", 3), "def");
        assert_eq!(screen_tail("ab", 10), "ab");
        assert_eq!(
            screen_tail("$ x\npassword=hunter2\n", 100),
            "$ x\npassword=[redacted]"
        );
    }
}
