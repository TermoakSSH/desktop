//! Hides obvious secrets in terminal text before the desktop sends it to an
//! AI (the quick assistant, the copilot, the AI on this computer): what a
//! command printed, the screen, a selection. Each secret becomes
//! [`REDACTED`]; the text around it stays, so the AI still understands it.
//!
//! What is hidden:
//! - private key blocks (`-----BEGIN … PRIVATE KEY-----` to its `END`);
//! - the value of anything named like a secret: `password=…`, `token: …`,
//!   `"api_key": "…"`, `export AWS_SECRET_ACCESS_KEY=…`, `--password …`;
//! - `Authorization: Bearer …` / `Basic …` and any `Bearer <token>`;
//! - passwords in URLs (`https://user:pass@host`);
//! - well-known token shapes: AWS access keys (`AKIA…`, `ASIA…`), GitHub
//!   (`ghp_…`, `github_pat_…`), GitLab (`glpat-…`), Slack (`xox?-…`), API
//!   keys starting with `sk-` / `sk_live_`, Google (`AIza…`) and JWTs.
//!
//! Passwords typed at a password prompt never get here: they are not
//! echoed, so they are neither in the output nor in the typed line the
//! terminal keeps (it only trusts what it sees on screen).

/// What a secret is replaced with.
pub const REDACTED: &str = "[redacted]";

/// The text with the obvious secrets replaced by [`REDACTED`].
pub fn redact(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut in_key = false;
    for line in text.split('\n') {
        if in_key {
            if line.contains("-----END") {
                in_key = false;
            }
            continue;
        }
        if let Some(start) = line.find("-----BEGIN")
            && line[start..].contains("PRIVATE KEY")
        {
            lines.push(format!("{}{REDACTED}", &line[..start]));
            // A block on one line (`BEGIN … END` together).
            let rest = &line[start + "-----BEGIN".len()..];
            in_key = !rest.contains("-----END");
            continue;
        }
        lines.push(redact_line(line));
    }
    lines.join("\n")
}

/// Names of things whose value is a secret (lowercase, without `_-.`).
const SECRET_NAMES: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "accesskey",
    "privatekey",
    "authorization",
    "credential",
];

/// Whole parts of a name (split at `_-.`) that are secrets by themselves.
const SECRET_PARTS: &[&str] = &["pwd", "pass", "auth", "pat"];

/// Is this name (`DB_PASSWORD`, `apiKey`, `--token`...) a secret's?
fn secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let joined: String = lower
        .chars()
        .filter(|c| !matches!(c, '_' | '-' | '.'))
        .collect();
    if SECRET_NAMES.iter().any(|s| joined.contains(s)) {
        return true;
    }
    lower
        .split(['_', '-', '.'])
        .any(|part| SECRET_PARTS.contains(&part))
}

fn name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')
}

fn token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

/// End of an unquoted value starting at `from`.
fn value_end(bytes: &[u8], from: usize) -> usize {
    let mut i = from;
    while i < bytes.len()
        && !bytes[i].is_ascii_whitespace()
        && !matches!(
            bytes[i],
            b'&' | b',' | b';' | b'"' | b'\'' | b')' | b'}' | b']'
        )
    {
        i += 1;
    }
    i
}

