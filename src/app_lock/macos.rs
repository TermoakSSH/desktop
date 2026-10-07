//! macOS: LocalAuthentication (`LAContext`). The policy
//! `LAPolicyDeviceOwnerAuthentication` uses Touch ID (or an Apple Watch)
//! and offers the Mac's password in the same system prompt when there is no
//! biometry, it is locked out or the user prefers the password.
//!
//! Plain Objective-C messages through `objc2` (the crates GPUI already
//! uses), without a binding crate for the framework.

use std::sync::Mutex;

use block2::RcBlock;
use futures::channel::oneshot;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use objc2_foundation::{NSError, NSString};

use super::Outcome;

// The framework with `LAContext`.
#[link(name = "LocalAuthentication", kind = "framework")]
unsafe extern "C" {}

/// `LAPolicyDeviceOwnerAuthentication`.
const POLICY_DEVICE_OWNER: isize = 2;

// `LAError` codes.
const ERROR_USER_CANCEL: isize = -2;
const ERROR_SYSTEM_CANCEL: isize = -4;
const ERROR_PASSCODE_NOT_SET: isize = -5;
const ERROR_APP_CANCEL: isize = -9;
const ERROR_NOT_INTERACTIVE: isize = -1004;

fn describe(error: &NSError) -> String {
    error.localizedDescription().to_string()
}

fn outcome_of(error: Option<&NSError>) -> Outcome {
    match error {
        None => Outcome::Failed(String::new()),
        Some(e) => match e.code() {
            ERROR_USER_CANCEL | ERROR_SYSTEM_CANCEL | ERROR_APP_CANCEL | ERROR_NOT_INTERACTIVE => {
                Outcome::Canceled
            }
            ERROR_PASSCODE_NOT_SET => Outcome::Unavailable(describe(e)),
            _ => Outcome::Failed(describe(e)),
        },
    }
}

/// Shows the system prompt with `reason` ("Termoak wants to …").
pub fn verify(reason: &str) -> oneshot::Receiver<Outcome> {
    let (tx, rx) = oneshot::channel();
    // SAFETY: `LAContext` is a plain NSObject subclass of the
    // LocalAuthentication framework (linked above); `+new` returns a
    // retained instance. `-canEvaluatePolicy:error:` takes an
    // `LAPolicy` (NSInteger) and an optional `NSError **`, and returns a
    // BOOL. `-evaluatePolicy:localizedReason:reply:` takes the policy, a
    // non-empty NSString and a block `void (^)(BOOL, NSError *)` that the
    // framework copies and calls once, on a private queue.
    unsafe {
        let context: Option<Retained<AnyObject>> = msg_send![class!(LAContext), new];
        let Some(context) = context else {
            let _ = tx.send(Outcome::Unavailable(String::new()));
            return rx;
        };
        let mut error: *mut NSError = std::ptr::null_mut();
        let can: Bool = msg_send![
            &*context,
            canEvaluatePolicy: POLICY_DEVICE_OWNER,
            error: &mut error
        ];
        if !can.as_bool() {
            let detail = error.as_ref().map(describe).unwrap_or_default();
            let _ = tx.send(Outcome::Unavailable(detail));
            return rx;
        }
        let tx = Mutex::new(Some(tx));
        // Kept alive until the answer (the framework holds it too, but
        // this does not depend on that).
        let keep = Mutex::new(Some(context.clone()));
        let reply = RcBlock::new(move |success: Bool, error: *mut NSError| {
            let outcome = if success.as_bool() {
                Outcome::Verified
            } else {
                // SAFETY: when non-null, `error` is a valid NSError for the
                // duration of the callback.
                outcome_of(error.as_ref())
            };
            if let Some(tx) = tx.lock().ok().and_then(|mut t| t.take()) {
                let _ = tx.send(outcome);
            }
            if let Ok(mut k) = keep.lock() {
                k.take();
            }
        });
        let reason = if reason.trim().is_empty() {
            NSString::from_str("Termoak")
        } else {
            NSString::from_str(reason)
        };
        let _: () = msg_send![
            &*context,
            evaluatePolicy: POLICY_DEVICE_OWNER,
            localizedReason: &*reason,
            reply: &*reply
        ];
    }
    rx
}
