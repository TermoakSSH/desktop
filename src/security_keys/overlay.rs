//! The security key dialogs: "plug in your key", the device picker, the
//! PIN and "touch your key" (with the host, user, key and purpose, and
//! Cancel).
//!
//! [`SkBridge`] is the [`SkUi`] of the app: the flow runs on tokio and
//! sends its requests through a channel; the app shows them in the window
//! that gets the notices, drawn above everything else (`render`), so a
//! request is never hidden by another dialog and is closed by the flow
//! itself when the device answers, the request is cancelled or decided
//! elsewhere (`security_key_done`). A request of the server when no key is
//! plugged in is shown as a card in a corner that does not block the
//! window (another device, such as a phone, may answer it).

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, Global, InteractiveElement,
    IntoElement, ParentElement, SharedString, Styled, Subscription, Window, WindowId, div,
    prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::spinner::Spinner;
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use termoak_ssh::sk::SkErrorCode;
use tokio::sync::{mpsc, oneshot};

use super::{DeviceInfo, SkPurpose, SkUi, Stage, Status, key_text, purpose_text, target_text};
use crate::ui::{self, IconName};

/// A request of the flow to the interface.
pub enum SkUiRequest {
    Status {
        id: u64,
        purpose: SkPurpose,
        stage: Stage,
        cancel: oneshot::Sender<()>,
    },
    Close {
        id: u64,
    },
    Pick {
        purpose: SkPurpose,
        devices: Vec<DeviceInfo>,
        reply: oneshot::Sender<Option<usize>>,
    },
    Pin {
        purpose: SkPurpose,
        retry: Option<SkErrorCode>,
        reply: oneshot::Sender<Option<String>>,
    },
}

/// The [`SkUi`] of the app: requests through a channel to the window.
pub struct SkBridge {
    tx: mpsc::UnboundedSender<SkUiRequest>,
    next: AtomicU64,
}

impl SkBridge {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<SkUiRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                next: AtomicU64::new(1),
            },
            rx,
        )
    }
}

/// Closes a status when dropped.
struct CloseOnDrop {
    id: u64,
    tx: mpsc::UnboundedSender<SkUiRequest>,
}

impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.tx.send(SkUiRequest::Close { id: self.id });
    }
}

#[async_trait]
impl SkUi for SkBridge {
    fn status(&self, purpose: &SkPurpose, stage: Stage) -> Status {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (cancel, rx) = oneshot::channel();
        let _ = self.tx.send(SkUiRequest::Status {
            id,
            purpose: purpose.clone(),
            stage,
            cancel,
        });
        Status::new(
            rx,
            CloseOnDrop {
                id,
                tx: self.tx.clone(),
            },
        )
    }

    async fn pick_device(&self, purpose: &SkPurpose, devices: &[DeviceInfo]) -> Option<usize> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(SkUiRequest::Pick {
            purpose: purpose.clone(),
            devices: devices.to_vec(),
            reply,
        });
        rx.await.ok().flatten()
    }

    async fn pin(&self, purpose: &SkPurpose, retry: Option<SkErrorCode>) -> Option<String> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(SkUiRequest::Pin {
            purpose: purpose.clone(),
            retry,
            reply,
        });
        rx.await.ok().flatten()
    }
}

enum Step {
    Status {
        id: u64,
        stage: Stage,
        cancel: Option<oneshot::Sender<()>>,
    },
    Pick {
        devices: Vec<DeviceInfo>,
        reply: Option<oneshot::Sender<Option<usize>>>,
    },
    Pin {
        retry: bool,
        input: Entity<InputState>,
        reply: Option<oneshot::Sender<Option<String>>>,
        _sub: Subscription,
    },
}

/// What is on screen now.
pub struct SkOverlay {
    current: Option<(SkPurpose, Step)>,
    /// The window that shows it.
    window: Option<WindowId>,
}

struct OverlayGlobal(Entity<SkOverlay>);

impl Global for OverlayGlobal {}

/// Creates the overlay (before the first window).
pub fn init(cx: &mut App) {
    let e = cx.new(|_| SkOverlay {
        current: None,
        window: None,
    });
    cx.set_global(OverlayGlobal(e));
}

/// The overlay entity (windows observe it to redraw).
pub fn overlay(cx: &App) -> Option<Entity<SkOverlay>> {
    cx.try_global::<OverlayGlobal>().map(|g| g.0.clone())
}

