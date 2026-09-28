//! What the panel and the `peridot` command can ask the daemon.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use nostr_sdk::prelude::*;
use opal_core::import::{ImportOptions, parse_secret};
use peridot_sync::gallery::ItemKind;
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

/// Every request from the socket: class the method, tell who's asking,
/// apply the limits, and route dangerous things through the panel.
pub async fn dispatch_gated(
    app: &Arc<App>,
    peer: &opal_kit::ipc::Peer,
    method: &str,
    mut params: Value,
) -> Result<Value> {
    use crate::authz::{Caller, Class};
    let Some(class) = crate::authz::classify(method) else {
        bail!("unknown method: {method}");
    };
    let caller = app.trust.caller(peer);
    let now = Timestamp::now().as_secs();
    app.limits.check(caller, class, method, now)?;
    // `confirm` and `force` are the panel's words, never a caller's data.
    let confirm = params
        .as_object_mut()
        .and_then(|m| m.remove("confirm"))
        .is_some_and(|v| v == json!(true));
    let force = params
        .as_object_mut()
        .and_then(|m| m.remove("force"))
        .is_some_and(|v| v == json!(true));
    match (class, caller) {
        (Class::Read | Class::Routine, _) => {}
        (Class::Sensitive | Class::Dangerous, Caller::Other) => {
            bail!("only Peridot's own panel and the peridot command may do that")
        }
        (Class::Sensitive, _) => {}
        (Class::Dangerous, Caller::Panel) => {
            if method != "approvals.answer" && !confirm {
                bail!("this needs to be confirmed in the panel");
            }
        }
        (Class::Dangerous, Caller::Cli) => {
            if method == "approvals.answer" {
                bail!("only the panel answers approvals");
            }
            let summary = crate::authz::describe(method, &params);
            let (pending, rx) = app
                .approvals
                .open(method, summary.clone(), caller.name(), now);
            app.emit("approval", json!(pending));
            app.emit_state().await;
            crate::notify::approval_needed(&summary);
            let ok = tokio::time::timeout(crate::approvals::APPROVAL_TIMEOUT, rx).await;
            app.approvals.forget(pending.id);
            app.emit_state().await;
            match ok {
                Ok(Ok(true)) => app.limits.allowed(),
                Ok(Ok(false)) => {
                    app.limits.denied(now);
                    bail!("not allowed in Peridot's panel");
                }
                _ => bail!("Peridot's panel didn't answer; open it and try again"),
            }
        }
    }
    if force && matches!(method, "share.file" | "share.text") && caller == Caller::Panel {
        params["_force"] = json!(true);
    }
    dispatch(app, method, params).await
}

