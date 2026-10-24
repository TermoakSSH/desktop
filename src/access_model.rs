//! Teams security in the app model (0.7 phase 4c): which accounts have it,
//! the effective authentication of hosts and groups (team certificates,
//! inherited from groups), what a host's vault policy allows, and the
//! `access_request` events (the shared [`AccessBook`], toasts).

use gpui::Context;
use serde_json::Value;
use termoak_client::Scope;
use termoak_core::Id;
use termoak_core::model::{Group, Host, HostAuth};

use crate::access::{self, AccessBook, Gates};
use crate::accounts::VaultEntry;
use crate::state::{AppModel, Item};

/// The authentication of a host or group as the editors show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveAuth {
    /// The method that applies (`None`: password/key).
    pub auth: Option<HostAuth>,
    /// It comes from this group (`None`: its own, or none).
    pub from_group: Option<String>,
}

/// The auth of a host or group from its own setting, else the nearest
/// group's (`groups`: the groups of its vault).
pub fn effective_auth(
    own: Option<&HostAuth>,
    group: Option<Id>,
    groups: &[&Group],
) -> EffectiveAuth {
    if let Some(a) = own {
        return EffectiveAuth {
            auth: Some(a.clone()),
            from_group: None,
        };
    }
    let mut next = group;
    let mut seen = 0;
    while let Some(id) = next {
        let Some(g) = groups.iter().find(|g| g.id == id) else {
            break;
        };
        if let Some(a) = &g.settings.auth {
            return EffectiveAuth {
                auth: Some(a.clone()),
                from_group: Some(g.name.clone()),
            };
        }
        next = g.parent_id;
        seen += 1;
        if seen > 16 {
            break;
        }
    }
    EffectiveAuth {
        auth: None,
        from_group: None,
    }
}

impl AppModel {
    /// Signed-in accounts whose server has teams security.
    pub fn access_accounts(&self) -> Vec<Id> {
        self.accounts
            .iter()
            .filter(|a| a.active() && a.info.features["teams_security"].is_object())
            .map(|a| a.id())
            .collect()
    }

    /// The server of an account has teams security.
    pub fn teams_security(&self, account: Id) -> bool {
        self.account(account)
            .is_some_and(|a| a.info.features["teams_security"].is_object())
    }

    /// What the vault's policy lets you do with an item (everything for
    /// This device items).
    pub fn gates_of<T>(&self, item: &Item<T>) -> Gates {
        match self.vault_entry_of(item) {
            Some(e) => access::gates(e.role(), e.vault.policy.as_ref()),
            None => access::gates(termoak_core::model::VaultRole::Manager, None),
        }
    }

    /// Gates of a host by id (everything when unknown).
    pub fn host_gates(&self, id: Id) -> Gates {
        match self.host_record(id) {
            Some(h) => self.gates_of(h),
            None => access::gates(termoak_core::model::VaultRole::Manager, None),
        }
    }

    /// Groups of the same place as an item.
    fn groups_like<T>(&self, item: &Item<T>) -> Vec<&Group> {
        let place = (item.scope, self.vault_of(item));
        self.groups
            .iter()
            .filter(|g| (g.scope, self.vault_of(g)) == place)
            .map(|g| &g.data)
            .collect()
    }

    /// The effective authentication of a host.
    pub fn host_auth(&self, h: &Item<Host>) -> EffectiveAuth {
        let groups = self.groups_like(h);
        effective_auth(h.data.settings.auth.as_ref(), h.data.group_id, &groups)
    }

    /// What a host or group in `place` inherits from `group` (the editors).
    pub fn inherited_auth(&self, place: (Scope, Option<Id>), group: Option<Id>) -> EffectiveAuth {
        let groups: Vec<&Group> = self
            .groups
            .iter()
            .filter(|g| (g.scope, self.vault_of(g)) == place)
            .map(|g| &g.data)
            .collect();
        effective_auth(None, group, &groups)
    }

    /// A host with "Team certificate" whose vault's owner has a CA, in an
    /// account whose server has teams security.
    pub fn team_certificate_usable(&self, h: &Item<Host>) -> bool {
        let Scope::Account(a) = h.scope else {
            return false;
        };
        self.teams_security(a)
            && self.gates_of(h).certificates
            && self
                .host_auth(h)
                .auth
                .is_some_and(|a| a.is_team_certificate())
    }

    /// The vault entry of a place, if it is an account vault.
    pub fn vault_entry_of_place(&self, place: (Scope, Option<Id>)) -> Option<&VaultEntry> {
        match place {
            (Scope::Account(a), Some(v)) => self.vault_entry(a, v),
            _ => None,
        }
    }

    /// After a sync of an account: its access requests, the first time.
    pub(crate) fn access_after_sync(&mut self, id: Id, cx: &mut Context<Self>) {
        if !self.teams_security(id) {
            return;
        }
        let Some(api) = self.api_of(Some(id)) else {
            return;
        };
        let book = AccessBook::global(cx);
        let loaded = book.read(cx).account(id).is_some_and(|a| a.loaded);
        if !loaded {
            book.update(cx, |b, cx| b.refresh(id, api, cx));
        }
    }

    /// An `access_request` event of an account: the book is read again, the
    /// engine forgets its host answers and certificates, and the user hears
    /// about what concerns them.
    pub(crate) fn access_event(&mut self, id: Id, v: &Value, cx: &mut Context<Self>) {
        if let Some(acc) = self.ws.account(id) {
            acc.forget_access();
        }
        if let Some(api) = self.api_of(Some(id)) {
            let book = AccessBook::global(cx);
            book.update(cx, |b, cx| b.on_event(id, api, cx));
        }
        let me = self.account(id).and_then(|a| a.info.user_id);
        tracing::debug!(account = %id, me = ?me, event = ?v["event"], "access request event");
        if let Some((kind, text)) = access::event_notice(me, v) {
            let text = if self.accounts.len() > 1 {
                let name = self
                    .account(id)
                    .map(|a| crate::accounts::shown_name(&a.info))
                    .unwrap_or_default();
                format!("{name}: {text}")
            } else {
                text
            };
            self.toast(kind, text, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_core::model::HostSettings;

    fn group(id: Id, parent: Option<Id>, auth: Option<HostAuth>, name: &str) -> Group {
        Group {
            id,
            name: name.into(),
            parent_id: parent,
            color: None,
            settings: HostSettings {
                auth,
                ..Default::default()
            },
        }
    }

    #[test]
    fn auth_is_inherited_from_the_nearest_group() {
        let (a, b) = (termoak_core::new_id(), termoak_core::new_id());
        let outer = group(a, None, Some(HostAuth::TeamCertificate), "Prod");
        let inner = group(b, Some(a), None, "DB");
        let groups = [&outer, &inner];
        let e = effective_auth(None, Some(b), &groups);
        assert_eq!(e.auth, Some(HostAuth::TeamCertificate));
        assert_eq!(e.from_group.as_deref(), Some("Prod"));
        // Its own wins.
        let own = HostAuth::Other("future".into());
        let e = effective_auth(Some(&own), Some(b), &groups);
        assert_eq!(e.auth, Some(own));
        assert!(e.from_group.is_none());
        // No group: none.
        assert_eq!(effective_auth(None, None, &groups).auth, None);
    }
}
