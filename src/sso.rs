//! Single sign-on in the app (0.7 phase 5b, docs-next/E2EE-SSO-AUDIT.md
//! §3 and §10): the identity provider presets and the fields each one
//! needs, the form → `SsoConfig` (checked with the core's rules), the
//! texts of the SSO error codes, and the system browser + loopback flow of
//! the desktop (sign in, link an identity).

use std::sync::Arc;
use std::time::Duration;

use termoak_client::{Account, ApiClient, ClientError, ServerChoice, Workspace};
use termoak_core::Id;
use termoak_core::sso::{SsoConfig, SsoPending, SsoPreset};

/// Error codes of SSO: always the app's text.
pub const ERROR_CODES: &[&str] = &[
    "sso_required",
    "sso_not_configured",
    "sso_issuer_invalid",
    "sso_redirect_invalid",
    "sso_pkce_invalid",
    "sso_state_invalid",
    "sso_code_invalid",
    "sso_failed",
    "sso_email_missing",
    "sso_email_unverified",
    "sso_account_not_found",
    "sso_account_not_linked",
    "sso_identity_taken",
    "sso_domain_taken",
    "sso_domain_unverified",
    "sso_only_account",
];

pub fn is_sso_code(code: &str) -> bool {
    ERROR_CODES.contains(&code)
}

/// The text of an SSO error, with the team's name of `sso_required`.
pub fn error_text(code: &str, details: &serde_json::Value) -> Option<String> {
    if code == "sso_required"
        && let Some(team) = details["team_name"].as_str().filter(|t| !t.is_empty())
    {
        return Some(t!("error.sso_required_team", team = team).to_string());
    }
    crate::i18n::api_error_text(code)
}

/// The presets in the order the picker shows them.
pub const PRESETS: [SsoPreset; 5] = [
    SsoPreset::Google,
    SsoPreset::Microsoft,
    SsoPreset::Keycloak,
    SsoPreset::Authentik,
    SsoPreset::Generic,
];

pub fn preset_label(p: SsoPreset) -> String {
    match p {
        SsoPreset::Google => t!("sso.preset.google"),
        SsoPreset::Microsoft => t!("sso.preset.microsoft"),
        SsoPreset::Keycloak => t!("sso.preset.keycloak"),
        SsoPreset::Authentik => t!("sso.preset.authentik"),
        SsoPreset::Generic | SsoPreset::Unknown => t!("sso.preset.generic"),
    }
    .to_string()
}

/// What a preset asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetFields {
    /// The issuer URL is typed (Google's is fixed; Microsoft's comes from
    /// the tenant).
    pub issuer: bool,
    pub tenant: bool,
    /// Example of the issuer.
    pub issuer_example: &'static str,
    /// Default of "require a verified email" (Entra does not send it).
    pub require_email_verified: bool,
    /// Where to create the client at the provider, in words.
    pub help_key: &'static str,
}

pub fn preset_fields(p: SsoPreset) -> PresetFields {
    match p {
        SsoPreset::Google => PresetFields {
            issuer: false,
            tenant: false,
            issuer_example: "https://accounts.google.com",
            require_email_verified: true,
            help_key: "sso.help.google",
        },
        SsoPreset::Microsoft => PresetFields {
            issuer: false,
            tenant: true,
            issuer_example: "https://login.microsoftonline.com/<tenant>/v2.0",
            require_email_verified: false,
            help_key: "sso.help.microsoft",
        },
        SsoPreset::Keycloak => PresetFields {
            issuer: true,
            tenant: false,
            issuer_example: "https://sso.example.com/realms/example",
            require_email_verified: true,
            help_key: "sso.help.keycloak",
        },
        SsoPreset::Authentik => PresetFields {
            issuer: true,
            tenant: false,
            issuer_example: "https://auth.example.com/application/o/termoak/",
            require_email_verified: true,
            help_key: "sso.help.authentik",
        },
        SsoPreset::Generic | SsoPreset::Unknown => PresetFields {
            issuer: true,
            tenant: false,
            issuer_example: "https://idp.example.com",
            require_email_verified: true,
            help_key: "sso.help.generic",
        },
    }
}

/// What the form holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SsoForm {
    pub enabled: bool,
    pub preset: SsoPreset,
    pub issuer: String,
    pub tenant: String,
    pub client_id: String,
    /// Empty: keep the stored one.
    pub client_secret: String,
    /// Extra scopes, space or comma separated.
    pub scopes: String,
    pub email_claim: String,
    pub require_email_verified: bool,
    pub enforce: bool,
    pub owner_password_allowed: bool,
    pub auto_create: bool,
    pub auto_join: bool,
    pub satisfies_2fa: bool,
}

