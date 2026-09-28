//! The Gallery's public side: themes, plugins and setups that Omarchy
//! users like, review and share with each other, as ordinary Nostr events
//! anyone can read.
//!
//! A theme or plugin is identified by its repository URL, the same string
//! `omarchy-theme-install` and `omarchy-plugin-add` take, so nothing needs
//! to be registered anywhere before it can be liked:
//!
//! - **Like:** kind 17 (NIP-25 website reaction) with an `r` tag holding the
//!   URL. Taking a like back is a NIP-09 deletion of it.
//! - **Review:** kind 1111 (NIP-22 comment) rooted at the URL with NIP-73
//!   `I`/`K` tags, plus an optional `rating` tag (1 to 5).
//! - **Listing:** kind 1985 (NIP-32 label) in the `omarchy` namespace,
//!   `theme` or `plugin`, so a repository that isn't in any registry shows
//!   up for everyone once its author publishes it.
//! - **Setup:** kind 30490, an addressable event listing the theme in use
//!   and the themes and plugins installed, with a title, a blurb and an
//!   optional screenshot. Installing one is a series of the same commands
//!   the offers from your other computers run.
//! - **Follow:** NIP-02 kind 3, appended to whatever the key already has.
//! - **Profile:** NIP-01 kind 0, created only when the key has none.
//!
//! Likes are ranked by who gave them (see [`Wot`]): people you follow count
//! more than strangers, and people they follow count more than that.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use nostr_sdk::prelude::*;
use serde::{Deserialize, Serialize};

pub const KIND_LIKE: u16 = 17;
pub const KIND_REVIEW: u16 = 1111;
pub const KIND_LABEL: u16 = 1985;
pub const KIND_SETUP: u16 = 30490;
pub const KIND_DELETE: u16 = 5;
pub const KIND_FOLLOWS: u16 = 3;
pub const KIND_PROFILE: u16 = 0;
/// The NIP-32 namespace listings live in.
pub const NAMESPACE: &str = "omarchy";

/// Longest review, title or blurb kept (bytes).
pub const MAX_TEXT: usize = 2000;
pub const MAX_TITLE: usize = 80;

/// What a repository holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Theme,
    Plugin,
}

impl ItemKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ItemKind::Theme => "theme",
            ItemKind::Plugin => "plugin",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "theme" => Some(ItemKind::Theme),
            "plugin" => Some(ItemKind::Plugin),
            _ => None,
        }
    }
}

/// One form of a repository URL, so `https://github.com/A/B.git`,
/// `https://github.com/a/b/` and `https://github.com/A/B` are the same
/// thing. Only public https hosts Peridot would install from are accepted.
pub fn canonical_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    let host = host.to_ascii_lowercase();
    if !["github.com", "gitlab.com", "codeberg.org"].contains(&host.as_str()) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    // A file inside a repository (themes.json lists built-in themes by
    // their folder in basecamp/omarchy): keep the repository only.
    if parts.len() > 2 && parts[2] == "tree" {
        parts.truncate(2);
    }
    if parts.len() != 2 {
        return None;
    }
    // Hosts treat these case-insensitively; one spelling keeps likes together.
    let owner = parts[0].to_ascii_lowercase();
    let repo = parts[1]
        .strip_suffix(".git")
        .unwrap_or(parts[1])
        .to_ascii_lowercase();
    let (owner, repo) = (owner.as_str(), repo.as_str());
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() < 120
            && !s.starts_with('.')
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c))
    };
    if !ok(owner) || !ok(repo) {
        return None;
    }
    Some(format!("https://{host}/{owner}/{repo}"))
}

/// `owner/repo` for display.
pub fn short_name(url: &str) -> String {
    url.trim_start_matches("https://")
        .split_once('/')
        .map(|x| x.1)
        .unwrap_or(url)
        .to_string()
}

// ── Building events ─────────────────────────────────────────────────

/// A like: kind 17 with the URL in an `r` tag.
pub fn like(url: &str, pubkey: PublicKey) -> UnsignedEvent {
    EventBuilder::new(Kind::Custom(KIND_LIKE), "+")
        .tag(Tag::custom("r", [url]))
        .tag(Tag::custom("k", ["web"]))
        .finalize_unsigned(pubkey)
}

