//! Debug builds only: a pretend security key for screenshots and trying
//! the dialogs on a machine without one (`TERMOAK_SK_FAKE_DEVICE=1`, or `2`
//! for two devices; `TERMOAK_SK_FAKE_PIN=1` asks for a PIN first). It never
//! signs nor creates anything: every operation waits for Cancel (or gives
//! up after 60 s with a timeout), so the "touch your key" dialog can be
//! seen with a real request. Release builds do not have it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use termoak_ssh::sk::{
    SkAssertion, SkCredential, SkError, SkErrorCode, SkMakeCredentialRequest, SkResidentCredential,
    SkSignRequest,
};

use super::{Authenticator, DeviceInfo};

pub struct FakeDevice {
    count: usize,
    pin: bool,
    cancelled: AtomicBool,
}

impl FakeDevice {
    /// From the environment, when asked for.
    pub fn from_env() -> Option<Self> {
        let n: usize = std::env::var("TERMOAK_SK_FAKE_DEVICE").ok()?.parse().ok()?;
        Some(Self {
            count: n.min(3),
            pin: std::env::var("TERMOAK_SK_FAKE_PIN").is_ok_and(|v| v == "1"),
            cancelled: AtomicBool::new(false),
        })
    }

    fn wait(&self, pin: Option<&str>) -> SkError {
        if self.pin && pin.is_none() {
            return SkError::of(SkErrorCode::PinRequired);
        }
        self.cancelled.store(false, Ordering::SeqCst);
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end {
            if self.cancelled.load(Ordering::SeqCst) {
                return SkError::of(SkErrorCode::Cancelled);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        SkError::of(SkErrorCode::Timeout)
    }
}

impl Authenticator for FakeDevice {
    fn devices(&self) -> Result<Vec<DeviceInfo>, SkError> {
        let names = ["YubiKey 5C NFC", "SoloKey 2", "Nitrokey 3"];
        Ok((0..self.count)
            .map(|i| DeviceInfo {
                id: format!("fake{i}"),
                label: names[i].into(),
            })
            .collect())
    }

    fn sign(
        &self,
        _: &DeviceInfo,
        _: &SkSignRequest,
        pin: Option<&str>,
    ) -> Result<SkAssertion, SkError> {
        Err(self.wait(pin))
    }

    fn make_credential(
        &self,
        _: &DeviceInfo,
        _: &SkMakeCredentialRequest,
        pin: Option<&str>,
    ) -> Result<SkCredential, SkError> {
        Err(self.wait(pin))
    }

    fn list_resident(
        &self,
        _: &DeviceInfo,
        _: &str,
        pin: Option<&str>,
    ) -> Result<Vec<SkResidentCredential>, SkError> {
        Err(self.wait(pin))
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}
