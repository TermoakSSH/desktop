//! The hosts of the AI on this computer: what the app shows.
//!
//! Since accounts and vaults, hosts live in several stores: This device
//! (the device store) and one store per account. The AI engine asks this
//! [`HostProvider`] instead of reading one store:
//!
//! - the **inventory** is the hosts and groups of the current view (This
//!   device and the accounts in sight, the same item seen through two
//!   accounts listed once), with where each one is;
//! - **connections** resolve each host in its own store (settings of its
//!   groups, identity, key and jump hosts from its vault, then This device),
//!   with just-in-time credentials from the server for Use-only vaults.
//!   Hosts of Strict vaults, and Use-only hosts of a signed-out account, are
//!   listed but marked unavailable: the engine refuses them with the
//!   reason. Only known host keys are accepted (nobody can confirm a new one
//!   in the middle of a task);
//! - snippets and memories come from the same stores; new memories go
//!   where their host is (when it can be changed) or to This device.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::{Mutex, RwLock};
use termoak_ai::hosts::{GroupEntry, HostEntry, HostProvider, Inventory};
use termoak_client::{ItemAccess, ItemFilter, ItemRef, LOCAL_OWNER, SaveTarget, Scope, Workspace};
use termoak_core::Id;
use termoak_core::model::{Group, Host, Memory, SecretUpdate, Snippet};
use termoak_ssh::{ConnectOptions, Connection, HostKeyPolicy};

use crate::state::{Item, dedupe};

/// Idle connections are closed after this long.
const IDLE: Duration = Duration::from_secs(300);

/// What the AI sees: the stores of the current view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiView {
    pub filter: ItemFilter,
    /// The current account: its copy wins when two accounts see one item.
    pub first: Option<Id>,
}

impl Default for AiView {
    fn default() -> Self {
        Self {
            filter: ItemFilter::all(),
            first: None,
        }
    }
}

struct Slot {
    conn: Option<Arc<Connection>>,
    last_used: Instant,
}

/// The hosts of the app's workspace (see the module).
pub struct WorkspaceHosts {
    ws: Workspace,
    view: RwLock<AiView>,
    slots: Mutex<HashMap<ItemRef, Arc<tokio::sync::Mutex<Slot>>>>,
}

/// Where an inventory host lives.
fn item_of(host: &HostEntry) -> ItemRef {
    ItemRef {
        scope: Scope::from_account(host.source),
        id: host.id,
    }
}

impl WorkspaceHosts {
    /// Needs a tokio runtime (it starts the task that closes idle
    /// connections).
    pub fn new(ws: Workspace) -> Arc<Self> {
        let hosts = Arc::new(Self {
            ws,
            view: RwLock::new(AiView::default()),
            slots: Mutex::new(HashMap::new()),
        });
        let weak: Weak<Self> = Arc::downgrade(&hosts);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                let Some(hosts) = weak.upgrade() else { break };
                hosts.close_idle().await;
            }
        });
        hosts
    }

    /// Follows the view of the app (This device, one account, all).
    pub fn set_view(&self, view: AiView) {
        *self.view.write() = view;
    }

    pub fn view(&self) -> AiView {
        self.view.read().clone()
    }

    async fn list<T: termoak_core::model::Entity>(&self) -> Result<Vec<Item<T>>, String> {
        let view = self.view();
        let items = self
            .ws
            .list_items::<T>(&view.filter)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(Item::from)
            .collect();
        Ok(dedupe(items, view.first))
    }

    /// Where an item is, for people: "This device", the account, its vault.
    async fn location(&self, scope: Scope, vault: Option<Id>, cache: &mut Cache) -> String {
        let Scope::Account(a) = scope else {
            return t!("local_ai.hosts.this_device").to_string();
        };
        let Some(acc) = self.ws.account(a) else {
            return a.to_string();
        };
        let info = acc.info();
        let name = if info.name.trim().is_empty() {
            info.email.clone()
        } else {
            info.name.clone()
        };
        let vault_name = match vault {
            Some(v) if Some(v) != acc.user_id() => {
                if !cache.vaults.contains_key(&a) {
                    let list = acc.store.local_vault_list().await.unwrap_or_default();
                    cache
                        .vaults
                        .insert(a, list.into_iter().map(|v| (v.id, v.name)).collect());
                }
                cache.vaults[&a].get(&v).cloned()
            }
            _ => None,
        };
        match vault_name {
            Some(v) => format!("{name} · {v}"),
            None => name,
        }
    }

    /// Why a host of an account cannot be used from this computer.
    async fn unavailable(&self, item: &Item<Host>) -> Option<String> {
        let Scope::Account(a) = item.scope else {
            return None;
        };
        if item.access != ItemAccess::UseOnly {
            return None;
        }
        let acc = self.ws.account(a)?;
        let vault = item.rec.meta.vault_id.unwrap_or_default();
        if acc.is_strict(vault).await.unwrap_or(false) {
            return Some(t!("local_ai.hosts.strict").to_string());
        }
        if !acc.is_signed_in() {
            return Some(t!("local_ai.hosts.signed_out").to_string());
        }
        None
    }

    async fn close_idle(&self) {
        let slots: Vec<_> = self.slots.lock().values().cloned().collect();
        for slot in slots {
            let Ok(mut s) = slot.try_lock() else { continue };
            if s.last_used.elapsed() >= IDLE
                && let Some(conn) = s.conn.take()
            {
                conn.disconnect().await;
            }
        }
    }
}

