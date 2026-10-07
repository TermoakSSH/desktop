//! SecureCRT sessions: the XML of "Export Settings" or the folder of
//! session files (`Config/Sessions/**.ini`).
//!
//! XML: `<VanDyke><key name="Sessions">` holds folders (`<key>` without a
//! `Hostname`) and sessions (`<key>` with `<string name="Hostname">`).
//! INI: one file per session, lines `S:"Hostname"=…` (string),
//! `D:"[SSH2] Port"=00000016` (hexadecimal dword), `Z:"Description"=…`
//! (a count followed by indented lines); the folders of the files are the
//! groups and `__FolderData__.ini` / `Default.ini` are not sessions.
//!
//! Used: `Hostname`, `[SSH2] Port` (or `Port`; Telnet: `Port`),
//! `Username`, `Protocol Name` (SSH1/SSH2, and Telnet as a Telnet host;
//! others are skipped), `Identity Filename V2` (the key file, imported
//! if readable here), `Description` (notes) and `Firewall Name` (a named
//! firewall/proxy cannot be imported: noted). Passwords are encrypted by
//! SecureCRT and are not imported.

use std::collections::HashMap;

use super::{ImportHost, ImportSet, protocol_of};

/// Values of one session (names lowercased).
type Values = HashMap<String, String>;

