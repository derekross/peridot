//! What signs for an identity: the key itself, or Opal on its behalf.
//! Beyond signing events, the identity needs NIP-44 encryption to its own
//! key (for the root event that carries the sync secret) and relay
//! authentication (NIP-42), so both go through the same object.

use futures::future::BoxFuture;
use nostr_sdk::prelude::*;
pub use opal_kit::signer::{EventSigner, SignError};

pub trait IdentitySigner: EventSigner {
    fn pubkey(&self) -> PublicKey;
    fn nip44_self_encrypt(&self, plaintext: String) -> BoxFuture<'_, Result<String, SignError>>;
    fn nip44_self_decrypt(&self, payload: String) -> BoxFuture<'_, Result<String, SignError>>;
    /// How long the sync engine waits for one signature. A signer that may
    /// ask the user (Opal, before its rules are in place) needs longer.
    fn sign_timeout(&self) -> std::time::Duration {
        opal_kit::signer::SIGN_TIMEOUT
    }
}

/// Signs with a key this computer holds.
#[derive(Debug)]
pub struct LocalSigner(pub Keys);

impl EventSigner for LocalSigner {
    fn sign(&self, unsigned: UnsignedEvent) -> BoxFuture<'_, Result<Event, SignError>> {
        Box::pin(async move {
            self.0
                .sign_event(unsigned)
                .map_err(|e| SignError::Failed(e.to_string()))
        })
    }
}

impl IdentitySigner for LocalSigner {
    fn pubkey(&self) -> PublicKey {
        self.0.public_key()
    }

    fn nip44_self_encrypt(&self, plaintext: String) -> BoxFuture<'_, Result<String, SignError>> {
        Box::pin(async move {
            nip44::encrypt(
                self.0.secret_key(),
                &self.0.public_key(),
                plaintext,
                nip44::Version::V2,
            )
            .map_err(|e| SignError::Failed(e.to_string()))
        })
    }

    fn nip44_self_decrypt(&self, payload: String) -> BoxFuture<'_, Result<String, SignError>> {
        Box::pin(async move {
            nip44::decrypt(self.0.secret_key(), &self.0.public_key(), &payload)
                .map_err(|e| SignError::Failed(e.to_string()))
        })
    }
}

/// NIP-42 relay login through an [`IdentitySigner`].
pub struct SignerAuth(pub std::sync::Arc<dyn IdentitySigner>);

impl std::fmt::Debug for SignerAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SignerAuth")
    }
}

impl Authenticator for SignerAuth {
    fn make_auth_event<'a>(
        &'a self,
        relay_url: &'a RelayUrl,
        challenge: &'a str,
    ) -> BoxFuture<'a, Result<Event, nostr_sdk::error::Error>> {
        Box::pin(async move {
            let unsigned = ClientAuthentication::new(challenge, relay_url.clone())
                .finalize_unsigned(self.0.pubkey());
            self.0
                .sign(unsigned)
                .await
                .map_err(nostr_sdk::error::Error::other)
        })
    }
}