/// Taking something back (a like, a review, a setup): NIP-09, with the
/// kind of each event in a `k` tag so relays can be asked for deletions
/// of just these kinds.
pub fn delete(ids: &[(EventId, u16)], pubkey: PublicKey) -> UnsignedEvent {
    let mut b = EventBuilder::new(Kind::Custom(KIND_DELETE), "");
    let mut kinds = BTreeSet::new();
    for (id, kind) in ids {
        b = b.tag(Tag::event(*id));
        kinds.insert(*kind);
    }
    for k in kinds {
        b = b.tag(Tag::custom("k", [k.to_string()]));
    }
    b.finalize_unsigned(pubkey)
}

/// A review: a NIP-22 comment on the URL, with an optional 1-5 rating.
pub fn review(url: &str, text: &str, rating: Option<u8>, pubkey: PublicKey) -> UnsignedEvent {
    let mut b = EventBuilder::new(Kind::Custom(KIND_REVIEW), text)
        .tag(Tag::custom("I", [url]))
        .tag(Tag::custom("K", ["web"]))
        .tag(Tag::custom("i", [url]))
        .tag(Tag::custom("k", ["web"]));
    if let Some(r) = rating.filter(|r| (1..=5).contains(r)) {
        b = b.tag(Tag::custom("rating", [r.to_string()]));
    }
    b.finalize_unsigned(pubkey)
}

/// A listing: "this repository is an Omarchy theme/plugin" (NIP-32).
pub fn listing(url: &str, kind: ItemKind, name: &str, pubkey: PublicKey) -> UnsignedEvent {
    EventBuilder::new(Kind::Custom(KIND_LABEL), name)
        .tag(Tag::custom("L", [NAMESPACE]))
        .tag(Tag::custom("l", [kind.as_str(), NAMESPACE]))
        .tag(Tag::custom("r", [url]))
        .finalize_unsigned(pubkey)
}

/// What a setup is made of.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupSpec {
    pub title: String,
    pub summary: String,
    /// The theme in use (its Omarchy name, e.g. `tokyo-night`).
    pub theme: Option<String>,
    /// Themes installed from git.
    pub themes: Vec<String>,
    /// Plugins installed from git.
    pub plugins: Vec<String>,
    /// A screenshot, already uploaded.
    pub image: Option<String>,
}

/// A setup: kind 30490 under `d = slug`.
pub fn setup(spec: &SetupSpec, slug: &str, pubkey: PublicKey) -> UnsignedEvent {
    let title = clip(&spec.title, MAX_TITLE);
    let summary = clip(&spec.summary, MAX_TEXT);
    let mut b = EventBuilder::new(Kind::Custom(KIND_SETUP), summary.clone())
        .tag(Tag::identifier(slug))
        .tag(Tag::custom("title", [title.as_str()]))
        .tag(Tag::custom("summary", [summary.as_str()]))
        .tag(Tag::custom("alt", [format!("An Omarchy setup: {title}")]))
        .tag(Tag::hashtag(NAMESPACE));
    if let Some(t) = &spec.theme {
        b = b.tag(Tag::custom("theme", [t]));
    }
    for u in &spec.themes {
        b = b.tag(Tag::custom("r", [u.as_str(), "theme"]));
    }
    for u in &spec.plugins {
        b = b.tag(Tag::custom("r", [u.as_str(), "plugin"]));
    }
    if let Some(img) = &spec.image {
        b = b.tag(Tag::custom("image", [img]));
    }
    b.finalize_unsigned(pubkey)
}

/// A follow list with `add` appended to `current` (the key's newest kind 3,
/// if any). Everything else in it (relays in content, petnames) is kept.
pub fn follows(current: Option<&Event>, add: &[PublicKey], remove: &[PublicKey]) -> EventBuilder {
    let mut tags: Vec<Tag> = current
        .map(|e| e.tags.iter().cloned().collect())
        .unwrap_or_default();
    let content = current.map(|e| e.content.clone()).unwrap_or_default();
    tags.retain(|t| {
        !(t.kind() == "p"
            && t.content()
                .and_then(|c| PublicKey::from_hex(c).ok())
                .is_some_and(|pk| remove.contains(&pk)))
    });
    let have: BTreeSet<String> = tags
        .iter()
        .filter(|t| t.kind() == "p")
        .filter_map(|t| t.content().map(String::from))
        .collect();
    for pk in add {
        if !have.contains(&pk.to_hex()) {
            tags.push(Tag::public_key(*pk));
        }
    }
    EventBuilder::new(Kind::Custom(KIND_FOLLOWS), content).tags(tags)
}

