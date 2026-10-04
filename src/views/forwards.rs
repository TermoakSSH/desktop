//! Tunnels (port forwarding): local (-L), remote (-R) and dynamic SOCKS5
//! (-D). They are saved in the vault and started or stopped with one click.

use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::Id;
use termoak_core::model::{ForwardKind, PortForward, Record, SecretUpdate};

use super::OpenRequest;
use crate::state::AppModel;
use crate::ui::{self, Choice, IconName};

pub struct ForwardsView {
    model: Entity<AppModel>,
    _ticker: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for ForwardsView {}

fn kind_label(kind: ForwardKind) -> SharedString {
    match kind {
        ForwardKind::Local => t!("forwards.kind.local"),
        ForwardKind::Remote => t!("forwards.kind.remote"),
        ForwardKind::Dynamic => t!("forwards.kind.dynamic"),
    }
}

impl ForwardsView {
    pub fn new(model: Entity<AppModel>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&model, |this, model, cx| {
            // Refresh the statistics periodically while there are active tunnels.
            if !model.read(cx).running_forwards.is_empty() && this._ticker.is_none() {
                this._ticker = Some(cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor().timer(Duration::from_secs(1)).await;
                        let keep = this
                            .update(cx, |this, cx| {
                                cx.notify();
                                !this.model.read(cx).running_forwards.is_empty()
                            })
                            .unwrap_or(false);
                        if !keep {
                            let _ = this.update(cx, |this, _| this._ticker = None);
                            break;
                        }
                    }
                }));
            }
            cx.notify();
        })];
        Self {
            model,
            _ticker: None,
            _subs: subs,
        }
    }

    fn edit(
        &mut self,
        rec: Option<Record<PortForward>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let data = rec.map(|r| r.data);
        let hosts: Vec<Choice<Option<Id>>> = self
            .model
            .read(cx)
            .hosts
            .iter()
            .map(|h| Choice::new(h.data.label.clone(), Some(h.data.id)))
            .collect();
        if hosts.is_empty() {
            ui::error(window, cx, t!("forwards.error.no_hosts"));
            return;
        }
        let first_host = hosts.first().map(|c| c.value);
        let host = ui::choice_state(
            hosts,
            data.as_ref()
                .map(|d| Some(d.host_id))
                .or(first_host)
                .as_ref(),
            window,
            cx,
        );
        let kind = ui::choice_state(
            vec![
                Choice::new(kind_label(ForwardKind::Local), ForwardKind::Local),
                Choice::new(kind_label(ForwardKind::Remote), ForwardKind::Remote),
                Choice::new(kind_label(ForwardKind::Dynamic), ForwardKind::Dynamic),
            ],
            Some(&data.as_ref().map(|d| d.kind).unwrap_or(ForwardKind::Local)),
            window,
            cx,
        );
        let text = |window: &mut Window,
                    cx: &mut Context<Self>,
                    placeholder: SharedString,
                    value: String| {
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(value)
            })
        };
        let label = text(
            window,
            cx,
            t!("forwards.form.label_placeholder"),
            data.as_ref().map(|d| d.label.clone()).unwrap_or_default(),
        );
        let bind_address = text(
            window,
            cx,
            "127.0.0.1".into(),
            data.as_ref()
                .map(|d| d.bind_address.clone())
                .unwrap_or_else(|| "127.0.0.1".into()),
        );
        let bind_port = text(
            window,
            cx,
            "8080".into(),
            data.as_ref()
                .map(|d| d.bind_port.to_string())
                .unwrap_or_default(),
        );
        let dest_host = text(
            window,
            cx,
            "localhost".into(),
            data.as_ref()
                .and_then(|d| d.dest_host.clone())
                .unwrap_or_default(),
        );
        let dest_port = text(
            window,
            cx,
            "5432".into(),
            data.as_ref()
                .and_then(|d| d.dest_port)
                .map(|p| p.to_string())
                .unwrap_or_default(),
        );
        let auto = cx.new(|_| data.as_ref().is_some_and(|d| d.auto_start));
        ui::focus_later(&label, window, cx);
        let model = self.model.clone();
        let (h2, k2, l2, ba2, bp2, dh2, dp2, a2) = (
            host.clone(),
            kind.clone(),
            label.clone(),
            bind_address.clone(),
            bind_port.clone(),
            dest_host.clone(),
            dest_port.clone(),
            auto.clone(),
        );
        ui::open_form_dialog(
            window,
            cx,
            if data.is_some() {
                t!("forwards.form.edit_title")
            } else {
                t!("forwards.form.new_title")
            },
            t!("common.save"),
            520.,
            move |_, cx| {
                let auto_on = *auto.read(cx);
                let auto_click = auto.clone();
                v_flex()
                    .gap_3()
                    .child(ui::field(t!("common.name"), Input::new(&label), cx))
                    .child(ui::field(
                        t!("forwards.form.ssh_host"),
                        Select::new(&host),
                        cx,
                    ))
                    .child(ui::field_with_hint(
                        t!("forwards.form.kind"),
                        Select::new(&kind),
                        t!("forwards.form.kind_hint"),
                        cx,
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(ui::field(
                                t!("forwards.form.bind_address"),
                                Input::new(&bind_address),
                                cx,
                            )))
                            .child(div().w(px(110.)).child(ui::field(
                                t!("forwards.form.port"),
                                Input::new(&bind_port),
                                cx,
                            ))),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(ui::field(
                                t!("forwards.form.destination"),
                                Input::new(&dest_host),
                                cx,
                            )))
                            .child(div().w(px(110.)).child(ui::field(
                                t!("forwards.form.dest_port"),
                                Input::new(&dest_port),
                                cx,
                            ))),
                    )
                    .child(
                        Checkbox::new("auto-start")
                            .label(t!("forwards.form.auto_start"))
                            .checked(auto_on)
                            .on_click(move |v, _, cx| {
                                auto_click.update(cx, |a, cx| {
                                    *a = *v;
                                    cx.notify();
                                })
                            }),
                    )
                    .into_any_element()
            },
            move |window, cx| {
                let label = l2.read(cx).value().trim().to_string();
                let Some(host_id) = ui::chosen(&h2, cx).flatten() else {
                    ui::error(window, cx, t!("forwards.error.choose_host"));
                    return false;
                };
                let kind = ui::chosen(&k2, cx).unwrap_or(ForwardKind::Local);
                let port = |s: &Entity<InputState>, cx: &gpui::App| -> Option<u16> {
                    s.read(cx).value().trim().parse::<u16>().ok()
                };
                let Some(bind_port) = port(&bp2, cx) else {
                    ui::error(window, cx, t!("forwards.error.bind_port"));
                    return false;
                };
                let dest_host =
                    Some(dh2.read(cx).value().trim().to_string()).filter(|s| !s.is_empty());
                let dest_port = port(&dp2, cx);
                let forward = PortForward {
                    id: data.as_ref().map(|d| d.id).unwrap_or(Id::nil()),
                    label,
                    host_id,
                    kind,
                    bind_address: Some(ba2.read(cx).value().trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "127.0.0.1".into()),
                    bind_port,
                    dest_host: if kind == ForwardKind::Dynamic {
                        None
                    } else {
                        dest_host
                    },
                    dest_port: if kind == ForwardKind::Dynamic {
                        None
                    } else {
                        dest_port
                    },
                    auto_start: *a2.read(cx),
                };
                let task = model.update(cx, |m, cx| m.save(forward, SecretUpdate::Keep, None, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
                true
            },
        );
    }

    fn delete(&mut self, rec: Record<PortForward>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("forwards.delete.title"),
            t!("forwards.delete.message", name = rec.data.label),
            t!("forwards.delete.ok"),
            true,
            move |window, cx| {
                let id = rec.data.id;
                model.update(cx, |m, cx| m.stop_forward(id, cx));
                let task = model.update(cx, |m, cx| m.delete::<PortForward>(id, cx));
                window
                    .spawn(cx, async move |cx| {
                        if let Err(e) = task.await {
                            let _ = cx.update(|window, cx| ui::error(window, cx, e));
                        }
                    })
                    .detach();
            },
        );
    }
}

