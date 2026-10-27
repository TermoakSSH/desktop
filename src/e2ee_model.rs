//! End-to-end encrypted vaults, SSO and the audit in the app model (0.7
//! phase 5b): which accounts have them, the [`E2eeBook`] kept up to date
//! after syncs and `e2ee` events (another device asking for the key), and
//! the notices of a sync with edits waiting for an unlock.

use gpui::Context;
use serde_json::Value;
use termoak_client::SyncReport;
use termoak_core::Id;

use crate::e2ee::{self, E2eeBook};
use crate::state::{AppModel, ToastKind};

impl AppModel {
    fn feature(&self, account: Id, name: &str) -> Value {
        self.account(account)
            .map(|a| a.info.features[name].clone())
            .unwrap_or(Value::Null)
    }

    /// The server of an account has E2EE vaults (`features.e2ee`).
    pub fn e2ee_supported(&self, account: Id) -> bool {
        self.feature(account, "e2ee").as_bool().unwrap_or(false)
    }

    /// The server of an account has team SSO (`features.sso`).
    pub fn sso_supported(&self, account: Id) -> bool {
        self.feature(account, "sso").as_bool().unwrap_or(false)
    }

    /// The server of an account has the extended audit
    /// (`features.audit_v2`: filters, exports, retention, sinks).
    pub fn audit_v2(&self, account: Id) -> Option<Value> {
        let f = self.feature(account, "audit_v2");
        f.is_object().then_some(f)
    }

    /// The E2EE state of the current account (if loaded).
    pub fn e2ee_of(&self, account: Id, cx: &gpui::App) -> Option<e2ee::AccountE2ee> {
        E2eeBook::try_global(cx).and_then(|b| b.read(cx).account(account).cloned())
    }

    /// Reads an account's E2EE state again (status, pairings, locked
    /// secrets); new pairings for this unlocked computer are announced.
    pub fn refresh_e2ee(&mut self, id: Id, cx: &mut Context<Self>) {
        let Some(acc) = self.ws.account(id).filter(|a| a.is_signed_in()) else {
            return;
        };
        let name = self
            .account(id)
            .map(|a| crate::accounts::shown_name(&a.info))
            .unwrap_or_default();
        let several = self.accounts.len() > 1;
        let model = cx.weak_entity();
        let book = E2eeBook::global(cx);
        book.update(cx, |b, cx| {
            b.refresh(acc, cx, move |_, new, cx| {
                if new.is_empty() {
                    return;
                }
                let Some(model) = model.upgrade() else {
                    return;
                };
                model.update(cx, |m, cx| {
                    for p in new {
                        let text = t!(
                            "e2ee.pairing.toast",
                            device = p.pairing.device_name,
                            code = p.code
                        )
                        .to_string();
                        let text = if several {
                            format!("{name}: {text}")
                        } else {
                            text
                        };
                        m.toast(ToastKind::Info, text, cx);
                        cx.emit(crate::state::ModelEvent::E2eePairing {
                            account: id,
                            pairing: p,
                        });
                    }
                });
            })
        });
    }

    /// An `e2ee` event of an account (a pairing asked or decided).
    pub(crate) fn e2ee_event(&mut self, id: Id, v: &Value, cx: &mut Context<Self>) {
        tracing::debug!(account = %id, event = ?v["event"], "e2ee event");
        self.refresh_e2ee(id, cx);
    }

    /// After a sync: the edits waiting for an unlock are said once (when
    /// their number grows), and the locked secrets kept for the badges.
    pub(crate) fn e2ee_synced(&mut self, id: Id, report: &SyncReport, cx: &mut Context<Self>) {
        if !self.e2ee_supported(id) {
            return;
        }
        let book = E2eeBook::global(cx);
        let before = book.read(cx).account(id).map(|a| a.waiting).unwrap_or(0);
        book.update(cx, |b, cx| {
            b.set_sync(id, report.e2ee_waiting, report.e2ee_locked, cx)
        });
        if report.e2ee_waiting > before
            && let Some(text) = e2ee::waiting_notice(report.e2ee_waiting)
        {
            self.toast(ToastKind::Warning, text, cx);
        }
        let loaded = book.read(cx).account(id).is_some_and(|a| a.loaded);
        if !loaded {
            self.refresh_e2ee(id, cx);
        }
        self.rotate_flagged(id, cx);
    }

    /// Someone lost access to an E2EE vault this computer holds the key of:
    /// the next key holder online rotates it (docs-next/E2EE-SSO-AUDIT.md
    /// §2.4), once per key version; a race with another holder ends in
    /// `e2ee_stale` and nothing to do.
    fn rotate_flagged(&mut self, id: Id, cx: &mut Context<Self>) {
        let unlocked = self.e2ee_of(id, cx).is_some_and(|s| s.unlocked);
        if !unlocked {
            return;
        }
        let flagged: Vec<(Id, u32, String)> = self
            .account(id)
            .map(|a| {
                a.vaults
                    .iter()
                    .filter_map(|v| {
                        let e = v.vault.e2ee.as_ref()?;
                        (e.rekey_needed && e.wrap.is_some())
                            .then(|| (v.id(), e.key_version, v.label()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let Some(acc) = self.ws.account(id).filter(|a| a.is_signed_in()) else {
            return;
        };
        let book = E2eeBook::global(cx);
        for (vault, version, name) in flagged {
            if !book.update(cx, |b, _| b.take_rotation(vault, version)) {
                continue;
            }
            tracing::info!(%vault, version, "rotating the key of an E2EE vault (rekey_needed)");
            let acc = acc.clone();
            crate::runtime::run(
                cx,
                async move { acc.e2ee_rotate_vault(vault).await },
                move |m: &mut Self, res, cx| match res {
                    Ok(v) => {
                        m.toast(
                            ToastKind::Info,
                            t!("vault_e2ee.auto_rotated", vault = name, version = v).to_string(),
                            cx,
                        );
                        m.sync_account(id, cx);
                    }
                    Err(e) => tracing::warn!(%vault, error = %e, "automatic E2EE key rotation"),
                },
            );
        }
    }
}
