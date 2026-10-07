//! Find in the terminal (screen and history): the pattern handed to the
//! emulator's regex search from what is typed and the toggles, the order of
//! the matches and which one is current, and the cells to highlight.
//!
//! Matches are listed from the top of the history down. They are numbered
//! from the bottom ("1 of 12" is the newest), because a terminal is searched
//! upwards: Enter (and F3) goes to the next older match, Shift+Enter (and
//! Shift+F3) back to a newer one, wrapping around at both ends.

use alacritty_terminal::index::Point;

/// A match: first and last cell (inclusive).
pub type Match = std::ops::RangeInclusive<Point>;

/// Most matches counted (and walked through); beyond it the count shows
/// "N+".
pub const MAX_MATCHES: usize = 5000;

/// Toggles of the find bar.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FindOptions {
    /// Upper and lower case are different letters.
    pub case_sensitive: bool,
    /// The text is a regular expression.
    pub regex: bool,
}

/// Regular expression that matches `text` literally.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Pattern for the regex search of the emulator (`None` for an empty
/// search). The case flag goes first: the emulator would otherwise ignore
/// case unless the text has capitals.
pub fn pattern(query: &str, options: FindOptions) -> Option<String> {
    if query.is_empty() {
        return None;
    }
    let body = if options.regex {
        query.to_string()
    } else {
        escape(query)
    };
    let flag = if options.case_sensitive {
        "(?-i)"
    } else {
        "(?i)"
    };
    Some(format!("{flag}{body}"))
}

/// Index of `m` in `list` (sorted from the top down), if it is there.
pub fn position(list: &[Match], m: &Match) -> Option<usize> {
    let ix = list.partition_point(|x| x.start() < m.start());
    (list.get(ix) == Some(m)).then_some(ix)
}

/// The match to start from: the last one that begins at or above `anchor`
/// (the bottom of what is on screen, or the current match), or the first
/// one if all of them are below it.
pub fn nearest(list: &[Match], anchor: Point) -> Option<usize> {
    if list.is_empty() {
        return None;
    }
    let after = list.partition_point(|x| *x.start() <= anchor);
    Some(after.saturating_sub(1))
}

/// The next match: `older` goes up (the previous index), otherwise down;
/// both wrap around. Without a current one, the search starts at the
/// bottom (older) or the top (newer).
pub fn step(len: usize, current: Option<usize>, older: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (current, older) {
        (None, true) => len - 1,
        (None, false) => 0,
        (Some(i), true) => (i.min(len - 1) + len - 1) % len,
        (Some(i), false) => (i.min(len - 1) + 1) % len,
    })
}

/// Number shown for match `index` of `len` ("1 of 12" is the newest, at the
/// bottom).
pub fn ordinal(len: usize, index: usize) -> usize {
    len.saturating_sub(index.min(len.saturating_sub(1)))
}

/// Cells to paint over a match on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Highlight {
    pub row: usize,
    pub col: usize,
    pub cells: usize,
    /// The current match (painted differently).
    pub current: bool,
}

