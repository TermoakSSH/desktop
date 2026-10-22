//! Private networks (0.7): what the interface needs besides the views.
//!
//! The server joins the networks (`termoak-netd`); the app manages them and
//! reaches their hosts through the server relay (the core does that when it
//! connects). Here:
//!
//! - reading a WireGuard `.conf` into a network's configuration and secret;
//! - the server's view of a network: its state in netd (`/status`), its
//!   peers (`/peers`) and the host test through it;
//! - the network of a host (its own or inherited from its groups, only from
//!   its own vault, as the core resolves it);
//! - friendly texts for the network errors of the API.
//!
//! Networks are created, changed and deleted through the REST API (the
//! server checks the plan's `max_networks` and fills in the WireGuard
//! public key at once) and then reach the local stores by sync, where the
//! host editors find them.

use std::collections::HashSet;

use base64::Engine;
use serde::Deserialize;
use serde_json::Value;
use termoak_client::ClientError;
use termoak_core::Id;
use termoak_core::model::{Group, Host, NetworkKind, WireguardConfig, WireguardPeer};

/// Feature of `/api/v1/info` that tells the server can reach private
/// networks.
pub const FEATURE: &str = "networks";

/// Most levels of nested groups followed for an inherited network (as the
/// core).
const MAX_DEPTH: usize = 16;

/// A WireGuard peer counts as online when its last handshake is this
/// recent (as netd says for `peers`).
const HANDSHAKE_FRESH_MS: i64 = 3 * 60 * 1000;

/// Whether a server's `/info.features` has private networks.
pub fn supported(features: &Value) -> bool {
    features[FEATURE].as_bool().unwrap_or(false)
}

// ----- WireGuard .conf -----

/// What a WireGuard `.conf` holds, split as Termoak stores it: the
/// configuration (no keys but the peers' public ones) and the secret.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WgConf {
    pub config: WireguardConfig,
    pub private_key: Option<String>,
    pub preshared_key: Option<String>,
    /// Keys of the file Termoak does not use (`ListenPort`, `PostUp`...).
    pub ignored: Vec<String>,
}

