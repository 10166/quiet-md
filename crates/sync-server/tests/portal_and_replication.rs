use futures_util::StreamExt;
use quiet_sync_proto::api::*;
use quiet_sync_proto::crypto::Envelope;
use quiet_sync_proto::entity::{sha256_hex, ContentBody, EntityKind, EntityPayload, SyncMeta};
use quiet_sync_server::config::ServerConfig;
use quiet_sync_server::{create_state, db, db_repl, router, AppState};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;

struct TestNode {
    base: String,
    dir: PathBuf,
    state: AppState,
    handle: tokio::task::JoinHandle<()>,
}

impl TestNode {
    async fn start(node_name: &str, admin_password: Option<&str>, public_url: Option<&str>) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = std::env::temp_dir().join(format!("quiet-sync-portal-test-{}", uuid::Uuid::new_v4()));
        let mut cfg = ServerConfig::for_test(dir.clone());
        cfg.node_name = Some(node_name.to_string());
        cfg.admin_password = admin_password.map(str::to_string);
        cfg.public_url = public_url.map(str::to_string);
        let state = create_state(cfg).unwrap();
        state.repl_supervisor.spawn(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { base: format!("http://{addr}"), dir, state, handle }
    }

    fn cleanup(self) {
        self.state.repl_supervisor.shutdown();
        self.handle.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn admin_login(client: &reqwest::Client, base: &str, password: &str) -> String {
    let resp: Value = client
        .post(format!("{base}/api/admin/auth/login"))
        .json(&json!({ "password": password }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    resp["token"].as_str().unwrap().to_string()
}

async fn register_user_with_doc_and_blob(
    client: &reqwest::Client,
    base: &str,
    email: &str,
    device_id: &str,
    blob_bytes: &[u8],
) -> (AuthResp, VaultSummary, String) {
    let auth: AuthResp = client
        .post(format!("{base}/v1/auth/register"))
        .json(&RegisterReq {
            email: email.into(),
            password: "password123".into(),
            display_name: email.split('@').next().unwrap_or("User").into(),
            device_id: device_id.into(),
            device_name: "Laptop".into(),
            platform: "macos".into(),
            invite_code: None,
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    let vault: VaultSummary = client
        .post(format!("{base}/v1/vaults"))
        .bearer_auth(&auth.access_token)
        .json(&CreateVaultReq { name: format!("{email}-vault"), e2ee: None })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();

    let blob_hash = sha256_hex(blob_bytes);
    let put = client
        .put(format!("{base}/v1/vaults/{}/blobs/{blob_hash}", vault.id))
        .bearer_auth(&auth.access_token)
        .body(blob_bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);

    let payload = EntityPayload::Document {
        name: "hello".into(),
        parent_id: None,
        ext: "xml".into(),
        content: ContentBody::inline_from_bytes(b"<doc>hello</doc>"),
        meta: SyncMeta::default(),
    };
    let env = Envelope::plain(&serde_json::to_vec(&payload).unwrap());
    let push: PushResp = client
        .post(format!("{base}/v1/vaults/{}/push", vault.id))
        .bearer_auth(&auth.access_token)
        .json(&PushReq {
            device_id: device_id.into(),
            ops: vec![PushOp {
                entity_id: "doc-1".into(),
                kind: EntityKind::Document,
                expected_version: 0,
                deleted: false,
                envelope: Some(env),
                blob_hashes: vec![blob_hash.clone()],
            }],
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(push.results.first(), Some(PushItemResult::Accepted { .. })));
    (auth, vault, blob_hash)
}

async fn wait_until<F, Fut>(timeout: Duration, mut pred: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if pred().await {
            return;
        }
        assert!(Instant::now() < deadline, "condition not met within {timeout:?}");
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

#[tokio::test]
async fn portal_admin_auth_and_ops_lifecycle() {
    let node = TestNode::start("Main-Ops", None, None).await;
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();

    // Root redirects to /portal and /portal serves embedded HTML with CSP header.
    let root = client.get(format!("{}/", node.base)).send().await.unwrap();
    assert_eq!(root.status(), StatusCode::TEMPORARY_REDIRECT);
    let portal = client.get(format!("{}/portal", node.base)).send().await.unwrap();
    assert_eq!(portal.status(), StatusCode::OK);
    assert!(portal.headers().get("content-security-policy").is_some());
    let html = portal.text().await.unwrap();
    assert!(html.contains("Quiet.md Sync Portal"));
    assert!(!html.contains("onclick="));

    // Uninitialized status hides internal node_id/role until authenticated.
    let st: Value = client.get(format!("{}/api/admin/auth/status", node.base)).send().await.unwrap().json().await.unwrap();
    assert_eq!(st["initialized"], false);
    assert_eq!(st["authenticated"], false);
    assert!(st.get("node_id").is_none());
    assert!(st.get("role").is_none());

    // First-time setup succeeds and sets HttpOnly Path=/api/admin cookie (+ Secure under X-Forwarded-Proto: https).
    let setup_resp = client
        .post(format!("{}/api/admin/auth/setup", node.base))
        .header("x-forwarded-proto", "https")
        .json(&json!({
            "password": "AdminPassword123!",
            "node_name": "Primary-Node",
            "portal_url": "https://primary.example.com/"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(setup_resp.status(), StatusCode::CREATED);
    let set_cookie = setup_resp.headers().get("set-cookie").unwrap().to_str().unwrap().to_string();
    assert!(set_cookie.contains("Path=/api/admin"));
    assert!(set_cookie.contains("Secure"));
    let cookie_pair = set_cookie.split(';').next().unwrap().to_string();
    let setup_body: Value = setup_resp.json().await.unwrap();
    let token1 = setup_body["token"].as_str().unwrap().to_string();

    // Second setup attempt must fail with 409 Conflict.
    let setup2 = client
        .post(format!("{}/api/admin/auth/setup", node.base))
        .json(&json!({ "password": "AnotherPassword123!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(setup2.status(), StatusCode::CONFLICT);

    // Cookie-authenticated mutation without CSRF header X-Quiet-Portal: 1 is rejected.
    let csrf_fail = client
        .post(format!("{}/api/admin/gc", node.base))
        .header("cookie", &cookie_pair)
        .send()
        .await
        .unwrap();
    assert_eq!(csrf_fail.status(), StatusCode::FORBIDDEN);

    // With X-Quiet-Portal: 1 it succeeds.
    let csrf_ok = client
        .post(format!("{}/api/admin/gc", node.base))
        .header("cookie", &cookie_pair)
        .header("x-quiet-portal", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(csrf_ok.status(), StatusCode::OK);

    // Wrong password rejected; changing password invalidates old token.
    let bad_login = client
        .post(format!("{}/api/admin/auth/login", node.base))
        .json(&json!({ "password": "WrongPassword" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_login.status(), StatusCode::UNAUTHORIZED);

    let chg: Value = client
        .post(format!("{}/api/admin/auth/change-password", node.base))
        .bearer_auth(&token1)
        .json(&json!({
            "current_password": "AdminPassword123!",
            "new_password": "NewAdminPassword456!"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let token2 = chg["token"].as_str().unwrap().to_string();

    let old_tok_resp = client
        .get(format!("{}/api/admin/overview", node.base))
        .bearer_auth(&token1)
        .send()
        .await
        .unwrap();
    assert_eq!(old_tok_resp.status(), StatusCode::UNAUTHORIZED);

    // Create two users with vaults, blobs, entities, and connect user1's WebSocket.
    let (u1, v1, h1) = register_user_with_doc_and_blob(&client, &node.base, "alice@example.com", "dev-a1", b"alice-unique-blob-payload").await;
    let (_u2, _v2, h2) = register_user_with_doc_and_blob(&client, &node.base, "bob@example.com", "dev-b1", b"bob-unique-blob-payload-larger-size").await;

    let t1: WsTicketResp = client
        .post(format!("{}/v1/auth/ws-ticket", node.base))
        .bearer_auth(&u1.access_token)
        .json(&json!({ "vault_id": v1.id }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ws_base = node.base.replacen("http://", "ws://", 1);
    let (mut ws1, _) = connect_async(format!("{ws_base}/v1/vaults/{}/ws?ticket={}", v1.id, t1.ticket)).await.unwrap();
    let _welcome = ws1.next().await.unwrap().unwrap();

    // Overview, Users, User detail, and Storage APIs reflect live data accurately.
    let ov: Value = client
        .get(format!("{}/api/admin/overview", node.base))
        .bearer_auth(&token2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(ov["node"]["node_name"], "Primary-Node");
    assert_eq!(ov["node"]["portal_url"], "https://primary.example.com");
    assert_eq!(ov["stats"]["user_count"], 2);
    assert_eq!(ov["stats"]["vault_count"], 2);
    assert_eq!(ov["stats"]["online_connections"], 1);
    assert_eq!(ov["stats"]["unique_blob_count"], 2);

    let users: Value = client
        .get(format!("{}/api/admin/users?sort=storage_desc", node.base))
        .bearer_auth(&token2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(users["total"], 2);
    assert_eq!(users["users"][0]["email"], "bob@example.com");

    let detail: Value = client
        .get(format!("{}/api/admin/users/{}", node.base, u1.user.id))
        .bearer_auth(&token2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(detail["vaults"].as_array().unwrap().len(), 1);
    assert_eq!(detail["devices"][0]["online"], true);

    let storage: Value = client
        .get(format!("{}/api/admin/storage", node.base))
        .bearer_auth(&token2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(storage["unique_blob_count"], 2);
    assert_eq!(storage["top_users"].as_array().unwrap().len(), 2);

    // Runtime toggle: disable registration, then enable with invite code.
    client
        .put(format!("{}/api/admin/node", node.base))
        .bearer_auth(&token2)
        .json(&json!({ "allow_register": false }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let blocked_reg = client
        .post(format!("{}/v1/auth/register", node.base))
        .json(&RegisterReq {
            email: "c@example.com".into(),
            password: "password123".into(),
            display_name: "C".into(),
            device_id: "dev-c".into(),
            device_name: "PC".into(),
            platform: "linux".into(),
            invite_code: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(blocked_reg.status(), StatusCode::FORBIDDEN);

    // Delete Alice: cascades DB, closes active WS, and immediately reclaims Alice's orphan blob!
    let del: Value = client
        .delete(format!("{}/api/admin/users/{}", node.base, u1.user.id))
        .bearer_auth(&token2)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(del["reclaimed_blobs"], 1);
    assert!(!node.state.blobs.exists(&h1).await.unwrap());
    assert!(node.state.blobs.exists(&h2).await.unwrap());

    // Alice's WS connection should be closed promptly.
    let ws_end = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(m)) = ws1.next().await {
            if matches!(m, tokio_tungstenite::tungstenite::Message::Close(_)) {
                break;
            }
        }
    })
    .await;
    assert!(ws_end.is_ok(), "deleted user's websocket should be disconnected");

    node.cleanup();
}

#[tokio::test]
async fn primary_backup_snapshot_binlog_eviction_and_promote() {
    let primary = TestNode::start("Primary-Cloud", Some("PrimarySecret99!"), Some("https://primary.quiet.local")).await;
    let backup = TestNode::start("Backup-NAS", Some("BackupSecret88!"), Some("http://192.168.10.20:8787")).await;
    let client = reqwest::Client::new();

    let p_tok = admin_login(&client, &primary.base, "PrimarySecret99!").await;
    let b_tok = admin_login(&client, &backup.base, "BackupSecret88!").await;

    // Seed pre-existing data on Primary BEFORE Backup joins (tests WAL snapshot bootstrap).
    let (alice, alice_vault, alice_blob) =
        register_user_with_doc_and_blob(&client, &primary.base, "alice@example.com", "dev-a", b"pre-existing-alice-blob").await;

    // Seed an old unrelated user on Backup to confirm snapshot cleanly replaces business tables while keeping Backup's admin password.
    let _ = register_user_with_doc_and_blob(&client, &backup.base, "stale@local.test", "dev-stale", b"stale-local-blob").await;

    // Wrong Primary password is rejected.
    let bad_cfg = client
        .post(format!("{}/api/admin/replication/configure-backup", backup.base))
        .bearer_auth(&b_tok)
        .json(&json!({
            "primary_url": primary.base,
            "primary_admin_password": "WrongPrimaryPassword!",
            "node_name": "Backup-NAS",
            "portal_url": "http://192.168.10.20:8787"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_cfg.status(), StatusCode::UNAUTHORIZED);

    // Self-replication is rejected.
    let self_cfg = client
        .post(format!("{}/api/admin/replication/configure-backup", primary.base))
        .bearer_auth(&p_tok)
        .json(&json!({
            "primary_url": primary.base,
            "primary_admin_password": "PrimarySecret99!"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(self_cfg.status(), StatusCode::BAD_REQUEST);

    // Pair Backup to Primary using Primary's URL + Admin Password.
    let ok_cfg = client
        .post(format!("{}/api/admin/replication/configure-backup", backup.base))
        .bearer_auth(&b_tok)
        .json(&json!({
            "primary_url": primary.base,
            "primary_admin_password": "PrimarySecret99!",
            "node_name": "Backup-NAS",
            "portal_url": "http://192.168.10.20:8787"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_cfg.status(), StatusCode::OK);

    // Wait for full snapshot bootstrap + blob sync + live WS streaming.
    wait_until(Duration::from_secs(10), || async {
        let r: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["node"]["role"] == "backup"
            && r["upstream"]["bootstrapped"] == true
            && r["upstream"]["state"] == "streaming"
            && r["upstream"]["blob_pending"] == 0
    })
    .await;

    // Backup's own admin password must remain untouched (independent of Primary).
    let _b_tok_relogin = admin_login(&client, &backup.base, "BackupSecret88!").await;

    // Verify Alice's pre-existing data & blob arrived on Backup, and stale local user was replaced.
    assert!(backup.state.blobs.exists(&alice_blob).await.unwrap());
    let b_users: Value = client
        .get(format!("{}/api/admin/users", backup.base))
        .bearer_auth(&b_tok)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(b_users["total"], 1);
    assert_eq!(b_users["users"][0]["email"], "alice@example.com");

    // Verify mutual Portal visibility & jump URLs on both sides.
    wait_until(Duration::from_secs(5), || async {
        let pr: Value = client
            .get(format!("{}/api/admin/replication", primary.base))
            .bearer_auth(&p_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let reps = pr["replicas"].as_array().unwrap();
        reps.len() == 1
            && reps[0]["online"] == true
            && reps[0]["portal_url"] == "http://192.168.10.20:8787"
            && reps[0]["blob_pending"] == 0
    })
    .await;

    let br: Value = client
        .get(format!("{}/api/admin/replication", backup.base))
        .bearer_auth(&b_tok)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(br["upstream"]["primary_portal_url"], "https://primary.quiet.local");
    assert_eq!(br["upstream"]["primary_name"], "Primary-Cloud");

    // Verify Backup read-only guard rejects client register & admin delete.
    let ro_reg = client
        .post(format!("{}/v1/auth/register", backup.base))
        .json(&RegisterReq {
            email: "ro@example.com".into(),
            password: "password123".into(),
            display_name: "RO".into(),
            device_id: "dev-ro".into(),
            device_name: "PC".into(),
            platform: "linux".into(),
            invite_code: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(ro_reg.status(), StatusCode::SERVICE_UNAVAILABLE);

    let ro_del = client
        .delete(format!("{}/api/admin/users/{}", backup.base, alice.user.id))
        .bearer_auth(&b_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(ro_del.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Test real-time incremental Binlog streaming: add Bob + blob, then delete Alice on Primary.
    let (_bob, bob_vault, bob_blob) =
        register_user_with_doc_and_blob(&client, &primary.base, "bob@example.com", "dev-b", b"incremental-bob-blob-content").await;
    client
        .delete(format!("{}/api/admin/users/{}", primary.base, alice.user.id))
        .bearer_auth(&p_tok)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    wait_until(Duration::from_secs(8), || async {
        let bu: Value = client
            .get(format!("{}/api/admin/users", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let br: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        bu["total"] == 1
            && bu["users"][0]["email"] == "bob@example.com"
            && br["upstream"]["blob_pending"] == 0
            && backup.state.blobs.exists(&bob_blob).await.unwrap_or(false)
    })
    .await;

    // Test real-time Portal URL & node name sync in both directions.
    client
        .put(format!("{}/api/admin/node", primary.base))
        .bearer_auth(&p_tok)
        .json(&json!({ "node_name": "Primary-Renamed", "portal_url": "https://p2.quiet.local" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    client
        .put(format!("{}/api/admin/node", backup.base))
        .bearer_auth(&b_tok)
        .json(&json!({ "node_name": "Backup-Renamed", "portal_url": "http://10.8.0.5:8787" }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    wait_until(Duration::from_secs(5), || async {
        let br: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pr: Value = client
            .get(format!("{}/api/admin/replication", primary.base))
            .bearer_auth(&p_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        br["upstream"]["primary_name"] == "Primary-Renamed"
            && br["upstream"]["primary_portal_url"] == "https://p2.quiet.local"
            && pr["replicas"][0]["name"] == "Backup-Renamed"
            && pr["replicas"][0]["portal_url"] == "http://10.8.0.5:8787"
    })
    .await;

    // Test binlog compaction watermark `resync_required` check + manual resync recovery.
    primary
        .state
        .db
        .run(|conn| db_repl::compact_binlog(conn, db::unix_secs() + 10, 0))
        .await
        .unwrap();
    let resync_check = primary
        .state
        .db
        .run(|conn| db_repl::read_binlog_since(conn, 0, 50))
        .await
        .unwrap();
    assert!(resync_check.4, "read_binlog_since(0) should require resync after compaction");

    // Evict Backup from Primary: Backup immediately receives Evicted frame and stops reconnecting.
    let backup_id = backup.state.db.run(|conn| db::get_node_config(conn).map(|c| c.node_id)).await.unwrap();
    let evict = client
        .delete(format!("{}/api/admin/replication/replicas/{backup_id}", primary.base))
        .bearer_auth(&p_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(evict.status(), StatusCode::OK);

    wait_until(Duration::from_secs(5), || async {
        let br: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        br["upstream"]["state"] == "evicted"
    })
    .await;

    // Primary's active replicas list is now empty.
    let pr_after: Value = client
        .get(format!("{}/api/admin/replication", primary.base))
        .bearer_auth(&p_tok)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pr_after["replicas"].as_array().unwrap().len(), 0);

    // Promote Backup to Primary for disaster recovery: preserves Bob's data & becomes writable!
    let promote = client
        .post(format!("{}/api/admin/replication/promote", backup.base))
        .bearer_auth(&b_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(promote.status(), StatusCode::OK);

    // Bob can now log in directly to the promoted server, read snapshot, download blob, and push new doc!
    let bob_login: AuthResp = client
        .post(format!("{}/v1/auth/login", backup.base))
        .json(&LoginReq {
            email: "bob@example.com".into(),
            password: "password123".into(),
            device_id: "dev-b-dr".into(),
            device_name: "DR-Laptop".into(),
            platform: "macos".into(),
        })
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let snap: SnapshotResp = client
        .get(format!("{}/v1/vaults/{}/snapshot", backup.base, bob_vault.id))
        .bearer_auth(&bob_login.access_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snap.items.len(), 1);
    assert_eq!(snap.items[0].entity_id, "doc-1");

    let blob_resp = client
        .get(format!("{}/v1/vaults/{}/blobs/{bob_blob}", backup.base, bob_vault.id))
        .bearer_auth(&bob_login.access_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(blob_resp.as_ref(), b"incremental-bob-blob-content");

    // Alice's vault was deleted before promotion so it must not exist on promoted server.
    let alice_gone = client
        .get(format!("{}/v1/vaults/{}/snapshot", backup.base, alice_vault.id))
        .bearer_auth(&bob_login.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(alice_gone.status(), StatusCode::NOT_FOUND);

    backup.cleanup();
    primary.cleanup();
}

#[tokio::test]
async fn offline_eviction_repair_and_automatic_resync() {
    let primary = TestNode::start("Primary-Resync", Some("PrimaryPass123!"), Some("https://p.local")).await;
    let backup = TestNode::start("Backup-Resync", Some("BackupPass123!"), Some("http://192.168.1.99:8787")).await;
    let client = reqwest::Client::new();

    let p_tok = admin_login(&client, &primary.base, "PrimaryPass123!").await;
    let b_tok = admin_login(&client, &backup.base, "BackupPass123!").await;

    // Pair Backup -> Primary.
    client
        .post(format!("{}/api/admin/replication/configure-backup", backup.base))
        .bearer_auth(&b_tok)
        .json(&json!({
            "primary_url": primary.base,
            "primary_admin_password": "PrimaryPass123!",
            "node_name": "Backup-Resync",
            "portal_url": "http://192.168.1.99:8787"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    wait_until(Duration::from_secs(8), || async {
        let r: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["upstream"]["state"] == "streaming" && r["upstream"]["bootstrapped"] == true
    })
    .await;

    // Trigger explicit resync AFTER primary empties repl_binlog completely (Critical #2 regression test).
    let (_u, _v, h) =
        register_user_with_doc_and_blob(&client, &primary.base, "resync@example.com", "dev-r1", b"resync-blob-data").await;
    primary
        .state
        .db
        .run(|conn| db_repl::compact_binlog(conn, db::unix_secs() + 10, 0))
        .await
        .unwrap();
    let (p_head, p_min) = primary
        .state
        .db
        .run(|conn| Ok((db::get_head_lsn(conn)?, db::get_node_config(conn)?.min_retained_lsn)))
        .await
        .unwrap();
    assert!(p_min > 0);
    assert_eq!(p_head, p_min, "head_lsn must never drop below min_retained_lsn when repl_binlog is empty");

    client
        .post(format!("{}/api/admin/replication/resync", backup.base))
        .bearer_auth(&b_tok)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // Verify Backup converges to applied_lsn == p_min AND stays stably streaming online on Primary (no resync loop!).
    wait_until(Duration::from_secs(8), || async {
        let bu: Value = client
            .get(format!("{}/api/admin/users", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let br: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pr: Value = client
            .get(format!("{}/api/admin/replication", primary.base))
            .bearer_auth(&p_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        bu["total"] == 1
            && bu["users"][0]["email"] == "resync@example.com"
            && backup.state.blobs.exists(&h).await.unwrap_or(false)
            && br["upstream"]["bootstrapped"] == true
            && br["upstream"]["state"] == "streaming"
            && br["upstream"]["applied_lsn"] == p_min
            && pr["replicas"][0]["online"] == true
    })
    .await;

    // Evict Backup, verify evicted replica token gets 403 replica_evicted on HTTP snapshot/binlog,
    // verify evicted Backup rejects manual resync with 409, then re-pair cleanly!
    let backup_id = backup.state.db.run(|conn| db::get_node_config(conn).map(|c| c.node_id)).await.unwrap();
    let old_token = backup
        .state
        .db
        .run(|conn| db_repl::get_upstream(conn).map(|u| u.unwrap().replica_token))
        .await
        .unwrap();

    client
        .delete(format!("{}/api/admin/replication/replicas/{backup_id}", primary.base))
        .bearer_auth(&p_tok)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let evicted_http = client
        .get(format!("{}/v1/repl/binlog?since_lsn=0", primary.base))
        .header("x-replica-id", &backup_id)
        .bearer_auth(&old_token)
        .send()
        .await
        .unwrap();
    assert_eq!(evicted_http.status(), StatusCode::FORBIDDEN);
    let evicted_body: Value = evicted_http.json().await.unwrap();
    assert_eq!(evicted_body["code"], "replica_evicted");

    wait_until(Duration::from_secs(5), || async {
        let r: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["upstream"]["state"] == "evicted"
    })
    .await;

    let resync_evicted = client
        .post(format!("{}/api/admin/replication/resync", backup.base))
        .bearer_auth(&b_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(resync_evicted.status(), StatusCode::CONFLICT);

    // Re-pair after eviction using Primary's admin password -> Backup resumes streaming!
    client
        .post(format!("{}/api/admin/replication/configure-backup", backup.base))
        .bearer_auth(&b_tok)
        .json(&json!({
            "primary_url": primary.base,
            "primary_admin_password": "PrimaryPass123!",
            "node_name": "Backup-Resync",
            "portal_url": "http://192.168.1.99:8787"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    wait_until(Duration::from_secs(8), || async {
        let r: Value = client
            .get(format!("{}/api/admin/replication", backup.base))
            .bearer_auth(&b_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["upstream"]["state"] == "streaming" && r["upstream"]["bootstrapped"] == true
    })
    .await;

    backup.cleanup();
    primary.cleanup();
}

#[tokio::test]
async fn role_transition_clears_stale_replica_tokens_and_startup_password_rotation_revokes_sessions() {
    let node_a = TestNode::start("Node-A", Some("PassA12345!"), Some("http://a.local")).await;
    let node_b = TestNode::start("Node-B", Some("PassB12345!"), Some("http://b.local")).await;
    let node_c = TestNode::start("Node-C", Some("PassC12345!"), Some("http://c.local")).await;
    let client = reqwest::Client::new();

    let a_tok = admin_login(&client, &node_a.base, "PassA12345!").await;
    let c_tok = admin_login(&client, &node_c.base, "PassC12345!").await;

    // 1. Strict URL validation rejects userinfo / query / fragment.
    let bad_url = client
        .put(format!("{}/api/admin/node", node_a.base))
        .bearer_auth(&a_tok)
        .json(&json!({ "portal_url": "https://user:pass@evil.example.com?a=1#frag" }))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_url.status(), StatusCode::BAD_REQUEST);

    // 2. Pair C -> A, capture C's replica token on A.
    client
        .post(format!("{}/api/admin/replication/configure-backup", node_c.base))
        .bearer_auth(&c_tok)
        .json(&json!({
            "primary_url": node_a.base,
            "primary_admin_password": "PassA12345!"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let c_id = node_c.state.db.run(|conn| db::get_node_config(conn).map(|c| c.node_id)).await.unwrap();
    let c_old_token = node_c
        .state
        .db
        .run(|conn| db_repl::get_upstream(conn).map(|u| u.unwrap().replica_token))
        .await
        .unwrap();

    // 3. Repurpose A as Backup of B, then promote A back to Primary -> C's old token must be invalidated!
    client
        .post(format!("{}/api/admin/replication/configure-backup", node_a.base))
        .bearer_auth(&a_tok)
        .json(&json!({
            "primary_url": node_b.base,
            "primary_admin_password": "PassB12345!"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // Verify DB-level read-only fence blocks direct binlog write while A is in backup mode.
    let fence_res = node_a
        .state
        .db
        .run(|conn| db::create_user(conn, "u-fence", "fence@example.com", "Fence", "hash"))
        .await;
    assert!(matches!(fence_res, Err(db::DbError::ReadOnly(_))));

    client
        .post(format!("{}/api/admin/replication/promote", node_a.base))
        .bearer_auth(&a_tok)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    let stale_c = client
        .get(format!("{}/v1/repl/binlog?since_lsn=0", node_a.base))
        .header("x-replica-id", &c_id)
        .bearer_auth(&c_old_token)
        .send()
        .await
        .unwrap();
    assert_eq!(stale_c.status(), StatusCode::UNAUTHORIZED);

    // Because A returned 401 unknown_replica, C automatically stops reconnecting and enters evicted state!
    wait_until(Duration::from_secs(6), || async {
        let r: Value = client
            .get(format!("{}/api/admin/replication", node_c.base))
            .bearer_auth(&c_tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        r["upstream"]["state"] == "evicted"
    })
    .await;

    // 4. Startup QUIET_SYNC_ADMIN_PASSWORD rotation invalidates previously issued admin sessions!
    let dir_a = node_a.dir.clone();
    node_a.state.repl_supervisor.shutdown();
    node_a.handle.abort();
    let mut cfg_rot = ServerConfig::for_test(dir_a.clone());
    cfg_rot.admin_password = Some("RotatedPass999!".into());
    let state_rot = create_state(cfg_rot).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr_rot = listener.local_addr().unwrap();
    let h_rot = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state_rot)).await;
    });
    let base_rot = format!("http://{addr_rot}");

    let old_sess = client
        .get(format!("{base_rot}/api/admin/overview"))
        .bearer_auth(&a_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(old_sess.status(), StatusCode::UNAUTHORIZED);
    let new_tok = admin_login(&client, &base_rot, "RotatedPass999!").await;
    let new_sess = client
        .get(format!("{base_rot}/api/admin/overview"))
        .bearer_auth(&new_tok)
        .send()
        .await
        .unwrap();
    assert_eq!(new_sess.status(), StatusCode::OK);

    h_rot.abort();
    let _ = std::fs::remove_dir_all(&dir_a);
    node_b.cleanup();
    node_c.cleanup();
}
