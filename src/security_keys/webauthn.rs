//! The hardware on Windows: the Windows WebAuthn API (webauthn.dll,
//! Windows 10 1903 or later) through the `windows` crate.
//!
//! Since Windows 10 1903 only administrators can open FIDO HID devices, so
//! USB HID libraries need admin rights; webauthn.dll does not. It accepts
//! any relying party id from a desktop app (`ssh:`, what Win32-OpenSSH and
//! libfido2 use) and shows its own dialogs: which key, its PIN and the
//! touch (so `platform_ui` is true and Termoak shows only its summary of
//! the request).
//!
//! - Signing: get assertion with the key handle in the allow list. Windows
//!   hashes the client data with SHA-256, so it gets the SSH `message`
//!   (whose SHA-256 is the `client_data_hash` the key signs).
//! - Windows always asks for a touch: `no-touch-required` keys still need
//!   one here.
//! - Resident keys cannot be listed (webauthn.dll lists only Windows
//!   Hello's own credentials): `can_list_resident` is false and the app
//!   says to use `ssh-keygen -K` and import the files.
//!
//! Compiled and checked for the Windows target from Linux; it needs a real
//! key on Windows to be tried (see NEXT.md, phase 3b).

use std::sync::Mutex;

use termoak_ssh::sk::{
    SkAlgorithm, SkAssertion, SkCredential, SkError, SkErrorCode, SkMakeCredentialRequest,
    SkResidentCredential, SkSignRequest,
};
use windows::Win32::Foundation::HWND;
use windows::Win32::Networking::WindowsWebServices::*;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
use windows::core::{GUID, HSTRING, PCWSTR};

use super::{Authenticator, DeviceInfo};

/// How long Windows waits for the user (the server waits 60 s).
const TIMEOUT_MS: u32 = 60_000;

pub struct WebAuthn {
    /// The operation in progress, to cancel it.
    cancel_id: Mutex<Option<GUID>>,
}

impl WebAuthn {
    pub fn new() -> Self {
        Self {
            cancel_id: Mutex::new(None),
        }
    }

    fn start(&self) -> GUID {
        // SAFETY: plain call into webauthn.dll; a failure leaves a nil id.
        let id = unsafe { WebAuthNGetCancellationId() }.unwrap_or_default();
        *self.cancel_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
        id
    }

