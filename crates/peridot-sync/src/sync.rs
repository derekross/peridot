//! The sync engine. It publishes what changed here, takes in what changed
//! on your other computers, and applies it when you say so (every apply
//! backs up what it replaces and can be undone). The daemon drives it:
//! file watching, timers and the relay subscription live there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nostr_sdk::prelude::*;
use opal_kit::relays::Outbox;
use opal_kit::signer::{EventSigner, SIGN_TIMEOUT, sign_within};
use serde::Serialize;
use tokio::sync::RwLock;

use crate::DATA_KIND;
use crate::apply::Home;
use crate::crypto::{SyncKeys, sha256_hex};
use crate::envelope::{self, DeviceInfo, Item, Sealed, Source, StateEntry};
use crate::identity::Identity;
use crate::manifest::{Manifest, Tier};
use crate::scan::{self, Skipped};
use crate::store::{FileStatus, SyncStore, decide};

/// How far back to look again when catching up (clock differences between
/// computers and relays).
const CATCH_UP_MARGIN: u64 = 10 * 60;
/// How long to wait for relays when fetching.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Backups kept for undo.
const KEEP_BACKUPS: usize = 20;

pub struct SyncParams {
    pub identity: Identity,
    pub signer: Arc<dyn EventSigner>,
    pub store: SyncStore,
    pub outbox: Outbox,
    pub home: Home,
    pub manifest: Manifest,
    pub client: Client,
    pub relays: Vec<RelayUrl>,
    /// Where undo backups go (outside the synced folders).
    pub backups_dir: PathBuf,
    pub device_name: String,
    pub version: String,
}

pub struct SyncEngine {
    identity: Identity,
    keys: SyncKeys,
    device: String,
    device_name: String,
    version: String,
    signer: Arc<dyn EventSigner>,
    pub store: SyncStore,
    outbox: Outbox,
    home: Home,
    manifest: RwLock<Manifest>,
    client: Client,
    relays: RwLock<Vec<RelayUrl>>,
    backups_dir: PathBuf,
    last_created: AtomicU64,
}

/// A file as the panel shows it.
#[derive(Debug, Clone, Serialize)]
pub struct FileRow {
    pub path: String,
    pub status: FileStatus,
    pub tier: Option<Tier>,
    /// Which computer the incoming version is from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The incoming change is a deletion.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// Can run commands (ask tier): always shown before applying.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub runs_commands: bool,
}

/// Something from another computer that needs a command run here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Offer {
    Theme {
        name: String,
        from: String,
    },
    InstallTheme {
        name: String,
        url: String,
        from: String,
    },
    InstallPlugin {
        name: String,
        url: String,
        from: String,
    },
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Overview {
    pub files: Vec<FileRow>,
    pub skipped: Vec<Skipped>,
    pub offers: Vec<Offer>,
    pub devices: Vec<DeviceInfo>,
    pub waiting_to_send: usize,
}

