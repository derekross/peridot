//! The background loop while set up: publish what changes here (files are
//! watched, changes debounced), take in what changes elsewhere (a live
//! subscription plus periodic catch-ups), and tell you when something is
//! waiting.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use nostr_sdk::prelude::*;
use notify::{RecursiveMode, Watcher};
use peridot_sync::manifest::Target;
use peridot_sync::store::FileStatus;
use peridot_sync::sync::SyncEngine;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::app::App;

/// Quiet time after a file change before publishing (editors write in
/// bursts).
const DEBOUNCE: Duration = Duration::from_secs(3);
const FLUSH_EVERY: Duration = Duration::from_secs(60);
const CATCH_UP_EVERY: Duration = Duration::from_secs(15 * 60);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(24 * 3600);
/// The servers are checked this often (and at startup when it's been
/// longer).
const AUDIT_EVERY: u64 = 24 * 3600;
const SUB_ID: &str = "peridot";

pub fn spawn(app: Arc<App>, engine: Arc<SyncEngine>, fresh: bool) -> JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run(app.clone(), engine, fresh).await {
            tracing::warn!("sync stopped: {e:#}");
            app.set_error(Some(format!("Sync stopped: {e}"))).await;
            app.emit_state().await;
        }
    })
}

async fn run(app: Arc<App>, engine: Arc<SyncEngine>, fresh: bool) -> anyhow::Result<()> {
    engine.connect().await;
    // Setup publishes that need the signer (the root event for a new
    // identity, this device's entry). With Opal locked or not running yet
    // they wait and are retried, rather than stopping sync.
    let mut root_pending = fresh || app.db.get_kv("peridot.root_pending")?.as_deref() == Some("1");
    let mut announced = false;
    if fresh {
        engine.mark_caught_up();
    }
    // Always catch up before saying anything, so we don't publish over
    // newer changes from elsewhere. Until it works, nothing is published.
    loop {
        match engine.catch_up().await {
            Ok(_) => {
                app.set_error(None).await;
                break;
            }
            Err(e) => {
                app.set_error(Some(format!("Waiting for your sync servers: {e}")))
                    .await;
                app.emit_state().await;
                tokio::time::sleep(Duration::from_secs(15)).await;
                engine.connect().await;
            }
        }
    }
    // Still on the first protocol version: move to epoch 1 now (another
    // computer may have done it already, in which case this one has to
    // pair again: it never got the new secret).
    if engine.epoch() == 0 {
        match engine.fetch_root().await {
            Ok(Some(root))
                if peridot_sync::identity::Identity::root_commitment(&root).is_some() =>
            {
                // Back to the welcome screen: pairing is how this computer
                // gets the new secret, and it's only offered there.
                app.handover("stopping: pair again", |app| async move {
                    app.stop_engine().await;
                    app.set_error(Some(
                        "Your other computers moved to a newer Peridot. Choose \"Add this computer\" and enter the code on one of them to keep syncing; your settings here stay as they are."
                            .into(),
                    ))
                    .await;
                    app.emit_state().await;
                    Ok(())
                });
                return Ok(());
            }
            Ok(_) => {
                tracing::info!("moving to epoch 1");
                // Not from here: starting the new engine stops this one,
                // and this task is the one being stopped.
                app.handover("moving to epoch 1", |app| async move {
                    app.rotate(&[]).await.map(|_| ())
                });
                return Ok(());
            }
            Err(e) => {
                app.set_error(Some(format!("Waiting for your sync servers: {e}")))
                    .await;
                app.emit_state().await;
                return Err(e);
            }
        }
    }
    setup_publishes(&app, &engine, &mut root_pending, &mut announced).await;
    if app.db.get_kv("peridot.republish_pending")?.as_deref() == Some("1") {
        match engine.republish_everything().await {
            Ok(n) => {
                tracing::info!("said {n} item(s) again under the new keys");
                let _ = app.db.set_kv("peridot.republish_pending", "0");
            }
            Err(e) => tracing::warn!("couldn't republish under the new keys yet: {e}"),
        }
    }
    if !app.config.read().await.paused {
        publish(&app, &engine).await;
    }
    app.mark_synced();
    app.emit_state().await;
    if rotated(&app, &engine).await {
        return Ok(());
    }
    let mut waiting = incoming(&engine).await;
    if Timestamp::now()
        .as_secs()
        .saturating_sub(engine.audited_at())
        > AUDIT_EVERY
    {
        audit(&app, &engine).await;
    }

    let mut notifications = engine.client().notifications();
    // Items are dated to the hour: look back past the current bucket.
    let since = Timestamp::now()
        .as_secs()
        .saturating_sub(peridot_sync::sync::CATCH_UP_MARGIN);
    let relays = engine.relays().await;
    let targets: Vec<(RelayUrl, Vec<Filter>)> = relays
        .iter()
        .map(|r| (r.clone(), vec![engine.filter(since)]))
        .collect();
    engine
        .client()
        .subscribe(targets)
        .with_id(SubscriptionId::new(SUB_ID))
        .await?;

    let (fs_tx, mut fs_rx) = mpsc::unbounded_channel::<()>();
    let mut watcher = watch(&engine, fs_tx.clone()).await;

    let mut flush = tokio::time::interval(FLUSH_EVERY);
    let mut catch_up = tokio::time::interval(CATCH_UP_EVERY);
    let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
    for t in [&mut flush, &mut catch_up, &mut heartbeat] {
        t.tick().await;
    }
    let debounce = tokio::time::sleep(Duration::from_secs(u32::MAX as u64));
    tokio::pin!(debounce);
    let mut dirty = false;

    loop {
        tokio::select! {
            n = notifications.next() => {
                let Some(n) = n else { break };
                if let ClientNotification::Event { event, .. } = n
                    && engine.ingest(&event)
                {
                    if rotated(&app, &engine).await {
                        return Ok(());
                    }
                    after_incoming(&app, &engine, &mut waiting).await;
                }
            }
            Some(()) = fs_rx.recv() => {
                dirty = true;
                debounce.as_mut().reset(tokio::time::Instant::now() + DEBOUNCE);
            }
            _ = &mut debounce, if dirty => {
                dirty = false;
                debounce.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(u32::MAX as u64));
                if !app.config.read().await.paused {
                    publish(&app, &engine).await;
                    let _ = engine.announce().await;
                }
                app.emit_state().await;
            }
            _ = app.nudge.notified() => {
                // An apply may have created folders we couldn't watch before.
                watcher = watch(&engine, fs_tx.clone()).await;
                setup_publishes(&app, &engine, &mut root_pending, &mut announced).await;
                let paused = app.config.read().await.paused;
                if engine.catch_up().await.is_ok_and(|n| n > 0) {
                    if rotated(&app, &engine).await {
                        return Ok(());
                    }
                    after_incoming(&app, &engine, &mut waiting).await;
                }
                if !paused {
                    publish(&app, &engine).await;
                }
                app.mark_synced();
                app.emit_state().await;
            }
            _ = flush.tick() => {
                setup_publishes(&app, &engine, &mut root_pending, &mut announced).await;
                let mut changed = engine.flush().await > 0;
                // Anything held back (e.g. Opal was locked) gets another go.
                if !app.config.read().await.paused
                    && engine.overview().await.count(FileStatus::Outgoing) > 0
                {
                    publish(&app, &engine).await;
                    changed = true;
                }
                if changed {
                    app.emit_state().await;
                }
            }
            _ = catch_up.tick() => {
                if engine.catch_up().await.is_ok_and(|n| n > 0) {
                    if rotated(&app, &engine).await {
                        return Ok(());
                    }
                    after_incoming(&app, &engine, &mut waiting).await;
                }
                // Top-level files (.XCompose, .bashrc) aren't watched.
                if !app.config.read().await.paused {
                    publish(&app, &engine).await;
                }
                // Folders that didn't exist before may now.
                watcher = watch(&engine, fs_tx.clone()).await;
                app.mark_synced();
                app.emit_state().await;
            }
            _ = heartbeat.tick() => {
                let _ = engine.announce().await;
                audit(&app, &engine).await;
                match app.cleanup_previous(false).await {
                    Ok(0) => {}
                    Ok(n) => {
                        tracing::info!("the previous epoch's {n} item(s) were removed");
                        // The engine was restarted without the old keys.
                        return Ok(());
                    }
                    Err(e) => tracing::info!("the previous epoch stays for now: {e}"),
                }
            }
        }
    }
    drop(watcher);
    Ok(())
}

