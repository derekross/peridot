//! The catalogues the Gallery starts from: the plugin marketplace
//! (plugins.omarchy.org) and the community theme site (omarchytheme.com).
//! Both publish their data as JSON in the open; Peridot reads it the way
//! their own pages do. Likes and reviews then come from Nostr.

use std::time::Duration;

use peridot_sync::gallery::{ItemKind, canonical_url};
use serde::Deserialize;

use super::store::{GalleryStore, Item};

pub const PLUGINS_URL: &str = "https://plugins.omarchy.org/catalog.json";
pub const THEMES_URL: &str = "https://raw.githubusercontent.com/limehawk/omarchy-theme-website/main/src/data/themes-data.json";
/// Where the marketplace's relative preview paths live.
const PLUGINS_SITE: &str = "https://plugins.omarchy.org/";

pub const FETCH_TIMEOUT: Duration = Duration::from_secs(90);
/// Refuse anything bigger (the plugin catalogue is about 11 MB today).
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// Fetch `url` unless it hasn't changed since `etag`. Ok(None) = unchanged.
pub async fn fetch(
    http: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
) -> anyhow::Result<Option<(Vec<u8>, Option<String>)>> {
    let mut req = http.get(url).header("Accept", "application/json");
    if let Some(e) = etag {
        req = req.header("If-None-Match", e);
    }
    let res = req.send().await?;
    if res.status().as_u16() == 304 {
        return Ok(None);
    }
    if !res.status().is_success() {
        anyhow::bail!("{url}: {}", res.status());
    }
    let etag = res
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if res.content_length().is_some_and(|n| n as usize > MAX_BYTES) {
        anyhow::bail!("{url}: too big");
    }
    let body = res.bytes().await?;
    if body.len() > MAX_BYTES {
        anyhow::bail!("{url}: too big");
    }
    Ok(Some((body.to_vec(), etag)))
}

// ── Plugins ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct PluginCatalog {
    #[serde(default)]
    plugins: Vec<PluginEntry>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PluginEntry {
    id: String,
    name: String,
    description: String,
    author: String,
    category: String,
    tags: Vec<String>,
    repo: String,
    stars: u32,
    #[serde(rename = "installAvailable")]
    install_available: Option<bool>,
    #[serde(rename = "previewThumbnail")]
    preview_thumbnail: Option<String>,
    #[serde(rename = "previewImage")]
    preview_image: Option<String>,
}

pub fn parse_plugins(json: &[u8]) -> anyhow::Result<Vec<Item>> {
    let cat: PluginCatalog = serde_json::from_slice(json)?;
    let mut out = Vec::with_capacity(cat.plugins.len());
    for p in cat.plugins {
        let Some(url) = canonical_url(&p.repo) else {
            continue;
        };
        let preview = p
            .preview_thumbnail
            .or(p.preview_image)
            .filter(|s| !s.is_empty())
            .map(|s| {
                if s.starts_with("https://") {
                    s
                } else {
                    format!("{PLUGINS_SITE}{}", s.trim_start_matches('/'))
                }
            });
        out.push(Item {
            url,
            kind: ItemKind::Plugin,
            name: clean(&p.name, 80),
            author: clean(&p.author, 60),
            description: clean(&p.description, 300),
            category: clean(&p.category, 40),
            tags: p.tags.iter().map(|t| clean(t, 30)).take(6).collect(),
            stars: p.stars,
            preview,
            source: "plugins".into(),
            installable: p.install_available.unwrap_or(true),
            publisher: None,
            catalog_id: Some(clean(&p.id, 100)).filter(|s| !s.is_empty()),
        });
    }
    dedup(&mut out);
    Ok(out)
}

// ── Themes ──────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
#[serde(default)]
struct ThemeEntry {
    name: String,
    github_url: String,
    github_owner: String,
    description: Option<String>,
    preview_url: Option<String>,
    primary_hue: Option<String>,
    is_builtin: Option<u8>,
    stars: u32,
}

