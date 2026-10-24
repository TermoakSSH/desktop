//! End to end with a real OpenSSH server (8.2+): keys made through the
//! desktop's flow on the software authenticator, saved in a workspace and
//! used by `Workspace::connect` (what terminals, SFTP, tunnels and "Run
//! on…" use) with the desktop's signer. Skipped without `/usr/sbin/sshd`.

use std::net::TcpListener as StdListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use termoak_client::{SaveTarget, Workspace};
use termoak_core::Id;
use termoak_core::model::{Host, HostSettings, SecretUpdate, SshKey, SshKeySecret};
use termoak_ssh::ExecOptions;
use termoak_ssh::prompt::AuthPrompter;
use termoak_ssh::sk::{
    SecurityKeySigner, SkAlgorithm, SkKeyOptions, authorized_keys_line, key_from_credential,
    make_credential_request,
};

use super::testing::{ScriptUi, SoftHw};
use super::{SecurityKeys, SkPurpose};

struct Sshd {
    child: Child,
    port: u16,
    _dir: tempfile::TempDir,
    user: String,
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_sshd(authorized: &[String]) -> Option<Sshd> {
    let sshd = PathBuf::from("/usr/sbin/sshd");
    if !sshd.exists() {
        return None;
    }
    let user = String::from_utf8(Command::new("id").arg("-un").output().ok()?.stdout)
        .ok()?
        .trim()
        .to_string();
    let dir = tempfile::tempdir().ok()?;
    let d = dir.path();
    let host_key = d.join("host_ed25519");
    let ok = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&host_key)
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    std::fs::write(d.join("authorized_keys"), authorized.join("\n") + "\n").ok()?;
    let port = StdListener::bind("127.0.0.1:0")
        .ok()?
        .local_addr()
        .ok()?
        .port();
    std::fs::create_dir_all("/run/sshd").ok();
    let cfg = d.join("sshd_config");
    std::fs::write(
        &cfg,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nPermitRootLogin yes\nMaxAuthTries 20\nLogLevel ERROR\n",
            host_key.display(),
            d.join("sshd.pid").display(),
            d.join("authorized_keys").display()
        ),
    )
    .ok()?;
    let child = Command::new(sshd)
        .args(["-D", "-e", "-f"])
        .arg(&cfg)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Some(Sshd {
                child,
                port,
                _dir: dir,
                user,
            });
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// Trusts the test server's host key; asks nothing else.
struct Trust;

#[async_trait]
impl AuthPrompter for Trust {
    async fn confirm_host_key(&self, _: &str, _: u16, _: &str, _: &str) -> bool {
        true
    }
}

/// A key made on the soft device through the desktop's flow.
async fn make(
    keys: &SecurityKeys,
    alg: SkAlgorithm,
    verify: bool,
    no_touch: bool,
) -> termoak_ssh::keys::KeyMaterial {
    let req = make_credential_request(&SkKeyOptions {
        algorithm: alg,
        application: None,
        user: None,
        resident: false,
        verify_required: verify,
        no_touch_required: no_touch,
        device: None,
    })
    .unwrap();
    let (cred, device) = keys.make_credential(&req).await.unwrap();
    let mut m = key_from_credential(&req, &cred, "sk@desktop").unwrap();
    m.security_key.as_mut().unwrap().device = device;
    m
}

