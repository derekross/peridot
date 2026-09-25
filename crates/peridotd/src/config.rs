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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
            sync: Choices::default(),
            device_name: None,
            auto_apply: false,
            paused: false,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
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
}
