//! What the Gallery remembers between runs: the catalogues, everyone's
//! likes, reviews, listings and setups, names, and who follows whom.

use std::collections::{BTreeSet, HashMap};

use nostr_sdk::prelude::*;
use opal_core::db::Db;
use peridot_sync::gallery::{ItemKind, Like, Listing, Profile, Review, Setup, SetupSpec};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// A theme or plugin as the Gallery shows it, before likes are counted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub url: String,
    pub kind: ItemKind,
    pub name: String,
    pub author: String,
    pub description: String,
    pub category: String,
    pub tags: Vec<String>,
    pub stars: u32,
    pub preview: Option<String>,
    /// Where it came from: `plugins`, `themes` (the registries) or
    /// `nostr` (someone published it).
    pub source: String,
    /// Can be installed with an Omarchy command as is.
    pub installable: bool,
    /// Published by this key (a listing from the Gallery).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<PublicKey>,
}

#[derive(Clone)]
pub struct GalleryStore {
    db: Db,
}

impl GalleryStore {
    pub fn new(db: Db) -> opal_core::Result<Self> {
        db.migrate(
            "peridot.gallery",
            &["CREATE TABLE gallery_items (
                url TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT NOT NULL,
                author TEXT NOT NULL,
                description TEXT NOT NULL,
                category TEXT NOT NULL,
                tags TEXT NOT NULL,
                stars INTEGER NOT NULL,
                preview TEXT,
                source TEXT NOT NULL,
                installable INTEGER NOT NULL,
                publisher TEXT,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE gallery_likes (
                id TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                pubkey TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX gallery_likes_url ON gallery_likes(url);
            CREATE TABLE gallery_reviews (
                id TEXT PRIMARY KEY,
                url TEXT NOT NULL,
                pubkey TEXT NOT NULL,
                text TEXT NOT NULL,
                rating INTEGER,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX gallery_reviews_url ON gallery_reviews(url);
            CREATE TABLE gallery_setups (
                coordinate TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                pubkey TEXT NOT NULL,
                slug TEXT NOT NULL,
                spec TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE gallery_profiles (
                pubkey TEXT PRIMARY KEY,
                profile TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE gallery_follows (
                pubkey TEXT PRIMARY KEY,
                follows TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE gallery_deleted (
                id TEXT PRIMARY KEY,
                pubkey TEXT NOT NULL
            );
            CREATE TABLE gallery_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"],
        )?;
        Ok(Self { db })
    }

    // ── Items ─────────────────────────────────────────────────────────

    /// Replace everything from `source` with `items` (a fresh catalogue).
    pub fn replace_source(&self, source: &str, items: &[Item], now: u64) -> opal_core::Result<()> {
        self.db.with(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute("DELETE FROM gallery_items WHERE source = ?1", [source])?;
            for it in items {
                upsert(&tx, it, now, false)?;
            }
            tx.commit()
        })
    }

    /// A listing someone published: fills a gap, never overrides a
    /// registry entry.
    pub fn add_listing(&self, l: &Listing, now: u64) -> opal_core::Result<()> {
        let item = Item {
            url: l.url.clone(),
            kind: l.kind,
            name: if l.name.is_empty() {
                peridot_sync::gallery::short_name(&l.url)
            } else {
                l.name.clone()
            },
            author: l
                .url
                .trim_start_matches("https://")
                .split('/')
                .nth(1)
                .unwrap_or("")
                .to_string(),
            description: String::new(),
            category: String::new(),
            tags: Vec::new(),
            stars: 0,
            preview: None,
            source: "nostr".into(),
            installable: true,
            publisher: Some(l.pubkey),
        };
        self.db.with(|c| upsert(c, &item, now, true))
    }

    pub fn item(&self, url: &str) -> opal_core::Result<Option<Item>> {
        self.db.with(|c| {
            c.query_row(&format!("{ITEM_SELECT} WHERE url = ?1"), [url], item_row)
                .optional()
        })
    }

    /// Items of `kind` (or all) whose name, author, description or tags
    /// contain every word of `query`.
    pub fn items(&self, kind: Option<ItemKind>, query: &str) -> opal_core::Result<Vec<Item>> {
        let words: Vec<String> = query
            .split_whitespace()
            .map(|w| w.to_lowercase())
            .filter(|w| !w.is_empty())
            .take(6)
            .collect();
        let all: Vec<Item> = self.db.with(|c| {
            let mut st = c.prepare(&match kind {
                Some(_) => format!("{ITEM_SELECT} WHERE kind = ?1"),
                None => format!("{ITEM_SELECT} WHERE kind != ?1"),
            })?;
            let k = kind.map(|k| k.as_str()).unwrap_or("");
            st.query_map([k], item_row).map(|r| r.collect())?
        })?;
        if words.is_empty() {
            return Ok(all);
        }
        Ok(all
            .into_iter()
            .filter(|it| {
                let hay = format!(
                    "{} {} {} {} {}",
                    it.name,
                    it.author,
                    it.description,
                    it.category,
                    it.tags.join(" ")
                )
                .to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect())
    }

    pub fn item_count(&self) -> (usize, usize) {
        self.db
            .with(|c| {
                let themes: i64 = c.query_row(
                    "SELECT COUNT(*) FROM gallery_items WHERE kind = 'theme'",
                    [],
                    |r| r.get(0),
                )?;
                let plugins: i64 = c.query_row(
                    "SELECT COUNT(*) FROM gallery_items WHERE kind = 'plugin'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((themes as usize, plugins as usize))
            })
            .unwrap_or((0, 0))
    }

    // ── Likes, reviews, setups ────────────────────────────────────────

    pub fn put_like(&self, l: &Like) -> opal_core::Result<bool> {
        self.db.with(|c| {
            if is_deleted(c, &l.id)? {
                return Ok(false);
            }
            let n = c.execute(
                "INSERT OR IGNORE INTO gallery_likes (id, url, pubkey, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![l.id.to_hex(), l.url, l.pubkey.to_hex(), l.created_at as i64],
            )?;
            Ok(n > 0)
        })
    }

    pub fn put_review(&self, r: &Review) -> opal_core::Result<bool> {
        self.db.with(|c| {
            if is_deleted(c, &r.id)? {
                return Ok(false);
            }
            let n = c.execute(
                "INSERT OR IGNORE INTO gallery_reviews (id, url, pubkey, text, rating, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    r.id.to_hex(),
                    r.url,
                    r.pubkey.to_hex(),
                    r.text,
                    r.rating.map(i64::from),
                    r.created_at as i64
                ],
            )?;
            Ok(n > 0)
        })
    }

    /// Newest version wins (addressable).
    pub fn put_setup(&self, s: &Setup) -> opal_core::Result<bool> {
        self.db.with(|c| {
            if is_deleted(c, &s.id)? {
                return Ok(false);
            }
            let coord = s.coordinate();
            let have: Option<i64> = c
                .query_row(
                    "SELECT created_at FROM gallery_setups WHERE coordinate = ?1",
                    [&coord],
                    |r| r.get(0),
                )
                .optional()?;
            if have.is_some_and(|t| t as u64 >= s.created_at) {
                return Ok(false);
            }
            c.execute(
                "INSERT OR REPLACE INTO gallery_setups (coordinate, id, pubkey, slug, spec, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    coord,
                    s.id.to_hex(),
                    s.pubkey.to_hex(),
                    s.slug,
                    serde_json::to_string(&s.spec).unwrap_or_default(),
                    s.created_at as i64
                ],
            )?;
            Ok(true)
        })
    }

    /// The author took these back: forget them and remember not to take
    /// them again from a slower relay.
    pub fn delete(&self, pubkey: &PublicKey, ids: &[EventId]) -> opal_core::Result<usize> {
        let pk = pubkey.to_hex();
        self.db.with(|c| {
            let mut n = 0;
            for id in ids {
                let id = id.to_hex();
                n += c.execute(
                    "DELETE FROM gallery_likes WHERE id = ?1 AND pubkey = ?2",
                    params![id, pk],
                )?;
                n += c.execute(
                    "DELETE FROM gallery_reviews WHERE id = ?1 AND pubkey = ?2",
                    params![id, pk],
                )?;
                n += c.execute(
                    "DELETE FROM gallery_setups WHERE id = ?1 AND pubkey = ?2",
                    params![id, pk],
                )?;
                c.execute(
                    "INSERT OR IGNORE INTO gallery_deleted (id, pubkey) VALUES (?1, ?2)",
                    params![id, pk],
                )?;
            }
            Ok(n)
        })
    }

    /// Who likes what: url → likers.
    pub fn likes(&self) -> opal_core::Result<HashMap<String, Vec<PublicKey>>> {
        self.db.with(|c| {
            let mut st = c.prepare("SELECT url, pubkey FROM gallery_likes")?;
            let mut out: HashMap<String, BTreeSet<PublicKey>> = HashMap::new();
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (url, pk) = r?;
                if let Ok(pk) = PublicKey::from_hex(&pk) {
                    out.entry(url).or_default().insert(pk);
                }
            }
            Ok(out
                .into_iter()
                .map(|(u, s)| (u, s.into_iter().collect()))
                .collect())
        })
    }

    /// This key's own like of `url`, if any (to take it back).
    pub fn my_like(&self, url: &str, me: &PublicKey) -> opal_core::Result<Option<EventId>> {
        self.db.with(|c| {
            let id: Option<String> = c
                .query_row(
                    "SELECT id FROM gallery_likes WHERE url = ?1 AND pubkey = ?2 ORDER BY created_at DESC LIMIT 1",
                    params![url, me.to_hex()],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(id.and_then(|i| EventId::from_hex(&i).ok()))
        })
    }

    pub fn reviews(&self, url: &str) -> opal_core::Result<Vec<Review>> {
        self.db.with(|c| {
            let mut st = c.prepare(
                "SELECT id, url, pubkey, text, rating, created_at FROM gallery_reviews
                 WHERE url = ?1 ORDER BY created_at DESC LIMIT 100",
            )?;
            let rows = st.query_map([url], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (id, url, pk, text, rating, at) = r?;
                if let (Ok(id), Ok(pubkey)) = (EventId::from_hex(&id), PublicKey::from_hex(&pk)) {
                    out.push(Review {
                        id,
                        url,
                        pubkey,
                        text,
                        rating: rating.map(|r| r as u8),
                        created_at: at as u64,
                    });
                }
            }
            Ok(out)
        })
    }

    /// Review counts and average rating per url.
    pub fn review_stats(&self) -> opal_core::Result<HashMap<String, (u32, Option<f32>)>> {
        self.db.with(|c| {
            let mut st =
                c.prepare("SELECT url, COUNT(*), AVG(rating) FROM gallery_reviews GROUP BY url")?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                ))
            })?;
            let mut out = HashMap::new();
            for r in rows {
                let (url, n, avg) = r?;
                out.insert(url, (n as u32, avg.map(|a| a as f32)));
            }
            Ok(out)
        })
    }

    pub fn setups(&self) -> opal_core::Result<Vec<Setup>> {
        self.db.with(|c| {
            let mut st = c.prepare(
                "SELECT id, pubkey, slug, spec, created_at FROM gallery_setups
                 ORDER BY created_at DESC LIMIT 500",
            )?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (id, pk, slug, spec, at) = r?;
                if let (Ok(id), Ok(pubkey), Ok(spec)) = (
                    EventId::from_hex(&id),
                    PublicKey::from_hex(&pk),
                    serde_json::from_str::<SetupSpec>(&spec),
                ) {
                    out.push(Setup {
                        id,
                        pubkey,
                        slug,
                        spec,
                        created_at: at as u64,
                    });
                }
            }
            Ok(out)
        })
    }

    pub fn setup(&self, coordinate: &str) -> opal_core::Result<Option<Setup>> {
        Ok(self
            .setups()?
            .into_iter()
            .find(|s| s.coordinate() == coordinate))
    }

    // ── People ────────────────────────────────────────────────────────

    pub fn put_profile(
        &self,
        pk: &PublicKey,
        p: &Profile,
        created_at: u64,
        now: u64,
    ) -> opal_core::Result<()> {
        self.db.with(|c| {
            let have: Option<i64> = c
                .query_row(
                    "SELECT created_at FROM gallery_profiles WHERE pubkey = ?1",
                    [pk.to_hex()],
                    |r| r.get(0),
                )
                .optional()?;
            if have.is_some_and(|t| t as u64 > created_at) {
                // Older than what we have: only note that we looked.
                c.execute(
                    "UPDATE gallery_profiles SET fetched_at = ?2 WHERE pubkey = ?1",
                    params![pk.to_hex(), now as i64],
                )?;
                return Ok(());
            }
            c.execute(
                "INSERT OR REPLACE INTO gallery_profiles (pubkey, profile, created_at, fetched_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    pk.to_hex(),
                    serde_json::to_string(p).unwrap_or_default(),
                    created_at as i64,
                    now as i64
                ],
            )?;
            Ok(())
        })
    }

