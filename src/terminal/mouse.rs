//! Mouse reports for terminal programs (vim, htop, tmux, mc...): when the
//! program enables an xterm mouse mode, clicks, drags, motion and the wheel
//! are sent to it as escape sequences instead of selecting text or scrolling
//! the history.
//!
//! Modes (`TermMode`): `MOUSE_REPORT_CLICK` (1000, press and release),
//! `MOUSE_DRAG` (1002, drags too) and `MOUSE_MOTION` (1003, any motion).
//! Encodings: SGR (1006), UTF-8 (1005) or the classic X10 one, which only
//! reaches column/row 223.

use alacritty_terminal::term::TermMode;

/// Button of the report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// Motion without any button pressed.
    None,
}

impl Button {
    fn code(self) -> u8 {
        match self {
            Button::Left => 0,
            Button::Middle => 1,
            Button::Right => 2,
            Button::None => 3,
            Button::WheelUp => 64,
            Button::WheelDown => 65,
        }
    }

    fn is_wheel(self) -> bool {
        matches!(self, Button::WheelUp | Button::WheelDown)
    }
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Press,
    Release,
    Motion,
}

/// Modifiers encoded in the report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

impl Mods {
    fn bits(self) -> u8 {
        4 * self.shift as u8 + 8 * self.alt as u8 + 16 * self.ctrl as u8
    }
}

/// A mouse event on cell (`col`, `row`), counting from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub button: Button,
    pub action: Action,
    pub col: usize,
    pub row: usize,
    pub mods: Mods,
}

/// Has the program asked for mouse reports?
pub fn reporting(mode: TermMode) -> bool {
    mode.intersects(TermMode::MOUSE_MODE)
}

/// Must this event be reported with the active modes?
pub fn wants(mode: TermMode, action: Action, button: Button) -> bool {
    match action {
        Action::Press | Action::Release => reporting(mode),
        Action::Motion if button == Button::None => mode.contains(TermMode::MOUSE_MOTION),
        Action::Motion => mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION),
    }
}

/// Escape sequence of the event, or `None` if nothing must be sent (a mode
/// without that kind of report, releasing the wheel or coordinates the
/// classic encoding cannot represent).
pub fn encode(r: &Report, mode: TermMode) -> Option<Vec<u8>> {
    if !wants(mode, r.action, r.button) {
        return None;
    }
    // The wheel is only reported as a press.
    if r.button.is_wheel() && r.action != Action::Press {
        return None;
    }
    let mut code = r.button.code() + r.mods.bits();
    if r.action == Action::Motion {
        code += 32;
    }
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if r.action == Action::Release {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{};{}{end}", r.col + 1, r.row + 1).into_bytes());
    }

    // Classic encoding: a release does not say which button it was.
    if r.action == Action::Release {
        code = Button::None.code() + r.mods.bits();
    }
    let utf8 = mode.contains(TermMode::UTF8_MOUSE);
    // Each coordinate is a byte (or UTF-8 character) of value 32 + 1 + n.
    let max = if utf8 { 2015 } else { 223 };
    if r.col >= max || r.row >= max {
        return None;
    }
    let mut out = b"\x1b[M".to_vec();
    out.push(32 + code);
    push_coord(&mut out, r.col, utf8);
    push_coord(&mut out, r.row, utf8);
    Some(out)
}