pub async fn dispatch(app: &Arc<App>, method: &str, params: Value) -> Result<Value> {
    match method {
        "approvals.list" => Ok(json!(app.approvals.list())),
        "approvals.answer" => {
            #[derive(Deserialize)]
            struct P {
                id: u64,
                ok: bool,
            }
            let p: P = parse(params)?;
            if !app.approvals.answer(p.id, p.ok) {
                bail!("that request is gone");
            }
            Ok(json!({"ok": true}))
        }
        "pair.view" => {
            // The pairing with its code, for the panel and the command only
            // (the general snapshot leaves the code out).
            let v = app.pairing.lock().await.as_ref().map(|p| p.view());
            Ok(json!(v))
        }
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
            // Pair first (you're here to answer), unless the pairing from
            // before is still good for this account.
            let paired = app.opal.has_token()
                && app
                    .opal
                    .status()
                    .await
                    .is_ok_and(|s| s.pubkey == account.pubkey);
            if !paired {
                app.pair_opal(Some(pubkey))
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", pair_error(&e)))?;
            }
            adopt(app, pubkey, None, Some(account.label.clone())).await?;
            Ok(json!({"ok": true}))
        }
        "opal.accounts" => Ok(json!(app.opal.accounts().await)),
        "opal.pair" => {
            // Pair (again) with Opal: its prompt appears in the bar.
            let pubkey = app
                .engine
                .read()
                .await
                .as_ref()
                .map(|engine| engine.identity().pubkey());
            app.pair_opal(pubkey)
                .await
                .map_err(|e| anyhow::anyhow!("{}", pair_error(&e)))?;
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
            // You asked: worth asking Opal again even after a "no".
            app.opal.clear_hold();
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

        // ── Private links ──────────────────────────────────────────────
        "share.file" => {
            // A file on this computer, by absolute path (the CLI and the
            // Share menu resolve relative ones).
            #[derive(Deserialize)]
            struct P {
                path: String,
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                expire_days: Option<u32>,
                /// The panel showed the warning and you went ahead.
                #[serde(default, rename = "_force")]
                force: bool,
            }
            let p: P = parse(params)?;
            let path = std::path::Path::new(&p.path);
            anyhow::ensure!(path.is_absolute(), "give the file's full path");
            // Never anything the sync would refuse: keys, keyrings, other
            // apps' credentials; nor system files.
            for root in ["/etc", "/proc", "/sys", "/dev", "/run", "/var", "/boot"] {
                anyhow::ensure!(!path.starts_with(root), "that isn't something to share");
            }
            if let Ok(rel) = path.strip_prefix(&app.home)
                && let Some(rel) = rel.to_str()
                && Manifest::new(Default::default()).tier(rel)
                    == Some(peridot_sync::manifest::Tier::Never)
            {
                bail!(
                    "{} looks like a key or a credential; Peridot won't share it",
                    p.path
                );
            }
            let data = read_for_share(path)
                .await
                .map_err(|e| anyhow::anyhow!("{}: {e}", p.path))?;
            if !p.force
                && let Some(what) = peridot_sync::manifest::looks_secret(&data)
            {
                bail!(
                    "{} seems to hold {what}; share it anyway from the panel if you're sure",
                    p.path
                );
            }
            let name = p.name.unwrap_or_else(|| {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into())
            });
            let mime = peridot_sync::share::mime_for(&name);
            let days = p
                .expire_days
                .unwrap_or(app.config.read().await.share.expire_days)
                .clamp(1, 365);
            let share = app
                .sharer()
                .await?
                .share(
                    &name,
                    mime,
                    &data,
                    Duration::from_secs(u64::from(days) * 86400),
                )
                .await
                .map_err(share_err)?;
            app.emit_state().await;
            Ok(json!(share))
        }
        "share.text" => {
            // Clipboard text and the like (small: it travels over the socket).
            #[derive(Deserialize)]
            struct P {
                text: String,
                #[serde(default)]
                name: Option<String>,
                #[serde(default)]
                expire_days: Option<u32>,
                #[serde(default, rename = "_force")]
                force: bool,
            }
            let p: P = parse(params)?;
            anyhow::ensure!(!p.text.trim().is_empty(), "nothing to share");
            if !p.force
                && let Some(what) = peridot_sync::manifest::looks_secret(p.text.as_bytes())
            {
                bail!(
                    "the clipboard seems to hold {what}; share it anyway from the panel if you're sure"
                );
            }
            let name = p.name.unwrap_or_else(|| "clipboard.txt".into());
            let days = p
                .expire_days
                .unwrap_or(app.config.read().await.share.expire_days)
                .clamp(1, 365);
            let share = app
                .sharer()
                .await?
                .share(
                    &name,
                    "text/plain",
                    p.text.as_bytes(),
                    Duration::from_secs(u64::from(days) * 86400),
                )
                .await
                .map_err(share_err)?;
            app.emit_state().await;
            Ok(json!(share))
        }
        "share.list" => Ok(json!(app.sharer().await?.store.list()?)),
        "share.sweep" => {
            // Remove expired links now, even if Opal said no earlier.
            let sharer = app.sharer().await?;
            sharer.clear_hold();
            let removed = sharer.sweep().await;
            app.emit_state().await;
            Ok(json!({"removed": removed}))
        }
        "share.revoke" => {
            #[derive(Deserialize)]
            struct P {
                id: i64,
            }
            let p: P = parse(params)?;
            app.sharer().await?.revoke(p.id).await?;
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }

        "share.send" => {
            // A link straight to someone, as a private message.
            #[derive(Deserialize)]
            struct P {
                id: i64,
                to: String,
            }
            let p: P = parse(params)?;
            let sharer = app.sharer().await?;
            let to = crate::contacts::resolve(&app.http, &p.to).await?;
            let gallery = app.gallery().await.ok();
            let fallback = opal_kit::relays::parse_urls(&app.config.read().await.gallery.relays);
            let share = match &gallery {
                Some(g) => sharer.send(p.id, to, g.client(), &fallback).await,
                None => {
                    let client = Client::default();
                    for r in &fallback {
                        let _ = client.add_relay(r).await;
                    }
                    client
                        .connect()
                        .and_wait(opal_kit::relays::CONNECT_WAIT)
                        .await;
                    let r = sharer.send(p.id, to, &client, &fallback).await;
                    client.shutdown().await;
                    r
                }
            }
            .map_err(share_err)?;
            let name = match &gallery {
                Some(g) => {
                    g.fetch_profiles(&[to], false).await;
                    g.name_of(&to)
                }
                None => crate::gallery::short_npub(&to),
            };
            Ok(json!({"ok": true, "name": share.name, "to": name, "pubkey": to.to_hex()}))
        }
        "contacts.resolve" => {
            #[derive(Deserialize)]
            struct P {
                who: String,
            }
            let p: P = parse(params)?;
            let pk = crate::contacts::resolve(&app.http, &p.who).await?;
            let name = match app.gallery().await {
                Ok(g) => {
                    g.fetch_profiles(&[pk], false).await;
                    g.name_of(&pk)
                }
                Err(_) => crate::gallery::short_npub(&pk),
            };
            Ok(json!({"pubkey": pk.to_hex(), "npub": pk.to_bech32().ok(), "name": name}))
        }

        // ── The Gallery ────────────────────────────────────────────────
        "gallery.list" => {
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                kind: Option<ItemKind>,
                query: String,
                sort: String,
                offset: usize,
                limit: usize,
            }
            let p: P = parse(params)?;
            let g = app.gallery().await?;
            let limit = if p.limit == 0 { 30 } else { p.limit };
            let (items, total) = g.list(p.kind, &p.query, &p.sort, p.offset, limit).await?;
            Ok(json!({"items": items, "total": total}))
        }
        "gallery.item" => {
            #[derive(Deserialize)]
            struct P {
                url: String,
            }
            let p: P = parse(params)?;
            let g = app.gallery().await?;
            let item = g.item(&p.url).await?;
            let reviews = g.reviews(&p.url).await?;
            Ok(json!({"item": item, "reviews": reviews}))
        }
        "gallery.reviews" => {
            #[derive(Deserialize)]
            struct P {
                url: String,
            }
            let p: P = parse(params)?;
            Ok(json!(app.gallery().await?.reviews(&p.url).await?))
        }
        "gallery.like" => {
            #[derive(Deserialize)]
            struct P {
                url: String,
                #[serde(default = "yes")]
                on: bool,
            }
            let p: P = parse(params)?;
            app.gallery()
                .await?
                .like(&p.url, p.on)
                .await
                .map_err(gallery_err)?;
            Ok(json!({"ok": true}))
        }
        "gallery.review" => {
            #[derive(Deserialize)]
            struct P {
                url: String,
                text: String,
                #[serde(default)]
                rating: Option<u8>,
            }
            let p: P = parse(params)?;
            Ok(json!(
                app.gallery()
                    .await?
                    .review(&p.url, &p.text, p.rating)
                    .await
                    .map_err(gallery_err)?
            ))
        }
        "gallery.unreview" => {
            #[derive(Deserialize)]
            struct P {
                id: String,
            }
            let p: P = parse(params)?;
            app.gallery()
                .await?
                .unreview(&p.id)
                .await
                .map_err(gallery_err)?;
            Ok(json!({"ok": true}))
        }
        "gallery.follow" => {
            #[derive(Deserialize)]
            struct P {
                pubkey: String,
                #[serde(default = "yes")]
                on: bool,
            }
            let p: P = parse(params)?;
            let pk =
                PublicKey::parse(&p.pubkey).map_err(|_| anyhow::anyhow!("that isn't a key"))?;
            app.gallery()
                .await?
                .follow(pk, p.on)
                .await
                .map_err(gallery_err)?;
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }
        "gallery.following" => Ok(json!(app.gallery().await?.following().await)),
        "gallery.setups" => {
            #[derive(Deserialize, Default)]
            #[serde(default)]
            struct P {
                query: String,
            }
            let p: P = parse(params)?;
            Ok(json!(app.gallery().await?.setups(&p.query).await?))
        }
        "gallery.setup.mine" => {
            // Everything that could go in, so the panel can offer a choice.
            let g = app.gallery().await?;
            let candidates = g.setup_candidates();
            let theme = g.my_setup().theme;
            let can = theme.is_some() || candidates.iter().any(|c| c.url.is_some());
            Ok(json!({
                "theme": theme,
                "candidates": candidates,
                "can_publish": can,
                "screenshot": last_screenshot(&app.home),
            }))
        }
        "gallery.setup.publish" => {
            #[derive(Deserialize)]
            struct P {
                title: String,
                #[serde(default)]
                summary: String,
                /// A picture to upload for everyone to see (its full path),
                /// or nothing.
                #[serde(default)]
                screenshot: Option<String>,
                /// Which of the candidates to include (their urls). Absent:
                /// everything with a public source. Present: exactly these.
                #[serde(default)]
                include: Option<Vec<String>>,
                /// Leave the current theme's name out.
                #[serde(default)]
                without_theme: bool,
            }
            let p: P = parse(params)?;
            let g = app.gallery().await?;
            let candidates = g.setup_candidates();
            let chosen: Vec<&crate::gallery::Candidate> = match &p.include {
                None => candidates.iter().filter(|c| c.url.is_some()).collect(),
                Some(urls) => {
                    let want: Vec<String> = urls
                        .iter()
                        .filter_map(|u| peridot_sync::gallery::canonical_url(u))
                        .collect();
                    candidates
                        .iter()
                        .filter(|c| {
                            c.url
                                .as_deref()
                                .is_some_and(|u| want.contains(&u.to_string()))
                        })
                        .collect()
                }
            };
            let mut spec = g.my_setup();
            spec.themes = chosen
                .iter()
                .filter(|c| c.kind == ItemKind::Theme)
                .filter_map(|c| c.url.clone())
                .collect();
            spec.plugins = chosen
                .iter()
                .filter(|c| c.kind == ItemKind::Plugin)
                .filter_map(|c| c.url.clone())
                .collect();
            if p.without_theme {
                spec.theme = None;
            }
            spec.title = p.title;
            spec.summary = p.summary;
            if let Some(path) = p.screenshot.filter(|s| !s.trim().is_empty()) {
                let path = std::path::PathBuf::from(path);
                anyhow::ensure!(path.is_absolute(), "give the picture's full path");
                spec.image = Some(
                    g.upload_image(app.sharer().await?.as_ref(), &path)
                        .await
                        .map_err(share_err)?,
                );
            }
            Ok(json!(g.publish_setup(spec).await.map_err(gallery_err)?))
        }
        "gallery.setup.like" => {
            #[derive(Deserialize)]
            struct P {
                coordinate: String,
                #[serde(default = "yes")]
                on: bool,
            }
            let p: P = parse(params)?;
            app.gallery()
                .await?
                .like_setup(&p.coordinate, p.on)
                .await
                .map_err(gallery_err)?;
            Ok(json!({"ok": true}))
        }
        "gallery.setup.remove" => {
            #[derive(Deserialize)]
            struct P {
                coordinate: String,
            }
            let p: P = parse(params)?;
            app.gallery()
                .await?
                .remove_setup(&p.coordinate)
                .await
                .map_err(gallery_err)?;
            Ok(json!({"ok": true}))
        }
        "gallery.setup.install" => {
            // Everything in someone's setup that isn't here yet, then
            // their theme. Each step is one of the commands the offers run.
            #[derive(Deserialize)]
            struct P {
                coordinate: String,
            }
            let p: P = parse(params)?;
            let g = app.gallery().await?;
            let setup = g
                .store
                .setup(&p.coordinate)?
                .ok_or_else(|| anyhow::anyhow!("no such setup"))?;
            let mut done = Vec::new();
            let mut failed = Vec::new();
            for step in g.setup_steps(&setup) {
                let r = match &step {
                    crate::gallery::Step::InstallTheme(url) if valid_source(url) => {
                        run_omarchy("omarchy-theme-install", &[url]).await
                    }
                    crate::gallery::Step::InstallPlugin(url) if valid_source(url) => {
                        run_omarchy("omarchy-plugin-add", &[url, "--yes"]).await
                    }
                    crate::gallery::Step::SwitchTheme(name) if valid_name(name) => {
                        let r = run_omarchy("omarchy-theme-set", &[name]).await;
                        if r.is_ok()
                            && let Ok(engine) = app.engine().await
                        {
                            let _ = engine.theme_applied(name);
                        }
                        r
                    }
                    _ => Err(anyhow::anyhow!("that isn't something Peridot can install")),
                };
                match r {
                    Ok(()) => done.push(step),
                    Err(e) => failed.push(json!({"step": step, "error": e.to_string()})),
                }
            }
            app.nudge.notify_one();
            app.emit_state().await;
            Ok(json!({"done": done, "failed": failed}))
        }
        "gallery.install" => {
            #[derive(Deserialize)]
            struct P {
                url: String,
                kind: ItemKind,
            }
            let p: P = parse(params)?;
            let url = peridot_sync::gallery::canonical_url(&p.url)
                .ok_or_else(|| anyhow::anyhow!("that isn't a repository address"))?;
            anyhow::ensure!(
                valid_source(&url),
                "that isn't something Peridot can install"
            );
            match p.kind {
                ItemKind::Theme => run_omarchy("omarchy-theme-install", &[&url]).await?,
                ItemKind::Plugin => run_omarchy("omarchy-plugin-add", &[&url, "--yes"]).await?,
            }
            app.nudge.notify_one();
            app.emit_state().await;
            Ok(json!({"ok": true}))
        }
        "gallery.list_item" => {
            // Put a repository on the map for everyone.
            #[derive(Deserialize)]
            struct P {
                url: String,
                kind: ItemKind,
                name: String,
            }
            let p: P = parse(params)?;
            Ok(json!(
                app.gallery()
                    .await?
                    .list_item(&p.url, p.kind, &p.name)
                    .await
                    .map_err(gallery_err)?
            ))
        }
        "gallery.refresh" => {
            let g = app.gallery().await?;
            g.connect().await;
            let registries = g
                .refresh_registries(true)
                .await
                .err()
                .map(|e| e.to_string());
            let _ = g.refresh_graph(true).await;
            let n = g.catch_up().await?;
            app.emit_state().await;
            Ok(json!({"new": n, "registry_error": registries}))
        }
        "profile.get" => {
            let g = app.gallery().await?;
            g.fetch_profiles(&[g.me()], false).await;
            Ok(json!({"profile": g.my_profile().await.flatten(), "pubkey": g.me().to_hex()}))
        }
        "profile.set" => {
            #[derive(Deserialize)]
            struct P {
                name: String,
            }
            let p: P = parse(params)?;
            let profile = app
                .gallery()
                .await?
                .set_profile(&p.name)
                .await
                .map_err(gallery_err)?;
            app.emit_state().await;
            Ok(json!(profile))
        }

        // ── Servers ────────────────────────────────────────────────────
        "servers.audit" => {
            // Check every server now, and tidy up: with Opal holding the
            // key this is where deletions get their prompt.
            let engine = app.engine().await?;
            let mut report = engine.audit(Timestamp::now().as_secs()).await?;
            if !report.stale.is_empty() {
                let signer = app.sharer().await?.signer.clone();
                let stale = report.stale.clone();
                match engine.remove_chunks(signer.as_ref(), &stale).await {
                    Ok(n) => report.removed_chunks += n,
                    Err(e) => {
                        app.emit_state().await;
                        return Err(gallery_err(anyhow::anyhow!("{e}"))
                            .context("the check is done, but the old chunks couldn't be removed"));
                    }
                }
            }
            app.emit_state().await;
            Ok(json!(report))
        }
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
            // On either computer. `hold_key` (sharing side) also hands the
            // identity's own key over. On until the sync key no longer
            // needs it (protocol v2): a computer paired from a local key
            // can't sign otherwise.
            #[derive(Deserialize)]
            struct P {
                matches: bool,
                #[serde(default = "yes")]
                hold_key: bool,
            }
            let p: P = parse(params)?;
            pair::confirm(app, p.matches, p.hold_key).await?;
            Ok(json!({"ok": true}))
        }
        "pair.cancel" => {
            pair::cancel(app).await;
            Ok(json!({"ok": true}))
        }

        // ── Your identity ──────────────────────────────────────────────
        "identity.card" => {
            // Your public address, for other people and other apps.
            let engine = app.engine().await?;
            let pubkey = engine.identity().pubkey();
            let relays = opal_kit::relays::parse_urls(&app.config.read().await.gallery.relays);
            let nprofile = Nip19Profile::new(pubkey, relays).to_bech32()?;
            let qr = opal_kit::qr::svg_data_url(&format!("nostr:{nprofile}"))?;
            let (has_profile, name) = match app.gallery().await {
                Ok(g) => {
                    g.fetch_profiles(&[pubkey], false).await;
                    let p = g.my_profile().await.flatten();
                    (p.is_some(), p.map(|p| p.name))
                }
                Err(_) => (false, None),
            };
            Ok(json!({
                "pubkey": pubkey.to_hex(),
                "npub": pubkey.to_bech32()?,
                "nprofile": nprofile,
                "qr": qr,
                "has_profile": has_profile,
                "name": name,
                "mode": if engine.identity().via_opal_mode() { "opal" } else { "local" },
            }))
        }
        "identity.move.start" => {
            // A one-time code for Opal's "Add account": the key encrypted
            // with six fresh words (exactly a recovery kit). Nothing is
            // kept here; cancelling is closing the card.
            let engine = app.engine().await?;
            let Some(keys) = engine.identity().keys.clone() else {
                bail!("Opal already holds your key");
            };
            let words = recovery::generate_words();
            let w = words.clone();
            let code = tokio::task::spawn_blocking(move || recovery::seal_key(&keys, &w)).await??;
            Ok(json!({
                "code": code,
                "words": words.as_str(),
                "npub": engine.identity().pubkey().to_bech32()?,
            }))
        }
        "identity.move.finish" => {
            // Opal has the account now: pair for it, forget the key here,
            // keep the identity and the sync secret. Safe to call again.
            let engine = app.engine().await?;
            let identity = engine.identity().clone();
            if identity.via_opal_mode() {
                bail!("Opal already holds your key");
            }
            let pubkey = identity.pubkey();
            let accounts = app.opal.accounts().await;
            let Some(account) = accounts.iter().find(|a| a.pubkey == pubkey.to_hex()) else {
                if accounts.is_empty() {
                    bail!(
                        "Opal doesn't have this key yet (is Opal running?): add it under Profiles first"
                    );
                }
                bail!("Opal doesn't have this key yet: add it under Profiles first");
            };
            let paired = app.opal.has_token()
                && app
                    .opal
                    .status()
                    .await
                    .is_ok_and(|s| s.pubkey == account.pubkey);
            if !paired {
                app.pair_opal(Some(pubkey))
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", pair_error(&e)))?;
            }
            app.switch_to_opal(Some(account.label.clone())).await?;
            Ok(json!({"ok": true, "label": account.label}))
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

fn yes() -> bool {
    true
}

/// Read a file to share: never through a symlink, never past the cap
/// (the size is checked while reading, not before).
async fn read_for_share(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY)
            .open(&path)?;
        if !f.metadata()?.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let cap = peridot_sync::share::MAX_SHARE_BYTES as u64;
        let mut data = Vec::new();
        f.take(cap + 1).read_to_end(&mut data)?;
        if data.len() as u64 > cap {
            return Err(std::io::Error::other("too big to share (64 MB at most)"));
        }
        Ok(data)
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))?
}