/// Shows a request of the flow in `window` (the notice window).
pub fn show(req: SkUiRequest, window: &mut Window, cx: &mut App) {
    let Some(o) = overlay(cx) else { return };
    let win = window.window_handle().window_id();
    match req {
        SkUiRequest::Status {
            id,
            purpose,
            stage,
            cancel,
        } => o.update(cx, |o, cx| {
            o.window = Some(win);
            o.current = Some((
                purpose,
                Step::Status {
                    id,
                    stage,
                    cancel: Some(cancel),
                },
            ));
            cx.notify();
        }),
        SkUiRequest::Close { id } => o.update(cx, |o, cx| {
            if matches!(&o.current, Some((_, Step::Status { id: i, .. })) if *i == id) {
                o.current = None;
                cx.notify();
            }
        }),
        SkUiRequest::Pick {
            purpose,
            devices,
            reply,
        } => o.update(cx, |o, cx| {
            o.window = Some(win);
            o.current = Some((
                purpose,
                Step::Pick {
                    devices,
                    reply: Some(reply),
                },
            ));
            cx.notify();
        }),
        SkUiRequest::Pin {
            purpose,
            retry,
            reply,
        } => {
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder(t!("security_keys.overlay.pin_placeholder"))
            });
            ui::focus_later(&input, window, cx);
            let weak = o.downgrade();
            let sub = window.subscribe(&input, cx, move |_, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::PressEnter { .. })
                    && let Some(o) = weak.upgrade()
                {
                    o.update(cx, |o, cx| o.submit_pin(cx));
                }
            });
            o.update(cx, |o, cx| {
                o.window = Some(win);
                o.current = Some((
                    purpose,
                    Step::Pin {
                        retry: retry.is_some(),
                        input,
                        reply: Some(reply),
                        _sub: sub,
                    },
                ));
                cx.notify();
            });
        }
    }
}

impl SkOverlay {
    /// Cancel: the flow stops (the device is told to stop waiting).
    fn cancel(&mut self, cx: &mut Context<Self>) {
        match self.current.take() {
            Some((_, Step::Status { cancel, .. })) => {
                if let Some(c) = cancel {
                    let _ = c.send(());
                }
            }
            Some((_, Step::Pick { reply, .. })) => {
                if let Some(r) = reply {
                    let _ = r.send(None);
                }
            }
            Some((_, Step::Pin { reply, .. })) => {
                if let Some(r) = reply {
                    let _ = r.send(None);
                }
            }
            None => {}
        }
        cx.notify();
    }

    fn pick(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some((_, Step::Pick { reply, .. })) = self.current.take()
            && let Some(r) = reply
        {
            let _ = r.send(Some(ix));
        }
        cx.notify();
    }

    fn submit_pin(&mut self, cx: &mut Context<Self>) {
        let value = match &self.current {
            Some((_, Step::Pin { input, .. })) => input.read(cx).value().to_string(),
            _ => return,
        };
        if value.is_empty() {
            return;
        }
        if let Some((_, Step::Pin { reply, .. })) = self.current.take()
            && let Some(r) = reply
        {
            let _ = r.send(Some(value));
        }
        cx.notify();
    }
}

/// One "label: value" row of the request.
fn row(label: SharedString, value: String, cx: &App) -> impl IntoElement {
    h_flex()
        .gap_3()
        .items_start()
        .child(
            div()
                .w(px(92.))
                .flex_shrink_0()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().flex_1().min_w_0().text_sm().child(value))
}

/// What the request is about.
fn details(purpose: &SkPurpose, cx: &App) -> Vec<AnyElement> {
    let mut rows = Vec::new();
    match purpose {
        SkPurpose::Sign(c) => {
            rows.push(row(t!("security_keys.overlay.host"), target_text(c), cx).into_any_element());
            rows.push(row(t!("security_keys.overlay.key"), key_text(c), cx).into_any_element());
        }
        SkPurpose::Create {
            application,
            algorithm,
        } => {
            rows.push(
                row(
                    t!("security_keys.overlay.algorithm"),
                    algorithm.ssh_name().to_string(),
                    cx,
                )
                .into_any_element(),
            );
            rows.push(
                row(
                    t!("security_keys.overlay.application"),
                    application.clone(),
                    cx,
                )
                .into_any_element(),
            );
        }
        SkPurpose::LoadResident => {}
    }
    rows.push(
        row(
            t!("security_keys.overlay.purpose"),
            purpose_text(purpose),
            cx,
        )
        .into_any_element(),
    );
    rows
}