/// The visible pieces of a match (one per screen row it covers), for a
/// screen of `lines` × `cols` scrolled `display_offset` lines up.
pub fn highlights(
    m: &Match,
    display_offset: usize,
    lines: usize,
    cols: usize,
    current: bool,
) -> Vec<Highlight> {
    let mut out = Vec::new();
    let (start, end) = (m.start(), m.end());
    if cols == 0 || end < start {
        return out;
    }
    for line in start.line.0..=end.line.0 {
        let row = line + display_offset as i32;
        if row < 0 || row as usize >= lines {
            continue;
        }
        let first = if line == start.line.0 {
            start.column.0
        } else {
            0
        };
        let last = if line == end.line.0 {
            end.column.0
        } else {
            cols - 1
        };
        let last = last.min(cols - 1);
        if first > last {
            continue;
        }
        out.push(Highlight {
            row: row as usize,
            col: first,
            cells: last - first + 1,
            current,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::index::{Column, Line};

    fn p(line: i32, col: usize) -> Point {
        Point::new(Line(line), Column(col))
    }

    fn m(line: i32, col: usize, len: usize) -> Match {
        p(line, col)..=p(line, col + len - 1)
    }

    #[test]
    fn patterns() {
        let plain = FindOptions::default();
        assert_eq!(pattern("", plain), None);
        assert_eq!(pattern("a.b", plain).unwrap(), "(?i)a\\.b");
        assert_eq!(
            pattern(
                "Error",
                FindOptions {
                    case_sensitive: true,
                    regex: false
                }
            )
            .unwrap(),
            "(?-i)Error"
        );
        assert_eq!(
            pattern(
                "err(or)?\\d+",
                FindOptions {
                    case_sensitive: false,
                    regex: true
                }
            )
            .unwrap(),
            "(?i)err(or)?\\d+"
        );
        assert_eq!(escape("[x]*"), "\\[x\\]\\*");
    }

    #[test]
    fn walking_wraps_both_ways() {
        assert_eq!(step(0, None, true), None);
        assert_eq!(step(3, None, true), Some(2));
        assert_eq!(step(3, None, false), Some(0));
        assert_eq!(step(3, Some(2), true), Some(1));
        assert_eq!(step(3, Some(0), true), Some(2));
        assert_eq!(step(3, Some(2), false), Some(0));
        assert_eq!(step(3, Some(1), false), Some(2));
        // A stale index past the end is clamped first.
        assert_eq!(step(2, Some(7), true), Some(0));
        assert_eq!(step(1, Some(0), true), Some(0));
    }

    #[test]
    fn numbered_from_the_bottom() {
        assert_eq!(ordinal(12, 11), 1);
        assert_eq!(ordinal(12, 0), 12);
        assert_eq!(ordinal(12, 9), 3);
        assert_eq!(ordinal(1, 0), 1);
    }

    #[test]
    fn nearest_and_position() {
        let list = vec![m(-20, 0, 3), m(-5, 4, 3), m(2, 0, 3), m(2, 10, 3)];
        // At the bottom of the screen: the last one.
        assert_eq!(nearest(&list, p(23, 79)), Some(3));
        // Scrolled up: the last one above the bottom of the view.
        assert_eq!(nearest(&list, p(-1, 79)), Some(1));
        assert_eq!(nearest(&list, p(2, 5)), Some(2));
        // Everything below: the first.
        assert_eq!(nearest(&list, p(-30, 0)), Some(0));
        assert_eq!(nearest(&[], p(0, 0)), None);

        assert_eq!(position(&list, &m(2, 10, 3)), Some(3));
        assert_eq!(position(&list, &m(-5, 4, 3)), Some(1));
        assert_eq!(position(&list, &m(-5, 4, 2)), None);
        assert_eq!(position(&list, &m(0, 0, 1)), None);
    }

    #[test]
    fn highlights_follow_the_view() {
        // One row, on screen.
        assert_eq!(
            highlights(&m(3, 5, 4), 0, 24, 80, true),
            vec![Highlight {
                row: 3,
                col: 5,
                cells: 4,
                current: true
            }]
        );
        // In the history, seen when scrolled up 10 lines.
        assert_eq!(highlights(&m(-4, 0, 2), 10, 24, 80, false)[0].row, 6);
        // Not on screen.
        assert!(highlights(&m(-4, 0, 2), 0, 24, 80, false).is_empty());
        assert!(highlights(&m(30, 0, 2), 0, 24, 80, false).is_empty());
        // Across a wrapped line: the end of one row and the start of the next.
        let wrapped = p(1, 76)..=p(2, 2);
        assert_eq!(
            highlights(&wrapped, 0, 24, 80, false),
            vec![
                Highlight {
                    row: 1,
                    col: 76,
                    cells: 4,
                    current: false
                },
                Highlight {
                    row: 2,
                    col: 0,
                    cells: 3,
                    current: false
                },
            ]
        );
        // Only the part on screen.
        assert_eq!(
            highlights(&(p(-1, 70)..=p(0, 3)), 0, 24, 80, false).len(),
            1
        );
    }
}
