//! The copilot (terminal assistant) running on this computer, when "This
//! computer" is chosen in Settings → AI.
//!
//! It is the same agent loop as on the server (`termoak_ai::agent`), in
//! process:
//! - with your own API key in this app (Anthropic, OpenAI, OpenRouter,
//!   OpenCode Go), Termoak runs the tool loop;
//! - with an agent installed here (Codex, Claude Code, Antigravity,
//!   OpenCode), the agent runs on its own and gets Termoak's tools through a
//!   local MCP endpoint (127.0.0.1, random port and token, only for the run).
//!
//! The tools are the server copilot's ones, but on this computer: the hosts
//! of the local vault (over the app's own SSH engine), the terminals open in
//! the app, files over SFTP and memories. Every call goes through the same
//! permission policy (read-only / ask / confirm / auto) and the approvals
//! are shown in the copilot. Usage and cost are recorded in the local
//! database, without plan limits. Nothing goes to the Termoak server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use termoak_ai::access::ChainEntry;
use termoak_ai::agent::{AgentHooks, AgentRun, SYSTEM_PROMPT, context_block, run_agent};
use termoak_ai::config::{AiConfig, Driver, ProviderConfig, split_spec};
use termoak_ai::engine::AiEngine;
use termoak_ai::engine::{TaskEvent, TaskStatus};
use termoak_ai::mcp::McpTools;
use termoak_ai::mcp_http::LocalMcpServer;
use termoak_ai::message::{Message, Usage};
use termoak_ai::policy::{PermissionMode, Verdict, decide};
use termoak_ai::pricing::{UsageCost, builtin_price, cost_micros};
use termoak_ai::provider::{Registry, ToolSpec};
use termoak_ai::tools::{ToolContext, ToolLimits, ToolOutcome, ToolRuntime, truncate_middle};
use termoak_ai::{AiError, ChainSource, SessionAccess};
use termoak_client::LOCAL_OWNER;
use termoak_core::model::Memory;
use termoak_core::store::AiUsageRow;
use termoak_core::{Id, Store, new_id};
use termoak_ssh::{ConnectionPool, HostKeyPolicy};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::agents;
use super::keys;
use super::{AiSettings, LocalSource, LocalTarget, local_target};

/// Maximum agent steps per message.
const MAX_STEPS: u32 = 40;
/// An unanswered approval counts as denied after this long.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the local AI needs, shared by every copilot and by the local AI
/// tasks (created once, inside the tokio runtime).
pub struct LocalAi {
    pub store: Store,
    pub tools: Arc<ToolRuntime>,
    pool: Arc<ConnectionPool>,
    terminals: Arc<dyn SessionAccess>,
    /// Engine of the AI tasks on this computer (started with
    /// [`LocalAi::start_engine`]) and its MCP endpoint for agents.
    engine: Mutex<Option<(Arc<AiEngine>, Arc<LocalMcpServer>)>>,
}

impl LocalAi {
    /// Tools over the local vault, the app's SSH engine (known hosts only:
    /// nobody can confirm a new host key in the middle of a task) and the
    /// app's terminals.
    pub fn new(store: Store, terminals: Arc<dyn SessionAccess>) -> Self {
        let pool = ConnectionPool::new(
            store.clone(),
            HostKeyPolicy::Strict,
            Duration::from_secs(300),
        );
        let tools = ToolRuntime::new(
            store.clone(),
            pool.clone(),
            Some(terminals.clone()),
            ToolLimits {
                command_timeout: Duration::from_secs(120),
                max_output_chars: 16_000,
            },
        );
        Self {
            store,
            tools: Arc::new(tools),
            pool,
            terminals,
            engine: Mutex::new(None),
        }
    }

    /// The engine of the local AI tasks, once started.
    pub fn engine(&self) -> Option<Arc<AiEngine>> {
        self.engine.lock().as_ref().map(|(e, _)| e.clone())
    }

    /// Starts the engine of the AI tasks on this computer: tasks and their
    /// events and approvals are kept in the local database. Tasks left
    /// running when the app closed are marked as stopped (they can be
    /// continued with a new message). Agents get the tools from a local MCP
    /// endpoint that only accepts the token of a running task.
    pub async fn start_engine(self: &Arc<Self>) -> Result<Arc<AiEngine>, AiError> {
        self.start_engine_with(
            local_engine_config(),
            Arc::new(SettingsChain(self.store.clone())),
        )
        .await
    }

