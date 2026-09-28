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

/// Whether `host` names something on the public internet: not a literal
/// IP address, not localhost or a LAN-style name. Lists published by
/// other people (inbox relays, NIP-05 sites) are only followed to such
/// hosts, so a list can't point this computer at its own network.
pub fn is_public_host(host: &str) -> bool {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    if host == "localhost" || !host.contains('.') {
        return false;
    }
    ![
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".home.arpa",
        ".onion",
    ]
    .iter()
    .any(|suffix| host.ends_with(suffix))
}

/// Loopback, private, link-local and other addresses that aren't on the
/// public internet (what a host may resolve to).
pub fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                // Carrier-grade NAT (100.64.0.0/10), also Tailscale's range.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(std::net::IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (first & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

/// A relay someone else's list may send us to: `wss://`, on a public host.
fn keep_relay(url: &RelayUrl) -> bool {
    url.as_str_without_trailing_slash().starts_with("wss://")
        && url.domain().is_some_and(is_public_host)
}

/// Where someone receives private messages: their kind 10050 list, else
/// their NIP-65 read relays, else `fallback`. Only `wss://` relays on
/// public hosts are taken from their lists.
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
    inbox.retain(keep_relay);
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

    #[test]
    fn public_hosts_and_private_addresses() {
        for h in [
            "relay.damus.io",
            "nos.lol",
            "purplepag.es",
            "a.b.example.co.uk",
        ] {
            assert!(is_public_host(h), "{h}");
        }
        for h in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "::1",
            "[::1]",
            "fe80::1",
            "relay",
            "nas.local",
            "router.lan",
            "svc.internal",
            "x.localhost",
            "x.home.arpa",
            "",
        ] {
            assert!(!is_public_host(h), "{h}");
        }
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "::1",
            "::",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn only_public_wss_relays_are_taken_from_someones_list() {
        let relay = loop {
            if let Ok(r) = MockRelay::run().await {
                break r;
            }
        };
        let url = relay.url().await;
        let them = Keys::generate();
        let client = Client::default();
        client.add_relay(&url).await.unwrap();
        client
            .connect()
            .and_wait(std::time::Duration::from_secs(3))
            .await;
        let listed = [
            "ws://relay.example.com",
            "wss://127.0.0.1:7777",
            "wss://localhost",
            "wss://10.0.0.2",
            "wss://[::1]:4443",
            "wss://nas.local",
            "wss://inbox.example.com",
        ];
        let mut b = EventBuilder::new(Kind::InboxRelays, "");
        for r in listed {
            b = b.tag(Tag::parse(["relay", r]).unwrap());
        }
        let ev = them
            .sign_event(b.finalize_unsigned(them.public_key()))
            .unwrap();
        client.send_event(&ev).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let fallback = [RelayUrl::parse("wss://fallback.example.com").unwrap()];
        let inbox = inbox_relays(&client, them.public_key(), &fallback).await;
        assert_eq!(
            inbox,
            [RelayUrl::parse("wss://inbox.example.com").unwrap()],
            "the one public wss relay"
        );
        // A list with nothing usable falls back.
        let other = Keys::generate();
        let ev = other
            .sign_event(
                EventBuilder::new(Kind::InboxRelays, "")
                    .tag(Tag::parse(["relay", "ws://127.0.0.1:7777"]).unwrap())
                    .finalize_unsigned(other.public_key()),
            )
            .unwrap();
        client.send_event(&ev).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(
            inbox_relays(&client, other.public_key(), &fallback).await,
            fallback
        );
        client.shutdown().await;
    }
}
