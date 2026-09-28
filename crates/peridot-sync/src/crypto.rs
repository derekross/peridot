//! Keys derived from the sync secret, and the encryption every synced item
//! goes through.
//!
//! The sync secret is 32 random bytes shared by your devices, in numbered
//! **epochs**: removing a device mints the next epoch, and the removed
//! device never learns it. From the secret of an epoch we derive
//! (HKDF-SHA256, with the epoch in the derivation):
//!
//! - a **naming key**: item names (`d` tags) are keyed hashes, so relays
//!   see neither file names nor contents, only opaque blobs;
//! - a NIP-44 v2 **conversation key** for the contents;
//! - a **signing key**: every item is signed by it, not by your public
//!   identity, so relays can't tell whose settings these are;
//! - the **rekey address** for the next epoch: the key under which the
//!   rotation to epoch+1 is announced, derivable only by holders of this
//!   epoch's secret.
//!
//! Epoch 0 is the first protocol version: no signing key (the identity
//! signed), no padding, salt `peridot/v1`. It is read, never written.

use base64::Engine;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use nostr_sdk::prelude::{Keys, SecretKey};
use opal_core::nostr::nips::nip44::v2::{self, ConversationKey};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// The epoch new identities start at (epoch 0 is the legacy format).
pub const FIRST_EPOCH: u64 = 1;

/// Sizes items are padded to, so relays learn little from lengths.
pub const SIZE_CLASSES: [usize; 4] = [1024, 4096, 16384, 32768];

/// 32 random bytes shared by all of a person's devices, for one epoch.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SyncSecret {
    #[zeroize(skip)]
    epoch: u64,
    bytes: [u8; 32],
}

impl SyncSecret {
    /// A fresh secret for the first epoch.
    pub fn generate() -> Self {
        Self::mint(FIRST_EPOCH)
    }

    /// A fresh secret for `epoch`.
    pub fn mint(epoch: u64) -> Self {
        Self {
            epoch,
            bytes: random_bytes(),
        }
    }

    /// The next epoch after this one, with a new secret.
    pub fn next(&self) -> Self {
        Self::mint(self.epoch + 1)
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_legacy(&self) -> bool {
        self.epoch == 0
    }

    /// A secret from the first protocol version (epoch 0).
    pub fn legacy(bytes: [u8; 32]) -> Self {
        Self { epoch: 0, bytes }
    }

    pub fn from_parts(epoch: u64, bytes: [u8; 32]) -> Self {
        Self { epoch, bytes }
    }

    pub fn bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// `v2:<epoch>:<hex>`, or plain hex for a legacy secret (how it was
    /// always stored, so old keyrings and root events still read).
    pub fn from_hex(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        if let Some(rest) = s.strip_prefix("v2:") {
            let (epoch, hex) = rest
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("sync secret: bad form"))?;
            let epoch: u64 = epoch.parse()?;
            anyhow::ensure!(epoch >= FIRST_EPOCH, "sync secret: bad epoch");
            return Ok(Self {
                epoch,
                bytes: parse_hex(hex)?,
            });
        }
        Ok(Self::legacy(parse_hex(s)?))
    }

    /// The secret as text, wiped when dropped. Callers borrow it (`&*`,
    /// `.as_str()`) rather than copying it out.
    pub fn to_hex(&self) -> Zeroizing<String> {
        if self.is_legacy() {
            Zeroizing::new(hex::encode(self.bytes))
        } else {
            Zeroizing::new(format!("v2:{}:{}", self.epoch, hex::encode(self.bytes)))
        }
    }