/// The overlay for `window`, if it shows one (drawn by `AppView` above its
/// dialogs).
pub fn render(window: &Window, cx: &mut App) -> Option<AnyElement> {
    let o = overlay(cx)?;
    let state = o.read(cx);
    let (purpose, step) = state.current.as_ref()?;
    if state.window != Some(window.window_handle().window_id()) {
        return None;
    }
    let theme = cx.theme();
    let (icon, title, text): (IconName, SharedString, SharedString) = match step {
        Step::Status { stage, .. } => match stage {
            Stage::Insert => (
                IconName::Usb,
                t!("security_keys.overlay.insert_title"),
                if purpose.forwarded() {
                    t!("security_keys.overlay.insert_forwarded")
                } else {
                    t!("security_keys.overlay.insert_text")
                },
            ),
            Stage::Touch => (
                IconName::FingerprintPattern,
                if matches!(purpose, SkPurpose::Create { .. }) {
                    t!("security_keys.overlay.create_title")
                } else {
                    t!("security_keys.overlay.touch_title")
                },
                if cfg!(windows) {
                    t!("security_keys.overlay.touch_text_windows")
                } else {
                    t!("security_keys.overlay.touch_text")
                },
            ),
            Stage::Working => (
                IconName::KeyRound,
                t!("security_keys.overlay.working_title"),
                t!("security_keys.overlay.working_text"),
            ),
        },
        Step::Pick { .. } => (
            IconName::Usb,
            t!("security_keys.overlay.pick_title"),
            t!("security_keys.overlay.pick_text"),
        ),
        Step::Pin { retry, .. } => (
            IconName::LockKeyhole,
            t!("security_keys.overlay.pin_title"),
            if *retry {
                t!("security_keys.overlay.pin_wrong")
            } else {
                t!("security_keys.overlay.pin_text")
            },
        ),
    };
    let wrong_pin = matches!(step, Step::Pin { retry: true, .. });
    let waiting = matches!(step, Step::Status { .. });
    let modal = !matches!(
        step,
        Step::Status {
            stage: Stage::Insert,
            ..
        }
    ) || !purpose.forwarded();
    let mut body = v_flex()
        .gap_3()
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(
                    div()
                        .size(px(40.))
                        .flex_shrink_0()
                        .rounded(theme.radius)
                        .bg(theme.accent)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(ui::icon(icon).size(px(22.)).text_color(theme.primary)),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(div().font_semibold().child(title))
                        .child(
                            div()
                                .text_sm()
                                .text_color(if wrong_pin {
                                    theme.danger
                                } else {
                                    theme.muted_foreground
                                })
                                .child(text),
                        ),
                )
                .when(waiting, |this| this.child(Spinner::new().small())),
        )
        .child(
            v_flex()
                .gap_1p5()
                .p_3()
                .rounded(theme.radius)
                .bg(theme.muted)
                .children(details(purpose, cx)),
        );
    let weak = o.downgrade();
    match step {
        Step::Pick { devices, .. } => {
            body = body.child(v_flex().gap_1().children(devices.iter().enumerate().map(
                |(ix, d)| {
                    let weak = weak.clone();
                    Button::new(("sk-device", ix))
                        .w_full()
                        .icon(ui::icon(IconName::Usb))
                        .label(d.label.clone())
                        .on_click(move |_: &ClickEvent, _, cx| {
                            if let Some(o) = weak.upgrade() {
                                o.update(cx, |o, cx| o.pick(ix, cx));
                            }
                        })
                },
            )));
        }
        Step::Pin { input, .. } => {
            body = body.child(Input::new(input).mask_toggle());
        }
        Step::Status { .. } => {}
    }
    let cancel_weak = weak.clone();
    let mut buttons = h_flex().w_full().justify_end().gap_2().child(
        Button::new("sk-cancel")
            .label(t!("common.cancel"))
            .on_click(move |_: &ClickEvent, _, cx| {
                if let Some(o) = cancel_weak.upgrade() {
                    o.update(cx, |o, cx| o.cancel(cx));
                }
            }),
    );
    if matches!(step, Step::Pin { .. }) {
        let ok_weak = weak.clone();
        buttons = buttons.child(
            Button::new("sk-pin-ok")
                .primary()
                .label(t!("security_keys.overlay.pin_ok"))
                .on_click(move |_: &ClickEvent, _, cx| {
                    if let Some(o) = ok_weak.upgrade() {
                        o.update(cx, |o, cx| o.submit_pin(cx));
                    }
                }),
        );
    }
    let card = v_flex()
        .id("security-key-card")
        .occlude()
        .w(px(440.))
        .p_5()
        .gap_4()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .child(body)
        .child(buttons);
    Some(if modal {
        div()
            .id("security-key-overlay")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .occlude()
            .bg(gpui::hsla(0., 0., 0., 0.45))
            .flex()
            .items_center()
            .justify_center()
            .child(card)
            .into_any_element()
    } else {
        div()
            .absolute()
            .top(px(52.))
            .right(px(16.))
            .child(card)
            .into_any_element()
    })
}
