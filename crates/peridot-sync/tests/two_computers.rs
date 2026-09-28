//! Two computers sharing one identity, syncing through a local relay.

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use opal_core::db::Db;
use opal_kit::relays::Outbox;
use peridot_sync::apply::Home;
use peridot_sync::identity::Identity;
use peridot_sync::manifest::{Choices, Manifest};
use peridot_sync::signer::LocalSigner;
use peridot_sync::store::{FileStatus, SyncStore};
use peridot_sync::sync::{SyncEngine, SyncParams};

struct Computer {
    engine: SyncEngine,
    home: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl Computer {
    async fn new(identity: &Identity, relay: &RelayUrl, name: &str) -> Self {
        Self::with_relays(identity, vec![relay.clone()], name).await
    }

    async fn with_relays(identity: &Identity, relays: Vec<RelayUrl>, name: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let engine = SyncEngine::new(SyncParams {
            identity: identity.clone(),
            signer: Arc::new(LocalSigner(identity.keys.clone().unwrap())),
            store: SyncStore::new(db.clone()).unwrap(),
            outbox: Outbox::new(db).unwrap(),
            home: Home::open(home.path()).unwrap(),
            manifest: Manifest::new(Choices::default()),
            client: Client::default(),
            relays,
            backups_dir: data.path().join("backups"),
            device_name: name.into(),
            version: "test".into(),
        })
        .unwrap();
        engine.connect().await;
        // As the daemon does before anything else.
        engine.catch_up().await.unwrap();
        Self {
            engine,
            home,
            _data: data,
        }
    }

    fn write(&self, path: &str, content: &[u8]) {
        let p = self.home.path().join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        std::fs::read(self.home.path().join(path)).ok()
    }

    async fn status(&self, path: &str) -> Option<FileStatus> {
        self.engine
            .overview()
            .await
            .files
            .into_iter()
            .find(|f| f.path == path)
            .map(|f| f.status)
    }

    async fn sync(&self) {
        // Relays need a moment to make events queryable.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.engine.catch_up().await.unwrap();
    }
}

const BINDINGS: &str = ".config/hypr/bindings.lua";

/// MockRelay picks a random port; retry the rare collision.
async fn mock_relay() -> MockRelay {
    for _ in 0..10 {
        if let Ok(r) = MockRelay::run().await {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no free port for a mock relay");
}

#[tokio::test]
async fn edits_flow_between_computers_and_can_be_undone() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let id = Identity::generate();
    let desk = Computer::new(&id, &url, "Desk").await;
    let laptop = Computer::new(&id, &url, "Laptop").await;

    desk.write(BINDINGS, b"bind = SUPER, Return, exec, alacritty");
    desk.write(".config/hypr/monitors.lua", b"monitor = DP-1");
    desk.engine.announce().await.unwrap();
    let report = desk.engine.publish_changes().await.unwrap();
    assert_eq!(report.published, [BINDINGS]);
    assert_eq!(desk.status(BINDINGS).await, Some(FileStatus::InSync));

    // The laptop sees it as incoming and nothing is written until applied.
    laptop.write(BINDINGS, b"-- laptop default");
    laptop.sync().await;
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::Incoming));
    let overview = laptop.engine.overview().await;
    let row = overview.files.iter().find(|f| f.path == BINDINGS).unwrap();
    assert_eq!(row.from.as_deref(), Some("Desk"));
    assert!(
        overview
            .files
            .iter()
            .all(|f| f.path != ".config/hypr/monitors.lua")
    );
    assert_eq!(laptop.read(BINDINGS).unwrap(), b"-- laptop default");