impl SsoForm {
    pub fn from_config(c: &SsoConfig) -> Self {
        Self {
            enabled: c.enabled,
            preset: c.preset,
            issuer: c.issuer.clone(),
            tenant: c.tenant.clone().unwrap_or_default(),
            client_id: c.client_id.clone(),
            client_secret: String::new(),
            scopes: c
                .scopes
                .iter()
                .filter(|s| !matches!(s.as_str(), "openid" | "email" | "profile"))
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
            email_claim: c.email_claim.clone().unwrap_or_default(),
            require_email_verified: c.require_email_verified,
            enforce: c.enforce,
            owner_password_allowed: c.owner_password_allowed,
            auto_create: c.auto_create,
            auto_join: c.auto_join,
            satisfies_2fa: c.satisfies_2fa,
        }
    }

    /// A new form for a team (SSO not set up yet).
    pub fn new_for(preset: SsoPreset) -> Self {
        Self {
            enabled: true,
            preset,
            require_email_verified: preset_fields(preset).require_email_verified,
            owner_password_allowed: true,
            auto_create: true,
            auto_join: true,
            ..Default::default()
        }
    }

    /// The settings to send, checked with the core's rules (`Err`: the
    /// text to show).
    pub fn to_config(&self, has_secret: bool) -> Result<SsoConfig, String> {
        let fields = preset_fields(self.preset);
        if self.client_id.trim().is_empty() {
            return Err(t!("sso.form.client_id_required").to_string());
        }
        if fields.tenant && self.tenant.trim().is_empty() {
            return Err(t!("sso.form.tenant_required").to_string());
        }
        if fields.issuer && self.issuer.trim().is_empty() {
            return Err(t!("sso.form.issuer_required").to_string());
        }
        if self.preset == SsoPreset::Google && self.client_secret.trim().is_empty() && !has_secret {
            return Err(t!("sso.form.secret_required_google").to_string());
        }
        let mut scopes = vec!["openid".to_string(), "email".into(), "profile".into()];
        scopes.extend(
            self.scopes
                .split(|c: char| c.is_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        );
        let cfg = SsoConfig {
            enabled: self.enabled,
            preset: self.preset,
            issuer: if fields.issuer {
                self.issuer.trim().to_string()
            } else {
                String::new()
            },
            tenant: fields
                .tenant
                .then(|| self.tenant.trim().to_string())
                .filter(|t| !t.is_empty()),
            client_id: self.client_id.trim().to_string(),
            scopes,
            email_claim: Some(self.email_claim.trim().to_string()).filter(|c| !c.is_empty()),
            require_email_verified: self.require_email_verified,
            enforce: self.enforce,
            owner_password_allowed: self.owner_password_allowed,
            auto_create: self.auto_create,
            auto_join: self.auto_join,
            satisfies_2fa: self.satisfies_2fa,
            has_client_secret: false,
        };
        // Google's issuer is filled by the core; a placeholder lets it check.
        let check = if self.preset == SsoPreset::Google {
            SsoConfig {
                issuer: "https://accounts.google.com".into(),
                ..cfg.clone()
            }
        } else {
            cfg.clone()
        };
        check.normalized().map_err(|e| e.to_string())?;
        Ok(cfg)
    }
}

/// Enforcement needs a verified domain.
pub fn can_enforce(domains: &[termoak_core::sso::TeamDomain]) -> bool {
    domains.iter().any(|d| d.verified_at.is_some())
}

/// The TXT record to publish, as one line for a DNS zone file.
pub fn txt_zone_line(d: &termoak_core::sso::TeamDomain) -> String {
    format!("{}. 300 IN TXT \"{}\"", d.record_name, d.record_value)
}

/// The state of a domain in words.
pub fn domain_state(d: &termoak_core::sso::TeamDomain) -> (String, bool) {
    match (&d.verified_at, &d.verified_by) {
        (Some(_), Some(by)) if by.starts_with("admin:") => {
            (t!("sso.domain.vouched").to_string(), true)
        }
        (Some(_), _) => (t!("sso.domain.verified").to_string(), true),
        (None, _) => match &d.last_error {
            Some(e) if !e.is_empty() => (t!("sso.domain.failed", error = e).to_string(), false),
            _ => (t!("sso.domain.pending").to_string(), false),
        },
    }
}

// ---------------------------------------------------------------------------
// The desktop flow: system browser + loopback (RFC 8252 §7.3)
// ---------------------------------------------------------------------------

/// How long the browser has to come back.
pub const BROWSER_TIMEOUT: Duration = Duration::from_secs(600);

/// A sign-in waiting for its two-step code: the same one-time code works
/// again with it.
#[derive(Clone)]
pub struct SsoTotpStep {
    pub pending: SsoPending,
    pub callback: String,
}

/// What a sign-in through SSO ended in.
pub enum SsoOutcome {
    Account(Arc<Account>),
    /// The account has two-step verification and the team does not count
    /// SSO as 2FA: ask for the code and finish with it.
    Totp(SsoTotpStep),
}

/// Starts a sign-in: binds the loopback, asks the server for the provider's
/// URL and returns it (to open in the system browser) with what the second
/// half needs.
pub async fn begin(
    ws: Workspace,
    server: ServerChoice,
    email: Option<String>,
    team_id: Option<Id>,
) -> Result<(String, SsoPending, termoak_client::sso::Loopback), ClientError> {
    let lb = termoak_client::sso::Loopback::bind().await?;
    let (url, pending) = ws
        .sso_start(server, email.as_deref(), team_id, lb.redirect_uri())
        .await?;
    Ok((url, pending, lb))
}

/// Waits for the browser and signs in.
pub async fn finish(
    ws: Workspace,
    pending: SsoPending,
    lb: termoak_client::sso::Loopback,
) -> Result<SsoOutcome, ClientError> {
    let callback = lb.wait(BROWSER_TIMEOUT).await?;
    complete(ws, pending, callback, None).await
}

/// The exchange (again with the two-step code).
pub async fn complete(
    ws: Workspace,
    pending: SsoPending,
    callback: String,
    totp: Option<String>,
) -> Result<SsoOutcome, ClientError> {
    match ws.sso_complete(&pending, &callback, totp.as_deref()).await {
        Ok(acc) => Ok(SsoOutcome::Account(acc)),
        Err(e) if e.is_totp_required() && totp.is_none() => {
            Ok(SsoOutcome::Totp(SsoTotpStep { pending, callback }))
        }
        Err(e) => Err(e),
    }
}

/// Links an identity of a team's provider to the signed-in account: the
/// server links it when the browser comes back (no new session here).
pub async fn link(
    api: ApiClient,
    team_id: Id,
    device: String,
) -> Result<(String, SsoPending, termoak_client::sso::Loopback), ClientError> {
    let lb = termoak_client::sso::Loopback::bind().await?;
    let (url, pending) = termoak_client::sso::start(
        &api,
        None,
        Some(team_id),
        lb.redirect_uri(),
        &device,
        &termoak_client::api::platform(),
        true,
    )
    .await?;
    Ok((url, pending, lb))
}

/// The second half of [`link`]: the callback must carry a code (an error
/// is the API error it names).
pub async fn link_wait(
    pending: SsoPending,
    lb: termoak_client::sso::Loopback,
) -> Result<(), ClientError> {
    let callback = lb.wait(BROWSER_TIMEOUT).await?;
    termoak_client::sso::code_from_callback(&callback, &pending)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_texts() {
        for lang in ["en", "es"] {
            rust_i18n::set_locale(lang);
            for code in ERROR_CODES {
                assert!(
                    crate::i18n::api_error_text(code).is_some(),
                    "{lang}: {code}"
                );
            }
            assert!(
                error_text("sso_required", &serde_json::json!({"team_name": "Corp"}))
                    .unwrap()
                    .contains("Corp")
            );
            for p in PRESETS {
                assert!(!preset_label(p).starts_with("sso."));
                let f = preset_fields(p);
                assert!(!t!(f.help_key).starts_with("sso."), "{lang} {:?}", p);
            }
        }
        rust_i18n::set_locale("en");
    }

    #[test]
    fn forms() {
        let mut f = SsoForm::new_for(SsoPreset::Keycloak);
        assert!(f.to_config(false).is_err());
        f.client_id = "termoak".into();
        f.issuer = "https://sso.example.com/realms/corp".into();
        f.scopes = "groups, offline_access".into();
        let c = f.to_config(false).unwrap();
        assert_eq!(
            c.scopes,
            ["openid", "email", "profile", "groups", "offline_access"]
        );
        assert!(c.require_email_verified && c.auto_create && c.owner_password_allowed);
        assert_eq!(SsoForm::from_config(&c).scopes, "groups offline_access");
        f.issuer = "not a url".into();
        assert!(f.to_config(false).is_err());

        let mut m = SsoForm::new_for(SsoPreset::Microsoft);
        assert!(!m.require_email_verified);
        m.client_id = "abc".into();
        assert!(m.to_config(false).is_err());
        m.tenant = "contoso.onmicrosoft.com".into();
        let c = m.to_config(false).unwrap();
        assert_eq!(c.tenant.as_deref(), Some("contoso.onmicrosoft.com"));
        assert!(c.issuer.is_empty());

        let mut g = SsoForm::new_for(SsoPreset::Google);
        g.client_id = "x.apps.googleusercontent.com".into();
        assert!(g.to_config(false).is_err());
        assert!(g.to_config(true).is_ok());
        g.client_secret = "s".into();
        assert!(g.to_config(false).is_ok());
    }

    #[test]
    fn domains() {
        let mut d = termoak_core::sso::TeamDomain {
            domain: "corp.example".into(),
            team_id: Id::nil(),
            token: "tok".into(),
            record_name: "_termoak-challenge.corp.example".into(),
            record_value: "termoak-verify=tok".into(),
            verified_at: None,
            verified_by: None,
            created_at: 0,
            checked_at: None,
            last_error: None,
        };
        assert!(!can_enforce(std::slice::from_ref(&d)));
        assert_eq!(
            txt_zone_line(&d),
            "_termoak-challenge.corp.example. 300 IN TXT \"termoak-verify=tok\""
        );
        assert!(!domain_state(&d).1);
        d.verified_at = Some(1);
        d.verified_by = Some("dns".into());
        assert!(can_enforce(std::slice::from_ref(&d)));
        assert!(domain_state(&d).1);
    }
}
