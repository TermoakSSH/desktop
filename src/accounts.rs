//! Accounts and vaults in the interface: the view models behind the account
//! switcher, the vault picker and chips, the Use-only rules, the transfer
//! confirmation and the notices of a sync. Pure functions (no GPUI), so
//! they are tested here; the views only draw what they return.
//!
//! - An **account** is a (server, user) pair signed in on this device; each
//!   one has its own store. "This device" items live in the device store.
//! - A **vault** is where an account item lives (personal, shared or team);
//!   the role in it decides what the user can do (`Manager`, `Editor`,
//!   `Use only`). Rows without a vault are the account's personal vault.

use std::collections::BTreeMap;

use termoak_client::sync::SyncReport;
use termoak_client::{AccountInfo, AccountStatus, ItemAccess, Scope};
use termoak_core::Id;
use termoak_core::model::{EntityKind, Vault, VaultKind, VaultRole};
use termoak_core::transfer::{TransferMode, TransferResult};

/// What the app shows (the account switcher).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// One account, plus This device items.
    Account(Id),
    /// Every account together, plus This device items.
    All,
    /// Only This device items.
    Device,
}

/// The vault picker under the switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VaultFilter {
    #[default]
    All,
    Vault {
        account: Id,
        vault: Id,
    },
    Device,
}

/// A vault of an account, as the last sync left it.
#[derive(Debug, Clone)]
pub struct VaultEntry {
    pub account: Id,
    pub vault: Vault,
}

impl VaultEntry {
    pub fn id(&self) -> Id {
        self.vault.id
    }

    /// The user's role (the personal vault and vaults the server did not
    /// describe count as theirs).
    pub fn role(&self) -> VaultRole {
        match self.vault.kind {
            VaultKind::Personal => VaultRole::Manager,
            _ => self.vault.role.unwrap_or(VaultRole::UseOnly),
        }
    }

    /// Strict: Use-only members connect only through the server.
    pub fn strict(&self) -> bool {
        !self.vault.settings.use_only_local
    }

    pub fn can_write(&self) -> bool {
        self.role().rank() >= VaultRole::Editor.rank()
    }

    pub fn label(&self) -> String {
        vault_label(&self.vault)
    }

    /// Every item in it, as the server counted them.
    pub fn item_count(&self) -> i64 {
        self.vault.item_counts.values().sum()
    }
}

/// Name of a vault: the personal one is translated, not stored per
/// language.
pub fn vault_label(v: &Vault) -> String {
    if v.kind == VaultKind::Personal {
        t!("vaults.personal").to_string()
    } else {
        v.name.clone()
    }
}

/// The vault an item of `scope` is in (`None` for This device, or an
/// account item before its first sync with a server without vaults).
pub fn effective_vault(scope: Scope, vault_id: Option<Id>, personal: Option<Id>) -> Option<Id> {
    match scope {
        Scope::Device => None,
        Scope::Account(_) => vault_id.or(personal),
    }
}

/// Whether an item passes the vault picker.
pub fn filter_matches(filter: VaultFilter, scope: Scope, vault: Option<Id>) -> bool {
    match filter {
        VaultFilter::All => true,
        VaultFilter::Device => scope == Scope::Device,
        VaultFilter::Vault { account, vault: v } => {
            scope == Scope::Account(account) && vault == Some(v)
        }
    }
}

// ----- Accounts -----

/// Letter of an account's avatar.
pub fn avatar_initial(name: &str, email: &str) -> String {
    name.trim()
        .chars()
        .chain(email.trim().chars())
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "?".into())
}

/// One line of the account switcher or of Settings → Accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRow {
    pub id: Id,
    pub email: String,
    pub name: String,
    pub initial: String,
    /// Server host when it is not the official server.
    pub server: Option<String>,
    pub color: Option<String>,
    pub status: AccountStatus,
}

impl AccountRow {
    pub fn new(info: &AccountInfo) -> Self {
        Self {
            id: info.id,
            email: info.email.clone(),
            name: info.name.clone(),
            initial: avatar_initial(&info.name, &info.email),
            server: (!info.official).then(|| info.server_host()),
            color: info.color.clone(),
            status: info.status,
        }
    }

