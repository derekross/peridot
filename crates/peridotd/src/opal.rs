//! Using an identity that Opal holds. Peridot never sees the key: it asks
//! the Opal daemon (on its control socket, same user only) to sign its own
//! event kinds and to encrypt to the user's key. Opal logs every use under
//! "Peridot" and refuses while locked.

use std::path::{Path, PathBuf};

use futures::future::BoxFuture;
use nostr_sdk::prelude::*;
use opal_core::ipc::{IpcMessage, IpcRequest};
use peridot_sync::signer::{EventSigner, IdentitySigner, SignError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const APP_ID: &str = "peridot";

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

#[derive(Clone)]
pub struct OpalClient {
    socket: PathBuf,
}

impl OpalClient {
    pub fn new(socket: &Path) -> Self {
        Self {
            socket: socket.to_path_buf(),
        }
    }

    /// One request, one reply. Every call opens its own connection: they're
    /// rare, and a long-lived one would need reconnect handling.
    pub async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|_| anyhow::anyhow!("Opal isn't running"))?;
        let (r, mut w) = stream.into_split();
        let mut line = serde_json::to_string(&IpcRequest {
            id: 1,
            method: method.into(),
            params,
        })?;
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
}

/// Signs as an Opal account.
pub struct OpalSigner {
    client: OpalClient,
    pubkey: PublicKey,
}

impl OpalSigner {
    pub fn new(client: OpalClient, pubkey: PublicKey) -> Self {
        Self { client, pubkey }
    }
}

/// Opal's "locked" and "not running" answers mean "try again later", not
/// "this will never work".
fn map_err(e: anyhow::Error) -> SignError {
    let msg = e.to_string();
    if msg.contains("locked") {
        SignError::Unavailable("Unlock Opal to keep syncing".into())
    } else if msg.contains("isn't running") {
        SignError::Unavailable("Opal isn't running; start it to keep syncing".into())
    } else {
        SignError::Failed(format!("Opal: {msg}"))
    }
}

impl EventSigner for OpalSigner {
    fn sign(&self, unsigned: UnsignedEvent) -> BoxFuture<'_, Result<Event, SignError>> {
        Box::pin(async move {
            let v = self
                .client
                .call(
                    "app.sign",
                    json!({"app": APP_ID, "pubkey": self.pubkey.to_hex(), "event": unsigned}),
                )
                .await
                .map_err(map_err)?;
            let ev: Event =
                serde_json::from_value(v).map_err(|e| SignError::Failed(e.to_string()))?;
            ev.verify().map_err(|e| SignError::Failed(e.to_string()))?;
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
}

impl OpalSigner {
    async fn nip44(&self, op: &str, content: String) -> Result<String, SignError> {
        let v = self
            .client
            .call(
                "app.nip44",
                json!({"app": APP_ID, "pubkey": self.pubkey.to_hex(), "op": op, "content": content}),
            )
            .await
            .map_err(map_err)?;
        v["content"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| SignError::Failed("Opal sent no content".into()))
    }
}
