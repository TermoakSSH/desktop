//! The hardware on Linux and macOS: CTAP2 over USB HID with
//! ctap-hid-fido2 (MIT).
//!
//! - Signing: get assertion with the key handle in the allow list. The
//!   library hashes its "challenge" into the client data hash, so it gets
//!   the SSH `message` (whose SHA-256 is `client_data_hash`).
//! - `uv`: the library sends `uv=true` by default, which keys without
//!   built-in verification refuse; Termoak sends no `uv` and a PIN when
//!   the key requires verification (or the device asks for it).
//! - Resident keys: CTAP 2.1 credential management (or 2.1-PRE, which
//!   older YubiKey firmware speaks), with the PIN.
//! - Linux: the user needs read/write access to `/dev/hidraw*` (the udev
//!   rules libfido2 and the U2F packages install); Flatpak needs
//!   `--device=all`.
//!
//! Not testable without a real key: everything here. The flow above it
//! (`SecurityKeys`) is tested with the core's software authenticator.

use ctap_hid_fido2::fidokey::get_assertion::get_assertion_params::Assertion;
use ctap_hid_fido2::fidokey::{
    CredentialSupportedKeyType, GetAssertionArgsBuilder, MakeCredentialArgsBuilder,
};
use ctap_hid_fido2::public_key::PublicKeyType;
use ctap_hid_fido2::public_key_credential_user_entity::PublicKeyCredentialUserEntity;
use ctap_hid_fido2::{FidoKeyHid, HidParam, LibCfg};
use termoak_ssh::sk::{
    SkAlgorithm, SkAssertion, SkCredential, SkError, SkErrorCode, SkMakeCredentialRequest,
    SkResidentCredential, SkSignRequest,
};

use super::{Authenticator, DeviceInfo, cose_from_raw};

pub struct CtapAuthenticator;

impl CtapAuthenticator {
    pub fn new() -> Self {
        Self
    }
}

fn cfg() -> LibCfg {
    let mut c = LibCfg::init();
    // Nothing printed on stdout/stderr: the app shows its own dialogs.
    c.enable_keep_alive_msg = false;
    c.enable_log = false;
    c
}

/// Opens a device (`id` is its HID path).
fn open(device: &DeviceInfo) -> Result<FidoKeyHid, SkError> {
    let params = [HidParam::Path(device.id.clone())];
    let mut cfg = cfg();
    // A device that announces CTAP 2.1 credential management gets the
    // final commands; older ones the 2.1-PRE ones.
    if let Ok(probe) = FidoKeyHid::new(&params, &cfg)
        && let Ok(info) = probe.get_info()
    {
        cfg.use_pre_credential_management = !info
            .options
            .iter()
            .any(|(name, on)| name == "credMgmt" && *on);
    }
    FidoKeyHid::new(&params, &cfg).map_err(|e| {
        SkError::new(
            SkErrorCode::Failed,
            if cfg!(target_os = "linux") {
                format!(
                    "cannot open the security key ({e}): your user needs access to /dev/hidraw* (the udev rules of libfido2 or the U2F packages)"
                )
            } else {
                format!("cannot open the security key ({e})")
            },
        )
    })
}

