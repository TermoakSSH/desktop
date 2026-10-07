//! Hides obvious secrets in terminal text before the desktop sends it to an
//! AI (the quick assistant, the copilot, the AI on this computer): what a
//! command printed, the screen, a selection. Each secret becomes
//! `[redacted]`; the text around it stays, so the AI still understands it.
//!
//! It is the AI engine's own redaction (`termoak_ai::redact`, the one the
//! server and the local engine apply to tool outputs), so the desktop hides
//! the same things: private keys, secret-looking keys and their values,
//! authorization headers and cookies, passwords in URLs and well-known
//! token formats. On top of it, a `Bearer <token>` outside a header. The
//! tests below are the desktop's cases, kept to check that the engine
//! still covers them.
//!
//! Passwords typed at a password prompt never get here: they are not
//! echoed, so they are neither in the output nor in the typed line the
//! terminal keeps (it only trusts what it sees on screen).

/// What a secret is replaced with (the engine's marker).
const REDACTED: &str = termoak_ai::redact::REDACTED;

/// The text with the obvious secrets replaced by `[redacted]`: the
/// engine's redaction, plus a `Bearer <token>` outside an `Authorization`
/// header (a token a command printed), which the engine leaves.
pub fn redact(text: &str) -> String {
    bare_bearer(&termoak_ai::redact::redact(text))
}

/// Hides the token after a `bearer ` word (any case) of 8 or more token
/// characters.
fn bare_bearer(text: &str) -> String {
    const WORD: &str = "bearer ";
    // ASCII lowercase keeps the byte offsets.
    let lower = text.to_ascii_lowercase();
    let token_char = |c: u8| c.is_ascii_alphanumeric() || b"-._~+/=".contains(&c);
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut from = 0;
    while let Some(pos) = lower[from..].find(WORD) {
        let start = from + pos + WORD.len();
        let at_word = from + pos == 0 || !lower.as_bytes()[from + pos - 1].is_ascii_alphanumeric();
        let len = text.as_bytes()[start..]
            .iter()
            .take_while(|c| token_char(**c))
            .count();
        if at_word && len >= 8 {
            out.push_str(&text[last..start]);
            out.push_str(REDACTED);
            last = start + len;
        }
        from = start + len;
        if from >= text.len() {
            break;
        }
    }
    out.push_str(&text[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The secret is gone and the text says something was hidden.
    #[track_caller]
    fn hidden(text: &str, secret: &str) {
        let out = redact(text);
        assert!(!out.contains(secret), "{secret} still in {out:?}");
        assert!(out.contains("[redacted]"), "nothing hidden in {out:?}");
    }

    /// Nothing is hidden.
    #[track_caller]
    fn kept(text: &str) {
        assert_eq!(redact(text), text);
    }

    #[test]
    fn key_value_secrets() {
        assert_eq!(redact("password=hunter2"), "password=[redacted]");
        hidden("mysql://x?user=ana&password=s3cr3t&db=app", "s3cr3t");
        assert!(redact("mysql://x?user=ana&password=s3cr3t&db=app").ends_with("&db=app"));
        hidden(
            "export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG",
            "wJalrXUtnFEMI",
        );
        hidden("DB_PASSWORD: \"a b c\"", "a b c");
        hidden(r#"{"api_key": "abc123", "name": "web"}"#, "abc123");
        assert!(redact(r#"{"api_key": "abc123", "name": "web"}"#).contains(r#""name": "web""#));
        hidden("token=abc; path=/", "abc");
        hidden("GITHUB_TOKEN=xyz", "xyz");
        hidden("clientSecret: 9f8e7d", "9f8e7d");
        hidden("MYSQL_PWD=pw", "=pw");
        hidden("app --password hunter2 --verbose", "hunter2");
        assert!(redact("app --password hunter2 --verbose").ends_with(" --verbose"));
        hidden("app --token=abc", "abc");
    }

    #[test]
    fn authorization_headers() {
        hidden("Authorization: Bearer eyJhbGciOi.abc.def", "eyJhbGciOi");
        hidden(
            "curl -H 'Authorization: Basic YWxhZGRpbjpvcGVu' https://x",
            "YWxhZGRpbjpvcGVu",
        );
        hidden("> bearer 0123456789abcdef", "0123456789abcdef");
        assert_eq!(
            redact("got Bearer abcdefgh12345 back"),
            "got Bearer [redacted] back"
        );
        // Short words after "bearer" are not tokens.
        kept("the bearer of bad news");
    }

    #[test]
    fn known_token_shapes() {
        hidden("key AKIAIOSFODNN7EXAMPLE in use", "AKIAIOSFODNN7EXAMPLE");
        hidden("ghp_abcdefghijklmnopqrstuvwxyz0123456789", "ghp_abc");
        hidden("OPENAI sk-proj-abc123def456ghi789jkl012", "sk-proj");
        hidden(
            "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dozjgNryP4J3jVmNHl0w5N",
            "eyJhbGciOiJIUzI1NiJ9",
        );
        // Ordinary words are left alone.
        kept("AKIA is a prefix");
        kept("sk-learn installed");
    }

    #[test]
    fn urls_with_passwords() {
        assert_eq!(
            redact("git clone https://ana:hunter2@git.example.com/repo.git"),
            "git clone https://ana:[redacted]@git.example.com/repo.git"
        );
        hidden("postgres://app:pw@db:5432/app", ":pw@");
        // A user without a password, and plain URLs, stay.
        kept("ssh://ana@host and https://example.com/a:b@c");
    }

    #[test]
    fn private_key_blocks() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        hidden(text, "b3BlbnNzaC1rZXk");
        assert!(redact(text).starts_with("before\n") && redact(text).ends_with("\nafter"));
        // Cut at the end (a screen tail): hidden to the end.
        hidden("x\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\nAAAA", "MIIE");
        assert!(!redact("x\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\nAAAA").contains("AAAA"));
        // Public keys and certificates are not secrets.
        kept("-----BEGIN PUBLIC KEY-----\nMFkw\n-----END PUBLIC KEY-----");
    }

    #[test]
    fn ordinary_output_is_untouched() {
        for text in [
            "Permission denied (publickey,password).",
            "[sudo] password for ana: ",
            "Password: ",
            "bash: foo: command not found",
            "Enter passphrase for key '/home/ana/.ssh/id_ed25519':",
            "Author: Ana <ana@example.com>",
            "ls -la /var/log | sort -k5 -n",
            "ñandú: café ☕ = 3",
        ] {
            kept(text);
        }
    }
}
