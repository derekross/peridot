//! A computer removed by a rotation still holds the previous epoch's
//! secret. Nothing it writes there may get it the next one.

use std::sync::Arc;
use std::time::Duration;

use nostr_sdk::prelude::*;
use opal_core::db::Db;
use opal_kit::relays::Outbox;
use peridot_sync::apply::Home;
use peridot_sync::crypto::SyncSecret;
use peridot_sync::envelope::{self, DeviceInfo, Item};
use peridot_sync::identity::Identity;
use peridot_sync::manifest::{Choices, Manifest};
use peridot_sync::rotation::{self, Rotation};
use peridot_sync::signer::LocalSigner;
use peridot_sync::store::SyncStore;
use peridot_sync::sync::{SyncEngine, SyncParams};

const DATA_KIND: u16 = 30078;

/// One computer: its identity (with its own device key) and database, so
/// it can be restarted on a new epoch the way the daemon does.
struct Computer {
    identity: Identity,
    db: Db,
    engine: SyncEngine,
    _home: tempfile::TempDir,
}

impl Computer {
    async fn new(shared: &Identity, relay: &RelayUrl, name: &str) -> Self {
        let mut identity = shared.clone();
        identity.device = Keys::generate();
        let db = Db::open_in_memory().unwrap();
        let (engine, home) = engine(&identity, None, &db, relay, name).await;
        Self {
            identity,
            db,
            engine,
            _home: home,
        }
    }

    /// Switch to `secret`, keeping the previous one for its window.
    async fn adopt(&mut self, secret: SyncSecret, relay: &RelayUrl) {
        let previous = self.identity.secret.clone();
        self.identity = self.identity.clone().with_secret(secret);
        let (engine, home) = engine(&self.identity, Some(previous), &self.db, relay, "again").await;
        self.engine = engine;
        self._home = home;
    }

    fn id(&self) -> String {
        self.engine.device_id().to_string()
    }

    async fn devices(&self) -> Vec<DeviceInfo> {
        self.engine.overview().await.devices
    }

    async fn sync(&self) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.engine.catch_up().await.unwrap();
    }
}

async fn engine(
    identity: &Identity,
    previous: Option<SyncSecret>,
    db: &Db,
    relay: &RelayUrl,
    name: &str,
) -> (SyncEngine, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let engine = SyncEngine::new(SyncParams {
        identity: identity.clone(),
        previous,
        signer: Arc::new(LocalSigner(identity.keys.clone().unwrap())),
        store: SyncStore::new(db.clone()).unwrap(),
        outbox: Outbox::new(db.clone()).unwrap(),
        home: Home::open(home.path()).unwrap(),
        manifest: Manifest::new(Choices::default()),
        client: Client::default(),
        relays: vec![relay.clone()],
        backups_dir: home.path().join(".backups"),
        device_name: name.into(),
        version: "test".into(),
    })
    .unwrap();
    engine.connect().await;
    engine.catch_up().await.unwrap();
    (engine, home)
}

