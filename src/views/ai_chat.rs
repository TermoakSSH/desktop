//! Live conversation with an AI task (text, reasoning, tools with their
//! output, and approvals). Used by the AI section and by the copilot, the
//! panel to the right of the terminal.
//!
//! The pieces it is drawn with (Markdown with code blocks, tool cards,
//! chips, skeletons...) are in [`super::ai_ui`].

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder, px, relative,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::text::TextView;
use gpui_component::{ActiveTheme, Selectable, Sizable, StyledExt, h_flex, v_flex};
use serde_json::{Value, json};
use termoak_ai::engine::TaskEvent;
use termoak_ai::policy::PermissionMode;
use termoak_ai::{ApprovalDecision, ApprovalPreview, HostRun, RiskLevel, TaskPlan};
use termoak_core::Id;
use termoak_core::model::SyncMode;

use super::ai_ui::status::{
    HostSort, Phase, ToolKind, ToolState, format_cost, format_duration, host_phase, model_name,
    progress, sort_hosts,
};
use super::ai_ui::{self as w, CodeActions, ToolView};
use crate::local_ai::copilot::{LocalAiGlobal, LocalConversation, prepare};
use crate::local_ai::tasks::{AiBackend, choose_backend};
use crate::local_ai::{AiSettings, LocalSource, RunOn};
use crate::runtime;
use crate::state::{AiFailure, AppModel, ModelEvent};
use crate::terminal::ai_assist::{ChipKind, ContextChip, context_block, typeable_command};
use crate::terminal::redact::redact;
use crate::terminal::{ExplainRequest, TerminalView};
use crate::ui::{self, IconName};

#[derive(Clone)]
pub struct TaskSummary {
    pub id: Id,
    pub title: String,
    pub status: String,
    pub mode: String,
    pub provider: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    pub cost_micros: i64,
    pub pending: usize,
    /// A multi-host task: one conversation per host.
    pub fan_out: bool,
    /// Hosts it is limited to.
    pub hosts: usize,
    pub host_ids: Vec<Id>,
    /// The group or tag it was started on.
    pub group_id: Option<Id>,
    pub tag: Option<String>,
    /// The multi-host task this host's conversation belongs to.
    pub parent_id: Option<Id>,
    /// "Plan before acting".
    pub plan_first: bool,
}

impl TaskSummary {
    pub fn from(v: &Value) -> Option<Self> {
        let host_ids: Vec<Id> = v["host_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()?.parse().ok()).collect())
            .unwrap_or_default();
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
            updated_at: v["updated_at"].as_i64().unwrap_or(0),
            finished_at: v["finished_at"].as_i64(),
            cost_micros: v["cost_micros"].as_i64().unwrap_or(0),
            pending: v["pending_approvals"]
                .as_array()
                .map(|a| a.len())
                .unwrap_or(0),
            fan_out: v["fan_out"].as_bool().unwrap_or(false),
            hosts: v["host_ids"].as_array().map(|a| a.len()).unwrap_or(0),
            host_ids,
            group_id: v["group_id"].as_str().and_then(|s| s.parse().ok()),
            tag: v["tag"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            parent_id: v["parent_id"].as_str().and_then(|s| s.parse().ok()),
            plan_first: v["plan_first"].as_bool().unwrap_or(false),
        })
    }

    pub fn running(&self) -> bool {
        self.phase().active()
    }

    pub fn phase(&self) -> Phase {
        Phase::of(&self.status, self.pending)
    }

