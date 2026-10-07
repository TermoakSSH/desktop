//! Saving a reviewed import into a vault, and gathering what an export
//! writes. Unlike the parsers, this talks to the [`Workspace`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use termoak_client::{ItemFilter, ItemRef, SaveTarget, Scope, Workspace};
use termoak_core::Id;
use termoak_core::model::{
    Entity, Group, Host, HostSecret, HostSettings, Identity, IdentitySecret, SecretUpdate, Snippet,
    SshKey, SshKeySecret,
};

use super::csv::ExportRow;
use super::termoak::{ExportFile, Secrets};
use super::{Action, DupKey, ExistingHost, ImportSet, unique_label};
use crate::state::api_error;

/// A vault (or This device): where an import goes or what an export reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    pub scope: Scope,
    /// Vault of an account (`None`: an account without vaults, or This
    /// device).
    pub vault: Option<Id>,
}

impl Place {
    /// Where new items go.
    pub fn target(self) -> SaveTarget {
        match self.scope {
            Scope::Device => SaveTarget::Device,
            Scope::Account(account) => SaveTarget::Account {
                account,
                vault: self.vault,
            },
        }
    }

    /// Where an existing item is saved (it stays in its vault).
    fn existing_target(self) -> SaveTarget {
        match self.scope {
            Scope::Device => SaveTarget::Device,
            Scope::Account(account) => SaveTarget::Account {
                account,
                vault: None,
            },
        }
    }

    fn filter(self) -> ItemFilter {
        match self.scope {
            Scope::Device => ItemFilter::device_only(),
            Scope::Account(a) => ItemFilter {
                accounts: Some(vec![a]),
                vaults: self.vault.map(|v| vec![v]),
                include_device: false,
            },
        }
    }

    fn item(self, id: Id) -> ItemRef {
        ItemRef {
            scope: self.scope,
            id,
        }
    }
}

/// The items of a place.
#[derive(Debug, Clone, Default)]
pub struct PlaceData {
    pub hosts: Vec<Host>,
    pub groups: Vec<Group>,
    pub keys: Vec<SshKey>,
    pub identities: Vec<Identity>,
    pub snippets: Vec<Snippet>,
}

async fn list<T: Entity>(ws: &Workspace, place: Place) -> Result<Vec<T>, String> {
    Ok(ws
        .list_items::<T>(&place.filter())
        .await
        .map_err(api_error)?
        .into_iter()
        .filter(|s| s.scope == place.scope)
        .map(|s| s.record.data)
        .collect())
}

/// Reads the items of a place.
pub async fn load_place(ws: &Workspace, place: Place) -> Result<PlaceData, String> {
    Ok(PlaceData {
        hosts: list(ws, place).await?,
        groups: list(ws, place).await?,
        keys: list(ws, place).await?,
        identities: list(ws, place).await?,
        snippets: list(ws, place).await?,
    })
}

impl PlaceData {
    fn group(&self, id: Option<Id>) -> Option<&Group> {
        id.and_then(|id| self.groups.iter().find(|g| g.id == id))
    }

    /// The hosts with what identifies them for duplicates: their own port
    /// and user, or the group's (and parents'), or the identity's user.
    pub fn existing_hosts(&self) -> Vec<ExistingHost> {
        self.hosts
            .iter()
            .map(|h| {
                let mut port = h.settings.port;
                let mut user = h.settings.username.clone();
                let mut identity = h.settings.identity_id;
                let mut g = self.group(h.group_id);
                let mut guard = 0;
                while let Some(group) = g {
                    port = port.or(group.settings.port);
                    user = user.or_else(|| group.settings.username.clone());
                    identity = identity.or(group.settings.identity_id);
                    g = self.group(group.parent_id);
                    guard += 1;
                    if guard > 32 {
                        break;
                    }
                }
                let user = user.or_else(|| {
                    identity
                        .and_then(|i| self.identities.iter().find(|x| x.id == i))
                        .map(|i| i.username.clone())
                });
                ExistingHost {
                    id: h.id,
                    label: h.label.clone(),
                    key: DupKey::new(&h.address, port.unwrap_or(22), user.as_deref()),
                }
            })
            .collect()
    }

