//! The extended audit in the app (0.7 phase 5b, docs-next/E2EE-SSO-AUDIT.md
//! §4 and §10): the filter presets, time ranges, readable sentences of the
//! new actions, sink forms and states, export names. Pure: the view is
//! `views::audit`.

use serde_json::Value;
use termoak_core::model::AuditEntry;
use termoak_core::store::audit::{AuditFilter, AuditSink, AuditSinkConfig, AuditSinkKind};

/// Error codes of the audit: always the app's text.
pub const ERROR_CODES: &[&str] = &["audit_sink_invalid", "invalid_format", "invalid_retention"];

pub fn is_audit_code(code: &str) -> bool {
    ERROR_CODES.contains(&code)
}

/// Entries per page (`limit`).
pub const PAGE: i64 = 50;

/// The action presets of the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPreset {
    All,
    Access,
    Certificates,
    Policy,
    E2ee,
    Sso,
    SignIns,
}

pub const PRESETS: [ActionPreset; 7] = [
    ActionPreset::All,
    ActionPreset::Access,
    ActionPreset::Certificates,
    ActionPreset::Policy,
    ActionPreset::E2ee,
    ActionPreset::Sso,
    ActionPreset::SignIns,
];

impl ActionPreset {
    /// The `action` filter (`None`: every action).
    pub fn actions(self) -> Option<&'static str> {
        match self {
            ActionPreset::All => None,
            ActionPreset::Access => Some("access.*,access_policy.*"),
            ActionPreset::Certificates => Some("cert.*,ca.*"),
            ActionPreset::Policy => Some("team.policy,access_policy.*,team.role_changed"),
            ActionPreset::E2ee => Some("e2ee.*"),
            ActionPreset::Sso => Some("sso.*,auth.login_refused"),
            ActionPreset::SignIns => Some("auth.*,sso.login,sso.login_failed"),
        }
    }

    pub fn label(self) -> String {
        match self {
            ActionPreset::All => t!("audit.preset.all"),
            ActionPreset::Access => t!("audit.preset.access"),
            ActionPreset::Certificates => t!("audit.preset.certificates"),
            ActionPreset::Policy => t!("audit.preset.policy"),
            ActionPreset::E2ee => t!("audit.preset.e2ee"),
            ActionPreset::Sso => t!("audit.preset.sso"),
            ActionPreset::SignIns => t!("audit.preset.sign_ins"),
        }
        .to_string()
    }
}

/// Time ranges of the filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Any,
    Day,
    Week,
    Month,
    Quarter,
}

pub const RANGES: [Range; 5] = [
    Range::Any,
    Range::Day,
    Range::Week,
    Range::Month,
    Range::Quarter,
];

impl Range {
    /// `since` (ms) for a range ending now.
    pub fn since(self, now_ms: i64) -> Option<i64> {
        let day = 24 * 3600 * 1000;
        match self {
            Range::Any => None,
            Range::Day => Some(now_ms - day),
            Range::Week => Some(now_ms - 7 * day),
            Range::Month => Some(now_ms - 30 * day),
            Range::Quarter => Some(now_ms - 90 * day),
        }
    }

    pub fn label(self) -> String {
        match self {
            Range::Any => t!("audit.range.any"),
            Range::Day => t!("audit.range.day"),
            Range::Week => t!("audit.range.week"),
            Range::Month => t!("audit.range.month"),
            Range::Quarter => t!("audit.range.quarter"),
        }
        .to_string()
    }
}

/// What the filter bar holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filters {
    pub preset: ActionPreset,
    /// Extra actions typed by hand (comma list, `*` prefixes).
    pub actions: String,
    pub actor: String,
    pub vault: Option<termoak_core::Id>,
    pub range: Range,
    pub text: String,
}

impl Default for Filters {
    fn default() -> Self {
        Self {
            preset: ActionPreset::All,
            actions: String::new(),
            actor: String::new(),
            vault: None,
            range: Range::Any,
            text: String::new(),
        }
    }
}

impl Filters {
    /// The server's filter for the first page (`before`: next pages).
    pub fn to_filter(&self, now_ms: i64, before: Option<i64>) -> AuditFilter {
        let mut actions: Vec<String> = self
            .preset
            .actions()
            .map(|a| a.split(',').map(str::to_string).collect())
            .unwrap_or_default();
        actions.extend(
            self.actions
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|a| !a.is_empty())
                .map(str::to_string),
        );
        let actor = self.actor.trim();
        AuditFilter {
            action: (!actions.is_empty()).then(|| actions.join(",")),
            actor: (!actor.is_empty()).then(|| actor.to_string()),
            vault_id: self.vault,
            since: self.range.since(now_ms),
            q: Some(self.text.trim().to_string()).filter(|q| !q.is_empty()),
            before,
            limit: Some(PAGE),
            ..Default::default()
        }
    }
}