impl Overview {
    pub fn count(&self, status: FileStatus) -> usize {
        self.files.iter().filter(|f| f.status == status).count()
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PublishReport {
    pub published: Vec<String>,
    pub deleted: Vec<String>,
    pub skipped: Vec<Skipped>,
    /// Events that went out now (the rest wait in the outbox).
    pub sent: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApplyReport {
    pub applied: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub history_id: Option<i64>,
}

impl SyncEngine {
    pub fn new(p: SyncParams) -> anyhow::Result<Self> {
        let keys = p.identity.secret.keys();
        let device = p.store.device_id()?;
        Ok(Self {
            keys,
            device,
            device_name: p.device_name,
            version: p.version,
            identity: p.identity,
            signer: p.signer,
            store: p.store,
            outbox: p.outbox,
            home: p.home,
            manifest: RwLock::new(p.manifest),
            client: p.client,
            relays: RwLock::new(p.relays),
            backups_dir: p.backups_dir,
            last_created: AtomicU64::new(0),
        })
    }

    pub fn device_id(&self) -> &str {
        &self.device
    }

    pub fn pubkey(&self) -> PublicKey {
        self.identity.pubkey()
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn relays(&self) -> Vec<RelayUrl> {
        self.relays.read().await.clone()
    }

    pub async fn set_manifest(&self, manifest: Manifest) {
        *self.manifest.write().await = manifest;
    }

    pub async fn choices(&self) -> crate::manifest::Choices {
        self.manifest.read().await.choices().clone()
    }

    /// Add our relays to the client and connect.
    pub async fn connect(&self) {
        for r in self.relays.read().await.iter() {
            let _ = self.client.add_relay(r).await;
        }
        self.client
            .connect()
            .and_wait(opal_kit::relays::CONNECT_WAIT)
            .await;
    }

    /// The filter for everything of ours from `since`.
    pub fn filter(&self, since: u64) -> Filter {
        Filter::new()
            .author(self.pubkey())
            .kind(Kind::Custom(DATA_KIND))
            .since(Timestamp::from(since))
    }

    /// Fetch what changed since we last looked and take it in. Returns how
    /// many items were new, or an error if no server could be reached.
    pub async fn catch_up(&self) -> anyhow::Result<usize> {
        let connected = self
            .client
            .relays()
            .await
            .values()
            .any(|r| r.status().is_connected());
        if !connected {
            anyhow::bail!("can't reach your sync servers");
        }
        let since = self.store.since().saturating_sub(CATCH_UP_MARGIN);
        let targets: Vec<(RelayUrl, Vec<Filter>)> = self
            .relays
            .read()
            .await
            .iter()
            .map(|r| (r.clone(), vec![self.filter(since)]))
            .collect();
        match self
            .client
            .fetch_events(targets)
            .timeout(FETCH_TIMEOUT)
            .await
        {
            Ok(events) => {
                let new = events.iter().filter(|e| self.ingest(e)).count();
                self.mark_caught_up();
                Ok(new)
            }
            Err(e) => Err(anyhow::anyhow!("catching up failed: {e}")),
        }
    }

    /// Whether this computer has heard from the servers at least once with
    /// this identity. Until then it publishes nothing: a newly paired
    /// computer must never push its defaults over your real settings just
    /// because it couldn't see them yet.
    pub fn caught_up(&self) -> bool {
        self.store
            .db()
            .get_kv("peridot.caught_up")
            .ok()
            .flatten()
            .as_deref()
            == Some(self.pubkey().to_hex().as_str())
    }

    /// For a brand-new identity there's nothing to catch up on.
    pub fn mark_caught_up(&self) {
        let _ = self
            .store
            .db()
            .set_kv("peridot.caught_up", &self.pubkey().to_hex());
    }

    /// Take in one event from a relay. Returns whether it told us something
    /// new.
    pub fn ingest(&self, ev: &Event) -> bool {
        if ev.pubkey != self.pubkey() || ev.kind != Kind::Custom(DATA_KIND) || ev.verify().is_err()
        {
            return false;
        }
        let Some(d) = ev.tags.identifier() else {
            return false;
        };
        let Some(item) = envelope::open(&self.keys, &d, &ev.content) else {
            return false;
        };
        let at = ev.created_at.as_secs();
        let _ = self.store.set_since(at);
        let new = match &item {
            Item::File(f) => {
                // Only paths the manifest could ever sync are kept; a
                // never-tier path can't be smuggled in by any device.
                if !matches!(
                    Manifest::new(Default::default()).tier(&f.path),
                    Some(Tier::Shared | Tier::Ask)
                ) {
                    tracing::warn!("ignoring a synced file outside what Peridot syncs");
                    return false;
                }
                self.store
                    .put_remote(f, at, &ev.id.to_hex())
                    .unwrap_or(false)
            }
            Item::Chunk(c) => {
                let _ = self.store.put_chunk(c, at);
                false
            }
            Item::Device(d) => {
                let _ = self.store.put_device(d, at);
                false
            }
            Item::State(s) => self
                .store
                .put_state(state_kind(s), s, at, &ev.id.to_hex())
                .unwrap_or(false),
        };
        if new {
            let _ = self.store.prune_chunks();
        }
        new
    }

    /// Every covered file and where it stands, plus offers and devices.
    pub async fn overview(&self) -> Overview {
        let manifest = self.manifest.read().await;
        let mut paths: Vec<String> = scan::syncable_paths(&self.home, &manifest);
        for p in self.store.remote_paths().unwrap_or_default() {
            if manifest.syncs(&p) && !paths.contains(&p) {
                paths.push(p);
            }
        }
        paths.sort();
        let devices = self.store.devices().unwrap_or_default();
        let name_of = |id: &str| {
            devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| "another computer".into())
        };
        let mut files = Vec::new();
        let mut skipped = Vec::new();
        for path in paths {
            let local = match scan::read_checked(&self.home, &path) {
                Ok(c) => c.map(|c| sha256_hex(&c)),
                Err(s) => {
                    skipped.push(s);
                    continue;
                }
            };
            let synced = self.store.synced(&path).ok().flatten();
            let remote = self.store.remote(&path).ok().flatten();
            let status =
                self.status_of(&path, local.as_deref(), synced.as_deref(), remote.as_ref());
            let tier = manifest.tier(&path);
            files.push(FileRow {
                from: matches!(
                    status,
                    FileStatus::Incoming | FileStatus::Conflict | FileStatus::Kept
                )
                .then(|| remote.as_ref().map(|r| name_of(&r.entry.device)))
                .flatten(),
                deleted: remote.as_ref().is_some_and(|r| r.entry.deleted),
                runs_commands: tier == Some(Tier::Ask),
                path,
                status,
                tier,
            });
        }
        Overview {
            files,
            skipped,
            offers: self.offers(&name_of),
            devices,
            waiting_to_send: self.outbox.len().unwrap_or(0),
        }
    }

    /// Publish everything that changed here (and deletions of files that
    /// synced before), then send what we can.
    pub async fn publish_changes(&self) -> anyhow::Result<PublishReport> {
        let mut report = PublishReport::default();
        if !self.caught_up() {
            return Ok(report);
        }
        let manifest = self.manifest.read().await;
        let mut paths = scan::syncable_paths(&self.home, &manifest);
        for p in self.store.remote_paths()? {
            if manifest.syncs(&p) && !paths.contains(&p) {
                paths.push(p);
            }
        }
        drop(manifest);
        for path in paths {
            let local = match scan::read_checked(&self.home, &path) {
                Ok(c) => c,
                Err(s) => {
                    report.skipped.push(s);
                    continue;
                }
            };
            let sha = local.as_deref().map(sha256_hex);
            let synced = self.store.synced(&path)?;
            let remote = self.store.remote(&path)?;
            if self.status_of(&path, sha.as_deref(), synced.as_deref(), remote.as_ref())
                != FileStatus::Outgoing
            {
                continue;
            }
            // Built on top of whatever version we last had in common, and
            // dated after it so it replaces it everywhere.
            let after = remote.as_ref().map(|r| r.created_at).unwrap_or(0);
            let base = remote
                .as_ref()
                .and_then(|r| r.sha().map(String::from))
                .or(synced);
            let sealed = match &local {
                Some(content) => {
                    envelope::pack_file(&self.keys, &path, content, base, &self.device)?
                }
                None => vec![envelope::pack_deletion(
                    &self.keys,
                    &path,
                    base,
                    &self.device,
                )?],
            };
            // Also taken in here, so status is right before it echoes back.
            self.queue(&sealed, after).await?;
            self.store.set_synced(&path, sha.as_deref(), now())?;
            if local.is_some() {
                report.published.push(path);
            } else {
                report.deleted.push(path);
            }
        }
        report.sent = self.flush().await;
        Ok(report)
    }

    /// Publish this computer's device entry and Omarchy state.
    pub async fn announce(&self) -> anyhow::Result<()> {
        let device = DeviceInfo {
            id: self.device.clone(),
            name: self.device_name.clone(),
            version: self.version.clone(),
            last_seen: now(),
            removed: false,
        };
        self.queue(&[envelope::seal(&self.keys, &Item::Device(device))?], 0)
            .await?;
        // Like files, never before we've seen what's already there.
        let states = if self.caught_up() {
            local_state(self.home.path(), &self.device)
        } else {
            Vec::new()
        };
        for s in states {
            let kind = state_kind(&s);
            let value = state_value(&s);
            let agreed = self.store.state_synced(kind);
            let remote = self.store.state(kind)?;
            // Like files: publish only what changed here since we last
            // agreed (or when nobody has published it yet).
            let changed_here = agreed.as_deref() != Some(value.as_str());
            if remote.is_none() || (changed_here && agreed.is_some()) {
                let after = self.store.state_created_at(kind);
                self.queue(&[envelope::seal(&self.keys, &Item::State(s))?], after)
                    .await?;
                self.store.set_state_synced(kind, &value)?;
            } else if agreed.is_none() && remote.as_ref().map(state_value) == Some(value.clone()) {
                self.store.set_state_synced(kind, &value)?;
            }
        }
        self.flush().await;
        Ok(())
    }

    /// Publish the root event (the sync secret, encrypted to our own key)
    /// so a recovery kit can restore everything.
    pub async fn publish_root(&self) -> anyhow::Result<()> {
        self.outbox.push(&self.identity.root_event()?)?;
        self.flush().await;
        Ok(())
    }

    /// Mark a device as removed (it stops showing; its own copy of your
    /// settings can't be wiped from here).
    pub async fn remove_device(&self, id: &str) -> anyhow::Result<()> {
        let mut info = self
            .store
            .devices()?
            .into_iter()
            .find(|d| d.id == id)
            .ok_or_else(|| anyhow::anyhow!("no such computer"))?;
        info.removed = true;
        info.last_seen = now();
        let sealed = envelope::seal(&self.keys, &Item::Device(info))?;
        self.queue(&[sealed], 0).await?;
        self.flush().await;
        Ok(())
    }

    /// Apply incoming changes to `paths` (all incoming ones if empty).
    /// Conflicts are only applied when named explicitly ("use theirs").
    pub async fn apply(&self, paths: &[String]) -> anyhow::Result<ApplyReport> {
        let overview = self.overview().await;
        let chosen: Vec<&FileRow> = overview
            .files
            .iter()
            .filter(|f| {
                if paths.is_empty() {
                    f.status == FileStatus::Incoming
                } else {
                    paths.contains(&f.path)
                        && matches!(
                            f.status,
                            FileStatus::Incoming | FileStatus::Conflict | FileStatus::Kept
                        )
                }
            })
            .collect();
        let mut report = ApplyReport::default();
        if chosen.is_empty() {
            return Ok(report);
        }
        let stamp = now();
        let backup = self.backups_dir.join(stamp.to_string());
        let mut backed_up = Vec::new();
        let mut from = Vec::new();
        for row in chosen {
            let path = &row.path;
            let result: anyhow::Result<()> = async {
                let remote = self
                    .store
                    .remote(path)?
                    .ok_or_else(|| anyhow::anyhow!("nothing to apply"))?;
                // Back up what's here first.
                if let Some(current) = self.home.read(path)? {
                    let dest = backup.join(path);
                    std::fs::create_dir_all(dest.parent().expect("has a parent"))?;
                    std::fs::write(&dest, current)?;
                    backed_up.push(path.clone());
                }
                if remote.entry.deleted {
                    self.home.remove(path)?;
                    self.store.set_synced(path, None, stamp)?;
                } else {
                    let content = envelope::assemble(&remote.entry, |sha| self.store.chunk(sha))?;
                    self.home.write(path, &content)?;
                    self.store
                        .set_synced(path, Some(&remote.entry.sha256), stamp)?;
                }
                self.store.unkeep(path)?;
                if let Some(f) = &row.from
                    && !from.contains(f)
                {
                    from.push(f.clone());
                }
                Ok(())
            }
            .await;
            match result {
                Ok(()) => report.applied.push(path.clone()),
                Err(e) => report.failed.push((path.clone(), e.to_string())),
            }
        }
        if !report.applied.is_empty() {
            let n = report.applied.len();
            let summary = format!(
                "Applied {n} setting{} from {}",
                if n == 1 { "" } else { "s" },
                if from.is_empty() {
                    "another computer".into()
                } else {
                    from.join(", ")
                }
            );
            let dir = (!backed_up.is_empty()).then(|| backup.to_string_lossy().into_owned());
            report.history_id =
                Some(
                    self.store
                        .add_history(stamp, &summary, &report.applied, dir.as_deref())?,
                );
            self.prune_backups();
        }
        Ok(report)
    }

    /// Resolve a conflict by keeping this computer's version (it becomes
    /// the new version everywhere).
    pub async fn keep_local(&self, path: &str) -> anyhow::Result<()> {
        let remote = self
            .store
            .remote(path)?
            .ok_or_else(|| anyhow::anyhow!("no conflict here"))?;
        // Pretend we'd seen theirs, so ours now counts as the newer change.
        self.store.set_synced(path, remote.sha(), now())?;
        self.publish_changes().await?;
        Ok(())
    }

    /// Put back what an apply replaced. Those files then count as changed
    /// here, so your other computers are offered this version.
    pub async fn undo(&self, history_id: i64) -> anyhow::Result<Vec<String>> {
        let entry = self
            .store
            .history(100)?
            .into_iter()
            .find(|h| h.id == history_id)
            .ok_or_else(|| anyhow::anyhow!("nothing to undo"))?;
        anyhow::ensure!(!entry.undone, "already undone");
        let dir = entry.backup_dir.map(PathBuf::from);
        let mut restored = Vec::new();
        for path in &entry.paths {
            let content = match dir.as_ref().map(|d| d.join(path)).filter(|p| p.exists()) {
                Some(saved) => {
                    let content = std::fs::read(saved)?;
                    self.home.write(path, &content)?;
                    Some(content)
                }
                // It didn't exist before the apply.
                None => {
                    self.home.remove(path)?;
                    None
                }
            };
            // Keep this version here without pushing it to your other
            // computers (they keep theirs), until either side changes it.
            let sha = content.as_deref().map(sha256_hex);
            self.store.set_synced(path, sha.as_deref(), now())?;
            if let Some(remote) = self.store.remote(path)? {
                self.store.keep(path, remote.sha().unwrap_or(""))?;
            }
            restored.push(path.clone());
        }
        self.store.mark_undone(history_id)?;
        Ok(restored)
    }

    /// [`decide`], plus versions you chose to keep here after an undo.
    fn status_of(
        &self,
        path: &str,
        local: Option<&str>,
        synced: Option<&str>,
        remote: Option<&crate::store::Remote>,
    ) -> FileStatus {
        let status = decide(local, synced, remote, &self.device);
        if status == FileStatus::Incoming
            && let Some(remote) = remote
            && self.store.kept(path).as_deref() == Some(remote.sha().unwrap_or(""))
        {
            return FileStatus::Kept;
        }
        status
    }

    /// Send what's waiting in the outbox.
    pub async fn flush(&self) -> usize {
        let relays = self.relays.read().await.clone();
        self.outbox.flush(&self.client, &relays).await
    }

    /// Sign sealed items (dated after `after`), take them in ourselves and
    /// queue them.
    async fn queue(&self, sealed: &[Sealed], after: u64) -> anyhow::Result<()> {
        for s in sealed {
            let unsigned = EventBuilder::new(Kind::Custom(DATA_KIND), s.content.clone())
                .tag(Tag::identifier(s.d.clone()))
                .custom_created_at(Timestamp::from(self.next_created_at(after)))
                .finalize_unsigned(self.pubkey());
            let ev = sign_within(self.signer.as_ref(), unsigned, SIGN_TIMEOUT).await?;
            self.ingest(&ev);
            self.outbox.push(&ev)?;
        }
        Ok(())
    }

    /// Strictly increasing timestamps, and always after the version being
    /// replaced (which another computer may have dated ahead of our clock):
    /// relays keep the newest per `d` tag.
    fn next_created_at(&self, after: u64) -> u64 {
        let now = now().max(after + 1);
        let mut prev = self.last_created.load(Ordering::SeqCst);
        loop {
            let next = now.max(prev + 1);
            match self
                .last_created
                .compare_exchange(prev, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return next,
                Err(p) => prev = p,
            }
        }
    }

    /// "Not now" for an offer: it isn't shown again for that same value.
    pub fn dismiss(&self, offer: &Offer) -> anyhow::Result<()> {
        let mut dismissed = self.dismissed();
        dismissed.insert(offer_key(offer));
        self.store
            .db()
            .set_kv("peridot.dismissed", &serde_json::to_string(&dismissed)?)?;
        Ok(())
    }

    /// The theme from another computer was applied here: that's now the
    /// agreed value, not a change of ours.
    pub fn theme_applied(&self, name: &str) -> anyhow::Result<()> {
        self.store.set_state_synced("theme", name)?;
        Ok(())
    }

    fn dismissed(&self) -> std::collections::BTreeSet<String> {
        self.store
            .db()
            .get_kv("peridot.dismissed")
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default()
    }

    /// Folders to watch for changes (relative to home): the manifest's,
    /// plus where Omarchy keeps the current theme, themes and plugins.
    pub async fn watch_targets(&self) -> Vec<crate::manifest::Target> {
        use crate::manifest::Target;
        let mut t = self.manifest.read().await.targets();
        t.push(Target::Dir(".local/state/omarchy/current".into()));
        t.push(Target::Dir(".config/omarchy/themes".into()));
        t.push(Target::Dir(".config/omarchy/plugins".into()));
        t
    }

    pub fn home_path(&self) -> &std::path::Path {
        self.home.path()
    }

    fn offers(&self, name_of: &dyn Fn(&str) -> String) -> Vec<Offer> {
        let dismissed = self.dismissed();
        let mut offers = self.all_offers(name_of);
        offers.retain(|o| !dismissed.contains(&offer_key(o)));
        offers
    }

    fn all_offers(&self, name_of: &dyn Fn(&str) -> String) -> Vec<Offer> {
        let mut offers = Vec::new();
        let here = local_state(self.home.path(), &self.device);
        let local_theme = here.iter().find_map(|s| match s {
            StateEntry::Theme { name, .. } => Some(name.clone()),
            _ => None,
        });
        let installed = |kind: &str| -> Vec<String> {
            here.iter()
                .filter_map(|s| match (kind, s) {
                    ("themes", StateEntry::Themes { themes, .. }) => Some(themes),
                    ("plugins", StateEntry::Plugins { plugins, .. }) => Some(plugins),
                    _ => None,
                })
                .flatten()
                .map(|s| s.name.clone())
                .collect()
        };
        if let Ok(Some(StateEntry::Theme { name, device })) = self.store.state("theme")
            && device != self.device
            && Some(&name) != local_theme.as_ref()
            && valid_name(&name)
        {
            offers.push(Offer::Theme {
                name,
                from: name_of(&device),
            });
        }
        if let Ok(Some(StateEntry::Themes { themes, device })) = self.store.state("themes")
            && device != self.device
        {
            let have = installed("themes");
            for t in themes.into_iter().filter(|t| !have.contains(&t.name)) {
                if valid_name(&t.name) && valid_source(&t.url) {
                    offers.push(Offer::InstallTheme {
                        name: t.name,
                        url: t.url,
                        from: name_of(&device),
                    });
                }
            }
        }
        if let Ok(Some(StateEntry::Plugins { plugins, device })) = self.store.state("plugins")
            && device != self.device
        {
            let have = installed("plugins");
            for p in plugins.into_iter().filter(|p| !have.contains(&p.name)) {
                if valid_plugin_id(&p.name) && valid_source(&p.url) {
                    offers.push(Offer::InstallPlugin {
                        name: p.name,
                        url: p.url,
                        from: name_of(&device),
                    });
                }
            }
        }
        offers
    }

    fn prune_backups(&self) {
        let Ok(entries) = std::fs::read_dir(&self.backups_dir) else {
            return;
        };
        let mut dirs: Vec<(u64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| Some((e.file_name().to_str()?.parse().ok()?, e.path())))
            .collect();
        dirs.sort();
        if dirs.len() > KEEP_BACKUPS {
            for (_, d) in &dirs[..dirs.len() - KEEP_BACKUPS] {
                let _ = std::fs::remove_dir_all(d);
            }
        }
    }
}

fn offer_key(o: &Offer) -> String {
    match o {
        Offer::Theme { name, .. } => format!("theme:{name}"),
        Offer::InstallTheme { url, .. } => format!("install-theme:{url}"),
        Offer::InstallPlugin { url, .. } => format!("install-plugin:{url}"),
    }
}

fn state_kind(s: &StateEntry) -> &'static str {
    match s {
        StateEntry::Theme { .. } => "theme",
        StateEntry::Themes { .. } => "themes",
        StateEntry::Plugins { .. } => "plugins",
    }
}

/// A state entry's value without who said it, for "did it change?".
fn state_value(s: &StateEntry) -> String {
    match s {
        StateEntry::Theme { name, .. } => name.clone(),
        StateEntry::Themes { themes, .. } => serde_json::to_string(themes).unwrap_or_default(),
        StateEntry::Plugins { plugins, .. } => serde_json::to_string(plugins).unwrap_or_default(),
    }
}

/// This computer's Omarchy state: current theme, and themes and plugins
/// installed from git (with a public https source).
pub fn local_state(home: &std::path::Path, device: &str) -> Vec<StateEntry> {
    let mut out = Vec::new();
    if let Ok(name) = std::fs::read_to_string(home.join(".local/state/omarchy/current/theme.name"))
    {
        let name = name.trim().to_string();
        if valid_name(&name) {
            out.push(StateEntry::Theme {
                name,
                device: device.into(),
            });
        }
    }
    out.push(StateEntry::Themes {
        themes: git_sources(&home.join(".config/omarchy/themes")),
        device: device.into(),
    });
    out.push(StateEntry::Plugins {
        plugins: git_sources(&home.join(".config/omarchy/plugins")),
        device: device.into(),
    });
    out
}

/// `name → https clone URL` for each git checkout in `dir` (links, e.g.
/// plugins in development, are skipped).
fn git_sources(dir: &std::path::Path) -> Vec<Source> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    for e in entries.flatten() {
        if e.file_type()
            .map(|t| t.is_symlink() || !t.is_dir())
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        let Ok(config) = std::fs::read_to_string(e.path().join(".git/config")) else {
            continue;
        };
        if let Some(url) = origin_url(&config).and_then(|u| https_source(&u)) {
            out.insert(name.clone(), url);
        }
    }
    out.into_iter()
        .map(|(name, url)| Source { name, url })
        .collect()
}

/// The `origin` remote's URL from a `.git/config`.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
        } else if in_origin && let Some(url) = line.strip_prefix("url") {
            return Some(url.trim_start().trim_start_matches('=').trim().to_string());
        }
    }
    None
}

