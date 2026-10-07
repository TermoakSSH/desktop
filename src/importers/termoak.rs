//! Termoak JSON: the app's own export format.
//!
//! ```json
//! {
//!   "format": "termoak-export",
//!   "version": 1,
//!   "exported_at": "2026-10-07T10:00:00Z",
//!   "app": "Termoak desktop 0.5.0",
//!   "groups": [ …Group… ],
//!   "hosts": [ …Host… ],
//!   "identities": [ …Identity… ],
//!   "keys": [ …SshKey (public part)… ],
//!   "snippets": [ …Snippet… ],
//!   "tags": ["prod", "web"],
//!   "secrets": {
//!     "kdf": "argon2id", "m_cost": 19456, "t_cost": 2, "p_cost": 1,
//!     "salt": "<base64>",
//!     "cipher": "xchacha20poly1305",
//!     "data": "<base64 of [1][nonce 24][ciphertext + tag]>"
//!   }
//! }
//! ```
//!
//! The items are the `termoak-core` models as they are synced (same field
//! names), so other Termoak apps can read them. Ids are those of the
//! exporting vault: an import gives every item a new id and rewrites the
//! references (group, identity, key, jumps, startup snippet).
//!
//! `secrets` is only there when the user chose "Include passwords and
//! private keys": a JSON object `{"hosts": {id: HostSecret}, "identities":
//! {id: IdentitySecret}, "keys": {id: SshKeySecret}}` sealed with
//! XChaCha20-Poly1305 under a key derived from the passphrase with
//! Argon2id (the vault crypto of `termoak-core`, AAD `termoak-export-v1`).

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use termoak_core::Id;
use termoak_core::crypto::{MasterKey, random_bytes};
use termoak_core::model::{
    Group, Host, HostSecret, Identity, IdentitySecret, Snippet, SshKey, SshKeySecret,
};
use zeroize::Zeroize;

use super::{ImportGroup, ImportHost, ImportIdentity, ImportKey, ImportSet, ImportSnippet};

/// Value of `format`.
pub const FORMAT: &str = "termoak-export";
/// Version this app writes and the highest it reads.
pub const VERSION: u32 = 1;
const AAD: &[u8] = b"termoak-export-v1";
/// Argon2id parameters of `MasterKey::derive_from_password` (the defaults
/// of the `argon2` crate), written down in the file.
const ARGON2: (u32, u32, u32) = (19_456, 2, 1);

/// An export file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportFile {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub exported_at: String,
    #[serde(default)]
    pub app: String,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub identities: Vec<Identity>,
    #[serde(default)]
    pub keys: Vec<SshKey>,
    #[serde(default)]
    pub snippets: Vec<Snippet>,
    /// Every tag used by the hosts (informative).
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SealedSecrets>,
}

impl ExportFile {
    pub fn new(app: String) -> Self {
        Self {
            format: FORMAT.into(),
            version: VERSION,
            exported_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            app,
            groups: Vec::new(),
            hosts: Vec::new(),
            identities: Vec::new(),
            keys: Vec::new(),
            snippets: Vec::new(),
            tags: Vec::new(),
            secrets: None,
        }
    }
}

/// Secrets sealed with a passphrase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SealedSecrets {
    pub kdf: String,
    #[serde(default)]
    pub m_cost: u32,
    #[serde(default)]
    pub t_cost: u32,
    #[serde(default)]
    pub p_cost: u32,
    pub salt: String,
    pub cipher: String,
    pub data: String,
}

/// Secrets of the exported items, by their id.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub hosts: BTreeMap<Id, HostSecret>,
    #[serde(default)]
    pub identities: BTreeMap<Id, IdentitySecret>,
    #[serde(default)]
    pub keys: BTreeMap<Id, SshKeySecret>,
}

