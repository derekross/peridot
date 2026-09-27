//! Peridot's settings (`~/.config/peridot/config.toml`).

use std::path::Path;

use peridot_sync::manifest::Choices;
use serde::{Deserialize, Serialize};

/// Where your encrypted settings are kept. Several independent servers,
/// so none of them going away loses anything; add your own any time.
pub const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.ditto.pub",
    "wss://auth.nostr1.com",
    "wss://relay.primal.net",
    "wss://relay.damus.io",
];

/// Where private links are kept. A link's blob is encrypted, so the server
/// has to accept arbitrary bytes: media hosts that check for a picture or a
/// video (blossom.band, blossom.primal.net) answer 415. These four take
/// anything, serve it cross-origin to the viewer, and honour removal.
pub const DEFAULT_SHARE_SERVERS: &[&str] = &[
    "https://nostr.download",
    "https://blossom.yakihonne.com",
    "https://files.sovbit.host",
    "https://cdn.hzrd149.com",
];

/// Servers Peridot used to list by default that refuse encrypted blobs.
/// Dropped from a saved list, so an older config doesn't keep failing.
const MEDIA_ONLY_SERVERS: &[&str] = &["blossom.band", "blossom.primal.net"];

/// Private share links.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShareConfig {
    /// Blossom servers to upload to, tried in order.
    pub servers: Vec<String>,
    /// How long a link works (days), unless you remove it sooner.
    pub expire_days: u32,
    /// The viewer page links point at.
    pub viewer: String,
}

impl Default for ShareConfig {
    fn default() -> Self {
        Self {
            servers: DEFAULT_SHARE_SERVERS.iter().map(|s| s.to_string()).collect(),
            expire_days: 7,
            viewer: "https://myperidot.app/s".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Servers (Nostr relays) your encrypted settings are stored on.
    pub relays: Vec<String>,
    /// What syncs, on top of the defaults.
    pub sync: Choices,
    /// This computer's name for your other computers (default: hostname).
    pub device_name: Option<String>,
    /// Apply incoming settings without asking (never for files that can
    /// run commands).
    pub auto_apply: bool,
    /// Pause syncing.
    pub paused: bool,
    pub share: ShareConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            sync: Choices::default(),
            device_name: None,
            auto_apply: false,
            paused: false,
            share: ShareConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut cfg: Self = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        cfg.share.servers.retain(|s| {
            let host = s
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .trim_end_matches('/');
            !MEDIA_ONLY_SERVERS.contains(&host)
        });
        if cfg.share.servers.is_empty() {
            cfg.share.servers = ShareConfig::default().servers;
        }
        Ok(cfg)
    }

    /// Written privately and atomically.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        if let Some(dir) = path.parent() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn device_name(&self) -> String {
        self.device_name
            .clone()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(hostname)
    }
}

fn hostname() -> String {
    let uname = rustix::system::uname();
    let name = uname.nodename().to_string_lossy().into_owned();
    if name.is_empty() {
        "This computer".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peridot/config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());
        let mut c = Config::default();
        c.sync.enabled.push(".bashrc".into());
        c.auto_apply = true;
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!Config::default().device_name().is_empty());
    }

    #[test]
    fn drops_media_only_share_servers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peridot/config.toml");
        let mut c = Config::default();
        c.share.servers = vec![
            "https://blossom.band".into(),
            "https://blossom.primal.net/".into(),
            "https://example.org".into(),
        ];
        c.save(&path).unwrap();
        assert_eq!(
            Config::load(&path).unwrap().share.servers,
            vec!["https://example.org".to_string()]
        );
        c.share.servers = vec!["https://blossom.band".into()];
        c.save(&path).unwrap();
        assert_eq!(
            Config::load(&path).unwrap().share.servers,
            ShareConfig::default().servers
        );
    }
}
