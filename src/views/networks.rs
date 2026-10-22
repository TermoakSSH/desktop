//! Private networks (0.7): the networks of the accounts in sight, whose
//! server joins them (`termoak-netd`) and reaches their hosts. Only shown
//! when an account's server has `features.networks`; never for This device.
//!
//! - The list: name, kind, vault and state (in the server: down, waiting
//!   for the WireGuard handshake, connected, error), with the plan's usage.
//! - A network: its state and last handshake, the server's public key to
//!   add as a peer on the other side (copy), its configuration, "Bring up"
//!   (which also tests it), "Bring down", the hosts that use it and the
//!   network's devices (WireGuard: its peers' allowed IPs) to add hosts at.
//! - Add or edit: WireGuard with a pasted or imported `.conf`, or with the
//!   private key generated on the server ("Generate key on the server").
//!   The private key is never shown again: it is a secret like a password.
//!   Tailscale and ZeroTier come later (shown, not offered).
//!
//! Changes go through the REST API (the plan's limit and the public key
//! are the server's) and reach the local stores by sync.

use std::collections::HashMap;
use std::time::Duration;

use gpui::{
    AppContext, ClickEvent, ClipboardItem, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, PathPromptOptions, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::Select;
use gpui_component::switch::Switch;
use gpui_component::tooltip::Tooltip;
use gpui_component::{ActiveTheme, Disableable, Sizable, StyledExt, h_flex, v_flex};
use termoak_client::{SaveTarget, Scope};
use termoak_core::Id;
use termoak_core::model::{
    Host, HostSettings, Network, NetworkKind, NetworkSecret, SyncMode, WireguardConfig,
};

use crate::accounts::VaultEntry;
use crate::networks::{self, NetBook, NetPeer, NetState, NetStatus, WgConf, api};
use crate::runtime;
use crate::state::{AppModel, ModelEvent, ToastKind};
use crate::ui::{self, Choice, ChoiceState, IconName};

/// Time between state checks of the networks on screen.
const POLL: Duration = Duration::from_secs(15);

/// A network as the screen shows it (from the local store, or just created
/// here and not synced yet).
#[derive(Clone)]
struct Row {
    account: Id,
    vault: Option<Id>,
    network: Network,
    can_write: bool,
    has_secret: bool,
}

/// Color of a state.
fn state_color(state: NetState, cx: &gpui::App) -> gpui::Hsla {
    let theme = cx.theme();
    match state {
        NetState::Up => theme.success,
        NetState::Waiting | NetState::Starting | NetState::NeedsAuth => theme.warning,
        NetState::Error => theme.danger,
        NetState::Down | NetState::Unknown => theme.muted_foreground,
    }
}

/// Name of a kind ("WireGuard").
pub fn kind_label(kind: &NetworkKind) -> String {
    match kind {
        NetworkKind::Wireguard => "WireGuard".into(),
        NetworkKind::Tailscale => "Tailscale".into(),
        NetworkKind::Zerotier => "ZeroTier".into(),
        NetworkKind::Other(s) => s.clone(),
    }
}

/// "5 s ago", "3 min ago"... from a time in ms.
fn ago(ms: i64) -> SharedString {
    let now = chrono::Utc::now().timestamp_millis();
    crate::host_status::ago_text(Duration::from_millis(now.saturating_sub(ms).max(0) as u64))
}

/// State pill of a network (with its explanation as a tooltip).
pub fn state_pill(
    id: impl Into<gpui::ElementId>,
    state: NetState,
    cx: &gpui::App,
) -> impl IntoElement {
    let hint = state.hint();
    h_flex()
        .id(id.into())
        .flex_shrink_0()
        .child(ui::pill(state.label(), state_color(state, cx)))
        .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
}

/// The state of a network, from what the server said.
pub fn state_of(book: &NetBook, network: &Network) -> NetState {
    book.status(network.id)
        .map(|s| s.state(&network.kind))
        .unwrap_or(NetState::Unknown)
}

/// A WireGuard configuration as a `.conf` without keys but the peers'
/// public ones (to edit it as text).
fn conf_text(wg: &WireguardConfig) -> String {
    let mut out = String::from("[Interface]\n");
    out.push_str(&format!("# {}\n", t!("networks.form.conf_key_kept")));
    if !wg.addresses.is_empty() {
        out.push_str(&format!("Address = {}\n", wg.addresses.join(", ")));
    }
    if !wg.dns.is_empty() {
        out.push_str(&format!("DNS = {}\n", wg.dns.join(", ")));
    }
    if let Some(mtu) = wg.mtu {
        out.push_str(&format!("MTU = {mtu}\n"));
    }
    for p in &wg.peers {
        out.push_str("\n[Peer]\n");
        out.push_str(&format!("PublicKey = {}\n", p.public_key));
        if let Some(e) = &p.endpoint {
            out.push_str(&format!("Endpoint = {e}\n"));
        }
        if !p.allowed_ips.is_empty() {
            out.push_str(&format!("AllowedIPs = {}\n", p.allowed_ips.join(", ")));
        }
        if let Some(k) = p.persistent_keepalive {
            out.push_str(&format!("PersistentKeepalive = {k}\n"));
        }
    }
    out
}

/// Template for a new network.
const CONF_TEMPLATE: &str = "[Interface]\nAddress = 10.0.0.2/32\n# DNS = 10.0.0.1\n\n[Peer]\nPublicKey = \nEndpoint = vpn.example.com:51820\nAllowedIPs = 10.0.0.0/24\nPersistentKeepalive = 25\n";

pub struct NetworksView {
    model: Entity<AppModel>,
    book: Entity<NetBook>,
    selected: Option<Id>,
    /// Networks created or changed here that sync has not brought yet.
    pending: HashMap<Id, Row>,
    /// Public keys generated here (before sync brings them).
    generated: HashMap<Id, String>,
    /// Per account: networks billed to the user and the plan's maximum.
    usage: HashMap<Id, (u64, Option<u64>)>,
    /// Devices of the selected network.
    peers: Option<(Id, Result<Vec<NetPeer>, String>)>,
    loading_peers: bool,
    busy: bool,
    poll: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl NetworksView {
    pub fn new(model: Entity<AppModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let book = NetBook::global(cx);
        let subs = vec![
            cx.observe_in(&model, window, |this, _, _, cx| {
                // What sync brought replaces what was pending.
                let known: Vec<Id> = this
                    .model
                    .read(cx)
                    .networks
                    .iter()
                    .map(|n| n.data.id)
                    .collect();
                this.pending.retain(|id, _| !known.contains(id));
                cx.notify();
            }),
            cx.observe(&book, |_, _, cx| cx.notify()),
            cx.subscribe_in(&model, window, |this, _, ev: &ModelEvent, window, cx| {
                if let ModelEvent::SessionChanged = ev {
                    this.selected = None;
                    this.peers = None;
                    this.pending.clear();
                    this.refresh(window, cx);
                }
            }),
        ];
        Self {
            model,
            book,
            selected: None,
            pending: HashMap::new(),
            generated: HashMap::new(),
            usage: HashMap::new(),
            peers: None,
            loading_peers: false,
            busy: false,
            poll: None,
            _subs: subs,
        }
    }

    /// Entering the section: the plan's usage and every state again.
    pub fn refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // The servers may have gained or lost private networks.
        let in_view: Vec<Id> = self
            .model
            .read(cx)
            .accounts_in_view()
            .into_iter()
            .map(|a| a.id())
            .collect();
        self.model.update(cx, |m, cx| {
            for id in in_view {
                m.refresh_server_info(id, cx);
            }
        });
        let accounts = self.model.read(cx).network_accounts();
        for account in accounts {
            let Some(api) = self.model.read(cx).api_of(Some(account)) else {
                continue;
            };
            runtime::run(
                cx,
                async move { api::usage(&api).await },
                move |this, res, cx| {
                    match res {
                        Ok(u) => {
                            this.usage.insert(account, u);
                        }
                        Err(e) => tracing::debug!(error = %e, "network usage"),
                    }
                    cx.notify();
                },
            );
        }
        let rows = self.rows(cx);
        for r in rows {
            self.refresh_state(&r, true, cx);
        }
        self.model.update(cx, |m, cx| m.sync_now(cx));
    }

    fn refresh_state(&mut self, row: &Row, force: bool, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api_of(Some(row.account)) else {
            return;
        };
        let id = row.network.id;
        self.book.update(cx, |b, cx| b.refresh(api, id, force, cx));
    }

    /// The networks on screen: those of the accounts with networks, plus
    /// the ones created here that sync has not brought yet.
    fn rows(&self, cx: &gpui::App) -> Vec<Row> {
        let m = self.model.read(cx);
        let accounts = m.network_accounts();
        let mut rows: Vec<Row> = m
            .networks
            .iter()
            .filter_map(|n| {
                let account = n.scope.account()?;
                accounts.contains(&account).then(|| Row {
                    account,
                    vault: m.vault_of(n),
                    network: n.data.clone(),
                    can_write: n.access.can_write(),
                    has_secret: n.meta.has_secret,
                })
            })
            .collect();
        for p in self.pending.values() {
            if !rows.iter().any(|r| r.network.id == p.network.id) {
                rows.push(p.clone());
            }
        }
        rows.sort_by_key(|r| r.network.name.to_lowercase());
        rows
    }

    fn row(&self, id: Id, cx: &gpui::App) -> Option<Row> {
        self.rows(cx).into_iter().find(|r| r.network.id == id)
    }

    fn vault_entry(&self, row: &Row, cx: &gpui::App) -> Option<VaultEntry> {
        self.model
            .read(cx)
            .vault_entry(row.account, row.vault?)
            .cloned()
    }

    fn select(&mut self, id: Id, cx: &mut Context<Self>) {
        if self.selected != Some(id) {
            self.selected = Some(id);
            self.peers = None;
        }
        if let Some(row) = self.row(id, cx) {
            self.refresh_state(&row, false, cx);
        }
        cx.notify();
    }

    /// Something changed in the server: sync the account (the editors
    /// read the local store).
    fn changed(&mut self, account: Id, cx: &mut Context<Self>) {
        self.model.update(cx, |m, cx| m.sync_account(account, cx));
        if let Some(api) = self.model.read(cx).api_of(Some(account)) {
            runtime::run(
                cx,
                async move { api::usage(&api).await },
                move |this, res, cx| {
                    if let Ok(u) = res {
                        this.usage.insert(account, u);
                        cx.notify();
                    }
                },
            );
        }
    }

    // ----- Actions -----

    fn up(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api_of(Some(row.account)) else {
            return;
        };
        self.busy = true;
        cx.notify();
        let id = row.network.id;
        let kind = row.network.kind.clone();
        let name = row.network.name.clone();
        runtime::run_in(
            cx,
            window,
            async move { api::up(&api, id).await.map_err(networks::error_text) },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(status) => {
                        let state = status.state(&kind);
                        this.book.update(cx, |b, cx| b.set_status(id, status, cx));
                        match state {
                            NetState::Up => {
                                ui::success(window, cx, t!("networks.up.done", name = name))
                            }
                            NetState::Error => {
                                ui::error(window, cx, t!("networks.up.error", name = name))
                            }
                            other => ui::notify(
                                window,
                                cx,
                                ToastKind::Warning,
                                format!("{} · {}", other.label(), other.hint()),
                            ),
                        }
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn down(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api_of(Some(row.account)) else {
            return;
        };
        self.busy = true;
        cx.notify();
        let id = row.network.id;
        let api2 = api.clone();
        runtime::run_in(
            cx,
            window,
            async move { api::down(&api, id).await.map_err(networks::error_text) },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(()) => {
                        this.book.update(cx, |b, cx| b.refresh(api2, id, true, cx));
                        ui::success(window, cx, t!("networks.down.done"));
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn generate_key(
        &mut self,
        row: Row,
        replace: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(api) = self.model.read(cx).api_of(Some(row.account)) else {
            return;
        };
        self.busy = true;
        cx.notify();
        let id = row.network.id;
        let account = row.account;
        runtime::run_in(
            cx,
            window,
            async move {
                api::generate_key(&api, id, replace)
                    .await
                    .map_err(networks::error_text)
            },
            move |this, res, window, cx| {
                this.busy = false;
                match res {
                    Ok(key) => {
                        this.generated.insert(id, key);
                        if let Some(p) = this.pending.get_mut(&id) {
                            p.has_secret = true;
                        }
                        ui::success(window, cx, t!("networks.key.generated"));
                        this.changed(account, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            },
        );
    }

    fn confirm_replace_key(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("networks.key.replace_title"),
            t!("networks.key.replace_message"),
            t!("networks.key.replace_ok"),
            true,
            move |window, cx| {
                if let Some(v) = weak.upgrade() {
                    let row = row.clone();
                    v.update(cx, |v, cx| v.generate_key(row, true, window, cx));
                }
            },
        );
    }

    fn delete(&mut self, row: Row, window: &mut Window, cx: &mut Context<Self>) {
        let hosts = self.hosts_using(row.network.id, cx).len();
        let message = if hosts > 0 {
            format!(
                "{} {}",
                t!("networks.delete.message", name = row.network.name),
                tn!("networks.delete.hosts", hosts)
            )
        } else {
            t!("networks.delete.message", name = row.network.name).to_string()
        };
        let weak = cx.entity().downgrade();
        ui::confirm(
            window,
            cx,
            t!("networks.delete.title"),
            message,
            t!("common.delete"),
            true,
            move |window, cx| {
                let Some(v) = weak.upgrade() else {
                    return;
                };
                let row = row.clone();
                v.update(cx, |this, cx| {
                    let Some(api) = this.model.read(cx).api_of(Some(row.account)) else {
                        return;
                    };
                    let id = row.network.id;
                    let name = row.network.name.clone();
                    let account = row.account;
                    runtime::run_in(
                        cx,
                        window,
                        async move { api::delete(&api, id).await.map_err(networks::error_text) },
                        move |this, res, window, cx| {
                            match res {
                                Ok(()) => {
                                    this.pending.remove(&id);
                                    if this.selected == Some(id) {
                                        this.selected = None;
                                    }
                                    ui::success(
                                        window,
                                        cx,
                                        t!("networks.delete.done", name = name),
                                    );
                                    this.changed(account, cx);
                                }
                                Err(e) => ui::error(window, cx, e),
                            }
                            cx.notify();
                        },
                    );
                });
            },
        );
    }

    fn load_peers(&mut self, row: &Row, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api_of(Some(row.account)) else {
            return;
        };
        self.loading_peers = true;
        cx.notify();
        let id = row.network.id;
        runtime::run_in(
            cx,
            window,
            async move { api::peers(&api, id).await.map_err(networks::error_text) },
            move |this, res, _, cx| {
                this.loading_peers = false;
                if this.selected == Some(id) {
                    this.peers = Some((id, res));
                }
                cx.notify();
            },
        );
    }

    /// Hosts whose connections go through a network (own or inherited).
    fn hosts_using(&self, network: Id, cx: &gpui::App) -> Vec<(Id, String)> {
        let m = self.model.read(cx);
        m.hosts
            .iter()
            .filter(|h| m.host_network(h.data.id).is_some_and(|n| n.id == network))
            .map(|h| (h.data.id, h.data.label.clone()))
            .collect()
    }

    /// "Add host…" at an address of a device of the network.
    fn add_host(
        &mut self,
        row: Row,
        address: networks::HostAddress,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let addr = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("10.0.0.5")
                .default_value(address.address.clone())
        });
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("networks.add_host.label_placeholder"))
                .default_value(if address.exact { label } else { String::new() })
        });
        ui::focus_later(if address.exact { &name } else { &addr }, window, cx);
        let (a2, n2) = (addr.clone(), name.clone());
        let model = self.model.clone();
        let hint = t!(
            "networks.add_host.address_hint",
            source = address.source,
            network = row.network.name
        );
        ui::open_form_dialog(
            window,
            cx,
            t!("networks.add_host.title", network = row.network.name),
            t!("networks.add_host.ok"),
            440.,
            move |_, cx| {
                v_flex()
                    .gap_3()
                    .child(ui::field_with_hint(
                        t!("networks.add_host.address"),
                        Input::new(&addr),
                        hint.clone(),
                        cx,
                    ))
                    .child(ui::field(t!("common.name"), Input::new(&name), cx))
                    .into_any_element()
            },
            move |window, cx| {
                let address = a2.read(cx).value().trim().to_string();
                if address.is_empty()
                    || address.ends_with('.')
                    || address.contains(char::is_whitespace)
                {
                    ui::error(window, cx, t!("networks.add_host.error_address"));
                    return false;
                }
                let label = Some(n2.read(cx).value().trim().to_string())
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| address.clone());
                let host = Host {
                    id: Id::nil(),
                    label: label.clone(),
                    address,
                    group_id: None,
                    tags: Vec::new(),
                    settings: HostSettings {
                        network_id: Some(row.network.id),
                        ..Default::default()
                    },
                    notes: String::new(),
                    color: None,
                    os: None,
                    os_version: None,
                    favorite: false,
                    protocol: Default::default(),
                    icon: None,
                };
                let target = SaveTarget::Account {
                    account: row.account,
                    vault: row.vault,
                };
                let task = model.update(cx, |m, cx| {
                    m.save_host(host, None, None, Some(SyncMode::Synced), Some(target), cx)
                });
                window
                    .spawn(cx, async move |cx| {
                        let res = task.await;
                        let _ = cx.update(|window, cx| match res {
                            Ok(_) => {
                                ui::success(window, cx, t!("networks.add_host.done", name = label))
                            }
                            Err(e) => ui::error(window, cx, e),
                        });
                    })
                    .detach();
                true
            },
        );
    }

    /// Add (`None`) or edit a network.
    fn open_form(&mut self, editing: Option<Row>, window: &mut Window, cx: &mut Context<Self>) {
        let form = cx.new(|cx| NetworkForm::new(self.model.clone(), editing.clone(), window, cx));
        if form.read(cx).destinations.is_empty() && editing.is_none() {
            ui::error(window, cx, t!("networks.form.no_vault"));
            return;
        }
        let weak = cx.entity().downgrade();
        let f2 = form.clone();
        ui::open_form_dialog(
            window,
            cx,
            if editing.is_some() {
                t!("networks.form.edit_title")
            } else {
                t!("networks.form.new_title")
            },
            t!("common.save"),
            620.,
            move |_, _| f2.clone().into_any_element(),
            move |window, cx| {
                let plan = match form.update(cx, |f, cx| f.plan(cx)) {
                    Ok(p) => p,
                    Err(e) => {
                        ui::error(window, cx, e);
                        return false;
                    }
                };
                if let Some(v) = weak.upgrade() {
                    v.update(cx, |v, cx| v.submit(plan, window, cx));
                }
                true
            },
        );
    }

    fn submit(&mut self, mut plan: SavePlan, window: &mut Window, cx: &mut Context<Self>) {
        let Some(api) = self.model.read(cx).api_of(Some(plan.account)) else {
            ui::error(window, cx, t!("error.not_logged_in"));
            return;
        };
        self.busy = true;
        cx.notify();
        let account = plan.account;
        let generate = plan.generate;
        let replace = plan.replace;
        let creating = plan.vault.is_some();
        let task = runtime::spawn(cx, async move {
            let res = match plan.vault {
                Some(vault) => api::create(&api, vault, &plan.network, plan.secret.as_ref()).await,
                None => api::update(&api, &plan.network, plan.secret.as_ref()).await,
            };
            if let Some(s) = plan.secret.as_mut() {
                s.zeroize();
            }
            let rec = res.map_err(networks::error_text)?;
            // The server makes the private key and keeps it.
            let key = if generate {
                match api::generate_key(&api, rec.data.id, replace).await {
                    Ok(k) => Some(Ok(k)),
                    Err(e) => Some(Err(networks::error_text(e))),
                }
            } else {
                None
            };
            Ok::<_, String>((rec, key))
        });
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match res {
                    Ok((rec, key)) => {
                        let id = rec.data.id;
                        let name = rec.data.name.clone();
                        let mut has_secret = rec.meta.has_secret;
                        match key {
                            Some(Ok(k)) => {
                                this.generated.insert(id, k);
                                has_secret = true;
                            }
                            Some(Err(e)) => ui::error(window, cx, e),
                            None => {}
                        }
                        this.pending.insert(
                            id,
                            Row {
                                account,
                                vault: rec.meta.vault_id,
                                network: rec.data,
                                can_write: true,
                                has_secret,
                            },
                        );
                        this.selected = Some(id);
                        this.peers = None;
                        ui::success(
                            window,
                            cx,
                            if creating {
                                t!("networks.form.created", name = name)
                            } else {
                                t!("networks.form.saved", name = name)
                            },
                        );
                        if let Some(row) = this.pending.get(&id).cloned() {
                            this.refresh_state(&row, true, cx);
                        }
                        this.changed(account, cx);
                    }
                    Err(e) => ui::error(window, cx, e),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ----- Rendering -----

    fn render_usage(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let m = self.model.read(cx);
        let accounts = m.network_accounts();
        let multi = accounts.len() > 1;
        let theme = cx.theme();
        let lines: Vec<gpui::AnyElement> = accounts
            .iter()
            .filter_map(|a| {
                let (used, max) = *self.usage.get(a)?;
                let text = match max {
                    Some(max) => t!("networks.usage", used = used, max = max),
                    None => t!("networks.usage_unlimited", used = used),
                };
                let full = max.is_some_and(|max| used >= max);
                let who = multi
                    .then(|| {
                        m.account(*a)
                            .map(|acc| crate::accounts::AccountRow::new(&acc.info).label())
                    })
                    .flatten();
                Some(
                    v_flex()
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(
                                    who.unwrap_or_else(|| t!("networks.usage_label").to_string()),
                                )
                                .child(
                                    div()
                                        .font_semibold()
                                        .text_color(if full {
                                            theme.warning
                                        } else {
                                            theme.foreground
                                        })
                                        .child(text),
                                ),
                        )
                        .when_some(max.filter(|m| *m > 0), |this, max| {
                            let pct = (used as f32 / max as f32).min(1.0);
                            this.child(
                                div()
                                    .h(px(4.))
                                    .w_full()
                                    .rounded_full()
                                    .bg(theme.border)
                                    .child(
                                        div()
                                            .h_full()
                                            .rounded_full()
                                            .w(gpui::relative(pct))
                                            .bg(if full { theme.warning } else { theme.primary }),
                                    ),
                            )
                        })
                        .into_any_element(),
                )
            })
            .collect();
        (!lines.is_empty()).then(|| v_flex().gap_2().mb_3().children(lines).into_any_element())
    }

    fn render_list(&self, rows: &[Row], cx: &mut Context<Self>) -> gpui::AnyElement {
        let book = self.book.read(cx);
        let states: Vec<NetState> = rows.iter().map(|r| state_of(book, &r.network)).collect();
        let vaults: Vec<String> = rows
            .iter()
            .map(|r| {
                self.vault_entry(r, cx)
                    .map(|v| v.label())
                    .unwrap_or_default()
            })
            .collect();
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .children(rows.iter().enumerate().map(|(i, r)| {
                let id = r.network.id;
                let active = self.selected == Some(id);
                let sub = if vaults[i].is_empty() {
                    kind_label(&r.network.kind)
                } else {
                    format!("{} · {}", kind_label(&r.network.kind), vaults[i])
                };
                h_flex()
                    .id(("network", i))
                    .p_3()
                    .gap_3()
                    .items_center()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(if active { theme.primary } else { theme.border })
                    .bg(theme.secondary)
                    .hover(|s| s.bg(theme.secondary_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.select(id, cx)))
                    .child(
                        div()
                            .size(px(36.))
                            .flex_shrink_0()
                            .rounded(theme.radius)
                            .bg(crate::theme::color_for(&r.network.name))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                ui::icon(IconName::Network)
                                    .size(px(18.))
                                    .text_color(gpui::white()),
                            ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_semibold()
                                    .text_sm()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(r.network.name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(sub),
                            ),
                    )
                    .child(state_pill(("network-state", i), states[i], cx))
            }))
            .into_any_element()
    }

    fn render_detail(
        &self,
        row: Row,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let status: Option<NetStatus> = self.book.read(cx).status(row.network.id).cloned();
        let loading = self.book.read(cx).is_loading(row.network.id);
        let state = status
            .as_ref()
            .map(|s| s.state(&row.network.kind))
            .unwrap_or(NetState::Unknown);
        let vault = self.vault_entry(&row, cx).map(|v| v.label());
        let hosts = self.hosts_using(row.network.id, cx);
        let busy = self.busy;
        let editable = row.can_write;
        let wireguard = row.network.kind == NetworkKind::Wireguard;
        let wg = row.network.wireguard.clone().unwrap_or_default();
        let public_key = self
            .generated
            .get(&row.network.id)
            .cloned()
            .or_else(|| wg.public_key.clone())
            .or_else(|| {
                status
                    .as_ref()
                    .and_then(|s| s.node.as_ref()?.public_key.clone())
            });
        let has_key = row.has_secret || public_key.is_some();
        let theme = cx.theme();
        let mono = ui::mono_family(cx);
        let (r_up, r_down, r_edit, r_del, r_gen, r_rep, r_peers, r_ref) = (
            row.clone(),
            row.clone(),
            row.clone(),
            row.clone(),
            row.clone(),
            row.clone(),
            row.clone(),
            row.clone(),
        );

        let header =
            h_flex()
                .gap_3()
                .items_center()
                .flex_wrap()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .text_xl()
                                        .font_semibold()
                                        .child(row.network.name.clone()),
                                )
                                .child(ui::pill(kind_label(&row.network.kind), theme.info)),
                        )
                        .child(div().text_sm().text_color(theme.muted_foreground).child(
                            match &vault {
                                Some(v) => t!("networks.detail.in_vault", vault = v),
                                None => SharedString::default(),
                            },
                        ))
                        .when(!row.network.description.trim().is_empty(), |this| {
                            this.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(row.network.description.clone()),
                            )
                        }),
                )
                .when(editable, |this| {
                    this.child(
                        Button::new("network-edit")
                            .small()
                            .icon(ui::icon(IconName::Pencil))
                            .label(t!("common.edit"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.open_form(Some(r_edit.clone()), window, cx)
                            })),
                    )
                    .child(
                        Button::new("network-delete")
                            .small()
                            .danger()
                            .icon(ui::icon(IconName::Trash))
                            .label(t!("common.delete"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.delete(r_del.clone(), window, cx)
                            })),
                    )
                });

        // State in the server.
        let mut facts: Vec<(SharedString, String)> = Vec::new();
        if let Some(s) = &status {
            if wireguard {
                facts.push((
                    t!("networks.detail.handshake"),
                    match s.last_handshake_at {
                        Some(at) => ago(at).to_string(),
                        None => t!("networks.detail.never").to_string(),
                    },
                ));
            }
            if let Some(node) = &s.node
                && !node.addresses.is_empty()
            {
                facts.push((
                    t!("networks.detail.node_addresses"),
                    node.addresses.join(", "),
                ));
            }
            if s.rx_bytes.is_some() || s.tx_bytes.is_some() {
                facts.push((
                    t!("networks.detail.traffic"),
                    format!(
                        "↓ {} · ↑ {}",
                        ui::format_bytes(s.rx_bytes.unwrap_or(0)),
                        ui::format_bytes(s.tx_bytes.unwrap_or(0))
                    ),
                ));
            }
            if let Some(n) = s.streams.filter(|n| *n > 0) {
                facts.push((t!("networks.detail.streams"), n.to_string()));
            }
        }
        let message = status
            .as_ref()
            .and_then(|s| s.message.clone())
            .filter(|m| !m.trim().is_empty());
        let state_card = ui::card(cx)
            .p_4()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .flex_wrap()
                    .child(div().font_semibold().child(t!("networks.detail.state")))
                    .child(state_pill("detail-state", state, cx))
                    .when(loading, |this| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("common.loading")),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("network-refresh")
                            .small()
                            .ghost()
                            .icon(ui::icon(IconName::RefreshCw))
                            .tooltip(t!("common.refresh"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.refresh_state(&r_ref, true, cx)
                            })),
                    )
                    .when(editable, |this| {
                        this.child(
                            Button::new("network-up")
                                .small()
                                .primary()
                                .icon(ui::icon(IconName::Power))
                                .label(t!("networks.up.button"))
                                .tooltip(t!("networks.up.tooltip"))
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.up(r_up.clone(), window, cx)
                                })),
                        )
                        .child(
                            Button::new("network-down")
                                .small()
                                .icon(ui::icon(IconName::PowerOff))
                                .label(t!("networks.down.button"))
                                .disabled(busy || matches!(state, NetState::Down))
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.down(r_down.clone(), window, cx)
                                })),
                        )
                    }),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(state.hint()),
            )
            .when_some(message, |this, m| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(if state == NetState::Error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(m),
                )
            })
            .children(facts.into_iter().map(|(k, v)| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(
                        div()
                            .w(px(150.))
                            .flex_shrink_0()
                            .text_color(theme.muted_foreground)
                            .child(k),
                    )
                    .child(div().min_w_0().child(v))
            }));

        // The server's public key, to add as a peer on the other side.
        let key_card = wireguard.then(|| {
            let allowed = wg.addresses.join(", ");
            ui::card(cx)
                .p_4()
                .gap_3()
                .child(div().font_semibold().child(t!("networks.key.title")))
                .map(|this| match public_key.clone() {
                    Some(key) => {
                        let copy = key.clone();
                        this.child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    div()
                                        .id("server-public-key")
                                        .flex_1()
                                        .min_w_0()
                                        .p_2()
                                        .rounded(theme.radius)
                                        .bg(theme.background)
                                        .border_1()
                                        .border_color(theme.border)
                                        .font_family(mono.clone())
                                        .text_sm()
                                        .child(key),
                                )
                                .child(
                                    Button::new("copy-public-key")
                                        .small()
                                        .icon(ui::icon(IconName::Copy))
                                        .label(t!("common.copy"))
                                        .on_click(move |_, window, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(
                                                copy.clone(),
                                            ));
                                            ui::success(window, cx, t!("networks.key.copied"));
                                        }),
                                ),
                        )
                        .child(
                            div().text_sm().text_color(theme.muted_foreground).child(
                                if allowed.is_empty() {
                                    t!("networks.key.instructions_no_address")
                                } else {
                                    t!("networks.key.instructions", addresses = allowed)
                                },
                            ),
                        )
                    }
                    None if has_key => this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("networks.key.pending")),
                    ),
                    None => this.child(
                        div()
                            .text_sm()
                            .text_color(theme.warning)
                            .child(t!("networks.key.missing")),
                    ),
                })
                .when(editable, |this| {
                    this.child(h_flex().gap_2().child(if has_key {
                        Button::new("replace-key")
                            .small()
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("networks.key.replace"))
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.confirm_replace_key(r_rep.clone(), window, cx)
                            }))
                    } else {
                        Button::new("generate-key")
                            .small()
                            .primary()
                            .icon(ui::icon(IconName::KeyRound))
                            .label(t!("networks.key.generate"))
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.generate_key(r_gen.clone(), false, window, cx)
                            }))
                    }))
                })
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t!("networks.key.private_note")),
                )
        });

        // Configuration (no secrets).
        let config_card = wireguard.then(|| {
            let mut lines: Vec<(SharedString, String)> = vec![(
                t!("networks.detail.addresses"),
                if wg.addresses.is_empty() {
                    "—".into()
                } else {
                    wg.addresses.join(", ")
                },
            )];
            if !wg.dns.is_empty() {
                lines.push((t!("networks.detail.dns"), wg.dns.join(", ")));
            }
            if let Some(mtu) = wg.mtu {
                lines.push(("MTU".into(), mtu.to_string()));
            }
            ui::card(cx)
                .p_4()
                .gap_2()
                .child(
                    div()
                        .font_semibold()
                        .child(t!("networks.detail.configuration")),
                )
                .children(lines.into_iter().map(|(k, v)| {
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(
                            div()
                                .w(px(150.))
                                .flex_shrink_0()
                                .text_color(theme.muted_foreground)
                                .child(k),
                        )
                        .child(div().min_w_0().child(v))
                }))
                .children(wg.peers.iter().map(|p| {
                    let key: String = p.public_key.chars().take(12).collect();
                    v_flex()
                        .mt_1()
                        .p_2()
                        .gap_0p5()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .text_sm()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().font_semibold().child(t!("networks.detail.peer")))
                                .child(
                                    div()
                                        .font_family(mono.clone())
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{key}…")),
                                ),
                        )
                        .child(format!(
                            "{}: {}",
                            t!("networks.detail.endpoint"),
                            p.endpoint.clone().unwrap_or_else(|| "—".into())
                        ))
                        .child(format!(
                            "{}: {}",
                            t!("networks.detail.allowed_ips"),
                            if p.allowed_ips.is_empty() {
                                "—".into()
                            } else {
                                p.allowed_ips.join(", ")
                            }
                        ))
                        .when_some(p.persistent_keepalive, |this, k| {
                            this.child(format!("{}: {k} s", t!("networks.detail.keepalive")))
                        })
                }))
        });

        // Hosts that use it.
        let hosts_card = ui::card(cx)
            .p_4()
            .gap_2()
            .child(
                div()
                    .font_semibold()
                    .child(tn!("networks.detail.hosts", hosts.len())),
            )
            .when(hosts.is_empty(), |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("networks.detail.no_hosts")),
                )
            })
            .child(
                h_flex().gap_1().flex_wrap().children(
                    hosts
                        .iter()
                        .map(|(_, l)| ui::pill(l.clone(), theme.muted_foreground)),
                ),
            );

        // Devices of the network, to add hosts at.
        let peers_card = editable.then(|| {
            let peers = self
                .peers
                .as_ref()
                .filter(|(id, _)| *id == row.network.id)
                .map(|(_, p)| p.clone());
            let now = chrono::Utc::now().timestamp_millis();
            ui::card(cx)
                .p_4()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .font_semibold()
                                .child(t!("networks.peers.title")),
                        )
                        .child(
                            Button::new("network-peers")
                                .small()
                                .icon(ui::icon(IconName::Import))
                                .label(if peers.is_some() {
                                    t!("common.refresh")
                                } else {
                                    t!("networks.peers.show")
                                })
                                .disabled(self.loading_peers)
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.load_peers(&r_peers, window, cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(if wireguard {
                            t!("networks.peers.hint_wireguard")
                        } else {
                            t!("networks.peers.hint")
                        }),
                )
                .map(|this| match peers {
                    None => this,
                    Some(Err(e)) => this.child(div().text_sm().text_color(theme.danger).child(e)),
                    Some(Ok(list)) if list.is_empty() => this.child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(t!("networks.peers.empty")),
                    ),
                    Some(Ok(list)) => this.children(list.into_iter().enumerate().map(|(i, p)| {
                        let online = p.online.unwrap_or_else(|| {
                            p.last_seen
                                .is_some_and(|at| networks::handshake_fresh(at, now))
                        });
                        let seen = p.last_seen.map(ago);
                        let title = p.title();
                        let addresses = networks::host_addresses(&p);
                        h_flex()
                            .gap_3()
                            .items_center()
                            .flex_wrap()
                            .p_2()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .child(div().size(px(8.)).rounded_full().bg(if online {
                                theme.success
                            } else {
                                theme.muted_foreground
                            }))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().font_semibold().child(title.clone()))
                                    .child(
                                        div().text_xs().text_color(theme.muted_foreground).child(
                                            match &seen {
                                                Some(s) => format!(
                                                    "{} · {}",
                                                    p.addresses.join(", "),
                                                    t!("networks.peers.seen", ago = s)
                                                ),
                                                None => p.addresses.join(", "),
                                            },
                                        ),
                                    ),
                            )
                            .children(addresses.into_iter().enumerate().map(|(j, a)| {
                                let row = row.clone();
                                let title = title.clone();
                                let label = if a.exact {
                                    t!("networks.peers.add_at", address = a.address)
                                } else {
                                    t!("networks.peers.add_in", subnet = a.source)
                                };
                                Button::new(("peer-add", i * 100 + j))
                                    .xsmall()
                                    .icon(ui::icon(IconName::ServerPlus))
                                    .label(label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.add_host(
                                                row.clone(),
                                                a.clone(),
                                                title.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    ))
                            }))
                    })),
                })
        });

        let _ = window;
        v_flex()
            .gap_4()
            .max_w(px(820.))
            .child(header)
            .child(state_card)
            .children(key_card)
            .children(config_card)
            .child(hosts_card)
            .children(peers_card)
            .when(!editable, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("networks.detail.read_only")),
                )
            })
            .into_any_element()
    }
}

