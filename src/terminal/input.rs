//! Translation of keys to xterm sequences.
//!
//! Special keys (arrows, function keys, Enter...), Ctrl combinations and Alt
//! as Meta are translated here, when pressed. Normal text is not: the
//! platform input handler ([`gpui::EntityInputHandler`]) delivers it already
//! composed (dead keys, AltGr, compose sequences and IME), so every character
//! arrives once and well formed.

use alacritty_terminal::term::TermMode;
use gpui::Keystroke;

/// Bytes to send to the server for a key press, or `None` if the key does
/// not produce direct input: a lone modifier, a dead key or normal text,
/// which arrives later through the input handler.
///
/// `prefer_character_input` is GPUI's flag for the keys the system composes
/// (AltGr and dead keys on Windows): they are always text.
pub fn to_bytes(ks: &Keystroke, prefer_character_input: bool, mode: TermMode) -> Option<Vec<u8>> {
    let m = &ks.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    // xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
    let modifier = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.control as u8;
    let key = ks.key.as_str();

    let csi_letter = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{letter}").into_bytes()
        } else {
            format!("\x1b[{letter}").into_bytes()
        }
    };
    let csi_tilde = |code: u8| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[{code};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{code}~").into_bytes()
        }
    };
    let ss3 = |letter: char| -> Vec<u8> {
        if modifier > 1 {
            format!("\x1b[1;{modifier}{letter}").into_bytes()
        } else {
            format!("\x1bO{letter}").into_bytes()
        }
    };

    let special = match key {
        "up" => Some(csi_letter('A')),
        "down" => Some(csi_letter('B')),
        "right" => Some(csi_letter('C')),
        "left" => Some(csi_letter('D')),
        "home" => Some(csi_letter('H')),
        "end" => Some(csi_letter('F')),
        "insert" => Some(csi_tilde(2)),
        "delete" => Some(csi_tilde(3)),
        "pageup" => Some(csi_tilde(5)),
        "pagedown" => Some(csi_tilde(6)),
        "f1" => Some(ss3('P')),
        "f2" => Some(ss3('Q')),
        "f3" => Some(ss3('R')),
        "f4" => Some(ss3('S')),
        "f5" => Some(csi_tilde(15)),
        "f6" => Some(csi_tilde(17)),
        "f7" => Some(csi_tilde(18)),
        "f8" => Some(csi_tilde(19)),
        "f9" => Some(csi_tilde(20)),
        "f10" => Some(csi_tilde(21)),
        "f11" => Some(csi_tilde(23)),
        "f12" => Some(csi_tilde(24)),
        "enter" => Some(if m.alt {
            b"\x1b\r".to_vec()
        } else {
            b"\r".to_vec()
        }),
        "tab" => Some(if m.shift {
            b"\x1b[Z".to_vec()
        } else {
            b"\t".to_vec()
        }),
        "escape" => Some(b"\x1b".to_vec()),
        "backspace" => Some(if m.control {
            vec![0x08]
        } else if m.alt {
            b"\x1b\x7f".to_vec()
        } else {
            vec![0x7f]
        }),
        "space" if m.control => Some(vec![0x00]),
        _ => None,
    };
    if special.is_some() {
        return special;
    }

    let composed = ks.key_char.as_deref().filter(|c| !c.is_empty());

    // Character composed by the system (AltGr, dead key): it is text.
    if prefer_character_input && composed.is_some() {
        return None;
    }

    // Ctrl combinations: classic control codes.
    if m.control && !m.platform {
        let c = key.chars().next()?;
        if key.chars().count() == 1 {
            let code = match c.to_ascii_lowercase() {
                c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
                '@' | '2' => Some(0x00),
                '[' | '3' => Some(0x1b),
                '\\' | '4' => Some(0x1c),
                ']' | '5' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '-' | '7' => Some(0x1f),
                '/' => Some(0x1f),
                '?' | '8' => Some(0x7f),
                _ => None,
            };
            if let Some(code) = code {
                return Some(if m.alt { vec![0x1b, code] } else { vec![code] });
            }
        }
        return None;
    }
    if m.platform {
        return None;
    }

    // Without a system character nothing is sent: it is a dead key (on X11
    // the key seen is the ASCII one at that position on a US keyboard, not
    // what is typed) or a key without text.
    let text = composed?;

    // Alt as Meta: ESC in front. If Alt (Option on macOS) composed a character
    // other than the key, it is text and the input handler delivers it.
    let base = if key == "space" { " " } else { key };
    if m.alt && text.to_lowercase() == base.to_lowercase() {
        let mut out = vec![0x1b];
        out.extend_from_slice(text.as_bytes());
        return Some(out);
    }

    // Normal text: it arrives through the input handler (once and composed).
    None
}

/// Bytes for text committed by the input handler (keyboard, dead keys or
/// IME). Line breaks are sent as Enter.
pub fn text_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

/// Length in UTF-16 units (the positions IMEs use).
pub fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Cells (approximately) taken by the first `utf16` units of `text`: East
/// Asian characters and emoji take two. Used to place the IME candidate
/// window.
pub fn cells_before(text: &str, utf16: usize) -> usize {
    let mut units = 0;
    let mut cells = 0;
    for c in text.chars() {
        if units >= utf16 {
            break;
        }
        units += c.len_utf16();
        cells += if is_wide(c) { 2 } else { 1 };
    }
    cells
}

pub fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
            | 0x20000..=0x3FFFD
    )
}

