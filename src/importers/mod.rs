//! Importers and exporters of hosts: pure parsers of other apps' files
//! (and of Termoak's own export) into one [`ImportSet`], the duplicate
//! detection of the preview, and the writers of the export files.
//!
//! Nothing here touches the vault: [`apply`] saves a reviewed set into a
//! vault and gathers what an export writes. The views are in
//! `views/import_export.rs`.
//!
//! Formats (assumptions are documented in each module):
//! - [`termoak`]: Termoak JSON, versioned, secrets sealed with a passphrase.
//! - [`csv`]: any CSV with a header row (`,`, `;` or tab), with a column
//!   mapping when the headers are unknown.
//! - [`termius`]: Termius CSV (and a best-effort JSON).
//! - [`putty`]: `.reg` exports and, on Windows, the registry.
//! - [`mobaxterm`]: `MobaXterm.ini` and `.mxtsessions` bookmarks.
//! - [`securecrt`]: XML export and the folder of session `.ini` files.
//! - [`zoc`]: host directory exports (`.zocdir`, CSV), best effort.

pub mod apply;
pub mod csv;
pub mod mobaxterm;
pub mod putty;
pub mod securecrt;
pub mod termius;
pub mod termoak;
pub mod text;
pub mod zoc;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use termoak_core::Id;
use termoak_core::model::{HostSettings, ProxySettings};

/// Where a set comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Termoak,
    Csv,
    SshConfig,
    Termius,
    Putty,
    MobaXterm,
    SecureCrt,
    Zoc,
}

impl Source {
    /// The importers of the "Import/Export" menu, in order.
    pub const MENU: [Source; 8] = [
        Source::Termoak,
        Source::Csv,
        Source::SshConfig,
        Source::Termius,
        Source::Putty,
        Source::MobaXterm,
        Source::SecureCrt,
        Source::Zoc,
    ];

    /// Name of the source (product names are not translated).
    pub fn name(self) -> String {
        match self {
            Source::Termoak => t!("import_export.source.termoak").to_string(),
            Source::Csv => "CSV".into(),
            Source::SshConfig => "~/.ssh/config".into(),
            Source::Termius => "Termius".into(),
            Source::Putty => "PuTTY".into(),
            Source::MobaXterm => "MobaXterm".into(),
            Source::SecureCrt => "SecureCRT".into(),
            Source::Zoc => "ZOC Terminal".into(),
        }
    }
}

/// Guesses the source of a file from its name and content (for "Import…").
pub fn detect(path: &Path, text: &str) -> Source {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let head: String = text.trim_start().chars().take(4096).collect();
    let lower = head.to_ascii_lowercase();
    if head.starts_with('{') || head.starts_with('[') && ext == "json" {
        return if lower.contains(termoak::FORMAT) {
            Source::Termoak
        } else {
            Source::Termius
        };
    }
    if lower.starts_with("windows registry editor") || lower.starts_with("regedit4") {
        return Source::Putty;
    }
    if lower.contains("[bookmarks") {
        return Source::MobaXterm;
    }
    if lower.contains("<vandyke") || lower.contains("s:\"hostname\"=") {
        return Source::SecureCrt;
    }
    match ext.as_str() {
        "reg" => Source::Putty,
        "mxtsessions" => Source::MobaXterm,
        "zocdir" | "zhd" => Source::Zoc,
        "xml" => Source::SecureCrt,
        "json" => Source::Termius,
        "csv" | "tsv" | "txt" => Source::Csv,
        _ if lower.lines().any(|l| {
            l.trim_start().starts_with("host ") || l.trim_start().starts_with("host\t")
        }) =>
        {
            Source::SshConfig
        }
        _ => Source::Csv,
    }
}

/// Folder of hosts in the set. `key` is unique in the set; parents come
/// before their children.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportGroup {
    pub key: String,
    pub name: String,
    pub parent: Option<String>,
    pub color: Option<String>,
    /// Settings the hosts inherit (Termoak JSON; ids are of the file).
    pub settings: HostSettings,
}

/// Host found in a file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportHost {
    pub label: String,
    pub address: String,
    pub port: Option<u16>,
    pub username: Option<String>,
    /// [`ImportGroup::key`].
    pub group: Option<String>,
    pub tags: Vec<String>,
    pub notes: String,
    pub color: Option<String>,
    pub favorite: bool,
    pub password: Option<String>,
    pub proxy: Option<ProxySettings>,
    pub proxy_password: Option<String>,
    /// Path of a private key file the source points to (imported if it
    /// can be read on this computer).
    pub key_file: Option<String>,
    /// Termoak JSON: id of the host in the file and the rest of its
    /// settings (ids of the file, remapped when imported).
    pub source_id: Option<Id>,
    pub settings: Option<HostSettings>,
    pub os: Option<String>,
    pub os_version: Option<String>,
}