    /// `Parent/Child` path of a group.
    pub fn group_path(&self, id: Option<Id>) -> String {
        let mut names = Vec::new();
        let mut g = self.group(id);
        while let Some(group) = g {
            names.push(group.name.clone());
            g = self.group(group.parent_id).filter(|_| names.len() < 32);
        }
        names.reverse();
        names.join("/")
    }

    /// A group and every group under it.
    pub fn subtree(&self, root: Id) -> Vec<Id> {
        let mut out = vec![root];
        let mut i = 0;
        while i < out.len() {
            let parent = out[i];
            for g in &self.groups {
                if g.parent_id == Some(parent) && !out.contains(&g.id) {
                    out.push(g.id);
                }
            }
            i += 1;
        }
        out
    }
}

/// An import to run.
pub struct Request {
    pub set: ImportSet,
    pub actions: Vec<Action>,
    pub place: Place,
    /// Group every imported host and top-level group goes under.
    pub base_group: Option<String>,
    pub existing: PlaceData,
    pub home: Option<PathBuf>,
}

/// What an import did.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub groups: usize,
    pub keys: usize,
    pub keys_reused: usize,
    pub identities: usize,
    pub snippets: usize,
    pub warnings: Vec<String>,
}

async fn save<T: Entity>(
    ws: &Workspace,
    target: SaveTarget,
    data: T,
    secret: SecretUpdate<T::Secret>,
) -> Result<Id, String> {
    ws.save_item(target, data, secret, None)
        .await
        .map(|s| s.record.data.id())
        .map_err(api_error)
}

/// A path as the user wrote it: `~` is the home folder.
fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            home.join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

/// Imports the key at `path` (a `.pub` points to its private key next to
/// it), reusing a key of the vault with the same fingerprint.
async fn key_from_file(
    ws: &Workspace,
    target: SaveTarget,
    path: &str,
    home: Option<&Path>,
    keys: &mut Vec<SshKey>,
    summary: &mut Summary,
) -> Result<Id, String> {
    let mut file = expand(path.trim(), home);
    if file.extension().is_some_and(|e| e == "pub") {
        file.set_extension("");
    }
    let bytes = tokio::fs::read(&file)
        .await
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let material = termoak_ssh::keys::import_private(&text, None).map_err(|e| e.to_string())?;
    if let Some(k) = keys.iter().find(|k| k.fingerprint == material.fingerprint) {
        summary.keys_reused += 1;
        return Ok(k.id);
    }
    let label = file
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| material.comment.clone());
    let key = SshKey {
        id: Id::nil(),
        label,
        algorithm: material.algorithm.clone(),
        public_key: material.public_openssh.clone(),
        fingerprint: material.fingerprint.clone(),
        comment: material.comment.clone(),
        has_passphrase: material.encrypted,
        certificate: None,
    };
    let secret = SshKeySecret {
        private_key: Some(material.private_openssh.clone()),
        passphrase: None,
    };
    let id = save(ws, target, key.clone(), SecretUpdate::Set(secret)).await?;
    keys.push(SshKey { id, ..key });
    summary.keys += 1;
    Ok(id)
}

/// Settings of the file with its ids rewritten to the new ones (jumps are
/// set afterwards, once every host exists).
fn remap(
    s: &HostSettings,
    keys: &HashMap<Id, Id>,
    identities: &HashMap<Id, Id>,
    snippets: &HashMap<Id, Id>,
) -> HostSettings {
    HostSettings {
        identity_id: s.identity_id.and_then(|i| identities.get(&i).copied()),
        key_id: s.key_id.and_then(|k| keys.get(&k).copied()),
        startup_snippet_id: s.startup_snippet_id.and_then(|k| snippets.get(&k).copied()),
        jump_host_ids: None,
        ..s.clone()
    }
}

struct NewGroup<'a> {
    name: &'a str,
    parent: Option<Id>,
    color: Option<String>,
    settings: HostSettings,
}

