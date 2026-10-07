//! Live conversation with an AI task (text, reasoning, tools with their
//! output, and approvals). Used by the AI section and by the copilot, the
//! panel to the right of the terminal.

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement,
    IntoElement, ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Subscription, WeakEntity, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::spinner::Spinner;
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_core::Id;
use termoak_core::model::SyncMode;

use std::sync::Arc;

use gpui::Task;
use termoak_ai::engine::TaskEvent;
use termoak_ai::policy::PermissionMode;
use termoak_ai::{ApprovalDecision, ApprovalPreview, HostRun, RiskLevel, TaskPlan};

use crate::local_ai::RunOn;
use crate::local_ai::copilot::{LocalAiGlobal, LocalConversation, prepare};
use crate::local_ai::tasks::{AiBackend, choose_backend};
use crate::runtime;
use crate::state::{AiFailure, AppModel, ModelEvent};
use crate::terminal::TerminalView;
use crate::ui::{self, IconName};

#[derive(Clone)]
pub struct TaskSummary {
    pub id: Id,
    pub title: String,
    pub status: String,
    pub mode: String,
    pub provider: String,
    pub created_at: i64,
    pub cost_micros: i64,
    pub pending: usize,
    /// A multi-host task: one conversation per host.
    pub fan_out: bool,
    /// Hosts it is limited to.
    pub hosts: usize,
    /// The multi-host task this host's conversation belongs to.
    pub parent_id: Option<Id>,
    /// "Plan before acting".
    pub plan_first: bool,
}

impl TaskSummary {
    pub fn from(v: &Value) -> Option<Self> {
        Some(Self {
            id: v["id"].as_str()?.parse().ok()?,
            title: v["title"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| t!("ai_chat.default_task_title").to_string()),
            status: v["status"].as_str().unwrap_or("").to_string(),
            mode: v["mode"].as_str().unwrap_or("ask").to_string(),
            provider: v["used_provider"]
                .as_str()
                .or_else(|| v["provider"].as_str())
                .unwrap_or("")
                .to_string(),
            created_at: v["created_at"].as_i64().unwrap_or(0),
            cost_micros: v["cost_micros"].as_i64().unwrap_or(0),
            pending: v["pending_approvals"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            fan_out: v["fan_out"].as_bool().unwrap_or(false),
            hosts: v["host_ids"].as_array().map(|a| a.len()).unwrap_or(0),
            parent_id: v["parent_id"].as_str().and_then(|s| s.parse().ok()),
            plan_first: v["plan_first"].as_bool().unwrap_or(false),
        })
    }

    pub fn running(&self) -> bool {
        matches!(
            self.status.as_str(),
            "queued" | "running" | "waiting_approval"
        )
    }
}

#[derive(Clone)]
struct Approval {
    id: Id,
    tool: String,
    summary: String,
    input: Value,
    /// What to show (servers before 0.5 send none: no edits then).
    preview: Option<ApprovalPreview>,
}

impl Approval {
    /// From a pending approval (`id_key`: `id`) or an `approval_requested`
    /// event (`approval_id`).
    fn from(v: &Value, id_key: &str) -> Option<Self> {
        Some(Self {
            id: v[id_key].as_str()?.parse().ok()?,
            tool: v["tool"].as_str().unwrap_or("").to_string(),
            summary: v["summary"].as_str().unwrap_or("").to_string(),
            input: v["input"].clone(),
            preview: serde_json::from_value(v["preview"].clone()).ok(),
        })
    }

    fn is_plan(&self) -> bool {
        self.tool == "plan"
    }

    /// The command or plan can be edited before approving it.
    fn editable(&self) -> bool {
        self.preview
            .as_ref()
            .is_some_and(|p| p.editable && (p.command.is_some() || p.plan.is_some()))
    }
}

struct TaskDetail {
    summary: TaskSummary,
    messages: Vec<Value>,
    approvals: Vec<Approval>,
    error: Option<String>,
    /// Per-host table of a multi-host task.
    hosts: Vec<HostRun>,
    plan: Option<TaskPlan>,
}

impl TaskDetail {
    fn from(v: &Value) -> Option<Self> {
        Some(Self {
            summary: TaskSummary::from(v)?,
            messages: v["messages"].as_array().cloned().unwrap_or_default(),
            approvals: v["pending_approvals"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|x| x["status"].as_str().unwrap_or("pending") == "pending")
                        .filter_map(|x| Approval::from(x, "id"))
                        .collect()
                })
                .unwrap_or_default(),
            error: v["error"].as_str().map(str::to_string),
            hosts: serde_json::from_value(v["hosts"].clone()).unwrap_or_default(),
            plan: serde_json::from_value(v["plan"].clone()).ok(),
        })
    }

    /// Does the result of this call already appear in the saved messages?
    fn has_result(&self, call_id: &str) -> bool {
        self.messages.iter().any(|m| {
            m["content"].as_array().is_some_and(|parts| {
                parts
                    .iter()
                    .any(|p| p["type"] == "tool_result" && p["id"] == call_id)
            })
        })
    }
}

/// Live items not yet saved in the conversation.
enum LiveItem {
    Notice(String),
    Tool {
        call_id: String,
        tool: String,
        summary: String,
        result: Option<(bool, String)>,
    },
}

pub fn mode_label(mode: &str) -> SharedString {
    match mode {
        "read_only" => t!("ai_chat.mode.read_only"),
        "auto" => t!("ai_chat.mode.auto"),
        "confirm" => t!("ai_chat.mode.confirm"),
        _ => t!("ai_chat.mode.ask"),
    }
}

/// Permission modes.
pub const MODES: [&str; 4] = ["ask", "confirm", "auto", "read_only"];

/// Explanation of a permission mode.
pub fn mode_hint(mode: &str) -> SharedString {
    match mode {
        "read_only" => t!("ai_chat.mode_hint.read_only"),
        "auto" => t!("ai_chat.mode_hint.auto"),
        "confirm" => t!("ai_chat.mode_hint.confirm"),
        _ => t!("ai_chat.mode_hint.ask"),
    }
}

pub fn status_label(status: &str) -> SharedString {
    match status {
        "queued" => t!("ai_chat.status.queued"),
        "running" => t!("ai_chat.status.running"),
        "waiting_approval" => t!("ai_chat.status.waiting_approval"),
        "completed" => t!("ai_chat.status.completed"),
        "failed" => t!("ai_chat.status.failed"),
        "cancelled" => t!("ai_chat.status.cancelled"),
        _ => "—".into(),
    }
}

/// Removes the `<context>…</context>` blocks in front of the user's message
/// (the engine's one and, in the copilot, the terminal screen's one).
fn strip_context(text: &str) -> String {
    let mut rest = text.trim_start();
    while rest.starts_with("<context>")
        && let Some(end) = rest.find("</context>")
    {
        rest = rest[end + "</context>".len()..].trim_start();
    }
    rest.trim_end().to_string()
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{head}…")
    }
}

/// The last `max` characters.
fn tail(s: &str, max: usize) -> &str {
    match s.char_indices().rev().nth(max.saturating_sub(1)) {
        Some((i, _)) if i > 0 => &s[i..],
        _ => s,
    }
}

