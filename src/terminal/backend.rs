//! Connection of a terminal with its source, in tokio:
//! - **Local**: SSH from this computer (`Workspace::connect` + `open_terminal`),
//!   or Telnet for the hosts whose protocol is Telnet (`open_telnet`).
//! - **Server**: a session that lives on the Termoak server and survives the
//!   app being closed (`POST /api/v1/sessions` + WebSocket).
//! - **Shell**: a shell of this computer in a PTY (see [`super::shell`]).
//!
//! The view sends commands ([`Cmd`]) and receives output and state changes ([`Out`]).

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use termoak_client::remote::{Participant, RemoteEvent, RemoteTerminal};
use termoak_client::{ApiClient, Workspace};
use termoak_core::Id;
use termoak_core::model::Host;
use termoak_ssh::prompt::Prompt;
use termoak_ssh::{Connection, Terminal};
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
    /// Something about the keyboard or the people in a shared session.
    Share(ShareAction),
    /// Measures the latency (see [`super::latency`]); `None` if there is no
    /// answer or this kind of terminal has none.
    Latency(oneshot::Sender<Option<std::time::Duration>>),
}

/// Actions in a shared session: asking for the keyboard (participants) and
/// deciding (the owner). See the server's `WEBSOCKET-PROTOCOL.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareAction {
    RequestControl,
    /// Gives the keyboard back (or withdraws the request).
    ReleaseControl,
    /// Hands the keyboard over for these minutes (`None`: until the owner
    /// takes it back).
    GrantControl(Id, Option<u32>),
    DenyControl(Id),
    TakeControl,
    AllowJoin(Id),
    DenyJoin(Id),
    /// Sends a participant away (`true`: and revokes the share they used).
    Kick(Id, bool),
    StopSharing,
}

/// A link to join a session shared with a link: the server (signed in to
/// it or not) and the path of its WebSocket.
#[derive(Clone)]
pub struct LinkJoin {
    pub api: ApiClient,
    /// `/api/v1/sessions/{id}/ws?share_token=…`.
    pub ws_path: String,
    /// Display name of a guest without an account on that server.
    pub guest_name: Option<String>,
}

impl std::fmt::Debug for LinkJoin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The path carries the token: not printed.
        f.debug_struct("LinkJoin")
            .field("server", &self.api.base_url())
            .field("guest_name", &self.guest_name)
            .finish()
    }
}

/// What this side learns when it gets into a server session (`hello`).
#[derive(Debug, Clone)]
pub struct RemoteSeat {
    pub session_id: Id,
    /// Input and resizes reach the terminal now.
    pub can_write: bool,
    pub owner: bool,
    /// Your participant id.
    pub participant: Option<Id>,
    /// `owner`, `control` (can ask for the keyboard) or `view`.
    pub access: String,
    /// Name of the session's owner (sessions shared with you).
    pub owner_name: Option<String>,
    pub participants: Vec<Participant>,
    pub driver: Option<Id>,
    /// End of the driver's timed grant (ms).
    pub until: Option<i64>,
}

