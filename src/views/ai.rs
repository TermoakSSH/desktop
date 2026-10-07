//! AI: tasks the agent runs on your hosts (on the server or on this
//! computer, as chosen in Settings → AI), with the live conversation (text,
//! reasoning and tools), approvals and follow-up messages.
//!
//! On the left, the tasks with their status, filters and search; on the
//! right, the task shown or the composer of a new one.

use std::collections::{BTreeMap, HashMap, HashSet};

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, Focusable, InteractiveElement,
    IntoElement, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Disableable, Selectable, Sizable, StyledExt, h_flex, v_flex};

use serde_json::{Value, json};
use termoak_ai::HostRun;
use termoak_ai::message::Message;
use termoak_ai::runbook::{self, ExecutedStep, HostNames};
use termoak_core::Id;
use termoak_core::model::{Snippet, SyncMode};

use super::OpenRequest;
use super::ai_chat::{
    AiChat, AiChatEvent, MODES, TaskSummary, ai_backend, ai_result, fix_banner, local_model_label,
    mode_hint, mode_icon, mode_label,
};
use super::ai_ui::status::{
    TaskFilter, format_cost, format_duration, matches_search, model_name, progress, relative_time,
};
use super::ai_ui::{self as w};
use crate::local_ai::RunOn;
use crate::local_ai::tasks::AiBackend;
use crate::runtime;
use crate::state::{AppModel, ModelEvent};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Examples to try in an empty composer: (icon, text).
fn examples() -> [(IconName, SharedString); 4] {
    [
        (IconName::HardDrive, t!("ai_ui.example.disk")),
        (IconName::CircleAlert, t!("ai_ui.example.nginx")),
        (IconName::PackageCheck, t!("ai_ui.example.updates")),
        (IconName::FileSearch, t!("ai_ui.example.logs")),
    ]
}

/// The shortcut that starts a task from the composer.
fn send_shortcut() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘↵"
    } else {
        "Ctrl+↵"
    }
}

