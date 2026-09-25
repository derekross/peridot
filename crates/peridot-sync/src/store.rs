//! What this computer knows about sync: the last version of each file your
//! computers agreed on, the newest version from elsewhere, pieces of big
//! files, your devices, Omarchy state and the undo history.

use opal_core::db::Db;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::envelope::{Chunk, DeviceInfo, FileEntry, StateEntry};

/// Where a file stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    /// Same everywhere.
    InSync,
    /// Changed here; will be published.
    Outgoing,
    /// Changed on another computer; waiting to be applied here.
    Incoming,
    /// Changed both here and elsewhere since they last matched.
    Conflict,
    /// You undid the version from another computer: this computer keeps
    /// its own, without pushing it to the others, until either changes.
    Kept,
}

/// The newest version of a file from any of your computers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub entry: FileEntry,
    pub created_at: u64,
}

impl Remote {
    /// Its hash, or None for a deletion.
    pub fn sha(&self) -> Option<&str> {
        (!self.entry.deleted).then_some(self.entry.sha256.as_str())
    }
}

/// Decide a file's status from its hash here (None = missing), the hash
/// this computer last agreed on (None = never), and the newest remote
/// version.
pub fn decide(
    local: Option<&str>,
    synced: Option<&str>,
    remote: Option<&Remote>,
    me: &str,
) -> FileStatus {
    let Some(remote) = remote else {
        return if local.is_some() {
            FileStatus::Outgoing
        } else {
            FileStatus::InSync
        };
    };
    let theirs = remote.sha();
    if local == theirs {
        return FileStatus::InSync;
    }
    if remote.entry.device == me {
        // Our own last publish; we've changed it since.
        return FileStatus::Outgoing;
    }
    if synced.is_some() && synced == theirs {
        // They haven't moved since we last matched; we have.
        return FileStatus::Outgoing;
    }
    if synced.is_none() || local == synced {
        // Nothing changed here since we last matched (or we never did,
        // e.g. a newly paired computer): take theirs, after asking.
        return FileStatus::Incoming;
    }
    FileStatus::Conflict
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub id: i64,
    pub at: u64,
    /// What happened, e.g. "Applied 3 settings from Laptop".
    pub summary: String,
    /// Paths whose previous versions were backed up.
    pub paths: Vec<String>,
    pub backup_dir: Option<String>,
    pub undone: bool,
}

#[derive(Clone)]
pub struct SyncStore {
    db: Db,
}