    /// How long it ran (so far, while it runs).
    pub fn duration_ms(&self, now: i64) -> Option<i64> {
        if self.created_at <= 0 {
            return None;
        }
        match self.finished_at {
            Some(end) => Some(end - self.created_at),
            None if self.running() => Some(now - self.created_at),
            None if self.updated_at > self.created_at => Some(self.updated_at - self.created_at),
            None => None,
        }
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
    /// When each command or file write ran (by call id).
    steps: HashMap<String, i64>,
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
            steps: v["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| Some((s["call_id"].as_str()?.to_string(), s["at"].as_i64()?)))
                .collect(),
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

    /// The results of the saved tool calls, by call id.
    fn results(&self) -> HashMap<String, (bool, String)> {
        let mut out = HashMap::new();
        for m in &self.messages {
            for p in m["content"].as_array().into_iter().flatten() {
                if p["type"] == "tool_result"
                    && let Some(id) = p["id"].as_str()
                {
                    out.insert(
                        id.to_string(),
                        (
                            !p["is_error"].as_bool().unwrap_or(false),
                            p["content"].as_str().unwrap_or("").to_string(),
                        ),
                    );
                }
            }
        }
        out
    }
}

/// Live items not yet saved in the conversation.
enum LiveItem {
    Notice(String),
    Tool {
        call_id: String,
        tool: String,
        summary: String,
        input: Value,
        result: Option<(bool, String)>,
        duration_ms: Option<u64>,
        at: i64,
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

/// Permission modes, from the most to the least careful.
pub const MODES: [&str; 4] = ["read_only", "confirm", "ask", "auto"];

/// Explanation of a permission mode.
pub fn mode_hint(mode: &str) -> SharedString {
    match mode {
        "read_only" => t!("ai_chat.mode_hint.read_only"),
        "auto" => t!("ai_chat.mode_hint.auto"),
        "confirm" => t!("ai_chat.mode_hint.confirm"),
        _ => t!("ai_chat.mode_hint.ask"),
    }
}

/// Icon of a permission mode.
pub fn mode_icon(mode: &str) -> IconName {
    match mode {
        "read_only" => IconName::Eye,
        "auto" => IconName::Zap,
        "confirm" => IconName::ShieldCheck,
        _ => IconName::ShieldQuestionMark,
    }
}

/// What runs the AI on this computer, as Settings → AI says it.
pub fn local_model_label(s: &AiSettings) -> SharedString {
    let named = match s.local_source {
        Some(LocalSource::Agent) => s
            .local_agent
            .as_deref()
            .and_then(crate::local_ai::agents::kind)
            .map(|k| k.name.to_string()),
        _ => s
            .local_provider
            .as_deref()
            .map(crate::local_ai::keys::provider_label)
            .filter(|l| !l.is_empty())
            .map(str::to_string),
    };
    match named {
        Some(n) => t!("ai_ui.model.local_named", model = n),
        None => t!("ai_ui.model.local"),
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
    let screen = redact(tail(screen.trim_end(), 4000));
    if screen.trim().is_empty() {
        return text.to_string();
    }
    format!(
        "<context>\nThe latest output shown by the user's terminal ({label}):\n```\n{screen}\n```\n</context>\n\n{text}"
    )
}

/// Quick questions of the empty copilot (sent as they are, with the
/// terminal's context).
fn suggestions() -> [(IconName, SharedString); 4] {
    [
        (IconName::CircleAlert, t!("ai_chat.suggestion.why_failed")),
        (IconName::ScrollText, t!("ai_chat.suggestion.summarize")),
        (
            IconName::SquareTerminal,
            t!("ai_chat.suggestion.what_command"),
        ),
        (IconName::WandSparkles, t!("ai_chat.suggestion.fix")),
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
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(w::tint(theme.warning, 0.55))
        .bg(w::tint(theme.warning, 0.08))
        .child(
            ui::icon(IconName::KeyRound)
                .size(px(16.))
                .text_color(w::readable(theme.warning, cx)),
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
    input: Entity<TextareaState>,
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
    /// Loading the task failed (shown in place, with Retry).
    load_error: Option<String>,
    /// Copilot conversation running on this computer ("This computer" in
    /// Settings → AI); `task` is its id.
    local: Option<Arc<LocalConversation>>,
    /// Approval being edited before approving it (its command or plan).
    editing: Option<(Id, Entity<TextareaState>)>,
    /// Approval being denied, with the reason for the AI.
    denying: Option<(Id, Entity<InputState>)>,
    /// Enter in the reason (deny), Cmd/Ctrl+Enter in the edit (approve).
    _form_sub: Option<Subscription>,
    /// Focus of each approval card (Enter approves, E edits, D denies).
    approval_focus: HashMap<Id, FocusHandle>,
    /// Tool cards and reasoning blocks that are open.
    expanded: HashSet<String>,
    /// When each message of the task shown arrived (when known).
    stamps: Vec<Option<i64>>,
    stamped: Option<Id>,
    /// Order of the per-host table (column, ascending).
    host_sort: (HostSort, bool),
    _local_events: Option<Task<()>>,
    scroll: ScrollHandle,
    /// Terminal context sent with the next message (copilot), removable.
    chips: Vec<ContextChip>,
    /// Quick explanation asked from the terminal (a failed command, the
    /// selection), shown above the conversation.
    quick: Option<QuickAnswer>,
    /// Quick explanations asked so far (an old answer is ignored).
    quick_gen: u64,
    _subs: Vec<Subscription>,
}

/// A quick explanation in the copilot (`/ai/explain`, no task).
struct QuickAnswer {
    title: String,
    /// What was explained (as sent: secrets hidden).
    quoted: String,
    /// `None` while the AI answers.
    answer: Option<Result<String, String>>,
    provider: String,
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
        // Enter sends, Shift+Enter starts a new line.
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(placeholder)
        });
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
                if let InputEvent::PressEnter { shift: false, .. } = ev {
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
            load_error: None,
            local: None,
            editing: None,
            denying: None,
            _form_sub: None,
            approval_focus: HashMap::new(),
            expanded: HashSet::new(),
            stamps: Vec::new(),
            stamped: None,
            host_sort: (HostSort::Status, true),
            _local_events: None,
            scroll: ScrollHandle::new(),
            chips: Vec::new(),
            quick: None,
            quick_gen: 0,
            _subs: subs,
        }
    }

    /// Takes the terminal's context again (host, directory, last command,
    /// selection) as chips for the next message.
    pub fn load_terminal_context(&mut self, cx: &mut Context<Self>) {
        if let Some(t) = self.terminal.as_ref().and_then(|t| t.upgrade()) {
            self.chips = t.read(cx).copilot_chips(cx);
            cx.notify();
        }
    }

    /// The `<context>` block of the chips, for the next message.
    fn chips_block(&self, cx: &App) -> String {
        let label = self
            .terminal
            .as_ref()
            .and_then(|t| t.upgrade())
            .map(|t| t.read(cx).copilot_context(cx).label)
            .unwrap_or_default();
        context_block(&label, &self.chips)
    }

    /// Explains something from the terminal right here, with the quick
    /// assistant (and keeps the terminal's context for a follow-up).
    pub fn quick_explain(
        &mut self,
        req: ExplainRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let terminal = self.terminal.as_ref().and_then(|t| t.upgrade());
        let assist = terminal.as_ref().and_then(|t| t.read(cx).assist(cx));
        self.quick_gen += 1;
        let generation = self.quick_gen;
        self.quick = Some(QuickAnswer {
            title: req.title,
            quoted: req.text.clone(),
            answer: None,
            provider: String::new(),
        });
        cx.notify();
        let (Some(terminal), Some(assist)) = (terminal, assist) else {
            if let Some(q) = self.quick.as_mut() {
                q.answer = Some(Err(t!("terminal.ai.not_signed_in").to_string()));
            }
            return;
        };
        let context = terminal.read(cx).ai_context(cx);
        runtime::run_in(
            cx,
            window,
            assist.explain(req.text, Some(req.question), context),
            move |this, res, _, cx| {
                if this.quick_gen != generation {
                    return;
                }
                if let Some(q) = this.quick.as_mut() {
                    match res {
                        Ok(v) => {
                            q.answer = Some(Ok(v["answer"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| t!("terminal.ai.no_answer").to_string())));
                            q.provider = v["provider"].as_str().unwrap_or("").to_string();
                        }
                        Err(e) => q.answer = Some(Err(e)),
                    }
                }
                cx.notify();
            },
        );
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
        self.load_error = None;
        self.editing = None;
        self.denying = None;
        self.expanded.clear();
        self.approval_focus.clear();
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
        self.stamp(&detail);
        self.load_error = None;
        cx.emit(AiChatEvent::Summary(detail.summary.clone()));
        self.detail = Some(detail);
        self.follow();
    }

    /// Notes when the messages arrived: the first one when the task was
    /// created, the last one of a finished task when it finished, and the
    /// ones that arrive while it is shown, now. (Saved messages have no
    /// time of their own.)
    fn stamp(&mut self, detail: &TaskDetail) {
        let n = detail.messages.len();
        if self.stamped != Some(detail.summary.id) {
            self.stamped = Some(detail.summary.id);
            self.stamps = vec![None; n];
            if let Some(first) = self.stamps.first_mut()
                && detail.summary.created_at > 0
            {
                *first = Some(detail.summary.created_at);
            }
            if n > 1
                && let Some(end) = detail.summary.finished_at
            {
                self.stamps[n - 1] = Some(end);
            }
        } else {
            self.stamps.resize(n, Some(w::now_ms()));
        }
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
            move |this, res, _, cx| {
                if this.task != Some(id) {
                    return;
                }
                match res {
                    Ok(v) => {
                        if let Some(detail) = TaskDetail::from(&v) {
                            this.apply_detail(detail, cx);
                        }
                    }
                    Err(e) => this.load_error = Some(t!("ai_chat.load_failed", error = e).into()),
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

    /// Sends this text (a quick question, "Continue" on a stopped task, a
    /// code block to run).
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
        // The terminal's context the user left (secrets already hidden).
        let prompt = format!("{}{prompt}", self.chips_block(cx));
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
                            this.chips.clear();
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
                                this.chips.clear();
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
        // The terminal's context the user left (secrets already hidden).
        let text = format!("{}{text}", self.chips_block(cx));
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
                        this.chips.clear();
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
        self.approval_focus.remove(&approval);
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
        let id = approval.id;
        self._form_sub = Some(cx.subscribe_in(
            &state,
            window,
            move |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = ev
                {
                    this.approve_edited(id, window, cx);
                }
            },
        ));
        self.editing = Some((approval.id, state));
        self.denying = None;
        cx.notify();
    }

    /// "Deny…": asks for the reason the AI gets (on this computer it is
    /// denied at once: the local engine takes no reason).
    fn start_deny(&mut self, approval: Id, window: &mut Window, cx: &mut Context<Self>) {
        if self.local_conversation().is_some() {
            self.decide(approval, ApprovalDecision::deny(None), window, cx);
            return;
        }
        let state = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("ai_chat.approval.reason_placeholder"))
        });
        ui::focus_later(&state, window, cx);
        self._form_sub = Some(cx.subscribe_in(
            &state,
            window,
            move |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.deny_with_reason(approval, window, cx);
                }
            },
        ));
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

    /// Keys on a focused approval card: Enter approves, E edits, D denies.
    fn approval_key(
        &mut self,
        approval: &Approval,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only the card itself (not the edit or the reason being typed).
        let focused = self
            .approval_focus
            .get(&approval.id)
            .is_some_and(|h| h.is_focused(window));
        let m = &ev.keystroke.modifiers;
        if !focused || m.control || m.platform || m.alt || m.function {
            return;
        }
        match ev.keystroke.key.as_str() {
            "enter" => self.decide(approval.id, ApprovalDecision::approve(), window, cx),
            "e" if approval.editable() && self.local_conversation().is_none() => {
                self.start_edit(approval, window, cx)
            }
            "d" => self.start_deny(approval.id, window, cx),
            _ => return,
        }
        cx.stop_propagation();
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

    /// Insert on a code block: the command, made safe to type (one line,
    /// no control characters), at the terminal's prompt without Enter.
    fn insert_command(&mut self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.terminal.as_ref().and_then(|t| t.upgrade()) else {
            return;
        };
        let line = typeable_command(&command);
        if line.is_empty() {
            return;
        }
        t.update(cx, |t, cx| t.insert_text(&line, cx));
        let focus = t.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Run on a code block: in the copilot, after approving it (with its
    /// risk and the classifier's reasons), typed and run in the terminal;
    /// in the AI section, the task's AI is asked to run it (through the
    /// task's approvals).
    fn run_command(&mut self, command: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.terminal.as_ref().and_then(|t| t.upgrade()) else {
            let prompt = t!("ai_ui.code.run_prompt", command = command.trim());
            self.send_text(prompt.to_string(), window, cx);
            return;
        };
        let line = typeable_command(&command);
        if line.is_empty() {
            return;
        }
        let risk = termoak_ai::policy::classify_command(&line);
        let mut message = format!("{line}\n\n{}", risk_label(risk.level));
        if !risk.reasons.is_empty() {
            let reasons: Vec<String> = risk
                .reasons
                .iter()
                .map(|r| reason_label(&r.code, &r.text).to_string())
                .collect();
            message.push_str(&format!(": {}", reasons.join(", ")));
        }
        let terminal = t.downgrade();
        ui::confirm(
            window,
            cx,
            t!("ai_ui.run.title"),
            message,
            t!("ai_ui.run.ok"),
            risk.level == RiskLevel::High,
            move |window, cx| {
                if let Some(t) = terminal.upgrade() {
                    t.update(cx, |t, cx| t.run_line(&line, cx));
                    let focus = t.read(cx).focus_handle(cx);
                    window.focus(&focus, cx);
                }
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
                input: ev["input"].clone(),
                result: None,
                duration_ms: None,
                at: w::now_ms(),
            }),
            "tool_result" => {
                let id = s("call_id");
                for item in self.live.iter_mut() {
                    if let LiveItem::Tool {
                        call_id,
                        result,
                        duration_ms,
                        ..
                    } = item
                        && *call_id == id
                    {
                        *result = Some((ev["ok"].as_bool().unwrap_or(false), s("output")));
                        *duration_ms = ev["duration_ms"].as_u64();
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

    /// Opens or closes a tool card or a reasoning block.
    fn toggle(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_string());
        }
        cx.notify();
    }

    /// What code blocks can do here.
    fn code_actions(&self, cx: &Context<Self>) -> CodeActions {
        let me = cx.entity().downgrade();
        let run = {
            let me = me.clone();
            Rc::new(move |command: String, window: &mut Window, cx: &mut App| {
                let _ = me.update(cx, |c, cx| c.run_command(command, window, cx));
            }) as Rc<dyn Fn(String, &mut Window, &mut App)>
        };
        if self.terminal.is_some() {
            CodeActions {
                insert: Some(Rc::new(move |command, window, cx| {
                    let _ = me.update(cx, |c, cx| c.insert_command(command, window, cx));
                })),
                run: Some(run),
                run_tooltip: t!("ai_ui.code.run_terminal"),
            }
        } else {
            // Only while the task can still take a message.
            CodeActions {
                insert: None,
                run: self.task.is_some().then_some(run),
                run_tooltip: t!("ai_ui.code.run_task"),
            }
        }
    }
}

/// A turn of the conversation as it is drawn.
enum Turn {
    User {
        text: String,
        at: Option<i64>,
    },
    Assistant {
        parts: Vec<AnyElement>,
        at: Option<i64>,
        model: Option<String>,
    },
}

impl AiChat {
    // ----- Rendering -----

    /// A tool call (saved or live) as a card.
    #[allow(clippy::too_many_arguments)]
    fn tool_card(
        &self,
        call_id: &str,
        name: &str,
        input: &Value,
        summary: Option<&str>,
        result: Option<(bool, &str)>,
        duration_ms: Option<u64>,
        at: Option<i64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = format!("tool-{call_id}");
        let expanded = self.expanded.contains(&key);
        let toggle_key = key.clone();
        w::tool_card(
            ToolView {
                key: key.into(),
                name,
                input,
                summary,
                state: ToolState::of(result),
                output: result.map(|(_, o)| o),
                duration_ms,
                at,
                expanded,
            },
            cx.listener(move |this, _: &ClickEvent, _, cx| this.toggle(&toggle_key, cx)),
            cx,
        )
    }

    /// The AI's reasoning, folded under "Reasoning".
    fn reasoning_block(&self, key: String, text: &str, cx: &mut Context<Self>) -> AnyElement {
        let open = self.expanded.contains(&key);
        let theme = cx.theme();
        let toggle_key = key.clone();
        v_flex()
            .gap_1()
            .child(
                h_flex()
                    .id(SharedString::from(key))
                    .gap_1p5()
                    .items_center()
                    .cursor_pointer()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .hover(|s| s.text_color(theme.foreground))
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.toggle(&toggle_key, cx)
                        }),
                    )
                    .child(ui::icon(IconName::Brain).size(px(12.)))
                    .child(t!("ai_ui.reasoning"))
                    .child(
                        ui::icon(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .size(px(12.)),
                    ),
            )
            .when(open, |this| {
                this.child(
                    div()
                        .pl_3()
                        .border_l_2()
                        .border_color(theme.border)
                        .text_xs()
                        .italic()
                        .text_color(theme.muted_foreground)
                        .child(truncate(text, 4000)),
                )
            })
            .into_any_element()
    }

    /// A part of an assistant message.
    fn render_part(
        &self,
        key: String,
        part: &Value,
        results: &HashMap<String, (bool, String)>,
        steps: &HashMap<String, i64>,
        actions: &CodeActions,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let text = |k: &str| part[k].as_str().unwrap_or("").to_string();
        Some(match part["type"].as_str().unwrap_or("") {
            "text" => {
                let t = text("text");
                if t.trim().is_empty() {
                    return None;
                }
                w::rich_text(&key, &t, actions, false, cx)
            }
            "reasoning" => {
                let t = text("text");
                if t.trim().is_empty() {
                    return None;
                }
                self.reasoning_block(format!("{key}-reasoning"), &t, cx)
            }
            "tool_call" => {
                let id = part["id"].as_str().unwrap_or("");
                // If the call is still live (with its progress), it is not repeated.
                if self
                    .live
                    .iter()
                    .any(|l| matches!(l, LiveItem::Tool { call_id, .. } if call_id == id))
                {
                    return None;
                }
                let result = results.get(id).map(|(ok, out)| (*ok, out.as_str()));
                // A call without a result in a finished task never ran.
                let result =
                    result.or_else(|| self.summary().filter(|s| !s.running()).map(|_| (false, "")));
                self.tool_card(
                    id,
                    part["name"].as_str().unwrap_or(""),
                    &part["input"],
                    None,
                    result,
                    None,
                    steps.get(id).copied(),
                    cx,
                )
            }
            _ => return None,
        })
    }

    fn render_turn(&self, i: usize, turn: Turn, cx: &mut Context<Self>) -> AnyElement {
        let compact = self.terminal.is_some();
        let theme = cx.theme();
        let group = SharedString::from(format!("ai-turn-{i}"));
        let time = |at: Option<i64>| {
            at.map(|at| {
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .opacity(0.)
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .child(w::short_time(at))
            })
        };
        match turn {
            Turn::User { text, at } => {
                let bg = if theme.is_dark() {
                    w::tint(theme.primary, 0.22)
                } else {
                    w::tint(theme.primary, 0.10)
                };
                h_flex()
                    .group(group.clone())
                    .w_full()
                    .justify_end()
                    .items_end()
                    .gap_2()
                    .children(time(at))
                    .child(
                        div()
                            .max_w(px(if compact { 300. } else { 560. }))
                            .px_3p5()
                            .py_2()
                            .rounded(theme.radius_lg)
                            .bg(bg)
                            .text_color(theme.foreground)
                            .text_sm()
                            .child(text),
                    )
                    .into_any_element()
            }
            Turn::Assistant { parts, at, model } => {
                let header = h_flex()
                    .h(px(if compact { 20. } else { 26. }))
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .when(compact, |this| {
                        this.child(
                            ui::icon(IconName::Sparkles)
                                .size(px(12.))
                                .text_color(w::readable(theme.primary, cx)),
                        )
                    })
                    .child(
                        div()
                            .font_semibold()
                            .text_color(theme.foreground)
                            .child(t!("ai_ui.assistant")),
                    )
                    .when_some(model, |this, m| {
                        this.child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(theme.muted_foreground)
                                .child(m),
                        )
                    })
                    .children(time(at));
                let column = v_flex()
                    .flex_1()
                    .min_w_0()
                    .max_w(px(w::READING_WIDTH))
                    .gap_2()
                    .child(header)
                    .children(parts);
                h_flex()
                    .group(group)
                    .w_full()
                    .items_start()
                    .gap_3()
                    .when(!compact, |this| this.child(w::assistant_avatar(cx)))
                    .child(column)
                    .into_any_element()
            }
        }
    }

    fn render_conversation(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(detail) = &self.detail else {
            if let Some(e) = self.load_error.clone() {
                let me = cx.entity().downgrade();
                return div()
                    .p_4()
                    .child(w::error_banner(
                        "ai-load-retry",
                        e,
                        Some(Rc::new(move |window, cx| {
                            let _ = me.update(cx, |c, cx| {
                                if let Some(id) = c.task {
                                    c.load_error = None;
                                    c.load(id, window, cx);
                                    cx.notify();
                                }
                            });
                        })),
                        cx,
                    ))
                    .into_any_element();
            }
            return w::conversation_skeleton();
        };
        let running = detail.summary.running();
        let phase = detail.summary.phase();
        let actions = self.code_actions(cx);
        let results = detail.results();
        let steps = detail.steps.clone();
        let task_model = (!detail.summary.provider.is_empty())
            .then(|| model_name(&detail.summary.provider).to_string());

        // Messages into turns: the tool results the user "sends" belong to
        // the assistant's turn (they are drawn in its tool cards).
        let mut turns: Vec<Turn> = Vec::new();
        for (mi, m) in detail.messages.iter().enumerate() {
            let at = self.stamps.get(mi).copied().flatten();
            let parts: Vec<&Value> = m["content"].as_array().into_iter().flatten().collect();
            if m["role"] == "user" {
                let text: Vec<String> = parts
                    .iter()
                    .filter(|p| p["type"] == "text")
                    .map(|p| strip_context(p["text"].as_str().unwrap_or("")))
                    .filter(|t| !t.is_empty())
                    .collect();
                if !text.is_empty() {
                    turns.push(Turn::User {
                        text: text.join("\n\n"),
                        at,
                    });
                }
                continue;
            }
            let model = m["provider"]
                .as_str()
                .map(|p| model_name(p).to_string())
                .or_else(|| task_model.clone());
            let elements: Vec<AnyElement> = parts
                .iter()
                .enumerate()
                .filter_map(|(pi, part)| {
                    self.render_part(
                        format!("ai-msg-{mi}-{pi}"),
                        part,
                        &results,
                        &steps,
                        &actions,
                        cx,
                    )
                })
                .collect();
            match turns.last_mut() {
                Some(Turn::Assistant { parts, .. }) => parts.extend(elements),
                _ => turns.push(Turn::Assistant {
                    parts: elements,
                    at,
                    model,
                }),
            }
        }

        // What is arriving now.
        let mut live: Vec<AnyElement> = Vec::new();
        let notice_color = w::readable(cx.theme().warning, cx);
        for item in &self.live {
            live.push(match item {
                LiveItem::Notice(msg) => h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_xs()
                    .text_color(notice_color)
                    .child(ui::icon(IconName::Info).size(px(12.)))
                    .child(msg.clone())
                    .into_any_element(),
                LiveItem::Tool {
                    call_id,
                    tool,
                    summary,
                    input,
                    result,
                    duration_ms,
                    at,
                } => self.tool_card(
                    call_id,
                    tool,
                    input,
                    Some(summary.as_str()),
                    result.as_ref().map(|(ok, out)| (*ok, out.as_str())),
                    *duration_ms,
                    Some(*at),
                    cx,
                ),
            });
        }
        if !self.live_text.is_empty() {
            live.push(w::rich_text("ai-live", &self.live_text, &actions, true, cx));
        } else if running && detail.approvals.is_empty() {
            let label = match phase {
                Phase::Queued => t!("ai_chat.status.queued"),
                _ if !self.live_reasoning.is_empty() => t!("ai_ui.thinking"),
                _ => t!("ai_ui.working"),
            };
            live.push(w::typing_indicator("ai-typing", label, cx));
            if !self.live_reasoning.is_empty() {
                let theme = cx.theme();
                live.push(
                    div()
                        .pl_3()
                        .border_l_2()
                        .border_color(theme.border)
                        .text_xs()
                        .italic()
                        .text_color(theme.muted_foreground)
                        .line_clamp(3)
                        .child(tail(&self.live_reasoning, 400).to_string())
                        .into_any_element(),
                );
            }
        }
        if !live.is_empty() {
            match turns.last_mut() {
                Some(Turn::Assistant { parts, .. }) => parts.extend(live),
                _ => turns.push(Turn::Assistant {
                    parts: live,
                    at: None,
                    model: task_model.clone(),
                }),
            }
        }

        let failed = detail.error.clone().map(|e| {
            let me = cx.entity().downgrade();
            // Retry on a failed task: it continues where it stopped.
            let retry: Option<Rc<dyn Fn(&mut Window, &mut App)>> =
                (!running && self.terminal.is_none()).then(|| {
                    Rc::new(move |window: &mut Window, cx: &mut App| {
                        let _ = me.update(cx, |c, cx| {
                            c.send_text(t!("ai.continue_prompt").to_string(), window, cx)
                        });
                    }) as Rc<dyn Fn(&mut Window, &mut App)>
                });
            w::error_banner("ai-task-retry", ui::capitalize(&e), retry, cx)
        });

        let approvals = detail.approvals.clone();
        let approvals_el = v_flex().gap_3().children(
            approvals
                .iter()
                .enumerate()
                .map(|(i, a)| self.render_approval(i, a, window, cx)),
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
            .map(|text| {
                h_flex().child(w::chip(
                    Some(IconName::ListChecks),
                    text,
                    cx.theme().info,
                    cx,
                ))
            });

        let turns: Vec<AnyElement> = turns
            .into_iter()
            .enumerate()
            .map(|(i, t)| self.render_turn(i, t, cx))
            .collect();
        let compact = self.terminal.is_some();
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
                            .w_full()
                            .when(compact, |this| this.p_3().gap_4())
                            .when(!compact, |this| this.px_6().py_5().gap_5())
                            .max_w(px(w::READING_WIDTH + 80.))
                            .children(back)
                            .children(plan_chip)
                            .children(hosts_el)
                            .children(turns)
                            .children(failed)
                            .child(approvals_el),
                    ),
            )
            .vertical_scrollbar(&self.scroll)
            .into_any_element()
    }

    /// An approval: what it is about (the command with its risk and the
    /// classifier's reasons, the diff of a file, the plan) and the answers
    /// (approve, edit and approve, deny with a reason, approve all). Once
    /// the card has the focus: Enter approves, E edits, D denies.
    fn render_approval(
        &self,
        i: usize,
        a: &Approval,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let id = a.id;
        let plan = a.is_plan();
        let preview = a.preview.clone();
        let risk = preview.as_ref().map(|p| p.risk).filter(|_| !plan);
        let amber = theme.warning;
        let compact = self.terminal.is_some();
        let focus = self.approval_focus.get(&id).cloned();
        let focused = focus.as_ref().is_some_and(|h| h.is_focused(window));
        let kind = ToolKind::of(&a.tool);
        let host = preview.as_ref().and_then(|p| p.host.clone());

        let header = h_flex()
            .px_4()
            .pt_3()
            .gap_3()
            .items_center()
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(30.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(w::tint(amber, 0.18))
                    .child(
                        ui::icon(if plan {
                            IconName::ListChecks
                        } else {
                            IconName::ShieldAlert
                        })
                        .size(px(16.))
                        .text_color(w::readable(amber, cx)),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().text_sm().font_semibold().child(if plan {
                        t!("ai_chat.approval.plan_title")
                    } else {
                        t!("ai_chat.approval.title")
                    }))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .flex_wrap()
                            .items_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .when(!plan, |this| {
                                this.child(ui::icon(kind.icon()).size(px(12.)))
                                    .child(kind.label(&a.tool))
                            })
                            .when_some(host, |this, host| {
                                this.child(w::neutral_chip(Some(IconName::Server), host, cx))
                            }),
                    ),
            )
            .when_some(risk, |this, r| {
                this.child(w::chip(
                    Some(match r {
                        RiskLevel::Low => IconName::ShieldCheck,
                        RiskLevel::Medium => IconName::ShieldAlert,
                        RiskLevel::High => IconName::TriangleAlert,
                    }),
                    risk_label(r),
                    risk_color(r, cx),
                    cx,
                ))
            });

        let mut body = v_flex().px_4().gap_3();
        if let Some(e) = preview.as_ref().and_then(|p| p.explanation.clone()) {
            body = body.child(div().text_sm().child(e));
        }
        let key = format!("ai-approval-{id}");
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
                            ui::icon(IconName::FilePen)
                                .size(px(14.))
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .font_family(ui::mono_family(cx))
                                .text_sm()
                                .child(p.path.clone().unwrap_or_default()),
                        )
                        .when(p.new_file, |this| {
                            this.child(w::chip(
                                None,
                                t!("ai_chat.approval.new_file"),
                                theme.info,
                                cx,
                            ))
                        })
                        .when_some(p.added, |this, n| {
                            this.child(w::chip(None, format!("+{n}"), theme.success, cx))
                        })
                        .when_some(p.removed, |this, n| {
                            this.child(w::chip(None, format!("−{n}"), theme.danger, cx))
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
                                .text_color(w::readable(theme.warning, cx))
                                .child(t!("ai_chat.approval.no_diff", error = e.clone())),
                        );
                    }
                    (None, None) => {}
                }
            }
            Some(p) if p.command.is_some() => {
                body = body.child(w::code_line(
                    &key,
                    &truncate(p.command.as_deref().unwrap_or(""), 4000),
                    true,
                    cx,
                ));
            }
            _ => {
                let what = a
                    .input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| a.input.to_string());
                body = body
                    .child(div().text_sm().child(a.summary.clone()))
                    .child(w::code_line(
                        &key,
                        &truncate(&what, 800),
                        kind.is_command(),
                        cx,
                    ));
            }
        }
        if let Some(p) = preview.as_ref().filter(|p| !p.reasons.is_empty()) {
            body = body.child(
                h_flex().gap_1().flex_wrap().children(
                    p.reasons
                        .iter()
                        .map(|r| w::neutral_chip(None, reason_label(&r.code, &r.text), cx)),
                ),
            );
        }

        let editing = self.editing.clone().filter(|(e, _)| *e == id);
        let denying = self.denying.clone().filter(|(e, _)| *e == id);
        let footer = if let Some((_, state)) = editing {
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
                .child(
                    div()
                        .font_family(ui::mono_family(cx))
                        .child(Textarea::new(&state)),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .justify_end()
                        .child(
                            Button::new(("edit-cancel", i))
                                .small()
                                .ghost()
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.editing = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("approve-edited", i))
                                .small()
                                .primary()
                                .icon(ui::icon(IconName::Check))
                                .label(t!("ai_chat.approval.approve_edited"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.approve_edited(id, window, cx)
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
                        .justify_end()
                        .child(
                            Button::new(("deny-cancel", i))
                                .small()
                                .ghost()
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.denying = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(("deny-send", i))
                                .small()
                                .danger()
                                .icon(ui::icon(IconName::Ban))
                                .label(t!("ai_chat.approval.deny"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.deny_with_reason(id, window, cx)
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
                .items_center()
                .child(
                    Button::new(("deny", i))
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::Ban))
                        .label(t!("ai_chat.approval.deny_with_reason"))
                        .tooltip(t!("ai_chat.approval.deny_hint"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.start_deny(id, window, cx)
                        })),
                )
                .when(!plan, |this| {
                    this.child(
                        Button::new(("always", i))
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::CheckCheck))
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
                .child(div().flex_1())
                .when(editable, |this| {
                    this.child(
                        Button::new(("edit", i))
                            .small()
                            .outline()
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
                    Button::new(("approve", i))
                        .small()
                        .primary()
                        .icon(ui::icon(IconName::Check))
                        .label(if plan {
                            t!("ai_chat.approval.approve_plan")
                        } else {
                            t!("ai_chat.approval.approve")
                        })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.decide(id, ApprovalDecision::approve(), window, cx)
                        })),
                )
                .into_any_element()
        };
        let hints = (focused && self.editing.is_none() && self.denying.is_none()).then(|| {
            let mut keys = vec![("↵", t!("ai_chat.approval.approve"))];
            if a.editable() && self.local_conversation().is_none() {
                keys.push(("E", t!("ai_ui.key.edit")));
            }
            keys.push(("D", t!("ai_chat.approval.deny")));
            w::key_hints(&keys, cx)
        });
        let key_approval = a.clone();
        v_flex()
            .id(("ai-approval", i))
            .when_some(focus, |this, h| this.track_focus(&h))
            .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                this.approval_key(&key_approval, ev, window, cx)
            }))
            .w_full()
            .when(!compact, |this| this.max_w(px(w::READING_WIDTH)))
            .gap_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if focused {
                theme.ring
            } else {
                w::tint(amber, 0.6)
            })
            .bg(w::mix(
                theme.background,
                amber,
                if theme.is_dark() { 0.07 } else { 0.06 },
            ))
            .shadow_sm()
            .child(header)
            .child(body)
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(w::tint(amber, 0.3))
                    .child(footer)
                    .children(hints),
            )
            .into_any_element()
    }

    /// Per-host table of a multi-host task: its progress, sortable by host,
    /// status and time; a row opens that host's conversation.
    fn render_hosts(&self, hosts: &[HostRun], cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (by, asc) = self.host_sort;
        let mut rows_data = hosts.to_vec();
        sort_hosts(&mut rows_data, by, asc);
        let (done, total) = progress(hosts);
        let count = |p: Phase| hosts.iter().filter(|h| host_phase(h) == p).count();
        let (failed, waiting, working) = (
            count(Phase::Failed),
            count(Phase::NeedsApproval),
            count(Phase::Running) + count(Phase::Queued),
        );
        let cell = |w: f32| {
            div()
                .w(px(w))
                .flex_shrink_0()
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
        };
        let sort_head = |id: &'static str,
                         label: SharedString,
                         col: HostSort,
                         width: Option<f32>,
                         cx: &Context<Self>| {
            let active = by == col;
            h_flex()
                .id(id)
                .when_some(width, |d, w| d.w(px(w)).flex_shrink_0())
                .when(width.is_none(), |d| d.flex_1().min_w_0())
                .gap_1()
                .items_center()
                .cursor_pointer()
                .hover(|s| s.text_color(cx.theme().foreground))
                .when(active, |d| d.text_color(cx.theme().foreground))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.host_sort = if this.host_sort.0 == col {
                        (col, !this.host_sort.1)
                    } else {
                        (col, true)
                    };
                    cx.notify();
                }))
                .child(label)
                .child(
                    ui::icon(match (active, asc) {
                        (false, _) => IconName::ArrowUpDown,
                        (true, true) => IconName::ArrowUp,
                        (true, false) => IconName::ArrowDown,
                    })
                    .size(px(12.))
                    .when(!active, |i| i.text_color(cx.theme().muted_foreground)),
                )
        };
        let head = h_flex()
            .px_4()
            .py_2()
            .gap_3()
            .bg(theme.muted)
            .text_xs()
            .font_medium()
            .text_color(theme.muted_foreground)
            .child(sort_head(
                "ai-hosts-sort-host",
                t!("ai_chat.hosts.host"),
                HostSort::Host,
                Some(150.),
                cx,
            ))
            .child(sort_head(
                "ai-hosts-sort-status",
                t!("ai_chat.hosts.status"),
                HostSort::Status,
                Some(170.),
                cx,
            ))
            .child(div().flex_1().min_w_0().child(t!("ai_chat.hosts.summary")))
            .child(sort_head(
                "ai-hosts-sort-time",
                t!("ai_chat.hosts.duration"),
                HostSort::Duration,
                Some(80.),
                cx,
            ))
            .child(cell(64.).child(t!("ai_chat.hosts.cost")))
            .child(div().w(px(14.)));
        let theme = cx.theme();
        let rows = rows_data.iter().enumerate().map(|(i, h)| {
            let task = h.task_id;
            let phase = host_phase(h);
            let detail = h
                .summary
                .clone()
                .or_else(|| h.error.clone())
                .unwrap_or_default();
            h_flex()
                .id(("ai-host-row", i))
                .px_4()
                .py_2()
                .gap_3()
                .items_center()
                .text_sm()
                .border_t_1()
                .border_color(theme.border)
                .cursor_pointer()
                .hover(|st| st.bg(theme.list_hover))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_task(Some(task), window, cx)
                }))
                .child(
                    h_flex()
                        .w(px(150.))
                        .flex_shrink_0()
                        .gap_2()
                        .items_center()
                        .child(
                            ui::icon(IconName::Server)
                                .size(px(14.))
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .font_medium()
                                .child(h.label.clone()),
                        ),
                )
                .child(
                    h_flex()
                        .w(px(170.))
                        .flex_shrink_0()
                        .gap_1()
                        .items_center()
                        .child(w::status_chip(phase, cx))
                        .when(h.pending_approvals > 1, |this| {
                            this.child(w::chip(
                                None,
                                h.pending_approvals.to_string(),
                                theme.warning,
                                cx,
                            ))
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_xs()
                        .text_color(if phase == Phase::Failed {
                            w::readable(theme.danger, cx)
                        } else {
                            theme.muted_foreground
                        })
                        .child(truncate(&detail, 200)),
                )
                .child(
                    cell(80.)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(h.duration_ms.map(format_duration).unwrap_or_default()),
                )
                .child(
                    cell(64.)
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format_cost(h.cost_micros).unwrap_or_else(|| "—".into())),
                )
                .child(
                    ui::icon(IconName::ChevronRight)
                        .size(px(14.))
                        .text_color(theme.muted_foreground),
                )
        });
        let ratio = if total == 0 {
            0.
        } else {
            done as f32 / total as f32
        };
        v_flex()
            .w_full()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .overflow_hidden()
            .child(
                v_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .flex_wrap()
                            .child(
                                div()
                                    .text_sm()
                                    .font_semibold()
                                    .child(tn!("ai_chat.hosts.title", total)),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(t!(
                                "ai_ui.hosts.progress",
                                done = done,
                                total = total
                            )))
                            .child(div().flex_1())
                            .when(working > 0, |this| {
                                this.child(w::chip(
                                    Some(IconName::LoaderCircle),
                                    t!("ai_ui.hosts.working", count = working),
                                    theme.info,
                                    cx,
                                ))
                            })
                            .when(waiting > 0, |this| {
                                this.child(w::chip(
                                    Some(IconName::ShieldAlert),
                                    t!("ai_ui.hosts.waiting", count = waiting),
                                    theme.warning,
                                    cx,
                                ))
                            })
                            .when(failed > 0, |this| {
                                this.child(w::chip(
                                    Some(IconName::CircleX),
                                    t!("ai_ui.hosts.failed", count = failed),
                                    theme.danger,
                                    cx,
                                ))
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .h(px(6.))
                            .rounded_full()
                            .bg(theme.muted)
                            .child(div().h_full().w(relative(ratio)).rounded_full().bg(
                                if failed > 0 {
                                    theme.warning
                                } else {
                                    theme.success
                                },
                            )),
                    ),
            )
            .child(
                div()
                    .id("ai-hosts-table")
                    .w_full()
                    .overflow_x_scroll()
                    .child(v_flex().min_w(px(640.)).child(head).children(rows)),
            )
            .child(
                div()
                    .px_4()
                    .py_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("ai_chat.hosts.hint")),
            )
            .into_any_element()
    }

    /// Copilot without a conversation: quick questions, what the AI can do
    /// here and the permissions.
    fn render_start(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mode = self.mode;
        let note = self.copilot_note(cx);
        let theme_primary = theme.primary;
        v_flex()
            .id("copilot-start")
            .size_full()
            .p_4()
            .gap_5()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .size(px(36.))
                            .rounded_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .bg(w::tint(theme_primary, 0.16))
                            .child(
                                ui::icon(IconName::Sparkles)
                                    .size(px(18.))
                                    .text_color(w::readable(theme_primary, cx)),
                            ),
                    )
                    .child(div().font_semibold().child(t!("ai_chat.start.title")))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_chat.start.detail")),
                    )
                    .when_some(note, |this, note| {
                        this.child(
                            h_flex()
                                .gap_1p5()
                                .items_start()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(
                                    ui::icon(IconName::Info)
                                        .size(px(12.))
                                        .mt(px(2.))
                                        .flex_shrink_0(),
                                )
                                .child(div().flex_1().min_w_0().child(note)),
                        )
                    }),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_ui.quick_questions")),
                    )
                    .child(
                        v_flex()
                            .gap_1p5()
                            .children(suggestions().into_iter().enumerate().map(
                                |(i, (icon, text))| {
                                    let send = text.to_string();
                                    h_flex()
                                        .id(("copilot-suggestion", i))
                                        .w_full()
                                        .gap_2()
                                        .items_center()
                                        .px_3()
                                        .py_2()
                                        .rounded(theme.radius)
                                        .border_1()
                                        .border_color(theme.border)
                                        .bg(theme.secondary)
                                        .text_sm()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(theme.secondary_hover))
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, window, cx| {
                                                this.send_text(send.clone(), window, cx)
                                            },
                                        ))
                                        .child(
                                            ui::icon(icon)
                                                .size(px(14.))
                                                .text_color(theme.muted_foreground),
                                        )
                                        .child(div().flex_1().min_w_0().child(text))
                                        .child(
                                            ui::icon(IconName::ArrowRight)
                                                .size(px(12.))
                                                .text_color(theme.muted_foreground),
                                        )
                                },
                            )),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_chat.permissions")),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(MODES.iter().map(|&value| {
                                Button::new(SharedString::from(format!("copilot-mode-{value}")))
                                    .xsmall()
                                    .icon(ui::icon(mode_icon(value)))
                                    .label(mode_label(value))
                                    .tooltip(mode_hint(value))
                                    .selected(mode == value)
                                    .map(|b| {
                                        if mode == value {
                                            b.primary()
                                        } else {
                                            b.ghost()
                                        }
                                    })
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.set_mode(value, window, cx);
                                        },
                                    ))
                            })),
                    ),
            )
            .into_any_element()
    }

    /// What the copilot will do with this terminal (shared with the
    /// server, local, could not share).
    fn copilot_note(&self, cx: &App) -> Option<SharedString> {
        let context = self
            .terminal
            .as_ref()
            .and_then(|t| t.upgrade())
            .map(|t| t.read(cx).copilot_context(cx))?;
        Some(if self.local_mode(cx) {
            t!("ai_chat.copilot.note_local")
        } else if context.session_id.is_some() {
            t!("ai_chat.copilot.note_shared")
        } else if self.share_failed {
            t!("ai_chat.copilot.note_share_failed")
        } else {
            t!("ai_chat.copilot.note_will_share")
        })
    }

    /// The model answering (of the task, or the one chosen on this
    /// computer).
    fn model_label(&self, cx: &App) -> Option<SharedString> {
        if let Some(p) = self.summary().map(|s| s.provider.as_str())
            && !p.is_empty()
        {
            return Some(model_name(p).to_string().into());
        }
        self.local_mode(cx)
            .then(|| local_model_label(&self.model.read(cx).settings.ai))
    }

    /// Copilot header: the host, the model, the permissions and the
    /// actions, in one row.
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
        let note = self.copilot_note(cx);
        let model = self.model_label(cx);
        let theme = cx.theme();
        let host = context
            .as_ref()
            .map(|c| c.label.clone())
            .unwrap_or_else(|| t!("ai_chat.copilot.terminal_closed").to_string());
        let tip: SharedString = note.unwrap_or_else(|| host.clone().into());
        h_flex()
            .h(px(44.))
            .flex_shrink_0()
            .px_3()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                ui::icon(IconName::Sparkles)
                    .size(px(16.))
                    .text_color(w::readable(theme.primary, cx)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .font_semibold()
                    .text_sm()
                    .child(t!("ai_chat.copilot.title")),
            )
            .child(
                div()
                    .id("copilot-host")
                    .min_w_0()
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    })
                    .child(w::neutral_chip(
                        Some(IconName::Server),
                        truncate(&host, 32),
                        cx,
                    )),
            )
            .child(div().flex_1())
            .when_some(model, |this, m| {
                let tip = t!("ai_ui.model.tooltip", model = m.clone());
                this.child(
                    div()
                        .id("copilot-model")
                        .min_w_0()
                        .max_w(px(110.))
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                        })
                        .child(
                            div()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(m),
                        ),
                )
            })
            .child({
                let me = cx.entity().downgrade();
                Button::new("copilot-mode")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(mode_icon(mode)))
                    .tooltip(t!(
                        "ai_ui.mode_tooltip",
                        mode = mode_label(mode),
                        hint = mode_hint(mode)
                    ))
                    .dropdown_menu(move |mut menu, _, _| {
                        for value in MODES {
                            let me = me.clone();
                            menu = menu.item(
                                PopupMenuItem::new(mode_label(value))
                                    .checked(mode == value)
                                    .on_click(move |_, window, cx| {
                                        if let Some(me) = me.upgrade() {
                                            me.update(cx, |c, cx| c.set_mode(value, window, cx));
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
                        .icon(ui::icon(IconName::SquarePen))
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
                    .on_click(cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(AiChatEvent::Close))),
            )
            .into_any_element()
    }

    /// The quick explanation asked from the terminal, above the conversation.
    fn render_quick(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let q = self.quick.as_ref()?;
        let quoted: String = {
            let lines: Vec<&str> = q.quoted.lines().collect();
            lines[lines.len().saturating_sub(12)..].join("\n")
        };
        let actions = self.code_actions(cx);
        let body = match &q.answer {
            None => w::typing_indicator("copilot-quick-typing", t!("terminal.ai.asking"), cx),
            Some(Ok(answer)) => w::rich_text("copilot-quick-answer", answer, &actions, false, cx),
            Some(Err(e)) => w::error_banner(
                "copilot-quick-error",
                t!("terminal.ai.failed", error = e),
                None,
                cx,
            ),
        };
        let done = q.answer.as_ref().is_some_and(Result::is_ok);
        let theme = cx.theme();
        Some(
            v_flex()
                .id("copilot-quick")
                .m_3()
                .p_3()
                .gap_2()
                .max_h(px(420.))
                .flex_shrink_0()
                .overflow_y_scrollbar()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border)
                .bg(theme.secondary)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            ui::icon(IconName::BookOpen)
                                .size(px(14.))
                                .text_color(w::readable(theme.primary, cx)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .font_semibold()
                                .child(q.title.clone()),
                        )
                        .child(
                            Button::new("copilot-quick-close")
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::X))
                                .tooltip(t!("common.close"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.quick = None;
                                    cx.notify();
                                })),
                        ),
                )
                .when(!quoted.trim().is_empty(), |this| {
                    this.child(w::code_line("copilot-quick-quote", &quoted, false, cx))
                })
                .child(body)
                .when(!q.provider.is_empty(), |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("terminal.ai.provider", provider = q.provider.clone())),
                    )
                })
                .when(done, |this| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_chat.quick.follow_up")),
                    )
                })
                .into_any_element(),
        )
    }

    /// The terminal's context that goes with the next message, as chips the
    /// user can remove, and the note about secrets.
    fn render_chips(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let chips = self.chips.iter().enumerate().map(|(i, chip)| {
            let icon = match chip.kind {
                ChipKind::Host => IconName::Server,
                ChipKind::Directory => IconName::Folder,
                ChipKind::LastCommand => IconName::SquareTerminal,
                ChipKind::Selection => IconName::TextQuote,
            };
            let tip: SharedString = truncate(&chip.text, 600).into();
            h_flex()
                .id(("copilot-chip", i))
                .h(px(22.))
                .gap_1()
                .pl_1p5()
                .items_center()
                .max_w(px(220.))
                .rounded(px(6.))
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .text_xs()
                .tooltip(move |window, cx| {
                    gpui_component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                })
                .child(
                    ui::icon(icon)
                        .size(px(12.))
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(chip.label.clone()),
                )
                .child(
                    Button::new(("copilot-chip-remove", i))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::X))
                        .tooltip(t!("ai_chat.context.remove"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if i < this.chips.len() {
                                this.chips.remove(i);
                            }
                            cx.notify();
                        })),
                )
        });
        let secrets: SharedString = t!("ai_chat.context.secrets_hidden");
        h_flex()
            .px_3()
            .pt_2()
            .gap_1()
            .flex_wrap()
            .items_center()
            .children(chips)
            .child(
                Button::new("copilot-context-reload")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::RefreshCw))
                    .when(self.chips.is_empty(), |b| {
                        b.label(t!("ai_chat.context.add"))
                    })
                    .tooltip(t!("ai_chat.context.reload"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.load_terminal_context(cx)),
                    ),
            )
            .child(div().flex_1())
            .child(
                h_flex()
                    .id("copilot-secrets")
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(secrets.clone()).build(window, cx)
                    })
                    .child(ui::icon(IconName::ShieldCheck).size(px(12.)))
                    .child(t!("ai_ui.secrets_hidden_short")),
            )
            .into_any_element()
    }

    /// The box to write in, with Send and how to send.
    fn render_composer(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let copilot = self.terminal.is_some();
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let theme = cx.theme();
        v_flex()
            .p_3()
            .gap_1()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_end()
                    .pl_1()
                    .pr_1p5()
                    .py_1p5()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(if focused { theme.ring } else { theme.input })
                    .bg(theme.background)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Textarea::new(&self.input).appearance(false)),
                    )
                    .child(
                        Button::new("ai-send")
                            .small()
                            .primary()
                            .icon(ui::icon(IconName::SendHorizontal))
                            .when(!copilot, |b| b.label(t!("ai_chat.send")))
                            .tooltip(t!("ai_chat.send"))
                            .loading(self.sending)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.send(window, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .px_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("ai_ui.reply_hint")),
            )
            .into_any_element()
    }
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

