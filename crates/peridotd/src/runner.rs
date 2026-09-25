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
    if fresh {
        engine.mark_caught_up();
        engine.publish_root().await?;
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
    engine.announce().await?;
    if !app.config.read().await.paused {
        publish(&app, &engine).await;
    }
    app.mark_synced();
    app.emit_state().await;
    let mut waiting = incoming(&engine).await;

    let mut notifications = engine.client().notifications();
    let since = Timestamp::now().as_secs().saturating_sub(60);
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
                let paused = app.config.read().await.paused;
                if engine.catch_up().await.is_ok_and(|n| n > 0) {
                    after_incoming(&app, &engine, &mut waiting).await;
                }
                if !paused {
                    publish(&app, &engine).await;
                }
                app.mark_synced();
                app.emit_state().await;
            }
            _ = flush.tick() => {
                if engine.flush().await > 0 {
                    app.emit_state().await;
                }
            }
            _ = catch_up.tick() => {
                if engine.catch_up().await.is_ok_and(|n| n > 0) {
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
            }
        }
    }
    drop(watcher);
    Ok(())
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
            app.set_error(None).await;
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