/// Community themes only: the ones built into Omarchy live in one
/// repository, can't be installed by URL, and are already on every
/// computer.
pub fn parse_themes(json: &[u8]) -> anyhow::Result<Vec<Item>> {
    let entries: Vec<ThemeEntry> = serde_json::from_slice(json)?;
    let mut out = Vec::with_capacity(entries.len());
    for t in entries {
        if t.is_builtin.unwrap_or(0) != 0 {
            continue;
        }
        let Some(url) = canonical_url(&t.github_url) else {
            continue;
        };
        if url.ends_with("/omarchy") {
            continue;
        }
        out.push(Item {
            url,
            kind: ItemKind::Theme,
            name: clean(&t.name, 80),
            author: clean(&t.github_owner, 60),
            description: clean(t.description.as_deref().unwrap_or(""), 300),
            category: "Theme".into(),
            tags: t
                .primary_hue
                .into_iter()
                .filter(|h| !h.is_empty())
                .map(|h| clean(&h, 30))
                .collect(),
            stars: t.stars,
            preview: t
                .preview_url
                .filter(|p| p.starts_with("https://") && p.len() < 300),
            source: "themes".into(),
            installable: true,
            publisher: None,
            catalog_id: None,
        });
    }
    dedup(&mut out);
    Ok(out)
}

fn clean(s: &str, max: usize) -> String {
    let s: String = s
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// One entry per repository (the first keeps).
fn dedup(items: &mut Vec<Item>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|it| seen.insert(it.url.clone()));
}

/// Load one catalogue into the store unless it hasn't changed.
pub async fn refresh(
    http: &reqwest::Client,
    store: &GalleryStore,
    source: &str,
    url: &str,
    now: u64,
) -> anyhow::Result<usize> {
    let etag_key = format!("etag:{source}");
    let etag = store.meta(&etag_key);
    let Some((body, new_etag)) = fetch(http, url, etag.as_deref()).await? else {
        store.set_meta(&format!("checked:{source}"), &now.to_string())?;
        return Ok(0);
    };
    let items = match source {
        "plugins" => parse_plugins(&body)?,
        _ => parse_themes(&body)?,
    };
    if items.is_empty() {
        anyhow::bail!("{url}: nothing in it");
    }
    store.replace_source(source, &items, now)?;
    if let Some(e) = new_etag {
        store.set_meta(&etag_key, &e)?;
    }
    store.set_meta(&format!("checked:{source}"), &now.to_string())?;
    Ok(items.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_catalogues() {
        let plugins = br#"{"plugins":[
          {"id":"a.b","name":"Clock","description":"A clock","author":"derek","category":"Widgets","tags":["bar","time"],
           "repo":"https://github.com/Derek/Omarchy-Calendar.git","stars":12,"installAvailable":true,
           "previewThumbnail":"assets/img/plugins/x.webp"},
          {"id":"c.d","name":"Dup","description":"","author":"x","category":"Other","tags":[],
           "repo":"https://github.com/derek/omarchy-calendar","stars":1},
          {"id":"e.f","name":"Elsewhere","description":"","author":"x","category":"Other","tags":[],
           "repo":"https://example.com/not/git","stars":1},
          {"id":"g.h","name":"Suite","description":"","author":"x","category":"Desktop","tags":[],
           "repo":"https://github.com/g/h","stars":1,"installAvailable":false}
        ]}"#;
        let items = parse_plugins(plugins).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url, "https://github.com/derek/omarchy-calendar");
        assert_eq!(
            items[0].preview.as_deref(),
            Some("https://plugins.omarchy.org/assets/img/plugins/x.webp")
        );
        assert!(items[0].installable && !items[1].installable);
        assert_eq!(items[0].catalog_id.as_deref(), Some("a.b"));

        let themes = br#"[
          {"name":"Catppuccin","github_url":"https://github.com/basecamp/omarchy/tree/quattro/themes/catppuccin",
           "github_owner":"basecamp","is_builtin":1,"stars":9},
          {"name":"Rose Pine","github_url":"https://github.com/x/omarchy-rose-pine-theme","github_owner":"x",
           "description":"Soho vibes","preview_url":"https://raw.githubusercontent.com/x/y/main/preview.png",
           "primary_hue":"purple","is_builtin":0,"stars":40}
        ]"#;
        let items = parse_themes(themes).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "Rose Pine");
        assert_eq!(items[0].tags, vec!["purple"]);
        assert_eq!(items[0].kind, ItemKind::Theme);
    }
}
