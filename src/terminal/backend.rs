//! Connection of a terminal with its source, in tokio:
//! - **Local**: SSH from this computer (`Workspace::connect` + `open_terminal`).
//! - **Server**: a session that lives on the Termoak server and survives the
//!   app being closed (`POST /api/v1/sessions` + WebSocket).
//! - **Shell**: a shell of this computer in a PTY (see [`super::shell`]).
//!
//! The view sends commands ([`Cmd`]) and receives output and state changes ([`Out`]).

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use termoak_client::remote::{RemoteEvent, RemoteTerminal};
use termoak_client::{ApiClient, LOCAL_OWNER, Workspace};
use termoak_core::Id;
use termoak_core::model::{Host, SecretUpdate};
use termoak_ssh::prompt::Prompt;
use termoak_ssh::{Connection, TerminalSession};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};

use super::shell::PtyLink;
use crate::prompts::{DesktopPrompter, PromptRequest};
use crate::state::api_error;

/// Commands from the view.
#[derive(Debug)]
pub enum Cmd {
    Input(Bytes),
    Resize(u16, u16),
    /// Closes the local terminal or detaches from the server session.
    Close,
    /// Ends the server session (owner only).
    CloseSession,
}

/// Events for the view.
pub enum Out {
    /// Status text while connecting.
    Status(String),
    /// Local terminal ready.
    Local(Arc<TerminalSession>),
    /// Local shell running.
    Shell,
    /// Attached to a server session.
    Remote {
        session_id: Id,
        can_write: bool,
    },
    Data(Bytes),
    /// The screen must be cleared: the full history is coming.
    Reset,
    /// Non-fatal notice from the server.
    Notice(String),
    /// Who is watching (server sessions).
    Presence(usize),
    /// Ended, with the reason (if any).
    Closed(Option<String>),
    /// Could not connect.
    Failed(String),
}

/// The view's end.
#[derive(Clone)]
pub struct Backend {
    tx: mpsc::UnboundedSender<Cmd>,
    /// Local shell only: wakes the PTY thread after every command and does
    /// the flow control of the output.
    pty: Option<Arc<PtyLink>>,
}

impl Backend {
    pub(super) fn new(tx: mpsc::UnboundedSender<Cmd>) -> Self {
        Self { tx, pty: None }
    }

    pub(super) fn with_pty(tx: mpsc::UnboundedSender<Cmd>, link: Arc<PtyLink>) -> Self {
        Self {
            tx,
            pty: Some(link),
        }
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
        if let Some(link) = &self.pty {
            link.wake();
        }
    }

    pub fn input(&self, data: impl Into<Bytes>) {
        self.send(Cmd::Input(data.into()));
    }

    /// The view has processed `n` bytes of output (flow control).
    pub fn consumed(&self, n: usize) {
        if let Some(link) = &self.pty {
            link.consumed(n);
        }
    }
}

/// Parameters of a local terminal.
pub struct LocalParams {
    pub ws: Workspace,
    pub host_id: Id,
    pub prompter: Arc<DesktopPrompter>,
    pub use_agent: bool,
    pub cols: u16,
    pub rows: u16,
    /// Already open connection to reuse (not used when reconnecting, for example).
    pub conn: Option<Arc<Connection>>,
}

/// Starts a local terminal.
pub fn start_local(
    rt: &tokio::runtime::Handle,
    p: LocalParams,
) -> (Backend, mpsc::UnboundedReceiver<Out>) {
    let (tx, cmd_rx) = mpsc::unbounded_channel();
    let (out, out_rx) = mpsc::unbounded_channel();
    rt.spawn(run_local(p, cmd_rx, out));
    (Backend::new(tx), out_rx)
}

