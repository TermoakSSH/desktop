//! The AI's Markdown cut into prose and fenced code blocks: the prose goes
//! to the Markdown view (headings, lists, inline code, links, tables) and
//! each code block gets its own card with syntax highlighting and actions
//! (Copy, Insert into terminal, Run).

/// A piece of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// Markdown without fenced code blocks.
    Text(String),
    /// A fenced code block (```` ```lang ```` … ```` ``` ````).
    Code {
        /// The info string's first word, in lowercase (`bash`, `yaml`...).
        lang: Option<String>,
        code: String,
        /// The closing fence was seen (while streaming it may not be yet).
        closed: bool,
    },
}

/// An opening fence: its indentation, character, length and language.
fn opening_fence(line: &str) -> Option<(usize, char, usize, Option<String>)> {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let len = rest.chars().take_while(|c| *c == ch).count();
    if len < 3 {
        return None;
    }
    let info = rest[len..].trim();
    // A backtick fence cannot have backticks in its info string (that is
    // inline code such as ```` ```x``` ````).
    if ch == '`' && info.contains('`') {
        return None;
    }
    let lang = info
        .split(|c: char| c.is_whitespace() || c == '{' || c == ',')
        .next()
        .filter(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('.').to_lowercase());
    Some((indent, ch, len, lang))
}

/// Does this line close a fence of `ch` × `len`?
fn closing_fence(line: &str, ch: char, len: usize) -> bool {
    let rest = line.trim();
    rest.chars().all(|c| c == ch) && rest.chars().count() >= len
}

/// Removes up to `indent` leading spaces (the fence's own indentation, e.g.
/// inside a list item).
fn dedent(line: &str, indent: usize) -> &str {
    let strip = line
        .bytes()
        .take(indent)
        .take_while(|b| *b == b' ' || *b == b'\t')
        .count();
    &line[strip..]
}

/// Cuts `text` into prose and fenced code blocks, in order. Prose blocks
/// are trimmed of blank lines around them and empty ones are left out; an
/// unclosed fence (a message still streaming) runs to the end.
pub fn split_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut prose: Vec<&str> = Vec::new();
    let mut lines = text.lines();
    let flush = |prose: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        let joined = prose.join("\n");
        let trimmed = joined.trim_matches('\n').trim_end();
        if !trimmed.trim().is_empty() {
            blocks.push(Block::Text(trimmed.to_string()));
        }
        prose.clear();
    };
    while let Some(line) = lines.next() {
        let Some((indent, ch, len, lang)) = opening_fence(line) else {
            prose.push(line);
            continue;
        };
        flush(&mut prose, &mut blocks);
        let mut code: Vec<&str> = Vec::new();
        let mut closed = false;
        for inner in lines.by_ref() {
            if closing_fence(inner, ch, len) {
                closed = true;
                break;
            }
            code.push(dedent(inner, indent));
        }
        blocks.push(Block::Code {
            lang,
            code: code.join("\n"),
            closed,
        });
    }
    flush(&mut prose, &mut blocks);
    blocks
}

/// Languages that are commands for a shell.
pub fn is_shell(lang: Option<&str>) -> bool {
    matches!(
        lang,
        None | Some(
            "sh" | "bash"
                | "shell"
                | "zsh"
                | "fish"
                | "ksh"
                | "console"
                | "shell-session"
                | "shellsession"
                | "terminal"
                | "cmd"
                | "powershell"
                | "ps"
                | "ps1"
                | "pwsh"
        )
    )
}