    pub(crate) async fn start_engine_with(
        self: &Arc<Self>,
        config: AiConfig,
        chains: Arc<dyn ChainSource>,
    ) -> Result<Arc<AiEngine>, AiError> {
        if let Some(e) = self.engine() {
            return Ok(e);
        }
        let stopped = self
            .store
            .ai_fail_orphan_tasks_with(&t!("local_ai.interrupted"))
            .await?;
        if stopped > 0 {
            tracing::info!(stopped, "local AI tasks stopped when the app was closed");
        }
        let engine = AiEngine::new(
            self.store.clone(),
            self.pool.clone(),
            Some(self.terminals.clone()),
            config,
        )
        .await?;
        engine.set_chain_source(chains);
        let mcp = LocalMcpServer::start_for_tasks(engine.clone())
            .await
            .map_err(|e| AiError::Process {
                provider: "mcp".into(),
                message: e.to_string(),
            })?;
        engine.set_mcp_url(mcp.url().to_string());
        *self.engine.lock() = Some((engine.clone(), Arc::new(mcp)));
        Ok(engine)
    }
}

/// Engine configuration of the local AI tasks: the built-in providers (used
/// with your own keys) and the agents, looked up when they run (in `PATH`
/// and the usual install folders).
pub fn local_engine_config() -> AiConfig {
    let path_env =
        agents::child_path(&agents::search_dirs()).map(|p| p.to_string_lossy().into_owned());
    let mut config = AiConfig::default();
    for kind in agents::AGENTS {
        let (key, cfg) = agent_provider_config(kind.id, kind.command.to_string(), path_env.clone());
        config.providers.insert(key.to_string(), cfg);
    }
    config
}

/// The chain of each local task, from the current settings (read from the
/// local database, so a change applies to the next message).
struct SettingsChain(Store);

#[async_trait]
impl ChainSource for SettingsChain {
    async fn chain(
        &self,
        _owner: Id,
        _requested: Option<&str>,
    ) -> Result<Vec<ChainEntry>, AiError> {
        let settings = crate::state::Settings::load(&self.0).await.ai;
        prepare(&self.0, &settings)
            .await
            .map(|run| run.chain)
            .map_err(PrepareError::into_ai_error)
    }
}

/// The local AI of the app (set by the main window).
pub struct LocalAiGlobal(pub Arc<LocalAi>);

impl gpui::Global for LocalAiGlobal {}

/// What runs a message: the providers and the chain (one step: nothing
/// falls back to the server).
pub struct LocalRun {
    pub config: AiConfig,
    pub chain: Vec<ChainEntry>,
    /// An agent installed here (it gets the tools over MCP).
    pub agent: bool,
}

/// Why the copilot cannot run with the current settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// "Your API keys" is chosen but there is none.
    NoKey,
    /// "An agent" is chosen but none is installed.
    NoAgent,
    /// Antigravity (experimental) was not turned on.
    AgyDisabled,
    Other(String),
}

impl std::fmt::Display for PrepareError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PrepareError::NoKey => f.write_str(&t!("local_ai.error.no_key")),
            PrepareError::NoAgent => f.write_str(&t!("local_ai.error.no_agent")),
            PrepareError::AgyDisabled => f.write_str(&t!("local_ai.error.agy_disabled")),
            PrepareError::Other(e) => f.write_str(e),
        }
    }
}

impl PrepareError {
    /// Something the user fixes in Settings → AI.
    pub fn fix_in_settings(&self) -> bool {
        !matches!(self, PrepareError::Other(_))
    }

    /// As an engine error: the ones fixed in Settings → AI are
    /// `ai_key_required`.
    pub fn into_ai_error(self) -> AiError {
        if self.fix_in_settings() {
            AiError::KeyRequired(self.to_string())
        } else {
            AiError::NotConfigured(self.to_string())
        }
    }
}

/// Provider key in the AI engine of each agent.
pub fn agent_provider(agent: &str) -> &'static str {
    match agent {
        "codex" => "codex",
        "claude" => "claude-code",
        "agy" => "agy",
        _ => "opencode",
    }
}

/// Engine configuration for an agent installed at `path`.
pub fn agent_config(agent: &str, path: &std::path::Path, path_env: Option<String>) -> AiConfig {
    let mut config = AiConfig::default();
    let (key, cfg) = agent_provider_config(agent, path.display().to_string(), path_env);
    config.providers.insert(key.to_string(), cfg);
    config
}

/// Provider key and configuration of an agent (`command`: name or path).
fn agent_provider_config(
    agent: &str,
    command: String,
    path_env: Option<String>,
) -> (&'static str, ProviderConfig) {
    let driver = match agent {
        "codex" => Driver::CodexCli,
        "claude" => Driver::ClaudeCode,
        "agy" => Driver::Antigravity,
        _ => Driver::OpencodeServer,
    };
    (
        agent_provider(agent),
        ProviderConfig {
            driver,
            label: agents::kind(agent).map(|k| k.name.to_string()),
            command: Some(command),
            path_env,
            timeout_secs: Some(1800),
            subscription: true,
            ..Default::default()
        },
    )
}