/// The group with that name under `parent`, created if missing.
async fn find_or_create(
    ws: &Workspace,
    target: SaveTarget,
    groups: &mut Vec<Group>,
    new: NewGroup<'_>,
    s: &mut Summary,
) -> Result<Id, String> {
    let name = new.name.trim();
    if let Some(g) = groups
        .iter()
        .find(|g| g.parent_id == new.parent && g.name.trim().eq_ignore_ascii_case(name))
    {
        return Ok(g.id);
    }
    let group = Group {
        id: Id::nil(),
        name: name.to_string(),
        parent_id: new.parent,
        color: new.color,
        settings: new.settings,
    };
    let id = save(ws, target, group.clone(), SecretUpdate::Keep).await?;
    groups.push(Group { id, ..group });
    s.groups += 1;
    Ok(id)
}

/// Runs an import.
pub async fn run(ws: Workspace, req: Request) -> Result<Summary, String> {
    let Request {
        set,
        actions,
        place,
        base_group,
        existing,
        home,
    } = req;
    let target = place.target();
    let mut s = Summary::default();
    let mut keys = existing.keys.clone();

    // Keys, identities and snippets (Termoak JSON).
    let mut key_map: HashMap<Id, Id> = HashMap::new();
    for k in &set.keys {
        if let Some(e) = keys
            .iter()
            .find(|e| !k.fingerprint.is_empty() && e.fingerprint == k.fingerprint)
        {
            key_map.insert(k.source_id, e.id);
            s.keys_reused += 1;
            continue;
        }
        let Some(private) = k.private_key.clone() else {
            s.warnings.push(
                t!(
                    "import_export.warn.key_without_private",
                    name = k.label.clone()
                )
                .to_string(),
            );
            continue;
        };
        let key = SshKey {
            id: Id::nil(),
            label: k.label.clone(),
            algorithm: k.algorithm.clone(),
            public_key: k.public_key.clone(),
            fingerprint: k.fingerprint.clone(),
            comment: k.comment.clone(),
            has_passphrase: k.has_passphrase,
            certificate: k.certificate.clone(),
        };
        let secret = SshKeySecret {
            private_key: Some(private),
            passphrase: k.passphrase.clone(),
        };
        let id = save(&ws, target, key.clone(), SecretUpdate::Set(secret)).await?;
        keys.push(SshKey { id, ..key });
        key_map.insert(k.source_id, id);
        s.keys += 1;
    }
    let mut identity_map: HashMap<Id, Id> = HashMap::new();
    for i in &set.identities {
        if let Some(e) = existing
            .identities
            .iter()
            .find(|e| e.label.eq_ignore_ascii_case(&i.label) && e.username == i.username)
        {
            identity_map.insert(i.source_id, e.id);
            continue;
        }
        let identity = Identity {
            id: Id::nil(),
            label: i.label.clone(),
            username: i.username.clone(),
            key_id: i.key.and_then(|k| key_map.get(&k).copied()),
        };
        let secret = match &i.password {
            Some(p) => SecretUpdate::Set(IdentitySecret {
                password: Some(p.clone()),
            }),
            None => SecretUpdate::Keep,
        };
        let id = save(&ws, target, identity, secret).await?;
        identity_map.insert(i.source_id, id);
        s.identities += 1;
    }
    let mut snippet_map: HashMap<Id, Id> = HashMap::new();
    for sn in &set.snippets {
        if let Some(e) = existing
            .snippets
            .iter()
            .find(|e| e.name == sn.name && e.script == sn.script)
        {
            snippet_map.insert(sn.source_id, e.id);
            continue;
        }
        let snippet = Snippet {
            id: Id::nil(),
            name: sn.name.clone(),
            script: sn.script.clone(),
            description: sn.description.clone(),
            tags: sn.tags.clone(),
        };
        let id = save(&ws, target, snippet, SecretUpdate::Keep).await?;
        snippet_map.insert(sn.source_id, id);
        s.snippets += 1;
    }

    // Groups the imported hosts use (and their parents).
    let mut needed: Vec<String> = Vec::new();
    for (h, a) in set.hosts.iter().zip(&actions) {
        if *a == Action::Skip {
            continue;
        }
        let mut current = h.group.clone();
        while let Some(k) = current {
            if needed.contains(&k) {
                break;
            }
            current = set
                .groups
                .iter()
                .find(|g| g.key == k)
                .and_then(|g| g.parent.clone());
            needed.push(k);
        }
    }
    let mut groups = existing.groups.clone();
    let any_host = actions.iter().any(|a| *a != Action::Skip);
    let base = match base_group
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        Some(name) if any_host => {
            let new = NewGroup {
                name,
                parent: None,
                color: None,
                settings: HostSettings::default(),
            };
            Some(find_or_create(&ws, target, &mut groups, new, &mut s).await?)
        }
        _ => None,
    };
    let mut group_map: HashMap<String, Id> = HashMap::new();
    for g in &set.groups {
        if !needed.contains(&g.key) {
            continue;
        }
        let parent = match &g.parent {
            Some(p) => group_map.get(p).copied().or(base),
            None => base,
        };
        let settings = remap(&g.settings, &key_map, &identity_map, &snippet_map);
        let new = NewGroup {
            name: &g.name,
            parent,
            color: g.color.clone(),
            settings,
        };
        let id = find_or_create(&ws, target, &mut groups, new, &mut s).await?;
        group_map.insert(g.key.clone(), id);
    }

    // Hosts.
    let mut labels: Vec<String> = existing.hosts.iter().map(|h| h.label.clone()).collect();
    let mut key_files: HashMap<String, Result<Id, String>> = HashMap::new();
    let mut host_map: HashMap<Id, Id> = HashMap::new();
    let mut jumps: Vec<(Id, Vec<Id>)> = Vec::new();
    for (h, action) in set.hosts.iter().zip(actions.iter().copied()) {
        if action == Action::Skip {
            s.skipped += 1;
            continue;
        }
        let mut settings = h
            .settings
            .as_ref()
            .map(|st| remap(st, &key_map, &identity_map, &snippet_map))
            .unwrap_or_default();
        if h.port.is_some() {
            settings.port = h.port;
        }
        if h.username.is_some() {
            settings.username = h.username.clone();
        }
        if h.proxy.is_some() {
            settings.proxy = h.proxy.clone();
        }
        let mut notes = h.notes.clone();
        if let Some(path) = &h.key_file {
            if !key_files.contains_key(path) {
                let res =
                    key_from_file(&ws, target, path, home.as_deref(), &mut keys, &mut s).await;
                key_files.insert(path.clone(), res);
            }
            match &key_files[path] {
                Ok(id) => settings.key_id = Some(*id),
                Err(e) => {
                    s.warnings.push(
                        t!(
                            "import_export.warn.key_file",
                            name = h.label.clone(),
                            path = path.clone(),
                            error = e.clone()
                        )
                        .to_string(),
                    );
                    if !notes.is_empty() {
                        notes.push('\n');
                    }
                    notes.push_str(&t!("import_export.note.key_file", path = path.clone()));
                }
            }
        }
        let group_id = h
            .group
            .as_ref()
            .and_then(|k| group_map.get(k).copied())
            .or(base);
        let secret = |old: Option<HostSecret>| {
            if h.password.is_none() && h.proxy_password.is_none() {
                return SecretUpdate::Keep;
            }
            let mut sec = old.unwrap_or_default();
            if h.password.is_some() {
                sec.password = h.password.clone();
            }
            if h.proxy_password.is_some() {
                sec.proxy_password = h.proxy_password.clone();
            }
            SecretUpdate::Set(sec)
        };
        let source_jumps = h
            .settings
            .as_ref()
            .and_then(|st| st.jump_host_ids.clone())
            .unwrap_or_default();
        let new_id = match action {
            Action::Create | Action::Copy => {
                let label = if action == Action::Copy {
                    unique_label(&h.label, &labels)
                } else {
                    h.label.clone()
                };
                labels.push(label.clone());
                let host = Host {
                    id: Id::nil(),
                    label,
                    address: h.address.clone(),
                    group_id,
                    tags: h.tags.clone(),
                    settings,
                    notes,
                    color: h.color.clone(),
                    os: h.os.clone(),
                    os_version: h.os_version.clone(),
                    favorite: h.favorite,
                };
                let id = save(&ws, target, host, secret(None)).await?;
                s.created += 1;
                id
            }
            Action::Update(id) => {
                let Some(mut host) = existing.hosts.iter().find(|x| x.id == id).cloned() else {
                    s.skipped += 1;
                    continue;
                };
                host.address = h.address.clone();
                if h.port.is_some() {
                    host.settings.port = h.port;
                }
                if h.username.is_some() {
                    host.settings.username = h.username.clone();
                }
                if h.proxy.is_some() {
                    host.settings.proxy = h.proxy.clone();
                }
                if settings.key_id.is_some() {
                    host.settings.key_id = settings.key_id;
                }
                if settings.identity_id.is_some() {
                    host.settings.identity_id = settings.identity_id;
                }
                if h.group.is_some() || base.is_some() {
                    host.group_id = group_id;
                }
                for t in &h.tags {
                    if !host.tags.iter().any(|x| x.eq_ignore_ascii_case(t)) {
                        host.tags.push(t.clone());
                    }
                }
                if !notes.trim().is_empty() {
                    host.notes = notes;
                }
                let old = if h.password.is_some() || h.proxy_password.is_some() {
                    ws.item_secret::<Host>(place.item(id)).await.ok()
                } else {
                    None
                };
                save(&ws, place.existing_target(), host, secret(old)).await?;
                s.updated += 1;
                id
            }
            Action::Skip => unreachable!(),
        };
        if let Some(src) = h.source_id {
            host_map.insert(src, new_id);
        }
        if !source_jumps.is_empty() {
            jumps.push((new_id, source_jumps));
        }
    }
    // Jump hosts, now that every host has its new id.
    for (id, chain) in jumps {
        let mapped: Vec<Id> = chain
            .iter()
            .filter_map(|j| host_map.get(j).copied())
            .filter(|j| *j != id)
            .collect();
        if mapped.is_empty() {
            continue;
        }
        let mut host = ws
            .get_item::<Host>(place.item(id))
            .await
            .map_err(api_error)?
            .record
            .data;
        host.settings.jump_host_ids = Some(mapped);
        save(&ws, place.existing_target(), host, SecretUpdate::Keep).await?;
    }
    Ok(s)
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// What to export.
#[derive(Debug, Clone, Copy)]
pub struct ExportRequest {
    pub place: Place,
    /// Only this group and its subgroups.
    pub group: Option<Id>,
    pub include_secrets: bool,
}

