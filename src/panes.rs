//! Split view (workspaces): several terminals in one tab, laid out in a grid,
//! with one focused pane, an optional focus mode (the focused pane big, the
//! others small) and broadcast input (what is typed in the focused pane also
//! goes to the other panes that are not excluded).
//!
//! Only the pure logic lives here (layout, keyboard navigation and broadcast
//! routing); the views are in `app.rs`.

use std::collections::HashSet;
use std::hash::Hash;

/// Most panes a workspace can hold (a 4 × 4 grid).
pub const MAX_PANES: usize = 16;

/// Panes per row of the automatic grid, from top to bottom: 2 side by side,
/// 3 as 2 + 1, 4 as 2 × 2, 5 as 3 + 2, 6 as 3 × 2, 7 as 3 + 2 + 2... The
/// columns are `ceil(sqrt(n))` and the leftover cells are taken from the
/// last rows, so the rows are as even as possible and the wider ones are on
/// top.
pub fn grid_rows(n: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (1..=n).find(|c| c * c >= n).unwrap_or(n);
    let rows = n.div_ceil(cols);
    // Spread `n` over `rows` rows: the first `n % rows` rows get one more.
    let base = n / rows;
    let extra = n % rows;
    (0..rows)
        .map(|r| if r < extra { base + 1 } else { base })
        .collect()
}

/// Row and column of pane `ix` in the grid of `n` panes.
pub fn position(n: usize, ix: usize) -> Option<(usize, usize)> {
    let mut start = 0;
    for (row, count) in grid_rows(n).into_iter().enumerate() {
        if ix < start + count {
            return Some((row, ix - start));
        }
        start += count;
    }
    None
}

/// Direction to move the focus between panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// Pane reached from `from` moving in `dir` in the grid of `n` panes,
/// wrapping around at the edges. Moving up or down picks the pane of the
/// other row whose horizontal centre is closest (rows can have different
/// widths).
pub fn neighbor(n: usize, from: usize, dir: Dir) -> Option<usize> {
    if n < 2 || from >= n {
        return None;
    }
    let rows = grid_rows(n);
    let (row, col) = position(n, from)?;
    let starts: Vec<usize> = rows
        .iter()
        .scan(0, |acc, c| {
            let s = *acc;
            *acc += c;
            Some(s)
        })
        .collect();
    match dir {
        Dir::Left | Dir::Right => {
            let count = rows[row];
            if count < 2 {
                // Alone in its row: left/right walk the whole list.
                return Some(match dir {
                    Dir::Left => (from + n - 1) % n,
                    _ => (from + 1) % n,
                });
            }
            let next = match dir {
                Dir::Left => (col + count - 1) % count,
                _ => (col + 1) % count,
            };
            Some(starts[row] + next)
        }
        Dir::Up | Dir::Down => {
            if rows.len() < 2 {
                return None;
            }
            let target = match dir {
                Dir::Up => (row + rows.len() - 1) % rows.len(),
                _ => (row + 1) % rows.len(),
            };
            // Horizontal centre as a fraction of the width.
            let centre = (col as f32 + 0.5) / rows[row] as f32;
            let count = rows[target];
            let col = ((centre * count as f32).floor() as usize).min(count - 1);
            Some(starts[target] + col)
        }
    }
}

/// Panes that receive what is typed in `source` while broadcasting: every
/// other pane that is not excluded, in order. Nothing when broadcasting is
/// off or the source itself is not part of the workspace.
pub fn broadcast_targets<T: Copy + Eq + Hash>(
    panes: &[T],
    source: T,
    on: bool,
    excluded: &HashSet<T>,
) -> Vec<T> {
    if !on || !panes.contains(&source) || excluded.contains(&source) {
        return Vec::new();
    }
    panes
        .iter()
        .copied()
        .filter(|p| *p != source && !excluded.contains(p))
        .collect()
}

/// How many terminals receive the input while broadcasting (the banner's
/// "Broadcasting to N terminals"): the included panes, the focused one too.
pub fn broadcast_count<T: Copy + Eq + Hash>(panes: &[T], excluded: &HashSet<T>) -> usize {
    panes.iter().filter(|p| !excluded.contains(p)).count()
}

