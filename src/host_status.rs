//! Status of the hosts in the hosts list, without connecting: a light TCP
//! connection to the host's SSH port (through its proxy, if it has one) that
//! is closed as soon as it opens. No SSH, no authentication, nothing in the
//! host's logs beyond an accepted and closed connection; only `debug` logs
//! here.
//!
//! - Green: it answered (with the time the connection took, "23 ms").
//! - Red: refused, timed out or the name does not resolve.
//! - Gray: unknown. Not checked yet, or not checked at all: hosts behind
//!   jump hosts (the path is inside SSH), hosts of Strict vaults (they are
//!   only reached through the server), proxies that need a password the
//!   user cannot read (Use-only), and hosts with the check turned off.
//!
//! The hosts on screen are checked in the background, at most
//! [`CONCURRENCY`] at a time, every [`EVERY`] while the hosts list is in
//! view (a hidden list does not render, so it does not ask), and at once
//! with "Check now". Settings → General turns it off for every host; the
//! host menu, for one host on this device.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures::StreamExt;
use gpui::{App, AppContext, Context, Entity, Global, Task};
use termoak_client::ItemRef;
use termoak_core::Id;
use termoak_core::model::{Group, Host, HostSecret, HostSettings, ProxyKind, ProxySettings};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use zeroize::Zeroize;

use crate::runtime;
use crate::state::AppModel;

/// Time between checks of a host while the list is on screen.
pub const EVERY: Duration = Duration::from_secs(60);
/// Most checks running at the same time.
pub const CONCURRENCY: usize = 8;
/// Longest wait for a host (then it is unreachable).
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// Most levels of nested groups followed for inherited settings (as the core).
const MAX_DEPTH: usize = 16;

/// Where a check connects to. A host whose target changed (edited address,
/// port or proxy) is checked again.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub address: String,
    pub port: u16,
    pub proxy: Option<ProxySettings>,
}

/// Why a host is not checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// Turned off for this host.
    Off,
    /// Reached through jump hosts.
    Jump,
    /// Strict vault: only through the server.
    Strict,
    /// Its proxy needs a password this user cannot read (Use-only).
    UseOnly,
}

/// A host to check.
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    pub host_id: Id,
    pub target: Target,
    /// Where its proxy password is (only when the proxy needs one).
    pub secret: Option<ItemRef>,
}

/// Result of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// It answered after this long.
    Up(Duration),
    Down,
}

/// What is known of a host.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    target: Target,
    reach: Reach,
    at: Instant,
}

/// What the card shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    Up {
        rtt: Duration,
        ago: Duration,
    },
    Down {
        ago: Duration,
    },
    /// Not checked yet (or its target changed and the new one is pending).
    Pending,
    Skipped(Skip),
}

/// Effective settings of a host: those of its groups, from the outermost,
/// with its own on top (like the core's resolver).
pub fn effective_settings(host: &Host, groups: &[&Group]) -> HostSettings {
    let mut chain: Vec<&Group> = Vec::new();
    let mut seen = HashSet::new();
    let mut next = host.group_id;
    while let Some(gid) = next {
        if !seen.insert(gid) || chain.len() >= MAX_DEPTH {
            break;
        }
        match groups.iter().find(|g| g.id == gid) {
            Some(g) => {
                next = g.parent_id;
                chain.push(g);
            }
            None => break,
        }
    }
    let mut settings = HostSettings::default();
    for g in chain.iter().rev() {
        settings = settings.overlay(&g.settings);
    }
    settings.overlay(&host.settings)
}

/// What to check for a host, or why not.
pub fn target_of(
    host: &Host,
    settings: &HostSettings,
    strict: bool,
    can_read_secrets: bool,
    off: bool,
) -> Result<(Target, bool), Skip> {
    if off {
        return Err(Skip::Off);
    }
    if strict {
        return Err(Skip::Strict);
    }
    if settings
        .jump_host_ids
        .as_ref()
        .is_some_and(|j| !j.is_empty())
    {
        return Err(Skip::Jump);
    }
    let proxy = settings
        .proxy
        .clone()
        .filter(|p| !p.host.trim().is_empty() && p.port != 0);
    // SOCKS4 sends only a user id; SOCKS5 and HTTP send a password too.
    let needs_password = proxy.as_ref().is_some_and(|p| {
        p.kind != ProxyKind::Socks4 && p.username.as_deref().is_some_and(|u| !u.is_empty())
    });
    if needs_password && !can_read_secrets {
        return Err(Skip::UseOnly);
    }
    let address = host
        .address
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    Ok((
        Target {
            address,
            port: settings.port.unwrap_or(host.protocol.default_port()),
            proxy,
        },
        needs_password,
    ))
}