/// A first profile for a key that has none.
pub fn profile(name: &str, about: Option<&str>) -> EventBuilder {
    let name = clip(name.trim(), MAX_TITLE);
    let mut v = serde_json::json!({ "name": name, "display_name": name });
    if let Some(a) = about {
        v["about"] = serde_json::json!(clip(a, MAX_TEXT));
    }
    EventBuilder::new(Kind::Custom(KIND_PROFILE), v.to_string())
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// A setup's slug from its title (`My Desk` → `my-desk`), never empty.
pub fn slug(title: &str) -> String {
    let mut s = String::new();
    let mut dash = true;
    for c in title.trim().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            s.push('-');
            dash = true;
        }
        if s.len() >= 40 {
            break;
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "setup".into() } else { s }
}

// ── Reading events ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Like {
    pub id: EventId,
    pub url: String,
    pub pubkey: PublicKey,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: EventId,
    pub url: String,
    pub pubkey: PublicKey,
    pub text: String,
    pub rating: Option<u8>,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub id: EventId,
    pub url: String,
    pub kind: ItemKind,
    pub name: String,
    pub pubkey: PublicKey,
    pub created_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub id: EventId,
    pub pubkey: PublicKey,
    pub slug: String,
    pub spec: SetupSpec,
    pub created_at: u64,
}

impl Setup {
    /// `30490:<pubkey>:<slug>`
    pub fn coordinate(&self) -> String {
        format!("{KIND_SETUP}:{}:{}", self.pubkey.to_hex(), self.slug)
    }
}

/// Something the Gallery understands, from any relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    Like(Like),
    Review(Review),
    Listing(Listing),
    Setup(Setup),
    /// Event ids the author took back.
    Deleted {
        pubkey: PublicKey,
        ids: Vec<EventId>,
    },
}

fn tag_value<'a>(ev: &'a Event, name: &str) -> Option<&'a str> {
    ev.tags
        .iter()
        .find(|t| t.kind() == name)
        .and_then(|t| t.content())
}

fn tag_values<'a>(ev: &'a Event, name: &'a str) -> impl Iterator<Item = &'a Tag> {
    ev.tags.iter().filter(move |t| t.kind() == name)
}

/// Read one event; None when it isn't about an Omarchy repository (or is
/// malformed). Signatures are the caller's business (the SDK verifies).
pub fn read(ev: &Event) -> Option<Seen> {
    let created_at = ev.created_at.as_secs();
    match ev.kind.as_u16() {
        KIND_LIKE => {
            if ev.content.trim() == "-" {
                return None;
            }
            let url = canonical_url(tag_value(ev, "r")?)?;
            Some(Seen::Like(Like {
                id: ev.id,
                url,
                pubkey: ev.pubkey,
                created_at,
            }))
        }
        KIND_REVIEW => {
            // Rooted at the URL, and a top-level comment (not a reply).
            let root = canonical_url(tag_value(ev, "I")?)?;
            let parent = tag_value(ev, "i").and_then(canonical_url);
            if parent.as_deref().is_some_and(|p| p != root) {
                return None;
            }
            let text = clip(&ev.content, MAX_TEXT);
            if text.is_empty() {
                return None;
            }
            let rating = tag_value(ev, "rating")
                .and_then(|r| r.parse::<u8>().ok())
                .filter(|r| (1..=5).contains(r));
            Some(Seen::Review(Review {
                id: ev.id,
                url: root,
                pubkey: ev.pubkey,
                text,
                rating,
                created_at,
            }))
        }
        KIND_LABEL => {
            let in_ns = tag_values(ev, "L").any(|t| t.content() == Some(NAMESPACE));
            if !in_ns {
                return None;
            }
            let kind = tag_values(ev, "l")
                .filter(|t| t.as_slice().get(2).map(String::as_str) == Some(NAMESPACE))
                .find_map(|t| t.content().and_then(ItemKind::parse))?;
            let url = canonical_url(tag_value(ev, "r")?)?;
            let name = clip(&ev.content, MAX_TITLE);
            Some(Seen::Listing(Listing {
                id: ev.id,
                url,
                kind,
                name,
                pubkey: ev.pubkey,
                created_at,
            }))
        }
        KIND_SETUP => {
            let slug = ev.tags.identifier()?;
            if slug.is_empty() || slug.len() > 64 {
                return None;
            }
            let mut spec = SetupSpec {
                title: clip(tag_value(ev, "title").unwrap_or(&slug), MAX_TITLE),
                summary: clip(
                    tag_value(ev, "summary").unwrap_or(ev.content.as_str()),
                    MAX_TEXT,
                ),
                theme: tag_value(ev, "theme")
                    .filter(|t| crate::sync::valid_name(t))
                    .map(String::from),
                image: tag_value(ev, "image")
                    .filter(|u| u.starts_with("https://") && u.len() < 300)
                    .map(String::from),
                ..Default::default()
            };
            for t in tag_values(ev, "r") {
                let s = t.as_slice();
                let (Some(url), Some(what)) = (s.get(1), s.get(2)) else {
                    continue;
                };
                let Some(url) = canonical_url(url) else {
                    continue;
                };
                match what.as_str() {
                    "theme" if !spec.themes.contains(&url) => spec.themes.push(url),
                    "plugin" if !spec.plugins.contains(&url) => spec.plugins.push(url),
                    _ => {}
                }
            }
            if spec.themes.is_empty() && spec.plugins.is_empty() && spec.theme.is_none() {
                return None;
            }
            Some(Seen::Setup(Setup {
                id: ev.id,
                pubkey: ev.pubkey,
                slug,
                spec,
                created_at,
            }))
        }
        KIND_DELETE => {
            let ids: Vec<EventId> = ev.tags.event_ids().collect();
            if ids.is_empty() {
                return None;
            }
            Some(Seen::Deleted {
                pubkey: ev.pubkey,
                ids,
            })
        }
        _ => None,
    }
}

