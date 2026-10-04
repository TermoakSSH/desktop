//! Known hosts: server keys you trust (the equivalent of
//! `~/.ssh/known_hosts`). If a server changes its key, the old one must be
//! deleted to trust it again.

use gpui::{
    AppContext, ClickEvent, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_core::model::{KnownHost, Record};

use super::OpenRequest;
use crate::state::AppModel;
use crate::ui::{self, IconName};

pub struct KnownHostsView {
    model: Entity<AppModel>,
    search: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRequest> for KnownHostsView {}

impl KnownHostsView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder(t!("known_hosts.search")));
        let subs = vec![
            cx.observe(&model, |_, _, cx| cx.notify()),
            cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            model,
            search,
            _subs: subs,
        }
    }

    fn delete(&mut self, rec: Record<KnownHost>, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model.clone();
        ui::confirm(
            window,
            cx,
            t!("known_hosts.forget_title"),
            t!(
                "known_hosts.forget_confirm",
                key_type = rec.data.key_type,
                host = rec.data.host,
                port = rec.data.port
            ),
            t!("known_hosts.forget"),
            true,
            move |window, cx| {
                let task = model.update(cx, |m, cx| m.delete::<KnownHost>(rec.data.id, cx));
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

impl Render for KnownHostsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().trim().to_lowercase();
        let mut list: Vec<Record<KnownHost>> = self
            .model
            .read(cx)
            .known_hosts
            .iter()
            .filter(|k| {
                query.is_empty()
                    || k.data.host.to_lowercase().contains(&query)
                    || k.data.fingerprint.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        list.sort_by(|a, b| {
            a.data
                .host
                .cmp(&b.data.host)
                .then(a.data.port.cmp(&b.data.port))
        });
        let empty = list.is_empty();
        let theme = cx.theme();
        let body: gpui::AnyElement = if empty {
            ui::empty_state(
                IconName::ShieldCheck,
                t!("known_hosts.empty_title"),
                t!("known_hosts.empty_detail"),
                cx,
            )
            .into_any_element()
        } else {
            v_flex()
                .gap_2()
                .children(list.into_iter().enumerate().map(|(i, rec)| {
                    let k = rec.data.clone();
                    h_flex()
                        .id(("known-host", i))
                        .p_3()
                        .gap_3()
                        .items_center()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.secondary)
                        .child(
                            ui::icon(IconName::ShieldCheck)
                                .size(px(20.))
                                .text_color(theme.success),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .items_center()
                                        .child(
                                            div()
                                                .font_semibold()
                                                .text_sm()
                                                .child(format!("{}:{}", k.host, k.port)),
                                        )
                                        .child(ui::pill(k.key_type.clone(), theme.primary)),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .font_family(ui::mono_family(cx))
                                        .text_color(theme.muted_foreground)
                                        .child(k.fingerprint.clone()),
                                ),
                        )
                        .child(
                            Button::new(("forget", i))
                                .small()
                                .ghost()
                                .icon(ui::icon(IconName::Trash))
                                .label(t!("known_hosts.forget"))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.delete(rec.clone(), window, cx)
                                })),
                        )
                }))
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("known_hosts.title"),
                t!("known_hosts.subtitle"),
                div()
                    .w(px(280.))
                    .child(Input::new(&self.search).cleanable(true)),
                cx,
            ))
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .size_full()
                        .overflow_y_scrollbar()
                        .child(div().p_6().child(body)),
                ),
            )
    }
}
