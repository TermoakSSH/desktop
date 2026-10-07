//! AI: tasks the server's agent runs on your hosts, with the live
//! conversation (text, reasoning and tools), approvals and follow-up
//! messages. Needs to be signed in to the server, with "Your Termoak
//! account" chosen in Settings → AI (with "This computer", it says that
//! running the AI locally comes next and sends nothing).

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use std::collections::{BTreeMap, HashSet};

use serde_json::{Value, json};
use termoak_ai::message::Message;
use termoak_ai::runbook::{self, ExecutedStep, HostNames};
use termoak_core::Id;
use termoak_core::model::{Snippet, SyncMode};

use super::OpenRequest;
use super::ai_chat::{
    AiChat, AiChatEvent, MODES, TaskSummary, ai_backend, ai_result, fix_banner, mode_hint,
    mode_label, status_label, truncate,
};
use crate::local_ai::RunOn;
use crate::local_ai::tasks::AiBackend;
use crate::runtime;
use crate::state::{AppModel, ModelEvent};
use crate::ui::{self, Choice, ChoiceState, IconName};

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
    error: Option<String>,
    /// Creating the task failed for a reason Settings → AI fixes.
    blocked: Option<String>,
    /// The tasks shown are the ones on this computer.
    local: bool,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for AiView {}

