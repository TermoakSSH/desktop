//! Where the AI runs on this device (Settings → AI).
//!
//! - **Your Termoak account**: the server runs the copilot and the tasks with
//!   the account's own keys or the plan's AI credit (what the app always did).
//! - **This computer**: with one of your own API keys stored in this app
//!   ([`keys`]) or with an agent installed here ([`agents`]).
//!
//! With "This computer", the copilot runs here ([`copilot`]), with the tools
//! over the local vault, the app's SSH engine and its terminals
//! ([`terminals`]). AI tasks still run only on the server: the AI section
//! says so and sends nothing (they come next, through [`RunOn`] and
//! [`local_target`] as well).

pub mod agents;
pub mod copilot;
pub mod keys;
pub mod tasks;
pub mod terminals;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use self::agents::AgentStatus;
use self::keys::LocalKeyInfo;

/// Where the AI runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOn {
    /// On the Termoak server, with the signed-in account.
    Server,
    /// On this computer.
    Local,
}

/// What runs the AI on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalSource {
    /// One of your own API keys stored in this app.
    ApiKey,
    /// An agent installed on this computer (Codex, Claude Code...).
    Agent,
}

/// AI preferences of this device (part of the desktop settings).
///
/// Every field is optional so that older settings files keep loading and
/// "not chosen yet" can follow the defaults of the spec.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// Explicit choice (`None`: server when signed in, otherwise local).
    pub run_on: Option<RunOn>,
    /// Source of the local AI (`None`: the first one available).
    pub local_source: Option<LocalSource>,
    /// Provider of your own key used locally (`claude`, `gpt`, `openrouter`,
    /// `opencode-api`; `None`: the first one with a key).
    pub local_provider: Option<String>,
    /// Agent used locally (`codex`, `claude`, `agy`, `opencode`; `None`: the
    /// first one installed).
    pub local_agent: Option<String>,
    /// Antigravity (experimental) was turned on after the warning: it can
    /// run commands on this computer without Termoak's approvals.
    pub agy_opt_in: bool,
}

impl AiSettings {
    /// Where the AI runs now. The server needs a session: signed out it is
    /// always this computer (the saved choice comes back when signing in).
    pub fn run_on(&self, logged_in: bool) -> RunOn {
        match self.run_on {
            Some(RunOn::Server) | None if logged_in => RunOn::Server,
            _ => RunOn::Local,
        }
    }

    /// Source of the local AI: the chosen one or else the first available
    /// (your keys first, then an installed agent).
    pub fn local_source(&self, has_key: bool, has_agent: bool) -> LocalSource {
        self.local_source.unwrap_or(if !has_key && has_agent {
            LocalSource::Agent
        } else {
            LocalSource::ApiKey
        })
    }
}

/// What would run the AI on this computer (phases 2-3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalTarget {
    /// Your own API key for `provider` (read it with [`keys::secret`]).
    ApiKey {
        provider: String,
        model: Option<String>,
    },
    /// An agent installed on this computer.
    Agent { agent: &'static str, path: PathBuf },
}

