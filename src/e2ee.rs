//! End-to-end encrypted vaults in the app (0.7 phase 5b,
//! docs-next/E2EE-SSO-AUDIT.md §2 and §10): the pure rules of the screens
//! (recovery phrase, confirmation words, passphrase strength, error and
//! reason texts) and the [`E2eeBook`], the E2EE state of every account
//! shared by every window (set up, unlocked on this computer, seals,
//! pairings waiting, secrets still locked).

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use termoak_client::Account;
use termoak_client::e2ee::PairingRequest;
use termoak_core::Id;
use termoak_core::e2ee::{RECOVERY_WORDS, SealKind};
use termoak_core::model::UserKeyInfo;

use crate::runtime;

/// Error codes of E2EE vaults: always the app's text (the server's English
/// message is for developers).
pub const ERROR_CODES: &[&str] = &[
    "e2ee_locked",
    "e2ee_unlock_required",
    "e2ee_vault",
    "e2ee_plaintext_rejected",
    "e2ee_key_outdated",
    "e2ee_not_enabled",
    "e2ee_already_enabled",
    "e2ee_incomplete",
    "e2ee_stale",
    "e2ee_wrap_invalid",
    "e2ee_not_set_up",
    "e2ee_key_exists",
    "e2ee_key_mismatch",
    "e2ee_client_required",
    "e2ee_pairing_closed",
    "e2ee_passphrase_invalid",
    "e2ee_passphrase_too_short",
    "e2ee_recovery_key_invalid",
    "e2ee_decrypt_failed",
];

pub fn is_e2ee_code(code: &str) -> bool {
    ERROR_CODES.contains(&code)
}

/// The text of an E2EE error, with what its details add (the reason a
/// recovery phrase was refused).
pub fn error_text(code: &str, details: &Value) -> Option<String> {
    if code == "e2ee_recovery_key_invalid"
        && let Some(reason) = details["reason"].as_str()
    {
        return Some(recovery_reason_text(
            reason,
            details["word"].as_u64().unwrap_or(0) as u32,
            details["words"].as_u64().unwrap_or(0) as u32,
        ));
    }
    crate::i18n::api_error_text(code)
}

/// Why a typed recovery phrase is refused, in words.
pub fn recovery_reason_text(reason: &str, word: u32, words: u32) -> String {
    match reason {
        "word_count" => t!(
            "e2ee.recovery.reason.word_count",
            count = words,
            expected = RECOVERY_WORDS
        )
        .to_string(),
        "unknown_word" => t!("e2ee.recovery.reason.unknown_word", n = word).to_string(),
        "checksum" => t!("e2ee.recovery.reason.checksum").to_string(),
        "old_format" => t!("e2ee.recovery.reason.old_format").to_string(),
        _ => t!("error.e2ee_recovery_key_invalid").to_string(),
    }
}

/// Live feedback while a recovery phrase is typed: `None` when it is
/// valid, else what is wrong (nothing while fewer than 24 words are there
/// and the ones typed are fine).
pub fn recovery_feedback(text: &str) -> RecoveryFeedback {
    let words = text
        .split(|c: char| !c.is_alphabetic())
        .filter(|t| !t.is_empty())
        .count();
    match termoak_core::e2ee::RecoveryKey::parse(text) {
        Ok(_) => RecoveryFeedback::Valid,
        Err(termoak_core::CoreError::Vault { detail, .. }) => {
            let d = detail.unwrap_or_default();
            let reason = d["reason"].as_str().unwrap_or_default();
            let word = d["word"].as_u64().unwrap_or(0) as u32;
            match reason {
                // Still typing: say how many words there are.
                "word_count" if words < RECOVERY_WORDS => {
                    // An unknown word among the typed ones is said now.
                    match first_unknown_word(text) {
                        Some(n) => {
                            RecoveryFeedback::Problem(recovery_reason_text("unknown_word", n, 0))
                        }
                        None => RecoveryFeedback::Typing(words),
                    }
                }
                _ => RecoveryFeedback::Problem(recovery_reason_text(reason, word, words as u32)),
            }
        }
        Err(e) => RecoveryFeedback::Problem(e.to_string()),
    }
}