/// The keys a kind 3 follows.
pub fn followed(ev: &Event) -> Vec<PublicKey> {
    ev.tags.public_keys().collect()
}

/// Name and picture from a kind 0.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nip05: Option<String>,
}

pub fn read_profile(ev: &Event) -> Option<Profile> {
    let v: serde_json::Value = serde_json::from_str(&ev.content).ok()?;
    let pick = |k: &str| {
        v[k].as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| clip(s, MAX_TITLE))
    };
    let name = pick("display_name")
        .or_else(|| pick("name"))
        .unwrap_or_default();
    Some(Profile {
        name,
        picture: pick("picture").filter(|p| p.starts_with("https://")),
        nip05: pick("nip05"),
    })
}

// ── Ranking ─────────────────────────────────────────────────────────

/// Who you trust, for weighing likes: yourself, the people you follow,
/// and the people they follow.
#[derive(Debug, Clone, Default)]
pub struct Wot {
    me: Option<PublicKey>,
    follows: BTreeSet<PublicKey>,
    second: BTreeSet<PublicKey>,
}

impl Wot {
    pub fn new(me: Option<PublicKey>, follows: &[PublicKey], theirs: &[Vec<PublicKey>]) -> Self {
        let follows: BTreeSet<PublicKey> = follows.iter().copied().collect();
        let mut second = BTreeSet::new();
        for list in theirs {
            for pk in list {
                if !follows.contains(pk) && Some(*pk) != me {
                    second.insert(*pk);
                }
            }
        }
        Self {
            me,
            follows,
            second,
        }
    }

    /// How much one like from `who` is worth.
    pub fn weight(&self, who: &PublicKey) -> u32 {
        if Some(*who) == self.me || self.follows.contains(who) {
            4
        } else if self.second.contains(who) {
            2
        } else {
            1
        }
    }

    pub fn follows(&self, who: &PublicKey) -> bool {
        self.follows.contains(who)
    }

    /// A score for a set of likes: one per person, weighted.
    pub fn score<'a>(&self, likers: impl IntoIterator<Item = &'a PublicKey>) -> u32 {
        let unique: BTreeSet<&PublicKey> = likers.into_iter().collect();
        unique.into_iter().map(|pk| self.weight(pk)).sum()
    }
}

