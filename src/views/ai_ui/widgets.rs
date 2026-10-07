//! Drawing pieces of the AI interface, shared by the AI section and the
//! copilot: chips and status icons, Markdown with code blocks (highlighted,
//! with Copy / Insert / Run), tool cards, the typing indicator, keyboard
//! hints, loading skeletons and error banners.

use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt, AnyElement, App, ClickEvent, ClipboardItem, Div, FontStyle,
    FontWeight, HighlightStyle, Hsla, InteractiveElement, IntoElement, ParentElement, Rgba,
    SharedString, StatefulInteractiveElement, Styled, StyledText, Window, div,
    prelude::FluentBuilder, px, rems,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::skeleton::Skeleton;
use gpui_component::spinner::Spinner;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{ActiveTheme, Sizable, StyledExt, h_flex, v_flex};
use serde_json::Value;

use super::markdown::{Block, block_command, is_shell, lang_label, split_blocks};
use super::shell::{Tok, tokenize, tokenize_console};
use super::status::{Phase, Tone, ToolKind, ToolState, format_duration, output_preview};
use crate::theme::TermPalette;
use crate::ui::{self, IconName};

/// Width of a comfortable line of text in the conversation.
pub const READING_WIDTH: f32 = 720.;

/// The theme color of a tone.
pub fn tone_color(tone: Tone, cx: &App) -> Hsla {
    let theme = cx.theme();
    match tone {
        Tone::Info => theme.info,
        Tone::Warning => theme.warning,
        Tone::Success => theme.success,
        Tone::Danger => theme.danger,
        Tone::Muted => theme.muted_foreground,
    }
}

/// A status color made readable as text on the theme's background: darker
/// in the light theme, lighter in the dark one.
pub fn readable(c: Hsla, cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        Hsla {
            l: c.l.max(0.62),
            ..c
        }
    } else {
        Hsla {
            l: c.l.min(0.36),
            ..c
        }
    }
}

/// The color with this transparency.
pub fn tint(mut c: Hsla, alpha: f32) -> Hsla {
    c.a = alpha;
    c
}

/// `over` at `alpha` on top of `base`, as an opaque color (a tinted
/// surface that a shadow does not show through).
pub fn mix(base: Hsla, over: Hsla, alpha: f32) -> Hsla {
    let (b, o) = (Rgba::from(base), Rgba::from(over));
    let ch = |x: f32, y: f32| x + (y - x) * alpha;
    Rgba {
        r: ch(b.r, o.r),
        g: ch(b.g, o.g),
        b: ch(b.b, o.b),
        a: 1.0,
    }
    .into()
}

/// Small rounded label with an optional icon, tinted with `color`.
pub fn chip(icon: Option<IconName>, text: impl Into<SharedString>, color: Hsla, cx: &App) -> Div {
    let fg = readable(color, cx);
    h_flex()
        .flex_shrink_0()
        .gap_1()
        .items_center()
        .h(px(20.))
        .px_1p5()
        .rounded(px(6.))
        .bg(tint(color, if cx.theme().is_dark() { 0.16 } else { 0.12 }))
        .text_color(fg)
        .text_xs()
        .font_medium()
        .when_some(icon, |d, i| d.child(ui::icon(i).size(px(12.))))
        .child(div().whitespace_nowrap().child(text.into()))
}

/// A neutral chip (a host, a group, a reason).
pub fn neutral_chip(icon: Option<IconName>, text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .flex_shrink_0()
        .min_w_0()
        .gap_1()
        .items_center()
        .h(px(20.))
        .px_1p5()
        .rounded(px(6.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.muted)
        .text_color(theme.muted_foreground)
        .text_xs()
        .when_some(icon, |d, i| d.child(ui::icon(i).size(px(12.))))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(text.into()),
        )
}

/// The icon of a task's phase: a spinner while it works.
pub fn status_icon(phase: Phase, size: f32, cx: &App) -> AnyElement {
    let color = readable(tone_color(phase.tone(), cx), cx);
    match phase {
        Phase::Running => Spinner::new().xsmall().color(color).into_any_element(),
        _ => ui::icon(phase.icon())
            .size(px(size))
            .text_color(color)
            .into_any_element(),
    }
}

/// Icon and label of a phase, as a chip.
pub fn status_chip(phase: Phase, cx: &App) -> Div {
    let color = tone_color(phase.tone(), cx);
    h_flex()
        .flex_shrink_0()
        .gap_1()
        .items_center()
        .h(px(20.))
        .px_1p5()
        .rounded(px(6.))
        .bg(tint(color, if cx.theme().is_dark() { 0.16 } else { 0.12 }))
        .text_color(readable(color, cx))
        .text_xs()
        .font_medium()
        .child(status_icon(phase, 12., cx))
        .child(div().whitespace_nowrap().child(phase.label()))
}

