//! Key of the local vault, read from the system keychain (macOS Keychain,
//! Windows Credential Manager, Secret Service on Linux).
//!
//! It follows the same layout as `termoak_client::vault` (service `Termoak`,
//! account `vault-key`, the `TERMOAK_VAULT_KEY` override and the `vault.key`
//! file when there is no keychain), with stricter rules about when the
//! keychain is touched and what happens when it fails:
//!
//! - When the item exists, the keychain is read exactly once per launch.
//! - The item of AceitunoakSSH (the former name) is only read when the new
//!   item is missing and there is already a database to open with it, so a
//!   fresh install never asks for it.
//! - If the item is missing but a `vault.key` file exists (the keychain was
//!   not available before), that file is the key of the database: it is
//!   used, and copied to the keychain for the next launches.
//! - If the keychain refuses (access denied, locked, no Secret Service...)
//!   and there is already a database without a `vault.key` file, opening
//!   fails instead of inventing a new key: a new key would make every saved
//!   password and key unreadable.

use std::path::Path;

use termoak_client::Workspace;
use termoak_core::crypto::{MasterKey, write_private_file};

/// Keychain item of the vault key.
const SERVICE: &str = "Termoak";
const ACCOUNT: &str = "vault-key";
/// Keychain service used before the rename to Termoak.
const LEGACY_SERVICE: &str = "AceitunoakSSH";
/// Key file used when there is no keychain.
const KEY_FILE: &str = "vault.key";
/// Databases that can already exist in the data directory (the second one
/// is renamed to the first when opened).
const DATABASES: [&str; 2] = ["termoak.db", "aceitunoak.db"];

/// Why the vault could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenError {
    pub message: String,
    /// The system keychain refused or failed: allowing access and trying
    /// again may fix it.
    pub keychain: bool,
}

impl OpenError {
    fn other(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            keychain: false,
        }
    }
}

/// Result of reading the keychain item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// The item exists (base64 key).
    Found(String),
    /// There is no item.
    Missing,
    /// The keychain did not answer: access denied, locked, no service...
    Failed(String),
}

/// What is on disk in the data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Disk {
    /// There is a `vault.key` file.
    pub key_file: bool,
    /// There is a database (with secrets sealed with some key).
    pub database: bool,
}

/// What to do after reading the keychain item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Use the key from the keychain.
    UseKeychain,
    /// Use `vault.key` and copy it to the keychain.
    UseFileAndStore,
    /// Look for the AceitunoakSSH item; otherwise generate a key. Then store it.
    TryLegacyThenGenerate,
    /// New key, stored in the keychain.
    GenerateAndStore,
    /// The keychain failed: use `vault.key`.
    UseFile,
    /// The keychain failed and there is no data yet: new key in `vault.key`.
    CreateFile,
    /// The keychain failed and the database needs the key that is in it.
    Refuse,
}

/// Decides how to get the key (pure: no keychain nor disk access).
pub fn plan(lookup: &Lookup, disk: Disk) -> Plan {
    match lookup {
        Lookup::Found(_) => Plan::UseKeychain,
        Lookup::Missing if disk.key_file => Plan::UseFileAndStore,
        Lookup::Missing if disk.database => Plan::TryLegacyThenGenerate,
        Lookup::Missing => Plan::GenerateAndStore,
        Lookup::Failed(_) if disk.key_file => Plan::UseFile,
        Lookup::Failed(_) if !disk.database => Plan::CreateFile,
        Lookup::Failed(_) => Plan::Refuse,
    }
}

/// Access to the keychain (the system one, or a fake in the tests).
pub trait Secrets {
    fn read(&self, service: &str) -> Lookup;
    fn write(&self, service: &str, value: &str) -> Result<(), String>;
}

/// The system keychain, through the `keyring` crate. It must not be used
/// from the main thread nor inside tokio.
pub struct SystemKeychain;

impl Secrets for SystemKeychain {
    fn read(&self, service: &str) -> Lookup {
        let entry = match keyring::Entry::new(service, ACCOUNT) {
            Ok(e) => e,
            Err(e) => return Lookup::Failed(e.to_string()),
        };
        match entry.get_password() {
            Ok(v) => Lookup::Found(v),
            Err(keyring::Error::NoEntry) => Lookup::Missing,
            Err(e) => Lookup::Failed(e.to_string()),
        }
    }