/// Why a `.conf` cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfError {
    /// Nothing in it, or no `[Interface]`/`[Peer]` section.
    NotAConf,
    /// A line that is not `Key = Value` nor a section (its number).
    BadLine(usize),
    /// A `[Peer]` without `PublicKey`.
    PeerWithoutKey,
    /// No `[Peer]` at all.
    NoPeer,
    /// A key that is not 32 bytes in base64 (which one).
    BadKey(&'static str),
    /// A number that is not one (`MTU`, `PersistentKeepalive`).
    BadNumber(&'static str),
    /// Several peers with different preshared keys (Termoak keeps one for
    /// the network).
    MixedPresharedKeys,
}

impl ConfError {
    pub fn message(&self) -> String {
        match self {
            ConfError::NotAConf => t!("networks.conf.error.not_a_conf").to_string(),
            ConfError::BadLine(n) => t!("networks.conf.error.bad_line", line = n).to_string(),
            ConfError::PeerWithoutKey => t!("networks.conf.error.peer_without_key").to_string(),
            ConfError::NoPeer => t!("networks.conf.error.no_peer").to_string(),
            ConfError::BadKey(field) => {
                t!("networks.conf.error.bad_key", field = field).to_string()
            }
            ConfError::BadNumber(field) => {
                t!("networks.conf.error.bad_number", field = field).to_string()
            }
            ConfError::MixedPresharedKeys => t!("networks.conf.error.mixed_psk").to_string(),
        }
    }
}

/// A WireGuard key: 32 bytes in standard base64.
pub fn valid_key(text: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .is_ok_and(|b| b.len() == 32)
}

/// `a, b,c` → `["a", "b", "c"]`.
fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Reads a WireGuard `.conf` (the `wg-quick` format: `[Interface]` with
/// `Address`, `DNS`, `MTU` and `PrivateKey`; one or more `[Peer]` with
/// `PublicKey`, `Endpoint`, `AllowedIPs`, `PresharedKey` and
/// `PersistentKeepalive`). Keys are case-insensitive, `#` and `;` start
/// comments, lists may be split over several lines. What Termoak does not
/// use (`ListenPort`, `Table`, `PostUp`...) is listed in `ignored`.
pub fn parse_conf(text: &str) -> Result<WgConf, ConfError> {
    #[derive(PartialEq)]
    enum Section {
        None,
        Interface,
        Peer,
        Other,
    }
    let mut out = WgConf::default();
    let mut section = Section::None;
    let mut seen_section = false;
    let mut peer: Option<(WireguardPeer, Option<String>)> = None;
    let mut psks: Vec<Option<String>> = Vec::new();
    let mut ignored = Vec::new();
    let mut finish_peer = |peer: Option<(WireguardPeer, Option<String>)>,
                           out: &mut WgConf|
     -> Result<(), ConfError> {
        if let Some((p, psk)) = peer {
            if p.public_key.is_empty() {
                return Err(ConfError::PeerWithoutKey);
            }
            out.config.peers.push(p);
            psks.push(psk);
        }
        Ok(())
    };
    for (i, raw) in text.lines().enumerate() {
        let line = raw
            .split(['#', ';'])
            .next()
            .unwrap_or("")
            .trim()
            .trim_start_matches('\u{feff}');
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            finish_peer(peer.take(), &mut out)?;
            seen_section = true;
            section = match line[1..line.len() - 1].trim().to_ascii_lowercase().as_str() {
                "interface" => Section::Interface,
                "peer" => {
                    peer = Some((WireguardPeer::default(), None));
                    Section::Peer
                }
                _ => Section::Other,
            };
            continue;
        }
        // Text before any section: not a WireGuard configuration.
        if section == Section::None {
            return Err(ConfError::NotAConf);
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ConfError::BadLine(i + 1));
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match (&section, key.as_str()) {
            (Section::Interface, "address") => out.config.addresses.extend(list(value)),
            (Section::Interface, "dns") => out.config.dns.extend(list(value)),
            (Section::Interface, "mtu") => {
                out.config.mtu = Some(
                    value
                        .parse::<u16>()
                        .ok()
                        .filter(|m| *m >= 576)
                        .ok_or(ConfError::BadNumber("MTU"))?,
                )
            }
            (Section::Interface, "privatekey") => {
                if !valid_key(value) {
                    return Err(ConfError::BadKey("PrivateKey"));
                }
                out.private_key = Some(value.to_string());
            }
            (Section::Peer, k) => {
                let (p, psk) = peer.as_mut().expect("a [Peer] section has its peer");
                match k {
                    "publickey" => {
                        if !valid_key(value) {
                            return Err(ConfError::BadKey("PublicKey"));
                        }
                        p.public_key = value.to_string();
                    }
                    "endpoint" => p.endpoint = Some(value.to_string()).filter(|e| !e.is_empty()),
                    "allowedips" => p.allowed_ips.extend(list(value)),
                    "presharedkey" => {
                        if !valid_key(value) {
                            return Err(ConfError::BadKey("PresharedKey"));
                        }
                        *psk = Some(value.to_string());
                    }
                    "persistentkeepalive" => {
                        p.persistent_keepalive = if value.eq_ignore_ascii_case("off") {
                            None
                        } else {
                            value
                                .parse::<u16>()
                                .map(|k| Some(k).filter(|k| *k > 0))
                                .map_err(|_| ConfError::BadNumber("PersistentKeepalive"))?
                        }
                    }
                    other => ignored.push(other.to_string()),
                }
            }
            (_, other) => ignored.push(other.to_string()),
        }
    }
    finish_peer(peer.take(), &mut out)?;
    if !seen_section {
        return Err(ConfError::NotAConf);
    }
    if out.config.peers.is_empty() {
        return Err(ConfError::NoPeer);
    }
    // One preshared key per network (netd applies it to every peer).
    let distinct: HashSet<&Option<String>> = psks.iter().collect();
    let with_psk: Vec<&String> = psks.iter().flatten().collect();
    if distinct.len() > 1 && !with_psk.is_empty() {
        return Err(ConfError::MixedPresharedKeys);
    }
    out.preshared_key = with_psk.first().map(|s| s.to_string());
    // Names as WireGuard writes them, once each.
    let mut seen = HashSet::new();
    out.ignored = ignored
        .into_iter()
        .map(|k| conf_key_name(&k))
        .filter(|k| seen.insert(k.clone()))
        .collect();
    Ok(out)
}

/// `listenport` → `ListenPort` (for the "ignored" note).
fn conf_key_name(lower: &str) -> String {
    const KNOWN: [&str; 9] = [
        "ListenPort",
        "Table",
        "PreUp",
        "PostUp",
        "PreDown",
        "PostDown",
        "SaveConfig",
        "FwMark",
        "Jc",
    ];
    KNOWN
        .iter()
        .find(|k| k.eq_ignore_ascii_case(lower))
        .map(|k| k.to_string())
        .unwrap_or_else(|| lower.to_string())
}

// ----- State in the server -----

/// `GET /networks/{id}/status` (and the answer of `up`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetStatus {
    pub state: String,
    pub message: Option<String>,
    pub node: Option<NetNode>,
    pub last_handshake_at: Option<i64>,
    pub since: Option<i64>,
    pub rx_bytes: Option<u64>,
    pub tx_bytes: Option<u64>,
    pub streams: Option<u64>,
    pub dials: Option<u64>,
    pub dials_failed: Option<u64>,
}

/// The server's node in a network.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetNode {
    pub name: Option<String>,
    pub addresses: Vec<String>,
    pub public_key: Option<String>,
    pub node_id: Option<String>,
    pub auth_url: Option<String>,
}

/// State of a network as the interface shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetState {
    /// Not running (it starts when used).
    Down,
    Starting,
    /// Running, but the other side never answered (WireGuard: no
    /// handshake yet; check its endpoint and that it has our key).
    Waiting,
    /// Running, and the other side answered.
    Up,
    /// Waiting for authorization (Tailscale, ZeroTier).
    NeedsAuth,
    Error,
    /// Not known (not asked yet, or the server did not answer).
    Unknown,
}

impl NetStatus {
    pub fn state(&self, kind: &NetworkKind) -> NetState {
        match self.state.as_str() {
            "down" => NetState::Down,
            "starting" => NetState::Starting,
            "needs_auth" => NetState::NeedsAuth,
            "error" => NetState::Error,
            "up" if *kind == NetworkKind::Wireguard && self.last_handshake_at.is_none() => {
                NetState::Waiting
            }
            "up" => NetState::Up,
            _ => NetState::Unknown,
        }
    }
}

impl NetState {
    pub fn label(self) -> String {
        match self {
            NetState::Down => t!("networks.state.down"),
            NetState::Starting => t!("networks.state.starting"),
            NetState::Waiting => t!("networks.state.waiting"),
            NetState::Up => t!("networks.state.up"),
            NetState::NeedsAuth => t!("networks.state.needs_auth"),
            NetState::Error => t!("networks.state.error"),
            NetState::Unknown => t!("networks.state.unknown"),
        }
        .to_string()
    }

