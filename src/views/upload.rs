//! After adding the first account: "Upload N items from this device to your
//! Personal vault?". Every This device item is listed and can be left out;
//! the ones marked "this device only" start unchecked. Uploaded items leave
//! this device's store and sync with the account.

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::checkbox::Checkbox;
use gpui_component::scroll::ScrollableElement;
use gpui_component::{ActiveTheme, Disableable, WindowExt, h_flex, v_flex};
use termoak_core::Id;

use crate::accounts::kind_name;
use crate::state::{AppModel, DeviceItem};
use crate::ui;

/// Which items start checked: everything but "this device only" ones.
pub fn default_selection(items: &[DeviceItem]) -> Vec<bool> {
    items.iter().map(|i| !i.device_only).collect()
}

/// Offers to upload This device items to `account`, if there are any.
pub fn offer(model: Entity<AppModel>, account: Id, window: &mut Window, cx: &mut App) {
    let task = model.update(cx, |m, cx| m.device_items(cx));
    window
        .spawn(cx, async move |cx| {
            let Ok(items) = task.await else {
                return;
            };
            if items.is_empty() {
                return;
            }
            let _ = cx.update(|window, cx| {
                let view = cx.new(|_| UploadDialog::new(model, account, items));
                window.open_dialog(cx, move |d, _, _| {
                    d.title(t!("upload.title"))
                        .w(px(520.))
                        .overlay_closable(false)
                        .child(view.clone())
                });
            });
        })
        .detach();
}

struct UploadDialog {
    model: Entity<AppModel>,
    account: Id,
    items: Vec<DeviceItem>,
    checked: Vec<bool>,
    busy: bool,
}

impl UploadDialog {
    fn new(model: Entity<AppModel>, account: Id, items: Vec<DeviceItem>) -> Self {
        let checked = default_selection(&items);
        Self {
            model,
            account,
            items,
            checked,
            busy: false,
        }
    }

    fn upload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let refs: Vec<_> = self
            .items
            .iter()
            .zip(&self.checked)
            .filter(|(_, c)| **c)
            .map(|(i, _)| i.item)
            .collect();
        if refs.is_empty() {
            window.close_dialog(cx);
            return;
        }
        let n = refs.len();
        self.busy = true;
        cx.notify();
        let account = self.account;
        let task = self
            .model
            .update(cx, |m, cx| m.upload_device_items(account, refs, cx));
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match res {
                    Ok(_) => {
                        ui::success(window, cx, tn!("upload.done", n));
                        window.close_dialog(cx);
                    }
                    Err(e) => ui::error(window, cx, t!("upload.failed", error = e.text)),
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Render for UploadDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let n = self.checked.iter().filter(|c| **c).count();
        let email = self
            .model
            .read(cx)
            .account(self.account)
            .map(|a| crate::accounts::shown_name(&a.info))
            .unwrap_or_default();
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child(tn!("upload.message", self.items.len(), email = email)),
            )
            .child(
                v_flex()
                    .id("upload-items")
                    .gap_1()
                    .max_h(px(280.))
                    .overflow_y_scrollbar()
                    .p_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .children(self.items.iter().enumerate().map(|(i, item)| {
                        Checkbox::new(("upload-item", i))
                            .label(format!(
                                "{} · {}{}",
                                kind_name(item.kind),
                                item.label,
                                if item.device_only {
                                    format!(" ({})", t!("upload.device_only"))
                                } else {
                                    String::new()
                                }
                            ))
                            .checked(self.checked[i])
                            .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                this.checked[i] = *v;
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("upload.hint")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("upload-later")
                            .label(t!("upload.not_now"))
                            .on_click(|_: &ClickEvent, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("upload-now")
                            .primary()
                            .label(tn!("upload.upload", n))
                            .loading(self.busy)
                            .disabled(n == 0)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.upload(window, cx)
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_client::{ItemRef, Scope};
    use termoak_core::model::EntityKind;

    #[test]
    fn device_only_items_start_unchecked() {
        let item = |device_only| DeviceItem {
            item: ItemRef {
                scope: Scope::Device,
                id: Id::nil(),
            },
            kind: EntityKind::Host,
            label: "web".into(),
            device_only,
        };
        assert_eq!(
            default_selection(&[item(false), item(true), item(false)]),
            vec![true, false, true]
        );
    }
}