/// The first typed word (1-based) that is no BIP39 word nor the beginning
/// of exactly one (the last word may still be being typed: it only counts
/// when it cannot start any word).
fn first_unknown_word(text: &str) -> Option<u32> {
    let lower = text.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(|c: char| !c.is_alphabetic())
        .filter(|t| !t.is_empty())
        .collect();
    let list = termoak_core::e2ee::recovery_wordlist();
    let ends_open = text.chars().last().is_some_and(|c| c.is_alphabetic());
    for (i, t) in tokens.iter().enumerate() {
        let last = i + 1 == tokens.len();
        let known = list.binary_search(t).is_ok()
            || (t.len() >= 4 && list.iter().filter(|w| w.starts_with(t)).count() == 1);
        if known {
            continue;
        }
        let could_start = list.iter().any(|w| w.starts_with(t));
        if last && ends_open && could_start {
            continue;
        }
        return Some(i as u32 + 1);
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryFeedback {
    Valid,
    /// Words typed so far (no problem yet).
    Typing(usize),
    Problem(String),
}

/// The 24 words of a phrase, numbered for the grid ("1 abandon").
pub fn numbered_words(phrase: &str) -> Vec<(usize, String)> {
    phrase
        .split_whitespace()
        .enumerate()
        .map(|(i, w)| (i + 1, w.to_string()))
        .collect()
}

/// Three different positions (0-based, sorted) to confirm, from a seed.
pub fn confirm_positions(seed: u64) -> [usize; 3] {
    let mut out = [0usize; 3];
    let mut n = 0;
    let mut x = seed | 1;
    while n < 3 {
        // xorshift64*
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let p = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) % RECOVERY_WORDS as u64) as usize;
        if !out[..n].contains(&p) {
            out[n] = p;
            n += 1;
        }
    }
    out.sort_unstable();
    out
}

/// Whether the typed words match the phrase at those positions (case and
/// spaces do not matter; the first four letters of a word are enough, as
/// when the phrase is typed to unlock).
pub fn confirm_matches(phrase: &str, positions: &[usize], typed: &[String]) -> bool {
    let words: Vec<&str> = phrase.split_whitespace().collect();
    positions.len() == typed.len()
        && positions.iter().zip(typed).all(|(p, t)| {
            let t = t.trim().to_lowercase();
            words
                .get(*p)
                .is_some_and(|w| *w == t || (t.len() >= 4 && w.starts_with(&t)))
        })
}

