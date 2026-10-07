//! PuTTY sessions: a `.reg` export of
//! `HKEY_CURRENT_USER\Software\SimonTatham\PuTTY\Sessions` (any OS) or, on
//! Windows, that registry key itself.
//!
//! Each session is a subkey whose name is %-escaped (`My%20Server`). Values
//! used: `HostName` (may be `user@host`), `PortNumber`, `UserName`,
//! `Protocol` (`ssh`, and `telnet` as a Telnet host; others are skipped),
//! `PublicKeyFile` (SSH only; the `.ppk` path:
//! imported if the file can be read here and is not encrypted, otherwise
//! written in the notes), `ProxyMethod` (1 SOCKS4, 2 SOCKS5, 3 HTTP;
//! Telnet and local-command proxies are not supported), `ProxyHost`,
//! `ProxyPort`, `ProxyUsername` and `ProxyPassword`. `Default Settings` is
//! not a host. Tunnels (`PortForwardings`) are not imported.

use std::collections::BTreeMap;

use termoak_core::model::{ProxyKind, ProxySettings};

use super::text::percent_decode;
use super::{ImportHost, ImportSet};

/// A registry value of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegValue {
    Str(String),
    Dword(u32),
}

impl RegValue {
    fn text(&self) -> String {
        match self {
            RegValue::Str(s) => s.clone(),
            RegValue::Dword(n) => n.to_string(),
        }
    }

    fn number(&self) -> Option<u32> {
        match self {
            RegValue::Dword(n) => Some(*n),
            RegValue::Str(s) => s.trim().parse().ok(),
        }
    }
}

/// A PuTTY session: its (decoded) name and its values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub values: BTreeMap<String, RegValue>,
}

/// Registry path of the sessions.
#[cfg(windows)]
const SESSIONS_KEY: &str = r"Software\SimonTatham\PuTTY\Sessions";

/// Sessions of a `.reg` file.
pub fn parse_reg(text: &str) -> Vec<Session> {
    let mut out: Vec<Session> = Vec::new();
    let mut current: Option<Session> = None;
    // Values may continue on the next line (`\` at the end).
    let mut logical: Vec<String> = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_suffix('\\')
            && !line.trim_start().starts_with('[')
            && !line.ends_with("\\\\\"")
            && (line.contains("=hex") || !pending.is_empty())
        {
            pending.push_str(rest.trim_start());
            continue;
        }
        pending.push_str(if pending.is_empty() {
            line
        } else {
            line.trim_start()
        });
        logical.push(std::mem::take(&mut pending));
    }
    for line in logical {
        let line = line.trim();
        if let Some(path) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            if let Some(s) = current.take() {
                out.push(s);
            }
            if path.starts_with('-') {
                continue;
            }
            let lower = path.to_ascii_lowercase();
            let marker = r"\simontatham\putty\sessions\";
            if let Some(ix) = lower.find(marker) {
                let name = &path[ix + marker.len()..];
                if !name.is_empty() && !name.contains('\\') {
                    current = Some(Session {
                        name: percent_decode(name),
                        values: BTreeMap::new(),
                    });
                }
            }
            continue;
        }
        let Some(session) = current.as_mut() else {
            continue;
        };
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((name, value)) = split_name(rest) else {
            continue;
        };
        let value = value.trim();
        if let Some(hex) = value.strip_prefix("dword:") {
            if let Ok(n) = u32::from_str_radix(hex.trim(), 16) {
                session.values.insert(name, RegValue::Dword(n));
            }
        } else if let Some(s) = value.strip_prefix('"') {
            session.values.insert(name, RegValue::Str(unescape(s)));
        }
    }
    if let Some(s) = current.take() {
        out.push(s);
    }
    out
}

