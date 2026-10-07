//! ZOC Terminal host directory (best effort: ZOC does not document the
//! format of its exports, and it changed between versions).
//!
//! Supported:
//! - CSV exports of the host directory: the usual columns plus ZOC's
//!   names (`Name`, `Connect to` with `host:port`, `Login`/`Username`,
//!   `Device`/`Connection type`, `Folder`).
//! - Text `.zocdir` files made of entries: a `[…]` header or a blank line
//!   starts an entry; lines are `key=value` (quotes optional). Keys read:
//!   `name`/`title`, `connectto`/`host`/`address`, `port`,
//!   `login`/`username`/`user`, `device`/`connectiontype`/`protocol`
//!   (entries whose device is Telnet, Serial/Modem, ISDN or Rlogin are
//!   skipped), `folder`/`group`/`path` (`Parent/Child` or `Parent\Child`),
//!   `comment`/`notes`.
//! - XML variants: any element with one of those keys as attributes or
//!   child elements is an entry; the `name` of enclosing elements is the
//!   folder.

use std::collections::HashMap;

use super::{ImportHost, ImportSet, csv};

/// Device names of ZOC that are not SSH.
fn not_ssh(device: &str) -> bool {
    let d = device.to_ascii_lowercase();
    [
        "telnet",
        "serial",
        "modem",
        "isdn",
        "rlogin",
        "tapi",
        "named pipe",
        "local shell",
    ]
    .iter()
    .any(|x| d.contains(x))
}

fn norm(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// An entry from its (normalized) keys.
fn entry(set: &mut ImportSet, v: &HashMap<String, String>, folder: &str) {
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| v.get(*k).filter(|s| !s.trim().is_empty()))
            .map(|s| s.trim().trim_matches('"').to_string())
            .unwrap_or_default()
    };
    let label = get(&["name", "title", "entry", "entryname"]);
    let device = get(&["device", "connectiontype", "protocol", "connection"]);
    if !device.is_empty() && not_ssh(&device) {
        set.warnings.push(
            t!(
                "import_export.warn.not_ssh",
                name = label,
                protocol = device
            )
            .to_string(),
        );
        return;
    }
    let address = get(&["connectto", "host", "hostname", "address", "destination"]);
    if address.is_empty() {
        return;
    }
    let own_folder = get(&["folder", "group", "path", "directory"]);
    let path = match (folder.is_empty(), own_folder.is_empty()) {
        (_, false) => own_folder,
        (false, true) => folder.to_string(),
        (true, true) => String::new(),
    };
    let group = set.group_path(&path);
    set.push_host(ImportHost {
        label,
        address,
        port: get(&["port"]).parse().ok(),
        username: Some(get(&["login", "username", "user", "loginname"])),
        group,
        notes: get(&["comment", "notes", "description", "remark"]),
        ..Default::default()
    });
}

/// Hosts of a ZOC export.
pub fn parse(text: &str) -> Result<ImportSet, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('<') {
        return parse_xml(trimmed);
    }
    let table = csv::read(text);
    let mapping = csv::guess_mapping(&table);
    let first_line = trimmed.lines().next().unwrap_or("");
    if mapping.is_usable() && !first_line.contains('=') {
        return Ok(csv::to_set(&table, &mapping));
    }
    let mut set = ImportSet::default();
    let mut current: HashMap<String, String> = HashMap::new();
    let flush = |set: &mut ImportSet, current: &mut HashMap<String, String>| {
        if !current.is_empty() {
            entry(set, current, "");
            current.clear();
        }
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('[') {
            flush(&mut set, &mut current);
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                current.insert("title".into(), name.to_string());
            }
            continue;
        }
        if line.starts_with(';') || line.starts_with('#') || line.starts_with("//") {
            continue;
        }
        if let Some((k, v)) = line.split_once('=').or_else(|| line.split_once(':')) {
            let k = norm(k);
            // A second "name" starts another entry (files without blanks).
            if (k == "name" || k == "title") && current.contains_key("connectto") {
                flush(&mut set, &mut current);
            }
            current.insert(k, v.trim().to_string());
        }
    }
    flush(&mut set, &mut current);
    if set.hosts.is_empty() && set.warnings.is_empty() {
        return Err(t!("import_export.error.no_hosts_found").to_string());
    }
    Ok(set)
}

