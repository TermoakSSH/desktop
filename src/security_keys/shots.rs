//! Data for the screenshots of phase 3b (not a test: `cargo test --
//! --ignored shot_prepare` with TERMOAK_SHOT_HOME, TERMOAK_VAULT_KEY and,
//! optionally, TERMOAK_SHOT_LANG=es|en, TERMOAK_SHOT_LIGHT=1 and
//! TERMOAK_SHOT_SSHD_PORT). Security keys made with the software
//! authenticator, hosts and snippets with variables in a This device vault,
//! and `authorized_keys` next to it for a local sshd.

use termoak_client::{SaveTarget, Workspace};
use termoak_core::Id;
use termoak_core::model::{Host, HostSettings, SecretUpdate, Snippet};
use termoak_ssh::sk::soft::SoftAuthenticator;
use termoak_ssh::sk::{
    SkAlgorithm, SkKeyOptions, authorized_keys_line, key_from_credential, make_credential_request,
};

#[tokio::test]
#[ignore]
async fn shot_prepare() {
    let Ok(home) = std::env::var("TERMOAK_SHOT_HOME") else {
        eprintln!("TERMOAK_SHOT_HOME not set");
        return;
    };
    let key =
        termoak_core::crypto::MasterKey::from_base64(&std::env::var("TERMOAK_VAULT_KEY").unwrap())
            .unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let ws = Workspace::open(std::path::Path::new(&home), key).unwrap();
    let soft = SoftAuthenticator::new();
    let make = |alg, resident, verify, no_touch, device: &str, user: Option<&str>| {
        let req = make_credential_request(&SkKeyOptions {
            algorithm: alg,
            application: None,
            user: user.map(str::to_string),
            resident,
            verify_required: verify,
            no_touch_required: no_touch,
            device: Some(device.into()),
        })
        .unwrap();
        key_from_credential(&req, &soft.make_credential(&req).unwrap(), "termoak@laptop").unwrap()
    };
    let yubi = make(
        SkAlgorithm::Ed25519,
        true,
        false,
        false,
        "YubiKey 5C NFC",
        Some("alice"),
    );
    let solo = make(SkAlgorithm::EcdsaP256, false, true, true, "SoloKey 2", None);
    let plain =
        termoak_ssh::keys::generate(termoak_ssh::keys::KeyType::Ed25519, "termoak@laptop", None)
            .unwrap();
    let mut ids = Vec::new();
    for (label, m) in [
        ("YubiKey (work)", &yubi),
        ("SoloKey (servers)", &solo),
        ("Laptop", &plain),
    ] {
        let (k, secret) = crate::views::keychain_sk::key_of(label.into(), m);
        let saved = ws
            .save_item(SaveTarget::Device, k, SecretUpdate::Set(secret), None)
            .await
            .unwrap();
        ids.push(saved.record.data.id);
    }
    let lines: Vec<String> = [&yubi, &solo]
        .iter()
        .map(|m| authorized_keys_line(&m.public_openssh, m.security_key.as_ref()))
        .collect();
    std::fs::write(format!("{home}/authorized_keys"), lines.join("\n") + "\n").unwrap();
    let port: u16 = std::env::var("TERMOAK_SHOT_SSHD_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(2222);
    for (label, address, port, user, key) in [
        ("web1", "127.0.0.1", port, "root", Some(ids[0])),
        ("db-primary", "10.0.0.12", 22, "postgres", Some(ids[1])),
        (
            "staging-api",
            "api.staging.example.com",
            22,
            "deploy",
            Some(ids[2]),
        ),
    ] {
        let host = Host {
            id: Id::nil(),
            label: label.into(),
            address: address.into(),
            group_id: None,
            tags: vec![],
            settings: HostSettings {
                port: Some(port),
                username: Some(user.into()),
                key_id: key,
                ..Default::default()
            },
            notes: String::new(),
            color: None,
            os: None,
            os_version: None,
            favorite: false,
            protocol: Default::default(),
            icon: None,
        };
        ws.save_item(SaveTarget::Device, host, SecretUpdate::Keep, None)
            .await
            .unwrap();
    }
    for (name, description, script) in [
        (
            "Deploy a branch",
            "Pulls a branch and deploys it",
            "cd /srv/app && git fetch && git checkout {{prompt:Branch|main}}\n./deploy.sh --env {{select:Environment|staging|production}} --token {{secret:Deploy token}}",
        ),
        (
            "Disk usage",
            "Checks the disk of each host",
            "df -h / && echo \"{{label}} ({{host}}) checked at {{datetime}}\"",
        ),
        (
            "Restart a service",
            "",
            "sudo systemctl restart {{service}}",
        ),
        (
            "Last log lines",
            "",
            "tail -n {{prompt:Lines|50}} /var/log/syslog",
        ),
    ] {
        let s = Snippet {
            id: Id::nil(),
            name: name.into(),
            script: script.into(),
            description: description.into(),
            tags: vec![],
        };
        ws.save_item(SaveTarget::Device, s, SecretUpdate::Keep, None)
            .await
            .unwrap();
    }
    // The local sshd's host key, already trusted (no host key question).
    let hk = format!("{home}/hostkey");
    let _ = std::fs::remove_file(&hk);
    let _ = std::fs::remove_file(format!("{hk}.pub"));
    assert!(
        std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f", &hk])
            .status()
            .unwrap()
            .success()
    );
    let public = std::fs::read_to_string(format!("{hk}.pub")).unwrap();
    let parsed = termoak_ssh::keys::parse_public(&public).unwrap();
    let known = termoak_core::model::KnownHost {
        id: Id::nil(),
        host: "127.0.0.1".into(),
        port,
        key_type: "ssh-ed25519".into(),
        public_key: termoak_ssh::keys::public_openssh(&parsed),
        fingerprint: termoak_ssh::keys::fingerprint(&parsed),
    };
    ws.save_item(SaveTarget::Device, known, SecretUpdate::Keep, None)
        .await
        .unwrap();
    let mut settings = crate::state::Settings::default();
    settings.dark = std::env::var("TERMOAK_SHOT_LIGHT")
        .map(|v| v != "1")
        .unwrap_or(true);
    settings.language = std::env::var("TERMOAK_SHOT_LANG").ok();
    ws.store
        .meta_set(
            "desktop.settings",
            &serde_json::to_string(&settings).unwrap(),
        )
        .await
        .unwrap();
    println!("prepared {home}");
}
