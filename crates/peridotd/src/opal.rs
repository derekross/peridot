//! Using an identity that Opal holds. Peridot never sees the key.
//!
//! Peridot pairs with Opal once: you approve it in Opal's bar, where Opal
//! shows which program is asking and what it may sign. Opal hands back a
//! token, kept in the keyring, that goes with every request. Each request
//! then passes through Opal's own rules for Peridot: syncing settings can be
//! allowed for good; relay logins and share uploads are sensitive and may
//! ask each time. Opal lists Peridot under Apps, with everything it signed,
//! and can revoke it there; Peridot then asks to pair again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use nostr_sdk::prelude::*;
use opal_core::ipc::{IpcMessage, IpcRequest};
use opal_core::keystore::{ItemKind, SecretStore};
use peridot_sync::signer::{EventSigner, IdentitySigner, SignError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::Notify;
use zeroize::Zeroizing;

/// How Peridot introduces itself to Opal.
pub const APP_KEY: &str = "peridot";
pub const APP_NAME: &str = "Peridot";
/// The event kinds Peridot signs: its synced data (NIP-78), relay logins
/// (NIP-42) and Blossom upload/delete authorizations (private links).
pub const KINDS: [u16; 3] = [30078, 22242, 24242];

/// Long enough for you to read and answer a prompt in Opal's bar.
pub const PROMPT_TIMEOUT: Duration = Duration::from_secs(180);
/// Longer than Opal's own pairing timeout (5 minutes), so its answer is the
/// definitive one; a hung Opal still can't hang a request forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(320);
/// After a "no" (or an unanswered prompt), background work waits this long
/// before asking Opal again, so an absent user doesn't get a wall of prompts.
const HOLD_AFTER_NO: Duration = Duration::from_secs(3600);

const TOKEN_ID: &str = "main";
const TOKEN_LABEL: &str = "Peridot's pairing with Opal";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpalAccount {
    pub pubkey: String,
    pub label: String,
    #[serde(default)]
    pub npub: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    #[serde(default)]
    pub current: bool,
}

/// What `app.connect` gives back.
pub struct Paired {
    pub token: Zeroizing<String>,
    pub pubkey: PublicKey,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AppStatus {
    pub pubkey: String,
    pub name: String,
    pub policy: String,
}

/// Where a request comes from: the background sync (which must never nag)
/// or something you just asked for (which may wait on a prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Background,
    Interactive,
}

/// The panel's view of the pairing.
#[derive(Debug, Clone, Serialize)]
pub struct OpalView {
    /// Peridot holds a token (Opal may still have revoked it).
    pub paired: bool,
    /// Opal said the token is no good, or there is none: pair again.
    pub needs_pairing: bool,
    /// A prompt is (probably) open in Opal's bar right now.
    pub waiting_approval: bool,
    pub pending_prompts: usize,
    /// Background requests wait until then, after a "no".
    pub held_until: Option<u64>,
    /// Why the last pairing didn't happen.
    pub pair_error: Option<String>,
}

pub async fn load_token(store: &SecretStore) -> anyhow::Result<Option<Zeroizing<String>>> {
    Ok(store.get(ItemKind::AppToken, TOKEN_ID).await?)
}

pub async fn save_token(store: &SecretStore, token: &str) -> anyhow::Result<()> {
    store
        .put(ItemKind::AppToken, TOKEN_ID, TOKEN_LABEL, token)
        .await?;
    Ok(())
}

pub async fn forget_token(store: &SecretStore) -> anyhow::Result<()> {
    store.delete(ItemKind::AppToken, TOKEN_ID).await?;
    Ok(())
}

struct Inner {
    socket: PathBuf,
    token: RwLock<Option<Zeroizing<String>>>,
    unpaired: AtomicBool,
    connecting: AtomicBool,
    prompting: AtomicUsize,
    held_until: AtomicU64,
    auto_pair_tried: AtomicBool,
    /// The first data signature since pairing went through: from then on
    /// rules are in place and the short timeout is enough.
    signed_once: AtomicBool,
    pair_error: Mutex<Option<String>>,
    changed: Notify,
}

/// One shared view of the pairing for the sync engine, the sharer and the
/// daemon, so a token learnt or lost in one place is known everywhere.
#[derive(Clone)]
pub struct OpalClient(Arc<Inner>);

impl OpalClient {
    pub fn new(socket: &Path) -> Self {
        Self(Arc::new(Inner {
            socket: socket.to_path_buf(),
            token: RwLock::new(None),
            unpaired: AtomicBool::new(false),
            connecting: AtomicBool::new(false),
            prompting: AtomicUsize::new(0),
            held_until: AtomicU64::new(0),
            auto_pair_tried: AtomicBool::new(false),
            signed_once: AtomicBool::new(false),
            pair_error: Mutex::new(None),
            changed: Notify::new(),
        }))
    }

    /// One request, one reply. Every call opens its own connection: they're
    /// rare, and a long-lived one would need reconnect handling.
    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        tokio::time::timeout(CALL_TIMEOUT, self.call_inner(method, params))
            .await
            .map_err(|_| anyhow::anyhow!("Opal didn't answer"))?
    }

    async fn call_inner(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let stream = UnixStream::connect(&self.0.socket)
            .await
            .map_err(|_| anyhow::anyhow!("Opal isn't running"))?;
        let (r, mut w) = stream.into_split();
        let mut line = Zeroizing::new(serde_json::to_string(&IpcRequest {
            id: 1,
            method: method.into(),
            params,
        })?);
        line.push('\n');
        w.write_all(line.as_bytes()).await?;
        let mut reader = BufReader::new(r);
        let mut buf = String::new();
        loop {
            buf.clear();
            if reader.read_line(&mut buf).await? == 0 {
                anyhow::bail!("Opal closed the connection");
            }
            if let IpcMessage::Response(resp) = serde_json::from_str(&buf)? {
                return match resp.error {
                    Some(e) => Err(anyhow::anyhow!(e)),
                    None => Ok(resp.result.unwrap_or(Value::Null)),
                };
            }
        }
    }

    /// Opal's accounts, or an empty list when Opal isn't running or has
    /// none.
    pub async fn accounts(&self) -> Vec<OpalAccount> {
        let Ok(status) = self.call("status", Value::Null).await else {
            return Vec::new();
        };
        serde_json::from_value(status["accounts"].clone()).unwrap_or_default()
    }

    pub async fn has_account(&self, pubkey: &PublicKey) -> bool {
        let hex = pubkey.to_hex();
        self.accounts().await.iter().any(|a| a.pubkey == hex)
    }

    /// Pair: Opal shows a prompt in the bar and answers when you do. On
    /// success the token is kept here (the caller saves it).
    pub async fn connect(&self, pubkey: Option<&PublicKey>) -> anyhow::Result<Paired> {
        if self.0.connecting.swap(true, Ordering::SeqCst) {
            anyhow::bail!("Peridot is already waiting for your answer in Opal");
        }
        self.notify();
        let mut params = json!({
            "app": APP_KEY,
            "name": APP_NAME,
            "kinds": KINDS,
            "nip44": true,
        });
        if let Some(pk) = pubkey {
            params["pubkey"] = json!(pk.to_hex());
        }
        let result = self.call("app.connect", params).await;
        self.0.connecting.store(false, Ordering::SeqCst);
        let result = result.and_then(|v| {
            let token = v["token"]
                .as_str()
                .filter(|t| t.len() == 64)
                .ok_or_else(|| anyhow::anyhow!("Opal sent no token"))?;
            let pubkey = PublicKey::from_hex(v["pubkey"].as_str().unwrap_or_default())?;
            Ok(Paired {
                token: Zeroizing::new(token.to_string()),
                pubkey,
            })
        });
        match &result {
            Ok(p) => {
                self.set_token(Some(p.token.clone()));
                *lock(&self.0.pair_error) = None;
            }
            Err(e) => *lock(&self.0.pair_error) = Some(pair_error_text(&e.to_string())),
        }
        self.notify();
        result
    }

    /// Is the pairing still good, and for whom?
    pub async fn status(&self) -> Result<AppStatus, SignError> {
        let v = self
            .app_call("app.status", json!({}), Mode::Interactive, false)
            .await?;
        serde_json::from_value(v).map_err(|e| SignError::Failed(e.to_string()))
    }

    /// A token from the keyring (or none). Clears every "no" from before.
    pub fn set_token(&self, token: Option<Zeroizing<String>>) {
        let has = token.is_some();
        *self.0.token.write().unwrap_or_else(|p| p.into_inner()) = token;
        self.0.unpaired.store(!has, Ordering::SeqCst);
        self.0.held_until.store(0, Ordering::SeqCst);
        self.0.auto_pair_tried.store(false, Ordering::SeqCst);
        self.0.signed_once.store(false, Ordering::SeqCst);
        if !has {
            *lock(&self.0.pair_error) = None;
        }
        self.notify();
    }

    pub fn has_token(&self) -> bool {
        self.0
            .token
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Opal no longer accepts the token (revoked, or a different program
    /// paired): pair again.
    pub fn mark_unpaired(&self) {
        if !self.0.unpaired.swap(true, Ordering::SeqCst) {
            self.notify();
        }
    }

    pub fn needs_pairing(&self) -> bool {
        self.0.unpaired.load(Ordering::SeqCst)
    }

    /// One automatic re-pair per "unpaired" episode; after that it's up to
    /// you (the panel's button, `peridot opal pair`).
    pub fn take_auto_pair(&self) -> bool {
        self.needs_pairing() && !self.0.auto_pair_tried.swap(true, Ordering::SeqCst)
    }

    /// Something you asked for: try Opal again even after a "no".
    pub fn clear_hold(&self) {
        self.0.held_until.store(0, Ordering::SeqCst);
    }

    #[cfg(test)]
    fn is_held(&self) -> bool {
        self.held_until().is_some()
    }

    fn held_until(&self) -> Option<u64> {
        let until = self.0.held_until.load(Ordering::SeqCst);
        (until > now_secs()).then_some(until)
    }

    fn hold(&self) {
        self.0
            .held_until
            .store(now_secs() + HOLD_AFTER_NO.as_secs(), Ordering::SeqCst);
    }

    pub fn view(&self) -> OpalView {
        OpalView {
            paired: self.has_token() && !self.needs_pairing(),
            needs_pairing: self.needs_pairing(),
            waiting_approval: self.0.connecting.load(Ordering::SeqCst)
                || self.0.prompting.load(Ordering::SeqCst) > 0,
            pending_prompts: self.0.prompting.load(Ordering::SeqCst),
            held_until: self.held_until(),
            pair_error: lock(&self.0.pair_error).clone(),
        }
    }

    /// Fires whenever the pairing state changes (the daemon re-emits state).
    pub fn changed(&self) -> &Notify {
        &self.0.changed
    }

    fn notify(&self) {
        self.0.changed.notify_one();
    }

    /// A request with the token. `prompt_prone` marks the ones Opal is
    /// likely to ask about, so the panel can say "look at Opal".
    async fn app_call(
        &self,
        method: &str,
        mut params: Value,
        mode: Mode,
        prompt_prone: bool,
    ) -> Result<Value, SignError> {
        let token = self
            .0
            .token
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let Some(token) = token else {
            self.mark_unpaired();
            return Err(SignError::Unavailable(PAIR_AGAIN.into()));
        };
        if mode == Mode::Background
            && let Some(until) = self.held_until()
        {
            return Err(SignError::Unavailable(hold_text(until, method)));
        }
        params["token"] = json!(token.as_str());
        if prompt_prone {
            self.0.prompting.fetch_add(1, Ordering::SeqCst);
            self.notify();
        }
        let result = self.call(method, params).await;
        if prompt_prone {
            self.0.prompting.fetch_sub(1, Ordering::SeqCst);
            self.notify();
        }
        match result {
            Ok(v) => Ok(v),
            Err(e) => Err(self.classify(&e.to_string(), mode, &token)),
        }
    }

    /// Opal's "come back later" answers are [`SignError::Unavailable`];
    /// everything else won't fix itself. `used` is the token the request
    /// went out with: a "not paired" for a token that has since been
    /// replaced (a request in flight during a re-pair) says nothing about
    /// the new one.
    fn classify(&self, msg: &str, mode: Mode, used: &str) -> SignError {
        if msg.contains("not paired") || msg.contains("different program") {
            let current = self
                .0
                .token
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .as_deref()
                .map(String::as_str)
                == Some(used);
            if current {
                self.mark_unpaired();
            }
            SignError::Unavailable(PAIR_AGAIN.into())
        } else if msg.contains("locked") {
            SignError::Unavailable("Unlock Opal to keep syncing".into())
        } else if msg.contains("isn't running") || msg.contains("didn't answer") {
            SignError::Unavailable("Opal isn't running; start it to keep syncing".into())
        } else if msg.contains("user rejected")
            || msg.contains("denied")
            || msg.contains("too many pending")
        {
            match mode {
                Mode::Background => {
                    self.hold();
                    self.notify();
                    SignError::Unavailable(format!(
                        "Opal didn't allow Peridot to sign; it will ask again {}. Change what Peridot may do under Apps in Opal",
                        in_words(self.held_until().unwrap_or(0))
                    ))
                }
                Mode::Interactive => SignError::Failed("Opal didn't allow it".into()),
            }
        } else if msg.contains("timed out") {
            match mode {
                Mode::Background => {
                    self.hold();
                    self.notify();
                    SignError::Unavailable(format!(
                        "Opal's prompt wasn't answered; Peridot will ask again {}",
                        in_words(self.held_until().unwrap_or(0))
                    ))
                }
                Mode::Interactive => {
                    SignError::Failed("Opal's prompt wasn't answered; try again".into())
                }
            }
        } else {
            SignError::Failed(format!("Opal: {msg}"))
        }
    }
}

const PAIR_AGAIN: &str = "Pair Peridot with Opal to keep syncing";

fn hold_text(until: u64, method: &str) -> String {
    let what = if method == "app.nip44" {
        "finish setting up"
    } else {
        "sync"
    };
    format!(
        "Opal said no earlier; Peridot will ask again to {what} {} (sync now to ask sooner)",
        in_words(until)
    )
}

fn pair_error_text(msg: &str) -> String {
    if msg.contains("declined") {
        "You declined in Opal. Nothing changed.".into()
    } else if msg.contains("timed out") || msg.contains("didn't answer") {
        "Opal's prompt wasn't answered.".into()
    } else if msg.contains("already waiting") {
        "Peridot is already waiting for your answer in Opal.".into()
    } else if msg.contains("isn't running") {
        "Opal isn't running.".into()
    } else if msg.contains("signer module") {
        "Opal's signer is turned off; turn it on in Opal's settings.".into()
    } else {
        format!("Opal: {msg}")
    }
}

fn now_secs() -> u64 {
    Timestamp::now().as_secs()
}

/// "in 58 minutes", for a hold that ends at `until`.
fn in_words(until: u64) -> String {
    let mins = until.saturating_sub(now_secs()).div_ceil(60).max(1);
    if mins == 1 {
        "in a minute".into()
    } else {
        format!("in {mins} minutes")
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Signs as an Opal account.
pub struct OpalSigner {
    client: OpalClient,
    pubkey: PublicKey,
    mode: Mode,
}

impl OpalSigner {
    pub fn new(client: OpalClient, pubkey: PublicKey, mode: Mode) -> Self {
        Self {
            client,
            pubkey,
            mode,
        }
    }

    async fn nip44(&self, op: &str, content: String) -> Result<String, SignError> {
        let v = self
            .client
            .app_call(
                "app.nip44",
                json!({"op": op, "content": content}),
                self.mode,
                op == "decrypt",
            )
            .await?;
        v["content"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| SignError::Failed("Opal sent no content".into()))
    }
}

impl EventSigner for OpalSigner {
    fn sign(&self, unsigned: UnsignedEvent) -> BoxFuture<'_, Result<Event, SignError>> {
        Box::pin(async move {
            let kind = unsigned.kind.as_u16();
            // Sensitive kinds: Opal asks unless allowed for a while.
            let prompt_prone = kind == 22242 || kind == 24242;
            let v = self
                .client
                .app_call(
                    "app.sign",
                    json!({"event": unsigned}),
                    self.mode,
                    prompt_prone,
                )
                .await?;
            let ev: Event =
                serde_json::from_value(v).map_err(|e| SignError::Failed(e.to_string()))?;
            if ev.pubkey != self.pubkey {
                return Err(SignError::Failed("Opal signed with another key".into()));
            }
            ev.verify().map_err(|e| SignError::Failed(e.to_string()))?;
            if kind == 30078 {
                self.client.0.signed_once.store(true, Ordering::SeqCst);
            }
            Ok(ev)
        })
    }
}

impl IdentitySigner for OpalSigner {
    fn pubkey(&self) -> PublicKey {
        self.pubkey
    }

    fn nip44_self_encrypt(&self, plaintext: String) -> BoxFuture<'_, Result<String, SignError>> {
        Box::pin(async move { self.nip44("encrypt", plaintext).await })
    }

    fn nip44_self_decrypt(&self, payload: String) -> BoxFuture<'_, Result<String, SignError>> {
        Box::pin(async move { self.nip44("decrypt", payload).await })
    }

    /// Until the first data signature after pairing, Opal may still be
    /// asking you (no rule yet): give the prompt time. After that the usual
    /// short timeout, so a stuck Opal doesn't stall the sync loop.
    fn sign_timeout(&self) -> Duration {
        if self.client.0.signed_once.load(Ordering::SeqCst) {
            opal_kit::signer::SIGN_TIMEOUT
        } else {
            PROMPT_TIMEOUT
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn token_round_trips_through_the_store() {
        let store = SecretStore::memory();
        assert!(load_token(&store).await.unwrap().is_none());
        save_token(&store, &"a".repeat(64)).await.unwrap();
        assert_eq!(
            load_token(&store)
                .await
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("a".repeat(64).as_str())
        );
        forget_token(&store).await.unwrap();
        assert!(load_token(&store).await.unwrap().is_none());
    }

    #[test]
    fn opal_answers_map_to_retry_or_fail() {
        let c = OpalClient::new(Path::new("/nonexistent"));
        let t = "t".repeat(64);
        c.set_token(Some(Zeroizing::new(t.clone())));
        let unavailable = |e: SignError| matches!(e, SignError::Unavailable(_));

        assert!(unavailable(c.classify(
            "Opal is locked",
            Mode::Background,
            &t
        )));
        assert!(unavailable(c.classify(
            "Opal isn't running",
            Mode::Background,
            &t
        )));
        assert!(!c.needs_pairing());
        // A stale token's "not paired" says nothing about the current one.
        assert!(unavailable(c.classify(
            "not paired",
            Mode::Background,
            "old"
        )));
        assert!(!c.needs_pairing());
        assert!(unavailable(c.classify("not paired", Mode::Background, &t)));
        assert!(c.needs_pairing());
        c.set_token(Some(Zeroizing::new(t.clone())));
        assert!(unavailable(c.classify(
            "paired with a different program (/x); pair again",
            Mode::Interactive,
            &t
        )));
        assert!(c.needs_pairing());

        c.set_token(Some(Zeroizing::new(t.clone())));
        assert!(!c.is_held());
        let e = c.classify("user rejected", Mode::Background, &t);
        assert!(
            unavailable(e.clone()) && e.to_string().contains("didn't allow"),
            "{e}"
        );
        assert!(c.is_held());
        c.clear_hold();
        assert!(!c.is_held());
        assert!(matches!(
            c.classify("user rejected", Mode::Interactive, &t),
            SignError::Failed(m) if m == "Opal didn't allow it"
        ));
        assert!(
            !c.is_held(),
            "a 'no' to something you asked for holds nothing"
        );

        assert!(unavailable(c.classify(
            "timed out waiting for approval",
            Mode::Background,
            &t
        )));
        assert!(c.is_held());
        assert!(matches!(
            c.classify(
                "Peridot didn't declare kind 1 when it paired",
                Mode::Background,
                &t
            ),
            SignError::Failed(_)
        ));
    }

    #[tokio::test]
    async fn no_token_means_pair_again() {
        let c = OpalClient::new(Path::new("/nonexistent"));
        let e = c
            .app_call("app.sign", json!({}), Mode::Background, false)
            .await
            .unwrap_err();
        assert_eq!(e.to_string(), PAIR_AGAIN);
        assert!(c.needs_pairing());
        assert!(c.take_auto_pair());
        assert!(!c.take_auto_pair(), "one automatic try per episode");
        assert_eq!(
            pair_error_text("declined"),
            "You declined in Opal. Nothing changed."
        );
    }
}
