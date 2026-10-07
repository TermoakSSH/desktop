//! Termius.
//!
//! - CSV (Termius' own CSV import/export layout): the columns `Groups`,
//!   `Label`, `Tags`, `Hostname/IP`, `Protocol`, `Port`, and when present
//!   `Username`, `Password` and `Notes`. Nested groups are written
//!   `Parent/Child`; tags are separated by commas. Telnet rows are Telnet
//!   hosts; other protocols are skipped.
//! - JSON (best effort, there is no documented Termius JSON export): an
//!   array of hosts, or an object with `hosts` (and optionally `groups`,
//!   whose `id`/`parent_group` build the folder path). A host may have
//!   `label`, `address`/`hostname`/`host`/`ip`, `port`, `username`,
//!   `group` (a name, an id of `groups` or an object with `label`),
//!   `tags` (strings or objects with `label`), `notes`, and the
//!   `ssh_config` object of the Termius API (`port`, `identity.username`).

use std::collections::HashMap;

use serde_json::Value;

use super::{ImportHost, ImportSet, csv, protocol_of};

/// Reads a Termius export (JSON or CSV).
pub fn parse(text: &str) -> Result<ImportSet, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        let value: Value = serde_json::from_str(trimmed).map_err(|e| e.to_string())?;
        return Ok(from_json(&value));
    }
    let table = csv::read(text);
    let mapping = csv::guess_mapping(&table);
    if !mapping.is_usable() {
        return Err(t!("import_export.error.no_address_column").to_string());
    }
    Ok(csv::to_set(&table, &mapping))
}

fn str_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()).filter(|s| !s.trim().is_empty()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(o) => str_of(o.get("label").or_else(|| o.get("name"))),
        _ => None,
    }
}

fn first<'a>(o: &'a serde_json::Map<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| o.get(*k).filter(|v| !v.is_null()))
}

fn id_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Object(o) => o.get("id").and_then(id_of),
        _ => None,
    }
}