/// A rotation another computer announced: switch this engine over, or
/// stop if this computer was left out. True when this engine is done.
async fn rotated(app: &Arc<App>, engine: &Arc<SyncEngine>) -> bool {
    use peridot_sync::rotation::Rotation;
    match engine.take_pending_rotation() {
        None => false,
        Some(Rotation::Adopt { secret, .. }) => {
            app.handover("moving to the new epoch", |app| async move {
                app.adopt_epoch(secret).await
            });
            true
        }
        Some(Rotation::Removed { epoch }) => {
            tracing::warn!("this computer was left out of epoch {epoch}");
            app.handover("stopping after removal", |app| async move {
                app.removed_elsewhere().await;
                Ok(())
            });
            true
        }
    }
}

/// Check the servers: fill gaps, refresh old items, and (when the key is
/// here, so nobody is asked) drop chunks nothing refers to. With Opal
/// holding the key, deletions wait for you: `peridot tidy`, or the panel.
async fn audit(app: &Arc<App>, engine: &Arc<SyncEngine>) {
    match engine.audit(Timestamp::now().as_secs()).await {
        Ok(report) => {
            if report.resent + report.refreshed > 0 {
                tracing::info!(
                    "servers checked: {} sent again, {} refreshed",
                    report.resent,
                    report.refreshed
                );
            }
            if !report.stale.is_empty() && engine.identity().keys.is_some() {
                let signer = engine.signer();
                match engine.remove_chunks(signer.as_ref(), &report.stale).await {
                    Ok(n) => tracing::info!("asked the servers to drop {n} old chunk(s)"),
                    Err(e) => tracing::info!("old chunks stay for now: {e}"),
                }
            }
        }
        Err(e) => tracing::info!("couldn't check the servers: {e}"),
    }
    app.emit_state().await;
}