/// `Name"=value` → (`Name`, `value`), honoring `\"` in the name.
fn split_name(rest: &str) -> Option<(String, &str)> {
    let mut name = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                if let Some((_, n)) = chars.next() {
                    name.push(n);
                }
            }
            '"' => {
                let tail = rest[i + 1..].trim_start();
                return tail.strip_prefix('=').map(|v| (name, v));
            }
            c => name.push(c),
        }
    }
    None
}

/// Body of a `"…"` value (without the opening quote): `\\` and `\"`.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '"' => break,
            c => out.push(c),
        }
    }
    out
}

/// The hosts of some sessions.
pub fn to_set(sessions: &[Session]) -> ImportSet {
    let mut set = ImportSet::default();
    for s in sessions {
        if s.name.eq_ignore_ascii_case("Default Settings") {
            continue;
        }
        let get = |k: &str| s.values.get(k);
        let text = |k: &str| get(k).map(RegValue::text).unwrap_or_default();
        let protocol_text = text("Protocol");
        let Some(protocol) = super::protocol_of(&protocol_text) else {
            let protocol = protocol_text;
            set.warnings.push(
                t!(
                    "import_export.warn.not_ssh",
                    name = s.name.clone(),
                    protocol = protocol
                )
                .to_string(),
            );
            continue;
        };
        let address = text("HostName");
        if address.trim().is_empty() {
            // A session that only keeps settings (no host).
            continue;
        }
        let mut notes = Vec::new();
        let proxy = match get("ProxyMethod").and_then(RegValue::number).unwrap_or(0) {
            0 => None,
            m @ 1..=3 => {
                let host = text("ProxyHost");
                let port = get("ProxyPort")
                    .and_then(RegValue::number)
                    .and_then(|p| u16::try_from(p).ok())
                    .unwrap_or(if m == 3 { 8080 } else { 1080 });
                Some(ProxySettings {
                    kind: match m {
                        1 => ProxyKind::Socks4,
                        2 => ProxyKind::Socks5,
                        _ => ProxyKind::Http,
                    },
                    host,
                    port,
                    username: Some(text("ProxyUsername")).filter(|u| !u.is_empty()),
                })
                .filter(|p| !p.host.is_empty())
            }
            _ => {
                set.warnings.push(
                    t!(
                        "import_export.warn.proxy_unsupported",
                        name = s.name.clone()
                    )
                    .to_string(),
                );
                None
            }
        };
        if !text("PortForwardings").is_empty() {
            notes.push(format!(
                "PuTTY PortForwardings: {}",
                text("PortForwardings")
            ));
        }
        let key_file = Some(text("PublicKeyFile")).filter(|k| !k.trim().is_empty());
        set.push_host(ImportHost {
            label: s.name.clone(),
            address,
            port: get("PortNumber")
                .and_then(RegValue::number)
                .and_then(|p| u16::try_from(p).ok()),
            username: Some(text("UserName")),
            proxy_password: proxy
                .as_ref()
                .and_then(|_| Some(text("ProxyPassword")).filter(|p| !p.is_empty())),
            proxy,
            key_file: key_file.filter(|_| protocol.is_ssh()),
            notes: notes.join("\n"),
            protocol,
            ..Default::default()
        });
    }
    set
}

