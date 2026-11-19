use fdl_core::board::new_element::{new_element, ElementOpts};
use fdl_core::board::types::ElementType;
use fdl_core::docxml;
use fdl_core::model::{Block, Document, RichText};
use fdl_core::store::Store;
use fdl_core::whiteboard::Board;
use quiet_sync_client::{
    config_entity_id, history_entity_id, trash_entity_id, FsLibraryAdapter, LocalChangeHint,
    SyncEvent, SyncHandle, SyncScope, SyncStatus, VaultUnlock,
};
use quiet_sync_proto::entity::EntityKind;
use quiet_sync_server::spawn_test_server;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, Semaphore};

static E2E_GATE: Semaphore = Semaphore::const_new(2);

struct TestClient {
    lib_dir: PathBuf,
    store: Arc<Mutex<Store>>,
    handle: SyncHandle,
    events: mpsc::UnboundedReceiver<SyncEvent>,
}

impl TestClient {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("quiet-e2e-{name}-{}", uuid::Uuid::new_v4()));
        let lib_dir = base.join("library");
        let sync_dir = base.join("sync");
        let store = Arc::new(Mutex::new(Store::open_at(lib_dir.clone()).unwrap()));
        let adapter = Arc::new(FsLibraryAdapter::new(store.clone()));
        let (handle, events) = SyncHandle::start_on(&tokio::runtime::Handle::current(), sync_dir, adapter, None).unwrap();
        assert_eq!(handle.sync_interval_secs(), quiet_sync_client::DEFAULT_SYNC_INTERVAL_SECS);
        handle.set_sync_interval_secs(0);
        Self { lib_dir, store, handle, events }
    }

    async fn wait_for_synced(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            if matches!(self.handle.status(), SyncStatus::Synced { .. }) {
                // Drain any pending event briefly.
                if tokio::time::timeout(Duration::from_millis(50), self.events.recv()).await.is_err() {
                    return;
                }
            } else {
                let _ = tokio::time::timeout(Duration::from_millis(100), self.events.recv()).await;
            }
        }
    }

    async fn sync_now(&mut self) {
        self.handle.sync_now().await.unwrap();
    }

    async fn wait_until<F: FnMut(&mut Store) -> bool>(&mut self, mut pred: F) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            {
                let mut s = self.store.lock().unwrap();
                if pred(&mut s) {
                    return;
                }
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for sync condition");
            let _ = tokio::time::timeout(Duration::from_millis(50), self.events.recv()).await;
        }
    }

    async fn cleanup(self) {
        self.handle.shutdown_and_flush_async(Duration::from_millis(500)).await;
        if let Some(base) = self.lib_dir.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

fn doc_with(title: &str, paras: &[&str]) -> Document {
    Document {
        title: RichText::plain(title),
        blocks: paras.iter().map(|p| Block::text(p)).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn realtime_ws_doc_and_board_sync() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("rt-a");
    let mut b = TestClient::new("rt-b");

    a.handle.register(&url, "rt@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "rt@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    // Create XML doc, MD doc, and Excalidraw board on Client A.
    let (xml_id, md_id, bd_id) = {
        let mut s = a.store.lock().unwrap();
        let xid = s.create_from(&doc_with("Realtime", &["Hello WS"])).unwrap();
        let mid = s.create_markdown(&doc_with("Readme", &["# MD Line"]), "---\ntag: sync\n---\n").unwrap();
        let mut bd = Board::default();
        bd.elements.push(new_element(ElementType::Rectangle, ElementOpts { x: 10., y: 20., width: 80., height: 40., ..Default::default() }));
        let bid = s.create_board_in("Sketch", &bd, None).unwrap();
        (xid, mid, bid)
    };
    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: xml_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: md_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: bd_id.clone() },
    ]);

    // Client B should receive all three via WebSocket broadcast automatically!
    b.wait_until(|s| {
        s.load(&xml_id).is_ok_and(|d| d.blocks[0].text.text == "Hello WS")
            && s.load(&md_id).is_ok_and(|d| d.blocks[0].text.text == "MD Line")
            && s.load(&bd_id).is_ok()
    })
    .await;

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn offline_concurrent_non_overlapping_blocks_auto_merge() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("merge-a");
    let mut b = TestClient::new("merge-b");
    a.handle.register(&url, "m@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let doc_id = a.store.lock().unwrap().create_from(&doc_with("Shared", &["P1", "P2", "P3"])).unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();

    b.handle.login(&url, "m@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    b.wait_until(|s| s.load(&doc_id).is_ok()).await;

    // Pause both to simulate offline concurrent edits.
    a.handle.set_paused(true);
    b.handle.set_paused(true);
    tokio::time::sleep(Duration::from_millis(80)).await;

    {
        let mut s = a.store.lock().unwrap();
        let mut d = s.load(&doc_id).unwrap();
        d.blocks[0].text = RichText::plain("P1-edited-by-A");
        s.save(&doc_id, &d).unwrap();
    }
    {
        let mut s = b.store.lock().unwrap();
        let mut d = s.load(&doc_id).unwrap();
        d.blocks[2].text = RichText::plain("P3-edited-by-B");
        d.blocks.push(Block::text("P4-added-by-B"));
        s.save(&doc_id, &d).unwrap();
    }

    a.handle.set_paused(false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    b.handle.set_paused(false);

    let expected = vec!["P1-edited-by-A", "P2", "P3-edited-by-B", "P4-added-by-B"];
    a.wait_until(|s| {
        s.load(&doc_id)
            .is_ok_and(|d| d.blocks.iter().map(|x| x.text.text.as_str()).collect::<Vec<_>>() == expected)
    })
    .await;
    b.wait_until(|s| {
        s.load(&doc_id)
            .is_ok_and(|d| d.blocks.iter().map(|x| x.text.text.as_str()).collect::<Vec<_>>() == expected)
    })
    .await;
    assert_eq!(a.store.lock().unwrap().list().len(), 1);
    assert_eq!(b.store.lock().unwrap().list().len(), 1);

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn offline_concurrent_board_elements_reconcile() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("bd-a");
    let mut b = TestClient::new("bd-b");
    a.handle.register(&url, "bd@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let mut base_bd = Board::default();
    let mut r1 = new_element(ElementType::Rectangle, ElementOpts { id: Some("rect1".into()), x: 0., y: 0., width: 50., height: 50., index: Some("a0".into()), ..Default::default() });
    r1.version = 1;
    let mut r2 = new_element(ElementType::Ellipse, ElementOpts { id: Some("ell2".into()), x: 100., y: 0., width: 50., height: 50., index: Some("a1".into()), ..Default::default() });
    r2.version = 1;
    base_bd.elements = vec![r1, r2];
    let bd_id = a.store.lock().unwrap().create_board_in("Diagram", &base_bd, None).unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();

    b.handle.login(&url, "bd@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    b.wait_until(|s| s.load(&bd_id).is_ok()).await;

    a.handle.set_paused(true);
    b.handle.set_paused(true);
    tokio::time::sleep(Duration::from_millis(80)).await;

    // A modifies rect1 and adds diaA.
    {
        let mut s = a.store.lock().unwrap();
        let mut doc = s.load(&bd_id).unwrap();
        let fdl_core::model::BlockKind::Board(bd) = &mut doc.blocks[0].kind else { panic!() };
        let bdm = Arc::make_mut(bd);
        bdm.elements[0].x = 42.;
        bdm.elements[0].version = 2;
        bdm.elements.push(new_element(ElementType::Diamond, ElementOpts { id: Some("diaA".into()), x: 200., y: 0., width: 40., height: 40., index: Some("a2".into()), ..Default::default() }));
        s.save(&bd_id, &doc).unwrap();
    }
    // B deletes ell2 and adds rectB.
    {
        let mut s = b.store.lock().unwrap();
        let mut doc = s.load(&bd_id).unwrap();
        let fdl_core::model::BlockKind::Board(bd) = &mut doc.blocks[0].kind else { panic!() };
        let bdm = Arc::make_mut(bd);
        bdm.elements.retain(|e| e.id != "ell2");
        bdm.elements.push(new_element(ElementType::Rectangle, ElementOpts { id: Some("rectB".into()), x: 300., y: 0., width: 40., height: 40., index: Some("a3".into()), ..Default::default() }));
        s.save(&bd_id, &doc).unwrap();
    }

    a.handle.set_paused(false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    b.handle.set_paused(false);

    let has_merged = |s: &mut Store| {
        let Ok(d) = s.load(&bd_id) else { return false };
        let Some(bd) = fdl_core::whiteboard::board_of(&d) else { return false };
        let ids: Vec<&str> = bd.elements.iter().map(|e| e.id.as_str()).collect();
        ids.contains(&"rect1") && !ids.contains(&"ell2") && ids.contains(&"diaA") && ids.contains(&"rectB")
            && bd.elements.iter().find(|e| e.id == "rect1").is_some_and(|e| e.x == 42.)
    };
    a.wait_until(has_merged).await;
    b.wait_until(has_merged).await;

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn offline_same_block_conflict_creates_conflict_copy() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("conf-a");
    let mut b = TestClient::new("conf-b");
    a.handle.register(&url, "c@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let doc_id = a.store.lock().unwrap().create_from(&doc_with("SameBlock", &["Line 1"])).unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();

    b.handle.login(&url, "c@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    b.wait_until(|s| s.load(&doc_id).is_ok()).await;

    a.handle.set_paused(true);
    b.handle.set_paused(true);
    tokio::time::sleep(Duration::from_millis(80)).await;

    {
        let mut s = a.store.lock().unwrap();
        let mut d = s.load(&doc_id).unwrap();
        d.blocks[0].text = RichText::plain("From A");
        s.save(&doc_id, &d).unwrap();
    }
    {
        let mut s = b.store.lock().unwrap();
        let mut d = s.load(&doc_id).unwrap();
        d.blocks[0].text = RichText::plain("From B");
        s.save(&doc_id, &d).unwrap();
    }

    a.handle.set_paused(false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    b.handle.set_paused(false);

    // Both clients must end up with 2 docs (primary + conflict copy) preserving both texts.
    let has_both = |s: &mut Store| {
        let list = s.list();
        list.len() == 2
            && list.iter().any(|e| e.body.contains("From A"))
            && list.iter().any(|e| e.body.contains("From B"))
    };
    b.wait_until(has_both).await;
    a.wait_until(has_both).await;

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn folder_rename_concurrent_new_doc_and_links() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("fld-a");
    let mut b = TestClient::new("fld-b");
    a.handle.register(&url, "f@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let folder_id = a.store.lock().unwrap().create_folder("Work", None).unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();

    b.handle.login(&url, "f@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    b.wait_until(|s| s.list().iter().any(|e| e.id == folder_id)).await;

    // Pause B, A renames folder Work -> Projects, B creates doc inside folder_id offline.
    b.handle.set_paused(true);
    tokio::time::sleep(Duration::from_millis(80)).await;

    a.store.lock().unwrap().rename(&folder_id, "Projects").unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Folder, entity_id: folder_id.clone() }]);

    let doc_id = b.store.lock().unwrap().create_from_in(&doc_with("Inside", &["Body"]), Some(&folder_id)).unwrap();
    b.handle.set_paused(false);

    // Both converge with doc inside renamed folder `Projects/Inside.xml`.
    a.wait_until(|s| s.list().iter().any(|e| e.id == doc_id && e.meta.rel == "Projects/Inside.xml")).await;
    b.wait_until(|s| s.list().iter().any(|e| e.id == doc_id && e.meta.rel == "Projects/Inside.xml")).await;

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn full_scope_assets_trash_history_config() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("full-a");
    let mut b = TestClient::new("full-b");
    a.handle.register(&url, "full@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "full@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    // 1. Asset + doc referencing asset.
    let asset_rel = ".assets/sample.png";
    let asset_bytes = b"fake-png-binary-content".to_vec();
    std::fs::create_dir_all(a.lib_dir.join(".assets")).unwrap();
    std::fs::write(a.lib_dir.join(asset_rel), &asset_bytes).unwrap();
    let doc_xml = "<title>WithImg</title>\n<img path=\".assets/sample.png\"/>\n";
    let doc_id = {
        let mut s = a.store.lock().unwrap();
        let d = docxml::parse(doc_xml).unwrap();
        let id = s.create_from(&d).unwrap();
        // Force a history snapshot and label it.
        let mut d2 = d.clone();
        d2.blocks.push(Block::text("v2"));
        s.save(&id, &d2).unwrap();
        s.save_version_now(&id).unwrap();
        id
    };
    let snap_name = a.store.lock().unwrap().versions(&doc_id)[0].name.clone();
    a.store.lock().unwrap().set_version_label(&doc_id, &snap_name, Some("Milestone 1")).unwrap();

    // 2. Config (shortcuts.json + custom theme).
    std::fs::write(a.lib_dir.join(".quiet/shortcuts.json"), r#"{"workspace::NewDoc":"cmd-shift-t"}"#).unwrap();
    std::fs::create_dir_all(a.lib_dir.join(".quiet/themes")).unwrap();
    std::fs::write(a.lib_dir.join(".quiet/themes/ocean.css"), "/* custom theme */\n:root { --theme-name: Ocean; }\n").unwrap();

    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Asset, entity_id: asset_rel.into() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::HistorySnapshot, entity_id: history_entity_id(&doc_id, &snap_name) },
        LocalChangeHint::Upsert { kind: EntityKind::Config, entity_id: config_entity_id("shortcuts") },
        LocalChangeHint::Upsert { kind: EntityKind::Config, entity_id: config_entity_id("theme:ocean.css") },
    ]);

    let b_lib = b.lib_dir.clone();
    b.wait_until(|s| {
        b_lib.join(asset_rel).exists()
            && s.versions(&doc_id).first().is_some_and(|v| v.label.as_deref() == Some("Milestone 1"))
            && b_lib.join(".quiet/themes/ocean.css").exists()
            && std::fs::read_to_string(b_lib.join(".quiet/shortcuts.json")).is_ok_and(|t| t.contains("cmd-shift-t"))
    })
    .await;
    assert_eq!(std::fs::read(b.lib_dir.join(asset_rel)).unwrap(), asset_bytes);

    // 3. Trash item sync.
    a.store.lock().unwrap().delete(&doc_id, None).unwrap();
    a.handle.notify_changes(vec![
        LocalChangeHint::Delete { kind: EntityKind::Document, entity_id: doc_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::TrashItem, entity_id: trash_entity_id(&doc_id) },
    ]);
    b.wait_until(|s| s.list().is_empty() && s.list_trash().iter().any(|t| t.id == doc_id)).await;

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn e2ee_zero_knowledge_verified_on_server() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let a = TestClient::new("zk-a");
    let mut b = TestClient::new("zk-b");
    let mut c = TestClient::new("zk-c");

    a.handle.register(&url, "zk@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, rec_opt) = a.handle.create_vault("ZeroKnowledge", Some("vault-passphrase-99".into())).await.unwrap();
    let recovery_key = rec_opt.expect("E2EE vault returns recovery key");

    let secret_title = "TOP_SECRET_PROJECT_PHOENIX";
    let secret_body = "ULTRA_CONFIDENTIAL_CONTENT_XYZ";
    let secret_img = b"SECRET_IMAGE_MARKER_BYTES_99999".to_vec();

    std::fs::create_dir_all(a.lib_dir.join(".assets/files/abc")).unwrap();
    std::fs::write(a.lib_dir.join(".assets/secret.bin"), &secret_img).unwrap();
    let payroll = b"CONFIDENTIAL_PAYROLL_PDF".to_vec();
    std::fs::write(a.lib_dir.join(".assets/files/abc/secret-payroll.pdf"), &payroll).unwrap();
    let doc_id = a.store.lock().unwrap().create_from(&doc_with(secret_title, &[secret_body])).unwrap();
    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Asset, entity_id: ".assets/secret.bin".into() },
        LocalChangeHint::Upsert { kind: EntityKind::Asset, entity_id: ".assets/files/abc/secret-payroll.pdf".into() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() },
    ]);

    // Client B unlocks via passphrase; Client C unlocks via Recovery Key!
    b.handle.login(&url, "zk@example.com", "secret12", "WinB").await.unwrap();
    b.handle.link_vault(vault.clone(), Some(VaultUnlock::Passphrase("vault-passphrase-99".into()))).await.unwrap();
    let b_lib = b.lib_dir.clone();
    b.wait_until(|s| {
        s.load(&doc_id).is_ok_and(|d| d.title.text == secret_title)
            && b_lib.join(".assets/secret.bin").exists()
            && b_lib.join(".assets/files/abc/secret-payroll.pdf").exists()
    })
    .await;
    assert_eq!(std::fs::read(b.lib_dir.join(".assets/secret.bin")).unwrap(), secret_img);
    assert_eq!(std::fs::read(b.lib_dir.join(".assets/files/abc/secret-payroll.pdf")).unwrap(), payroll);

    c.handle.login(&url, "zk@example.com", "secret12", "LinuxC").await.unwrap();
    c.handle.link_vault(vault, Some(VaultUnlock::RecoveryKey(recovery_key))).await.unwrap();
    c.wait_until(|s| s.load(&doc_id).is_ok_and(|d| d.blocks[0].text.text == secret_body)).await;

    // Directly inspect server's SQLite DB and blob files to prove zero plaintext leakage!
    let conn = Connection::open(srv_dir.join("sync.db")).unwrap();
    for table in ["entities", "op_log"] {
        let mut id_stmt = conn.prepare(&format!("SELECT entity_id, kind FROM {table}")).unwrap();
        let rows = id_stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).unwrap();
        for row in rows {
            let (eid, kind) = row.unwrap();
            assert!(!eid.contains("secret-payroll"), "server {table} leaked asset filename: {eid}");
            assert!(!eid.contains(".assets/"), "server {table} leaked asset path: {eid}");
            if kind == "asset" {
                assert!(eid.starts_with("asset:"), "expected opaque HMAC asset id, got {eid}");
            }
        }
    }
    let mut stmt = conn.prepare("SELECT envelope FROM entities").unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0)).unwrap();
    for env in rows {
        if let Some(bytes) = env.unwrap() {
            let txt = String::from_utf8_lossy(&bytes);
            assert!(!txt.contains(secret_title), "server leaked plaintext title!");
            assert!(!txt.contains(secret_body), "server leaked plaintext body!");
            assert!(!txt.contains("\"mode\":\"plain\""), "server stored plain envelope!");
        }
    }
    fn check_no_plaintext_in_dir(dir: &std::path::Path, needle: &[u8]) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                check_no_plaintext_in_dir(&p, needle);
            } else {
                let data = std::fs::read(&p).unwrap();
                assert!(!data.windows(needle.len()).any(|w| w == needle), "server blob leaked plaintext bytes!");
            }
        }
    }
    check_no_plaintext_in_dir(&srv_dir.join("blobs"), b"SECRET_IMAGE_MARKER");

    a.cleanup().await;
    b.cleanup().await;
    c.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn initial_sync_deduplicates_identical_guides() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("dedup-a");
    let mut b = TestClient::new("dedup-b");

    // Both libraries start with an identical "Getting started" guide under different local UUIDs!
    let guide_doc = doc_with("Getting started", &["Welcome to Quiet.md"]);
    let id_a = a.store.lock().unwrap().create_from(&guide_doc).unwrap();
    let id_b = b.store.lock().unwrap().create_from(&guide_doc).unwrap();
    assert_ne!(id_a, id_b);

    a.handle.register(&url, "dedup@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    a.wait_for_synced().await;

    b.handle.login(&url, "dedup@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    b.wait_for_synced().await;
    a.wait_for_synced().await;

    // Must deduplicate to exactly 1 document sharing id_a!
    let list_a = a.store.lock().unwrap().list();
    let list_b = b.store.lock().unwrap().list();
    assert_eq!(list_a.len(), 1);
    assert_eq!(list_b.len(), 1);
    assert_eq!(list_b[0].id, id_a);

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn relink_same_vault_preserves_offline_edits() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("relink-a");
    let mut b = TestClient::new("relink-b");

    a.handle.register(&url, "relink@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "relink@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault.clone(), None).await.unwrap();

    let (doc1, doc2) = {
        let mut s = a.store.lock().unwrap();
        let d1 = s.create_from(&doc_with("MergeOnRelink", &["Para A base", "Para B base"])).unwrap();
        let d2 = s.create_from(&doc_with("ConflictOnRelink", &["Shared base"])).unwrap();
        (d1, d2)
    };
    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc1.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc2.clone() },
    ]);
    b.wait_until(|s| s.load(&doc1).is_ok() && s.load(&doc2).is_ok()).await;
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    // B unlinks, then both sides edit different paragraphs of doc1 and the same paragraph of doc2.
    b.handle.unlink_vault().await.unwrap();
    {
        let mut sa = a.store.lock().unwrap();
        let mut d1 = sa.load(&doc1).unwrap();
        d1.blocks[0].text = RichText::plain("Para A edited on cloud");
        sa.save_editor(&doc1, &d1).unwrap();
        let mut d2 = sa.load(&doc2).unwrap();
        d2.blocks[0].text = RichText::plain("Cloud concurrent edit");
        sa.save_editor(&doc2, &d2).unwrap();
    }
    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc1.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc2.clone() },
    ]);
    a.sync_now().await;

    {
        let mut sb = b.store.lock().unwrap();
        let mut d1 = sb.load(&doc1).unwrap();
        d1.blocks[1].text = RichText::plain("Para B edited while unlinked");
        sb.save_editor(&doc1, &d1).unwrap();
        let mut d2 = sb.load(&doc2).unwrap();
        d2.blocks[0].text = RichText::plain("B offline unlinked edit");
        sb.save_editor(&doc2, &d2).unwrap();
    }

    // Re-link same vault: doc1 must 3-way merge cleanly; doc2 must preserve B's edit and copy A's.
    b.handle.link_vault(vault, None).await.unwrap();
    b.sync_now().await;
    a.sync_now().await;
    a.wait_until(|s| {
        s.load(&doc1).is_ok_and(|d| {
            d.blocks.len() >= 2
                && d.blocks[0].text.text == "Para A edited on cloud"
                && d.blocks[1].text.text == "Para B edited while unlinked"
        })
    })
    .await;

    let b_d1 = b.store.lock().unwrap().load(&doc1).unwrap();
    assert_eq!(b_d1.blocks[0].text.text, "Para A edited on cloud");
    assert_eq!(b_d1.blocks[1].text.text, "Para B edited while unlinked");
    let b_d2 = b.store.lock().unwrap().load(&doc2).unwrap();
    assert_eq!(b_d2.blocks[0].text.text, "B offline unlinked edit");
    assert!(b.store.lock().unwrap().list().len() >= 3, "conflict copy should preserve cloud edit of doc2");

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn multiversion_batch_coalesces_without_false_conflict() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("multi-a");
    let mut b = TestClient::new("multi-b");

    a.handle.register(&url, "multi@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "multi@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();

    let doc_id = a.store.lock().unwrap().create_from(&doc_with("Coalesce", &["v1"])).unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() }]);
    b.wait_until(|s| s.load(&doc_id).is_ok_and(|d| d.blocks[0].text.text == "v1")).await;
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    // Pause B while A saves two sequential edits to the same block (v2 then v3).
    b.handle.set_paused(true);
    for text in ["v2", "v3"] {
        {
            let mut sa = a.store.lock().unwrap();
            let mut doc = sa.load(&doc_id).unwrap();
            doc.blocks[0].text = RichText::plain(text);
            sa.save_editor(&doc_id, &doc).unwrap();
        }
        a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() }]);
        a.sync_now().await;
    }

    b.handle.set_paused(false);
    b.wait_until(|s| s.load(&doc_id).is_ok_and(|d| d.blocks[0].text.text == "v3")).await;
    b.sync_now().await;
    a.sync_now().await;

    assert_eq!(b.store.lock().unwrap().list().len(), 1, "must not generate false conflict copy");
    assert_eq!(a.store.lock().unwrap().load(&doc_id).unwrap().blocks[0].text.text, "v3");

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn concurrent_delete_and_edit_protects_live_doc_from_trash() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("trashprot-a");
    let mut b = TestClient::new("trashprot-b");

    a.handle.register(&url, "tp@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "tp@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();

    let doc_id = a.store.lock().unwrap().create_from(&doc_with("LiveOrTrash", &["v1"])).unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() }]);
    b.wait_until(|s| s.load(&doc_id).is_ok()).await;
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    b.handle.set_paused(true);

    // A deletes doc_id -> pushes Document tombstone + TrashItem.
    a.store.lock().unwrap().delete(&doc_id, None).unwrap();
    a.handle.notify_changes(vec![
        LocalChangeHint::Delete { kind: EntityKind::Document, entity_id: doc_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::TrashItem, entity_id: trash_entity_id(&doc_id) },
    ]);
    a.wait_until(|s| s.list().is_empty() && s.list_trash().iter().any(|t| t.id == doc_id)).await;
    a.sync_now().await;

    // B edits doc_id offline, then resumes.
    {
        let mut sb = b.store.lock().unwrap();
        let mut doc = sb.load(&doc_id).unwrap();
        doc.blocks[0].text = RichText::plain("Important offline edit");
        sb.save_editor(&doc_id, &doc).unwrap();
    }
    b.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() }]);
    b.handle.set_paused(false);

    a.wait_until(|s| s.load(&doc_id).is_ok_and(|d| d.blocks[0].text.text == "Important offline edit")).await;
    let b_doc = b.store.lock().unwrap().load(&doc_id).unwrap();
    assert_eq!(b_doc.blocks[0].text.text, "Important offline edit");
    assert!(b.store.lock().unwrap().list_trash().iter().all(|t| t.id != doc_id));

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn renamed_parent_folder_preserves_nesting_in_snapshot() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("topo-a");
    let mut b = TestClient::new("topo-b");

    a.handle.register(&url, "topo@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();

    let (parent_id, child_id, doc_id) = {
        let mut sa = a.store.lock().unwrap();
        let pid = sa.create_folder("Parent", None).unwrap();
        let cid = sa.create_folder("Child", Some(&pid)).unwrap();
        let did = sa.create_from(&doc_with("NestedNote", &["Inside Child"])).unwrap();
        sa.set_parent(&did, Some(&cid)).unwrap();
        (pid, cid, did)
    };
    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Folder, entity_id: parent_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::Folder, entity_id: child_id.clone() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() },
    ]);
    a.sync_now().await;

    // Rename Parent -> ParentRenamed so Parent gets the highest seq in entities table!
    a.store.lock().unwrap().rename(&parent_id, "ParentRenamed").unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Folder, entity_id: parent_id.clone() }]);
    a.sync_now().await;

    b.handle.login(&url, "topo@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    b.wait_until(|s| s.load(&doc_id).is_ok()).await;

    {
        let mut sb = b.store.lock().unwrap();
        let list_b = sb.list();
        let p_entry = list_b.iter().find(|e| e.id == parent_id).expect("parent exists");
        let c_entry = list_b.iter().find(|e| e.id == child_id).expect("child exists");
        let d_entry = list_b.iter().find(|e| e.id == doc_id).expect("doc exists");
        assert_eq!(p_entry.title, "ParentRenamed");
        assert_eq!(p_entry.meta.parent.as_deref(), None);
        assert_eq!(c_entry.meta.parent.as_deref(), Some(parent_id.as_str()));
        assert_eq!(d_entry.meta.parent.as_deref(), Some(child_id.as_str()));
        let path_str = sb.file_path(&doc_id).unwrap().to_string_lossy().replace('\\', "/");
        assert!(path_str.ends_with("ParentRenamed/Child/NestedNote.xml"), "unexpected path: {path_str}");
    }

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn scope_toggle_blocks_outbox_and_reconciles_on_enable() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("scope-a");
    let mut b = TestClient::new("scope-b");

    a.handle.register(&url, "scope@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "scope@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    // Queue preferences while paused, then disable settings scope before resuming.
    a.handle.set_paused(true);
    std::fs::create_dir_all(a.lib_dir.join(".quiet")).unwrap();
    std::fs::write(a.lib_dir.join(".quiet/preferences.json"), br#"{"theme":"nord-a"}"#).unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert {
        kind: EntityKind::Config,
        entity_id: config_entity_id("preferences"),
    }]);
    tokio::time::sleep(Duration::from_millis(30)).await;
    a.handle.set_scope(SyncScope { trash: true, history: true, settings: false });
    a.handle.set_paused(false);
    a.sync_now().await;

    let conn = Connection::open(srv_dir.join("sync.db")).unwrap();
    let cfg_eid = config_entity_id("preferences");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities WHERE entity_id = ?1", [&cfg_eid], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0, "disabled scope config must not upload queued outbox item");

    // B creates preferences; A must ignore while disabled, then reconcile when re-enabled.
    std::fs::create_dir_all(b.lib_dir.join(".quiet")).unwrap();
    std::fs::write(b.lib_dir.join(".quiet/preferences.json"), br#"{"font_size":18,"theme":"warm-b"}"#).unwrap();
    b.handle.notify_changes(vec![LocalChangeHint::Upsert {
        kind: EntityKind::Config,
        entity_id: config_entity_id("preferences"),
    }]);
    b.sync_now().await;

    a.handle.set_scope(SyncScope { trash: true, history: true, settings: true });
    let a_prefs = a.lib_dir.join(".quiet/preferences.json");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if let Ok(txt) = std::fs::read_to_string(&a_prefs)
            && txt.contains("font_size")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let a_txt = std::fs::read_to_string(&a_prefs).unwrap();
    assert!(a_txt.contains("font_size"), "re-enabling settings scope should reconcile remote config");

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn url_validation_and_asset_tombstone_ref_protection() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("assetdel-a");
    let mut b = TestClient::new("assetdel-b");

    assert!(a.handle.check_server("http://sync.example.com").await.is_err(), "non-loopback http:// must be rejected");
    a.handle.check_server(&url).await.unwrap();

    a.handle.register(&url, "asset@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "asset@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();

    std::fs::create_dir_all(a.lib_dir.join(".assets")).unwrap();
    std::fs::write(a.lib_dir.join(".assets/keep.png"), b"KEEP_ASSET").unwrap();
    std::fs::write(a.lib_dir.join(".assets/orphan.png"), b"ORPHAN_ASSET").unwrap();
    let md_doc = doc_with("ImgDoc", &[]);
    let mut md_full = doc_with("ImgDoc", &[]);
    md_full.blocks.push(Block::text("![keep](.assets/keep.png)"));
    let doc_id = {
        let mut sa = a.store.lock().unwrap();
        let id = sa.create_markdown(&md_doc, "").unwrap();
        let path = sa.file_path(&id).unwrap();
        std::fs::write(&path, "# ImgDoc\n\n![keep](.assets/keep.png)\n").unwrap();
        id
    };
    let _ = md_full;
    a.handle.notify_changes(vec![
        LocalChangeHint::Upsert { kind: EntityKind::Asset, entity_id: ".assets/keep.png".into() },
        LocalChangeHint::Upsert { kind: EntityKind::Asset, entity_id: ".assets/orphan.png".into() },
        LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() },
    ]);

    let b_keep = b.lib_dir.join(".assets/keep.png");
    let b_orphan = b.lib_dir.join(".assets/orphan.png");
    b.wait_until(|s| s.load(&doc_id).is_ok() && b_keep.exists() && b_orphan.exists()).await;
    a.sync_now().await;

    // Remove both asset files on A and push deletions; B's referenced keep.png must survive & re-queue.
    std::fs::remove_file(a.lib_dir.join(".assets/keep.png")).unwrap();
    std::fs::remove_file(a.lib_dir.join(".assets/orphan.png")).unwrap();
    a.handle.notify_changes(vec![
        LocalChangeHint::Delete { kind: EntityKind::Asset, entity_id: ".assets/keep.png".into() },
        LocalChangeHint::Delete { kind: EntityKind::Asset, entity_id: ".assets/orphan.png".into() },
    ]);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        if !b_orphan.exists() && a.lib_dir.join(".assets/keep.png").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!b_orphan.exists(), "unreferenced asset tombstone must delete local orphan file");
    assert!(b_keep.exists(), "referenced asset must survive stale tombstone");
    assert!(a.lib_dir.join(".assets/keep.png").exists(), "referenced asset should recover via requeue");

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}

