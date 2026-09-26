//! What the panel and the `peridot` command can ask the daemon.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use nostr_sdk::prelude::*;
use opal_core::import::{ImportOptions, parse_secret};
use peridot_sync::identity::Identity;
use peridot_sync::manifest::{Choices, Manifest};
use peridot_sync::recovery;
use peridot_sync::signer::{IdentitySigner, LocalSigner};
use peridot_sync::sync::{Offer, valid_name, valid_plugin_id, valid_source};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use zeroize::Zeroizing;

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
        "setup.import" => {
            // A key you already have: nsec, hex, ncryptsec or recovery phrase.
            #[derive(Deserialize)]
            struct P {
                secret: Zeroizing<String>,
                #[serde(default)]
                password: Option<Zeroizing<String>>,
            }
            let p: P = parse(params)?;
            if app.is_set_up().await {
                bail!("this computer is already set up");
            }
            let keys = parse_secret(
                &p.secret,
                ImportOptions {
                    ncryptsec_password: p.password.as_deref().map(|s| s.as_str()),
                    ..Default::default()
                },
            )
            .map_err(|e| anyhow::anyhow!("that key couldn't be read: {e}"))?;
            adopt(app, keys.public_key(), Some(keys), None).await?;
            Ok(json!({"ok": true}))
        }
        "setup.use_opal" => {
            // The key stays in Opal; Opal signs for Peridot.
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                pubkey: Option<String>,
            }
            let p: P = parse(params)?;
            if app.is_set_up().await {
                bail!("this computer is already set up");
            }
            let accounts = app.opal.accounts().await;
            if accounts.is_empty() {
                bail!("Opal isn't running or has no key yet");
            }
            let account = match &p.pubkey {
                Some(pk) => accounts.iter().find(|a| &a.pubkey == pk),
                None => accounts.iter().find(|a| a.current).or(accounts.first()),
            }
            .ok_or_else(|| anyhow::anyhow!("Opal has no such account"))?;
            let pubkey = PublicKey::from_hex(&account.pubkey)?;
            adopt(app, pubkey, None, Some(account.label.clone())).await?;
            Ok(json!({"ok": true}))
        }
        "opal.accounts" => Ok(json!(app.opal.accounts().await)),
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

        // ── Servers ────────────────────────────────────────────────────
        "relays.set" => {
            #[derive(Deserialize)]
            struct P {
                relays: Vec<String>,
            }
            let p: P = parse(params)?;
            let mut relays = Vec::new();
            for r in p.relays {
                let r = r.trim().trim_end_matches('/').to_string();
                let url = RelayUrl::parse(&r)
                    .map_err(|_| anyhow::anyhow!("{r} isn't a relay address"))?;
                if !r.starts_with("wss://") && !url.is_local_addr() {
                    bail!("{r}: only secure (wss://) relays are used");
                }
                if !relays.contains(&r) {
                    relays.push(r);
                }
            }
            if relays.is_empty() {
                bail!("keep at least one relay");
            }
            let mut cfg = app.config.read().await.clone();
            cfg.relays = relays;
            app.save_config(cfg).await?;
            // Reconnect and resubscribe with the new list.
            if app.is_set_up().await {
                let identity = app.engine().await?.identity().clone();
                app.start_engine(identity, false).await?;
            }
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }
        "relays.check" => {
            // Before adding one: can we reach it, and does it want a login?
            #[derive(Deserialize)]
            struct P {
                url: String,
            }
            let p: P = parse(params)?;
            let url = RelayUrl::parse(p.url.trim())
                .map_err(|_| anyhow::anyhow!("that isn't a relay address"))?;
            let info = opal_kit::relays::probe(&url, Duration::from_secs(6)).await;
            let client = Client::default();
            client.add_relay(&url).await?;
            client.connect().and_wait(Duration::from_secs(6)).await;
            let connected = client
                .relays()
                .await
                .values()
                .any(|r| r.status().is_connected());
            client.shutdown().await;
            Ok(json!({
                "reachable": connected,
                "auth_required": info.as_ref().map(|i| i.auth_required).unwrap_or(false),
                "max_message_length": info.as_ref().ok().and_then(|i| i.max_message_length),
            }))
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
            let Some(keys) = engine.identity().keys.clone() else {
                bail!(
                    "Opal holds your key, so your Opal backup (an ncryptsec) is your recovery kit: restore it in Opal on the new computer, then choose \"Use your Opal identity\" here"
                );
            };
            let words = recovery::generate_words();
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
            let identity = fetch_identity(app, keys.public_key(), Some(keys))
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("couldn't find your settings on the servers; check your connection and try again")
                })?;
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

/// Start using `pubkey` on this computer. If it already has settings on the
/// servers (a key used with Peridot before), join them; otherwise start a
/// new sync secret and publish its root.
async fn adopt(
    app: &Arc<App>,
    pubkey: PublicKey,
    keys: Option<Keys>,
    name: Option<String>,
) -> Result<()> {
    // Only a definite "nothing there" starts a new sync secret. A locked
    // Opal or an unreachable server must not: that would replace the secret
    // your other computers use.
    let (identity, fresh) = match fetch_identity(app, pubkey, keys.clone()).await? {
        Some(existing) => (existing, false),
        None => {
            tracing::info!("no settings on the servers for this key yet; starting new");
            (
                match keys {
                    Some(k) => Identity::from_keys(k),
                    None => Identity::via_opal(pubkey),
                },
                true,
            )
        }
    };
    identity.save(&app.secrets).await?;
    // start_engine resets per-identity state first, so the name goes after.
    app.start_engine(identity, fresh).await?;
    app.db
        .set_kv("peridot.identity_name", name.as_deref().unwrap_or(""))?;
    app.emit_state().await;
    Ok(())
}

/// A key's sync secret, from its root event on the servers: Some when
/// found, None when no server has one, Err when we couldn't tell (no
/// server reachable, Opal locked). `keys` is the key itself when this
/// computer holds it; otherwise Opal decrypts.
async fn fetch_identity(
    app: &Arc<App>,
    pubkey: PublicKey,
    keys: Option<Keys>,
) -> Result<Option<Identity>> {
    let signer: Arc<dyn IdentitySigner> = match &keys {
        Some(k) => Arc::new(LocalSigner(k.clone())),
        None => Arc::new(crate::opal::OpalSigner::new(app.opal.clone(), pubkey)),
    };
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
        .author(pubkey)
        .kind(Kind::Custom(peridot_sync::DATA_KIND))
        .identifier(Identity::root_name(&pubkey));
    let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
        .iter()
        .map(|r| (r.clone(), vec![filter.clone()]))
        .collect();
    let connected = client
        .relays()
        .await
        .values()
        .any(|r| r.status().is_connected());
    let events = client
        .fetch_events(targets)
        .timeout(Duration::from_secs(15))
        .await;
    client.shutdown().await;
    if !connected {
        bail!("couldn't reach your sync servers; check your connection and try again");
    }
    let events = events?;
    let Some(root) = events.iter().max_by_key(|e| e.created_at) else {
        return Ok(None);
    };
    let identity = Identity::from_root(pubkey, keys, signer.as_ref(), root)
        .await
        .map_err(
            |e| match e.downcast_ref::<peridot_sync::signer::SignError>() {
                Some(peridot_sync::signer::SignError::Unavailable(m)) => {
                    anyhow::anyhow!("{m} (it has to read your existing settings first)")
                }
                _ => e,
            },
        )?;
    Ok(Some(identity))
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
