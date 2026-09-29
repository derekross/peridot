//! Your Peridot identity: a Nostr key that signs your synced settings, and
//! the sync secret that encrypts them.
//!
//! The key comes in three ways: made new, imported (an nsec you already
//! have), or held by Opal. In the first two, the key lives in the login
//! keyring with no passphrase of its own ("as safe as your login": on
//! Omarchy, your disk encryption and your session). In Opal mode Peridot
//! never sees the key; Opal signs for it (see [`crate::signer`]).
//!
//! The sync secret is also published once, encrypted to your own key, so
//! recovering means having the key (a recovery kit, or Opal's backup).

use nostr_sdk::prelude::*;
use opal_core::keystore::{ItemKind, SecretStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::DATA_KIND;
use crate::crypto::SyncSecret;
use crate::signer::IdentitySigner;

const ITEM_ID: &str = "main";
/// This computer's permanent device key, under the same item kind.
const DEVICE_ID: &str = "device";
/// The previous epoch's sync secret, during a rotation's window.
const PREVIOUS_ID: &str = "prev";
const OPAL_PREFIX: &str = "opal:";

#[derive(Clone)]
pub struct Identity {
    pub pubkey: PublicKey,
    /// The key itself when this computer holds it; None when Opal does.
    pub keys: Option<Keys>,
    /// The sync secret of the current epoch.
    pub secret: SyncSecret,
    /// This computer's own key: the sync secret is handed to it, and it
    /// signs nothing public. Never leaves this computer.
    pub device: Keys,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("pubkey", &self.pubkey.to_hex())
            .field("via_opal", &self.keys.is_none())
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A brand-new identity for someone starting fresh.
    pub fn generate() -> Self {
        Self::from_keys(Keys::generate())
    }

    /// An identity around a key you already have.
    pub fn from_keys(keys: Keys) -> Self {
        Self {
            pubkey: keys.public_key(),
            keys: Some(keys),
            secret: SyncSecret::generate(),
            device: Keys::generate(),
        }
    }

    /// An identity whose key Opal holds.
    pub fn via_opal(pubkey: PublicKey) -> Self {
        Self {
            pubkey,
            keys: None,
            secret: SyncSecret::generate(),
            device: Keys::generate(),
        }
    }

    /// The same identity with the sync secret of a newer epoch.
    pub fn with_secret(mut self, secret: SyncSecret) -> Self {
        self.secret = secret;
        self
    }

    /// The epoch this computer is on (0 = the first protocol version).
    pub fn epoch(&self) -> u64 {
        self.secret.epoch()
    }

    pub fn pubkey(&self) -> PublicKey {
        self.pubkey
    }

    /// The same identity, with Opal holding the key from now on: the key
    /// leaves this struct, the sync secret stays.
    pub fn into_opal_mode(self) -> Self {
        Self {
            pubkey: self.pubkey,
            keys: None,
            secret: self.secret,
            device: self.device,
        }
    }

    /// The previous epoch's secret, kept while its items are cleaned up.
    pub async fn load_previous(store: &SecretStore) -> anyhow::Result<Option<SyncSecret>> {
        match store.get(ItemKind::SyncSecret, PREVIOUS_ID).await? {
            Some(hex) => Ok(Some(SyncSecret::from_hex(&hex)?)),
            None => Ok(None),
        }
    }

    pub async fn save_previous(store: &SecretStore, secret: &SyncSecret) -> anyhow::Result<()> {
        store
            .put(
                ItemKind::SyncSecret,
                PREVIOUS_ID,
                "Peridot sync key (previous epoch)",
                &secret.to_hex(),
            )
            .await?;
        Ok(())
    }

    pub async fn clear_previous(store: &SecretStore) -> anyhow::Result<()> {
        store.delete(ItemKind::SyncSecret, PREVIOUS_ID).await?;
        Ok(())
    }

    /// This computer's device key from the keyring, made once.
    pub async fn device_key(store: &SecretStore) -> anyhow::Result<Keys> {
        if let Some(hex) = store.get(ItemKind::DeviceIdentity, DEVICE_ID).await? {
            return Ok(Keys::parse(hex.trim())?);
        }
        let keys = Keys::generate();
        store
            .put(
                ItemKind::DeviceIdentity,
                DEVICE_ID,
                "Peridot device key",
                &keys.secret_key().to_secret_hex(),
            )
            .await?;
        Ok(keys)
    }

    pub fn via_opal_mode(&self) -> bool {
        self.keys.is_none()
    }

    /// The identity saved on this computer, if any.
    pub async fn load(store: &SecretStore) -> anyhow::Result<Option<Self>> {
        let Some(key) = store.get(ItemKind::DeviceIdentity, ITEM_ID).await? else {
            return Ok(None);
        };
        let Some(secret) = store.get(ItemKind::SyncSecret, ITEM_ID).await? else {
            anyhow::bail!("the keyring has a Peridot identity but no sync secret");
        };
        let secret = SyncSecret::from_hex(&secret)?;
        let device = Self::device_key(store).await?;
        Ok(Some(match key.strip_prefix(OPAL_PREFIX) {
            Some(hex) => Self {
                pubkey: PublicKey::from_hex(hex.trim())?,
                keys: None,
                secret,
                device,
            },
            None => {
                let keys = Keys::parse(&key)?;
                Self {
                    pubkey: keys.public_key(),
                    keys: Some(keys),
                    secret,
                    device,
                }
            }
        }))
    }

    pub async fn save(&self, store: &SecretStore) -> anyhow::Result<()> {
        store
            .put(
                ItemKind::SyncSecret,
                ITEM_ID,
                "Peridot sync key",
                &self.secret.to_hex(),
            )
            .await?;
        let (label, value) = match &self.keys {
            Some(keys) => (
                "Peridot identity",
                zeroize::Zeroizing::new(keys.secret_key().to_secret_hex()),
            ),
            None => (
                "Peridot identity (key held by Opal)",
                zeroize::Zeroizing::new(format!("{OPAL_PREFIX}{}", self.pubkey.to_hex())),
            ),
        };
        store
            .put(ItemKind::DeviceIdentity, ITEM_ID, label, &value)
            .await?;
        store
            .put(
                ItemKind::DeviceIdentity,
                DEVICE_ID,
                "Peridot device key",
                &self.device.secret_key().to_secret_hex(),
            )
            .await?;
        Ok(())
    }

    pub async fn forget(store: &SecretStore) -> anyhow::Result<()> {
        store.delete(ItemKind::DeviceIdentity, ITEM_ID).await?;
        store.delete(ItemKind::SyncSecret, ITEM_ID).await?;
        store.delete(ItemKind::SyncSecret, PREVIOUS_ID).await?;
        store.delete(ItemKind::DeviceIdentity, DEVICE_ID).await?;
        Ok(())
    }

    /// The `d` tag of the root event. Derived from the public key only, so
    /// it can be found with nothing but the key (recovery) or a signer
    /// (Opal); what it labels is encrypted.
    pub fn root_name(pubkey: &PublicKey) -> String {
        let mut h = Sha256::new();
        h.update(b"peridot/root");
        h.update(pubkey.to_bytes());
        hex::encode(&h.finalize()[..16])
    }

    /// The root event (the "anchor"): the current sync secret, NIP-44
    /// encrypted to ourselves, with a commitment to it in the clear so two
    /// rotations racing each other can be told apart. The one thing under
    /// your public key that says "Peridot"; recovery starts from it.
    ///
    /// `after` is the `created_at` of the newest root known to be on the
    /// servers (0 if none): the root is addressable, so a relay keeps the
    /// newer of two and breaks a tie by id, and a rotation's root landing
    /// in the same second as the one it replaces could lose. This one is
    /// dated strictly after, whatever the clock says.
    pub async fn root_event(
        &self,
        signer: &dyn IdentitySigner,
        after: u64,
    ) -> anyhow::Result<Event> {
        let hex = self.secret.to_hex();
        let body = serde_json::to_string(&RootRef {
            v: if self.secret.is_legacy() { 1 } else { 2 },
            sync_secret: &hex,
        })?;
        let content = signer.nip44_self_encrypt(body).await?;
        let at = Timestamp::now().as_secs().max(after + 1);
        let mut b = EventBuilder::new(Kind::Custom(DATA_KIND), content)
            .custom_created_at(Timestamp::from(at))
            .tag(Tag::identifier(Self::root_name(&self.pubkey)));
        if !self.secret.is_legacy() {
            b = b.tag(Tag::custom("c", [self.secret.commitment()]));
        }
        let unsigned = b.finalize_unsigned(self.pubkey);
        Ok(signer.sign(unsigned).await?)
    }

    /// The commitment a root event carries (None for a legacy root).
    pub fn root_commitment(root: &Event) -> Option<String> {
        root.tags
            .iter()
            .find(|t| t.kind() == "c")
            .and_then(|t| t.content().map(String::from))
    }

    /// Rebuild the identity from its root event, using whatever can decrypt
    /// for `pubkey` (the key itself, or Opal).
    pub async fn from_root(
        pubkey: PublicKey,
        keys: Option<Keys>,
        signer: &dyn IdentitySigner,
        root: &Event,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(root.pubkey == pubkey, "root event from another key");
        anyhow::ensure!(
            root.tags.identifier().as_deref() == Some(Self::root_name(&pubkey).as_str()),
            "not a Peridot root event"
        );
        anyhow::ensure!(root.verify().is_ok(), "the root event isn't validly signed");
        let mut body = signer.nip44_self_decrypt(root.content.clone()).await?;
        let mut root: Root = serde_json::from_str(&body)?;
        anyhow::ensure!(root.v <= 2, "this root event is from a newer Peridot");
        let secret = SyncSecret::from_hex(&root.sync_secret);
        zeroize::Zeroize::zeroize(&mut root.sync_secret);
        zeroize::Zeroize::zeroize(&mut body);
        let secret = secret?;
        anyhow::ensure!(
            (root.v == 1) == secret.is_legacy(),
            "root event version and secret form disagree"
        );
        Ok(Self {
            pubkey,
            keys,
            secret,
            // A restored computer is a new device.
            device: Keys::generate(),
        })
    }
}

/// The root event's body, borrowed for writing…
#[derive(Serialize)]
struct RootRef<'a> {
    v: u8,
    sync_secret: &'a str,
}