/// Events for the view.
pub enum Out {
    /// Status text while connecting.
    Status(String),
    /// Local terminal ready (SSH or Telnet).
    Local(Terminal),
    /// Local shell running.
    Shell,
    /// Attached to a server session.
    Remote(RemoteSeat),
    /// The keyboard changed hands (`driver`: `None` = the owner; `until`:
    /// end of a timed grant, ms).
    Control {
        driver: Option<Id>,
        driver_name: Option<String>,
        can_write: bool,
        until: Option<i64>,
    },
    /// A timed grant ended (`participant`: who had the keyboard).
    ControlExpired(Option<Id>),
    /// Who is in the session (and, for the owner, who waits).
    Participants {
        list: Vec<Participant>,
        driver: Option<Id>,
    },
    /// In the waiting room until the owner lets you in.
    Waiting {
        owner: String,
        title: String,
    },
    /// Owner: someone waits to be let in.
    JoinRequest(Participant),
    /// Owner: someone asks for the keyboard.
    ControlRequest(Participant),
    /// The owner said no to your request for the keyboard.
    ControlDenied,
    /// The server sent you away for good (`revoked`, `kicked`...).
    Ended(String),
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
    /// Telnet hosts: answer their first login prompts with the host's
    /// username and password.
    pub telnet_auto_login: bool,
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
    let host = p.ws.find_item::<Host>(p.host_id).await.ok();
    let label = host
        .as_ref()
        .map(|h| h.record.data.label.clone())
        .unwrap_or_else(|| "host".into());
    let telnet = host
        .as_ref()
        .is_some_and(|h| h.record.data.protocol.is_telnet());
    // Latest size (the window adjusts while connecting).
    let mut size = (p.cols, p.rows);
    let term: Terminal = if telnet {
        let _ = out.send(Out::Status(
            t!("terminal.status.connecting_to", host = label).to_string(),
        ));
        let open =
            p.ws.open_telnet(p.host_id, size.0, size.1, false, p.telnet_auto_login);
        tokio::pin!(open);
        let term = loop {
            tokio::select! {
                res = &mut open => match res {
                    Ok(t) => break t,
                    Err(e) => {
                        let _ = out.send(Out::Failed(crate::state::api_error(e)));
                        return;
                    }
                },
                cmd = cmd_rx.recv() => match cmd {
                    Some(Cmd::Close) | Some(Cmd::CloseSession) | None => return,
                    Some(Cmd::Resize(c, r)) => size = (c, r),
                    Some(Cmd::Input(_)) | Some(Cmd::Share(_)) | Some(Cmd::Latency(_)) => {}
                }
            }
        };
        // The window may have changed while connecting.
        let _ = term.resize(size.0, size.1).await;
        Terminal::Telnet(term)
    } else {
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
                                // Vault rules (Use only, Strict...) are translated.
                                let _ = out.send(Out::Failed(crate::state::api_error(e)));
                                return;
                            }
                        },
                        cmd = cmd_rx.recv() => match cmd {
                            Some(Cmd::Close) | Some(Cmd::CloseSession) | None => return,
                            Some(Cmd::Resize(c, r)) => size = (c, r),
                            Some(Cmd::Input(_)) | Some(Cmd::Share(_)) | Some(Cmd::Latency(_)) => {}
                        }
                    }
                }
            }
        };
        let _ = out.send(Out::Status(t!("terminal.status.opening").to_string()));
        match p
            .ws
            .open_terminal(p.host_id, conn, size.0, size.1, false)
            .await
        {
            Ok(t) => Terminal::Ssh(t),
            Err(e) => {
                let _ = out.send(Out::Failed(e.to_string()));
                return;
            }
        }
    };
    let _ = out.send(Out::Local(term.clone()));

    // Detects the operating system to show it on the host card (SSH only:
    // it runs a command on its own channel).
    if let Some(conn) = term.connection().cloned() {
        let ws = p.ws.clone();
        let host_id = p.host_id;
        tokio::spawn(async move {
            // Saved where the host is, if the user can change it (Use-only
            // members skip it: it is only metadata).
            if let Some(info) = termoak_ssh::detect::detect_os_info(&conn).await
                && let Ok(item) = ws.find_item::<Host>(host_id).await
                && item.access.can_write()
                && (item.record.data.os.as_deref() != Some(info.id.as_str())
                    || item.record.data.os_version.as_deref() != Some(info.display().as_str()))
            {
                let _ = crate::state::update_stored::<Host>(&ws, item.item(), |host| {
                    host.os_version = Some(info.display());
                    host.os = Some(info.id);
                })
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
                // Sharing a local terminal goes through its relay.
                Some(Cmd::Share(_)) => {}
                // Measured apart: the output keeps flowing meanwhile.
                Some(Cmd::Latency(reply)) => {
                    let term = term.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(term.latency(super::latency::TIMEOUT).await.ok());
                    });
                }
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
    /// Join with a link (maybe on another server, maybe as a guest).
    Link { session_id: Id, link: LinkJoin },
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
    let (session_id, link) = match target {
        ServerTarget::Attach { session_id } => (session_id, None),
        ServerTarget::Link { session_id, link } => (session_id, Some(link)),
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
                    Some(id) => (id, None),
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
    let attached = match &link {
        Some(l) => RemoteTerminal::attach_as(&l.api, &l.ws_path, l.guest_name.as_deref()).await,
        None => RemoteTerminal::attach(&api, session_id).await,
    };
    let (remote, mut events) = match attached {
        Ok(r) => r,
        Err(e) => {
            let _ = out.send(Out::Failed(api_error(e)));
            return;
        }
    };
    // Remembered by the client and sent only while this side can write
    // (the owner or whoever has the keyboard decides the size).
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
                        let _ = out.send(Out::Remote(seat_from_hello(session_id, &v, &remote)));
                        announced = true;
                        if let Some(msg) = state_message(&v["session"]["state"]) {
                            let _ = out.send(Out::Status(msg));
                        }
                    }
                    RemoteEvent::Output(b) => {
                        if !announced {
                            // Servers that do not say hello: the owner.
                            let _ = out.send(Out::Remote(RemoteSeat {
                                session_id,
                                can_write: true,
                                owner: true,
                                participant: None,
                                access: "owner".into(),
                                owner_name: None,
                                participants: Vec::new(),
                                driver: None,
                                until: None,
                            }));
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
                    RemoteEvent::Participants { participants, driver } => {
                        let _ = out.send(Out::Participants { list: participants, driver });
                    }
                    RemoteEvent::Control { driver, driver_name, can_write, until } => {
                        let _ = out.send(Out::Control { driver, driver_name, can_write, until });
                    }
                    RemoteEvent::ControlExpired { participant } => {
                        let _ = out.send(Out::ControlExpired(participant));
                    }
                    RemoteEvent::Waiting(v) => {
                        let _ = out.send(Out::Waiting {
                            owner: v["session"]["owner"].as_str().unwrap_or("").to_string(),
                            title: v["session"]["title"].as_str().unwrap_or("").to_string(),
                        });
                    }
                    RemoteEvent::JoinRequest(p) => { let _ = out.send(Out::JoinRequest(p)); }
                    RemoteEvent::ControlRequest(p) => { let _ = out.send(Out::ControlRequest(p)); }
                    RemoteEvent::ControlDenied => { let _ = out.send(Out::ControlDenied); }
                    RemoteEvent::Prompt(v) => forward_prompt(&prompter, &remote, v),
                    RemoteEvent::Error(msg) => { let _ = out.send(Out::Notice(msg)); }
                    RemoteEvent::Ended { code, .. } => { let _ = out.send(Out::Ended(code)); }
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
                // Read-only: nothing is sent (the client drops it too).
                Some(Cmd::Input(b)) => if remote.can_write() { remote.input(b).await },
                Some(Cmd::Resize(c, r)) => remote.resize(c, r).await,
                Some(Cmd::CloseSession) => remote.close_session().await,
                Some(Cmd::Share(action)) => share_action(&remote, action).await,
                Some(Cmd::Latency(reply)) => {
                    let remote = remote.clone();
                    tokio::spawn(async move {
                        let _ = reply.send(remote.latency(super::latency::TIMEOUT).await);
                    });
                }
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

/// What the `hello` says about this side.
fn seat_from_hello(session_id: Id, v: &Value, remote: &RemoteTerminal) -> RemoteSeat {
    let you = &v["you"];
    let session = &v["session"];
    let access = you["access"]
        .as_str()
        .or_else(|| session["access"].as_str())
        .unwrap_or("view")
        .to_string();
    RemoteSeat {
        session_id,
        can_write: remote.can_write(),
        owner: remote.is_owner(),
        participant: remote.participant_id(),
        access,
        owner_name: session["owner_name"]
            .as_str()
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string),
        participants: Participant::list_from_json(&session["participants"]),
        driver: remote.driver(),
        until: remote.control_until(),
    }
}

/// Sends an action of a shared session.
async fn share_action(remote: &RemoteTerminal, action: ShareAction) {
    match action {
        ShareAction::RequestControl => remote.request_control().await,
        ShareAction::ReleaseControl => remote.release_control().await,
        ShareAction::GrantControl(p, minutes) => remote.grant_control(p, minutes).await,
        ShareAction::DenyControl(p) => remote.deny_control(p).await,
        ShareAction::TakeControl => remote.take_control().await,
        ShareAction::AllowJoin(p) => remote.allow_join(p).await,
        ShareAction::DenyJoin(p) => remote.deny_join(p).await,
        ShareAction::Kick(p, revoke) => remote.kick(p, revoke).await,
        ShareAction::StopSharing => remote.stop_sharing().await,
    }
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
