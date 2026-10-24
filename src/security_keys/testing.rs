//! Test doubles for the security key layers: the core's software
//! authenticator as the hardware (with a PIN, devices that come and go,
//! slow touches) and a scripted user.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use termoak_ssh::sk::soft::SoftAuthenticator;
use termoak_ssh::sk::{
    SkAssertion, SkCredential, SkError, SkErrorCode, SkMakeCredentialRequest, SkResidentCredential,
    SkSignRequest,
};
use tokio::sync::oneshot;

use super::{Authenticator, DeviceInfo, SkPurpose, SkUi, Stage, Status};

/// The software authenticator behind the hardware trait.
pub struct SoftHw {
    pub soft: SoftAuthenticator,
    pub devices: Mutex<Vec<DeviceInfo>>,
    /// The device's PIN (`None`: no PIN set).
    pub pin: Option<String>,
    /// The device asks for the PIN even when the key does not require
    /// verification (some keys always do for make credential).
    pub always_pin: bool,
    /// A blocking call takes this long (the touch).
    pub touch_delay: Mutex<Duration>,
    /// Devices used, in order.
    pub used: Mutex<Vec<String>>,
    pub cancels: AtomicUsize,
    pub platform: bool,
}

impl SoftHw {
    pub fn new(devices: usize) -> Arc<Self> {
        Arc::new(Self::build(devices, None))
    }

    pub fn with_pin(devices: usize, pin: &str) -> Arc<Self> {
        Arc::new(Self::build(devices, Some(pin.into())))
    }

    pub fn build(devices: usize, pin: Option<String>) -> Self {
        Self {
            soft: SoftAuthenticator::new(),
            devices: Mutex::new((0..devices).map(device).collect()),
            pin,
            always_pin: false,
            touch_delay: Mutex::new(Duration::ZERO),
            used: Mutex::new(Vec::new()),
            cancels: AtomicUsize::new(0),
            platform: false,
        }
    }

    fn check_pin(&self, needed: bool, pin: Option<&str>) -> Result<(), SkError> {
        if !(needed || self.always_pin) {
            return Ok(());
        }
        match (&self.pin, pin) {
            (None, _) => Ok(()),
            (Some(_), None) => Err(SkError::of(SkErrorCode::PinRequired)),
            (Some(p), Some(given)) if p == given => Ok(()),
            _ => Err(SkError::of(SkErrorCode::PinInvalid)),
        }
    }

    fn touch(&self, device: &DeviceInfo) {
        self.used.lock().push(device.id.clone());
        let d = *self.touch_delay.lock();
        if !d.is_zero() {
            std::thread::sleep(d);
        }
    }
}

/// Device number `n` ("Soft key n").
pub fn device(n: usize) -> DeviceInfo {
    DeviceInfo {
        id: format!("/dev/hidraw{n}"),
        label: format!("Soft key {n}"),
    }
}

impl Authenticator for SoftHw {
    fn devices(&self) -> Result<Vec<DeviceInfo>, SkError> {
        Ok(self.devices.lock().clone())
    }

    fn platform_ui(&self) -> bool {
        self.platform
    }

    fn sign(
        &self,
        device: &DeviceInfo,
        req: &SkSignRequest,
        pin: Option<&str>,
    ) -> Result<SkAssertion, SkError> {
        self.check_pin(req.require_verification, pin)?;
        self.touch(device);
        self.soft.sign_now(req)
    }

    fn make_credential(
        &self,
        device: &DeviceInfo,
        req: &SkMakeCredentialRequest,
        pin: Option<&str>,
    ) -> Result<SkCredential, SkError> {
        self.check_pin(req.require_verification, pin)?;
        self.touch(device);
        self.soft.make_credential(req)
    }

    fn list_resident(
        &self,
        device: &DeviceInfo,
        prefix: &str,
        pin: Option<&str>,
    ) -> Result<Vec<SkResidentCredential>, SkError> {
        self.check_pin(true, pin)?;
        self.used.lock().push(device.id.clone());
        Ok(self.soft.list_resident(prefix))
    }

    fn cancel(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }
}

/// What the scripted user does.
#[derive(Default)]
pub struct ScriptUi {
    /// What was shown, in order: "insert", "touch", "working", "closed",
    /// "pick:<n>", "pin", "pin:retry".
    pub events: Mutex<Vec<String>>,
    /// Answers to PIN dialogs (`None`: cancel).
    pub pins: Mutex<VecDeque<Option<String>>>,
    /// Answer to the device picker.
    pub pick: Mutex<Option<usize>>,
    /// Cancel when this stage is shown.
    pub cancel_on: Mutex<Option<Stage>>,
    /// Purposes seen.
    pub purposes: Mutex<Vec<SkPurpose>>,
}

impl ScriptUi {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<String> {
        self.events.lock().clone()
    }
}

/// Writes "closed" when the status ends.
struct CloseGuard(Arc<ScriptUi>);

impl Drop for CloseGuard {
    fn drop(&mut self) {
        self.0.events.lock().push("closed".into());
    }
}

#[async_trait]
impl SkUi for Arc<ScriptUi> {
    fn status(&self, purpose: &SkPurpose, stage: Stage) -> Status {
        self.purposes.lock().push(purpose.clone());
        self.events.lock().push(
            match stage {
                Stage::Insert => "insert",
                Stage::Touch => "touch",
                Stage::Working => "working",
            }
            .into(),
        );
        let (tx, rx) = oneshot::channel();
        let guard = CloseGuard(self.clone());
        if *self.cancel_on.lock() == Some(stage) {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                let _ = tx.send(());
            });
            Status::new(rx, guard)
        } else {
            Status::new(rx, (guard, tx))
        }
    }

    async fn pick_device(&self, _: &SkPurpose, devices: &[DeviceInfo]) -> Option<usize> {
        self.events.lock().push(format!("pick:{}", devices.len()));
        *self.pick.lock()
    }

    async fn pin(&self, _: &SkPurpose, retry: Option<SkErrorCode>) -> Option<String> {
        self.events
            .lock()
            .push(if retry.is_some() { "pin:retry" } else { "pin" }.into());
        self.pins.lock().pop_front().flatten()
    }
}