#[tokio::test]
async fn idle_sync_interval_waits_for_inactivity() {
    let _gate = E2E_GATE.acquire().await.unwrap();
    let srv_dir = std::env::temp_dir().join(format!("quiet-e2e-srv-{}", uuid::Uuid::new_v4()));
    let (addr, srv) = spawn_test_server(srv_dir.clone()).await.unwrap();
    let url = format!("http://{addr}");

    let mut a = TestClient::new("idle-a");
    let mut b = TestClient::new("idle-b");

    a.handle.register(&url, "idle@example.com", "secret12", "Alice", "MacA", None).await.unwrap();
    let (vault, _) = a.handle.create_vault("Main", None).await.unwrap();
    b.handle.login(&url, "idle@example.com", "secret12", "MacB").await.unwrap();
    b.handle.link_vault(vault, None).await.unwrap();
    a.wait_for_synced().await;
    b.wait_for_synced().await;

    // Configure 1s idle sync interval on A.
    a.handle.set_sync_interval_secs(1);
    assert_eq!(a.handle.sync_interval_secs(), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let doc_id = a.store.lock().unwrap().create_from(&doc_with("IdleDoc", &["hello"])).unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc_id.clone() }]);

    // Keep signalling user activity every 350ms for ~1.05s: sync must stay deferred while typing.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(matches!(a.handle.status(), SyncStatus::Pending { pending } if pending > 0));
        assert!(b.store.lock().unwrap().load(&doc_id).is_err(), "sync should wait while user is still active");
        a.handle.notify_user_activity();
    }

    // Once user stops typing, the change syncs automatically after 1s of inactivity.
    b.wait_until(|s| s.load(&doc_id).is_ok_and(|d| d.blocks[0].text.text == "hello")).await;

    // Switching from a long interval (60s) to instant (0s) while a change is queued flushes promptly.
    a.handle.set_sync_interval_secs(60);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let doc2 = a.store.lock().unwrap().create_from(&doc_with("Shortened", &["now"])).unwrap();
    a.handle.notify_changes(vec![LocalChangeHint::Upsert { kind: EntityKind::Document, entity_id: doc2.clone() }]);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(b.store.lock().unwrap().load(&doc2).is_err());
    a.handle.set_sync_interval_secs(0);
    b.wait_until(|s| s.load(&doc2).is_ok_and(|d| d.blocks[0].text.text == "now")).await;

    let usage = a.handle.storage_usage().await.unwrap();
    assert_eq!(usage.used_bytes, usage.entity_bytes + usage.blob_bytes);
    assert!(usage.entity_count >= 2 && usage.entity_bytes > 0);
    assert_eq!(usage.max_blob_bytes, 32 * 1024 * 1024);
    assert_eq!(usage.quota_bytes, None);

    a.cleanup().await;
    b.cleanup().await;
    srv.abort();
    let _ = std::fs::remove_dir_all(srv_dir);
}
