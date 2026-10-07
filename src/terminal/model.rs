//! Terminal emulation with `alacritty_terminal`: interprets the server output
//! (VT100/xterm), keeps the grid and the history, handles the selection and
//! prepares a snapshot ready to paint.

use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor};
use gpui::Hsla;
use parking_lot::Mutex;

use crate::theme::{TermPalette, rgb8};

/// A match of the find bar: first and last cell (inclusive).
pub type FindMatch = super::find::Match;

/// Terminal size for `alacritty_terminal`.
#[derive(Clone, Copy, Debug)]
struct Size {
    cols: usize,
    lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Collects the emulator events (replies to the server, title...).
#[derive(Clone, Default)]
struct Proxy(Arc<Mutex<Vec<Event>>>);

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        self.0.lock().push(event);
    }
}

/// What the emulator asks of the app after processing output.
#[derive(Debug, Clone)]
pub enum TermEvent {
    /// Reply to send to the server (e.g. the cursor position).
    Write(Vec<u8>),
    Title(String),
    ResetTitle,
    Bell,
    /// The remote program asks to copy text to the clipboard (OSC 52).
    Copy(String),
}

/// Run of text with the same style, starting at `col`.
#[derive(Debug, Clone)]
pub struct TextBatch {
    pub col: usize,
    pub text: String,
    /// Cells each character takes (2 for wide ones).
    pub cell_width: usize,
    /// Number of cells of the run.
    pub cells: usize,
    pub fg: Hsla,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

/// Background of a run of cells.
#[derive(Debug, Clone, Copy)]
pub struct BgRun {
    pub col: usize,
    pub cells: usize,
    pub color: Hsla,
}

/// A row ready to paint.
#[derive(Debug, Clone, Default)]
pub struct RowSnapshot {
    pub backgrounds: Vec<BgRun>,
    pub text: Vec<TextBatch>,
}

/// Shape of the painted cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorKind {
    Block,
    Hollow,
    Beam,
    Underline,
}

#[derive(Debug, Clone, Copy)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
    pub kind: CursorKind,
    pub wide: bool,
}

/// Visible screen ready to paint.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub rows: Vec<RowSnapshot>,
    pub cursor: Option<CursorSnapshot>,
    /// Matches of the find bar on screen.
    pub highlights: Vec<super::find::Highlight>,
}

/// Emulator state of a terminal.
pub struct TermModel {
    term: Term<Proxy>,
    processor: Processor,
    events: Proxy,
    size: Size,
}