    fn finish(&self) {
        *self.cancel_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// The window the system dialog belongs to.
fn parent() -> HWND {
    // SAFETY: no arguments; a null HWND makes the dialog top-level.
    unsafe { GetForegroundWindow() }
}

/// A WebAuthn error as a security key error.
fn map_error(e: windows::core::Error) -> SkError {
    // SAFETY: WebAuthNGetErrorName returns a static string for any HRESULT.
    let name = unsafe { WebAuthNGetErrorName(e.code()) };
    let name = if name.is_null() {
        String::new()
    } else {
        // SAFETY: a valid, NUL-terminated static wide string.
        unsafe { name.to_string() }.unwrap_or_default()
    };
    let code = match name.as_str() {
        // The user cancelled, or the request timed out (Windows does not
        // tell them apart).
        "NotAllowedError" | "AbortError" => SkErrorCode::Cancelled,
        "NotSupportedError" => SkErrorCode::Unsupported,
        "InvalidStateError" => SkErrorCode::NotFound,
        "ConstraintError" => SkErrorCode::PinRequired,
        _ => SkErrorCode::Failed,
    };
    SkError::new(code, format!("{name} ({e})"))
}

impl Authenticator for WebAuthn {
    fn devices(&self) -> Result<Vec<DeviceInfo>, SkError> {
        // Windows chooses the device in its own dialog.
        Ok(vec![DeviceInfo::platform()])
    }

    fn platform_ui(&self) -> bool {
        true
    }

    fn can_list_resident(&self) -> bool {
        false
    }

    fn sign(
        &self,
        _device: &DeviceInfo,
        req: &SkSignRequest,
        _pin: Option<&str>,
    ) -> Result<SkAssertion, SkError> {
        let rp = HSTRING::from(req.application.as_str());
        let mut message = req.message.clone();
        let client = WEBAUTHN_CLIENT_DATA {
            dwVersion: WEBAUTHN_CLIENT_DATA_CURRENT_VERSION,
            cbClientDataJSON: message.len() as u32,
            pbClientDataJSON: message.as_mut_ptr(),
            pwszHashAlgId: WEBAUTHN_HASH_ALGORITHM_SHA_256,
        };
        let mut handle = req.key_handle.clone();
        let mut cred = WEBAUTHN_CREDENTIAL {
            dwVersion: WEBAUTHN_CREDENTIAL_CURRENT_VERSION,
            cbId: handle.len() as u32,
            pbId: handle.as_mut_ptr(),
            pwszCredentialType: WEBAUTHN_CREDENTIAL_TYPE_PUBLIC_KEY,
        };
        let mut cancel = self.start();
        let options = WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS {
            // Version 4: what every webauthn.dll since 1903 reads.
            dwVersion: WEBAUTHN_AUTHENTICATOR_GET_ASSERTION_OPTIONS_VERSION_4,
            dwTimeoutMilliseconds: TIMEOUT_MS,
            CredentialList: WEBAUTHN_CREDENTIALS {
                cCredentials: 1,
                pCredentials: &mut cred,
            },
            dwAuthenticatorAttachment: WEBAUTHN_AUTHENTICATOR_ATTACHMENT_CROSS_PLATFORM,
            dwUserVerificationRequirement: if req.require_verification {
                WEBAUTHN_USER_VERIFICATION_REQUIREMENT_REQUIRED
            } else {
                WEBAUTHN_USER_VERIFICATION_REQUIREMENT_DISCOURAGED
            },
            pCancellationId: &mut cancel,
            ..Default::default()
        };
        // SAFETY: every pointer refers to locals that outlive the call; the
        // result is freed with WebAuthNFreeAssertion below.
        let res =
            unsafe { WebAuthNAuthenticatorGetAssertion(parent(), &rp, &client, Some(&options)) };
        self.finish();
        let ptr = res.map_err(map_error)?;
        // SAFETY: on success the pointer is a valid assertion until freed.
        let out = unsafe {
            let a = &*ptr;
            let auth =
                std::slice::from_raw_parts(a.pbAuthenticatorData, a.cbAuthenticatorData as usize)
                    .to_vec();
            let sig = std::slice::from_raw_parts(a.pbSignature, a.cbSignature as usize).to_vec();
            WebAuthNFreeAssertion(ptr);
            (auth, sig)
        };
        SkAssertion::from_authenticator_data(&out.0, out.1)
    }

    fn make_credential(
        &self,
        _device: &DeviceInfo,
        req: &SkMakeCredentialRequest,
        _pin: Option<&str>,
    ) -> Result<SkCredential, SkError> {
        let rp_id = HSTRING::from(req.application.as_str());
        let rp = WEBAUTHN_RP_ENTITY_INFORMATION {
            dwVersion: WEBAUTHN_RP_ENTITY_INFORMATION_CURRENT_VERSION,
            pwszId: PCWSTR(rp_id.as_ptr()),
            pwszName: PCWSTR(rp_id.as_ptr()),
            pwszIcon: PCWSTR::null(),
        };
        let name = HSTRING::from(req.user_name.as_str());
        let mut user_id = req.user_id.clone();
        let user = WEBAUTHN_USER_ENTITY_INFORMATION {
            dwVersion: WEBAUTHN_USER_ENTITY_INFORMATION_CURRENT_VERSION,
            cbId: user_id.len() as u32,
            pbId: user_id.as_mut_ptr(),
            pwszName: PCWSTR(name.as_ptr()),
            pwszIcon: PCWSTR::null(),
            pwszDisplayName: PCWSTR(name.as_ptr()),
        };
        let mut param = WEBAUTHN_COSE_CREDENTIAL_PARAMETER {
            dwVersion: WEBAUTHN_COSE_CREDENTIAL_PARAMETER_CURRENT_VERSION,
            pwszCredentialType: WEBAUTHN_CREDENTIAL_TYPE_PUBLIC_KEY,
            lAlg: match req.algorithm {
                SkAlgorithm::Ed25519 => -8,
                SkAlgorithm::EcdsaP256 => -7,
            },
        };
        let params = WEBAUTHN_COSE_CREDENTIAL_PARAMETERS {
            cCredentialParameters: 1,
            pCredentialParameters: &mut param,
        };
        let mut challenge = req.client_data_hash.to_vec();
        let client = WEBAUTHN_CLIENT_DATA {
            dwVersion: WEBAUTHN_CLIENT_DATA_CURRENT_VERSION,
            cbClientDataJSON: challenge.len() as u32,
            pbClientDataJSON: challenge.as_mut_ptr(),
            pwszHashAlgId: WEBAUTHN_HASH_ALGORITHM_SHA_256,
        };
        let mut cancel = self.start();
        let options = WEBAUTHN_AUTHENTICATOR_MAKE_CREDENTIAL_OPTIONS {
            // Version 3: what every webauthn.dll since 1903 reads.
            dwVersion: WEBAUTHN_AUTHENTICATOR_MAKE_CREDENTIAL_OPTIONS_VERSION_3,
            dwTimeoutMilliseconds: TIMEOUT_MS,
            dwAuthenticatorAttachment: WEBAUTHN_AUTHENTICATOR_ATTACHMENT_CROSS_PLATFORM,
            bRequireResidentKey: req.resident.into(),
            dwUserVerificationRequirement: if req.require_verification {
                WEBAUTHN_USER_VERIFICATION_REQUIREMENT_REQUIRED
            } else {
                WEBAUTHN_USER_VERIFICATION_REQUIREMENT_DISCOURAGED
            },
            dwAttestationConveyancePreference: WEBAUTHN_ATTESTATION_CONVEYANCE_PREFERENCE_NONE,
            pCancellationId: &mut cancel,
            ..Default::default()
        };
        // SAFETY: every pointer refers to locals that outlive the call; the
        // result is freed with WebAuthNFreeCredentialAttestation below.
        let res = unsafe {
            WebAuthNAuthenticatorMakeCredential(
                parent(),
                &rp,
                &user,
                &params,
                &client,
                Some(&options),
            )
        };
        self.finish();
        let ptr = res.map_err(map_error)?;
        // SAFETY: on success the pointer is a valid attestation until freed.
        let auth = unsafe {
            let a = &*ptr;
            let auth =
                std::slice::from_raw_parts(a.pbAuthenticatorData, a.cbAuthenticatorData as usize)
                    .to_vec();
            WebAuthNFreeCredentialAttestation(Some(ptr));
            auth
        };
        Ok(SkCredential {
            authenticator_data: auth,
            attestation_object: Vec::new(),
        })
    }

    fn list_resident(
        &self,
        _device: &DeviceInfo,
        _prefix: &str,
        _pin: Option<&str>,
    ) -> Result<Vec<SkResidentCredential>, SkError> {
        Err(SkError::new(
            SkErrorCode::Unsupported,
            "Windows cannot list the keys stored on a security key",
        ))
    }

    fn cancel(&self) {
        let id = *self.cancel_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(id) = id {
            // SAFETY: cancels the operation with this id, if still running.
            let _ = unsafe { WebAuthNCancelCurrentOperation(&id) };
        }
    }
}
