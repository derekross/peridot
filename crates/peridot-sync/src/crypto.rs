//! Keys derived from the sync secret, and the encryption every synced item
//! goes through.
//!
//! The sync secret is 32 random bytes shared by your devices. From it we
//! derive (HKDF-SHA256) a key for naming items and a NIP-44 v2 conversation
//! key for their contents. Item names (`d` tags) are keyed hashes, so relays
//! see neither file names nor contents, only opaque blobs from your key.

use base64::Engine;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use opal_core::nostr::nips::nip44::v2::{self, ConversationKey};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// 32 random bytes shared by all of a person's devices.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SyncSecret([u8; 32]);

impl SyncSecret {
    pub fn generate() -> Self {
        Self(random_bytes())
    }

    pub fn from_hex(s: &str) -> anyhow::Result<Self> {
        let bytes = hex::decode(s.trim())?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("sync secret must be 32 bytes"))?;
        Ok(Self(arr))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// The working keys for this secret.
    pub fn keys(&self) -> SyncKeys {
        let hk = Hkdf::<Sha256>::new(Some(b"peridot/v1"), &self.0);
        let mut naming = [0u8; 32];
        let mut content = [0u8; 32];
        hk.expand(b"names", &mut naming)
            .expect("32 bytes is a valid length");
        hk.expand(b"content", &mut content)
            .expect("32 bytes is a valid length");
        let conversation = ConversationKey::new(content);
        content.zeroize();
        SyncKeys {
            naming,
            conversation,
        }
    }
}

impl std::fmt::Debug for SyncSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SyncSecret(…)")
    }
}

pub struct SyncKeys {
    naming: [u8; 32],
    conversation: ConversationKey,
}

impl Drop for SyncKeys {
    fn drop(&mut self) {
        self.naming.zeroize();
    }
}

impl SyncKeys {
    /// Opaque, stable `d` tag for `label` (e.g. `f:.config/hypr/bindings.lua`).
    pub fn name(&self, label: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.naming).expect("any key length");
        mac.update(label.as_bytes());
        hex::encode(&mac.finalize().into_bytes()[..16])
    }

    /// Encrypt with NIP-44 v2 under the shared conversation key.
    pub fn seal(&self, plaintext: &[u8]) -> anyhow::Result<String> {
        let bytes = v2::encrypt_to_bytes_with_nonce(&self.conversation, plaintext, random_bytes())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// Decrypt; fails for anything not sealed with these keys (including
    /// other apps' data under the same account).
    pub fn open(&self, payload: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(payload.trim())?;
        Ok(v2::decrypt_to_bytes(&self.conversation, &bytes)?)
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the OS random source works");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_opaque_and_secret_dependent() {
        let a = SyncSecret::from_hex(&"11".repeat(32)).unwrap().keys();
        let b = SyncSecret::from_hex(&"22".repeat(32)).unwrap().keys();
        let n = a.name("f:.config/hypr/bindings.lua");
        assert_eq!(n, a.name("f:.config/hypr/bindings.lua"));
        assert_eq!(n.len(), 32);
        assert!(!n.contains("hypr"));
        assert_ne!(n, b.name("f:.config/hypr/bindings.lua"));
        assert_ne!(n, a.name("f:.config/hypr/hyprland.lua"));
    }

    #[test]
    fn seals_and_opens_only_with_the_same_secret() {
        let secret = SyncSecret::generate();
        let keys = secret.keys();
        let sealed = keys.seal(b"hello").unwrap();
        assert_ne!(sealed, keys.seal(b"hello").unwrap(), "random nonce");
        assert_eq!(keys.open(&sealed).unwrap(), b"hello");
        assert!(SyncSecret::generate().keys().open(&sealed).is_err());
        let again = SyncSecret::from_hex(&secret.to_hex()).unwrap().keys();
        assert_eq!(again.open(&sealed).unwrap(), b"hello");
    }

    #[test]
    fn handles_a_full_size_chunk() {
        let keys = SyncSecret::generate().keys();
        let big = vec![7u8; 65535];
        assert_eq!(keys.open(&keys.seal(&big).unwrap()).unwrap(), big);
    }
}