impl AiView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let prompt = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(5, 12)
                .placeholder(t!("ai.prompt_placeholder"))
        });
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
            error: None,
            blocked: None,
            local: false,
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
            |this, res, _, cx| {
                this.loading = false;
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
                    }
                    Err(e) => this.error = Some(e),
                }
                cx.notify();
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
        match self.tasks.iter_mut().find(|t| t.id == task_id) {
            Some(t) => match ty {
                "status" | "finished" => t.status = ev["status"].as_str().unwrap_or("").to_string(),
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

    /// "Add a group", "Add a tag" and "Clear" above the host list.
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
                .small()
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
            .pl_6()
            .gap_2()
            .flex_wrap()
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
            .when(!self.scope.is_empty(), |this| {
                this.child(
                    Button::new("ai-scope-clear")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::X))
                        .label(t!("ai.scope_clear"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.scope.clear();
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    // ----- Rendering -----

    fn render_task_list(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme();
        let selected = self.selected;
        v_flex()
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .child(
                        Button::new("ai-new")
                            .flex_1()
                            .primary()
                            .icon(ui::icon(IconName::Plus))
                            .label(t!("ai.new_task"))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.select(None, window, cx)
                            })),
                    )
                    .child(
                        Button::new("ai-refresh")
                            .ghost()
                            .icon(ui::icon(IconName::RefreshCw))
                            .loading(self.loading)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.refresh(window, cx)
                            })),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("ai-tasks")
                        .size_full()
                        .px_2()
                        .gap_1()
                        .overflow_y_scrollbar()
                        .when(self.tasks.is_empty(), |this| {
                            this.child(
                                div()
                                    .p_3()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(t!("ai.no_tasks")),
                            )
                        })
                        .children(self.tasks.iter().enumerate().map(|(i, t)| {
                            let active = selected == Some(t.id);
                            let id = t.id;
                            let color = match t.status.as_str() {
                                "completed" => theme.success,
                                "failed" => theme.danger,
                                "waiting_approval" => theme.warning,
                                "running" | "queued" => theme.info,
                                _ => theme.muted_foreground,
                            };
                            v_flex()
                                .id(("ai-task", i))
                                .p_2()
                                .gap_0p5()
                                .rounded(theme.radius)
                                .cursor_pointer()
                                .when(active, |this| this.bg(theme.list_active))
                                .when(!active, |this| this.hover(|s| s.bg(theme.list_hover)))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.select(Some(id), window, cx)
                                }))
                                .child(
                                    div()
                                        .text_sm()
                                        .font_medium()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(t.title.clone()),
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .items_center()
                                        .child(div().size(px(6.)).rounded_full().bg(color))
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(status_label(&t.status)),
                                        )
                                        .when(t.fan_out, |this| {
                                            this.child(ui::pill(
                                                tn!("ai.hosts_count", t.hosts),
                                                theme.info,
                                            ))
                                        })
                                        .when(t.pending > 0, |this| {
                                            this.child(ui::pill(
                                                tn!("ai.pending_approvals", t.pending),
                                                theme.warning,
                                            ))
                                        }),
                                )
                        })),
                ),
            )
    }

    fn render_new_task(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let (groups, tags) = self.scope_shortcuts(cx);
        let theme = cx.theme();
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
        let mode = self.mode;
        let mode_button =
            |id: &'static str, value: &'static str, hint: SharedString, cx: &mut Context<Self>| {
                Button::new(id)
                    .label(mode_label(value))
                    .tooltip(hint)
                    .map(|b| {
                        if mode == value {
                            b.primary()
                        } else {
                            b.ghost()
                        }
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.mode = value;
                        cx.notify();
                    }))
            };
        v_flex()
            .p_6()
            .gap_4()
            .max_w(px(820.))
            .child(div().text_xl().font_semibold().child(t!("ai.new_task")))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(if self.local {
                        t!("ai.new_task_detail_local")
                    } else {
                        t!("ai.new_task_detail")
                    }),
            )
            .child(Textarea::new(&self.prompt))
            .when_some(self.blocked.clone(), |this, text| {
                this.child(fix_banner(&text, self.open_settings(cx), cx))
            })
            .child(ui::field(
                t!("ai.permissions"),
                h_flex().gap_2().flex_wrap().children(
                    MODES
                        .iter()
                        .map(|value| mode_button(value, value, mode_hint(value), cx)),
                ),
                cx,
            ))
            .when_some(self.providers.clone(), |this, p| {
                this.child(ui::field(t!("ai.provider"), Select::new(&p), cx))
            })
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        Checkbox::new("ai-scope")
                            .label(if self.scope.is_empty() {
                                t!("ai.scope_optional")
                            } else {
                                tn!("ai.scope_limited", self.scope.len())
                            })
                            .checked(self.show_scope)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.show_scope = *v;
                                if !*v {
                                    this.scope.clear();
                                }
                                cx.notify();
                            })),
                    )
                    .when(self.show_scope, |this| {
                        this.child(self.render_scope_shortcuts(groups, tags, cx))
                    })
                    .when(self.show_scope, |this| {
                        this.child(
                            v_flex()
                                .id("ai-scope-list")
                                .max_h(px(180.))
                                .pl_6()
                                .gap_1()
                                .overflow_y_scrollbar()
                                .children(hosts.into_iter().enumerate().map(
                                    |(i, (id, label, device_only))| {
                                        Checkbox::new(("scope", i))
                                            .label(if device_only {
                                                t!("ai.scope_device_only", host = label).to_string()
                                            } else {
                                                label
                                            })
                                            .checked(self.scope.contains(&id))
                                            .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                                if *v {
                                                    this.scope.push(id);
                                                } else {
                                                    this.scope.retain(|s| *s != id);
                                                }
                                                cx.notify();
                                            }))
                                    },
                                )),
                        )
                    }),
            )
            .when(self.scope.len() > 1, |this| {
                this.child(
                    v_flex()
                        .gap_1()
                        .child(
                            Checkbox::new("ai-fan-out")
                                .label(tn!("ai.fan_out", self.scope.len()))
                                .checked(self.fan_out)
                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                    this.fan_out = *v;
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .pl_6()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(if self.fan_out {
                                    t!("ai.fan_out_hint")
                                } else {
                                    t!("ai.fan_out_off_hint")
                                }),
                        ),
                )
            })
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        Checkbox::new("ai-plan-first")
                            .label(t!("ai.plan_first"))
                            .checked(self.plan_first)
                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                this.plan_first = *v;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .pl_6()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(t!("ai.plan_first_hint")),
                    ),
            )
            .child(
                h_flex().child(
                    Button::new("ai-create")
                        .primary()
                        .icon(ui::icon(IconName::Sparkles))
                        .label(t!("ai.start"))
                        .loading(self.creating)
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| this.create(window, cx)),
                        ),
                ),
            )
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
        let theme = cx.theme();
        let header =
            summary.map(|t| {
                let running = t.running();
                let finished = matches!(t.status.as_str(), "completed" | "failed" | "cancelled");
                h_flex()
                    .px_6()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_semibold()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(t.title.clone()),
                            )
                            .child(div().text_xs().text_color(theme.muted_foreground).child(
                                format!(
                                    "{} · {} · {}{}{}",
                                    status_label(&t.status),
                                    mode_label(&t.mode),
                                    if t.provider.is_empty() {
                                        t!("ai.provider_default_lower").to_string()
                                    } else {
                                        t.provider.clone()
                                    },
                                    if t.fan_out {
                                        format!(" · {}", tn!("ai.hosts_count", t.hosts))
                                    } else {
                                        String::new()
                                    },
                                    if t.cost_micros > 0 {
                                        format!(" · {:.4} $", t.cost_micros as f64 / 1_000_000.0)
                                    } else {
                                        String::new()
                                    }
                                ),
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(ui::format_ms(t.created_at)),
                            ),
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let local = self.model.read(cx).ai_run_on() == RunOn::Local;
        let list = self.render_task_list(cx);
        let right = if self.selected.is_some() {
            self.render_detail(cx)
        } else {
            v_flex()
                .size_full()
                .child(
                    div().flex_1().min_h_0().child(
                        v_flex()
                            .size_full()
                            .overflow_y_scrollbar()
                            .child(self.render_new_task(cx)),
                    ),
                )
                .into_any_element()
        };
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
                    .children((!local).then(|| self.render_account_picker(cx)).flatten())
                    .child(div().when_some(self.error.clone(), |this, e| {
                        this.text_xs()
                            .text_color(cx.theme().danger)
                            .child(truncate(&e, 80))
                    })),
                cx,
            ))
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
