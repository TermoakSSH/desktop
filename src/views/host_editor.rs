//! Host editor (side panel): address, credentials, group, jumps, startup
//! snippet, environment, keep-alive, tags, notes and "only on this device".

use std::collections::BTreeMap;

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{Host, HostSettings, ProxyKind, ProxySettings, Record, SyncMode};

use super::OpenRequest;
use crate::state::AppModel;
use crate::ui::{self, Choice, ChoiceState, IconName};

pub enum EditorEvent {
    Close,
    Open(OpenRequest),
}

pub struct HostEditor {
    model: Entity<AppModel>,
    original: Option<Record<Host>>,
    label: Entity<InputState>,
    address: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    clear_password: bool,
    group: ChoiceState<Option<Id>>,
    identity: ChoiceState<Option<Id>>,
    key: ChoiceState<Option<Id>>,
    snippet: ChoiceState<Option<Id>>,
    jumps: Vec<Id>,
    proxy_kind: ChoiceState<Option<ProxyKind>>,
    proxy_host: Entity<InputState>,
    proxy_port: Entity<InputState>,
    proxy_user: Entity<InputState>,
    proxy_password: Entity<InputState>,
    clear_proxy_password: bool,
    env: Entity<TextareaState>,
    keepalive: Entity<InputState>,
    tags: Entity<InputState>,
    notes: Entity<TextareaState>,
    device_only: bool,
    favorite: bool,
    agent_forwarding: bool,
    record: bool,
    saving: bool,
}

impl EventEmitter<EditorEvent> for HostEditor {}

fn input(
    window: &mut Window,
    cx: &mut Context<HostEditor>,
    placeholder: SharedString,
    value: String,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(placeholder)
            .default_value(value)
    })
}

