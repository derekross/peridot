//! Recovery kits: a way back if every computer is lost.
//!
//! The kit is your key encrypted (NIP-49, scrypt) with six random words
//! from the EFF's long wordlist (about 77 bits). The list's four dashed
//! words (drop-down, felt-tip, t-shirt, yo-yo) are left out so typed words
//! can be split on dashes as well as spaces. The encrypted key can be
//! printed or saved anywhere; the words are shown once and written down
//! separately. Restoring decrypts the key, then fetches the sync secret
//! from the root event (see [`crate::identity`]).

use nostr_sdk::prelude::*;
use opal_core::nostr::nips::nip49::{EncryptedSecretKey, KeySecurity};
use zeroize::Zeroizing;

use crate::crypto::random_bytes;

const WORDLIST: &str = include_str!("eff_large_wordlist.txt");
const WORDS: usize = 6;
/// scrypt cost for kits: slow to guess, about a second to open.
pub const KIT_LOG_N: u8 = 18;

fn wordlist() -> Vec<&'static str> {
    WORDLIST.lines().filter(|l| !l.is_empty()).collect()
}

/// Six random words, e.g. `canopy-glider-sulfur-mammal-dizzy-overlap`.
pub fn generate_words() -> Zeroizing<String> {
    let list = wordlist();
    let n = list.len() as u32;
    // Rejection sampling keeps every word equally likely.
    let limit = u32::MAX - (u32::MAX % n);
    let mut words = Vec::with_capacity(WORDS);
    while words.len() < WORDS {
        let r = u32::from_le_bytes(random_bytes());
        if r < limit {
            words.push(list[(r % n) as usize]);
        }
    }
    Zeroizing::new(words.join("-"))
}

/// Normalize typed words: any case, separated by spaces, dashes or commas.
pub fn normalize_words(input: &str) -> Result<Zeroizing<String>, String> {
    let list = wordlist();
    let words: Vec<String> = input
        .split(|c: char| c.is_whitespace() || c == '-' || c == ',')
        .filter(|w| !w.is_empty())
        .map(|w| w.to_ascii_lowercase())
        .collect();
    if words.len() != WORDS {
        return Err(format!(
            "the kit has {WORDS} words; you entered {}",
            words.len()
        ));
    }
    for w in &words {
        if !list.contains(&w.as_str()) {
            return Err(format!(
                "\"{w}\" isn't one of the recovery words; check the spelling"
            ));
        }
    }
    Ok(Zeroizing::new(words.join("-")))
}

/// The encrypted key (`ncryptsec1…`) for a kit.
pub fn seal_key(keys: &Keys, words: &str) -> anyhow::Result<String> {
    seal_key_with(keys, words, KIT_LOG_N)
}

pub(crate) fn seal_key_with(keys: &Keys, words: &str, log_n: u8) -> anyhow::Result<String> {
    let enc = EncryptedSecretKey::new(keys.secret_key(), words, log_n, KeySecurity::Medium)?;
    Ok(enc.to_bech32()?)
}

/// Open a kit: the encrypted key and the six words.
pub fn open_kit(ncryptsec: &str, words: &str) -> anyhow::Result<Keys> {
    let words = normalize_words(words).map_err(|e| anyhow::anyhow!(e))?;
    let enc = EncryptedSecretKey::from_bech32(ncryptsec.trim())
        .map_err(|_| anyhow::anyhow!("that isn't a Peridot recovery code"))?;
    let secret = enc
        .decrypt(&words)
        .map_err(|_| anyhow::anyhow!("those words don't open this kit"))?;
    Ok(Keys::new(secret))
}

/// A printable page: the encrypted key as text and QR code, and where to
/// write the words. The words themselves are never in the file.
pub fn kit_page(ncryptsec: &str, qr_data_url: &str, created: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
    };
    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Peridot recovery kit</title>
<style>
body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;color:#111;background:#fff}}
h1{{font-size:1.6rem}} code{{word-break:break-all;font-size:.85rem}}
.box{{border:2px solid #111;border-radius:8px;padding:1rem;margin:1rem 0}}
.words{{height:6rem}} img{{width:14rem;height:14rem}}
</style></head><body>
<h1>Peridot recovery kit</h1>
<p>Created {created}. If you ever lose every computer you use Peridot on, this page and your six recovery words bring your settings back.</p>
<div class="box"><p><strong>Recovery code</strong></p><img alt="QR code of the recovery code" src="{qr}"><p><code>{code}</code></p></div>
<div class="box words"><p><strong>Your six words</strong> (write them here by hand, or keep them somewhere else)</p></div>
<p>To restore: install Peridot, choose <em>Use my recovery kit</em>, then enter the code and the six words.</p>
<p>Keep this page private. Anyone with the page <em>and</em> the words can read your synced settings.</p>
</body></html>
"#,
        created = esc(created),
        qr = esc(qr_data_url),
        code = esc(ncryptsec),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_are_six_from_the_list() {
        let list = wordlist();
        assert_eq!(list.len(), 7772);
        assert!(
            list.iter()
                .all(|w| w.chars().all(|c| c.is_ascii_lowercase()))
        );
        let words = generate_words();
        assert_eq!(words.split('-').count(), 6);
        assert_eq!(*normalize_words(&words.replace('-', " ")).unwrap(), *words);
        assert_ne!(*generate_words(), *generate_words());
    }

    #[test]
    fn kits_open_with_the_right_words_only() {
        let keys = Keys::generate();
        let words = "canopy glider sulfur mammal dizzy overlap";
        let sealed = seal_key_with(&keys, &normalize_words(words).unwrap(), 4).unwrap();
        assert!(sealed.starts_with("ncryptsec1"));
        let back = open_kit(&sealed, "Canopy-Glider-Sulfur-Mammal-Dizzy-Overlap").unwrap();
        assert_eq!(back.public_key(), keys.public_key());
        assert!(open_kit(&sealed, "canopy glider sulfur mammal dizzy dizzy").is_err());
        assert!(open_kit(&sealed, "canopy glider").is_err());
        assert!(open_kit(&sealed, "canopy glider sulfur mammal dizzy notaword").is_err());
        assert!(open_kit("nsec1abc", words).is_err());
    }

    #[test]
    fn the_page_never_contains_the_words_and_escapes_input() {
        let page = kit_page("ncryptsec1abc", "data:image/svg+xml;base64,AAA", "<today>");
        assert!(page.contains("ncryptsec1abc"));
        assert!(page.contains("&lt;today&gt;"));
    }
}
