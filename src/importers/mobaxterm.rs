//! MobaXterm bookmarks: `MobaXterm.ini` (sections `[Bookmarks]`,
//! `[Bookmarks_1]`…) and `.mxtsessions` exports (the same sections).
//!
//! Each section has `SubRep=` (its folder, `Parent\Child`; empty for the
//! root), `ImgNum=` and one line per session:
//! `Name=#<icon>#<type>%<host>%<port>%<user>%…#<terminal settings>#…`.
//! Assumptions about the `%` fields (MobaXterm does not document them):
//! type `0` is SSH (`7` SFTP is imported as an SSH host too), `1` is
//! Telnet (a Telnet host), the rest are skipped; fields 1–3 are host, port and user; field 8–10 an SSH
//! gateway (host, port, user), written in the notes; the private key path
//! is the first field after the 10th that looks like a key file
//! (`_ProfileDir_` is the user's home). Saved passwords are encrypted by
//! MobaXterm and are not imported.

use super::text::parse_ini;
use super::{ImportHost, ImportSet};
use termoak_core::model::HostProtocol;

/// What a MobaXterm session type becomes (`None`: skipped).
fn protocol_of_type(t: &str) -> Option<HostProtocol> {
    match t.trim() {
        "0" | "7" => Some(HostProtocol::Ssh),
        "1" => Some(HostProtocol::Telnet),
        _ => None,
    }
}

/// Name of a MobaXterm session type (for the warnings).
fn type_name(t: &str) -> String {
    match t.trim() {
        "1" => "Telnet".into(),
        "2" => "Rlogin".into(),
        "3" => "XDMCP".into(),
        "4" => "RDP".into(),
        "5" => "VNC".into(),
        "6" => "FTP".into(),
        "8" => "Serial".into(),
        "9" => "File".into(),
        "10" => "Shell".into(),
        "11" => "Browser".into(),
        "12" => "Mosh".into(),
        "13" => "AWS S3".into(),
        "14" => "WSL".into(),
        other => format!("#{other}"),
    }
}

fn looks_like_key(f: &str) -> bool {
    let l = f.to_ascii_lowercase();
    !l.is_empty()
        && (l.ends_with(".ppk")
            || l.ends_with(".pem")
            || l.ends_with(".key")
            || l.contains("id_rsa")
            || l.contains("id_ed25519")
            || l.contains("id_ecdsa")
            || l.contains("_profiledir_"))
        && (l.contains('\\') || l.contains('/') || l.contains('.'))
}

/// Private key path as this computer can read it (`_ProfileDir_` → `~`).
fn key_path(f: &str) -> String {
    let f = f.trim();
    match f.strip_prefix("_ProfileDir_") {
        Some(rest) => format!("~{}", rest.replace('\\', "/")),
        None => f.to_string(),
    }
}