impl HostEditor {
    pub fn new(
        model: Entity<AppModel>,
        original: Option<Record<Host>>,
        default_group: Option<Id>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let h = original.as_ref().map(|r| r.data.clone());
        let s = h.as_ref().map(|h| h.settings.clone()).unwrap_or_default();
        let has_secret = original.as_ref().is_some_and(|r| r.meta.has_secret);
        let m = model.read(cx);

        let mut groups = vec![Choice::new(t!("host_editor.no_group"), None)];
        groups.extend(
            m.groups
                .iter()
                .map(|g| Choice::new(g.data.name.clone(), Some(g.data.id))),
        );
        let mut identities = vec![Choice::new(t!("host_editor.no_identity"), None)];
        identities.extend(m.identities.iter().map(|i| {
            Choice::new(
                format!("{} ({})", i.data.label, i.data.username),
                Some(i.data.id),
            )
        }));
        let mut keys = vec![Choice::new(t!("host_editor.no_key"), None)];
        keys.extend(
            m.keys
                .iter()
                .map(|k| Choice::new(k.data.label.clone(), Some(k.data.id))),
        );
        let mut snippets = vec![Choice::new(t!("common.none"), None)];
        snippets.extend(
            m.snippets
                .iter()
                .map(|sn| Choice::new(sn.data.name.clone(), Some(sn.data.id))),
        );

        let group_sel = h.as_ref().map(|h| h.group_id).unwrap_or(default_group);
        let env_text = s
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        let notes_text = h.as_ref().map(|h| h.notes.clone()).unwrap_or_default();

        let label = input(
            window,
            cx,
            t!("host_editor.label_placeholder"),
            h.as_ref().map(|h| h.label.clone()).unwrap_or_default(),
        );
        let address = input(
            window,
            cx,
            t!("host_editor.address_placeholder"),
            h.as_ref().map(|h| h.address.clone()).unwrap_or_default(),
        );
        let port = input(
            window,
            cx,
            "22".into(),
            s.port.map(|p| p.to_string()).unwrap_or_default(),
        );
        let username = input(
            window,
            cx,
            t!("host_editor.username_placeholder"),
            s.username.clone().unwrap_or_default(),
        );
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(if has_secret {
                    t!("host_editor.password_saved")
                } else {
                    t!("host_editor.password_none")
                })
        });
        let keepalive = input(
            window,
            cx,
            "30".into(),
            s.keepalive_secs.map(|k| k.to_string()).unwrap_or_default(),
        );
        let tags = input(
            window,
            cx,
            t!("host_editor.tags_placeholder"),
            h.as_ref().map(|h| h.tags.join(", ")).unwrap_or_default(),
        );
        let env = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(t!("host_editor.env_placeholder"))
                .default_value(env_text)
        });
        let notes = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 10)
                .placeholder(t!("host_editor.notes"))
                .default_value(notes_text)
        });
        let proxy = s.proxy.clone();
        let proxy_kind = ui::choice_state(
            vec![
                Choice::new(t!("host_editor.proxy.none"), None),
                Choice::new("SOCKS5", Some(ProxyKind::Socks5)),
                Choice::new("SOCKS4", Some(ProxyKind::Socks4)),
                Choice::new("HTTP (CONNECT)", Some(ProxyKind::Http)),
            ],
            Some(&proxy.as_ref().map(|p| p.kind)),
            window,
            cx,
        );
        let proxy_host = input(
            window,
            cx,
            t!("host_editor.proxy.host_placeholder"),
            proxy.as_ref().map(|p| p.host.clone()).unwrap_or_default(),
        );
        let proxy_port = input(
            window,
            cx,
            "1080".into(),
            proxy
                .as_ref()
                .map(|p| p.port.to_string())
                .unwrap_or_default(),
        );
        let proxy_user = input(
            window,
            cx,
            t!("host_editor.optional"),
            proxy
                .as_ref()
                .and_then(|p| p.username.clone())
                .unwrap_or_default(),
        );
        let proxy_password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("host_editor.proxy.password_placeholder"))
        });
        let group = ui::choice_state(groups, Some(&group_sel), window, cx);
        let identity = ui::choice_state(identities, Some(&s.identity_id), window, cx);
        let key = ui::choice_state(keys, Some(&s.key_id), window, cx);
        let snippet = ui::choice_state(snippets, Some(&s.startup_snippet_id), window, cx);
        if original.is_none() {
            label.update(cx, |i, cx| i.focus(window, cx));
        }

        Self {
            device_only: original
                .as_ref()
                .is_some_and(|r| r.meta.sync_mode == SyncMode::DeviceOnly),
            favorite: h.as_ref().is_some_and(|h| h.favorite),
            agent_forwarding: s.agent_forwarding.unwrap_or(false),
            record: s.record_sessions.unwrap_or(false),
            jumps: s.jump_host_ids.clone().unwrap_or_default(),
            proxy_kind,
            proxy_host,
            proxy_port,
            proxy_user,
            proxy_password,
            clear_proxy_password: false,
            model,
            original,
            label,
            address,
            port,
            username,
            password,
            clear_password: false,
            group,
            identity,
            key,
            snippet,
            env,
            keepalive,
            tags,
            notes,
            saving: false,
        }
    }

    pub fn host_id(&self) -> Option<Id> {
        self.original.as_ref().map(|r| r.data.id)
    }

    /// Builds the host from the form.
    fn build(
        &self,
        cx: &Context<Self>,
    ) -> Result<
        (
            Host,
            Option<Option<String>>,
            Option<Option<String>>,
            SyncMode,
        ),
        String,
    > {
        let text = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let label = text(&self.label);
        let address = text(&self.address);
        if label.is_empty() {
            return Err(t!("host_editor.error.name").to_string());
        }
        if address.is_empty() {
            return Err(t!("host_editor.error.address").to_string());
        }
        let port = match text(&self.port) {
            p if p.is_empty() => None,
            p => Some(
                p.parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or_else(|| t!("host_editor.error.port").to_string())?,
            ),
        };
        let keepalive = match text(&self.keepalive) {
            k if k.is_empty() => None,
            k => Some(
                k.parse::<u32>()
                    .map_err(|_| t!("host_editor.error.keepalive").to_string())?,
            ),
        };
        let mut env = BTreeMap::new();
        for line in self.env.read(cx).value().lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match line.split_once('=') {
                Some((k, v)) if !k.trim().is_empty() => {
                    env.insert(k.trim().to_string(), v.trim().to_string());
                }
                _ => {
                    return Err(t!("host_editor.error.env", line = line).to_string());
                }
            }
        }
        let tags: Vec<String> = text(&self.tags)
            .split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect();

        let mut host = self
            .original
            .as_ref()
            .map(|r| r.data.clone())
            .unwrap_or(Host {
                id: Id::nil(),
                label: String::new(),
                address: String::new(),
                group_id: None,
                tags: Vec::new(),
                settings: HostSettings::default(),
                notes: String::new(),
                color: None,
                os: None,
                os_version: None,
                favorite: false,
            });
        host.label = label;
        host.address = address;
        host.group_id = ui::chosen(&self.group, cx).flatten();
        host.tags = tags;
        host.notes = self.notes.read(cx).value().to_string();
        host.favorite = self.favorite;
        let s = &mut host.settings;
        s.port = port;
        s.username = Some(text(&self.username)).filter(|u| !u.is_empty());
        s.identity_id = ui::chosen(&self.identity, cx).flatten();
        s.key_id = ui::chosen(&self.key, cx).flatten();
        s.startup_snippet_id = ui::chosen(&self.snippet, cx).flatten();
        s.jump_host_ids = (!self.jumps.is_empty()).then(|| self.jumps.clone());
        s.env = env;
        s.keepalive_secs = keepalive;
        s.agent_forwarding = self.agent_forwarding.then_some(true);
        s.record_sessions = self.record.then_some(true);
        s.proxy = match ui::chosen(&self.proxy_kind, cx).flatten() {
            None => None,
            Some(kind) => {
                let host = text(&self.proxy_host);
                if host.is_empty() {
                    return Err(t!("host_editor.error.proxy_address").to_string());
                }
                let port = text(&self.proxy_port)
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p > 0)
                    .ok_or_else(|| t!("host_editor.error.proxy_port").to_string())?;
                Some(ProxySettings {
                    kind,
                    host,
                    port,
                    username: Some(text(&self.proxy_user)).filter(|u| !u.is_empty()),
                })
            }
        };

        // `None`: keep; `Some(None)`: delete.
        let password = self.password.read(cx).value().to_string();
        let password = if !password.is_empty() {
            Some(Some(password))
        } else if self.clear_password {
            Some(None)
        } else {
            None
        };
        let proxy_password = self.proxy_password.read(cx).value().to_string();
        let proxy_password = if !proxy_password.is_empty() {
            Some(Some(proxy_password))
        } else if self.clear_proxy_password || s.proxy.is_none() {
            Some(None)
        } else {
            None
        };
        let mode = if self.device_only {
            SyncMode::DeviceOnly
        } else {
            SyncMode::Synced
        };
        Ok((host, password, proxy_password, mode))
    }

    fn save(&mut self, then_connect: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (host, password, proxy_password, mode) = match self.build(cx) {
            Ok(v) => v,
            Err(e) => {
                ui::error(window, cx, e);
                return;
            }
        };
        self.saving = true;
        cx.notify();
        let task = self.model.update(cx, |m, cx| {
            m.save_host(host, password, proxy_password, Some(mode), cx)
        });
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match res {
                    Ok(rec) => {
                        let id = rec.data.id;
                        let label = rec.data.label.clone();
                        let placeholder = if rec.meta.has_secret {
                            t!("host_editor.password_saved")
                        } else {
                            t!("host_editor.password_none")
                        };
                        this.original = Some(rec);
                        this.clear_password = false;
                        this.clear_proxy_password = false;
                        this.password.update(cx, |i, cx| {
                            i.set_value("", window, cx);
                            i.set_placeholder(placeholder, window, cx);
                        });
                        this.proxy_password
                            .update(cx, |i, cx| i.set_value("", window, cx));
                        ui::success(window, cx, t!("host_editor.saved", name = label));
                        if then_connect {
                            cx.emit(EditorEvent::Open(OpenRequest::Local { host_id: id }));
                        }
                    }
                    Err(e) => ui::error(window, cx, t!("host_editor.error.save", error = e)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(rec) = self.original.clone() else {
            return;
        };
        let model = self.model.clone();
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("host_editor.delete.title"),
            t!("host_editor.delete.message", name = rec.data.label),
            t!("host_editor.delete.ok"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<Host>(rec.data.id, cx));
                let weak = weak.clone();
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(()) => {
                                if let Some(e) = weak.upgrade() {
                                    e.update(cx, |_, cx| cx.emit(EditorEvent::Close));
                                }
                            }
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
            },
        );
    }

    fn toggle_jump(&mut self, id: Id, cx: &mut Context<Self>) {
        if let Some(pos) = self.jumps.iter().position(|j| *j == id) {
            self.jumps.remove(pos);
        } else {
            self.jumps.push(id);
        }
        cx.notify();
    }
}