pub struct AiView {
    model: Entity<AppModel>,
    tasks: Vec<TaskSummary>,
    selected: Option<Id>,
    /// Conversation of the selected task.
    chat: Entity<AiChat>,
    providers: Option<ChoiceState<Option<String>>>,
    prompt: Entity<TextareaState>,
    mode: &'static str,
    scope: Vec<Id>,
    show_scope: bool,
    /// With several hosts: one conversation per host.
    fan_out: bool,
    /// The AI first proposes a plan to approve.
    plan_first: bool,
    creating: bool,
    /// Building a runbook from the task shown.
    runbook_busy: bool,
    /// Tasks with events that are not in the list (the hosts of a
    /// multi-host task): no reload for them.
    not_listed: HashSet<Id>,
    /// Unknown tasks waiting for the list to be reloaded.
    unknown: HashSet<Id>,
    loading: bool,
    /// The list was loaded at least once (before that, a skeleton).
    loaded: bool,
    error: Option<String>,
    /// Creating the task failed for a reason Settings → AI fixes.
    blocked: Option<String>,
    /// The tasks shown are the ones on this computer.
    local: bool,
    /// Filter and search of the task list.
    filter: TaskFilter,
    search: Entity<InputState>,
    /// Finished and all hosts of the running multi-host tasks.
    fan_progress: HashMap<Id, (usize, usize)>,
    /// The multi-host task each host's conversation belongs to.
    child_parent: HashMap<Id, Id>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for AiView {}

impl AiView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 14)
                .placeholder(t!("ai.prompt_placeholder"))
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("ai_ui.search")));
        let chat = cx.new(|cx| AiChat::new(model.clone(), window, cx));
        let subs = vec![
            cx.subscribe_in(
                &model,
                window,
                |this, _, ev: &ModelEvent, window, cx| match ev {
                    ModelEvent::Server(v) if v["type"] == "ai" => this.on_event(v, window, cx),
                    ModelEvent::Server(v) if v["type"] == "hello" => this.refresh(window, cx),
                    ModelEvent::SessionChanged => {
                        this.providers = None;
                        this.refresh(window, cx);
                    }
                    _ => {}
                },
            ),
            // Another choice in Settings → AI: other tasks.
            cx.observe_in(&model, window, |this, model, window, cx| {
                let local = model.read(cx).ai_run_on() == RunOn::Local;
                if local != this.local {
                    this.select(None, window, cx);
                    this.refresh(window, cx);
                }
            }),
            cx.subscribe_in(&chat, window, |this, _, ev: &AiChatEvent, _, cx| match ev {
                AiChatEvent::Summary(t) => {
                    if let Some(old) = this.tasks.iter_mut().find(|x| x.id == t.id) {
                        *old = t.clone();
                        cx.notify();
                    }
                }
                AiChatEvent::OpenAiSettings => cx.emit(OpenRequest::AiSettings),
                AiChatEvent::Close => {}
            }),
            // Cmd/Ctrl+Enter starts the task.
            cx.subscribe_in(&prompt, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter {
                    secondary: true, ..
                } = ev
                {
                    this.create(window, cx);
                }
            }),
            cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut view = Self {
            model,
            tasks: Vec::new(),
            selected: None,
            chat,
            providers: None,
            prompt,
            mode: "ask",
            scope: Vec::new(),
            show_scope: false,
            fan_out: true,
            plan_first: false,
            creating: false,
            runbook_busy: false,
            not_listed: HashSet::new(),
            unknown: HashSet::new(),
            loading: false,
            loaded: false,
            error: None,
            blocked: None,
            local: false,
            filter: TaskFilter::All,
            search,
            fan_progress: HashMap::new(),
            child_parent: HashMap::new(),
            _subs: subs,
        };
        view.refresh(window, cx);
        view
    }

    /// Reloads the providers and the task list (of the server, or of this
    /// computer with "This computer" in Settings → AI).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.local = self.model.read(cx).ai_run_on() == RunOn::Local;
        let Some(backend) = ai_backend(&self.model, cx) else {
            self.tasks.clear();
            self.select(None, window, cx);
            return;
        };
        self.loading = true;
        cx.notify();
        // Locally the provider is the one chosen in Settings → AI.
        if backend.is_local() {
            self.providers = None;
        }
        let api = match &backend {
            AiBackend::Server(api) if self.providers.is_none() => api.clone(),
            _ => {
                self.load_tasks(window, cx);
                return;
            }
        };
        let api2 = api.clone();
        runtime::run_in(
            cx,
            window,
            async move { api2.get::<Value>("/api/v1/ai/providers").await },
            |this, res, window, cx| {
                match res {
                    Ok(v) => {
                        let default = v["default"].as_str().unwrap_or("").to_string();
                        let mut items = vec![Choice::new(
                            if default.is_empty() {
                                t!("ai.provider_default")
                            } else {
                                t!("ai.provider_default_named", provider = default)
                            },
                            None,
                        )];
                        for p in v["providers"].as_array().into_iter().flatten() {
                            if p["hidden"].as_bool().unwrap_or(false) {
                                continue;
                            }
                            let key = p["key"].as_str().unwrap_or("").to_string();
                            let label = p["label"].as_str().unwrap_or(&key).to_string();
                            let available = p["available"].as_bool().unwrap_or(true);
                            let text = if available {
                                label
                            } else {
                                t!("ai.provider_unavailable", provider = label).to_string()
                            };
                            items.push(Choice::new(text, Some(key)));
                        }
                        let state = ui::choice_state(items, Some(&None), window, cx);
                        this.providers = Some(state);
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
        self.load_tasks(window, cx);
    }

    fn load_tasks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = ai_backend(&self.model, cx) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move { backend.list(50).await.map_err(|f| f.text) },
            |this, res, window, cx| {
                this.loading = false;
                this.loaded = true;
                match res {
                    Ok(list) => {
                        this.tasks = list.iter().filter_map(TaskSummary::from).collect();
                        this.error = None;
                        // Still not in the list: a host of a multi-host task.
                        let listed: HashSet<Id> = this.tasks.iter().map(|t| t.id).collect();
                        for id in std::mem::take(&mut this.unknown) {
                            if !listed.contains(&id) {
                                this.not_listed.insert(id);
                            }
                        }
                        // The progress of the multi-host tasks still running.
                        let running: Vec<Id> = this
                            .tasks
                            .iter()
                            .filter(|t| t.fan_out && t.running())
                            .map(|t| t.id)
                            .take(10)
                            .collect();
                        for id in running {
                            this.load_progress(id, window, cx);
                        }
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
            },
        );
    }

    /// How many hosts of a multi-host task have finished.
    fn load_progress(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = ai_backend(&self.model, cx) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move { backend.get(id).await.map_err(|f| f.text) },
            move |this, res, _, cx| {
                if let Ok(v) = res {
                    let hosts: Vec<HostRun> =
                        serde_json::from_value(v["hosts"].clone()).unwrap_or_default();
                    for h in &hosts {
                        this.child_parent.insert(h.task_id, id);
                        this.not_listed.insert(h.task_id);
                    }
                    if !hosts.is_empty() {
                        this.fan_progress.insert(id, progress(&hosts));
                    }
                    cx.notify();
                }
            },
        );
    }

    /// Shows a task (from a notification).
    pub fn open_task(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        self.select(Some(id), window, cx);
    }

    /// A new task on these hosts ("Ask AI" on a group or a selection in the
    /// hosts list): one conversation per host by default.
    pub fn start_with_hosts(&mut self, ids: Vec<Id>, window: &mut Window, cx: &mut Context<Self>) {
        self.select(None, window, cx);
        self.scope = ids;
        self.show_scope = !self.scope.is_empty();
        self.fan_out = true;
        ui::focus_later(&self.prompt, window, cx);
        cx.notify();
    }

    fn select(&mut self, id: Option<Id>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = id;
        self.chat.update(cx, |c, cx| c.set_task(id, window, cx));
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = ai_backend(&self.model, cx) else {
            return;
        };
        let prompt = self.prompt.read(cx).value().trim().to_string();
        if prompt.is_empty() {
            ui::error(window, cx, t!("ai.prompt_required"));
            return;
        }
        let provider = self
            .providers
            .as_ref()
            .and_then(|p| ui::chosen(p, cx))
            .flatten();
        let mut body = json!({"prompt": prompt, "mode": self.mode});
        if let Some(p) = provider {
            body["provider"] = json!(p);
        }
        if !self.scope.is_empty() {
            body["host_ids"] = json!(self.scope);
        }
        let fan_out = self.fan_out && self.scope.len() > 1;
        if fan_out {
            body["fan_out"] = json!(true);
        }
        let plan_first = self.plan_first;
        if plan_first {
            body["plan_first"] = json!(true);
        }
        self.creating = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { Ok::<_, std::convert::Infallible>(backend.create(body).await) },
            move |this, res, window, cx| {
                this.creating = false;
                match ai_result(res) {
                    Ok(v) => {
                        this.blocked = None;
                        this.prompt.update(cx, |i, cx| i.set_value("", window, cx));
                        // A server before 0.5 ignores them: say so.
                        if plan_first && v["plan_first"].as_bool() != Some(true) {
                            ui::notify(
                                window,
                                cx,
                                crate::state::ToastKind::Info,
                                t!("ai.plan_first_unsupported"),
                            );
                        }
                        if fan_out && v["fan_out"].as_bool() != Some(true) {
                            ui::notify(
                                window,
                                cx,
                                crate::state::ToastKind::Info,
                                t!("ai.fan_out_unsupported"),
                            );
                        }
                        if let Some(t) = TaskSummary::from(&v) {
                            let id = t.id;
                            this.tasks.insert(0, t);
                            this.select(Some(id), window, cx);
                        }
                    }
                    Err(f) if f.fix_in_settings => this.blocked = Some(f.text),
                    Err(f) => ui::error(window, cx, t!("ai.create_failed", error = f.text)),
                }
                cx.notify();
            },
        );
    }

    fn delete_task(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(backend), Some(id)) = (ai_backend(&self.model, cx), self.selected) else {
            return;
        };
        runtime::run_in(
            cx,
            window,
            async move { backend.delete(id).await.map_err(|f| f.text) },
            move |this, res, window, cx| match res {
                Ok(_) => {
                    this.tasks.retain(|t| t.id != id);
                    this.select(None, window, cx);
                }
                Err(e) => ui::error(window, cx, t!("ai.delete_failed", error = e)),
            },
        );
    }

    /// "Save as runbook": the commands the task ran (for a multi-host task,
    /// those of the first host that ran any), as a new snippet to review and
    /// save in the vault.
    fn save_runbook(&mut self, id: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = ai_backend(&self.model, cx) else {
            return;
        };
        let known: Vec<(Id, HostNames)> = self
            .model
            .read(cx)
            .hosts
            .iter()
            .map(|h| {
                (
                    h.data.id,
                    HostNames {
                        label: h.data.label.clone(),
                        address: h.data.address.clone(),
                    },
                )
            })
            .collect();
        self.runbook_busy = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                let view = backend.get(id).await.map_err(|f| f.text)?;
                let title = view["title"].as_str().unwrap_or("").to_string();
                if let Some(rb) = runbook_of(&title, &view, &known) {
                    return Ok::<_, String>(Some(rb));
                }
                for h in view["hosts"].as_array().into_iter().flatten() {
                    let Some(task) = h["task_id"].as_str().and_then(|s| s.parse::<Id>().ok())
                    else {
                        continue;
                    };
                    let host = backend.get(task).await.map_err(|f| f.text)?;
                    if let Some(rb) = runbook_of(&title, &host, &known) {
                        return Ok(Some(rb));
                    }
                }
                Ok(None)
            },
            |this, res, window, cx| {
                this.runbook_busy = false;
                cx.notify();
                match res {
                    Ok(Some(rb)) => {
                        let snippet = Snippet {
                            id: Id::nil(),
                            name: rb.name,
                            script: rb.script,
                            description: rb.description,
                            tags: vec!["ai".into(), "runbook".into()],
                        };
                        crate::views::snippets::open_snippet_form(
                            this.model.clone(),
                            Some(snippet),
                            window,
                            cx,
                        );
                    }
                    Ok(None) => ui::notify(
                        window,
                        cx,
                        crate::state::ToastKind::Info,
                        t!("ai.runbook.empty"),
                    ),
                    Err(e) => ui::error(window, cx, t!("ai.runbook.failed", error = e)),
                }
            },
        );
    }

    /// Live event of a task: here it only matters for the list (`AiChat`
    /// handles the conversation).
    fn on_event(&mut self, v: &Value, window: &mut Window, cx: &mut Context<Self>) {
        let Some(task_id) = v["task_id"].as_str().and_then(|s| s.parse::<Id>().ok()) else {
            return;
        };
        let ev = &v["event"];
        let ty = ev["type"].as_str().unwrap_or("");
        // A host of a multi-host task: its progress.
        if let Some(parent) = self.child_parent.get(&task_id).copied() {
            if matches!(ty, "finished" | "status") {
                self.load_progress(parent, window, cx);
            }
            return;
        }
        match self.tasks.iter_mut().find(|t| t.id == task_id) {
            Some(t) => match ty {
                "status" | "finished" => {
                    t.status = ev["status"].as_str().unwrap_or("").to_string();
                    if ty == "finished" && t.finished_at.is_none() {
                        t.finished_at = Some(w::now_ms());
                    }
                }
                "approval_requested" => t.pending += 1,
                "approval_decided" => t.pending = t.pending.saturating_sub(1),
                _ => {}
            },
            // A task created elsewhere (once: the hosts of a multi-host task
            // are not in the list).
            None if ty == "status" && !self.not_listed.contains(&task_id) => {
                if self.unknown.insert(task_id) {
                    self.load_tasks(window, cx);
                }
            }
            None => {}
        }
        cx.notify();
    }

    /// Callback that opens Settings → AI.
    fn open_settings(
        &self,
        cx: &Context<Self>,
    ) -> impl Fn(&mut Window, &mut gpui::App) + Clone + use<> {
        let me = cx.entity().downgrade();
        move |_, cx| {
            let _ = me.update(cx, |_, cx| cx.emit(OpenRequest::AiSettings));
        }
    }

    /// Adds hosts to the task's scope.
    fn add_scope(&mut self, ids: &[Id], cx: &mut Context<Self>) {
        for id in ids {
            if !self.scope.contains(id) {
                self.scope.push(*id);
            }
        }
        cx.notify();
    }

    /// Groups (with their subgroups) and tags of the hosts the AI can use,
    /// with those hosts: shortcuts to fill the scope.
    #[allow(clippy::type_complexity)]
    fn scope_shortcuts(&self, cx: &gpui::App) -> (Vec<(String, Vec<Id>)>, Vec<(String, Vec<Id>)>) {
        let m = self.model.read(cx);
        let account = m.ai_account.or(m.current_account);
        let usable: Vec<_> = m
            .hosts
            .iter()
            .filter(|h| {
                self.local || h.scope == termoak_client::Scope::Device || h.account() == account
            })
            .collect();
        let mut groups: Vec<(String, Vec<Id>)> = Vec::new();
        for g in &m.groups {
            let mut ids = vec![g.data.id];
            let mut i = 0;
            while i < ids.len() {
                for c in &m.groups {
                    if c.data.parent_id == Some(ids[i]) && !ids.contains(&c.data.id) {
                        ids.push(c.data.id);
                    }
                }
                i += 1;
            }
            let hosts: Vec<Id> = usable
                .iter()
                .filter(|h| h.data.group_id.is_some_and(|gid| ids.contains(&gid)))
                .map(|h| h.data.id)
                .collect();
            if !hosts.is_empty() {
                groups.push((g.data.name.clone(), hosts));
            }
        }
        groups.sort_by_key(|(name, _)| name.to_lowercase());
        let mut tags: BTreeMap<String, Vec<Id>> = BTreeMap::new();
        for h in &usable {
            for t in &h.data.tags {
                let list = tags.entry(t.trim().to_string()).or_default();
                if !list.contains(&h.data.id) {
                    list.push(h.data.id);
                }
            }
        }
        tags.remove("");
        (groups, tags.into_iter().collect())
    }

    /// "Add a group" and "Add a tag": menus that add their hosts.
    fn render_scope_shortcuts(
        &self,
        groups: Vec<(String, Vec<Id>)>,
        tags: Vec<(String, Vec<Id>)>,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let me = cx.entity().downgrade();
        let menu_button = |id: &'static str,
                           icon: IconName,
                           label: SharedString,
                           items: Vec<(String, Vec<Id>)>| {
            let me = me.clone();
            Button::new(id)
                .xsmall()
                .ghost()
                .icon(ui::icon(icon))
                .label(label)
                .dropdown_menu(move |mut menu, _, _| {
                    for (name, ids) in &items {
                        let me = me.clone();
                        let ids = ids.clone();
                        menu = menu.item(
                            PopupMenuItem::new(format!("{name} ({})", ids.len())).on_click(
                                move |_, _, cx| {
                                    if let Some(v) = me.upgrade() {
                                        v.update(cx, |v, cx| v.add_scope(&ids, cx));
                                    }
                                },
                            ),
                        );
                    }
                    menu
                })
        };
        h_flex()
            .gap_1()
            .when(!groups.is_empty(), |this| {
                this.child(menu_button(
                    "ai-scope-group",
                    IconName::Folder,
                    t!("ai.scope_add_group"),
                    groups,
                ))
            })
            .when(!tags.is_empty(), |this| {
                this.child(menu_button(
                    "ai-scope-tag",
                    IconName::Tag,
                    t!("ai.scope_add_tag"),
                    tags,
                ))
            })
            .into_any_element()
    }

    // ----- Rendering -----

    /// Where a task works: its group, tag, host or number of hosts.
    fn scope_of(&self, t: &TaskSummary, cx: &gpui::App) -> Option<(IconName, String)> {
        let m = self.model.read(cx);
        if let Some(g) = t
            .group_id
            .and_then(|g| m.groups.iter().find(|x| x.data.id == g))
        {
            return Some((IconName::Folder, g.data.name.clone()));
        }
        if let Some(tag) = &t.tag {
            return Some((IconName::Tag, tag.clone()));
        }
        match t.host_ids.as_slice() {
            [one] => m
                .hosts
                .iter()
                .find(|h| h.data.id == *one)
                .map(|h| (IconName::Server, h.data.label.clone())),
            _ if t.hosts > 1 => Some((IconName::Layers, tn!("ai.hosts_count", t.hosts).into())),
            _ => None,
        }
    }

    /// The tasks the filter and the search leave.
    fn visible_tasks(&self, cx: &gpui::App) -> Vec<&TaskSummary> {
        let query = self.search.read(cx).value().trim().to_string();
        self.tasks
            .iter()
            .filter(|t| self.filter.matches(t.phase()))
            .filter(|t| {
                let scope = self.scope_of(t, cx).map(|(_, s)| s).unwrap_or_default();
                matches_search(&query, &[&t.title, &scope, &t.provider])
            })
            .collect()
    }

    fn render_task_row(
        &self,
        i: usize,
        t: &TaskSummary,
        now: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let active = self.selected == Some(t.id);
        let id = t.id;
        let phase = t.phase();
        let scope = self.scope_of(t, cx);
        let progress = self.fan_progress.get(&t.id).copied();
        let mut meta: Vec<String> = Vec::new();
        if let Some((done, total)) = progress.filter(|_| t.fan_out) {
            meta.push(t!("ai_ui.hosts.progress_short", done = done, total = total).to_string());
        }
        meta.push(relative_time(now, t.created_at).to_string());
        if let Some(d) = t.duration_ms(now).filter(|_| !t.running()) {
            meta.push(format_duration(d));
        }
        if let Some(c) = format_cost(t.cost_micros) {
            meta.push(c);
        }
        let title: SharedString = t.title.clone().into();
        v_flex()
            .id(("ai-task", i))
            .px_3()
            .py_2()
            .gap_1()
            .rounded(theme.radius)
            .border_1()
            .border_color(gpui::transparent_black())
            .cursor_pointer()
            .when(active, |this| {
                this.bg(theme.list_active)
                    .border_color(w::tint(theme.list_active_border, 0.5))
            })
            .when(!active, |this| this.hover(|s| s.bg(theme.list_hover)))
            .tooltip(move |window, cx| {
                gpui_component::tooltip::Tooltip::new(title.clone()).build(window, cx)
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select(Some(id), window, cx)
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child(w::status_icon(phase, 14., cx)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .font_medium()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(t.title.clone()),
                    )
                    .when(t.pending > 0, |this| {
                        this.child(w::chip(
                            Some(IconName::ShieldAlert),
                            t.pending.to_string(),
                            theme.warning,
                            cx,
                        ))
                    }),
            )
            .child(
                h_flex()
                    .pl(px(24.))
                    .gap_1p5()
                    .items_center()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .when_some(scope, |this, (icon, label)| {
                        this.child(w::neutral_chip(Some(icon), label, cx).max_w(px(130.)))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(meta.join(" · ")),
                    ),
            )
    }

    fn render_task_list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let now = w::now_ms();
        let counts: Vec<(TaskFilter, usize)> = TaskFilter::ALL
            .iter()
            .map(|f| {
                (
                    *f,
                    self.tasks.iter().filter(|t| f.matches(t.phase())).count(),
                )
            })
            .collect();
        let visible: Vec<TaskSummary> = self.visible_tasks(cx).into_iter().cloned().collect();
        let filtering =
            self.filter != TaskFilter::All || !self.search.read(cx).value().trim().is_empty();
        let body: gpui::AnyElement = if !self.loaded && self.error.is_none() {
            w::list_skeleton(6)
        } else if let Some(e) = self.error.clone().filter(|_| self.tasks.is_empty()) {
            let me = cx.entity().downgrade();
            div()
                .px_2()
                .child(w::error_banner(
                    "ai-tasks-retry",
                    ui::capitalize(&e),
                    Some(std::rc::Rc::new(move |window, cx| {
                        let _ = me.update(cx, |v, cx| v.refresh(window, cx));
                    })),
                    cx,
                ))
                .into_any_element()
        } else if visible.is_empty() {
            let theme = cx.theme();
            v_flex()
                .px_4()
                .py_8()
                .gap_2()
                .items_center()
                .text_center()
                .child(
                    ui::icon(if filtering {
                        IconName::SearchX
                    } else {
                        IconName::Inbox
                    })
                    .size(px(28.))
                    .text_color(theme.muted_foreground),
                )
                .child(div().text_sm().font_medium().child(if filtering {
                    t!("ai_ui.list.no_match")
                } else {
                    t!("ai.no_tasks")
                }))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if filtering {
                            t!("ai_ui.list.no_match_hint")
                        } else {
                            t!("ai_ui.list.empty_hint")
                        }),
                )
                .when(filtering, |this| {
                    this.child(
                        Button::new("ai-clear-filters")
                            .xsmall()
                            .ghost()
                            .icon(ui::icon(IconName::X))
                            .label(t!("ai_ui.list.clear_filters"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.filter = TaskFilter::All;
                                this.search.update(cx, |s, cx| s.set_value("", window, cx));
                                cx.notify();
                            })),
                    )
                })
                .into_any_element()
        } else {
            v_flex()
                .gap_0p5()
                .px_2()
                .pb_2()
                .children(
                    visible
                        .iter()
                        .enumerate()
                        .map(|(i, t)| self.render_task_row(i, t, now, cx)),
                )
                .into_any_element()
        };
        let theme = cx.theme();
        v_flex()
            .w(px(320.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.sidebar)
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("ai-new")
                                    .flex_1()
                                    .primary()
                                    .icon(ui::icon(IconName::Plus))
                                    .label(t!("ai.new_task"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.select(None, window, cx);
                                        ui::focus_later(&this.prompt, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("ai-refresh")
                                    .ghost()
                                    .icon(ui::icon(IconName::RefreshCw))
                                    .tooltip(t!("ai_ui.refresh"))
                                    .loading(self.loading)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.refresh(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        Input::new(&self.search)
                            .small()
                            .cleanable(true)
                            .prefix(ui::icon(IconName::Search).size(px(14.))),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .flex_wrap()
                            .children(counts.into_iter().map(|(f, n)| {
                                let active = self.filter == f;
                                Button::new(SharedString::from(format!("ai-filter-{f:?}")))
                                    .xsmall()
                                    .label(format!("{} · {n}", f.label()))
                                    .selected(active)
                                    .map(|b| if active { b.primary() } else { b.ghost() })
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.filter = f;
                                        cx.notify();
                                    }))
                            })),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("ai-tasks")
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(body),
                ),
            )
    }

    /// The model the new task uses: the server's providers, or what Settings
    /// → AI chose on this computer.
    fn render_model_picker(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if let Some(p) = self.providers.clone() {
            return div()
                .w(px(210.))
                .child(Select::new(&p).small().icon(ui::icon(IconName::Bot)))
                .into_any_element();
        }
        let label = if self.local {
            local_model_label(&self.model.read(cx).settings.ai)
        } else {
            t!("ai.provider_default")
        };
        let open = self.open_settings(cx);
        Button::new("ai-model")
            .xsmall()
            .ghost()
            .icon(ui::icon(IconName::Bot))
            .label(label)
            .tooltip(t!("ai_ui.model.change"))
            .on_click(move |_: &ClickEvent, window, cx| open(window, cx))
            .into_any_element()
    }

    fn render_new_task(&self, window: &Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (groups, tags) = self.scope_shortcuts(cx);
        // The server AI reaches the hosts of its account (This device ones
        // and other accounts' cannot be used there).
        let hosts: Vec<(Id, String, bool)> = {
            let m = self.model.read(cx);
            let account = m.ai_account.or(m.current_account);
            m.hosts
                .iter()
                .filter(|h| {
                    self.local || h.scope == termoak_client::Scope::Device || h.account() == account
                })
                .map(|h| {
                    (
                        h.data.id,
                        h.data.label.clone(),
                        h.meta.sync_mode == SyncMode::DeviceOnly
                            || (!self.local && h.account().is_none()),
                    )
                })
                .collect()
        };
        let focused = self.prompt.read(cx).focus_handle(cx).is_focused(window);
        let model_picker = self.render_model_picker(cx);
        let scope_chips: Vec<(Id, String)> = self
            .scope
            .iter()
            .filter_map(|id| {
                hosts
                    .iter()
                    .find(|(h, _, _)| h == id)
                    .map(|(_, l, _)| (*id, l.clone()))
            })
            .collect();
        let theme = cx.theme();
        let mode = self.mode;

        let hero = h_flex()
            .gap_3()
            .items_center()
            .child(
                div()
                    .flex_shrink_0()
                    .size(px(40.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(w::tint(theme.primary, 0.16))
                    .child(
                        ui::icon(IconName::Sparkles)
                            .size(px(20.))
                            .text_color(w::readable(theme.primary, cx)),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(div().text_xl().font_semibold().child(t!("ai.new_task")))
                    .child(div().text_sm().text_color(theme.muted_foreground).child(
                        if self.local {
                            t!("ai.new_task_detail_local")
                        } else {
                            t!("ai.new_task_detail")
                        },
                    )),
            );

        let scope_button_label = if self.scope.is_empty() {
            t!("ai_ui.scope.all_hosts")
        } else {
            tn!("ai.hosts_count", self.scope.len())
        };
        let toolbar = h_flex()
            .px_2()
            .py_1p5()
            .gap_1()
            .items_center()
            .flex_wrap()
            .border_t_1()
            .border_color(theme.border)
            .child(
                Button::new("ai-scope")
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Server))
                    .label(scope_button_label)
                    .tooltip(t!("ai.scope_optional"))
                    .selected(self.show_scope)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.show_scope = !this.show_scope;
                        cx.notify();
                    })),
            )
            .child(self.render_scope_shortcuts(groups, tags, cx))
            .child(div().flex_1())
            .child(model_picker)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(w::kbd(send_shortcut(), cx)),
            )
            .child(
                Button::new("ai-create")
                    .small()
                    .primary()
                    .icon(ui::icon(IconName::Sparkles))
                    .label(t!("ai.start"))
                    .tooltip(t!("ai_ui.start_hint", keys = send_shortcut()))
                    .loading(self.creating)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.create(window, cx)),
                    ),
            );

        let composer = v_flex()
            .w_full()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if focused { theme.ring } else { theme.border })
            .bg(theme.background)
            .shadow_sm()
            .child(
                div()
                    .px_1()
                    .pt_1()
                    .child(Textarea::new(&self.prompt).appearance(false)),
            )
            .when(!scope_chips.is_empty(), |this| {
                this.child(
                    h_flex()
                        .px_3()
                        .pb_2()
                        .gap_1()
                        .flex_wrap()
                        .children(scope_chips.into_iter().enumerate().map(|(i, (id, label))| {
                            h_flex()
                                .h(px(22.))
                                .pl_1p5()
                                .gap_0p5()
                                .items_center()
                                .rounded(px(6.))
                                .bg(w::tint(theme.primary, 0.12))
                                .text_xs()
                                .text_color(w::readable(theme.primary, cx))
                                .child(ui::icon(IconName::Server).size(px(12.)))
                                .child(label)
                                .child(
                                    Button::new(("ai-scope-remove", i))
                                        .xsmall()
                                        .ghost()
                                        .icon(ui::icon(IconName::X))
                                        .tooltip(t!("ai_ui.scope.remove"))
                                        .on_click(cx.listener(
                                            move |this, _: &ClickEvent, _, cx| {
                                                this.scope.retain(|s| *s != id);
                                                cx.notify();
                                            },
                                        )),
                                )
                        }))
                        .child(
                            Button::new("ai-scope-clear")
                                .xsmall()
                                .ghost()
                                .label(t!("ai.scope_clear"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.scope.clear();
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(toolbar);

        let scope_panel =
            self.show_scope.then(|| {
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_ui.scope.hint")),
                    )
                    .child(
                        div()
                            .id("ai-scope-list")
                            .max_h(px(200.))
                            .overflow_y_scrollbar()
                            .child(h_flex().flex_wrap().gap_y_1().children(
                                hosts.into_iter().enumerate().map(
                                    |(i, (id, label, device_only))| {
                                        div().w(px(220.)).child(
                                            Checkbox::new(("scope", i))
                                                .label(if device_only {
                                                    t!("ai.scope_device_only", host = label)
                                                        .to_string()
                                                } else {
                                                    label
                                                })
                                                .checked(self.scope.contains(&id))
                                                .on_click(cx.listener(
                                                    move |this, v: &bool, _, cx| {
                                                        if *v {
                                                            this.scope.push(id);
                                                        } else {
                                                            this.scope.retain(|s| *s != id);
                                                        }
                                                        cx.notify();
                                                    },
                                                )),
                                        )
                                    },
                                ),
                            )),
                    )
            });

        let theme = cx.theme();
        let options = v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_x_6()
                    .gap_y_3()
                    .flex_wrap()
                    .items_center()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("ai.permissions")),
                            )
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .p_0p5()
                                    .rounded(theme.radius)
                                    .bg(theme.muted)
                                    .children(MODES.iter().map(|&value| {
                                        Button::new(SharedString::from(format!("ai-mode-{value}")))
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
                                                move |this, _: &ClickEvent, _, cx| {
                                                    this.mode = value;
                                                    cx.notify();
                                                },
                                            ))
                                    })),
                            ),
                    )
                    .child(
                        Switch::new("ai-plan-first")
                            .small()
                            .label(t!("ai.plan_first"))
                            .tooltip(t!("ai.plan_first_hint"))
                            .checked(self.plan_first)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.plan_first = *v;
                                cx.notify();
                            })),
                    )
                    .when(self.scope.len() > 1, |this| {
                        this.child(
                            Switch::new("ai-fan-out")
                                .small()
                                .label(tn!("ai.fan_out", self.scope.len()))
                                .tooltip(if self.fan_out {
                                    t!("ai.fan_out_hint")
                                } else {
                                    t!("ai.fan_out_off_hint")
                                })
                                .checked(self.fan_out)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    this.fan_out = *v;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(ui::icon(mode_icon(mode)).size(px(12.)))
                    .child(mode_hint(mode)),
            );

        let examples =
            v_flex()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .font_medium()
                        .text_color(theme.muted_foreground)
                        .child(t!("ai_ui.examples_title")),
                )
                .child(h_flex().flex_wrap().gap_2().children(
                    examples().into_iter().enumerate().map(|(i, (icon, text))| {
                        let fill = text.to_string();
                        h_flex()
                            .id(("ai-example", i))
                            .min_w(px(280.))
                            .flex_1()
                            .gap_2()
                            .items_start()
                            .px_3()
                            .py_2p5()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.secondary)
                            .text_sm()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.secondary_hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.prompt.update(cx, |p, cx| {
                                    p.set_value(fill.clone(), window, cx);
                                    p.focus(window, cx);
                                });
                            }))
                            .child(
                                ui::icon(icon)
                                    .size(px(14.))
                                    .mt(px(2.))
                                    .flex_shrink_0()
                                    .text_color(theme.muted_foreground),
                            )
                            .child(div().flex_1().min_w_0().child(text))
                    }),
                ));

        v_flex()
            .w_full()
            .max_w(px(820.))
            .px_8()
            .py_8()
            .gap_5()
            .child(hero)
            .child(composer)
            .children(scope_panel)
            .when_some(self.blocked.clone(), |this, text| {
                this.child(fix_banner(&text, self.open_settings(cx), cx))
            })
            .child(options)
            .when(self.prompt.read(cx).value().trim().is_empty(), |this| {
                this.child(examples)
            })
            .into_any_element()
    }

    fn render_detail(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let summary = self.chat.read(cx).summary().cloned().or_else(|| {
            self.tasks
                .iter()
                .find(|t| Some(t.id) == self.selected)
                .cloned()
        });
        // A host's conversation after drilling down into a multi-host task.
        let shown = self.chat.read(cx).task_id().or(self.selected);
        let runbook_busy = self.runbook_busy;
        let now = w::now_ms();
        let header = summary.map(|t| {
            let running = t.running();
            let phase = t.phase();
            let finished = phase.finished();
            let scope = self.scope_of(&t, cx);
            let progress = self.fan_progress.get(&t.id).copied();
            let theme = cx.theme();
            let created: SharedString = ui::format_ms(t.created_at).into();
            let meta = h_flex()
                .gap_1p5()
                .flex_wrap()
                .items_center()
                .child(w::status_chip(phase, cx))
                .child(w::neutral_chip(
                    Some(mode_icon(&t.mode)),
                    mode_label(&t.mode),
                    cx,
                ))
                .child(w::neutral_chip(
                    Some(IconName::Bot),
                    if !t.provider.is_empty() {
                        model_name(&t.provider).to_string()
                    } else if self.local {
                        local_model_label(&self.model.read(cx).settings.ai).to_string()
                    } else {
                        t!("ai.provider_default_lower").to_string()
                    },
                    cx,
                ))
                .when_some(scope, |this, (icon, label)| {
                    this.child(w::neutral_chip(Some(icon), label, cx))
                })
                .when_some(progress.filter(|_| t.fan_out && running), |this, (d, n)| {
                    this.child(w::neutral_chip(
                        Some(IconName::Layers),
                        t!("ai_ui.hosts.progress_short", done = d, total = n),
                        cx,
                    ))
                })
                .when_some(t.duration_ms(now).filter(|_| finished), |this, d| {
                    this.child(w::neutral_chip(
                        Some(IconName::Timer),
                        format_duration(d),
                        cx,
                    ))
                })
                .when_some(format_cost(t.cost_micros), |this, c| {
                    this.child(w::neutral_chip(Some(IconName::CircleDollarSign), c, cx))
                })
                .child(
                    div()
                        .id("ai-task-created")
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .tooltip(move |window, cx| {
                            gpui_component::tooltip::Tooltip::new(created.clone()).build(window, cx)
                        })
                        .child(relative_time(now, t.created_at)),
                );
            h_flex()
                .px_6()
                .py_3()
                .gap_3()
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .child(
                            div()
                                .text_lg()
                                .font_semibold()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(t.title.clone()),
                        )
                        .child(meta),
                )
                .when(
                    !running && matches!(t.status.as_str(), "failed" | "cancelled"),
                    |this| {
                        this.child(
                            Button::new("ai-continue")
                                .small()
                                .icon(ui::icon(IconName::Play))
                                .label(t!("ai.continue"))
                                .tooltip(t!("ai.continue_hint"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.chat.update(cx, |c, cx| {
                                        c.send_text(
                                            t!("ai.continue_prompt").to_string(),
                                            window,
                                            cx,
                                        )
                                    })
                                })),
                        )
                    },
                )
                .when(running, |this| {
                    this.child(
                        Button::new("ai-cancel")
                            .small()
                            .outline()
                            .icon(ui::icon(IconName::CircleStop))
                            .label(t!("ai.stop"))
                            .tooltip(t!("ai.stop_hint"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.chat.update(cx, |c, cx| c.cancel(window, cx))
                            })),
                    )
                })
                .when_some(shown.filter(|_| finished), |this, id| {
                    this.child(
                        Button::new("ai-runbook")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::ScrollText))
                            .label(t!("ai.runbook.save"))
                            .tooltip(t!("ai.runbook.hint"))
                            .loading(runbook_busy)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.save_runbook(id, window, cx)
                            })),
                    )
                })
                // A host's conversation is deleted with its multi-host task.
                .when(t.parent_id.is_none(), |this| {
                    this.child(
                        Button::new("ai-delete")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::Trash))
                            .tooltip(t!("ai.delete_task"))
                            .disabled(running)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.delete_task(window, cx)
                            })),
                    )
                })
        });
        v_flex()
            .size_full()
            .children(header)
            .child(div().flex_1().min_h_0().child(self.chat.clone()))
            .into_any_element()
    }
}

