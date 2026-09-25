//! Two computers sharing one identity, syncing through a local relay.

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use opal_core::db::Db;
use opal_kit::relays::Outbox;
use opal_kit::signer::KeysSigner;
use peridot_sync::apply::Home;
use peridot_sync::identity::Identity;
use peridot_sync::manifest::{Choices, Manifest};
use peridot_sync::store::{FileStatus, SyncStore};
use peridot_sync::sync::{SyncEngine, SyncParams};

struct Computer {
    engine: SyncEngine,
    home: tempfile::TempDir,
    _data: tempfile::TempDir,
}

impl Computer {
    async fn new(identity: &Identity, relay: &RelayUrl, name: &str) -> Self {
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let engine = SyncEngine::new(SyncParams {
            identity: identity.clone(),
            signer: Arc::new(KeysSigner(identity.keys.clone())),
            store: SyncStore::new(db.clone()).unwrap(),
            outbox: Outbox::new(db).unwrap(),
            home: Home::open(home.path()).unwrap(),
            manifest: Manifest::new(Choices::default()),
            client: Client::default(),
            relays: vec![relay.clone()],
            backups_dir: data.path().join("backups"),
            device_name: name.into(),
            version: "test".into(),
        })
        .unwrap();
        engine.connect().await;
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
        self.engine.catch_up().await;
    }
}

const BINDINGS: &str = ".config/hypr/bindings.lua";

#[tokio::test]
async fn edits_flow_between_computers_and_can_be_undone() {
    let relay = MockRelay::run().await.unwrap();
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
    assert_eq!(laptop.status(BINDINGS).await, Some(FileStatus::Outgoing));
}

#[tokio::test]
async fn concurrent_edits_become_a_conflict_and_either_side_can_win() {
    let relay = MockRelay::run().await.unwrap();
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
    let relay = MockRelay::run().await.unwrap();
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
    let relay = MockRelay::run().await.unwrap();
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
    let relay = MockRelay::run().await.unwrap();
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