/// Screen of a terminal the AI cannot read on its own, in front of the
/// user's message.
fn with_screen(label: &str, screen: &str, text: &str) -> String {
    let screen = tail(screen.trim_end(), 4000);
    if screen.trim().is_empty() {
        return text.to_string();
    }
    format!(
        "<context>\nThe latest output shown by the user's terminal ({label}):\n```\n{screen}\n```\n</context>\n\n{text}"
    )
}

/// Suggestions of the empty copilot.
fn suggestions() -> [SharedString; 3] {
    [
        t!("ai_chat.suggestion.error"),
        t!("ai_chat.suggestion.disk_memory"),
        t!("ai_chat.suggestion.slow_server"),
    ]
}

pub enum AiChatEvent {
    /// The task changed (status, title...) or was created from the chat.
    Summary(TaskSummary),
    /// The copilot asks to be closed.
    Close,
    /// Open Settings → AI (to add an API key, see the credit...).
    OpenAiSettings,
}

/// The backend of the AI tasks for the current choice in Settings → AI:
/// this computer's engine or the account's server.
pub fn ai_backend(model: &Entity<AppModel>, cx: &App) -> Option<AiBackend> {
    let m = model.read(cx);
    let engine = cx.try_global::<LocalAiGlobal>().and_then(|g| g.0.engine());
    choose_backend(m.ai_run_on() == RunOn::Local, m.ai_api(), engine)
}

/// The result of an AI request (with its [`AiFailure`]) once back on the
/// interface thread.
pub fn ai_result<T>(res: Result<Result<T, AiFailure>, String>) -> Result<T, AiFailure> {
    res.map_err(AiFailure::other).and_then(|r| r)
}

/// An AI request failed for a reason Settings → AI fixes (no API key, AI
/// credit spent): the message with a button that opens it.
pub fn fix_banner(
    text: &str,
    open_settings: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .gap_2()
        .items_center()
        .flex_wrap()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.warning)
        .child(
            ui::icon(IconName::KeyRound)
                .size(px(16.))
                .text_color(theme.warning),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(160.))
                .text_sm()
                .child(ui::capitalize(text)),
        )
        .child(
            Button::new("ai-fix-settings")
                .small()
                .primary()
                .label(t!("ai_local.open_settings"))
                .on_click(move |_: &ClickEvent, window, cx| open_settings(window, cx)),
        )
        .into_any_element()
}

pub struct AiChat {
    model: Entity<AppModel>,
    /// Terminal of the copilot (without it, this is the AI section's conversation).
    terminal: Option<WeakEntity<TerminalView>>,
    task: Option<Id>,
    detail: Option<TaskDetail>,
    live_text: String,
    live_reasoning: String,
    live: Vec<LiveItem>,
    input: Entity<InputState>,
    /// Permissions of the task created from the copilot.
    mode: &'static str,
    sending: bool,
    /// Sharing the terminal was already tried and failed: its screen is
    /// attached instead.
    share_failed: bool,
    /// Terminal session known by the AI (if it changes, the AI is told).
    bound_session: Option<Id>,
    /// The last request failed for a reason Settings → AI fixes.
    blocked: Option<String>,
    /// Copilot conversation running on this computer ("This computer" in
    /// Settings → AI); `task` is its id.
    local: Option<Arc<LocalConversation>>,
    /// Approval being edited before approving it (its command or plan).
    editing: Option<(Id, Entity<TextareaState>)>,
    /// Approval being denied, with the reason for the AI.
    denying: Option<(Id, Entity<InputState>)>,
    _local_events: Option<Task<()>>,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<AiChatEvent> for AiChat {}

impl AiChat {
    /// Conversation of the AI section (the task is chosen from outside).
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(model, None, t!("ai_chat.reply_placeholder"), window, cx)
    }

    /// Copilot of a terminal.
    pub fn copilot(
        model: Entity<AppModel>,
        terminal: WeakEntity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(
            model,
            Some(terminal),
            t!("ai_chat.copilot_placeholder"),
            window,
            cx,
        )
    }

    fn build(
        model: Entity<AppModel>,
        terminal: Option<WeakEntity<TerminalView>>,
        placeholder: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subs = vec![
            cx.subscribe_in(
                &model,
                window,
                |this, _, ev: &ModelEvent, window, cx| match ev {
                    ModelEvent::Server(v) if v["type"] == "ai" => this.on_event(v, window, cx),
                    // A conversation on this computer does not depend on the session.
                    ModelEvent::SessionChanged if this.local.is_none() => {
                        this.set_task(None, window, cx)
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.send(window, cx);
                }
            }),
        ];
        Self {
            model,
            terminal,
            task: None,
            detail: None,
            live_text: String::new(),
            live_reasoning: String::new(),
            live: Vec::new(),
            input,
            mode: "ask",
            sending: false,
            share_failed: false,
            bound_session: None,
            blocked: None,
            local: None,
            editing: None,
            denying: None,
            _local_events: None,
            scroll: ScrollHandle::new(),
            _subs: subs,
        }
    }

    /// Where this conversation's task is: the copilot on the server only
    /// talks to the server (on this computer it has its own conversation);
    /// the AI section follows Settings → AI.
    fn backend(&self, cx: &App) -> Option<AiBackend> {
        if let Some(terminal) = &self.terminal {
            // The copilot uses the account of the terminal's host or session.
            // (Its output never goes to another account's server.)
            terminal
                .upgrade()
                .and_then(|t| t.read(cx).account_api(cx))
                .map(AiBackend::Server)
        } else {
            ai_backend(&self.model, cx)
        }
    }

    pub fn summary(&self) -> Option<&TaskSummary> {
        self.detail.as_ref().map(|d| &d.summary)
    }

    /// The task shown (a host's conversation after drilling down into a
    /// multi-host task).
    pub fn task_id(&self) -> Option<Id> {
        self.task
    }