/// Why a file could not be read or opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    NotTermoak,
    NewerVersion(u32),
    Invalid(String),
    WrongPassphrase,
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileError::NotTermoak => write!(f, "{}", t!("import_export.error.not_termoak")),
            FileError::NewerVersion(v) => {
                write!(
                    f,
                    "{}",
                    t!("import_export.error.newer_version", version = v)
                )
            }
            FileError::Invalid(e) => write!(f, "{}", t!("import_export.error.invalid", error = e)),
            FileError::WrongPassphrase => {
                write!(f, "{}", t!("import_export.error.wrong_passphrase"))
            }
        }
    }
}

/// Reads an export file.
pub fn parse(text: &str) -> Result<ExportFile, FileError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| FileError::Invalid(e.to_string()))?;
    if value.get("format").and_then(|f| f.as_str()) != Some(FORMAT) {
        return Err(FileError::NotTermoak);
    }
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if version > VERSION {
        return Err(FileError::NewerVersion(version));
    }
    serde_json::from_value(value).map_err(|e| FileError::Invalid(e.to_string()))
}

/// Writes an export file (pretty JSON).
pub fn write(file: &ExportFile) -> String {
    serde_json::to_string_pretty(file).expect("the export is always serializable") + "\n"
}

/// Seals the secrets with a passphrase.
pub fn seal(secrets: &Secrets, passphrase: &str) -> Result<SealedSecrets, String> {
    let salt = random_bytes::<16>();
    let key =
        MasterKey::derive_from_password(passphrase.as_bytes(), &salt).map_err(|e| e.to_string())?;
    let mut plain = serde_json::to_vec(secrets).map_err(|e| e.to_string())?;
    let sealed = key.seal(&plain, AAD).map_err(|e| e.to_string());
    plain.zeroize();
    Ok(SealedSecrets {
        kdf: "argon2id".into(),
        m_cost: ARGON2.0,
        t_cost: ARGON2.1,
        p_cost: ARGON2.2,
        salt: B64.encode(salt),
        cipher: "xchacha20poly1305".into(),
        data: B64.encode(sealed?),
    })
}

/// Opens sealed secrets.
pub fn open(sealed: &SealedSecrets, passphrase: &str) -> Result<Secrets, FileError> {
    if sealed.kdf != "argon2id" || sealed.cipher != "xchacha20poly1305" {
        return Err(FileError::Invalid(format!(
            "{} / {}",
            sealed.kdf, sealed.cipher
        )));
    }
    let salt = B64
        .decode(&sealed.salt)
        .map_err(|e| FileError::Invalid(e.to_string()))?;
    let data = B64
        .decode(&sealed.data)
        .map_err(|e| FileError::Invalid(e.to_string()))?;
    let key = MasterKey::derive_from_password(passphrase.as_bytes(), &salt)
        .map_err(|e| FileError::Invalid(e.to_string()))?;
    let plain = key
        .open(&data, AAD)
        .map_err(|_| FileError::WrongPassphrase)?;
    serde_json::from_slice(&plain).map_err(|e| FileError::Invalid(e.to_string()))
}

