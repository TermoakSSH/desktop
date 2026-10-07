//! Knowing when a command typed in a terminal ends, to tell about the long
//! ones that end out of sight (see `TerminalEvent::CommandFinished`).
//!
//! When the shell has shell integration it marks its prompt and every
//! command with OSC 133 (FinalTerm, also iTerm2, WezTerm, kitty, Ghostty and
//! the shells' own scripts) or OSC 633 (VS Code, which can also send the
//! command line): `A` prompt, `B` typing, `C` the command runs, `D;<exit>`
//! it ended. With those marks the start, the end and the exit status are
//! exact ([`MarkScanner`] finds them in the output; the emulator ignores
//! them).
//!
//! Without them, a heuristic: the command starts when Enter is pressed
//! (outside full-screen programs) and ends when the output has been quiet
//! for a moment ([`IDLE`]) with a prompt at the cursor: the same text that
//! was in front of the typed line, or something ending like a prompt
//! (`$ `, `# `, `% `, `> `, `❯ `...). Programs that use the alternate screen
//! (vim, less, top) are interactive: they are never notified.

use std::time::{Duration, Instant};

/// Quiet output needed before a prompt counts as the end of a command (no
/// shell integration).
pub const IDLE: Duration = Duration::from_millis(1000);
/// Longest OSC kept while looking for marks (a 633;E command line).
const MAX_OSC: usize = 4096;

/// A shell-integration mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mark {
    /// `A`: the prompt starts.
    PromptStart,
    /// `B`: the prompt ended, the user types the command.
    InputStart,
    /// `C`: the command runs (its output starts).
    Executed,
    /// `D[;exit]`: the command ended, with its exit status if given.
    Finished(Option<i32>),
    /// `633;E;<command line>`: the command line, as the shell read it.
    CommandLine(String),
}

/// Where the scanner is in the byte stream (sequences can be split between
/// two pieces of output).
#[derive(Debug, Default)]
enum Scan {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// Inside an OSC that may be a mark (its first bytes).
    Osc(Vec<u8>),
    /// Inside an OSC, after ESC (maybe the `ESC \` terminator).
    OscEsc(Vec<u8>),
    /// Inside an OSC that is not a mark (or too long): until its end.
    Skip,
    SkipEsc,
}

/// Finds the shell-integration marks in the output.
#[derive(Debug, Default)]
pub struct MarkScanner {
    state: Scan,
}

impl MarkScanner {
    /// The marks in the next piece of output, in order.
    #[cfg(test)]
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Mark> {
        self.scan_at(bytes).into_iter().map(|(_, m)| m).collect()
    }

    /// The marks in the next piece of output, in order, each with the
    /// offset in `bytes` right after it (where the command's output starts
    /// after `C`, or where it ended at `D`).
    pub fn scan_at(&mut self, bytes: &[u8]) -> Vec<(usize, Mark)> {
        let mut marks = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            self.state = match std::mem::take(&mut self.state) {
                Scan::Ground => {
                    // Fast path: straight to the next ESC.
                    match bytes[i..].iter().position(|&c| c == 0x1b) {
                        Some(n) => {
                            i += n + 1;
                            self.state = Scan::Esc;
                            continue;
                        }
                        None => return marks,
                    }
                }
                Scan::Esc if b == b']' => Scan::Osc(Vec::new()),
                Scan::Esc if b == 0x1b => Scan::Esc,
                Scan::Esc => Scan::Ground,
                Scan::Osc(buf) | Scan::OscEsc(buf) if b == 0x07 => {
                    marks.extend(parse(&buf).map(|m| (i + 1, m)));
                    Scan::Ground
                }
                Scan::OscEsc(buf) if b == b'\\' => {
                    marks.extend(parse(&buf).map(|m| (i + 1, m)));
                    Scan::Ground
                }
                // Another sequence starts: this OSC was cut.
                Scan::OscEsc(_) | Scan::SkipEsc if b == b']' => Scan::Osc(Vec::new()),
                Scan::OscEsc(_) | Scan::SkipEsc => Scan::Ground,
                Scan::Osc(buf) if b == 0x1b => Scan::OscEsc(buf),
                Scan::Osc(_) | Scan::Skip if b == 0x18 || b == 0x1a => Scan::Ground,
                Scan::Osc(mut buf) => {
                    buf.push(b);
                    let maybe_mark = if buf.len() <= 4 {
                        b"133;"[..buf.len()] == buf[..] || b"633;"[..buf.len()] == buf[..]
                    } else {
                        buf.len() <= MAX_OSC
                    };
                    if maybe_mark {
                        Scan::Osc(buf)
                    } else {
                        Scan::Skip
                    }
                }
                Scan::Skip if b == 0x07 => Scan::Ground,
                Scan::Skip if b == 0x1b => Scan::SkipEsc,
                Scan::Skip => Scan::Skip,
            };
            i += 1;
        }
        marks
    }
}

