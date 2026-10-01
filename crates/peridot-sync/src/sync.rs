//! The sync engine. It publishes what changed here, takes in what changed
//! on your other computers, and applies it when you say so (every apply
//! backs up what it replaces and can be undone). The daemon drives it:
//! file watching, timers and the relay subscription live there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nostr_sdk::prelude::*;
use opal_kit::relays::Outbox;
use opal_kit::signer::sign_within;
use serde::Serialize;
use tokio::sync::RwLock;

use crate::DATA_KIND;
use crate::apply::Home;
use crate::crypto::{SyncKeys, SyncSecret, sha256_hex};
use crate::envelope::{self, DeviceInfo, Item, Sealed, Source, StateEntry};
use crate::identity::Identity;
use crate::manifest::{Manifest, Tier};
use crate::rotation::{self, Rotation};
use crate::scan::{self, Skipped};
use crate::store::{FileStatus, SyncStore, decide};

/// How far back to look again when catching up: items are dated to the
/// hour (plus clock differences between computers and relays).
pub const CATCH_UP_MARGIN: u64 = 2 * 3600 + 10 * 60;
/// How long to wait for relays when fetching.
const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Backups kept for undo.
const KEEP_BACKUPS: usize = 20;
/// Items older than this on the servers are published again, so servers
/// that let old events lapse keep yours.
pub const REFRESH_AFTER: u64 = 30 * 86_400;
/// Chunks nothing refers to any more are removed from the servers once
/// they're this old (a computer may still be sending the entry that
/// refers to them).
pub const CHUNK_GRACE: u64 = 7 * 86_400;
/// At most this many chunks in one deletion request.
const DELETE_BATCH: usize = 100;
/// Events dated further ahead than this are refused: a far-future date
/// would sit on top of every later change (and push the catch-up point
/// past them).
pub const MAX_CLOCK_AHEAD: u64 = 600;

pub struct SyncParams {
    pub identity: Identity,
    /// The secret of the epoch before the current one, while its items
    /// are still being cleaned up (a rotation's window).
    pub previous: Option<SyncSecret>,
    pub signer: Arc<dyn crate::signer::IdentitySigner>,
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
    /// The previous epoch's keys during a rotation's window: its items are
    /// still read (and re-sealed under the current keys) until cleanup.
    previous: Option<(SyncSecret, SyncKeys)>,
    /// A rotation another computer announced, waiting for the daemon to
    /// switch this engine over (or the news that we were removed).
    pending_rotation: std::sync::Mutex<Option<Rotation>>,
    /// Set for good once a rotation from this epoch was heard: the engine
    /// is replaced when this computer switches, so no rotation is started
    /// from an epoch that has already moved on.
    rotation_heard: AtomicBool,
    device: String,
    device_name: String,
    version: String,
    signer: Arc<dyn crate::signer::IdentitySigner>,
    pub store: SyncStore,
    outbox: Outbox,
    home: Home,
    manifest: RwLock<Manifest>,
    client: Client,
    relays: RwLock<Vec<RelayUrl>>,
    backups_dir: PathBuf,
    last_created: AtomicU64,
    last_audit: RwLock<Option<AuditReport>>,
}

/// One server's copy of your settings, as of the last audit.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RelayHealth {
    pub url: String,
    pub reachable: bool,
    /// Current items it holds (of `expected`).
    pub items: usize,
    /// Current items it lacked (sent again).
    pub missing: usize,
}