/// The next page: older than the last entry shown (`None` when the last
/// page was not full).
pub fn next_before(entries: &[AuditEntry], last_page: usize) -> Option<i64> {
    if (last_page as i64) < PAGE {
        return None;
    }
    entries.last().map(|e| e.id)
}

/// The file name of an export.
pub fn export_name(scope: &str, format: &str, date: &str) -> String {
    let scope: String = scope
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .to_lowercase();
    let scope = scope.trim_matches('-');
    let scope = if scope.is_empty() { "audit" } else { scope };
    format!("termoak-audit-{scope}-{date}.{format}")
}

/// What the new actions (0.7 phase 5a) mean, in words; older ones keep
/// the admin view's labels; anything else is shown as is.
pub fn action_text(action: &str) -> String {
    let key = match action {
        "e2ee.setup" => "audit.action.e2ee_setup",
        "e2ee.seal" => "audit.action.e2ee_seal",
        "e2ee.passphrase_removed" => "audit.action.e2ee_passphrase_removed",
        "e2ee.key_rotate" => "audit.action.e2ee_key_rotate",
        "e2ee.reset" => "audit.action.e2ee_reset",
        "e2ee.pairing_request" => "audit.action.e2ee_pairing_request",
        "e2ee.pairing_approve" => "audit.action.e2ee_pairing_approve",
        "e2ee.pairing_deny" => "audit.action.e2ee_pairing_deny",
        "e2ee.enable" => "audit.action.e2ee_enable",
        "e2ee.rotate" => "audit.action.e2ee_rotate",
        "e2ee.disable" => "audit.action.e2ee_disable",
        "e2ee.wraps_add" => "audit.action.e2ee_wraps_add",
        "e2ee.unlock" => "audit.action.e2ee_unlock",
        "sso.config" => "audit.action.sso_config",
        "sso.domain_add" => "audit.action.sso_domain_add",
        "sso.domain_verify" => "audit.action.sso_domain_verify",
        "sso.domain_check_failed" => "audit.action.sso_domain_check_failed",
        "sso.domain_remove" => "audit.action.sso_domain_remove",
        "sso.login" => "audit.action.sso_login",
        "sso.login_failed" => "audit.action.sso_login_failed",
        "sso.link" => "audit.action.sso_link",
        "sso.unlink" => "audit.action.sso_unlink",
        "sso.user_created" => "audit.action.sso_user_created",
        "auth.login_refused" => "audit.action.auth_login_refused",
        "audit.export" => "audit.action.audit_export",
        "audit.settings" => "audit.action.audit_settings",
        "audit.sink_create" => "audit.action.audit_sink_create",
        "audit.sink_update" => "audit.action.audit_sink_update",
        "audit.sink_delete" => "audit.action.audit_sink_delete",
        "audit.test" => "audit.action.audit_test",
        "access.request" => "audit.action.access_request",
        "access.approve" => "audit.action.access_approve",
        "access.deny" => "audit.action.access_deny",
        "access.revoke" => "audit.action.access_revoke",
        "access.cancel" => "audit.action.access_cancel",
        "access_policy.set" => "audit.action.access_policy_set",
        "access_policy.delete" => "audit.action.access_policy_delete",
        "cert.issue" => "audit.action.cert_issue",
        "cert.revoke" => "audit.action.cert_revoke",
        "ca.create" => "audit.action.ca_create",
        "ca.rotate" => "audit.action.ca_rotate",
        "ca.disable" => "audit.action.ca_disable",
        "team.policy" => "audit.action.team_policy",
        "vault.create" => "audit.action.vault_create",
        "vault.update" => "audit.action.vault_update",
        "vault.delete" => "audit.action.vault_delete",
        "vault.member_add" => "audit.action.vault_member_add",
        "vault.member_removed" => "audit.action.vault_member_removed",
        "vault.role_changed" => "audit.action.vault_role_changed",
        "vault.transfer" => "audit.action.vault_transfer",
        "auth.devices_revoked" => "audit.action.auth_devices_revoked",
        "auth.email_changed" => "audit.action.auth_email_changed",
        "auth.email_verified" => "audit.action.auth_email_verified",
        "auth.password_reset" => "audit.action.auth_password_reset",
        "network.up" => "audit.action.network_up",
        "network.down" => "audit.action.network_down",
        "network.key_generated" => "audit.action.network_key_generated",
        "team.principal_changed" => "audit.action.team_principal_changed",
        "team.invite_created" => "audit.action.team_invite_created",
        "team.invite_revoked" => "audit.action.team_invite_revoked",
        "secret.reveal" => "audit.action.secret_reveal",
        "secret.use" => "audit.action.secret_use",
        _ => {
            return crate::views::admin::action_label(action)
                .map(|s| s.to_string())
                .unwrap_or_else(|| action.to_string());
        }
    };
    rust_i18n::t!(key).to_string()
}