/// What an export writes.
#[derive(Debug, Clone)]
pub struct Gathered {
    pub file: ExportFile,
    pub secrets: Secrets,
    pub rows: Vec<ExportRow>,
    /// Secrets that could not be read (Use-only vaults).
    pub hidden_secrets: usize,
}

/// Gathers the items of an export.
pub async fn gather(ws: Workspace, req: ExportRequest) -> Result<Gathered, String> {
    let data = load_place(&ws, req.place).await?;
    let groups: Vec<Id> = match req.group {
        Some(root) => data.subtree(root),
        None => data.groups.iter().map(|g| g.id).collect(),
    };
    let hosts: Vec<Host> = data
        .hosts
        .iter()
        .filter(|h| req.group.is_none() || h.group_id.is_some_and(|g| groups.contains(&g)))
        .cloned()
        .collect();
    let mut file = ExportFile::new(format!("Termoak desktop {}", env!("CARGO_PKG_VERSION")));
    file.groups = data
        .groups
        .iter()
        .filter(|g| groups.contains(&g.id))
        .cloned()
        .map(|mut g| {
            // The root of a partial export becomes top level.
            if g.parent_id.is_some_and(|p| !groups.contains(&p)) {
                g.parent_id = None;
            }
            g
        })
        .collect();
    // Keys, identities and snippets: all of them, or what the hosts and
    // groups of a partial export use.
    let all_settings: Vec<&HostSettings> = hosts
        .iter()
        .map(|h| &h.settings)
        .chain(file.groups.iter().map(|g| &g.settings))
        .collect();
    let identities: Vec<Identity> = data
        .identities
        .iter()
        .filter(|i| req.group.is_none() || all_settings.iter().any(|s| s.identity_id == Some(i.id)))
        .cloned()
        .collect();
    file.keys = data
        .keys
        .iter()
        .filter(|k| {
            req.group.is_none()
                || all_settings.iter().any(|s| s.key_id == Some(k.id))
                || identities.iter().any(|i| i.key_id == Some(k.id))
        })
        .cloned()
        .collect();
    file.snippets = data
        .snippets
        .iter()
        .filter(|sn| {
            req.group.is_none()
                || all_settings
                    .iter()
                    .any(|s| s.startup_snippet_id == Some(sn.id))
        })
        .cloned()
        .collect();
    file.identities = identities;
    let mut tags: Vec<String> = hosts.iter().flat_map(|h| h.tags.clone()).collect();
    tags.sort_by_key(|t| t.to_lowercase());
    tags.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    file.tags = tags;
    let rows = hosts
        .iter()
        .map(|h| ExportRow {
            label: h.label.clone(),
            address: h.address.clone(),
            port: h.settings.port,
            username: h.settings.username.clone(),
            group: data.group_path(h.group_id),
            tags: h.tags.clone(),
            notes: h.notes.clone(),
        })
        .collect();
    file.hosts = hosts;

    let mut secrets = Secrets::default();
    let mut hidden = 0;
    if req.include_secrets {
        for h in &file.hosts {
            match ws.item_secret::<Host>(req.place.item(h.id)).await {
                Ok(sec) if sec.password.is_some() || sec.proxy_password.is_some() => {
                    secrets.hosts.insert(h.id, sec);
                }
                Ok(_) => {}
                Err(_) => hidden += 1,
            }
        }
        for i in &file.identities {
            match ws.item_secret::<Identity>(req.place.item(i.id)).await {
                Ok(sec) if sec.password.is_some() => {
                    secrets.identities.insert(i.id, sec);
                }
                Ok(_) => {}
                Err(_) => hidden += 1,
            }
        }
        for k in &file.keys {
            match ws.item_secret::<SshKey>(req.place.item(k.id)).await {
                Ok(sec) if sec.private_key.is_some() => {
                    secrets.keys.insert(k.id, sec);
                }
                Ok(_) => {}
                Err(_) => hidden += 1,
            }
        }
    }
    Ok(Gathered {
        file,
        secrets,
        rows,
        hidden_secrets: hidden,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: u128, name: &str, parent: Option<u128>, port: Option<u16>) -> Group {
        Group {
            id: Id::from_u128(id),
            name: name.into(),
            parent_id: parent.map(Id::from_u128),
            color: None,
            settings: HostSettings {
                port,
                ..Default::default()
            },
        }
    }

    #[test]
    fn existing_hosts_inherit_port_and_user() {
        let data = PlaceData {
            groups: vec![
                group(1, "Prod", None, Some(2222)),
                group(2, "Web", Some(1), None),
            ],
            identities: vec![Identity {
                id: Id::from_u128(9),
                label: "deploy".into(),
                username: "deploy".into(),
                key_id: None,
            }],
            hosts: vec![Host {
                id: Id::from_u128(5),
                label: "web".into(),
                address: "Web.Example.com".into(),
                group_id: Some(Id::from_u128(2)),
                tags: vec![],
                settings: HostSettings {
                    identity_id: Some(Id::from_u128(9)),
                    ..Default::default()
                },
                notes: String::new(),
                color: None,
                os: None,
                os_version: None,
                favorite: false,
            }],
            ..Default::default()
        };
        let e = data.existing_hosts();
        assert_eq!(
            e[0].key,
            DupKey::new("web.example.com", 2222, Some("deploy"))
        );
        assert_eq!(data.group_path(Some(Id::from_u128(2))), "Prod/Web");
        assert_eq!(
            data.subtree(Id::from_u128(1)),
            vec![Id::from_u128(1), Id::from_u128(2)]
        );
    }

    #[test]
    fn tilde_paths() {
        let home = PathBuf::from("/home/ana");
        assert_eq!(
            expand("~/.ssh/id_rsa", Some(&home)),
            PathBuf::from("/home/ana/.ssh/id_rsa")
        );
        assert_eq!(expand("C:\\k.ppk", Some(&home)), PathBuf::from("C:\\k.ppk"));
    }
}
