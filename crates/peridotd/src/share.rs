//! Private links: encrypt a file, put the blob on a Blossom server, hand
//! back a link with the key after the `#`. Shares are remembered so they
//! can be listed, revoked, and removed from the server when they expire.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine;
use nostr_sdk::prelude::*;
use opal_core::db::Db;
use peridot_sync::share::{self, Link, Sealed};
use peridot_sync::signer::{IdentitySigner, SignError};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

const KIND_BLOSSOM_AUTH: u16 = 24242;
const HTTP_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a signed Blossom authorization stays valid. Each attempt at
/// each server gets a fresh one, so a captured header is worth little.
const AUTH_TTL: u64 = 300;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub id: i64,
    pub sha256: String,
    pub server: String,
    pub name: String,
    pub size: u64,
    pub created: u64,
    pub expires: u64,
    pub url: String,
    pub revoked: bool,
}

#[derive(Clone)]
pub struct ShareStore {
    db: Db,
}

impl ShareStore {
    pub fn new(db: Db) -> opal_core::Result<Self> {
        db.migrate(
            "peridot.share",
            &["CREATE TABLE shares (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                sha256 TEXT NOT NULL,
                server TEXT NOT NULL,
                name TEXT NOT NULL,
                size INTEGER NOT NULL,
                created INTEGER NOT NULL,
                expires INTEGER NOT NULL,
                url TEXT NOT NULL,
                revoked INTEGER NOT NULL DEFAULT 0
            );"],
        )?;
        Ok(Self { db })
    }

    fn add(&self, s: &Share) -> opal_core::Result<i64> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO shares (sha256, server, name, size, created, expires, url)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    s.sha256,
                    s.server,
                    s.name,
                    s.size as i64,
                    s.created as i64,
                    s.expires as i64,
                    s.url
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn list(&self) -> opal_core::Result<Vec<Share>> {
        self.db.with(|c| {
            let mut st = c.prepare(
                "SELECT id, sha256, server, name, size, created, expires, url, revoked
                 FROM shares ORDER BY id DESC LIMIT 200",
            )?;
            st.query_map([], row).map(|r| r.collect())?
        })
    }

    pub fn get(&self, id: i64) -> opal_core::Result<Option<Share>> {
        self.db.with(|c| {
            c.query_row(
                "SELECT id, sha256, server, name, size, created, expires, url, revoked
                 FROM shares WHERE id = ?1",
                [id],
                row,
            )
            .optional()
        })
    }