/// Antigravity can run commands on this computer without Termoak's
/// approvals: it only runs once the user turned it on.
pub fn check_agent_allowed(agent: &str, settings: &AiSettings) -> Result<(), PrepareError> {
    if agent == "agy" && !settings.agy_opt_in {
        return Err(PrepareError::AgyDisabled);
    }
    Ok(())
}

/// Builds what runs the next message from the settings: the chosen key
/// (read from the vault) or the chosen agent (located now).
pub async fn prepare(store: &Store, settings: &AiSettings) -> Result<LocalRun, PrepareError> {
    let keys = keys::list(store)
        .await
        .map_err(|e| PrepareError::Other(e.to_string()))?;
    let found = agents::locate().await;
    let target = local_target(settings, &keys, &found);
    let has_agent = found.iter().any(|a| a.path.is_some());
    match target {
        Some(LocalTarget::ApiKey { provider, model }) => {
            let secret = keys::secret(store, &provider)
                .await
                .map_err(|e| PrepareError::Other(e.to_string()))?
                .ok_or(PrepareError::NoKey)?;
            Ok(LocalRun {
                config: AiConfig::default(),
                chain: vec![ChainEntry {
                    spec: match model {
                        Some(m) => format!("{provider}::{m}"),
                        None => provider,
                    },
                    own_key: Some(secret.key),
                }],
                agent: false,
            })
        }
        Some(LocalTarget::Agent { agent, path }) => {
            check_agent_allowed(agent, settings)?;
            let path_env = agents::child_path(&agents::search_dirs())
                .map(|p| p.to_string_lossy().into_owned());
            Ok(LocalRun {
                config: agent_config(agent, &path, path_env),
                chain: vec![ChainEntry::server(agent_provider(agent))],
                agent: true,
            })
        }
        None => Err(match settings.local_source(!keys.is_empty(), has_agent) {
            LocalSource::ApiKey => PrepareError::NoKey,
            LocalSource::Agent => PrepareError::NoAgent,
        }),
    }
}

/// Text of a failed run for the interface (translated by its code).
pub fn error_text(e: &AiError) -> String {
    match e.code() {
        "not_installed" => t!("local_ai.error.not_installed", detail = e.to_string()).to_string(),
        "not_logged_in" => t!("local_ai.error.not_logged_in", detail = e.to_string()).to_string(),
        "timeout" => t!("local_ai.error.timeout").to_string(),
        "key_rejected" => t!("local_ai.error.key_rejected").to_string(),
        "rate_limited" => t!("local_ai.error.rate_limited").to_string(),
        "network" => t!("local_ai.error.network", detail = e.to_string()).to_string(),
        "cancelled" => t!("local_ai.error.cancelled").to_string(),
        _ => t!("local_ai.error.other", detail = e.to_string()).to_string(),
    }
}

/// A pending approval (for the view).
#[derive(Clone)]
struct Pending {
    id: Id,
    tool: String,
    summary: String,
    input: Value,
    preview: Option<termoak_ai::ApprovalPreview>,
}

struct State {
    title: String,
    status: TaskStatus,
    mode: PermissionMode,
    messages: Vec<Message>,
    used_provider: Option<String>,
    error: Option<String>,
    cost_micros: i64,
    created_at: i64,
    pending: Vec<Pending>,
}

/// A copilot conversation on this computer (it lives while the copilot
/// panel does; it is not saved).
pub struct LocalConversation {
    pub id: Id,
    services: Arc<LocalAi>,
    state: Mutex<State>,
    approvals: Mutex<HashMap<Id, oneshot::Sender<(bool, bool)>>>,
    cancel: Mutex<CancellationToken>,
    events: mpsc::UnboundedSender<TaskEvent>,
    /// A complete message waits for the checkpoint that saves it, so the
    /// view never reloads without it.
    held: Mutex<Option<TaskEvent>>,
}