    /// "ana@example.com" or "ana@example.com · ssh.example.com".
    pub fn label(&self) -> String {
        match &self.server {
            Some(s) => format!("{} · {s}", self.email),
            None => self.email.clone(),
        }
    }
}

/// An entry of the account switcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitcherEntry {
    Account { row: AccountRow, selected: bool },
    All { selected: bool },
    Device { selected: bool },
    Add,
    Manage,
}

/// Entries of the account switcher: each account, "All accounts" (with
/// two or more), "This device only" (with at least one), "Add account" and
/// "Manage accounts" (with at least one).
pub fn switcher_entries(accounts: &[AccountInfo], view: ViewMode) -> Vec<SwitcherEntry> {
    let mut out: Vec<SwitcherEntry> = accounts
        .iter()
        .map(|a| SwitcherEntry::Account {
            row: AccountRow::new(a),
            selected: view == ViewMode::Account(a.id),
        })
        .collect();
    if accounts.len() > 1 {
        out.push(SwitcherEntry::All {
            selected: view == ViewMode::All,
        });
    }
    if !accounts.is_empty() {
        out.push(SwitcherEntry::Device {
            selected: view == ViewMode::Device,
        });
    }
    out.push(SwitcherEntry::Add);
    if !accounts.is_empty() {
        out.push(SwitcherEntry::Manage);
    }
    out
}

/// The view that really applies: an account that is gone falls back to the
/// first one, "all" with a single account is that account, and without
/// accounts everything is on this device.
pub fn normalize_view(accounts: &[AccountInfo], view: ViewMode) -> ViewMode {
    match view {
        _ if accounts.is_empty() => ViewMode::Device,
        ViewMode::Account(id) if accounts.iter().any(|a| a.id == id) => view,
        ViewMode::Account(_) => ViewMode::Account(accounts[0].id),
        ViewMode::All if accounts.len() == 1 => ViewMode::Account(accounts[0].id),
        other => other,
    }
}

/// Title and subtitle of the switcher button.
pub fn switcher_title(accounts: &[AccountInfo], view: ViewMode) -> (String, String) {
    match normalize_view(accounts, view) {
        ViewMode::Account(id) => {
            let a = accounts.iter().find(|a| a.id == id).expect("normalized");
            let row = AccountRow::new(a);
            let detail = row
                .server
                .clone()
                .unwrap_or_else(|| a.server_host().to_string());
            (row.email, detail)
        }
        ViewMode::All => (
            t!("accounts.switcher.all").to_string(),
            tn!("accounts.switcher.count", accounts.len()).to_string(),
        ),
        ViewMode::Device => (
            t!("accounts.switcher.device").to_string(),
            if accounts.is_empty() {
                t!("accounts.switcher.no_account").to_string()
            } else {
                t!("accounts.switcher.device_hint").to_string()
            },
        ),
    }
}

/// Account badges on items only make sense with more than one account in
/// sight.
pub fn show_account_badges(accounts_in_view: usize) -> bool {
    accounts_in_view > 1
}

/// The vault picker (and the vault chips) only show when there is more than
/// one place to choose from: the vaults of the accounts in sight, plus This
/// device when it has items.
pub fn show_vault_picker(vaults_in_view: usize, device_items: bool) -> bool {
    vaults_in_view + usize::from(device_items) > 1
}

/// Accounts in sight in a view.
pub fn accounts_in_view(accounts: &[AccountInfo], view: ViewMode) -> Vec<Id> {
    match normalize_view(accounts, view) {
        ViewMode::Account(id) => vec![id],
        ViewMode::All => accounts.iter().map(|a| a.id).collect(),
        ViewMode::Device => Vec::new(),
    }
}

/// Text of an account's status.
pub fn status_text(status: AccountStatus, signed_in: bool) -> String {
    match status {
        AccountStatus::Active if signed_in => t!("accounts.status.active"),
        AccountStatus::Unverified => t!("accounts.status.unverified"),
        _ => t!("accounts.status.needs_sign_in"),
    }
    .to_string()
}

// ----- Places (account → vault) -----

/// Where items are shown together: This device, or a vault of an account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Place {
    /// Index of the account in the registry, then of the vault in the
    /// account's listing (personal first).
    Vault {
        account: usize,
        vault: usize,
    },
    /// Items of an account without a known vault (servers without vaults).
    Account {
        account: usize,
    },
    Device,
}