impl ImportHost {
    /// `user@host:port` as shown in the preview.
    pub fn target(&self) -> String {
        let mut s = String::new();
        if let Some(u) = &self.username {
            s.push_str(u);
            s.push('@');
        }
        s.push_str(&self.address);
        if let Some(p) = self.port.filter(|p| *p != 22) {
            s.push_str(&format!(":{p}"));
        }
        s
    }

    /// Port used for duplicate detection.
    pub fn effective_port(&self) -> u16 {
        self.port
            .or(self.settings.as_ref().and_then(|s| s.port))
            .unwrap_or(22)
    }
}

/// SSH key of a Termoak export.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportKey {
    pub source_id: Id,
    pub label: String,
    pub algorithm: String,
    pub public_key: String,
    pub fingerprint: String,
    pub comment: String,
    pub has_passphrase: bool,
    pub certificate: Option<String>,
    pub private_key: Option<String>,
    pub passphrase: Option<String>,
}

/// Identity of a Termoak export.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportIdentity {
    pub source_id: Id,
    pub label: String,
    pub username: String,
    pub key: Option<Id>,
    pub password: Option<String>,
}

/// Snippet of a Termoak export.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportSnippet {
    pub source_id: Id,
    pub name: String,
    pub script: String,
    pub description: String,
    pub tags: Vec<String>,
}

/// Everything a file brings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImportSet {
    pub groups: Vec<ImportGroup>,
    pub hosts: Vec<ImportHost>,
    pub keys: Vec<ImportKey>,
    pub identities: Vec<ImportIdentity>,
    pub snippets: Vec<ImportSnippet>,
    /// What could not be imported, or only partly.
    pub warnings: Vec<String>,
}

impl ImportSet {
    /// Key of the group at a folder path (`Prod/Web`, `Prod\Web`),
    /// creating it and its parents. `None` for an empty path.
    pub fn group_path(&mut self, path: &str) -> Option<String> {
        let parts: Vec<&str> = path
            .split(['/', '\\'])
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let mut parent: Option<String> = None;
        for (i, name) in parts.iter().enumerate() {
            let key = parts[..=i].join("/");
            if !self.groups.iter().any(|g| g.key == key) {
                self.groups.push(ImportGroup {
                    key: key.clone(),
                    name: (*name).to_string(),
                    parent: parent.clone(),
                    color: None,
                    settings: HostSettings::default(),
                });
            }
            parent = Some(key);
        }
        parent
    }

    /// Path of a group (`Prod / Web`) for the preview.
    pub fn group_label(&self, key: &str) -> String {
        let mut names = Vec::new();
        let mut current = Some(key.to_string());
        let mut guard = 0;
        while let Some(k) = current {
            let Some(g) = self.groups.iter().find(|g| g.key == k) else {
                break;
            };
            names.push(g.name.clone());
            current = g.parent.clone();
            guard += 1;
            if guard > 64 {
                break;
            }
        }
        names.reverse();
        names.join(" / ")
    }

    /// Adds a host after cleaning it up: `user@host:port` in the address
    /// is split, the label defaults to the address. Hosts without an
    /// address are left out with a warning.
    pub fn push_host(&mut self, mut host: ImportHost) {
        normalize(&mut host);
        if host.address.is_empty() {
            self.warnings.push(
                t!(
                    "import_export.warn.no_address",
                    name = if host.label.is_empty() {
                        "?".to_string()
                    } else {
                        host.label.clone()
                    }
                )
                .to_string(),
            );
            return;
        }
        if host.label.is_empty() {
            host.label = host.address.clone();
        }
        self.hosts.push(host);
    }
}

/// Splits `user@host:port` and trims the fields.
pub fn normalize(host: &mut ImportHost) {
    host.label = host.label.trim().to_string();
    let mut address = host.address.trim().to_string();
    if let Some((user, rest)) = address.rsplit_once('@') {
        if host.username.as_deref().is_none_or(str::is_empty) && !user.is_empty() {
            host.username = Some(user.to_string());
        }
        address = rest.to_string();
    }
    // `[v6]:port`, `host:port` (one colon only: a bare IPv6 stays).
    if let Some(inner) = address.strip_prefix('[')
        && let Some((addr, tail)) = inner.split_once(']')
    {
        if host.port.is_none() {
            host.port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
        }
        address = addr.to_string();
    } else if address.matches(':').count() == 1
        && let Some((h, p)) = address.split_once(':')
        && let Ok(port) = p.parse::<u16>()
    {
        if host.port.is_none() {
            host.port = Some(port);
        }
        address = h.to_string();
    }
    host.address = address
        .trim()
        .trim_start_matches("ssh://")
        .trim_end_matches('/')
        .to_string();
    host.username = host
        .username
        .take()
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    host.port = host.port.filter(|p| *p != 0);
    host.tags = clean_tags(std::mem::take(&mut host.tags));
}