/// The command of a code block, to insert into a terminal or run: for a
/// shell block, its text (in a `console` transcript, only the lines after a
/// `$ ` prompt, without it); `None` for other languages (a config file, a
/// Python script...) or an empty block.
pub fn block_command(lang: Option<&str>, code: &str) -> Option<String> {
    if !is_shell(lang) {
        return None;
    }
    // A session transcript: `$ ` (and `# ` for root) prompts.
    let console = matches!(
        lang,
        Some("console" | "shell-session" | "shellsession" | "terminal")
    );
    let text = if console || code.lines().any(|l| l.starts_with("$ ")) {
        code.lines()
            .filter_map(|l| {
                l.strip_prefix("$ ")
                    .or_else(|| l.strip_prefix("# ").filter(|_| console))
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        // Comment lines are explanations, not commands (joined into one
        // line they would comment out everything after them).
        code.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// A label for the language of a block (`bash`, `yaml`, or `text`).
pub fn lang_label(lang: Option<&str>) -> String {
    match lang {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => "text".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(lang: Option<&str>, code: &str, closed: bool) -> Block {
        Block::Code {
            lang: lang.map(str::to_string),
            code: code.to_string(),
            closed,
        }
    }

    #[test]
    fn prose_and_code_in_order() {
        let text = "Restart it:\n\n```bash\nsudo systemctl restart nginx\nsystemctl status nginx\n```\n\nThen check **the logs**.";
        assert_eq!(
            split_blocks(text),
            vec![
                Block::Text("Restart it:".into()),
                code(
                    Some("bash"),
                    "sudo systemctl restart nginx\nsystemctl status nginx",
                    true
                ),
                Block::Text("Then check **the logs**.".into()),
            ]
        );
    }

    #[test]
    fn only_prose_stays_one_block() {
        let text = "# Title\n\n- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |";
        assert_eq!(split_blocks(text), vec![Block::Text(text.into())]);
        assert!(split_blocks("").is_empty());
        assert!(split_blocks("\n\n  \n").is_empty());
    }

    #[test]
    fn streaming_fence_is_open() {
        assert_eq!(
            split_blocks("Run:\n```sh\nls -la"),
            vec![
                Block::Text("Run:".into()),
                code(Some("sh"), "ls -la", false)
            ]
        );
        // Just the opening fence.
        assert_eq!(split_blocks("```"), vec![code(None, "", false)]);
    }

    #[test]
    fn fences_in_lists_tildes_and_info_strings() {
        let text =
            "1. Edit:\n   ```yaml title=\"x\"\n   key: value\n     nested: 1\n   ```\n2. Done";
        assert_eq!(
            split_blocks(text),
            vec![
                Block::Text("1. Edit:".into()),
                code(Some("yaml"), "key: value\n  nested: 1", true),
                Block::Text("2. Done".into()),
            ]
        );
        // A longer fence holds a shorter one; tildes work too.
        let nested = "````md\n```bash\nls\n```\n````";
        assert_eq!(
            split_blocks(nested),
            vec![code(Some("md"), "```bash\nls\n```", true)]
        );
        assert_eq!(
            split_blocks("~~~Python\nprint(1)\n~~~"),
            vec![code(Some("python"), "print(1)", true)]
        );
        // Inline code with three backticks is not a fence.
        assert_eq!(
            split_blocks("use ```x``` here"),
            vec![Block::Text("use ```x``` here".into())]
        );
    }

    #[test]
    fn commands_of_blocks() {
        assert_eq!(
            block_command(Some("bash"), "  df -h\n"),
            Some("df -h".into())
        );
        assert_eq!(block_command(None, "uptime"), Some("uptime".into()));
        assert_eq!(
            block_command(
                Some("bash"),
                "# keep it small\nsudo journalctl --vacuum-size=200M\n  # done"
            ),
            Some("sudo journalctl --vacuum-size=200M".into())
        );
        assert_eq!(block_command(Some("sh"), "# only a comment"), None);
        assert_eq!(block_command(Some("yaml"), "a: 1"), None);
        assert_eq!(block_command(Some("bash"), "   "), None);
        // A transcript: the commands, without the prompt or the output.
        assert_eq!(
            block_command(
                Some("console"),
                "$ uname -a\nLinux web-1 6.8\n$ whoami\nroot"
            ),
            Some("uname -a\nwhoami".into())
        );
        assert_eq!(
            block_command(Some("sh"), "$ free -m\n              total"),
            Some("free -m".into())
        );
        // Outside a console block, `# ` is a comment.
        assert_eq!(
            block_command(Some("bash"), "$ ls\n# a comment"),
            Some("ls".into())
        );
        assert_eq!(
            block_command(Some("console"), "# apt update"),
            Some("apt update".into())
        );
        assert_eq!(lang_label(None), "text");
        assert_eq!(lang_label(Some("bash")), "bash");
    }
}