    /// Explanation for a tooltip or the detail panel.
    pub fn hint(self) -> String {
        match self {
            NetState::Down => t!("networks.state.down_hint"),
            NetState::Starting => t!("networks.state.starting_hint"),
            NetState::Waiting => t!("networks.state.waiting_hint"),
            NetState::Up => t!("networks.state.up_hint"),
            NetState::NeedsAuth => t!("networks.state.needs_auth_hint"),
            NetState::Error => t!("networks.state.error_hint"),
            NetState::Unknown => t!("networks.state.unknown_hint"),
        }
        .to_string()
    }
}

/// A device of a network (`GET /networks/{id}/peers`). For WireGuard, its
/// configured peers: `addresses` are their allowed IPs.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct NetPeer {
    pub name: Option<String>,
    pub dns_name: Option<String>,
    pub addresses: Vec<String>,
    pub os: Option<String>,
    pub online: Option<bool>,
    pub last_seen: Option<i64>,
    pub public_key: Option<String>,
    pub endpoint: Option<String>,
}

impl NetPeer {
    /// Name to show: its name, DNS name, endpoint or the start of its key.
    pub fn title(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.dns_name.clone())
            .or_else(|| self.endpoint.clone())
            .or_else(|| {
                self.public_key
                    .as_ref()
                    .map(|k| k.chars().take(10).collect::<String>() + "…")
            })
            .unwrap_or_else(|| "—".to_string())
    }
}

/// An address a host can be added at, from a peer's address or allowed
/// IP: `10.9.0.5/32` is `10.9.0.5`; a subnet (`10.9.0.0/24`) has no single
/// host, so it gives its prefix for the user to complete (`exact` false).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAddress {
    pub address: String,
    pub exact: bool,
    /// The address or subnet as the peer has it.
    pub source: String,
}

pub fn host_addresses(peer: &NetPeer) -> Vec<HostAddress> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for a in &peer.addresses {
        let a = a.trim();
        let (ip, bits) = match a.split_once('/') {
            Some((ip, bits)) => (ip, bits.parse::<u8>().ok()),
            None => (a, None),
        };
        let Ok(parsed) = ip.parse::<std::net::IpAddr>() else {
            continue;
        };
        // 0.0.0.0/0 and ::/0 (everything) are no host.
        if bits == Some(0) {
            continue;
        }
        let full = match parsed {
            std::net::IpAddr::V4(_) => 32,
            std::net::IpAddr::V6(_) => 128,
        };
        let exact = bits.is_none_or(|b| b >= full);
        let address = if exact {
            parsed.to_string()
        } else {
            match parsed {
                // 10.9.0.0/24 → "10.9.0." to complete.
                std::net::IpAddr::V4(v4) => {
                    let keep = (bits.unwrap_or(32) as usize).div_ceil(8).clamp(1, 3);
                    let octets = v4.octets();
                    let mut s = octets[..keep]
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join(".");
                    s.push('.');
                    s
                }
                std::net::IpAddr::V6(v6) => v6.to_string(),
            }
        };
        if seen.insert(address.clone()) {
            out.push(HostAddress {
                address,
                exact,
                source: a.to_string(),
            });
        }
    }
    if let Some(dns) = peer.dns_name.as_ref().filter(|d| !d.trim().is_empty())
        && seen.insert(dns.clone())
    {
        out.insert(
            0,
            HostAddress {
                address: dns.trim_end_matches('.').to_string(),
                exact: true,
                source: dns.clone(),
            },
        );
    }
    out
}

/// Whether a WireGuard handshake time (ms) is recent.
pub fn handshake_fresh(at: i64, now_ms: i64) -> bool {
    now_ms.saturating_sub(at) < HANDSHAKE_FRESH_MS
}

/// `POST /hosts/{id}/test`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HostTest {
    pub ok: bool,
    pub latency_ms: u64,
    /// The host's key (when it is unknown or changed).
    pub fingerprint: Option<String>,
    pub error: Option<String>,
    pub error_code: Option<String>,
    pub network: Option<HostTestNetwork>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct HostTestNetwork {
    pub id: Option<Id>,
    pub name: String,
    pub kind: String,
}

/// "via Office · 23 ms".
pub fn via_text(network: &str, latency: Option<String>) -> String {
    match latency {
        Some(ms) => t!("networks.via_latency", network = network, ms = ms).to_string(),
        None => t!("networks.via", network = network).to_string(),
    }
}

/// Text of a failed host test.
pub fn host_test_error(test: &HostTest) -> String {
    let code = test.error_code.as_deref().unwrap_or("");
    match crate::i18n::api_error_text(code).or_else(|| host_test_code_text(code)) {
        // The network codes say more with the server's detail.
        Some(text) if code.starts_with("network_") => match &test.error {
            Some(e) if !e.trim().is_empty() => format!("{text} ({e})"),
            _ => text,
        },
        Some(text) => text,
        None => test
            .error
            .clone()
            .unwrap_or_else(|| t!("networks.test.failed").to_string()),
    }
}

fn host_test_code_text(code: &str) -> Option<String> {
    match code {
        "auth_failed" => Some(t!("networks.test.auth_failed").to_string()),
        "timeout" => Some(t!("networks.test.timeout").to_string()),
        "connect_failed" => Some(t!("networks.test.connect_failed").to_string()),
        _ => None,
    }
}

// ----- The network of a host -----

