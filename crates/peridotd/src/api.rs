//! What the panel and the `peridot` command can ask the daemon.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use nostr_sdk::prelude::*;
use peridot_sync::identity::Identity;
use peridot_sync::manifest::{Choices, Manifest};
use peridot_sync::recovery;
use peridot_sync::sync::{Offer, valid_name, valid_plugin_id, valid_source};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::app::App;
use crate::pair;

fn parse<T: DeserializeOwned>(params: Value) -> Result<T> {
    let params = if params.is_null() { json!({}) } else { params };
    Ok(serde_json::from_value(params)?)
}

pub async fn dispatch(app: &Arc<App>, method: &str, params: Value) -> Result<Value> {
    match method {
        "status" => Ok(app.snapshot().await),

        // ── Getting started ────────────────────────────────────────────
        "setup.start_fresh" => {
            if app.is_set_up().await {
                bail!("this computer is already set up");
            }
            let identity = Identity::generate();
            identity.save(&app.secrets).await?;
            app.start_engine(identity, true).await?;
            Ok(json!({"ok": true}))
        }
        "setup.leave" => {
            // This computer only; your others keep syncing.
            app.leave().await?;
            Ok(json!({"ok": true}))
        }

        // ── Sync ───────────────────────────────────────────────────────
        "sync.now" => {
            app.engine().await?;
            app.nudge.notify_one();
            Ok(json!({"ok": true}))
        }
        "sync.pause" => {
            #[derive(Deserialize)]
            struct P {
                paused: bool,
            }
            let p: P = parse(params)?;
            let mut cfg = app.config.read().await.clone();
            cfg.paused = p.paused;
            app.save_config(cfg).await?;
            app.emit_state().await;
            Ok(json!({"paused": p.paused}))
        }
        "sync.auto_apply" => {
            #[derive(Deserialize)]
            struct P {
                on: bool,
            }
            let p: P = parse(params)?;
            let mut cfg = app.config.read().await.clone();
            cfg.auto_apply = p.on;
            app.save_config(cfg).await?;
            app.emit_state().await;
            Ok(json!({"auto_apply": p.on}))
        }
        "apply" => {
            // Empty = everything incoming (never conflicts).
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                paths: Vec<String>,
            }
            let p: P = parse(params)?;
            let report = app.engine().await?.apply(&p.paths).await?;
            app.nudge.notify_one();
            app.emit_state().await;
            Ok(json!(report))
        }
        "conflict.keep_local" => {
            #[derive(Deserialize)]
            struct P {
                path: String,
            }
            let p: P = parse(params)?;
            app.engine().await?.keep_local(&p.path).await?;
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }
        "history.undo" => {
            #[derive(Deserialize)]
            struct P {
                id: i64,
            }
            let p: P = parse(params)?;
            let restored = app.engine().await?.undo(p.id).await?;
            app.nudge.notify_one();
            app.emit_state().await;
            Ok(json!({"restored": restored}))
        }

        // ── What syncs ─────────────────────────────────────────────────
        "items.defaults" => Ok(json!(
            Manifest::defaults()
                .into_iter()
                .map(|(pattern, tier)| json!({"pattern": pattern, "tier": tier}))
                .collect::<Vec<_>>()
        )),
        "items.set" => {
            let choices: Choices = parse(params)?;
            let mut cfg = app.config.read().await.clone();
            cfg.sync = choices;
            app.save_config(cfg).await?;
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }

        // ── Themes and plugins from your other computers ───────────────
        "offer.accept" => {
            let offer: Offer = offer_from(params)?;
            let engine = app.engine().await?;
            match &offer {
                Offer::Theme { name, .. } => {
                    run_omarchy("omarchy-theme-set", &[name]).await?;
                    engine.theme_applied(name)?;
                }
                Offer::InstallTheme { url, .. } => {
                    run_omarchy("omarchy-theme-install", &[url]).await?
                }
                // You confirmed in Peridot; the command would otherwise ask again.
                Offer::InstallPlugin { url, .. } => {
                    run_omarchy("omarchy-plugin-add", &[url, "--yes"]).await?
                }
            }
            app.nudge.notify_one();
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }
        "offer.dismiss" => {
            let offer: Offer = offer_from(params)?;
            app.engine().await?.dismiss(&offer)?;
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }

        // ── Your computers ─────────────────────────────────────────────
        "device.rename" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
            }
            let p: P = parse(params)?;
            let name: String = p
                .name
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .take(40)
                .collect();
            let mut cfg = app.config.read().await.clone();
            cfg.device_name = (!name.is_empty()).then_some(name);
            app.save_config(cfg).await?;
            // Takes effect for your other computers on the next announce.
            if app.is_set_up().await {
                let identity = app.engine().await?.identity().clone();
                app.start_engine(identity, false).await?;
            }
            Ok(json!({"ok": true}))
        }
        "device.remove" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
            }
            let p: P = parse(params)?;
            let engine = app.engine().await?;
            if p.id == engine.device_id() {
                bail!("to remove this computer, use \"Stop syncing here\"");
            }
            engine.remove_device(&p.id).await?;
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }

        // ── Pairing ────────────────────────────────────────────────────
        "pair.new" => Ok(json!(pair::start_new(app).await?)),
        "pair.join" => {
            #[derive(Deserialize)]
            struct P {
                code: String,
            }
            let p: P = parse(params)?;
            Ok(json!(pair::start_existing(app, &p.code).await?))
        }
        "pair.confirm" => {
            #[derive(Deserialize)]
            struct P {
                matches: bool,
            }
            let p: P = parse(params)?;
            pair::confirm(app, p.matches).await?;
            Ok(json!({"ok": true}))
        }
        "pair.cancel" => {
            pair::cancel(app).await;
            Ok(json!({"ok": true}))
        }

        // ── Recovery kit ───────────────────────────────────────────────
        "recovery.create" => {
            let engine = app.engine().await?;
            let words = recovery::generate_words();
            let keys = engine.identity().keys.clone();
            let w = words.clone();
            let code = tokio::task::spawn_blocking(move || recovery::seal_key(&keys, &w)).await??;
            let qr = opal_kit::qr::svg_data_url(&code)?;
            Ok(json!({
                "words": words.replace('-', " "),
                "code": code,
                "qr": qr,
            }))
        }
        "recovery.save_page" => {
            #[derive(Deserialize)]
            struct P {
                code: String,
            }
            let p: P = parse(params)?;
            anyhow::ensure!(p.code.starts_with("ncryptsec1"), "not a recovery code");
            let qr = opal_kit::qr::svg_data_url(&p.code)?;
            let page = recovery::kit_page(&p.code, &qr, &ymd(Timestamp::now().as_secs()));
            let dir = documents_dir(&app.home);
            std::fs::create_dir_all(&dir)?;
            let path = dir.join("Peridot recovery kit.html");
            write_private(&path, page.as_bytes())?;
            Ok(json!({"path": path}))
        }
        "recovery.restore" => {
            #[derive(Deserialize)]
            struct P {
                code: String,
                words: String,
            }
            let p: P = parse(params)?;
            if app.is_set_up().await {
                bail!("this computer is already set up");
            }
            let (code, words) = (p.code.clone(), p.words.clone());
            let keys =
                tokio::task::spawn_blocking(move || recovery::open_kit(&code, &words)).await??;
            let identity = fetch_identity(app, keys).await?;
            identity.save(&app.secrets).await?;
            app.start_engine(identity, false).await?;
            Ok(json!({"ok": true}))
        }

        other => bail!("unknown method: {other}"),
    }
}

