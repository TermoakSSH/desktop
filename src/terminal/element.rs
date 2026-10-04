//! GPUI element that paints the terminal grid: backgrounds, text with colors
//! and styles, selection, cursor and the IME composition text. It fits the
//! terminal size (columns × rows) to the available space on every frame and
//! registers the text input handler (dead keys, IME) and mouse drags. It also
//! paints the autocompletion suggestion, dimmed, after the cursor.

use gpui::{
    App, Bounds, ContentMask, DispatchPhase, Element, ElementId, ElementInputHandler, Entity,
    FocusHandle, Font, FontStyle, FontWeight, GlobalElementId, Hitbox, HitboxBehavior,
    InspectorElementId, IntoElement, LayoutId, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ShapedLine, StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle, Window, fill, point,
    px, relative, size,
};

use super::TerminalView;
use super::input::is_wide;
use super::model::{CursorKind, Snapshot};
use crate::theme::TermPalette;

/// Inner padding of the terminal.
pub const PADDING: Pixels = px(8.);

/// Painting element of a [`TerminalView`].
pub struct TerminalElement {
    view: Entity<TerminalView>,
    focus: FocusHandle,
    font: Font,
    font_size: Pixels,
    palette: TermPalette,
}

impl TerminalElement {
    pub fn new(
        view: Entity<TerminalView>,
        focus: FocusHandle,
        font: Font,
        font_size: Pixels,
        palette: TermPalette,
    ) -> Self {
        Self {
            view,
            focus,
            font,
            font_size,
            palette,
        }
    }
}

