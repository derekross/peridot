//! The daemon's state: settings, the sync engine once you're set up, a
//! pairing in progress, and the event stream the panel listens to.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nostr_sdk::prelude::*;
use opal_core::db::Db;
use opal_core::ipc::IpcEvent;
use opal_core::keystore::SecretStore;
use opal_kit::relays::Outbox;
use opal_kit::signer::KeysSigner;
use peridot_sync::apply::Home;
use peridot_sync::identity::Identity;
use peridot_sync::manifest::Manifest;
use peridot_sync::store::{FileStatus, SyncStore};
use peridot_sync::sync::{SyncEngine, SyncParams};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::pair::PairSession;

pub struct Options {
    pub config: Config,
    pub config_path: PathBuf,
    pub db: Db,
    pub secrets: SecretStore,
    pub home: PathBuf,
    pub data_dir: PathBuf,
}

pub struct App {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub db: Db,
    pub secrets: SecretStore,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub events: broadcast::Sender<IpcEvent>,
    pub engine: RwLock<Option<Arc<SyncEngine>>>,
    pub runner: Mutex<Option<JoinHandle<()>>>,
    pub pairing: Mutex<Option<PairSession>>,
    /// Wakes the runner for an immediate sync.
    pub nudge: Notify,
    pub last_sync: AtomicU64,
    pub last_error: RwLock<Option<String>>,
}