fn offer_from(params: Value) -> Result<Offer> {
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum O {
        Theme { name: String },
        InstallTheme { name: String, url: String },
        InstallPlugin { name: String, url: String },
    }
    // Checked again here: these become command arguments.
    let offer = match parse::<O>(params)? {
        O::Theme { name } if valid_name(&name) => Offer::Theme {
            name,
            from: String::new(),
        },
        O::InstallTheme { name, url } if valid_name(&name) && valid_source(&url) => {
            Offer::InstallTheme {
                name,
                url,
                from: String::new(),
            }
        }
        O::InstallPlugin { name, url } if valid_plugin_id(&name) && valid_source(&url) => {
            Offer::InstallPlugin {
                name,
                url,
                from: String::new(),
            }
        }
        _ => bail!("that isn't something Peridot can install"),
    };
    Ok(offer)
}

/// Run an Omarchy command in your session (outside the daemon's sandbox)
/// with fixed arguments, and wait for it.
async fn run_omarchy(program: &str, args: &[&str]) -> Result<()> {
    let mut cmd = tokio::process::Command::new("systemd-run");
    cmd.args([
        "--user",
        "--quiet",
        "--collect",
        "--wait",
        "--pipe",
        "--",
        program,
    ])
    .args(args)
    .stdin(std::process::Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(300), cmd.output()).await??;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().last().unwrap_or("it failed");
        bail!("{program}: {why}");
    }
    Ok(())
}

/// A restored key's sync secret, from its root event on the servers.
async fn fetch_identity(app: &Arc<App>, keys: Keys) -> Result<Identity> {
    let relays = opal_kit::relays::parse_urls(&app.config.read().await.relays);
    let client = Client::default();
    for r in &relays {
        let _ = client.add_relay(r).await;
    }
    client
        .connect()
        .and_wait(opal_kit::relays::CONNECT_WAIT)
        .await;
    let filter = Filter::new()
        .author(keys.public_key())
        .kind(Kind::Custom(peridot_sync::DATA_KIND))
        .identifier(Identity::root_name(&keys));
    let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
        .iter()
        .map(|r| (r.clone(), vec![filter.clone()]))
        .collect();
    let events = client
        .fetch_events(targets)
        .timeout(Duration::from_secs(15))
        .await;
    client.shutdown().await;
    let events = events?;
    let root = events.iter().max_by_key(|e| e.created_at).ok_or_else(|| {
        anyhow::anyhow!(
            "couldn't find your settings on the servers; check your connection and try again"
        )
    })?;
    Identity::from_root(keys, root)
}

/// `YYYY-MM-DD` (UTC) for a Unix time.
fn ymd(secs: u64) -> String {
    // Howard Hinnant's days-to-civil.
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn documents_dir(home: &std::path::Path) -> std::path::PathBuf {
    let dirs = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
    for line in dirs.lines() {
        if let Some(v) = line.strip_prefix("XDG_DOCUMENTS_DIR=") {
            let v = v
                .trim_matches('"')
                .replace("$HOME", &home.to_string_lossy());
            let p = std::path::PathBuf::from(v);
            if p.starts_with(home) && p != home {
                return p;
            }
        }
    }
    home.join("Documents")
}

fn write_private(path: &std::path::Path, data: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn formats_dates() {
        assert_eq!(super::ymd(0), "1970-01-01");
        assert_eq!(super::ymd(1_790_349_633), "2026-09-25");
        assert_eq!(super::ymd(951_782_400), "2000-02-29");
    }
}
