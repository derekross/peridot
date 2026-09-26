//! Private links. A file is encrypted on your computer with a one-time
//! key, the encrypted blob goes to a Blossom server (addressed by its
//! hash), and the link carries the key after the `#`, which browsers never
//! send anywhere. The viewer page on myperidot.app fetches the blob and
//! decrypts it in the browser.
//!
//! Blob layout, before encryption: `PDS1` + u32 header length + a JSON
//! header (name, type, size) + the file. Encryption is AES-256-GCM; the
//! 12-byte nonce comes first in the stored blob.
//!
//! Link: `https://myperidot.app/s#1.<sha256 of blob>.<server host>.<key>`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::{random_bytes, sha256_hex};

const MAGIC: &[u8; 4] = b"PDS1";
/// Biggest file a link can carry (Blossom servers cap uploads too).
pub const MAX_SHARE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    #[serde(rename = "type")]
    pub mime: String,
    pub size: u64,
}

/// An encrypted file ready to upload.
pub struct Sealed {
    pub blob: Vec<u8>,
    /// Hex SHA-256 of `blob` (what the server addresses it by).
    pub sha256: String,
    pub key: Zeroizing<[u8; 32]>,
}

pub fn seal(name: &str, mime: &str, data: &[u8]) -> anyhow::Result<Sealed> {
    anyhow::ensure!(
        data.len() <= MAX_SHARE_BYTES,
        "that file is too big to share (64 MB at most)"
    );
    let header = serde_json::to_vec(&Header {
        name: clean_name(name),
        mime: if mime.is_empty() {
            "application/octet-stream".into()
        } else {
            mime.into()
        },
        size: data.len() as u64,
    })?;
    let mut plain = Vec::with_capacity(8 + header.len() + data.len());
    plain.extend_from_slice(MAGIC);
    plain.extend_from_slice(&(header.len() as u32).to_be_bytes());
    plain.extend_from_slice(&header);
    plain.extend_from_slice(data);

    let key = Zeroizing::new(random_bytes::<32>());
    let nonce_bytes = random_bytes::<12>();
    let cipher = Aes256Gcm::new_from_slice(key.as_ref()).expect("32-byte key");
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plain.as_ref())
        .map_err(|_| anyhow::anyhow!("encryption failed"))?;
    let mut blob = Vec::with_capacity(12 + ciphertext.len());
    blob.extend_from_slice(&nonce_bytes);
    blob.extend_from_slice(&ciphertext);
    Ok(Sealed {
        sha256: sha256_hex(&blob),
        blob,
        key,
    })
}

/// Decrypt a blob (what the viewer page does, here for tests and the CLI).
pub fn open(key: &[u8; 32], blob: &[u8]) -> anyhow::Result<(Header, Vec<u8>)> {
    anyhow::ensure!(blob.len() > 12, "not a Peridot share");
    let cipher = Aes256Gcm::new_from_slice(key).expect("32-byte key");
    let plain = cipher
        .decrypt(Nonce::from_slice(&blob[..12]), &blob[12..])
        .map_err(|_| anyhow::anyhow!("the key doesn't open this file"))?;
    anyhow::ensure!(
        plain.len() >= 8 && &plain[..4] == MAGIC,
        "not a Peridot share"
    );
    let len = u32::from_be_bytes([plain[4], plain[5], plain[6], plain[7]]) as usize;
    anyhow::ensure!(plain.len() >= 8 + len, "not a Peridot share");
    let header: Header = serde_json::from_slice(&plain[8..8 + len])?;
    Ok((header, plain[8 + len..].to_vec()))
}

/// What a link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub sha256: String,
    /// Blossom server host, e.g. `blossom.band`.
    pub server: String,
    pub key: [u8; 32],
}

impl Link {
    pub fn to_url(&self, viewer: &str) -> String {
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(self.key);
        format!(
            "{}#1.{}.{}.{}",
            viewer.trim_end_matches('/'),
            self.sha256,
            self.server,
            key
        )
    }

