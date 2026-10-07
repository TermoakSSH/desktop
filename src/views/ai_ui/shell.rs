//! A small shell tokenizer for highlighting the commands the AI shows:
//! commands, keywords, flags, strings, variables, comments, operators and
//! numbers. It does not parse the shell (no nesting beyond `$(`), it only
//! colors what a reader expects.

use std::ops::Range;

/// What a piece of a command is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Command,
    Keyword,
    Flag,
    String,
    Variable,
    Comment,
    Operator,
    Number,
}

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "time", "!", "{", "}", "[[", "]]",
];

/// Words after which the next word is a command again.
const PREFIXES: &[&str] = &[
    "sudo", "doas", "env", "nohup", "exec", "xargs", "watch", "timeout", "nice", "command",
    "builtin", "time", "strace", "then", "else", "do", "if", "elif", "while", "until", "!", "{",
];

fn is_operator_char(c: char) -> bool {
    matches!(c, '|' | '&' | ';' | '<' | '>' | '(' | ')')
}

fn is_word_end(c: char) -> bool {
    c.is_whitespace() || is_operator_char(c) || c == '"' || c == '\'' || c == '`'
}

/// The highlighted ranges (byte offsets) of `code`; what is not covered
/// is plain text (arguments, whitespace).
pub fn tokenize(code: &str) -> Vec<(Range<usize>, Tok)> {
    let mut out: Vec<(Range<usize>, Tok)> = Vec::new();
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let end_of = |i: usize| chars.get(i).map(|(b, _)| *b).unwrap_or(code.len());
    let mut i = 0;
    // The next word is a command (start of a line, after `|`, `&&`, `;`...).
    let mut expect_command = true;
    while i < chars.len() {
        let (start, c) = chars[i];
        let at_word_start = i == 0 || {
            let p = chars[i - 1].1;
            p.is_whitespace() || is_operator_char(p)
        };
        match c {
            '\n' => {
                // A line continued with `\` keeps the state.
                let continued = i > 0 && chars[i - 1].1 == '\\';
                if !continued {
                    expect_command = true;
                }
                i += 1;
            }
            c if c.is_whitespace() => i += 1,
            '#' if at_word_start => {
                let mut j = i;
                while j < chars.len() && chars[j].1 != '\n' {
                    j += 1;
                }
                out.push((start..end_of(j), Tok::Comment));
                i = j;
            }
            '\'' => {
                let mut j = i + 1;
                while j < chars.len() && chars[j].1 != '\'' {
                    j += 1;
                }
                j = (j + 1).min(chars.len());
                out.push((start..end_of(j), Tok::String));
                expect_command = false;
                i = j;
            }
            '"' | '`' => {
                let quote = c;
                let mut j = i + 1;
                while j < chars.len() && chars[j].1 != quote {
                    if chars[j].1 == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                j = (j + 1).min(chars.len());
                out.push((start..end_of(j), Tok::String));
                expect_command = false;
                i = j;
            }
            '$' => {
                let next = chars.get(i + 1).map(|(_, c)| *c);
                match next {
                    Some('(') => {
                        // `$(`: a command inside.
                        out.push((start..end_of(i + 2), Tok::Operator));
                        expect_command = true;
                        i += 2;
                    }
                    Some('{') => {
                        let mut j = i + 2;
                        while j < chars.len() && chars[j].1 != '}' {
                            j += 1;
                        }
                        j = (j + 1).min(chars.len());
                        out.push((start..end_of(j), Tok::Variable));
                        i = j;
                    }
                    Some(n) if n.is_ascii_alphanumeric() || n == '_' => {
                        let mut j = i + 1;
                        if n.is_ascii_digit() {
                            j += 1;
                        } else {
                            while j < chars.len()
                                && (chars[j].1.is_ascii_alphanumeric() || chars[j].1 == '_')
                            {
                                j += 1;
                            }
                        }
                        out.push((start..end_of(j), Tok::Variable));
                        i = j;
                    }
                    Some('?' | '!' | '#' | '@' | '*' | '$' | '-') => {
                        out.push((start..end_of(i + 2), Tok::Variable));
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            c if is_operator_char(c) => {
                let mut j = i + 1;
                while j < chars.len() && is_operator_char(chars[j].1) && chars[j].1 != '(' {
                    j += 1;
                }
                // `2>` / `2>&1`: the descriptor goes with the redirection.
                let mut from = start;
                if let Some((last, Tok::Number)) = out.last()
                    && last.end == start
                    && matches!(c, '<' | '>')
                {
                    from = last.start;
                    out.pop();
                }
                let op = &code[start..end_of(j)];
                if op.ends_with('&') && chars.get(j).is_some_and(|(_, d)| d.is_ascii_digit()) {
                    j += 1;
                }
                out.push((from..end_of(j), Tok::Operator));
                // After a redirection comes a file name, not a command.
                expect_command = !op.contains(['<', '>']);
                i = j;
            }
            _ => {
                // A word.
                let mut j = i;
                while j < chars.len() && !is_word_end(chars[j].1) {
                    if chars[j].1 == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
                let j = j.min(chars.len());
                let word = &code[start..end_of(j)];
                let range = start..end_of(j);
                if expect_command {
                    if let Some(eq) = assignment(word) {
                        // `VAR=value cmd`: still a command after it.
                        out.push((start..start + eq + 1, Tok::Variable));
                    } else if KEYWORDS.contains(&word) {
                        out.push((range, Tok::Keyword));
                        expect_command = PREFIXES.contains(&word);
                    } else {
                        out.push((range, Tok::Command));
                        expect_command = PREFIXES.contains(&word);
                    }
                } else if word.starts_with('-') && word.len() > 1 {
                    out.push((range, Tok::Flag));
                } else if word.chars().all(|c| c.is_ascii_digit() || c == '.')
                    && word.chars().any(|c| c.is_ascii_digit())
                {
                    out.push((range, Tok::Number));
                } else if word == "\\" {
                    // A line continuation.
                } else if KEYWORDS.contains(&word) && matches!(word, "do" | "then" | "in") {
                    out.push((range, Tok::Keyword));
                    expect_command = word != "in";
                }
                i = j;
            }
        }
    }
    out
}

/// A console session (`$ command` lines and their output): only the
/// commands after a `$ ` or `# ` prompt are highlighted, the prompt as an
/// operator; the output stays plain.
pub fn tokenize_console(code: &str) -> Vec<(Range<usize>, Tok)> {
    let mut out = Vec::new();
    let mut offset = 0;
    for line in code.split_inclusive('\n') {
        if line.starts_with("$ ") || line.starts_with("# ") {
            out.push((offset..offset + 1, Tok::Operator));
            let command = &line[2..];
            out.extend(
                tokenize(command)
                    .into_iter()
                    .map(|(r, t)| (r.start + offset + 2..r.end + offset + 2, t)),
            );
        }
        offset += line.len();
    }
    out
}

/// `NAME=` at the start of a word: the length of `NAME`.
fn assignment(word: &str) -> Option<usize> {
    let eq = word.find('=')?;
    let name = &word[..eq];
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(eq)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens as (text, kind) for readable asserts.
    fn toks(code: &str) -> Vec<(&str, Tok)> {
        tokenize(code)
            .into_iter()
            .map(|(r, t)| (&code[r], t))
            .collect()
    }

    #[test]
    fn commands_flags_and_operators() {
        assert_eq!(
            toks("sudo systemctl restart nginx && journalctl -u nginx -n 50 | tail"),
            vec![
                ("sudo", Tok::Command),
                ("systemctl", Tok::Command),
                ("&&", Tok::Operator),
                ("journalctl", Tok::Command),
                ("-u", Tok::Flag),
                ("-n", Tok::Flag),
                ("50", Tok::Number),
                ("|", Tok::Operator),
                ("tail", Tok::Command),
            ]
        );
    }

    #[test]
    fn strings_variables_and_comments() {
        assert_eq!(
            toks("echo \"disk: $USER\" '$x' $HOME ${PATH} $? # done"),
            vec![
                ("echo", Tok::Command),
                ("\"disk: $USER\"", Tok::String),
                ("'$x'", Tok::String),
                ("$HOME", Tok::Variable),
                ("${PATH}", Tok::Variable),
                ("$?", Tok::Variable),
                ("# done", Tok::Comment),
            ]
        );
        // `#` inside a word is not a comment.
        assert_eq!(toks("echo a#b"), vec![("echo", Tok::Command)]);
        // An unclosed string runs to the end.
        assert_eq!(
            toks("echo \"abc"),
            vec![("echo", Tok::Command), ("\"abc", Tok::String)]
        );
    }

    #[test]
    fn keywords_assignments_and_substitutions() {
        assert_eq!(
            toks("for f in *.log; do gzip \"$f\"; done"),
            vec![
                ("for", Tok::Keyword),
                ("in", Tok::Keyword),
                (";", Tok::Operator),
                ("do", Tok::Keyword),
                ("gzip", Tok::Command),
                ("\"$f\"", Tok::String),
                (";", Tok::Operator),
                ("done", Tok::Keyword),
            ]
        );
        assert_eq!(
            toks("LANG=C ls $(pwd)"),
            vec![
                ("LANG=", Tok::Variable),
                ("ls", Tok::Command),
                ("$(", Tok::Operator),
                ("pwd", Tok::Command),
                (")", Tok::Operator),
            ]
        );
    }

    #[test]
    fn redirections_and_lines() {
        assert_eq!(
            toks("make 2>&1 > out.txt\ncat out.txt"),
            vec![
                ("make", Tok::Command),
                ("2>&1", Tok::Operator),
                (">", Tok::Operator),
                ("cat", Tok::Command),
            ]
        );
        // A continued line keeps its arguments.
        assert_eq!(
            toks("apt install \\\n  -y curl"),
            vec![("apt", Tok::Command), ("-y", Tok::Flag)]
        );
        // Ranges stay on character boundaries with non-ASCII text.
        for (r, _) in tokenize("echo 'ñandú' # café ☕ | x") {
            assert!(r.start <= r.end);
        }
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn console_sessions_highlight_only_commands() {
        let code = "$ df -h /\n/dev/sda1  40G  58% /\n# apt update";
        let toks: Vec<(&str, Tok)> = tokenize_console(code)
            .into_iter()
            .map(|(r, t)| (&code[r], t))
            .collect();
        assert_eq!(
            toks,
            vec![
                ("$", Tok::Operator),
                ("df", Tok::Command),
                ("-h", Tok::Flag),
                ("#", Tok::Operator),
                ("apt", Tok::Command),
            ]
        );
    }
}