/// Sessions saved in this computer's registry (Windows).
#[cfg(windows)]
pub fn read_registry() -> std::io::Result<Vec<Session>> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, REG_DWORD};

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let sessions = match hkcu.open_subkey_with_flags(SESSIONS_KEY, KEY_READ) {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for name in sessions.enum_keys().flatten() {
        let Ok(key) = sessions.open_subkey_with_flags(&name, KEY_READ) else {
            continue;
        };
        let mut values = BTreeMap::new();
        for (value_name, value) in key.enum_values().flatten() {
            if value.vtype == REG_DWORD {
                if let Ok(n) = key.get_value::<u32, _>(&value_name) {
                    values.insert(value_name, RegValue::Dword(n));
                }
            } else if let Ok(s) = key.get_value::<String, _>(&value_name) {
                values.insert(value_name, RegValue::Str(s));
            }
        }
        out.push(Session {
            name: percent_decode(&name),
            values,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// As `regedit /e` writes it (here already decoded from UTF-16).
    const SAMPLE: &str = "Windows Registry Editor Version 5.00\r\n\
\r\n\
[HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions]\r\n\
\r\n\
[HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions\\Default%20Settings]\r\n\
\"HostName\"=\"\"\r\n\
\"PortNumber\"=dword:00000016\r\n\
\r\n\
[HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions\\Web%20prod]\r\n\
\"Present\"=dword:00000001\r\n\
\"HostName\"=\"deploy@web.example.com\"\r\n\
\"PortNumber\"=dword:000008ae\r\n\
\"Protocol\"=\"ssh\"\r\n\
\"UserName\"=\"\"\r\n\
\"PublicKeyFile\"=\"C:\\\\Users\\\\ana\\\\keys\\\\deploy.ppk\"\r\n\
\"ProxyMethod\"=dword:00000002\r\n\
\"ProxyHost\"=\"socks.example.com\"\r\n\
\"ProxyPort\"=dword:00000438\r\n\
\"ProxyUsername\"=\"ana\"\r\n\
\"ProxyPassword\"=\"s3cr\\\"et\"\r\n\
\"PortForwardings\"=\"L8080=localhost:80\"\r\n\
\"Colour0\"=\"187,187,187\"\r\n\
\"Font\"=hex(2):43,00,6f,00,6e,00,73,00,6f,00,6c,00,61,00,73,00,00,\\\r\n\
  00\r\n\
\r\n\
[HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions\\router]\r\n\
\"HostName\"=\"192.168.1.1\"\r\n\
\"Protocol\"=\"telnet\"\r\n\
\"PortNumber\"=dword:00000017\r\n\
\r\n\
[HKEY_CURRENT_USER\\Software\\SimonTatham\\PuTTY\\Sessions\\db]\r\n\
\"HostName\"=\"10.0.0.21\"\r\n\
\"PortNumber\"=dword:00000016\r\n\
\"UserName\"=\"postgres\"\r\n\
\"ProxyMethod\"=dword:00000005\r\n";

    #[test]
    fn reads_reg_exports() {
        let sessions = parse_reg(SAMPLE);
        assert_eq!(sessions.len(), 4);
        assert_eq!(sessions[1].name, "Web prod");
        assert_eq!(
            sessions[1].values.get("PortNumber"),
            Some(&RegValue::Dword(2222))
        );
        let set = to_set(&sessions);
        assert_eq!(set.hosts.len(), 3);
        let web = &set.hosts[0];
        assert_eq!(web.label, "Web prod");
        assert_eq!(web.address, "web.example.com");
        assert_eq!(web.username.as_deref(), Some("deploy"));
        assert_eq!(web.port, Some(2222));
        assert_eq!(
            web.key_file.as_deref(),
            Some(r"C:\Users\ana\keys\deploy.ppk")
        );
        let proxy = web.proxy.as_ref().unwrap();
        assert_eq!(proxy.kind, ProxyKind::Socks5);
        assert_eq!(proxy.port, 1080);
        assert_eq!(proxy.username.as_deref(), Some("ana"));
        assert_eq!(web.proxy_password.as_deref(), Some("s3cr\"et"));
        assert!(web.notes.contains("L8080=localhost:80"));
        // The telnet session is a Telnet host.
        let router = &set.hosts[1];
        assert_eq!(router.label, "router");
        assert!(router.protocol.is_telnet());
        assert_eq!(router.port, Some(23));
        assert!(web.protocol.is_ssh());
        let db = &set.hosts[2];
        assert_eq!(db.username.as_deref(), Some("postgres"));
        assert!(db.proxy.is_none());
        // The local-command proxy.
        assert_eq!(set.warnings.len(), 1, "{:?}", set.warnings);
    }
}