/// Place of an item, given the accounts and vaults in order.
pub fn place_of(
    accounts: &[AccountInfo],
    vaults: &[VaultEntry],
    scope: Scope,
    vault: Option<Id>,
) -> Place {
    let Scope::Account(acc) = scope else {
        return Place::Device;
    };
    let account = accounts
        .iter()
        .position(|a| a.id == acc)
        .unwrap_or(usize::MAX);
    match vault.and_then(|v| {
        vaults
            .iter()
            .filter(|e| e.account == acc)
            .position(|e| e.id() == v)
    }) {
        Some(ix) => Place::Vault { account, vault: ix },
        None => Place::Account { account },
    }
}

// ----- What the user can do with an item (Use-only rules) -----

/// How a connection to a host is made from this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectMode {
    /// With the credentials stored on this device.
    Local,
    /// Use-only: with just-in-time credentials from the server (online).
    LocalJustInTime,
    /// Use-only in a Strict vault: a server session instead.
    ServerOnly,
}

/// What the interface offers for an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub edit: bool,
    pub delete: bool,
    pub duplicate: bool,
    /// Reveal or copy a password, export a private key.
    pub reveal: bool,
    /// Move it to another vault (or to This device).
    pub move_out: bool,
    /// Copy it elsewhere (Use-only: only snippets, without secrets).
    pub copy_out: bool,
    pub connect: ConnectMode,
    /// Show the "Use only" lock badge.
    pub use_only_badge: bool,
}

/// The Use-only rules: Use-only members never see or change an item, but
/// use it (just-in-time credentials, or only through the server in Strict
/// vaults); snippets can still be copied out.
pub fn caps(access: ItemAccess, strict: bool, kind: EntityKind) -> Caps {
    if access.can_write() {
        return Caps {
            edit: true,
            delete: true,
            duplicate: true,
            reveal: true,
            move_out: true,
            copy_out: true,
            connect: ConnectMode::Local,
            use_only_badge: false,
        };
    }
    Caps {
        edit: false,
        delete: false,
        duplicate: false,
        reveal: false,
        move_out: false,
        copy_out: kind == EntityKind::Snippet,
        connect: if strict {
            ConnectMode::ServerOnly
        } else {
            ConnectMode::LocalJustInTime
        },
        use_only_badge: true,
    }
}

/// Opening a terminal on a host: from this device or through the server.
/// `Err` is the error code to show (`use_only_needs_server` when a
/// Use-only host has no signed-in server to ask).
pub fn connect_route(mode: ConnectMode, signed_in: bool) -> Result<bool, &'static str> {
    match mode {
        ConnectMode::Local => Ok(false),
        ConnectMode::LocalJustInTime if signed_in => Ok(false),
        ConnectMode::ServerOnly if signed_in => Ok(true),
        ConnectMode::ServerOnly => Err(termoak_core::error::codes::USE_ONLY_STRICT),
        ConnectMode::LocalJustInTime => Err(termoak_core::error::codes::USE_ONLY_NEEDS_SERVER),
    }
}

// ----- Destinations (new items, move and copy) -----

/// A place items can go to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub scope: Scope,
    /// Vault of an account (`None`: its personal vault, or This device).
    pub vault: Option<Id>,
    pub label: String,
}

/// Where the user can put items: every vault they can write to in the
/// accounts given (personal first, as listed), accounts without vaults as
/// a whole, and This device. With several accounts the label says which.
pub fn destinations(
    accounts: &[AccountInfo],
    vaults: &[VaultEntry],
    include_device: bool,
) -> Vec<Destination> {
    let multi = accounts.len() > 1;
    let mut out = Vec::new();
    for a in accounts {
        if a.status != AccountStatus::Active && a.status != AccountStatus::NeedsSignIn {
            continue;
        }
        let mine: Vec<&VaultEntry> = vaults.iter().filter(|v| v.account == a.id).collect();
        if mine.is_empty() {
            out.push(Destination {
                scope: Scope::Account(a.id),
                vault: None,
                label: a.email.clone(),
            });
            continue;
        }
        for v in mine.into_iter().filter(|v| v.can_write()) {
            out.push(Destination {
                scope: Scope::Account(a.id),
                vault: Some(v.id()),
                label: if multi {
                    format!("{} · {}", a.email, v.label())
                } else {
                    v.label()
                },
            });
        }
    }
    if include_device {
        out.push(Destination {
            scope: Scope::Device,
            vault: None,
            label: t!("accounts.switcher.device").to_string(),
        });
    }
    out
}