/// What the daily check of the servers found and did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditReport {
    pub at: u64,
    pub relays: Vec<RelayHealth>,
    /// Current items: files, their chunks, state, computers, the root.
    pub expected: usize,
    /// Items published again because a server lacked them.
    pub resent: usize,
    /// Items published again because they were getting old everywhere.
    pub refreshed: usize,
    /// Chunk events nothing refers to any more, still on the servers.
    pub stale_chunks: usize,
    /// Of those, asked to be removed this time.
    pub removed_chunks: usize,
    /// The stale chunks' coordinates, for [`SyncEngine::remove_chunks`].
    #[serde(skip)]
    pub stale: Vec<String>,
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
    /// Something here runs what's in it (a shell, Hyprland, an editor…):
    /// always shown before applying, never applied automatically.
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
        let previous = p.previous.map(|s| {
            let k = s.keys();
            (s, k)
        });
        Ok(Self {
            keys,
            previous,
            pending_rotation: std::sync::Mutex::new(None),
            rotation_heard: AtomicBool::new(false),
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
            last_audit: RwLock::new(None),
        })
    }

    pub async fn last_audit(&self) -> Option<AuditReport> {
        self.last_audit.read().await.clone()
    }

    /// When the servers were last audited (0 = never).
    pub fn audited_at(&self) -> u64 {
        self.store
            .db()
            .get_kv("peridot.audited_at")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Everything that should be on every server right now, with the
    /// newest `created_at` we know for it. Device entries only from
    /// `device_epochs` (see [`SyncStore::devices_of`]).
    fn expected_items(&self, device_epochs: &[u64]) -> anyhow::Result<Vec<(Sealed, Item, u64)>> {
        let mut out = Vec::new();
        let mut chunks: BTreeMap<String, u64> = BTreeMap::new();
        for path in self.store.remote_paths()? {
            let Some(r) = self.store.remote(&path)? else {
                continue;
            };
            if !r.entry.deleted {
                for c in &r.entry.chunks {
                    chunks.insert(c.clone(), r.created_at);
                }
            }
            let item = Item::File(r.entry);
            out.push((envelope::seal(&self.keys, &item)?, item, r.created_at));
        }
        for (sha, at) in chunks {
            // A chunk we never received can't be sent again; the entry
            // that needs it will show as unreadable until it turns up.
            if let Some(c) = self.store.chunk(&sha) {
                let item = Item::Chunk(c);
                out.push((envelope::seal(&self.keys, &item)?, item, at));
            }
        }
        for kind in ["theme", "themes", "plugins"] {
            if let Some(s) = self.store.state(kind)? {
                let at = self.store.state_created_at(kind);
                let item = Item::State(s);
                out.push((envelope::seal(&self.keys, &item)?, item, at));
            }
        }
        for d in self.store.devices_of(device_epochs)? {
            // Ours goes out daily anyway; the others' only need to exist.
            let item = Item::Device(d);
            out.push((envelope::seal(&self.keys, &item)?, item, u64::MAX));
        }
        Ok(out)
    }

    /// Check every server for everything of ours: send again what any of
    /// them lacks, publish again what is getting old everywhere (so
    /// servers that let old events lapse keep it), and find chunk events
    /// nothing refers to any more. `now` is the clock (a parameter so tests
    /// can move it). Chunks are only found here; [`Self::remove_chunks`]
    /// asks the servers to drop them, because that needs a signature you
    /// may want to be asked about.
    pub async fn audit(&self, now: u64) -> anyhow::Result<AuditReport> {
        let relays = self.relays.read().await.clone();
        let expected = self.expected_items(&[self.epoch()])?;
        let root_d = Identity::root_name(&self.pubkey());
        let mut report = AuditReport {
            at: now,
            expected: expected.len() + 1,
            ..Default::default()
        };
        // d → newest created_at seen anywhere, and which relays have it.
        let mut newest: BTreeMap<String, u64> = BTreeMap::new();
        let mut holders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        // Events under a d we don't expect: candidates for removal.
        let mut unknown: BTreeMap<String, (Event, u64)> = BTreeMap::new();
        let expected_ds: BTreeSet<&str> = expected.iter().map(|(s, ..)| s.d.as_str()).collect();
        let mut reachable = 0;
        // Relays where the root could be looked for at all (see below).
        let mut root_checked = 0;
        for relay in &relays {
            let mut health = RelayHealth {
                url: relay.to_string(),
                ..Default::default()
            };
            // Relay by relay, and the items apart from the root: a relay
            // that serves private data only to its authenticated author
            // (relay.ditto.pub does) answers a request that mixes the
            // epoch's key and the identity with a refusal, and a refusal
            // has to be seen, which the pooled fetch doesn't show.
            let Ok(Some(conn)) = self.client.relay(relay).await else {
                report.relays.push(health);
                continue;
            };
            // This epoch's own items first; without them the relay is
            // as good as unreachable. The others (the previous epoch's,
            // the rekey address) may be refused and don't count.
            // All at once, so a relay that answers nothing costs one
            // timeout, not one per request.
            let root_filter = Filter::new()
                .author(self.pubkey())
                .kind(Kind::Custom(DATA_KIND))
                .identifier(root_d.clone());
            let mut requests = self.filters(0);
            requests.push(root_filter);
            let answers = futures::future::join_all(requests.into_iter().map(|f| {
                let conn = conn.clone();
                async move { conn.fetch_events(f).timeout(FETCH_TIMEOUT).await }
            }))
            .await;
            let mut answers = answers.into_iter();
            let own = answers.next().expect("at least this epoch");
            let root_answer = answers.next_back().expect("the root");
            let Ok(mut events) = own else {
                report.relays.push(health);
                continue;
            };
            for more in answers.flatten() {
                events.extend(more);
            }
            health.reachable = true;
            reachable += 1;
            // The root is the identity's; a relay that only serves what
            // the logged-in key wrote can't show it to the epoch's key, and
            // an empty answer looks the same as none. So the root is not
            // counted against any one relay (below): it is published again
            // when no relay shows it at all.
            match root_answer {
                Ok(roots) => {
                    root_checked += 1;
                    events.extend(roots);
                }
                Err(e) => tracing::debug!("{relay}: the root can't be looked for here: {e}"),
            }
            let mut held = BTreeSet::new();
            for ev in events.iter() {
                let Some(d) = ev.tags.identifier() else {
                    continue;
                };
                // Only this epoch's items (the root is the identity's).
                let root = d == root_d && ev.pubkey == self.pubkey();
                if (ev.pubkey != self.sync_pubkey() && !root) || ev.verify().is_err() {
                    continue;
                }
                let at = ev.created_at.as_secs();
                let e = newest.entry(d.clone()).or_insert(0);
                *e = (*e).max(at);
                holders
                    .entry(d.clone())
                    .or_default()
                    .insert(relay.to_string());
                if expected_ds.contains(d.as_str()) || d == root_d {
                    held.insert(d.clone());
                } else {
                    let u = unknown.entry(d.clone()).or_insert((ev.clone(), 0));
                    u.1 = u.1.max(at);
                }
                // A relay that was down during a catch-up may hold news.
                self.ingest(ev);
            }
            health.items = held.len();
            health.missing = expected
                .iter()
                .filter(|(s, ..)| !held.contains(&s.d))
                .count();
            report.relays.push(health);
        }
        if reachable == 0 {
            anyhow::bail!("none of your sync servers could be reached");
        }

        // Send again what a server lacks; publish again what's getting old.
        let mut again: Vec<(Sealed, u64)> = Vec::new();
        for (sealed, item, known_at) in &expected {
            let on = holders.get(&sealed.d).map(BTreeSet::len).unwrap_or(0);
            let seen_at = newest
                .get(&sealed.d)
                .copied()
                .unwrap_or(0)
                .max(if *known_at == u64::MAX { 0 } else { *known_at });
            if on < reachable {
                again.push((envelope::seal(&self.keys, item)?, seen_at));
                report.resent += 1;
            } else if *known_at != u64::MAX && now.saturating_sub(seen_at) > REFRESH_AFTER {
                again.push((envelope::seal(&self.keys, item)?, seen_at));
                report.refreshed += 1;
            }
        }
        for (sealed, after) in again {
            self.queue(std::slice::from_ref(&sealed), after).await?;
        }
        // The root, the recovery anchor, needs to be somewhere, not
        // everywhere (see above): published again when no relay that could
        // be asked shows it, or when it is getting old.
        let root_on = holders.get(&root_d).map(BTreeSet::len).unwrap_or(0);
        let root_at = newest.get(&root_d).copied().unwrap_or(0);
        if root_checked > 0 && (root_on == 0 || now.saturating_sub(root_at) > REFRESH_AFTER) {
            match self.publish_root().await {
                Ok(()) => {
                    if root_on == 0 {
                        report.resent += 1;
                    } else {
                        report.refreshed += 1;
                    }
                }
                Err(e) => tracing::info!("the root event wasn't refreshed: {e}"),
            }
        }
        self.flush().await;

        // Chunks nothing refers to: only ones old enough that no computer
        // can still be in the middle of sending the entry for them.
        let mut referenced = BTreeSet::new();
        for path in self.store.remote_paths()? {
            if let Some(r) = self.store.remote(&path)?
                && !r.entry.deleted
            {
                referenced.extend(r.entry.chunks);
            }
        }
        for (d, (ev, at)) in unknown {
            if now.saturating_sub(at) <= CHUNK_GRACE {
                continue;
            }
            if let Some(Item::Chunk(c)) = envelope::open(&self.keys, &d, &ev.content)
                && !referenced.contains(&c.sha256)
            {
                report.stale_chunks += 1;
                report
                    .stale
                    .push(format!("{DATA_KIND}:{}:{d}", self.sync_pubkey().to_hex()));
            }
        }
        let _ = self
            .store
            .db()
            .set_kv("peridot.audited_at", &now.to_string());
        *self.last_audit.write().await = Some(report.clone());
        Ok(report)
    }

    /// Ask the servers to drop chunk events nothing refers to (NIP-09, by
    /// coordinate). `signer` signs the request: yours may want to be
    /// asked about deletions, so the caller chooses which one.
    pub async fn remove_chunks(
        &self,
        signer: &dyn crate::signer::IdentitySigner,
        coordinates: &[String],
    ) -> anyhow::Result<usize> {
        let mut removed = 0;
        for batch in coordinates.chunks(DELETE_BATCH) {
            let mut b = EventBuilder::new(Kind::Custom(5), "")
                .tag(Tag::custom("k", [DATA_KIND.to_string()]));
            for coord in batch {
                b = b.tag(Tag::custom("a", [coord.as_str()]));
            }
            let ev = match self.keys.signer() {
                Some(k) => b.finalize(k)?,
                None => {
                    let unsigned = b.finalize_unsigned(self.pubkey());
                    sign_within(signer, unsigned, signer.sign_timeout()).await?
                }
            };
            self.outbox.push(&ev)?;
            removed += batch.len();
        }
        self.flush().await;
        if let Some(r) = self.last_audit.write().await.as_mut() {
            r.removed_chunks += removed;
            r.stale_chunks = r.stale_chunks.saturating_sub(removed);
            r.stale.retain(|c| !coordinates.contains(c));
        }
        Ok(removed)
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

    pub fn signer(&self) -> Arc<dyn crate::signer::IdentitySigner> {
        self.signer.clone()
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

    /// The key this epoch's items are signed by: the sync signing key, or
    /// the identity for the legacy epoch.
    pub fn sync_pubkey(&self) -> PublicKey {
        self.keys
            .signer()
            .map(|k| k.public_key())
            .unwrap_or_else(|| self.identity.pubkey())
    }

    /// The previous epoch's author, while its window is open.
    fn previous_pubkey(&self) -> Option<PublicKey> {
        self.previous.as_ref().map(|(_, k)| {
            k.signer()
                .map(|s| s.public_key())
                .unwrap_or_else(|| self.identity.pubkey())
        })
    }

    /// Where the rotation to the next epoch would be announced.
    fn rekey_pubkey(&self) -> Option<PublicKey> {
        (!self.identity.secret.is_legacy()).then(|| self.identity.secret.rekey_keys().public_key())
    }

    /// Everyone we listen to: this epoch, the previous one during its
    /// window, and the next epoch's rekey address.
    fn authors(&self) -> Vec<PublicKey> {
        let mut a = vec![self.sync_pubkey()];
        if let Some(p) = self.previous_pubkey()
            && !a.contains(&p)
        {
            a.push(p);
        }
        if let Some(r) = self.rekey_pubkey() {
            a.push(r);
        }
        a
    }

    /// The filter for everything of ours from `since`, all authors in one.
    pub fn filter(&self, since: u64) -> Filter {
        Filter::new()
            .authors(self.authors())
            .kind(Kind::Custom(DATA_KIND))
            .since(Timestamp::from(since))
    }

    /// The same, one filter per author, each sent as a request of its own.
    /// A relay that serves private data only to its authenticated author
    /// (relay.ditto.pub does) refuses a request naming anyone else, and
    /// only the epoch's key is logged in: asked apart, it serves this
    /// epoch's items and refuses the rest, which other relays carry. The
    /// first is always this epoch's own.
    pub fn filters(&self, since: u64) -> Vec<Filter> {
        self.authors()
            .into_iter()
            .map(|a| {
                Filter::new()
                    .author(a)
                    .kind(Kind::Custom(DATA_KIND))
                    .since(Timestamp::from(since))
            })
            .collect()
    }

    /// The epoch this computer is on.
    pub fn epoch(&self) -> u64 {
        self.keys.epoch()
    }

    /// This epoch's signing key (tests build events with it).
    pub fn sync_signing_keys(&self) -> Option<Keys> {
        self.keys.signer().cloned()
    }

    /// A rotation that arrived from another computer, once; the daemon
    /// switches over (or tells you this computer was removed).
    pub fn take_pending_rotation(&self) -> Option<Rotation> {
        self.pending_rotation
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
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
        // One request per relay and author (see `filters`); a relay that
        // refuses an author it can't serve still answers for the others.
        let relays = self.relays.read().await.clone();
        let mut lookups = Vec::new();
        for r in &relays {
            let Ok(Some(conn)) = self.client.relay(r).await else {
                continue;
            };
            for f in self.filters(since) {
                let conn = conn.clone();
                lookups.push(async move { conn.fetch_events(f).timeout(FETCH_TIMEOUT).await });
            }
        }
        let mut answered = false;
        let mut new = 0;
        for r in futures::future::join_all(lookups).await {
            match r {
                Ok(events) => {
                    answered = true;
                    new += events.iter().filter(|e| self.ingest(e)).count();
                }
                Err(e) => tracing::debug!("a catch-up request was refused: {e}"),
            }
        }
        if !answered {
            anyhow::bail!("catching up failed: no server answered");
        }
        self.mark_caught_up();
        Ok(new)
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
        if ev.kind != Kind::Custom(DATA_KIND) || ev.verify().is_err() {
            return false;
        }
        let Some(d) = ev.tags.identifier() else {
            return false;
        };
        let at = ev.created_at.as_secs();
        if at > now() + MAX_CLOCK_AHEAD {
            tracing::debug!("ignoring an event dated {}s in the future", at - now());
            return false;
        }
        // A rotation announced under the next epoch's address.
        if Some(ev.pubkey) == self.rekey_pubkey() && ev.pubkey != self.sync_pubkey() {
            return self.take_rotation(ev);
        }
        // From the previous epoch, during its window: read it, and say it
        // again under the current keys so it survives the cleanup. Never a
        // device entry: a computer removed by the rotation still holds the
        // previous secret, and an entry it wrote there (a new ID with its
        // own key, or its own entry no longer marked removed) would make
        // the next rotation hand it the new secret. The computers that
        // remain announce themselves again under the new keys once they
        // switch.
        let item = if ev.pubkey == self.sync_pubkey() {
            envelope::open(&self.keys, &d, &ev.content)
        } else if Some(ev.pubkey) == self.previous_pubkey() {
            let Some((_, old_keys)) = &self.previous else {
                return false;
            };
            let opened = envelope::open(old_keys, &d, &ev.content);
            if let Some(Item::Device(_)) = &opened {
                tracing::debug!("ignoring a device entry from the previous epoch");
                return false;
            }
            if let Some(item) = &opened
                && let Ok(sealed) = envelope::seal(&self.keys, item)
            {
                let _ = self.queue_local(&sealed, at);
            }
            opened
        } else {
            None
        };
        let Some(item) = item else {
            return false;
        };
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
                let _ = self.store.put_device(d, at, self.epoch());
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
                runs_commands: manifest.runs_commands(&path),
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
            pubkey: Some(self.identity.device.public_key().to_hex()),
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

    /// The newest root event on the servers, if any.
    pub async fn fetch_root(&self) -> anyhow::Result<Option<Event>> {
        let relays = self.relays.read().await.clone();
        let filter = Filter::new()
            .author(self.pubkey())
            .kind(Kind::Custom(DATA_KIND))
            .identifier(Identity::root_name(&self.pubkey()));
        let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
            .iter()
            .map(|r| (r.clone(), vec![filter.clone()]))
            .collect();
        let events = self
            .client
            .fetch_events(targets)
            .timeout(FETCH_TIMEOUT)
            .await?;
        Ok(events.iter().max_by_key(|e| e.created_at).cloned())
    }

    /// Publish the root event (the sync secret, encrypted to our own key)
    /// so a recovery kit can restore everything.
    pub async fn publish_root(&self) -> anyhow::Result<()> {
        // Dated after the root the servers hold now, so it replaces it
        // even within the same second (offline, "now" is all there is).
        let after = match self.fetch_root().await {
            Ok(Some(root)) => root.created_at.as_secs(),
            _ => 0,
        };
        self.outbox.push(
            &self
                .identity
                .root_event(self.signer.as_ref(), after)
                .await?,
        )?;
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

    /// Everything this computer knows, said again under the current keys.
    /// After a rotation, so the new epoch has it all: the rotator's device
    /// entries were all taken in under the epoch it rotated from, before
    /// any removed computer could write anything that counts.
    pub async fn republish_everything(&self) -> anyhow::Result<usize> {
        let epoch = self.epoch();
        let items = self.expected_items(&[epoch, epoch.saturating_sub(1)])?;
        let n = items.len();
        for (sealed, _, at) in items {
            let after = if at == u64::MAX { 0 } else { at };
            self.queue(std::slice::from_ref(&sealed), after).await?;
        }
        self.flush().await;
        Ok(n)
    }

    /// Start the next epoch: a new secret, handed to every current device
    /// except `remove`, announced under the address derived from the
    /// current secret. Returns the new secret; the daemon then saves it,
    /// restarts this engine with it, publishes the root again and calls
    /// [`Self::republish_everything`]. Nothing here changes the current
    /// keys, so a crash before the switch leaves everything as it was.
    pub async fn rotate(&self, remove: &[String]) -> anyhow::Result<SyncSecret> {
        // Everything must be here to be said again under the new keys.
        let missing = self.missing_chunks()?;
        anyhow::ensure!(
            missing == 0,
            "{missing} piece(s) of your settings haven't arrived yet; try again in a moment"
        );
        // A rotation from elsewhere that this computer hasn't switched to
        // yet: its directory may hold entries a computer removed there
        // wrote under this epoch. Switch first.
        anyhow::ensure!(
            !self.rotation_heard.load(Ordering::SeqCst),
            "another computer has just started a new epoch; try again once this one has switched"
        );
        let devices = self.store.devices()?;
        for id in remove {
            anyhow::ensure!(*id != self.device, "this computer can't remove itself");
            anyhow::ensure!(devices.iter().any(|d| &d.id == id), "no such computer");
        }
        // Mark the removed ones, under the current keys; the directory is
        // republished under the new epoch with the flags.
        for id in remove {
            if let Some(mut info) = devices.iter().find(|d| &d.id == id).cloned() {
                info.removed = true;
                info.last_seen = now();
                let _ = self.store.put_device(&info, now(), self.epoch());
            }
        }
        // Only entries said under this epoch (or kept from before epochs
        // were recorded): see [`SyncStore::devices_of`].
        let recipients: Vec<PublicKey> = self
            .store
            .devices_of(&[self.epoch()])?
            .iter()
            .filter(|d| !d.removed && !remove.contains(&d.id))
            .filter_map(|d| {
                d.pubkey
                    .as_deref()
                    .and_then(|p| PublicKey::from_hex(p).ok())
            })
            .filter(|p| *p != self.identity.device.public_key())
            .collect();
        let next = self.identity.secret.next();
        let ev = rotation::announce(
            &self.identity.secret,
            &next,
            &self.identity.device,
            &recipients,
        )?;
        self.outbox.push(&ev)?;
        self.flush().await;
        Ok(next)
    }

    /// Chunks current entries refer to that never arrived.
    fn missing_chunks(&self) -> anyhow::Result<usize> {
        let mut n = 0;
        for path in self.store.remote_paths()? {
            if let Some(r) = self.store.remote(&path)?
                && !r.entry.deleted
            {
                n += r
                    .entry
                    .chunks
                    .iter()
                    .filter(|c| self.store.chunk(c).is_none())
                    .count();
            }
        }
        Ok(n)
    }

    /// A rotation event under our rekey address: open it, find our wrap.
    fn take_rotation(&self, ev: &Event) -> bool {
        let outcome = rotation::adopt(ev, &self.identity.secret, &self.identity.device);
        match outcome {
            Ok(r) => {
                self.rotation_heard.store(true, Ordering::SeqCst);
                let mut slot = self
                    .pending_rotation
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                // Two rotations at once: the lower secret wins, so every
                // computer picks the same one.
                let replace = match (&*slot, &r) {
                    (None, _) => true,
                    (Some(Rotation::Removed { .. }), Rotation::Adopt { .. }) => true,
                    (
                        Some(Rotation::Adopt { secret: have, .. }),
                        Rotation::Adopt { secret, .. },
                    ) => secret.bytes() < have.bytes(),
                    _ => false,
                };
                if replace {
                    *slot = Some(r);
                }
                true
            }
            Err(e) => {
                tracing::debug!("ignoring a rotation event: {e}");
                false
            }
        }
    }

    /// The previous epoch's items on the servers, as coordinates to delete
    /// once its window has closed.
    pub async fn previous_coordinates(&self) -> anyhow::Result<Vec<String>> {
        let Some(prev) = self.previous_pubkey() else {
            return Ok(Vec::new());
        };
        let relays = self.relays.read().await.clone();
        let filter = Filter::new().author(prev).kind(Kind::Custom(DATA_KIND));
        let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
            .iter()
            .map(|r| (r.clone(), vec![filter.clone()]))
            .collect();
        let events = self
            .client
            .fetch_events(targets)
            .timeout(FETCH_TIMEOUT)
            .await?;
        let mut coords = BTreeSet::new();
        for ev in events.iter() {
            if let Some(d) = ev.tags.identifier() {
                coords.insert(format!("{DATA_KIND}:{}:{d}", prev.to_hex()));
            }
        }
        Ok(coords.into_iter().collect())
    }

    /// Delete the previous epoch's items (signed by its own key when it
    /// has one; the legacy epoch's need the identity's signer).
    pub async fn delete_previous(
        &self,
        signer: &dyn crate::signer::IdentitySigner,
        coordinates: &[String],
    ) -> anyhow::Result<usize> {
        let Some((_, old_keys)) = &self.previous else {
            return Ok(0);
        };
        let mut removed = 0;
        for batch in coordinates.chunks(DELETE_BATCH) {
            let mut b = EventBuilder::new(Kind::Custom(5), "")
                .tag(Tag::custom("k", [DATA_KIND.to_string()]));
            for coord in batch {
                b = b.tag(Tag::custom("a", [coord.as_str()]));
            }
            let ev = match old_keys.signer() {
                Some(k) => b.finalize(k)?,
                None => {
                    let unsigned = b.finalize_unsigned(self.pubkey());
                    sign_within(signer, unsigned, signer.sign_timeout()).await?
                }
            };
            self.outbox.push(&ev)?;
            removed += batch.len();
        }
        self.flush().await;
        Ok(removed)
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
        // Opened when the first file needs saving; written to like home
        // (beneath it, no links, 0600).
        let mut vault: Option<Home> = None;
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
                    let vault = match &vault {
                        Some(v) => v,
                        None => vault.insert(open_backup_dir(&backup)?),
                    };
                    vault.write_private(path, &current)?;
                    backed_up.push(path.clone());
                }
                if remote.entry.deleted {
                    self.home.remove(path)?;
                    self.store.set_synced(path, None, stamp)?;
                } else {
                    let content = envelope::assemble(&remote.entry, |sha| self.store.chunk(sha))?;
                    self.home.write(path, &content, false)?;
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
        let vault = match entry.backup_dir.as_deref().map(std::path::Path::new) {
            Some(dir) if dir.is_dir() => Some(Home::open(dir)?),
            _ => None,
        };
        let mut restored = Vec::new();
        for path in &entry.paths {
            let saved = match &vault {
                Some(v) => v.read(path)?,
                None => None,
            };
            let content = match saved {
                Some(content) => {
                    self.home.write(path, &content, false)?;
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
    /// queue them. This epoch's signing key signs here, without asking
    /// anyone; only the legacy epoch goes through the identity's signer.
    async fn queue(&self, sealed: &[Sealed], after: u64) -> anyhow::Result<()> {
        if self.keys.signer().is_some() {
            for s in sealed {
                self.queue_local(s, after)?;
            }
            return Ok(());
        }
        for s in sealed {
            let unsigned = EventBuilder::new(Kind::Custom(DATA_KIND), s.content.clone())
                .tag(Tag::identifier(s.d.clone()))
                .custom_created_at(Timestamp::from(self.next_created_at(after)))
                .finalize_unsigned(self.pubkey());
            let ev =
                sign_within(self.signer.as_ref(), unsigned, self.signer.sign_timeout()).await?;
            self.ingest(&ev);
            self.outbox.push(&ev)?;
        }
        Ok(())
    }

    /// The same, signed by this epoch's key (no waiting, no prompts).
    fn queue_local(&self, s: &Sealed, after: u64) -> anyhow::Result<()> {
        let signer = self
            .keys
            .signer()
            .ok_or_else(|| anyhow::anyhow!("the legacy epoch has no signing key"))?;
        let ev = EventBuilder::new(Kind::Custom(DATA_KIND), s.content.clone())
            .tag(Tag::identifier(s.d.clone()))
            .custom_created_at(Timestamp::from(self.next_created_at(after)))
            .finalize(signer)?;
        self.ingest(&ev);
        self.outbox.push(&ev)?;
        Ok(())
    }

    /// Strictly increasing timestamps, and always after the version being
    /// replaced (which another computer may have dated ahead of our clock):
    /// relays keep the newest per `d` tag. Dated to the hour (legacy
    /// items aside), so relays don't learn when exactly you work.
    fn next_created_at(&self, after: u64) -> u64 {
        let base = if self.keys.is_legacy() {
            now()
        } else {
            now() / 3600 * 3600
        };
        let now = base.max(after + 1);
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
                // One spelling of the address: the one the panel consents
                // to must be the one that runs.
                if valid_name(&t.name)
                    && let Some(url) = crate::gallery::canonical_url(&t.url)
                    && valid_source(&url)
                {
                    offers.push(Offer::InstallTheme {
                        name: t.name,
                        url,
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
                if valid_plugin_id(&p.name)
                    && let Some(url) = crate::gallery::canonical_url(&p.url)
                    && valid_source(&url)
                {
                    offers.push(Offer::InstallPlugin {
                        name: p.name,
                        url,
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

/// Something installed under `~/.config/omarchy/{themes,plugins}`: what
/// it's called, where it came from if that can be told, and whether it's
/// a link (a checkout you develop) rather than an install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Installed {
    pub name: String,
    /// A public https repository, when the folder's git origin says so.
    pub url: Option<String>,
    pub linked: bool,
}

/// Every folder in `dir`, links included, with its origin when public.
pub fn installed_in(dir: &std::path::Path) -> Vec<Installed> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let Ok(meta) = e.path().symlink_metadata() else {
            continue;
        };
        let linked = meta.file_type().is_symlink();
        if !e.path().is_dir() {
            continue;
        }
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let url = std::fs::read_to_string(e.path().join(".git/config"))
            .ok()
            .and_then(|c| origin_url(&c))
            .and_then(|u| https_source(&u));
        out.push(Installed { name, url, linked });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
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

/// Make the folder for one apply's backups (owner-only) and open it the
/// way home is opened, so files are saved beneath it and never through a
/// link.
fn open_backup_dir(dir: &std::path::Path) -> anyhow::Result<Home> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    Ok(Home::open(dir)?)
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