impl LocalConversation {
    pub fn new(
        services: Arc<LocalAi>,
        mode: PermissionMode,
        events: mpsc::UnboundedSender<TaskEvent>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: new_id(),
            services,
            state: Mutex::new(State {
                title: String::new(),
                status: TaskStatus::Completed,
                mode,
                messages: Vec::new(),
                used_provider: None,
                error: None,
                cost_micros: 0,
                created_at: termoak_core::time::now_ms(),
                pending: Vec::new(),
            }),
            approvals: Mutex::new(HashMap::new()),
            cancel: Mutex::new(CancellationToken::new()),
            events,
            held: Mutex::new(None),
        })
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().status.is_active()
    }

    fn emit(&self, ev: TaskEvent) {
        let _ = self.events.send(ev);
    }

    fn set_status(&self, status: TaskStatus) {
        self.state.lock().status = status;
        self.emit(TaskEvent::Status { status });
    }

    /// The conversation in the shape of a server task (`GET /ai/tasks/{id}`),
    /// so the copilot shows it the same way.
    pub fn view(&self) -> Value {
        let s = self.state.lock();
        json!({
            "id": self.id,
            "title": s.title,
            "status": s.status.as_str(),
            "mode": s.mode.as_str(),
            "provider": "",
            "used_provider": s.used_provider,
            "created_at": s.created_at,
            "cost_micros": s.cost_micros,
            "error": s.error,
            "messages": s.messages,
            "pending_approvals": s.pending.iter().map(|p| json!({
                "id": p.id,
                "tool": p.tool,
                "summary": p.summary,
                "input": p.input,
                "preview": p.preview,
                "status": "pending",
            })).collect::<Vec<_>>(),
        })
    }

    /// Adds the user's message and runs the agent in the background. The
    /// first message carries the context (mode, terminal, memories).
    pub fn send(
        self: &Arc<Self>,
        text: String,
        run: LocalRun,
        terminal: Option<Id>,
    ) -> Result<(), String> {
        if self.is_running() {
            return Err(t!("local_ai.error.busy").to_string());
        }
        {
            let mut s = self.state.lock();
            s.status = TaskStatus::Queued;
            s.error = None;
            if s.title.is_empty() {
                s.title = title_of(&text);
            }
        }
        let cancel = CancellationToken::new();
        *self.cancel.lock() = cancel.clone();
        let this = self.clone();
        tokio::spawn(async move { this.run(text, run, terminal, cancel).await });
        Ok(())
    }

    async fn run(
        self: Arc<Self>,
        text: String,
        run: LocalRun,
        terminal: Option<Id>,
        cancel: CancellationToken,
    ) {
        let first = self.state.lock().messages.is_empty();
        let prompt = if first {
            let mode = self.state.lock().mode;
            let memories: Vec<String> = self
                .services
                .store
                .list::<Memory>(LOCAL_OWNER)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|m| m.data.content)
                .collect();
            context_block(
                mode,
                None,
                terminal.map(|t| t.to_string()).as_deref(),
                &memories,
            ) + &text
        } else {
            text.clone()
        };
        let mut messages = {
            let mut s = self.state.lock();
            s.messages.push(Message::user_text(prompt));
            s.messages.clone()
        };
        self.emit(TaskEvent::Message {
            role: "user".into(),
            text,
            provider: None,
        });
        self.set_status(TaskStatus::Running);

        // External agents get the tools through a local MCP endpoint.
        let mcp = if run.agent {
            match LocalMcpServer::start(Arc::new(ConversationTools(self.clone()))).await {
                Ok(server) => Some(server),
                Err(e) => {
                    self.finish(Err(AiError::Process {
                        provider: "mcp".into(),
                        message: e.to_string(),
                    }));
                    return;
                }
            }
        } else {
            None
        };
        let registry = Registry::new(run.config);
        let agent_run = AgentRun {
            registry: &registry,
            chain: run.chain,
            tools: self.services.tools.specs(),
            system: SYSTEM_PROMPT.to_string(),
            session_id: format!("ses_{}", self.id.simple()),
            max_steps: MAX_STEPS,
            effort: None,
            mcp: mcp
                .as_ref()
                .map(|m| (m.url().to_string(), m.token().to_string())),
            cancel: cancel.clone(),
        };
        let hooks = ConversationHooks(self.clone());
        let result = run_agent(agent_run, &mut messages, &hooks).await;
        drop(mcp);
        self.state.lock().messages = messages;
        self.finish(result.map(|o| o.final_text));
    }

    fn finish(&self, result: Result<String, AiError>) {
        self.approvals.lock().clear();
        if let Some(ev) = self.held.lock().take() {
            self.emit(ev);
        }
        let (status, result, error) = match result {
            Ok(text) => (TaskStatus::Completed, Some(text), None),
            Err(AiError::Cancelled) => (
                TaskStatus::Cancelled,
                None,
                Some(t!("local_ai.error.cancelled").to_string()),
            ),
            Err(e) => {
                tracing::warn!(error = %e, "the local copilot failed");
                (TaskStatus::Failed, None, Some(error_text(&e)))
            }
        };
        {
            let mut s = self.state.lock();
            s.status = status;
            s.error = error.clone();
            s.pending.clear();
        }
        self.emit(TaskEvent::Finished {
            status,
            result,
            error,
        });
    }

    /// Answers a pending approval (`always`: autonomous from now on).
    pub fn decide(&self, approval: Id, approve: bool, always: bool) -> bool {
        match self.approvals.lock().remove(&approval) {
            Some(tx) => tx.send((approve, always)).is_ok(),
            None => false,
        }
    }

    pub fn cancel(&self) {
        self.cancel.lock().cancel();
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        self.state.lock().mode = mode;
    }

    /// Runs a tool with the permission policy and, if needed, the user's
    /// approval (the same steps as a server task).
    pub async fn call_tool(&self, call_id: &str, name: &str, input: &Value) -> ToolOutcome {
        let effect = ToolRuntime::effect(name, input);
        let summary = ToolRuntime::summarize(name, input);
        let mode = self.state.lock().mode;
        let verdict = decide(mode, name, effect);
        self.emit(TaskEvent::ToolCall {
            call_id: call_id.to_string(),
            tool: name.to_string(),
            summary: summary.clone(),
            input: input.clone(),
            needs_approval: verdict == Verdict::NeedsApproval,
        });
        let started = std::time::Instant::now();
        let outcome = match verdict {
            Verdict::Deny(reason) => ToolOutcome {
                ok: false,
                content: format!("Denied: {reason}."),
            },
            Verdict::NeedsApproval => {
                if self.request_approval(call_id, name, input, &summary).await {
                    self.execute(name, input).await
                } else {
                    ToolOutcome {
                        ok: false,
                        content: "The user did NOT approve this action. Do not retry it in another form; explain alternatives or ask.".into(),
                    }
                }
            }
            Verdict::Allow => self.execute(name, input).await,
        };
        self.emit(TaskEvent::ToolResult {
            call_id: call_id.to_string(),
            ok: outcome.ok,
            output: truncate_middle(&outcome.content, 4000),
            duration_ms: started.elapsed().as_millis() as u64,
        });
        outcome
    }

    async fn execute(&self, name: &str, input: &Value) -> ToolOutcome {
        let ctx = ToolContext {
            owner: LOCAL_OWNER,
            task_id: Some(self.id),
            host_scope: None,
        };
        self.services.tools.execute(&ctx, name, input).await
    }

    async fn request_approval(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        summary: &str,
    ) -> bool {
        let approval_id = new_id();
        let (tx, rx) = oneshot::channel();
        self.approvals.lock().insert(approval_id, tx);
        // What the approval shows (the command with its risk, the diff of
        // a file). The copilot only approves or denies: no edits here.
        let ctx = ToolContext {
            owner: LOCAL_OWNER,
            task_id: Some(self.id),
            host_scope: None,
        };
        let mut preview = self.services.tools.preview(&ctx, name, input).await;
        preview.editable = false;
        self.state.lock().pending.push(Pending {
            id: approval_id,
            tool: name.to_string(),
            summary: summary.to_string(),
            input: input.clone(),
            preview: Some(preview.clone()),
        });
        self.emit(TaskEvent::ApprovalRequested {
            approval_id,
            call_id: call_id.to_string(),
            tool: name.to_string(),
            summary: summary.to_string(),
            input: input.clone(),
            preview: Some(preview),
        });
        self.set_status(TaskStatus::WaitingApproval);
        let cancel = self.cancel.lock().clone();
        let decision = tokio::select! {
            _ = cancel.cancelled() => None,
            r = tokio::time::timeout(APPROVAL_TIMEOUT, rx) => r.ok().and_then(|r| r.ok()),
        };
        self.approvals.lock().remove(&approval_id);
        self.state.lock().pending.retain(|p| p.id != approval_id);
        let (approved, by) = match decision {
            Some((approved, always)) => {
                if approved && always {
                    self.state.lock().mode = PermissionMode::Auto;
                }
                (approved, "user")
            }
            None => (false, "timeout"),
        };
        self.emit(TaskEvent::ApprovalDecided {
            approval_id,
            approved,
            by: by.into(),
            edited: None,
            reason: None,
        });
        if !cancel.is_cancelled() {
            self.set_status(TaskStatus::Running);
        }
        approved
    }

    /// Records the usage of a turn in the local database.
    async fn record(&self, spec: &str, usage: &Usage, cost: UsageCost) {
        self.state.lock().cost_micros += cost.cost_micros;
        record_usage(&self.services.store, Some(self.id), spec, usage, cost).await;
    }
}