// ----- Transfer confirmation -----

/// Name of an entity kind, in lowercase ("host", "key"...).
pub fn kind_name(kind: EntityKind) -> String {
    match kind {
        EntityKind::Group => t!("kind.group"),
        EntityKind::Host => t!("kind.host"),
        EntityKind::Identity => t!("kind.identity"),
        EntityKind::Key => t!("kind.key"),
        EntityKind::Snippet => t!("kind.snippet"),
        EntityKind::Forward => t!("kind.forward"),
        EntityKind::KnownHost => t!("kind.known_host"),
        EntityKind::Memory => t!("kind.memory"),
    }
    .to_string()
}

/// Headline of the confirmation of a move or copy.
pub fn transfer_headline(mode: TransferMode, items: usize, target: &str) -> String {
    match mode {
        TransferMode::Move => tn!("transfer.headline.move", items, target = target),
        TransferMode::Copy => tn!("transfer.headline.copy", items, target = target),
    }
    .to_string()
}

/// Lines of the confirmation (from the dry run): what moves, what is
/// copied (the chosen items, and what they use), what is reused, which
/// references are cleared and the warnings. `requested` are the chosen
/// items; `name` gives the label of an item of the source.
pub fn transfer_lines(
    result: &TransferResult,
    mode: TransferMode,
    requested: &[Id],
    name: impl Fn(EntityKind, Id) -> String,
) -> Vec<String> {
    let item = |kind: EntityKind, id: Id| {
        t!(
            "transfer.item",
            kind = kind_name(kind),
            name = name(kind, id)
        )
        .to_string()
    };
    let mut out = Vec::new();
    for m in &result.moved {
        out.push(t!("transfer.line.moved", item = item(m.kind, m.id)).to_string());
    }
    for c in &result.copied {
        let key = if requested.contains(&c.from) {
            "transfer.line.copied"
        } else if mode == TransferMode::Move {
            "transfer.line.copied_shared"
        } else {
            "transfer.line.copied_dependency"
        };
        out.push(t!(key, item = item(c.kind, c.from)).to_string());
    }
    for r in &result.reused {
        out.push(t!("transfer.line.reused", item = item(r.kind, r.from)).to_string());
    }
    for d in &result.detached {
        out.push(
            t!(
                "transfer.line.detached",
                field = field_name(&d.field),
                item = item(d.kind, d.id)
            )
            .to_string(),
        );
    }
    for w in &result.warnings {
        out.push(
            t!(
                "transfer.line.warning",
                warning = crate::i18n::api_error_text(&w.code).unwrap_or_else(|| w.code.clone()),
                item = item(w.kind, w.id)
            )
            .to_string(),
        );
    }
    out
}

/// Name of a reference field cleared by a transfer.
fn field_name(field: &str) -> String {
    let key = match field.rsplit('.').next().unwrap_or(field) {
        "group_id" | "parent_id" => "transfer.field.group",
        "identity_id" => "transfer.field.identity",
        "key_id" => "transfer.field.key",
        "jump_host_ids" => "transfer.field.jumps",
        "startup_snippet_id" => "transfer.field.startup_snippet",
        "host_id" => "transfer.field.host",
        _ => return field.to_string(),
    };
    t!(key).to_string()
}

// ----- Sync notices -----

/// How important a notice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeLevel {
    Info,
    Warning,
}