impl Render for ForwardsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let m = self.model.read(cx);
        let forwards = m.forwards.clone();
        let rows: Vec<_> = forwards
            .into_iter()
            .map(|rec| {
                let running = m
                    .running_forwards
                    .get(&rec.data.id)
                    .map(|r| (r.bound_port, r.stats()));
                let starting = m.starting_forwards.contains(&rec.data.id);
                let host = m.host_label(rec.data.host_id);
                (rec, running, starting, host)
            })
            .collect();
        let theme = cx.theme();
        let list: gpui::AnyElement = if rows.is_empty() {
            ui::empty_state(
                IconName::ArrowLeftRight,
                t!("forwards.empty.title"),
                t!("forwards.empty.detail"),
                cx,
            )
            .into_any_element()
        } else {
            v_flex()
                .gap_2()
                .children(rows.into_iter().enumerate().map(
                    |(i, (rec, running, starting, host))| {
                        let f = rec.data.clone();
                        let bind = format!("{}:{}", f.bind_address, f.bind_port);
                        let dest = format!(
                            "{}:{}",
                            f.dest_host.clone().unwrap_or_default(),
                            f.dest_port.unwrap_or(0)
                        );
                        let route = match f.kind {
                            ForwardKind::Local => {
                                t!(
                                    "forwards.route.local",
                                    bind = bind,
                                    dest = dest,
                                    host = host
                                )
                            }
                            ForwardKind::Remote => {
                                t!(
                                    "forwards.route.remote",
                                    host = host,
                                    bind = bind,
                                    dest = dest
                                )
                            }
                            ForwardKind::Dynamic => {
                                t!("forwards.route.dynamic", bind = bind, host = host)
                            }
                        };
                        let (r_edit, r_del, f_start) = (rec.clone(), rec.clone(), f.clone());
                        let id = f.id;
                        h_flex()
                            .id(("forward", i))
                            .p_3()
                            .gap_3()
                            .items_center()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(if running.is_some() {
                                theme.success
                            } else {
                                theme.border
                            })
                            .bg(theme.secondary)
                            .child(ui::icon(IconName::ArrowLeftRight).size(px(20.)).text_color(
                                if running.is_some() {
                                    theme.success
                                } else {
                                    theme.muted_foreground
                                },
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .child(
                                                div()
                                                    .font_semibold()
                                                    .text_sm()
                                                    .child(f.label.clone()),
                                            )
                                            .child(ui::pill(kind_label(f.kind), theme.primary))
                                            .when(f.auto_start, |this| {
                                                this.child(ui::pill(
                                                    t!("forwards.auto"),
                                                    theme.info,
                                                ))
                                            }),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(route),
                                    )
                                    .when_some(running.clone(), |this, (port, stats)| {
                                        this.child(div().text_xs().text_color(theme.success).child(
                                            t!(
                                                "forwards.stats",
                                                port = port,
                                                total = stats.total_connections,
                                                open = stats.active_connections,
                                                down = ui::format_bytes(stats.bytes_in),
                                                up = ui::format_bytes(stats.bytes_out)
                                            ),
                                        ))
                                    }),
                            )
                            .child(if running.is_some() {
                                Button::new(("stop-forward", i))
                                    .small()
                                    .danger()
                                    .icon(ui::icon(IconName::CircleStop))
                                    .label(t!("forwards.stop"))
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.model.update(cx, |m, cx| m.stop_forward(id, cx));
                                    }))
                            } else {
                                Button::new(("start-forward", i))
                                    .small()
                                    .primary()
                                    .icon(ui::icon(IconName::Play))
                                    .label(t!("forwards.start"))
                                    .loading(starting)
                                    .disabled(starting)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        let f = f_start.clone();
                                        this.model.update(cx, |m, cx| m.start_forward(f, None, cx));
                                    }))
                            })
                            .child(
                                Button::new(("edit-forward", i))
                                    .small()
                                    .ghost()
                                    .icon(ui::icon(IconName::Pencil))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.edit(Some(r_edit.clone()), window, cx)
                                        },
                                    )),
                            )
                            .child(
                                Button::new(("delete-forward", i))
                                    .small()
                                    .ghost()
                                    .icon(ui::icon(IconName::Trash))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.delete(r_del.clone(), window, cx)
                                        },
                                    )),
                            )
                    },
                ))
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("forwards.title"),
                t!("forwards.subtitle"),
                Button::new("new-forward")
                    .primary()
                    .icon(ui::icon(IconName::Plus))
                    .label(t!("forwards.form.new_title"))
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.edit(None, window, cx)),
                    ),
                cx,
            ))
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().p_6().child(list)),
                ),
            )
    }
}