fn host_of(set: &mut ImportSet, name: &str, folder: &str, v: &Values) {
    let get = |k: &str| v.get(k).map(|s| s.trim().to_string()).unwrap_or_default();
    let protocol_name = get("protocol name");
    let Some(protocol) = protocol_of(&protocol_name) else {
        let protocol = protocol_name;
        set.warnings.push(
            t!(
                "import_export.warn.not_ssh",
                name = name.to_string(),
                protocol = protocol
            )
            .to_string(),
        );
        return;
    };
    let mut notes = vec![get("description")];
    let firewall = get("firewall name");
    if !firewall.is_empty() && !firewall.eq_ignore_ascii_case("none") {
        notes.push(t!("import_export.note.firewall", name = firewall).to_string());
    }
    let key = get("identity filename v2");
    let key = if key.is_empty() {
        get("identity filename")
    } else {
        key
    };
    // `C:\key.pub::rawkey` (V2 adds the kind after `::`).
    let key = key.split("::").next().unwrap_or("").trim().to_string();
    // Sessions keep the SSH ports even when they are Telnet.
    let ports = if protocol.is_telnet() {
        vec![get("port")]
    } else {
        vec![get("[ssh2] port"), get("[ssh1] port"), get("port")]
    };
    let port = ports.into_iter().find_map(|p| p.parse::<u16>().ok());
    let group = set.group_path(folder);
    set.push_host(ImportHost {
        label: name.to_string(),
        address: get("hostname"),
        port,
        username: Some(get("username")),
        group,
        notes: notes
            .into_iter()
            .filter(|n| !n.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        key_file: Some(key).filter(|k| !k.is_empty() && protocol.is_ssh()),
        protocol,
        ..Default::default()
    });
}

/// Sessions of a SecureCRT XML export.
pub fn parse_xml(text: &str) -> Result<ImportSet, String> {
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    let mut set = ImportSet::default();
    let root = doc.root_element();
    let sessions = root
        .descendants()
        .find(|n| {
            n.is_element()
                && n.tag_name().name() == "key"
                && n.attribute("name")
                    .is_some_and(|a| a.eq_ignore_ascii_case("Sessions"))
        })
        .ok_or_else(|| t!("import_export.error.no_sessions").to_string())?;
    walk_xml(&mut set, sessions, "", true);
    Ok(set)
}

fn walk_xml(set: &mut ImportSet, node: roxmltree::Node, folder: &str, top: bool) {
    for child in node.children().filter(|n| n.is_element()) {
        if child.tag_name().name() != "key" {
            continue;
        }
        let name = child.attribute("name").unwrap_or("").to_string();
        let mut values = Values::new();
        for v in child.children().filter(|n| n.is_element()) {
            let Some(vname) = v.attribute("name") else {
                continue;
            };
            let text = match v.tag_name().name() {
                "array" => v
                    .children()
                    .filter(|n| n.is_element())
                    .map(|n| n.text().unwrap_or("").to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
                "string" | "dword" => v.text().unwrap_or("").to_string(),
                _ => continue,
            };
            values.insert(vname.to_ascii_lowercase(), text);
        }
        if values.contains_key("hostname") {
            if top && name.eq_ignore_ascii_case("Default") {
                continue;
            }
            host_of(set, &name, folder, &values);
        } else {
            let sub = if folder.is_empty() {
                name
            } else {
                format!("{folder}/{name}")
            };
            walk_xml(set, child, &sub, false);
        }
    }
}

/// Values of a session `.ini` file.
pub fn parse_ini(text: &str) -> Values {
    let mut out = Values::new();
    let mut multi: Option<(String, usize)> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some((name, left)) = multi.as_mut() {
            if *left > 0 && line.starts_with(' ') {
                let entry = out.entry(name.clone()).or_default();
                if !entry.is_empty() {
                    entry.push('\n');
                }
                entry.push_str(line.trim());
                *left -= 1;
                continue;
            }
            multi = None;
        }
        let Some((kind, rest)) = line.split_once(":\"") else {
            continue;
        };
        let Some((name, value)) = rest.split_once("\"=") else {
            continue;
        };
        let name = name.to_ascii_lowercase();
        match kind.trim() {
            "S" => {
                out.insert(name, value.to_string());
            }
            "D" => {
                if let Ok(n) = u32::from_str_radix(value.trim(), 16) {
                    out.insert(name, n.to_string());
                }
            }
            "Z" | "A" => {
                let count = usize::from_str_radix(value.trim(), 16).unwrap_or(0);
                out.insert(name.clone(), String::new());
                multi = Some((name, count));
            }
            _ => {}
        }
    }
    out
}

/// Sessions of a folder of `.ini` files: (path relative to the
/// `Sessions` folder, with `/` and without `.ini`; content).
pub fn from_ini_files(files: &[(String, String)]) -> ImportSet {
    let mut set = ImportSet::default();
    let mut files: Vec<&(String, String)> = files.iter().collect();
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    for (path, text) in files {
        let (folder, name) = match path.rsplit_once('/') {
            Some((f, n)) => (f, n),
            None => ("", path.as_str()),
        };
        if name.eq_ignore_ascii_case("__FolderData__")
            || (folder.is_empty() && name.eq_ignore_ascii_case("Default"))
        {
            continue;
        }
        let values = parse_ini(text);
        if !values.contains_key("hostname") {
            continue;
        }
        host_of(&mut set, name, folder, &values);
    }
    set
}

/// Reads the `.ini` files under a SecureCRT `Sessions` folder.
pub fn read_folder(dir: &std::path::Path) -> std::io::Result<Vec<(String, String)>> {
    fn walk(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut Vec<(String, String)>,
        depth: usize,
    ) -> std::io::Result<()> {
        if depth > 16 {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                walk(base, &path, out, depth + 1)?;
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("ini"))
                && let Ok(rel) = path.with_extension("").strip_prefix(base)
            {
                let rel = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
                    .join("/");
                let bytes = std::fs::read(&path)?;
                out.push((rel, super::text::decode(&bytes)));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out, 0)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<VanDyke version="3.0">
  <key name="Sessions">
    <key name="Default">
      <string name="Hostname"></string>
      <dword name="[SSH2] Port">22</dword>
    </key>
    <key name="Production">
      <key name="Web">
        <key name="web-1">
          <string name="Hostname">10.0.0.11</string>
          <dword name="[SSH2] Port">2222</dword>
          <string name="Username">deploy</string>
          <string name="Protocol Name">SSH2</string>
          <string name="Identity Filename V2">C:\Users\ana\.ssh\id_rsa.pub::rawkey</string>
          <array name="Description">
            <string>Front end</string>
            <string>nginx</string>
          </array>
          <string name="Firewall Name">None</string>
        </key>
      </key>
      <key name="router">
        <string name="Hostname">10.0.0.1</string>
        <string name="Protocol Name">Telnet</string>
        <dword name="[SSH2] Port">22</dword>
        <dword name="Port">2323</dword>
      </key>
    </key>
    <key name="lab">
      <string name="Hostname">lab.local</string>
      <string name="Username">root</string>
      <string name="Firewall Name">Corp SOCKS</string>
    </key>
  </key>
</VanDyke>"#;

    #[test]
    fn reads_xml_exports() {
        let set = parse_xml(XML).unwrap();
        assert_eq!(set.hosts.len(), 3, "{:?}", set.warnings);
        let web = &set.hosts[0];
        assert_eq!(web.target(), "deploy@10.0.0.11:2222");
        assert_eq!(
            set.group_label(web.group.as_deref().unwrap()),
            "Production / Web"
        );
        assert_eq!(web.notes, "Front end\nnginx");
        assert_eq!(
            web.key_file.as_deref(),
            Some(r"C:\Users\ana\.ssh\id_rsa.pub")
        );
        // Telnet sessions are Telnet hosts (with the Telnet port).
        let router = &set.hosts[1];
        assert!(router.protocol.is_telnet());
        assert_eq!(router.port, Some(2323));
        let lab = &set.hosts[2];
        assert_eq!(lab.group, None);
        assert!(lab.notes.contains("Corp SOCKS"));
        assert_eq!(set.warnings.len(), 0);
        assert!(parse_xml("<VanDyke/>").is_err());
    }

    #[test]
    fn reads_session_files() {
        let web = "S:\"Protocol Name\"=SSH2\r\n\
S:\"Hostname\"=10.0.0.11\r\n\
S:\"Username\"=deploy\r\n\
D:\"[SSH2] Port\"=000008ae\r\n\
S:\"Identity Filename V2\"=\r\n\
Z:\"Description\"=00000002\r\n\
\x20Front end\r\n\
\x20nginx\r\n\
B:\"Color Scheme\"=00000004\r\n\
\x2000 00 00 00\r\n";
        let files = vec![
            ("Production/web-1".to_string(), web.to_string()),
            (
                "Production/__FolderData__".to_string(),
                "S:\"Is Expanded\"=1\r\n".to_string(),
            ),
            ("Default".to_string(), "S:\"Hostname\"=\r\n".to_string()),
            (
                "serial".to_string(),
                "S:\"Protocol Name\"=Serial\r\nS:\"Hostname\"=\r\n".to_string(),
            ),
        ];
        let set = from_ini_files(&files);
        assert_eq!(set.hosts.len(), 1);
        let h = &set.hosts[0];
        assert_eq!(h.label, "web-1");
        assert_eq!(h.port, Some(2222));
        assert_eq!(h.notes, "Front end\nnginx");
        assert_eq!(h.key_file, None);
        assert_eq!(set.group_label(h.group.as_deref().unwrap()), "Production");
        assert_eq!(set.warnings.len(), 1);
    }
}