async fn mock_relay() -> MockRelay {
    for _ in 0..10 {
        if let Ok(r) = MockRelay::run().await {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no free port for a mock relay");
}

/// What the removed computer can still write: an item sealed and signed
/// under the epoch it was removed from.
fn written_under(secret: &SyncSecret, item: &Item) -> Event {
    let keys = secret.keys();
    let s = envelope::seal(&keys, item).unwrap();
    EventBuilder::new(Kind::Custom(DATA_KIND), s.content)
        .tag(Tag::identifier(s.d))
        .custom_created_at(Timestamp::now())
        .finalize(keys.signer().unwrap())
        .unwrap()
}

/// A new device ID carrying the removed computer's own device key.
fn ghost(removed: &Computer) -> Item {
    Item::Device(DeviceInfo {
        id: "ghost".into(),
        name: "Ghost".into(),
        version: "test".into(),
        last_seen: Timestamp::now().as_secs(),
        removed: false,
        pubkey: Some(removed.identity.device.public_key().to_hex()),
    })
}

/// Its own entry again, no longer marked removed.
fn unremoved(removed: &Computer) -> Item {
    Item::Device(DeviceInfo {
        id: removed.id(),
        name: "Laptop".into(),
        version: "test".into(),
        last_seen: Timestamp::now().as_secs() + 60,
        removed: false,
        pubkey: Some(removed.identity.device.public_key().to_hex()),
    })
}

/// The rotation announced from `from`'s epoch, as the relay holds it.
async fn rotation_from(relay: &RelayUrl, from: &SyncSecret) -> Event {
    let client = Client::default();
    client.add_relay(relay.clone()).await.unwrap();
    client.connect().await;
    let filter = Filter::new()
        .author(from.rekey_keys().public_key())
        .kind(Kind::Custom(DATA_KIND));
    for _ in 0..20 {
        let events = client
            .fetch_events(filter.clone())
            .timeout(Duration::from_secs(2))
            .await
            .unwrap();
        if let Some(ev) = events.into_iter().next() {
            return ev;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("no rotation from epoch {}", from.epoch());
}

/// Who the rotation hands the next secret to, among these device keys.
fn receives(ev: &Event, from: &SyncSecret, device: &Keys) -> bool {
    matches!(
        rotation::adopt(ev, from, device),
        Ok(Rotation::Adopt { .. })
    )
}

/// Whether an item named `label` exists under `secret`'s epoch on the relay.
async fn on_relay(relay: &RelayUrl, secret: &SyncSecret, label: &str) -> bool {
    let keys = secret.keys();
    let client = Client::default();
    client.add_relay(relay.clone()).await.unwrap();
    client.connect().await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let filter = Filter::new()
        .author(keys.signer().unwrap().public_key())
        .kind(Kind::Custom(DATA_KIND))
        .identifier(keys.name(label));
    !client
        .fetch_events(filter)
        .timeout(Duration::from_secs(2))
        .await
        .unwrap()
        .is_empty()
}

/// Desk, spare and laptop on epoch 1, all knowing each other.
async fn three(relay: &RelayUrl) -> (Computer, Computer, Computer) {
    let id = Identity::generate();
    let desk = Computer::new(&id, relay, "Desk").await;
    let spare = Computer::new(&id, relay, "Spare").await;
    let laptop = Computer::new(&id, relay, "Laptop").await;
    for c in [&desk, &spare, &laptop] {
        c.engine.announce().await.unwrap();
    }
    for c in [&desk, &spare, &laptop] {
        c.sync().await;
        assert_eq!(c.devices().await.len(), 3);
    }
    (desk, spare, laptop)
}

#[tokio::test]
async fn a_removed_computer_cant_enroll_again_through_the_window() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let (mut desk, spare, laptop) = three(&url).await;
    let s1 = desk.identity.secret.clone();

    // The desk removes the laptop and switches, as the daemon does.
    let s2 = desk.engine.rotate(&[laptop.id()]).await.unwrap();
    desk.adopt(s2.clone(), &url).await;
    desk.engine.republish_everything().await.unwrap();

    // During the window the laptop writes, under epoch 1, a new device
    // with its own key and its own entry no longer marked removed.
    for item in [ghost(&laptop), unremoved(&laptop)] {
        assert!(!desk.engine.ingest(&written_under(&s1, &item)));
    }
    let devices = desk.devices().await;
    assert!(devices.iter().all(|d| d.id != "ghost"), "{devices:?}");
    assert!(
        devices.iter().any(|d| d.id == laptop.id() && d.removed),
        "{devices:?}"
    );
    assert!(
        !on_relay(&url, &s2, "dev:ghost").await,
        "not said again under epoch 2"
    );

    // The next rotation reaches the spare and never the laptop.
    let s3 = desk.engine.rotate(&[]).await.unwrap();
    let ev = rotation_from(&url, &s2).await;
    assert!(receives(&ev, &s2, &spare.identity.device));
    assert!(!receives(&ev, &s2, &laptop.identity.device));
    assert_eq!(s3.epoch(), 3);
}

#[tokio::test]
async fn what_a_computer_heard_before_switching_doesnt_count_after() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let (desk, mut spare, laptop) = three(&url).await;
    let s1 = desk.identity.secret.clone();
    let s2 = desk.engine.rotate(&[laptop.id()]).await.unwrap();

    // The spare, not yet switched, takes the laptop's epoch-1 writes in as
    // current: from where it stands they are.
    spare.engine.ingest(&written_under(&s1, &ghost(&laptop)));
    spare
        .engine
        .ingest(&written_under(&s1, &unremoved(&laptop)));
    assert!(spare.devices().await.iter().any(|d| d.id == "ghost"));

    // Having heard the rotation, it won't start one of its own from
    // epoch 1.
    spare.sync().await;
    let refused = spare.engine.rotate(&[]).await.unwrap_err();
    assert!(refused.to_string().contains("new epoch"), "{refused}");

    // Once switched, neither its check of the servers nor its own next
    // rotation carries what it took in under epoch 1.
    spare.adopt(s2.clone(), &url).await;
    spare.engine.announce().await.unwrap();
    spare
        .engine
        .audit(Timestamp::now().as_secs())
        .await
        .unwrap();
    assert!(!on_relay(&url, &s2, "dev:ghost").await);
    assert!(!on_relay(&url, &s2, &format!("dev:{}", laptop.id())).await);
    spare.engine.rotate(&[]).await.unwrap();
    let ev = rotation_from(&url, &s2).await;
    assert!(!receives(&ev, &s2, &laptop.identity.device));
}

#[tokio::test]
async fn saying_everything_again_never_carries_what_was_heard_before_switching() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let (desk, mut spare, laptop) = three(&url).await;
    let s1 = desk.identity.secret.clone();
    let s2 = desk.engine.rotate(&[laptop.id()]).await.unwrap();

    // The spare, lagging, takes the laptop's epoch-1 entries in, then
    // switches, and says everything again: as it would with a republish
    // flag left behind by a rotation of its own that was refused.
    spare.engine.ingest(&written_under(&s1, &ghost(&laptop)));
    spare
        .engine
        .ingest(&written_under(&s1, &unremoved(&laptop)));
    spare.adopt(s2.clone(), &url).await;
    spare.engine.republish_everything().await.unwrap();
    assert!(!on_relay(&url, &s2, "dev:ghost").await);
    assert!(!on_relay(&url, &s2, &format!("dev:{}", laptop.id())).await);
}

#[tokio::test]
async fn the_rotator_says_its_directory_again_and_isnt_removed_by_its_own_announcement() {
    let relay = mock_relay().await;
    let url = relay.url().await;
    let (mut desk, spare, laptop) = three(&url).await;
    let s2 = desk.engine.rotate(&[laptop.id()]).await.unwrap();

    // Its own announcement comes back from the relay: no wrap for itself,
    // and still not a removal.
    desk.sync().await;
    assert!(desk.engine.take_pending_rotation().is_none());

    // After switching, the directory it rotated from is said again under
    // the new keys, the laptop's entry marked removed.
    desk.adopt(s2.clone(), &url).await;
    desk.engine.republish_everything().await.unwrap();
    for id in [desk.id(), spare.id(), laptop.id()] {
        assert!(on_relay(&url, &s2, &format!("dev:{id}")).await, "{id}");
    }
    let devices = desk.devices().await;
    assert!(
        devices.iter().any(|d| d.id == laptop.id() && d.removed),
        "{devices:?}"
    );
}