/// Everything needed to paint a frame.
pub struct Prepared {
    snapshot: Snapshot,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    lines: Vec<(Point<Pixels>, ShapedLine)>,
    /// IME composition text, at the cursor position.
    preedit: Option<(Point<Pixels>, ShapedLine)>,
    /// Autocompletion suggestion (ghost text) after the cursor.
    ghost: Option<(Point<Pixels>, ShapedLine)>,
    hitbox: Hitbox,
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepared;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        style.flex_grow = 1.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&self.font);
        let cell_width = text_system
            .advance(font_id, self.font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(self.font_size * 0.6);
        let cell_width = if cell_width <= px(1.) {
            self.font_size * 0.6
        } else {
            cell_width
        };
        let line_height = (self.font_size * 1.3).round();

        let origin = point(bounds.origin.x + PADDING, bounds.origin.y + PADDING / 2.);
        let usable_w = (bounds.size.width - PADDING * 2.).max(cell_width * 2.);
        let usable_h = (bounds.size.height - PADDING).max(line_height);
        let cols = ((usable_w / cell_width).floor() as u16).clamp(2, 1000);
        let rows = ((usable_h / line_height).floor() as u16).clamp(1, 500);

        let focused = self.focus.is_focused(window);
        self.view.update(cx, |view, cx| {
            view.set_geometry(origin, cell_width, line_height, cols, rows, cx)
        });
        let view = self.view.read(cx);
        let snapshot = view.snapshot(&self.palette, focused);
        let preedit = view.ime_preedit().map(|(text, row, col)| {
            let run = TextRun {
                len: text.len(),
                font: self.font.clone(),
                color: self.palette.foreground,
                background_color: None,
                underline: Some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(self.palette.foreground),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let shaped = text_system.shape_line(text.into(), self.font_size, &[run], None);
            let pos = point(
                origin.x + cell_width * col as f32,
                origin.y + line_height * row as f32,
            );
            (pos, shaped)
        });

        let ghost = view.ghost_text(cx).and_then(|(text, row, col)| {
            // Only what fits in the row.
            let room = (cols as usize).saturating_sub(col);
            let mut cells = 0;
            let text: String = text
                .chars()
                .take_while(|c| {
                    cells += if is_wide(*c) { 2 } else { 1 };
                    cells <= room
                })
                .collect();
            let first = text.chars().next()?;
            let mut dim = self.palette.foreground;
            dim.a *= 0.42;
            // Under the block cursor, the first character uses the background
            // color, like normal text.
            let under_block = snapshot
                .cursor
                .is_some_and(|c| c.kind == CursorKind::Block && c.row == row && c.col == col);
            let run = |len: usize, color| TextRun {
                len,
                font: self.font.clone(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let runs = if under_block {
                let rest = text.len() - first.len_utf8();
                let mut runs = vec![run(first.len_utf8(), self.palette.background)];
                if rest > 0 {
                    runs.push(run(rest, dim));
                }
                runs
            } else {
                vec![run(text.len(), dim)]
            };
            let shaped =
                text_system.shape_line(text.into(), self.font_size, &runs, Some(cell_width));
            let pos = point(
                origin.x + cell_width * col as f32,
                origin.y + line_height * row as f32,
            );
            Some((pos, shaped))
        });

        let mut lines = Vec::new();
        for (row_ix, row) in snapshot.rows.iter().enumerate() {
            let y = origin.y + line_height * row_ix as f32;
            for batch in &row.text {
                let text = batch.text.trim_end().to_string();
                if text.is_empty() {
                    continue;
                }
                let font = Font {
                    weight: if batch.bold {
                        FontWeight::BOLD
                    } else {
                        self.font.weight
                    },
                    style: if batch.italic {
                        FontStyle::Italic
                    } else {
                        FontStyle::Normal
                    },
                    ..self.font.clone()
                };
                let run = TextRun {
                    len: text.len(),
                    font,
                    color: batch.fg,
                    background_color: None,
                    underline: batch.underline.then_some(UnderlineStyle {
                        thickness: px(1.),
                        color: Some(batch.fg),
                        wavy: false,
                    }),
                    strikethrough: batch.strike.then_some(StrikethroughStyle {
                        thickness: px(1.),
                        color: Some(batch.fg),
                    }),
                };
                let shaped = text_system.shape_line(
                    text.into(),
                    self.font_size,
                    &[run],
                    Some(cell_width * batch.cell_width as f32),
                );
                lines.push((point(origin.x + cell_width * batch.col as f32, y), shaped));
            }
        }

        Prepared {
            snapshot,
            origin,
            cell_width,
            line_height,
            lines,
            preedit,
            ghost,
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::Normal),
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepared: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.paint_quad(fill(bounds, self.palette.background));

        // Keyboard text: dead keys, composition and IME (see
        // `EntityInputHandler` in the view).
        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        // Drag and release are tracked outside the terminal too (selection and
        // mouse reports); motion without a button, only over it.
        let view = self.view.clone();
        let hitbox = prepared.hitbox.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hovered = hitbox.is_hovered(window);
                view.update(cx, |v, cx| v.mouse_move(ev, hovered, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, _, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |v, cx| v.mouse_up(ev, cx));
            }
        });

        let Prepared {
            snapshot,
            origin,
            cell_width,
            line_height,
            lines,
            preedit,
            ghost,
            ..
        } = prepared;
        let (origin, cw, lh) = (*origin, *cell_width, *line_height);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            // Backgrounds.
            for (row_ix, row) in snapshot.rows.iter().enumerate() {
                let y = origin.y + lh * row_ix as f32;
                for bg in &row.backgrounds {
                    let b = Bounds::new(
                        point(origin.x + cw * bg.col as f32, y),
                        size(cw * bg.cells as f32, lh),
                    );
                    window.paint_quad(fill(b, bg.color));
                }
            }

            // Cursor (the block goes under the text, painted in the background color).
            if let Some(cursor) = snapshot.cursor {
                let w = cw * if cursor.wide { 2. } else { 1. };
                let x = origin.x + cw * cursor.col as f32;
                let y = origin.y + lh * cursor.row as f32;
                let color = self.palette.cursor;
                match cursor.kind {
                    CursorKind::Block => {
                        window.paint_quad(fill(Bounds::new(point(x, y), size(w, lh)), color));
                    }
                    CursorKind::Beam => {
                        window.paint_quad(fill(Bounds::new(point(x, y), size(px(2.), lh)), color));
                    }
                    CursorKind::Underline => {
                        window.paint_quad(fill(
                            Bounds::new(point(x, y + lh - px(2.)), size(w, px(2.))),
                            color,
                        ));
                    }
                    CursorKind::Hollow => {
                        let t = px(1.);
                        window.paint_quad(fill(Bounds::new(point(x, y), size(w, t)), color));
                        window
                            .paint_quad(fill(Bounds::new(point(x, y + lh - t), size(w, t)), color));
                        window.paint_quad(fill(Bounds::new(point(x, y), size(t, lh)), color));
                        window
                            .paint_quad(fill(Bounds::new(point(x + w - t, y), size(t, lh)), color));
                    }
                }
            }

            // Text.
            for (pos, line) in lines.iter() {
                let _ = line.paint(*pos, lh, TextAlign::Left, None, window, cx);
            }

            // Autocompletion suggestion, dimmed.
            if let Some((pos, line)) = ghost {
                let _ = line.paint(*pos, lh, TextAlign::Left, None, window, cx);
            }

            // Composition text (underlined, covering what is below) and a thin
            // cursor after it.
            if let Some((pos, line)) = preedit {
                let w = line.width.max(cw);
                window.paint_quad(fill(
                    Bounds::new(*pos, size(w, lh)),
                    self.palette.background,
                ));
                let _ = line.paint(*pos, lh, TextAlign::Left, None, window, cx);
                window.paint_quad(fill(
                    Bounds::new(point(pos.x + line.width, pos.y), size(px(2.), lh)),
                    self.palette.cursor,
                ));
            }
        });
    }
}