    /// The working keys for this secret.
    pub fn keys(&self) -> SyncKeys {
        if self.is_legacy() {
            let hk = Hkdf::<Sha256>::new(Some(b"peridot/v1"), &self.bytes);
            let mut naming = [0u8; 32];
            let mut content = [0u8; 32];
            hk.expand(b"names", &mut naming).expect("valid length");
            hk.expand(b"content", &mut content).expect("valid length");
            let conversation = ConversationKey::new(content);
            content.zeroize();
            return SyncKeys {
                epoch: 0,
                naming,
                conversation,
                signer: None,
            };
        }
        let mut naming = [0u8; 32];
        let mut content = [0u8; 32];
        self.expand("names", self.epoch, &mut naming);
        self.expand("content", self.epoch, &mut content);
        let conversation = ConversationKey::new(content);
        content.zeroize();
        SyncKeys {
            epoch: self.epoch,
            naming,
            conversation,
            signer: Some(self.scalar("signer", self.epoch)),
        }
    }

    /// The key under which the rotation to the next epoch is announced.
    /// Derived from **this** epoch's secret, so every current device can
    /// listen for it before the rotation exists.
    pub fn rekey_keys(&self) -> Keys {
        self.scalar("rekey", self.epoch + 1)
    }

    /// `sha256("peridot/epoch-commit" ‖ epoch ‖ secret)`: proves which
    /// secret a rotation continues from, without revealing it.
    pub fn commitment(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"peridot/epoch-commit");
        h.update(self.epoch.to_be_bytes());
        h.update(self.bytes);
        hex::encode(h.finalize())
    }

    /// HKDF-SHA256(ikm = secret, salt = "peridot/v2", info = label ‖ 0 ‖ epoch).
    fn expand(&self, label: &str, epoch: u64, out: &mut [u8]) {
        let hk = Hkdf::<Sha256>::new(Some(b"peridot/v2"), &self.bytes);
        let mut info = Vec::with_capacity(label.len() + 9);
        info.extend_from_slice(label.as_bytes());
        info.push(0);
        info.extend_from_slice(&epoch.to_be_bytes());
        hk.expand(&info, out).expect("valid length");
    }

    /// A secp256k1 key from the derivation, retrying with a counter byte
    /// on the (vanishingly rare) invalid scalar.
    fn scalar(&self, label: &str, epoch: u64) -> Keys {
        let mut counter = 0u8;
        loop {
            let mut sk = Zeroizing::new([0u8; 32]);
            let l = if counter == 0 {
                label.to_string()
            } else {
                format!("{label}/{counter}")
            };
            self.expand(&l, epoch, sk.as_mut());
            if let Ok(secret) = SecretKey::from_slice(sk.as_ref()) {
                return Keys::new(secret);
            }
            counter += 1;
        }
    }
}

fn parse_hex(s: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(s.trim())?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("sync secret must be 32 bytes"))
}

impl std::fmt::Debug for SyncSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SyncSecret(epoch {}, …)", self.epoch)
    }
}

pub struct SyncKeys {
    epoch: u64,
    naming: [u8; 32],
    conversation: ConversationKey,
    /// Signs every item of this epoch; None for the legacy epoch, whose
    /// items the identity signed.
    signer: Option<Keys>,
}

impl Drop for SyncKeys {
    fn drop(&mut self) {
        self.naming.zeroize();
    }
}

impl SyncKeys {
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn is_legacy(&self) -> bool {
        self.epoch == 0
    }

    /// The key that signs this epoch's items (None for legacy).
    pub fn signer(&self) -> Option<&Keys> {
        self.signer.as_ref()
    }

    /// Opaque, stable `d` tag for `label` (e.g. `f:.config/hypr/bindings.lua`).
    pub fn name(&self, label: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.naming).expect("any key length");
        mac.update(label.as_bytes());
        hex::encode(&mac.finalize().into_bytes()[..16])
    }

    /// Encrypt with NIP-44 v2 under the shared conversation key, padded
    /// to a size class (not for the legacy epoch).
    pub fn seal(&self, plaintext: &[u8]) -> anyhow::Result<String> {
        let padded;
        let body: &[u8] = if self.is_legacy() {
            plaintext
        } else {
            padded = pad(plaintext);
            &padded
        };
        let bytes = v2::encrypt_to_bytes_with_nonce(&self.conversation, body, random_bytes())?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }

    /// Decrypt; fails for anything not sealed with these keys (including
    /// other apps' data under the same account).
    pub fn open(&self, payload: &str) -> anyhow::Result<Vec<u8>> {
        let bytes = base64::engine::general_purpose::STANDARD.decode(payload.trim())?;
        let plain = v2::decrypt_to_bytes(&self.conversation, &bytes)?;
        if self.is_legacy() {
            return Ok(plain);
        }
        unpad(&plain)
    }
}

