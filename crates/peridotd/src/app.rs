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
use peridot_sync::apply::Home;
use peridot_sync::identity::Identity;
use peridot_sync::manifest::Manifest;
use peridot_sync::signer::{IdentitySigner, LocalSigner, SignerAuth};

use crate::opal::{Mode, OpalSigner, Paired};
use peridot_sync::store::{FileStatus, SyncStore};
use peridot_sync::sync::{SyncEngine, SyncParams};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::pair::PairSession;

/// How long the previous epoch's items are still read after a rotation
/// (a computer that was offline catches up), before they're deleted.
const WINDOW: u64 = 7 * 86_400;

pub struct Options {
    pub config: Config,
    pub config_path: PathBuf,
    pub db: Db,
    pub secrets: SecretStore,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub opal_socket: PathBuf,
    pub trust: crate::authz::Trust,
}

pub struct App {
    pub config: RwLock<Config>,
    pub config_path: PathBuf,
    pub db: Db,
    pub secrets: SecretStore,
    pub home: PathBuf,
    pub data_dir: PathBuf,
    pub opal: crate::opal::OpalClient,
    /// Opal's control socket: its presence says Opal is installed.
    pub opal_socket: PathBuf,
    /// For small lookups (NIP-05 addresses).
    pub http: reqwest::Client,
    pub events: broadcast::Sender<IpcEvent>,
    pub engine: RwLock<Option<Arc<SyncEngine>>>,
    pub sharer: RwLock<Option<Arc<crate::share::Sharer>>>,
    pub gallery: RwLock<Option<Arc<crate::gallery::Gallery>>>,
    pub runner: Mutex<Option<JoinHandle<()>>>,
    pub gallery_runner: Mutex<Option<JoinHandle<()>>>,
    pub pairing: Mutex<Option<PairSession>>,
    pub pair_limits: Mutex<crate::pair::PairLimiter>,
    /// Who may ask for what on the socket.
    pub trust: crate::authz::Trust,
    pub limits: crate::authz::Limits,
    pub approvals: crate::approvals::Approvals,
    /// Wakes the runner for an immediate sync.
    pub nudge: Notify,
    pub last_sync: AtomicU64,
    pub last_error: RwLock<Option<String>>,
}