    fn write(&self, service: &str, value: &str) -> Result<(), String> {
        keyring::Entry::new(service, ACCOUNT)
            .and_then(|e| e.set_password(value))
            .map_err(|e| e.to_string())
    }
}

/// Opens the user's workspace (data directory and database) with the vault
/// key from `TERMOAK_VAULT_KEY` or the system keychain.
pub fn open_workspace() -> Result<Workspace, OpenError> {
    let dir = termoak_client::vault::data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| OpenError::other(e.to_string()))?;
    let key = match std::env::var("TERMOAK_VAULT_KEY") {
        Ok(v) if !v.trim().is_empty() => {
            MasterKey::from_base64(&v).map_err(|e| OpenError::other(e.to_string()))?
        }
        _ => load_key(&dir, &SystemKeychain)?,
    };
    Workspace::open(&dir, key).map_err(|e| OpenError::other(e.to_string()))
}

/// Gets (or creates) the vault key for the data directory `dir`.
pub fn load_key(dir: &Path, secrets: &dyn Secrets) -> Result<MasterKey, OpenError> {
    let key_file = dir.join(KEY_FILE);
    let disk = Disk {
        key_file: key_file.exists(),
        database: DATABASES.iter().any(|db| dir.join(db).exists()),
    };
    let lookup = secrets.read(SERVICE);
    let file_key =
        || MasterKey::load_or_create(&key_file).map_err(|e| OpenError::other(e.to_string()));
    match plan(&lookup, disk) {
        Plan::UseKeychain => {
            let Lookup::Found(value) = lookup else {
                unreachable!("UseKeychain comes from Found");
            };
            // A damaged item is not replaced: the database needs the key.
            MasterKey::from_base64(&value).map_err(|e| OpenError {
                message: t!("startup.keychain_invalid", error = e.to_string()).to_string(),
                keychain: true,
            })
        }
        Plan::UseFileAndStore => {
            let key = file_key()?;
            // The file stays: it is the key the database was sealed with.
            if let Err(e) = store(secrets, &key) {
                tracing::warn!(error = %e, "could not copy vault.key to the keychain");
            }
            Ok(key)
        }
        Plan::TryLegacyThenGenerate => {
            let legacy = match secrets.read(LEGACY_SERVICE) {
                Lookup::Found(v) => MasterKey::from_base64(&v).ok(),
                _ => None,
            };
            if legacy.is_none() {
                tracing::warn!("there is a database but no vault key; creating a new key");
            }
            persist(
                secrets,
                legacy.unwrap_or_else(MasterKey::generate),
                &key_file,
            )
        }
        Plan::GenerateAndStore => persist(secrets, MasterKey::generate(), &key_file),
        Plan::UseFile => {
            tracing::warn!("system keychain unavailable; using vault.key");
            file_key()
        }
        Plan::CreateFile => {
            if let Lookup::Failed(e) = &lookup {
                tracing::warn!(error = %e, "system keychain unavailable; using a protected file");
            }
            file_key()
        }
        Plan::Refuse => {
            let Lookup::Failed(error) = lookup else {
                unreachable!("Refuse comes from Failed");
            };
            Err(OpenError {
                message: t!("startup.keychain_denied", error = error).to_string(),
                keychain: true,
            })
        }
    }
}

/// Saves the key in the keychain and checks that it was really kept (some
/// environments ignore it).
fn store(secrets: &dyn Secrets, key: &MasterKey) -> Result<(), String> {
    let value = key.to_base64();
    secrets.write(SERVICE, &value)?;
    match secrets.read(SERVICE) {
        Lookup::Found(back) if back == *value => Ok(()),
        Lookup::Found(_) => Err("the keychain does not keep the key".into()),
        Lookup::Missing => Err("the keychain does not keep the key".into()),
        Lookup::Failed(e) => Err(e),
    }
}

