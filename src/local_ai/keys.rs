//! Your own AI API keys kept in this app, to run the AI on this computer.
//!
//! Each key is sealed with the local vault key (the one in the system
//! keyring that also protects your SSH passwords and keys), bound to its
//! provider (AAD `termoak:desktop-ai-key:{provider}`), and stored in the
//! `meta` table of the local database. It is never sent to the Termoak server,
//! never written to the settings or the logs, and the interface only shows
//! its last 4 characters.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use termoak_ai::AiError;
use termoak_core::{CoreError, Store};
use zeroize::Zeroizing;

/// Providers that accept your own key (same as on the server).
pub const PROVIDERS: &[&str] = termoak_ai::OWN_KEY_PROVIDERS;

/// Prefix of the `meta` entries (`desktop.ai.key.<provider>`).
const META_PREFIX: &str = "desktop.ai.key.";

/// Maximum length of a key and of a model name (as on the server).
const MAX_KEY: usize = 512;
const MAX_MODEL: usize = 128;

/// Display name of a provider (product names, not translated).
pub fn provider_label(provider: &str) -> &'static str {
    match provider {
        "claude" => "Anthropic (Claude)",
        "gpt" => "OpenAI",
        "openrouter" => "OpenRouter",
        "opencode-api" => "OpenCode Go",
        _ => "",
    }
}

/// Default model and suggested models of a provider (from the AI engine's
/// built-in providers).
pub fn provider_models(provider: &str) -> (Option<String>, Vec<String>) {
    let all = termoak_ai::config::builtin_providers();
    let Some(cfg) = all.get(provider) else {
        return (None, Vec::new());
    };
    let mut models: Vec<String> = cfg.model.iter().cloned().collect();
    for m in &cfg.models {
        if !models.contains(m) {
            models.push(m.clone());
        }
    }
    (cfg.model.clone(), models)
}

/// Public data of a stored key (never the key itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalKeyInfo {
    pub provider: String,
    /// Chosen model (`None` = the provider's default).
    pub model: Option<String>,
    /// Last 4 characters of the key.
    pub hint: String,
    pub updated_at: i64,
}

/// A decrypted key, only to call the provider.
pub struct LocalKeySecret {
    pub provider: String,
    pub key: Zeroizing<String>,
    pub model: Option<String>,
}

impl std::fmt::Debug for LocalKeySecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalKeySecret")
            .field("provider", &self.provider)
            .field("key", &"***")
            .field("model", &self.model)
            .finish()
    }
}

/// What is sealed.
#[derive(Serialize, Deserialize)]
struct Sealed {
    key: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    updated_at: i64,
}

impl Drop for Sealed {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.key.zeroize();
    }
}

/// Why a key could not be saved.
#[derive(Debug)]
pub enum KeyError {
    /// The provider does not accept your own key.
    UnknownProvider,
    /// 1 to 512 printable characters without spaces.
    InvalidKey,
    /// At most 128 characters.
    InvalidModel,
    /// Only the model changes, but there is no saved key.
    NoKey,
    Store(CoreError),
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            KeyError::UnknownProvider => t!("error.unknown_provider"),
            KeyError::InvalidKey => t!("ai_settings.keys.invalid_key"),
            KeyError::InvalidModel => t!("ai_settings.keys.invalid_model"),
            KeyError::NoKey => t!("ai_settings.keys.key_required"),
            KeyError::Store(e) => return write!(f, "{e}"),
        };
        f.write_str(&text)
    }
}

impl From<CoreError> for KeyError {
    fn from(e: CoreError) -> Self {
        KeyError::Store(e)
    }
}

/// A key the providers would accept: 1 to 512 printable characters without
/// spaces (the server's rule).
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY && key.chars().all(|c| c.is_ascii_graphic())
}

/// Last 4 characters of a key.
pub fn key_hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

fn meta_key(provider: &str) -> String {
    format!("{META_PREFIX}{provider}")
}

