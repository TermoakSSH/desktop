//! A second device for the live check of phase 5b (not a test: `cargo test
//! -- --ignored e2e_5b_peer` with TERMOAK_SHOT_HOME, TERMOAK_VAULT_KEY,
//! TERMOAK_SHOT_SERVER, TERMOAK_SHOT_LOGIN (`email password`, read from a
//! file the caller keeps outside the repository) and TERMOAK_5B_CMD, with
//! TERMOAK_5B_OUT a directory for what it reports). The desktop's own
//! screens are checked in the app; this plays the other person or the
//! other device: set up E2EE, start and finish a pairing, open a host's
//! secret after a sync, test a host from the server with a per-test
//! unlock, reset.

use std::sync::Arc;

use termoak_client::{Account, ServerChoice, Workspace};
use termoak_core::model::Host;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

fn report(name: &str, text: &str) {
    let dir = env("TERMOAK_5B_OUT");
    if !dir.is_empty() {
        std::fs::write(format!("{dir}/{name}"), text).unwrap();
    }
    println!("{name}: {text}");
}

async fn account() -> (Workspace, Arc<Account>) {
    let login = std::fs::read_to_string(env("TERMOAK_SHOT_LOGIN")).unwrap();
    let mut parts = login.split_whitespace();
    let (email, password) = (parts.next().unwrap(), parts.next().unwrap());
    let key = termoak_core::crypto::MasterKey::from_base64(&env("TERMOAK_VAULT_KEY")).unwrap();
    let home = env("TERMOAK_SHOT_HOME");
    std::fs::create_dir_all(&home).unwrap();
    let ws = Workspace::open(std::path::Path::new(&home), key).unwrap();
    let acc = match ws.account_list().into_iter().find(|a| a.is_signed_in()) {
        Some(a) => a,
        None => ws
            .sign_in(
                ServerChoice::Custom(env("TERMOAK_SHOT_SERVER")),
                email,
                password,
                None,
            )
            .await
            .unwrap(),
    };
    (ws, acc)
}