/// The CTAP status in a library error ("… = 0x31 CTAP2_ERR_PIN_INVALID …").
fn ctap_status(text: &str) -> Option<u8> {
    let at = text.find("0x")?;
    let hex: String = text[at + 2..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .take(2)
        .collect();
    u8::from_str_radix(&hex, 16).ok()
}

/// A library error as a security key error.
pub(crate) fn map_error(e: impl std::fmt::Display) -> SkError {
    let text = e.to_string();
    let code = match ctap_status(&text) {
        // CTAP2_ERR_NO_CREDENTIALS, CTAP2_ERR_INVALID_CREDENTIAL
        Some(0x2E) | Some(0x22) => SkErrorCode::NotFound,
        // CTAP2_ERR_PIN_REQUIRED, CTAP2_ERR_PUAT_REQUIRED (2.1), PIN not set
        Some(0x36) | Some(0x35) => SkErrorCode::PinRequired,
        Some(0x31) | Some(0x33) => SkErrorCode::PinInvalid,
        // Blocked: the device needs a power cycle (0x34) or a reset (0x32).
        Some(0x32) | Some(0x34) => {
            return SkError::new(
                SkErrorCode::PinInvalid,
                "the PIN is blocked: unplug the key and plug it in again (or reset it)",
            );
        }
        // CTAP2_ERR_USER_ACTION_TIMEOUT, CTAP2_ERR_ACTION_TIMEOUT, CTAP1 timeout
        Some(0x2F) | Some(0x3A) | Some(0x05) => SkErrorCode::Timeout,
        // CTAP2_ERR_KEEPALIVE_CANCEL, CTAP2_ERR_OPERATION_DENIED
        Some(0x2D) | Some(0x27) => SkErrorCode::Cancelled,
        // Unsupported algorithm, option or command; key store full.
        Some(0x26) | Some(0x2B) | Some(0x2C) | Some(0x01) | Some(0x28) => SkErrorCode::Unsupported,
        _ => SkErrorCode::Failed,
    };
    SkError::new(code, text)
}

impl Authenticator for CtapAuthenticator {
    fn devices(&self) -> Result<Vec<DeviceInfo>, SkError> {
        // hidapi panics when it cannot start (no hidraw at all, some
        // sandboxes): that is "no device".
        let list =
            std::panic::catch_unwind(ctap_hid_fido2::get_fidokey_devices).unwrap_or_default();
        Ok(list
            .into_iter()
            .filter_map(|d| match d.param {
                HidParam::Path(p) => Some(DeviceInfo {
                    id: p,
                    label: if d.product_string.trim().is_empty() {
                        format!("FIDO {:04x}:{:04x}", d.vid, d.pid)
                    } else {
                        d.product_string.trim().to_string()
                    },
                }),
                HidParam::VidPid { .. } => None,
            })
            .collect())
    }

    fn sign(
        &self,
        device: &DeviceInfo,
        req: &SkSignRequest,
        pin: Option<&str>,
    ) -> Result<SkAssertion, SkError> {
        let dev = open(device)?;
        let mut args = GetAssertionArgsBuilder::new(&req.application, &req.message)
            .credential_id(&req.key_handle);
        args = match pin {
            Some(p) => args.pin(p),
            None => args.without_pin_and_uv(),
        };
        if !req.require_presence {
            args = args.without_up();
        }
        let list = dev
            .get_assertion_with_args(&args.build())
            .map_err(map_error)?;
        let a: Assertion = list
            .into_iter()
            .next()
            .ok_or_else(|| SkError::of(SkErrorCode::NotFound))?;
        if a.auth_data.len() >= 37 {
            SkAssertion::from_authenticator_data(&a.auth_data, a.signature)
        } else {
            let mut flags = 0u8;
            if a.flags.user_present_result {
                flags |= termoak_ssh::sk::FLAG_USER_PRESENCE;
            }
            if a.flags.user_verified_result {
                flags |= termoak_ssh::sk::FLAG_USER_VERIFICATION;
            }
            Ok(SkAssertion {
                flags,
                counter: a.sign_count,
                signature: a.signature,
            })
        }
    }

    fn make_credential(
        &self,
        device: &DeviceInfo,
        req: &SkMakeCredentialRequest,
        pin: Option<&str>,
    ) -> Result<SkCredential, SkError> {
        let dev = open(device)?;
        let user = PublicKeyCredentialUserEntity::new(
            Some(&req.user_id),
            Some(&req.user_name),
            Some(&req.user_name),
        );
        let key_type = match req.algorithm {
            SkAlgorithm::Ed25519 => CredentialSupportedKeyType::Ed25519,
            SkAlgorithm::EcdsaP256 => CredentialSupportedKeyType::Ecdsa256,
        };
        // The challenge only feeds the attestation, which is not checked
        // (as OpenSSH by default).
        let mut args = MakeCredentialArgsBuilder::new(&req.application, &req.client_data_hash)
            .key_type(key_type)
            .user_entity(&user);
        args = match pin {
            Some(p) => args.pin(p),
            None => args.without_pin_and_uv(),
        };
        if req.resident {
            args = args.resident_key();
        }
        let att = dev
            .make_credential_with_args(&args.build())
            .map_err(map_error)?;
        Ok(SkCredential {
            authenticator_data: att.auth_data,
            attestation_object: Vec::new(),
        })
    }

    fn list_resident(
        &self,
        device: &DeviceInfo,
        prefix: &str,
        pin: Option<&str>,
    ) -> Result<Vec<SkResidentCredential>, SkError> {
        let dev = open(device)?;
        let pin = pin.ok_or_else(|| SkError::of(SkErrorCode::PinRequired))?;
        let rps = dev
            .credential_management_enumerate_rps(Some(pin))
            .map_err(map_error)?;
        let mut out = Vec::new();
        for rp in rps {
            let app = rp.public_key_credential_rp_entity.id.clone();
            if !app.starts_with(prefix) {
                continue;
            }
            let creds = dev
                .credential_management_enumerate_credentials(Some(pin), &rp.rpid_hash)
                .map_err(map_error)?;
            for c in creds {
                let algorithm = match c.public_key.key_type {
                    PublicKeyType::Ed25519 => SkAlgorithm::Ed25519,
                    PublicKeyType::Ecdsa256 => SkAlgorithm::EcdsaP256,
                    PublicKeyType::Unknown => continue,
                };
                let Some(cose) = cose_from_raw(algorithm, &c.public_key.der) else {
                    continue;
                };
                let user = &c.public_key_credential_user_entity;
                out.push(SkResidentCredential {
                    application: app.clone(),
                    credential_id: c.public_key_credential_descriptor.id.clone(),
                    public_key_cose: cose,
                    user_id: user.id.clone(),
                    user_name: (!user.name.is_empty()).then(|| user.name.clone()),
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_errors_get_codes() {
        let code = |s: &str| map_error(s).code;
        assert_eq!(
            code("response_status err = 0x31 CTAP2_ERR_PIN_INVALID   PIN Invalid."),
            SkErrorCode::PinInvalid
        );
        assert_eq!(
            code("response_status err = 0x36 CTAP2_ERR_PIN_REQUIRED"),
            SkErrorCode::PinRequired
        );
        assert_eq!(
            code("= 0x2E CTAP2_ERR_NO_CREDENTIALS"),
            SkErrorCode::NotFound
        );
        assert_eq!(
            code("= 0x2F CTAP2_ERR_USER_ACTION_TIMEOUT"),
            SkErrorCode::Timeout
        );
        assert_eq!(
            code("= 0x2D CTAP2_ERR_KEEPALIVE_CANCEL"),
            SkErrorCode::Cancelled
        );
        assert_eq!(
            code("= 0x26 CTAP2_ERR_UNSUPPORTED_ALGORITHM"),
            SkErrorCode::Unsupported
        );
        assert_eq!(code("Failed to open device."), SkErrorCode::Failed);
        let blocked = map_error("= 0x32 CTAP2_ERR_PIN_BLOCKED");
        assert_eq!(blocked.code, SkErrorCode::PinInvalid);
        assert!(super::super::pin_blocked(&blocked));
    }

    /// Listing devices never panics, even without hidraw (this container).
    #[test]
    fn listing_without_devices() {
        assert!(CtapAuthenticator::new().devices().is_ok());
    }
}