/// What the "Save" and "Print" of a recovery phrase write: who it is for,
/// the numbered words and what to do with it.
pub fn recovery_document(phrase: &str, account: &str, server: &str, date: &str) -> String {
    let words = numbered_words(phrase);
    let mut out = String::new();
    out.push_str(&t!("e2ee.recovery.doc.title"));
    out.push_str("\n\n");
    out.push_str(&t!(
        "e2ee.recovery.doc.account",
        account = account,
        server = server
    ));
    out.push('\n');
    out.push_str(&t!("e2ee.recovery.doc.date", date = date));
    out.push_str("\n\n");
    for row in words.chunks(4) {
        let line: Vec<String> = row
            .iter()
            .map(|(n, w)| format!("{n:>2}. {w:<10}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    out.push('\n');
    out.push_str(&t!("e2ee.recovery.doc.keep"));
    out.push('\n');
    out
}

/// The same as a page to print (opened in the browser).
pub fn recovery_print_page(phrase: &str, account: &str, server: &str, date: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    let cells: String = numbered_words(phrase)
        .iter()
        .map(|(n, w)| format!("<li><span>{n}</span> {}</li>", esc(w)))
        .collect();
    format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
<style>body{{font-family:system-ui,sans-serif;margin:2em auto;max-width:42em;color:#111}}\
ol{{list-style:none;padding:0;display:grid;grid-template-columns:repeat(4,1fr);gap:.6em 1.2em;font:1.1em ui-monospace,monospace}}\
li span{{display:inline-block;width:2em;color:#777;text-align:right;margin-right:.4em}}\
p{{line-height:1.5}}</style>\
<h1>{title}</h1><p>{who}<br>{date}</p><ol>{cells}</ol><p>{keep}</p>\
<script>window.onload=()=>window.print()</script>",
        title = esc(&t!("e2ee.recovery.doc.title")),
        who = esc(&t!(
            "e2ee.recovery.doc.account",
            account = account,
            server = server
        )),
        date = esc(&t!("e2ee.recovery.doc.date", date = date)),
        keep = esc(&t!("e2ee.recovery.doc.keep")),
    )
}

/// How strong an E2EE passphrase looks (a hint, not a rule: the core only
/// asks for 8 characters; Argon2id makes guessing slow).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    TooShort,
    Weak,
    Fair,
    Good,
    Strong,
}

pub fn passphrase_strength(p: &str) -> Strength {
    let len = p.chars().count();
    if len < 8 {
        return Strength::TooShort;
    }
    let classes = [
        p.chars().any(|c| c.is_lowercase()),
        p.chars().any(|c| c.is_uppercase()),
        p.chars().any(|c| c.is_ascii_digit()),
        p.chars().any(|c| !c.is_alphanumeric()),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    let words = p.split_whitespace().filter(|w| w.len() >= 3).count();
    let distinct = {
        let mut v: Vec<char> = p.chars().collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    if distinct <= 3 {
        return Strength::Weak;
    }
    let score = len / 4 + classes + words.min(4);
    match score {
        0..=3 => Strength::Weak,
        4..=5 => Strength::Fair,
        6..=8 => Strength::Good,
        _ => Strength::Strong,
    }
}

pub fn strength_text(s: Strength) -> String {
    match s {
        Strength::TooShort => t!("e2ee.passphrase.strength.too_short"),
        Strength::Weak => t!("e2ee.passphrase.strength.weak"),
        Strength::Fair => t!("e2ee.passphrase.strength.fair"),
        Strength::Good => t!("e2ee.passphrase.strength.good"),
        Strength::Strong => t!("e2ee.passphrase.strength.strong"),
    }
    .to_string()
}

/// Seconds left of a pairing, for "expires in 9:12".
pub fn pairing_left(expires_at: i64, now: i64) -> String {
    let secs = ((expires_at - now) / 1000).max(0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The notice of a sync with E2EE work left: edits waiting for an unlock
/// (they stay on this computer until then).
pub fn waiting_notice(waiting: usize) -> Option<String> {
    (waiting > 0).then(|| tn!("e2ee.notice.waiting", waiting).to_string())
}

// ---------------------------------------------------------------------------
// The book
// ---------------------------------------------------------------------------

/// E2EE state of one account.
#[derive(Debug, Clone, Default)]
pub struct AccountE2ee {
    pub loaded: bool,
    pub loading: bool,
    /// The server has E2EE vaults (`/info features.e2ee`).
    pub supported: bool,
    pub key: Option<UserKeyInfo>,
    /// This computer holds the key.
    pub unlocked: bool,
    pub has_passphrase: bool,
    pub has_recovery: bool,
    /// Other devices of yours waiting for approval (only listed when this
    /// computer is unlocked: it is the one that can approve them).
    pub pairings: Vec<PairingRequest>,
    /// Secrets of E2EE vaults still sealed here.
    pub locked_secrets: usize,
    /// Edits waiting for an unlock (last sync).
    pub waiting: usize,
    pub error: Option<String>,
}

impl AccountE2ee {
    pub fn set_up(&self) -> bool {
        self.key.is_some()
    }
}

/// The E2EE state of every account, shared by every window.
#[derive(Default)]
pub struct E2eeBook {
    accounts: HashMap<Id, AccountE2ee>,
    /// Vault keys this computer already tried to rotate (vault, version):
    /// once each.
    rotations: std::collections::HashSet<(Id, u32)>,
}

struct E2eeBookGlobal(gpui::Entity<E2eeBook>);

impl gpui::Global for E2eeBookGlobal {}

struct Loaded {
    key: Option<UserKeyInfo>,
    unlocked: bool,
    has_passphrase: bool,
    has_recovery: bool,
    pairings: Vec<PairingRequest>,
    locked: usize,
}

async fn load(acc: Arc<Account>) -> Result<Loaded, String> {
    let st = acc.e2ee_status().await.map_err(crate::state::api_error)?;
    let unlocked = acc
        .e2ee_user_key()
        .await
        .map_err(crate::state::api_error)?
        .is_some_and(|k| st.key.as_ref().is_some_and(|p| p.id == k.id));
    let pairings = if unlocked && !st.pairings.is_empty() {
        acc.e2ee_pairings().await.unwrap_or_default()
    } else {
        Vec::new()
    };
    let locked = termoak_client::e2ee::locked_count(&acc.store)
        .await
        .unwrap_or(0);
    Ok(Loaded {
        has_passphrase: st.seals.iter().any(|s| s.kind == SealKind::Passphrase),
        has_recovery: st.seals.iter().any(|s| s.kind == SealKind::Recovery),
        key: st.key,
        unlocked,
        pairings: pairings
            .into_iter()
            .filter(|p| p.pairing.status == "pending")
            .collect(),
        locked,
    })
}

impl E2eeBook {
    pub fn global(cx: &mut gpui::App) -> gpui::Entity<Self> {
        use gpui::AppContext;
        if let Some(g) = cx.try_global::<E2eeBookGlobal>() {
            return g.0.clone();
        }
        let entity = cx.new(|_| E2eeBook::default());
        cx.set_global(E2eeBookGlobal(entity.clone()));
        entity
    }

    pub fn try_global(cx: &gpui::App) -> Option<gpui::Entity<Self>> {
        cx.try_global::<E2eeBookGlobal>().map(|g| g.0.clone())
    }

    pub fn account(&self, id: Id) -> Option<&AccountE2ee> {
        self.accounts.get(&id)
    }

    /// Pairings waiting in every account (the badge).
    pub fn pairing_count(&self) -> usize {
        self.accounts.values().map(|a| a.pairings.len()).sum()
    }

    /// Whether this computer should rotate this vault key now (the first
    /// time it sees it flagged).
    pub fn take_rotation(&mut self, vault: Id, version: u32) -> bool {
        self.rotations.insert((vault, version))
    }

    pub fn forget(&mut self, account: Id, cx: &mut gpui::Context<Self>) {
        if self.accounts.remove(&account).is_some() {
            cx.notify();
        }
    }

    /// The edits waiting for an unlock after a sync.
    pub fn set_sync(
        &mut self,
        account: Id,
        waiting: usize,
        locked: usize,
        cx: &mut gpui::Context<Self>,
    ) {
        let a = self.accounts.entry(account).or_default();
        if a.waiting != waiting || a.locked_secrets != locked {
            a.waiting = waiting;
            a.locked_secrets = locked;
            cx.notify();
        }
    }

    /// Reads the state of an account again. `done` runs afterwards with the
    /// pairings that were not there before (to tell the person).
    pub fn refresh(
        &mut self,
        account: Arc<Account>,
        cx: &mut gpui::Context<Self>,
        done: impl FnOnce(&mut Self, Vec<PairingRequest>, &mut gpui::Context<Self>) + 'static,
    ) {
        let id = account.id;
        let supported = account.e2ee_supported();
        let entry = self.accounts.entry(id).or_default();
        entry.supported = supported;
        if !supported || !account.is_signed_in() {
            entry.loaded = true;
            cx.notify();
            return;
        }
        entry.loading = true;
        cx.notify();
        runtime::run(cx, load(account), move |book: &mut Self, res, cx| {
            let a = book.accounts.entry(id).or_default();
            a.loading = false;
            a.loaded = true;
            let mut new = Vec::new();
            match res {
                Ok(l) => {
                    new = l
                        .pairings
                        .iter()
                        .filter(|p| !a.pairings.iter().any(|o| o.pairing.id == p.pairing.id))
                        .cloned()
                        .collect();
                    a.key = l.key;
                    a.unlocked = l.unlocked;
                    a.has_passphrase = l.has_passphrase;
                    a.has_recovery = l.has_recovery;
                    a.pairings = l.pairings;
                    a.locked_secrets = l.locked;
                    a.error = None;
                }
                Err(e) => a.error = Some(e),
            }
            cx.notify();
            done(book, new, cx);
        });
    }
}

/// An `e2ee` event (`{"type":"e2ee","event":"pairing","pairing":{...}}`)
/// concerns this computer when it is unlocked and the pairing waits.
pub fn is_pairing_event(v: &Value) -> bool {
    v["type"] == "e2ee" && v["event"] == "pairing"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_and_reason_has_texts() {
        for lang in ["en", "es"] {
            rust_i18n::set_locale(lang);
            for code in ERROR_CODES {
                let text = crate::i18n::api_error_text(code);
                assert!(text.is_some(), "{lang}: error.{code}");
            }
            for reason in ["word_count", "unknown_word", "checksum", "old_format"] {
                let t = recovery_reason_text(reason, 3, 12);
                assert!(!t.contains("e2ee."), "{lang}: {reason}: {t}");
            }
        }
        rust_i18n::set_locale("en");
    }

    #[test]
    fn recovery_phrase_feedback() {
        rust_i18n::set_locale("en");
        let rk = termoak_core::e2ee::RecoveryKey::generate();
        let phrase = rk.display().to_string();
        assert_eq!(recovery_feedback(&phrase), RecoveryFeedback::Valid);
        let words: Vec<&str> = phrase.split(' ').collect();
        // Typing: fine so far, the last word may be incomplete.
        let partial = format!("{} {}", words[..5].join(" "), &words[5][..2]);
        assert_eq!(recovery_feedback(&partial), RecoveryFeedback::Typing(6));
        // An unknown word is said at once, with its number.
        let bad = format!("{} qqqq {}", words[0], words[1]);
        match recovery_feedback(&bad) {
            RecoveryFeedback::Problem(t) => assert!(t.contains('2'), "{t}"),
            other => panic!("{other:?}"),
        }
        // The old format.
        assert_eq!(
            recovery_feedback(&"A".repeat(56)),
            RecoveryFeedback::Problem(recovery_reason_text("old_format", 0, 0))
        );
        // Wrong checksum: the last word's low bits.
        let list = termoak_core::e2ee::recovery_wordlist();
        let last = list.iter().position(|w| *w == words[23]).unwrap();
        let mut sum: Vec<&str> = words.clone();
        sum[23] = list[last ^ 1];
        assert_eq!(
            recovery_feedback(&sum.join(" ")),
            RecoveryFeedback::Problem(recovery_reason_text("checksum", 0, 24))
        );
        assert_eq!(numbered_words(&phrase).len(), 24);
        assert_eq!(numbered_words(&phrase)[0], (1, words[0].to_string()));
    }

    #[test]
    fn confirmation_words() {
        for seed in [1u64, 2, 99, u64::MAX, 0xdead_beef] {
            let p = confirm_positions(seed);
            assert!(p[0] < p[1] && p[1] < p[2] && p[2] < RECOVERY_WORDS);
        }
        let rk = termoak_core::e2ee::RecoveryKey::generate();
        let phrase = rk.display().to_string();
        let words: Vec<&str> = phrase.split(' ').collect();
        let pos = [0, 5, 23];
        let typed: Vec<String> = pos.iter().map(|p| words[*p].to_uppercase()).collect();
        assert!(confirm_matches(&phrase, &pos, &typed));
        let short: Vec<String> = pos
            .iter()
            .map(|p| words[*p].chars().take(4).collect())
            .collect();
        assert!(confirm_matches(&phrase, &pos, &short) || words.iter().any(|w| w.len() < 4));
        let wrong: Vec<String> = vec![words[1].into(), words[5].into(), words[23].into()];
        assert!(words[0] == words[1] || !confirm_matches(&phrase, &pos, &wrong));
    }

    #[test]
    fn documents_and_strength() {
        rust_i18n::set_locale("en");
        let rk = termoak_core::e2ee::RecoveryKey::from_bytes([0u8; 32]);
        let phrase = rk.display().to_string();
        let doc = recovery_document(
            &phrase,
            "ana@example.com",
            "https://t.example",
            "2026-10-27",
        );
        assert!(doc.contains("24. art"), "{doc}");
        assert!(doc.contains(" 1. abandon"));
        assert!(doc.contains("ana@example.com"));
        let page = recovery_print_page(&phrase, "a<b>@x", "s", "d");
        assert!(page.contains("a&lt;b&gt;@x"));
        assert!(page.contains("<span>24</span> art"));
        assert_eq!(passphrase_strength("short"), Strength::TooShort);
        assert_eq!(passphrase_strength("aaaaaaaaaaaa"), Strength::Weak);
        assert!(passphrase_strength("correct horse battery staple") >= Strength::Good);
        assert!(passphrase_strength("Tr0ub4dor&3xyz!Q") >= Strength::Good);
        assert_eq!(pairing_left(70_000, 0), "1:10");
        assert_eq!(pairing_left(0, 5_000), "0:00");
    }
}
