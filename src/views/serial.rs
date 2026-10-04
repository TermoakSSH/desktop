//! Dialog to open a serial port as a terminal: detected ports (or a path
//! typed by hand) and speed.

use std::rc::Rc;

use gpui::{
    App, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, div, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::select::Select;
use gpui_component::{ActiveTheme, WindowExt, h_flex, v_flex};

use crate::terminal::serial::{self, SerialParams};
use crate::ui::{self, Choice, ChoiceState};

type OnOpen = Rc<dyn Fn(SerialParams, &mut Window, &mut App)>;

struct SerialForm {
    ports: ChoiceState<Option<String>>,
    detected: usize,
    path: Entity<InputState>,
    baud: ChoiceState<u32>,
}

impl SerialForm {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let found = serial::available_ports();
        let detected = found.len();
        let mut choices: Vec<Choice<Option<String>>> = found
            .iter()
            .map(|p| {
                let label = if p.description.is_empty() {
                    p.path.clone()
                } else {
                    format!("{} — {}", p.path, p.description)
                };
                Choice::new(label, Some(p.path.clone()))
            })
            .collect();
        choices.push(Choice::new(t!("serial.other_port"), None));
        let first = choices.first().map(|c| c.value.clone());
        let ports = ui::choice_state(choices, first.as_ref(), window, cx);
        let placeholder = if cfg!(windows) {
            "COM3"
        } else {
            "/dev/ttyUSB0"
        };
        let path = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let bauds = serial::BAUD_RATES
            .iter()
            .map(|b| Choice::new(t!("serial.baud", baud = b), *b))
            .collect();
        let baud = ui::choice_state(bauds, Some(&115200), window, cx);
        Self {
            ports,
            detected,
            path,
            baud,
        }
    }

    fn params(&self, cx: &App) -> Result<SerialParams, String> {
        let typed = self.path.read(cx).value().trim().to_string();
        let path = match ui::chosen(&self.ports, cx).flatten() {
            Some(p) if typed.is_empty() => p,
            _ if !typed.is_empty() => typed,
            _ => return Err(t!("serial.choose_port").to_string()),
        };
        let baud = ui::chosen(&self.baud, cx).unwrap_or(115200);
        Ok(SerialParams { path, baud })
    }
}

impl Render for SerialForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .child(ui::field_with_hint(
                t!("serial.port"),
                Select::new(&self.ports),
                if self.detected == 0 {
                    t!("serial.no_ports")
                } else {
                    t!("serial.usb_first")
                },
                cx,
            ))
            .child(ui::field(
                t!("serial.port_path"),
                Input::new(&self.path),
                cx,
            ))
            .child(ui::field(t!("serial.speed"), Select::new(&self.baud), cx))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("serial.line_settings")),
            )
    }
}

/// Opens the dialog; `on_open` receives the chosen port.
pub fn open(
    window: &mut Window,
    cx: &mut App,
    on_open: impl Fn(SerialParams, &mut Window, &mut App) + 'static,
) {
    let form = cx.new(|cx| SerialForm::new(window, cx));
    let on_open: OnOpen = Rc::new(on_open);
    window.open_dialog(cx, move |d, _, _| {
        let submit = {
            let form = form.clone();
            let on_open = on_open.clone();
            move |window: &mut Window, cx: &mut App| match form.read(cx).params(cx) {
                Ok(p) => {
                    window.close_dialog(cx);
                    on_open(p, window, cx);
                }
                Err(e) => ui::error(window, cx, e),
            }
        };
        let submit_ok = submit.clone();
        d.title(t!("serial.title"))
            .w(px(480.))
            .on_ok(move |_, window, cx| {
                submit_ok(window, cx);
                false
            })
            .child(form.clone())
            .footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("serial-cancel")
                            .label(t!("common.cancel"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("serial-open")
                            .primary()
                            .icon(ui::icon(ui::IconName::ArrowLeftRight))
                            .label(t!("serial.open"))
                            .on_click(move |_: &ClickEvent, window, cx| submit(window, cx)),
                    ),
            )
    });
}