/// `u32_be(len) ‖ data ‖ zeros` up to the smallest class that fits (or the
/// exact size plus header when bigger than the largest class).
fn pad(data: &[u8]) -> Vec<u8> {
    let need = 4 + data.len();
    let size = SIZE_CLASSES
        .iter()
        .copied()
        .find(|c| *c >= need)
        .unwrap_or(need);
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
    out.resize(size, 0);
    out
}

fn unpad(padded: &[u8]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(padded.len() >= 4, "padded item too short");
    let len = u32::from_be_bytes([padded[0], padded[1], padded[2], padded[3]]) as usize;
    anyhow::ensure!(4 + len <= padded.len(), "padded item length is wrong");
    Ok(padded[4..4 + len].to_vec())
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

    #[test]
    fn epochs_change_every_key_and_legacy_stays_readable() {
        let s1 = SyncSecret::generate();
        assert_eq!(s1.epoch(), FIRST_EPOCH);
        let s2 = s1.next();
        assert_eq!(s2.epoch(), 2);
        let (k1, k2) = (s1.keys(), s2.keys());
        assert_ne!(k1.name("f:x"), k2.name("f:x"));
        assert_ne!(
            k1.signer().unwrap().public_key(),
            k2.signer().unwrap().public_key()
        );
        assert!(k2.open(&k1.seal(b"a").unwrap()).is_err());
        // The rekey address comes from the prior epoch and is stable.
        assert_eq!(
            s1.rekey_keys().public_key(),
            SyncSecret::from_parts(1, *s1.bytes())
                .rekey_keys()
                .public_key()
        );
        assert_ne!(s1.rekey_keys().public_key(), s2.rekey_keys().public_key());
        assert_ne!(s1.commitment(), s2.commitment());
        assert_eq!(s1.to_hex().len(), "v2:1:".len() + 64);
        assert!(SyncSecret::from_hex("v2:0:00").is_err());

        // Epoch 0 reads exactly as before: same keys, no padding.
        let legacy = SyncSecret::from_hex(&"33".repeat(32)).unwrap();
        assert!(legacy.is_legacy() && legacy.keys().signer().is_none());
        assert_eq!(legacy.to_hex().as_str(), "33".repeat(32));
        let sealed = legacy.keys().seal(b"old").unwrap();
        let raw = base64::engine::general_purpose::STANDARD
            .decode(&sealed)
            .unwrap();
        assert!(raw.len() < 200, "legacy items aren't padded");
        assert_eq!(legacy.keys().open(&sealed).unwrap(), b"old");
    }

    #[test]
    fn items_fall_into_size_classes() {
        let keys = SyncSecret::generate().keys();
        let len = |data: &[u8]| {
            base64::engine::general_purpose::STANDARD
                .decode(keys.seal(data).unwrap())
                .unwrap()
                .len()
        };
        // NIP-44 adds its own padding on top; two very different small
        // payloads land on the same wire size.
        assert_eq!(len(b"a"), len(&[b'b'; 900]));
        assert_eq!(len(&[0u8; 2000]), len(&[1u8; 4000]));
        assert!(len(&[0u8; 5000]) > len(&[0u8; 4000]));
        assert_eq!(pad(b"").len(), 1024);
        assert_eq!(unpad(&pad(b"xyz")).unwrap(), b"xyz");
        assert!(unpad(&[0, 0, 0, 9, 1]).is_err());
    }
}