impl Render for NetworksView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let supported = !self.model.read(cx).network_accounts().is_empty();
        if self
            .selected
            .is_none_or(|s| !rows.iter().any(|r| r.network.id == s))
            && let Some(first) = rows.first()
        {
            self.selected = Some(first.network.id);
        }
        // States of what is on screen, every POLL while it is.
        for r in &rows {
            self.refresh_state(r, false, cx);
        }
        if self.poll.is_none() && supported {
            self.poll = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(POLL).await;
                let _ = this.update(cx, |this, cx| {
                    this.poll = None;
                    cx.notify();
                });
            }));
        }
        let can_add = self.model.read(cx).network_accounts().iter().any(|a| {
            self.model.read(cx).account(*a).is_some_and(|acc| {
                acc.vaults.iter().any(VaultEntry::can_write) || acc.vaults.is_empty()
            })
        });
        let usage = self.render_usage(cx);
        let detail = self
            .selected
            .and_then(|id| rows.iter().find(|r| r.network.id == id).cloned())
            .map(|r| self.render_detail(r, window, cx));
        let list = (!rows.is_empty()).then(|| self.render_list(&rows, cx));
        let theme = cx.theme();
        let body: gpui::AnyElement = if !supported {
            ui::empty_state(
                IconName::Network,
                t!("networks.unsupported"),
                t!("networks.unsupported_hint"),
                cx,
            )
            .into_any_element()
        } else if rows.is_empty() {
            v_flex()
                .p_6()
                .gap_4()
                .children(usage)
                .child(ui::empty_state(
                    IconName::Network,
                    t!("networks.empty"),
                    t!("networks.empty_hint"),
                    cx,
                ))
                .into_any_element()
        } else {
            h_flex()
                .size_full()
                .items_start()
                .child(
                    div()
                        .w(px(320.))
                        .h_full()
                        .flex_shrink_0()
                        .border_r_1()
                        .border_color(theme.border)
                        .child(
                            v_flex()
                                .id("network-list")
                                .size_full()
                                .overflow_y_scroll()
                                .p_4()
                                .children(usage)
                                .children(list),
                        ),
                )
                .child(
                    div().flex_1().min_w_0().h_full().child(
                        v_flex()
                            .id("network-detail")
                            .size_full()
                            .overflow_y_scroll()
                            .p_6()
                            .children(detail),
                    ),
                )
                .into_any_element()
        };
        v_flex()
            .size_full()
            .child(ui::section_header(
                t!("networks.title"),
                t!("networks.subtitle"),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("refresh-networks")
                            .icon(ui::icon(IconName::RefreshCw))
                            .label(t!("common.refresh"))
                            .disabled(!supported)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.refresh(window, cx)
                            })),
                    )
                    .child(
                        Button::new("new-network")
                            .primary()
                            .icon(ui::icon(IconName::Plus))
                            .label(t!("networks.add"))
                            .disabled(!supported || !can_add || self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_form(None, window, cx)
                            })),
                    ),
                cx,
            ))
            .child(div().flex_1().min_h_0().child(body))
    }
}

