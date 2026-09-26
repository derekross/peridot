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
const OPAL_PREFIX: &str = "opal:";

#[derive(Clone)]
pub struct Identity {
    pub pubkey: PublicKey,
    /// The key itself when this computer holds it; None when Opal does.
    pub keys: Option<Keys>,
    pub secret: SyncSecret,
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
        }
    }

    /// An identity whose key Opal holds.
    pub fn via_opal(pubkey: PublicKey) -> Self {
        Self {
            pubkey,
            keys: None,
            secret: SyncSecret::generate(),
        }
    }

    pub fn pubkey(&self) -> PublicKey {
        self.pubkey
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
        Ok(Some(match key.strip_prefix(OPAL_PREFIX) {
            Some(hex) => Self {
                pubkey: PublicKey::from_hex(hex.trim())?,
                keys: None,
                secret,
            },
            None => {
                let keys = Keys::parse(&key)?;
                Self {
                    pubkey: keys.public_key(),
                    keys: Some(keys),
                    secret,
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
        Ok(())
    }

    pub async fn forget(store: &SecretStore) -> anyhow::Result<()> {
        store.delete(ItemKind::DeviceIdentity, ITEM_ID).await?;
        store.delete(ItemKind::SyncSecret, ITEM_ID).await?;
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

    /// The root event: the sync secret, NIP-44 encrypted to ourselves.
    pub async fn root_event(&self, signer: &dyn IdentitySigner) -> anyhow::Result<Event> {
        let body = serde_json::to_string(&Root {
            v: 1,
            sync_secret: self.secret.to_hex(),
        })?;
        let content = signer.nip44_self_encrypt(body).await?;
        let unsigned = EventBuilder::new(Kind::Custom(DATA_KIND), content)
            .tag(Tag::identifier(Self::root_name(&self.pubkey)))
            .finalize_unsigned(self.pubkey);
        Ok(signer.sign(unsigned).await?)
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
        let body = signer.nip44_self_decrypt(root.content.clone()).await?;
        let root: Root = serde_json::from_str(&body)?;
        Ok(Self {
            pubkey,
            keys,
            secret: SyncSecret::from_hex(&root.sync_secret)?,
        })
    }
}

#[derive(Serialize, Deserialize)]
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
        let root = id.root_event(&signer).await.unwrap();
        assert!(root.verify().is_ok());
        assert!(!root.content.contains(&id.secret.to_hex()));
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