/// A short readable summary of an entry's details (the device, the email,
/// the reason...), at most `max` characters.
pub fn detail_summary(detail: &Value, max: usize) -> String {
    let Some(obj) = detail.as_object() else {
        return String::new();
    };
    let mut parts = Vec::new();
    for (k, v) in obj {
        let v = match v {
            Value::String(s) => s.clone(),
            Value::Null => continue,
            Value::Array(a) if a.is_empty() => continue,
            other => other.to_string(),
        };
        parts.push(format!("{k}: {v}"));
    }
    let s = parts.join(" · ");
    if s.chars().count() > max {
        format!("{}…", s.chars().take(max).collect::<String>())
    } else {
        s
    }
}

/// Retention in days: 1 to the server's maximum (empty: the server's
/// default).
pub fn parse_retention(text: &str, max: u32) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<u32>() {
        Ok(d) if d >= 1 && d <= max => Ok(Some(d)),
        _ => Err(t!("audit.retention.invalid", max = max).to_string()),
    }
}

/// What the sink form holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkForm {
    pub kind: AuditSinkKind,
    pub name: String,
    pub url: String,
    pub host: String,
    pub port: String,
    pub tls: bool,
    pub ca_pem: String,
    pub actions: String,
}

impl Default for SinkForm {
    fn default() -> Self {
        Self {
            kind: AuditSinkKind::Webhook,
            name: String::new(),
            url: String::new(),
            host: String::new(),
            port: String::new(),
            tls: true,
            ca_pem: String::new(),
            actions: String::new(),
        }
    }
}

impl SinkForm {
    /// Name, config and actions to send (`Err`: the text to show).
    pub fn to_parts(&self) -> Result<(String, AuditSinkConfig, Vec<String>), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(t!("audit.sink.name_required").to_string());
        }
        let actions: Vec<String> = self
            .actions
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|a| !a.is_empty())
            .map(str::to_string)
            .collect();
        let config = match self.kind {
            AuditSinkKind::Webhook => {
                let url = self.url.trim();
                let ok = url::Url::parse(url)
                    .ok()
                    .is_some_and(|u| matches!(u.scheme(), "https" | "http") && u.host().is_some());
                if !ok {
                    return Err(t!("audit.sink.url_invalid").to_string());
                }
                AuditSinkConfig {
                    url: Some(url.to_string()),
                    ..Default::default()
                }
            }
            AuditSinkKind::Syslog => {
                let host = self.host.trim();
                if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
                    return Err(t!("audit.sink.host_invalid").to_string());
                }
                let port = match self.port.trim() {
                    "" => None,
                    p => Some(
                        p.parse::<u16>()
                            .ok()
                            .filter(|p| *p > 0)
                            .ok_or_else(|| t!("audit.sink.port_invalid").to_string())?,
                    ),
                };
                AuditSinkConfig {
                    host: Some(host.to_string()),
                    port,
                    transport: Some(if self.tls { "tls" } else { "tcp" }.into()),
                    ca_pem: (self.tls && !self.ca_pem.trim().is_empty())
                        .then(|| self.ca_pem.trim().to_string()),
                    ..Default::default()
                }
            }
        };
        Ok((name.to_string(), config, actions))
    }
}

/// Where a sink delivers, in one line.
pub fn sink_target(s: &AuditSink) -> String {
    match s.kind {
        AuditSinkKind::Webhook => s.config.url.clone().unwrap_or_default(),
        AuditSinkKind::Syslog => {
            let tls = s.config.transport.as_deref() != Some("tcp");
            let port = s.config.port.unwrap_or(if tls { 6514 } else { 514 });
            format!(
                "syslog{}://{}:{port}",
                if tls { "+tls" } else { "" },
                s.config.host.clone().unwrap_or_default()
            )
        }
    }
}