/// The newest screenshot in the usual folder, if any.
fn last_screenshot(home: &std::path::Path) -> Option<std::path::PathBuf> {
    let dirs = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
    let mut pictures = home.join("Pictures");
    for line in dirs.lines() {
        if let Some(v) = line.strip_prefix("XDG_PICTURES_DIR=") {
            let v = v
                .trim_matches('"')
                .replace("$HOME", &home.to_string_lossy());
            pictures = std::path::PathBuf::from(v);
        }
    }
    let mut newest: Option<(std::time::SystemTime, std::path::PathBuf)> = None;
    for dir in [pictures.join("Screenshots"), pictures] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let is_img = p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
                ["png", "jpg", "jpeg", "webp"].contains(&x.to_ascii_lowercase().as_str())
            });
            if !is_img {
                continue;
            }
            if let Ok(m) = e.metadata()
                && let Ok(t) = m.modified()
                && newest.as_ref().is_none_or(|(nt, _)| t > *nt)
            {
                newest = Some((t, p));
            }
        }
        if newest.is_some() {
            break;
        }
    }
    newest.map(|(_, p)| p)
}

/// Gallery actions sign as you; the same Opal wording applies.
fn gallery_err(e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("Unlock Opal") {
        anyhow::anyhow!("Unlock Opal first: it signs for you")
    } else if msg.contains("Opal isn't running") {
        anyhow::anyhow!("Start Opal first: it signs for you")
    } else if msg.contains("didn't allow") {
        anyhow::anyhow!("Opal didn't allow it: you said no, or a rule under Apps in Opal blocks it")
    } else {
        e
    }
}

