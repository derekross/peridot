//! What syncs. Paths are relative to your home folder and fall into tiers
//! (the vocabulary of Omarchy's own dots plan):
//!
//! - **shared**: syncs by default.
//! - **ask**: can run commands on your other computers (autostart, shell,
//!   hooks…), so it's off until you turn it on.
//! - **local**: belongs to this machine (monitor layout, input devices).
//! - **never**: secrets and other apps' private data. Can't be turned on.
//!
//! The most restrictive matching tier wins. On top of that, files are
//! checked for things that look like secrets and skipped if found.

use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

/// Files bigger than this aren't synced (configs are small; wallpapers and
/// binaries are out of scope).
pub const MAX_FILE_SIZE: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Shared,
    Ask,
    Local,
    Never,
}

const SHARED: &[&str] = &[
    ".config/hypr/*.lua",
    ".config/hypr/*.conf",
    ".config/omarchy/shell.json",
    ".config/omarchy/extensions/**",
    ".config/omarchy/branding/**",
    ".config/omarchy/themed/**",
    ".config/alacritty/**",
    ".config/foot/**",
    ".config/ghostty/**",
    ".config/kitty/**",
    ".config/btop/btop.conf",
    ".config/starship.toml",
    ".config/tmux/**",
    ".config/lazygit/config.yml",
    ".config/mpv/*.conf",
    ".XCompose",
];

const ASK: &[&str] = &[
    ".config/hypr/autostart.lua",
    ".bashrc",
    ".config/omarchy/hooks/**",
    ".config/nvim/**",
    ".config/git/config",
    ".config/mimeapps.list",
];

const LOCAL: &[&str] = &[
    ".config/hypr/monitors.lua",
    ".config/hypr/input.lua",
    "**/*.bak",
    "**/*.bak.*",
    "**/*.orig",
    "**/*~",
    "**/.luarc.json",
];

const NEVER: &[&str] = &[
    ".ssh/**",
    ".gnupg/**",
    ".password-store/**",
    ".local/share/keyrings/**",
    ".config/gh/**",
    ".config/rclone/**",
    ".config/vdirsyncer/**",
    ".config/khal/**",
    ".config/khard/**",
    ".config/opencode/**",
    ".config/opal/**",
    ".config/peridot/**",
    ".config/Signal/**",
    ".config/BraveSoftware/**",
    ".config/chromium/**",
    ".config/google-chrome*/**",
    ".config/microsoft-edge*/**",
    ".config/omarchy/plugins/**",
    ".config/omarchy/themes/**",
    "**/*.env",
    "**/.env*",
    "**/*token*",
    "**/*secret*",
    "**/*password*",
    "**/*credential*",
    "**/*.pem",
    "**/*.key",
];

fn set(patterns: &[impl AsRef<str>]) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = Glob::new(p.as_ref()) {
            b.add(g);
        }
    }
    b.build().expect("valid globs")
}

/// Your choices on top of the defaults (from Peridot's config file).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Choices {
    /// Ask-tier patterns you turned on.
    pub enabled: Vec<String>,
    /// Shared patterns you turned off, and extra paths to leave alone.
    pub excluded: Vec<String>,
}

pub struct Manifest {
    shared: GlobSet,
    ask: GlobSet,
    local: GlobSet,
    never: GlobSet,
    enabled: GlobSet,
    excluded: GlobSet,
    choices: Choices,
}

impl Manifest {
    pub fn new(choices: Choices) -> Self {
        Self {
            shared: set(SHARED),
            ask: set(ASK),
            local: set(LOCAL),
            never: set(NEVER),
            enabled: set(&choices.enabled),
            excluded: set(&choices.excluded),
            choices,
        }
    }

    pub fn choices(&self) -> &Choices {
        &self.choices
    }

    /// The tier of `path` (relative to home), or None if the manifest
    /// doesn't cover it at all.
    pub fn tier(&self, path: &str) -> Option<Tier> {
        if !is_clean_relative(path) || self.never.is_match(path) {
            return Some(Tier::Never);
        }
        if self.local.is_match(path) {
            return Some(Tier::Local);
        }
        if self.ask.is_match(path) {
            return Some(Tier::Ask);
        }
        if self.shared.is_match(path) {
            return Some(Tier::Shared);
        }
        None
    }

