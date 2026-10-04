//! Settings → AI: where the AI runs on this device.
//!
//! - **Your Termoak account** (server): the account's AI status (own keys or
//!   the plan's monthly credit) and its own API keys (`/me/ai/...`).
//! - **This computer**: your own API keys stored in this app (local vault)
//!   or an agent installed here (Codex, Claude Code, Antigravity, OpenCode).
//!
//! Saved keys are never shown, only `•••• hint`.

use std::collections::HashMap;

use gpui::{
    AnyElement, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Subscription, Window,
    div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::menu::{DropdownMenu, PopupMenuItem};
use gpui_component::progress::Progress;
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use termoak_client::api::{AiAccess, AiKeyInfo};

use crate::local_ai::agents::{self, AgentStatus};
use crate::local_ai::keys::{self, LocalKeyInfo};
use crate::local_ai::{self, LocalSource, LocalTarget, RunOn};
use crate::runtime;
use crate::state::{AppModel, ModelEvent, api_error};
use crate::ui::{self, IconName};

/// Where a key lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Backend {
    /// In the Termoak account (`/me/ai/keys`).
    Server,
    /// In this app's local vault.
    Local,
}

impl Backend {
    fn id(self) -> &'static str {
        match self {
            Backend::Server => "server",
            Backend::Local => "local",
        }
    }
}

/// Operation running on a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Save,
    Test,
    Delete,
}

/// A provider's row: its data and the inputs for the key and the model.
struct KeyRow {
    provider: String,
    label: String,
    models: Vec<String>,
    key: Entity<InputState>,
    model: Entity<InputState>,
}

impl KeyRow {
    fn new(
        provider: &str,
        label: String,
        default_model: Option<String>,
        models: Vec<String>,
        window: &mut Window,
        cx: &mut Context<AiSettingsView>,
    ) -> Self {
        let key = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("ai_settings.keys.key_placeholder"))
        });
        let placeholder = match &default_model {
            Some(m) => t!("ai_settings.keys.model_default", model = m),
            None => t!("ai_settings.keys.model_placeholder"),
        };
        let model = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        Self {
            provider: provider.to_string(),
            label,
            models,
            key,
            model,
        }
    }
}

pub struct AiSettingsView {
    model: Entity<AppModel>,
    // ----- Account (server) -----
    access: Option<AiAccess>,
    server_keys: Vec<AiKeyInfo>,
    server_rows: Vec<KeyRow>,
    server_error: Option<String>,
    loading_server: bool,
    // ----- This computer -----
    local_keys: Vec<LocalKeyInfo>,
    /// Spent this month by the AI on this computer (micro-USD).
    local_spent: Option<i64>,
    local_rows: Vec<KeyRow>,
    /// Agents found (`None` = not looked for yet).
    agents: Option<Vec<AgentStatus>>,
    detecting: bool,
    /// Operation running (only one at a time).
    busy: Option<(Backend, String, Op)>,
    /// Result of the last test of each key.
    tests: HashMap<(Backend, String), Result<(), String>>,
    _subs: Vec<Subscription>,
}