// ----- Add / edit form -----

/// What the form decided: the request to send.
struct SavePlan {
    account: Id,
    /// Creating: the vault (editing: `None`).
    vault: Option<Id>,
    network: Network,
    /// `None`: keep (editing) or none yet.
    secret: Option<NetworkSecret>,
    /// Ask the server for the private key afterwards.
    generate: bool,
    /// ...replacing the stored one.
    replace: bool,
}

/// Where a new network can go: a vault (Editor) of an account with
/// networks.
#[derive(Clone, PartialEq)]
struct Destination {
    account: Id,
    vault: Id,
}

struct NetworkForm {
    editing: Option<Row>,
    destinations: Vec<Destination>,
    destination: Option<ChoiceState<usize>>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    conf: Entity<TextareaState>,
    parsed: Option<Result<WgConf, String>>,
    /// The server makes the private key (when the `.conf` has none it
    /// always does).
    generate: bool,
    /// Whether the last parsed `.conf` had a private key.
    conf_has_key: Option<bool>,
    _subs: Vec<Subscription>,
}

impl NetworkForm {
    fn new(
        model: Entity<AppModel>,
        editing: Option<Row>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let m = model.read(cx);
        let multi = m.network_accounts().len() > 1;
        let mut destinations = Vec::new();
        let mut choices = Vec::new();
        if editing.is_none() {
            for account in m.network_accounts() {
                let Some(acc) = m.account(account) else {
                    continue;
                };
                let label = crate::accounts::AccountRow::new(&acc.info).label();
                for v in acc.vaults.iter().filter(|v| v.can_write()) {
                    choices.push(Choice::new(
                        if multi {
                            format!("{} · {label}", v.label())
                        } else {
                            v.label()
                        },
                        destinations.len(),
                    ));
                    destinations.push(Destination {
                        account,
                        vault: v.id(),
                    });
                }
            }
        }
        // The vault of the picker or the last one used, if it is offered.
        let preferred = match m.new_item_target() {
            SaveTarget::Account { account, vault } => destinations
                .iter()
                .position(|d| d.account == account && (vault.is_none() || Some(d.vault) == vault)),
            _ => None,
        }
        .unwrap_or(0);
        let destination = (!choices.is_empty() && editing.is_none())
            .then(|| ui::choice_state(choices, Some(&preferred), window, cx));
        let net = editing.as_ref().map(|r| r.network.clone());
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("networks.form.name_placeholder"))
                .default_value(net.as_ref().map(|n| n.name.clone()).unwrap_or_default())
        });
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("networks.form.description_placeholder"))
                .default_value(
                    net.as_ref()
                        .map(|n| n.description.clone())
                        .unwrap_or_default(),
                )
        });
        let conf_value = net
            .as_ref()
            .and_then(|n| n.wireguard.as_ref())
            .map(conf_text)
            .unwrap_or_default();
        let conf = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(8, 16)
                .placeholder(CONF_TEMPLATE)
                .default_value(conf_value)
        });
        ui::focus_later(&name, window, cx);
        let subs = vec![
            cx.subscribe_in(&conf, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.reparse(cx);
                }
            }),
        ];
        let mut form = Self {
            editing,
            destinations,
            destination,
            name,
            description,
            conf,
            parsed: None,
            generate: true,
            conf_has_key: None,
            _subs: subs,
        };
        form.reparse(cx);
        form
    }

    fn reparse(&mut self, cx: &mut Context<Self>) {
        let text = self.conf.read(cx).value().to_string();
        self.parsed =
            (!text.trim().is_empty()).then(|| networks::parse_conf(&text).map_err(|e| e.message()));
        // A .conf with its own private key uses it; without, the server
        // makes one (decided again only when that changes, so the switch
        // keeps the user's choice while typing).
        let has_key = matches!(&self.parsed, Some(Ok(c)) if c.private_key.is_some());
        if self.conf_has_key != Some(has_key) {
            self.conf_has_key = Some(has_key);
            self.generate = !has_key && self.editing.as_ref().is_none_or(|r| !r.has_secret);
        }
        cx.notify();
    }

    fn import_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("networks.form.import_prompt")),
        });
        let conf = self.conf.clone();
        let name = self.name.clone();
        window
            .spawn(cx, async move |cx| {
                let Ok(Ok(Some(paths))) = rx.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let text = match std::fs::metadata(&path) {
                    Ok(meta) if meta.len() > 64 * 1024 => {
                        Err(t!("networks.form.import_too_big").to_string())
                    }
                    Ok(_) => std::fs::read_to_string(&path).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let _ = cx.update(|window, cx| match text {
                    Ok(text) => {
                        conf.update(cx, |c, cx| c.set_value(text, window, cx));
                        if name.read(cx).value().trim().is_empty() {
                            name.update(cx, |n, cx| n.set_value(stem, window, cx));
                        }
                    }
                    Err(e) => ui::error(window, cx, e),
                });
            })
            .detach();
    }

    /// The request to send, or why not.
    fn plan(&mut self, cx: &mut Context<Self>) -> Result<SavePlan, String> {
        self.reparse(cx);
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Err(t!("networks.form.error_name").to_string());
        }
        let description = self.description.read(cx).value().trim().to_string();
        let parsed = match self.parsed.clone() {
            Some(Ok(c)) => c,
            Some(Err(e)) => return Err(e),
            None => return Err(t!("networks.form.error_conf").to_string()),
        };
        let (account, vault, mut network) = match &self.editing {
            Some(r) => (r.account, None, r.network.clone()),
            None => {
                let ix = self
                    .destination
                    .as_ref()
                    .and_then(|d| ui::chosen(d, cx))
                    .unwrap_or(0);
                let d = self
                    .destinations
                    .get(ix)
                    .cloned()
                    .ok_or_else(|| t!("networks.form.no_vault").to_string())?;
                (
                    d.account,
                    Some(d.vault),
                    Network {
                        id: Id::nil(),
                        name: String::new(),
                        kind: NetworkKind::Wireguard,
                        description: String::new(),
                        wireguard: None,
                        tailscale: None,
                        zerotier: None,
                    },
                )
            }
        };
        if parsed.private_key.is_none()
            && parsed.preshared_key.is_some()
            && self.editing.as_ref().is_some_and(|r| r.has_secret)
        {
            return Err(t!("networks.form.error_psk_needs_key").to_string());
        }
        network.name = name;
        network.description = description;
        network.kind = NetworkKind::Wireguard;
        let mut config = parsed.config.clone();
        // The server derives it from the private key.
        config.public_key = None;
        network.wireguard = Some(config);
        let secret = match (&parsed.private_key, self.generate) {
            (Some(k), false) => Some(NetworkSecret {
                private_key: Some(k.clone()),
                preshared_key: parsed.preshared_key.clone(),
                ..Default::default()
            }),
            // A preshared key alone (the server's key comes next).
            (_, true) if parsed.preshared_key.is_some() && vault.is_some() => Some(NetworkSecret {
                preshared_key: parsed.preshared_key.clone(),
                ..Default::default()
            }),
            _ => None,
        };
        let has_secret = self.editing.as_ref().is_some_and(|r| r.has_secret);
        Ok(SavePlan {
            account,
            vault,
            network,
            secret,
            generate: self.generate,
            replace: self.generate && has_secret,
        })
    }
}