fn parse_xml(text: &str) -> Result<ImportSet, String> {
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    let mut set = ImportSet::default();
    const ADDRESS: [&str; 4] = ["connectto", "host", "hostname", "address"];
    for node in doc.descendants().filter(|n| n.is_element()) {
        let mut v: HashMap<String, String> = HashMap::new();
        for a in node.attributes() {
            v.insert(norm(a.name()), a.value().to_string());
        }
        for c in node.children().filter(|c| c.is_element()) {
            if c.children().all(|n| n.is_text())
                && let Some(t) = c.text()
            {
                v.insert(norm(c.tag_name().name()), t.to_string());
            }
        }
        if !ADDRESS.iter().any(|k| v.contains_key(*k)) {
            continue;
        }
        let folder: Vec<String> = node
            .ancestors()
            .skip(1)
            .filter(|a| a.is_element() && a.parent().is_some_and(|p| p.is_element()))
            .filter_map(|a| a.attribute("name").or_else(|| a.attribute("title")))
            .map(str::to_string)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        entry(&mut set, &v, &folder.join("/"));
    }
    if set.hosts.is_empty() && set.warnings.is_empty() {
        return Err(t!("import_export.error.no_hosts_found").to_string());
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_export() {
        let text = "Name;Connect to;Login;Device;Folder\n\
            web-1;10.0.0.11:2222;deploy;Secure Shell;Production\\Web\n\
            modem;5551234;;Modem/TAPI;\n";
        let set = parse(text).unwrap();
        assert_eq!(set.hosts.len(), 1);
        let h = &set.hosts[0];
        assert_eq!(h.target(), "deploy@10.0.0.11:2222");
        assert_eq!(
            set.group_label(h.group.as_deref().unwrap()),
            "Production / Web"
        );
        assert_eq!(set.warnings.len(), 1);
    }

    #[test]
    fn zocdir_entries() {
        let text = "[web-1]\n\
            connectto=\"10.0.0.11\"\n\
            port=2222\n\
            login=deploy\n\
            device=SSH\n\
            folder=Production/Web\n\
            \n\
            [switch]\n\
            connectto=10.0.0.254\n\
            device=Telnet\n\
            \n\
            name=db\n\
            connectto=db.internal\n\
            name=lab\n\
            connectto=lab.local\n\
            comment=Test box\n";
        let set = parse(text).unwrap();
        assert_eq!(set.hosts.len(), 3, "{:?}", set.hosts);
        assert_eq!(set.hosts[0].port, Some(2222));
        assert_eq!(set.hosts[0].label, "web-1");
        assert_eq!(set.hosts[1].label, "db");
        assert_eq!(set.hosts[2].notes, "Test box");
        assert_eq!(set.warnings.len(), 1);
        assert!(parse("hello\nworld\n").is_err());
    }

    #[test]
    fn xml_variant() {
        let text = r#"<hostdirectory>
          <folder name="Production">
            <entry name="web-1" connectto="10.0.0.11" port="22" login="deploy"/>
            <entry><name>db</name><connectto>10.0.0.21</connectto><device>SSH</device></entry>
          </folder>
          <entry name="router" connectto="10.0.0.1" device="Telnet"/>
        </hostdirectory>"#;
        let set = parse(text).unwrap();
        assert_eq!(set.hosts.len(), 2);
        assert_eq!(
            set.group_label(set.hosts[0].group.as_deref().unwrap()),
            "Production"
        );
        assert_eq!(set.hosts[1].label, "db");
        assert_eq!(set.warnings.len(), 1);
    }
}