/// The mark of an OSC payload (`133;D;0`, `633;E;ls -la`...), if it is one.
fn parse(payload: &[u8]) -> Option<Mark> {
    let text = String::from_utf8_lossy(payload);
    let rest = text
        .strip_prefix("133;")
        .or_else(|| text.strip_prefix("633;"))?;
    let mut parts = rest.splitn(2, ';');
    let kind = parts.next()?;
    let args = parts.next().unwrap_or("");
    Some(match kind {
        "A" => Mark::PromptStart,
        "B" => Mark::InputStart,
        "C" => Mark::Executed,
        "D" => Mark::Finished(args.split(';').next().and_then(|c| c.trim().parse().ok())),
        "E" if text.starts_with("633;") => {
            // `E;<line>[;<nonce>]`, with `\\`, `\x3b` (`;`) and other bytes escaped.
            let line = args.split(';').next().unwrap_or("");
            let line = unescape_633(line);
            if line.trim().is_empty() {
                return None;
            }
            Mark::CommandLine(line)
        }
        _ => return None,
    })
}

/// Undoes the escaping of the command line of `633;E` (`\\` and `\xAB`).
fn unescape_633(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if bytes.get(i + 1) == Some(&b'\\') {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if bytes.get(i + 1) == Some(&b'x')
                && let Some(hex) = text.get(i + 2..i + 4)
                && let Ok(v) = u8::from_str_radix(hex, 16)
            {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Text in front of the cursor that looks like a shell prompt: `text` is
/// what is before the cursor on its line and `after_blank` says nothing is
/// written after it. `known` is the prompt seen when the command was typed.
pub fn looks_like_prompt(text: &str, after_blank: bool, known: Option<&str>) -> bool {
    if !after_blank || text.trim().is_empty() {
        return false;
    }
    if known.is_some_and(|k| !k.trim().is_empty() && k == text) {
        return true;
    }
    let trimmed = text.trim_end();
    let Some(last) = trimmed.chars().next_back() else {
        return false;
    };
    let spaced = text.len() > trimmed.len();
    const ENDINGS: &[char] = &['$', '#', '%', '>', '❯', '➜', 'λ', '»', '›', '→', '♪'];
    if spaced {
        return ENDINGS.contains(&last);
    }
    // Windows cmd.exe: `C:\Users\ana>` without a space.
    let mut chars = trimmed.chars();
    last == '>'
        && chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.next() == Some(':')
        && chars.next() == Some('\\')
}

/// A command that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The command line, when known (typed with Enter and seen on screen,
    /// or sent by the shell).
    pub command: Option<String>,
    pub duration: Duration,
    /// Exit status (only with shell integration).
    pub exit: Option<i32>,
    /// It used the alternate screen (vim, less, top...).
    pub interactive: bool,
}

impl Finished {
    /// Long enough, and not an interactive program.
    pub fn worth_notifying(&self, threshold: Duration) -> bool {
        !self.interactive && self.duration >= threshold
    }
}

/// A command running.
#[derive(Debug, Clone)]
struct Run {
    started: Instant,
    last_output: Instant,
    command: Option<String>,
    /// Prompt in front of the typed line (heuristic).
    prompt: Option<String>,
    interactive: bool,
    /// Started by the shell's `C` mark.
    marked: bool,
}

impl Run {
    fn finish(self, end: Instant, exit: Option<i32>) -> Finished {
        Finished {
            command: self.command,
            duration: end.saturating_duration_since(self.started),
            exit,
            interactive: self.interactive,
        }
    }
}

/// Follows the commands of one terminal (see the module docs).
#[derive(Debug, Default)]
pub struct CommandWatch {
    /// The shell sends marks: they decide, Enter and prompts do not.
    integrated: bool,
    /// Command line for the next `C` mark.
    next_command: Option<String>,
    run: Option<Run>,
}

impl CommandWatch {
    /// The user pressed Enter at a shell line (not in a full-screen
    /// program): `command` is the line if it is known and `prompt` what was
    /// in front of it. Returns whether a new command started (without
    /// shell integration; with it, the `C` mark starts it).
    pub fn enter(&mut self, now: Instant, command: Option<String>, prompt: Option<String>) -> bool {
        self.next_command = command.clone();
        if self.integrated {
            return false;
        }
        // Enter while a program is busy printing is input for it, not a
        // new command; after a quiet moment it is a new one (the previous
        // one ended at a prompt that did not look like one, or the program
        // was waiting for an answer).
        if self
            .run
            .as_ref()
            .is_some_and(|r| now.saturating_duration_since(r.last_output) < IDLE)
        {
            return false;
        }
        self.run = Some(Run {
            started: now,
            last_output: now,
            command,
            prompt,
            interactive: false,
            marked: false,
        });
        true
    }

    /// Output arrived, with these marks in it; `alt_screen` is the screen in
    /// use after it. Returns the command that ended, if one did.
    pub fn output(&mut self, now: Instant, marks: &[Mark], alt_screen: bool) -> Option<Finished> {
        let mut done = None;
        for mark in marks {
            match mark {
                Mark::CommandLine(line) => self.next_command = Some(line.clone()),
                Mark::PromptStart | Mark::InputStart => {
                    self.integrated = true;
                    // A new prompt without `D`: the command ended anyway.
                    if let Some(run) = self.run.take()
                        && run.marked
                    {
                        done = Some(run.finish(now, None));
                    }
                }
                Mark::Executed => {
                    self.integrated = true;
                    self.run = Some(Run {
                        started: now,
                        last_output: now,
                        command: self.next_command.take(),
                        prompt: None,
                        interactive: false,
                        marked: true,
                    });
                }
                Mark::Finished(exit) => {
                    self.integrated = true;
                    if let Some(run) = self.run.take()
                        && run.marked
                    {
                        done = Some(run.finish(now, *exit));
                    }
                }
            }
        }
        if let Some(run) = self.run.as_mut() {
            run.last_output = now;
            run.interactive |= alt_screen;
        }
        done
    }

    /// Without shell integration, a command is waiting for its prompt (the
    /// terminal checks [`Self::idle`] every so often meanwhile).
    pub fn waiting_for_prompt(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.marked)
    }

    /// The periodic check without shell integration: `before` is the text
    /// in front of the cursor and `after_blank` whether its right is empty.
    /// After a quiet moment with a prompt there, the command ended when its
    /// last output (the prompt) arrived.
    pub fn idle(
        &mut self,
        now: Instant,
        alt_screen: bool,
        before: &str,
        after_blank: bool,
    ) -> Option<Finished> {
        let run = self.run.as_ref().filter(|r| !r.marked)?;
        if alt_screen || now.saturating_duration_since(run.last_output) < IDLE {
            return None;
        }
        if !looks_like_prompt(before, after_blank, run.prompt.as_deref()) {
            return None;
        }
        let run = self.run.take()?;
        let end = run.last_output;
        Some(run.finish(end, None))
    }

    /// Forgets everything (a new connection).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn marks_are_found_across_pieces() {
        let mut s = MarkScanner::default();
        assert_eq!(s.scan(b"hello \x1b]133;A\x07$ "), vec![Mark::PromptStart]);
        // Split in the middle of the sequence, ended with ESC \.
        assert!(s.scan(b"ls\r\n\x1b]13").is_empty());
        assert_eq!(s.scan(b"3;C\x1b\\out"), vec![Mark::Executed]);
        assert_eq!(
            s.scan(b"put\r\n\x1b]133;D;2\x07\x1b]133;A\x07"),
            vec![Mark::Finished(Some(2)), Mark::PromptStart]
        );
        assert!(s.scan(b"\x1b").is_empty());
        assert_eq!(s.scan(b"]133;D\x07"), vec![Mark::Finished(None)]);
        // VS Code's marks, with the command line.
        assert_eq!(
            s.scan(b"\x1b]633;E;echo a\\x3bb \\\\ c;nonce\x07\x1b]633;C\x07"),
            vec![Mark::CommandLine("echo a;b \\ c".into()), Mark::Executed]
        );
        // Other OSCs (title, a big clipboard copy) and CSI are not marks.
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', 10_000));
        big.extend(b"\x07\x1b]0;title\x07\x1b[1;31mred\x1b[0m");
        assert!(s.scan(&big).is_empty());
        // And the scanner is back at the start: the next mark is found.
        assert_eq!(s.scan(b"\x1b]133;B\x07"), vec![Mark::InputStart]);
        // A cut OSC followed by a new one.
        assert_eq!(s.scan(b"\x1b]133;\x1b]133;C\x07"), vec![Mark::Executed]);
        assert!(s.scan(b"\x1b]133;Z\x07\x1b]1337;x\x07").is_empty());
    }

    #[test]
    fn marks_know_where_they_end() {
        let mut s = MarkScanner::default();
        let out = b"ls\r\n\x1b]133;C\x07a.txt\r\n\x1b]133;D;0\x1b\\$ ";
        let marks = s.scan_at(out);
        assert_eq!(marks.len(), 2);
        assert_eq!(&out[marks[0].0..marks[0].0 + 5], b"a.txt");
        assert_eq!(marks[1].1, Mark::Finished(Some(0)));
        assert_eq!(&out[marks[1].0..], b"$ ");
        // Split across pieces: the offset is in the piece where it ends.
        assert!(s.scan_at(b"\x1b]133").is_empty());
        assert_eq!(s.scan_at(b";C\x07out"), vec![(3, Mark::Executed)]);
    }

    #[test]
    fn prompts() {
        assert!(looks_like_prompt("ana@web:~$ ", true, None));
        assert!(looks_like_prompt("root@db:/# ", true, None));
        assert!(looks_like_prompt("host% ", true, None));
        assert!(looks_like_prompt("~/src ❯ ", true, None));
        assert!(looks_like_prompt("PS C:\\Users\\ana> ", true, None));
        assert!(looks_like_prompt("C:\\Users\\ana>", true, None));
        // The prompt seen when the command was typed, whatever it ends with.
        assert!(looks_like_prompt("➜  src ", true, Some("➜  src ")));
        assert!(!looks_like_prompt("➜  src ", true, None));
        // Not prompts: output, questions, text at the right, nothing.
        assert!(!looks_like_prompt("Downloading 45%", true, None));
        assert!(!looks_like_prompt("Continue? [y/N] ", true, None));
        assert!(!looks_like_prompt("Password: ", true, None));
        assert!(!looks_like_prompt("=====>", true, None));
        assert!(!looks_like_prompt("ana@web:~$ ", false, None));
        assert!(!looks_like_prompt("", true, Some("")));
        assert!(!looks_like_prompt("   ", true, None));
    }

    #[test]
    fn heuristic_long_command() {
        let t0 = Instant::now();
        let mut w = CommandWatch::default();
        w.enter(t0, Some("make".into()), Some("ana@web:~$ ".into()));
        assert!(w.waiting_for_prompt());
        // Output keeps coming: not finished, even with a prompt-like line.
        assert_eq!(w.output(t0 + secs(5), &[], false), None);
        assert_eq!(w.idle(t0 + secs(5), false, "ana@web:~$ ", true), None);
        // Quiet, but no prompt yet (the command is just slow).
        assert_eq!(w.idle(t0 + secs(9), false, "", true), None);
        // The prompt comes back and the output goes quiet.
        w.output(t0 + secs(42), &[], false);
        assert_eq!(
            w.idle(t0 + secs(42) + IDLE / 2, false, "ana@web:~$ ", true),
            None
        );
        let done = w
            .idle(t0 + secs(43) + IDLE, false, "ana@web:~$ ", true)
            .unwrap();
        assert_eq!(done.command.as_deref(), Some("make"));
        // Measured up to the prompt, not to when it was noticed.
        assert_eq!(done.duration, secs(42));
        assert_eq!(done.exit, None);
        assert!(done.worth_notifying(secs(10)));
        assert!(!done.worth_notifying(secs(60)));
        assert!(!w.waiting_for_prompt());
        assert_eq!(w.idle(t0 + secs(50), false, "ana@web:~$ ", true), None);
    }

    #[test]
    fn heuristic_enter_while_busy_or_quiet() {
        let t0 = Instant::now();
        let mut w = CommandWatch::default();
        w.enter(t0, Some("apt upgrade".into()), None);
        w.output(t0 + secs(20), &[], false);
        // Enter while it prints: an answer for it, the command goes on.
        w.enter(t0 + secs(20), None, None);
        w.output(t0 + secs(30), &[], false);
        let done = w.idle(t0 + secs(32), false, "root@db:~# ", true).unwrap();
        assert_eq!(done.duration, secs(30));
        assert_eq!(done.command.as_deref(), Some("apt upgrade"));

        // A prompt that does not look like one: the next Enter after a
        // quiet moment starts a new command instead of adding up.
        w.enter(t0 + secs(100), Some("ls".into()), None);
        w.output(t0 + secs(100), &[], false);
        assert_eq!(w.idle(t0 + secs(105), false, "weird prompt", true), None);
        w.enter(t0 + secs(200), Some("sleep 15".into()), None);
        w.output(t0 + secs(215), &[], false);
        let done = w.idle(t0 + secs(217), false, "weird prompt", true);
        // Still not a prompt...
        assert_eq!(done, None);
        let done = w.idle(t0 + secs(217), false, "$ ", true).unwrap();
        assert_eq!(done.command.as_deref(), Some("sleep 15"));
        assert_eq!(done.duration, secs(15));
    }

    #[test]
    fn heuristic_full_screen_programs_are_interactive() {
        let t0 = Instant::now();
        let mut w = CommandWatch::default();
        w.enter(t0, Some("vim notes".into()), None);
        w.output(t0 + secs(1), &[], true);
        // While in vim nothing ends.
        assert_eq!(w.idle(t0 + secs(100), true, "~", true), None);
        w.output(t0 + secs(300), &[], false);
        let done = w.idle(t0 + secs(302), false, "$ ", true).unwrap();
        assert!(done.interactive);
        assert!(!done.worth_notifying(secs(10)));
    }

    #[test]
    fn shell_integration_decides() {
        let t0 = Instant::now();
        let mut w = CommandWatch::default();
        assert_eq!(
            w.output(t0, &[Mark::PromptStart, Mark::InputStart], false),
            None
        );
        // Enter does not start anything by itself...
        w.enter(t0 + secs(1), Some("cargo build".into()), None);
        assert!(!w.waiting_for_prompt());
        // ...the C mark does, with the typed line.
        assert_eq!(w.output(t0 + secs(1), &[Mark::Executed], false), None);
        assert_eq!(w.output(t0 + secs(30), &[], false), None);
        // A prompt-looking line is not the end with marks.
        assert_eq!(w.idle(t0 + secs(40), false, "$ ", true), None);
        let done = w
            .output(
                t0 + secs(61),
                &[Mark::Finished(Some(101)), Mark::PromptStart],
                false,
            )
            .unwrap();
        assert_eq!(done.command.as_deref(), Some("cargo build"));
        assert_eq!(done.duration, secs(60));
        assert_eq!(done.exit, Some(101));
        assert!(done.worth_notifying(secs(10)));

        // The shell's own command line wins over the typed one.
        w.enter(t0 + secs(70), None, None);
        w.output(
            t0 + secs(70),
            &[Mark::CommandLine("git pull".into()), Mark::Executed],
            false,
        );
        let done = w
            .output(t0 + secs(75), &[Mark::Finished(Some(0))], false)
            .unwrap();
        assert_eq!(done.command.as_deref(), Some("git pull"));
        assert!(!done.worth_notifying(secs(10)));

        // A prompt without D still ends the command; D without C ends nothing.
        w.output(t0 + secs(80), &[Mark::Executed], false);
        let done = w
            .output(t0 + secs(95), &[Mark::PromptStart], false)
            .unwrap();
        assert_eq!(done.exit, None);
        assert_eq!(done.duration, secs(15));
        assert_eq!(
            w.output(t0 + secs(96), &[Mark::Finished(Some(130))], false),
            None
        );

        w.reset();
        w.enter(t0 + secs(100), None, None);
        assert!(w.waiting_for_prompt());
    }
}