async fn run_local(
    p: LocalParams,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<Out>,
) {
    let label =
        p.ws.store
            .get::<Host>(LOCAL_OWNER, p.host_id)
            .await
            .map(|h| h.data.label)
            .unwrap_or_else(|_| "host".into());
    // Latest size (the window adjusts while connecting).
    let mut size = (p.cols, p.rows);
    let conn = match p.conn {
        Some(c) if !c.is_closed() => c,
        _ => {
            let _ = out.send(Out::Status(
                t!("terminal.status.connecting_to", host = label).to_string(),
            ));
            // While connecting, close requests are handled (e.g. the tab is closed).
            let connect = p.ws.connect(p.host_id, p.prompter.clone(), p.use_agent);
            tokio::pin!(connect);
            loop {
                tokio::select! {
                    res = &mut connect => match res {
                        Ok(c) => break c,
                        Err(e) => {
                            let _ = out.send(Out::Failed(e.to_string()));
                            return;
                        }
                    },
                    cmd = cmd_rx.recv() => match cmd {
                        Some(Cmd::Close) | Some(Cmd::CloseSession) | None => return,
                        Some(Cmd::Resize(c, r)) => size = (c, r),
                        Some(Cmd::Input(_)) => {}
                    }
                }
            }
        }
    };
    let _ = out.send(Out::Status(t!("terminal.status.opening").to_string()));
    let term = match p
        .ws
        .open_terminal(p.host_id, conn.clone(), size.0, size.1, false)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            let _ = out.send(Out::Failed(e.to_string()));
            return;
        }
    };
    let _ = out.send(Out::Local(term.clone()));

    // Detects the operating system to show it on the host card.
    {
        let ws = p.ws.clone();
        let host_id = p.host_id;
        let conn = conn.clone();
        tokio::spawn(async move {
            if let Some(info) = termoak_ssh::detect::detect_os_info(&conn).await
                && let Ok(rec) = ws.store.get::<Host>(LOCAL_OWNER, host_id).await
                && (rec.data.os.as_deref() != Some(info.id.as_str())
                    || rec.data.os_version.as_deref() != Some(info.display().as_str()))
            {
                let mut host = rec.data;
                host.os_version = Some(info.display());
                host.os = Some(info.id);
                let _ = ws
                    .store
                    .save(LOCAL_OWNER, host, SecretUpdate::Keep, None)
                    .await;
            }
        });
    }

    let (snapshot, mut rx) = term.attach();
    if !snapshot.is_empty() {
        let _ = out.send(Out::Data(snapshot));
    }
    let mut status = term.watch_status();
    loop {
        tokio::select! {
            data = rx.recv() => match data {
                Ok(bytes) => { let _ = out.send(Out::Data(bytes)); }
                Err(RecvError::Lagged(_)) => {
                    let (snapshot, fresh) = term.attach();
                    rx = fresh;
                    let _ = out.send(Out::Reset);
                    let _ = out.send(Out::Data(snapshot));
                }
                Err(RecvError::Closed) => break,
            },
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Input(b)) => { let _ = term.write(b).await; }
                Some(Cmd::Resize(c, r)) => { let _ = term.resize(c, r).await; }
                Some(Cmd::Close) | Some(Cmd::CloseSession) | None => {
                    term.close().await;
                    return;
                }
            },
            changed = status.changed() => {
                if changed.is_err() || status.borrow().is_closed() {
                    // Last pending output.
                    while let Ok(bytes) = rx.try_recv() {
                        let _ = out.send(Out::Data(bytes));
                    }
                    let reason = match status.borrow().clone() {
                        termoak_ssh::TermStatus::Closed { exit_code, reason } => reason.or_else(|| exit_code.map(|c| t!("terminal.status.exited", code = c).to_string())),
                        _ => None,
                    };
                    let _ = out.send(Out::Closed(reason));
                    break;
                }
            }
        }
    }
    // Waits for the view to close before releasing the session.
    while let Some(cmd) = cmd_rx.recv().await {
        if matches!(cmd, Cmd::Close | Cmd::CloseSession) {
            break;
        }
    }
}