/// The delivery state of a sink: text and whether it is healthy.
pub fn sink_state(s: &AuditSink) -> (String, bool) {
    if !s.enabled {
        return (t!("audit.sink.state.disabled").to_string(), true);
    }
    if s.failures > 0 {
        return (
            t!(
                "audit.sink.state.failing",
                count = s.failures,
                error = s.last_error.clone().unwrap_or_default()
            )
            .to_string(),
            false,
        );
    }
    match s.last_success_at {
        Some(ms) => (
            t!(
                "audit.sink.state.delivered",
                date = crate::ui::format_ms(ms)
            )
            .to_string(),
            true,
        ),
        None => (t!("audit.sink.state.waiting").to_string(), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64) -> AuditEntry {
        AuditEntry {
            id,
            owner_id: termoak_core::Id::nil(),
            actor: "user:x".into(),
            action: "e2ee.enable".into(),
            target: None,
            detail: serde_json::json!({"device": "laptop", "vaults": [], "n": 2}),
            created_at: 0,
            vault_id: None,
            team_id: None,
        }
    }

    #[test]
    fn filters_and_pages() {
        let mut f = Filters {
            preset: ActionPreset::E2ee,
            actions: "sso.login, cert.*".into(),
            actor: " user:abc ".into(),
            range: Range::Week,
            text: "prod".into(),
            ..Default::default()
        };
        let q = f.to_filter(1_000_000_000_000, Some(42));
        assert_eq!(q.action.as_deref(), Some("e2ee.*,sso.login,cert.*"));
        assert_eq!(q.actor.as_deref(), Some("user:abc"));
        assert_eq!(q.since, Some(1_000_000_000_000 - 7 * 86_400_000));
        assert_eq!((q.before, q.limit), (Some(42), Some(PAGE)));
        assert_eq!(q.q.as_deref(), Some("prod"));
        f = Filters::default();
        let q = f.to_filter(0, None);
        assert!(q.action.is_none() && q.actor.is_none() && q.since.is_none() && q.q.is_none());
        let page: Vec<AuditEntry> = (0..PAGE).map(|i| entry(1000 - i)).collect();
        assert_eq!(next_before(&page, page.len()), Some(1000 - PAGE + 1));
        assert_eq!(next_before(&page[..3], 3), None);
        for p in PRESETS {
            assert!(p == ActionPreset::All || p.actions().is_some());
        }
    }

    #[test]
    fn texts() {
        for lang in ["en", "es"] {
            rust_i18n::set_locale(lang);
            for code in ERROR_CODES {
                assert!(
                    crate::i18n::api_error_text(code).is_some(),
                    "{lang}: {code}"
                );
            }
            for a in [
                "e2ee.setup",
                "e2ee.enable",
                "e2ee.unlock",
                "e2ee.pairing_approve",
                "sso.login",
                "sso.domain_verify",
                "auth.login_refused",
                "audit.export",
                "audit.sink_create",
                "access.approve",
                "cert.issue",
                "ca.create",
                "team.policy",
            ] {
                let t = action_text(a);
                assert!(!t.starts_with("audit.action"), "{lang}: {a}: {t}");
                assert_ne!(t, a, "{lang}: {a}");
            }
            for p in PRESETS {
                assert!(!p.label().starts_with("audit."));
            }
            for r in RANGES {
                assert!(!r.label().starts_with("audit."));
            }
        }
        rust_i18n::set_locale("en");
        assert_eq!(action_text("something.new"), "something.new");
        assert_eq!(
            detail_summary(&entry(1).detail, 200),
            "device: laptop · n: 2"
        );
        assert!(detail_summary(&entry(1).detail, 5).ends_with('…'));
        assert_eq!(
            export_name("Ops team!", "csv", "2026-10-27"),
            "termoak-audit-ops-team-2026-10-27.csv"
        );
        assert_eq!(export_name("", "json", "d"), "termoak-audit-audit-d.json");
    }

    #[test]
    fn sinks() {
        assert_eq!(parse_retention("", 3650), Ok(None));
        assert_eq!(parse_retention("90", 3650), Ok(Some(90)));
        assert!(parse_retention("0", 3650).is_err());
        assert!(parse_retention("4000", 3650).is_err());
        let mut f = SinkForm::default();
        assert!(f.to_parts().is_err());
        f.name = "SIEM".into();
        f.url = "ftp://x".into();
        assert!(f.to_parts().is_err());
        f.url = "https://siem.example.com/hook".into();
        f.actions = "access.*, e2ee.*".into();
        let (n, c, a) = f.to_parts().unwrap();
        assert_eq!((n.as_str(), a.len()), ("SIEM", 2));
        assert_eq!(c.url.as_deref(), Some("https://siem.example.com/hook"));
        f.kind = AuditSinkKind::Syslog;
        f.host = "logs.example.com".into();
        f.port = "x".into();
        assert!(f.to_parts().is_err());
        f.port = "6514".into();
        let (_, c, _) = f.to_parts().unwrap();
        assert_eq!(c.transport.as_deref(), Some("tls"));
        let sink = AuditSink {
            id: termoak_core::Id::nil(),
            team_id: None,
            kind: AuditSinkKind::Syslog,
            name: "s".into(),
            config: AuditSinkConfig {
                host: Some("h".into()),
                transport: Some("tcp".into()),
                ..Default::default()
            },
            actions: vec![],
            enabled: true,
            cursor: 0,
            failures: 3,
            last_error: Some("refused".into()),
            last_attempt_at: None,
            last_success_at: None,
            created_by: termoak_core::Id::nil(),
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(sink_target(&sink), "syslog://h:514");
        assert!(!sink_state(&sink).1);
    }
}