/// A unified diff with its lines colored: added in green, removed in red,
/// hunk headers in blue.
fn diff_block(i: usize, diff: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let lines = diff.lines().take(1500).map(|line| {
        let (color, bg) = if line.starts_with("+++") || line.starts_with("---") {
            (theme.muted_foreground, None)
        } else if line.starts_with('+') {
            (
                w::readable(theme.success, cx),
                Some(w::tint(theme.success, 0.12)),
            )
        } else if line.starts_with('-') {
            (
                w::readable(theme.danger, cx),
                Some(w::tint(theme.danger, 0.12)),
            )
        } else if line.starts_with("@@") {
            (w::readable(theme.info, cx), None)
        } else {
            (theme.foreground, None)
        };
        div()
            .px_3()
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
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(w::code_background(cx))
        .font_family(ui::mono_family(cx))
        .text_xs()
        .child(v_flex().min_w_full().children(lines))
        .into_any_element()
}

impl Render for AiChat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A focus for each approval card (keys work once it is clicked or
        // reached with Tab).
        let pending: Vec<Id> = self
            .detail
            .as_ref()
            .map(|d| d.approvals.iter().map(|a| a.id).collect())
            .unwrap_or_default();
        self.approval_focus.retain(|id, _| pending.contains(id));
        for id in pending {
            self.approval_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle().tab_stop(true));
        }
        let copilot = self.terminal.is_some();
        let me = cx.entity().downgrade();
        let open_settings = move |_: &mut Window, cx: &mut App| {
            let _ = me.update(cx, |_, cx| cx.emit(AiChatEvent::OpenAiSettings));
        };
        let body = if self.task.is_none() && copilot {
            self.render_start(cx)
        } else {
            self.render_conversation(window, cx)
        };
        let banner = self
            .blocked
            .as_deref()
            .map(|text| fix_banner(text, open_settings, cx));
        let header = copilot.then(|| self.render_copilot_header(cx));
        let quick = self.render_quick(cx);
        let chips = copilot.then(|| self.render_chips(cx));
        let composer = self.render_composer(window, cx);
        let running = self.summary().is_some_and(TaskSummary::running);
        let theme = cx.theme();
        v_flex()
            .size_full()
            .children(header)
            .children(quick)
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
                                .small()
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
                v_flex()
                    .flex_shrink_0()
                    .when(!(copilot && running), |this| this.border_t_1())
                    .border_color(theme.border)
                    .children(chips)
                    .child(composer),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approvals_read_their_preview() {
        let v = json!({
            "id": "0192a8c4-7b6e-7c1a-9f3e-123456789abc",
            "call_id": "c1",
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

    #[test]
    fn summaries_read_scope_and_times() {
        let group = termoak_core::new_id();
        let v = json!({
            "id": "0192a8c4-7b6e-7c1a-9f3e-123456789abc",
            "title": "Update packages",
            "status": "completed",
            "created_at": 1_000,
            "updated_at": 9_000,
            "finished_at": 61_000,
            "host_ids": ["0192a8c4-7b6e-7c1a-9f3e-123456789abd", "bad"],
            "group_id": group,
            "tag": "",
            "pending_approvals": [{}],
        });
        let t = TaskSummary::from(&v).unwrap();
        assert_eq!(t.host_ids.len(), 1);
        assert_eq!(t.hosts, 2);
        assert_eq!(t.group_id, Some(group));
        assert_eq!(t.tag, None);
        assert_eq!(t.duration_ms(100_000), Some(60_000));
        assert_eq!(t.phase(), Phase::Completed);
        let running = TaskSummary::from(&json!({
            "id": "0192a8c4-7b6e-7c1a-9f3e-123456789abc", "status": "running", "created_at": 1_000
        }))
        .unwrap();
        assert_eq!(running.duration_ms(5_000), Some(4_000));
    }
}