    /// Note that we asked and nobody had a profile (so we don't ask again
    /// for a while).
    pub fn touch_profile(&self, pk: &PublicKey, now: u64) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT OR IGNORE INTO gallery_profiles (pubkey, profile, created_at, fetched_at)
                 VALUES (?1, '{}', 0, ?2)",
                params![pk.to_hex(), now as i64],
            )?;
            c.execute(
                "UPDATE gallery_profiles SET fetched_at = ?2 WHERE pubkey = ?1",
                params![pk.to_hex(), now as i64],
            )?;
            Ok(())
        })
    }

    pub fn profile(&self, pk: &PublicKey) -> Option<(Profile, u64)> {
        self.db
            .with(|c| {
                c.query_row(
                    "SELECT profile, fetched_at FROM gallery_profiles WHERE pubkey = ?1",
                    [pk.to_hex()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()
            })
            .ok()
            .flatten()
            .and_then(|(p, at)| {
                serde_json::from_str::<Profile>(&p)
                    .ok()
                    .map(|p| (p, at as u64))
            })
    }

    /// Keys we have no profile for, or looked up longer than `max_age` ago.
    pub fn stale_profiles<'a>(
        &self,
        keys: impl IntoIterator<Item = &'a PublicKey>,
        now: u64,
        max_age: u64,
    ) -> Vec<PublicKey> {
        keys.into_iter()
            .filter(|pk| match self.profile(pk) {
                Some((_, at)) => now.saturating_sub(at) > max_age,
                None => true,
            })
            .copied()
            .collect()
    }

    pub fn put_follows(
        &self,
        pk: &PublicKey,
        follows: &[PublicKey],
        created_at: u64,
        now: u64,
    ) -> opal_core::Result<()> {
        self.db.with(|c| {
            let have: Option<i64> = c
                .query_row(
                    "SELECT created_at FROM gallery_follows WHERE pubkey = ?1",
                    [pk.to_hex()],
                    |r| r.get(0),
                )
                .optional()?;
            if have.is_some_and(|t| t as u64 > created_at) {
                return Ok(());
            }
            let list: Vec<String> = follows.iter().map(|p| p.to_hex()).collect();
            c.execute(
                "INSERT OR REPLACE INTO gallery_follows (pubkey, follows, created_at, fetched_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    pk.to_hex(),
                    serde_json::to_string(&list).unwrap_or_default(),
                    created_at as i64,
                    now as i64
                ],
            )?;
            Ok(())
        })
    }

    pub fn follows(&self, pk: &PublicKey) -> Option<(Vec<PublicKey>, u64, u64)> {
        self.db
            .with(|c| {
                c.query_row(
                    "SELECT follows, created_at, fetched_at FROM gallery_follows WHERE pubkey = ?1",
                    [pk.to_hex()],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()
            })
            .ok()
            .flatten()
            .map(|(f, created, fetched)| {
                let list: Vec<String> = serde_json::from_str(&f).unwrap_or_default();
                (
                    list.iter()
                        .filter_map(|h| PublicKey::from_hex(h).ok())
                        .collect(),
                    created as u64,
                    fetched as u64,
                )
            })
    }

    // ── Bookkeeping ───────────────────────────────────────────────────

    pub fn meta(&self, key: &str) -> Option<String> {
        self.db
            .with(|c| {
                c.query_row(
                    "SELECT value FROM gallery_meta WHERE key = ?1",
                    [key],
                    |r| r.get(0),
                )
                .optional()
            })
            .ok()
            .flatten()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO gallery_meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )?;
            Ok(())
        })
    }
}