    pub fn focus_input(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.focus(window, cx));
    }

    /// Changes the task shown (`None`: new conversation).
    pub fn set_task(&mut self, id: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        if self.local.as_ref().is_some_and(|c| Some(c.id) != id) {
            self.drop_local();
        }
        self.task = id;
        self.blocked = None;
        self.editing = None;
        self.denying = None;
        if id.is_none() {
            self.bound_session = None;
            self.share_failed = false;
        }
        self.detail = None;
        self.live.clear();
        self.live_text.clear();
        self.live_reasoning.clear();
        if let Some(id) = id {
            self.load(id, window, cx);
        }
        cx.notify();
    }

    /// Shows a task's saved state (without the live items it already has).
    fn apply_detail(&mut self, detail: TaskDetail, cx: &mut Context<Self>) {
        self.live_text.clear();
        self.live_reasoning.clear();
        self.live.retain(|item| match item {
            LiveItem::Tool { call_id, .. } => !detail.has_result(call_id),
            LiveItem::Notice(_) => false,
        });
        cx.emit(AiChatEvent::Summary(detail.summary.clone()));
        self.detail = Some(detail);
        self.follow();
    }

    fn load(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(conv) = self.local.clone().filter(|c| c.id == id) {
            if let Some(detail) = TaskDetail::from(&conv.view()) {
                self.apply_detail(detail, cx);
            }
            cx.notify();
            return;
        }
        let Some(backend) = self.backend(cx) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move { backend.get(id).await.map_err(|f| f.text) },
            move |this, res, window, cx| {
                if this.task != Some(id) {
                    return;
                }
                match res {
                    Ok(v) => {
                        if let Some(detail) = TaskDetail::from(&v) {
                            this.apply_detail(detail, cx);
                        }
                    }
                    Err(e) => ui::error(window, cx, t!("ai_chat.load_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    /// Scrolls to the bottom if it was already at the bottom (or nearly).
    fn follow(&self) {
        let offset = self.scroll.offset().y;
        let max = self.scroll.max_offset().y;
        if -offset >= max - px(80.) {
            self.scroll.scroll_to_bottom();
        }
    }

    /// Fills the input (suggestions).
    fn fill(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| {
            i.set_value(text, window, cx);
            i.focus(window, cx);
        });
    }

    /// Sends this text (e.g. "Continue" on a stopped task).
    pub fn send_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |i, cx| i.set_value(text, window, cx));
        self.send(window, cx);
    }

    /// Sends what was typed: creates the task (copilot without a task) or
    /// continues the conversation.
    pub fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.local_mode(cx) {
            self.send_local(window, cx);
            return;
        }
        // Back to the server after a conversation on this computer.
        if self.local.is_some() {
            self.set_task(None, window, cx);
        }
        let Some(backend) = self.backend(cx) else {
            return;
        };
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.sending {
            return;
        }
        let terminal = self.terminal.as_ref().and_then(|t| t.upgrade());
        // So that the AI can write to the terminal (and read it), it is shared
        // with the server, only for you, before asking (unless it already is).
        if !self.share_failed
            && let Some(t) = &terminal
            && let Some(share) = t.update(cx, |t, cx| t.copilot_share(cx))
        {
            self.sending = true;
            cx.notify();
            let t = t.downgrade();
            runtime::run_in(cx, window, share, move |this, res, window, cx| {
                this.sending = false;
                match (res, t.upgrade()) {
                    (Ok(share), Some(t)) => t.update(cx, |t, cx| t.adopt_share(share, cx)),
                    (res, t) => {
                        if let Err(e) = res {
                            tracing::warn!(error = %e, "the copilot could not share the terminal");
                        }
                        if let Some(t) = t {
                            t.update(cx, |t, cx| t.forget_share(cx));
                        }
                        this.share_failed = true;
                    }
                }
                this.send(window, cx);
            });
            return;
        }
        let context = terminal.as_ref().map(|t| t.read(cx).copilot_context(cx));
        // What the AI cannot read on its own (it is not a server session) is
        // attached: what the screen shows.
        let session = context.as_ref().and_then(|c| c.session_id);
        let prompt = match (&terminal, &context) {
            (Some(t), Some(c)) if c.session_id.is_none() => {
                with_screen(&c.label, &t.read(cx).screen_text(), &text)
            }
            // The AI knows another session (shared again or reconnected).
            _ if self.task.is_some() && session.is_some() && session != self.bound_session => {
                format!(
                    "<context>\nThe user's terminal is now session {}: use it with read_terminal and send_to_terminal.\n</context>\n\n{text}",
                    session.unwrap_or_default()
                )
            }
            _ => text.clone(),
        };
        if session.is_some() {
            self.bound_session = session;
        }
        self.sending = true;
        cx.notify();
        match self.task {
            Some(id) => runtime::run_in(
                cx,
                window,
                async move { Ok::<_, std::convert::Infallible>(backend.message(id, &prompt).await) },
                move |this, res, window, cx| {
                    this.sending = false;
                    match ai_result(res) {
                        Ok(_) => {
                            this.blocked = None;
                            this.input.update(cx, |i, cx| i.set_value("", window, cx));
                            this.load(id, window, cx);
                            this.scroll.scroll_to_bottom();
                        }
                        Err(f) if f.fix_in_settings => this.blocked = Some(f.text),
                        Err(f) => ui::error(window, cx, t!("ai_chat.send_failed", error = f.text)),
                    }
                    cx.notify();
                },
            ),
            None => {
                let mut body = json!({
                    "prompt": prompt,
                    "title": truncate(&text, 60),
                    "mode": self.mode,
                });
                if let Some(c) = &context {
                    // The server does not know a device-only host.
                    let synced =
                        c.host_id.filter(|id| {
                            self.model.read(cx).hosts.iter().any(|h| {
                                h.data.id == *id && h.meta.sync_mode != SyncMode::DeviceOnly
                            })
                        });
                    if let Some(id) = synced {
                        body["host_ids"] = json!([id]);
                    }
                    if let Some(sid) = c.session_id {
                        body["session_id"] = json!(sid);
                    }
                }
                runtime::run_in(
                    cx,
                    window,
                    async move { Ok::<_, std::convert::Infallible>(backend.create(body).await) },
                    |this, res, window, cx| {
                        this.sending = false;
                        match ai_result(res) {
                            Ok(v) => {
                                this.blocked = None;
                                this.input.update(cx, |i, cx| i.set_value("", window, cx));
                                if let Some(t) = TaskSummary::from(&v) {
                                    cx.emit(AiChatEvent::Summary(t.clone()));
                                    this.set_task(Some(t.id), window, cx);
                                }
                            }
                            Err(f) if f.fix_in_settings => this.blocked = Some(f.text),
                            Err(f) => {
                                ui::error(window, cx, t!("ai_chat.create_failed", error = f.text))
                            }
                        }
                        cx.notify();
                    },
                );
            }
        }
    }

    /// The copilot runs on this computer ("This computer" in Settings → AI).
    fn local_mode(&self, cx: &App) -> bool {
        self.terminal.is_some() && self.model.read(cx).ai_run_on() == RunOn::Local
    }

    /// The local conversation shown, if any.
    fn local_conversation(&self) -> Option<Arc<LocalConversation>> {
        self.local.clone().filter(|c| Some(c.id) == self.task)
    }

    fn drop_local(&mut self) {
        if let Some(c) = self.local.take() {
            c.cancel();
        }
        self._local_events = None;
    }

    /// Starts a conversation on this computer and listens to its events.
    fn new_local(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Arc<LocalConversation>> {
        let services = cx.try_global::<LocalAiGlobal>()?.0.clone();
        self.set_task(None, window, cx);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TaskEvent>();
        let mode = PermissionMode::parse(self.mode).unwrap_or_default();
        let conv = LocalConversation::new(services, mode, tx);
        let id = conv.id;
        self._local_events = Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = rx.recv().await {
                let alive = this
                    .update_in(cx, |this, window, cx| {
                        let v = json!({"type": "ai", "task_id": id.to_string(), "event": ev});
                        this.on_event(&v, window, cx);
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        }));
        self.local = Some(conv.clone());
        self.task = Some(id);
        Some(conv)
    }

    /// Sends the message to the copilot running on this computer.
    fn send_local(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() || self.sending {
            return;
        }
        let conv = match self.local_conversation() {
            Some(c) => c,
            None => match self.new_local(window, cx) {
                Some(c) => c,
                None => return,
            },
        };
        if conv.is_running() {
            ui::error(window, cx, t!("local_ai.error.busy"));
            return;
        }
        let terminal = self
            .terminal
            .as_ref()
            .and_then(|t| t.upgrade())
            .map(|t| t.read(cx).ai_id());
        let Some(store) = cx.try_global::<LocalAiGlobal>().map(|g| g.0.store.clone()) else {
            return;
        };
        let settings = self.model.read(cx).settings.ai.clone();
        self.sending = true;
        cx.notify();
        let id = conv.id;
        runtime::run_in(
            cx,
            window,
            async move {
                let res = match prepare(&store, &settings).await {
                    Ok(run) => conv.send(text, run, terminal).map_err(|e| (false, e)),
                    Err(e) => Err((e.fix_in_settings(), e.to_string())),
                };
                Ok::<_, std::convert::Infallible>(res)
            },
            move |this, res, window, cx| {
                this.sending = false;
                match res.map_err(|e| (false, e)).and_then(|r| r) {
                    Ok(()) => {
                        this.blocked = None;
                        this.input.update(cx, |i, cx| i.set_value("", window, cx));
                        this.load(id, window, cx);
                        this.scroll.scroll_to_bottom();
                    }
                    Err((true, e)) => this.blocked = Some(e),
                    Err((false, e)) => ui::error(window, cx, t!("ai_chat.send_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    fn decide(
        &mut self,
        approval: Id,
        decision: ApprovalDecision,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing = None;
        self.denying = None;
        if let Some(conv) = self.local_conversation() {
            if let Some(d) = self.detail.as_mut() {
                d.approvals.retain(|a| a.id != approval);
            }
            conv.decide(approval, decision.approve, decision.always);
            cx.notify();
            return;
        }
        let (Some(backend), Some(id)) = (self.backend(cx), self.task) else {
            return;
        };
        if let Some(d) = self.detail.as_mut() {
            d.approvals.retain(|a| a.id != approval);
        }
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                backend
                    .decide(id, approval, decision)
                    .await
                    .map_err(|f| f.text)
            },
            move |this, res, window, cx| {
                if let Err(e) = res {
                    ui::error(window, cx, t!("ai_chat.decision_failed", error = e));
                }
                this.load(id, window, cx);
            },
        );
    }

    /// "Edit and approve": the command (or plan) in an editor.
    fn start_edit(&mut self, approval: &Approval, window: &mut Window, cx: &mut Context<Self>) {
        let text = approval
            .preview
            .as_ref()
            .and_then(|p| p.plan.clone().or_else(|| p.command.clone()))
            .unwrap_or_default();
        let rows = if approval.is_plan() { (4, 16) } else { (1, 10) };
        let state = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(rows.0, rows.1)
                .default_value(text)
        });
        ui::focus_later(&state, window, cx);
        self.editing = Some((approval.id, state));
        self.denying = None;
        cx.notify();
    }

    /// "Deny…": asks for the reason the AI gets.
    fn start_deny(&mut self, approval: Id, window: &mut Window, cx: &mut Context<Self>) {
        let state = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("ai_chat.approval.reason_placeholder"))
        });
        ui::focus_later(&state, window, cx);
        self.denying = Some((approval, state));
        self.editing = None;
        cx.notify();
    }

    fn approve_edited(&mut self, approval: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, state)) = self.editing.clone().filter(|(id, _)| *id == approval) else {
            return;
        };
        let text = state.read(cx).value().to_string();
        if text.trim().is_empty() {
            ui::error(window, cx, t!("ai_chat.approval.edit_empty"));
            return;
        }
        self.decide(
            id,
            ApprovalDecision {
                approve: true,
                edited: Some(text),
                ..Default::default()
            },
            window,
            cx,
        );
    }

    fn deny_with_reason(&mut self, approval: Id, window: &mut Window, cx: &mut Context<Self>) {
        let reason = self
            .denying
            .as_ref()
            .filter(|(id, _)| *id == approval)
            .map(|(_, s)| s.read(cx).value().trim().to_string())
            .filter(|r| !r.is_empty());
        self.decide(approval, ApprovalDecision::deny(reason), window, cx);
    }

    /// Stop: cancels the task and the AI loses access to the terminal.
    pub fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.summary().is_some_and(TaskSummary::running) {
            self.cancel(window, cx);
        }
        self.drop_terminal_access(cx);
    }

    /// The copilot is closed: same as stopping.
    pub fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop(window, cx);
    }

    fn drop_terminal_access(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.terminal.as_ref().and_then(|t| t.upgrade()) {
            t.update(cx, |t, cx| t.stop_copilot_share(cx));
        }
    }

    /// Changes the permissions (of the task, or of the one to be created).
    fn set_mode(&mut self, mode: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = mode;
        if let Some(conv) = self.local_conversation() {
            conv.set_mode(PermissionMode::parse(mode).unwrap_or_default());
            if let Some(d) = self.detail.as_mut() {
                d.summary.mode = mode.to_string();
            }
            cx.notify();
            return;
        }
        let (Some(backend), Some(id)) = (self.backend(cx), self.task) else {
            cx.notify();
            return;
        };
        if let Some(d) = self.detail.as_mut() {
            d.summary.mode = mode.to_string();
        }
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { backend.set_mode(id, mode).await.map_err(|f| f.text) },
            move |this, res, window, cx| {
                if let Err(e) = res {
                    ui::error(window, cx, t!("ai_chat.mode_failed", error = e));
                }
                this.load(id, window, cx);
            },
        );
    }

    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(conv) = self.local_conversation() {
            conv.cancel();
            return;
        }
        let (Some(backend), Some(id)) = (self.backend(cx), self.task) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move { backend.cancel(id).await.map_err(|f| f.text) },
            move |this, res, window, cx| {
                if let Err(e) = res {
                    ui::error(window, cx, t!("ai_chat.cancel_failed", error = e));
                }
                this.load(id, window, cx);
            },
        );
    }

    /// Live event of a task (`{"type":"ai","task_id","event":{...}}`).
    fn on_event(&mut self, v: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task_id) = v["task_id"].as_str().and_then(|s| s.parse::<Id>().ok()) else {
            return;
        };
        let ev = &v["event"];
        if self.task != Some(task_id) {
            // A host of the multi-host task shown: refresh its table.
            let host = self
                .detail
                .as_ref()
                .is_some_and(|d| d.hosts.iter().any(|h| h.task_id == task_id));
            if host
                && matches!(
                    ev["type"].as_str(),
                    Some("finished" | "approval_requested" | "approval_decided" | "status")
                )
                && let Some(id) = self.task
            {
                self.load(id, window, cx);
            }
            return;
        }
        let s = |k: &str| ev[k].as_str().unwrap_or("").to_string();
        match ev["type"].as_str().unwrap_or("") {
            "text" => self.live_text.push_str(&s("delta")),
            "reasoning" => self.live_reasoning.push_str(&s("delta")),
            "reset" => self.live_text.clear(),
            "notice" => self.live.push(LiveItem::Notice(s("message"))),
            "tool_call" => self.live.push(LiveItem::Tool {
                call_id: s("call_id"),
                tool: s("tool"),
                summary: s("summary"),
                result: None,
            }),
            "tool_result" => {
                let id = s("call_id");
                for item in self.live.iter_mut() {
                    if let LiveItem::Tool {
                        call_id, result, ..
                    } = item
                        && *call_id == id
                    {
                        *result = Some((ev["ok"].as_bool().unwrap_or(false), s("output")));
                    }
                }
            }
            "approval_requested" => {
                if let (Some(d), Some(a)) =
                    (self.detail.as_mut(), Approval::from(ev, "approval_id"))
                    && !d.approvals.iter().any(|x| x.id == a.id)
                {
                    d.approvals.push(a);
                }
            }
            "approval_decided" => {
                if let (Some(d), Some(aid)) = (
                    self.detail.as_mut(),
                    ev["approval_id"]
                        .as_str()
                        .and_then(|s| s.parse::<Id>().ok()),
                ) {
                    d.approvals.retain(|a| a.id != aid);
                }
            }
            "status" => {
                if let Some(d) = self.detail.as_mut() {
                    d.summary.status = s("status");
                }
            }
            "message" | "finished" => self.load(task_id, window, cx),
            _ => {}
        }
        self.follow();
        cx.notify();
    }

    // ----- Rendering -----

    fn render_part(
        &self,
        key: String,
        role: &str,
        part: &Value,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.theme();
        let ty = part["type"].as_str().unwrap_or("");
        // If the call is still live (with its progress), it is not repeated.
        if ty == "tool_call"
            && let Some(id) = part["id"].as_str()
            && self
                .live
                .iter()
                .any(|l| matches!(l, LiveItem::Tool { call_id, .. } if call_id == id))
        {
            return None;
        }
        let text = |k: &str| part[k].as_str().unwrap_or("").to_string();
        Some(match (role, ty) {
            ("user", "text") => h_flex()
                .justify_end()
                .child(
                    div()
                        .max_w(px(560.))
                        .px_3()
                        .py_2()
                        .rounded(theme.radius_lg)
                        .bg(theme.primary)
                        .text_color(theme.primary_foreground)
                        .text_sm()
                        .child(strip_context(&text("text"))),
                )
                .into_any_element(),
            (_, "text") => div()
                .text_sm()
                .child(TextView::markdown(SharedString::from(key), text("text")).selectable(true))
                .into_any_element(),
            (_, "reasoning") => div()
                .pl_3()
                .border_l_2()
                .border_color(theme.border)
                .text_xs()
                .italic()
                .text_color(theme.muted_foreground)
                .child(truncate(&text("text"), 600))
                .into_any_element(),
            (_, "tool_call") => {
                let input = &part["input"];
                let what = input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| input.to_string());
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .child(
                        ui::icon(IconName::Wrench)
                            .size(px(12.))
                            .text_color(theme.info),
                    )
                    .child(div().font_medium().child(text("name")))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .font_family(ui::mono_family(cx))
                            .text_color(theme.muted_foreground)
                            .child(truncate(&what, 200)),
                    )
                    .into_any_element()
            }
            (_, "tool_result") => {
                let error = part["is_error"].as_bool().unwrap_or(false);
                output_block(&text("content"), !error, cx)
            }
            _ => return None,
        })
    }

    fn render_conversation(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Spinner::new())
                .into_any_element();
        };
        let running = detail.summary.running();
        let status = detail.summary.status.clone();
        let mut conversation = v_flex().gap_3();
        for (mi, m) in detail.messages.iter().enumerate() {
            let role = m["role"].as_str().unwrap_or("assistant").to_string();
            for (pi, part) in m["content"].as_array().into_iter().flatten().enumerate() {
                if let Some(el) = self.render_part(format!("ai-msg-{mi}-{pi}"), &role, part, cx) {
                    conversation = conversation.child(el);
                }
            }
        }
        let theme = cx.theme();
        if !self.live_reasoning.is_empty() {
            conversation = conversation.child(
                div()
                    .pl_3()
                    .border_l_2()
                    .border_color(theme.border)
                    .text_xs()
                    .italic()
                    .text_color(theme.muted_foreground)
                    .child(truncate(&self.live_reasoning, 800)),
            );
        }
        for item in &self.live {
            conversation = conversation.child(match item {
                LiveItem::Notice(msg) => div()
                    .text_xs()
                    .text_color(theme.warning)
                    .child(msg.clone())
                    .into_any_element(),
                LiveItem::Tool {
                    tool,
                    summary,
                    result,
                    ..
                } => v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .child(
                                ui::icon(IconName::Wrench)
                                    .size(px(12.))
                                    .text_color(theme.info),
                            )
                            .child(div().font_medium().child(tool.clone()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_color(theme.muted_foreground)
                                    .child(summary.clone()),
                            )
                            .when(result.is_none(), |this| this.child(Spinner::new().xsmall())),
                    )
                    .when_some(result.clone(), |this, (ok, out)| {
                        this.child(output_block(&out, ok, cx))
                    })
                    .into_any_element(),
            });
        }
        if !self.live_text.is_empty() {
            conversation = conversation.child(
                div()
                    .text_sm()
                    .child(TextView::markdown("ai-live", self.live_text.clone()).selectable(true)),
            );
        }
        if running && self.live_text.is_empty() && detail.approvals.is_empty() {
            conversation = conversation.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Spinner::new().xsmall())
                    .child(status_label(&status)),
            );
        }
        if let Some(e) = &detail.error {
            conversation =
                conversation.child(div().text_sm().text_color(theme.danger).child(e.clone()));
        }

        let approvals = detail.approvals.clone();
        let approvals_el = v_flex().gap_2().children(
            approvals
                .iter()
                .enumerate()
                .map(|(i, a)| self.render_approval(i, a, cx)),
        );
        let hosts_el = (!detail.hosts.is_empty()).then(|| self.render_hosts(&detail.hosts, cx));
        let back = detail.summary.parent_id.map(|parent| {
            Button::new("ai-back-to-hosts")
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::ArrowLeft))
                .label(t!("ai_chat.hosts.back"))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_task(Some(parent), window, cx)
                }))
        });
        let plan_chip = detail
            .summary
            .plan_first
            .then(|| match &detail.plan {
                Some(p) if p.approved && p.edited => t!("ai_chat.plan.approved_edited"),
                Some(p) if p.approved => t!("ai_chat.plan.approved"),
                _ => t!("ai_chat.plan.pending"),
            })
            .map(|text| h_flex().child(ui::pill(text, cx.theme().info)));

        let pad = if self.terminal.is_some() { 3. } else { 6. };
        div()
            .size_full()
            .child(
                v_flex()
                    .id("ai-conversation")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        v_flex()
                            .p(gpui::rems(pad * 0.25))
                            .gap_4()
                            .max_w(px(900.))
                            .children(back)
                            .children(plan_chip)
                            .child(conversation)
                            .children(hosts_el)
                            .child(approvals_el),
                    ),
            )
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }

    /// An approval: what it is about (the command with its risk and the
    /// classifier's reasons, the diff of a file, the plan) and the answers
    /// (approve, edit and approve, deny with a reason, approve all).
    fn render_approval(&self, i: usize, a: &Approval, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = a.id;
        let plan = a.is_plan();
        let preview = a.preview.clone();
        let risk = preview.as_ref().map(|p| p.risk);
        let border = match (plan, risk) {
            (true, _) => theme.primary,
            (_, Some(RiskLevel::High)) => theme.danger,
            (_, Some(RiskLevel::Low)) => theme.info,
            _ => theme.warning,
        };
        let mono = ui::mono_family(cx);
        let header = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .child(
                ui::icon(if plan {
                    IconName::ListChecks
                } else {
                    IconName::ShieldAlert
                })
                .size(px(16.))
                .text_color(border),
            )
            .child(div().font_semibold().text_sm().child(if plan {
                t!("ai_chat.approval.plan_title")
            } else {
                t!("ai_chat.approval.title")
            }))
            .when(!plan, |this| {
                this.child(ui::pill(a.tool.clone(), theme.info))
            })
            .when_some(risk.filter(|_| !plan), |this, r| {
                this.child(ui::pill(risk_label(r), risk_color(r, cx)))
            })
            .when_some(
                preview.as_ref().and_then(|p| p.host.clone()),
                |this, host| {
                    this.child(ui::pill(
                        t!("ai_chat.approval.on_host", host = host),
                        theme.muted_foreground,
                    ))
                },
            );
        let mut body = v_flex().gap_2();
        if let Some(e) = preview.as_ref().and_then(|p| p.explanation.clone()) {
            body = body.child(div().text_sm().text_color(theme.muted_foreground).child(e));
        }
        match &preview {
            Some(p) if p.kind == "plan" => {
                body = body.child(
                    div().text_sm().child(
                        TextView::markdown(
                            SharedString::from(format!("ai-plan-{id}")),
                            p.plan.clone().unwrap_or_default(),
                        )
                        .selectable(true),
                    ),
                );
            }
            Some(p) if p.kind == "file" => {
                body = body.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_wrap()
                        .child(
                            div()
                                .font_family(mono.clone())
                                .text_sm()
                                .child(p.path.clone().unwrap_or_default()),
                        )
                        .when(p.new_file, |this| {
                            this.child(ui::pill(t!("ai_chat.approval.new_file"), theme.info))
                        })
                        .when_some(p.added, |this, n| {
                            this.child(ui::pill(format!("+{n}"), theme.success))
                        })
                        .when_some(p.removed, |this, n| {
                            this.child(ui::pill(format!("−{n}"), theme.danger))
                        }),
                );
                match (&p.diff, &p.diff_error) {
                    (Some(d), _) if !d.is_empty() => {
                        body = body.child(diff_block(i, d, cx));
                        if p.diff_truncated {
                            body = body.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("ai_chat.approval.diff_truncated")),
                            );
                        }
                    }
                    (Some(_), _) => {
                        body = body.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("ai_chat.approval.no_changes")),
                        );
                    }
                    (None, Some(e)) => {
                        body = body.child(
                            div()
                                .text_xs()
                                .text_color(theme.warning)
                                .child(t!("ai_chat.approval.no_diff", error = e.clone())),
                        );
                    }
                    (None, None) => {}
                }
            }
            Some(p) if p.command.is_some() => {
                body = body.child(
                    div()
                        .p_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .font_family(mono.clone())
                        .text_xs()
                        .child(truncate(p.command.as_deref().unwrap_or(""), 4000)),
                );
            }
            _ => {
                body = body.child(div().text_sm().child(a.summary.clone())).child(
                    div()
                        .p_2()
                        .rounded(theme.radius)
                        .bg(theme.muted)
                        .font_family(mono.clone())
                        .text_xs()
                        .child(truncate(
                            &a.input
                                .get("command")
                                .and_then(|c| c.as_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| a.input.to_string()),
                            800,
                        )),
                );
            }
        }
        if let Some(p) = preview.as_ref().filter(|p| !p.reasons.is_empty()) {
            body = body.child(
                h_flex().gap_1().flex_wrap().children(
                    p.reasons
                        .iter()
                        .map(|r| ui::pill(reason_label(&r.code, &r.text), theme.muted_foreground)),
                ),
            );
        }

        let editing = self.editing.clone().filter(|(e, _)| *e == id);
        let denying = self.denying.clone().filter(|(e, _)| *e == id);
        let actions = if let Some((_, state)) = editing {
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if plan {
                            t!("ai_chat.approval.edit_plan_hint")
                        } else {
                            t!("ai_chat.approval.edit_hint")
                        }),
                )
                .child(div().font_family(mono.clone()).child(Textarea::new(&state)))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(("approve-edited", i))
                                .small()
                                .primary()
                                .label(t!("ai_chat.approval.approve_edited"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.approve_edited(id, window, cx)
                                })),
                        )
                        .child(
                            Button::new(("edit-cancel", i))
                                .small()
                                .ghost()
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.editing = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element()
        } else if let Some((_, state)) = denying {
            v_flex()
                .gap_2()
                .child(Input::new(&state))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(("deny-send", i))
                                .small()
                                .danger()
                                .label(t!("ai_chat.approval.deny"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.deny_with_reason(id, window, cx)
                                })),
                        )
                        .child(
                            Button::new(("deny-cancel", i))
                                .small()
                                .ghost()
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.denying = None;
                                    cx.notify();
                                })),
                        ),
                )
                .into_any_element()
        } else {
            let approval = a.clone();
            let editable = a.editable() && self.local_conversation().is_none();
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(
                    Button::new(("approve", i))
                        .small()
                        .primary()
                        .label(if plan {
                            t!("ai_chat.approval.approve_plan")
                        } else {
                            t!("ai_chat.approval.approve")
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.decide(id, ApprovalDecision::approve(), window, cx)
                        })),
                )
                .when(editable, |this| {
                    this.child(
                        Button::new(("edit", i))
                            .small()
                            .icon(ui::icon(IconName::Pencil))
                            .label(if plan {
                                t!("ai_chat.approval.edit_plan")
                            } else {
                                t!("ai_chat.approval.edit")
                            })
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.start_edit(&approval, window, cx)
                            })),
                    )
                })
                .child(
                    Button::new(("deny", i))
                        .small()
                        .danger()
                        .label(t!("ai_chat.approval.deny_with_reason"))
                        .tooltip(t!("ai_chat.approval.deny_hint"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            if this.local_conversation().is_some() {
                                this.decide(id, ApprovalDecision::deny(None), window, cx)
                            } else {
                                this.start_deny(id, window, cx)
                            }
                        })),
                )
                .when(!plan, |this| {
                    this.child(
                        Button::new(("always", i))
                            .small()
                            .ghost()
                            .label(t!("ai_chat.approval.always"))
                            .tooltip(t!("ai_chat.approval.always_hint"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.decide(
                                    id,
                                    ApprovalDecision {
                                        approve: true,
                                        always: true,
                                        ..Default::default()
                                    },
                                    window,
                                    cx,
                                )
                            })),
                    )
                })
                .into_any_element()
        };
        v_flex()
            .p_3()
            .gap_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(border)
            .bg(theme.secondary)
            .child(header)
            .child(body)
            .child(actions)
            .into_any_element()
    }

    /// Per-host table of a multi-host task; a row opens that host's
    /// conversation.
    fn render_hosts(&self, hosts: &[HostRun], cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let cell = |w: f32| {
            div()
                .w(px(w))
                .flex_shrink_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
        };
        let head = h_flex()
            .px_2()
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(cell(140.).child(t!("ai_chat.hosts.host")))
            .child(cell(130.).child(t!("ai_chat.hosts.status")))
            .child(div().flex_1().min_w_0().child(t!("ai_chat.hosts.summary")))
            .child(cell(70.).child(t!("ai_chat.hosts.duration")))
            .child(cell(70.).child(t!("ai_chat.hosts.cost")));
        let rows = hosts.iter().enumerate().map(|(i, h)| {
            let task = h.task_id;
            let status = h.status.as_str();
            let color = match status {
                "completed" => theme.success,
                "failed" => theme.danger,
                "waiting_approval" => theme.warning,
                "running" | "queued" => theme.info,
                _ => theme.muted_foreground,
            };
            let detail = h
                .summary
                .clone()
                .or_else(|| h.error.clone())
                .unwrap_or_default();
            h_flex()
                .id(("ai-host-row", i))
                .px_2()
                .py_1p5()
                .gap_2()
                .items_center()
                .text_sm()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(|st| st.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_task(Some(task), window, cx)
                }))
                .child(cell(140.).font_medium().child(h.label.clone()))
                .child(
                    cell(130.).child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(div().size(px(6.)).rounded_full().bg(color))
                            .child(div().text_xs().child(status_label(status)))
                            .when(h.pending_approvals > 0, |this| {
                                this.child(ui::pill(
                                    tn!("ai.pending_approvals", h.pending_approvals),
                                    theme.warning,
                                ))
                            }),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(if h.status.as_str() == "failed" {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(truncate(&detail, 200)),
                )
                .child(
                    cell(70.)
                        .text_xs()
                        .child(h.duration_ms.map(format_duration).unwrap_or_default()),
                )
                .child(cell(70.).text_xs().child(if h.cost_micros > 0 {
                    format!("{:.4} $", h.cost_micros as f64 / 1_000_000.0)
                } else {
                    "—".to_string()
                }))
        });
        v_flex()
            .p_2()
            .gap_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .px_2()
                    .pb_1()
                    .text_sm()
                    .font_semibold()
                    .child(tn!("ai_chat.hosts.title", hosts.len())),
            )
            .child(head)
            .children(rows)
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("ai_chat.hosts.hint")),
            )
            .into_any_element()
    }

    /// Copilot without a conversation: suggestions and permissions.
    fn render_start(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mode = self.mode;
        let mode_button =
            |id: &'static str, value: &'static str, hint: SharedString, cx: &mut Context<Self>| {
                Button::new(id)
                    .xsmall()
                    .label(mode_label(value))
                    .tooltip(hint)
                    .map(|b| {
                        if mode == value {
                            b.primary()
                        } else {
                            b.ghost()
                        }
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_mode(value, window, cx);
                    }))
            };
        v_flex()
            .id("copilot-start")
            .size_full()
            .p_4()
            .gap_4()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        ui::icon(IconName::Sparkles)
                            .size(px(22.))
                            .text_color(theme.primary),
                    )
                    .child(div().font_semibold().child(t!("ai_chat.start.title")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_chat.start.detail")),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .children(suggestions().into_iter().enumerate().map(|(i, text)| {
                        let fill = text.clone();
                        div()
                            .id(("copilot-suggestion", i))
                            .px_3()
                            .py_2()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .text_sm()
                            .cursor_pointer()
                            .hover(|st| st.bg(theme.secondary_hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.fill(&fill, window, cx)
                            }))
                            .child(text)
                    })),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_chat.permissions")),
                    )
                    .child(
                        h_flex().gap_1().flex_wrap().children(
                            MODES
                                .iter()
                                .map(|value| mode_button(value, value, mode_hint(value), cx)),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// Copilot header: context and actions.
    fn render_copilot_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let context = self
            .terminal
            .as_ref()
            .and_then(|t| t.upgrade())
            .map(|t| t.read(cx).copilot_context(cx));
        let has_task = self.task.is_some();
        // The actual mode of the task (it changes with "Always approve").
        let mode: &'static str = match self.summary().map(|t| t.mode.as_str()) {
            Some(m) => MODES.iter().copied().find(|v| *v == m).unwrap_or("ask"),
            None => self.mode,
        };
        let local = self.local_mode(cx);
        let theme = cx.theme();
        let (chip, note) = match &context {
            Some(c) if local => (c.label.clone(), Some(t!("ai_chat.copilot.note_local"))),
            Some(c) if c.session_id.is_some() => {
                (c.label.clone(), Some(t!("ai_chat.copilot.note_shared")))
            }
            Some(c) if self.share_failed => (
                c.label.clone(),
                Some(t!("ai_chat.copilot.note_share_failed")),
            ),
            Some(c) => (c.label.clone(), Some(t!("ai_chat.copilot.note_will_share"))),
            None => (t!("ai_chat.copilot.terminal_closed").to_string(), None),
        };
        v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        ui::icon(IconName::Sparkles)
                            .size(px(16.))
                            .text_color(theme.primary),
                    )
                    .child(
                        div()
                            .font_semibold()
                            .text_sm()
                            .child(t!("ai_chat.copilot.title")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(ui::pill(truncate(&chip, 40), theme.info)),
                    )
                    .child({
                        let me = cx.entity().downgrade();
                        Button::new("copilot-mode")
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::ShieldCheck))
                            .label(mode_label(mode))
                            .tooltip(t!("ai_chat.copilot.permissions_tooltip"))
                            .dropdown_menu(move |mut menu, _, _| {
                                for value in MODES {
                                    let me = me.clone();
                                    menu = menu.item(
                                        PopupMenuItem::new(mode_label(value))
                                            .checked(mode == value)
                                            .on_click(move |_, window, cx| {
                                                if let Some(me) = me.upgrade() {
                                                    me.update(cx, |c, cx| {
                                                        c.set_mode(value, window, cx)
                                                    });
                                                }
                                            }),
                                    );
                                }
                                menu
                            })
                    })
                    .when(has_task, |this| {
                        this.child(
                            Button::new("copilot-new")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::Plus))
                                .tooltip(t!("ai_chat.copilot.new_conversation"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.set_task(None, window, cx);
                                    this.focus_input(window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new("copilot-close")
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::X))
                            .tooltip(t!("common.close"))
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(AiChatEvent::Close)),
                            ),
                    ),
            )
            .when_some(note, |this, n| {
                this.child(div().text_xs().text_color(theme.muted_foreground).child(n))
            })
            .into_any_element()
    }
}