/// The signer's "come back later" messages are worded for syncing.
fn share_err(e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("Unlock Opal") {
        anyhow::anyhow!("Unlock Opal first: it signs the upload")
    } else if msg.contains("Opal isn't running") {
        anyhow::anyhow!("Start Opal first: it signs the upload")
    } else if msg.contains("Pair Peridot with Opal") {
        anyhow::anyhow!(
            "Pair Peridot with Opal again first (Peridot's panel, or `peridot opal pair`)"
        )
    } else if msg.contains("didn't allow") {
        anyhow::anyhow!(
            "Opal didn't allow the upload: you said no, or a rule under Apps in Opal blocks it"
        )
    } else {
        e
    }
}

/// Why pairing didn't happen, for someone who just asked for it.
fn pair_error(e: &anyhow::Error) -> String {
    let msg = e.to_string();
    if msg.contains("declined") {
        "You declined in Opal; nothing was set up".into()
    } else if msg.contains("timed out") || msg.contains("didn't answer") {
        "Opal's prompt wasn't answered; try again".into()
    } else if msg.contains("already waiting") {
        "Peridot is already waiting for your answer in Opal".into()
    } else if msg.contains("isn't running") {
        "Opal isn't running".into()
    } else {
        format!("Opal didn't pair with Peridot: {msg}")
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
        None => Arc::new(crate::opal::OpalSigner::new(
            app.opal.clone(),
            pubkey,
            crate::opal::Mode::Interactive,
        )),
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
pub fn ymd(secs: u64) -> String {
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
