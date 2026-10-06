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
use serde_json::{Value, json};
use termoak_core::Id;
use termoak_core::model::SyncMode;

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
    creating: bool,
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
            creating: false,
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
        self.creating = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move { Ok::<_, std::convert::Infallible>(backend.create(body).await) },
            |this, res, window, cx| {
                this.creating = false;
                match ai_result(res) {
                    Ok(v) => {
                        this.blocked = None;
                        this.prompt.update(cx, |i, cx| i.set_value("", window, cx));
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
            None if ty == "status" => self.load_tasks(window, cx),
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
        let theme = cx.theme();
        let header =
            summary.map(|t| {
                let running = t.running();
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
                                    "{} · {} · {}{}",
                                    status_label(&t.status),
                                    mode_label(&t.mode),
                                    if t.provider.is_empty() {
                                        t!("ai.provider_default_lower").to_string()
                                    } else {
                                        t.provider.clone()
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
                                .label(t!("common.cancel"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.chat.update(cx, |c, cx| c.cancel(window, cx))
                                })),
                        )
                    })
                    .child(
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
            });
        v_flex()
            .size_full()
            .children(header)
            .child(div().flex_1().min_h_0().child(self.chat.clone()))
            .into_any_element()
    }
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