/// Records some usage of the AI on this computer (no plan limits: only to
/// know what it cost).
async fn record_usage(store: &Store, task: Option<Id>, spec: &str, usage: &Usage, cost: UsageCost) {
    let row = AiUsageRow {
        owner_id: LOCAL_OWNER,
        task_id: task,
        provider: spec.to_string(),
        own_key: true,
        input_tokens: (usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens)
            as i64,
        output_tokens: usage.output_tokens as i64,
        cost_micros: cost.cost_micros,
        credit_micros: 0,
        created_at: termoak_core::time::now_ms(),
    };
    if let Err(e) = store.ai_record_usage(row).await {
        tracing::warn!(error = %e, "could not record the local AI usage");
    }
}

/// Quick assistant on this computer: a command for a request, in the shape
/// of `POST /api/v1/ai/suggest` (`command`, `explanation`, `risk`,
/// `provider`). `context` is the terminal's (`os`, `screen`).
pub async fn suggest(
    store: &Store,
    settings: &AiSettings,
    request: &str,
    context: Value,
) -> Result<Value, String> {
    let run = prepare(store, settings).await.map_err(|e| e.to_string())?;
    let registry = Registry::new(run.config);
    let ctx = serde_json::from_value(context).unwrap_or_default();
    let cancel = CancellationToken::new();
    let (suggestion, turn) =
        termoak_ai::assist::suggest_with(&registry, &run.chain, request, &ctx, &cancel)
            .await
            .map_err(|e| error_text(&e))?;
    record_usage(
        store,
        None,
        &turn.spec,
        &turn.usage,
        local_cost(&turn.spec, &turn.usage),
    )
    .await;
    serde_json::to_value(suggestion).map_err(|e| e.to_string())
}