impl App {
    pub fn new(o: Options) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            config: RwLock::new(o.config),
            config_path: o.config_path,
            db: o.db,
            secrets: o.secrets,
            home: o.home,
            data_dir: o.data_dir,
            events,
            engine: RwLock::new(None),
            runner: Mutex::new(None),
            pairing: Mutex::new(None),
            nudge: Notify::new(),
            last_sync: AtomicU64::new(0),
            last_error: RwLock::new(None),
        })
    }

    /// Start syncing if this computer is already set up.
    pub async fn resume(self: &Arc<Self>) -> anyhow::Result<()> {
        if let Some(identity) = Identity::load(&self.secrets).await? {
            self.start_engine(identity, false).await?;
        }
        Ok(())
    }

    pub async fn engine(&self) -> anyhow::Result<Arc<SyncEngine>> {
        self.engine
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Peridot isn't set up on this computer yet"))
    }

    pub async fn is_set_up(&self) -> bool {
        self.engine.read().await.is_some()
    }

    /// Build the engine for `identity` and start the background runner.
    /// `fresh` publishes the root event (a brand-new identity, or a
    /// restored one whose root we already have).
    pub async fn start_engine(
        self: &Arc<Self>,
        identity: Identity,
        fresh: bool,
    ) -> anyhow::Result<()> {
        // Sync state belongs to one identity. If the keyring now holds a
        // different one (reset keyring, rejoined as someone else), start
        // over rather than trust state that isn't ours.
        let pubkey = identity.pubkey().to_hex();
        if self.db.get_kv("peridot.identity")?.as_deref() != Some(pubkey.as_str()) {
            self.clear_sync_state()?;
            self.db.set_kv("peridot.identity", &pubkey)?;
        }
        let cfg = self.config.read().await.clone();
        let store = SyncStore::new(self.db.clone())?;
        // Signs relay logins (NIP-42), which private-data relays require.
        let client = Client::builder()
            .authenticator(SignerAuthenticator::new(identity.keys.clone()))
            .build();
        let engine = Arc::new(SyncEngine::new(SyncParams {
            signer: Arc::new(KeysSigner(identity.keys.clone())),
            identity,
            store,
            outbox: Outbox::new(self.db.clone())?,
            home: Home::open(&self.home)?,
            manifest: Manifest::new(cfg.sync.clone()),
            client,
            relays: opal_kit::relays::parse_urls(&cfg.relays),
            backups_dir: self.data_dir.join("backups"),
            device_name: cfg.device_name(),
            version: env!("CARGO_PKG_VERSION").into(),
        })?);
        if let Some(old) = self.runner.lock().await.take() {
            old.abort();
        }
        *self.engine.write().await = Some(engine.clone());
        let handle = crate::runner::spawn(self.clone(), engine, fresh);
        *self.runner.lock().await = Some(handle);
        self.emit_state().await;
        Ok(())
    }

    /// Forget this computer's identity and synced state (the other
    /// computers keep theirs).
    pub async fn leave(self: &Arc<Self>) -> anyhow::Result<()> {
        if let Some(r) = self.runner.lock().await.take() {
            r.abort();
        }
        if let Some(engine) = self.engine.write().await.take() {
            engine.client().shutdown().await;
        }
        Identity::forget(&self.secrets).await?;
        self.clear_sync_state()?;
        self.emit_state().await;
        Ok(())
    }

    /// Forget everything synced (not the files themselves).
    fn clear_sync_state(&self) -> anyhow::Result<()> {
        // The tables exist once a store has been opened.
        SyncStore::new(self.db.clone())?;
        Outbox::new(self.db.clone())?;
        self.db.with(|c| {
            c.execute_batch(
                "DELETE FROM synced; DELETE FROM remote; DELETE FROM chunks;
                 DELETE FROM devices; DELETE FROM state; DELETE FROM history;
                 DELETE FROM kit_outbox;
                 DELETE FROM kv WHERE key LIKE 'peridot.%' AND key != 'peridot.device_id';",
            )
        })?;
        Ok(())
    }

    pub async fn save_config(&self, cfg: Config) -> anyhow::Result<()> {
        cfg.save(&self.config_path)?;
        if let Some(engine) = self.engine.read().await.as_ref() {
            engine.set_manifest(Manifest::new(cfg.sync.clone())).await;
        }
        *self.config.write().await = cfg;
        self.nudge.notify_one();
        Ok(())
    }

    pub fn emit(&self, event: &str, data: Value) {
        let _ = self.events.send(IpcEvent {
            event: event.into(),
            data,
        });
    }

    pub async fn emit_state(&self) {
        let snapshot = self.snapshot().await;
        self.emit("state", snapshot);
    }

    pub async fn set_error(&self, e: Option<String>) {
        *self.last_error.write().await = e;
    }

    pub fn mark_synced(&self) {
        self.last_sync
            .store(Timestamp::now().as_secs(), Ordering::Relaxed);
    }

    /// Everything the panel shows.
    pub async fn snapshot(&self) -> Value {
        let cfg = self.config.read().await.clone();
        let pairing = self.pairing.lock().await.as_ref().map(|p| p.view());
        let base = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "device_name": cfg.device_name(),
            "auto_apply": cfg.auto_apply,
            "paused": cfg.paused,
            "relays": cfg.relays,
            "pairing": pairing,
            "error": *self.last_error.read().await,
        });
        let Some(engine) = self.engine.read().await.clone() else {
            let mut v = base;
            v["set_up"] = json!(false);
            return v;
        };
        let overview = engine.overview().await;
        let history = engine.store.history(20).unwrap_or_default();
        let mut v = base;
        v["set_up"] = json!(true);
        v["device_id"] = json!(engine.device_id());
        v["counts"] = json!({
            "in_sync": overview.count(FileStatus::InSync),
            "outgoing": overview.count(FileStatus::Outgoing),
            "incoming": overview.count(FileStatus::Incoming),
            "conflicts": overview.count(FileStatus::Conflict),
            "offers": overview.offers.len(),
            "skipped": overview.skipped.len(),
        });
        v["files"] = json!(overview.files);
        v["skipped"] = json!(overview.skipped);
        v["offers"] = json!(overview.offers);
        v["devices"] = json!(
            overview
                .devices
                .iter()
                .filter(|d| !d.removed)
                .map(|d| json!({
                    "id": d.id,
                    "name": d.name,
                    "last_seen": d.last_seen,
                    "this": d.id == engine.device_id(),
                }))
                .collect::<Vec<_>>()
        );
        v["history"] = json!(history);
        v["waiting_to_send"] = json!(overview.waiting_to_send);
        v["last_sync"] = json!(self.last_sync.load(Ordering::Relaxed));
        v["choices"] = json!(cfg.sync);
        v
    }
}
