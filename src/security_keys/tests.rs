//! The security key flow with the software authenticator and a scripted
//! user (everything above the hardware layer).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use termoak_ssh::sk::{
    SkAlgorithm, SkContext, SkErrorCode, SkKeyOptions, key_from_credential, key_from_resident,
    make_credential_request, parse_private,
};

use super::testing::{ScriptUi, SoftHw, device};
use super::*;

fn service(hw: &Arc<SoftHw>, ui: &Arc<ScriptUi>) -> SecurityKeys {
    SecurityKeys::new(hw.clone(), Arc::new(ui.clone()))
        .with_waits(Duration::from_millis(300), Duration::from_millis(20))
}

fn options(alg: SkAlgorithm, verify: bool, resident: bool, no_touch: bool) -> SkKeyOptions {
    SkKeyOptions {
        algorithm: alg,
        application: None,
        user: None,
        resident,
        verify_required: verify,
        no_touch_required: no_touch,
        device: None,
    }
}

fn context(purpose: &str) -> SkContext {
    SkContext {
        host: "web1".into(),
        address: "10.0.0.5".into(),
        username: "deploy".into(),
        key_label: "YubiKey".into(),
        key_id: None,
        device: Some("YubiKey 5C".into()),
        purpose: purpose.into(),
        session_id: None,
    }
}

/// A key made on the soft device and its sk key (to build requests).
async fn new_key(
    keys: &SecurityKeys,
    opts: SkKeyOptions,
) -> (termoak_ssh::keys::KeyMaterial, termoak_ssh::sk::SkKey) {
    let req = make_credential_request(&opts).unwrap();
    let (cred, dev) = keys.make_credential(&req).await.unwrap();
    assert!(dev.is_some());
    let m = key_from_credential(&req, &cred, "sk@test").unwrap();
    let sk = parse_private(&m.private_openssh, None).unwrap().unwrap();
    (m, sk)
}

#[tokio::test]
async fn signs_with_the_only_key_and_the_signature_verifies() {
    for alg in [SkAlgorithm::Ed25519, SkAlgorithm::EcdsaP256] {
        let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
        let keys = service(&hw, &ui);
        let (_, sk) = new_key(&keys, options(alg, false, false, false)).await;
        ui.events.lock().clear();
        let message = b"session id + userauth request".to_vec();
        let req = sk.sign_request(message.clone(), context("connect"));
        let a = keys.sign_request(req).await.unwrap();
        sk.signature_blob(&message, &a)
            .expect("the signature verifies");
        assert_eq!(ui.events(), ["touch", "closed"]);
        assert!(matches!(ui.purposes.lock().last(), Some(SkPurpose::Sign(c)) if c.host == "web1"));
    }
}

#[tokio::test]
async fn no_touch_keys_show_working_not_touch() {
    let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
    let keys = service(&hw, &ui);
    let (m, sk) = new_key(&keys, options(SkAlgorithm::Ed25519, false, false, true)).await;
    assert!(!m.security_key.as_ref().unwrap().touch_required);
    ui.events.lock().clear();
    let req = sk.sign_request(b"m".to_vec(), context("connect"));
    assert!(!req.require_presence);
    keys.sign_request(req).await.unwrap();
    assert_eq!(ui.events(), ["working", "closed"]);
}

#[tokio::test]
async fn waits_for_a_key_to_be_plugged_in() {
    let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
    let keys = Arc::new(service(&hw, &ui));
    let (_, sk) = new_key(&keys, options(SkAlgorithm::Ed25519, false, false, false)).await;
    hw.devices.lock().clear();
    ui.events.lock().clear();
    let hw2 = hw.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        hw2.devices.lock().push(device(3));
    });
    let req = sk.sign_request(b"m".to_vec(), context("connect"));
    keys.sign_request(req).await.unwrap();
    assert_eq!(ui.events(), ["insert", "closed", "touch", "closed"]);
    assert_eq!(hw.used.lock().last().unwrap(), "/dev/hidraw3");
}