/// Which server session to open.
pub enum ServerTarget {
    /// New session on the server for this host.
    New { host_id: Id },
    /// Attach to an existing session (own or shared).
    Attach { session_id: Id },
}

/// Starts a terminal that lives on the server.
pub fn start_server(
    rt: &tokio::runtime::Handle,
    api: ApiClient,
    target: ServerTarget,
    prompter: Arc<DesktopPrompter>,
    cols: u16,
    rows: u16,
) -> (Backend, mpsc::UnboundedReceiver<Out>) {
    let (tx, cmd_rx) = mpsc::unbounded_channel();
    let (out, out_rx) = mpsc::unbounded_channel();
    rt.spawn(run_server(api, target, prompter, cols, rows, cmd_rx, out));
    (Backend::new(tx), out_rx)
}

async fn run_server(
    api: ApiClient,
    target: ServerTarget,
    prompter: Arc<DesktopPrompter>,
    cols: u16,
    rows: u16,
    mut cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    out: mpsc::UnboundedSender<Out>,
) {
    let session_id = match target {
        ServerTarget::Attach { session_id } => session_id,
        ServerTarget::New { host_id } => {
            let _ = out.send(Out::Status(
                t!("terminal.status.opening_server_session").to_string(),
            ));
            let created: Result<Value, _> = api
                .post(
                    "/api/v1/sessions",
                    &json!({"host_id": host_id, "cols": cols, "rows": rows}),
                )
                .await;
            match created {
                Ok(v) => match v["id"].as_str().and_then(|s| s.parse::<Id>().ok()) {
                    Some(id) => id,
                    None => {
                        let _ =
                            out.send(Out::Failed(t!("terminal.status.no_session_id").to_string()));
                        return;
                    }
                },
                Err(e) => {
                    let _ = out.send(Out::Failed(api_error(e)));
                    return;
                }
            }
        }
    };
    let _ = out.send(Out::Status(
        t!("terminal.status.connecting_server_session").to_string(),
    ));
    let (remote, mut events) = match RemoteTerminal::attach(&api, session_id).await {
        Ok(r) => r,
        Err(e) => {
            let _ = out.send(Out::Failed(api_error(e)));
            return;
        }
    };
    remote.resize(cols, rows).await;
    let mut announced = false;
    loop {
        tokio::select! {
            ev = events.recv() => {
                let Some(ev) = ev else {
                    let _ = out.send(Out::Closed(Some(t!("terminal.status.server_connection_closed").to_string())));
                    break;
                };
                match ev {
                    RemoteEvent::Hello(v) => {
                        let access = v["you"]["access"].as_str().unwrap_or("view");
                        let can_write = access == "owner" || access == "control";
                        let _ = out.send(Out::Remote { session_id, can_write });
                        announced = true;
                        if let Some(n) = v["session"]["viewers"].as_array().map(|a| a.len()) {
                            let _ = out.send(Out::Presence(n));
                        }
                        if let Some(msg) = state_message(&v["session"]["state"]) {
                            let _ = out.send(Out::Status(msg));
                        }
                    }
                    RemoteEvent::Output(b) => {
                        if !announced {
                            let _ = out.send(Out::Remote { session_id, can_write: true });
                            announced = true;
                        }
                        let _ = out.send(Out::Data(b));
                    }
                    RemoteEvent::Resync => { let _ = out.send(Out::Reset); }
                    RemoteEvent::Status(v) => {
                        if v["state"].as_str() == Some("closed") {
                            let reason = v["reason"].as_str().map(str::to_string).or_else(|| {
                                v["exit_code"].as_u64().map(|c| t!("terminal.status.exited", code = c).to_string())
                            });
                            let _ = out.send(Out::Closed(reason));
                        } else if let Some(msg) = state_message(&v) {
                            let _ = out.send(Out::Status(msg));
                        }
                    }
                    RemoteEvent::Presence(v) => {
                        let _ = out.send(Out::Presence(v.as_array().map(|a| a.len()).unwrap_or(0)));
                    }
                    RemoteEvent::Prompt(v) => forward_prompt(&prompter, &remote, v),
                    RemoteEvent::Error(msg) => { let _ = out.send(Out::Notice(msg)); }
                    RemoteEvent::Reconnecting => {
                        let _ = out.send(Out::Status(t!("terminal.status.reconnecting").to_string()));
                    }
                    RemoteEvent::Closed => {
                        let _ = out.send(Out::Closed(None));
                        break;
                    }
                    RemoteEvent::Resize { .. } | RemoteEvent::Other(_) => {}
                }
            }
            cmd = cmd_rx.recv() => match cmd {
                Some(Cmd::Input(b)) => remote.input(b).await,
                Some(Cmd::Resize(c, r)) => remote.resize(c, r).await,
                Some(Cmd::CloseSession) => remote.close_session().await,
                Some(Cmd::Close) | None => {
                    remote.detach().await;
                    return;
                }
            }
        }
    }
    while let Some(cmd) = cmd_rx.recv().await {
        if matches!(cmd, Cmd::Close | Cmd::CloseSession) {
            break;
        }
    }
    remote.detach().await;
}