/// Ranges (start, end) of secrets in one line, in order and not overlapping.
fn secret_ranges(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let push = |ranges: &mut Vec<(usize, usize)>, s: usize, e: usize| {
        if e > s && !ranges.iter().any(|&(a, b)| s < b && a < e) {
            ranges.push((s, e));
        }
    };

    // `name=value`, `name: value`, `"name": "value"`, `--name value`.
    let mut i = 0;
    while i < bytes.len() {
        if !name_char(bytes[i]) || (i > 0 && name_char(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && name_char(bytes[i]) {
            i += 1;
        }
        let name = &line[start..i];
        if !secret_name(name.trim_start_matches('-')) {
            continue;
        }
        let mut j = i;
        // A quoted name (JSON, YAML).
        if j < bytes.len() && matches!(bytes[j], b'"' | b'\'') {
            j += 1;
        }
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        let flag = name.starts_with("--");
        let sep = match bytes.get(j) {
            Some(b'=') => {
                j += 1;
                // `=>` (Ruby, PHP).
                if bytes.get(j) == Some(&b'>') {
                    j += 1;
                }
                true
            }
            Some(b':') => {
                j += 1;
                // Not a URL (`token://`) nor `::`.
                !matches!(bytes.get(j), Some(b'/') | Some(b':'))
            }
            // `--password secret`.
            Some(_) if flag && j > i => true,
            _ => false,
        };
        if !sep {
            continue;
        }
        while j < bytes.len() && bytes[j] == b' ' {
            j += 1;
        }
        let Some(&first) = bytes.get(j) else {
            continue;
        };
        if matches!(first, b'"' | b'\'') {
            let close = line[j + 1..].find(first as char).map(|n| j + 1 + n);
            let end = close.unwrap_or(bytes.len());
            push(&mut ranges, j + 1, end);
            i = end;
            continue;
        }
        let end = value_end(bytes, j);
        let value = &line[j..end];
        // `Authorization: Bearer <token>`: the token is the secret.
        if matches!(
            value.to_ascii_lowercase().as_str(),
            "bearer" | "basic" | "token" | "digest"
        ) {
            let mut k = end;
            while k < bytes.len() && bytes[k] == b' ' {
                k += 1;
            }
            let tend = value_end(bytes, k);
            push(&mut ranges, k, tend);
            i = tend;
            continue;
        }
        push(&mut ranges, j, end);
        i = end;
    }

    // `Bearer <token>` anywhere.
    let lower = line.to_ascii_lowercase();
    let mut from = 0;
    while let Some(n) = lower[from..].find("bearer ") {
        let at = from + n;
        from = at + 7;
        if at > 0 && name_char(bytes[at - 1]) {
            continue;
        }
        let mut k = at + 7;
        while k < bytes.len() && bytes[k] == b' ' {
            k += 1;
        }
        let mut end = k;
        while end < bytes.len() && token_char(bytes[end]) {
            end += 1;
        }
        if end - k >= 8 {
            push(&mut ranges, k, end);
        }
    }

    // Passwords in URLs: `scheme://user:password@host`.
    let mut from = 0;
    while let Some(n) = line[from..].find("://") {
        let auth_start = from + n + 3;
        from = auth_start;
        let mut end = auth_start;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() && bytes[end] != b'/' {
            if bytes[end] == b'@' {
                break;
            }
            end += 1;
        }
        if bytes.get(end) != Some(&b'@') {
            continue;
        }
        if let Some(colon) = line[auth_start..end].find(':') {
            push(&mut ranges, auth_start + colon + 1, end);
        }
    }

    // Known token shapes, word by word.
    let mut i = 0;
    while i < bytes.len() {
        if !token_char(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && token_char(bytes[i]) {
            i += 1;
        }
        let word = line[start..i].trim_end_matches(['.', '=']);
        if known_token(word) {
            push(&mut ranges, start, start + word.len());
        }
    }

    ranges.sort_unstable();
    ranges
}

/// A word with the shape of a well-known secret.
fn known_token(word: &str) -> bool {
    let len = word.len();
    let alnum = |s: &str| s.bytes().all(|c| c.is_ascii_alphanumeric());
    let upper_alnum = |s: &str| {
        s.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };
    if (word.starts_with("AKIA") || word.starts_with("ASIA")) && len == 20 && upper_alnum(word) {
        return true;
    }
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_"] {
        if let Some(rest) = word.strip_prefix(prefix)
            && rest.len() >= 20
        {
            return true;
        }
    }
    for prefix in ["glpat-", "xoxb-", "xoxp-", "xoxa-", "xoxs-", "xoxr-"] {
        if let Some(rest) = word.strip_prefix(prefix)
            && rest.len() >= 10
        {
            return true;
        }
    }
    for prefix in ["sk-", "sk_live_", "rk_live_", "sk_test_"] {
        if let Some(rest) = word.strip_prefix(prefix)
            && rest.len() >= 20
            && rest.bytes().any(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    if let Some(rest) = word.strip_prefix("AIza")
        && rest.len() >= 30
    {
        return true;
    }
    // JWT: `eyJ<header>.eyJ<payload>.<signature>`.
    if word.starts_with("eyJ") && len >= 30 {
        let parts: Vec<&str> = word.split('.').collect();
        if parts.len() == 3
            && parts[1].starts_with("eyJ")
            && parts
                .iter()
                .all(|p| !p.is_empty() && alnum(&p.replace(['-', '_'], "")))
        {
            return true;
        }
    }
    false
}

fn redact_line(line: &str) -> String {
    let ranges = secret_ranges(line);
    if ranges.is_empty() {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut last = 0;
    for (s, e) in ranges {
        if s < last {
            continue;
        }
        out.push_str(&line[last..s]);
        out.push_str(REDACTED);
        last = e;
    }
    out.push_str(&line[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_value_secrets() {
        assert_eq!(redact("password=hunter2"), "password=[redacted]");
        assert_eq!(
            redact("mysql://x?user=ana&password=s3cr3t&db=app"),
            "mysql://x?user=ana&password=[redacted]&db=app"
        );
        assert_eq!(
            redact("export AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG"),
            "export AWS_SECRET_ACCESS_KEY=[redacted]"
        );
        assert_eq!(
            redact("DB_PASSWORD: \"a b c\""),
            "DB_PASSWORD: \"[redacted]\""
        );
        assert_eq!(
            redact(r#"{"api_key": "abc123", "name": "web"}"#),
            r#"{"api_key": "[redacted]", "name": "web"}"#
        );
        assert_eq!(redact("token=abc; path=/"), "token=[redacted]; path=/");
        assert_eq!(redact("GITHUB_TOKEN=xyz"), "GITHUB_TOKEN=[redacted]");
        assert_eq!(redact("clientSecret: 9f8e7d"), "clientSecret: [redacted]");
        assert_eq!(redact("MYSQL_PWD=pw"), "MYSQL_PWD=[redacted]");
        assert_eq!(
            redact("app --password hunter2 --verbose"),
            "app --password [redacted] --verbose"
        );
        assert_eq!(redact("app --token=abc"), "app --token=[redacted]");
    }

    #[test]
    fn authorization_headers() {
        assert_eq!(
            redact("Authorization: Bearer eyJhbGciOi.abc.def"),
            "Authorization: Bearer [redacted]"
        );
        assert_eq!(
            redact("curl -H 'Authorization: Basic YWxhZGRpbjpvcGVu' https://x"),
            "curl -H 'Authorization: Basic [redacted]' https://x"
        );
        assert_eq!(redact("> bearer 0123456789abcdef"), "> bearer [redacted]");
    }

    #[test]
    fn known_token_shapes() {
        assert_eq!(
            redact("key AKIAIOSFODNN7EXAMPLE in use"),
            "key [redacted] in use"
        );
        assert_eq!(
            redact("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
            "[redacted]"
        );
        assert_eq!(
            redact("OPENAI sk-proj-abc123def456ghi789jkl012"),
            "OPENAI [redacted]"
        );
        assert_eq!(
            redact("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dozjgNryP4J3jVmNHl0w5N"),
            "jwt [redacted]"
        );
        // Ordinary words are left alone.
        assert_eq!(redact("AKIA is a prefix"), "AKIA is a prefix");
        assert_eq!(redact("sk-learn installed"), "sk-learn installed");
    }

    #[test]
    fn urls_with_passwords() {
        assert_eq!(
            redact("git clone https://ana:hunter2@git.example.com/repo.git"),
            "git clone https://ana:[redacted]@git.example.com/repo.git"
        );
        assert_eq!(
            redact("postgres://app:pw@db:5432/app"),
            "postgres://app:[redacted]@db:5432/app"
        );
        // A user without a password, and plain URLs, stay.
        assert_eq!(
            redact("ssh://ana@host and https://example.com/a:b@c"),
            "ssh://ana@host and https://example.com/a:b@c"
        );
    }

    #[test]
    fn private_key_blocks() {
        let text = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\nAAAA\n-----END OPENSSH PRIVATE KEY-----\nafter";
        assert_eq!(redact(text), "before\n[redacted]\nafter");
        // Cut at the end (a screen tail): hidden to the end.
        assert_eq!(
            redact("x\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\nAAAA"),
            "x\n[redacted]"
        );
        // Public keys and certificates are not secrets.
        let public = "-----BEGIN PUBLIC KEY-----\nMFkw\n-----END PUBLIC KEY-----";
        assert_eq!(redact(public), public);
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
            assert_eq!(redact(text), text, "{text}");
        }
    }
}
