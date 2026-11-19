use futures_util::{SinkExt, StreamExt};
use quiet_sync_proto::api::*;
use quiet_sync_proto::crypto::{E2eeSetup, Envelope, KdfParams, VaultKeys};
use quiet_sync_proto::entity::{sha256_hex, EntityKind, EntityPayload, SyncMeta};
use quiet_sync_server::config::ServerConfig;
use quiet_sync_server::{create_state, db, gc, router};
use reqwest::StatusCode;
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

async fn start_server(allow_register: bool, invite_code: Option<&str>) -> (String, PathBuf, tokio::task::JoinHandle<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("quiet-sync-srv-test-{}", uuid::Uuid::new_v4()));
    let mut cfg = ServerConfig::for_test(dir.clone());
    cfg.allow_register = allow_register;
    cfg.invite_code = invite_code.map(str::to_string);
    let state = create_state(cfg).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    (format!("http://{addr}"), dir, handle)
}

#[tokio::test]
async fn auth_vaults_occ_blobs_and_ws_realtime() {
    let (base, dir, handle) = start_server(true, Some("SECRET42")).await;
    let client = reqwest::Client::new();

    // Invite code enforced on register.
    let bad_reg = client
        .post(format!("{base}/v1/auth/register"))
        .json(&RegisterReq {
            email: "a@example.com".into(),
            password: "password123".into(),
            display_name: "Alice".into(),
            device_id: "dev-1".into(),
            device_name: "MacBook".into(),
            platform: "macos".into(),
            invite_code: Some("WRONG".into()),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(bad_reg.status(), StatusCode::FORBIDDEN);

    let reg1: AuthResp = client
        .post(format!("{base}/v1/auth/register"))
        .json(&RegisterReq {
            email: "a@example.com".into(),
            password: "password123".into(),
            display_name: "Alice".into(),
            device_id: "dev-1".into(),
            device_name: "MacBook".into(),
            platform: "macos".into(),
            invite_code: Some("SECRET42".into()),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Duplicate email rejected.
    let dup = client
        .post(format!("{base}/v1/auth/register"))
        .json(&RegisterReq {
            email: "A@example.com".into(),
            password: "password123".into(),
            display_name: "Alice2".into(),
            device_id: "dev-9".into(),
            device_name: "Other".into(),
            platform: "linux".into(),
            invite_code: Some("SECRET42".into()),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(dup.status(), StatusCode::CONFLICT);

    // Second device login & refresh rotation.
    let login2: AuthResp = client
        .post(format!("{base}/v1/auth/login"))
        .json(&LoginReq {
            email: "a@example.com".into(),
            password: "password123".into(),
            device_id: "dev-2".into(),
            device_name: "Desktop".into(),
            platform: "windows".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let refreshed: AuthResp = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&RefreshReq { refresh_token: login2.refresh_token.clone(), device_id: "dev-2".into() })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(refreshed.refresh_token, login2.refresh_token);

    // Old refresh token is invalidated after rotation.
    let reuse = client
        .post(format!("{base}/v1/auth/refresh"))
        .json(&RefreshReq { refresh_token: login2.refresh_token.clone(), device_id: "dev-2".into() })
        .send()
        .await
        .unwrap();
    assert_eq!(reuse.status(), StatusCode::UNAUTHORIZED);

    // Create vault with E2EE setup metadata.
    let keys = VaultKeys::generate();
    let e2ee: E2eeSetup = keys
        .wrap_with_params("sync-pass", KdfParams { alg: "argon2id".into(), mem_kib: 4096, time_cost: 1, parallelism: 1, salt_b64: "MTIzNDU2Nzg5MDEyMzQ1Ng==".into() })
        .unwrap();
    let vault: VaultSummary = client
        .post(format!("{base}/v1/vaults"))
        .bearer_auth(&reg1.access_token)
        .json(&CreateVaultReq { name: "Main".into(), e2ee: Some(e2ee) })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Another user cannot read or push into Alice's vault.
    let bob: AuthResp = client
        .post(format!("{base}/v1/auth/register"))
        .json(&RegisterReq {
            email: "bob@example.com".into(),
            password: "password123".into(),
            display_name: "Bob".into(),
            device_id: "bob-1".into(),
            device_name: "BobPC".into(),
            platform: "linux".into(),
            invite_code: Some("SECRET42".into()),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let cross = client
        .get(format!("{base}/v1/vaults/{}/snapshot", vault.id))
        .bearer_auth(&bob.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(cross.status(), StatusCode::FORBIDDEN);

    // Connect two WS clients for dev-1 and dev-2 via single-use ticket.
    let t1: WsTicketResp = client
        .post(format!("{base}/v1/auth/ws-ticket"))
        .bearer_auth(&reg1.access_token)
        .json(&serde_json::json!({"vault_id": vault.id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let t2: WsTicketResp = client
        .post(format!("{base}/v1/auth/ws-ticket"))
        .bearer_auth(&refreshed.access_token)
        .json(&serde_json::json!({"vault_id": vault.id}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let ws_base = base.replacen("http://", "ws://", 1);
    let (mut ws1, _) = connect_async(format!("{ws_base}/v1/vaults/{}/ws?ticket={}", vault.id, t1.ticket)).await.unwrap();
    let (mut ws2, _) = connect_async(format!("{ws_base}/v1/vaults/{}/ws?ticket={}", vault.id, t2.ticket)).await.unwrap();

    // Wait for Welcome on ws2.
    let msg = ws2.next().await.unwrap().unwrap().into_text().unwrap();
    let frame: ServerFrame = serde_json::from_str(&msg).unwrap();
    assert!(matches!(frame, ServerFrame::Welcome { head_seq: 0, .. }));

    // Upload blob (missing blob check first).
    let blob_data = b"encrypted-image-payload".to_vec();
    let blob_hash = sha256_hex(&blob_data);
    let payload = EntityPayload::Asset {
        rel_path: ".assets/demo.png".into(),
        blob_hash: blob_hash.clone(),
        raw_sha256: blob_hash.clone(),
        size: blob_data.len() as u64,
        mime: "image/png".into(),
    };
    let env = keys.encrypt_entity(&vault.id, ".assets/demo.png", EntityKind::Asset, 1, &payload.encode_bytes().unwrap()).unwrap();

    let missing_push: PushResp = client
        .post(format!("{base}/v1/vaults/{}/push", vault.id))
        .bearer_auth(&reg1.access_token)
        .json(&PushReq {
            device_id: "dev-1".into(),
            ops: vec![PushOp {
                entity_id: ".assets/demo.png".into(),
                kind: EntityKind::Asset,
                expected_version: 0,
                deleted: false,
                envelope: Some(env.clone()),
                blob_hashes: vec![blob_hash.clone()],
            }],
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(missing_push.results[0], PushItemResult::MissingBlobs { .. }));

    // Upload blob then push via WS.
    let put_status = client
        .put(format!("{base}/v1/vaults/{}/blobs/{blob_hash}", vault.id))
        .bearer_auth(&reg1.access_token)
        .body(blob_data.clone())
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(put_status, StatusCode::CREATED);

    let got_blob = client
        .get(format!("{base}/v1/vaults/{}/blobs/{blob_hash}", vault.id))
        .bearer_auth(&refreshed.access_token)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(got_blob.as_ref(), blob_data.as_slice());

    let push_frame = serde_json::to_string(&ClientFrame::Push {
        req_id: "req-1".into(),
        ops: vec![PushOp {
            entity_id: ".assets/demo.png".into(),
            kind: EntityKind::Asset,
            expected_version: 0,
            deleted: false,
            envelope: Some(env.clone()),
            blob_hashes: vec![blob_hash],
        }],
    })
    .unwrap();
    ws1.send(Message::Text(push_frame.into())).await.unwrap();

    // ws2 receives Broadcast.
    let mut saw_broadcast = false;
    for _ in 0..5 {
        let txt = ws2.next().await.unwrap().unwrap().into_text().unwrap();
        let srv: ServerFrame = serde_json::from_str(&txt).unwrap();
        if let ServerFrame::Broadcast { item } = srv {
            assert_eq!(item.entity_id, ".assets/demo.png");
            assert_eq!(item.version, 1);
            saw_broadcast = true;
            break;
        }
    }
    assert!(saw_broadcast);

    // Stale OCC push from dev-2 (expected_version = 0 when server is 1) returns Conflict.
    let conflict_resp: PushResp = client
        .post(format!("{base}/v1/vaults/{}/push", vault.id))
        .bearer_auth(&refreshed.access_token)
        .json(&PushReq {
            device_id: "dev-2".into(),
            ops: vec![PushOp {
                entity_id: ".assets/demo.png".into(),
                kind: EntityKind::Asset,
                expected_version: 0,
                deleted: false,
                envelope: Some(env.clone()),
                blob_hashes: vec![],
            }],
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(conflict_resp.results[0], PushItemResult::Conflict { .. }));

    // E2EE vault rejects plaintext envelopes and unencrypted deletes with 400 Bad Request.
    let plain_env = Envelope::plain(&EntityPayload::Folder { name: "F".into(), parent_id: None, meta: SyncMeta::default() }.encode_bytes().unwrap());
    let plain_reject = client
        .post(format!("{base}/v1/vaults/{}/push", vault.id))
        .bearer_auth(&reg1.access_token)
        .json(&PushReq {
            device_id: "dev-1".into(),
            ops: vec![PushOp {
                entity_id: "f-plain".into(),
                kind: EntityKind::Folder,
                expected_version: 0,
                deleted: false,
                envelope: Some(plain_env),
                blob_hashes: vec![],
            }],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(plain_reject.status(), StatusCode::BAD_REQUEST);

    let unauth_del = client
        .post(format!("{base}/v1/vaults/{}/push", vault.id))
        .bearer_auth(&reg1.access_token)
        .json(&PushReq {
            device_id: "dev-1".into(),
            ops: vec![PushOp {
                entity_id: ".assets/demo.png".into(),
                kind: EntityKind::Asset,
                expected_version: 1,
                deleted: true,
                envelope: None,
                blob_hashes: vec![],
            }],
        })
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_del.status(), StatusCode::BAD_REQUEST);

    // Revoking dev-2 terminates its already-open WS and rejects further Pull/Push or API calls.
    let rev = client
        .delete(format!("{base}/v1/devices/dev-2"))
        .bearer_auth(&reg1.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(rev.status(), StatusCode::NO_CONTENT);

    // Attempt Pull over already-open ws2: server must close ws2 cleanly without returning Changes.
    let _ = ws2
        .send(Message::Text(serde_json::to_string(&ClientFrame::Pull { req_id: "p-after-rev".into(), since_seq: 0, limit: 10 }).unwrap().into()))
        .await;
    let ws2_after = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(Ok(msg)) = ws2.next().await {
            if let Message::Text(ref t) = msg
                && let Ok(ServerFrame::Changes { .. }) = serde_json::from_str::<ServerFrame>(t)
            {
                return true;
            }
            if matches!(msg, Message::Close(_)) {
                return false;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    assert!(!ws2_after, "revoked WS must not serve Changes");

    let after_revoke = client
        .get(format!("{base}/v1/vaults"))
        .bearer_auth(&refreshed.access_token)
        .send()
        .await
        .unwrap();
    assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);

    // Logout also immediately disconnects open WS connections (ws1).
    let logout_resp = client
        .post(format!("{base}/v1/auth/logout"))
        .json(&LogoutReq { refresh_token: reg1.refresh_token.clone() })
        .send()
        .await
        .unwrap();
    assert_eq!(logout_resp.status(), StatusCode::NO_CONTENT);

    let ws1_closed = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match ws1.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                Some(Ok(Message::Text(_))) => continue,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(ws1_closed, "logout must terminate active WS session");

    handle.abort();
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn compaction_watermark_stale_base_and_blob_gc_work_together() {
    let dir = std::env::temp_dir().join(format!("quiet-sync-gc-test-{}", uuid::Uuid::new_v4()));
    let state = create_state(ServerConfig::for_test(dir.clone())).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn({
        let app = router(state.clone());
        async move { let _ = axum::serve(listener, app).await; }
    });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    let reg: AuthResp = client
        .post(format!("{base}/v1/auth/register"))
        .json(&RegisterReq {
            email: "gc@example.com".into(),
            password: "password123".into(),
            display_name: "GC".into(),
            device_id: "dev-gc".into(),
            device_name: "Mac".into(),
            platform: "macos".into(),
            invite_code: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let vault: VaultSummary = client
        .post(format!("{base}/v1/vaults"))
        .bearer_auth(&reg.access_token)
        .json(&CreateVaultReq { name: "Plain".into(), e2ee: None })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Upload two blobs and attach blob1 then replace with blob2.
    let b1 = b"old-blob-bytes".to_vec();
    let h1 = sha256_hex(&b1);
    let b2 = b"new-blob-bytes".to_vec();
    let h2 = sha256_hex(&b2);
    for (h, bytes) in [(&h1, b1), (&h2, b2)] {
        assert_eq!(
            client.put(format!("{base}/v1/vaults/{}/blobs/{h}", vault.id)).bearer_auth(&reg.access_token).body(bytes).send().await.unwrap().status(),
            StatusCode::CREATED
        );
    }

    let doc_env = Envelope::plain(&EntityPayload::Folder { name: "A".into(), parent_id: None, meta: SyncMeta::default() }.encode_bytes().unwrap());
    client.post(format!("{base}/v1/vaults/{}/push", vault.id)).bearer_auth(&reg.access_token).json(&PushReq {
        device_id: "dev-gc".into(),
        ops: vec![
            PushOp { entity_id: "alive-early".into(), kind: EntityKind::Folder, expected_version: 0, deleted: false, envelope: Some(doc_env.clone()), blob_hashes: vec![] },
            PushOp { entity_id: "ephem".into(), kind: EntityKind::Asset, expected_version: 0, deleted: false, envelope: Some(doc_env.clone()), blob_hashes: vec![h1.clone()] },
            PushOp { entity_id: "ephem".into(), kind: EntityKind::Asset, expected_version: 1, deleted: false, envelope: Some(doc_env.clone()), blob_hashes: vec![h2.clone()] },
            PushOp { entity_id: "ephem".into(), kind: EntityKind::Asset, expected_version: 2, deleted: true, envelope: None, blob_hashes: vec![] },
            PushOp { entity_id: "alive-late".into(), kind: EntityKind::Folder, expected_version: 0, deleted: false, envelope: Some(doc_env), blob_hashes: vec![] },
        ],
    }).send().await.unwrap().error_for_status().unwrap();

    // Backdate seq 2..=4 and h1/h2 vault_blobs rows so GC prunes seq 2..=4 and h1/h2 while keeping seq 1 alive.
    state.db.run({
        let vid = vault.id.clone();
        move |conn| {
            conn.execute("UPDATE op_log SET updated_at = 100 WHERE vault_id = ?1 AND seq IN (2, 3, 4)", [&vid])?;
            conn.execute("UPDATE entities SET updated_at = 100 WHERE vault_id = ?1 AND entity_id = 'ephem'", [&vid])?;
            conn.execute("UPDATE vault_blobs SET created_at = 100 WHERE vault_id = ?1", [&vid])?;
            Ok(())
        }
    }).await.unwrap();

    let deleted_blobs = gc::run_once_with_cutoffs(&state, 500, 500).await.unwrap();
    assert_eq!(deleted_blobs, 2);
    assert!(!state.blobs.exists(&h1).await.unwrap());
    assert!(!state.blobs.exists(&h2).await.unwrap());

    // Client stopped at seq 2 missed pruned tombstones at seq 3..=4 even though seq 1 is still alive!
    let chg: ChangesResp = client
        .get(format!("{base}/v1/vaults/{}/changes?since=2&limit=50", vault.id))
        .bearer_auth(&reg.access_token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(chg.resync_required);

    // Pushing an update with non-zero expected_version for purged entity returns StaleBase.
    let stale_resp: PushResp = client
        .post(format!("{base}/v1/vaults/{}/push", vault.id))
        .bearer_auth(&reg.access_token)
        .json(&PushReq {
            device_id: "dev-gc".into(),
            ops: vec![PushOp {
                entity_id: "ephem".into(),
                kind: EntityKind::Asset,
                expected_version: 1,
                deleted: false,
                envelope: Some(Envelope::plain(b"x")),
                blob_hashes: vec![],
            }],
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(stale_resp.results[0], PushItemResult::StaleBase { .. }));

    // Upload under grace period must NOT be deleted by GC.
    let fresh = b"fresh-upload".to_vec();
    let h_fresh = sha256_hex(&fresh);
    assert_eq!(
        client.put(format!("{base}/v1/vaults/{}/blobs/{h_fresh}", vault.id)).bearer_auth(&reg.access_token).body(fresh).send().await.unwrap().status(),
        StatusCode::CREATED
    );
    gc::run_once_with_cutoffs(&state, 500, db::unix_secs().saturating_sub(3600)).await.unwrap();
    assert!(state.blobs.exists(&h_fresh).await.unwrap());

    handle.abort();
    let _ = std::fs::remove_dir_all(dir);
}