/// Quick assistant on this computer: explains a text (with an optional
/// question about it), in the shape of `POST /api/v1/ai/explain`
/// (`answer`, `provider`).
pub async fn explain(
    store: &Store,
    settings: &AiSettings,
    text: &str,
    question: Option<&str>,
    context: Value,
) -> Result<Value, String> {
    let run = prepare(store, settings).await.map_err(|e| e.to_string())?;
    let registry = Registry::new(run.config);
    let ctx = serde_json::from_value(context).unwrap_or_default();
    let cancel = CancellationToken::new();
    let turn =
        termoak_ai::assist::explain_with(&registry, &run.chain, text, question, &ctx, &cancel)
            .await
            .map_err(|e| error_text(&e))?;
    record_usage(
        store,
        None,
        &turn.spec,
        &turn.usage,
        local_cost(&turn.spec, &turn.usage),
    )
    .await;
    Ok(json!({"answer": turn.text, "provider": turn.spec}))
}

/// What the AI on this computer spent this month (UTC), in micro-USD, from
/// the usage ledger: the real cost with your own keys (agents on a
/// subscription cost nothing).
pub async fn spent_this_month(store: &Store) -> Result<i64, termoak_core::CoreError> {
    store
        .ai_usage_cost_since(LOCAL_OWNER, termoak_ai::engine::month_start_ms())
        .await
}

/// Short title from the first message.
fn title_of(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut t: String = line.trim().chars().take(60).collect();
    if line.trim().chars().count() > 60 {
        t.push('…');
    }
    t
}

/// Real cost of some usage on this computer: the price of the model with
/// your own key, nothing on an agent's subscription.
fn local_cost(spec: &str, usage: &Usage) -> UsageCost {
    let (key, model) = split_spec(spec);
    let subscription = matches!(
        key.as_str(),
        "codex" | "claude-code" | "agy" | "opencode" | "opencode-api"
    );
    let price = model.as_deref().and_then(builtin_price);
    UsageCost {
        cost_micros: cost_micros(usage, price, subscription),
        credit_micros: 0,
    }
}

struct ConversationHooks(Arc<LocalConversation>);

#[async_trait]
impl AgentHooks for ConversationHooks {
    fn emit(&self, ev: TaskEvent) {
        if let TaskEvent::Message {
            provider: Some(p), ..
        } = &ev
        {
            self.0.state.lock().used_provider = Some(p.clone());
            *self.0.held.lock() = Some(ev);
            return;
        }
        self.0.emit(ev);
    }

    async fn call_tool(&self, call_id: &str, name: &str, input: &Value) -> ToolOutcome {
        self.0.call_tool(call_id, name, input).await
    }

    async fn checkpoint(&self, messages: &[Message]) {
        self.0.state.lock().messages = messages.to_vec();
        if let Some(ev) = self.0.held.lock().take() {
            self.0.emit(ev);
        }
    }

    fn cost(&self, spec: &str, _own_key: bool, usage: &Usage) -> UsageCost {
        local_cost(spec, usage)
    }

    async fn record_usage(&self, spec: &str, _own_key: bool, usage: &Usage, cost: UsageCost) {
        self.0.record(spec, usage, cost).await;
    }

    /// No plan limits on this computer.
    async fn server_credit_left(&self) -> bool {
        true
    }
}

/// The conversation's tools for an external agent (over local MCP).
struct ConversationTools(Arc<LocalConversation>);

#[async_trait]
impl McpTools for ConversationTools {
    fn specs(&self) -> Vec<ToolSpec> {
        self.0.services.tools.specs()
    }