const ITEM_SELECT: &str = "SELECT url, kind, name, author, description, category, tags, stars, preview, source, installable, publisher FROM gallery_items";

fn upsert(
    c: &rusqlite::Connection,
    it: &Item,
    now: u64,
    only_if_missing: bool,
) -> rusqlite::Result<()> {
    let verb = if only_if_missing {
        "INSERT OR IGNORE"
    } else {
        "INSERT OR REPLACE"
    };
    c.execute(
        &format!(
            "{verb} INTO gallery_items
             (url, kind, name, author, description, category, tags, stars, preview, source, installable, publisher, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)"
        ),
        params![
            it.url,
            it.kind.as_str(),
            it.name,
            it.author,
            it.description,
            it.category,
            serde_json::to_string(&it.tags).unwrap_or_default(),
            it.stars as i64,
            it.preview,
            it.source,
            it.installable as i64,
            it.publisher.map(|p| p.to_hex()),
            now as i64
        ],
    )?;
    Ok(())
}

fn item_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    let kind: String = r.get(1)?;
    let tags: String = r.get(6)?;
    let publisher: Option<String> = r.get(11)?;
    Ok(Item {
        url: r.get(0)?,
        kind: ItemKind::parse(&kind).unwrap_or(ItemKind::Plugin),
        name: r.get(2)?,
        author: r.get(3)?,
        description: r.get(4)?,
        category: r.get(5)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        stars: r.get::<_, i64>(7)? as u32,
        preview: r.get(8)?,
        source: r.get(9)?,
        installable: r.get::<_, i64>(10)? != 0,
        publisher: publisher.and_then(|p| PublicKey::from_hex(&p).ok()),
    })
}

