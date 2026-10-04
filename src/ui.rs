//! Reusable interface pieces: form dialogs, confirmations, notifications,
//! form rows, headers, empty states and dropdowns.

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, Div, Entity, Focusable, IntoElement, ParentElement, SharedString,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::notification::Notification;
use gpui_component::searchable_list::SearchableListItem;
use gpui_component::select::SelectState;
use gpui_component::{ActiveTheme, Icon, IndexPath, StyledExt, WindowExt, h_flex, v_flex};

pub use gpui_kit_assets::IconName;

use crate::state::ToastKind;

/// Monospace family of the theme.
pub fn mono_family(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}

/// Icon from the full Lucide catalog.
pub fn icon(name: IconName) -> Icon {
    Icon::from(name)
}

/// Shows a notification in the window.
pub fn notify(window: &mut Window, cx: &mut App, kind: ToastKind, msg: impl Into<SharedString>) {
    let msg = msg.into();
    let note = match kind {
        ToastKind::Info => Notification::info(msg),
        ToastKind::Success => Notification::success(msg),
        ToastKind::Warning => Notification::warning(msg),
        ToastKind::Error => Notification::error(msg),
    };
    window.push_notification(note, cx);
}

/// Error notification.
pub fn error(window: &mut Window, cx: &mut App, msg: impl Into<SharedString>) {
    notify(window, cx, ToastKind::Error, msg);
}

/// Success notification.
pub fn success(window: &mut Window, cx: &mut App, msg: impl Into<SharedString>) {
    notify(window, cx, ToastKind::Success, msg);
}

/// Opens a dialog with a form. `on_ok` returns `true` to close it.
/// Enter accepts and Escape cancels.
pub fn open_form_dialog(
    window: &mut Window,
    cx: &mut App,
    title: impl Into<SharedString>,
    ok_label: impl Into<SharedString>,
    width: f32,
    body: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    on_ok: impl Fn(&mut Window, &mut App) -> bool + 'static,
) {
    let title = title.into();
    let ok_label = ok_label.into();
    let on_ok = Rc::new(on_ok);
    let body = Rc::new(body);
    window.open_dialog(cx, move |dialog, window, cx| {
        let ok_enter = on_ok.clone();
        let ok_click = on_ok.clone();
        dialog
            .title(title.clone())
            .w(px(width))
            .overlay_closable(false)
            .child(body(window, cx))
            .on_ok(move |_, window, cx| ok_enter(window, cx))
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("dialog-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("dialog-ok")
                            .primary()
                            .label(ok_label.clone())
                            .on_click(move |_, window, cx| {
                                if ok_click(window, cx) {
                                    window.close_dialog(cx);
                                }
                            }),
                    ),
            )
    });
}

/// Focuses a field when the current cycle ends (e.g. after opening a
/// dialog, which takes the focus when it opens).
pub fn focus_later<T: Focusable + 'static>(entity: &Entity<T>, window: &mut Window, cx: &mut App) {
    let entity = entity.clone();
    window.defer(cx, move |window, cx| {
        let handle = entity.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    });
}

/// Confirmation dialog (e.g. before deleting).
pub fn confirm(
    window: &mut Window,
    cx: &mut App,
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
    ok_label: impl Into<SharedString>,
    danger: bool,
    on_ok: impl Fn(&mut Window, &mut App) + 'static,
) {
    let title = title.into();
    let message = message.into();
    let ok_label = ok_label.into();
    let on_ok = Rc::new(on_ok);
    window.open_dialog(cx, move |dialog, _, _| {
        let ok_enter = on_ok.clone();
        let ok_click = on_ok.clone();
        dialog
            .title(title.clone())
            .w(px(420.))
            .child(div().child(message.clone()))
            .on_ok(move |_, window, cx| {
                ok_enter(window, cx);
                true
            })
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("confirm-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("confirm-ok")
                            .label(ok_label.clone())
                            .map(|b| if danger { b.danger() } else { b.primary() })
                            .on_click(move |_, window, cx| {
                                window.close_dialog(cx);
                                ok_click(window, cx);
                            }),
                    ),
            )
    });
}

