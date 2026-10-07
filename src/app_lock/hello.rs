//! Windows: Windows Hello through `UserConsentVerifier` (face, fingerprint
//! or the PIN, whatever the user set up). A desktop app shows the prompt
//! over its window with `IUserConsentVerifierInterop`; without the window
//! handle, the plain WinRT call is used.
//!
//! The WinRT calls block until the user answers, so they run on a thread of
//! their own; the result comes back through a channel.

use futures::channel::oneshot;
use windows::Security::Credentials::UI::{
    UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
use windows::core::{HSTRING, factory};
use windows_future::IAsyncOperation;

use super::Outcome;

/// Handle of the window, for the prompt to show over it.
pub fn hwnd(window: &gpui::Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get()),
        _ => None,
    }
}

/// Shows the Windows Hello prompt with `reason`.
pub fn verify(reason: &str, hwnd: Option<isize>) -> oneshot::Receiver<Outcome> {
    let (tx, rx) = oneshot::channel();
    let reason = reason.to_string();
    // If the thread cannot start, `tx` is dropped and the caller gets a
    // failure.
    let _ = std::thread::Builder::new()
        .name("termoak-hello".into())
        .spawn(move || {
            let _ = tx.send(run(&HSTRING::from(reason.as_str()), hwnd));
        });
    rx
}

fn run(reason: &HSTRING, hwnd: Option<isize>) -> Outcome {
    let available = UserConsentVerifier::CheckAvailabilityAsync().and_then(|op| op.join());
    match available {
        Ok(a) if a == UserConsentVerifierAvailability::Available => {}
        // Busy (another prompt is open): it may work in a moment.
        Ok(a) if a == UserConsentVerifierAvailability::DeviceBusy => {
            return Outcome::Failed(String::new());
        }
        Ok(_) => return Outcome::Unavailable(String::new()),
        Err(e) => return Outcome::Unavailable(e.message()),
    }
    let op: windows::core::Result<IAsyncOperation<UserConsentVerificationResult>> = match hwnd {
        Some(h) => factory::<UserConsentVerifier, IUserConsentVerifierInterop>().and_then(
            // SAFETY: `h` is the handle of a live window of this process
            // (taken from GPUI just before), and the interface returns an
            // `IAsyncOperation<UserConsentVerificationResult>` for the IID
            // it is asked for.
            |interop| unsafe {
                interop.RequestVerificationForWindowAsync(HWND(h as *mut core::ffi::c_void), reason)
            },
        ),
        None => UserConsentVerifier::RequestVerificationAsync(reason),
    };
    match op.and_then(|op| op.join()) {
        Ok(r) if r == UserConsentVerificationResult::Verified => Outcome::Verified,
        Ok(r) if r == UserConsentVerificationResult::Canceled => Outcome::Canceled,
        Ok(r)
            if r == UserConsentVerificationResult::DeviceNotPresent
                || r == UserConsentVerificationResult::NotConfiguredForUser
                || r == UserConsentVerificationResult::DisabledByPolicy =>
        {
            Outcome::Unavailable(String::new())
        }
        Ok(_) => Outcome::Failed(String::new()),
        Err(e) => Outcome::Failed(e.message()),
    }
}
