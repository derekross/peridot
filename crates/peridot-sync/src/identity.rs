//! Your Peridot identity: a key that signs your synced settings, and the
//! sync secret that encrypts them. Both live in the login keyring with no
//! passphrase of their own ("as safe as your login"): on Omarchy that means
//! your disk encryption and your session.
//!
//! The sync secret is also published once, encrypted to your own key, so a
//! recovery kit only has to carry the key.

use hmac::{Hmac, Mac};
use nostr_sdk::prelude::*;
use opal_core::keystore::{ItemKind, SecretStore};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::DATA_KIND;
use crate::crypto::SyncSecret;

const ITEM_ID: &str = "main";

#[derive(Clone)]
pub struct Identity {
    pub keys: Keys,
    pub secret: SyncSecret,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("pubkey", &self.keys.public_key().to_hex())
            .finish_non_exhaustive()
    }
}

impl Identity {
    /// A brand-new identity for someone starting fresh.
    pub fn generate() -> Self {
        Self {
            keys: Keys::generate(),
            secret: SyncSecret::generate(),
        }
    }

    pub fn pubkey(&self) -> PublicKey {
        self.keys.public_key()
    }

    /// The identity saved on this computer, if any.
    pub async fn load(store: &SecretStore) -> anyhow::Result<Option<Self>> {
        let Some(key) = store.get(ItemKind::DeviceIdentity, ITEM_ID).await? else {
            return Ok(None);
        };
        let Some(secret) = store.get(ItemKind::SyncSecret, ITEM_ID).await? else {
            anyhow::bail!("the keyring has a Peridot key but no sync secret");
        };
        Ok(Some(Self {
            keys: Keys::parse(&key)?,
            secret: SyncSecret::from_hex(&secret)?,
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
        store
            .put(
                ItemKind::DeviceIdentity,
                ITEM_ID,
                "Peridot identity",
                &self.keys.secret_key().to_secret_hex(),
            )
            .await?;
        Ok(())
    }

    pub async fn forget(store: &SecretStore) -> anyhow::Result<()> {
        store.delete(ItemKind::DeviceIdentity, ITEM_ID).await?;
        store.delete(ItemKind::SyncSecret, ITEM_ID).await?;
        Ok(())
    }

    /// The `d` tag of the root event, derived from the key alone (the sync
    /// secret isn't known yet when restoring from a recovery kit).
    pub fn root_name(keys: &Keys) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(keys.secret_key().as_secret_bytes())
            .expect("any key length");
        mac.update(b"peridot/root");
        hex::encode(&mac.finalize().into_bytes()[..16])
    }

    /// The root event: the sync secret, NIP-44 encrypted to ourselves.
    pub fn root_event(&self) -> anyhow::Result<Event> {
        let body = serde_json::to_string(&Root {
            v: 1,
            sync_secret: self.secret.to_hex(),
        })?;
        let content = nip44::encrypt(
            self.keys.secret_key(),
            &self.keys.public_key(),
            body,
            nip44::Version::V2,
        )?;
        Ok(EventBuilder::new(Kind::Custom(DATA_KIND), content)
            .tag(Tag::identifier(Self::root_name(&self.keys)))
            .finalize(&self.keys)?)
    }

    /// Rebuild the identity from a key and its root event.
    pub fn from_root(keys: Keys, root: &Event) -> anyhow::Result<Self> {
        anyhow::ensure!(
            root.pubkey == keys.public_key(),
            "root event from another key"
        );
        anyhow::ensure!(
            root.tags.identifier().as_deref() == Some(Self::root_name(&keys).as_str()),
            "not a Peridot root event"
        );
        let body = nip44::decrypt(keys.secret_key(), &keys.public_key(), &root.content)?;
        let root: Root = serde_json::from_str(&body)?;
        Ok(Self {
            secret: SyncSecret::from_hex(&root.sync_secret)?,
            keys,
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

    #[tokio::test]
    async fn saves_loads_and_forgets() {
        let store = SecretStore::memory();
        assert!(Identity::load(&store).await.unwrap().is_none());
        let id = Identity::generate();
        id.save(&store).await.unwrap();
        let back = Identity::load(&store).await.unwrap().unwrap();
        assert_eq!(back.pubkey(), id.pubkey());
        assert_eq!(back.secret.to_hex(), id.secret.to_hex());
        Identity::forget(&store).await.unwrap();
        assert!(Identity::load(&store).await.unwrap().is_none());
    }

    #[test]
    fn root_event_restores_the_sync_secret() {
        let id = Identity::generate();
        let root = id.root_event().unwrap();
        assert!(root.verify().is_ok());
        assert!(!root.content.contains(&id.secret.to_hex()));
        let back = Identity::from_root(id.keys.clone(), &root).unwrap();
        assert_eq!(back.secret.to_hex(), id.secret.to_hex());
        assert!(Identity::from_root(Keys::generate(), &root).is_err());
    }
}