/// What is known of every host, and which checks are running.
#[derive(Debug, Default)]
pub struct Book {
    entries: HashMap<Id, Entry>,
    running: HashSet<Id>,
}

impl Book {
    /// Hosts to check now: not running, and never checked, checked
    /// `every` ago or more, or with a different target. Never checked
    /// first.
    pub fn due(&self, targets: &[(Id, &Target)], now: Instant, every: Duration) -> Vec<Id> {
        let mut fresh = Vec::new();
        let mut stale: Vec<(Instant, Id)> = Vec::new();
        for (id, target) in targets {
            if self.running.contains(id) {
                continue;
            }
            match self.entries.get(id) {
                Some(e) if e.target != **target => fresh.push(*id),
                Some(e) if now.saturating_duration_since(e.at) >= every => stale.push((e.at, *id)),
                Some(_) => {}
                None => fresh.push(*id),
            }
        }
        stale.sort_by_key(|(at, _)| *at);
        fresh.extend(stale.into_iter().map(|(_, id)| id));
        fresh
    }

    /// When the next host of `targets` is due (`None`: one is running and
    /// the others are fresh, or there are none).
    pub fn next_due(
        &self,
        targets: &[(Id, &Target)],
        now: Instant,
        every: Duration,
    ) -> Option<Duration> {
        targets
            .iter()
            .filter(|(id, _)| !self.running.contains(id))
            .map(|(id, target)| match self.entries.get(id) {
                Some(e) if e.target == **target => (e.at + every).saturating_duration_since(now),
                _ => Duration::ZERO,
            })
            .min()
    }

    pub fn start(&mut self, ids: &[Id]) {
        self.running.extend(ids.iter().copied());
    }

    pub fn is_running(&self, id: Id) -> bool {
        self.running.contains(&id)
    }

    pub fn finish(&mut self, id: Id, target: Target, reach: Reach, now: Instant) {
        self.running.remove(&id);
        self.entries.insert(
            id,
            Entry {
                target,
                reach,
                at: now,
            },
        );
    }

    /// What a card shows for a host whose check is `target`.
    pub fn shown(&self, id: Id, target: Result<&Target, Skip>, now: Instant) -> Shown {
        let target = match target {
            Ok(t) => t,
            Err(skip) => return Shown::Skipped(skip),
        };
        match self.entries.get(&id) {
            Some(e) if e.target == *target => {
                let ago = now.saturating_duration_since(e.at);
                match e.reach {
                    Reach::Up(rtt) => Shown::Up { rtt, ago },
                    Reach::Down => Shown::Down { ago },
                }
            }
            _ => Shown::Pending,
        }
    }
}

/// "10 s", "3 min", "2 h" for the tooltip.
pub fn ago_text(ago: Duration) -> gpui::SharedString {
    let s = ago.as_secs();
    if s < 5 {
        t!("host_status.just_now")
    } else if s < 60 {
        t!("host_status.seconds_ago", n = s)
    } else if s < 3600 {
        t!("host_status.minutes_ago", n = s / 60)
    } else {
        t!("host_status.hours_ago", n = s / 3600)
    }
}

/// Tooltip of the status dot: "Reachable · 23 ms · checked 10 s ago".
pub fn tooltip(shown: Shown) -> gpui::SharedString {
    match shown {
        Shown::Up { rtt, ago } => t!(
            "host_status.up_tooltip",
            ms = crate::terminal::latency::format(Some(rtt)),
            ago = ago_text(ago)
        ),
        Shown::Down { ago } => t!("host_status.down_tooltip", ago = ago_text(ago)),
        Shown::Pending => t!("host_status.pending_tooltip"),
        Shown::Skipped(Skip::Off) => t!("host_status.off_tooltip"),
        Shown::Skipped(Skip::Jump) => t!("host_status.jump_tooltip"),
        Shown::Skipped(Skip::Strict) => t!("host_status.strict_tooltip"),
        Shown::Skipped(Skip::UseOnly) => t!("host_status.use_only_tooltip"),
    }
}