/// What runs the AI locally with these settings, the stored keys and the
/// detected agents. `None` when the chosen source has nothing usable.
pub fn local_target(
    settings: &AiSettings,
    keys: &[LocalKeyInfo],
    agents: &[AgentStatus],
) -> Option<LocalTarget> {
    let installed = || agents.iter().filter(|a| a.path.is_some());
    match settings.local_source(!keys.is_empty(), installed().next().is_some()) {
        LocalSource::ApiKey => {
            let key = settings
                .local_provider
                .as_deref()
                .and_then(|p| keys.iter().find(|k| k.provider == p))
                .or_else(|| keys.first())?;
            Some(LocalTarget::ApiKey {
                provider: key.provider.clone(),
                model: key.model.clone(),
            })
        }
        LocalSource::Agent => {
            // Antigravity is only picked by default once turned on.
            let agent = settings
                .local_agent
                .as_deref()
                .and_then(|id| installed().find(|a| a.id == id))
                .or_else(|| installed().find(|a| a.id != "agy" || settings.agy_opt_in))
                .or_else(|| installed().next())?;
            Some(LocalTarget::Agent {
                agent: agent.id,
                path: agent.path.clone()?,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_on_defaults_to_the_server_only_when_signed_in() {
        let mut s = AiSettings::default();
        assert_eq!(s.run_on(true), RunOn::Server);
        assert_eq!(s.run_on(false), RunOn::Local);
        s.run_on = Some(RunOn::Local);
        assert_eq!(s.run_on(true), RunOn::Local);
        // The server needs a session.
        s.run_on = Some(RunOn::Server);
        assert_eq!(s.run_on(false), RunOn::Local);
        assert_eq!(s.run_on(true), RunOn::Server);
    }

    #[test]
    fn local_source_defaults_to_the_first_available() {
        let mut s = AiSettings::default();
        assert_eq!(s.local_source(false, false), LocalSource::ApiKey);
        assert_eq!(s.local_source(true, true), LocalSource::ApiKey);
        assert_eq!(s.local_source(false, true), LocalSource::Agent);
        s.local_source = Some(LocalSource::Agent);
        assert_eq!(s.local_source(true, false), LocalSource::Agent);
    }

    #[test]
    fn settings_serde_defaults() {
        // Missing fields (and the whole section) fall back to the defaults.
        let s: AiSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s, AiSettings::default());
        let s: AiSettings =
            serde_json::from_str(r#"{"run_on":"local","local_source":"agent","x":1}"#).unwrap();
        assert_eq!(s.run_on, Some(RunOn::Local));
        assert_eq!(s.local_source, Some(LocalSource::Agent));
        assert_eq!(s.local_agent, None);
        let json = serde_json::to_string(&AiSettings {
            run_on: Some(RunOn::Server),
            local_source: Some(LocalSource::ApiKey),
            local_provider: Some("claude".into()),
            local_agent: Some("codex".into()),
            agy_opt_in: false,
        })
        .unwrap();
        assert!(json.contains(r#""run_on":"server""#));
        assert!(json.contains(r#""local_source":"api_key""#));
    }

    #[test]
    fn local_target_follows_the_choice() {
        let key = |p: &str| LocalKeyInfo {
            provider: p.into(),
            model: Some(format!("{p}-model")),
            hint: "1234".into(),
            updated_at: 0,
        };
        let agent = |id: &'static str, found: bool| AgentStatus {
            id,
            path: found.then(|| PathBuf::from(format!("/usr/bin/{id}"))),
            version: None,
        };
        let keys = [key("claude"), key("gpt")];
        let agents = [agent("codex", false), agent("opencode", true)];
        let mut s = AiSettings::default();
        assert_eq!(
            local_target(&s, &keys, &agents),
            Some(LocalTarget::ApiKey {
                provider: "claude".into(),
                model: Some("claude-model".into())
            })
        );
        s.local_provider = Some("gpt".into());
        assert!(matches!(
            local_target(&s, &keys, &agents),
            Some(LocalTarget::ApiKey { provider, .. }) if provider == "gpt"
        ));
        // Without keys, the first installed agent.
        assert_eq!(
            local_target(&s, &[], &agents),
            Some(LocalTarget::Agent {
                agent: "opencode",
                path: PathBuf::from("/usr/bin/opencode")
            })
        );
        // The chosen agent is not installed: the first one that is.
        s.local_source = Some(LocalSource::Agent);
        s.local_agent = Some("codex".into());
        assert!(matches!(
            local_target(&s, &keys, &agents),
            Some(LocalTarget::Agent {
                agent: "opencode",
                ..
            })
        ));
        assert_eq!(local_target(&s, &keys, &[agent("codex", false)]), None);

        // Antigravity is not the default pick until it is turned on.
        let both = [agent("agy", true), agent("opencode", true)];
        s.local_agent = None;
        assert!(matches!(
            local_target(&s, &keys, &both),
            Some(LocalTarget::Agent {
                agent: "opencode",
                ..
            })
        ));
        s.agy_opt_in = true;
        assert!(matches!(
            local_target(&s, &keys, &both),
            Some(LocalTarget::Agent { agent: "agy", .. })
        ));
        // Old settings files have it off.
        let old: AiSettings = serde_json::from_str(r#"{"local_agent":"agy"}"#).unwrap();
        assert!(!old.agy_opt_in);
    }
}