    let applied = laptop.engine.apply(&[]).await.unwrap();
    assert_eq!(applied.applied, [BINDINGS]);
    assert_eq!(
        laptop.read(BINDINGS).unwrap(),
        b"bind = SUPER, Return, exec, alacritty"
    );
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::InSync));
    assert_eq!(
        laptop.read(".config/hypr/monitors.lua"),
        None,
        "local tier never travels"
    );

    // Undo puts the laptop's version back.
    let restored = laptop
        .engine
        .undo(applied.history_id.unwrap())
        .await
        .unwrap();
    assert_eq!(restored, [BINDINGS]);
    assert_eq!(laptop.read(BINDINGS).unwrap(), b"-- laptop default");
    // Kept here only: the desk isn't asked to take it.
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::Kept));
    assert!(
        laptop
            .engine
            .publish_changes()
            .await
            .unwrap()
            .published
            .is_empty()
    );
    desk.sync().await;
    assert_eq!(desk.status(BINDINGS).await, Some(FileStatus::InSync));

    // A new change on the desk is offered again.
    desk.write(BINDINGS, b"bind = SUPER, Return, exec, kitty");
    desk.engine.publish_changes().await.unwrap();
    laptop.sync().await;
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::Incoming));
    // And a kept file can still be taken explicitly.
    laptop.engine.apply(&[BINDINGS.to_string()]).await.unwrap();
    assert_eq!(
        laptop.read(BINDINGS).unwrap(),
        b"bind = SUPER, Return, exec, kitty"
    );
}

#[tokio::test]
async fn concurrent_edits_become_a_conflict_and_either_side_can_win() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let id = Identity::generate();
    let desk = Computer::new(&id, &url, "Desk").await;
    let laptop = Computer::new(&id, &url, "Laptop").await;

    desk.write(BINDINGS, b"v1");
    desk.engine.publish_changes().await.unwrap();
    laptop.sync().await;
    laptop.engine.apply(&[]).await.unwrap();

    desk.write(BINDINGS, b"desk v2");
    desk.engine.publish_changes().await.unwrap();
    laptop.write(BINDINGS, b"laptop v2");
    laptop.sync().await;
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::Conflict));
    // "Apply all" never overwrites a conflict.
    assert!(laptop.engine.apply(&[]).await.unwrap().applied.is_empty());
    assert_eq!(laptop.read(BINDINGS).unwrap(), b"laptop v2");

    // Keep the laptop's: it becomes the version everywhere.
    laptop.engine.keep_local(BINDINGS).await.unwrap();
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::InSync));
    desk.sync().await;
    assert_eq!(desk.status(BINDINGS).await, Some(FileStatus::Incoming));
    desk.engine.apply(&[]).await.unwrap();
    assert_eq!(desk.read(BINDINGS).unwrap(), b"laptop v2");
}

#[tokio::test]
async fn deletions_and_big_files_sync() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let id = Identity::generate();
    let desk = Computer::new(&id, &url, "Desk").await;
    let laptop = Computer::new(&id, &url, "Laptop").await;

    let big: Vec<u8> = (0..200_000u32)
        .map(|i| b"abcdefghij\n"[(i % 11) as usize])
        .collect();
    desk.write(".config/kitty/kitty.conf", &big);
    desk.write(".XCompose", "<Multi_key> <o> <o> : \"°\"".as_bytes());
    desk.engine.publish_changes().await.unwrap();
    laptop.sync().await;
    laptop.engine.apply(&[]).await.unwrap();
    assert_eq!(laptop.read(".config/kitty/kitty.conf").unwrap(), big);

    std::fs::remove_file(desk.home.path().join(".XCompose")).unwrap();
    let report = desk.engine.publish_changes().await.unwrap();
    assert_eq!(report.deleted, [".XCompose"]);
    laptop.sync().await;
    assert_eq!(laptop.status(".XCompose").await, Some(FileStatus::Incoming));
    laptop.engine.apply(&[]).await.unwrap();
    assert_eq!(laptop.read(".XCompose"), None);
}

#[tokio::test]
async fn secrets_and_other_identities_stay_out() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let id = Identity::generate();
    let desk = Computer::new(&id, &url, "Desk").await;
    let stranger = Computer::new(&Identity::generate(), &url, "Stranger").await;

    desk.write(
        ".config/hypr/looknfeel.lua",
        b"-- token = ghp_abcdefghijklmnopqrstuv",
    );
    desk.write(BINDINGS, b"fine");
    let report = desk.engine.publish_changes().await.unwrap();
    assert_eq!(report.published, [BINDINGS]);
    assert_eq!(report.skipped.len(), 1);

    stranger.sync().await;
    assert!(stranger.engine.overview().await.files.is_empty());
    assert!(stranger.engine.store.remote_paths().unwrap().is_empty());
}