/// Form field: label above the control.
pub fn field(label: impl Into<SharedString>, control: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .gap_1()
        .w_full()
        .child(
            div()
                .text_xs()
                .font_medium()
                .text_color(cx.theme().muted_foreground)
                .child(label.into()),
        )
        .child(control)
}

/// Field with a help text below.
pub fn field_with_hint(
    label: impl Into<SharedString>,
    control: impl IntoElement,
    hint: impl Into<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .gap_1()
        .w_full()
        .child(field(label, control, cx))
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(hint.into()),
        )
}

/// Header of a section: title, subtitle and actions on the right.
pub fn section_header(
    title: impl Into<SharedString>,
    subtitle: impl Into<SharedString>,
    actions: impl IntoElement,
    cx: &App,
) -> Div {
    h_flex()
        .w_full()
        .flex_wrap()
        .items_center()
        .justify_between()
        .gap_4()
        .px_6()
        .py_4()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(
            v_flex()
                .flex_shrink_0()
                .gap_0p5()
                .child(div().text_xl().font_semibold().child(title.into()))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(subtitle.into()),
                ),
        )
        .child(h_flex().flex_wrap().gap_2().items_center().child(actions))
}

/// Empty state with an icon, a title and an explanation.
pub fn empty_state(
    icon_name: IconName,
    title: impl Into<SharedString>,
    detail: impl Into<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .w_full()
        .py_16()
        .gap_3()
        .items_center()
        .justify_center()
        .text_color(cx.theme().muted_foreground)
        .child(icon(icon_name).size(px(40.)))
        .child(
            div()
                .text_lg()
                .font_semibold()
                .text_color(cx.theme().foreground)
                .child(title.into()),
        )
        .child(
            div()
                .text_sm()
                .max_w(px(460.))
                .text_center()
                .child(detail.into()),
        )
}

/// Card with a border (container of lists and forms).
pub fn card(cx: &App) -> Div {
    v_flex()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary)
}

/// Small colored label (system, active tunnel...).
pub fn pill(text: impl Into<SharedString>, color: gpui::Hsla) -> Div {
    let mut bg = color;
    bg.a = 0.18;
    div()
        .px_1p5()
        .py_0p5()
        .rounded(px(4.))
        .bg(bg)
        .text_color(color)
        .text_xs()
        .font_medium()
        .child(text.into())
}

/// Dropdown item with its own value.
#[derive(Clone)]
pub struct Choice<V: Clone + PartialEq + 'static> {
    pub label: SharedString,
    pub value: V,
}

impl<V: Clone + PartialEq + 'static> Choice<V> {
    pub fn new(label: impl Into<SharedString>, value: V) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

impl<V: Clone + PartialEq + 'static> SearchableListItem for Choice<V> {
    type Value = V;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// State of a dropdown.
pub type ChoiceState<V> = Entity<SelectState<Vec<Choice<V>>>>;

/// Creates the state of a dropdown with the `selected` option checked.
pub fn choice_state<V: Clone + PartialEq + 'static>(
    items: Vec<Choice<V>>,
    selected: Option<&V>,
    window: &mut Window,
    cx: &mut App,
) -> ChoiceState<V> {
    let ix = selected
        .and_then(|v| items.iter().position(|c| &c.value == v))
        .map(IndexPath::new);
    cx.new(|cx| SelectState::new(items, ix, window, cx))
}

/// Selected value of a dropdown.
pub fn chosen<V: Clone + PartialEq + 'static>(state: &ChoiceState<V>, cx: &App) -> Option<V> {
    state.read(cx).selected_value().cloned()
}

/// Readable size (`1.5 MB`).
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Local date and time from Unix milliseconds, in the format of the
/// interface language (`format.datetime`, a `chrono` format string).
pub fn format_ms(ms: i64) -> String {
    let format = t!("format.datetime");
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format(&format).to_string())
        .unwrap_or_default()
}

/// Local date and time from Unix seconds.
pub fn format_secs(secs: i64) -> String {
    format_ms(secs * 1000)
}

/// The text with its first letter in uppercase (server errors come in
/// lowercase so that they can follow a colon).
pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn capitalize_first_letter() {
        assert_eq!(super::capitalize("the code is wrong"), "The code is wrong");
        assert_eq!(super::capitalize("ñandú"), "Ñandú");
        assert_eq!(super::capitalize(""), "");
    }
}