    /// Whether `path` syncs with the current choices.
    pub fn syncs(&self, path: &str) -> bool {
        if self.excluded.is_match(path) {
            return false;
        }
        match self.tier(path) {
            Some(Tier::Shared) => true,
            Some(Tier::Ask) => self.enabled.is_match(path),
            _ => false,
        }
    }

    /// Where syncable files can be, for scanning and watching: single
    /// files, one folder level, or a whole folder tree. Never all of home.
    pub fn targets(&self) -> Vec<Target> {
        let mut out: Vec<Target> = Vec::new();
        for p in SHARED.iter().chain(ASK.iter()) {
            let t = target_of(p);
            if !out.contains(&t) {
                out.push(t);
            }
        }
        // A tree already covers anything under it.
        let trees: Vec<String> = out
            .iter()
            .filter_map(|t| match t {
                Target::Tree(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        let dirs: Vec<String> = out
            .iter()
            .filter_map(|t| match t {
                Target::Dir(d) => Some(d.clone()),
                _ => None,
            })
            .collect();
        out.retain(|t| {
            let path = match t {
                Target::File(p) | Target::Dir(p) | Target::Tree(p) => p,
            };
            let in_tree = trees
                .iter()
                .any(|d| d != path && path.starts_with(&format!("{d}/")));
            // A single file directly inside a folder we already list.
            let in_dir = matches!(t, Target::File(_))
                && dirs
                    .iter()
                    .any(|d| path.rsplit_once('/').is_some_and(|(parent, _)| parent == d));
            !in_tree && !in_dir
        });
        out
    }

    /// Default patterns for the settings screen: (pattern, tier).
    pub fn defaults() -> Vec<(&'static str, Tier)> {
        SHARED
            .iter()
            .map(|p| (*p, Tier::Shared))
            .chain(ASK.iter().map(|p| (*p, Tier::Ask)))
            .chain(LOCAL.iter().map(|p| (*p, Tier::Local)))
            .collect()
    }
}

/// A place to look for files (paths relative to home).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// One file.
    File(String),
    /// The files directly in a folder.
    Dir(String),
    /// A folder and everything under it.
    Tree(String),
}

fn target_of(pattern: &str) -> Target {
    let parts: Vec<&str> = pattern.split('/').collect();
    let fixed: Vec<&str> = parts
        .iter()
        .take_while(|p| !p.contains(['*', '?', '[', '{']))
        .copied()
        .collect();
    if fixed.len() == parts.len() {
        Target::File(pattern.to_string())
    } else if parts[fixed.len()..].iter().any(|p| *p == "**") || parts.len() - fixed.len() > 1 {
        Target::Tree(fixed.join("/"))
    } else {
        Target::Dir(fixed.join("/"))
    }
}

/// A relative path with no `..`, no absolute prefix, no empty or `.` parts.
pub fn is_clean_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\0')
        && path
            .split('/')
            .all(|p| !p.is_empty() && p != "." && p != "..")
}

/// Does this look like it holds a secret? Checked before anything is
/// published, whatever the tier.
pub fn looks_secret(content: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(content);
    const MARKERS: &[(&str, &str)] = &[
        ("-----BEGIN", "a private key"),
        ("nsec1", "a Nostr private key"),
        ("ncryptsec1", "an encrypted Nostr key"),
        ("ghp_", "a GitHub token"),
        ("github_pat_", "a GitHub token"),
        ("gho_", "a GitHub token"),
        ("glpat-", "a GitLab token"),
        ("xoxb-", "a Slack token"),
        ("xoxp-", "a Slack token"),
        ("AKIA", "an AWS key"),
        ("sk-ant-", "an API key"),
        ("sk-proj-", "an API key"),
        ("AIza", "a Google API key"),
    ];
    for (marker, what) in MARKERS {
        if text.contains(marker) {
            return Some(what);
        }
    }
    // Generic `api_key = "…"` / `password: …` assignments.
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        let assigns = lower.contains('=') || lower.contains(':');
        if assigns
            && [
                "api_key", "apikey", "api-key", "password", "passwd", "secret", "token",
            ]
            .iter()
            .any(|k| lower.contains(k))
            && line.split(['=', ':']).nth(1).is_some_and(|v| {
                let v = v.trim().trim_matches(['"', '\'', ',', ';']);
                v.len() >= 12 && !v.contains(' ')
            })
        {
            return Some("a password or token");
        }
    }
    None
}