    fn mark_revoked(&self, id: i64) -> opal_core::Result<()> {
        self.db.with(|c| {
            c.execute("UPDATE shares SET revoked = 1 WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// Shares past their time that are still on a server.
    pub fn expired(&self, now: u64) -> opal_core::Result<Vec<Share>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|s| !s.revoked && s.expires <= now)
            .collect())
    }
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Share> {
    Ok(Share {
        id: r.get(0)?,
        sha256: r.get(1)?,
        server: r.get(2)?,
        name: r.get(3)?,
        size: r.get::<_, i64>(4)? as u64,
        created: r.get::<_, i64>(5)? as u64,
        expires: r.get::<_, i64>(6)? as u64,
        url: r.get(7)?,
        revoked: r.get::<_, i64>(8)? != 0,
    })
}

pub struct Sharer {
    pub store: ShareStore,
    pub signer: Arc<dyn IdentitySigner>,
    /// Blossom servers to try, in order (`https://host`).
    pub servers: Vec<String>,
    /// The viewer page links point at.
    pub viewer: String,
    http: reqwest::Client,
    /// After the signer said no to a removal (Opal asks about Blossom
    /// authorizations), the sweeper waits until then before asking again.
    retry_after: std::sync::atomic::AtomicU64,
}

/// How long the sweeper leaves you alone after a "no".
const SWEEP_HOLD: Duration = Duration::from_secs(3600);

impl Sharer {
    pub fn new(
        store: ShareStore,
        signer: Arc<dyn IdentitySigner>,
        servers: Vec<String>,
        viewer: String,
    ) -> anyhow::Result<Self> {
        opal_core::identity::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            store,
            signer,
            servers,
            viewer,
            http,
            retry_after: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn held(&self, now: u64) -> bool {
        self.retry_after.load(Ordering::SeqCst) > now
    }

    /// Expired links still on their servers because the signer said no.
    pub fn waiting(&self) -> usize {
        let now = Timestamp::now().as_secs();
        if !self.held(now) {
            return 0;
        }
        self.store.expired(now).map(|v| v.len()).unwrap_or(0)
    }

    /// You asked: try the removals again now.
    pub fn clear_hold(&self) {
        self.retry_after.store(0, Ordering::SeqCst);
    }

    /// Encrypt, upload to the first server that takes it, remember it.
    pub async fn share(
        &self,
        name: &str,
        mime: &str,
        data: &[u8],
        keep_for: Duration,
    ) -> anyhow::Result<Share> {
        let sealed = share::seal(name, mime, data)?;
        let now = Timestamp::now().as_secs();
        let expires = now + keep_for.as_secs();
        let mut last_err = anyhow::anyhow!("no share server is configured");
        for server in &self.servers {
            match self.upload(server, &sealed).await {
                Ok(()) => {
                    let host = server
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .trim_end_matches('/')
                        .to_string();
                    let link = Link {
                        sha256: sealed.sha256.clone(),
                        server: host.clone(),
                        key: *sealed.key,
                    };
                    let mut s = Share {
                        id: 0,
                        sha256: sealed.sha256.clone(),
                        server: host,
                        name: name.to_string(),
                        size: data.len() as u64,
                        created: now,
                        expires,
                        url: link.to_url(&self.viewer),
                        revoked: false,
                    };
                    s.id = self.store.add(&s)?;
                    return Ok(s);
                }
                Err(e) => {
                    tracing::warn!("upload to {server} failed: {e}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Remove the blob from its server and forget the link.
    pub async fn revoke(&self, id: i64) -> anyhow::Result<()> {
        let s = self
            .store
            .get(id)?
            .ok_or_else(|| anyhow::anyhow!("no such share"))?;
        if !s.revoked {
            self.delete(&server_base(&s.server), &s.sha256).await?;
        }
        self.store.mark_revoked(id)?;
        Ok(())
    }

    /// Remove blobs whose time is up. Returns how many were removed. One
    /// refused signature stops the round and holds the sweeper for an hour:
    /// each removal would otherwise be its own prompt in Opal.
    pub async fn sweep(&self) -> usize {
        let now = Timestamp::now().as_secs();
        if self.held(now) {
            return 0;
        }
        let mut n = 0;
        for s in self.store.expired(now).unwrap_or_default() {
            match self.delete(&server_base(&s.server), &s.sha256).await {
                Ok(()) => {
                    let _ = self.store.mark_revoked(s.id);
                    n += 1;
                }
                Err(e) if e.downcast_ref::<SignError>().is_some() => {
                    tracing::info!("expired share {} stays until the signer allows: {e}", s.id);
                    self.retry_after
                        .store(now + SWEEP_HOLD.as_secs(), Ordering::SeqCst);
                    break;
                }
                Err(e) => tracing::debug!("couldn't remove expired share {}: {e}", s.id),
            }
        }
        n
    }

    /// Send a link to someone as a private message (NIP-17). Their inbox
    /// relays are looked up through `client`; `fallback` relays get the
    /// message when they list none, and always get your own copy.
    pub async fn send(
        &self,
        id: i64,
        to: PublicKey,
        client: &Client,
        fallback: &[RelayUrl],
    ) -> anyhow::Result<Share> {
        let s = self
            .store
            .get(id)?
            .ok_or_else(|| anyhow::anyhow!("no such link"))?;
        anyhow::ensure!(!s.revoked, "that link was removed");
        anyhow::ensure!(
            s.expires > Timestamp::now().as_secs(),
            "that link has expired; share the file again"
        );
        let text = format!(
            "{}\n{}\n\nA private link from Peridot: it opens in your browser and works until {}.",
            s.name,
            s.url,
            ymd(s.expires)
        );
        let wrapped = peridot_sync::dm::wrap(self.signer.clone(), to, &text)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let inbox = peridot_sync::dm::inbox_relays(client, to, fallback).await;
        for r in &inbox {
            let _ = client.add_relay(r).await;
        }
        client
            .connect()
            .and_wait(opal_kit::relays::CONNECT_WAIT)
            .await;
        opal_kit::relays::publish(client, &wrapped.to_them, &inbox)
            .await
            .map_err(|e| anyhow::anyhow!("couldn't deliver it: {e}"))?;
        // Your copy is best effort.
        let _ = opal_kit::relays::publish(client, &wrapped.to_me, fallback).await;
        Ok(s)
    }

    /// Upload something for everyone to see (a setup's screenshot), to the
    /// first server that takes it. Returns its address.
    pub async fn upload_public(&self, data: &[u8], mime: &str) -> anyhow::Result<String> {
        let sha256 = peridot_sync::crypto::sha256_hex(data);
        let mut last_err = anyhow::anyhow!("no share server is configured");
        for server in &self.servers {
            let auth = self
                .auth("upload", &sha256, Timestamp::now().as_secs() + AUTH_TTL)
                .await?;
            let res = self
                .http
                .put(format!("{}/upload", server.trim_end_matches('/')))
                .header("Authorization", auth)
                .header("Content-Type", mime)
                .header("X-SHA-256", &sha256)
                .body(data.to_vec())
                .send()
                .await;
            match res {
                Ok(res) if res.status().is_success() => {
                    let desc: serde_json::Value = res.json().await.unwrap_or_default();
                    return Ok(public_url(server, &sha256, mime, desc["url"].as_str()));
                }
                Ok(res) => {
                    last_err = anyhow::anyhow!("{server} refused the upload ({})", res.status())
                }
                Err(e) => last_err = e.into(),
            }
        }
        Err(last_err)
    }

    /// BUD-01: `PUT /upload` with a signed kind 24242 authorization.
    async fn upload(&self, server: &str, sealed: &Sealed) -> anyhow::Result<()> {
        let auth = self
            .auth(
                "upload",
                &sealed.sha256,
                Timestamp::now().as_secs() + AUTH_TTL,
            )
            .await?;
        let res = self
            .http
            .put(format!("{}/upload", server.trim_end_matches('/')))
            .header("Authorization", auth)
            .header("Content-Type", "application/octet-stream")
            .header("X-SHA-256", &sealed.sha256)
            .body(sealed.blob.clone())
            .send()
            .await?;
        let status = res.status();
        let reason = res
            .headers()
            .get("x-reason")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        if status.as_u16() == 415 {
            // Media hosts check for a picture or a video; an encrypted blob
            // is neither.
            anyhow::bail!(
                "{server} only takes pictures and videos, so it can't hold a private link"
            );
        }
        if !status.is_success() {
            anyhow::bail!(
                "{server} refused the upload ({status}{})",
                reason.map(|r| format!(": {r}")).unwrap_or_default()
            );
        }
        let desc: serde_json::Value = res.json().await.unwrap_or_default();
        if let Some(sha) = desc["sha256"].as_str()
            && sha != sealed.sha256
        {
            anyhow::bail!("{server} stored something other than what was sent");
        }
        Ok(())
    }

    /// BUD-02: `DELETE /<sha256>` with a signed authorization.
    async fn delete(&self, server: &str, sha256: &str) -> anyhow::Result<()> {
        let auth = self
            .auth("delete", sha256, Timestamp::now().as_secs() + AUTH_TTL)
            .await?;
        let res = self
            .http
            .delete(format!("{}/{sha256}", server.trim_end_matches('/')))
            .header("Authorization", auth)
            .send()
            .await?;
        // Already gone is fine.
        if !res.status().is_success() && res.status().as_u16() != 404 {
            anyhow::bail!("{server} refused the removal ({})", res.status());
        }
        Ok(())
    }

    async fn auth(&self, verb: &str, sha256: &str, expiration: u64) -> anyhow::Result<String> {
        let unsigned = EventBuilder::new(Kind::Custom(KIND_BLOSSOM_AUTH), "Peridot share")
            .tag(Tag::parse(["t", verb])?)
            .tag(Tag::parse(["x", sha256])?)
            .tag(Tag::parse(["expiration", &expiration.to_string()])?)
            .finalize_unsigned(self.signer.pubkey());
        let ev = self.signer.sign(unsigned).await?;
        Ok(format!(
            "Nostr {}",
            base64::engine::general_purpose::STANDARD.encode(ev.as_json())
        ))
    }
}

/// Where a public upload ended up: the server's word for it when that's
/// an https address naming the blob by its hash, else the address every
/// Blossom server serves it at.
fn public_url(server: &str, sha256: &str, mime: &str, returned: Option<&str>) -> String {
    if let Some(url) = returned
        && url.starts_with("https://")
        && url.contains(sha256)
    {
        return url.to_string();
    }
    let ext = mime.split('/').nth(1).unwrap_or("bin");
    format!("{}/{sha256}.{ext}", server.trim_end_matches('/'))
}

/// `YYYY-MM-DD` (UTC).
fn ymd(secs: u64) -> String {
    crate::api::ymd(secs)
}

fn server_base(host: &str) -> String {
    format!("{}://{host}", share::scheme_for(host))
}

/// Remove expired blobs from their servers, every ten minutes. Stops when
/// the sharer is dropped (leave, or a new identity).
pub fn spawn_sweeper(sharer: Arc<Sharer>) {
    let weak = Arc::downgrade(&sharer);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(600));
        loop {
            tick.tick().await;
            let Some(s) = weak.upgrade() else { break };
            let n = s.sweep().await;
            if n > 0 {
                tracing::info!("removed {n} expired share link(s)");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// A server that answers every request the same way and keeps the
    /// authorization events it was shown.
    async fn answering(
        status: &'static str,
        body: &'static str,
    ) -> (String, Arc<Mutex<Vec<Event>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let log = log.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    let (head, len) = loop {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..i]).to_string();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                                })
                                .unwrap_or(0);
                            break (head, i + 4 + len);
                        }
                    };
                    while buf.len() < len {
                        let n = s.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    if let Some(a) = head.lines().find_map(|l| {
                        l.split_once(':')
                            .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                            .map(|(_, v)| v.trim().to_string())
                    }) && let Some(b64) = a.strip_prefix("Nostr ")
                        && let Ok(json) = base64::engine::general_purpose::STANDARD.decode(b64)
                        && let Ok(ev) = Event::from_json(&json)
                    {
                        log.lock().unwrap().push(ev);
                    }
                    let r = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = s.write_all(r.as_bytes()).await;
                });
            }
        });
        (base, seen)
    }