impl SyncStore {
    pub fn new(db: Db) -> opal_core::Result<Self> {
        db.migrate(
            "peridot.sync",
            &[
                "CREATE TABLE synced (
                path TEXT PRIMARY KEY,
                sha TEXT,
                at INTEGER NOT NULL
            );
            CREATE TABLE remote (
                path TEXT PRIMARY KEY,
                entry TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                event_id TEXT NOT NULL
            );
            CREATE TABLE chunks (
                sha TEXT PRIMARY KEY,
                data TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE devices (
                id TEXT PRIMARY KEY,
                info TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE state (
                kind TEXT PRIMARY KEY,
                entry TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                event_id TEXT NOT NULL
            );
            CREATE TABLE history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                at INTEGER NOT NULL,
                summary TEXT NOT NULL,
                paths TEXT NOT NULL,
                backup_dir TEXT,
                undone INTEGER NOT NULL DEFAULT 0
            );",
                "CREATE TABLE kept (
                path TEXT PRIMARY KEY,
                remote TEXT NOT NULL
            );",
            ],
        )?;
        Ok(Self { db })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn synced(&self, path: &str) -> opal_core::Result<Option<String>> {
        self.db.with(|c| {
            c.query_row("SELECT sha FROM synced WHERE path = ?1", [path], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()
            .map(Option::flatten)
        })
    }

    /// Record that this computer and the others agree on `sha` (None =
    /// deleted everywhere).
    pub fn set_synced(&self, path: &str, sha: Option<&str>, at: u64) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO synced (path, sha, at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(path) DO UPDATE SET sha = excluded.sha, at = excluded.at",
                params![path, sha, at as i64],
            )?;
            Ok(())
        })
    }

    pub fn remote(&self, path: &str) -> opal_core::Result<Option<Remote>> {
        let row: Option<(String, i64)> = self.db.with(|c| {
            c.query_row(
                "SELECT entry, created_at FROM remote WHERE path = ?1",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })?;
        Ok(row.and_then(|(json, at)| {
            serde_json::from_str(&json).ok().map(|entry| Remote {
                entry,
                created_at: at as u64,
            })
        }))
    }

    /// Keep `entry` if it's newer than what we have (on a tie, the lower
    /// event id wins, as relays decide). Returns whether it was.
    pub fn put_remote(
        &self,
        entry: &FileEntry,
        created_at: u64,
        event_id: &str,
    ) -> opal_core::Result<bool> {
        let json = serde_json::to_string(entry).expect("serializable");
        self.db.with(|c| {
            let n = c.execute(
                "INSERT INTO remote (path, entry, created_at, event_id) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(path) DO UPDATE SET entry = excluded.entry,
                    created_at = excluded.created_at, event_id = excluded.event_id
                 WHERE excluded.created_at > remote.created_at
                    OR (excluded.created_at = remote.created_at AND excluded.event_id < remote.event_id)",
                params![entry.path, json, created_at as i64, event_id],
            )?;
            Ok(n > 0)
        })
    }

    pub fn remote_paths(&self) -> opal_core::Result<Vec<String>> {
        self.db.with(|c| {
            let mut st = c.prepare("SELECT path FROM remote ORDER BY path")?;
            st.query_map([], |r| r.get(0))?.collect()
        })
    }

    pub fn put_chunk(&self, chunk: &Chunk, at: u64) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO chunks (sha, data, created_at) VALUES (?1, ?2, ?3)",
                params![chunk.sha256, chunk.data, at as i64],
            )?;
            Ok(())
        })
    }

    pub fn chunk(&self, sha: &str) -> Option<Chunk> {
        self.db
            .with(|c| {
                c.query_row("SELECT data FROM chunks WHERE sha = ?1", [sha], |r| {
                    r.get::<_, String>(0)
                })
                .optional()
            })
            .ok()
            .flatten()
            .map(|data| Chunk {
                sha256: sha.to_string(),
                data,
            })
    }

    /// Drop pieces no current file refers to.
    pub fn prune_chunks(&self) -> opal_core::Result<usize> {
        let mut keep = std::collections::HashSet::new();
        for path in self.remote_paths()? {
            if let Some(r) = self.remote(&path)? {
                keep.extend(r.entry.chunks);
            }
        }
        let all: Vec<String> = self.db.with(|c| {
            let mut st = c.prepare("SELECT sha FROM chunks")?;
            st.query_map([], |r| r.get(0))?.collect()
        })?;
        let mut removed = 0;
        for sha in all.into_iter().filter(|s| !keep.contains(s)) {
            self.db
                .with(|c| c.execute("DELETE FROM chunks WHERE sha = ?1", [&sha]))?;
            removed += 1;
        }
        Ok(removed)
    }

    pub fn put_device(&self, info: &DeviceInfo, created_at: u64) -> opal_core::Result<()> {
        let json = serde_json::to_string(info).expect("serializable");
        self.db.with(|c| {
            c.execute(
                "INSERT INTO devices (id, info, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET info = excluded.info, created_at = excluded.created_at
                 WHERE excluded.created_at >= devices.created_at",
                params![info.id, json, created_at as i64],
            )?;
            Ok(())
        })
    }

    pub fn devices(&self) -> opal_core::Result<Vec<DeviceInfo>> {
        let rows: Vec<String> = self.db.with(|c| {
            let mut st = c.prepare("SELECT info FROM devices")?;
            st.query_map([], |r| r.get(0))?.collect()
        })?;
        let mut out: Vec<DeviceInfo> = rows
            .iter()
            .filter_map(|j| serde_json::from_str(j).ok())
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.last_seen));
        Ok(out)
    }

    pub fn put_state(
        &self,
        kind: &str,
        entry: &StateEntry,
        created_at: u64,
        event_id: &str,
    ) -> opal_core::Result<bool> {
        let json = serde_json::to_string(entry).expect("serializable");
        self.db.with(|c| {
            let n = c.execute(
                "INSERT INTO state (kind, entry, created_at, event_id) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(kind) DO UPDATE SET entry = excluded.entry,
                    created_at = excluded.created_at, event_id = excluded.event_id
                 WHERE excluded.created_at > state.created_at
                    OR (excluded.created_at = state.created_at AND excluded.event_id < state.event_id)",
                params![kind, json, created_at as i64, event_id],
            )?;
            Ok(n > 0)
        })
    }

    /// When the newest known state of `kind` was published.
    pub fn state_created_at(&self, kind: &str) -> u64 {
        self.db
            .with(|c| {
                c.query_row(
                    "SELECT created_at FROM state WHERE kind = ?1",
                    [kind],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
            })
            .ok()
            .flatten()
            .unwrap_or(0) as u64
    }

    /// The value of `kind` this computer last agreed on (like `synced` for
    /// files), as a comparable string.
    pub fn state_synced(&self, kind: &str) -> Option<String> {
        self.db
            .get_kv(&format!("peridot.state.{kind}"))
            .ok()
            .flatten()
    }

    pub fn set_state_synced(&self, kind: &str, value: &str) -> opal_core::Result<()> {
        self.db.set_kv(&format!("peridot.state.{kind}"), value)
    }

    pub fn state(&self, kind: &str) -> opal_core::Result<Option<StateEntry>> {
        let row: Option<String> = self.db.with(|c| {
            c.query_row("SELECT entry FROM state WHERE kind = ?1", [kind], |r| {
                r.get(0)
            })
            .optional()
        })?;
        Ok(row.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// Keep this computer's version of `path` while the other computers'
    /// version stays `remote` (its hash, or empty for a deletion).
    pub fn keep(&self, path: &str, remote: &str) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO kept (path, remote) VALUES (?1, ?2)
                 ON CONFLICT(path) DO UPDATE SET remote = excluded.remote",
                params![path, remote],
            )?;
            Ok(())
        })
    }

    pub fn kept(&self, path: &str) -> Option<String> {
        self.db
            .with(|c| {
                c.query_row("SELECT remote FROM kept WHERE path = ?1", [path], |r| {
                    r.get(0)
                })
                .optional()
            })
            .ok()
            .flatten()
    }

    pub fn unkeep(&self, path: &str) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute("DELETE FROM kept WHERE path = ?1", [path])?;
            Ok(())
        })
    }

    pub fn add_history(
        &self,
        at: u64,
        summary: &str,
        paths: &[String],
        backup_dir: Option<&str>,
    ) -> opal_core::Result<i64> {
        let paths = serde_json::to_string(paths).expect("serializable");
        self.db.with(|c| {
            c.execute(
                "INSERT INTO history (at, summary, paths, backup_dir) VALUES (?1, ?2, ?3, ?4)",
                params![at as i64, summary, paths, backup_dir],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn history(&self, limit: usize) -> opal_core::Result<Vec<HistoryEntry>> {
        self.db.with(|c| {
            let mut st = c.prepare(
                "SELECT id, at, summary, paths, backup_dir, undone FROM history
                 ORDER BY id DESC LIMIT ?1",
            )?;
            st.query_map([limit as i64], |r| {
                Ok(HistoryEntry {
                    id: r.get(0)?,
                    at: r.get::<_, i64>(1)? as u64,
                    summary: r.get(2)?,
                    paths: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                    backup_dir: r.get(4)?,
                    undone: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect()
        })
    }

    pub fn mark_undone(&self, id: i64) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute("UPDATE history SET undone = 1 WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// Newest remote `created_at` seen, to subscribe from.
    pub fn since(&self) -> u64 {
        self.db
            .get_kv("peridot.since")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    pub fn set_since(&self, at: u64) -> opal_core::Result<()> {
        if at > self.since() {
            self.db.set_kv("peridot.since", &at.to_string())?;
        }
        Ok(())
    }

    /// This installation's device id (random, created once).
    pub fn device_id(&self) -> opal_core::Result<String> {
        if let Some(id) = self.db.get_kv("peridot.device_id")? {
            return Ok(id);
        }
        let id = hex::encode(crate::crypto::random_bytes::<8>());
        self.db.set_kv("peridot.device_id", &id)?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(sha: &str, device: &str) -> Remote {
        Remote {
            entry: FileEntry {
                path: "p".into(),
                sha256: sha.into(),
                size: 1,
                base: None,
                device: device.into(),
                deleted: sha.is_empty(),
                data: None,
                chunks: vec![],
            },
            created_at: 1,
        }
    }

    #[test]
    fn decides_every_case() {
        use FileStatus::*;
        let me = "me";
        // Nothing published yet.
        assert_eq!(decide(Some("a"), None, None, me), Outgoing);
        assert_eq!(decide(None, None, None, me), InSync);
        // Same everywhere.
        assert_eq!(
            decide(Some("a"), Some("a"), Some(&remote("a", "them")), me),
            InSync
        );
        // We changed it after our own publish.
        assert_eq!(
            decide(Some("b"), Some("a"), Some(&remote("a", me)), me),
            Outgoing
        );
        // We changed it; they're still where we last matched.
        assert_eq!(
            decide(Some("b"), Some("a"), Some(&remote("a", "them")), me),
            Outgoing
        );
        // They changed it; we didn't.
        assert_eq!(
            decide(Some("a"), Some("a"), Some(&remote("b", "them")), me),
            Incoming
        );
        // Newly paired computer: take theirs (after asking).
        assert_eq!(
            decide(Some("x"), None, Some(&remote("b", "them")), me),
            Incoming
        );
        assert_eq!(decide(None, None, Some(&remote("b", "them")), me), Incoming);
        // Both changed.
        assert_eq!(
            decide(Some("c"), Some("a"), Some(&remote("b", "them")), me),
            Conflict
        );
        // They deleted it; we didn't touch it.
        assert_eq!(
            decide(Some("a"), Some("a"), Some(&remote("", "them")), me),
            Incoming
        );
        // They deleted it; we edited it.
        assert_eq!(
            decide(Some("c"), Some("a"), Some(&remote("", "them")), me),
            Conflict
        );
        // Deleted everywhere.
        assert_eq!(decide(None, None, Some(&remote("", "them")), me), InSync);
    }

    #[test]
    fn keeps_only_newer_remote_versions_and_prunes_chunks() {
        let store = SyncStore::new(Db::open_in_memory().unwrap()).unwrap();
        let mut e = remote("a", "them").entry;
        e.chunks = vec!["c1".into()];
        assert!(store.put_remote(&e, 10, "bb").unwrap());
        let mut older = e.clone();
        older.sha256 = "old".into();
        assert!(!store.put_remote(&older, 5, "aa").unwrap());
        assert_eq!(store.remote("p").unwrap().unwrap().entry.sha256, "a");
        // Same second: the lower event id wins, whichever arrives first.
        let mut tie = e.clone();
        tie.sha256 = "tie".into();
        assert!(!store.put_remote(&tie, 10, "cc").unwrap());
        assert!(store.put_remote(&tie, 10, "aa").unwrap());
        assert_eq!(store.remote("p").unwrap().unwrap().entry.sha256, "tie");
        store
            .put_chunk(
                &Chunk {
                    sha256: "c1".into(),
                    data: "x".into(),
                },
                1,
            )
            .unwrap();
        store
            .put_chunk(
                &Chunk {
                    sha256: "c2".into(),
                    data: "y".into(),
                },
                1,
            )
            .unwrap();
        assert_eq!(store.prune_chunks().unwrap(), 1);
        assert!(store.chunk("c1").is_some() && store.chunk("c2").is_none());
    }

    #[test]
    fn device_id_is_stable_and_since_only_moves_forward() {
        let store = SyncStore::new(Db::open_in_memory().unwrap()).unwrap();
        let id = store.device_id().unwrap();
        assert_eq!(id, store.device_id().unwrap());
        store.set_since(100).unwrap();
        store.set_since(50).unwrap();
        assert_eq!(store.since(), 100);
    }
}
