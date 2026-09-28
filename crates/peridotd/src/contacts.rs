//! Who a message goes to: an npub, a hex key, an nprofile, or a
//! `name@domain` address (NIP-05), looked up over https.
//!
//! The lookup is the one place Peridot fetches from a site someone else
//! named, so it's kept narrow: https only, no redirects, a small body,
//! and never a host on this computer or its network.

use std::sync::OnceLock;

use anyhow::{Context, bail};
use nostr_sdk::prelude::*;
use peridot_sync::dm::{is_private_ip, is_public_host};

/// NIP-05 documents are a few hundred bytes; this is generous.
const MAX_BODY: usize = 64 * 1024;
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The client lookups go through, whatever the caller's is set up for.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        opal_core::identity::ensure_crypto_provider();
        reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .user_agent(format!("peridot/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("a plain https client builds")
    })
}

/// Resolve `who` to a key. NIP-05 lookups use their own client (see the
/// module notes); `_http` stays for callers until they drop it.
pub async fn resolve(_http: &reqwest::Client, who: &str) -> anyhow::Result<PublicKey> {
    let who = who.trim().trim_start_matches("nostr:");
    if who.is_empty() {
        bail!("who should it go to? An npub or a name@domain address");
    }
    if let Ok(pk) = PublicKey::parse(who) {
        return Ok(pk);
    }
    if who.starts_with("nprofile1")
        && let Ok(p) = Nip19Profile::from_bech32(who)
    {
        return Ok(p.public_key);
    }
    if !who.contains('@') && !who.contains('.') {
        bail!("that isn't an npub or a name@domain address");
    }
    let address = Nip05Address::parse(who)
        .map_err(|_| anyhow::anyhow!("that isn't a name@domain address"))?;
    let domain = address.domain();
    if !is_public_host(domain) {
        bail!("{domain} is a local address; a name@domain address lives on a public site");
    }
    // A public name that resolves to this network is refused too.
    if let Ok(addrs) = tokio::net::lookup_host((domain, 443)).await
        && addrs.into_iter().any(|a| is_private_ip(a.ip()))
    {
        bail!("{domain} points at a local address");
    }
    let res = client()
        .get(address.url().as_str())
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("couldn't reach {domain}"))?;
    if !res.status().is_success() {
        bail!("{domain} doesn't know {who}");
    }
    let body = crate::gallery::registry::read_capped(res, MAX_BODY)
        .await
        .with_context(|| format!("couldn't read {domain}'s answer"))?;
    let body = String::from_utf8_lossy(&body);
    let profile = Nip05Profile::from_raw_json(&address, &body)
        .map_err(|_| anyhow::anyhow!("{domain} doesn't know {who}"))?;
    Ok(profile.public_key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn keys_parse_without_the_network() {
        opal_core::identity::ensure_crypto_provider();
        let http = reqwest::Client::new();
        let k = Keys::generate().public_key();
        assert_eq!(resolve(&http, &k.to_hex()).await.unwrap(), k);
        assert_eq!(resolve(&http, &k.to_bech32().unwrap()).await.unwrap(), k);
        assert_eq!(
            resolve(&http, &format!("nostr:{}", k.to_bech32().unwrap()))
                .await
                .unwrap(),
            k
        );
        assert!(resolve(&http, "").await.is_err());
        assert!(resolve(&http, "derek").await.is_err());
    }

    #[tokio::test]
    async fn local_addresses_are_refused_before_any_request() {
        let http = reqwest::Client::new();
        for who in [
            "derek@localhost",
            "derek@127.0.0.1",
            "derek@10.0.0.5",
            "derek@192.168.1.20",
            "derek@172.16.0.9",
            "derek@nas.local",
            "derek@router.lan",
            "derek@printer",
        ] {
            let e = resolve(&http, who).await.unwrap_err().to_string();
            assert!(
                e.contains("local address") || e.contains("isn't a name@domain"),
                "{who}: {e}"
            );
        }
    }
}
