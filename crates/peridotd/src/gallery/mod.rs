//! The Gallery: themes, plugins and setups from other Omarchy users, with
//! real likes and reviews, ranked by people you trust. Everything public
//! here is an ordinary Nostr event (see `peridot_sync::gallery`), published
//! with the same identity that syncs your settings.

pub mod registry;
pub mod store;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use nostr_sdk::prelude::*;
use opal_kit::signer::sign_within;
use peridot_sync::gallery::{
    self as proto, ItemKind, KIND_DELETE, KIND_FOLLOWS, KIND_LABEL, KIND_LIKE, KIND_PROFILE,
    KIND_REACTION, KIND_REVIEW, KIND_SETUP, NAMESPACE, Profile, Review, Seen, Setup, SetupSpec,
    Wot,
};
use peridot_sync::signer::IdentitySigner;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

pub use store::{GalleryStore, Item};

const SUB_ID: &str = "peridot-gallery";
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Look back this far past the cursor when catching up.
const CATCH_UP_MARGIN: u64 = 30 * 60;
/// How old a catalogue or the follow graph may be before it is fetched again.
const REGISTRY_MAX_AGE: u64 = 24 * 3600;
const GRAPH_MAX_AGE: u64 = 24 * 3600;
const PROFILE_MAX_AGE: u64 = 7 * 24 * 3600;
const CATCH_UP_EVERY: Duration = Duration::from_secs(15 * 60);
/// At most this many of your follows have their own follows fetched.
const GRAPH_FANOUT: usize = 300;

pub struct GalleryParams {
    pub store: GalleryStore,
    pub signer: Arc<dyn IdentitySigner>,
    pub relays: Vec<RelayUrl>,
    pub home: PathBuf,
    pub plugins_url: String,
    pub themes_url: String,
}

pub struct Gallery {
    pub store: GalleryStore,
    signer: Arc<dyn IdentitySigner>,
    me: PublicKey,
    client: Client,
    relays: Vec<RelayUrl>,
    home: PathBuf,
    http: reqwest::Client,
    plugins_url: String,
    themes_url: String,
    wot: RwLock<Wot>,
    /// The profile this key has, once known (None = looked, none there).
    profile: RwLock<Option<Option<Profile>>>,
    last_error: RwLock<Option<String>>,
}

