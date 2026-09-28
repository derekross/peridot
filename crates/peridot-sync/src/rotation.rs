//! Moving your devices to the next epoch of the sync secret.
//!
//! A rotation is one event, published under the **rekey address** of the
//! new epoch (a key derived from the *current* secret, so every current
//! device is already listening for it), encrypted to that address's own
//! key. Inside: the new epoch number, a commitment to the secret it
//! continues from, the rotating device's key, and one **wrap** per
//! remaining device: the new secret encrypted between the rotator's device
//! key and that device's key, filed under a locator only the two of them
//! can compute. A removed device holds the current secret, so it can open
//! the envelope and count the wraps, but none of them is for it and none
//! opens for it. It keeps what it already had; it learns nothing new.

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::DATA_KIND;
use crate::crypto::SyncSecret;

/// The `d` tag of a rotation event (one per rekey address).
const D: &str = "0";

#[derive(Serialize, Deserialize)]
struct Announcement {
    v: u8,
    epoch: u64,
    prev: u64,
    prevcommit: String,
    rotator: String,
    wraps: Vec<Wrap>,
}

#[derive(Serialize, Deserialize)]
struct Wrap {
    /// Who this is for, without saying so: `sha256("peridot/locator" ‖
    /// rotator ‖ recipient ‖ epoch)`.
    loc: String,
    /// NIP-44 between the rotator's device key and the recipient's, of
    /// the new secret in its text form.
    w: String,
}

/// What a rotation event means for this device.
#[derive(Debug)]
pub enum Rotation {
    /// We're in: switch to this secret.
    Adopt {
        secret: SyncSecret,
        rotator: PublicKey,
    },
    /// Every other device got a wrap; we didn't.
    Removed { epoch: u64 },
}

fn locator(rotator: &PublicKey, recipient: &PublicKey, epoch: u64) -> String {
    let mut h = Sha256::new();
    h.update(b"peridot/locator");
    h.update(rotator.to_bytes());
    h.update(recipient.to_bytes());
    h.update(epoch.to_be_bytes());
    hex::encode(&h.finalize()[..16])
}

/// Build the rotation from `current` to `next`, for `recipients` (device
/// keys), signed and encrypted for the rekey address.
pub fn announce(
    current: &SyncSecret,
    next: &SyncSecret,
    rotator: &Keys,
    recipients: &[PublicKey],
) -> anyhow::Result<Event> {
    anyhow::ensure!(next.epoch() == current.epoch() + 1, "epochs go up by one");
    let secret_text = next.to_hex();
    let mut wraps = Vec::with_capacity(recipients.len());
    for r in recipients {
        wraps.push(Wrap {
            loc: locator(&rotator.public_key(), r, next.epoch()),
            w: nip44::encrypt(
                rotator.secret_key(),
                r,
                secret_text.as_str(),
                nip44::Version::V2,
            )?,
        });
    }
    let body = Zeroizing::new(serde_json::to_string(&Announcement {
        v: 2,
        epoch: next.epoch(),
        prev: current.epoch(),
        prevcommit: current.commitment(),
        rotator: rotator.public_key().to_hex(),
        wraps,
    })?);
    let address = current.rekey_keys();
    let content = nip44::encrypt(
        address.secret_key(),
        &address.public_key(),
        body.as_str(),
        nip44::Version::V2,
    )?;
    Ok(EventBuilder::new(Kind::Custom(DATA_KIND), content)
        .tag(Tag::identifier(D))
        .finalize(&address)?)
}

/// Read a rotation event with the current secret and this device's key.
pub fn adopt(ev: &Event, current: &SyncSecret, device: &Keys) -> anyhow::Result<Rotation> {
    let address = current.rekey_keys();
    anyhow::ensure!(ev.pubkey == address.public_key(), "not for this epoch");
    anyhow::ensure!(ev.verify().is_ok(), "bad signature");
    let body = Zeroizing::new(nip44::decrypt(
        address.secret_key(),
        &address.public_key(),
        &ev.content,
    )?);
    let a: Announcement = serde_json::from_str(&body)?;
    anyhow::ensure!(a.v == 2, "unknown rotation version");
    anyhow::ensure!(a.prev == current.epoch(), "continues from another epoch");
    anyhow::ensure!(a.epoch == current.epoch() + 1, "epochs go up by one");
    anyhow::ensure!(
        a.prevcommit == current.commitment(),
        "continues from a secret this computer doesn't hold"
    );
    let rotator = PublicKey::from_hex(&a.rotator)?;
    let mine = locator(&rotator, &device.public_key(), a.epoch);
    let Some(wrap) = a.wraps.iter().find(|w| w.loc == mine) else {
        return Ok(Rotation::Removed { epoch: a.epoch });
    };
    let text = Zeroizing::new(nip44::decrypt(device.secret_key(), &rotator, &wrap.w)?);
    let secret = SyncSecret::from_hex(&text)?;
    anyhow::ensure!(secret.epoch() == a.epoch, "the wrap names another epoch");
    Ok(Rotation::Adopt { secret, rotator })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_devices_adopt_and_the_removed_one_is_told() {
        let current = SyncSecret::generate();
        let next = current.next();
        let rotator = Keys::generate();
        let laptop = Keys::generate();
        let stolen = Keys::generate();
        let ev = announce(&current, &next, &rotator, &[laptop.public_key()]).unwrap();
        assert_eq!(ev.pubkey, current.rekey_keys().public_key());
        assert!(!ev.content.contains(next.to_hex().as_str()));
        match adopt(&ev, &current, &laptop).unwrap() {
            Rotation::Adopt { secret, rotator: r } => {
                assert_eq!(secret.to_hex().as_str(), next.to_hex().as_str());
                assert_eq!(r, rotator.public_key());
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            adopt(&ev, &current, &stolen).unwrap(),
            Rotation::Removed { epoch: 2 }
        ));
        // Without the current secret the envelope doesn't even open.
        assert!(adopt(&ev, &SyncSecret::generate(), &laptop).is_err());
        // A rotation from a different secret at the same epoch is refused.
        let other = SyncSecret::from_parts(current.epoch(), *SyncSecret::generate().bytes());
        let forged = announce(&other, &other.next(), &rotator, &[laptop.public_key()]).unwrap();
        assert!(adopt(&forged, &current, &laptop).is_err());
    }
}