    async fn call(&self, name: &str, args: &Value) -> Result<ToolOutcome, (i64, String)> {
        if !self.0.is_running() {
            return Err((-32001, "the conversation is no longer active".into()));
        }
        Ok(self
            .0
            .call_tool(&format!("mcp_{}", new_id().simple()), name, args)
            .await)
    }
}

#[cfg(test)]
mod tests {
    use termoak_ai::TerminalOutput;
    use termoak_core::crypto::MasterKey;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    /// One terminal that records what is typed into it.
    #[derive(Default)]
    struct FakeTerminal {
        id: Mutex<Option<Id>>,
        typed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SessionAccess for FakeTerminal {
        async fn list(&self, _: Id) -> Vec<termoak_ai::SessionSummary> {
            Vec::new()
        }
        async fn read(&self, _: Id, _: Id, _: usize) -> Result<String, String> {
            Ok("$ ".into())
        }
        async fn send(&self, _: Id, _: Id, input: &str) -> Result<(), String> {
            self.typed.lock().push(input.to_string());
            Ok(())
        }
        async fn send_and_collect(
            &self,
            _: Id,
            session: Id,
            input: &str,
            _: Duration,
            _: Duration,
        ) -> Result<Option<TerminalOutput>, String> {
            if *self.id.lock() != Some(session) {
                return Err("no such terminal".into());
            }
            self.typed.lock().push(input.to_string());
            Ok(Some(TerminalOutput {
                text: " 10:00:00 up 3 days, load average: 0.01".into(),
                still_running: false,
            }))
        }
    }

    fn services(term: Arc<FakeTerminal>) -> Arc<LocalAi> {
        let store = Store::open_in_memory(MasterKey::generate()).unwrap();
        Arc::new(LocalAi::new(store, term))
    }

    async fn next_approval(rx: &mut mpsc::UnboundedReceiver<TaskEvent>) -> Id {
        loop {
            match rx.recv().await.unwrap() {
                TaskEvent::ApprovalRequested { approval_id, .. } => return approval_id,
                _ => continue,
            }
        }
    }

