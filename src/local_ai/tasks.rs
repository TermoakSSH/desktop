//! AI tasks, on the account's server or on this computer.
//!
//! The AI section and its conversations talk to an [`AiBackend`]: the
//! server's API, or the local engine (`termoak_ai::AiEngine` over the local
//! database, with the chain chosen in Settings → AI). Both answer with the
//! same JSON (`TaskView`), so the views do not change. In local mode nothing
//! is sent to the server.

use std::sync::Arc;

use serde_json::{Value, json};
use termoak_ai::engine::{AiEngine, CreateTask};
use termoak_ai::{AiError, ApprovalDecision, PermissionMode};
use termoak_client::{ApiClient, LOCAL_OWNER};
use termoak_core::Id;

use super::copilot::error_text;
use crate::state::AiFailure;

/// Where the AI tasks are.
#[derive(Clone)]
pub enum AiBackend {
    Server(ApiClient),
    Local(Arc<AiEngine>),
}

/// The backend for the current choice: the local engine with "This
/// computer" (`None` until it has started), the server otherwise (`None`
/// when signed out). Never the server in local mode.
pub fn choose_backend(
    local: bool,
    api: Option<ApiClient>,
    engine: Option<Arc<AiEngine>>,
) -> Option<AiBackend> {
    if local {
        engine.map(AiBackend::Local)
    } else {
        api.map(AiBackend::Server)
    }
}

/// A failure of the local engine for the interface.
pub fn local_failure(e: AiError) -> AiFailure {
    match e {
        // Missing key, no agent, Antigravity off: fixed in Settings → AI.
        AiError::KeyRequired(text) => AiFailure {
            text,
            fix_in_settings: true,
        },
        AiError::NotFound(_) => AiFailure::other(t!("local_ai.error.task_not_found").to_string()),
        AiError::Invalid(m) => AiFailure::other(m),
        other => AiFailure::other(error_text(&other)),
    }
}

fn to_json<T: serde::Serialize>(v: T) -> Result<Value, AiFailure> {
    serde_json::to_value(v).map_err(|e| AiFailure::other(e.to_string()))
}

impl AiBackend {
    pub fn is_local(&self) -> bool {
        matches!(self, AiBackend::Local(_))
    }

    /// The latest tasks.
    pub async fn list(&self, limit: i64) -> Result<Vec<Value>, AiFailure> {
        match self {
            AiBackend::Server(api) => Ok(api
                .get::<Value>(&format!("/api/v1/ai/tasks?limit={limit}"))
                .await?
                .as_array()
                .cloned()
                .unwrap_or_default()),
            AiBackend::Local(e) => e
                .list(LOCAL_OWNER, limit)
                .await
                .map_err(local_failure)?
                .into_iter()
                .map(to_json)
                .collect(),
        }
    }

    /// A task with its conversation.
    pub async fn get(&self, id: Id) -> Result<Value, AiFailure> {
        match self {
            AiBackend::Server(api) => Ok(api
                .get::<Value>(&format!("/api/v1/ai/tasks/{id}?messages=true"))
                .await?),
            AiBackend::Local(e) => {
                to_json(e.get(LOCAL_OWNER, id, true).await.map_err(local_failure)?)
            }
        }
    }

    /// Creates a task (`POST /api/v1/ai/tasks` body).
    pub async fn create(&self, body: Value) -> Result<Value, AiFailure> {
        match self {
            AiBackend::Server(api) => Ok(api.post::<Value>("/api/v1/ai/tasks", &body).await?),
            AiBackend::Local(e) => {
                let mut req: CreateTask = serde_json::from_value(body)
                    .map_err(|err| AiFailure::other(err.to_string()))?;
                // The chain comes from Settings → AI.
                req.provider = None;
                to_json(
                    e.create_task(LOCAL_OWNER, req)
                        .await
                        .map_err(local_failure)?,
                )
            }
        }
    }

    /// Continues a conversation.
    pub async fn message(&self, id: Id, text: &str) -> Result<Value, AiFailure> {
        match self {
            AiBackend::Server(api) => Ok(api
                .post::<Value>(
                    &format!("/api/v1/ai/tasks/{id}/messages"),
                    &json!({"text": text}),
                )
                .await?),
            AiBackend::Local(e) => to_json(
                e.send_message(LOCAL_OWNER, id, text)
                    .await
                    .map_err(local_failure)?,
            ),
        }
    }

    /// Answers an approval: approve or deny, `always`, the edited command
    /// or plan and the reason of a denial. (A server older than 0.5 ignores
    /// `edited` and `reason`: the interface only offers editing when the
    /// approval has a preview, which those servers do not send.)
    pub async fn decide(
        &self,
        id: Id,
        approval: Id,
        decision: ApprovalDecision,
    ) -> Result<(), AiFailure> {
        match self {
            AiBackend::Server(api) => {
                api.post::<Value>(
                    &format!("/api/v1/ai/tasks/{id}/approvals/{approval}"),
                    &serde_json::to_value(&decision).unwrap_or_default(),
                )
                .await?;
                Ok(())
            }
            AiBackend::Local(e) => e
                .decide_with(LOCAL_OWNER, id, approval, decision, "user")
                .await
                .map_err(local_failure),
        }
    }

