//! Data for the screenshots and the live check of phase 4c (not a test:
//! `cargo test -- --ignored shot_prepare_4c` with TERMOAK_SHOT_HOME,
//! TERMOAK_VAULT_KEY, TERMOAK_SHOT_SERVER, TERMOAK_SHOT_LOGIN
//! (`email password`, read from a file the caller keeps outside the
//! repository), optionally TERMOAK_SHOT_LANG=es|en, TERMOAK_SHOT_LIGHT=1
//! and TERMOAK_SHOT_KNOWN (a file with `host port key-line`): signs in on that server
//! and trusts the test sshd's key on This device.

use termoak_client::{SaveTarget, ServerChoice, Workspace};
use termoak_core::Id;
use termoak_core::model::{KnownHost, SecretUpdate};

#[tokio::test]
#[ignore]
async fn shot_prepare_4c() {
    let (Ok(home), Ok(server), Ok(login)) = (
        std::env::var("TERMOAK_SHOT_HOME"),
        std::env::var("TERMOAK_SHOT_SERVER"),
        std::env::var("TERMOAK_SHOT_LOGIN"),
    ) else {
        eprintln!("TERMOAK_SHOT_HOME, TERMOAK_SHOT_SERVER and TERMOAK_SHOT_LOGIN are needed");
        return;
    };
    let login = std::fs::read_to_string(login).unwrap();
    let mut parts = login.split_whitespace();
    let (email, password) = (parts.next().unwrap(), parts.next().unwrap());
    let key =
        termoak_core::crypto::MasterKey::from_base64(&std::env::var("TERMOAK_VAULT_KEY").unwrap())
            .unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let ws = Workspace::open(std::path::Path::new(&home), key).unwrap();
    ws.sign_in(ServerChoice::Custom(server), email, password, None)
        .await
        .unwrap();
    if let Ok(path) = std::env::var("TERMOAK_SHOT_KNOWN") {
        let known = std::fs::read_to_string(path).unwrap();
        let known = known.trim();
        let mut p = known.splitn(3, ' ');
        let (host, port, line) = (p.next().unwrap(), p.next().unwrap(), p.next().unwrap());
        let parsed = termoak_ssh::keys::parse_public(line).unwrap();
        let k = KnownHost {
            id: Id::nil(),
            host: host.into(),
            port: port.parse().unwrap(),
            key_type: line.split(' ').next().unwrap().into(),
            public_key: termoak_ssh::keys::public_openssh(&parsed),
            fingerprint: termoak_ssh::keys::fingerprint(&parsed),
        };
        ws.save_item(SaveTarget::Device, k, SecretUpdate::Keep, None)
            .await
            .unwrap();
    }
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