    fn expiration(ev: &Event) -> u64 {
        ev.tags
            .iter()
            .find(|t| t.kind().to_string() == "expiration")
            .and_then(|t| t.content())
            .and_then(|v| v.parse().ok())
            .unwrap()
    }

    #[tokio::test]
    async fn each_attempt_gets_a_fresh_short_lived_authorization() {
        let (first, seen_first) = answering("500 Internal Server Error", "").await;
        let (second, seen_second) = answering("500 Internal Server Error", "").await;
        let keys = Keys::generate();
        let sharer = Sharer::new(
            ShareStore::new(Db::open_in_memory().unwrap()).unwrap(),
            Arc::new(peridot_sync::signer::LocalSigner(keys)),
            vec![first, second],
            "https://example.test/s".into(),
        )
        .unwrap();
        let now = Timestamp::now().as_secs();
        // A link kept for a week: the authorizations still last minutes.
        assert!(
            sharer
                .share(
                    "x.txt",
                    "text/plain",
                    b"hello",
                    Duration::from_secs(7 * 86_400)
                )
                .await
                .is_err()
        );
        let a = seen_first.lock().unwrap().clone();
        let b = seen_second.lock().unwrap().clone();
        assert_eq!((a.len(), b.len()), (1, 1));
        for ev in [&a[0], &b[0]] {
            assert!(ev.verify().is_ok());
            assert_eq!(ev.kind.as_u16(), KIND_BLOSSOM_AUTH);
            let exp = expiration(ev);
            assert!(
                (now + AUTH_TTL - 5..=now + AUTH_TTL + 5).contains(&exp),
                "{exp}"
            );
        }

        // The same for public uploads.
        assert!(sharer.upload_public(b"png", "image/png").await.is_err());
        let a = seen_first.lock().unwrap().clone();
        let b = seen_second.lock().unwrap().clone();
        assert_eq!((a.len(), b.len()), (2, 2));
        assert!(expiration(&a[1]) <= now + AUTH_TTL + 5);
    }