/// A theme or plugin with its standing.
#[derive(Debug, Clone, Serialize)]
pub struct ItemView {
    #[serde(flatten)]
    pub item: Item,
    pub likes: usize,
    /// Likes weighted by who gave them.
    pub score: u32,
    pub liked: bool,
    pub reviews: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f32>,
    pub installed: bool,
    /// Names of people you follow who like it (a few).
    pub liked_by: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SetupView {
    pub coordinate: String,
    pub likes: usize,
    pub score: u32,
    pub liked: bool,
    pub pubkey: String,
    pub author: String,
    pub mine: bool,
    pub following: bool,
    pub created_at: u64,
    #[serde(flatten)]
    pub spec: SetupSpec,
    /// How many of its themes and plugins are already here.
    pub installed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewView {
    pub id: String,
    pub pubkey: String,
    pub author: String,
    pub mine: bool,
    pub following: bool,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    pub created_at: u64,
}

impl Gallery {
    pub fn new(p: GalleryParams) -> anyhow::Result<Self> {
        opal_core::identity::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .timeout(registry::FETCH_TIMEOUT)
            .user_agent(format!("peridot/{}", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            me: p.signer.pubkey(),
            store: p.store,
            signer: p.signer,
            client: Client::default(),
            relays: p.relays,
            home: p.home,
            http,
            plugins_url: p.plugins_url,
            themes_url: p.themes_url,
            wot: RwLock::new(Wot::default()),
            profile: RwLock::new(None),
            last_error: RwLock::new(None),
        })
    }

    pub fn me(&self) -> PublicKey {
        self.me
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn connect(&self) {
        for r in &self.relays {
            let _ = self.client.add_relay(r).await;
        }
        self.client
            .connect()
            .and_wait(opal_kit::relays::CONNECT_WAIT)
            .await;
    }

    pub async fn error(&self) -> Option<String> {
        self.last_error.read().await.clone()
    }

    async fn set_error(&self, e: Option<String>) {
        *self.last_error.write().await = e;
    }

    // ── Catalogues ────────────────────────────────────────────────────

    /// Fetch the catalogues if they're older than a day (or forced).
    pub async fn refresh_registries(&self, force: bool) -> anyhow::Result<()> {
        let now = now();
        let mut errors = Vec::new();
        for (source, url) in [
            ("plugins", self.plugins_url.as_str()),
            ("themes", self.themes_url.as_str()),
        ] {
            let checked: u64 = self
                .store
                .meta(&format!("checked:{source}"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            if !force && now.saturating_sub(checked) < REGISTRY_MAX_AGE {
                continue;
            }
            match registry::refresh(&self.http, &self.store, source, url, now).await {
                Ok(n) if n > 0 => tracing::info!("gallery: {n} {source} from the catalogue"),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("gallery: couldn't read the {source} catalogue: {e:#}");
                    errors.push(format!("{source}: {e}"));
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            bail!("couldn't read the catalogues ({})", errors.join("; "))
        }
    }

    // ── Events in ─────────────────────────────────────────────────────

    fn filters(&self, since: u64) -> Vec<Filter> {
        let since = Timestamp::from(since);
        vec![
            Filter::new().kind(Kind::Custom(KIND_LIKE)).since(since),
            Filter::new()
                .kind(Kind::Custom(KIND_REACTION))
                .custom_tag(SingleLetterTag::LOWERCASE_K, KIND_SETUP.to_string())
                .since(since),
            Filter::new()
                .kind(Kind::Custom(KIND_REVIEW))
                .custom_tag(SingleLetterTag::UPPERCASE_K, "web")
                .since(since),
            Filter::new()
                .kind(Kind::Custom(KIND_LABEL))
                .custom_tag(SingleLetterTag::UPPERCASE_L, NAMESPACE)
                .since(since),
            Filter::new().kind(Kind::Custom(KIND_SETUP)).since(since),
            Filter::new()
                .kind(Kind::Custom(KIND_DELETE))
                .custom_tags(
                    SingleLetterTag::LOWERCASE_K,
                    [
                        KIND_LIKE.to_string(),
                        KIND_REACTION.to_string(),
                        KIND_REVIEW.to_string(),
                        KIND_SETUP.to_string(),
                    ],
                )
                .since(since),
        ]
    }

    fn targets(&self, filters: Vec<Filter>) -> Vec<(RelayUrl, Vec<Filter>)> {
        self.relays
            .iter()
            .map(|r| (r.clone(), filters.clone()))
            .collect()
    }

    fn since(&self) -> u64 {
        self.store
            .meta("since")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Everything since the last time, with a margin. Returns how many
    /// events changed something.
    pub async fn catch_up(&self) -> anyhow::Result<usize> {
        let since = self.since().saturating_sub(CATCH_UP_MARGIN);
        let started = now();
        let events = self
            .client
            .fetch_events(self.targets(self.filters(since)))
            .timeout(FETCH_TIMEOUT)
            .await
            .context("couldn't reach the gallery relays")?;
        let mut n = 0;
        for ev in events.iter() {
            if self.ingest(ev) {
                n += 1;
            }
        }
        self.store.set_meta("since", &started.to_string())?;
        Ok(n)
    }

    pub async fn subscribe(&self) -> anyhow::Result<()> {
        let since = now().saturating_sub(60);
        self.client
            .subscribe(self.targets(self.filters(since)))
            .with_id(SubscriptionId::new(SUB_ID))
            .await?;
        Ok(())
    }

    /// Take one event in. True if it changed what we know.
    pub fn ingest(&self, ev: &Event) -> bool {
        let Some(seen) = proto::read(ev) else {
            return false;
        };
        let r = match seen {
            Seen::Like(l) => self.store.put_like(&l),
            Seen::Review(r) => self.store.put_review(&r),
            Seen::Listing(l) => self.store.add_listing(&l, now()).map(|_| true),
            Seen::Setup(s) => self.store.put_setup(&s),
            Seen::Deleted { pubkey, ids } => self.store.delete(&pubkey, &ids).map(|n| n > 0),
        };
        match r {
            Ok(changed) => changed,
            Err(e) => {
                tracing::debug!("gallery: couldn't keep an event: {e}");
                false
            }
        }
    }

    // ── People ────────────────────────────────────────────────────────

    /// Your follow list and your follows' follow lists, if older than a
    /// day; then the trust weights.
    pub async fn refresh_graph(&self, force: bool) -> anyhow::Result<()> {
        let now = now();
        let mine = self.store.follows(&self.me);
        let stale = mine
            .as_ref()
            .is_none_or(|(_, _, fetched)| now.saturating_sub(*fetched) > GRAPH_MAX_AGE);
        if force || stale {
            self.fetch_follows(&[self.me]).await?;
            self.fetch_profiles(&[self.me], true).await;
            let follows = self
                .store
                .follows(&self.me)
                .map(|(f, ..)| f)
                .unwrap_or_default();
            let theirs: Vec<PublicKey> = follows.into_iter().take(GRAPH_FANOUT).collect();
            let need: Vec<PublicKey> = theirs
                .iter()
                .filter(|pk| {
                    self.store
                        .follows(pk)
                        .is_none_or(|(_, _, f)| now.saturating_sub(f) > GRAPH_MAX_AGE)
                })
                .copied()
                .collect();
            if !need.is_empty() {
                let _ = self.fetch_follows(&need).await;
            }
        }
        self.rebuild_wot().await;
        Ok(())
    }

    async fn rebuild_wot(&self) {
        let follows = self
            .store
            .follows(&self.me)
            .map(|(f, ..)| f)
            .unwrap_or_default();
        let theirs: Vec<Vec<PublicKey>> = follows
            .iter()
            .take(GRAPH_FANOUT)
            .filter_map(|pk| self.store.follows(pk).map(|(f, ..)| f))
            .collect();
        *self.wot.write().await = Wot::new(Some(self.me), &follows, &theirs);
    }

    /// Newest kind 3 of each key, into the store (an empty list when a key
    /// has none, so we don't ask again today).
    async fn fetch_follows(&self, keys: &[PublicKey]) -> anyhow::Result<()> {
        let now = now();
        let filter = Filter::new()
            .authors(keys.iter().copied())
            .kind(Kind::Custom(KIND_FOLLOWS));
        let events = self
            .client
            .fetch_events(self.targets(vec![filter]))
            .timeout(FETCH_TIMEOUT)
            .await
            .context("couldn't reach the gallery relays")?;
        let mut newest: HashMap<PublicKey, &Event> = HashMap::new();
        for ev in events.iter() {
            let e = newest.entry(ev.pubkey).or_insert(ev);
            if ev.created_at > e.created_at {
                *e = ev;
            }
        }
        for pk in keys {
            match newest.get(pk) {
                Some(ev) => self.store.put_follows(
                    pk,
                    &proto::followed(ev),
                    ev.created_at.as_secs(),
                    now,
                )?,
                None => self.store.put_follows(pk, &[], 0, now)?,
            }
        }
        Ok(())
    }

    /// Names for keys we haven't looked up lately.
    pub async fn fetch_profiles(&self, keys: &[PublicKey], force: bool) {
        let now = now();
        let want: Vec<PublicKey> = if force {
            keys.to_vec()
        } else {
            self.store.stale_profiles(keys, now, PROFILE_MAX_AGE)
        };
        if want.is_empty() {
            return;
        }
        let filter = Filter::new()
            .authors(want.iter().copied())
            .kind(Kind::Custom(KIND_PROFILE));
        let Ok(events) = self
            .client
            .fetch_events(self.targets(vec![filter]))
            .timeout(FETCH_TIMEOUT)
            .await
        else {
            return;
        };
        let mut found = HashSet::new();
        for ev in events.iter() {
            if let Some(p) = proto::read_profile(ev) {
                found.insert(ev.pubkey);
                let _ = self
                    .store
                    .put_profile(&ev.pubkey, &p, ev.created_at.as_secs(), now);
            }
        }
        for pk in &want {
            if !found.contains(pk) {
                let _ = self.store.touch_profile(pk, now);
            }
        }
        if want.contains(&self.me) {
            let mine = self.store.profile(&self.me).map(|(p, _)| p);
            *self.profile.write().await = Some(mine.filter(|p| !p.name.is_empty()));
        }
    }

    /// A name to show for a key: their profile's, else a short npub.
    pub fn name_of(&self, pk: &PublicKey) -> String {
        self.store
            .profile(pk)
            .map(|(p, _)| p.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| short_npub(pk))
    }

    /// Your own profile, if this key has one (None until checked).
    pub async fn my_profile(&self) -> Option<Option<Profile>> {
        self.profile.read().await.clone()
    }

    /// Give this key a name, when it has no profile yet. A profile from
    /// elsewhere (Opal, another app) is never overwritten from here.
    pub async fn set_profile(&self, name: &str) -> anyhow::Result<Profile> {
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.len() <= 60,
            "pick a name up to 60 characters"
        );
        self.fetch_profiles(&[self.me], true).await;
        if let Some(Some(p)) = self.my_profile().await {
            bail!(
                "this key already has a name ({}); change it in a Nostr app",
                p.name
            );
        }
        let unsigned =
            proto::profile(name, Some("Omarchy user (via Peridot)")).finalize_unsigned(self.me);
        let ev = self.publish(unsigned).await?;
        let p = proto::read_profile(&ev).unwrap_or_default();
        let _ = self
            .store
            .put_profile(&self.me, &p, ev.created_at.as_secs(), now());
        *self.profile.write().await = Some(Some(p.clone()));
        Ok(p)
    }

    /// Follow (or unfollow) someone: their key is added to your follow
    /// list, which any Nostr app you use later will show.
    pub async fn follow(&self, who: PublicKey, on: bool) -> anyhow::Result<()> {
        if who == self.me {
            bail!("that's you");
        }
        // Always from the newest list on the relays, never a stale copy.
        let filter = Filter::new()
            .author(self.me)
            .kind(Kind::Custom(KIND_FOLLOWS));
        let events = self
            .client
            .fetch_events(self.targets(vec![filter]))
            .timeout(FETCH_TIMEOUT)
            .await
            .context("couldn't reach the gallery relays")?;
        let current = events.iter().max_by_key(|e| e.created_at);
        let (add, remove) = if on {
            (vec![who], vec![])
        } else {
            (vec![], vec![who])
        };
        let unsigned = proto::follows(current, &add, &remove).finalize_unsigned(self.me);
        let ev = self.publish(unsigned).await?;
        self.store.put_follows(
            &self.me,
            &proto::followed(&ev),
            ev.created_at.as_secs(),
            now(),
        )?;
        if on {
            self.fetch_profiles(&[who], false).await;
        }
        self.rebuild_wot().await;
        Ok(())
    }

    /// People you follow, with names.
    pub async fn following(&self) -> Vec<Value> {
        let list = self
            .store
            .follows(&self.me)
            .map(|(f, ..)| f)
            .unwrap_or_default();
        self.fetch_profiles(&list, false).await;
        list.iter()
            .map(|pk| {
                json!({
                    "pubkey": pk.to_hex(),
                    "npub": pk.to_bech32().ok(),
                    "name": self.name_of(pk),
                })
            })
            .collect()
    }

    // ── Reading the gallery ───────────────────────────────────────────

    /// Repositories installed here, as canonical URLs.
    fn installed(&self) -> HashSet<String> {
        peridot_sync::sync::local_state(&self.home, "")
            .into_iter()
            .flat_map(|s| match s {
                peridot_sync::envelope::StateEntry::Themes { themes, .. } => themes,
                peridot_sync::envelope::StateEntry::Plugins { plugins, .. } => plugins,
                _ => Vec::new(),
            })
            .filter_map(|s| proto::canonical_url(&s.url))
            .collect()
    }

    pub async fn list(
        &self,
        kind: Option<ItemKind>,
        query: &str,
        sort: &str,
        offset: usize,
        limit: usize,
    ) -> anyhow::Result<(Vec<ItemView>, usize)> {
        let items = self.store.items(kind, query)?;
        let likes = self.store.likes()?;
        let stats = self.store.review_stats()?;
        let installed = self.installed();
        let wot = self.wot.read().await;
        let mut views: Vec<ItemView> = items
            .into_iter()
            .map(|item| {
                let likers = likes.get(&item.url).map(Vec::as_slice).unwrap_or(&[]);
                let (reviews, rating) = stats.get(&item.url).copied().unwrap_or((0, None));
                ItemView {
                    likes: likers.len(),
                    score: wot.score(likers.iter()),
                    liked: likers.contains(&self.me),
                    reviews,
                    rating,
                    installed: installed.contains(&item.url),
                    liked_by: likers
                        .iter()
                        .filter(|pk| **pk != self.me && wot.follows(pk))
                        .take(3)
                        .map(|pk| self.name_of(pk))
                        .collect(),
                    item,
                }
            })
            .collect();
        match sort {
            "name" => views.sort_by_key(|v| v.item.name.to_lowercase()),
            "stars" => views.sort_by(|a, b| {
                b.item
                    .stars
                    .cmp(&a.item.stars)
                    .then_with(|| b.score.cmp(&a.score))
            }),
            _ => views.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| b.likes.cmp(&a.likes))
                    .then_with(|| b.reviews.cmp(&a.reviews))
                    .then_with(|| b.item.stars.cmp(&a.item.stars))
                    .then_with(|| a.item.name.cmp(&b.item.name))
            }),
        }
        let total = views.len();
        let page: Vec<ItemView> = views
            .into_iter()
            .skip(offset)
            .take(limit.clamp(1, 200))
            .collect();
        Ok((page, total))
    }

    pub async fn item(&self, url: &str) -> anyhow::Result<Option<ItemView>> {
        let url = proto::canonical_url(url).context("that isn't a repository address")?;
        let (views, _) = self.list(None, "", "top", 0, usize::MAX).await?;
        Ok(views.into_iter().find(|v| v.item.url == url))
    }

    pub async fn reviews(&self, url: &str) -> anyhow::Result<Vec<ReviewView>> {
        let url = proto::canonical_url(url).context("that isn't a repository address")?;
        let reviews = self.store.reviews(&url)?;
        let authors: Vec<PublicKey> = reviews.iter().map(|r| r.pubkey).collect();
        self.fetch_profiles(&authors, false).await;
        let wot = self.wot.read().await;
        Ok(reviews
            .into_iter()
            .map(|r: Review| ReviewView {
                id: r.id.to_hex(),
                pubkey: r.pubkey.to_hex(),
                author: self.name_of(&r.pubkey),
                mine: r.pubkey == self.me,
                following: wot.follows(&r.pubkey),
                text: r.text,
                rating: r.rating,
                created_at: r.created_at,
            })
            .collect())
    }

    pub async fn setups(&self, query: &str) -> anyhow::Result<Vec<SetupView>> {
        let setups = self.store.setups()?;
        let authors: Vec<PublicKey> = setups.iter().map(|s| s.pubkey).collect();
        self.fetch_profiles(&authors, false).await;
        let installed = self.installed();
        let likes = self.store.likes()?;
        let wot = self.wot.read().await;
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut views: Vec<SetupView> = setups
            .into_iter()
            .map(|s: Setup| {
                let total = s.spec.themes.len() + s.spec.plugins.len();
                let likers = likes.get(&s.coordinate()).map(Vec::as_slice).unwrap_or(&[]);
                let have = s
                    .spec
                    .themes
                    .iter()
                    .chain(&s.spec.plugins)
                    .filter(|u| installed.contains(*u))
                    .count();
                SetupView {
                    coordinate: s.coordinate(),
                    likes: likers.len(),
                    score: wot.score(likers.iter()),
                    liked: likers.contains(&self.me),
                    pubkey: s.pubkey.to_hex(),
                    author: self.name_of(&s.pubkey),
                    mine: s.pubkey == self.me,
                    following: wot.follows(&s.pubkey),
                    created_at: s.created_at,
                    spec: s.spec,
                    installed: have,
                    total,
                }
            })
            .filter(|v| {
                words.is_empty() || {
                    let hay =
                        format!("{} {} {}", v.spec.title, v.spec.summary, v.author).to_lowercase();
                    words.iter().all(|w| hay.contains(w.as_str()))
                }
            })
            .collect();
        // Yours first, then people you follow, then the best liked, then newest.
        views.sort_by(|a, b| {
            b.mine
                .cmp(&a.mine)
                .then_with(|| b.following.cmp(&a.following))
                .then_with(|| b.score.cmp(&a.score))
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        Ok(views)
    }

    /// What a setup published from this computer would say.
    pub fn my_setup(&self) -> SetupSpec {
        let mut spec = SetupSpec::default();
        for s in peridot_sync::sync::local_state(&self.home, "") {
            match s {
                peridot_sync::envelope::StateEntry::Theme { name, .. } => spec.theme = Some(name),
                peridot_sync::envelope::StateEntry::Themes { themes, .. } => {
                    spec.themes = themes
                        .into_iter()
                        .filter_map(|s| proto::canonical_url(&s.url))
                        .collect()
                }
                peridot_sync::envelope::StateEntry::Plugins { plugins, .. } => {
                    spec.plugins = plugins
                        .into_iter()
                        .filter_map(|s| proto::canonical_url(&s.url))
                        .collect()
                }
            }
        }
        spec
    }

    // ── Writing ───────────────────────────────────────────────────────

    /// Sign and send to the gallery relays; keep it locally right away.
    async fn publish(&self, unsigned: UnsignedEvent) -> anyhow::Result<Event> {
        let ev = sign_within(self.signer.as_ref(), unsigned, self.signer.sign_timeout())
            .await
            .map_err(gallery_sign_err)?;
        self.connect().await;
        opal_kit::relays::publish(&self.client, &ev, &self.relays)
            .await
            .map_err(|e| anyhow::anyhow!("couldn't publish: {e}"))?;
        self.ingest(&ev);
        Ok(ev)
    }

    pub async fn like(&self, url: &str, on: bool) -> anyhow::Result<()> {
        let url = proto::canonical_url(url).context("that isn't a repository address")?;
        let mine = self.store.my_like(&url, &self.me)?;
        match (on, mine) {
            (true, Some(_)) | (false, None) => Ok(()),
            (true, None) => {
                self.publish(proto::like(&url, self.me)).await?;
                Ok(())
            }
            (false, Some(id)) => {
                self.publish(proto::delete(&[(id, KIND_LIKE)], self.me))
                    .await?;
                self.store.delete(&self.me, &[id])?;
                Ok(())
            }
        }
    }

    pub async fn review(
        &self,
        url: &str,
        text: &str,
        rating: Option<u8>,
    ) -> anyhow::Result<ReviewView> {
        let url = proto::canonical_url(url).context("that isn't a repository address")?;
        let text = text.trim();
        anyhow::ensure!(!text.is_empty(), "write something first");
        anyhow::ensure!(text.len() <= proto::MAX_TEXT, "that review is too long");
        if let Some(r) = rating {
            anyhow::ensure!((1..=5).contains(&r), "a rating is 1 to 5");
        }
        let ev = self
            .publish(proto::review(&url, text, rating, self.me))
            .await?;
        Ok(ReviewView {
            id: ev.id.to_hex(),
            pubkey: self.me.to_hex(),
            author: self.name_of(&self.me),
            mine: true,
            following: false,
            text: text.to_string(),
            rating,
            created_at: ev.created_at.as_secs(),
        })
    }

    pub async fn unreview(&self, id: &str) -> anyhow::Result<()> {
        let id = EventId::from_hex(id).context("no such review")?;
        self.publish(proto::delete(&[(id, KIND_REVIEW)], self.me))
            .await?;
        self.store.delete(&self.me, &[id])?;
        Ok(())
    }

    /// Put a repository on the map for everyone (a theme or plugin that
    /// isn't in the catalogues, yours or not).
    pub async fn list_item(&self, url: &str, kind: ItemKind, name: &str) -> anyhow::Result<Item> {
        let url = proto::canonical_url(url).context("that isn't a repository address")?;
        let name = name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.len() <= proto::MAX_TITLE,
            "give it a name"
        );
        self.publish(proto::listing(&url, kind, name, self.me))
            .await?;
        self.store
            .item(&url)?
            .context("published, but it isn't showing yet")
    }

    pub async fn publish_setup(&self, spec: SetupSpec) -> anyhow::Result<SetupView> {
        anyhow::ensure!(!spec.title.trim().is_empty(), "give your setup a title");
        anyhow::ensure!(
            spec.theme.is_some() || !spec.themes.is_empty() || !spec.plugins.is_empty(),
            "there's nothing to publish yet: no theme or plugin from git on this computer"
        );
        let slug = proto::slug(&spec.title);
        let ev = self.publish(proto::setup(&spec, &slug, self.me)).await?;
        let Some(Seen::Setup(s)) = proto::read(&ev) else {
            bail!("the setup didn't read back");
        };
        let total = s.spec.themes.len() + s.spec.plugins.len();
        Ok(SetupView {
            coordinate: s.coordinate(),
            likes: 0,
            score: 0,
            liked: false,
            pubkey: self.me.to_hex(),
            author: self.name_of(&self.me),
            mine: true,
            following: false,
            created_at: s.created_at,
            spec: s.spec,
            installed: total,
            total,
        })
    }

    pub async fn like_setup(&self, coordinate: &str, on: bool) -> anyhow::Result<()> {
        let (author, _) =
            proto::parse_setup_coordinate(coordinate).context("that isn't a setup")?;
        let mine = self.store.my_like(coordinate, &self.me)?;
        match (on, mine) {
            (true, Some(_)) | (false, None) => Ok(()),
            (true, None) => {
                self.publish(proto::like_setup(coordinate, author, self.me))
                    .await?;
                Ok(())
            }
            (false, Some(id)) => {
                self.publish(proto::delete(&[(id, KIND_REACTION)], self.me))
                    .await?;
                self.store.delete(&self.me, &[id])?;
                Ok(())
            }
        }
    }

    pub async fn remove_setup(&self, coordinate: &str) -> anyhow::Result<()> {
        let s = self.store.setup(coordinate)?.context("no such setup")?;
        anyhow::ensure!(s.pubkey == self.me, "that isn't your setup");
        self.publish(proto::delete(&[(s.id, KIND_SETUP)], self.me))
            .await?;
        self.store.delete(&self.me, &[s.id])?;
        Ok(())
    }

    /// Upload a screenshot for a setup: public, unencrypted, by choice.
    pub async fn upload_image(
        &self,
        sharer: &crate::share::Sharer,
        path: &std::path::Path,
    ) -> anyhow::Result<String> {
        let data = tokio::fs::read(path).await?;
        anyhow::ensure!(
            data.len() <= 8 * 1024 * 1024,
            "that picture is too big (8 MB at most)"
        );
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mime = peridot_sync::share::mime_for(&name);
        anyhow::ensure!(mime.starts_with("image/"), "that isn't a picture");
        sharer.upload_public(&data, mime).await
    }

    /// The commands to run for a setup, in order: theme installs, plugin
    /// installs, then the theme switch. Only what isn't here yet.
    pub fn setup_steps(&self, s: &Setup) -> Vec<Step> {
        let installed = self.installed();
        let mut steps = Vec::new();
        for u in &s.spec.themes {
            if !installed.contains(u) {
                steps.push(Step::InstallTheme(u.clone()));
            }
        }
        for u in &s.spec.plugins {
            if !installed.contains(u) {
                steps.push(Step::InstallPlugin(u.clone()));
            }
        }
        if let Some(t) = &s.spec.theme {
            steps.push(Step::SwitchTheme(t.clone()));
        }
        steps
    }

    pub fn wot_size(&self) -> (usize, usize) {
        let follows = self
            .store
            .follows(&self.me)
            .map(|(f, ..)| f.len())
            .unwrap_or(0);
        let second: BTreeSet<PublicKey> = self
            .store
            .follows(&self.me)
            .map(|(f, ..)| f)
            .unwrap_or_default()
            .iter()
            .take(GRAPH_FANOUT)
            .filter_map(|pk| self.store.follows(pk).map(|(f, ..)| f))
            .flatten()
            .collect();
        (follows, second.len())
    }
}

/// One thing to run for a setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    InstallTheme(String),
    InstallPlugin(String),
    SwitchTheme(String),
}

fn gallery_sign_err(e: peridot_sync::signer::SignError) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("didn't declare kind") {
        anyhow::anyhow!(
            "Pair Peridot with Opal again first: the Gallery needs permissions Opal doesn't have yet (Peridot's panel, or `peridot opal pair`)"
        )
    } else {
        anyhow::anyhow!("{msg}")
    }
}

pub fn short_npub(pk: &PublicKey) -> String {
    let n = pk.to_bech32().unwrap_or_else(|_| pk.to_hex());
    format!("{}…{}", &n[..12], &n[n.len() - 4..])
}

fn now() -> u64 {
    Timestamp::now().as_secs()
}

/// Keep the Gallery fresh in the background: catalogues daily, the
/// follow graph daily, likes and reviews live plus a catch-up every
/// quarter hour.
pub fn spawn(app: Arc<crate::app::App>, gallery: Arc<Gallery>) -> JoinHandle<()> {
    tokio::spawn(async move {
        gallery.connect().await;
        if let Err(e) = gallery.refresh_registries(false).await {
            gallery.set_error(Some(e.to_string())).await;
        }
        if let Err(e) = gallery.refresh_graph(false).await {
            tracing::info!("gallery: {e:#}");
        }
        match gallery.catch_up().await {
            Ok(_) => gallery.set_error(None).await,
            Err(e) => gallery.set_error(Some(e.to_string())).await,
        }
        app.emit_state().await;
        if let Err(e) = gallery.subscribe().await {
            tracing::warn!("gallery: couldn't subscribe: {e}");
        }
        let mut notifications = gallery.client().notifications();
        let mut catch_up = tokio::time::interval(CATCH_UP_EVERY);
        catch_up.tick().await;
        let mut daily = tokio::time::interval(Duration::from_secs(3600));
        daily.tick().await;
        loop {
            tokio::select! {
                n = notifications.next() => {
                    let Some(n) = n else { break };
                    if let ClientNotification::Event { event, .. } = n
                        && gallery.ingest(&event)
                    {
                        app.emit("gallery", json!({"changed": true}));
                    }
                }
                _ = catch_up.tick() => {
                    match gallery.catch_up().await {
                        Ok(n) => {
                            gallery.set_error(None).await;
                            if n > 0 {
                                app.emit("gallery", json!({"changed": true}));
                            }
                        }
                        Err(e) => gallery.set_error(Some(e.to_string())).await,
                    }
                }
                _ = daily.tick() => {
                    let _ = gallery.refresh_registries(false).await;
                    let _ = gallery.refresh_graph(false).await;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn npubs_shorten() {
        let pk = Keys::generate().public_key();
        let s = short_npub(&pk);
        assert!(
            s.starts_with("npub1") && s.contains('…') && s.len() < 20,
            "{s}"
        );
    }
}