/// Counts per URL from a pile of likes (deleted ones already removed).
pub fn tally(likes: &[Like]) -> HashMap<String, Vec<PublicKey>> {
    let mut by_url: HashMap<String, BTreeMap<PublicKey, u64>> = HashMap::new();
    for l in likes {
        by_url
            .entry(l.url.clone())
            .or_default()
            .insert(l.pubkey, l.created_at);
    }
    by_url
        .into_iter()
        .map(|(u, m)| (u, m.into_keys().collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        Keys::generate()
    }

    #[test]
    fn urls_have_one_form() {
        for u in [
            "https://github.com/Derekross/Omarchy-Calendar.git",
            "https://github.com/derekross/omarchy-calendar/",
            "https://github.com/derekross/omarchy-calendar?x=1",
            "  https://github.com/derekross/omarchy-calendar#readme",
        ] {
            assert_eq!(
                canonical_url(u).as_deref(),
                Some("https://github.com/derekross/omarchy-calendar"),
                "{u}"
            );
        }
        assert_eq!(
            canonical_url("https://github.com/basecamp/omarchy/tree/quattro/themes/catppuccin")
                .as_deref(),
            Some("https://github.com/basecamp/omarchy")
        );
        assert_eq!(canonical_url("http://github.com/a/b"), None);
        assert_eq!(canonical_url("https://evil.example/a/b"), None);
        assert_eq!(canonical_url("https://github.com/a"), None);
        assert_eq!(canonical_url("https://github.com/a/b/c"), None);
        assert_eq!(canonical_url("https://github.com/../b"), None);
        assert_eq!(canonical_url("https://github.com/a/b;rm"), None);
        assert_eq!(short_name("https://github.com/a/b"), "a/b");
    }

    #[test]
    fn like_review_listing_round_trip() {
        let k = keys();
        let url = "https://github.com/derekross/omarchy-calendar";
        let ev = k.sign_event(like(url, k.public_key())).unwrap();
        assert!(
            matches!(read(&ev), Some(Seen::Like(l)) if l.url == url && l.pubkey == k.public_key())
        );

        let ev = k
            .sign_event(review(url, "  Lovely clock  ", Some(5), k.public_key()))
            .unwrap();
        match read(&ev) {
            Some(Seen::Review(r)) => {
                assert_eq!(r.text, "Lovely clock");
                assert_eq!(r.rating, Some(5));
                assert_eq!(r.url, url);
            }
            other => panic!("{other:?}"),
        }
        // A reply to a review is not a review.
        let reply = k
            .sign_event(
                EventBuilder::new(Kind::Custom(KIND_REVIEW), "me too")
                    .tag(Tag::custom("I", [url]))
                    .tag(Tag::custom("K", ["web"]))
                    .tag(Tag::custom("i", ["https://github.com/x/y"]))
                    .finalize_unsigned(k.public_key()),
            )
            .unwrap();
        assert!(read(&reply).is_none());
        let bad_rating = k
            .sign_event(review(url, "hm", Some(9), k.public_key()))
            .unwrap();
        assert!(matches!(read(&bad_rating), Some(Seen::Review(r)) if r.rating.is_none()));

        let ev = k
            .sign_event(listing(
                url,
                ItemKind::Plugin,
                "Calendar Clock",
                k.public_key(),
            ))
            .unwrap();
        assert!(
            matches!(read(&ev), Some(Seen::Listing(l)) if l.kind == ItemKind::Plugin && l.name == "Calendar Clock")
        );
        // Another namespace's label says nothing to us.
        let other = k
            .sign_event(
                EventBuilder::new(Kind::Custom(KIND_LABEL), "")
                    .tag(Tag::custom("L", ["ugc"]))
                    .tag(Tag::custom("l", ["theme", "ugc"]))
                    .tag(Tag::custom("r", [url]))
                    .finalize_unsigned(k.public_key()),
            )
            .unwrap();
        assert!(read(&other).is_none());
    }

    #[test]
    fn setup_round_trip_and_slug() {
        let k = keys();
        let spec = SetupSpec {
            title: "Derek's desk".into(),
            summary: "Tokyo night and a clock".into(),
            theme: Some("tokyo-night".into()),
            themes: vec!["https://github.com/x/omarchy-x-theme".into()],
            plugins: vec![
                "https://github.com/derekross/omarchy-calendar".into(),
                "https://github.com/derekross/omarchy-calendar".into(),
            ],
            image: Some("https://blossom.example/abc.png".into()),
        };
        let ev = k
            .sign_event(setup(&spec, &slug(&spec.title), k.public_key()))
            .unwrap();
        match read(&ev) {
            Some(Seen::Setup(s)) => {
                assert_eq!(s.slug, "derek-s-desk");
                assert_eq!(s.spec.title, spec.title);
                assert_eq!(s.spec.theme.as_deref(), Some("tokyo-night"));
                assert_eq!(s.spec.plugins.len(), 1, "duplicates collapse");
                assert_eq!(s.spec.image, spec.image);
                assert_eq!(
                    s.coordinate(),
                    format!("30490:{}:derek-s-desk", k.public_key().to_hex())
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(slug("  !!  "), "setup");
        assert_eq!(slug("Ünïcode → Desk"), "n-code-desk");
        // A setup that lists nothing is noise.
        let empty = k
            .sign_event(setup(
                &SetupSpec {
                    title: "x".into(),
                    ..Default::default()
                },
                "x",
                k.public_key(),
            ))
            .unwrap();
        assert!(read(&empty).is_none());
    }

    #[test]
    fn follows_merge_and_keep_what_was_there() {
        let k = keys();
        let a = keys().public_key();
        let b = keys().public_key();
        let current = k
            .sign_event(
                EventBuilder::new(Kind::Custom(3), "{\"wss://r\":{}}")
                    .tag(Tag::custom(
                        "p",
                        [a.to_hex(), "wss://r".into(), "alice".into()],
                    ))
                    .finalize_unsigned(k.public_key()),
            )
            .unwrap();
        let ev = k
            .sign_event(follows(Some(&current), &[a, b], &[]).finalize_unsigned(k.public_key()))
            .unwrap();
        assert_eq!(ev.content, "{\"wss://r\":{}}");
        assert_eq!(followed(&ev), vec![a, b]);
        assert!(
            ev.tags
                .iter()
                .any(|t| t.as_slice().get(3).map(String::as_str) == Some("alice"))
        );
        let ev = k
            .sign_event(follows(Some(&ev), &[], &[a]).finalize_unsigned(k.public_key()))
            .unwrap();
        assert_eq!(followed(&ev), vec![b]);
        let fresh = k
            .sign_event(follows(None, &[b], &[]).finalize_unsigned(k.public_key()))
            .unwrap();
        assert_eq!(followed(&fresh), vec![b]);
    }

    #[test]
    fn profiles_and_deletions_read() {
        let k = keys();
        let ev = k
            .sign_event(profile("  Derek ", Some("hi")).finalize_unsigned(k.public_key()))
            .unwrap();
        let p = read_profile(&ev).unwrap();
        assert_eq!(p.name, "Derek");
        let del = k
            .sign_event(delete(&[(ev.id, KIND_LIKE)], k.public_key()))
            .unwrap();
        assert!(matches!(read(&del), Some(Seen::Deleted { ids, .. }) if ids == vec![ev.id]));
        assert!(del.tags.iter().any(|t| t.as_slice() == ["k", "17"]));
    }

    #[test]
    fn likes_from_people_you_trust_count_more() {
        let me = keys().public_key();
        let friend = keys().public_key();
        let fof = keys().public_key();
        let stranger = keys().public_key();
        let wot = Wot::new(Some(me), &[friend], &[vec![fof, friend]]);
        assert_eq!(wot.weight(&me), 4);
        assert_eq!(wot.weight(&friend), 4);
        assert_eq!(wot.weight(&fof), 2);
        assert_eq!(wot.weight(&stranger), 1);
        // One person, one vote.
        assert_eq!(wot.score([&stranger, &stranger, &friend]), 5);
        let url = "https://github.com/a/b".to_string();
        let likes = vec![
            Like {
                id: EventId::from_slice(&[7u8; 32]).unwrap(),
                url: url.clone(),
                pubkey: friend,
                created_at: 1,
            },
            Like {
                id: EventId::from_slice(&[7u8; 32]).unwrap(),
                url: url.clone(),
                pubkey: friend,
                created_at: 2,
            },
            Like {
                id: EventId::from_slice(&[7u8; 32]).unwrap(),
                url: url.clone(),
                pubkey: stranger,
                created_at: 3,
            },
        ];
        let t = tally(&likes);
        assert_eq!(t[&url].len(), 2);
    }
}