/// Public https form of a clone URL (GitHub/GitLab/Codeberg SSH URLs are
/// converted); None for anything else.
pub fn https_source(url: &str) -> Option<String> {
    let url = url.trim();
    let https = if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        if !["github.com", "gitlab.com", "codeberg.org"].contains(&host) {
            return None;
        }
        format!("https://{host}/{path}")
    } else {
        url.to_string()
    };
    valid_source(&https).then_some(https)
}

/// Only plain public https git URLs are ever offered for install.
pub fn valid_source(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() < 300
        && !url.contains('@')
        && url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._~/:%+".contains(c))
}

/// Theme names as Omarchy stores them (`tokyo-night`).
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Plugin ids (`derekross.calendar`).
pub fn valid_plugin_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 100
        && !id.starts_with(['.', '-'])
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn now() -> u64 {
    Timestamp::now().as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_git_origins_and_only_offers_https() {
        let cfg = "[core]\n\tbare = false\n[remote \"origin\"]\n\turl = git@github.com:derekross/omarchy-calendar.git\n\tfetch = +refs/heads/*\n";
        assert_eq!(
            origin_url(cfg).and_then(|u| https_source(&u)).as_deref(),
            Some("https://github.com/derekross/omarchy-calendar.git")
        );
        assert_eq!(https_source("git@evil.example:x/y.git"), None);
        assert_eq!(https_source("http://github.com/x/y"), None);
        assert_eq!(https_source("https://user:pw@github.com/x/y"), None);
        assert_eq!(https_source("https://github.com/x/y; rm -rf ~"), None);
        assert!(valid_source(
            "https://github.com/ax1g/quickshell-cpu-ram-usage.git"
        ));
        assert!(valid_name("tokyo-night") && !valid_name("x; rm") && !valid_name("../x"));
        assert!(
            valid_plugin_id("derekross.calendar")
                && !valid_plugin_id("../x")
                && !valid_plugin_id("-x")
        );
    }
}