/// Index to focus after closing pane `closed` of `n` (before closing) when
/// `focused` had the focus.
pub fn focus_after_close(n: usize, focused: usize, closed: usize) -> Option<usize> {
    if n <= 1 {
        return None;
    }
    let left = n - 1;
    Some(if focused > closed {
        focused - 1
    } else if focused == closed {
        closed.min(left - 1)
    } else {
        focused
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_layouts() {
        assert_eq!(grid_rows(0), Vec::<usize>::new());
        assert_eq!(grid_rows(1), vec![1]);
        assert_eq!(grid_rows(2), vec![2]);
        assert_eq!(grid_rows(3), vec![2, 1]);
        assert_eq!(grid_rows(4), vec![2, 2]);
        assert_eq!(grid_rows(5), vec![3, 2]);
        assert_eq!(grid_rows(6), vec![3, 3]);
        assert_eq!(grid_rows(7), vec![3, 2, 2]);
        assert_eq!(grid_rows(9), vec![3, 3, 3]);
        assert_eq!(grid_rows(10), vec![4, 3, 3]);
        assert_eq!(grid_rows(12), vec![4, 4, 4]);
        assert_eq!(grid_rows(13), vec![4, 3, 3, 3]);
        assert_eq!(grid_rows(16), vec![4, 4, 4, 4]);
        for n in 1..=MAX_PANES {
            let rows = grid_rows(n);
            assert_eq!(rows.iter().sum::<usize>(), n, "{n}");
            assert!(rows.len() <= 4 && rows.iter().all(|c| *c <= 4), "{n}");
            assert!(
                rows.windows(2).all(|w| w[0] >= w[1]),
                "{n}: wider rows first"
            );
        }
    }

    #[test]
    fn positions() {
        assert_eq!(position(3, 0), Some((0, 0)));
        assert_eq!(position(3, 1), Some((0, 1)));
        assert_eq!(position(3, 2), Some((1, 0)));
        assert_eq!(position(3, 3), None);
        assert_eq!(position(7, 6), Some((2, 1)));
    }

    #[test]
    fn keyboard_navigation() {
        // 2 × 2: 0 1 / 2 3
        assert_eq!(neighbor(4, 0, Dir::Right), Some(1));
        assert_eq!(neighbor(4, 1, Dir::Right), Some(0));
        assert_eq!(neighbor(4, 0, Dir::Left), Some(1));
        assert_eq!(neighbor(4, 0, Dir::Down), Some(2));
        assert_eq!(neighbor(4, 3, Dir::Up), Some(1));
        assert_eq!(neighbor(4, 3, Dir::Down), Some(1));
        // Side by side: no rows above or below.
        assert_eq!(neighbor(2, 0, Dir::Down), None);
        assert_eq!(neighbor(2, 0, Dir::Right), Some(1));
        // 3 = 2 + 1: the bottom one is alone in its row.
        assert_eq!(neighbor(3, 2, Dir::Up), Some(1));
        assert_eq!(neighbor(3, 0, Dir::Down), Some(2));
        assert_eq!(neighbor(3, 1, Dir::Down), Some(2));
        assert_eq!(neighbor(3, 2, Dir::Right), Some(0));
        assert_eq!(neighbor(3, 2, Dir::Left), Some(1));
        // 5 = 3 + 2: from the middle of the top row down to the closest.
        assert_eq!(neighbor(5, 1, Dir::Down), Some(4));
        assert_eq!(neighbor(5, 0, Dir::Down), Some(3));
        assert_eq!(neighbor(5, 4, Dir::Up), Some(2));
        // A single pane or out of range: nowhere to go.
        assert_eq!(neighbor(1, 0, Dir::Right), None);
        assert_eq!(neighbor(4, 9, Dir::Right), None);
    }

    #[test]
    fn broadcast_routing() {
        let panes = [10, 11, 12, 13];
        let none = HashSet::new();
        assert_eq!(broadcast_targets(&panes, 11, true, &none), vec![10, 12, 13]);
        assert!(broadcast_targets(&panes, 11, false, &none).is_empty());
        let excluded: HashSet<_> = [12].into_iter().collect();
        assert_eq!(broadcast_targets(&panes, 10, true, &excluded), vec![11, 13]);
        // An excluded pane types only into itself.
        assert!(broadcast_targets(&panes, 12, true, &excluded).is_empty());
        // A terminal of another tab is not a source.
        assert!(broadcast_targets(&panes, 99, true, &none).is_empty());
        assert_eq!(broadcast_count(&panes, &excluded), 3);
        assert_eq!(broadcast_count(&panes, &none), 4);
    }

    #[test]
    fn focus_after_closing() {
        assert_eq!(focus_after_close(1, 0, 0), None);
        assert_eq!(focus_after_close(3, 2, 2), Some(1));
        assert_eq!(focus_after_close(3, 0, 0), Some(0));
        assert_eq!(focus_after_close(3, 2, 0), Some(1));
        assert_eq!(focus_after_close(3, 0, 2), Some(0));
    }
}