async fn save_host(
    ws: &Workspace,
    port: u16,
    user: &str,
    m: &termoak_ssh::keys::KeyMaterial,
    label: &str,
) -> Id {
    let (key, secret) = crate::views::keychain_sk::key_of(label.into(), m);
    let key: SshKey = key;
    let secret: SshKeySecret = secret;
    let key = ws
        .save_item(SaveTarget::Device, key, SecretUpdate::Set(secret), None)
        .await
        .unwrap();
    let host = Host {
        id: Id::nil(),
        label: format!("sshd-{label}"),
        address: "127.0.0.1".into(),
        group_id: None,
        tags: vec![],
        settings: HostSettings {
            port: Some(port),
            username: Some(user.into()),
            key_id: Some(key.record.data.id),
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
        .unwrap()
        .record
        .data
        .id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workspace_connections_sign_with_the_desktop_flow() {
    let (hw, ui) = (SoftHw::with_pin(1, "2468"), ScriptUi::new());
    let keys = Arc::new(
        SecurityKeys::new(hw.clone(), Arc::new(ui.clone()))
            .with_waits(Duration::from_millis(300), Duration::from_millis(20)),
    );
    let ed = make(&keys, SkAlgorithm::Ed25519, false, false).await;
    let ec = make(&keys, SkAlgorithm::EcdsaP256, false, false).await;
    ui.pins.lock().push_back(Some("2468".into()));
    let uv = make(&keys, SkAlgorithm::EcdsaP256, true, false).await;
    let nt = make(&keys, SkAlgorithm::Ed25519, false, true).await;
    // The lines the key details show (with the options the flags need).
    let lines: Vec<String> = [&ed, &ec, &uv, &nt]
        .iter()
        .map(|m| authorized_keys_line(&m.public_openssh, m.security_key.as_ref()))
        .collect();
    assert!(lines[2].starts_with("verify-required "));
    assert!(lines[3].starts_with("no-touch-required "));
    let Some(sshd) = start_sshd(&lines) else {
        eprintln!("no sshd here: skipped");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let ws = Workspace::open(dir.path(), termoak_core::crypto::MasterKey::generate()).unwrap();
    // Without a signer: the security key cannot be used here.
    let h = save_host(&ws, sshd.port, &sshd.user, &ed, "ed").await;
    let e = ws
        .connect(h, Arc::new(Trust), false)
        .await
        .err()
        .expect("no signer");
    assert!(
        crate::state::api_error(e).contains(&t!("security_keys.error.required").to_string()),
        "the app's text"
    );
    ws.set_security_key_signer(Some(keys.clone() as Arc<dyn SecurityKeySigner>));
    for (m, label) in [(&ed, "ed2"), (&ec, "ec"), (&nt, "nt")] {
        ui.events.lock().clear();
        let h = save_host(&ws, sshd.port, &sshd.user, m, label).await;
        let conn = ws
            .connect(h, Arc::new(Trust), false)
            .await
            .unwrap_or_else(|e| panic!("{label}: {}", crate::state::api_error(e)));
        let out = conn
            .exec("echo sk-ok", &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(out.stdout_text().trim(), "sk-ok");
        conn.disconnect().await;
        let shown = ui.purposes.lock().last().cloned().unwrap();
        assert!(
            matches!(&shown, SkPurpose::Sign(c) if c.purpose == "connect" && c.host == format!("sshd-{label}") && c.key_label == label),
            "{shown:?}"
        );
        let expected = if label == "nt" { "working" } else { "touch" };
        assert_eq!(ui.events(), [expected, "closed"], "{label}");
    }
    // verify-required: the PIN, a wrong one first.
    ui.events.lock().clear();
    ui.pins
        .lock()
        .extend([Some("0000".into()), Some("2468".into())]);
    let h = save_host(&ws, sshd.port, &sshd.user, &uv, "uv").await;
    let conn = ws.connect(h, Arc::new(Trust), false).await.unwrap();
    conn.disconnect().await;
    assert_eq!(
        ui.events(),
        ["pin", "touch", "closed", "pin:retry", "touch", "closed"]
    );
    // Cancelled: the connection stops with the app's text, no password
    // prompt afterwards.
    *ui.cancel_on.lock() = Some(super::Stage::Touch);
    *hw.touch_delay.lock() = Duration::from_millis(300);
    let h = save_host(&ws, sshd.port, &sshd.user, &ec, "ec2").await;
    let e = ws
        .connect(h, Arc::new(Trust), false)
        .await
        .err()
        .expect("cancelled");
    assert_eq!(
        crate::state::api_error(e),
        t!("security_keys.error.cancelled").to_string()
    );
}
