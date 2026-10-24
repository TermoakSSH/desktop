//! Signatures the server asks for (server sessions, host tests, `/exec`,
//! SFTP and the AI with a security key; docs/SSH-PRO.md §5 in the core).
//!
//! The events socket of an account declares `features=security_keys` when
//! this computer can sign, and every `security_key_request` is answered on
//! the same socket: the request goes through [`super::SecurityKeys`]
//! (which shows "touch your key" with the host, user, key and purpose) on
//! its own task, so the socket keeps reading. `security_key_done` (another
//! device answered, the server timed out or the user cancelled elsewhere)
//! drops that task, which closes the dialog.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use termoak_ssh::sk::{SecurityKeySigner, wire};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// The requests of one events socket being answered.
pub struct Forwarding {
    signer: Arc<dyn SecurityKeySigner>,
    replies: mpsc::UnboundedSender<Value>,
    inflight: HashMap<String, AbortHandle>,
}

impl Forwarding {
    /// Answers go to `replies` (sent on the socket).
    pub fn new(signer: Arc<dyn SecurityKeySigner>, replies: mpsc::UnboundedSender<Value>) -> Self {
        Self {
            signer,
            replies,
            inflight: HashMap::new(),
        }
    }

    /// Handles a server event: starts answering a request, or stops one
    /// that was decided.
    pub fn on_event(&mut self, v: &Value) {
        self.inflight.retain(|_, h| !h.is_finished());
        let id = v["request_id"].as_str().unwrap_or_default().to_string();
        if v["type"] == wire::REQUEST {
            let (signer, replies, v) = (self.signer.clone(), self.replies.clone(), v.clone());
            let task = tokio::spawn(async move {
                if let Some(reply) =
                    termoak_client::events::answer_security_key(signer.as_ref(), &v).await
                {
                    let _ = replies.send(reply);
                }
            });
            if let Some(old) = self.inflight.insert(id, task.abort_handle()) {
                old.abort();
            }
        } else if v["type"] == wire::DONE
            && let Some(h) = self.inflight.remove(&id)
        {
            h.abort();
        }
    }

    /// Requests being answered now.
    #[cfg(test)]
    pub fn pending(&mut self) -> usize {
        self.inflight.retain(|_, h| !h.is_finished());
        self.inflight.len()
    }
}

impl Drop for Forwarding {
    fn drop(&mut self) {
        for (_, h) in self.inflight.drain() {
            h.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use termoak_ssh::sk::{
        SkAlgorithm, SkContext, SkKeyOptions, key_from_credential, make_credential_request,
        parse_private,
    };

    use super::super::testing::{ScriptUi, SoftHw};
    use super::super::{SecurityKeys, Stage};
    use super::*;

    fn setup() -> (
        Arc<SoftHw>,
        Arc<ScriptUi>,
        Arc<SecurityKeys>,
        termoak_ssh::sk::SkKey,
    ) {
        let (hw, ui) = (SoftHw::new(1), ScriptUi::new());
        let keys = Arc::new(
            SecurityKeys::new(hw.clone(), Arc::new(ui.clone()))
                .with_waits(Duration::from_millis(300), Duration::from_millis(20)),
        );
        let req = make_credential_request(&SkKeyOptions {
            algorithm: SkAlgorithm::EcdsaP256,
            application: None,
            user: None,
            resident: false,
            verify_required: false,
            no_touch_required: false,
            device: None,
        })
        .unwrap();
        let m = key_from_credential(&req, &hw.soft.make_credential(&req).unwrap(), "").unwrap();
        let sk = parse_private(&m.private_openssh, None).unwrap().unwrap();
        (hw, ui, keys, sk)
    }

    fn request(sk: &termoak_ssh::sk::SkKey, id: &str, message: &[u8]) -> Value {
        let ctx = SkContext {
            host: "web1".into(),
            address: "10.0.0.5".into(),
            username: "deploy".into(),
            key_label: "YubiKey".into(),
            key_id: Some("k1".into()),
            device: None,
            purpose: "server_session".into(),
            session_id: Some("s1".into()),
        };
        let req = sk.sign_request(message.to_vec(), ctx);
        wire::request(id, &req, 0)
    }

    #[tokio::test]
    async fn answers_the_server_with_a_signature_that_verifies() {
        let (_hw, ui, keys, sk) = setup();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut f = Forwarding::new(keys, tx);
        f.on_event(&request(&sk, "r1", b"exchange hash + request"));
        let reply = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["type"], wire::RESPONSE);
        let (id, res) = wire::parse_response(&reply).unwrap();
        assert_eq!(id, "r1");
        sk.signature_blob(b"exchange hash + request", &res.unwrap())
            .expect("the server can verify it");
        // The user saw where and why.
        let p = ui.purposes.lock()[0].clone();
        assert!(matches!(p, super::super::SkPurpose::Sign(c)
            if c.purpose == "server_session" && c.host == "web1" && c.session_id.as_deref() == Some("s1")));
    }

    #[tokio::test]
    async fn done_elsewhere_closes_the_dialog_and_sends_nothing() {
        let (hw, ui, keys, sk) = setup();
        *hw.touch_delay.lock() = Duration::from_millis(500);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut f = Forwarding::new(keys, tx);
        f.on_event(&request(&sk, "r2", b"m"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(f.pending(), 1);
        f.on_event(&wire::done("r2", "signed"));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(f.pending(), 0);
        assert_eq!(ui.events(), ["touch", "closed"]);
        assert!(
            tokio::time::timeout(Duration::from_millis(700), rx.recv())
                .await
                .is_err(),
            "no answer for a decided request"
        );
    }

    #[tokio::test]
    async fn cancelling_answers_cancelled() {
        let (hw, ui, keys, sk) = setup();
        *hw.touch_delay.lock() = Duration::from_millis(300);
        *ui.cancel_on.lock() = Some(Stage::Touch);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut f = Forwarding::new(keys, tx);
        f.on_event(&request(&sk, "r3", b"m"));
        let reply = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["error"]["code"], "cancelled");
    }

    #[tokio::test]
    async fn no_key_here_answers_no_device() {
        let (hw, _ui, keys, sk) = setup();
        hw.devices.lock().clear();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut f = Forwarding::new(keys, tx);
        f.on_event(&request(&sk, "r4", b"m"));
        let reply = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["error"]["code"], "no_device");
    }

    #[tokio::test]
    async fn other_events_are_ignored() {
        let (_hw, _ui, keys, _sk) = setup();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut f = Forwarding::new(keys, tx);
        f.on_event(&serde_json::json!({"type": "vault", "event": "changed"}));
        f.on_event(&wire::done("unknown", "timeout"));
        assert_eq!(f.pending(), 0);
        assert!(rx.try_recv().is_err());
    }
}