impl Render for HostEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let editing = self.original.is_some();
        let has_secret = self.original.as_ref().is_some_and(|r| r.meta.has_secret);
        let own_id = self.host_id();
        let other_hosts: Vec<(Id, String)> = self
            .model
            .read(cx)
            .hosts
            .iter()
            .filter(|h| Some(h.data.id) != own_id)
            .map(|h| (h.data.id, h.data.label.clone()))
            .collect();
        let title: SharedString = if editing {
            self.original
                .as_ref()
                .map(|r| r.data.label.clone())
                .unwrap_or_default()
                .into()
        } else {
            t!("host_editor.new_title")
        };

        let jumps_list = v_flex()
            .gap_1()
            .max_h(px(160.))
            .overflow_y_scrollbar()
            .children(other_hosts.iter().enumerate().map(|(i, (id, label))| {
                let id = *id;
                let pos = self.jumps.iter().position(|j| *j == id);
                Checkbox::new(("jump", i))
                    .label(match pos {
                        Some(p) => format!("{}. {label}", p + 1),
                        None => label.clone(),
                    })
                    .checked(pos.is_some())
                    .on_click(cx.listener(move |this, _: &bool, _, cx| this.toggle_jump(id, cx)))
            }));

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        ui::icon(if editing {
                            IconName::Pencil
                        } else {
                            IconName::ServerPlus
                        })
                        .size(px(16.)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .font_semibold()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(title),
                    )
                    .child(
                        Button::new("editor-close")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::X))
                            .on_click(
                                cx.listener(|_, _: &ClickEvent, _, cx| cx.emit(EditorEvent::Close)),
                            ),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("host-editor-scroll")
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(
                            v_flex()
                                .p_4()
                                .gap_4()
                                .child(ui::field(t!("common.name"), Input::new(&self.label), cx))
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(div().flex_1().child(ui::field(
                                            t!("host_editor.address"),
                                            Input::new(&self.address),
                                            cx,
                                        )))
                                        .child(div().w(px(90.)).child(ui::field(
                                            t!("host_editor.port"),
                                            Input::new(&self.port),
                                            cx,
                                        ))),
                                )
                                .child(ui::field(
                                    t!("host_editor.group"),
                                    Select::new(&self.group),
                                    cx,
                                ))
                                .child(section_title(t!("host_editor.section.credentials"), cx))
                                .child(ui::field(
                                    t!("host_editor.username"),
                                    Input::new(&self.username),
                                    cx,
                                ))
                                .child(ui::field(
                                    t!("host_editor.password"),
                                    Input::new(&self.password).mask_toggle(),
                                    cx,
                                ))
                                .when(has_secret, |this| {
                                    this.child(
                                        Checkbox::new("clear-password")
                                            .label(t!("host_editor.clear_password"))
                                            .checked(self.clear_password)
                                            .on_click(cx.listener(|this, v: &bool, _, cx| {
                                                this.clear_password = *v;
                                                cx.notify();
                                            })),
                                    )
                                })
                                .child(ui::field_with_hint(
                                    t!("host_editor.identity"),
                                    Select::new(&self.identity),
                                    t!("host_editor.identity_hint"),
                                    cx,
                                ))
                                .child(ui::field_with_hint(
                                    t!("host_editor.key"),
                                    Select::new(&self.key),
                                    t!("host_editor.key_hint"),
                                    cx,
                                ))
                                .child(section_title(t!("host_editor.section.connection"), cx))
                                .child(ui::field_with_hint(
                                    t!("host_editor.jumps"),
                                    if other_hosts.is_empty() {
                                        div()
                                            .text_sm()
                                            .text_color(theme.muted_foreground)
                                            .child(t!("host_editor.jumps_none"))
                                            .into_any_element()
                                    } else {
                                        jumps_list.into_any_element()
                                    },
                                    t!("host_editor.jumps_hint"),
                                    cx,
                                ))
                                .child(section_title(t!("host_editor.section.proxy"), cx))
                                .child(ui::field_with_hint(
                                    t!("host_editor.proxy.kind"),
                                    Select::new(&self.proxy_kind),
                                    t!("host_editor.proxy.kind_hint"),
                                    cx,
                                ))
                                .when(
                                    ui::chosen(&self.proxy_kind, cx).flatten().is_some(),
                                    |this| {
                                        this.child(
                                            h_flex()
                                                .gap_2()
                                                .child(div().flex_1().child(ui::field(
                                                    t!("host_editor.proxy.address"),
                                                    Input::new(&self.proxy_host),
                                                    cx,
                                                )))
                                                .child(div().w(px(90.)).child(ui::field(
                                                    t!("host_editor.port"),
                                                    Input::new(&self.proxy_port),
                                                    cx,
                                                ))),
                                        )
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .child(div().flex_1().child(ui::field(
                                                    t!("host_editor.username"),
                                                    Input::new(&self.proxy_user),
                                                    cx,
                                                )))
                                                .child(div().flex_1().child(ui::field(
                                                    t!("host_editor.password"),
                                                    Input::new(&self.proxy_password).mask_toggle(),
                                                    cx,
                                                ))),
                                        )
                                        .when(
                                            has_secret,
                                            |this| {
                                                this.child(
                                                    Checkbox::new("clear-proxy-password")
                                                        .label(t!(
                                                            "host_editor.proxy.clear_password"
                                                        ))
                                                        .checked(self.clear_proxy_password)
                                                        .on_click(cx.listener(
                                                            |this, v: &bool, _, cx| {
                                                                this.clear_proxy_password = *v;
                                                                cx.notify();
                                                            },
                                                        )),
                                                )
                                            },
                                        )
                                    },
                                )
                                .child(section_title(t!("host_editor.section.terminal"), cx))
                                .child(ui::field(
                                    t!("host_editor.startup_snippet"),
                                    Select::new(&self.snippet),
                                    cx,
                                ))
                                .child(ui::field(
                                    t!("host_editor.env"),
                                    Textarea::new(&self.env),
                                    cx,
                                ))
                                .child(ui::field_with_hint(
                                    t!("host_editor.keepalive"),
                                    Input::new(&self.keepalive),
                                    t!("host_editor.keepalive_hint"),
                                    cx,
                                ))
                                .child(
                                    Switch::new("agent-forwarding")
                                        .label(t!("host_editor.agent_forwarding"))
                                        .checked(self.agent_forwarding)
                                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                                            this.agent_forwarding = *v;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    Switch::new("record")
                                        .label(t!("host_editor.record"))
                                        .checked(self.record)
                                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                                            this.record = *v;
                                            cx.notify();
                                        })),
                                )
                                .child(section_title(t!("host_editor.section.organization"), cx))
                                .child(ui::field(
                                    t!("host_editor.tags"),
                                    Input::new(&self.tags),
                                    cx,
                                ))
                                .child(ui::field(
                                    t!("host_editor.notes"),
                                    Textarea::new(&self.notes),
                                    cx,
                                ))
                                .child(
                                    Switch::new("favorite")
                                        .label(t!("host_editor.favorite"))
                                        .checked(self.favorite)
                                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                                            this.favorite = *v;
                                            cx.notify();
                                        })),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            Switch::new("device-only")
                                                .label(t!("host_editor.device_only"))
                                                .checked(self.device_only)
                                                .on_click(cx.listener(|this, v: &bool, _, cx| {
                                                    this.device_only = *v;
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(t!("host_editor.device_only_hint")),
                                        ),
                                ),
                        ),
                ),
            )
            .child(
                h_flex()
                    .p_3()
                    .gap_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .when(editing, |this| {
                        this.child(
                            Button::new("delete-host")
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .tooltip(t!("host_editor.delete.title"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.delete(window, cx)
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("save-connect")
                            .icon(ui::icon(IconName::SquareTerminal))
                            .label(t!("host_editor.save_connect"))
                            .loading(self.saving)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("save-host")
                            .primary()
                            .label(t!("common.save"))
                            .loading(self.saving)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save(false, window, cx)
                            })),
                    ),
            )
    }
}

fn section_title(text: SharedString, cx: &gpui::App) -> gpui::Div {
    div()
        .pt_2()
        .text_xs()
        .font_semibold()
        .text_color(cx.theme().muted_foreground)
        .child(text.to_uppercase())
}