/// The network id a host's first connection goes through: its own, or
/// the nearest group's (like the proxy). Jump chains: the first jump's
/// network applies, so a host with jumps only reports its own setting
/// here (the core resolves the chain when connecting).
pub fn network_id_of(host: &Host, groups: &[&Group]) -> Option<Id> {
    if host.settings.network_id.is_some() {
        return host.settings.network_id;
    }
    inherited_network(host.group_id, groups)
}

/// The network a group (and its parents) gives the hosts in it.
pub fn inherited_network(mut group: Option<Id>, groups: &[&Group]) -> Option<Id> {
    let mut seen = HashSet::new();
    while let Some(gid) = group {
        if !seen.insert(gid) || seen.len() > MAX_DEPTH {
            return None;
        }
        let g = groups.iter().find(|g| g.id == gid)?;
        if g.settings.network_id.is_some() {
            return g.settings.network_id;
        }
        group = g.parent_id;
    }
    None
}

// ----- Errors -----

/// Server codes whose own translation is always shown (also in English):
/// their English message is meant for logs.
pub const FRIENDLY_CODES: [&str; 8] = [
    "network_unavailable",
    "network_not_found",
    "network_dial_failed",
    "unsupported_kind",
    "key_exists",
    "not_wireguard",
    "relay_limit",
    "not_in_network",
];

/// Friendly text of a network error from the API, with its details:
/// `plan_limit` says the plan's number of networks, `network_dial_failed`
/// what netd found (`netd_code`). Other errors as usual.
pub fn api_error_text(code: &str, details: &Value) -> Option<String> {
    match code {
        "plan_limit" if details["limit"] == "max_networks" => Some(match details["max"].as_u64() {
            Some(max) => t!("networks.error.plan_limit", count = max).to_string(),
            None => t!("networks.error.plan_limit_unknown").to_string(),
        }),
        "network_dial_failed" => {
            let netd = details["netd_code"].as_str().unwrap_or("");
            netd_text(netd)
                .or_else(|| crate::i18n::api_error_text(code))
                .map(|s| s.to_string())
        }
        c if FRIENDLY_CODES.contains(&c) => crate::i18n::api_error_text(c),
        _ => None,
    }
}

/// What netd found when it could not reach a host.
fn netd_text(code: &str) -> Option<String> {
    Some(
        match code {
            "dial_failed" => t!("networks.netd.dial_failed"),
            "timeout" => t!("networks.netd.timeout"),
            "needs_auth" => t!("networks.netd.needs_auth"),
            "network_error" => t!("networks.netd.network_error"),
            "limit" => t!("networks.netd.limit"),
            _ => return None,
        }
        .to_string(),
    )
}

/// Text of an error of the networks screens.
pub fn error_text(e: ClientError) -> String {
    crate::state::api_error(e)
}

// ----- Server API -----

/// The REST calls of the networks screens (see the server's docs/API.md,
/// "Private networks").
pub mod api {
    use serde_json::{Value, json};
    use termoak_client::{ApiClient, ClientError};
    use termoak_core::Id;
    use termoak_core::model::{Network, NetworkSecret, Record};

    use super::{HostTest, NetPeer, NetStatus};

    type Result<T> = std::result::Result<T, ClientError>;

    /// The body of a create or update: the network, its vault and, when
    /// given, the secret (`None` keeps the stored one).
    fn body(network: &Network, vault: Option<Id>, secret: Option<&NetworkSecret>) -> Value {
        let mut v = serde_json::to_value(network).unwrap_or_else(|_| json!({}));
        if let Some(obj) = v.as_object_mut() {
            obj.remove("id");
            if let Some(vault) = vault {
                obj.insert("vault_id".into(), json!(vault));
            }
            if let Some(s) = secret {
                obj.insert(
                    "secret".into(),
                    serde_json::to_value(s).unwrap_or(Value::Null),
                );
            }
        }
        v
    }

    pub async fn create(
        api: &ApiClient,
        vault: Id,
        network: &Network,
        secret: Option<&NetworkSecret>,
    ) -> Result<Record<Network>> {
        api.post("/api/v1/networks", &body(network, Some(vault), secret))
            .await
    }

    pub async fn update(
        api: &ApiClient,
        network: &Network,
        secret: Option<&NetworkSecret>,
    ) -> Result<Record<Network>> {
        api.put(
            &format!("/api/v1/networks/{}", network.id),
            &body(network, None, secret),
        )
        .await
    }

    pub async fn delete(api: &ApiClient, id: Id) -> Result<()> {
        api.delete(&format!("/api/v1/networks/{id}"))
            .await
            .map(|_| ())
    }

    pub async fn status(api: &ApiClient, id: Id) -> Result<NetStatus> {
        api.get(&format!("/api/v1/networks/{id}/status")).await
    }

    pub async fn up(api: &ApiClient, id: Id) -> Result<NetStatus> {
        api.post(&format!("/api/v1/networks/{id}/up"), &json!({}))
            .await
    }

    pub async fn down(api: &ApiClient, id: Id) -> Result<()> {
        api.post::<Value>(&format!("/api/v1/networks/{id}/down"), &json!({}))
            .await
            .map(|_| ())
    }

    pub async fn peers(api: &ApiClient, id: Id) -> Result<Vec<NetPeer>> {
        api.get(&format!("/api/v1/networks/{id}/peers")).await
    }

    /// The server generates the network's WireGuard private key (stored
    /// as its secret) and returns the public one.
    pub async fn generate_key(api: &ApiClient, id: Id, replace: bool) -> Result<String> {
        let v: Value = api
            .post(
                &format!("/api/v1/networks/{id}/wireguard/key"),
                &json!({"replace": replace}),
            )
            .await?;
        Ok(v["public_key"].as_str().unwrap_or_default().to_string())
    }

