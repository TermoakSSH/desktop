//! OSC 7: the shell reports its working directory
//! (`ESC ] 7 ; file://host/path BEL` or `ESC \`), as `vte.sh`, fish, zsh
//! setups and others do. It is read from the raw output, next to the
//! emulator (which ignores it), to know where files dropped on a terminal
//! go. Without it the server's home folder is used.

/// Longest OSC kept (longer ones are not a path).
const MAX: usize = 4096;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

/// Finds OSC 7 in the output, across chunks.
#[derive(Debug, Default)]
pub struct Scanner {
    state: State,
    buf: Vec<u8>,
    cwd: Option<String>,
}

impl Scanner {
    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state = match self.state {
                State::Ground | State::Esc if b == 0x1b => State::Esc,
                State::Ground => State::Ground,
                State::Esc if b == b']' => {
                    self.buf.clear();
                    State::Osc
                }
                State::Esc => State::Ground,
                State::Osc => match b {
                    0x07 => {
                        self.finish();
                        State::Ground
                    }
                    0x1b => State::OscEsc,
                    0x18 | 0x1a => State::Ground,
                    _ if self.buf.len() >= MAX => State::Ground,
                    _ => {
                        self.buf.push(b);
                        State::Osc
                    }
                },
                State::OscEsc if b == b'\\' => {
                    self.finish();
                    State::Ground
                }
                State::OscEsc if b == b']' => {
                    self.buf.clear();
                    State::Osc
                }
                State::OscEsc => State::Ground,
            };
        }
    }

    fn finish(&mut self) {
        if let Some(rest) = self.buf.strip_prefix(b"7;")
            && let Ok(text) = std::str::from_utf8(rest)
            && let Some(path) = parse_url(text)
        {
            self.cwd = Some(path);
        }
        self.buf.clear();
    }

    /// The last directory reported.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }
}

/// Path of a `file://host/path` (or kitty's `kitty-shell-cwd://`) URL.
pub fn parse_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("file://")
        .or_else(|| url.strip_prefix("kitty-shell-cwd://"))?;
    let path = &rest[rest.find('/')?..];
    let path = crate::importers::text::percent_decode(path);
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_directory_across_chunks() {
        let mut s = Scanner::default();
        s.feed(b"prompt\x1b]0;title\x07more\x1b]7;file://web-1/home/ana/My%20");
        assert_eq!(s.cwd(), None);
        s.feed(b"docs\x1b\\$ ");
        assert_eq!(s.cwd(), Some("/home/ana/My docs"));
        s.feed(b"\x1b]7;kitty-shell-cwd://host/srv/app\x07");
        assert_eq!(s.cwd(), Some("/srv/app"));
        // Other sequences do not change it.
        s.feed(b"\x1b[31mred\x1b]8;;http://x\x07link\x1b]8;;\x07");
        assert_eq!(s.cwd(), Some("/srv/app"));
        assert_eq!(parse_url("file:///tmp"), Some("/tmp".into()));
        assert_eq!(parse_url("http://x/y"), None);
    }
}