fn is_deleted(c: &rusqlite::Connection, id: &EventId) -> rusqlite::Result<bool> {
    let n: i64 = c.query_row(
        "SELECT COUNT(*) FROM gallery_deleted WHERE id = ?1",
        [id.to_hex()],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(url: &str, kind: ItemKind, name: &str) -> Item {
        Item {
            url: url.into(),
            kind,
            name: name.into(),
            author: "someone".into(),
            description: "A thing".into(),
            category: "Widgets".into(),
            tags: vec!["bar".into()],
            stars: 3,
            preview: None,
            source: "plugins".into(),
            installable: true,
            publisher: None,
        }
    }

    #[test]
    fn catalogues_replace_and_listings_only_fill_gaps() {
        let s = GalleryStore::new(Db::open_in_memory().unwrap()).unwrap();
        let a = item("https://github.com/a/a", ItemKind::Plugin, "Alpha");
        let b = item("https://github.com/b/b", ItemKind::Plugin, "Beta");
        s.replace_source("plugins", &[a.clone(), b.clone()], 1)
            .unwrap();
        assert_eq!(s.items(Some(ItemKind::Plugin), "").unwrap().len(), 2);
        assert_eq!(
            s.items(Some(ItemKind::Plugin), "alp").unwrap(),
            vec![a.clone()]
        );
        assert_eq!(
            s.items(Some(ItemKind::Plugin), "bar someone")
                .unwrap()
                .len(),
            2
        );
        assert_eq!(s.items(Some(ItemKind::Theme), "").unwrap().len(), 0);
        s.replace_source("plugins", std::slice::from_ref(&a), 2)
            .unwrap();
        assert_eq!(s.items(None, "").unwrap().len(), 1);

        let pk = Keys::generate().public_key();
        let l = Listing {
            id: EventId::from_slice(&[1; 32]).unwrap(),
            url: a.url.clone(),
            kind: ItemKind::Theme,
            name: "Renamed".into(),
            pubkey: pk,
            created_at: 5,
        };
        s.add_listing(&l, 3).unwrap();
        assert_eq!(
            s.item(&a.url).unwrap().unwrap().name,
            "Alpha",
            "the registry wins"
        );
        let l2 = Listing {
            url: "https://github.com/c/c".into(),
            ..l
        };
        s.add_listing(&l2, 3).unwrap();
        let c = s.item("https://github.com/c/c").unwrap().unwrap();
        assert_eq!(
            (c.name.as_str(), c.source.as_str(), c.publisher),
            ("Renamed", "nostr", Some(pk))
        );
        assert_eq!(s.item_count(), (1, 1));
    }

    #[test]
    fn likes_reviews_setups_and_deletions() {
        let s = GalleryStore::new(Db::open_in_memory().unwrap()).unwrap();
        let me = Keys::generate().public_key();
        let url = "https://github.com/a/a".to_string();
        let like = Like {
            id: EventId::from_slice(&[2; 32]).unwrap(),
            url: url.clone(),
            pubkey: me,
            created_at: 10,
        };
        assert!(s.put_like(&like).unwrap());
        assert!(!s.put_like(&like).unwrap(), "seen twice, counted once");
        assert_eq!(s.likes().unwrap()[&url], vec![me]);
        assert_eq!(s.my_like(&url, &me).unwrap(), Some(like.id));
        // A deletion that arrives before the like from a slower relay.
        let later = EventId::from_slice(&[3; 32]).unwrap();
        s.delete(&me, &[later, like.id]).unwrap();
        assert!(s.likes().unwrap().is_empty());
        assert!(
            !s.put_like(&Like {
                id: later,
                ..like.clone()
            })
            .unwrap()
        );
        // Someone else can't delete my like.
        assert!(
            s.put_like(&Like {
                id: EventId::from_slice(&[4; 32]).unwrap(),
                ..like.clone()
            })
            .unwrap()
        );
        s.delete(
            &Keys::generate().public_key(),
            &[EventId::from_slice(&[4; 32]).unwrap()],
        )
        .unwrap();
        assert_eq!(s.likes().unwrap()[&url].len(), 1);

        let r = Review {
            id: EventId::from_slice(&[5; 32]).unwrap(),
            url: url.clone(),
            pubkey: me,
            text: "Nice".into(),
            rating: Some(4),
            created_at: 11,
        };
        assert!(s.put_review(&r).unwrap());
        assert_eq!(s.reviews(&url).unwrap(), vec![r.clone()]);
        assert_eq!(s.review_stats().unwrap()[&url], (1, Some(4.0)));

        let setup = Setup {
            id: EventId::from_slice(&[6; 32]).unwrap(),
            pubkey: me,
            slug: "desk".into(),
            spec: SetupSpec {
                title: "Desk".into(),
                ..Default::default()
            },
            created_at: 20,
        };
        assert!(s.put_setup(&setup).unwrap());
        let older = Setup {
            id: EventId::from_slice(&[7; 32]).unwrap(),
            created_at: 19,
            ..setup.clone()
        };
        assert!(
            !s.put_setup(&older).unwrap(),
            "an older version doesn't replace a newer one"
        );
        let newer = Setup {
            id: EventId::from_slice(&[8; 32]).unwrap(),
            created_at: 21,
            spec: SetupSpec {
                title: "Desk 2".into(),
                ..Default::default()
            },
            ..setup.clone()
        };
        assert!(s.put_setup(&newer).unwrap());
        assert_eq!(s.setups().unwrap()[0].spec.title, "Desk 2");
        assert!(s.setup(&setup.coordinate()).unwrap().is_some());
    }

    #[test]
    fn people_and_meta() {
        let s = GalleryStore::new(Db::open_in_memory().unwrap()).unwrap();
        let pk = Keys::generate().public_key();
        assert_eq!(s.stale_profiles([&pk], 100, 10), vec![pk]);
        s.put_profile(
            &pk,
            &Profile {
                name: "Ann".into(),
                ..Default::default()
            },
            50,
            100,
        )
        .unwrap();
        assert_eq!(s.profile(&pk).unwrap().0.name, "Ann");
        assert!(s.stale_profiles([&pk], 105, 10).is_empty());
        assert_eq!(s.stale_profiles([&pk], 200, 10), vec![pk]);
        s.put_profile(
            &pk,
            &Profile {
                name: "Old".into(),
                ..Default::default()
            },
            40,
            200,
        )
        .unwrap();
        assert_eq!(
            s.profile(&pk).unwrap().0.name,
            "Ann",
            "older profiles don't win"
        );
        assert!(
            s.stale_profiles([&pk], 205, 10).is_empty(),
            "but the lookup counts"
        );
        s.touch_profile(&Keys::generate().public_key(), 1).unwrap();

        let f = Keys::generate().public_key();
        s.put_follows(&pk, &[f], 1, 2).unwrap();
        assert_eq!(s.follows(&pk).unwrap().0, vec![f]);
        s.put_follows(&pk, &[], 0, 3).unwrap();
        assert_eq!(
            s.follows(&pk).unwrap().0,
            vec![f],
            "an older list doesn't win"
        );

        assert!(s.meta("since").is_none());
        s.set_meta("since", "42").unwrap();
        assert_eq!(s.meta("since").as_deref(), Some("42"));
    }
}
