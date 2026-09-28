//! Who a message goes to: an npub, a hex key, an nprofile, or a
//! `name@domain` address (NIP-05), looked up over https.

use anyhow::{Context, bail};
use nostr_sdk::prelude::*;

/// Resolve `who` to a key. `http` does the NIP-05 lookups.
pub async fn resolve(http: &reqwest::Client, who: &str) -> anyhow::Result<PublicKey> {
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
    let res = http
        .get(address.url().as_str())
        .header("Accept", "application/json")
        .send()
        .await
        .with_context(|| format!("couldn't reach {}", address.domain()))?;
    if !res.status().is_success() {
        bail!("{} doesn't know {who}", address.domain());
    }
    let body = res.text().await?;
    let profile = Nip05Profile::from_raw_json(&address, &body)
        .map_err(|_| anyhow::anyhow!("{} doesn't know {who}", address.domain()))?;
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
}