#[tokio::test]
async fn devices_and_theme_are_shared() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let id = Identity::generate();
    let desk = Computer::new(&id, &url, "Desk").await;
    let laptop = Computer::new(&id, &url, "Laptop").await;

    desk.write(".local/state/omarchy/current/theme.name", b"catppuccin\n");
    laptop.write(".local/state/omarchy/current/theme.name", b"tokyo-night\n");
    desk.engine.announce().await.unwrap();
    // A computer always catches up before announcing (as the daemon does).
    laptop.sync().await;
    laptop.engine.announce().await.unwrap();
    let overview = laptop.engine.overview().await;
    let names: Vec<&str> = overview.devices.iter().map(|d| d.name.as_str()).collect();
    assert!(names.contains(&"Desk") && names.contains(&"Laptop"));
    assert_eq!(
        serde_json::to_value(&overview.offers).unwrap(),
        serde_json::json!([{"kind": "theme", "name": "catppuccin", "from": "Desk"}])
    );

    // "Not now" hides it.
    laptop.engine.dismiss(&overview.offers[0]).unwrap();
    assert!(laptop.engine.overview().await.offers.is_empty());

    // The laptop didn't change its theme, so it doesn't override the desk's.
    desk.sync().await;
    assert!(desk.engine.overview().await.offers.is_empty());
}

#[tokio::test]
async fn a_computer_that_cant_see_the_servers_publishes_nothing() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let id = Identity::generate();
    let desk = Computer::new(&id, &url, "Desk").await;
    desk.write(BINDINGS, b"the real bindings");
    desk.engine.publish_changes().await.unwrap();

    // A newly paired laptop whose servers are unreachable.
    let dead = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let db = Db::open_in_memory().unwrap();
    let laptop = SyncEngine::new(SyncParams {
        identity: id.clone(),
        signer: Arc::new(LocalSigner(id.keys.clone().unwrap())),
        store: SyncStore::new(db.clone()).unwrap(),
        outbox: Outbox::new(db).unwrap(),
        home: Home::open(home.path()).unwrap(),
        manifest: Manifest::new(Choices::default()),
        client: Client::default(),
        relays: vec![dead],
        backups_dir: data.path().join("b"),
        device_name: "Laptop".into(),
        version: "test".into(),
    })
    .unwrap();
    std::fs::create_dir_all(home.path().join(".config/hypr")).unwrap();
    std::fs::write(home.path().join(BINDINGS), b"laptop defaults").unwrap();
    assert!(laptop.catch_up().await.is_err());
    let report = laptop.publish_changes().await.unwrap();
    assert!(
        report.published.is_empty(),
        "held back until it has caught up"
    );
    laptop.announce().await.unwrap();
    assert_eq!(laptop.store.state("theme").unwrap(), None);
}

/// Everything of ours on one relay: d → created_at.
async fn on_relay(relay: &RelayUrl, id: &Identity) -> std::collections::BTreeMap<String, u64> {
    let client = Client::default();
    client.add_relay(relay).await.unwrap();
    client.connect().and_wait(Duration::from_secs(3)).await;
    let filter = Filter::new()
        .author(id.pubkey())
        .kind(Kind::Custom(peridot_sync::DATA_KIND));
    let events = client
        .fetch_events(vec![(relay.clone(), vec![filter])])
        .timeout(Duration::from_secs(5))
        .await
        .unwrap();
    let out = events
        .iter()
        .filter_map(|e| Some((e.tags.identifier()?, e.created_at.as_secs())))
        .collect();
    client.shutdown().await;
    out
}

async fn deletions_on(relay: &RelayUrl, id: &Identity) -> Vec<Event> {
    let client = Client::default();
    client.add_relay(relay).await.unwrap();
    client.connect().and_wait(Duration::from_secs(3)).await;
    let filter = Filter::new().author(id.pubkey()).kind(Kind::Custom(5));
    let events = client
        .fetch_events(vec![(relay.clone(), vec![filter])])
        .timeout(Duration::from_secs(5))
        .await
        .unwrap();
    let out = events.iter().cloned().collect();
    client.shutdown().await;
    out
}

