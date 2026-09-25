//! What goes inside each encrypted event. Everything Peridot publishes is an
//! [`Item`] sealed with the sync keys and stored under an opaque `d` tag:
//!
//! - a file entry (path, hash, and the contents inline or as chunks)
//! - a chunk of a bigger file, addressed by its hash
//! - a device (so your computers can list each other)
//! - a piece of Omarchy state (current theme, installed themes and plugins)

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::crypto::{SyncKeys, sha256_hex};

/// Files up to this size travel inside the entry itself.
pub const INLINE_MAX: usize = 16 * 1024;
/// Bigger files are split into chunks of this size. After base64, NIP-44
/// padding and base64 again, each event stays around 40 KB, under the
/// smallest message limit of common relays (64 KB).
pub const CHUNK_SIZE: usize = 20 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Item {
    File(FileEntry),
    Chunk(Chunk),
    Device(DeviceInfo),
    State(StateEntry),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative to home.
    pub path: String,
    /// Hash of the contents (empty when deleted).
    pub sha256: String,
    pub size: u64,
    /// The version this change was made on top of (for spotting conflicts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Which device made the change.
    pub device: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// Base64 contents for small files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Chunk hashes, in order, for bigger files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub sha256: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub last_seen: u64,
    /// Set when this device was removed from your devices.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StateEntry {
    /// The Omarchy theme in use.
    Theme { name: String, device: String },
    /// Themes installed from git, by name → clone URL.
    Themes { themes: Vec<Source>, device: String },
    /// Plugins installed from git.
    Plugins { plugins: Vec<Source>, device: String },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Source {
    pub name: String,
    pub url: String,
}

impl Item {
    /// The label its `d` tag is derived from.
    pub fn label(&self) -> String {
        match self {
            Item::File(f) => file_label(&f.path),
            Item::Chunk(c) => format!("c:{}", c.sha256),
            Item::Device(d) => format!("dev:{}", d.id),
            Item::State(StateEntry::Theme { .. }) => "s:theme".into(),
            Item::State(StateEntry::Themes { .. }) => "s:themes".into(),
            Item::State(StateEntry::Plugins { .. }) => "s:plugins".into(),
        }
    }
}

pub fn file_label(path: &str) -> String {
    format!("f:{path}")
}

/// An item ready to be signed: its `d` tag and sealed content.
#[derive(Debug, Clone)]
pub struct Sealed {
    pub d: String,
    pub content: String,
}

pub fn seal(keys: &SyncKeys, item: &Item) -> anyhow::Result<Sealed> {
    let json = serde_json::to_vec(&serde_json::json!({"v": 1, "item": item}))?;
    Ok(Sealed {
        d: keys.name(&item.label()),
        content: keys.seal(&json)?,
    })
}

/// Open an event's content. None if it isn't one of ours (another app's
/// data, another sync secret) or doesn't sit under the right `d` tag.
pub fn open(keys: &SyncKeys, d: &str, content: &str) -> Option<Item> {
    let bytes = keys.open(content).ok()?;
    #[derive(Deserialize)]
    struct Wrapper {
        v: u8,
        item: Item,
    }
    let w: Wrapper = serde_json::from_slice(&bytes).ok()?;
    if w.v != 1 {
        return None;
    }
    // Refuse items moved under another item's tag (e.g. a replayed entry
    // for one path posing as another).
    (keys.name(&w.item.label()) == d).then_some(w.item)
}

/// Everything needed to publish a file: its chunks (if any), then the entry.
pub fn pack_file(
    keys: &SyncKeys,
    path: &str,
    content: &[u8],
    base: Option<String>,
    device: &str,
) -> anyhow::Result<Vec<Sealed>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = Vec::new();
    let mut entry = FileEntry {
        path: path.to_string(),
        sha256: sha256_hex(content),
        size: content.len() as u64,
        base,
        device: device.to_string(),
        deleted: false,
        data: None,
        chunks: Vec::new(),
    };
    if content.len() <= INLINE_MAX {
        entry.data = Some(b64.encode(content));
    } else {
        for piece in content.chunks(CHUNK_SIZE) {
            let chunk = Chunk {
                sha256: sha256_hex(piece),
                data: b64.encode(piece),
            };
            entry.chunks.push(chunk.sha256.clone());
            out.push(seal(keys, &Item::Chunk(chunk))?);
        }
    }
    out.push(seal(keys, &Item::File(entry))?);
    Ok(out)
}