/// The root event and this device's entry, until both have gone out.
async fn setup_publishes(
    app: &Arc<App>,
    engine: &Arc<SyncEngine>,
    root_pending: &mut bool,
    announced: &mut bool,
) {
    if *root_pending {
        match engine.publish_root().await {
            Ok(()) => {
                *root_pending = false;
                let _ = app.db.set_kv("peridot.root_pending", "0");
            }
            Err(e) => {
                app.set_error(Some(format!("{e} (finishing setup)"))).await;
                return;
            }
        }
    }
    if !*announced {
        match engine.announce().await {
            Ok(()) => {
                *announced = true;
                app.set_error(None).await;
            }
            Err(e) => app.set_error(Some(e.to_string())).await,
        }
    }
}

async fn publish(app: &Arc<App>, engine: &Arc<SyncEngine>) {
    match engine.publish_changes().await {
        Ok(report) => {
            if !report.published.is_empty() || !report.deleted.is_empty() {
                tracing::info!(
                    "published {} change(s), {} deletion(s)",
                    report.published.len(),
                    report.deleted.len()
                );
            }
            // Settings sign themselves; a root event still waiting on the
            // identity's signer keeps its message.
            let keep = app
                .last_error
                .read()
                .await
                .as_deref()
                .is_some_and(|e| e.ends_with("(finishing setup)"));
            if !keep {
                app.set_error(None).await;
            }
        }
        Err(e) => app.set_error(Some(e.to_string())).await,
    }
}

async fn incoming(engine: &Arc<SyncEngine>) -> usize {
    let o = engine.overview().await;
    o.count(FileStatus::Incoming) + o.count(FileStatus::Conflict) + o.offers.len()
}

/// Something new arrived: apply it if you asked for that (never files that
/// can run commands), and tell you once per new batch.
async fn after_incoming(app: &Arc<App>, engine: &Arc<SyncEngine>, waiting: &mut usize) {
    if app.config.read().await.auto_apply {
        let safe: Vec<String> = engine
            .overview()
            .await
            .files
            .into_iter()
            .filter(|f| f.status == FileStatus::Incoming && !f.runs_commands)
            .map(|f| f.path)
            .collect();
        if !safe.is_empty() {
            let _ = engine.apply(&safe).await;
        }
    }
    let now = incoming(engine).await;
    if now > *waiting {
        let from = engine
            .overview()
            .await
            .files
            .into_iter()
            .find_map(|f| f.from)
            .unwrap_or_else(|| "another computer".into());
        crate::notify::changes_waiting(now, &from);
    }
    *waiting = now;
    app.emit_state().await;
}

/// Watch every folder that can hold something to sync. Missing folders are
/// skipped (and picked up by the next re-watch).
async fn watch(
    engine: &Arc<SyncEngine>,
    tx: mpsc::UnboundedSender<()>,
) -> Option<notify::RecommendedWatcher> {
    let home = engine.home_path().to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && !matches!(ev.kind, notify::EventKind::Access(_))
            // Our own temp files during an apply.
            && !ev.paths.iter().all(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".peridot-"))
            })
        {
            let _ = tx.send(());
        }
    })
    .ok()?;
    for target in engine.watch_targets().await {
        let (path, mode) = match target {
            Target::File(p) => (
                home.join(&p)
                    .parent()
                    .map(|d| d.to_path_buf())
                    .unwrap_or(home.clone()),
                RecursiveMode::NonRecursive,
            ),
            Target::Dir(d) => (home.join(d), RecursiveMode::NonRecursive),
            Target::Tree(d) => (home.join(d), RecursiveMode::Recursive),
        };
        // Watching all of home, even non-recursively, would wake us for
        // every download; single files at the top are caught by the
        // periodic catch-up scan instead.
        if path == home || path.symlink_metadata().map(|m| !m.is_dir()).unwrap_or(true) {
            continue;
        }
        let _ = watcher.watch(&path, mode);
    }
    Some(watcher)
}