#[tokio::test]
async fn no_key_plugged_in_ends_with_no_device_and_cancel_stops_waiting() {
    let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
    let keys = service(&hw, &ui);
    let (_, sk) = new_key(&keys, options(SkAlgorithm::Ed25519, false, false, false)).await;
    hw.devices.lock().clear();
    let e = keys
        .sign_request(sk.sign_request(b"m".to_vec(), context("server_session")))
        .await
        .unwrap_err();
    assert_eq!(e.code, SkErrorCode::NoDevice);
    *ui.cancel_on.lock() = Some(Stage::Insert);
    let e = keys
        .sign_request(sk.sign_request(b"m".to_vec(), context("connect")))
        .await
        .unwrap_err();
    assert_eq!(e.code, SkErrorCode::Cancelled);
    assert_eq!(ui.events().last().unwrap(), "closed");
}

#[tokio::test]
async fn several_keys_ask_which_one() {
    let (hw, ui) = (SoftHw::new(3), ScriptUi::new());
    let keys = service(&hw, &ui);
    *ui.pick.lock() = Some(2);
    let (_, sk) = new_key(&keys, options(SkAlgorithm::EcdsaP256, false, false, false)).await;
    assert_eq!(hw.used.lock().last().unwrap(), "/dev/hidraw2");
    ui.events.lock().clear();
    *ui.pick.lock() = None;
    let e = keys
        .sign_request(sk.sign_request(b"m".to_vec(), context("connect")))
        .await
        .unwrap_err();
    assert_eq!(e.code, SkErrorCode::Cancelled);
    assert_eq!(ui.events(), ["pick:3"]);
}

#[tokio::test]
async fn verify_required_asks_the_pin_and_again_when_wrong() {
    let (hw, ui) = (SoftHw::with_pin(1, "1234"), ScriptUi::new());
    let keys = service(&hw, &ui);
    ui.pins.lock().push_back(Some("1234".into()));
    let (m, sk) = new_key(&keys, options(SkAlgorithm::Ed25519, true, false, false)).await;
    assert!(m.security_key.as_ref().unwrap().verify_required);
    ui.events.lock().clear();
    // Wrong, then right.
    ui.pins
        .lock()
        .extend([Some("0000".into()), Some("1234".into())]);
    let message = b"m".to_vec();
    let a = keys
        .sign_request(sk.sign_request(message.clone(), context("connect")))
        .await
        .unwrap();
    sk.signature_blob(&message, &a).unwrap();
    assert_eq!(
        ui.events(),
        ["pin", "touch", "closed", "pin:retry", "touch", "closed"]
    );
    // Three wrong PINs: the error, no fourth try.
    ui.events.lock().clear();
    ui.pins
        .lock()
        .extend([Some("1".into()), Some("2".into()), Some("3".into())]);
    let e = keys
        .sign_request(sk.sign_request(message.clone(), context("connect")))
        .await
        .unwrap_err();
    assert_eq!(e.code, SkErrorCode::PinInvalid);
    assert_eq!(
        ui.events().iter().filter(|e| e.starts_with("pin")).count(),
        3
    );
    // Cancelling the PIN dialog cancels.
    ui.pins.lock().push_back(None);
    let e = keys
        .sign_request(sk.sign_request(message, context("connect")))
        .await
        .unwrap_err();
    assert_eq!(e.code, SkErrorCode::Cancelled);
}

#[tokio::test]
async fn a_device_that_asks_for_its_pin_gets_it() {
    let mut hw = SoftHw::build(1, Some("4321".into()));
    hw.always_pin = true;
    let (hw, ui) = (Arc::new(hw), ScriptUi::new());
    let keys = service(&hw, &ui);
    ui.pins.lock().push_back(Some("4321".into()));
    new_key(&keys, options(SkAlgorithm::Ed25519, false, false, false)).await;
    assert_eq!(ui.events(), ["touch", "closed", "pin", "touch", "closed"]);
}