/// A tombstone: the file was deleted on `device`.
pub fn pack_deletion(
    keys: &SyncKeys,
    path: &str,
    base: Option<String>,
    device: &str,
) -> anyhow::Result<Sealed> {
    seal(
        keys,
        &Item::File(FileEntry {
            path: path.to_string(),
            sha256: String::new(),
            size: 0,
            base,
            device: device.to_string(),
            deleted: true,
            data: None,
            chunks: Vec::new(),
        }),
    )
}

/// Put a file's contents back together, checking every hash.
pub fn assemble(
    entry: &FileEntry,
    chunk: impl Fn(&str) -> Option<Chunk>,
) -> anyhow::Result<Vec<u8>> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let bytes = match &entry.data {
        Some(data) => b64.decode(data)?,
        None => {
            let mut all = Vec::with_capacity(entry.size as usize);
            for sha in &entry.chunks {
                let c = chunk(sha).ok_or_else(|| anyhow::anyhow!("a piece of it hasn't arrived yet"))?;
                let piece = b64.decode(&c.data)?;
                anyhow::ensure!(sha256_hex(&piece) == *sha, "a piece of it is damaged");
                all.extend_from_slice(&piece);
            }
            all
        }
    };
    anyhow::ensure!(
        bytes.len() as u64 == entry.size && sha256_hex(&bytes) == entry.sha256,
        "the contents don't match what was sent"
    );
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::SyncSecret;
    use std::collections::HashMap;

    #[test]
    fn small_files_travel_inline() {
        let keys = SyncSecret::generate().keys();
        let sealed = pack_file(&keys, ".XCompose", b"hello", None, "dev1").unwrap();
        assert_eq!(sealed.len(), 1);
        let Some(Item::File(entry)) = open(&keys, &sealed[0].d, &sealed[0].content) else {
            panic!("not a file entry")
        };
        assert_eq!(entry.path, ".XCompose");
        assert_eq!(assemble(&entry, |_| None).unwrap(), b"hello");
    }

    #[test]
    fn big_files_are_chunked_and_reassembled() {
        let keys = SyncSecret::generate().keys();
        let content: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let sealed = pack_file(&keys, ".config/kitty/kitty.conf", &content, None, "d").unwrap();
        assert_eq!(sealed.len(), 10 + 1);
        let mut chunks = HashMap::new();
        let mut entry = None;
        for s in &sealed {
            assert!(s.content.len() < 45_000, "fits every relay we use: {}", s.content.len());
            match open(&keys, &s.d, &s.content).unwrap() {
                Item::Chunk(c) => {
                    chunks.insert(c.sha256.clone(), c);
                }
                Item::File(f) => entry = Some(f),
                _ => unreachable!(),
            }
        }
        let entry = entry.unwrap();
        assert_eq!(assemble(&entry, |sha| chunks.get(sha).cloned()).unwrap(), content);
        // A missing piece is reported, not papered over.
        assert!(assemble(&entry, |_| None).is_err());
    }

    #[test]
    fn rejects_foreign_tampered_or_moved_items() {
        let keys = SyncSecret::generate().keys();
        let other = SyncSecret::generate().keys();
        let a = pack_file(&keys, "a", b"1", None, "d").unwrap().pop().unwrap();
        let b = pack_file(&keys, "b", b"2", None, "d").unwrap().pop().unwrap();
        assert!(open(&other, &a.d, &a.content).is_none());
        assert!(open(&keys, &b.d, &a.content).is_none(), "entry for a under b's tag");
        let mut bad = FileEntry {
            data: Some(base64::engine::general_purpose::STANDARD.encode(b"evil")),
            ..match open(&keys, &a.d, &a.content).unwrap() {
                Item::File(f) => f,
                _ => unreachable!(),
            }
        };
        assert!(assemble(&bad, |_| None).is_err(), "hash mismatch");
        bad.data = None;
        assert!(assemble(&bad, |_| None).is_err());
    }

    #[test]
    fn deletions_round_trip() {
        let keys = SyncSecret::generate().keys();
        let s = pack_deletion(&keys, ".config/foot/foot.ini", Some("abc".into()), "d").unwrap();
        let Some(Item::File(f)) = open(&keys, &s.d, &s.content) else {
            panic!()
        };
        assert!(f.deleted);
        assert_eq!(f.base.as_deref(), Some("abc"));
    }
}
