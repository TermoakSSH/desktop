//! FIDO2 security keys on this computer (YubiKey, SoloKey, Nitrokey...):
//! signing for SSH, generating keys on the device and loading the keys
//! stored on it (docs/SSH-PRO.md in TermoakSSH/core).
//!
//! Three layers, so everything above the hardware is tested with the
//! core's software authenticator:
//! - [`Authenticator`]: the hardware, blocking calls. ctap-hid-fido2 over
//!   USB HID on Linux and macOS (`ctap.rs`), webauthn.dll on Windows
//!   (`webauthn.rs`, where Windows shows its own device, PIN and touch
//!   dialogs).
//! - [`SkUi`]: what the user sees: "plug in your key", the device picker,
//!   the PIN and "touch your key" with the host, user, key and purpose and
//!   a Cancel button (`overlay.rs` in the app; scripted in tests).
//! - [`SecurityKeys`]: the flow between them (one operation at a time,
//!   waiting for a device, PIN retries, cancel). It is the core's
//!   `SecurityKeySigner`, set on the workspace for local connections and
//!   answering the server's forwarding requests (`state.rs`, events).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use termoak_ssh::sk::{
    SecurityKeySigner, SkAlgorithm, SkAssertion, SkContext, SkCredential, SkError, SkErrorCode,
    SkMakeCredentialRequest, SkResidentCredential, SkSignRequest,
};
use tokio::sync::oneshot;

#[cfg(all(unix, not(any(target_os = "ios", target_os = "android"))))]
mod ctap;
#[cfg(debug_assertions)]
mod fake;
pub mod forwarding;
pub mod overlay;
#[cfg(test)]
pub(crate) mod testing;
#[cfg(windows)]
mod webauthn;

/// How long a connection waits for a security key to be plugged in (the
/// server waits 60 s for forwarded requests).
const DEVICE_WAIT: Duration = Duration::from_secs(60);
/// How often the devices are listed while waiting for one.
const DEVICE_POLL: Duration = Duration::from_millis(500);
/// Wrong PINs in a row before giving up (the device blocks the PIN after
/// a few more; a power cycle is needed after 3 in a row).
const PIN_TRIES: usize = 3;

/// A FIDO device plugged into this computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// How the backend finds it again (a HID path).
    pub id: String,
    /// What the device calls itself ("YubiKey OTP+FIDO+CCID").
    pub label: String,
}

impl DeviceInfo {
    /// The device chosen by the platform's own dialogs (Windows).
    fn platform() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
        }
    }
}

/// The hardware layer. Every call blocks (it runs on a blocking thread)
/// and fails with an [`SkError`] whose code says what happened.
pub trait Authenticator: Send + Sync + 'static {
    /// FIDO devices plugged in now.
    fn devices(&self) -> Result<Vec<DeviceInfo>, SkError>;
    /// The platform shows its own device picker, PIN and touch dialogs
    /// (Windows): no device listing nor PIN dialog of ours.
    fn platform_ui(&self) -> bool {
        false
    }
    /// CTAP2 get assertion for `req` (`pin` when user verification is
    /// required or the device asked for it).
    fn sign(
        &self,
        device: &DeviceInfo,
        req: &SkSignRequest,
        pin: Option<&str>,
    ) -> Result<SkAssertion, SkError>;
    /// CTAP2 make credential.
    fn make_credential(
        &self,
        device: &DeviceInfo,
        req: &SkMakeCredentialRequest,
        pin: Option<&str>,
    ) -> Result<SkCredential, SkError>;
    /// The resident credentials whose relying party starts with `prefix`
    /// (CTAP 2.1 credential management, which needs the PIN).
    fn list_resident(
        &self,
        device: &DeviceInfo,
        prefix: &str,
        pin: Option<&str>,
    ) -> Result<Vec<SkResidentCredential>, SkError>;
    /// Asks the device to stop waiting (best effort; called from another
    /// thread while an operation blocks).
    fn cancel(&self) {}
    /// Whether it can list the resident keys of a device.
    fn can_list_resident(&self) -> bool {
        true
    }
}