/// The set of an export file, with its secrets if they were opened.
pub fn to_set(file: &ExportFile, secrets: Option<&Secrets>) -> ImportSet {
    let mut set = ImportSet::default();
    // Parents before children (and groups whose parent is not in the file
    // become top level).
    let ids: Vec<Id> = file.groups.iter().map(|g| g.id).collect();
    let mut pending: Vec<&Group> = file.groups.iter().collect();
    let mut placed: Vec<Id> = Vec::new();
    while !pending.is_empty() {
        let before = pending.len();
        pending.retain(|g| {
            let parent = g.parent_id.filter(|p| ids.contains(p) && *p != g.id);
            if parent.is_none_or(|p| placed.contains(&p)) {
                set.groups.push(ImportGroup {
                    key: g.id.to_string(),
                    name: g.name.clone(),
                    parent: parent.map(|p| p.to_string()),
                    color: g.color.clone(),
                    settings: g.settings.clone(),
                });
                placed.push(g.id);
                false
            } else {
                true
            }
        });
        if pending.len() == before {
            // A cycle: the rest goes to the top level.
            for g in pending.drain(..) {
                set.groups.push(ImportGroup {
                    key: g.id.to_string(),
                    name: g.name.clone(),
                    parent: None,
                    color: g.color.clone(),
                    settings: g.settings.clone(),
                });
            }
        }
    }
    for k in &file.keys {
        let secret = secrets.and_then(|s| s.keys.get(&k.id));
        set.keys.push(ImportKey {
            source_id: k.id,
            label: k.label.clone(),
            algorithm: k.algorithm.clone(),
            public_key: k.public_key.clone(),
            fingerprint: k.fingerprint.clone(),
            comment: k.comment.clone(),
            has_passphrase: k.has_passphrase,
            certificate: k.certificate.clone(),
            private_key: secret.and_then(|s| s.private_key.clone()),
            passphrase: secret.and_then(|s| s.passphrase.clone()),
        });
    }
    for i in &file.identities {
        set.identities.push(ImportIdentity {
            source_id: i.id,
            label: i.label.clone(),
            username: i.username.clone(),
            key: i.key_id,
            password: secrets
                .and_then(|s| s.identities.get(&i.id))
                .and_then(|s| s.password.clone()),
        });
    }
    for s in &file.snippets {
        set.snippets.push(ImportSnippet {
            source_id: s.id,
            name: s.name.clone(),
            script: s.script.clone(),
            description: s.description.clone(),
            tags: s.tags.clone(),
        });
    }
    for h in &file.hosts {
        let secret = secrets.and_then(|s| s.hosts.get(&h.id));
        let mut settings = h.settings.clone();
        let port = settings.port.take();
        let username = settings.username.take();
        let proxy = settings.proxy.take();
        set.push_host(ImportHost {
            label: h.label.clone(),
            address: h.address.clone(),
            port,
            username,
            group: h
                .group_id
                .filter(|g| ids.contains(g))
                .map(|g| g.to_string()),
            tags: h.tags.clone(),
            notes: h.notes.clone(),
            color: h.color.clone(),
            favorite: h.favorite,
            password: secret.and_then(|s| s.password.clone()),
            proxy,
            proxy_password: secret.and_then(|s| s.proxy_password.clone()),
            key_file: None,
            source_id: Some(h.id),
            settings: Some(settings),
            os: h.os.clone(),
            os_version: h.os_version.clone(),
        });
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_core::model::HostSettings;

    fn sample() -> (ExportFile, Secrets) {
        let gid = Id::from_u128(1);
        let child = Id::from_u128(2);
        let kid = Id::from_u128(3);
        let iid = Id::from_u128(4);
        let jump = Id::from_u128(5);
        let web = Id::from_u128(6);
        let sid = Id::from_u128(7);
        let mut f = ExportFile::new("Termoak desktop test".into());
        // The child first: the reader must order them.
        f.groups.push(Group {
            id: child,
            name: "Web".into(),
            parent_id: Some(gid),
            color: None,
            settings: HostSettings::default(),
        });
        f.groups.push(Group {
            id: gid,
            name: "Production".into(),
            parent_id: None,
            color: Some("#ff0000".into()),
            settings: HostSettings {
                username: Some("deploy".into()),
                ..Default::default()
            },
        });
        f.keys.push(SshKey {
            id: kid,
            label: "deploy key".into(),
            algorithm: "ssh-ed25519".into(),
            public_key: "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample deploy".into(),
            fingerprint: "SHA256:abc".into(),
            comment: "deploy".into(),
            has_passphrase: false,
            certificate: None,
        });
        f.identities.push(Identity {
            id: iid,
            label: "Deploy".into(),
            username: "deploy".into(),
            key_id: Some(kid),
        });
        f.snippets.push(Snippet {
            id: sid,
            name: "uptime".into(),
            script: "uptime".into(),
            description: String::new(),
            tags: vec![],
        });
        f.hosts.push(Host {
            id: jump,
            label: "bastion".into(),
            address: "bastion.example.com".into(),
            group_id: Some(gid),
            tags: vec!["edge".into()],
            settings: HostSettings::default(),
            notes: String::new(),
            color: None,
            os: None,
            os_version: None,
            favorite: false,
        });
        f.hosts.push(Host {
            id: web,
            label: "web-1".into(),
            address: "10.0.0.11".into(),
            group_id: Some(child),
            tags: vec!["web".into(), "prod".into()],
            settings: HostSettings {
                port: Some(2222),
                identity_id: Some(iid),
                jump_host_ids: Some(vec![jump]),
                startup_snippet_id: Some(sid),
                ..Default::default()
            },
            notes: "nginx".into(),
            color: None,
            os: Some("ubuntu".into()),
            os_version: Some("Ubuntu 24.04".into()),
            favorite: true,
        });
        f.tags = vec!["edge".into(), "prod".into(), "web".into()];
        let mut s = Secrets::default();
        s.hosts.insert(
            web,
            HostSecret {
                password: Some("hunter2".into()),
                proxy_password: None,
            },
        );
        s.keys.insert(
            kid,
            SshKeySecret {
                private_key: Some("-----BEGIN OPENSSH PRIVATE KEY-----\n…".into()),
                passphrase: None,
            },
        );
        (f, s)
    }

    #[test]
    fn round_trip_without_secrets() {
        let (f, _) = sample();
        let text = write(&f);
        assert!(text.contains("\"format\": \"termoak-export\""));
        assert!(!text.contains("secrets"));
        let back = parse(&text).unwrap();
        let set = to_set(&back, None);
        assert_eq!(set.groups.len(), 2);
        assert_eq!(set.groups[0].name, "Production");
        assert_eq!(set.groups[1].parent, Some(Id::from_u128(1).to_string()));
        assert_eq!(set.hosts.len(), 2);
        let web = &set.hosts[1];
        assert_eq!(web.port, Some(2222));
        assert_eq!(web.password, None);
        assert!(web.favorite);
        let settings = web.settings.as_ref().unwrap();
        assert_eq!(settings.jump_host_ids, Some(vec![Id::from_u128(5)]));
        assert_eq!(settings.port, None, "moved to the host fields");
        assert_eq!(set.keys[0].private_key, None);
        assert_eq!(set.identities[0].key, Some(Id::from_u128(3)));
    }

    #[test]
    fn secrets_need_the_passphrase() {
        let (mut f, s) = sample();
        f.secrets = Some(seal(&s, "correct horse").unwrap());
        let text = write(&f);
        assert!(!text.contains("hunter2"));
        let back = parse(&text).unwrap();
        let sealed = back.secrets.as_ref().unwrap();
        assert_eq!(sealed.kdf, "argon2id");
        assert_eq!(
            open(sealed, "wrong").unwrap_err(),
            FileError::WrongPassphrase
        );
        let opened = open(sealed, "correct horse").unwrap();
        let set = to_set(&back, Some(&opened));
        assert_eq!(set.hosts[1].password.as_deref(), Some("hunter2"));
        assert!(set.keys[0].private_key.is_some());
    }

    #[test]
    fn rejects_other_files() {
        assert_eq!(parse("{\"hosts\":[]}").unwrap_err(), FileError::NotTermoak);
        assert_eq!(
            parse("{\"format\":\"termoak-export\",\"version\":9}").unwrap_err(),
            FileError::NewerVersion(9)
        );
        assert!(matches!(parse("nope"), Err(FileError::Invalid(_))));
        // A minimal file of a future app that only adds fields still reads.
        let set = to_set(
            &parse(
                "{\"format\":\"termoak-export\",\"version\":1,\"hosts\":[{\"label\":\"a\",\
                 \"address\":\"1.2.3.4\",\"new_field\":true}]}",
            )
            .unwrap(),
            None,
        );
        assert_eq!(set.hosts.len(), 1);
    }
}