fn push_coord(out: &mut Vec<u8>, n: usize, utf8: bool) {
    let v = 32 + 1 + n as u32;
    if utf8 && v >= 0x80 {
        out.push((0xC0 | (v >> 6)) as u8);
        out.push((0x80 | (v & 0x3F)) as u8);
    } else {
        out.push(v as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(button: Button, action: Action, col: usize, row: usize) -> Report {
        Report {
            button,
            action,
            col,
            row,
            mods: Mods::default(),
        }
    }

    const CLICK: TermMode = TermMode::MOUSE_REPORT_CLICK;

    fn sgr(mode: TermMode) -> TermMode {
        mode | TermMode::SGR_MOUSE
    }

    fn enc(r: Report, mode: TermMode) -> Option<String> {
        encode(&r, mode).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    #[test]
    fn nothing_without_mouse_mode() {
        let r = report(Button::Left, Action::Press, 0, 0);
        assert_eq!(encode(&r, TermMode::empty()), None);
        assert_eq!(encode(&r, TermMode::SGR_MOUSE), None);
        assert!(!reporting(TermMode::SGR_MOUSE | TermMode::ALT_SCREEN));
        assert!(reporting(TermMode::MOUSE_DRAG));
    }

    #[test]
    fn sgr_press_release_and_coordinates() {
        let mode = sgr(CLICK);
        // Coordinates from 1.
        assert_eq!(
            enc(report(Button::Left, Action::Press, 0, 0), mode).as_deref(),
            Some("\x1b[<0;1;1M")
        );
        assert_eq!(
            enc(report(Button::Left, Action::Release, 9, 4), mode).as_deref(),
            Some("\x1b[<0;10;5m")
        );
        assert_eq!(
            enc(report(Button::Right, Action::Press, 2, 3), mode).as_deref(),
            Some("\x1b[<2;3;4M")
        );
        assert_eq!(
            enc(report(Button::Middle, Action::Release, 2, 3), mode).as_deref(),
            Some("\x1b[<1;3;4m")
        );
        // SGR has no 223 limit.
        assert_eq!(
            enc(report(Button::Left, Action::Press, 300, 250), mode).as_deref(),
            Some("\x1b[<0;301;251M")
        );
    }

    #[test]
    fn sgr_wheel_and_modifiers() {
        let mode = sgr(CLICK);
        assert_eq!(
            enc(report(Button::WheelUp, Action::Press, 4, 1), mode).as_deref(),
            Some("\x1b[<64;5;2M")
        );
        assert_eq!(
            enc(report(Button::WheelDown, Action::Press, 4, 1), mode).as_deref(),
            Some("\x1b[<65;5;2M")
        );
        assert_eq!(
            enc(report(Button::WheelUp, Action::Release, 4, 1), mode),
            None
        );
        let mut r = report(Button::Left, Action::Press, 0, 0);
        r.mods = Mods {
            shift: true,
            alt: true,
            ctrl: true,
        };
        assert_eq!(enc(r, mode).as_deref(), Some("\x1b[<28;1;1M"));
        r.mods = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(enc(r, mode).as_deref(), Some("\x1b[<16;1;1M"));
        r.mods = Mods {
            alt: true,
            ..Default::default()
        };
        r.button = Button::WheelDown;
        assert_eq!(enc(r, mode).as_deref(), Some("\x1b[<73;1;1M"));
    }

    #[test]
    fn drag_and_motion_depend_on_mode() {
        let drag = report(Button::Left, Action::Motion, 5, 6);
        let motion = report(Button::None, Action::Motion, 5, 6);
        // Clicks only: neither drags nor motion.
        assert_eq!(enc(drag, sgr(CLICK)), None);
        assert_eq!(enc(motion, sgr(CLICK)), None);
        // 1002: drags (button + 32), but no motion without a button.
        let mode = sgr(CLICK | TermMode::MOUSE_DRAG);
        assert_eq!(enc(drag, mode).as_deref(), Some("\x1b[<32;6;7M"));
        assert_eq!(enc(motion, mode), None);
        // 1003: all motion (no button = 3 + 32).
        let mode = sgr(TermMode::MOUSE_MOTION);
        assert_eq!(enc(drag, mode).as_deref(), Some("\x1b[<32;6;7M"));
        assert_eq!(enc(motion, mode).as_deref(), Some("\x1b[<35;6;7M"));
        let right_drag = report(Button::Right, Action::Motion, 0, 0);
        assert_eq!(enc(right_drag, mode).as_deref(), Some("\x1b[<34;1;1M"));
    }

    #[test]
    fn legacy_encoding() {
        let mode = CLICK | TermMode::MOUSE_DRAG;
        // ESC [ M, button + 32, column + 33, row + 33.
        assert_eq!(
            encode(&report(Button::Left, Action::Press, 0, 0), mode),
            Some(vec![0x1b, b'[', b'M', 32, 33, 33])
        );
        // On release, the button is 3.
        assert_eq!(
            encode(&report(Button::Right, Action::Release, 9, 4), mode),
            Some(vec![0x1b, b'[', b'M', 32 + 3, 33 + 9, 33 + 4])
        );
        assert_eq!(
            encode(&report(Button::Middle, Action::Motion, 1, 2), mode),
            Some(vec![0x1b, b'[', b'M', 32 + 1 + 32, 34, 35])
        );
        assert_eq!(
            encode(&report(Button::WheelUp, Action::Press, 0, 0), mode),
            Some(vec![0x1b, b'[', b'M', 32 + 64, 33, 33])
        );
        let mut r = report(Button::Left, Action::Release, 0, 0);
        r.mods = Mods {
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            encode(&r, mode),
            Some(vec![0x1b, b'[', b'M', 32 + 3 + 16, 33, 33])
        );
    }

    #[test]
    fn legacy_coordinates_beyond_223_are_skipped() {
        let mode = CLICK;
        // Column 223 (from 1) is the last one that fits: byte 255.
        assert_eq!(
            encode(&report(Button::Left, Action::Press, 222, 0), mode),
            Some(vec![0x1b, b'[', b'M', 32, 255, 33])
        );
        assert_eq!(
            encode(&report(Button::Left, Action::Press, 223, 0), mode),
            None
        );
        assert_eq!(
            encode(&report(Button::Left, Action::Press, 0, 300), mode),
            None
        );
    }

    #[test]
    fn utf8_encoding() {
        let mode = CLICK | TermMode::UTF8_MOUSE;
        // Below 95 it is the same as the classic one.
        assert_eq!(
            encode(&report(Button::Left, Action::Press, 10, 94), mode),
            Some(vec![0x1b, b'[', b'M', 32, 43, 127])
        );
        // From 95 on, the coordinate is a UTF-8 character (U+0080 onwards).
        let bytes = encode(&report(Button::Left, Action::Press, 300, 95), mode).unwrap();
        let expected = format!("\x1b[M {}{}", char::from_u32(333).unwrap(), '\u{80}');
        assert_eq!(bytes, expected.into_bytes());
        assert_eq!(
            encode(&report(Button::Left, Action::Press, 2015, 0), mode),
            None
        );
    }
}
