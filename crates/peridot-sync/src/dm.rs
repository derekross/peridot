//! Private messages (NIP-17): a link sent straight to someone.
//!
//! The text is a kind 14 rumor (never signed), sealed (kind 13, signed by
//! you, encrypted to the recipient) and gift-wrapped (kind 1059, signed by
//! a throwaway key, encrypted to the recipient again) so relays see neither
//! sender nor content. A second wrap goes to yourself, so your other
//! computers and Nostr apps have the conversation too.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use nostr_sdk::prelude::*;

use crate::signer::{IdentitySigner, SignError};

/// A message ready to send: one wrap for them, one for you.
#[derive(Debug, Clone)]
pub struct Wrapped {
    pub to_them: Event,
    pub to_me: Event,
}

/// Wrap `text` for `to`.
pub async fn wrap(
    signer: Arc<dyn IdentitySigner>,
    to: PublicKey,
    text: &str,
) -> Result<Wrapped, SignError> {
    let me = signer.pubkey();
    let mut rumor = EventBuilder::new(Kind::PrivateDirectMessage, text)
        .tag(Tag::public_key(to))
        .finalize_unsigned(me);
    rumor.ensure_id();
    let adapter = Adapter(signer);
    let to_them = GiftWrapBuilder::new(to, rumor.clone())
        .finalize_async(&adapter)
        .await
        .map_err(|e| SignError::Failed(e.to_string()))?;
    let to_me = GiftWrapBuilder::new(me, rumor)
        .finalize_async(&adapter)
        .await
        .map_err(|e| SignError::Failed(e.to_string()))?;
    Ok(Wrapped { to_them, to_me })
}

/// Lets the SDK's gift-wrap builders sign and encrypt through an
/// [`IdentitySigner`] (a key here, or Opal).
struct Adapter(Arc<dyn IdentitySigner>);

impl fmt::Debug for Adapter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Adapter")
    }
}

impl AsyncGetPublicKey for Adapter {
    type Error = SignError;
    fn get_public_key_async(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<PublicKey, SignError>> + Send + '_>> {
        Box::pin(async move { Ok(self.0.pubkey()) })
    }
}

impl AsyncSignEvent for Adapter {
    type Error = SignError;
    fn sign_event_async(
        &self,
        unsigned: UnsignedEvent,
    ) -> Pin<Box<dyn Future<Output = Result<Event, SignError>> + Send + '_>> {
        Box::pin(async move { self.0.sign(unsigned).await })
    }
}

impl AsyncNip44 for Adapter {
    type Error = SignError;
    fn nip44_encrypt_async<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, SignError>> + Send + 'a>> {
        Box::pin(async move {
            if *public_key == self.0.pubkey() {
                self.0.nip44_self_encrypt(content.to_string()).await
            } else {
                self.0
                    .nip44_encrypt_to(*public_key, content.to_string())
                    .await
            }
        })
    }

    fn nip44_decrypt_async<'a>(
        &'a self,
        _public_key: &'a PublicKey,
        _payload: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String, SignError>> + Send + 'a>> {
        Box::pin(async move { Err(SignError::Failed("Peridot only sends messages".into())) })
    }
}

/// Where someone receives private messages: their kind 10050 list, else
/// their NIP-65 read relays, else `fallback`.
pub async fn inbox_relays(client: &Client, who: PublicKey, fallback: &[RelayUrl]) -> Vec<RelayUrl> {
    let filter = Filter::new()
        .author(who)
        .kinds([Kind::InboxRelays, Kind::RelayList]);
    let relays: Vec<RelayUrl> = client.relays().await.keys().cloned().collect();
    let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
        .iter()
        .map(|r| (r.clone(), vec![filter.clone()]))
        .collect();
    let events = client
        .fetch_events(targets)
        .timeout(std::time::Duration::from_secs(10))
        .await
        .ok();
    let mut inbox: Vec<RelayUrl> = Vec::new();
    if let Some(events) = &events {
        if let Some(ev) = events
            .iter()
            .filter(|e| e.kind == Kind::InboxRelays)
            .max_by_key(|e| e.created_at)
        {
            inbox = nip17::extract_relay_list(ev).collect();
        }
        if inbox.is_empty()
            && let Some(ev) = events
                .iter()
                .filter(|e| e.kind == Kind::RelayList)
                .max_by_key(|e| e.created_at)
        {
            inbox = opal_kit::relays::RelayList::from_event(ev).read;
        }
    }
    inbox.truncate(5);
    if inbox.is_empty() {
        fallback.to_vec()
    } else {
        inbox
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signer::LocalSigner;

    #[tokio::test]
    async fn wraps_for_them_and_for_me() {
        let me = Keys::generate();
        let them = Keys::generate();
        let w = wrap(
            Arc::new(LocalSigner(me.clone())),
            them.public_key(),
            "https://x/s#1.a.b.c",
        )
        .await
        .unwrap();
        assert_eq!(w.to_them.kind, Kind::GiftWrap);
        assert_ne!(
            w.to_them.pubkey,
            me.public_key(),
            "a throwaway key signs the wrap"
        );
        assert_eq!(w.to_them.tags.public_keys().next(), Some(them.public_key()));
        let mut got = nip59::extract_rumor(&them, &w.to_them).unwrap();
        assert_eq!(got.sender, me.public_key());
        assert_eq!(got.rumor.kind, Kind::PrivateDirectMessage);
        assert_eq!(got.rumor.content, "https://x/s#1.a.b.c");
        let mut mine = nip59::extract_rumor(&me, &w.to_me).unwrap();
        assert_eq!(
            mine.rumor.id(),
            got.rumor.id(),
            "the same message, kept for me"
        );
        assert!(nip59::extract_rumor(&Keys::generate(), &w.to_them).is_err());
    }
}