    pub fn parse(url: &str) -> anyhow::Result<Self> {
        let (_, frag) = url
            .split_once('#')
            .ok_or_else(|| anyhow::anyhow!("not a share link"))?;
        let parts: Vec<&str> = frag.split('.').collect();
        // Server hosts contain dots: everything between the hash and the key.
        anyhow::ensure!(parts.len() >= 4 && parts[0] == "1", "not a share link");
        let sha256 = parts[1].to_string();
        anyhow::ensure!(
            sha256.len() == 64 && sha256.chars().all(|c| c.is_ascii_hexdigit()),
            "not a share link"
        );
        let key_b64 = parts[parts.len() - 1];
        let server = parts[2..parts.len() - 1].join(".");
        anyhow::ensure!(valid_host(&server), "not a share link");
        let key_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(key_b64)?;
        let key: [u8; 32] = key_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("not a share link"))?;
        Ok(Self {
            sha256,
            server,
            key,
        })
    }

    pub fn blob_url(&self) -> String {
        format!(
            "{}://{}/{}",
            scheme_for(&self.server),
            self.server,
            self.sha256
        )
    }
}

/// Servers are always https, except a loopback one used for testing.
pub fn scheme_for(host: &str) -> &'static str {
    let name = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if name == "127.0.0.1" || name == "localhost" {
        "http"
    } else {
        "https"
    }
}

/// `host` or `host:port`.
pub fn valid_host(h: &str) -> bool {
    let (name, port) = match h.rsplit_once(':') {
        Some((n, p)) => (n, Some(p)),
        None => (h, None),
    };
    !name.is_empty()
        && h.len() < 200
        && name.contains('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && port.is_none_or(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// File names come from the file system and end up in a browser: keep
/// them to one line of plain text.
fn clean_name(name: &str) -> String {
    let n: String = name
        .chars()
        .filter(|c| !c.is_control() && !"/\\".contains(*c))
        .take(120)
        .collect();
    if n.trim().is_empty() {
        "file".into()
    } else {
        n
    }
}

/// A guess at the media type from the name, for the viewer.
pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" | "md" | "log" | "lua" | "conf" | "toml" | "json" | "jsonc" | "yaml" | "yml"
        | "sh" | "rs" | "py" | "js" | "ts" | "css" | "html" | "csv" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens() {
        let s = seal("shot.png", "image/png", b"PNG bytes here").unwrap();
        assert_ne!(s.blob, b"PNG bytes here");
        let (h, data) = open(&s.key, &s.blob).unwrap();
        assert_eq!(h.name, "shot.png");
        assert_eq!(h.mime, "image/png");
        assert_eq!(h.size, 14);
        assert_eq!(data, b"PNG bytes here");
        assert_eq!(s.sha256, sha256_hex(&s.blob));
        // Wrong key, tampered blob.
        assert!(open(&random_bytes(), &s.blob).is_err());
        let mut bad = s.blob.clone();
        bad[20] ^= 1;
        assert!(open(&s.key, &bad).is_err());
    }

    #[test]
    fn links_round_trip() {
        let s = seal("a b/../c.txt", "", b"x").unwrap();
        let link = Link {
            sha256: s.sha256.clone(),
            server: "blossom.band".into(),
            key: *s.key,
        };
        let url = link.to_url("https://myperidot.app/s/");
        assert!(url.starts_with("https://myperidot.app/s#1."));
        assert_eq!(Link::parse(&url).unwrap(), link);
        assert_eq!(
            link.blob_url(),
            format!("https://blossom.band/{}", s.sha256)
        );
        assert!(Link::parse("https://myperidot.app/s#2.abc.x.y").is_err());
        assert!(Link::parse("https://myperidot.app/s").is_err());
        assert!(valid_host("127.0.0.1:4433") && !valid_host("host:port") && !valid_host("nodots"));
        assert_eq!(scheme_for("127.0.0.1:4433"), "http");
        assert_eq!(scheme_for("blossom.band"), "https");
        let (h, _) = open(&s.key, &s.blob).unwrap();
        assert_eq!(h.name, "a b..c.txt");
        assert_eq!(h.mime, "application/octet-stream");
    }

    #[test]
    fn refuses_huge_files() {
        let big = vec![0u8; MAX_SHARE_BYTES + 1];
        assert!(seal("big", "", &big).is_err());
    }
}