fn aad(provider: &str) -> Vec<u8> {
    format!("termoak:desktop-ai-key:{provider}").into_bytes()
}

fn known(provider: &str) -> Result<(), KeyError> {
    if PROVIDERS.contains(&provider) {
        Ok(())
    } else {
        Err(KeyError::UnknownProvider)
    }
}

async fn read(store: &Store, provider: &str) -> Result<Option<Sealed>, CoreError> {
    // Deleting leaves the value empty.
    let Some(text) = store
        .meta_get(&meta_key(provider))
        .await?
        .filter(|t| !t.is_empty())
    else {
        return Ok(None);
    };
    let raw = STANDARD
        .decode(text)
        .map_err(|e| CoreError::Crypto(e.to_string()))?;
    let plain = store.master_key().open(&raw, &aad(provider))?;
    serde_json::from_slice(&plain)
        .map(Some)
        .map_err(|e| CoreError::Crypto(e.to_string()))
}

async fn write(store: &Store, provider: &str, sealed: &Sealed) -> Result<(), CoreError> {
    let json =
        Zeroizing::new(serde_json::to_vec(sealed).map_err(|e| CoreError::Crypto(e.to_string()))?);
    let blob = store.master_key().seal(&json, &aad(provider))?;
    store
        .meta_set(&meta_key(provider), &STANDARD.encode(blob))
        .await
}

fn info(provider: &str, sealed: &Sealed) -> LocalKeyInfo {
    LocalKeyInfo {
        provider: provider.to_string(),
        model: sealed.model.clone(),
        hint: key_hint(&sealed.key),
        updated_at: sealed.updated_at,
    }
}

/// Stored keys (without the keys), in the order of [`PROVIDERS`]. A key that
/// cannot be decrypted is skipped with a warning.
pub async fn list(store: &Store) -> Result<Vec<LocalKeyInfo>, CoreError> {
    let mut out = Vec::new();
    for provider in PROVIDERS {
        match read(store, provider).await {
            Ok(Some(sealed)) => out.push(info(provider, &sealed)),
            Ok(None) => {}
            Err(e) => tracing::warn!(%provider, error = %e, "could not open a local AI key"),
        }
    }
    Ok(out)
}

/// One decrypted key, if there is one.
pub async fn secret(store: &Store, provider: &str) -> Result<Option<LocalKeySecret>, CoreError> {
    Ok(read(store, provider).await?.map(|s| LocalKeySecret {
        provider: provider.to_string(),
        key: Zeroizing::new(s.key.clone()),
        model: s.model.clone(),
    }))
}

/// Saves a key (replacing the stored one) with its model. With `key`
/// `None`, only the model of the stored key changes. `model` empty or
/// `None` = the provider's default.
pub async fn set(
    store: &Store,
    provider: &str,
    key: Option<&str>,
    model: Option<&str>,
) -> Result<LocalKeyInfo, KeyError> {
    known(provider)?;
    let model = model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .map(str::to_string);
    if model
        .as_ref()
        .is_some_and(|m| m.chars().count() > MAX_MODEL)
    {
        return Err(KeyError::InvalidModel);
    }
    let key = match key.map(str::trim) {
        Some(k) if valid_key(k) => k.to_string(),
        Some(_) => return Err(KeyError::InvalidKey),
        None => match read(store, provider).await? {
            Some(old) => old.key.clone(),
            None => return Err(KeyError::NoKey),
        },
    };
    let sealed = Sealed {
        key,
        model,
        updated_at: termoak_core::time::now_ms(),
    };
    write(store, provider, &sealed).await?;
    Ok(info(provider, &sealed))
}

/// Deletes a stored key. `false` if there was none.
pub async fn delete(store: &Store, provider: &str) -> Result<bool, CoreError> {
    let had = store
        .meta_get(&meta_key(provider))
        .await?
        .is_some_and(|t| !t.is_empty());
    if had {
        store.meta_set(&meta_key(provider), "").await?;
    }
    Ok(had)
}