/// Text for a server session state (`{"state": ...}`).
fn state_message(v: &Value) -> Option<String> {
    match v["state"].as_str()? {
        "connecting" => Some(
            v["message"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| t!("terminal.status.server_connecting").to_string()),
        ),
        "host_offline" => Some(t!("terminal.status.host_offline").to_string()),
        "running" => Some(String::new()),
        _ => None,
    }
}

/// Shows an authentication question of a server session and sends the answer.
fn forward_prompt(prompter: &DesktopPrompter, remote: &RemoteTerminal, v: Value) {
    let Some(prompt_id) = v["prompt_id"].as_str().and_then(|s| s.parse::<Id>().ok()) else {
        return;
    };
    let kind = v["kind"].as_str().unwrap_or("").to_string();
    let host = v["host"].as_str().unwrap_or("").to_string();
    let message = v["message"].as_str().unwrap_or("").to_string();
    let remote = remote.clone();
    if kind == "hostkey" {
        let (h, port) = match host.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
            None => (host.clone(), 22),
        };
        let (reply, rx) = oneshot::channel();
        prompter.ask(PromptRequest::HostKey {
            host: h,
            port,
            key_type: v["key_type"].as_str().unwrap_or("").to_string(),
            fingerprint: v["fingerprint"].as_str().unwrap_or("").to_string(),
            reply,
        });
        tokio::spawn(async move {
            let accept = rx.await.unwrap_or(false);
            remote.answer_prompt(prompt_id, Some(accept), None).await;
        });
        return;
    }
    let prompts: Vec<Prompt> = serde_json::from_value(v["prompts"].clone()).unwrap_or_default();
    let prompts = if prompts.is_empty() {
        vec![Prompt {
            text: t!("terminal.prompt.answer").to_string(),
            echo: false,
        }]
    } else {
        prompts
    };
    let name = match kind.as_str() {
        "password" => t!("terminal.prompt.password"),
        "passphrase" => t!("terminal.prompt.passphrase"),
        _ => t!("terminal.prompt.verification"),
    }
    .to_string();
    let (reply, rx) = oneshot::channel();
    prompter.ask(PromptRequest::Keyboard {
        host: host.clone(),
        name,
        instructions: if message.is_empty() {
            t!("terminal.prompt.server_asks", host = host).to_string()
        } else {
            message
        },
        prompts,
        reply,
    });
    tokio::spawn(async move {
        match rx.await.ok().flatten() {
            Some(answers) => remote.answer_prompt(prompt_id, None, Some(answers)).await,
            None => remote.answer_prompt(prompt_id, Some(false), None).await,
        }
    });
}