#[tokio::test]
async fn the_audit_fills_gaps_refreshes_old_items_and_finds_stale_chunks() {
    let r1 = mock_relay().await;
    let r2 = mock_relay().await;
    let (u1, u2) = (r1.url().await, r2.url().await);
    let id = Identity::generate();
    let now = Timestamp::now().as_secs();

    // The desk only knows the first server. A big file (chunks), a small
    // one, its device entry and the root go there.
    let desk = Computer::new(&id, &u1, "Desk").await;
    let big = vec![b'x'; 50 * 1024];
    desk.write(BINDINGS, &big);
    desk.write(".config/kitty/kitty.conf", b"font_size 13");
    desk.engine.publish_root().await.unwrap();
    desk.engine.announce().await.unwrap();
    desk.engine.publish_changes().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let before = on_relay(&u1, &id).await;
    // 2 files, their chunks, state entries, the device and the root.
    assert!(before.len() >= 7, "{before:?}");
    let chunks = before.len() - 6;
    assert!(on_relay(&u2, &id).await.is_empty());

    // The laptop uses both servers: its audit notices the second has
    // nothing and sends everything there.
    let laptop = Computer::with_relays(&id, vec![u1.clone(), u2.clone()], "Laptop").await;
    laptop.engine.announce().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let report = laptop.engine.audit(now).await.unwrap();
    assert_eq!(report.relays.len(), 2);
    assert!(report.relays.iter().all(|r| r.reachable));
    assert_eq!(report.relays[0].missing, 0, "{report:?}");
    assert!(report.relays[1].missing >= before.len(), "{report:?}");
    assert!(report.resent >= before.len(), "{report:?}");
    assert_eq!(report.refreshed, 0);
    assert_eq!(report.stale_chunks, 0);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second = on_relay(&u2, &id).await;
    assert!(
        second.len() > before.len(),
        "both device entries and everything else: {second:?}"
    );
    assert!(laptop.engine.audited_at() >= now);
    // A computer that only knows the second server now gets everything.
    let spare = Computer::new(&id, &u2, "Spare").await;
    assert_eq!(spare.status(BINDINGS).await, Some(FileStatus::Incoming));
    let applied = spare.engine.apply(&[]).await.unwrap();
    assert!(applied.failed.is_empty(), "{applied:?}");
    assert_eq!(spare.read(BINDINGS).unwrap(), big);

    // Nothing to do when everything is everywhere and recent.
    let report = laptop.engine.audit(now + 60).await.unwrap();
    assert_eq!((report.resent, report.refreshed), (0, 0), "{report:?}");

    // A month later, everything is published again with fresh dates.
    let old = on_relay(&u1, &id).await;
    let report = laptop
        .engine
        .audit(now + peridot_sync::sync::REFRESH_AFTER + 60)
        .await
        .unwrap();
    assert!(report.refreshed >= 6, "{report:?}");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let fresh = on_relay(&u1, &id).await;
    for (d, at) in &old {
        assert!(fresh[d] > *at || d.len() == 32, "{d} wasn't refreshed");
    }
    assert_eq!(
        laptop.status(BINDINGS).await,
        Some(FileStatus::Incoming),
        "a refresh changes no status"
    );

    // The desk shrinks the big file: its chunks are orphaned. They're left
    // alone for a week, then found, then removed on request.
    desk.write(BINDINGS, b"small now");
    desk.sync().await;
    desk.engine.publish_changes().await.unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let report = desk.engine.audit(now + 3600).await.unwrap();
    assert_eq!(report.stale_chunks, 0, "too soon: {report:?}");
    let report = desk
        .engine
        .audit(now + peridot_sync::sync::CHUNK_GRACE + 3600)
        .await
        .unwrap();
    assert_eq!(report.stale_chunks, chunks, "{report:?}");
    let signer = desk.engine.signer();
    let removed = desk
        .engine
        .remove_chunks(signer.as_ref(), &report.stale)
        .await
        .unwrap();
    assert_eq!(removed, chunks);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let dels = deletions_on(&u1, &id).await;
    assert_eq!(dels.len(), 1);
    let coords: Vec<String> = dels[0]
        .tags
        .iter()
        .filter(|t| t.kind() == "a")
        .filter_map(|t| t.content().map(String::from))
        .collect();
    assert_eq!(coords.len(), chunks);
    assert!(
        coords
            .iter()
            .all(|c| c.starts_with(&format!("30078:{}:", id.pubkey().to_hex())))
    );
    assert!(dels[0].tags.iter().any(|t| t.as_slice() == ["k", "30078"]));
    let after = desk.engine.last_audit().await.unwrap();
    assert_eq!((after.stale_chunks, after.removed_chunks), (0, chunks));
}