/// Prepares pasted text: line breaks as carriage returns and, if the program
/// asks for it, bracketed paste delimiters.
pub fn paste_bytes(text: &str, mode: TermMode) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if mode.contains(TermMode::BRACKETED_PASTE) {
        // Embedded delimiters are removed so the paste cannot escape.
        let clean = normalized.replace("\x1b[201~", "").replace("\x1b[200~", "");
        let mut out = b"\x1b[200~".to_vec();
        out.extend_from_slice(clean.as_bytes());
        out.extend_from_slice(b"\x1b[201~");
        out
    } else {
        normalized.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn ks(key: &str, ch: Option<&str>, m: Modifiers) -> Keystroke {
        Keystroke {
            modifiers: m,
            key: key.into(),
            key_char: ch.map(Into::into),
        }
    }

    fn bytes(key: &str, ch: Option<&str>, m: Modifiers) -> Option<Vec<u8>> {
        to_bytes(&ks(key, ch, m), false, TermMode::empty())
    }

    const NONE: Modifiers = Modifiers {
        control: false,
        alt: false,
        shift: false,
        platform: false,
        function: false,
    };

    fn ctrl() -> Modifiers {
        Modifiers {
            control: true,
            ..Default::default()
        }
    }

    fn alt() -> Modifiers {
        Modifiers {
            alt: true,
            ..Default::default()
        }
    }

    #[test]
    fn special_keys() {
        assert_eq!(bytes("enter", None, NONE), Some(b"\r".to_vec()));
        assert_eq!(bytes("up", None, NONE), Some(b"\x1b[A".to_vec()));
        assert_eq!(
            to_bytes(&ks("up", None, NONE), false, TermMode::APP_CURSOR),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(bytes("c", None, ctrl()), Some(vec![3]));
        assert_eq!(bytes("left", None, ctrl()), Some(b"\x1b[1;5D".to_vec()));
        assert_eq!(bytes("f5", None, NONE), Some(b"\x1b[15~".to_vec()));
        assert_eq!(bytes("space", Some(" "), ctrl()), Some(vec![0]));
        assert_eq!(bytes("backspace", None, alt()), Some(b"\x1b\x7f".to_vec()));
    }

    #[test]
    fn plain_text_goes_through_the_input_handler() {
        // Letters, space and composed characters are not sent on key press:
        // the input handler delivers them (so they do not appear twice).
        assert_eq!(bytes("a", Some("a"), NONE), None);
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(bytes("a", Some("A"), shift), None);
        assert_eq!(bytes("space", Some(" "), NONE), None);
        assert_eq!(bytes("aacute", Some("á"), NONE), None);
        // Dead key (X11/Wayland): produces nothing until composed.
        assert_eq!(bytes("dead_acute", None, NONE), None);
        // AltGr on Windows (Ctrl+Alt with a character composed by the system).
        let altgr = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(
            to_bytes(&ks("2", Some("@"), altgr), true, TermMode::empty()),
            None
        );
        // Dead key on Windows: the system marks it as a composed character.
        assert_eq!(
            to_bytes(&ks("´", Some("´"), NONE), true, TermMode::empty()),
            None
        );
    }

    #[test]
    fn keys_without_system_character_send_nothing() {
        // Dead key on X11 with a Spanish layout: GPUI names it by the ASCII key
        // at that position (the apostrophe on a US keyboard), but it writes
        // nothing until composed.
        assert_eq!(bytes("'", None, NONE), None);
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(bytes("[", None, shift), None);
        assert_eq!(bytes("'", None, alt()), None);
    }

    #[test]
    fn alt_is_meta_unless_it_composes() {
        assert_eq!(bytes("b", Some("b"), alt()), Some(b"\x1bb".to_vec()));
        let alt_shift = Modifiers {
            alt: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(bytes("a", Some("A"), alt_shift), Some(b"\x1bA".to_vec()));
        assert_eq!(bytes(".", Some("."), alt()), Some(b"\x1b.".to_vec()));
        assert_eq!(bytes("space", Some(" "), alt()), Some(b"\x1b ".to_vec()));
        // Option on macOS composes another character: it is text.
        assert_eq!(bytes("a", Some("å"), alt()), None);
        // Ctrl+Alt+letter (without AltGr): ESC + control code.
        let ctrl_alt = Modifiers {
            control: true,
            alt: true,
            ..Default::default()
        };
        assert_eq!(bytes("x", None, ctrl_alt), Some(vec![0x1b, 0x18]));
    }

    #[test]
    fn committed_text() {
        assert_eq!(text_bytes("á"), "á".as_bytes().to_vec());
        assert_eq!(text_bytes("日本"), "日本".as_bytes().to_vec());
        assert_eq!(text_bytes("a\nb"), b"a\rb".to_vec());
        assert_eq!(utf16_len("aá日"), 3);
        assert_eq!(utf16_len("😀"), 2);
    }

    #[test]
    fn preedit_cells() {
        assert_eq!(cells_before("´", 1), 1);
        assert_eq!(cells_before("にほん", 0), 0);
        assert_eq!(cells_before("にほん", 2), 4);
        assert_eq!(cells_before("ab日本", 4), 6);
        // An emoji is two UTF-16 units and two cells.
        assert_eq!(cells_before("😀x", 2), 2);
        assert_eq!(cells_before("😀x", 3), 3);
    }

    #[test]
    fn paste_is_bracketed() {
        assert_eq!(paste_bytes("a\nb", TermMode::empty()), b"a\rb".to_vec());
        assert_eq!(
            paste_bytes("x", TermMode::BRACKETED_PASTE),
            b"\x1b[200~x\x1b[201~".to_vec()
        );
    }
}
