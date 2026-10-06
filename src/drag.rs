//! Dragging tabs and panes with the mouse: reorder the tabs of the title
//! bar, drop a tab on a side of a terminal to make a split view, and take a
//! pane out of a split view by dropping it on the tab bar.
//!
//! What is dragged ([`DraggedTab`], [`DraggedPane`]), the little card that
//! follows the pointer ([`DragPreview`]) and the pure logic of the tab order
//! live here; the drop targets are in `app.rs` and where panes land in a
//! grid is `panes::drop_index`.

use gpui::{
    Context, EntityId, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px,
};
use gpui_component::{ActiveTheme, h_flex};

use crate::ui::{self, IconName};

/// A tab of the title bar being dragged.
#[derive(Debug, Clone)]
pub struct DraggedTab {
    /// The window (its `AppView`) the tab belongs to: tabs do not move
    /// between windows.
    pub app: EntityId,
    /// Id of the tab (not its position, which changes).
    pub tab: usize,
    pub title: SharedString,
    pub icon: IconName,
}

/// A terminal of a split view being dragged by its toolbar.
#[derive(Debug, Clone)]
pub struct DraggedPane {
    /// The terminal (`TerminalView` entity).
    pub terminal: EntityId,
    pub title: SharedString,
}

/// What follows the pointer while dragging.
pub struct DragPreview {
    title: SharedString,
    icon: IconName,
}

impl DragPreview {
    pub fn new(title: SharedString, icon: IconName) -> Self {
        Self { title, icon }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .h(px(28.))
            .max_w(px(240.))
            .px_3()
            .gap_2()
            .items_center()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.primary)
            .bg(theme.tab_active)
            .text_color(theme.tab_active_foreground)
            .opacity(0.9)
            .shadow_md()
            .child(ui::icon(self.icon).size(px(14.)))
            .child(
                div()
                    .text_sm()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(self.title.clone()),
            )
    }
}

/// Where the item at `from` ends up when dropped in `slot` of a list of
/// `len` items (`slot` is a gap: 0 before the first item, `len` after the
/// last). `None` when it stays where it is.
pub fn move_target(len: usize, from: usize, slot: usize) -> Option<usize> {
    if from >= len {
        return None;
    }
    let slot = slot.min(len);
    let to = if slot > from { slot - 1 } else { slot };
    (to != from).then_some(to)
}

/// The gap a tab is dropped in: before or after the tab at `ix`.
pub fn slot_of(ix: usize, before: bool) -> usize {
    if before { ix } else { ix + 1 }
}

/// Where the item at `ix` is after moving the one at `from` to `to`.
pub fn index_after_move(ix: usize, from: usize, to: usize) -> usize {
    if ix == from {
        to
    } else if from < ix && ix <= to {
        ix - 1
    } else if to <= ix && ix < from {
        ix + 1
    } else {
        ix
    }
}

/// Moves the item at `from` into `slot` (see [`move_target`]). Returns where
/// it went, or `None` if nothing moved.
pub fn move_item<T>(items: &mut Vec<T>, from: usize, slot: usize) -> Option<usize> {
    let to = move_target(items.len(), from, slot)?;
    let item = items.remove(from);
    items.insert(to, item);
    Some(to)
}

/// The slot of the tab to the left (`-1`) or right (`+1`) of `ix`, for the
/// keyboard ("Move tab left/right"). `None` at the ends.
pub fn step_slot(len: usize, ix: usize, right: bool) -> Option<usize> {
    if ix >= len {
        return None;
    }
    if right {
        (ix + 1 < len).then_some(ix + 2)
    } else {
        ix.checked_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        // A B C D: B dropped before D → A C B D.
        assert_eq!(move_target(4, 1, 3), Some(2));
        // B dropped after D (the end).
        assert_eq!(move_target(4, 1, 4), Some(3));
        // D dropped at the start.
        assert_eq!(move_target(4, 3, 0), Some(0));
        // Next to itself: it does not move.
        assert_eq!(move_target(4, 1, 1), None);
        assert_eq!(move_target(4, 1, 2), None);
        // Out of range.
        assert_eq!(move_target(4, 4, 0), None);
        assert_eq!(move_target(4, 0, 99), Some(3));
        assert_eq!(slot_of(2, true), 2);
        assert_eq!(slot_of(2, false), 3);
    }

    #[test]
    fn moving_items() {
        let mut v = vec!['a', 'b', 'c', 'd'];
        assert_eq!(move_item(&mut v, 1, 3), Some(2));
        assert_eq!(v, vec!['a', 'c', 'b', 'd']);
        assert_eq!(move_item(&mut v, 3, 0), Some(0));
        assert_eq!(v, vec!['d', 'a', 'c', 'b']);
        assert_eq!(move_item(&mut v, 0, 1), None);
        assert_eq!(v, vec!['d', 'a', 'c', 'b']);
        assert_eq!(move_item(&mut v, 0, 4), Some(3));
        assert_eq!(v, vec!['a', 'c', 'b', 'd']);
    }

    #[test]
    fn indexes_follow_the_move() {
        // Every index keeps pointing at the same item.
        for len in 1..6 {
            for from in 0..len {
                for slot in 0..=len {
                    let Some(to) = move_target(len, from, slot) else {
                        continue;
                    };
                    let mut v: Vec<usize> = (0..len).collect();
                    move_item(&mut v, from, slot);
                    for ix in 0..len {
                        assert_eq!(v[index_after_move(ix, from, to)], ix);
                    }
                }
            }
        }
    }

    #[test]
    fn keyboard_steps() {
        assert_eq!(step_slot(3, 0, false), None);
        assert_eq!(step_slot(3, 1, false), Some(0));
        assert_eq!(step_slot(3, 1, true), Some(3));
        assert_eq!(step_slot(3, 2, true), None);
        assert_eq!(move_target(3, 1, step_slot(3, 1, true).unwrap()), Some(2));
        assert_eq!(move_target(3, 1, step_slot(3, 1, false).unwrap()), Some(0));
        assert_eq!(step_slot(0, 0, true), None);
    }
}