    #[tokio::test]
    async fn a_public_upload_is_addressed_by_its_hash() {
        let sha = peridot_sync::crypto::sha256_hex(b"png");
        // The server names it something else: the address is built here.
        let (server, _) = answering(
            "200 OK",
            r#"{"url":"https://cdn.example/somewhere-else.png"}"#,
        )
        .await;
        let sharer = Sharer::new(
            ShareStore::new(Db::open_in_memory().unwrap()).unwrap(),
            Arc::new(peridot_sync::signer::LocalSigner(Keys::generate())),
            vec![server.clone()],
            "https://example.test/s".into(),
        )
        .unwrap();
        assert_eq!(
            sharer.upload_public(b"png", "image/png").await.unwrap(),
            format!("{server}/{sha}.png")
        );
        assert_eq!(
            public_url(
                "https://b.example/",
                &sha,
                "image/png",
                Some(&format!("https://b.example/{sha}"))
            ),
            format!("https://b.example/{sha}")
        );
        assert_eq!(
            public_url(
                "https://b.example",
                &sha,
                "image/png",
                Some(&format!("http://b.example/{sha}.png"))
            ),
            format!("https://b.example/{sha}.png"),
            "not over plain http"
        );
        assert_eq!(
            public_url("https://b.example", &sha, "image/jpeg", None),
            format!("https://b.example/{sha}.jpeg")
        );
    }