// ----- Checking -----

/// Connects to the target (through its proxy) and closes at once.
async fn check(target: &Target, proxy_password: Option<&str>) -> Reach {
    let attempt = async {
        match &target.proxy {
            None => {
                // The name is resolved first, so the time is the
                // connection's only.
                let addrs: Vec<std::net::SocketAddr> =
                    tokio::net::lookup_host((target.address.as_str(), target.port))
                        .await
                        .ok()?
                        .collect();
                for addr in addrs {
                    let start = Instant::now();
                    if TcpStream::connect(addr).await.is_ok() {
                        return Some(start.elapsed());
                    }
                }
                None
            }
            Some(proxy) => {
                let start = Instant::now();
                let mut s = TcpStream::connect((proxy.host.as_str(), proxy.port))
                    .await
                    .ok()?;
                let _ = s.set_nodelay(true);
                let user = proxy.username.as_deref().filter(|u| !u.is_empty());
                let pass = proxy_password.unwrap_or("");
                let ok = match proxy.kind {
                    ProxyKind::Socks5 => {
                        socks5(
                            &mut s,
                            &target.address,
                            target.port,
                            user.map(|u| (u, pass)),
                        )
                        .await
                    }
                    ProxyKind::Socks4 => {
                        socks4(&mut s, &target.address, target.port, user.unwrap_or("")).await
                    }
                    ProxyKind::Http => {
                        http_connect(
                            &mut s,
                            &target.address,
                            target.port,
                            user.map(|u| (u, pass)),
                        )
                        .await
                    }
                };
                ok.then(|| start.elapsed())
            }
        }
    };
    match tokio::time::timeout(TIMEOUT, attempt).await {
        Ok(Some(rtt)) => Reach::Up(rtt),
        _ => Reach::Down,
    }
}