/// Hosts of a Termius JSON document.
pub fn from_json(value: &Value) -> ImportSet {
    let mut set = ImportSet::default();
    let root = value.get("data").unwrap_or(value);
    let hosts = match root {
        Value::Array(a) => a.clone(),
        Value::Object(o) => first(o, &["hosts", "host", "items"])
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    // id → (name, parent id)
    let mut groups: HashMap<String, (String, Option<String>)> = HashMap::new();
    if let Some(list) = root
        .get("groups")
        .or_else(|| root.get("group"))
        .and_then(Value::as_array)
    {
        for g in list {
            let Some(o) = g.as_object() else { continue };
            if let (Some(id), Some(name)) = (o.get("id").and_then(id_of), str_of(Some(g))) {
                let parent = first(o, &["parent_group", "parent", "parent_id"]).and_then(id_of);
                groups.insert(id, (name, parent));
            }
        }
    }
    let group_path = |v: &Value| -> Option<String> {
        if let Some(id) = id_of(v)
            && groups.contains_key(&id)
        {
            let mut names = Vec::new();
            let mut current = Some(id);
            while let Some(c) = current {
                let Some((name, parent)) = groups.get(&c) else {
                    break;
                };
                names.push(name.clone());
                current = parent.clone();
                if names.len() > 32 {
                    break;
                }
            }
            names.reverse();
            return Some(names.join("/"));
        }
        str_of(Some(v))
    };
    for h in &hosts {
        let Some(o) = h.as_object() else { continue };
        let ssh = o.get("ssh_config").and_then(Value::as_object);
        let protocol_text = str_of(first(o, &["protocol", "type"])).unwrap_or_default();
        let label = str_of(first(o, &["label", "name", "alias"])).unwrap_or_default();
        let Some(protocol) = protocol_of(&protocol_text) else {
            let protocol = protocol_text;
            set.warnings.push(
                t!(
                    "import_export.warn.not_ssh",
                    name = label.clone(),
                    protocol = protocol
                )
                .to_string(),
            );
            continue;
        };
        let port = first(o, &["port"])
            .or_else(|| ssh.and_then(|s| s.get("port")))
            .and_then(|p| p.as_u64().or_else(|| p.as_str()?.trim().parse().ok()))
            .and_then(|p| u16::try_from(p).ok());
        let username = str_of(first(o, &["username", "user", "login"])).or_else(|| {
            ssh.and_then(|s| s.get("identity"))
                .and_then(|i| i.get("username"))
                .and_then(|u| str_of(Some(u)))
        });
        let group = first(o, &["group", "group_name", "groups", "folder"])
            .and_then(group_path)
            .and_then(|p| set.group_path(&p));
        let tags = match first(o, &["tags", "tag"]) {
            Some(Value::Array(a)) => a.iter().filter_map(|t| str_of(Some(t))).collect(),
            Some(Value::String(s)) => super::split_tags(s),
            _ => Vec::new(),
        };
        set.push_host(ImportHost {
            label,
            address: str_of(first(o, &["address", "hostname", "host", "ip"])).unwrap_or_default(),
            port,
            username,
            group,
            tags,
            notes: str_of(first(o, &["notes", "note", "description"])).unwrap_or_default(),
            password: str_of(first(o, &["password"])),
            protocol,
            ..Default::default()
        });
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn termius_csv() {
        let text = "Groups,Label,Tags,Hostname/IP,Protocol,Port,Username\n\
            Production/Web,web-1,\"nginx,prod\",10.0.0.11,ssh,22,deploy\n\
            Production,db-1,,10.0.0.21,ssh,5522,postgres\n\
            Network,switch,,10.0.0.254,telnet,23,\n\
            ,lab,,lab.local,,,\n";
        let set = parse(text).unwrap();
        // The telnet row is a Telnet host.
        assert_eq!(set.hosts.len(), 4);
        assert_eq!(set.warnings.len(), 0);
        assert!(set.hosts[2].protocol.is_telnet());
        assert_eq!(set.hosts[2].port, Some(23));
        let web = &set.hosts[0];
        assert_eq!(web.label, "web-1");
        assert_eq!(web.tags, vec!["nginx", "prod"]);
        assert_eq!(
            set.group_label(web.group.as_deref().unwrap()),
            "Production / Web"
        );
        assert_eq!(set.hosts[1].port, Some(5522));
        assert_eq!(set.hosts[3].group, None);
    }

    #[test]
    fn termius_json() {
        let text = r#"{
          "groups": [
            {"id": 10, "label": "Production", "parent_group": null},
            {"id": 11, "label": "Web", "parent_group": 10}
          ],
          "hosts": [
            {"label": "web-1", "address": "10.0.0.11", "group": 11,
             "tags": [{"label": "prod"}, "web"],
             "ssh_config": {"port": 2222, "identity": {"username": "deploy"}}},
            {"label": "db", "hostname": "db.internal", "port": "22", "username": "pg",
             "group": {"label": "Databases"}},
            {"label": "router", "address": "10.0.0.1", "protocol": "telnet"}
          ]
        }"#;
        let set = parse(text).unwrap();
        assert_eq!(set.hosts.len(), 3);
        assert!(set.hosts[2].protocol.is_telnet());
        let web = &set.hosts[0];
        assert_eq!(web.port, Some(2222));
        assert_eq!(web.username.as_deref(), Some("deploy"));
        assert_eq!(web.tags, vec!["prod", "web"]);
        assert_eq!(
            set.group_label(web.group.as_deref().unwrap()),
            "Production / Web"
        );
        assert_eq!(
            set.group_label(set.hosts[1].group.as_deref().unwrap()),
            "Databases"
        );
        assert_eq!(set.warnings.len(), 0);
    }

    #[test]
    fn csv_without_address_column_fails() {
        assert!(parse("a,b\n1,2\n").is_err());
    }
}
