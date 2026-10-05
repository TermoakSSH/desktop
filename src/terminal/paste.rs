//! Copy and paste preferences of the terminal and the decisions that depend
//! on them: what the right mouse button does, whether a plain Ctrl+V pastes
//! and when a paste asks for confirmation first.

use serde::{Deserialize, Serialize};

/// What the right mouse button does in the terminal (when the program
/// running in it does not use the mouse).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RightClick {
    /// Menu with Copy, Paste, Select all, Clear, Find...
    #[default]
    Menu,
    /// Pastes the clipboard (like PuTTY or Windows Terminal).
    Paste,
    /// Copies the selection, or pastes if nothing is selected.
    CopyOrPaste,
}

impl RightClick {
    pub const ALL: [RightClick; 3] = [RightClick::Menu, RightClick::Paste, RightClick::CopyOrPaste];
}

/// What a right click does now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightClickAction {
    ShowMenu,
    Paste,
    /// Copy the selection (and clear it, like PuTTY).
    Copy,
}

pub fn right_click_action(setting: RightClick, has_selection: bool) -> RightClickAction {
    match setting {
        RightClick::Menu => RightClickAction::ShowMenu,
        RightClick::Paste => RightClickAction::Paste,
        RightClick::CopyOrPaste if has_selection => RightClickAction::Copy,
        RightClick::CopyOrPaste => RightClickAction::Paste,
    }
}

/// Lines of a paste, not counting a final line break (a copied line usually
/// ends with one).
pub fn line_count(text: &str) -> usize {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.trim_end_matches('\n');
    if text.is_empty() {
        0
    } else {
        text.split('\n').count()
    }
}

/// Whether pasting `text` asks first: only with the option on, for more
/// than one line, and when the program has not enabled bracketed paste
/// (with it, the shell does not run the lines until Enter is pressed).
pub fn needs_confirmation(text: &str, confirm: bool, bracketed: bool) -> bool {
    confirm && !bracketed && line_count(text) > 1
}

/// Whether a key press is a plain Ctrl+V that should paste: only off macOS
/// (there Cmd+V pastes) and with the option on. Ctrl+V is a control
/// character in terminals (the shell's "insert the next key literally", vim's
/// block selection), so by default it goes to the program.
pub fn ctrl_v_pastes(
    is_macos: bool,
    option_on: bool,
    key: &str,
    control: bool,
    shift: bool,
    alt: bool,
    platform: bool,
) -> bool {
    !is_macos && option_on && key == "v" && control && !shift && !alt && !platform
}

/// First lines of a paste for the confirmation dialog.
pub fn preview(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text
        .trim_end_matches(['\r', '\n'])
        .split('\n')
        .map(|l| l.trim_end_matches('\r'))
        .collect();
    let mut out: Vec<String> = lines
        .iter()
        .take(max_lines)
        .map(|l| {
            if l.chars().count() > 120 {
                let cut: String = l.chars().take(119).collect();
                format!("{cut}…")
            } else {
                l.to_string()
            }
        })
        .collect();
    if lines.len() > max_lines {
        out.push("…".into());
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn right_click() {
        use RightClickAction as A;
        assert_eq!(right_click_action(RightClick::Menu, true), A::ShowMenu);
        assert_eq!(right_click_action(RightClick::Paste, true), A::Paste);
        assert_eq!(right_click_action(RightClick::CopyOrPaste, true), A::Copy);
        assert_eq!(right_click_action(RightClick::CopyOrPaste, false), A::Paste);
        assert_eq!(RightClick::default(), RightClick::Menu);
    }

    #[test]
    fn lines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("ls"), 1);
        assert_eq!(line_count("ls\n"), 1);
        assert_eq!(line_count("ls\r\n"), 1);
        assert_eq!(line_count("ls\npwd"), 2);
        assert_eq!(line_count("ls\r\npwd\r\n"), 2);
        assert_eq!(line_count("a\rb\rc"), 3);
        assert_eq!(line_count("a\n\nb\n"), 3);
    }

    #[test]
    fn confirmation() {
        assert!(needs_confirmation("rm -rf x\nreboot\n", true, false));
        assert!(!needs_confirmation("rm -rf x\nreboot\n", false, false));
        // The shell will not run it until Enter: no need to ask.
        assert!(!needs_confirmation("rm -rf x\nreboot\n", true, true));
        // A single line, even with its line break.
        assert!(!needs_confirmation("uptime\n", true, false));
    }

    #[test]
    fn plain_ctrl_v() {
        assert!(ctrl_v_pastes(false, true, "v", true, false, false, false));
        assert!(!ctrl_v_pastes(false, false, "v", true, false, false, false));
        assert!(!ctrl_v_pastes(true, true, "v", true, false, false, false));
        // Ctrl+Shift+V is the normal paste shortcut, handled elsewhere.
        assert!(!ctrl_v_pastes(false, true, "v", true, true, false, false));
        assert!(!ctrl_v_pastes(false, true, "v", true, false, true, false));
        assert!(!ctrl_v_pastes(false, true, "c", true, false, false, false));
    }

    #[test]
    fn previews() {
        assert_eq!(preview("a\nb\nc\n", 2), "a\nb\n…");
        assert_eq!(preview("a\r\nb", 5), "a\nb");
        let long = "x".repeat(200);
        assert_eq!(preview(&long, 1).chars().count(), 120);
    }
}