async fn socks5(s: &mut TcpStream, host: &str, port: u16, auth: Option<(&str, &str)>) -> bool {
    async fn run(
        s: &mut TcpStream,
        host: &str,
        port: u16,
        auth: Option<(&str, &str)>,
    ) -> std::io::Result<bool> {
        let greeting: &[u8] = if auth.is_some() {
            &[5, 2, 0x00, 0x02]
        } else {
            &[5, 1, 0x00]
        };
        s.write_all(greeting).await?;
        let mut reply = [0u8; 2];
        s.read_exact(&mut reply).await?;
        if reply[0] != 5 {
            return Ok(false);
        }
        match (reply[1], auth) {
            (0x00, _) => {}
            (0x02, Some((user, pass))) if user.len() <= 255 && pass.len() <= 255 => {
                let mut msg = vec![1, user.len() as u8];
                msg.extend_from_slice(user.as_bytes());
                msg.push(pass.len() as u8);
                msg.extend_from_slice(pass.as_bytes());
                s.write_all(&msg).await?;
                msg.zeroize();
                let mut r = [0u8; 2];
                s.read_exact(&mut r).await?;
                if r[1] != 0 {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
        let mut req = vec![5, 1, 0];
        match host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V4(ip)) => {
                req.push(1);
                req.extend_from_slice(&ip.octets());
            }
            Ok(std::net::IpAddr::V6(ip)) => {
                req.push(4);
                req.extend_from_slice(&ip.octets());
            }
            Err(_) if host.len() <= 255 => {
                req.push(3);
                req.push(host.len() as u8);
                req.extend_from_slice(host.as_bytes());
            }
            Err(_) => return Ok(false),
        }
        req.extend_from_slice(&port.to_be_bytes());
        s.write_all(&req).await?;
        let mut head = [0u8; 4];
        s.read_exact(&mut head).await?;
        Ok(head[1] == 0)
    }
    run(s, host, port, auth).await.unwrap_or(false)
}

async fn socks4(s: &mut TcpStream, host: &str, port: u16, user: &str) -> bool {
    let mut req = vec![4, 1];
    req.extend_from_slice(&port.to_be_bytes());
    let ipv4 = host.parse::<std::net::Ipv4Addr>().ok();
    match ipv4 {
        Some(ip) => req.extend_from_slice(&ip.octets()),
        // SOCKS4a: 0.0.0.x and the name at the end.
        None => req.extend_from_slice(&[0, 0, 0, 1]),
    }
    req.extend_from_slice(user.as_bytes());
    req.push(0);
    if ipv4.is_none() {
        req.extend_from_slice(host.as_bytes());
        req.push(0);
    }
    if s.write_all(&req).await.is_err() {
        return false;
    }
    let mut reply = [0u8; 8];
    s.read_exact(&mut reply).await.is_ok() && reply[1] == 0x5A
}

async fn http_connect(
    s: &mut TcpStream,
    host: &str,
    port: u16,
    auth: Option<(&str, &str)>,
) -> bool {
    use base64::Engine;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some((user, pass)) = auth {
        let mut plain = format!("{user}:{pass}");
        let token = base64::engine::general_purpose::STANDARD.encode(&plain);
        plain.zeroize();
        req.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    req.push_str("\r\n");
    let sent = s.write_all(req.as_bytes()).await.is_ok();
    req.zeroize();
    if !sent {
        return false;
    }
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > 16 * 1024 {
            return false;
        }
        let mut b = [0u8; 1];
        if s.read_exact(&mut b).await.is_err() {
            return false;
        }
        head.push(b[0]);
    }
    let text = String::from_utf8_lossy(&head);
    text.lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        == Some("200")
}

// ----- Model -----

/// Statuses of the hosts, shared by every window.
pub struct HostStatus {
    model: Entity<AppModel>,
    book: Book,
    /// Wakes the views when the next host is due.
    timer: Option<Task<()>>,
}

struct HostStatusGlobal(Entity<HostStatus>);

impl Global for HostStatusGlobal {}

impl HostStatus {
    /// The one of the app (created by the first hosts list).
    pub fn global(model: &Entity<AppModel>, cx: &mut App) -> Entity<Self> {
        if let Some(g) = cx.try_global::<HostStatusGlobal>() {
            return g.0.clone();
        }
        let model = model.clone();
        let entity = cx.new(|_| HostStatus {
            model,
            book: Book::default(),
            timer: None,
        });
        cx.set_global(HostStatusGlobal(entity.clone()));
        entity
    }

    pub fn shown(&self, id: Id, target: Result<&Target, Skip>) -> Shown {
        self.book.shown(id, target, Instant::now())
    }

    pub fn is_running(&self, id: Id) -> bool {
        self.book.is_running(id)
    }

    /// The list on screen: checks the hosts that are due and wakes the
    /// views when the next one is.
    pub fn tick(&mut self, probes: Vec<Probe>, cx: &mut Context<Self>) {
        let now = Instant::now();
        let targets: Vec<(Id, &Target)> = probes.iter().map(|p| (p.host_id, &p.target)).collect();
        let due = self.book.due(&targets, now, EVERY);
        let next = if due.is_empty() {
            self.book.next_due(&targets, now, EVERY)
        } else {
            None
        };
        if !due.is_empty() {
            let run: Vec<Probe> = probes
                .iter()
                .filter(|p| due.contains(&p.host_id))
                .cloned()
                .collect();
            self.run(run, cx);
        } else if let Some(wait) = next {
            // Rendering again then asks for the hosts that are due.
            self.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(wait + Duration::from_millis(250))
                    .await;
                let _ = this.update(cx, |_, cx| cx.notify());
            }));
        }
    }

    /// "Check now": these hosts at once, whatever their last check.
    pub fn check_now(&mut self, probes: Vec<Probe>, cx: &mut Context<Self>) {
        let run: Vec<Probe> = probes
            .into_iter()
            .filter(|p| !self.book.is_running(p.host_id))
            .collect();
        self.run(run, cx);
    }

    fn run(&mut self, probes: Vec<Probe>, cx: &mut Context<Self>) {
        if probes.is_empty() {
            return;
        }
        let ids: Vec<Id> = probes.iter().map(|p| p.host_id).collect();
        self.book.start(&ids);
        cx.notify();
        let ws = self.model.read(cx).ws.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Id, Target, Reach)>();
        let rt = runtime::handle(cx);
        rt.spawn(async move {
            futures::stream::iter(probes)
                .map(|p| {
                    let ws = ws.clone();
                    async move {
                        let mut password = match p.secret {
                            Some(item) => ws
                                .item_secret::<termoak_core::model::Host>(item)
                                .await
                                .ok()
                                .and_then(|s: HostSecret| s.proxy_password),
                            None => None,
                        };
                        let reach = check(&p.target, password.as_deref()).await;
                        password.zeroize();
                        tracing::debug!(host = %p.host_id, ?reach, "host status");
                        (p.host_id, p.target, reach)
                    }
                })
                .buffer_unordered(CONCURRENCY)
                .for_each(|res| {
                    let _ = tx.send(res);
                    async {}
                })
                .await;
        });
        cx.spawn(async move |this, cx| {
            while let Some((id, target, reach)) = rx.recv().await {
                let alive = this
                    .update(cx, |s, cx| {
                        s.book.finish(id, target, reach, Instant::now());
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termoak_core::new_id;

    fn host(group: Option<Id>, settings: HostSettings) -> Host {
        Host {
            id: new_id(),
            label: "web".into(),
            address: "web.example".into(),
            group_id: group,
            tags: Vec::new(),
            settings,
            notes: String::new(),
            color: None,
            os: None,
            os_version: None,
            favorite: false,
            protocol: Default::default(),
            icon: None,
        }
    }

    fn group(id: Id, parent: Option<Id>, settings: HostSettings) -> Group {
        Group {
            id,
            name: "g".into(),
            parent_id: parent,
            color: None,
            settings,
        }
    }

    fn target(port: u16) -> Target {
        Target {
            address: "h".into(),
            port,
            proxy: None,
        }
    }

    #[test]
    fn settings_come_from_the_groups() {
        let (outer, inner) = (new_id(), new_id());
        let groups = [
            group(
                outer,
                None,
                HostSettings {
                    port: Some(2200),
                    proxy: Some(ProxySettings {
                        kind: ProxyKind::Http,
                        host: "proxy".into(),
                        port: 3128,
                        username: None,
                    }),
                    ..Default::default()
                },
            ),
            group(
                inner,
                Some(outer),
                HostSettings {
                    port: Some(2222),
                    ..Default::default()
                },
            ),
        ];
        let refs: Vec<&Group> = groups.iter().collect();
        let h = host(Some(inner), HostSettings::default());
        let s = effective_settings(&h, &refs);
        assert_eq!(s.port, Some(2222));
        assert_eq!(s.proxy.as_ref().map(|p| p.port), Some(3128));
        // The host's own settings win.
        let h = host(
            Some(inner),
            HostSettings {
                port: Some(22),
                ..Default::default()
            },
        );
        assert_eq!(effective_settings(&h, &refs).port, Some(22));
        // A loop of groups ends.
        let a = new_id();
        let looped = [group(a, Some(a), HostSettings::default())];
        let refs: Vec<&Group> = looped.iter().collect();
        effective_settings(&host(Some(a), HostSettings::default()), &refs);
    }

    #[test]
    fn what_is_not_checked() {
        let plain = host(None, HostSettings::default());
        let s = HostSettings::default();
        let (t, needs) = target_of(&plain, &s, false, true, false).unwrap();
        assert_eq!(t.port, 22);
        assert_eq!(t.address, "web.example");
        assert!(!needs);
        assert_eq!(target_of(&plain, &s, false, true, true), Err(Skip::Off));
        assert_eq!(target_of(&plain, &s, true, true, false), Err(Skip::Strict));
        let jumps = HostSettings {
            jump_host_ids: Some(vec![new_id()]),
            ..Default::default()
        };
        assert_eq!(
            target_of(&plain, &jumps, false, true, false),
            Err(Skip::Jump)
        );
        // An empty chain is no chain.
        let empty = HostSettings {
            jump_host_ids: Some(Vec::new()),
            ..Default::default()
        };
        assert!(target_of(&plain, &empty, false, true, false).is_ok());
        // A proxy with a user needs its password: Use-only cannot read it.
        let mut proxied = HostSettings {
            proxy: Some(ProxySettings {
                kind: ProxyKind::Socks5,
                host: "p".into(),
                port: 1080,
                username: Some("me".into()),
            }),
            ..Default::default()
        };
        assert_eq!(
            target_of(&plain, &proxied, false, false, false),
            Err(Skip::UseOnly)
        );
        let (t, needs) = target_of(&plain, &proxied, false, true, false).unwrap();
        assert!(needs && t.proxy.is_some());
        // SOCKS4 only sends the user.
        proxied.proxy.as_mut().unwrap().kind = ProxyKind::Socks4;
        assert_eq!(
            target_of(&plain, &proxied, false, false, false).map(|(_, n)| n),
            Ok(false)
        );
        // A proxy without a host is ignored (as when connecting).
        proxied.proxy.as_mut().unwrap().host = " ".into();
        let (t, _) = target_of(&plain, &proxied, false, false, false).unwrap();
        assert!(t.proxy.is_none());
        // IPv6 in brackets.
        let mut v6 = host(None, HostSettings::default());
        v6.address = "[::1]".into();
        assert_eq!(
            target_of(&v6, &s, false, true, false).unwrap().0.address,
            "::1"
        );
    }

    #[test]
    fn scheduler_checks_what_is_due() {
        let (a, b, c) = (new_id(), new_id(), new_id());
        let (ta, tb, tc) = (target(22), target(22), target(22));
        let mut book = Book::default();
        let t0 = Instant::now();
        let targets = [(a, &ta), (b, &tb), (c, &tc)];
        // Nothing known: all of them.
        assert_eq!(book.due(&targets, t0, EVERY), vec![a, b, c]);
        assert_eq!(book.next_due(&targets, t0, EVERY), Some(Duration::ZERO));
        book.start(&[a, b, c]);
        // Running: not again.
        assert!(book.due(&targets, t0, EVERY).is_empty());
        assert_eq!(book.next_due(&targets, t0, EVERY), None);
        book.finish(a, ta.clone(), Reach::Up(Duration::from_millis(23)), t0);
        book.finish(b, tb.clone(), Reach::Down, t0 + Duration::from_secs(10));
        book.finish(c, tc.clone(), Reach::Up(Duration::from_millis(5)), t0);
        let t1 = t0 + Duration::from_secs(30);
        assert!(book.due(&targets, t1, EVERY).is_empty());
        assert_eq!(
            book.next_due(&targets, t1, EVERY),
            Some(Duration::from_secs(30))
        );
        // A minute later: the oldest checks first.
        let t2 = t0 + Duration::from_secs(75);
        assert_eq!(book.due(&targets, t2, EVERY), vec![a, c, b]);
        // An edited host is checked at once and shown as pending.
        let moved = target(2222);
        let edited = [(a, &moved)];
        assert_eq!(book.due(&edited, t1, EVERY), vec![a]);
        assert_eq!(book.shown(a, Ok(&moved), t1), Shown::Pending);
        // What the cards show.
        assert_eq!(
            book.shown(a, Ok(&ta), t1),
            Shown::Up {
                rtt: Duration::from_millis(23),
                ago: Duration::from_secs(30)
            }
        );
        assert_eq!(
            book.shown(b, Ok(&tb), t1),
            Shown::Down {
                ago: Duration::from_secs(20)
            }
        );
        assert_eq!(
            book.shown(a, Err(Skip::Jump), t1),
            Shown::Skipped(Skip::Jump)
        );
        assert_eq!(book.shown(new_id(), Ok(&ta), t1), Shown::Pending);
    }

    #[tokio::test]
    async fn checks_a_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let open = Target {
            address: "127.0.0.1".into(),
            port,
            proxy: None,
        };
        assert!(matches!(check(&open, None).await, Reach::Up(_)));
        drop(listener);
        // Closed now: refused.
        assert_eq!(check(&open, None).await, Reach::Down);
    }

    #[tokio::test]
    async fn checks_through_a_socks5_proxy() {
        let dest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest_port = dest.local_addr().unwrap().port();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut c, _) = proxy.accept().await.unwrap();
            let mut g = [0u8; 3];
            c.read_exact(&mut g).await.unwrap();
            c.write_all(&[5, 0]).await.unwrap();
            let mut head = [0u8; 4];
            c.read_exact(&mut head).await.unwrap();
            assert_eq!(head[3], 1);
            let mut rest = [0u8; 6];
            c.read_exact(&mut rest).await.unwrap();
            let port = u16::from_be_bytes([rest[4], rest[5]]);
            let ok = TcpStream::connect(("127.0.0.1", port)).await.is_ok();
            let code = if ok { 0 } else { 5 };
            c.write_all(&[5, code, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
        });
        let t = Target {
            address: "127.0.0.1".into(),
            port: dest_port,
            proxy: Some(ProxySettings {
                kind: ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                port: proxy_port,
                username: None,
            }),
        };
        assert!(matches!(check(&t, None).await, Reach::Up(_)));
        drop(dest);
    }
}