#[tokio::test]
#[ignore]
async fn e2e_5b_peer() {
    if env("TERMOAK_SHOT_HOME").is_empty() {
        eprintln!(
            "TERMOAK_SHOT_HOME, TERMOAK_SHOT_SERVER, TERMOAK_SHOT_LOGIN and TERMOAK_5B_CMD are needed"
        );
        return;
    }
    let (ws, acc) = account().await;
    let cmd = env("TERMOAK_5B_CMD");
    let arg = env("TERMOAK_5B_ARG");
    match cmd.as_str() {
        "setup" => {
            let pass = Some(arg.as_str()).filter(|p| !p.is_empty());
            let (info, phrase) = acc.e2ee_setup(pass).await.unwrap();
            report("fingerprint", &info.fingerprint);
            report("phrase", &phrase);
        }
        "status" => {
            let st = acc.e2ee_status().await.unwrap();
            let here = acc.e2ee_user_key().await.unwrap().is_some();
            let r = acc.sync_once().await.unwrap();
            let vaults = acc.api.vaults().await.unwrap();
            let e2ee: Vec<String> = vaults
                .iter()
                .filter_map(|v| {
                    v.e2ee.as_ref().map(|e| {
                        format!(
                            "{} v{} wrap={} rekey={} pending={}",
                            v.name,
                            e.key_version,
                            e.wrap.is_some(),
                            e.rekey_needed,
                            e.pending.len()
                        )
                    })
                })
                .collect();
            report(
                "status",
                &format!(
                    "key={} unlocked_here={} pairings={} locked={} waiting={} vaults=[{}]",
                    st.key.map(|k| k.fingerprint).unwrap_or_default(),
                    here,
                    st.pairings.len(),
                    r.e2ee_locked,
                    r.e2ee_waiting,
                    e2ee.join("; ")
                ),
            );
        }
        "unlock-pass" => {
            let n = acc.e2ee_unlock_passphrase(&arg).await.unwrap();
            report("unlocked", &n.to_string());
        }
        "pair-start" => {
            let (start, key) = acc.e2ee_pairing_start().await.unwrap();
            let out = env("TERMOAK_5B_OUT");
            std::fs::write(format!("{out}/pairing.key"), key.as_slice()).unwrap();
            report("pairing", &format!("{} {}", start.id, start.code));
        }
        "pair-finish" => {
            let out = env("TERMOAK_5B_OUT");
            let raw = std::fs::read(format!("{out}/pairing.key")).unwrap();
            let key: [u8; 32] = raw.try_into().unwrap();
            let id = arg.parse().unwrap();
            let res = acc.e2ee_pairing_finish(id, &key).await;
            let _ = std::fs::remove_file(format!("{out}/pairing.key"));
            report("paired", &format!("{res:?}"));
        }
        "secret" => {
            acc.sync_once().await.unwrap();
            let hosts = acc.store.local_items(None).await.unwrap();
            let found = hosts
                .iter()
                .find(|i| i.data["label"].as_str() == Some(arg.as_str()));
            let Some(item) = found else {
                report("secret", "no such host");
                return;
            };
            let item_ref = termoak_client::ItemRef {
                scope: termoak_client::Scope::Account(acc.id),
                id: item.id,
            };
            let res = ws.item_secret::<Host>(item_ref).await;
            report(
                "secret",
                &match res {
                    Ok(s) => format!(
                        "open, password of {} characters",
                        s.password.map(|p| p.len()).unwrap_or(0)
                    ),
                    Err(e) => format!("error {:?}: {e}", crate::state::error_code(&e)),
                },
            );
        }
        "keysecret" => {
            acc.sync_once().await.unwrap();
            let items = acc.store.local_items(None).await.unwrap();
            let Some(item) = items.iter().find(|i| {
                i.data["label"].as_str() == Some(arg.as_str()) && i.data.get("public_key").is_some()
            }) else {
                report("keysecret", "no such key");
                return;
            };
            let item_ref = termoak_client::ItemRef {
                scope: termoak_client::Scope::Account(acc.id),
                id: item.id,
            };
            let res = ws
                .item_secret::<termoak_core::model::SshKey>(item_ref)
                .await;
            report(
                "keysecret",
                &match res {
                    Ok(s) => format!(
                        "open, private key {}",
                        if s.private_key
                            .is_some_and(|k| k.contains("OPENSSH PRIVATE KEY"))
                        {
                            "readable"
                        } else {
                            "missing"
                        }
                    ),
                    Err(e) => format!("error {:?}: {e}", crate::state::error_code(&e)),
                },
            );
        }
        "test-unlocked" => {
            // A server test of the host (trusting its key) with its
            // credentials sent for that test.
            let id: termoak_core::Id = arg.parse().unwrap();
            let unlock = crate::terminal::backend::unlock_payload(&ws, &acc, id)
                .await
                .unwrap();
            let v: serde_json::Value = acc
                .api
                .post(
                    &format!("/api/v1/hosts/{id}/test?trust=true"),
                    &serde_json::json!({"unlock": unlock}),
                )
                .await
                .unwrap();
            report("test", &v.to_string());
        }
        "export" => {
            let team: termoak_core::Id = arg.parse().unwrap();
            let out = env("TERMOAK_5B_OUT");
            let path = std::path::PathBuf::from(format!("{out}/audit-team.csv"));
            let filter = termoak_core::store::audit::AuditFilter {
                action: Some("e2ee.*,sso.*,audit.*".into()),
                ..Default::default()
            };
            let n = acc
                .audit_export(
                    termoak_client::audit::AuditOf::Team(team),
                    &filter,
                    "csv",
                    &path,
                )
                .await
                .unwrap();
            report("export", &format!("{n} bytes"));
        }
        "reset" => {
            let lost = acc.e2ee_reset(true).await.unwrap();
            report("reset", &format!("{lost:?}"));
        }
        other => panic!("unknown TERMOAK_5B_CMD {other}"),
    }
}
