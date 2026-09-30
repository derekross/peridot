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
/// The body is read in pieces and dropped as soon as it passes this, so a
/// server can't fill memory whatever it claims in Content-Length.
const MAX_BYTES: usize = 16 * 1024 * 1024;

/// A catalogue address Peridot will read: https, or plain http on this
/// computer (a local copy for development and tests).
pub fn allowed_url(url: &str) -> bool {
    url.starts_with("https://") || is_loopback_http(url)
}

/// `http://127.0.0.1…`, `http://localhost…`, `http://[::1]…`.
pub fn is_loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Fetch `url` unless it hasn't changed since `etag`. Ok(None) = unchanged.
pub async fn fetch(
    http: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
) -> anyhow::Result<Option<(Vec<u8>, Option<String>)>> {
    fetch_capped(http, url, etag, MAX_BYTES).await
}

async fn fetch_capped(
    http: &reqwest::Client,
    url: &str,
    etag: Option<&str>,
    max: usize,
) -> anyhow::Result<Option<(Vec<u8>, Option<String>)>> {
    anyhow::ensure!(allowed_url(url), "{url}: catalogs are read over https only");
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
    let body = read_capped(res, max)
        .await
        .map_err(|e| anyhow::anyhow!("{url}: {e}"))?;
    Ok(Some((body, etag)))
}

/// The body of a response, refused as soon as it goes past `max` bytes
/// (whatever Content-Length said, or didn't).
pub async fn read_capped(mut res: reqwest::Response, max: usize) -> anyhow::Result<Vec<u8>> {
    if res.content_length().is_some_and(|n| n > max as u64) {
        anyhow::bail!("too big");
    }
    let mut out = Vec::with_capacity(res.content_length().unwrap_or(0).min(max as u64) as usize);
    while let Some(chunk) = res.chunk().await? {
        if out.len() + chunk.len() > max {
            anyhow::bail!("too big");
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
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

    /// One response per connection: a status line, optional Content-Length,
    /// then `body`.
    async fn serve(body: Vec<u8>, with_length: bool) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    break;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let mut head = String::from(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"v9\"\r\nConnection: close\r\n",
                    );
                    if with_length {
                        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
                    }
                    head.push_str("\r\n");
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        base
    }

    #[tokio::test]
    async fn bodies_are_capped_whatever_the_headers_say() {
        opal_core::identity::ensure_crypto_provider();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let body = vec![b'x'; 1000];
        // Announced and too big: refused before reading.
        let base = serve(body.clone(), true).await;
        let e = fetch_capped(&http, &base, None, 500).await.unwrap_err();
        assert!(e.to_string().contains("too big"), "{e}");
        // Unannounced and too big: refused while reading.
        let base = serve(body.clone(), false).await;
        let e = fetch_capped(&http, &base, None, 500).await.unwrap_err();
        assert!(e.to_string().contains("too big"), "{e}");
        // Within the cap: the body and its ETag.
        let base = serve(body.clone(), false).await;
        let (got, etag) = fetch_capped(&http, &base, None, 1000)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, body);
        assert_eq!(etag.as_deref(), Some("\"v9\""));
        // Only https, or this computer over http.
        let e = fetch_capped(&http, "http://plugins.omarchy.org/catalog.json", None, 1000)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("https only"), "{e}");
        assert!(allowed_url("https://plugins.omarchy.org/catalog.json"));
        assert!(allowed_url("http://127.0.0.1:8080/plugins"));
        assert!(allowed_url("http://localhost:8080/plugins"));
        assert!(allowed_url("http://[::1]:8080/plugins"));
        assert!(!allowed_url("http://10.0.0.1/plugins"));
        assert!(!allowed_url("http://127.0.0.1.evil.example/plugins"));
        assert!(!allowed_url("ftp://127.0.0.1/plugins"));
    }

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