/// Hosts of a `MobaXterm.ini` or `.mxtsessions`.
pub fn parse(text: &str) -> ImportSet {
    let mut set = ImportSet::default();
    for section in parse_ini(text) {
        if !section.name.to_ascii_lowercase().starts_with("bookmarks") {
            continue;
        }
        let folder = section.get("SubRep").unwrap_or("").to_string();
        let group = set.group_path(&folder);
        for (name, value) in &section.entries {
            if name.eq_ignore_ascii_case("SubRep") || name.eq_ignore_ascii_case("ImgNum") {
                continue;
            }
            let Some(body) = value.strip_prefix('#') else {
                continue;
            };
            // `<icon>#<params>#…`
            let mut parts = body.split('#');
            let _icon = parts.next();
            let Some(params) = parts.next() else { continue };
            let f: Vec<&str> = params.split('%').collect();
            let kind = f.first().copied().unwrap_or("");
            let Some(protocol) = protocol_of_type(kind) else {
                set.warnings.push(
                    t!(
                        "import_export.warn.not_ssh",
                        name = name.clone(),
                        protocol = type_name(kind)
                    )
                    .to_string(),
                );
                continue;
            };
            let field = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
            let mut notes = Vec::new();
            let gateway = field(8);
            if protocol.is_ssh() && !gateway.is_empty() && gateway != "-1" && gateway != "0" {
                let mut g = String::new();
                if !field(10).is_empty() {
                    g.push_str(field(10));
                    g.push('@');
                }
                g.push_str(gateway);
                if !field(9).is_empty() && field(9) != "22" {
                    g.push(':');
                    g.push_str(field(9));
                }
                notes.push(t!("import_export.note.gateway", gateway = g).to_string());
            }
            let key_file = f
                .iter()
                .skip(11)
                .find(|v| looks_like_key(v))
                .map(|v| key_path(v))
                .filter(|_| protocol.is_ssh());
            let empty_group = group.is_none() && folder.trim().is_empty();
            set.push_host(ImportHost {
                label: name.trim().to_string(),
                address: field(1).to_string(),
                port: field(2).parse().ok(),
                username: Some(field(3).to_string()),
                group: if empty_group { None } else { group.clone() },
                notes: notes.join("\n"),
                key_file,
                protocol,
                ..Default::default()
            });
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "[Misc]\r\n\
PasswordsInRegistry=0\r\n\
\r\n\
[Bookmarks]\r\n\
SubRep=\r\n\
ImgNum=42\r\n\
Home server=#109#0%192.168.1.10%22%ana%%-1%-1%%%22%%0%0%0%%%-1%0%0%0%%1080%%0%0%1#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,180%0%-1%0%%xterm%-1%-1%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1#0# #-1\r\n\
\r\n\
[Bookmarks_1]\r\n\
SubRep=Production\\Web\r\n\
ImgNum=41\r\n\
web-1=#109#0%10.0.0.11%2222%deploy%%-1%-1%%bastion.example.com%22%jump%0%0%0%_ProfileDir_\\.ssh\\id_ed25519%%-1%0%0%0%%1080%%0%0%1#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,180%0%-1%0%%xterm%-1%-1%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1#0# #-1\r\n\
files=#140#7%10.0.0.12%22%deploy%%-1%-1%%%%%0%0%0%C:\\keys\\deploy.ppk%%-1%0%0%0%%1080%%0%0%1#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,180%0%-1%0%%xterm%-1%-1%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1#0# #-1\r\n\
router=#98#1%192.168.1.1%23%admin%%2%%%%%0%0%%1080%#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,180%0%-1%0%%xterm%-1%-1%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1#0# #-1\r\n\
desktop=#91#4%10.0.0.50%3389%%-1%-1%0%0%-1#MobaFont%10%0%0%-1%15%236,236,236%30,30,30%180,180,180%0%-1%0%%xterm%-1%-1%_Std_Colors_0_%80%24%0%1%-1%<none>%%0%0%-1#0# #-1\r\n";

    #[test]
    fn reads_bookmarks() {
        let set = parse(SAMPLE);
        assert_eq!(set.hosts.len(), 4, "{:?}", set.warnings);
        let home = &set.hosts[0];
        assert_eq!(home.label, "Home server");
        assert_eq!(home.target(), "ana@192.168.1.10");
        assert_eq!(home.group, None);
        assert!(home.notes.is_empty());
        let web = &set.hosts[1];
        assert_eq!(web.port, Some(2222));
        assert_eq!(
            set.group_label(web.group.as_deref().unwrap()),
            "Production / Web"
        );
        assert_eq!(web.key_file.as_deref(), Some("~/.ssh/id_ed25519"));
        assert!(
            web.notes.contains("jump@bastion.example.com"),
            "{}",
            web.notes
        );
        let files = &set.hosts[2];
        assert_eq!(files.key_file.as_deref(), Some("C:\\keys\\deploy.ppk"));
        // Telnet sessions are Telnet hosts.
        let router = &set.hosts[3];
        assert!(router.protocol.is_telnet() && home.protocol.is_ssh());
        assert_eq!(router.target(), "telnet://admin@192.168.1.1");
        assert!(router.notes.is_empty() && router.key_file.is_none());
        assert_eq!(set.warnings.len(), 1);
        assert!(set.warnings[0].contains("RDP"));
    }
}