    /// A signer that always says "not now", counting the asks.
    struct SaysNo(AtomicUsize);

    impl peridot_sync::signer::EventSigner for SaysNo {
        fn sign(&self, _: UnsignedEvent) -> BoxFuture<'_, Result<Event, SignError>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(SignError::Unavailable("Opal didn't allow it".into())) })
        }
    }

    impl IdentitySigner for SaysNo {
        fn nip44_encrypt_to(
            &self,
            _peer: PublicKey,
            _plaintext: String,
        ) -> futures::future::BoxFuture<'_, Result<String, SignError>> {
            Box::pin(async { Err(SignError::Failed("no".into())) })
        }

        fn pubkey(&self) -> PublicKey {
            Keys::generate().public_key()
        }
        fn nip44_self_encrypt(&self, _: String) -> BoxFuture<'_, Result<String, SignError>> {
            Box::pin(async { Err(SignError::Failed("no".into())) })
        }
        fn nip44_self_decrypt(&self, _: String) -> BoxFuture<'_, Result<String, SignError>> {
            Box::pin(async { Err(SignError::Failed("no".into())) })
        }
    }

    #[tokio::test]
    async fn sweep_backs_off_when_the_signer_says_no() {
        let store = ShareStore::new(Db::open_in_memory().unwrap()).unwrap();
        let signer = Arc::new(SaysNo(AtomicUsize::new(0)));
        let sharer = Sharer::new(
            store,
            signer.clone(),
            vec!["http://127.0.0.1:9".into()],
            "https://example.test/s".into(),
        )
        .unwrap();
        let now = Timestamp::now().as_secs();
        for i in 0..2 {
            sharer
                .store
                .add(&Share {
                    id: 0,
                    sha256: format!("{i:0>64}"),
                    server: "127.0.0.1:9".into(),
                    name: "x".into(),
                    size: 1,
                    created: now - 100,
                    expires: now - 10,
                    url: String::new(),
                    revoked: false,
                })
                .unwrap();
        }
        assert_eq!(sharer.waiting(), 0, "nothing is held yet");
        assert_eq!(sharer.sweep().await, 0);
        assert_eq!(signer.0.load(Ordering::SeqCst), 1, "one ask, then stop");
        assert_eq!(sharer.waiting(), 2);
        assert_eq!(sharer.sweep().await, 0);
        assert_eq!(signer.0.load(Ordering::SeqCst), 1, "held: no ask");
        sharer.clear_hold();
        assert_eq!(sharer.sweep().await, 0);
        assert_eq!(signer.0.load(Ordering::SeqCst), 2, "asked again on request");
    }
}