/// The runbook of a task view (with its messages), if it ran any command.
fn runbook_of(title: &str, view: &Value, known: &[(Id, HostNames)]) -> Option<runbook::Runbook> {
    let messages: Vec<Message> =
        serde_json::from_value(view["messages"].clone()).unwrap_or_default();
    let mut steps: Vec<ExecutedStep> =
        serde_json::from_value(view["steps"].clone()).unwrap_or_default();
    // Tasks of an older server or engine: from the transcript.
    if steps.is_empty() {
        steps = runbook::steps_from_messages(&messages);
    }
    let scope: Option<Vec<Id>> = serde_json::from_value(view["host_ids"].clone())
        .ok()
        .flatten();
    let host = runbook::single_host(&steps, scope.as_deref(), known);
    let rb = runbook::build(title, &messages, &steps, host.as_slice());
    (rb.steps > 0).then_some(rb)
}

impl AiView {
    /// With several signed-in accounts: whose server AI the panel uses.
    fn render_account_picker(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let m = self.model.read(cx);
        let active: Vec<(Id, crate::accounts::AccountRow)> = m
            .accounts
            .iter()
            .filter(|a| a.active())
            .map(|a| (a.id(), crate::accounts::AccountRow::new(&a.info)))
            .collect();
        if active.len() < 2 {
            return None;
        }
        let chosen = m.ai_account.or(m.current_account);
        let label = active
            .iter()
            .find(|(id, _)| Some(*id) == chosen)
            .map(|(_, r)| r.label())
            .unwrap_or_default();
        let model = self.model.clone();
        Some(
            Button::new("ai-account")
                .small()
                .icon(ui::icon(IconName::CircleUser))
                .label(t!("ai.account", account = label))
                .tooltip(t!("ai.account_tooltip"))
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, row) in &active {
                        let model = model.clone();
                        let id = *id;
                        menu = menu.item(
                            PopupMenuItem::new(row.label())
                                .checked(Some(id) == chosen)
                                .on_click(move |_, _, cx| {
                                    model.update(cx, |m, cx| m.set_ai_account(Some(id), cx))
                                }),
                        );
                    }
                    menu
                })
                .into_any_element(),
        )
    }
}