    pub async fn set_mode(&self, id: Id, mode: &str) -> Result<(), AiFailure> {
        match self {
            AiBackend::Server(api) => {
                api.post::<Value>(
                    &format!("/api/v1/ai/tasks/{id}/mode"),
                    &json!({"mode": mode}),
                )
                .await?;
                Ok(())
            }
            AiBackend::Local(e) => e
                .set_mode(
                    LOCAL_OWNER,
                    id,
                    PermissionMode::parse(mode).unwrap_or_default(),
                )
                .await
                .map_err(local_failure),
        }
    }

    pub async fn cancel(&self, id: Id) -> Result<(), AiFailure> {
        match self {
            AiBackend::Server(api) => {
                api.post::<Value>(&format!("/api/v1/ai/tasks/{id}/cancel"), &json!({}))
                    .await?;
                Ok(())
            }
            AiBackend::Local(e) => e.cancel(LOCAL_OWNER, id).await.map_err(local_failure),
        }
    }

    pub async fn delete(&self, id: Id) -> Result<(), AiFailure> {
        match self {
            AiBackend::Server(api) => {
                api.delete(&format!("/api/v1/ai/tasks/{id}")).await?;
                Ok(())
            }
            AiBackend::Local(e) => e.delete(LOCAL_OWNER, id).await.map_err(local_failure),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use parking_lot::Mutex;
    use termoak_ai::config::AiConfig;
    use termoak_ai::engine::TaskEvent;
    use termoak_ai::{ChainEntry, ChainSource, SessionAccess, SessionSummary, TerminalOutput};
    use termoak_core::crypto::MasterKey;
    use termoak_core::store::AiTaskRow;
    use termoak_core::{Store, new_id};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::local_ai::copilot::{LocalAi, PrepareError, check_agent_allowed, spent_this_month};

    #[derive(Default)]
    struct FakeTerminal {
        typed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl SessionAccess for FakeTerminal {
        async fn list(&self, _: Id) -> Vec<SessionSummary> {
            Vec::new()
        }
        async fn read(&self, _: Id, _: Id, _: usize) -> Result<String, String> {
            Ok("$ ".into())
        }
        async fn send(&self, _: Id, _: Id, input: &str) -> Result<(), String> {
            self.typed.lock().push(input.to_string());
            Ok(())
        }
        async fn send_and_collect(
            &self,
            _: Id,
            _: Id,
            input: &str,
            _: Duration,
            _: Duration,
        ) -> Result<Option<TerminalOutput>, String> {
            self.typed.lock().push(input.to_string());
            Ok(Some(TerminalOutput {
                text: "done".into(),
                still_running: false,
            }))
        }
    }

    /// The user's own key for a mock OpenAI-compatible server.
    struct MockChain;

    #[async_trait]
    impl ChainSource for MockChain {
        async fn chain(&self, _: Id, _: Option<&str>) -> Result<Vec<ChainEntry>, AiError> {
            Ok(vec![ChainEntry {
                spec: "openrouter::test-model".into(),
                own_key: Some(zeroize::Zeroizing::new("sk-local-1".into())),
            }])
        }
    }

    /// OpenAI-compatible server answering each request with the next turn
    /// (the last one again when they run out). Returns its URL and the
    /// requests it got.
    async fn mock_openai(turns: Vec<Vec<Value>>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let body_start = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                while buf.len() < body_start + len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                }
                let n = {
                    let mut l = log.lock();
                    l.push(String::from_utf8_lossy(&buf).to_string());
                    l.len()
                };
                let events = &turns[(n - 1).min(turns.len() - 1)];
                let mut out = String::from(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
                );
                for e in events {
                    out.push_str(&format!("data: {e}\n\n"));
                }
                out.push_str("data: [DONE]\n\n");
                let _ = sock.write_all(out.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/v1"), seen)
    }

    fn config(base_url: &str) -> AiConfig {
        let mut config = AiConfig::default();
        let mut provider = termoak_ai::config::builtin_providers()["openrouter"].clone();
        provider.base_url = Some(base_url.to_string());
        config.providers.insert("openrouter".into(), provider);
        config
    }

    /// A "Termoak server" that counts connections (none expected).
    async fn counting_server() -> (ApiClient, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        tokio::spawn(async move {
            while let Ok((_sock, _)) = listener.accept().await {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        (ApiClient::new(&url).unwrap(), count)
    }

    async fn until_finished(
        events: &mut tokio::sync::broadcast::Receiver<termoak_ai::UserEvent>,
        task: Id,
        mut on: impl FnMut(&TaskEvent),
    ) -> TaskEvent {
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(20), events.recv())
                .await
                .expect("the task did not finish")
                .unwrap();
            if ev.task_id != task {
                continue;
            }
            if matches!(ev.event, TaskEvent::Finished { .. }) {
                return ev.event;
            }
            on(&ev.event);
        }
    }

    #[tokio::test]
    async fn local_task_lifecycle_persistence_and_restart() {
        let session = new_id();
        let tool_turn = vec![
            json!({"model": "test-model", "choices": [{"delta": {"content": "Restarting it."}}]}),
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "send_to_terminal", "arguments": json!({"session_id": session, "input": "systemctl restart nginx"}).to_string()}}]}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10, "cost": 0.0006}}),
        ];
        let text_turn = vec![
            json!({"choices": [{"delta": {"content": "Restarted."}}]}),
            json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
            json!({"choices": [], "usage": {"prompt_tokens": 150, "completion_tokens": 8, "cost": 0.0006}}),
        ];
        let (url, seen) = mock_openai(vec![tool_turn, text_turn]).await;
        let store = Store::open_in_memory(MasterKey::generate()).unwrap();
        let term = Arc::new(FakeTerminal::default());
        let local = Arc::new(LocalAi::new(store.clone(), term.clone()));
        let engine = local
            .start_engine_with(config(&url), Arc::new(MockChain))
            .await
            .unwrap();

        // Local mode never picks the server, even when signed in.
        let (api, server_hits) = counting_server().await;
        assert!(choose_backend(true, Some(api.clone()), None).is_none());
        let backend = choose_backend(true, Some(api), Some(engine.clone())).unwrap();
        assert!(backend.is_local());

        let mut events = engine.subscribe();
        let created = backend
            .create(json!({"prompt": "restart nginx", "mode": "ask", "provider": "claude"}))
            .await
            .unwrap();
        let id: Id = created["id"].as_str().unwrap().parse().unwrap();

        // The change waits for the approval, which is given.
        let mut approval = None;
        let finished = until_finished(&mut events, id, |ev| {
            if let TaskEvent::ApprovalRequested { approval_id, .. } = ev {
                approval = Some(*approval_id);
            }
        });
        let decide = async {
            loop {
                let pending = backend.get(id).await.unwrap()["pending_approvals"].clone();
                if let Some(a) = pending.as_array().and_then(|a| a.first()) {
                    let aid: Id = a["id"].as_str().unwrap().parse().unwrap();
                    backend
                        .decide(id, aid, ApprovalDecision::approve())
                        .await
                        .unwrap();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let (finished, _) = tokio::join!(finished, decide);
        assert!(approval.is_some());
        assert!(matches!(
            finished,
            TaskEvent::Finished {
                status: termoak_ai::TaskStatus::Completed,
                ..
            }
        ));
        assert_eq!(term.typed.lock().as_slice(), ["systemctl restart nginx\r"]);

        // Saved in the local database with its conversation.
        let task = backend.get(id).await.unwrap();
        assert_eq!(task["status"], "completed");
        assert_eq!(task["result"], "Restarted.");
        assert!(task["messages"].as_array().unwrap().len() >= 4);
        assert_eq!(backend.list(10).await.unwrap().len(), 1);
        let requests = seen.lock().clone();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .to_lowercase()
                .contains("authorization: bearer sk-local-1")
        );
        assert!(requests[1].contains("done"));
        // Usage recorded on this computer.
        assert_eq!(spent_this_month(&store).await.unwrap(), 1200);

        // The app closes while a task runs: on restart it is stopped, with
        // the reason, and it can be continued.
        let mut row: AiTaskRow = store.ai_task(LOCAL_OWNER, id).await.unwrap();
        row.status = "running".into();
        row.finished_at = None;
        store.ai_update_task(row).await.unwrap();
        let restarted = Arc::new(LocalAi::new(store.clone(), term.clone()));
        let engine = restarted
            .start_engine_with(config(&url), Arc::new(MockChain))
            .await
            .unwrap();
        let backend = AiBackend::Local(engine.clone());
        let task = backend.get(id).await.unwrap();
        assert_eq!(task["status"], "failed");
        assert_eq!(
            task["error"].as_str(),
            Some(t!("local_ai.interrupted").as_ref())
        );
        let mut events = engine.subscribe();
        backend.message(id, "Continue").await.unwrap();
        let finished = until_finished(&mut events, id, |_| {}).await;
        assert!(matches!(
            finished,
            TaskEvent::Finished {
                status: termoak_ai::TaskStatus::Completed,
                ..
            }
        ));

        // Nothing went to the server.
        assert_eq!(server_hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn antigravity_needs_the_opt_in() {
        let mut s = crate::local_ai::AiSettings::default();
        assert_eq!(
            check_agent_allowed("agy", &s),
            Err(PrepareError::AgyDisabled)
        );
        assert!(check_agent_allowed("claude", &s).is_ok());
        s.agy_opt_in = true;
        assert!(check_agent_allowed("agy", &s).is_ok());
        // It is fixed in Settings → AI.
        let f = local_failure(PrepareError::AgyDisabled.into_ai_error());
        assert!(f.fix_in_settings);
        assert!(f.text.contains("Antigravity"));
        assert!(!local_failure(AiError::Timeout("x".into())).fix_in_settings);
    }
}