impl AiSettingsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let local_rows = keys::PROVIDERS
            .iter()
            .map(|p| {
                let (default_model, models) = keys::provider_models(p);
                KeyRow::new(
                    p,
                    keys::provider_label(p).to_string(),
                    default_model,
                    models,
                    window,
                    cx,
                )
            })
            .collect();
        let subs = vec![
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.access = None;
                    this.server_keys.clear();
                    this.server_rows.clear();
                    this.server_error = None;
                    this.tests.retain(|(b, _), _| *b == Backend::Local);
                    this.refresh(window, cx);
                }
            }),
            cx.observe(&model, |_, _, cx| cx.notify()),
        ];
        let mut view = Self {
            model,
            access: None,
            server_keys: Vec::new(),
            server_rows: Vec::new(),
            server_error: None,
            loading_server: false,
            local_keys: Vec::new(),
            local_spent: None,
            local_rows,
            agents: None,
            detecting: false,
            busy: None,
            tests: HashMap::new(),
            _subs: subs,
        };
        view.load_local_keys(window, cx);
        view
    }

    /// Reloads everything (when the page is shown).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_server(window, cx);
        self.load_local_keys(window, cx);
        if self.agents.is_none() {
            self.detect_agents(cx);
        }
    }

    fn row(&self, backend: Backend, provider: &str) -> Option<&KeyRow> {
        let rows = match backend {
            Backend::Server => &self.server_rows,
            Backend::Local => &self.local_rows,
        };
        rows.iter().find(|r| r.provider == provider)
    }

    /// Saved hint and model of a key (`None` = no key).
    fn saved(&self, backend: Backend, provider: &str) -> Option<(String, Option<String>)> {
        match backend {
            Backend::Server => self
                .server_keys
                .iter()
                .find(|k| k.provider == provider)
                .map(|k| (k.hint.clone(), k.model.clone())),
            Backend::Local => self
                .local_keys
                .iter()
                .find(|k| k.provider == provider)
                .map(|k| (k.hint.clone(), k.model.clone())),
        }
    }

    // ----- Loading -----

    fn load_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api.clone() else {
            return;
        };
        self.loading_server = true;
        cx.notify();
        runtime::run_in(
            cx,
            window,
            async move {
                let access = api.ai_access().await.map_err(api_error)?;
                let keys = api.ai_keys().await.map_err(api_error)?;
                Ok::<_, String>((access, keys))
            },
            |this, res, window, cx| {
                this.loading_server = false;
                match res {
                    Ok((access, keys)) => {
                        // Keep the rows (and what is being typed) of the
                        // providers that are still there.
                        let mut old: Vec<KeyRow> = std::mem::take(&mut this.server_rows);
                        for p in &access.providers {
                            let row = match old.iter().position(|r| r.provider == p.provider) {
                                Some(ix) => old.swap_remove(ix),
                                None => KeyRow::new(
                                    &p.provider,
                                    if p.label.is_empty() {
                                        p.provider.clone()
                                    } else {
                                        p.label.clone()
                                    },
                                    p.default_model.clone(),
                                    p.models.clone(),
                                    window,
                                    cx,
                                ),
                            };
                            this.server_rows.push(row);
                        }
                        this.server_keys = keys;
                        this.access = Some(access);
                        this.server_error = None;
                        this.fill_models(Backend::Server, window, cx);
                    }
                    Err(e) => this.server_error = Some(e),
                }
                cx.notify();
            },
        );
    }

    fn load_local_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.model.read(cx).ws.store.clone();
        runtime::run_in(
            cx,
            window,
            async move {
                let list = keys::list(&store).await?;
                let spent = crate::local_ai::copilot::spent_this_month(&store)
                    .await
                    .ok();
                Ok::<_, termoak_core::CoreError>((list, spent))
            },
            |this, res, window, cx| {
                match res {
                    Ok((list, spent)) => {
                        this.local_keys = list;
                        this.local_spent = spent;
                        this.fill_models(Backend::Local, window, cx);
                    }
                    Err(e) => ui::error(window, cx, t!("ai_settings.keys.load_failed", error = e)),
                }
                cx.notify();
            },
        );
    }

    /// Puts each saved model in its empty input (what is being typed in
    /// the others stays).
    fn fill_models(&mut self, backend: Backend, window: &mut Window, cx: &mut Context<Self>) {
        let rows = match backend {
            Backend::Server => &self.server_rows,
            Backend::Local => &self.local_rows,
        };
        let values: Vec<(Entity<InputState>, String)> = rows
            .iter()
            .filter(|r| r.model.read(cx).value().trim().is_empty())
            .filter_map(|r| {
                let saved = self.saved(backend, &r.provider).and_then(|(_, m)| m)?;
                Some((r.model.clone(), saved))
            })
            .collect();
        for (input, value) in values {
            input.update(cx, |i, cx| i.set_value(value, window, cx));
        }
    }

    fn detect_agents(&mut self, cx: &mut Context<Self>) {
        if self.detecting {
            return;
        }
        self.detecting = true;
        cx.notify();
        runtime::run(
            cx,
            async move { Ok::<_, std::convert::Infallible>(agents::detect().await) },
            |this, res, cx| {
                this.detecting = false;
                if let Ok(found) = res {
                    this.agents = Some(found);
                }
                cx.notify();
            },
        );
    }

    // ----- Choices -----

    fn update_ai(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut local_ai::AiSettings)) {
        self.model.update(cx, |m, cx| {
            let mut ai = m.settings.ai.clone();
            f(&mut ai);
            m.set_ai_settings(ai, cx);
        });
    }

    fn set_run_on(&mut self, run_on: RunOn, cx: &mut Context<Self>) {
        if run_on == RunOn::Server && !self.model.read(cx).logged_in() {
            return;
        }
        self.update_ai(cx, |ai| ai.run_on = Some(run_on));
    }

    /// Uses an installed agent. Antigravity (experimental) is turned on only
    /// after a warning: it can run commands on this computer without
    /// Termoak's approvals.
    fn choose_agent(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        let choose = move |this: &mut Self, cx: &mut Context<Self>| {
            this.update_ai(cx, |ai| {
                ai.local_source = Some(LocalSource::Agent);
                ai.local_agent = Some(id.to_string());
                if id == "agy" {
                    ai.agy_opt_in = true;
                }
            });
        };
        if id != "agy" || self.model.read(cx).settings.ai.agy_opt_in {
            choose(self, cx);
            return;
        }
        let me = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("ai_settings.agents.agy_confirm_title"),
            t!("ai_settings.agents.agy_confirm_message"),
            t!("ai_settings.agents.agy_confirm_ok"),
            true,
            move |_, cx| {
                let _ = me.update(cx, |this, cx| choose(this, cx));
            },
        );
    }

    fn set_local_source(&mut self, source: LocalSource, cx: &mut Context<Self>) {
        self.update_ai(cx, |ai| ai.local_source = Some(source));
        if source == LocalSource::Agent && self.agents.is_none() {
            self.detect_agents(cx);
        }
    }

    // ----- Keys -----

    fn start(&mut self, backend: Backend, provider: &str, op: Op, cx: &mut Context<Self>) -> bool {
        if self.busy.is_some() {
            return false;
        }
        self.busy = Some((backend, provider.to_string(), op));
        cx.notify();
        true
    }

    fn save_key(
        &mut self,
        backend: Backend,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row(backend, &provider) else {
            return;
        };
        let key = row.key.read(cx).value().trim().to_string();
        let model = row.model.read(cx).value().trim().to_string();
        let has_saved = self.saved(backend, &provider).is_some();
        if key.is_empty() && !has_saved {
            ui::error(window, cx, t!("ai_settings.keys.key_required"));
            return;
        }
        if !key.is_empty() && !keys::valid_key(&key) {
            ui::error(window, cx, t!("ai_settings.keys.invalid_key"));
            return;
        }
        if !self.start(backend, &provider, Op::Save, cx) {
            return;
        }
        let provider_task = provider.clone();
        let key_given = !key.is_empty();
        let done = move |this: &mut Self,
                         res: Result<(), String>,
                         window: &mut Window,
                         cx: &mut Context<Self>| {
            this.busy = None;
            match res {
                Ok(()) => {
                    if let Some(row) = this.row(backend, &provider) {
                        let input = row.key.clone();
                        input.update(cx, |i, cx| i.set_value("", window, cx));
                    }
                    this.tests.remove(&(backend, provider.clone()));
                    ui::success(
                        window,
                        cx,
                        if key_given {
                            t!("ai_settings.keys.saved")
                        } else {
                            t!("ai_settings.keys.model_saved")
                        },
                    );
                    match backend {
                        Backend::Server => this.load_server(window, cx),
                        Backend::Local => this.load_local_keys(window, cx),
                    }
                }
                Err(e) => ui::error(window, cx, t!("ai_settings.keys.save_failed", error = e)),
            }
            cx.notify();
        };
        match backend {
            Backend::Server => {
                let Some(api) = self.model.read(cx).api.clone() else {
                    self.busy = None;
                    return;
                };
                runtime::run_in(
                    cx,
                    window,
                    async move {
                        let key = Some(key).filter(|k| !k.is_empty());
                        api.set_ai_key(&provider_task, key.as_deref(), Some(&model))
                            .await
                            .map(|_| ())
                            .map_err(api_error)
                    },
                    done,
                );
            }
            Backend::Local => {
                let store = self.model.read(cx).ws.store.clone();
                runtime::run_in(
                    cx,
                    window,
                    async move {
                        let key = Some(key).filter(|k| !k.is_empty());
                        keys::set(&store, &provider_task, key.as_deref(), Some(&model))
                            .await
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    },
                    done,
                );
            }
        }
    }

    fn test_key(
        &mut self,
        backend: Backend,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row(backend, &provider) else {
            return;
        };
        let key = row.key.read(cx).value().trim().to_string();
        if key.is_empty() && self.saved(backend, &provider).is_none() {
            ui::error(window, cx, t!("ai_settings.keys.key_required"));
            return;
        }
        if !self.start(backend, &provider, Op::Test, cx) {
            return;
        }
        let provider_task = provider.clone();
        let done = move |this: &mut Self,
                         res: Result<Result<(), String>, String>,
                         _: &mut Window,
                         cx: &mut Context<Self>| {
            this.busy = None;
            this.tests
                .insert((backend, provider.clone()), res.and_then(|r| r));
            cx.notify();
        };
        match backend {
            Backend::Server => {
                let Some(api) = self.model.read(cx).api.clone() else {
                    self.busy = None;
                    return;
                };
                runtime::run_in(
                    cx,
                    window,
                    async move {
                        let key = Some(key).filter(|k| !k.is_empty());
                        let test = api
                            .test_ai_key(&provider_task, key.as_deref())
                            .await
                            .map_err(api_error)?;
                        Ok::<_, String>(if test.ok {
                            Ok(())
                        } else if matches!(test.status, Some(401 | 403)) {
                            Err(t!("ai_settings.keys.test_rejected").to_string())
                        } else {
                            Err(test
                                .error
                                .unwrap_or_else(|| t!("ai_settings.keys.test_failed").to_string()))
                        })
                    },
                    done,
                );
            }
            Backend::Local => {
                let store = self.model.read(cx).ws.store.clone();
                runtime::run_in(
                    cx,
                    window,
                    async move {
                        let key = match Some(key).filter(|k| !k.is_empty()) {
                            Some(k) => zeroize::Zeroizing::new(k),
                            None => match keys::secret(&store, &provider_task)
                                .await
                                .map_err(|e| e.to_string())?
                            {
                                Some(s) => s.key,
                                None => {
                                    return Err(t!("ai_settings.keys.key_required").to_string());
                                }
                            },
                        };
                        Ok::<_, String>(keys::check(&provider_task, &key).await)
                    },
                    done,
                );
            }
        }
    }

    fn delete_key(
        &mut self,
        backend: Backend,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = self
            .row(backend, &provider)
            .map(|r| r.label.clone())
            .unwrap_or_else(|| provider.clone());
        let me = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("ai_settings.keys.delete_title"),
            match backend {
                Backend::Server => t!("ai_settings.keys.delete_message_server", provider = label),
                Backend::Local => t!("ai_settings.keys.delete_message_local", provider = label),
            },
            t!("common.delete"),
            true,
            move |window, cx| {
                let provider = provider.clone();
                let _ = me.update(cx, |this, cx| {
                    this.really_delete_key(backend, provider, window, cx)
                });
            },
        );
    }

    fn really_delete_key(
        &mut self,
        backend: Backend,
        provider: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.start(backend, &provider, Op::Delete, cx) {
            return;
        }
        let provider_task = provider.clone();
        let done = move |this: &mut Self,
                         res: Result<bool, String>,
                         window: &mut Window,
                         cx: &mut Context<Self>| {
            this.busy = None;
            this.tests.remove(&(backend, provider.clone()));
            match res {
                Ok(_) => {
                    ui::success(window, cx, t!("ai_settings.keys.deleted"));
                    if let Some(row) = this.row(backend, &provider) {
                        let input = row.model.clone();
                        input.update(cx, |i, cx| i.set_value("", window, cx));
                    }
                    match backend {
                        Backend::Server => this.load_server(window, cx),
                        Backend::Local => this.load_local_keys(window, cx),
                    }
                }
                Err(e) => ui::error(window, cx, t!("ai_settings.keys.delete_failed", error = e)),
            }
            cx.notify();
        };
        match backend {
            Backend::Server => {
                let Some(api) = self.model.read(cx).api.clone() else {
                    self.busy = None;
                    return;
                };
                runtime::run_in(
                    cx,
                    window,
                    async move { api.delete_ai_key(&provider_task).await.map_err(api_error) },
                    done,
                );
            }
            Backend::Local => {
                let store = self.model.read(cx).ws.store.clone();
                runtime::run_in(
                    cx,
                    window,
                    async move { keys::delete(&store, &provider_task).await },
                    done,
                );
            }
        }
    }

    // ----- Rendering -----

    fn card(
        &self,
        title: impl Into<SharedString>,
        icon: IconName,
        cx: &Context<Self>,
    ) -> gpui::Div {
        ui::card(cx).p_5().gap_4().w_full().max_w(px(760.)).child(
            h_flex()
                .gap_2()
                .items_center()
                .child(ui::icon(icon).size(px(18.)).text_color(cx.theme().primary))
                .child(div().text_lg().font_semibold().child(title.into())),
        )
    }

    /// A choice with an explanation (radio-like card).
    #[allow(clippy::too_many_arguments)]
    fn option(
        &self,
        id: &'static str,
        selected: bool,
        disabled: bool,
        icon: IconName,
        title: SharedString,
        detail: SharedString,
        extra: Option<SharedString>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        h_flex()
            .id(id)
            .flex_1()
            .min_w(px(240.))
            .items_start()
            .gap_3()
            .p_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(if selected {
                theme.primary
            } else {
                theme.border
            })
            .when(selected, |this| this.bg(theme.list_active))
            .when(disabled, |this| this.opacity(0.55))
            .when(!disabled, |this| {
                this.cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| on_click(this, cx)))
            })
            .child(
                ui::icon(if selected {
                    IconName::CircleCheck
                } else {
                    IconName::Circle
                })
                .size(px(16.))
                .mt_0p5()
                .text_color(if selected {
                    theme.primary
                } else {
                    theme.muted_foreground
                }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(ui::icon(icon).size(px(16.)))
                            .child(div().font_semibold().child(title)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(detail),
                    )
                    .when_some(extra, |this, e| {
                        this.child(div().text_xs().text_color(theme.warning).child(e))
                    }),
            )
            .into_any_element()
    }

    /// A note with an icon (privacy, what happens when the app closes...).
    fn note(
        icon: IconName,
        text: SharedString,
        color: gpui::Hsla,
        cx: &Context<Self>,
    ) -> gpui::Div {
        h_flex()
            .gap_2()
            .items_start()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(
                ui::icon(icon)
                    .size(px(14.))
                    .mt_0p5()
                    .flex_shrink_0()
                    .text_color(color),
            )
            .child(div().flex_1().min_w_0().child(text))
    }

    fn render_where(&self, cx: &mut Context<Self>) -> AnyElement {
        let m = self.model.read(cx);
        let logged_in = m.logged_in();
        let run_on = m.ai_run_on();
        self.card(t!("ai_settings.where.title"), IconName::Sparkles, cx)
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(t!("ai_settings.where.intro")),
            )
            .child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .items_stretch()
                    .child(self.option(
                        "ai-run-server",
                        run_on == RunOn::Server,
                        !logged_in,
                        IconName::Cloud,
                        t!("ai_settings.where.server"),
                        t!("ai_settings.where.server_detail"),
                        (!logged_in).then(|| t!("ai_settings.where.server_needs_login")),
                        |this, cx| this.set_run_on(RunOn::Server, cx),
                        cx,
                    ))
                    .child(self.option(
                        "ai-run-local",
                        run_on == RunOn::Local,
                        false,
                        IconName::Laptop,
                        t!("ai_settings.where.local"),
                        t!("ai_settings.where.local_detail"),
                        None,
                        |this, cx| this.set_run_on(RunOn::Local, cx),
                        cx,
                    )),
            )
            .into_any_element()
    }

    fn render_server(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut card = self.card(t!("ai_settings.server.title"), IconName::Cloud, cx);
        card = card.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("ai_settings.server.intro")),
                )
                .child(
                    Button::new("ai-server-refresh")
                        .small()
                        .ghost()
                        .icon(ui::icon(IconName::RefreshCw))
                        .tooltip(t!("ai_settings.refresh"))
                        .loading(self.loading_server)
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.load_server(window, cx)
                        })),
                ),
        );
        if let Some(e) = &self.server_error {
            card = card.child(div().text_sm().text_color(theme.danger).child(e.clone()));
        }
        if let Some(access) = &self.access {
            card = card.child(self.render_access(access, cx));
            card = card.child(
                v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_semibold()
                            .child(t!("ai_settings.server.keys_title")),
                    )
                    .children(
                        self.server_rows
                            .iter()
                            .enumerate()
                            .map(|(i, r)| self.render_key_row(Backend::Server, i, r, false, cx)),
                    ),
            );
        } else if self.loading_server {
            card = card.child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("ai_settings.loading")),
            );
        }
        card.child(Self::note(
            IconName::Lock,
            t!("ai_settings.server.privacy"),
            cx.theme().success,
            cx,
        ))
        .into_any_element()
    }

    /// Status of the account: own keys and the plan's AI credit.
    fn render_access(&self, access: &AiAccess, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let own = if access.own_keys.is_empty() {
            None
        } else {
            let labels: Vec<String> = access
                .own_keys
                .iter()
                .map(|p| {
                    access
                        .providers
                        .iter()
                        .find(|x| &x.provider == p)
                        .map(|x| x.label.clone())
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| p.clone())
                })
                .collect();
            Some(t!(
                "ai_settings.server.own_keys",
                providers = labels.join(", ")
            ))
        };
        let mut status = v_flex()
            .gap_2()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border);
        if !access.server_ai {
            status = status
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(ui::icon(IconName::KeyRound).size(px(16.)))
                        .child(div().font_medium().child(t!("ai_settings.server.free"))),
                )
                .when(access.own_keys.is_empty(), |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.warning)
                            .child(t!("ai_settings.server.free_no_keys")),
                    )
                });
        } else if let Some(credit) = access.credit_usd {
            let spent = access.spent_usd.max(0.0);
            let left = access
                .remaining_usd
                .unwrap_or((credit - spent).max(0.0))
                .max(0.0);
            let pct = if credit > 0.0 {
                (spent / credit * 100.0).clamp(0.0, 100.0) as f32
            } else {
                100.0
            };
            let color = if left <= 0.0 {
                theme.danger
            } else if pct >= 80.0 {
                theme.warning
            } else {
                theme.primary
            };
            status = status
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(ui::icon(IconName::Gauge).size(px(16.)))
                        .child(
                            div()
                                .font_medium()
                                .child(t!("ai_settings.server.credit_title")),
                        ),
                )
                .child(
                    Progress::new("ai-credit")
                        .value(pct)
                        .color(color)
                        .accessibility_label(t!("ai_settings.server.credit_title")),
                )
                .child(div().text_sm().child(t!(
                    "ai_settings.server.credit_usage",
                    spent = usd(spent),
                    credit = usd(credit),
                    left = usd(left)
                )))
                .when(left <= 0.0, |this| {
                    this.child(
                        div()
                            .text_sm()
                            .text_color(theme.danger)
                            .child(t!("ai_settings.server.credit_spent")),
                    )
                });
        } else {
            status = status.child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(ui::icon(IconName::Sparkles).size(px(16.)))
                    .child(div().font_medium().child(t!("ai_settings.server.no_cap"))),
            );
        }
        status
            .when_some(own, |this, own| {
                this.child(div().text_sm().text_color(theme.success).child(own))
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("ai_settings.server.own_keys_first")),
            )
            .into_any_element()
    }

    /// A provider: saved key (only its hint), key and model inputs and the
    /// actions. `choose` adds "Use this one" (local keys).
    fn render_key_row(
        &self,
        backend: Backend,
        ix: usize,
        row: &KeyRow,
        choose: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let saved = self.saved(backend, &row.provider);
        let busy_op = self
            .busy
            .as_ref()
            .filter(|(b, p, _)| *b == backend && *p == row.provider)
            .map(|(_, _, op)| *op);
        let any_busy = self.busy.is_some();
        let test = self.tests.get(&(backend, row.provider.clone())).cloned();
        let id = |what: &str| SharedString::from(format!("ai-{}-{what}-{ix}", backend.id()));
        let provider = row.provider.clone();
        let chosen = choose
            && matches!(
                self.local_target(cx),
                Some(LocalTarget::ApiKey { provider: p, .. }) if p == row.provider
            );
        let model_input = row.model.clone();
        let models = row.models.clone();
        v_flex()
            .gap_2()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(if chosen { theme.primary } else { theme.border })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(div().font_semibold().child(row.label.clone()))
                    .child(match &saved {
                        Some((hint, _)) => ui::pill(
                            t!("ai_settings.keys.saved_hint", hint = hint),
                            theme.success,
                        ),
                        None => ui::pill(t!("ai_settings.keys.none"), theme.muted_foreground),
                    })
                    .when(chosen, |this| {
                        this.child(ui::pill(t!("ai_settings.local.in_use"), theme.primary))
                    })
                    .child(div().flex_1())
                    .when(choose && saved.is_some() && !chosen, |this| {
                        let provider = provider.clone();
                        this.child(
                            Button::new(id("use"))
                                .xsmall()
                                .ghost()
                                .label(t!("ai_settings.local.use_this"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    let provider = provider.clone();
                                    this.update_ai(cx, |ai| {
                                        ai.local_source = Some(LocalSource::ApiKey);
                                        ai.local_provider = Some(provider);
                                    });
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .flex_wrap()
                    .child(div().flex_1().min_w(px(220.)).child(ui::field(
                        match &saved {
                            Some(_) => t!("ai_settings.keys.replace_label"),
                            None => t!("ai_settings.keys.key_label"),
                        },
                        Input::new(&row.key).mask_toggle(),
                        cx,
                    )))
                    .child(
                        div().w(px(240.)).child(ui::field(
                            t!("ai_settings.keys.model_label"),
                            h_flex()
                                .gap_1()
                                .child(div().flex_1().min_w_0().child(Input::new(&row.model)))
                                .when(!models.is_empty(), |this| {
                                    this.child(
                                        Button::new(id("models"))
                                            .ghost()
                                            .icon(ui::icon(IconName::ChevronDown))
                                            .tooltip(t!("ai_settings.keys.suggested_models"))
                                            .dropdown_menu(move |mut menu, _, _| {
                                                for m in &models {
                                                    let input = model_input.clone();
                                                    let value = m.clone();
                                                    menu = menu.item(
                                                        PopupMenuItem::new(m.clone()).on_click(
                                                            move |_, window, cx| {
                                                                let value = value.clone();
                                                                input.update(cx, |i, cx| {
                                                                    i.set_value(value, window, cx)
                                                                });
                                                            },
                                                        ),
                                                    );
                                                }
                                                menu
                                            }),
                                    )
                                }),
                            cx,
                        )),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child({
                        let provider = provider.clone();
                        Button::new(id("save"))
                            .small()
                            .primary()
                            .icon(ui::icon(IconName::Save))
                            .label(t!("common.save"))
                            .loading(busy_op == Some(Op::Save))
                            .disabled(any_busy && busy_op != Some(Op::Save))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.save_key(backend, provider.clone(), window, cx)
                            }))
                    })
                    .child({
                        let provider = provider.clone();
                        Button::new(id("test"))
                            .small()
                            .icon(ui::icon(IconName::PlugZap))
                            .label(t!("ai_settings.keys.test"))
                            .tooltip(t!("ai_settings.keys.test_hint"))
                            .loading(busy_op == Some(Op::Test))
                            .disabled(any_busy && busy_op != Some(Op::Test))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.test_key(backend, provider.clone(), window, cx)
                            }))
                    })
                    .when(saved.is_some(), |this| {
                        let provider = provider.clone();
                        this.child(
                            Button::new(id("delete"))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .label(t!("common.delete"))
                                .loading(busy_op == Some(Op::Delete))
                                .disabled(any_busy && busy_op != Some(Op::Delete))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.delete_key(backend, provider.clone(), window, cx)
                                })),
                        )
                    })
                    .when_some(test, |this, res| {
                        let (icon, color, text) = match res {
                            Ok(()) => (
                                IconName::CircleCheck,
                                theme.success,
                                t!("ai_settings.keys.test_ok").to_string(),
                            ),
                            Err(e) => (IconName::CircleX, theme.danger, e),
                        };
                        this.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .min_w_0()
                                .text_xs()
                                .text_color(color)
                                .child(ui::icon(icon).size(px(14.)))
                                .child(div().min_w_0().child(ui::capitalize(&text))),
                        )
                    }),
            )
            .into_any_element()
    }

    fn local_target(&self, cx: &Context<Self>) -> Option<LocalTarget> {
        let agents = self.agents.as_deref().unwrap_or(&[]);
        local_ai::local_target(&self.model.read(cx).settings.ai, &self.local_keys, agents)
    }

    fn render_local(&self, cx: &mut Context<Self>) -> AnyElement {
        let ai = self.model.read(cx).settings.ai.clone();
        let has_agent = self
            .agents
            .as_ref()
            .is_some_and(|a| a.iter().any(|x| x.path.is_some()));
        let source = ai.local_source(!self.local_keys.is_empty(), has_agent);
        let theme = cx.theme();
        let target = self.local_target(cx);
        let summary = match &target {
            Some(LocalTarget::ApiKey { provider, model }) => {
                let model = model
                    .clone()
                    .or_else(|| keys::provider_models(provider).0)
                    .unwrap_or_default();
                (
                    IconName::CircleCheck,
                    theme.success,
                    t!(
                        "ai_settings.local.ready_key",
                        provider = keys::provider_label(provider),
                        model = model
                    ),
                )
            }
            Some(LocalTarget::Agent { agent, .. }) => (
                IconName::CircleCheck,
                theme.success,
                t!(
                    "ai_settings.local.ready_agent",
                    agent = agents::kind(agent).map(|a| a.name).unwrap_or(agent)
                ),
            ),
            None if source == LocalSource::Agent && self.agents.is_none() => (
                IconName::Loader,
                theme.muted_foreground,
                t!("ai_settings.agents.detecting"),
            ),
            None => (
                IconName::TriangleAlert,
                theme.warning,
                match source {
                    LocalSource::ApiKey => t!("ai_settings.local.missing_key"),
                    LocalSource::Agent => t!("ai_settings.local.missing_agent"),
                },
            ),
        };
        let mut card = self
            .card(t!("ai_settings.local.title"), IconName::Laptop, cx)
            .child(
                v_flex()
                    .gap_1()
                    .p_3()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.info)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                ui::icon(IconName::Info)
                                    .size(px(16.))
                                    .text_color(theme.info),
                            )
                            .child(
                                div()
                                    .font_medium()
                                    .text_sm()
                                    .child(t!("ai_settings.local.coming_title")),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_settings.local.coming_detail")),
                    ),
            )
            .child(Self::note(
                IconName::TriangleAlert,
                t!("ai_settings.local.stops_note"),
                theme.warning,
                cx,
            ))
            .child(
                h_flex()
                    .gap_3()
                    .flex_wrap()
                    .items_stretch()
                    .child(self.option(
                        "ai-source-keys",
                        source == LocalSource::ApiKey,
                        false,
                        IconName::KeyRound,
                        t!("ai_settings.local.source_keys"),
                        t!("ai_settings.local.source_keys_detail"),
                        None,
                        |this, cx| this.set_local_source(LocalSource::ApiKey, cx),
                        cx,
                    ))
                    .child(self.option(
                        "ai-source-agent",
                        source == LocalSource::Agent,
                        false,
                        IconName::Bot,
                        t!("ai_settings.local.source_agent"),
                        t!("ai_settings.local.source_agent_detail"),
                        None,
                        |this, cx| this.set_local_source(LocalSource::Agent, cx),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .text_color(summary.1)
                    .child(ui::icon(summary.0).size(px(16.)))
                    .child(summary.2),
            )
            .when_some(self.local_spent.filter(|s| *s > 0), |this, micros| {
                this.child(div().text_xs().text_color(theme.muted_foreground).child(t!(
                    "ai_settings.local.spent",
                    amount = usd(micros as f64 / 1_000_000.0)
                )))
            });
        card = match source {
            LocalSource::ApiKey => card
                .children(
                    self.local_rows
                        .iter()
                        .enumerate()
                        .map(|(i, r)| self.render_key_row(Backend::Local, i, r, true, cx)),
                )
                .child(Self::note(
                    IconName::Lock,
                    t!("ai_settings.local.keys_privacy"),
                    theme.success,
                    cx,
                )),
            LocalSource::Agent => card.child(self.render_agents(cx)),
        };
        card.into_any_element()
    }

    fn render_agents(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let target = self.local_target(cx);
        let selected = match &target {
            Some(LocalTarget::Agent { agent, .. }) => Some(*agent),
            _ => None,
        };
        let list = self.agents.clone().unwrap_or_default();
        let agy_on = self.model.read(cx).settings.ai.agy_opt_in;
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("ai_settings.agents.intro")),
                    )
                    .child(
                        Button::new("ai-agents-detect")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("ai_settings.agents.detect"))
                            .loading(self.detecting)
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.detect_agents(cx)),
                            ),
                    ),
            )
            .children(agents::AGENTS.iter().enumerate().map(|(i, kind)| {
                let status = list.iter().find(|s| s.id == kind.id);
                let path = status.and_then(|s| s.path.clone());
                let version = status.and_then(|s| s.version.clone());
                let installed = path.is_some();
                let is_selected = selected == Some(kind.id);
                let id = kind.id;
                let url = kind.install_url;
                h_flex()
                    .id(("ai-agent", i))
                    .gap_3()
                    .p_3()
                    .items_start()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(if is_selected {
                        theme.primary
                    } else {
                        theme.border
                    })
                    .when(is_selected, |this| this.bg(theme.list_active))
                    .when(installed && !is_selected, |this| {
                        this.cursor_pointer()
                            .hover(|s| s.bg(theme.list_hover))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.choose_agent(id, window, cx);
                            }))
                    })
                    .child(
                        ui::icon(if is_selected {
                            IconName::CircleCheck
                        } else {
                            IconName::Circle
                        })
                        .size(px(16.))
                        .mt_0p5()
                        .text_color(if installed {
                            theme.primary
                        } else {
                            theme.muted_foreground
                        }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_wrap()
                                    .child(div().font_semibold().child(kind.name))
                                    .when(id == "agy", |this| {
                                        this.child(ui::pill(
                                            t!("ai_settings.agents.experimental"),
                                            theme.warning,
                                        ))
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_family(ui::mono_family(cx))
                                            .text_color(theme.muted_foreground)
                                            .child(kind.command),
                                    )
                                    .child(match (&path, &version) {
                                        (Some(_), Some(v)) => ui::pill(
                                            t!("ai_settings.agents.installed_version", version = v),
                                            theme.success,
                                        ),
                                        (Some(_), None) => ui::pill(
                                            t!("ai_settings.agents.installed"),
                                            theme.success,
                                        ),
                                        (None, _) if self.agents.is_none() => ui::pill(
                                            t!("ai_settings.agents.detecting"),
                                            theme.muted_foreground,
                                        ),
                                        (None, _) => ui::pill(
                                            t!("ai_settings.agents.not_installed"),
                                            theme.muted_foreground,
                                        ),
                                    })
                                    .when(is_selected, |this| {
                                        this.child(ui::pill(
                                            t!("ai_settings.local.in_use"),
                                            theme.primary,
                                        ))
                                    }),
                            )
                            .when_some(path.clone(), |this, p| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .font_family(ui::mono_family(cx))
                                        .text_color(theme.muted_foreground)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(p.display().to_string()),
                                )
                            })
                            .when(installed, |this| {
                                this.child(
                                    div().text_xs().text_color(theme.muted_foreground).child(t!(
                                        "ai_settings.agents.login_hint",
                                        command = kind.login_command
                                    )),
                                )
                            }),
                    )
                    .when(id == "agy" && agy_on, |this| {
                        this.child(
                            Button::new("ai-agy-off")
                                .xsmall()
                                .ghost()
                                .label(t!("ai_settings.agents.agy_turn_off"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.update_ai(cx, |ai| {
                                        ai.agy_opt_in = false;
                                        if ai.local_agent.as_deref() == Some("agy") {
                                            ai.local_agent = None;
                                        }
                                    });
                                })),
                        )
                    })
                    .when(!installed && self.agents.is_some(), |this| {
                        this.child(
                            Button::new(("ai-agent-install", i))
                                .xsmall()
                                .ghost()
                                .icon(ui::icon(IconName::ExternalLink))
                                .label(t!("ai_settings.agents.install"))
                                .on_click(move |_: &ClickEvent, _, cx| cx.open_url(url)),
                        )
                    })
            }))
            .into_any_element()
    }
}

/// Amount in dollars with cents (`$1.25`).
fn usd(v: f64) -> String {
    format!("${v:.2}")
}

impl Render for AiSettingsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let run_on = self.model.read(cx).ai_run_on();
        let where_ = self.render_where(cx);
        let panel = match run_on {
            RunOn::Server => self.render_server(cx),
            RunOn::Local => self.render_local(cx),
        };
        div().flex_1().min_h_0().size_full().child(
            v_flex()
                .size_full()
                .overflow_y_scrollbar()
                .child(v_flex().p_6().gap_5().child(where_).child(panel)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts() {
        assert_eq!(usd(1.25), "$1.25");
        assert_eq!(usd(5.0), "$5.00");
        assert_eq!(usd(0.004), "$0.00");
    }
}