/// Tags without blanks or repeats, in order.
pub fn clean_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tags {
        let t = t.trim().to_string();
        if !t.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(&t)) {
            out.push(t);
        }
    }
    out
}

/// Splits a list of tags written as text (`a, b; c | d`).
pub fn split_tags(text: &str) -> Vec<String> {
    clean_tags(text.split([',', ';', '|']).map(str::to_string).collect())
}

/// Protocols that mean SSH (other sessions are skipped).
pub fn is_ssh_protocol(p: &str) -> bool {
    let p = p.trim().to_ascii_lowercase();
    p.is_empty()
        || p.starts_with("ssh")
        || p == "sftp"
        || p == "mosh"
        || p == "scp"
        || p == "secure shell"
}

// ---------------------------------------------------------------------------
// Duplicates and what to do with each host
// ---------------------------------------------------------------------------

/// What identifies a host when looking for duplicates.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DupKey {
    pub address: String,
    pub port: u16,
    pub username: String,
}

impl DupKey {
    pub fn new(address: &str, port: u16, username: Option<&str>) -> Self {
        Self {
            address: address.trim().to_lowercase(),
            port,
            username: username.unwrap_or("").trim().to_string(),
        }
    }
}

/// A host already in the target vault.
#[derive(Debug, Clone)]
pub struct ExistingHost {
    pub id: Id,
    pub label: String,
    pub key: DupKey,
}

/// What the import does with duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DupPolicy {
    #[default]
    Skip,
    Update,
    Copy,
}

/// Why a host of the file is a duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Duplicate {
    /// Of a host already in the vault (index in the list given).
    Existing(usize),
    /// Of an earlier host of the same file.
    InFile(usize),
}

/// The duplicate of each host of the set (same address, port and user).
pub fn find_duplicates(set: &ImportSet, existing: &[ExistingHost]) -> Vec<Option<Duplicate>> {
    let by_key: HashMap<&DupKey, usize> = existing
        .iter()
        .enumerate()
        .rev()
        .map(|(i, e)| (&e.key, i))
        .collect();
    let mut seen: HashMap<DupKey, usize> = HashMap::new();
    set.hosts
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let key = DupKey::new(&h.address, h.effective_port(), h.username.as_deref());
            if let Some(ix) = by_key.get(&key) {
                return Some(Duplicate::Existing(*ix));
            }
            match seen.get(&key) {
                Some(first) => Some(Duplicate::InFile(*first)),
                None => {
                    seen.insert(key, i);
                    None
                }
            }
        })
        .collect()
}

/// What happens to a host of the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    /// Updates that host of the vault.
    Update(Id),
    /// Creates it although it is a duplicate.
    Copy,
    Skip,
}

/// The action for each host: unchecked ones are skipped, duplicates follow
/// the policy (a repeat inside the file is only created with "copy").
pub fn plan(
    dups: &[Option<Duplicate>],
    included: &[bool],
    existing: &[ExistingHost],
    policy: DupPolicy,
) -> Vec<Action> {
    dups.iter()
        .enumerate()
        .map(|(i, dup)| {
            if !included.get(i).copied().unwrap_or(true) {
                return Action::Skip;
            }
            match (dup, policy) {
                (None, _) => Action::Create,
                (Some(_), DupPolicy::Skip) => Action::Skip,
                (Some(Duplicate::Existing(ix)), DupPolicy::Update) => existing
                    .get(*ix)
                    .map_or(Action::Create, |e| Action::Update(e.id)),
                (Some(Duplicate::InFile(_)), DupPolicy::Update) => Action::Skip,
                (Some(_), DupPolicy::Copy) => Action::Copy,
            }
        })
        .collect()
}

/// How many hosts each action gets.
pub fn count_actions(actions: &[Action]) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for a in actions {
        let k = match a {
            Action::Create => "create",
            Action::Update(_) => "update",
            Action::Copy => "copy",
            Action::Skip => "skip",
        };
        *out.entry(k).or_insert(0) += 1;
    }
    out
}