#[tokio::test]
async fn cancel_while_waiting_for_the_touch() {
    let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
    let keys = Arc::new(service(&hw, &ui));
    let (_, sk) = new_key(&keys, options(SkAlgorithm::Ed25519, false, false, false)).await;
    *hw.touch_delay.lock() = Duration::from_millis(400);
    *ui.cancel_on.lock() = Some(Stage::Touch);
    let started = std::time::Instant::now();
    let e = keys
        .sign_request(sk.sign_request(b"m".to_vec(), context("connect")))
        .await
        .unwrap_err();
    assert_eq!(e.code, SkErrorCode::Cancelled);
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "cancel does not wait for the device"
    );
    assert_eq!(hw.cancels.load(Ordering::SeqCst), 1);
    // The next request waits for the device to finish the cancelled one.
    *ui.cancel_on.lock() = None;
    *hw.touch_delay.lock() = Duration::ZERO;
    let message = b"again".to_vec();
    let a = keys
        .sign_request(sk.sign_request(message.clone(), context("connect")))
        .await
        .unwrap();
    sk.signature_blob(&message, &a).unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
}

#[tokio::test]
async fn dropping_the_request_closes_the_dialog() {
    // What the events socket does on security_key_done: the request is
    // decided elsewhere, the future is dropped and the dialog goes away.
    let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
    let keys = Arc::new(service(&hw, &ui));
    let (_, sk) = new_key(&keys, options(SkAlgorithm::Ed25519, false, false, false)).await;
    ui.events.lock().clear();
    *hw.touch_delay.lock() = Duration::from_millis(300);
    let k = keys.clone();
    let task = tokio::spawn(async move {
        k.sign_request(sk.sign_request(b"m".to_vec(), context("server_session")))
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    task.abort();
    let _ = task.await;
    assert_eq!(ui.events(), ["touch", "closed"]);
}

#[tokio::test]
async fn resident_keys_are_loaded_with_the_pin() {
    let (hw, ui) = (SoftHw::with_pin(1, "1234"), ScriptUi::new());
    let keys = service(&hw, &ui);
    let mut opts = options(SkAlgorithm::Ed25519, false, true, false);
    opts.user = Some("alice".into());
    let (made, _) = new_key(&keys, opts).await;
    ui.pins.lock().push_back(Some("1234".into()));
    let (list, dev) = keys.list_resident("ssh:").await.unwrap();
    assert_eq!(dev.as_deref(), Some("Soft key 0"));
    assert_eq!(list.len(), 1);
    let m = key_from_resident(&list[0], false, dev, "").unwrap();
    assert_eq!(m.fingerprint, made.fingerprint);
    let info = m.security_key.unwrap();
    assert!(info.resident);
    assert_eq!(info.user.as_deref(), Some("alice"));
    assert_eq!(info.device.as_deref(), Some("Soft key 0"));
    // Without the PIN: cancelled.
    ui.pins.lock().push_back(None);
    assert_eq!(
        keys.list_resident("ssh:").await.unwrap_err().code,
        SkErrorCode::Cancelled
    );
}

#[tokio::test]
async fn the_platform_shows_its_own_dialogs() {
    let mut hw = SoftHw::build(0, Some("1234".into()));
    hw.platform = true;
    let (hw, ui) = (Arc::new(hw), ScriptUi::new());
    let keys = service(&hw, &ui);
    // No device listing (none "plugged in" here), no PIN dialog of ours:
    // Windows asks for both. The soft device has no PIN check here.
    let req = make_credential_request(&options(SkAlgorithm::Ed25519, false, false, false)).unwrap();
    let (cred, dev) = keys.make_credential(&req).await.unwrap();
    assert!(dev.is_none());
    key_from_credential(&req, &cred, "").unwrap();
    assert_eq!(ui.events(), ["touch", "closed"]);
}

#[test]
fn forwarded_requests() {
    assert!(SkPurpose::Sign(context("server_session")).forwarded());
    assert!(SkPurpose::Sign(context("server")).forwarded());
    assert!(!SkPurpose::Sign(context("connect")).forwarded());
    assert!(!SkPurpose::LoadResident.forwarded());
}

/// The raw public key inside an OpenSSH sk public key.
fn raw_public(openssh: &str) -> (SkAlgorithm, Vec<u8>) {
    let b64 = openssh.split_whitespace().nth(1).unwrap();
    let blob = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .unwrap();
    let mut fields = Vec::new();
    let mut i = 0;
    while i + 4 <= blob.len() {
        let n = u32::from_be_bytes(blob[i..i + 4].try_into().unwrap()) as usize;
        fields.push(blob[i + 4..i + 4 + n].to_vec());
        i += 4 + n;
    }
    if openssh.starts_with("sk-ssh-ed25519") {
        (SkAlgorithm::Ed25519, fields[1].clone())
    } else {
        (SkAlgorithm::EcdsaP256, fields[2].clone())
    }
}

#[tokio::test]
async fn cose_keys_from_raw_keys_give_the_same_key() {
    for alg in [SkAlgorithm::Ed25519, SkAlgorithm::EcdsaP256] {
        let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
        let keys = service(&hw, &ui);
        let (made, _) = new_key(&keys, options(alg, false, true, false)).await;
        let (a, raw) = raw_public(&made.public_openssh);
        assert_eq!(a, alg);
        let cred = &hw.soft.list_resident("ssh:")[0];
        let mine = cose_from_raw(alg, &raw).unwrap();
        assert_eq!(mine, cred.public_key_cose, "same CBOR as the core");
        let mut c = cred.clone();
        c.public_key_cose = mine;
        let m = key_from_resident(&c, false, None, "").unwrap();
        assert_eq!(m.fingerprint, made.fingerprint);
    }
    assert!(cose_from_raw(SkAlgorithm::Ed25519, &[0; 31]).is_none());
    assert!(cose_from_raw(SkAlgorithm::EcdsaP256, &[0; 64]).is_none());
}

#[test]
fn texts_of_errors_and_requests() {
    assert_eq!(
        error_text("security_key_no_device", "none of your devices…"),
        t!("security_keys.error.no_device")
    );
    assert_eq!(error_text("timeout", ""), t!("security_keys.error.timeout"));
    let failed = error_text("security_key_failed", "USB error");
    assert!(failed.ends_with("(USB error)"), "{failed}");
    let c = context("server_session");
    assert_eq!(target_text(&c), "deploy@web1 (10.0.0.5)");
    assert_eq!(key_text(&c), "YubiKey · YubiKey 5C");
    assert_eq!(
        purpose_text(&SkPurpose::Sign(c)),
        t!("security_keys.purpose.server_session")
    );
}

#[test]
fn key_details_of_files() {
    // An sk file gives its details; another key none.
    let soft = termoak_ssh::sk::soft::SoftAuthenticator::new();
    let req = make_credential_request(&options(SkAlgorithm::Ed25519, true, false, true)).unwrap();
    let m = key_from_credential(&req, &soft.make_credential(&req).unwrap(), "c").unwrap();
    let info = info_of(&m.private_openssh, None, &m.public_openssh).unwrap();
    assert!(info.verify_required && !info.touch_required);
    // Only the public key (an encrypted file without its passphrase).
    let info = info_of("garbage", None, &m.public_openssh).unwrap();
    assert_eq!(info.application, "ssh:");
    let plain =
        termoak_ssh::keys::generate(termoak_ssh::keys::KeyType::Ed25519, "x", None).unwrap();
    assert!(info_of(&plain.private_openssh, None, &plain.public_openssh).is_none());
}

#[test]
fn server_texts_become_the_apps() {
    assert_eq!(
        server_error_text(
            "none of your devices that can use security keys is online: open Termoak on your phone or computer (signed in) and try again"
        ),
        t!("security_keys.error.no_device")
    );
    assert_eq!(
        server_error_text("nobody confirmed the security key in 60 seconds"),
        t!("security_keys.error.timeout")
    );
    assert_eq!(
        server_error_text("the security key request was cancelled"),
        t!("security_keys.error.cancelled")
    );
    assert_eq!(
        server_error_text("connection refused"),
        "connection refused"
    );
}

#[test]
fn server_session_messages() {
    assert_eq!(
        session_message("Touch your security key (YubiKey 5C) on one of your devices…"),
        t!("security_keys.session.touch_key", key = "YubiKey 5C")
    );
    assert_eq!(
        session_message("Touch your security key on one of your devices…"),
        t!("security_keys.session.touch")
    );
    assert_eq!(
        session_message("Connecting to web1…"),
        "Connecting to web1…"
    );
}