impl Render for NetworkForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.editing.is_some();
        let has_secret = self.editing.as_ref().is_some_and(|r| r.has_secret);
        let theme = cx.theme();
        let kind_button = |id: &'static str, label: &'static str, on: bool| {
            Button::new(id)
                .small()
                .label(label)
                .map(|b| if on { b.primary() } else { b.ghost() })
                .disabled(!on)
        };
        let summary: gpui::AnyElement = match &self.parsed {
            None => div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(t!("networks.form.conf_hint"))
                .into_any_element(),
            Some(Err(e)) => div()
                .text_xs()
                .text_color(theme.danger)
                .child(e.clone())
                .into_any_element(),
            Some(Ok(c)) => {
                let peers = c.config.peers.len();
                v_flex()
                    .gap_0p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(div().text_color(theme.success).child(t!(
                        "networks.form.conf_ok",
                        addresses = if c.config.addresses.is_empty() {
                            "—".to_string()
                        } else {
                            c.config.addresses.join(", ")
                        },
                        peers = peers
                    )))
                    .child(match (&c.private_key, has_secret) {
                        (Some(_), _) => t!("networks.form.key_from_conf"),
                        (None, true) => t!("networks.form.key_kept"),
                        (None, false) => t!("networks.form.key_generated"),
                    })
                    .when(c.preshared_key.is_some(), |this| {
                        this.child(t!("networks.form.psk_from_conf"))
                    })
                    .when(!c.ignored.is_empty(), |this| {
                        this.child(t!("networks.form.ignored", keys = c.ignored.join(", ")))
                    })
                    .into_any_element()
            }
        };
        let conf_has_key = matches!(&self.parsed, Some(Ok(c)) if c.private_key.is_some());
        v_flex()
            .gap_3()
            .when(!editing, |this| {
                this.child(ui::field(
                    t!("networks.form.kind"),
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_wrap()
                        .child(kind_button("kind-wireguard", "WireGuard", true))
                        .child(kind_button(
                            "kind-tailscale",
                            "Tailscale / Headscale",
                            false,
                        ))
                        .child(kind_button("kind-zerotier", "ZeroTier", false))
                        .child(ui::pill(
                            t!("networks.form.coming_soon"),
                            theme.muted_foreground,
                        )),
                    cx,
                ))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().flex_1().min_w_0().child(ui::field(
                        t!("common.name"),
                        Input::new(&self.name),
                        cx,
                    )))
                    .when_some(self.destination.clone(), |this, d| {
                        this.child(div().w(px(220.)).flex_shrink_0().child(ui::field(
                            t!("networks.form.vault"),
                            Select::new(&d),
                            cx,
                        )))
                    }),
            )
            .child(ui::field(
                t!("networks.form.description"),
                Input::new(&self.description),
                cx,
            ))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_xs()
                                    .font_medium()
                                    .text_color(theme.muted_foreground)
                                    .child(if editing {
                                        t!("networks.form.conf_edit")
                                    } else {
                                        t!("networks.form.conf")
                                    }),
                            )
                            .child(
                                Button::new("conf-template")
                                    .xsmall()
                                    .ghost()
                                    .label(t!("networks.form.template"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.conf.update(cx, |c, cx| {
                                            c.set_value(CONF_TEMPLATE, window, cx)
                                        });
                                    })),
                            )
                            .child(
                                Button::new("conf-import")
                                    .xsmall()
                                    .icon(ui::icon(IconName::FileUp))
                                    .label(t!("networks.form.import"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.import_file(window, cx)
                                    })),
                            ),
                    )
                    .child(Textarea::new(&self.conf))
                    .child(summary),
            )
            .when(conf_has_key, |this| {
                this.child(
                    Switch::new("generate-key")
                        .label(t!("networks.form.generate_instead"))
                        .checked(self.generate)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.generate = *v;
                            cx.notify();
                        })),
                )
            })
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("networks.form.secret_note")),
            )
    }
}