    /// `(networks billed to you, max)` from `/me/plan` (`max` `None`: no
    /// limit).
    pub async fn usage(api: &ApiClient) -> Result<(u64, Option<u64>)> {
        let v: Value = api.get("/api/v1/me/plan").await?;
        Ok((
            v["usage"]["networks"].as_u64().unwrap_or(0),
            v["plan"]["limits"]["max_networks"].as_u64(),
        ))
    }

    /// `trust`: accept the host's key if the server does not know it yet
    /// (after the user confirmed its fingerprint).
    pub async fn test_host(api: &ApiClient, host: Id, trust: bool) -> Result<HostTest> {
        let query = if trust { "?trust=true" } else { "" };
        api.post(&format!("/api/v1/hosts/{host}/test{query}"), &json!({}))
            .await
    }
}

// ----- States shared by the views -----

/// What is known of the networks in the server (their state) and of the
/// hosts tested through them, shared by every window: the networks
/// screen, the host and group editors and the host cards.
#[derive(Default)]
pub struct NetBook {
    states: std::collections::HashMap<Id, (NetStatus, std::time::Instant)>,
    loading: HashSet<Id>,
    tests: std::collections::HashMap<Id, (HostTest, std::time::Instant)>,
    testing: HashSet<Id>,
}

struct NetBookGlobal(gpui::Entity<NetBook>);

impl gpui::Global for NetBookGlobal {}

/// States younger than this are not asked again (unless forced).
const STATE_FRESH: std::time::Duration = std::time::Duration::from_secs(15);

impl NetBook {
    pub fn global(cx: &mut gpui::App) -> gpui::Entity<Self> {
        use gpui::AppContext;
        if let Some(g) = cx.try_global::<NetBookGlobal>() {
            return g.0.clone();
        }
        let entity = cx.new(|_| NetBook::default());
        cx.set_global(NetBookGlobal(entity.clone()));
        entity
    }

    pub fn status(&self, id: Id) -> Option<&NetStatus> {
        self.states.get(&id).map(|(s, _)| s)
    }

    pub fn is_loading(&self, id: Id) -> bool {
        self.loading.contains(&id)
    }

    pub fn set_status(&mut self, id: Id, status: NetStatus, cx: &mut gpui::Context<Self>) {
        self.states.insert(id, (status, std::time::Instant::now()));
        cx.notify();
    }

    /// Asks the server for the state of a network (`force`: even if it is
    /// fresh).
    pub fn refresh(
        &mut self,
        api: termoak_client::ApiClient,
        id: Id,
        force: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        let fresh = self
            .states
            .get(&id)
            .is_some_and(|(_, at)| at.elapsed() < STATE_FRESH);
        if self.loading.contains(&id) || (fresh && !force) {
            return;
        }
        self.loading.insert(id);
        crate::runtime::run(
            cx,
            async move { api::status(&api, id).await.map_err(error_text) },
            move |b, res, cx| {
                b.loading.remove(&id);
                match res {
                    Ok(s) => {
                        b.states.insert(id, (s, std::time::Instant::now()));
                    }
                    // Kept as unknown for a while (no request storm).
                    Err(e) => {
                        tracing::debug!(error = %e, "network status");
                        let unknown = NetStatus {
                            state: "unknown".into(),
                            message: Some(e),
                            ..Default::default()
                        };
                        b.states.insert(id, (unknown, std::time::Instant::now()));
                    }
                }
                cx.notify();
            },
        );
    }

    pub fn test(&self, host: Id) -> Option<&HostTest> {
        self.tests.get(&host).map(|(t, _)| t)
    }

    pub fn is_testing(&self, host: Id) -> bool {
        self.testing.contains(&host)
    }