/// …and owned when read (wiped by hand: no serde for `Zeroizing` here).
#[derive(Deserialize)]
struct Root {
    v: u8,
    sync_secret: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::LocalSigner;

    #[tokio::test]
    async fn saves_loads_and_forgets_both_kinds() {
        let store = SecretStore::memory();
        assert!(Identity::load(&store).await.unwrap().is_none());
        let id = Identity::generate();
        id.save(&store).await.unwrap();
        let back = Identity::load(&store).await.unwrap().unwrap();
        assert_eq!(back.pubkey(), id.pubkey());
        assert!(back.keys.is_some());
        assert_eq!(back.secret.to_hex(), id.secret.to_hex());

        let opal = Identity::via_opal(Keys::generate().public_key());
        opal.save(&store).await.unwrap();
        let back = Identity::load(&store).await.unwrap().unwrap();
        assert_eq!(back.pubkey(), opal.pubkey());
        assert!(back.via_opal_mode());
        assert_eq!(back.secret.to_hex(), opal.secret.to_hex());

        Identity::forget(&store).await.unwrap();
        assert!(Identity::load(&store).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn root_event_restores_the_sync_secret() {
        let id = Identity::generate();
        let keys = id.keys.clone().unwrap();
        let signer = LocalSigner(keys.clone());
        let root = id.root_event(&signer, 0).await.unwrap();
        assert!(root.verify().is_ok());
        assert!(!root.content.contains(id.secret.to_hex().as_str()));
        let back = Identity::from_root(id.pubkey(), Some(keys), &signer, &root)
            .await
            .unwrap();
        assert_eq!(back.secret.to_hex(), id.secret.to_hex());
        let other = Keys::generate();
        assert!(
            Identity::from_root(other.public_key(), None, &LocalSigner(other), &root)
                .await
                .is_err()
        );
    }
}

#[cfg(test)]
mod move_tests {
    use super::*;

    #[tokio::test]
    async fn moving_into_opal_keeps_the_identity_and_secret_but_drops_the_key() {
        let store = SecretStore::memory();
        let local = Identity::generate();
        local.save(&store).await.unwrap();
        let moved = local.clone().into_opal_mode();
        assert_eq!(moved.pubkey(), local.pubkey());
        assert!(moved.via_opal_mode() && moved.keys.is_none());
        assert_eq!(moved.secret.to_hex(), local.secret.to_hex());
        moved.save(&store).await.unwrap();
        let back = Identity::load(&store).await.unwrap().unwrap();
        assert!(back.via_opal_mode(), "the keyring no longer holds the key");
        assert_eq!(back.pubkey(), local.pubkey());
        assert_eq!(back.secret.to_hex(), local.secret.to_hex());
    }
}