#[derive(Default)]
struct Cache {
    /// Vault names per account.
    vaults: HashMap<Id, HashMap<Id, String>>,
}

#[async_trait]
impl HostProvider for WorkspaceHosts {
    async fn inventory(&self, _owner: Id) -> Result<Inventory, String> {
        let hosts = self.list::<Host>().await?;
        let groups = self.list::<Group>().await?;
        let mut cache = Cache::default();
        let mut out = Inventory {
            hosts: Vec::with_capacity(hosts.len()),
            groups: groups
                .into_iter()
                .map(|g| GroupEntry {
                    id: g.rec.data.id,
                    name: g.rec.data.name,
                    parent_id: g.rec.data.parent_id,
                })
                .collect(),
        };
        for item in hosts {
            let vault = item.rec.meta.vault_id;
            let location = self.location(item.scope, vault, &mut cache).await;
            let unavailable = self.unavailable(&item).await;
            let h = &item.rec.data;
            // The settings of its groups (in its own store).
            let settings = match self.ws.store_of(item.scope) {
                Ok(store) => store
                    .effective_settings(LOCAL_OWNER, h)
                    .await
                    .unwrap_or_else(|_| h.settings.clone()),
                Err(_) => h.settings.clone(),
            };
            out.hosts.push(HostEntry {
                port: settings
                    .port
                    .unwrap_or(if h.protocol.is_telnet() { 23 } else { 22 }),
                user: settings.username.filter(|u| !u.trim().is_empty()),
                group_id: h.group_id,
                tags: h.tags.clone(),
                os: h.os.clone(),
                notes: h.notes.clone(),
                protocol: h.protocol.as_str().to_string(),
                via_jump: settings.jump_host_ids.is_some_and(|j| !j.is_empty()),
                vault_id: vault,
                location: Some(location),
                source: item.scope.account(),
                use_only: item.access == ItemAccess::UseOnly,
                unavailable,
                ..HostEntry::new(h.id, h.label.clone(), h.address.clone())
            });
        }
        Ok(out)
    }