/// Binary files (NUL bytes) aren't synced.
pub fn is_binary(content: &[u8]) -> bool {
    content.iter().take(8192).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_follow_the_most_restrictive_rule() {
        let m = Manifest::new(Choices::default());
        assert_eq!(m.tier(".config/hypr/bindings.lua"), Some(Tier::Shared));
        assert_eq!(m.tier(".config/hypr/autostart.lua"), Some(Tier::Ask));
        assert_eq!(m.tier(".config/hypr/monitors.lua"), Some(Tier::Local));
        assert_eq!(
            m.tier(".config/hypr/input.lua.bak.1790282060"),
            Some(Tier::Local)
        );
        assert_eq!(m.tier(".config/hypr/.luarc.json"), Some(Tier::Local));
        assert_eq!(m.tier(".ssh/id_ed25519"), Some(Tier::Never));
        assert_eq!(m.tier(".config/kitty/secrets.conf"), Some(Tier::Never));
        assert_eq!(
            m.tier(".config/omarchy/plugins/x/manifest.json"),
            Some(Tier::Never)
        );
        assert_eq!(m.tier("../etc/passwd"), Some(Tier::Never));
        assert_eq!(m.tier("/etc/passwd"), Some(Tier::Never));
        assert_eq!(m.tier(".config/hypr/./x.lua"), Some(Tier::Never));
        assert_eq!(m.tier("Documents/notes.txt"), None);
    }

    #[test]
    fn ask_needs_opt_in_and_never_cannot_be_enabled() {
        let m = Manifest::new(Choices::default());
        assert!(m.syncs(".config/hypr/bindings.lua"));
        assert!(!m.syncs(".bashrc"));
        assert!(!m.syncs(".config/hypr/monitors.lua"));
        let m = Manifest::new(Choices {
            enabled: vec![
                ".bashrc".into(),
                ".ssh/**".into(),
                ".config/hypr/monitors.lua".into(),
            ],
            excluded: vec![".config/kitty/**".into()],
        });
        assert!(m.syncs(".bashrc"));
        assert!(!m.syncs(".ssh/config"));
        assert!(!m.syncs(".config/hypr/monitors.lua"));
        assert!(!m.syncs(".config/kitty/kitty.conf"));
    }

    #[test]
    fn targets_are_precise_and_never_all_of_home() {
        let targets = Manifest::new(Choices::default()).targets();
        assert!(targets.contains(&Target::Dir(".config/hypr".into())));
        assert!(targets.contains(&Target::File(".XCompose".into())));
        assert!(targets.contains(&Target::File(".bashrc".into())));
        assert!(targets.contains(&Target::Tree(".config/omarchy/extensions".into())));
        assert!(targets.contains(&Target::Tree(".config/nvim".into())));
        assert!(!targets.contains(&Target::File(".config/hypr/autostart.lua".into())));
        assert!(
            targets
                .iter()
                .all(|t| !matches!(t, Target::Dir(d) | Target::Tree(d) if d.is_empty()))
        );
        assert_eq!(
            target_of(".config/btop/btop.conf"),
            Target::File(".config/btop/btop.conf".into())
        );
    }

    #[test]
    fn spots_secrets_but_not_ordinary_config() {
        assert!(looks_secret(b"-----BEGIN OPENSSH PRIVATE KEY-----").is_some());
        assert!(looks_secret(b"token = ghp_abcdefghijklmnop").is_some());
        assert!(looks_secret(b"api_key = \"a1b2c3d4e5f6g7h8\"").is_some());
        assert!(looks_secret(b"password: hunter2hunter2").is_some());
        assert!(looks_secret(b"bind = SUPER, P, exec, 1password").is_none());
        assert!(looks_secret(b"font_size = 12\ntheme = tokyo-night").is_none());
        assert!(looks_secret(b"-- show password prompts: on").is_none());
        assert!(is_binary(b"\x7fELF\0\0"));
        assert!(!is_binary(b"plain text"));
    }
}