/// The hardware of this platform, if Termoak can use security keys here.
pub fn platform_authenticator() -> Option<Arc<dyn Authenticator>> {
    // Debug builds: a pretend key for screenshots (see `fake.rs`).
    #[cfg(debug_assertions)]
    if let Some(f) = fake::FakeDevice::from_env() {
        return Some(Arc::new(f));
    }
    #[cfg(all(unix, not(any(target_os = "ios", target_os = "android"))))]
    {
        Some(Arc::new(ctap::CtapAuthenticator::new()))
    }
    #[cfg(windows)]
    {
        Some(Arc::new(webauthn::WebAuthn::new()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// What an operation is for (what the dialogs show).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkPurpose {
    /// Signing an SSH login: the host, user, key and why (`purpose`:
    /// `connect` from this computer; `server_session`, `test` and `server`
    /// when the server asks).
    Sign(SkContext),
    /// Generating a key on the device.
    Create {
        application: String,
        algorithm: SkAlgorithm,
    },
    /// Reading the keys stored on the device.
    LoadResident,
}

impl SkPurpose {
    /// The server asked for it (forwarding): when no key is plugged in,
    /// the request is shown without blocking the window (another device,
    /// such as a phone, may answer it).
    pub fn forwarded(&self) -> bool {
        matches!(self, SkPurpose::Sign(c) if c.purpose != "connect" && !c.purpose.is_empty())
    }
}

/// What the device is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// No security key plugged in: waiting for one.
    Insert,
    /// Waiting for a touch on the key.
    Touch,
    /// Working without a touch (`no-touch-required` keys).
    Working,
}

/// A status shown by the [`SkUi`] while a device works. Dropping it
/// closes what was shown.
pub struct Status {
    cancel: Option<oneshot::Receiver<()>>,
    _close: Box<dyn Send>,
}

impl Status {
    /// `cancel` receives `()` when the user cancels; `close` is dropped
    /// (and, with it, whatever it holds) when the status ends.
    pub fn new(cancel: oneshot::Receiver<()>, close: impl Send + 'static) -> Self {
        Self {
            cancel: Some(cancel),
            _close: Box::new(close),
        }
    }

    /// Resolves when the user cancels (never if the dialog went away
    /// without a cancel).
    pub async fn cancelled(&mut self) {
        match self.cancel.as_mut() {
            Some(rx) => {
                if rx.await.is_err() {
                    self.cancel = None;
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    }
}

/// What the user sees and answers.
#[async_trait]
pub trait SkUi: Send + Sync + 'static {
    /// Shows a status ("plug in", "touch") until the returned [`Status`]
    /// is dropped.
    fn status(&self, purpose: &SkPurpose, stage: Stage) -> Status;
    /// Several keys are plugged in: which one (`None`: cancelled).
    async fn pick_device(&self, purpose: &SkPurpose, devices: &[DeviceInfo]) -> Option<usize>;
    /// The key's PIN (`retry`: the previous one was wrong). `None`:
    /// cancelled.
    async fn pin(&self, purpose: &SkPurpose, retry: Option<SkErrorCode>) -> Option<String>;
}

/// The security keys of this computer: the flow between the hardware and
/// the dialogs.
pub struct SecurityKeys {
    auth: Arc<dyn Authenticator>,
    ui: Arc<dyn SkUi>,
    /// One operation at a time (the dialogs show one request).
    busy: tokio::sync::Mutex<()>,
    /// Held by the blocking call that talks to the device: after a cancel
    /// the device may still be waiting for a touch (until it gives up,
    /// about 30 s), and the next operation waits for it instead of
    /// getting "channel busy".
    hw: Arc<std::sync::Mutex<()>>,
    device_wait: Duration,
    poll: Duration,
}

impl SecurityKeys {
    pub fn new(auth: Arc<dyn Authenticator>, ui: Arc<dyn SkUi>) -> Self {
        Self {
            auth,
            ui,
            busy: tokio::sync::Mutex::new(()),
            hw: Arc::new(std::sync::Mutex::new(())),
            device_wait: DEVICE_WAIT,
            poll: DEVICE_POLL,
        }
    }

    /// Shorter waits (tests).
    #[cfg(test)]
    pub fn with_waits(mut self, device_wait: Duration, poll: Duration) -> Self {
        self.device_wait = device_wait;
        self.poll = poll;
        self
    }

    /// Whether loading resident keys works here (not with webauthn.dll).
    pub fn can_list_resident(&self) -> bool {
        self.auth.can_list_resident()
    }

    /// Signs an SSH login (the core verifies the signature afterwards).
    pub async fn sign_request(&self, req: SkSignRequest) -> Result<SkAssertion, SkError> {
        let purpose = SkPurpose::Sign(req.context.clone());
        let (needs_pin, touch) = (req.require_verification, req.require_presence);
        let req = Arc::new(req);
        self.run(purpose, needs_pin, touch, move |auth, dev, pin| {
            auth.sign(dev, &req, pin)
        })
        .await
        .map(|(a, _)| a)
    }

    /// Generates a key on the device. Returns the device's answer and the
    /// label of the device used.
    pub async fn make_credential(
        &self,
        req: &SkMakeCredentialRequest,
    ) -> Result<(SkCredential, Option<String>), SkError> {
        let purpose = SkPurpose::Create {
            application: req.application.clone(),
            algorithm: req.algorithm,
        };
        let r = Arc::new(req.clone());
        self.run(
            purpose,
            req.require_verification,
            true,
            move |auth, dev, pin| auth.make_credential(dev, &r, pin),
        )
        .await
        .map(|(c, dev)| (c, label_of(&dev)))
    }

    /// The resident keys stored on the device (it needs the PIN).
    pub async fn list_resident(
        &self,
        prefix: &str,
    ) -> Result<(Vec<SkResidentCredential>, Option<String>), SkError> {
        if !self.auth.can_list_resident() {
            return Err(SkError::new(
                SkErrorCode::Unsupported,
                "this system cannot list the keys stored on a security key",
            ));
        }
        let prefix = prefix.to_string();
        self.run(
            SkPurpose::LoadResident,
            true,
            false,
            move |auth, dev, pin| auth.list_resident(dev, &prefix, pin),
        )
        .await
        .map(|(list, dev)| (list, label_of(&dev)))
    }

    /// Runs `op` on a device: chooses it (waiting for one to be plugged
    /// in, asking which one), asks for the PIN when needed, shows "touch
    /// your key" with Cancel while it works and asks again on a wrong PIN.
    async fn run<T, F>(
        &self,
        purpose: SkPurpose,
        needs_pin: bool,
        touch: bool,
        op: F,
    ) -> Result<(T, DeviceInfo), SkError>
    where
        T: Send + 'static,
        F: Fn(&dyn Authenticator, &DeviceInfo, Option<&str>) -> Result<T, SkError>
            + Send
            + Sync
            + 'static,
    {
        let _busy = self.busy.lock().await;
        let platform = self.auth.platform_ui();
        let device = if platform {
            DeviceInfo::platform()
        } else {
            self.choose_device(&purpose).await?
        };
        let mut pin = None;
        if needs_pin && !platform {
            pin = Some(self.ask_pin(&purpose, None).await?);
        }
        let op = Arc::new(op);
        let mut wrong = 0;
        loop {
            let stage = if touch { Stage::Touch } else { Stage::Working };
            let mut status = self.ui.status(&purpose, stage);
            let job = {
                let (auth, hw, op, dev, pin) = (
                    self.auth.clone(),
                    self.hw.clone(),
                    op.clone(),
                    device.clone(),
                    pin.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    let _hw = hw.lock().unwrap_or_else(|e| e.into_inner());
                    op(auth.as_ref(), &dev, pin.as_deref())
                })
            };
            let res = tokio::select! {
                r = job => r.unwrap_or_else(|e| Err(SkError::new(SkErrorCode::Failed, e.to_string()))),
                _ = status.cancelled() => {
                    self.auth.cancel();
                    return Err(SkError::of(SkErrorCode::Cancelled));
                }
            };
            drop(status);
            match res {
                Err(e) if !platform && e.code == SkErrorCode::PinRequired && pin.is_none() => {
                    pin = Some(self.ask_pin(&purpose, None).await?);
                }
                Err(e)
                    if !platform
                        && e.code == SkErrorCode::PinInvalid
                        && !pin_blocked(&e)
                        && wrong + 1 < PIN_TRIES =>
                {
                    wrong += 1;
                    pin = Some(
                        self.ask_pin(&purpose, Some(SkErrorCode::PinInvalid))
                            .await?,
                    );
                }
                other => return other.map(|v| (v, device)),
            }
        }
    }

    async fn ask_pin(
        &self,
        purpose: &SkPurpose,
        retry: Option<SkErrorCode>,
    ) -> Result<String, SkError> {
        match self.ui.pin(purpose, retry).await {
            Some(p) if !p.is_empty() => Ok(p),
            Some(_) => Err(SkError::of(SkErrorCode::PinRequired)),
            None => Err(SkError::of(SkErrorCode::Cancelled)),
        }
    }

    /// The device to use: the only one plugged in, the one chosen among
    /// several, or the first one plugged in while waiting.
    async fn choose_device(&self, purpose: &SkPurpose) -> Result<DeviceInfo, SkError> {
        let mut devices = self.list_devices().await?;
        if devices.is_empty() {
            let mut status = self.ui.status(purpose, Stage::Insert);
            let deadline = Instant::now() + self.device_wait;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(self.poll) => {}
                    _ = status.cancelled() => return Err(SkError::of(SkErrorCode::Cancelled)),
                }
                devices = self.list_devices().await?;
                if !devices.is_empty() {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(SkError::new(
                        SkErrorCode::NoDevice,
                        "no security key is plugged into this computer",
                    ));
                }
            }
        }
        if devices.len() == 1 {
            return Ok(devices.remove(0));
        }
        match self.ui.pick_device(purpose, &devices).await {
            Some(ix) if ix < devices.len() => Ok(devices.remove(ix)),
            _ => Err(SkError::of(SkErrorCode::Cancelled)),
        }
    }

    async fn list_devices(&self) -> Result<Vec<DeviceInfo>, SkError> {
        let auth = self.auth.clone();
        tokio::task::spawn_blocking(move || auth.devices())
            .await
            .unwrap_or_else(|e| Err(SkError::new(SkErrorCode::Failed, e.to_string())))
    }
}

#[async_trait]
impl SecurityKeySigner for SecurityKeys {
    async fn sign(&self, request: SkSignRequest) -> Result<SkAssertion, SkError> {
        self.sign_request(request).await
    }
}

fn label_of(dev: &DeviceInfo) -> Option<String> {
    let l = dev.label.trim();
    (!l.is_empty()).then(|| l.chars().take(64).collect())
}

/// A wrong PIN that blocked the PIN (the backends say "blocked"): asking
/// again is useless.
fn pin_blocked(e: &SkError) -> bool {
    e.message.contains("blocked")
}

/// The security key details of a key file (from the file itself when it
/// can be read; else what its public key says), `None` for other keys.
pub fn info_of(
    private_key: &str,
    passphrase: Option<&str>,
    public_key: &str,
) -> Option<termoak_core::model::SecurityKeyInfo> {
    match termoak_ssh::sk::parse_private(private_key, passphrase) {
        Ok(Some(k)) => Some(k.info()),
        _ => termoak_ssh::keys::parse_public(public_key)
            .ok()
            .and_then(|p| termoak_ssh::sk::public_info(&p)),
    }
}

/// COSE public key (CBOR) from a raw public key: 32 bytes for Ed25519, the
/// 65-byte uncompressed point (`04 || x || y`) for P-256. The resident
/// key listing of the backends gives raw keys; the core reads COSE.
#[cfg_attr(windows, allow(dead_code))]
pub fn cose_from_raw(algorithm: SkAlgorithm, raw: &[u8]) -> Option<Vec<u8>> {
    fn int(out: &mut Vec<u8>, v: i64) {
        if v >= 0 {
            head(out, 0, v as u64);
        } else {
            head(out, 1, (-1 - v) as u64);
        }
    }
    fn head(out: &mut Vec<u8>, major: u8, n: u64) {
        let m = major << 5;
        match n {
            0..=23 => out.push(m | n as u8),
            24..=0xff => out.extend([m | 24, n as u8]),
            _ => {
                out.push(m | 25);
                out.extend((n as u16).to_be_bytes());
            }
        }
    }
    fn bytes(out: &mut Vec<u8>, b: &[u8]) {
        head(out, 2, b.len() as u64);
        out.extend_from_slice(b);
    }
    let mut out = Vec::new();
    match algorithm {
        SkAlgorithm::Ed25519 if raw.len() == 32 => {
            head(&mut out, 5, 4);
            int(&mut out, 1);
            int(&mut out, 1); // kty OKP
            int(&mut out, 3);
            int(&mut out, -8); // alg EdDSA
            int(&mut out, -1);
            int(&mut out, 6); // crv Ed25519
            int(&mut out, -2);
            bytes(&mut out, raw);
        }
        SkAlgorithm::EcdsaP256 if raw.len() == 65 && raw[0] == 4 => {
            head(&mut out, 5, 5);
            int(&mut out, 1);
            int(&mut out, 2); // kty EC2
            int(&mut out, 3);
            int(&mut out, -7); // alg ES256
            int(&mut out, -1);
            int(&mut out, 1); // crv P-256
            int(&mut out, -2);
            bytes(&mut out, &raw[1..33]);
            int(&mut out, -3);
            bytes(&mut out, &raw[33..]);
        }
        _ => return None,
    }
    Some(out)
}

/// The user's text for a security key error code (`security_key_…` or the
/// short forwarding code), with the device's own message after it when it
/// adds something (failures, unsupported).
pub fn error_text(code: &str, message: &str) -> String {
    let c = SkErrorCode::parse(code);
    let text = t!(match c {
        SkErrorCode::Required => "security_keys.error.required",
        SkErrorCode::NoDevice => "security_keys.error.no_device",
        SkErrorCode::Timeout => "security_keys.error.timeout",
        SkErrorCode::Cancelled => "security_keys.error.cancelled",
        SkErrorCode::NotFound => "security_keys.error.not_found",
        SkErrorCode::PinRequired => "security_keys.error.pin_required",
        SkErrorCode::PinInvalid => "security_keys.error.pin_invalid",
        SkErrorCode::WrongKey => "security_keys.error.wrong_key",
        SkErrorCode::Unsupported => "security_keys.error.unsupported",
        SkErrorCode::Invalid => "security_keys.error.invalid",
        SkErrorCode::Failed => "security_keys.error.failed",
    })
    .to_string();
    let detail = message.trim();
    let adds = matches!(
        c,
        SkErrorCode::Failed | SkErrorCode::Unsupported | SkErrorCode::Invalid
    ) || (c == SkErrorCode::PinInvalid && pin_blocked(&SkError::new(c, detail)));
    if adds && !detail.is_empty() {
        format!("{text} ({detail})")
    } else {
        text
    }
}

/// The security key code of an English error text of the server or the
/// core (the server's `/exec` results and session reasons carry only the
/// text), if it is one of theirs.
pub fn code_of_message(message: &str) -> Option<SkErrorCode> {
    let m = message.to_lowercase();
    const PATTERNS: &[(&str, SkErrorCode)] = &[
        (
            "none of your devices that can use security keys",
            SkErrorCode::NoDevice,
        ),
        (
            "no device with a security key is available",
            SkErrorCode::NoDevice,
        ),
        ("no security key is plugged", SkErrorCode::NoDevice),
        ("nobody confirmed the security key", SkErrorCode::Timeout),
        ("security key was not touched in time", SkErrorCode::Timeout),
        ("security key request was cancelled", SkErrorCode::Cancelled),
        (
            "is a security key and none can be used here",
            SkErrorCode::Required,
        ),
        (
            "this key is a security key and none can be used",
            SkErrorCode::Required,
        ),
        ("security key does not have this key", SkErrorCode::NotFound),
        ("security key needs its pin", SkErrorCode::PinRequired),
        ("wrong pin for the security key", SkErrorCode::PinInvalid),
        (
            "security key's signature does not match",
            SkErrorCode::WrongKey,
        ),
    ];
    PATTERNS
        .iter()
        .find(|(p, _)| m.contains(p))
        .map(|(_, c)| *c)
}

/// An error text from the server: the app's text when it is a security
/// key error, else as it came.
pub fn server_error_text(message: &str) -> String {
    // Teams security: the close reasons and codes the server sends as they
    // are (`access_expired`, `access_approval_required`...).
    if let Some(text) = crate::access::close_reason_text(message.trim()) {
        return text;
    }
    if crate::access::is_teams_code(message.trim())
        && let Some(text) = crate::i18n::api_error_text(message.trim())
    {
        return text;
    }
    match code_of_message(message) {
        Some(code) => error_text(code.api_code(), ""),
        None => message.to_string(),
    }
}

/// A server session's "connecting" message in the app's language when it
/// is the server's "Touch your security key (X) on one of your devices…";
/// any other message as it came.
pub fn session_message(message: &str) -> String {
    if let Some(rest) = message.strip_prefix("Touch your security key") {
        let key = rest
            .trim_start()
            .strip_prefix('(')
            .and_then(|r| r.split_once(')'))
            .map(|(k, _)| k.to_string());
        return match key {
            Some(k) if !k.is_empty() => t!("security_keys.session.touch_key", key = k).to_string(),
            _ => t!("security_keys.session.touch").to_string(),
        };
    }
    message.to_string()
}

/// The text of what a request is for, shown in the dialogs: "Connecting
/// from this computer", "Server session", "Connection test from the
/// server", "The server connects (commands, files or AI)".
pub fn purpose_text(purpose: &SkPurpose) -> String {
    match purpose {
        SkPurpose::Sign(c) => match c.purpose.as_str() {
            "server_session" => t!("security_keys.purpose.server_session").to_string(),
            "test" => t!("security_keys.purpose.test").to_string(),
            "server" => t!("security_keys.purpose.server").to_string(),
            _ => t!("security_keys.purpose.connect").to_string(),
        },
        SkPurpose::Create { .. } => t!("security_keys.purpose.create").to_string(),
        SkPurpose::LoadResident => t!("security_keys.purpose.load").to_string(),
    }
}

/// "deploy@web1 (10.0.0.5)" of a sign request.
pub fn target_text(c: &SkContext) -> String {
    let host = if c.host.is_empty() {
        &c.address
    } else {
        &c.host
    };
    let mut s = if c.username.is_empty() {
        host.clone()
    } else {
        format!("{}@{host}", c.username)
    };
    if !c.address.is_empty() && c.address != *host {
        s.push_str(&format!(" ({})", c.address));
    }
    s
}

/// The key of a sign request: its label and the device it lives on.
pub fn key_text(c: &SkContext) -> String {
    match c.device.as_deref().filter(|d| !d.is_empty()) {
        Some(d) if d != c.key_label => format!("{} · {d}", c.key_label),
        _ => c.key_label.clone(),
    }
}

#[cfg(test)]
mod shots;
#[cfg(test)]
mod sshd_tests;
#[cfg(test)]
mod tests;