    async fn connect(&self, _owner: Id, host: &HostEntry) -> Result<Arc<Connection>, String> {
        let item = item_of(host);
        let slot = self
            .slots
            .lock()
            .entry(item)
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(Slot {
                    conn: None,
                    last_used: Instant::now(),
                }))
            })
            .clone();
        let mut slot = slot.lock().await;
        slot.last_used = Instant::now();
        if let Some(conn) = &slot.conn
            && !conn.is_closed()
        {
            return Ok(conn.clone());
        }
        if let Some(old) = slot.conn.take() {
            old.disconnect().await;
        }
        // Settings, identity, key and jumps from the host's own store; a
        // Use-only host gets its credentials from its server (refused in
        // Strict vaults).
        let mut resolved = self
            .ws
            .resolve_for(item, "ssh")
            .await
            .map_err(|e| e.to_string())?;
        let use_agent = crate::state::Settings::load(&self.ws.store).await.use_agent;
        let verifier = self
            .ws
            .verifier_for(item.scope, None, HostKeyPolicy::Strict)
            .map_err(|e| e.to_string())?;
        let opts = ConnectOptions::new(verifier).with_agent(use_agent);
        let conn = Connection::connect(&resolved, &opts).await;
        resolved.zeroize_secrets();
        let conn = conn.map_err(|e| e.to_string())?;
        slot.conn = Some(conn.clone());
        Ok(conn)
    }

    async fn invalidate(&self, _owner: Id, host: &HostEntry) {
        let slot = self.slots.lock().get(&item_of(host)).cloned();
        if let Some(slot) = slot
            && let Some(conn) = slot.lock().await.conn.take()
        {
            conn.disconnect().await;
        }
    }

    async fn snippets(&self, _owner: Id) -> Result<Vec<Snippet>, String> {
        Ok(self
            .list::<Snippet>()
            .await?
            .into_iter()
            .map(|s| s.rec.data)
            .collect())
    }

    async fn memories(&self, _owner: Id) -> Result<Vec<Memory>, String> {
        Ok(self
            .list::<Memory>()
            .await?
            .into_iter()
            .map(|m| m.rec.data)
            .collect())
    }

    /// With the host when it is on This device or in a vault the user can
    /// change; otherwise on This device, naming the host.
    async fn remember(
        &self,
        _owner: Id,
        host: Option<&HostEntry>,
        content: &str,
    ) -> Result<(), String> {
        let content = content.trim().to_string();
        let (target, host_id, content) = match host {
            Some(h) if h.source.is_none() => (SaveTarget::Device, Some(h.id), content),
            Some(h) if !h.use_only => (
                SaveTarget::Account {
                    account: h.source.unwrap_or_default(),
                    vault: h.vault_id,
                },
                Some(h.id),
                content,
            ),
            Some(h) => (SaveTarget::Device, None, format!("{}: {content}", h.label)),
            None => (SaveTarget::Device, None, content),
        };
        let memory = Memory {
            id: Id::nil(),
            content,
            host_id,
        };
        self.ws
            .save_item(target, memory, SecretUpdate::Keep, None)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn save_snippet(&self, _owner: Id, snippet: Snippet) -> Result<Snippet, String> {
        self.ws
            .save_item(SaveTarget::Device, snippet, SecretUpdate::Keep, None)
            .await
            .map(|s| s.record.data)
            .map_err(|e| e.to_string())
    }
}