/// A key cap (`↵`, `E`, `D`) for keyboard hints.
pub fn kbd(key: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .min_w(px(18.))
        .h(px(18.))
        .px_1()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(4.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .text_color(theme.muted_foreground)
        .font_family(ui::mono_family(cx))
        .text_xs()
        .child(key.into())
}

/// "↵ Approve · E Edit · D Deny": keys and what they do.
pub fn key_hints(hints: &[(&'static str, SharedString)], cx: &App) -> Div {
    let theme = cx.theme();
    h_flex()
        .gap_3()
        .flex_wrap()
        .text_xs()
        .text_color(theme.muted_foreground)
        .children(hints.iter().map(|(key, label)| {
            h_flex()
                .gap_1()
                .items_center()
                .child(kbd(*key, cx))
                .child(label.clone())
        }))
}

/// Three dots that pulse one after another, with a label ("Thinking…").
pub fn typing_indicator(id: &'static str, label: SharedString, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let dot_color = theme.muted_foreground;
    let dots = (0..3usize).map(move |i| {
        div()
            .size(px(5.))
            .rounded_full()
            .bg(dot_color)
            .with_animation(
                (id, i),
                Animation::new(Duration::from_millis(1200)).repeat(),
                move |d, t| {
                    // A wave: each dot a bit later than the previous one.
                    let x = (t + 1.0 - i as f32 * 0.18).fract();
                    let v = 1.0 - (2.0 * x - 1.0).abs();
                    d.opacity(0.25 + 0.75 * v)
                },
            )
    });
    h_flex()
        .gap_2()
        .items_center()
        .h(px(24.))
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(h_flex().gap_1().items_center().children(dots))
        .child(label)
        .into_any_element()
}

/// The round mark of the assistant's messages.
pub fn assistant_avatar(cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .size(px(26.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(tint(theme.primary, 0.16))
        .child(
            ui::icon(IconName::Sparkles)
                .size(px(14.))
                .text_color(readable(theme.primary, cx)),
        )
}

/// Placeholder rows while the task list loads.
pub fn list_skeleton(rows: usize) -> AnyElement {
    v_flex()
        .gap_1()
        .px_2()
        .children((0..rows).map(|i| {
            v_flex()
                .px_3()
                .py_2p5()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Skeleton::new().size(px(14.)).rounded_full())
                        .child(
                            Skeleton::new()
                                .h(px(12.))
                                .w(px(if i % 2 == 0 { 170. } else { 130. }))
                                .rounded(px(4.)),
                        ),
                )
                .child(
                    Skeleton::new()
                        .secondary()
                        .ml(px(22.))
                        .h(px(10.))
                        .w(px(110.))
                        .rounded(px(4.)),
                )
        }))
        .into_any_element()
}

/// Placeholder of a conversation while it loads.
pub fn conversation_skeleton() -> AnyElement {
    let line = |w: f32| Skeleton::new().h(px(12.)).w(px(w)).rounded(px(4.));
    v_flex()
        .gap_6()
        .p_6()
        .max_w(px(READING_WIDTH))
        .child(
            h_flex()
                .justify_end()
                .child(Skeleton::new().h(px(36.)).w(px(260.)).rounded(px(12.))),
        )
        .child(
            h_flex()
                .gap_3()
                .items_start()
                .child(Skeleton::new().size(px(26.)).rounded_full())
                .child(
                    v_flex()
                        .gap_2()
                        .pt_1()
                        .child(line(420.))
                        .child(line(360.))
                        .child(line(240.))
                        .child(
                            Skeleton::new()
                                .secondary()
                                .h(px(64.))
                                .w(px(420.))
                                .rounded(px(8.)),
                        ),
                ),
        )
        .into_any_element()
}

/// An error in place, with an optional Retry.
pub fn error_banner(
    id: &'static str,
    text: impl Into<SharedString>,
    retry: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    h_flex()
        .gap_2()
        .items_center()
        .flex_wrap()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(tint(theme.danger, 0.5))
        .bg(tint(theme.danger, 0.08))
        .child(
            ui::icon(IconName::CircleAlert)
                .size(px(16.))
                .text_color(readable(theme.danger, cx)),
        )
        .child(div().flex_1().min_w(px(140.)).text_sm().child(text.into()))
        .when_some(retry, |this, retry| {
            this.child(
                Button::new(id)
                    .small()
                    .icon(ui::icon(IconName::RefreshCw))
                    .label(t!("ai_ui.retry"))
                    .on_click(move |_: &ClickEvent, window, cx| retry(window, cx)),
            )
        })
        .into_any_element()
}

/// What a code block can do besides Copy.
#[derive(Clone, Default)]
pub struct CodeActions {
    /// Types the command into the terminal (without Enter).
    pub insert: Option<Rc<dyn Fn(String, &mut Window, &mut App)>>,
    /// Runs the command (after an approval).
    pub run: Option<Rc<dyn Fn(String, &mut Window, &mut App)>>,
    /// Tooltip of Run (what it does here).
    pub run_tooltip: SharedString,
}

/// Highlight colors of the shell tokens (the terminal's palette, so a
/// command looks as it does in the terminal).
fn token_style(tok: Tok, cx: &App) -> HighlightStyle {
    let dark = cx.theme().is_dark();
    let p = TermPalette::for_mode(dark);
    let ansi = |normal: usize, bright: usize| if dark { p.ansi[bright] } else { p.ansi[normal] };
    let color = match tok {
        Tok::Command => ansi(4, 12),
        Tok::Keyword => ansi(5, 13),
        Tok::Flag => ansi(6, 14),
        Tok::String => ansi(2, 10),
        Tok::Variable | Tok::Number => ansi(3, 11),
        Tok::Operator => ansi(1, 9),
        Tok::Comment => cx.theme().muted_foreground,
    };
    HighlightStyle {
        color: Some(color),
        font_style: (tok == Tok::Comment).then_some(FontStyle::Italic),
        font_weight: (tok == Tok::Command).then_some(FontWeight::MEDIUM),
        ..Default::default()
    }
}

/// How a code block is highlighted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Highlight {
    None,
    Shell,
    /// A session transcript: only the commands after a prompt.
    Console,
}

impl Highlight {
    pub fn of(lang: Option<&str>) -> Self {
        match lang {
            Some("console" | "shell-session" | "shellsession" | "terminal") => Highlight::Console,
            l if is_shell(l) => Highlight::Shell,
            _ => Highlight::None,
        }
    }
}

/// `code` as text, highlighted as shell or as a console session.
pub fn highlighted(code: &str, how: Highlight, cx: &App) -> StyledText {
    let text = SharedString::from(code.to_string());
    let tokens = match how {
        Highlight::None => return StyledText::new(text),
        Highlight::Shell => tokenize(code),
        Highlight::Console => tokenize_console(code),
    };
    let highlights: Vec<_> = tokens
        .into_iter()
        .map(|(range, tok)| (range, token_style(tok, cx)))
        .collect();
    StyledText::new(text).with_highlights(highlights)
}

/// Background of code (a bit apart from the conversation's).
pub fn code_background(cx: &App) -> Hsla {
    if cx.theme().is_dark() {
        crate::theme::hex(0x10131b)
    } else {
        crate::theme::hex(0xf6f7fa)
    }
}

/// Copies to the clipboard and says so.
pub fn copy_to_clipboard(text: String, window: &mut Window, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    ui::success(window, cx, t!("ai_ui.copied"));
}

/// A fenced code block: language, Copy and (for commands) Insert and Run in
/// its header; the code highlighted and scrolling sideways.
pub fn code_block(
    id: SharedString,
    lang: Option<&str>,
    code: &str,
    actions: &CodeActions,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let shell = is_shell(lang);
    let command = block_command(lang, code);
    let copy_text = code.to_string();
    let header = h_flex()
        .h(px(30.))
        .pl_3()
        .pr_1()
        .gap_1()
        .items_center()
        .border_b_1()
        .border_color(theme.border)
        .bg(tint(theme.muted, 0.6))
        .child(
            ui::icon(if shell {
                IconName::SquareTerminal
            } else {
                IconName::FileCode
            })
            .size(px(12.))
            .text_color(theme.muted_foreground),
        )
        .child(
            div()
                .flex_1()
                .text_xs()
                .font_family(ui::mono_family(cx))
                .text_color(theme.muted_foreground)
                .child(lang_label(lang)),
        )
        .child(
            Button::new(SharedString::from(format!("{id}-copy")))
                .xsmall()
                .ghost()
                .icon(ui::icon(IconName::Copy))
                .tooltip(t!("ai_ui.code.copy"))
                .on_click(move |_: &ClickEvent, window, cx| {
                    copy_to_clipboard(copy_text.clone(), window, cx)
                }),
        )
        .when_some(
            command.clone().zip(actions.insert.clone()),
            |this, (command, insert)| {
                this.child(
                    Button::new(SharedString::from(format!("{id}-insert")))
                        .xsmall()
                        .ghost()
                        .icon(ui::icon(IconName::TextCursorInput))
                        .tooltip(t!("ai_ui.code.insert"))
                        .on_click(move |_: &ClickEvent, window, cx| {
                            insert(command.clone(), window, cx)
                        }),
                )
            },
        )
        .when_some(command.zip(actions.run.clone()), |this, (command, run)| {
            this.child(
                Button::new(SharedString::from(format!("{id}-run")))
                    .xsmall()
                    .ghost()
                    .icon(ui::icon(IconName::Play))
                    .label(t!("ai_ui.code.run"))
                    .tooltip(actions.run_tooltip.clone())
                    .on_click(move |_: &ClickEvent, window, cx| run(command.clone(), window, cx)),
            )
        });
    let code = code.trim_end_matches('\n');
    v_flex()
        .w_full()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(code_background(cx))
        .overflow_hidden()
        .child(header)
        .child(
            div()
                .id(SharedString::from(format!("{id}-body")))
                .overflow_x_scroll()
                .px_3()
                .py_2()
                .font_family(ui::mono_family(cx))
                .text_xs()
                .line_height(rems(1.25))
                .child(
                    v_flex()
                        .min_w_full()
                        .child(div().whitespace_nowrap().child(highlighted(
                            if code.is_empty() { " " } else { code },
                            Highlight::of(lang),
                            cx,
                        ))),
                ),
        )
        .into_any_element()
}

/// The style of Markdown in the conversation (tighter paragraphs).
fn markdown_style() -> TextViewStyle {
    TextViewStyle::default().paragraph_gap(rems(0.6))
}

/// Markdown with its fenced code blocks drawn as [`code_block`]s.
/// `streaming`: the text is still arriving (new text fades in).
pub fn rich_text(
    key: &str,
    text: &str,
    actions: &CodeActions,
    streaming: bool,
    cx: &App,
) -> AnyElement {
    v_flex()
        .w_full()
        .min_w_0()
        .gap_3()
        .text_sm()
        .children(
            split_blocks(text)
                .into_iter()
                .enumerate()
                .map(|(i, block)| match block {
                    Block::Text(t) => {
                        TextView::markdown(SharedString::from(format!("{key}-{i}")), t)
                            .style(markdown_style())
                            .selectable(true)
                            .stream_fade(streaming)
                            .into_any_element()
                    }
                    Block::Code { lang, code, .. } => code_block(
                        format!("{key}-code-{i}").into(),
                        lang.as_deref(),
                        &code,
                        actions,
                        cx,
                    ),
                }),
        )
        .into_any_element()
}

/// A tool call as the conversation shows it.
pub struct ToolView<'a> {
    /// Unique id (the call's).
    pub key: SharedString,
    pub name: &'a str,
    pub input: &'a Value,
    /// The engine's one-line summary (live calls).
    pub summary: Option<&'a str>,
    pub state: ToolState,
    pub output: Option<&'a str>,
    pub duration_ms: Option<u64>,
    /// When it ran (Unix ms), shown on hover.
    pub at: Option<i64>,
    pub expanded: bool,
}

/// A tool call as a compact card: its icon, what it acts on, the host,
/// the time it took and its state; the first lines of its output, and all
/// of it when expanded.
pub fn tool_card(
    view: ToolView<'_>,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let kind = ToolKind::of(view.name);
    let target = super::status::tool_target(view.input)
        .or_else(|| view.summary.map(str::to_string))
        .unwrap_or_default();
    let host = super::status::tool_host(view.input);
    let state_color = readable(tone_color(view.state.tone(), cx), cx);
    let key = view.key.clone();
    let group = SharedString::from(format!("{key}-group"));
    let first_line = target.lines().next().unwrap_or("").to_string();
    let state_el = match view.state {
        ToolState::Running => Spinner::new()
            .xsmall()
            .color(state_color)
            .into_any_element(),
        ToolState::Ok => ui::icon(IconName::CircleCheck)
            .size(px(14.))
            .text_color(state_color)
            .into_any_element(),
        ToolState::Failed => ui::icon(IconName::CircleX)
            .size(px(14.))
            .text_color(state_color)
            .into_any_element(),
        ToolState::Denied => {
            chip(Some(IconName::Ban), view.state.label(), theme.warning, cx).into_any_element()
        }
    };
    let header = h_flex()
        .id(SharedString::from(format!("{key}-head")))
        .w_full()
        .min_w_0()
        .h(px(32.))
        .px_2()
        .gap_2()
        .items_center()
        .cursor_pointer()
        .rounded(theme.radius)
        .hover(|s| s.bg(theme.secondary_hover))
        .on_click(on_toggle)
        .child(
            ui::icon(if view.expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(px(12.))
            .text_color(theme.muted_foreground),
        )
        .child(
            ui::icon(kind.icon())
                .size(px(14.))
                .text_color(theme.muted_foreground),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_xs()
                .font_medium()
                .child(kind.label(view.name)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_xs()
                .font_family(ui::mono_family(cx))
                .text_color(theme.muted_foreground)
                .child(first_line),
        )
        .when_some(host, |this, host| {
            this.child(neutral_chip(Some(IconName::Server), host, cx).max_w(px(140.)))
        })
        .when_some(view.at, |this, at| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .opacity(0.)
                    .group_hover(group.clone(), |s| s.opacity(1.))
                    .child(short_time(at)),
            )
        })
        .when_some(view.duration_ms, |this, ms| {
            this.child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format_duration(ms as i64)),
            )
        })
        .child(div().flex_shrink_0().child(state_el));

    let mono = ui::mono_family(cx);
    let body: Option<AnyElement> = if view.expanded {
        let full_output = view
            .output
            .map(|o| crate::views::ai_chat::truncate(o, 20_000));
        Some(
            v_flex()
                .gap_2()
                .px_2()
                .pb_2()
                .when(
                    !target.is_empty() && (kind.is_command() || target.len() > 48),
                    |this| this.child(code_line(&key, &target, kind.is_command(), cx)),
                )
                .when_some(full_output, |this, out| {
                    let copy = out.clone();
                    this.child(
                        v_flex()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .bg(code_background(cx))
                            .child(
                                h_flex()
                                    .pl_3()
                                    .pr_1()
                                    .h(px(26.))
                                    .items_center()
                                    .border_b_1()
                                    .border_color(theme.border)
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(t!("ai_ui.tool.output")),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("{key}-copy")))
                                            .xsmall()
                                            .ghost()
                                            .icon(ui::icon(IconName::Copy))
                                            .tooltip(t!("ai_ui.code.copy"))
                                            .on_click(move |_: &ClickEvent, window, cx| {
                                                copy_to_clipboard(copy.clone(), window, cx)
                                            }),
                                    ),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("{key}-out")))
                                    .max_h(px(320.))
                                    .overflow_y_scroll()
                                    .px_3()
                                    .py_2()
                                    .font_family(mono.clone())
                                    .text_xs()
                                    .text_color(if view.state == ToolState::Failed {
                                        readable(theme.danger, cx)
                                    } else {
                                        theme.foreground
                                    })
                                    .child(if out.trim().is_empty() {
                                        t!("ai_ui.tool.no_output").to_string()
                                    } else {
                                        out
                                    }),
                            ),
                    )
                })
                .into_any_element(),
        )
    } else {
        view.output
            .map(|o| output_preview(o, 3))
            .filter(|(p, _)| !p.trim().is_empty())
            .map(|(preview, more)| {
                div()
                    .mx_2()
                    .mb_2()
                    .ml(px(30.))
                    .px_2()
                    .py_1()
                    .rounded(px(6.))
                    .bg(code_background(cx))
                    .font_family(mono.clone())
                    .text_xs()
                    .text_color(if view.state == ToolState::Failed {
                        readable(theme.danger, cx)
                    } else {
                        theme.muted_foreground
                    })
                    .overflow_hidden()
                    .child(div().whitespace_nowrap().overflow_hidden().child(if more {
                        format!("{preview}\n…")
                    } else {
                        preview
                    }))
                    .into_any_element()
            })
    };
    v_flex()
        .group(group)
        .w_full()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .child(header)
        .children(body)
        .into_any_element()
}

/// A command (or other text) on its own, highlighted, scrolling sideways.
pub fn code_line(key: &str, text: &str, shell: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    div()
        .id(SharedString::from(format!("{key}-line")))
        .w_full()
        .overflow_x_scroll()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(code_background(cx))
        .font_family(ui::mono_family(cx))
        .text_xs()
        .line_height(rems(1.25))
        .child(
            v_flex()
                .min_w_full()
                .child(div().whitespace_nowrap().child(highlighted(
                    text,
                    if shell {
                        Highlight::Shell
                    } else {
                        Highlight::None
                    },
                    cx,
                ))),
        )
        .into_any_element()
}

/// `14:05` today, the date and time otherwise.
pub fn short_time(ms: i64) -> String {
    let Some(d) = chrono::DateTime::from_timestamp_millis(ms) else {
        return String::new();
    };
    let local = d.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("%H:%M").to_string()
    } else {
        ui::format_ms(ms)
    }
}

/// Now, in Unix milliseconds.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