/// Output of a tool (monospace, truncated).
fn output_block(text: &str, ok: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .p_2()
        .rounded(theme.radius)
        .bg(theme.muted)
        .font_family(ui::mono_family(cx))
        .text_xs()
        .max_h(px(200.))
        .overflow_hidden()
        .text_color(if ok { theme.foreground } else { theme.danger })
        .child(truncate(text, 1500))
        .into_any_element()
}

fn risk_label(risk: RiskLevel) -> SharedString {
    match risk {
        RiskLevel::Low => t!("ai_chat.risk.low"),
        RiskLevel::Medium => t!("ai_chat.risk.medium"),
        RiskLevel::High => t!("ai_chat.risk.high"),
    }
}

fn risk_color(risk: RiskLevel, cx: &App) -> gpui::Hsla {
    let theme = cx.theme();
    match risk {
        RiskLevel::Low => theme.success,
        RiskLevel::Medium => theme.warning,
        RiskLevel::High => theme.danger,
    }
}

/// A reason of the risk classifier, in the user's language (its English
/// text for codes this version does not know).
fn reason_label(code: &str, text: &str) -> SharedString {
    match code {
        "pipe" => t!("ai_chat.reason.pipe"),
        "chain" => t!("ai_chat.reason.chain"),
        "redirect" => t!("ai_chat.reason.redirect"),
        "substitution" => t!("ai_chat.reason.substitution"),
        "sudo" => t!("ai_chat.reason.sudo"),
        "rm_rf" => t!("ai_chat.reason.rm_rf"),
        "delete" => t!("ai_chat.reason.delete"),
        "disk" => t!("ai_chat.reason.disk"),
        "reboot" => t!("ai_chat.reason.reboot"),
        "service" => t!("ai_chat.reason.service"),
        "packages" => t!("ai_chat.reason.packages"),
        "firewall" => t!("ai_chat.reason.firewall"),
        "permissions" => t!("ai_chat.reason.permissions"),
        "kill" => t!("ai_chat.reason.kill"),
        "users" => t!("ai_chat.reason.users"),
        "remote_script" => t!("ai_chat.reason.remote_script"),
        "containers" => t!("ai_chat.reason.containers"),
        "cron" => t!("ai_chat.reason.cron"),
        "git_history" => t!("ai_chat.reason.git_history"),
        "system_path" => t!(
            "ai_chat.reason.system_path",
            path = text.strip_prefix("writes to ").unwrap_or(text)
        ),
        "critical_file" => t!("ai_chat.reason.critical_file"),
        "redacted" => t!("ai_chat.reason.redacted"),
        "changes" => t!("ai_chat.reason.changes"),
        _ => SharedString::from(text.to_string()),
    }
}