/// An empty workspace in a temporary folder (tests).
#[cfg(test)]
pub fn test_workspace() -> Workspace {
    let dir = std::env::temp_dir().join(format!("termoak-ai-hosts-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    Workspace::open(&dir, termoak_core::crypto::MasterKey::generate()).unwrap()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use base64::Engine;
    use serde_json::{Value, json};
    use termoak_ai::config::AiConfig;
    use termoak_ai::engine::{AiEngine, CreateTask};
    use termoak_ai::tools::{ToolContext, ToolLimits, ToolRuntime};
    use termoak_ai::{AiError, ChainEntry, ChainSource, PermissionMode};
    use termoak_core::crypto::MasterKey;
    use termoak_core::model::{EntityKind, HostProtocol, SyncMode, SyncRecord, TokenPair, Vault};
    use termoak_core::store::SyncV2Apply;
    use termoak_core::{Store, new_id};

    use super::*;

    /// A workspace with one account (signed in to a server that does not
    /// answer, as Termoak 0.3 left it before accounts).
    async fn workspace_with_account() -> (Workspace, Id) {
        let dir = std::env::temp_dir().join(format!("termoak-ai-hosts-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = MasterKey::generate();
        {
            let store = Store::open(&dir.join("termoak.db"), key.clone()).unwrap();
            let tokens = TokenPair {
                access_token: "access".into(),
                access_expires_at: i64::MAX,
                refresh_token: "refresh".into(),
                refresh_expires_at: i64::MAX,
                device_id: Id::nil(),
            };
            let sealed = key
                .seal(
                    &serde_json::to_vec(&tokens).unwrap(),
                    b"aceitunoak:server-tokens",
                )
                .unwrap();
            let sealed = base64::engine::general_purpose::STANDARD.encode(sealed);
            store
                .meta_set("server.url", "http://127.0.0.1:9")
                .await
                .unwrap();
            store
                .meta_set("server.user", "ana@example.com")
                .await
                .unwrap();
            store.meta_set("server.tokens", &sealed).await.unwrap();
        }
        let ws = Workspace::open(&dir, key).unwrap();
        let account = ws.accounts()[0].id;
        (ws, account)
    }

    fn host(label: &str, address: &str, port: u16) -> Host {
        serde_json::from_value(json!({
            "label": label,
            "address": address,
            "settings": {"username": "deploy", "port": port},
        }))
        .unwrap()
    }

    async fn save_host(ws: &Workspace, target: SaveTarget, h: Host) -> Id {
        ws.save_item(target, h, SecretUpdate::Keep, None)
            .await
            .unwrap()
            .record
            .data
            .id
    }

    async fn save_group(ws: &Workspace, target: SaveTarget, name: &str, parent: Option<Id>) -> Id {
        let g: Group = serde_json::from_value(json!({"name": name, "parent_id": parent})).unwrap();
        ws.save_item(target, g, SecretUpdate::Keep, None)
            .await
            .unwrap()
            .record
            .data
            .id
    }

    /// A port where nothing listens.
    async fn closed_port() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    }

    fn tools(hosts: Arc<WorkspaceHosts>) -> ToolRuntime {
        ToolRuntime::with_hosts(
            hosts,
            None,
            ToolLimits {
                command_timeout: Duration::from_secs(10),
                max_output_chars: 16_000,
            },
        )
    }

    const CTX: ToolContext = ToolContext {
        owner: LOCAL_OWNER,
        task_id: None,
        host_scope: None,
    };

    fn listed(content: &str) -> Vec<Value> {
        serde_json::from_str(content).unwrap_or_else(|_| panic!("not a list: {content}"))
    }

    fn labels(list: &[Value]) -> Vec<String> {
        let mut l: Vec<String> = list
            .iter()
            .map(|r| r["label"].as_str().unwrap().to_string())
            .collect();
        l.sort();
        l
    }

    #[tokio::test]
    async fn this_device_without_an_account() {
        // The reported case: "This device", no account, hosts added in the app.
        let ws = test_workspace();
        let port = closed_port().await;
        save_host(&ws, SaveTarget::Device, host("web-1", "127.0.0.1", port)).await;
        let hosts = WorkspaceHosts::new(ws);
        let tools = tools(hosts);
        let out = tools.execute(&CTX, "list_hosts", &json!({})).await;
        assert!(out.ok, "{}", out.content);
        let list = listed(&out.content);
        assert_eq!(labels(&list), ["web-1"]);
        assert_eq!(list[0]["location"], "This device");
        assert_eq!(list[0]["protocol"], "ssh");
        let out = tools
            .execute(
                &CTX,
                "run_command",
                &json!({"host": "web-1", "command": "uptime"}),
            )
            .await;
        assert!(!out.ok);
        assert!(
            out.content.starts_with("could not connect to web-1:"),
            "{}",
            out.content
        );
    }

    struct NoServer(String);

    #[async_trait]
    impl ChainSource for NoServer {
        async fn chain(&self, _: Id, _: Option<&str>) -> Result<Vec<ChainEntry>, AiError> {
            Ok(vec![ChainEntry {
                spec: "openrouter::test-model".into(),
                own_key: Some(zeroize::Zeroizing::new(self.0.clone())),
            }])
        }
    }

    #[tokio::test]
    async fn device_and_account_hosts_together() {
        let (ws, account) = workspace_with_account().await;
        let in_account = SaveTarget::Account {
            account,
            vault: None,
        };
        let port = closed_port().await;

        // This device: web-1, a Telnet router and "api".
        let web1 = {
            let mut h = host("web-1", "127.0.0.1", port);
            h.tags = vec!["prod".into()];
            save_host(&ws, SaveTarget::Device, h).await
        };
        let mut router = host("router", "192.168.1.1", 23);
        router.protocol = HostProtocol::Telnet;
        save_host(&ws, SaveTarget::Device, router).await;
        let api_device = save_host(&ws, SaveTarget::Device, host("api", "10.0.2.1", 22)).await;

        // The account: web-2 and web-3 in Servers/Web, db and another "api".
        let servers = save_group(&ws, in_account, "Servers", None).await;
        let web_group = save_group(&ws, in_account, "Web", Some(servers)).await;
        let web2 = {
            let mut h = host("web-2", "127.0.0.2", port);
            h.group_id = Some(servers);
            h.tags = vec!["Prod".into()];
            save_host(&ws, in_account, h).await
        };
        let web3 = {
            let mut h = host("web-3", "10.0.0.3", 22);
            h.group_id = Some(web_group);
            save_host(&ws, in_account, h).await
        };
        save_host(&ws, in_account, host("db", "10.0.0.9", 22)).await;
        let api_account = save_host(&ws, in_account, host("api", "10.0.2.2", 22)).await;

        // Two shared vaults where the user is Use-only: one Strict.
        let acc = ws.account(account).unwrap();
        let (strict, shared) = (new_id(), new_id());
        let vault = |id: Id, name: &str, use_only_local: bool| -> Vault {
            serde_json::from_value(json!({
                "id": id, "kind": "shared", "name": name, "created_by": id, "owner_user_id": id,
                "created_at": 1, "updated_at": 1, "role": "use_only",
                "settings": {"use_only_local": use_only_local},
            }))
            .unwrap()
        };
        let record = |label: &str, vault: Id| {
            let id = new_id();
            let mut h = host(label, "10.9.9.9", 22);
            h.id = id;
            SyncRecord {
                id,
                kind: EntityKind::Host,
                data: serde_json::to_value(&h).unwrap(),
                secret: None,
                sync_mode: SyncMode::Synced,
                updated_at: 1,
                deleted: false,
                rev: 1,
                vault_id: Some(vault),
                has_secret: Some(true),
                sealed: None,
                base_rev: None,
            }
        };
        acc.store
            .apply_sync_v2(
                LOCAL_OWNER,
                SyncV2Apply {
                    vaults: vec![vault(strict, "Prod", false), vault(shared, "Ops", true)],
                    cursors: vec![(strict, 1), (shared, 1)],
                    changes: vec![record("vault-box", strict), record("ops-1", shared)],
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let hosts = WorkspaceHosts::new(ws.clone());
        let tools = tools(hosts.clone());

        // list_hosts: both places, with where each host is.
        let out = tools.execute(&CTX, "list_hosts", &json!({})).await;
        assert!(out.ok, "{}", out.content);
        let list = listed(&out.content);
        assert_eq!(
            labels(&list),
            [
                "api",
                "api",
                "db",
                "ops-1",
                "router",
                "vault-box",
                "web-1",
                "web-2",
                "web-3"
            ]
        );
        let row = |label: &str| list.iter().find(|r| r["label"] == label).unwrap().clone();
        assert_eq!(row("web-1")["location"], "This device");
        assert_eq!(row("web-2")["location"], "ana@example.com");
        assert_eq!(row("web-2")["group"], "Servers");
        assert_eq!(row("web-2")["user"], "deploy");
        assert_eq!(row("router")["protocol"], "telnet");
        assert!(
            row("router")["unavailable"]
                .as_str()
                .unwrap()
                .contains("Telnet")
        );
        assert_eq!(row("vault-box")["access"], "use_only");
        assert!(
            row("vault-box")["unavailable"]
                .as_str()
                .unwrap()
                .contains("Strict")
        );
        // Use-only but not Strict, signed in: usable with credentials from
        // its server.
        assert_eq!(row("ops-1")["access"], "use_only");
        assert!(row("ops-1").get("unavailable").is_none());

        // run_command finds web-1 (This device) and web-2 (the account),
        // each resolved in its own store.
        let run = |h: &str| json!({"host": h, "command": "uptime"});
        let out = tools.execute(&CTX, "run_command", &run("web-1")).await;
        assert!(
            out.content.starts_with("could not connect to web-1:"),
            "{}",
            out.content
        );
        let out = tools.execute(&CTX, "run_command", &run("WEB-2")).await;
        assert!(
            out.content.starts_with("could not connect to web-2:"),
            "{}",
            out.content
        );
        // Strict: refused before connecting. Use-only: asks its server for
        // the credentials (which does not answer here).
        let out = tools.execute(&CTX, "run_command", &run("vault-box")).await;
        assert!(
            !out.ok && out.content.contains("Strict") && !out.content.contains("connect"),
            "{}",
            out.content
        );
        let out = tools.execute(&CTX, "run_command", &run("ops-1")).await;
        assert!(
            out.content
                .contains("only be used while connected to its server"),
            "{}",
            out.content
        );
        // Telnet: refused with the reason.
        let out = tools.execute(&CTX, "run_command", &run("router")).await;
        assert!(
            !out.ok && out.content.contains("only work over SSH"),
            "{}",
            out.content
        );
        // Same name in both places: the candidates, not a guess.
        let out = tools.execute(&CTX, "run_command", &run("api")).await;
        assert!(out.content.contains("matches 2 hosts"), "{}", out.content);
        assert!(
            out.content.contains(&api_device.to_string())
                && out.content.contains(&api_account.to_string())
        );
        assert!(
            out.content.contains("This device") && out.content.contains("ana@example.com"),
            "{}",
            out.content
        );

        // Memories go where their host is.
        let out = tools
            .execute(
                &CTX,
                "remember",
                &json!({"content": "nginx lives in /etc/nginx", "host": "web-2"}),
            )
            .await;
        assert_eq!(out.content, "Noted.");
        let saved = acc.store.list::<Memory>(LOCAL_OWNER).await.unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].data.host_id, Some(web2));
        assert!(
            ws.store
                .list::<Memory>(LOCAL_OWNER)
                .await
                .unwrap()
                .is_empty()
        );

        // Multi-host tasks by group (with its subgroups), by tag and by
        // host ids, across both places.
        let mut config = AiConfig::default();
        let mut provider = termoak_ai::config::builtin_providers()["openrouter"].clone();
        provider.base_url = Some(format!("http://127.0.0.1:{port}/v1"));
        config.providers.insert("openrouter".into(), provider);
        let engine = AiEngine::with_hosts(ws.store.clone(), hosts.clone(), None, config)
            .await
            .unwrap();
        engine.set_chain_source(Arc::new(NoServer("sk-test".into())));
        let fan_out = |group: Option<Id>, tag: Option<&str>, ids: Option<Vec<Id>>| CreateTask {
            prompt: "check uptime".into(),
            mode: Some(PermissionMode::ReadOnly),
            group_id: group,
            tag: tag.map(str::to_string),
            host_ids: ids,
            fan_out: Some(true),
            ..Default::default()
        };
        let sorted = |mut v: Vec<Id>| {
            v.sort();
            v
        };
        let view = engine
            .create_task(LOCAL_OWNER, fan_out(Some(servers), None, None))
            .await
            .unwrap();
        assert_eq!(sorted(view.host_ids.unwrap()), sorted(vec![web2, web3]));
        let view = engine
            .create_task(LOCAL_OWNER, fan_out(None, Some("prod"), None))
            .await
            .unwrap();
        assert_eq!(sorted(view.host_ids.unwrap()), sorted(vec![web1, web2]));
        let view = engine
            .create_task(LOCAL_OWNER, fan_out(None, None, Some(vec![web1, web3])))
            .await
            .unwrap();
        assert_eq!(view.host_ids, Some(vec![web1, web3]));

        // Only This device in view: the account's hosts are not seen.
        hosts.set_view(AiView {
            filter: ItemFilter::device_only(),
            first: None,
        });
        let out = tools.execute(&CTX, "list_hosts", &json!({})).await;
        assert_eq!(labels(&listed(&out.content)), ["api", "router", "web-1"]);
        let out = tools.execute(&CTX, "run_command", &run("web-2")).await;
        assert!(
            out.content.contains("there is no host \"web-2\""),
            "{}",
            out.content
        );
        let err = engine
            .create_task(LOCAL_OWNER, fan_out(Some(servers), None, None))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no hosts in that group"), "{err}");
    }
}