impl App {
    pub fn new(o: Options) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        let app = Arc::new(Self {
            config: RwLock::new(o.config),
            config_path: o.config_path,
            db: o.db,
            secrets: o.secrets,
            home: o.home,
            data_dir: o.data_dir,
            opal: crate::opal::OpalClient::new(&o.opal_socket),
            opal_socket: o.opal_socket,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::limited(3))
                .build()
                .expect("an http client"),
            events,
            engine: RwLock::new(None),
            sharer: RwLock::new(None),
            gallery: RwLock::new(None),
            runner: Mutex::new(None),
            gallery_runner: Mutex::new(None),
            pairing: Mutex::new(None),
            pair_limits: Mutex::new(Default::default()),
            trust: o.trust,
            limits: Default::default(),
            approvals: Default::default(),
            nudge: Notify::new(),
            last_sync: AtomicU64::new(0),
            last_error: RwLock::new(None),
        });
        tokio::spawn(watch_opal(Arc::downgrade(&app)));
        app
    }

    /// Pair with Opal (a prompt appears in its bar) and keep the token.
    /// `pubkey` says which account, when known.
    pub async fn pair_opal(self: &Arc<Self>, pubkey: Option<PublicKey>) -> anyhow::Result<Paired> {
        let result = self
            .opal
            .connect(pubkey.as_ref(), self.gallery_declared())
            .await;
        if let Ok(p) = &result {
            if pubkey.is_some_and(|pk| pk != p.pubkey) {
                self.opal.set_token(None);
                anyhow::bail!("Opal paired Peridot with a different account; pair again");
            }
            crate::opal::save_token(&self.secrets, &p.token).await?;
            self.set_error(None).await;
            self.nudge.notify_one();
        }
        self.emit_state().await;
        result
    }

    /// Whether this computer has asked Opal for the Gallery's kinds (set
    /// the first time the Gallery is used; a re-pair declares them).
    pub fn gallery_declared(&self) -> bool {
        self.db
            .get_kv("peridot.gallery_declared")
            .ok()
            .flatten()
            .as_deref()
            == Some("1")
    }

    /// Set up in Opal mode: the identity Opal signs for.
    async fn opal_identity(&self) -> Option<PublicKey> {
        let engine = self.engine.read().await.clone()?;
        let identity = engine.identity();
        identity.via_opal_mode().then(|| identity.pubkey())
    }

    /// Start syncing if this computer is already set up.
    pub async fn resume(self: &Arc<Self>) -> anyhow::Result<()> {
        if let Some(identity) = Identity::load(&self.secrets).await? {
            if identity.via_opal_mode() {
                // A token from before; Opal may have revoked it meanwhile
                // (or not be up yet, which the first request sorts out).
                let token = crate::opal::load_token(&self.secrets).await?;
                self.opal.set_token(token);
                if self.opal.has_token()
                    && let Err(e) = self.opal.status().await
                    && e.to_string().contains("Pair Peridot")
                {
                    tracing::info!("Opal no longer knows Peridot's pairing");
                }
            }
            let via_opal = identity.via_opal_mode();
            self.start_engine(identity, false).await?;
            // The watcher only pairs once the engine knows the identity, so
            // a missing or revoked token found above is announced now.
            if via_opal && self.opal.needs_pairing() {
                self.opal.changed().notify_one();
            }
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

    pub async fn sharer(&self) -> anyhow::Result<Arc<crate::share::Sharer>> {
        self.sharer
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Peridot isn't set up on this computer yet"))
    }

    pub async fn gallery(&self) -> anyhow::Result<Arc<crate::gallery::Gallery>> {
        self.gallery.read().await.clone().ok_or_else(|| {
            anyhow::anyhow!("the Gallery is turned off (or Peridot isn't set up yet)")
        })
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
        // Whatever ran before goes away first (its client, its signer and
        // any key it held).
        self.stop_engine().await;
        let cfg = self.config.read().await.clone();
        let store = SyncStore::new(self.db.clone())?;
        let signer: Arc<dyn IdentitySigner> = match &identity.keys {
            Some(keys) => Arc::new(LocalSigner(keys.clone())),
            None => Arc::new(OpalSigner::new(
                self.opal.clone(),
                identity.pubkey(),
                Mode::Background,
            )),
        };
        // Share links are something you ask for, so they may wait on an
        // Opal prompt; the sync loop must never.
        let share_signer: Arc<dyn IdentitySigner> = match &identity.keys {
            Some(_) => signer.clone(),
            None => Arc::new(OpalSigner::new(
                self.opal.clone(),
                identity.pubkey(),
                Mode::Interactive,
            )),
        };
        // Signs relay logins (NIP-42), which private-data relays require.
        let client = Client::builder()
            .authenticator(SignerAuth(signer.clone()))
            .build();
        let previous = Identity::load_previous(&self.secrets).await?;
        let engine = Arc::new(SyncEngine::new(SyncParams {
            signer,
            identity,
            previous,
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
        *self.engine.write().await = Some(engine.clone());
        // Share links sign with the same identity.
        let sharer = Arc::new(crate::share::Sharer::new(
            crate::share::ShareStore::new(self.db.clone())?,
            share_signer.clone(),
            cfg.share.servers.clone(),
            cfg.share.viewer.clone(),
        )?);
        *self.sharer.write().await = Some(sharer.clone());
        crate::share::spawn_sweeper(sharer);
        let handle = crate::runner::spawn(self.clone(), engine, fresh);
        *self.runner.lock().await = Some(handle);
        // The Gallery: public, so it signs interactively too (a like is
        // something you just did).
        if cfg.gallery.enabled {
            let gallery = Arc::new(crate::gallery::Gallery::new(
                crate::gallery::GalleryParams {
                    store: crate::gallery::GalleryStore::new(self.db.clone())?,
                    signer: share_signer,
                    relays: opal_kit::relays::parse_urls(&cfg.gallery.relays),
                    home: self.home.clone(),
                    plugins_url: cfg.gallery.plugins_url.clone(),
                    themes_url: cfg.gallery.themes_url.clone(),
                },
            )?);
            *self.gallery.write().await = Some(gallery.clone());
            *self.gallery_runner.lock().await = Some(crate::gallery::spawn(self.clone(), gallery));
        }
        self.emit_state().await;
        Ok(())
    }

    /// Forget this computer's identity and synced state (the other
    /// computers keep theirs).
    /// Stop the engine, the sharer and the Gallery, and drop them (with
    /// the signer, and so any key it held).
    pub async fn stop_engine(&self) {
        if let Some(r) = self.runner.lock().await.take() {
            r.abort();
        }
        if let Some(engine) = self.engine.write().await.take() {
            engine.client().shutdown().await;
        }
        self.sharer.write().await.take();
        if let Some(r) = self.gallery_runner.lock().await.take() {
            r.abort();
        }
        if let Some(g) = self.gallery.write().await.take() {
            g.client().shutdown().await;
        }
    }

    /// Opal now holds the key this computer held: keep the identity and
    /// the sync secret, forget the key, sign through Opal from now on. The
    /// pairing must already be in place (a crash between the two leaves a
    /// working local identity plus an unused token, never a half state).
    pub async fn switch_to_opal(self: &Arc<Self>, label: Option<String>) -> anyhow::Result<()> {
        let identity = self.engine().await?.identity().clone();
        anyhow::ensure!(!identity.via_opal_mode(), "Opal already holds your key");
        let moved = identity.into_opal_mode();
        moved.save(&self.secrets).await?;
        self.start_engine(moved, false).await?;
        self.db
            .set_kv("peridot.identity_name", label.as_deref().unwrap_or(""))?;
        self.emit_state().await;
        Ok(())
    }

    /// Move every remaining computer to the next epoch, leaving `remove`
    /// behind: mint the secret and announce it (engine), save it here with
    /// the old one kept for the cleanup window, restart with it, publish
    /// the root again and say everything again under the new keys (the
    /// runner does the last two, and retries them).
    pub async fn rotate(self: &Arc<Self>, remove: &[String]) -> anyhow::Result<u64> {
        let engine = self.engine().await?;
        let next = engine.rotate(remove).await?;
        let epoch = next.epoch();
        let old = engine.identity().secret.clone();
        let identity = engine.identity().clone().with_secret(next);
        // Order: the new secret is saved before anything switches, so a
        // crash here restarts into the new epoch and finishes the rest.
        Identity::save_previous(&self.secrets, &old).await?;
        identity.save(&self.secrets).await?;
        self.db.set_kv(
            "peridot.window_until",
            &(Timestamp::now().as_secs() + WINDOW).to_string(),
        )?;
        self.db.set_kv("peridot.republish_pending", "1")?;
        // The root event names the new epoch; a restart before it went
        // out must still publish it.
        self.db.set_kv("peridot.root_pending", "1")?;
        self.start_engine(identity, true).await?;
        Ok(epoch)
    }

    /// Run an engine transition from a task of its own. The runner must
    /// not start or stop engines itself: `stop_engine` aborts the runner's
    /// task, which would be the caller, and the transition would die
    /// half-way with the engine gone and nothing said.
    pub fn handover<F, Fut>(self: &Arc<Self>, what: &'static str, f: F)
    where
        F: FnOnce(Arc<Self>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let app = self.clone();
        tokio::spawn(async move {
            if let Err(e) = f(app.clone()).await {
                tracing::warn!("{what} failed: {e:#}");
                app.set_error(Some(format!("Sync stopped ({what}): {e}")))
                    .await;
                app.emit_state().await;
            }
        });
    }

    /// Another computer rotated and handed us the new secret.
    pub async fn adopt_epoch(
        self: &Arc<Self>,
        secret: peridot_sync::crypto::SyncSecret,
    ) -> anyhow::Result<()> {
        let engine = self.engine().await?;
        if secret.epoch() <= engine.epoch() {
            return Ok(());
        }
        let old = engine.identity().secret.clone();
        let identity = engine.identity().clone().with_secret(secret);
        Identity::save_previous(&self.secrets, &old).await?;
        identity.save(&self.secrets).await?;
        self.db.set_kv(
            "peridot.window_until",
            &(Timestamp::now().as_secs() + WINDOW).to_string(),
        )?;
        self.start_engine(identity, false).await?;
        tracing::info!("moved to epoch {}", self.engine().await?.epoch());
        Ok(())
    }

    /// Another computer removed this one: stop, and say so. The settings
    /// here stay; syncing needs a new pairing.
    pub async fn removed_elsewhere(self: &Arc<Self>) {
        self.stop_engine().await;
        self.set_error(Some(
            "This computer was removed from your Peridot on another computer. Pair it again to keep syncing."
                .into(),
        ))
        .await;
        self.emit_state().await;
    }

    /// The previous epoch's items, once its window has closed: delete
    /// them from the servers and forget that secret. `interactive` lets
    /// a legacy epoch's deletions (signed by the identity) prompt in Opal.
    pub async fn cleanup_previous(self: &Arc<Self>, interactive: bool) -> anyhow::Result<usize> {
        let engine = self.engine().await?;
        if Identity::load_previous(&self.secrets).await?.is_none() {
            return Ok(0);
        }
        let until: u64 = self
            .db
            .get_kv("peridot.window_until")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if Timestamp::now().as_secs() < until {
            return Ok(0);
        }
        let coords = engine.previous_coordinates().await?;
        let signer = if interactive {
            self.sharer().await?.signer.clone()
        } else {
            engine.signer()
        };
        let n = engine.delete_previous(signer.as_ref(), &coords).await?;
        Identity::clear_previous(&self.secrets).await?;
        let _ = self.db.set_kv("peridot.window_until", "0");
        let identity = engine.identity().clone();
        self.start_engine(identity, false).await?;
        Ok(n)
    }

    pub async fn leave(self: &Arc<Self>) -> anyhow::Result<()> {
        self.stop_engine().await;
        Identity::forget(&self.secrets).await?;
        // Opal keeps its side of the pairing until you revoke it there.
        crate::opal::forget_token(&self.secrets).await?;
        self.opal.set_token(None);
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
        // The pairing code is the one secret in here: only `pair.view`
        // (the panel and the command) gets it, never every listener.
        let pairing = self.pairing.lock().await.as_ref().map(|p| {
            let mut v = p.view();
            v.code = None;
            v.qr = None;
            v
        });
        let base = json!({
            "version": env!("CARGO_PKG_VERSION"),
            "device_name": cfg.device_name(),
            "auto_apply": cfg.auto_apply,
            "paused": cfg.paused,
            "relays": cfg.relays,
            "pairing": pairing,
            "approvals": self.approvals.list(),
            "error": *self.last_error.read().await,
        });
        let opal = self.opal.view();
        let Some(engine) = self.engine.read().await.clone() else {
            let mut v = base;
            // The welcome screen offers Opal's accounts when there are any.
            v["opal_accounts"] = json!(self.opal.accounts().await);
            v["set_up"] = json!(false);
            // Setting up with Opal: "approve Peridot in Opal's bar".
            v["opal"] = if opal.waiting_approval {
                json!(opal)
            } else {
                Value::Null
            };
            return v;
        };
        let overview = engine.overview().await;
        let history = engine.store.history(20).unwrap_or_default();
        let mut v = base;
        v["set_up"] = json!(true);
        v["device_id"] = json!(engine.device_id());
        let identity = engine.identity();
        v["identity"] = json!({
            "mode": if identity.via_opal_mode() { "opal" } else { "local" },
            "pubkey": identity.pubkey().to_hex(),
            "npub": identity.pubkey().to_bech32().ok(),
            "name": self.db.get_kv("peridot.identity_name").ok().flatten(),
            "epoch": engine.epoch(),
            "window_until": self.db.get_kv("peridot.window_until").ok().flatten()
                .and_then(|s| s.parse::<u64>().ok()).filter(|t| *t > 0),
            // Opal is installed (its socket exists), so a key held here
            // could move into it.
            "opal_installed": self.opal_socket.exists(),
        });
        v["opal"] = if identity.via_opal_mode() {
            let mut o = json!(opal);
            o["shares_waiting"] =
                json!(self.sharer.read().await.as_ref().map_or(0, |s| s.waiting()));
            o
        } else {
            Value::Null
        };
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
        v["shares"] = json!(
            self.sharer
                .read()
                .await
                .as_ref()
                .and_then(|s| s.store.list().ok())
                .unwrap_or_default()
                .into_iter()
                .filter(|s| !s.revoked)
                .collect::<Vec<_>>()
        );
        v["share_expire_days"] = json!(cfg.share.expire_days);
        v["gallery"] = match self.gallery.read().await.clone() {
            Some(g) => {
                let (themes, plugins) = g.store.item_count();
                let (following, second) = g.wot_size();
                json!({
                    "enabled": true,
                    // Opal mode: the Gallery's kinds haven't been declared
                    // yet; the first use asks Opal once.
                    "needs_enable": identity.via_opal_mode() && !self.gallery_declared(),
                    "themes": themes,
                    "plugins": plugins,
                    "following": following,
                    "trusted": following + second,
                    // null = no profile yet (ask for a name before the first
                    // like); missing = not checked yet.
                    "profile": g.my_profile().await.map(|p| json!(p)),
                    "error": g.error().await,
                })
            }
            None => json!({"enabled": false}),
        };
        v["waiting_to_send"] = json!(overview.waiting_to_send);
        v["servers"] = json!(engine.last_audit().await);
        v["last_sync"] = json!(self.last_sync.load(Ordering::Relaxed));
        v["choices"] = json!(cfg.sync);
        v
    }
}

/// Follows the pairing: re-emits state when it changes, and when Opal
/// stops accepting the token, tries to pair again once (you chose Opal, and
/// its prompt is the designed way to say yes). If that doesn't go through,
/// the panel and `peridot opal pair` take over; no unattended retry loop.
async fn watch_opal(app: std::sync::Weak<App>) {
    loop {
        let Some(app) = app.upgrade() else { break };
        app.opal.changed().notified().await;
        if let Some(pubkey) = app.opal_identity().await
            && app.opal.take_auto_pair()
        {
            if app.pair_opal(Some(pubkey)).await.is_err() {
                crate::notify::opal_pairing_needed();
            }
            continue;
        }
        app.emit_state().await;
    }
}