/// A label not used yet: `web`, `web (2)`, `web (3)`…
pub fn unique_label(label: &str, taken: &[String]) -> String {
    let used = |l: &str| taken.iter().any(|t| t.eq_ignore_ascii_case(l));
    if !used(label) {
        return label.to_string();
    }
    (2..)
        .map(|n| format!("{label} ({n})"))
        .find(|l| !used(l))
        .unwrap_or_else(|| label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(label: &str, address: &str) -> ImportHost {
        ImportHost {
            label: label.into(),
            address: address.into(),
            ..Default::default()
        }
    }

    #[test]
    fn normalizes_user_host_and_port() {
        let mut set = ImportSet::default();
        set.push_host(host("", "deploy@web.example.com:2222"));
        set.push_host(host("v6", "[2001:db8::1]:2200"));
        set.push_host(host("bare v6", "2001:db8::2"));
        set.push_host(host("nothing", "  "));
        assert_eq!(set.hosts.len(), 3);
        let h = &set.hosts[0];
        assert_eq!(h.label, "web.example.com");
        assert_eq!(h.address, "web.example.com");
        assert_eq!(h.username.as_deref(), Some("deploy"));
        assert_eq!(h.port, Some(2222));
        assert_eq!(h.target(), "deploy@web.example.com:2222");
        assert_eq!(set.hosts[1].address, "2001:db8::1");
        assert_eq!(set.hosts[1].port, Some(2200));
        assert_eq!(set.hosts[2].address, "2001:db8::2");
        assert_eq!(set.hosts[2].port, None);
        assert_eq!(set.warnings.len(), 1);
    }

    #[test]
    fn group_paths_create_parents_once() {
        let mut set = ImportSet::default();
        assert_eq!(set.group_path("Prod/Web").as_deref(), Some("Prod/Web"));
        assert_eq!(set.group_path("Prod\\DB").as_deref(), Some("Prod/DB"));
        assert_eq!(set.group_path(" / "), None);
        assert_eq!(set.groups.len(), 3);
        assert_eq!(set.groups[0].name, "Prod");
        assert_eq!(set.groups[1].parent.as_deref(), Some("Prod"));
        assert_eq!(set.group_label("Prod/DB"), "Prod / DB");
    }

    #[test]
    fn duplicates_and_plan() {
        let mut set = ImportSet::default();
        set.push_host(host("a", "root@10.0.0.1"));
        set.push_host(host("b", "10.0.0.2:2222"));
        set.push_host(host("c", "ROOT@10.0.0.1:22"));
        set.push_host(host("d", "Web.Example.com"));
        let existing = vec![ExistingHost {
            id: Id::from_u128(7),
            label: "web".into(),
            key: DupKey::new("web.example.com", 22, None),
        }];
        let dups = find_duplicates(&set, &existing);
        // "ROOT" is another user: users are case-sensitive.
        assert_eq!(dups, vec![None, None, None, Some(Duplicate::Existing(0))]);

        set.push_host(host("e", "root@10.0.0.1"));
        let dups = find_duplicates(&set, &existing);
        assert_eq!(dups[4], Some(Duplicate::InFile(0)));

        let included = vec![true, false, true, true, true];
        assert_eq!(
            plan(&dups, &included, &existing, DupPolicy::Skip),
            vec![
                Action::Create,
                Action::Skip,
                Action::Create,
                Action::Skip,
                Action::Skip
            ]
        );
        assert_eq!(
            plan(&dups, &included, &existing, DupPolicy::Update)[3],
            Action::Update(Id::from_u128(7))
        );
        let copy = plan(&dups, &included, &existing, DupPolicy::Copy);
        assert_eq!(copy[3], Action::Copy);
        assert_eq!(copy[4], Action::Copy);
        assert_eq!(count_actions(&copy)["copy"], 2);
    }

    #[test]
    fn labels_and_tags() {
        let taken = vec!["web".to_string(), "web (2)".to_string()];
        assert_eq!(unique_label("Web", &taken), "Web (3)");
        assert_eq!(unique_label("db", &taken), "db");
        assert_eq!(
            split_tags("prod, web;Prod | eu "),
            vec!["prod", "web", "eu"]
        );
        assert!(is_ssh_protocol("SSH2"));
        assert!(is_ssh_protocol(""));
        assert!(!is_ssh_protocol("telnet"));
    }

    #[test]
    fn detects_sources() {
        let p = |s: &str| Path::new(s).to_path_buf();
        assert_eq!(
            detect(&p("x.json"), "{\"format\":\"termoak-export\"}"),
            Source::Termoak
        );
        assert_eq!(detect(&p("hosts.json"), "[{\"label\":1}]"), Source::Termius);
        assert_eq!(
            detect(&p("putty.reg"), "Windows Registry Editor Version 5.00\r\n"),
            Source::Putty
        );
        assert_eq!(
            detect(&p("MobaXterm.ini"), "[Misc]\n[Bookmarks]\nSubRep=\n"),
            Source::MobaXterm
        );
        assert_eq!(
            detect(&p("export.xml"), "<?xml version=\"1.0\"?><VanDyke>"),
            Source::SecureCrt
        );
        assert_eq!(detect(&p("hosts.csv"), "label,host\n"), Source::Csv);
        assert_eq!(detect(&p("my.zocdir"), "x"), Source::Zoc);
        assert_eq!(
            detect(&p("config"), "Host web\n  HostName 1.2.3.4\n"),
            Source::SshConfig
        );
    }
}