/// Checks a key with its provider, with a call that spends nothing (the
/// same check the server does: list the models, or OpenRouter's `/key`).
pub async fn check(provider: &str, key: &str) -> Result<(), String> {
    let registry = termoak_ai::provider::Registry::new(termoak_ai::AiConfig::default());
    registry
        .check_key(provider, key)
        .await
        .map_err(|e| match e {
            AiError::Http {
                status: 401 | 403, ..
            } => t!("ai_settings.keys.test_rejected").to_string(),
            AiError::Http {
                status, message, ..
            } => t!(
                "ai_settings.keys.test_http",
                status = status,
                message = message
            )
            .to_string(),
            other => other.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use termoak_core::crypto::MasterKey;

    use super::*;

    fn store() -> Store {
        Store::open_in_memory(MasterKey::generate()).unwrap()
    }

    #[test]
    fn keys_are_validated() {
        assert!(valid_key("sk-ant-api03-abc_DEF"));
        assert!(!valid_key(""));
        assert!(!valid_key("sk with spaces"));
        assert!(!valid_key("sk-ñ"));
        assert!(!valid_key(&"x".repeat(513)));
        assert_eq!(key_hint("sk-abcdef"), "cdef");
        assert_eq!(key_hint("ab"), "ab");
    }

    #[test]
    fn providers_have_labels_and_models() {
        for p in PROVIDERS {
            assert!(!provider_label(p).is_empty(), "{p}");
            let (default, models) = provider_models(p);
            assert!(default.is_some(), "{p}");
            assert_eq!(models.first(), default.as_ref(), "{p}");
        }
    }

    #[tokio::test]
    async fn keys_are_sealed_bound_and_removable() {
        let store = store();
        assert!(list(&store).await.unwrap().is_empty());
        let saved = set(&store, "claude", Some(" sk-ant-secret-1234 "), None)
            .await
            .unwrap();
        assert_eq!(
            (saved.hint.as_str(), saved.model.as_deref()),
            ("1234", None)
        );

        // Encrypted at rest and bound to the provider.
        let raw = store
            .meta_get("desktop.ai.key.claude")
            .await
            .unwrap()
            .unwrap();
        assert!(!raw.contains("sk-ant"));
        let blob = STANDARD.decode(&raw).unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("sk-ant"));
        assert!(store.master_key().open(&blob, &aad("gpt")).is_err());

        // Only the model changes: the key stays.
        let changed = set(&store, "claude", None, Some("claude-sonnet-5"))
            .await
            .unwrap();
        assert_eq!(changed.hint, "1234");
        let s = secret(&store, "claude").await.unwrap().unwrap();
        assert_eq!(s.key.as_str(), "sk-ant-secret-1234");
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-5"));
        assert!(!format!("{s:?}").contains("sk-ant"));

        assert!(matches!(
            set(&store, "gpt", None, None).await,
            Err(KeyError::NoKey)
        ));
        assert!(matches!(
            set(&store, "codex", Some("k"), None).await,
            Err(KeyError::UnknownProvider)
        ));
        assert!(matches!(
            set(&store, "gpt", Some("a b"), None).await,
            Err(KeyError::InvalidKey)
        ));
        assert!(matches!(
            set(&store, "gpt", Some("k"), Some(&"m".repeat(129))).await,
            Err(KeyError::InvalidModel)
        ));

        set(&store, "openrouter", Some("sk-or-9999"), Some(" "))
            .await
            .unwrap();
        let all = list(&store).await.unwrap();
        assert_eq!(
            all.iter().map(|k| k.provider.as_str()).collect::<Vec<_>>(),
            ["claude", "openrouter"]
        );
        assert_eq!(all[1].model, None);

        assert!(delete(&store, "claude").await.unwrap());
        assert!(!delete(&store, "claude").await.unwrap());
        assert!(secret(&store, "claude").await.unwrap().is_none());
        assert_eq!(list(&store).await.unwrap().len(), 1);
    }
}