impl TermModel {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let size = Size {
            cols: cols.max(2) as usize,
            lines: rows.max(1) as usize,
        };
        let events = Proxy::default();
        let config = Config {
            scrolling_history: scrollback,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, events.clone()),
            processor: Processor::new(),
            events,
            size,
        }
    }

    pub fn cols(&self) -> u16 {
        self.size.cols as u16
    }

    pub fn rows(&self) -> u16 {
        self.size.lines as u16
    }

    pub fn mode(&self) -> TermMode {
        *self.term.mode()
    }

    /// Processes server output.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<TermEvent> {
        self.processor.advance(&mut self.term, bytes);
        self.drain_events()
    }

    fn drain_events(&mut self) -> Vec<TermEvent> {
        let raw: Vec<Event> = std::mem::take(&mut *self.events.0.lock());
        let mut out = Vec::new();
        for ev in raw {
            match ev {
                Event::PtyWrite(s) => out.push(TermEvent::Write(s.into_bytes())),
                Event::Title(t) => out.push(TermEvent::Title(t)),
                Event::ResetTitle => out.push(TermEvent::ResetTitle),
                Event::Bell => out.push(TermEvent::Bell),
                Event::ClipboardStore(_, text) => out.push(TermEvent::Copy(text)),
                Event::TextAreaSizeRequest(format) => {
                    let size = alacritty_terminal::event::WindowSize {
                        num_lines: self.size.lines as u16,
                        num_cols: self.size.cols as u16,
                        cell_width: 8,
                        cell_height: 16,
                    };
                    out.push(TermEvent::Write(format(size).into_bytes()));
                }
                Event::ColorRequest(index, format) => {
                    let palette = TermPalette::dark();
                    let color = match index {
                        256 => palette.foreground,
                        257 => palette.background,
                        258 => palette.cursor,
                        i if i < 256 => palette.indexed(i as u8),
                        _ => palette.foreground,
                    };
                    let rgba = gpui::Rgba::from(color);
                    let rgb = alacritty_terminal::vte::ansi::Rgb {
                        r: (rgba.r * 255.0) as u8,
                        g: (rgba.g * 255.0) as u8,
                        b: (rgba.b * 255.0) as u8,
                    };
                    out.push(TermEvent::Write(format(rgb).into_bytes()));
                }
                _ => {}
            }
        }
        out
    }

    /// Resizes the grid.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let size = Size {
            cols: cols.max(2) as usize,
            lines: rows.max(1) as usize,
        };
        if size.cols == self.size.cols && size.lines == self.size.lines {
            return;
        }
        self.size = size;
        self.term.resize(size);
    }

    /// Scrolls the history (`delta` > 0 upwards).
    pub fn scroll(&mut self, delta: i32) {
        if delta != 0 {
            self.term.scroll_display(Scroll::Delta(delta));
        }
    }

    pub fn scroll_page(&mut self, up: bool) {
        self.term
            .scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
    }

    pub fn scroll_to_bottom(&mut self) {
        if self.term.grid().display_offset() != 0 {
            self.term.scroll_display(Scroll::Bottom);
        }
    }

    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Visible cell of the cursor (row, column), even if the program hides
    /// it: the IME composition text is written there.
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        let point = self.term.grid().cursor.point;
        let row = point.line.0 + self.display_offset() as i32;
        (row >= 0 && (row as usize) < self.size.lines).then(|| {
            (
                row as usize,
                point.column.0.min(self.size.cols.saturating_sub(1)),
            )
        })
    }

    /// Screen text right before the cursor (at most `max` characters,
    /// joining the rows split for being too long) and whether nothing is
    /// written to the right of the cursor. Used to check that the line we
    /// think was typed is the one the shell shows.
    pub fn text_around_cursor(&self, max: usize) -> (String, bool) {
        let grid = self.term.grid();
        let cursor = &grid.cursor;
        let cols = self.size.cols;
        let blank = |c: char| c == ' ' || c == '\0';
        let spacer = Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER;
        let row = &grid[cursor.point.line];
        // After writing in the last column the cursor stays there (with
        // `input_needs_wrap`): that cell is already text.
        let end = if cursor.input_needs_wrap {
            cols
        } else {
            cursor.point.column.0.min(cols)
        };
        let after_blank = (end..cols).all(|c| blank(row[Column(c)].c));
        let row_text = |line: Line, upto: usize| -> Vec<char> {
            let r = &grid[line];
            (0..upto)
                .map(|c| &r[Column(c)])
                .filter(|cell| !cell.flags.intersects(spacer))
                .map(|cell| if cell.c == '\0' { ' ' } else { cell.c })
                .collect()
        };
        let mut before = row_text(cursor.point.line, end);
        let mut line = cursor.point.line;
        let top = grid.topmost_line();
        while before.len() < max && line > top {
            let prev = Line(line.0 - 1);
            let wrapped = grid[prev][Column(cols - 1)].flags.contains(Flags::WRAPLINE);
            if !wrapped {
                break;
            }
            let mut text = row_text(prev, cols);
            text.extend(before);
            before = text;
            line = prev;
        }
        let skip = before.len().saturating_sub(max);
        (before[skip..].iter().collect(), after_blank)
    }

    /// Clears the screen and the history (e.g. before receiving the full history).
    pub fn reset(&mut self) {
        let cols = self.size.cols;
        let lines = self.size.lines;
        let config = Config {
            scrolling_history: self.term.grid().history_size().max(10_000),
            ..Config::default()
        };
        self.events.0.lock().clear();
        self.term = Term::new(config, &Size { cols, lines }, self.events.clone());
        self.processor = Processor::new();
    }

    /// Forgets the history (scrollback) and the selection; the visible
    /// screen stays (the shell clears it when asked with Ctrl+L).
    pub fn clear_history(&mut self) {
        self.term.selection = None;
        self.scroll_to_bottom();
        self.term.grid_mut().clear_history();
    }

    // ----- Find -----

    /// Every match of `regex` in the screen and the history, from the top
    /// down (at most `max`; the flag says there were more).
    pub fn find_all(&self, regex: &mut RegexSearch, max: usize) -> (Vec<FindMatch>, bool) {
        let start = Point::new(self.term.topmost_line(), Column(0));
        let end = Point::new(
            self.term.bottommost_line(),
            Column(self.size.cols.saturating_sub(1)),
        );
        let mut all: Vec<FindMatch> =
            RegexIter::new(start, end, Direction::Right, &self.term, regex)
                .take(max + 1)
                .collect();
        let capped = all.len() > max;
        all.truncate(max);
        (all, capped)
    }

    /// The matches of `regex` on screen (to highlight them), including the
    /// ones that start or end on wrapped lines just outside it.
    pub fn visible_matches(&self, regex: &mut RegexSearch) -> Vec<FindMatch> {
        let offset = self.display_offset() as i32;
        let start = self
            .term
            .line_search_left(Point::new(Line(-offset), Column(0)));
        let end = self.term.line_search_right(Point::new(
            Line(self.size.lines as i32 - 1 - offset),
            Column(self.size.cols.saturating_sub(1)),
        ));
        RegexIter::new(start, end, Direction::Right, &self.term, regex)
            .take(super::find::MAX_MATCHES)
            .collect()
    }

    /// Selects a match and scrolls it into view.
    pub fn select_match(&mut self, found: &FindMatch) {
        let mut sel = Selection::new(SelectionType::Simple, *found.start(), Side::Left);
        sel.update(*found.end(), Side::Right);
        self.term.selection = Some(sel);
        self.term.scroll_to_point(*found.start());
    }

    /// First and last cell of the selection (the current match while
    /// finding; it moves with the text when new output scrolls it).
    pub fn selection_range(&self) -> Option<FindMatch> {
        let range = self.term.selection.as_ref()?.to_range(&self.term)?;
        Some(range.start..=range.end)
    }

    /// Last cell on screen (where searching starts from).
    pub fn view_bottom(&self) -> Point {
        let offset = self.display_offset() as i32;
        Point::new(
            Line(self.size.lines as i32 - 1 - offset),
            Column(self.size.cols.saturating_sub(1)),
        )
    }

    // ----- Links -----

    /// Link under the visible cell `row`, `col`: the one the program marks
    /// (OSC 8) or a URL written on the line.
    pub fn link_at(&self, row: usize, col: usize) -> Option<String> {
        let point = self.grid_point(row, col);
        let grid = self.term.grid();
        if let Some(link) = grid[point].hyperlink() {
            return Some(link.uri().to_string());
        }
        let line = &grid[point.line];
        let chars: Vec<char> = (0..self.size.cols)
            .map(|c| match line[Column(c)].c {
                '\0' | '\t' => ' ',
                ch => ch,
            })
            .collect();
        find_url(&chars, point.column.0)
    }

    // ----- Selection -----

    /// Grid point from a visible row/column.
    pub fn grid_point(&self, row: usize, col: usize) -> Point {
        let offset = self.display_offset() as i32;
        let row = row.min(self.size.lines.saturating_sub(1)) as i32;
        let col = col.min(self.size.cols.saturating_sub(1));
        Point::new(Line(row - offset), Column(col))
    }

    pub fn start_selection(&mut self, row: usize, col: usize, right_half: bool, clicks: usize) {
        let point = self.grid_point(row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        let ty = match clicks {
            2 => SelectionType::Semantic,
            n if n >= 3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        self.term.selection = Some(Selection::new(ty, point, side));
    }

    pub fn update_selection(&mut self, row: usize, col: usize, right_half: bool) {
        let point = self.grid_point(row, col);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, side);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn has_selection(&self) -> bool {
        self.term.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn select_all(&mut self) {
        let top = self.term.topmost_line();
        let bottom = self.term.bottommost_line();
        let mut sel = Selection::new(SelectionType::Lines, Point::new(top, Column(0)), Side::Left);
        sel.update(
            Point::new(bottom, Column(self.size.cols.saturating_sub(1))),
            Side::Right,
        );
        self.term.selection = Some(sel);
    }

    /// Text of the visible screen (for the AI).
    pub fn screen_text(&self) -> String {
        let offset = self.display_offset() as i32;
        let start = Point::new(Line(-offset), Column(0));
        let end = Point::new(
            Line(self.size.lines as i32 - 1 - offset),
            Column(self.size.cols.saturating_sub(1)),
        );
        let text = self.term.bounds_to_string(start, end);
        text.lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string()
    }

    // ----- Painting -----

    /// Prepares the visible screen to paint it with the given palette.
    pub fn snapshot(&self, palette: &TermPalette, focused: bool) -> Snapshot {
        let content = self.term.renderable_content();
        let offset = content.display_offset as i32;
        let lines = self.size.lines;
        let colors = content.colors;
        let selection = content.selection;
        let mut rows: Vec<RowSnapshot> = vec![RowSnapshot::default(); lines];

        let cursor_point = content.cursor.point;
        let cursor_shape = content.cursor.shape;
        let cursor_row = cursor_point.line.0 + offset;
        let cursor_visible =
            cursor_shape != CursorShape::Hidden && cursor_row >= 0 && (cursor_row as usize) < lines;
        let cursor_kind = if !focused {
            CursorKind::Hollow
        } else {
            match cursor_shape {
                CursorShape::Beam => CursorKind::Beam,
                CursorShape::Underline => CursorKind::Underline,
                CursorShape::HollowBlock => CursorKind::Hollow,
                _ => CursorKind::Block,
            }
        };
        let mut cursor_wide = false;

        for indexed in content.display_iter {
            let row = indexed.point.line.0 + offset;
            if row < 0 || row as usize >= lines {
                continue;
            }
            let row = row as usize;
            let col = indexed.point.column.0;
            let cell = indexed.cell;
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let wide = cell.flags.contains(Flags::WIDE_CHAR);
            let width = if wide { 2 } else { 1 };

            let mut fg = resolve(cell.fg, colors, palette, true);
            let mut bg = resolve(cell.bg, colors, palette, false);
            let mut has_bg = cell.bg != Color::Named(NamedColor::Background);
            if cell.flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
                has_bg = true;
            }
            if cell.flags.contains(Flags::DIM) {
                fg.a *= 0.66;
            }
            if cell.flags.contains(Flags::HIDDEN) {
                fg = bg;
            }
            let selected = selection.is_some_and(|s| s.contains(indexed.point));
            if selected {
                bg = blend(bg, palette.selection, has_bg);
                has_bg = true;
            }
            let is_cursor =
                cursor_visible && row as i32 == cursor_row && col == cursor_point.column.0;
            if is_cursor {
                cursor_wide = wide;
                if cursor_kind == CursorKind::Block {
                    // The character under the cursor is painted with the background color.
                    fg = palette.background;
                }
            }

            let r = &mut rows[row];
            if has_bg {
                match r.backgrounds.last_mut() {
                    Some(last) if last.col + last.cells == col && last.color == bg => {
                        last.cells += width
                    }
                    _ => r.backgrounds.push(BgRun {
                        col,
                        cells: width,
                        color: bg,
                    }),
                }
            }

            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            let underline = cell.flags.intersects(Flags::ALL_UNDERLINES);
            let strike = cell.flags.contains(Flags::STRIKEOUT);
            let mut text = String::new();
            text.push(if cell.c == '\0' || cell.c == '\t' {
                ' '
            } else {
                cell.c
            });
            if let Some(extra) = cell.zerowidth() {
                text.extend(extra.iter());
            }
            // Spaces without decoration need no painting.
            if text == " " && !underline && !strike {
                continue;
            }
            match r.text.last_mut() {
                Some(last)
                    if !wide
                        && last.cell_width == 1
                        && last.col + last.cells == col
                        && last.fg == fg
                        && last.bold == bold
                        && last.italic == italic
                        && last.underline == underline
                        && last.strike == strike =>
                {
                    last.text.push_str(&text);
                    last.cells += 1;
                }
                Some(last)
                    if !wide
                        && last.cell_width == 1
                        && last.fg == fg
                        && last.bold == bold
                        && last.italic == italic
                        && !last.underline
                        && !underline
                        && !last.strike
                        && !strike
                        && col > last.col + last.cells
                        && col - (last.col + last.cells) <= 4 =>
                {
                    // Joins words separated by a few spaces (fewer runs to shape).
                    let gap = col - (last.col + last.cells);
                    last.text.extend(std::iter::repeat_n(' ', gap));
                    last.text.push_str(&text);
                    last.cells += gap + 1;
                }
                _ => r.text.push(TextBatch {
                    col,
                    text,
                    cell_width: width,
                    cells: width,
                    fg,
                    bold,
                    italic,
                    underline,
                    strike,
                }),
            }
        }

        let cursor = cursor_visible.then(|| CursorSnapshot {
            row: cursor_row as usize,
            col: cursor_point.column.0,
            kind: cursor_kind,
            wide: cursor_wide,
        });
        Snapshot {
            rows,
            cursor,
            highlights: Vec::new(),
        }
    }
}

/// URL of `chars` that contains column `col`, without the trailing
/// punctuation that usually follows it in a text ("(see https://x.es).").
fn find_url(chars: &[char], col: usize) -> Option<String> {
    const SCHEMES: [&str; 4] = ["https://", "http://", "ftp://", "file://"];
    let text: String = chars.iter().collect();
    // Character indices (not bytes): columns are characters.
    let lower = text.to_lowercase();
    let lower_chars: Vec<char> = lower.chars().collect();
    let mut start = 0;
    while start < lower_chars.len() {
        let rest: String = lower_chars[start..].iter().collect();
        let Some(scheme) = SCHEMES.iter().find(|s| rest.starts_with(**s)) else {
            start += 1;
            continue;
        };
        let mut end = start + scheme.chars().count();
        while end < chars.len() && !chars[end].is_whitespace() && !"<>\"'`".contains(chars[end]) {
            end += 1;
        }
        // Trailing punctuation that is not part of the URL.
        while end > start {
            let last = chars[end - 1];
            let open_parens = chars[start..end].iter().filter(|c| **c == '(').count();
            let close_parens = chars[start..end].iter().filter(|c| **c == ')').count();
            if ".,;:!?]}".contains(last) || (last == ')' && close_parens > open_parens) {
                end -= 1;
            } else {
                break;
            }
        }
        if end > start + scheme.chars().count() && (start..end).contains(&col) {
            return Some(chars[start..end].iter().collect());
        }
        start = end.max(start + 1);
    }
    None
}

/// Blends the selection color over the background.
fn blend(bg: Hsla, sel: Hsla, has_bg: bool) -> Hsla {
    if !has_bg {
        return sel;
    }
    let a = gpui::Rgba::from(bg);
    let b = gpui::Rgba::from(sel);
    let t = b.a;
    gpui::Rgba {
        r: a.r * (1.0 - t) + b.r * t,
        g: a.g * (1.0 - t) + b.g * t,
        b: a.b * (1.0 - t) + b.b * t,
        a: 1.0,
    }
    .into()
}

/// Translates an emulator color to the theme palette.
fn resolve(color: Color, colors: &Colors, palette: &TermPalette, is_fg: bool) -> Hsla {
    match color {
        Color::Spec(rgb) => rgb8(rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => match colors[i as usize] {
            Some(rgb) => rgb8(rgb.r, rgb.g, rgb.b),
            None => palette.indexed(i),
        },
        Color::Named(named) => {
            let idx = named as usize;
            if let Some(rgb) = colors[idx] {
                return rgb8(rgb.r, rgb.g, rgb.b);
            }
            match named {
                NamedColor::Foreground | NamedColor::BrightForeground => palette.foreground,
                NamedColor::Background => palette.background,
                NamedColor::Cursor => palette.cursor,
                NamedColor::DimForeground => dim(palette.foreground),
                NamedColor::DimBlack => dim(palette.ansi[0]),
                NamedColor::DimRed => dim(palette.ansi[1]),
                NamedColor::DimGreen => dim(palette.ansi[2]),
                NamedColor::DimYellow => dim(palette.ansi[3]),
                NamedColor::DimBlue => dim(palette.ansi[4]),
                NamedColor::DimMagenta => dim(palette.ansi[5]),
                NamedColor::DimCyan => dim(palette.ansi[6]),
                NamedColor::DimWhite => dim(palette.ansi[7]),
                _ if idx < 16 => palette.ansi[idx],
                _ if is_fg => palette.foreground,
                _ => palette.background,
            }
        }
    }
}

fn dim(mut c: Hsla) -> Hsla {
    c.l *= 0.7;
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls_under_the_cursor() {
        let chars: Vec<char> = "see (https://ohz.es/docs?a=1). or http://x.io,"
            .chars()
            .collect();
        assert_eq!(
            find_url(&chars, 10).as_deref(),
            Some("https://ohz.es/docs?a=1")
        );
        assert_eq!(find_url(&chars, 2), None);
        assert_eq!(find_url(&chars, 40).as_deref(), Some("http://x.io"));
        // The final parenthesis is only removed if it is not opened inside.
        let wiki: Vec<char> = "https://en.wikipedia.org/wiki/Terminal_(computing)"
            .chars()
            .collect();
        assert_eq!(find_url(&wiki, 5).map(|u| u.ends_with(')')), Some(true));
    }

    #[test]
    fn find_walks_the_history() {
        use super::super::find::{self, FindOptions};
        let regex = |q: &str, case_sensitive: bool, is_regex: bool| {
            let options = FindOptions {
                case_sensitive,
                regex: is_regex,
            };
            RegexSearch::new(&find::pattern(q, options).unwrap()).unwrap()
        };
        let mut m = TermModel::new(40, 3, 100);
        for i in 0..10 {
            m.feed(format!("line {i} needle.{i}\r\n").as_bytes());
        }
        // Every match, from the top of the history down.
        let (all, capped) = m.find_all(&mut regex("needle.", false, false), 100);
        assert_eq!(all.len(), 10);
        assert!(!capped);
        assert!(all.windows(2).all(|w| w[0].start() < w[1].start()));
        // Case: ignored unless asked for, even with capitals.
        assert_eq!(
            m.find_all(&mut regex("NEEDLE.9", false, false), 100)
                .0
                .len(),
            1
        );
        assert!(
            m.find_all(&mut regex("NEEDLE.9", true, false), 100)
                .0
                .is_empty()
        );
        // Literal text: the dot is not "any character"; as a regex it is.
        m.feed(b"needleX");
        assert!(
            m.find_all(&mut regex("l.X", false, false), 100)
                .0
                .is_empty()
        );
        assert_eq!(m.find_all(&mut regex("l.X", false, true), 100).0.len(), 1);
        assert_eq!(
            m.find_all(&mut regex(r"needle\.[0-4]", false, true), 100)
                .0
                .len(),
            5
        );
        // The limit.
        let (some, capped) = m.find_all(&mut regex("needle", false, false), 4);
        assert_eq!(some.len(), 4);
        assert!(capped);
        // On screen (3 rows): only the last ones.
        let visible = m.visible_matches(&mut regex("needle", false, false));
        assert!(!visible.is_empty() && visible.len() < 11);
        // Selecting one scrolls to it and it can be read back.
        m.select_match(&all[0]);
        assert_eq!(m.selection_text().as_deref(), Some("needle."));
        assert_eq!(m.selection_range(), Some(all[0].clone()));
        assert!(m.display_offset() > 0);
        // Searching again starts from what is on screen (3 rows).
        assert!(find::nearest(&all, m.view_bottom()).unwrap() <= 2);
        m.clear_history();
        assert_eq!(m.display_offset(), 0);
        assert!(!m.has_selection());
    }

    #[test]
    fn link_at_reads_the_screen_and_osc8() {
        let mut m = TermModel::new(60, 3, 100);
        m.feed(b"docs: https://termoak.com/download ok");
        assert_eq!(
            m.link_at(0, 12).as_deref(),
            Some("https://termoak.com/download")
        );
        assert_eq!(m.link_at(0, 2), None);
        m.feed(b"\r\n\x1b]8;;https://ohz.es\x1b\\click\x1b]8;;\x1b\\");
        assert_eq!(m.link_at(1, 2).as_deref(), Some("https://ohz.es"));
    }

    #[test]
    fn feed_and_screen_text() {
        let mut m = TermModel::new(20, 5, 100);
        m.feed(b"hello\r\nworld");
        assert_eq!(m.screen_text(), "hello\nworld");
    }

    #[test]
    fn colors_and_wide_chars_in_snapshot() {
        let palette = TermPalette::dark();
        let mut m = TermModel::new(20, 3, 100);
        m.feed("\x1b[31mwarn\x1b[0m 日本".as_bytes());
        let snap = m.snapshot(&palette, true);
        let row = &snap.rows[0];
        let warn = row
            .text
            .iter()
            .find(|b| b.text.starts_with("warn"))
            .unwrap();
        assert_eq!(warn.col, 0);
        assert_eq!(warn.fg, palette.ansi[1]);
        // Each wide character takes two cells: "本" starts at column 7.
        let hon = row.text.iter().find(|b| b.text == "本").unwrap();
        assert_eq!((hon.col, hon.cell_width), (7, 2));
        let cursor = snap.cursor.unwrap();
        assert_eq!((cursor.row, cursor.col), (0, 9));
    }

    #[test]
    fn selection_and_resize() {
        let mut m = TermModel::new(20, 3, 100);
        m.feed(b"one two three");
        m.start_selection(0, 4, false, 2);
        assert_eq!(m.selection_text().as_deref(), Some("two"));
        m.resize(40, 10);
        assert_eq!((m.cols(), m.rows()), (40, 10));
    }

    #[test]
    fn cursor_cell_follows_scrollback() {
        let mut m = TermModel::new(20, 3, 100);
        m.feed(b"ab\x1b[?25l");
        // Hidden, but it still knows where it is (for the IME text).
        assert_eq!(m.cursor_cell(), Some((0, 2)));
        m.feed(b"\r\n1\r\n2\r\n3\r\n4");
        assert_eq!(m.cursor_cell(), Some((2, 1)));
        // While looking at the history, the cursor may be off screen.
        m.scroll(3);
        assert_eq!(m.cursor_cell(), None);
    }

    #[test]
    fn text_around_the_cursor() {
        let mut m = TermModel::new(20, 5, 100);
        m.feed(b"output\r\nana@srv:~$ ls -l");
        assert_eq!(
            m.text_around_cursor(100),
            ("ana@srv:~$ ls -l".to_string(), true)
        );
        assert_eq!(m.text_around_cursor(4), ("ls -l"[1..].to_string(), true));
        // With the cursor moved left, there is text to its right.
        m.feed(b"\x1b[2D");
        assert_eq!(
            m.text_around_cursor(100),
            ("ana@srv:~$ ls ".to_string(), false)
        );
        // Rows split for being long are joined; wide characters count once.
        let mut m = TermModel::new(10, 5, 100);
        m.feed("$ echo 0123456789日本".as_bytes());
        assert_eq!(
            m.text_around_cursor(100),
            ("$ echo 0123456789日本".to_string(), true)
        );
        // Right when the row is full, the last character already counts.
        let mut m = TermModel::new(10, 5, 100);
        m.feed(b"$ 12345678");
        assert_eq!(m.text_around_cursor(100), ("$ 12345678".to_string(), true));
    }

    #[test]
    fn terminal_replies_are_forwarded() {
        let mut m = TermModel::new(20, 3, 100);
        // Cursor position request (DSR): the emulator replies.
        let events = m.feed(b"ab\x1b[6n");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, TermEvent::Write(b) if b == b"\x1b[1;3R"))
        );
    }
}
