//! peridotd — keeps your Omarchy computers matching.

mod api;
mod app;
mod config;
mod ipc;
mod notify;
mod opal;
mod pair;
mod runner;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use opal_core::db::Db;
use opal_core::keystore::SecretStore;
use opal_core::paths::AppDirs;

const DIRS: AppDirs = AppDirs::PERIDOT;

#[derive(Parser)]
#[command(
    version,
    about = "Peridot daemon: keeps your Omarchy computers matching"
)]
struct Args {
    /// Control socket path.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Config file path.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Database path.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Home folder to sync (development only).
    #[arg(long, hide = true)]
    home: Option<PathBuf>,
    /// Keep the identity in memory instead of the keyring (development only).
    #[arg(long, hide = true)]
    memory_keyring: bool,
    /// Opal's control socket (development only).
    #[arg(long, hide = true)]
    opal_socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Your key lives in this process: no core dumps, no reading its memory
    // from other processes of your user. The unit also sets LimitCORE=0.
    if let Err(e) =
        rustix::process::set_dumpable_behavior(rustix::process::DumpableBehavior::NotDumpable)
    {
        eprintln!("warning: could not disable core dumps: {e}");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PERIDOT_LOG")
                .unwrap_or_else(|_| "peridotd=info,peridot_sync=info".into()),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    opal_core::identity::ensure_crypto_provider();
    let config_path = args.config.clone().unwrap_or_else(|| DIRS.config_file());
    let config = config::Config::load(&config_path)?;
    let db = match &args.db {
        Some(p) => Db::open(p)?,
        None => Db::open_default_for(DIRS)?,
    };
    let secrets = if args.memory_keyring {
        tracing::warn!("using an in-memory keyring: the identity is lost on exit");
        SecretStore::memory()
    } else {
        SecretStore::keyring_for(DIRS)
            .await
            .context("connecting to the Secret Service (is gnome-keyring running?)")?
    };
    let home = match args.home {
        Some(h) => h,
        None => dirs_home()?,
    };
    let data_dir = match &args.db {
        Some(p) => p
            .parent()
            .map(|d| d.to_path_buf())
            .unwrap_or_else(|| DIRS.data_dir()),
        None => DIRS.data_dir(),
    };

    let app = app::App::new(app::Options {
        config,
        config_path,
        db,
        secrets,
        home,
        data_dir,
        opal_socket: args
            .opal_socket
            .unwrap_or_else(|| AppDirs::OPAL.socket_path()),
    });
    // Say so plainly if the sandbox breaks file access, instead of every
    // file quietly showing as unreadable.
    let check = peridot_sync::apply::Home::open(&app.home)
        .map_err(|e| format!("can't open your home folder: {e}"))
        .and_then(|h| h.self_check());
    let sandbox_ok = match check {
        Ok(()) => true,
        Err(msg) => {
            tracing::error!("{msg}");
            app.set_error(Some(msg)).await;
            false
        }
    };
    if sandbox_ok && let Err(e) = app.resume().await {
        tracing::warn!("couldn't start syncing: {e:#}");
        app.set_error(Some(e.to_string())).await;
    }

    let socket = args.socket.unwrap_or_else(|| DIRS.socket_path());
    tokio::select! {
        r = opal_kit::ipc::serve(app.clone(), &socket, "peridotd") => r?,
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }
    if let Some(engine) = app.engine.read().await.clone() {
        // Get anything waiting out before we go.
        engine.flush().await;
        engine.client().shutdown().await;
    }
    Ok(())
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .context("HOME is not set")
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