    #[tokio::test]
    async fn tools_follow_the_permission_policy() {
        let term = Arc::new(FakeTerminal::default());
        let session = new_id();
        *term.id.lock() = Some(session);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let conv = LocalConversation::new(services(term.clone()), PermissionMode::ReadOnly, tx);
        let typing = |cmd: &str| json!({"session_id": session.to_string(), "input": cmd});

        // Read-only: changes are denied, reading commands run.
        let out = conv
            .call_tool("c1", "send_to_terminal", &typing("rm -rf /tmp/x"))
            .await;
        assert!(!out.ok && out.content.starts_with("Denied"));
        assert!(term.typed.lock().is_empty());
        let out = conv
            .call_tool("c2", "send_to_terminal", &typing("uptime"))
            .await;
        assert!(out.ok, "{}", out.content);
        assert_eq!(term.typed.lock().as_slice(), ["uptime\r"]);

        // Ask: a change waits for the user; denied, it does not run.
        conv.set_mode(PermissionMode::Ask);
        let c = conv.clone();
        let input = typing("systemctl restart nginx");
        let call = tokio::spawn(async move { c.call_tool("c3", "send_to_terminal", &input).await });
        let approval = next_approval(&mut rx).await;
        assert_eq!(
            conv.view()["pending_approvals"].as_array().unwrap().len(),
            1
        );
        assert!(conv.decide(approval, false, false));
        let out = call.await.unwrap();
        assert!(!out.ok && out.content.contains("did NOT approve"));
        assert_eq!(term.typed.lock().len(), 1);
        assert!(
            conv.view()["pending_approvals"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        // Approved with "always": it runs and the mode becomes autonomous.
        let c = conv.clone();
        let input = typing("systemctl restart nginx");
        let call = tokio::spawn(async move { c.call_tool("c4", "send_to_terminal", &input).await });
        let approval = next_approval(&mut rx).await;
        assert!(conv.decide(approval, true, true));
        assert!(call.await.unwrap().ok);
        assert_eq!(term.typed.lock().len(), 2);
        assert_eq!(conv.view()["mode"], "auto");
        // Now changes run without asking.
        let out = conv
            .call_tool("c5", "send_to_terminal", &typing("touch /tmp/y"))
            .await;
        assert!(out.ok);
    }

    /// OpenAI-compatible server: the first turn asks for a tool, the second
    /// answers. Returns its address and the requests it got.
    async fn mock_openai() -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                // Request: headers, then Content-Length bytes.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                while buf.len() < body_start + len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let request = String::from_utf8_lossy(&buf).to_string();
                let turn = {
                    let mut l = log.lock();
                    l.push(request);
                    l.len()
                };
                let events: Vec<Value> = if turn == 1 {
                    let session = {
                        let l = log.lock();
                        let body = &l[0];
                        let i = body.find("Terminal the request comes from: ").unwrap() + 33;
                        body[i..i + 36].to_string()
                    };
                    vec![
                        json!({"model": "test-model", "choices": [{"delta": {"content": "Let me check."}}]}),
                        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "send_to_terminal", "arguments": json!({"session_id": session, "input": "uptime"}).to_string()}}]}}]}),
                        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10, "cost": 0.0006}}),
                    ]
                } else {
                    vec![
                        json!({"choices": [{"delta": {"content": "The server is fine."}}]}),
                        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
                        json!({"choices": [], "usage": {"prompt_tokens": 150, "completion_tokens": 8, "cost": 0.0006}}),
                    ]
                };
                let mut out = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for e in events {
                    out.push_str(&format!("data: {e}\n\n"));
                }
                out.push_str("data: [DONE]\n\n");
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    #[tokio::test]
    async fn local_copilot_runs_against_an_openai_compatible_server() {
        let (base_url, seen) = mock_openai().await;
        let term = Arc::new(FakeTerminal::default());
        let session = new_id();
        *term.id.lock() = Some(session);
        let services = services(term.clone());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let conv = LocalConversation::new(services.clone(), PermissionMode::Ask, tx);

        let mut config = AiConfig::default();
        let mut provider = termoak_ai::config::builtin_providers()["openrouter"].clone();
        provider.base_url = Some(base_url);
        config.providers.insert("openrouter".into(), provider);
        let run = LocalRun {
            config,
            chain: vec![ChainEntry {
                spec: "openrouter::test-model".into(),
                own_key: Some(zeroize::Zeroizing::new("sk-test-123".into())),
            }],
            agent: false,
        };
        conv.send("is the server ok?".into(), run, Some(session))
            .unwrap();
        assert!(conv.is_running());

        let mut tools = Vec::new();
        let finished = loop {
            match tokio::time::timeout(Duration::from_secs(20), rx.recv())
                .await
                .unwrap()
                .unwrap()
            {
                TaskEvent::ToolCall {
                    tool,
                    needs_approval,
                    ..
                } => tools.push((tool, needs_approval)),
                TaskEvent::Finished { status, result, .. } => break (status, result),
                _ => {}
            }
        };
        assert_eq!(finished.0, TaskStatus::Completed);
        assert_eq!(finished.1.as_deref(), Some("The server is fine."));
        // A read-only command typed into the terminal needs no approval.
        assert_eq!(tools, [("send_to_terminal".to_string(), false)]);
        assert_eq!(term.typed.lock().as_slice(), ["uptime\r"]);

        let requests = seen.lock().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .to_lowercase()
                .contains("authorization: bearer sk-test-123")
        );
        assert!(requests[0].contains("\"send_to_terminal\""));
        assert!(requests[0].contains("Permission mode: ask"));
        assert!(requests[1].contains("load average"));

        let view = conv.view();
        assert_eq!(view["status"], "completed");
        assert_eq!(view["used_provider"], "openrouter::test-model");
        assert_eq!(view["cost_micros"], 1200);
        // The usage is recorded on this computer.
        assert_eq!(spent_this_month(&services.store).await.unwrap(), 1200);
    }

    #[test]
    fn agents_map_to_drivers() {
        let cfg = agent_config("claude", std::path::Path::new("/usr/bin/claude"), None);
        let p = &cfg.providers["claude-code"];
        assert_eq!(p.driver, Driver::ClaudeCode);
        assert_eq!(p.command.as_deref(), Some("/usr/bin/claude"));
        assert!(p.subscription);
        assert_eq!(
            agent_config("agy", std::path::Path::new("/x/agy"), None).providers["agy"].driver,
            Driver::Antigravity
        );
        assert_eq!(
            agent_config("opencode", std::path::Path::new("/x/opencode"), None).providers
                ["opencode"]
                .driver,
            Driver::OpencodeServer
        );
        assert_eq!(agent_provider("codex"), "codex");
    }

    #[test]
    fn costs_are_local() {
        let usage = Usage {
            input_tokens: 1_000_000,
            ..Default::default()
        };
        assert!(local_cost("claude::claude-opus-5", &usage).cost_micros > 0);
        assert_eq!(local_cost("claude-code", &usage).cost_micros, 0);
        assert_eq!(local_cost("claude::claude-opus-5", &usage).credit_micros, 0);
    }

    #[test]
    fn titles() {
        assert_eq!(
            title_of("\n  why is nginx down?\nmore"),
            "why is nginx down?"
        );
        assert_eq!(title_of(&"x".repeat(70)).chars().count(), 61);
    }
}