/// Keeps a new key: in the keychain or, if it does not keep it, in
/// `vault.key` (so the next launch finds the same key).
fn persist(secrets: &dyn Secrets, key: MasterKey, key_file: &Path) -> Result<MasterKey, OpenError> {
    if let Err(e) = store(secrets, &key) {
        tracing::warn!(error = %e, "system keychain unavailable; using a protected file");
        write_private_file(key_file, key.to_base64().as_bytes())
            .map_err(|e| OpenError::other(e.to_string()))?;
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::PathBuf;

    use super::*;

    fn disk(key_file: bool, database: bool) -> Disk {
        Disk { key_file, database }
    }

    #[test]
    fn plans() {
        let found = Lookup::Found("k".into());
        let failed = Lookup::Failed("denied".into());
        for d in [
            disk(false, false),
            disk(true, true),
            disk(true, false),
            disk(false, true),
        ] {
            assert_eq!(plan(&found, d), Plan::UseKeychain);
        }
        assert_eq!(
            plan(&Lookup::Missing, disk(true, true)),
            Plan::UseFileAndStore
        );
        assert_eq!(
            plan(&Lookup::Missing, disk(true, false)),
            Plan::UseFileAndStore
        );
        assert_eq!(
            plan(&Lookup::Missing, disk(false, true)),
            Plan::TryLegacyThenGenerate
        );
        assert_eq!(
            plan(&Lookup::Missing, disk(false, false)),
            Plan::GenerateAndStore
        );
        assert_eq!(plan(&failed, disk(true, true)), Plan::UseFile);
        assert_eq!(plan(&failed, disk(true, false)), Plan::UseFile);
        assert_eq!(plan(&failed, disk(false, false)), Plan::CreateFile);
        assert_eq!(plan(&failed, disk(false, true)), Plan::Refuse);
    }

    /// Keychain in memory that counts the reads of each service.
    #[derive(Default)]
    struct Fake {
        items: RefCell<HashMap<String, String>>,
        reads: RefCell<HashMap<String, usize>>,
        fail: Option<String>,
        /// Accepts writes but forgets them.
        forgetful: bool,
    }

    impl Fake {
        fn with(items: &[(&str, &str)]) -> Self {
            let fake = Self::default();
            for (s, v) in items {
                fake.items.borrow_mut().insert(s.to_string(), v.to_string());
            }
            fake
        }

        fn reads(&self, service: &str) -> usize {
            self.reads.borrow().get(service).copied().unwrap_or(0)
        }
    }

    impl Secrets for Fake {
        fn read(&self, service: &str) -> Lookup {
            *self.reads.borrow_mut().entry(service.into()).or_default() += 1;
            if let Some(e) = &self.fail {
                return Lookup::Failed(e.clone());
            }
            match self.items.borrow().get(service) {
                Some(v) => Lookup::Found(v.clone()),
                None => Lookup::Missing,
            }
        }

        fn write(&self, service: &str, value: &str) -> Result<(), String> {
            if let Some(e) = &self.fail {
                return Err(e.clone());
            }
            if !self.forgetful {
                self.items.borrow_mut().insert(service.into(), value.into());
            }
            Ok(())
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("termoak-vault-key-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn touch(&self, name: &str, content: &str) {
            std::fs::write(self.0.join(name), content).unwrap();
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn b64(key: &MasterKey) -> String {
        key.to_base64().to_string()
    }

    #[test]
    fn existing_item_is_read_once() {
        let dir = TempDir::new();
        dir.touch("termoak.db", "");
        let key = MasterKey::generate();
        let fake = Fake::with(&[(SERVICE, &b64(&key)), (LEGACY_SERVICE, "x")]);
        let got = load_key(&dir.0, &fake).unwrap();
        assert_eq!(b64(&got), b64(&key));
        assert_eq!(fake.reads(SERVICE), 1);
        assert_eq!(fake.reads(LEGACY_SERVICE), 0);
        assert!(!dir.0.join(KEY_FILE).exists());
    }

    #[test]
    fn damaged_item_is_not_replaced() {
        let dir = TempDir::new();
        let fake = Fake::with(&[(SERVICE, "not a key")]);
        let err = load_key(&dir.0, &fake).unwrap_err();
        assert!(err.keychain);
        assert_eq!(fake.items.borrow()[SERVICE], "not a key");
    }

    #[test]
    fn fresh_install_never_reads_the_legacy_item() {
        let dir = TempDir::new();
        let legacy = MasterKey::generate();
        let fake = Fake::with(&[(LEGACY_SERVICE, &b64(&legacy))]);
        let got = load_key(&dir.0, &fake).unwrap();
        assert_ne!(b64(&got), b64(&legacy));
        assert_eq!(fake.reads(LEGACY_SERVICE), 0);
        assert_eq!(fake.items.borrow()[SERVICE], b64(&got));
        assert!(!dir.0.join(KEY_FILE).exists());
    }

    #[test]
    fn migrated_data_takes_the_legacy_key() {
        let dir = TempDir::new();
        dir.touch("aceitunoak.db", "");
        let legacy = MasterKey::generate();
        let fake = Fake::with(&[(LEGACY_SERVICE, &b64(&legacy))]);
        let got = load_key(&dir.0, &fake).unwrap();
        assert_eq!(b64(&got), b64(&legacy));
        assert_eq!(fake.items.borrow()[SERVICE], b64(&legacy));
        // The next launch only reads the new item.
        let again = load_key(&dir.0, &fake).unwrap();
        assert_eq!(b64(&again), b64(&legacy));
        assert_eq!(fake.reads(LEGACY_SERVICE), 1);
    }

    #[test]
    fn database_without_any_key_gets_a_new_one() {
        let dir = TempDir::new();
        dir.touch("termoak.db", "");
        let fake = Fake::default();
        let got = load_key(&dir.0, &fake).unwrap();
        assert_eq!(fake.items.borrow()[SERVICE], b64(&got));
    }

    #[test]
    fn missing_item_uses_the_key_file_and_copies_it() {
        let dir = TempDir::new();
        dir.touch("termoak.db", "");
        let key = MasterKey::generate();
        dir.touch(KEY_FILE, &b64(&key));
        let fake = Fake::default();
        let got = load_key(&dir.0, &fake).unwrap();
        assert_eq!(b64(&got), b64(&key));
        assert_eq!(fake.items.borrow()[SERVICE], b64(&key));
        assert!(dir.0.join(KEY_FILE).exists());
        assert_eq!(fake.reads(LEGACY_SERVICE), 0);
    }

    #[test]
    fn keychain_that_forgets_falls_back_to_a_file() {
        let dir = TempDir::new();
        let fake = Fake {
            forgetful: true,
            ..Fake::default()
        };
        let got = load_key(&dir.0, &fake).unwrap();
        let saved = std::fs::read_to_string(dir.0.join(KEY_FILE)).unwrap();
        assert_eq!(saved, b64(&got));
        // Next launch: same key, from the file.
        assert_eq!(b64(&load_key(&dir.0, &fake).unwrap()), b64(&got));
    }

    #[test]
    fn failing_keychain_uses_the_key_file() {
        let dir = TempDir::new();
        dir.touch("termoak.db", "");
        let key = MasterKey::generate();
        dir.touch(KEY_FILE, &b64(&key));
        let fake = Fake {
            fail: Some("locked".into()),
            ..Fake::default()
        };
        assert_eq!(b64(&load_key(&dir.0, &fake).unwrap()), b64(&key));
    }

    #[test]
    fn failing_keychain_on_a_fresh_install_creates_a_file() {
        let dir = TempDir::new();
        let fake = Fake {
            fail: Some("no secret service".into()),
            ..Fake::default()
        };
        let got = load_key(&dir.0, &fake).unwrap();
        let saved = std::fs::read_to_string(dir.0.join(KEY_FILE)).unwrap();
        assert_eq!(saved.trim(), b64(&got));
    }

    #[test]
    fn denied_keychain_never_invents_a_key_for_existing_data() {
        let dir = TempDir::new();
        dir.touch("termoak.db", "");
        let fake = Fake {
            fail: Some("user canceled".into()),
            ..Fake::default()
        };
        let err = load_key(&dir.0, &fake).unwrap_err();
        assert!(err.keychain);
        assert!(err.message.contains("user canceled"));
        assert!(!dir.0.join(KEY_FILE).exists());
        assert_eq!(fake.reads(LEGACY_SERVICE), 0);
    }
}