impl Render for AiView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let local = self.model.read(cx).ai_run_on() == RunOn::Local;
        let list = self.render_task_list(cx);
        let right = if self.selected.is_some() {
            self.render_detail(cx)
        } else {
            let composer = self.render_new_task(window, cx);
            v_flex()
                .size_full()
                .child(
                    div().flex_1().min_h_0().child(
                        v_flex()
                            .id("ai-new-task")
                            .size_full()
                            .items_center()
                            .overflow_y_scrollbar()
                            .child(composer),
                    ),
                )
                .into_any_element()
        };
        // A failed reload with tasks already shown: said in place.
        let reload_error = self.error.clone().filter(|_| !self.tasks.is_empty());
        let me = cx.entity().downgrade();
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("ai.title"),
                if local {
                    t!("ai.subtitle_local")
                } else {
                    t!("ai.subtitle")
                },
                h_flex()
                    .gap_2()
                    .items_center()
                    .children((!local).then(|| self.render_account_picker(cx)).flatten()),
                cx,
            ))
            .when_some(reload_error, |this, e| {
                this.child(div().px_6().py_2().child(w::error_banner(
                    "ai-reload-retry",
                    ui::capitalize(&e),
                    Some(std::rc::Rc::new(move |window, cx| {
                        let _ = me.update(cx, |v, cx| v.refresh(window, cx));
                    })),
                    cx,
                )))
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(list)
                    .child(div().flex_1().min_w_0().h_full().child(right)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runbook_from_a_task_view() {
        let web = termoak_core::new_id();
        let known = vec![(
            web,
            HostNames {
                label: "web1".into(),
                address: "10.0.0.5".into(),
            },
        )];
        // An older server: no steps, only the transcript.
        let view = json!({
            "title": "Restart nginx",
            "host_ids": [web],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "Restart nginx on web1"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "Checking the config first."},
                    {"type": "tool_call", "id": "c1", "name": "run_command",
                     "input": {"host": "web1", "command": "nginx -t && curl -sI http://10.0.0.5"}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "id": "c1", "content": "ok", "is_error": false}]}
            ]
        });
        let rb = runbook_of("Restart nginx", &view, &known).unwrap();
        assert_eq!(rb.steps, 1);
        assert!(rb.script.contains("# Checking the config first.\n"));
        assert!(
            rb.script.contains("curl -sI http://{{host}}\n"),
            "{}",
            rb.script
        );
        assert_eq!(rb.variables, ["host"]);
        // Nothing ran: no runbook.
        let empty = json!({"title": "x", "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        assert!(runbook_of("x", &empty, &known).is_none());
    }
}