/// Notices of a sync: vaults shared with you, vaults you lost (with the
/// changes that were discarded) and changes the server refused.
pub fn sync_notices(report: &SyncReport) -> Vec<(NoticeLevel, String)> {
    let mut out = Vec::new();
    let discarded: BTreeMap<Id, usize> = report
        .discarded
        .iter()
        .map(|d| (d.vault_id, d.count))
        .collect();
    for v in &report.vaults_added {
        out.push((
            NoticeLevel::Info,
            t!("accounts.notice.vault_added", vault = v.name).to_string(),
        ));
    }
    for v in &report.vaults_lost {
        let lost = discarded.get(&v.id).copied().unwrap_or(0);
        out.push((
            NoticeLevel::Warning,
            if lost > 0 {
                tn!("accounts.notice.vault_lost_discarded", lost, vault = v.name).to_string()
            } else {
                t!("accounts.notice.vault_lost", vault = v.name).to_string()
            },
        ));
    }
    for d in &report.discarded {
        if d.count > 0 && !report.vaults_lost.iter().any(|v| v.id == d.vault_id) {
            out.push((
                NoticeLevel::Warning,
                tn!("accounts.notice.discarded", d.count, vault = d.vault_name).to_string(),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_client::sync::{DiscardedChanges, VaultRef};
    use termoak_core::transfer::{CopiedItem, DetachedRef, ItemRef};

    fn id(n: u128) -> Id {
        uuid::Uuid::from_u128(n)
    }

    fn account(n: u128, email: &str, official: bool) -> AccountInfo {
        AccountInfo {
            id: id(n),
            server_url: if official {
                "https://termoak.com".into()
            } else {
                "https://ssh.example.com".into()
            },
            instance_id: None,
            official,
            user_id: Some(id(n + 1000)),
            email: email.into(),
            name: String::new(),
            status: AccountStatus::Active,
            features: serde_json::json!({"sync_v2": true}),
            color: None,
            position: n as i64,
            added_at: 0,
            last_used_at: None,
            last_sync_at: None,
        }
    }

    fn vault(account: Id, n: u128, kind: &str, role: &str, name: &str) -> VaultEntry {
        let v: Vault = serde_json::from_value(serde_json::json!({
            "id": id(n), "kind": kind, "name": name, "role": role,
            "settings": {"use_only_local": role != "use_only" || n % 2 == 0},
            "created_by": id(1), "created_at": 0, "updated_at": 0,
            "item_counts": {"host": 3, "key": 1}
        }))
        .unwrap();
        VaultEntry { account, vault: v }
    }

    #[test]
    fn switcher_lists_accounts_all_device_add_manage() {
        let none = switcher_entries(&[], ViewMode::Device);
        assert_eq!(none, vec![SwitcherEntry::Add]);

        let one = [account(1, "ana@example.com", true)];
        let e = switcher_entries(&one, ViewMode::Account(id(1)));
        assert_eq!(e.len(), 4);
        assert!(
            matches!(&e[0], SwitcherEntry::Account { selected: true, row } if row.server.is_none())
        );
        assert_eq!(e[1], SwitcherEntry::Device { selected: false });
        assert_eq!(e[2], SwitcherEntry::Add);
        assert_eq!(e[3], SwitcherEntry::Manage);

        let two = [
            account(1, "ana@example.com", true),
            account(2, "ana@work.com", false),
        ];
        let e = switcher_entries(&two, ViewMode::All);
        assert!(
            matches!(&e[1], SwitcherEntry::Account { selected: false, row }
            if row.server.as_deref() == Some("ssh.example.com")
            && row.label() == "ana@work.com · ssh.example.com")
        );
        assert_eq!(e[2], SwitcherEntry::All { selected: true });
        assert_eq!(e[3], SwitcherEntry::Device { selected: false });
    }

    #[test]
    fn views_are_normalized() {
        let one = [account(1, "a@x.com", true)];
        let two = [account(1, "a@x.com", true), account(2, "b@x.com", true)];
        assert_eq!(normalize_view(&[], ViewMode::All), ViewMode::Device);
        assert_eq!(
            normalize_view(&one, ViewMode::All),
            ViewMode::Account(id(1))
        );
        assert_eq!(
            normalize_view(&two, ViewMode::Account(id(9))),
            ViewMode::Account(id(1))
        );
        assert_eq!(normalize_view(&two, ViewMode::Device), ViewMode::Device);
        assert_eq!(accounts_in_view(&two, ViewMode::All).len(), 2);
        assert!(accounts_in_view(&two, ViewMode::Device).is_empty());
        assert_eq!(
            switcher_title(&two, ViewMode::All),
            ("All accounts".to_string(), "2 accounts".to_string())
        );
        assert_eq!(
            switcher_title(&one, ViewMode::Account(id(1))).0,
            "a@x.com".to_string()
        );
        assert_eq!(switcher_title(&[], ViewMode::Device).0, "This device");
    }

    #[test]
    fn avatars_and_badges() {
        assert_eq!(avatar_initial("ana", "x@y.z"), "A");
        assert_eq!(avatar_initial("  ", "élodie@y.z"), "É");
        assert_eq!(avatar_initial("", ""), "?");
        assert!(!show_account_badges(1));
        assert!(show_account_badges(2));
        assert!(!show_vault_picker(1, false));
        assert!(show_vault_picker(1, true));
        assert!(show_vault_picker(2, false));
        assert!(!show_vault_picker(0, true));
    }

    #[test]
    fn vault_chips_and_filtering() {
        let a = id(1);
        let personal = vault(a, 1001, "personal", "manager", "Personal");
        let ops = vault(a, 20, "team", "use_only", "Ops");
        assert_eq!(personal.label(), "Personal");
        assert_eq!(ops.label(), "Ops");
        assert_eq!(personal.role(), VaultRole::Manager);
        assert!(!ops.can_write() && ops.strict() == false);
        assert_eq!(ops.item_count(), 4);

        // Rows without a vault are the personal vault.
        let scope = Scope::Account(a);
        assert_eq!(effective_vault(scope, None, Some(id(1001))), Some(id(1001)));
        assert_eq!(
            effective_vault(Scope::Device, Some(id(5)), Some(id(1))),
            None
        );

        let f = VaultFilter::Vault {
            account: a,
            vault: id(20),
        };
        assert!(filter_matches(f, scope, Some(id(20))));
        assert!(!filter_matches(f, scope, Some(id(1001))));
        assert!(!filter_matches(f, Scope::Account(id(2)), Some(id(20))));
        assert!(filter_matches(VaultFilter::Device, Scope::Device, None));
        assert!(!filter_matches(VaultFilter::Device, scope, None));
        assert!(filter_matches(VaultFilter::All, scope, None));

        // Grouping: account → vault, This device last.
        let accounts = [account(1, "a@x.com", true), account(2, "b@x.com", true)];
        let vaults = vec![personal.clone(), ops.clone()];
        let p1 = place_of(&accounts, &vaults, scope, Some(id(1001)));
        let p2 = place_of(&accounts, &vaults, scope, Some(id(20)));
        let p3 = place_of(&accounts, &vaults, Scope::Account(id(2)), None);
        let p4 = place_of(&accounts, &vaults, Scope::Device, None);
        let mut places = vec![p4, p3, p2, p1];
        places.sort();
        assert_eq!(places, vec![p1, p2, p3, p4]);
        assert_eq!(p3, Place::Account { account: 1 });
    }

    #[test]
    fn destinations_are_the_writable_vaults() {
        let a = id(1);
        let accounts = [account(1, "a@x.com", true)];
        let vaults = vec![
            vault(a, 1001, "personal", "manager", "Personal"),
            vault(a, 20, "team", "use_only", "Ops"),
            vault(a, 30, "shared", "editor", "Web"),
        ];
        let d = destinations(&accounts, &vaults, true);
        let labels: Vec<&str> = d.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, vec!["Personal", "Web", "This device"]);
        assert_eq!(d[1].vault, Some(id(30)));

        let two = [account(1, "a@x.com", true), account(2, "b@x.com", true)];
        let d = destinations(&two, &vaults, false);
        let labels: Vec<&str> = d.iter().map(|d| d.label.as_str()).collect();
        // The second account has no vaults (old server): it is one place.
        assert_eq!(
            labels,
            vec!["a@x.com · Personal", "a@x.com · Web", "b@x.com"]
        );
    }

    #[test]
    fn use_only_rules() {
        let editor = caps(ItemAccess::Editor, true, EntityKind::Host);
        assert!(editor.edit && editor.reveal && editor.duplicate && !editor.use_only_badge);
        assert_eq!(editor.connect, ConnectMode::Local);
        assert_eq!(
            caps(ItemAccess::Device, false, EntityKind::Key).connect,
            ConnectMode::Local
        );

        let jit = caps(ItemAccess::UseOnly, false, EntityKind::Host);
        assert!(!jit.edit && !jit.delete && !jit.duplicate && !jit.reveal && !jit.move_out);
        assert!(!jit.copy_out && jit.use_only_badge);
        assert_eq!(jit.connect, ConnectMode::LocalJustInTime);
        assert!(caps(ItemAccess::UseOnly, false, EntityKind::Snippet).copy_out);

        let strict = caps(ItemAccess::UseOnly, true, EntityKind::Host);
        assert_eq!(strict.connect, ConnectMode::ServerOnly);

        assert_eq!(connect_route(ConnectMode::Local, false), Ok(false));
        assert_eq!(connect_route(ConnectMode::LocalJustInTime, true), Ok(false));
        assert_eq!(
            connect_route(ConnectMode::LocalJustInTime, false),
            Err("use_only_needs_server")
        );
        assert_eq!(connect_route(ConnectMode::ServerOnly, true), Ok(true));
        assert_eq!(
            connect_route(ConnectMode::ServerOnly, false),
            Err("use_only_strict")
        );
    }

    #[test]
    fn transfer_confirmation_text() {
        let result = TransferResult {
            moved: vec![ItemRef {
                kind: EntityKind::Host,
                id: id(1),
            }],
            copied: vec![CopiedItem {
                kind: EntityKind::Key,
                from: id(2),
                to: id(3),
            }],
            reused: vec![],
            detached: vec![DetachedRef {
                kind: EntityKind::Host,
                id: id(1),
                field: "settings.jump_host_ids".into(),
            }],
            warnings: vec![],
            rev: 0,
            dry_run: true,
        };
        let names = |_: EntityKind, i: Id| {
            if i == id(1) {
                "web".into()
            } else {
                "deploy".into()
            }
        };
        assert_eq!(
            transfer_lines(&result, TransferMode::Move, &[id(1)], names),
            vec![
                "Moves host “web”".to_string(),
                "Also copies key “deploy” (items that stay still use it)".to_string(),
                "Clears the jump hosts of host “web”".to_string(),
            ]
        );
        let copy = TransferResult {
            copied: vec![
                CopiedItem {
                    kind: EntityKind::Host,
                    from: id(1),
                    to: id(4),
                },
                CopiedItem {
                    kind: EntityKind::Key,
                    from: id(2),
                    to: id(3),
                },
            ],
            reused: vec![CopiedItem {
                kind: EntityKind::Identity,
                from: id(5),
                to: id(6),
            }],
            ..Default::default()
        };
        assert_eq!(
            transfer_lines(&copy, TransferMode::Copy, &[id(1)], names),
            vec![
                "Copies host “web”".to_string(),
                "Also copies key “deploy” (the copied items use it)".to_string(),
                "Uses identity “deploy”, already there".to_string(),
            ]
        );
        assert_eq!(
            transfer_headline(TransferMode::Move, 1, "Ops"),
            "Move 1 item to Ops?"
        );
        assert_eq!(
            transfer_headline(TransferMode::Copy, 3, "This device"),
            "Copy 3 items to This device?"
        );
    }

    #[test]
    fn sync_report_notices() {
        let report = SyncReport {
            vaults_added: vec![VaultRef {
                id: id(1),
                name: "Web".into(),
            }],
            vaults_lost: vec![VaultRef {
                id: id(2),
                name: "Ops".into(),
            }],
            discarded: vec![
                DiscardedChanges {
                    vault_id: id(2),
                    vault_name: "Ops".into(),
                    count: 2,
                },
                DiscardedChanges {
                    vault_id: id(3),
                    vault_name: "Db".into(),
                    count: 1,
                },
            ],
            ..Default::default()
        };
        let notices = sync_notices(&report);
        assert_eq!(
            notices,
            vec![
                (
                    NoticeLevel::Info,
                    "Vault “Web” was shared with you".to_string()
                ),
                (
                    NoticeLevel::Warning,
                    "You no longer have access to Ops; 2 unsynced changes were discarded"
                        .to_string()
                ),
                (
                    NoticeLevel::Warning,
                    "1 change in Db was discarded: you can no longer change that vault".to_string()
                ),
            ]
        );
        assert!(sync_notices(&SyncReport::default()).is_empty());
    }
}