// ----- The "Network" field of the host and group editors -----

/// Choices of the "Network" field for a host or group in `place`: Direct
/// (or the group's, for hosts) and the networks of the same vault.
pub fn network_choices(m: &AppModel, place: (Scope, Option<Id>)) -> Vec<Choice<Option<Id>>> {
    let mut out = vec![Choice::new(t!("networks.field.direct"), None)];
    out.extend(m.networks_in(place).into_iter().map(|n| {
        Choice::new(
            format!("{} ({})", n.data.name, kind_label(&n.data.kind)),
            Some(n.data.id),
        )
    }));
    out
}

/// The field is offered: a place in an account whose server has networks,
/// or a host or group that already has one (to see it and keep it).
pub fn network_field_shown(m: &AppModel, place: (Scope, Option<Id>), current: Option<Id>) -> bool {
    match place.0 {
        Scope::Device => false,
        Scope::Account(a) => m.networks_supported(a) || current.is_some(),
    }
}

/// Help under the "Network" field: the chosen network's state in the
/// server (asking for it when needed), or what "Direct" means here (the
/// group's network, for hosts in a group that has one).
pub fn network_field_hint(
    model: &Entity<AppModel>,
    book: &Entity<NetBook>,
    place: (Scope, Option<Id>),
    chosen: Option<Id>,
    group: Option<Id>,
    cx: &mut gpui::App,
) -> SharedString {
    match chosen {
        Some(id) => {
            let (net, api) = {
                let m = model.read(cx);
                (
                    m.networks_in(place)
                        .into_iter()
                        .find(|n| n.data.id == id)
                        .map(|n| n.data.clone()),
                    place.0.account().and_then(|a| m.api_of(Some(a))),
                )
            };
            let Some(net) = net else {
                return t!("networks.field.unavailable");
            };
            if let Some(api) = api {
                book.update(cx, |b, cx| b.refresh(api, id, false, cx));
            }
            let book = book.read(cx);
            let state = state_of(book, &net);
            let mut text = format!("{} · {}", kind_label(&net.kind), state.label());
            if let Some(at) = book.status(id).and_then(|s| s.last_handshake_at) {
                text.push_str(" · ");
                text.push_str(&t!("networks.field.handshake", ago = ago(at)));
            }
            text.push_str(". ");
            text.push_str(&t!("networks.field.how"));
            text.into()
        }
        None => {
            let m = model.read(cx);
            let groups: Vec<&termoak_core::model::Group> = m
                .groups
                .iter()
                .filter(|g| g.scope == place.0 && m.vault_of(g) == place.1)
                .map(|g| &g.data)
                .collect();
            match networks::inherited_network(group, &groups) {
                Some(net) => {
                    let name = m
                        .network_record(net)
                        .map(|n| n.data.name.clone())
                        .unwrap_or_else(|| t!("networks.missing").to_string());
                    t!("networks.field.inherited", network = name)
                }
                None => t!("networks.field.direct_hint"),
            }
        }
    }
}