/// `12 s`, `3 min 4 s`, `1 h 2 min`.
fn format_duration(ms: i64) -> String {
    let secs = (ms / 1000).max(0);
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {} s", s / 60, s % 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// A unified diff with its lines colored: added in green, removed in red,
/// hunk headers in blue.
fn diff_block(i: usize, diff: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let tint = |mut c: gpui::Hsla| {
        c.a = 0.12;
        c
    };
    let lines = diff.lines().take(1500).map(|line| {
        let (color, bg) = if line.starts_with("+++") || line.starts_with("---") {
            (theme.muted_foreground, None)
        } else if line.starts_with('+') {
            (theme.success, Some(tint(theme.success)))
        } else if line.starts_with('-') {
            (theme.danger, Some(tint(theme.danger)))
        } else if line.starts_with("@@") {
            (theme.info, None)
        } else {
            (theme.foreground, None)
        };
        div()
            .px_2()
            .whitespace_nowrap()
            .text_color(color)
            .when_some(bg, |d, bg| d.bg(bg))
            .child(if line.is_empty() {
                " ".to_string()
            } else {
                line.to_string()
            })
    });
    div()
        .id(("ai-diff", i))
        .max_h(px(360.))
        .overflow_scroll()
        .py_1()
        .rounded(theme.radius)
        .bg(theme.muted)
        .font_family(ui::mono_family(cx))
        .text_xs()
        .child(v_flex().min_w_full().children(lines))
        .into_any_element()
}

impl Render for AiChat {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let copilot = self.terminal.is_some();
        let me = cx.entity().downgrade();
        let open_settings = move |_: &mut Window, cx: &mut App| {
            let _ = me.update(cx, |_, cx| cx.emit(AiChatEvent::OpenAiSettings));
        };
        let body = if self.task.is_none() && copilot {
            self.render_start(cx)
        } else {
            self.render_conversation(cx)
        };
        let banner = self
            .blocked
            .as_deref()
            .map(|text| fix_banner(text, open_settings, cx));
        let header = copilot.then(|| self.render_copilot_header(cx));
        let running = self.summary().is_some_and(TaskSummary::running);
        let theme = cx.theme();
        v_flex()
            .size_full()
            .children(header)
            .child(div().flex_1().min_h_0().child(body))
            .when(copilot && running, |this| {
                this.child(
                    div()
                        .px_3()
                        .pt_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            Button::new("copilot-stop")
                                .w_full()
                                .danger()
                                .icon(ui::icon(IconName::CircleStop))
                                .label(t!("ai_chat.stop"))
                                .tooltip(t!("ai_chat.stop_hint"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.stop(window, cx)
                                })),
                        ),
                )
            })
            .when_some(banner, |this, banner| {
                this.child(div().px_3().pt_2().child(banner))
            })
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .when(!(copilot && running), |this| this.border_t_1())
                    .border_color(theme.border)
                    .child(div().flex_1().min_w_0().child(Input::new(&self.input)))
                    .child(
                        Button::new("ai-send")
                            .primary()
                            .icon(ui::icon(IconName::Send))
                            .when(!copilot, |b| b.label(t!("ai_chat.send")))
                            .tooltip(t!("ai_chat.send"))
                            .loading(self.sending)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.send(window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration(12_400), "12 s");
        assert_eq!(format_duration(184_000), "3 min 4 s");
        assert_eq!(format_duration(3_720_000), "1 h 2 min");
    }

    #[test]
    fn approvals_read_their_preview() {
        let v = json!({
            "id": "0192a8c4-7b6e-7c1a-9f3e-123456789abc",
            "tool": "run_command",
            "summary": "Run on web1: rm -rf /tmp/x",
            "input": {"host": "web1", "command": "rm -rf /tmp/x"},
            "preview": {"kind": "command", "command": "rm -rf /tmp/x", "risk": "high",
                        "reasons": [{"code": "rm_rf", "text": "deletes files recursively (rm -rf)"}],
                        "editable": true}
        });
        let a = Approval::from(&v, "id").unwrap();
        assert!(a.editable());
        assert_eq!(a.preview.as_ref().unwrap().risk, RiskLevel::High);
        // An older server sends no preview: no edits.
        let mut old = v.clone();
        old.as_object_mut().unwrap().remove("preview");
        assert!(!Approval::from(&old, "id").unwrap().editable());
        assert_eq!(
            reason_label("system_path", "writes to /etc"),
            t!("ai_chat.reason.system_path", path = "/etc")
        );
        assert_eq!(
            reason_label("future_code", "something new"),
            "something new"
        );
    }

    #[test]
    fn user_messages_hide_every_context_block() {
        let msg = "<context>\nengine\n</context>\n\n<context>\nscreen\n</context>\n\nwhat's up?";
        assert_eq!(strip_context(msg), "what's up?");
        assert_eq!(strip_context("hello"), "hello");
    }

    #[test]
    fn screen_goes_before_the_question_and_is_bounded() {
        let screen = "x".repeat(5000) + "\nerror: something";
        let p = with_screen("web-1", &screen, "what's up?");
        assert!(p.starts_with("<context>\nThe latest output shown by the user's terminal (web-1)"));
        assert!(p.ends_with("</context>\n\nwhat's up?"));
        assert!(p.contains("error: something"));
        assert!(p.len() < 4200);
        assert_eq!(with_screen("web-1", "  \n ", "hello"), "hello");
    }
}