    /// Tests a host from the server (through its network); `done` gets the
    /// result or the error text.
    pub fn test_host(
        &mut self,
        api: termoak_client::ApiClient,
        host: Id,
        trust: bool,
        cx: &mut gpui::Context<Self>,
        done: impl FnOnce(Result<HostTest, String>, &mut gpui::App) + 'static,
    ) {
        if !self.testing.insert(host) {
            return;
        }
        cx.notify();
        let task = crate::runtime::spawn(cx, async move {
            api::test_host(&api, host, trust).await.map_err(error_text)
        });
        cx.spawn(async move |this, cx| {
            let res = task.await;
            let _ = this.update(cx, |b, cx| {
                b.testing.remove(&host);
                if let Ok(t) = &res {
                    b.tests.insert(host, (t.clone(), std::time::Instant::now()));
                }
                cx.notify();
            });
            let _ = cx.update(|cx| done(res, cx));
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY_A: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";
    const KEY_B: &str = "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=";
    const KEY_C: &str = "TrMvSoP4jYQlY6RIzBgbssQqY3vxI2Pi+y71lOWWXX0=";

    #[test]
    fn reads_a_wg_quick_conf() {
        let conf = format!(
            "\u{feff}# office\n[Interface]\nPrivateKey = {KEY_A}\nAddress = 10.9.0.2/32, fd00::2/128\n\
             DNS = 10.9.0.1, lan\nMTU = 1380\nListenPort = 51820\nPostUp = iptables -A …\n\n\
             [Peer]\npublickey={KEY_B}  ; the router\nEndpoint = vpn.example.com:51820\n\
             AllowedIPs = 10.9.0.0/24\nAllowedIPs = 192.168.1.0/24\nPersistentKeepalive = 25\n\
             PresharedKey = {KEY_C}\n"
        );
        let c = parse_conf(&conf).unwrap();
        assert_eq!(c.private_key.as_deref(), Some(KEY_A));
        assert_eq!(c.preshared_key.as_deref(), Some(KEY_C));
        assert_eq!(c.config.addresses, ["10.9.0.2/32", "fd00::2/128"]);
        assert_eq!(c.config.dns, ["10.9.0.1", "lan"]);
        assert_eq!(c.config.mtu, Some(1380));
        assert_eq!(c.config.peers.len(), 1);
        let p = &c.config.peers[0];
        assert_eq!(p.public_key, KEY_B);
        assert_eq!(p.endpoint.as_deref(), Some("vpn.example.com:51820"));
        assert_eq!(p.allowed_ips, ["10.9.0.0/24", "192.168.1.0/24"]);
        assert_eq!(p.persistent_keepalive, Some(25));
        assert_eq!(c.ignored, ["ListenPort", "PostUp"]);
        // The public key is the server's to fill in.
        assert_eq!(c.config.public_key, None);
    }

    #[test]
    fn a_conf_without_private_key_waits_for_a_generated_one() {
        let c = parse_conf(&format!(
            "[Interface]\nAddress = 10.9.0.3/32\n[Peer]\nPublicKey = {KEY_B}\nEndpoint = 127.0.0.1:51999\nAllowedIPs = 10.9.0.0/24\nPersistentKeepalive = off\n"
        ))
        .unwrap();
        assert_eq!(c.private_key, None);
        assert_eq!(c.config.peers[0].persistent_keepalive, None);
        // Two peers sharing a preshared key, or none.
        let two = parse_conf(&format!(
            "[Interface]\n[Peer]\nPublicKey = {KEY_B}\nPresharedKey = {KEY_C}\n[Peer]\nPublicKey = {KEY_A}\nPresharedKey = {KEY_C}\n"
        ))
        .unwrap();
        assert_eq!(two.config.peers.len(), 2);
        assert_eq!(two.preshared_key.as_deref(), Some(KEY_C));
    }

    #[test]
    fn bad_confs_say_why() {
        assert_eq!(parse_conf(""), Err(ConfError::NotAConf));
        assert_eq!(parse_conf("hello world"), Err(ConfError::NotAConf));
        assert_eq!(
            parse_conf("[Interface]\nAddress = 10.0.0.2/32\n"),
            Err(ConfError::NoPeer)
        );
        assert_eq!(
            parse_conf("[Interface]\n[Peer]\nEndpoint = a:1\n"),
            Err(ConfError::PeerWithoutKey)
        );
        assert_eq!(
            parse_conf("[Interface]\nPrivateKey = nope\n"),
            Err(ConfError::BadKey("PrivateKey"))
        );
        assert_eq!(
            parse_conf("[Interface]\nthis is not a pair\n"),
            Err(ConfError::BadLine(2))
        );
        assert_eq!(
            parse_conf(&format!(
                "[Interface]\nMTU = big\n[Peer]\nPublicKey = {KEY_B}\n"
            )),
            Err(ConfError::BadNumber("MTU"))
        );
        assert_eq!(
            parse_conf(&format!(
                "[Interface]\n[Peer]\nPublicKey = {KEY_B}\nPresharedKey = {KEY_C}\n[Peer]\nPublicKey = {KEY_A}\n"
            )),
            Err(ConfError::MixedPresharedKeys)
        );
        // A bad key never echoes the key in the error.
        let e = parse_conf("[Interface]\nPrivateKey = c2VjcmV0\n").unwrap_err();
        assert!(!format!("{e:?}").contains("c2VjcmV0"));
    }

    #[test]
    fn states_from_netd() {
        let st = |v: Value| serde_json::from_value::<NetStatus>(v).unwrap();
        let wg = NetworkKind::Wireguard;
        assert_eq!(st(json!({"state": "down"})).state(&wg), NetState::Down);
        assert_eq!(st(json!({"state": "up"})).state(&wg), NetState::Waiting);
        assert_eq!(
            st(json!({"state": "up", "last_handshake_at": 1})).state(&wg),
            NetState::Up
        );
        assert_eq!(
            st(json!({"state": "up"})).state(&NetworkKind::Tailscale),
            NetState::Up
        );
        assert_eq!(
            st(json!({"state": "error", "message": "bad key"})).state(&wg),
            NetState::Error
        );
        assert_eq!(st(json!({"state": "later"})).state(&wg), NetState::Unknown);
        // The whole answer of the server reads.
        let full = st(json!({"network_id": "x", "state": "up", "message": null,
            "node": {"name": "termoak-0190abcd", "addresses": ["10.9.0.2"], "public_key": KEY_A},
            "revision": "3f9a", "since": 1, "last_used_at": 2, "rx_bytes": 3, "tx_bytes": 4,
            "last_handshake_at": 5, "streams": 0, "dials": 1, "dials_failed": 0}));
        assert_eq!(full.node.unwrap().public_key.as_deref(), Some(KEY_A));
    }

    #[test]
    fn peers_give_host_addresses() {
        let peer = |addrs: &[&str]| NetPeer {
            addresses: addrs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let a = host_addresses(&peer(&[
            "10.9.0.5/32",
            "10.9.0.0/24",
            "0.0.0.0/0",
            "fd00::7/128",
            "nonsense",
        ]));
        assert_eq!(
            a.iter()
                .map(|x| (x.address.as_str(), x.exact))
                .collect::<Vec<_>>(),
            [("10.9.0.5", true), ("10.9.0.", false), ("fd00::7", true)]
        );
        assert_eq!(host_addresses(&peer(&["10.0.0.0/8"]))[0].address, "10.");
        assert_eq!(
            host_addresses(&peer(&["192.168.4.20"]))[0].address,
            "192.168.4.20"
        );
        let named = NetPeer {
            dns_name: Some("nas.tail1234.ts.net.".into()),
            addresses: vec!["100.64.0.7".into()],
            ..Default::default()
        };
        assert_eq!(host_addresses(&named)[0].address, "nas.tail1234.ts.net");
        assert_eq!(
            NetPeer {
                public_key: Some(KEY_B.into()),
                ..Default::default()
            }
            .title(),
            "xTIBA5rboU…"
        );
    }

    #[test]
    fn hosts_inherit_the_network_of_their_groups() {
        use termoak_core::model::HostSettings;
        let net = Id::now_v7();
        let other = Id::now_v7();
        let group = |id, parent, network| Group {
            id,
            name: "g".into(),
            parent_id: parent,
            color: None,
            settings: HostSettings {
                network_id: network,
                ..Default::default()
            },
        };
        let (outer, inner) = (Id::now_v7(), Id::now_v7());
        let groups = [
            group(outer, None, Some(net)),
            group(inner, Some(outer), None),
        ];
        let refs: Vec<&Group> = groups.iter().collect();
        let mut host = Host {
            id: Id::now_v7(),
            label: "h".into(),
            address: "10.9.0.1".into(),
            group_id: Some(inner),
            tags: Vec::new(),
            settings: HostSettings::default(),
            notes: String::new(),
            color: None,
            os: None,
            os_version: None,
            favorite: false,
            protocol: Default::default(),
            icon: None,
        };
        assert_eq!(network_id_of(&host, &refs), Some(net));
        host.settings.network_id = Some(other);
        assert_eq!(network_id_of(&host, &refs), Some(other));
        host.settings.network_id = None;
        host.group_id = None;
        assert_eq!(network_id_of(&host, &refs), None);
        // A loop of groups ends.
        let looped = [
            group(outer, Some(inner), None),
            group(inner, Some(outer), None),
        ];
        let refs: Vec<&Group> = looped.iter().collect();
        assert_eq!(inherited_network(Some(inner), &refs), None);
    }

    #[test]
    fn network_errors_are_friendly() {
        let plan =
            api_error_text("plan_limit", &json!({"limit": "max_networks", "max": 10})).unwrap();
        assert!(plan.contains("10"), "{plan}");
        // Other limits keep the generic text.
        assert_eq!(
            api_error_text("plan_limit", &json!({"limit": "max_teams", "max": 3})),
            None
        );
        let dial = api_error_text("network_dial_failed", &json!({"netd_code": "timeout"})).unwrap();
        assert_eq!(dial, t!("networks.netd.timeout"));
        let generic = api_error_text("network_dial_failed", &json!({})).unwrap();
        assert_eq!(generic, t!("error.network_dial_failed"));
        for code in FRIENDLY_CODES {
            assert!(api_error_text(code, &json!({})).is_some(), "{code}");
        }
        assert_eq!(api_error_text("not_found", &json!({})), None);
    }

    /// Prompter of the live test: accepts the test site's host key.
    struct TrustFirstKey;

    #[async_trait::async_trait]
    impl termoak_ssh::AuthPrompter for TrustFirstKey {
        async fn confirm_host_key(&self, _: &str, _: u16, _: &str, _: &str) -> bool {
            true
        }
    }

    /// End to end against a real server with networks and a WireGuard
    /// test site (opt-in: `cargo test -- --ignored live_`), as the desktop
    /// does it: the network from a pasted `.conf` without a private key,
    /// the key generated by the server, its public key authorized on the
    /// site, a host in the network, and an SSH connection from this
    /// computer through the server relay (`Workspace::connect`, the path of
    /// the terminal, SFTP and tunnels); the host test and the network's
    /// state and peers. Everything it creates is deleted.
    ///
    /// `TERMOAK_LIVE_SERVER`, `TERMOAK_LIVE_EMAIL`, `TERMOAK_LIVE_PASSWORD`
    /// (an account there), `TERMOAK_LIVE_SITE_KEY` and
    /// `TERMOAK_LIVE_SITE_ENDPOINT` (the site's WireGuard public key and
    /// `host:port` as the server sees it), `TERMOAK_LIVE_SITE_FIFO` (where
    /// `peer <key> <ip>` authorizes a client: termoak-wgpeer's stdin),
    /// `TERMOAK_LIVE_ADDRESS` (this network's address in the tunnel, e.g.
    /// `10.9.0.3/32`), `TERMOAK_LIVE_HOST` (the SSH host inside, e.g.
    /// `10.9.0.1`), `TERMOAK_LIVE_SSH_USER` and `TERMOAK_LIVE_SSH_KEY` (a
    /// private key file).
    #[tokio::test]
    #[ignore]
    async fn live_relay_through_a_wireguard_network() {
        use termoak_client::{ServerChoice, Workspace};
        let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
        let Ok(server) = std::env::var("TERMOAK_LIVE_SERVER") else {
            eprintln!("TERMOAK_LIVE_SERVER not set: skipped");
            return;
        };
        let dir = std::env::temp_dir().join(format!("termoak-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = Workspace::open(&dir, termoak_core::crypto::MasterKey::generate()).unwrap();
        let account = ws
            .sign_in(
                ServerChoice::Custom(server.clone()),
                &var("TERMOAK_LIVE_EMAIL"),
                &var("TERMOAK_LIVE_PASSWORD"),
                None,
            )
            .await
            .expect("sign in");
        account.sync_once().await.expect("first sync");
        assert!(
            supported(&account.info().features),
            "the server has no networks"
        );
        let api = account.api.clone();
        let vault = account.info().user_id.expect("personal vault");

        // The network, as the form sends it: a .conf without PrivateKey.
        let conf = format!(
            "[Interface]\nAddress = {}\n\n[Peer]\nPublicKey = {}\nEndpoint = {}\nAllowedIPs = 10.9.0.0/24\nPersistentKeepalive = 25\n",
            var("TERMOAK_LIVE_ADDRESS"),
            var("TERMOAK_LIVE_SITE_KEY"),
            var("TERMOAK_LIVE_SITE_ENDPOINT")
        );
        let parsed = parse_conf(&conf).unwrap();
        assert!(parsed.private_key.is_none());
        let network = termoak_core::model::Network {
            id: Id::nil(),
            name: format!("desktop-live-{}", &uuid::Uuid::new_v4().to_string()[..8]),
            kind: NetworkKind::Wireguard,
            description: "desktop live test".into(),
            wireguard: Some(parsed.config.clone()),
            tailscale: None,
            zerotier: None,
        };
        let rec = api::create(&api, vault, &network, None)
            .await
            .expect("create");
        let net_id = rec.data.id;
        let cleanup = async |host: Option<Id>, key: Option<Id>| {
            if let Some(h) = host {
                let _ = api.delete(&format!("/api/v1/hosts/{h}")).await;
            }
            if let Some(k) = key {
                let _ = api.delete(&format!("/api/v1/keys/{k}")).await;
            }
            let _ = api::delete(&api, net_id).await;
        };
        let public_key = api::generate_key(&api, net_id, false).await.expect("key");
        assert!(valid_key(&public_key), "{public_key}");
        // A second time needs replace: the friendly key_exists text.
        let again = api::generate_key(&api, net_id, false).await.unwrap_err();
        assert_eq!(again.api_code(), Some("key_exists"));
        assert_eq!(error_text(again), t!("error.key_exists"));
        std::fs::write(
            var("TERMOAK_LIVE_SITE_FIFO"),
            format!("peer {public_key} {}\n", var("TERMOAK_LIVE_ADDRESS")),
        )
        .expect("authorize on the site");

        // A host in the network, with the site's SSH key.
        let key: Value = api
            .post(
                "/api/v1/keys/import",
                &serde_json::json!({"label": "desktop-live", "private_key": std::fs::read_to_string(var("TERMOAK_LIVE_SSH_KEY")).unwrap()}),
            )
            .await
            .expect("key import");
        let key_id: Id = serde_json::from_value(key["id"].clone()).unwrap();
        let host: Value = api
            .post(
                "/api/v1/hosts",
                &serde_json::json!({"label": "desktop-live-host", "address": var("TERMOAK_LIVE_HOST"),
                    "settings": {"port": 22, "username": var("TERMOAK_LIVE_SSH_USER"), "key_id": key_id, "network_id": net_id}}),
            )
            .await
            .expect("host");
        let host_id: Id = serde_json::from_value(host["id"].clone()).unwrap();

        let result = async {
            account.sync_once().await.map_err(|e| e.to_string())?;
            // The network reached the local store, with its public key.
            let local = ws
                .find_item::<termoak_core::model::Network>(net_id)
                .await
                .map_err(|e| format!("network not synced: {e}"))?;
            if local
                .record
                .data
                .wireguard
                .as_ref()
                .and_then(|w| w.public_key.clone())
                != Some(public_key.clone())
            {
                return Err("the synced network lacks the server's public key".to_string());
            }
            // SSH from "this computer" through the relay.
            let conn = ws
                .connect(host_id, std::sync::Arc::new(TrustFirstKey), false)
                .await
                .map_err(|e| {
                    format!("connect through the relay: {}", crate::state::api_error(e))
                })?;
            let out = conn
                .exec("echo relay-ok", &termoak_ssh::ExecOptions::default())
                .await
                .map_err(|e| e.to_string())?;
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            let rtt = conn
                .latency(std::time::Duration::from_secs(5))
                .await
                .map_err(|e| e.to_string())?;
            conn.disconnect().await;
            if !text.contains("relay-ok") {
                return Err(format!("unexpected output {text:?}"));
            }
            // The server's view: state with a handshake, the host test
            // through the network, the peers to add hosts at.
            let status = api::status(&api, net_id).await.map_err(|e| e.to_string())?;
            // The server keeps its own known hosts: trust the site's key.
            let test = api::test_host(&api, host_id, true)
                .await
                .map_err(|e| e.to_string())?;
            let peers = api::peers(&api, net_id).await.map_err(|e| e.to_string())?;
            Ok::<_, String>((rtt, status, test, peers))
        }
        .await;
        cleanup(Some(host_id), Some(key_id)).await;
        let (rtt, status, test, peers) = result.unwrap();
        eprintln!(
            "relay: echo ok, SSH round trip {} through the network; state {:?}; test ok={} {} ms via {:?}; peers {:?}",
            crate::terminal::latency::format(Some(rtt)),
            status.state(&NetworkKind::Wireguard),
            test.ok,
            test.latency_ms,
            test.network.as_ref().map(|n| n.name.clone()),
            peers.iter().map(|p| host_addresses(p)).collect::<Vec<_>>()
        );
        assert_eq!(status.state(&NetworkKind::Wireguard), NetState::Up);
        assert!(test.ok, "{:?}", test.error);
        assert_eq!(test.network.map(|n| n.id), Some(Some(net_id)));
        assert!(!peers.is_empty());
        // Deleted: the server forgets it.
        let gone = api::status(&api, net_id).await.unwrap_err();
        assert_eq!(gone.api_status(), Some(404));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
