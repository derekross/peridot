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

/// Files whose contents are run, sourced or evaluated by something on the
/// other computer (a shell, a window manager, an editor, a hook). Whatever
/// their tier, they're never applied without you looking first.
const RUNS_COMMANDS: &[&str] = &[
    ".config/hypr/*.lua",
    ".config/hypr/*.conf",
    ".config/hypr/autostart.lua",
    ".config/omarchy/extensions/**",
    ".config/omarchy/hooks/**",
    ".bashrc",
    ".config/kitty/**",
    ".config/ghostty/**",
    ".config/alacritty/**",
    ".config/tmux/**",
    ".config/starship.toml",
    ".config/lazygit/config.yml",
    ".config/mpv/**",
    ".config/nvim/**",
];

const NEVER: &[&str] = &[
    ".ssh/**",
    ".gnupg/**",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".docker/config.json",
    ".aws/**",
    ".kube/**",
    ".config/sops/**",
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
    runs_commands: GlobSet,
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
            runs_commands: set(RUNS_COMMANDS),
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

    /// Whether something on this computer runs what's in `path` (a shell,
    /// Hyprland, a terminal, an editor, a hook). Independent of tier: such
    /// files are shown before they're applied, and auto-apply skips them.
    pub fn runs_commands(&self, path: &str) -> bool {
        self.runs_commands.is_match(path)
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
    } else if parts[fixed.len()..].contains(&"**") || parts.len() - fixed.len() > 1 {
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
/// published, whatever the tier. Tuned against real Omarchy configs: bind
/// lines, paths, colours, UUIDs and commit hashes must pass.
pub fn looks_secret(content: &[u8]) -> Option<&'static str> {
    let text = String::from_utf8_lossy(content);
    // Prefixes that mean one thing only.
    const MARKERS: &[(&str, &str)] = &[
        ("-----BEGIN", "a private key"),
        ("nsec1", "a Nostr private key"),
        ("ncryptsec1", "an encrypted Nostr key"),
        ("AGE-SECRET-KEY-1", "an age key"),
        ("ghp_", "a GitHub token"),
        ("github_pat_", "a GitHub token"),
        ("gho_", "a GitHub token"),
        ("glpat-", "a GitLab token"),
        ("sk-ant-", "an API key"),
        ("sk-proj-", "an API key"),
        ("sk_live_", "a Stripe key"),
        ("sk_test_", "a Stripe key"),
        ("AIza", "a Google API key"),
    ];
    for (marker, what) in MARKERS {
        if text.contains(marker) {
            return Some(what);
        }
    }
    // Short prefixes that also start ordinary words (desk-, npm_config_,
    // SLOVAKIA): only at the start of a word, and only with a token's
    // worth of characters after them.
    const PREFIXED: &[(&str, usize, &str)] = &[
        ("sk-", 20, "an API key"),
        ("hf_", 30, "a Hugging Face token"),
        ("npm_", 30, "an npm token"),
        ("pypi-", 20, "a PyPI token"),
        ("SG.", 20, "a SendGrid key"),
        ("AKIA", 16, "an AWS key"),
        ("ASIA", 16, "an AWS key"),
    ];
    for (prefix, min, what) in PREFIXED {
        if has_prefixed_token(&text, prefix, *min) {
            return Some(what);
        }
    }
    if has_slack_token(&text) {
        return Some("a Slack token");
    }
    for line in text.lines() {
        if let Some(what) = line_looks_secret(line) {
            return Some(what);
        }
    }
    None
}

fn line_looks_secret(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    // .netrc: `machine host login name password x`, or a `password x`
    // line by itself (not PAM's `password required pam_unix.so`).
    let words: Vec<&str> = line.split_whitespace().collect();
    let netrc_line = matches!(words.first(), Some(&"machine" | &"default" | &"login"))
        || (words.len() == 2 && words[0] == "password");
    if netrc_line
        && words
            .windows(2)
            .any(|w| w[0] == "password" && !w[1].starts_with(['=', ':']))
    {
        return Some("a password");
    }
    // Authorization: Bearer <token> (not a $VARIABLE or <placeholder>).
    if lower.contains("authorization")
        && let Some(i) = lower.find("bearer ")
        && let Some(v) = line[i + 7..].split_whitespace().next()
    {
        let v = v.trim_matches(['"', '\'', ',', ';', '`']);
        if v.len() >= 8 && !is_placeholder(v) {
            return Some("a token");
        }
    }
    // Credentials in a URL: scheme://user:pass@host
    for (i, _) in line.match_indices("://") {
        let authority = line[i + 3..]
            .split(|c: char| c == '/' || c.is_whitespace() || c == '"' || c == '\'')
            .next()
            .unwrap_or("");
        if let Some((userinfo, _)) = authority.rsplit_once('@')
            && let Some((_, pass)) = userinfo.split_once(':')
            && !pass.is_empty()
            && !is_placeholder(pass)
        {
            return Some("a password in a link");
        }
    }
    // JSON: "token": "…" and friends, with a value worth stealing.
    for key in [
        "auth", "key", "apikey", "api_key", "secret", "token", "password",
    ] {
        let quoted = format!("\"{key}\"");
        let mut from = 0;
        while let Some(i) = lower[from..].find(&quoted) {
            let after = &line[from + i + quoted.len()..];
            let after = after.trim_start();
            if let Some(after) = after.strip_prefix(':')
                && let Some(value) = after.trim_start().strip_prefix('"')
                && let Some(end) = value.find('"')
                && end >= 12
                && !value[..end].contains(' ')
                // "key" and "auth" name ordinary things too (a setting's
                // key, an auth mode): only a token-shaped value counts.
                && (!matches!(key, "key" | "auth") || token_shaped(&value[..end]))
            {
                return Some("a password or token");
            }
            from += i + quoted.len();
        }
    }
    // export NAME=value: any name, when the value looks like a token.
    if let Some(rest) = line.trim_start().strip_prefix("export ")
        && let Some((_, value)) = rest.split_once('=')
    {
        let value = value.trim().trim_matches(['"', '\'']);
        if value.len() >= 16 && !value.contains(' ') && token_shaped(value) {
            return Some("a token");
        }
    }
    // Generic `api_key = "…"` / `password: …` assignments: the keyword in
    // the key, a value worth stealing after it. Names of things
    // (`api_key_name = "OPENAI_API_KEY"`, `--password-store=gnome-libsecret`)
    // have no digit in them; passwords and tokens nearly always do.
    if let Some((key, value)) = line.split_once(['=', ':']) {
        let key = key.to_ascii_lowercase();
        let value = value.trim().trim_matches(['"', '\'', ',', ';']);
        if [
            "api_key", "apikey", "api-key", "password", "passwd", "secret", "token",
        ]
        .iter()
        .any(|k| key.contains(k))
            && value.len() >= 12
            && !value.contains(' ')
            && !is_placeholder(value)
            && (value.bytes().any(|b| b.is_ascii_digit())
                || (value.len() >= 24 && token_charset(value) && mixed_case(value)))
        {
            return Some("a password or token");
        }
    }
    // Anything else that reads like a token: long, dense, several digits
    // among the letters. Paths, names and hashes are excluded first
    // (identifiers such as XF86KbdBrightnessDown are dense too, but have
    // a digit or two, not a token's share). Words are cut at quotes and
    // separators, so `NAME=value` is judged by its value.
    for word in line.split(|c: char| c.is_whitespace() || "\"'`=:,;".contains(c)) {
        if word.len() >= 20
            && token_shaped(word)
            && word.bytes().filter(u8::is_ascii_digit).count() >= 3
            && entropy(word) >= 4.0
        {
            return Some("a token");
        }
        if is_jwt(word) {
            return Some("a token");
        }
    }
    None
}

/// Not a value: a `$VARIABLE` or `"$quoted"`, a `<placeholder>`, a `%s`,
/// an expression `f(x)`, a path (`~/…`, `/…`), an `…`/`xxx` stand-in.
fn is_placeholder(v: &str) -> bool {
    v.starts_with(['$', '{', '<', '%', '(', '~', '/', '\\'])
        || v.starts_with("./")
        || v.starts_with("...")
        || v.starts_with("xxx")
        || v.contains(['$', '(', ')'])
}

/// Token characters only (letters, digits, `+=_-`; no `/` or `.`, so no
/// paths, names or versions), starting like a token rather than a flag,
/// not just hex (hashes, UUIDs), not words strung together with three or
/// more separators (`omarchy-hw-asus-zenbook-ux5406aa`,
/// `legacy_force_igpu_sha256`), and with letters and digits mixed inside
/// one run: names and dates (`tokyo-night-2024`) keep them apart.
fn token_shaped(v: &str) -> bool {
    if !token_charset(v)
        || v.starts_with(['-', '+', '='])
        || v.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
        || v.bytes().filter(|b| *b == b'-' || *b == b'_').count() >= 3
    {
        return false;
    }
    v.split(['-', '_']).any(|seg| {
        seg.bytes().any(|b| b.is_ascii_digit()) && seg.bytes().any(|b| b.is_ascii_alphabetic())
    })
}

fn token_charset(v: &str) -> bool {
    v.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"+=_-".contains(&b))
}

fn mixed_case(v: &str) -> bool {
    v.bytes().any(|b| b.is_ascii_uppercase()) && v.bytes().any(|b| b.is_ascii_lowercase())
}

/// Shannon entropy in bits per character.
fn entropy(v: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in v.bytes() {
        counts[b as usize] += 1;
    }
    let n = v.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// A JWT: header.payload.signature, the first two base64url of `{"`.
fn is_jwt(word: &str) -> bool {
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.');
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 3
        && parts[0].starts_with("eyJ")
        && parts[1].starts_with("eyJ")
        && parts[2].len() >= 10
        && parts.iter().all(|p| {
            p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
}

/// `prefix` at the start of a word, followed by at least `min` token
/// characters (uppercase letters and digits for AWS-style ids).
fn has_prefixed_token(text: &str, prefix: &str, min: usize) -> bool {
    let upper_only = prefix.bytes().all(|b| b.is_ascii_uppercase());
    text.match_indices(prefix).any(|(i, _)| {
        let at_word_start = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let run = text[i + prefix.len()..]
            .bytes()
            .take_while(|b| {
                if upper_only {
                    b.is_ascii_uppercase() || b.is_ascii_digit()
                } else {
                    b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-'
                }
            })
            .count();
        at_word_start && run >= min
    })
}

/// `xox[abposr]-…`
fn has_slack_token(text: &str) -> bool {
    text.match_indices("xox").any(|(i, _)| {
        let rest = &text[i + 3..];
        let mut chars = rest.chars();
        matches!(chars.next(), Some('a' | 'b' | 'o' | 'p' | 's' | 'r'))
            && chars.next() == Some('-')
            && rest[2..]
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-')
                .count()
                >= 8
    })
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
    fn files_that_run_commands_are_known_whatever_their_tier() {
        let m = Manifest::new(Choices::default());
        for p in [
            ".config/hypr/bindings.lua",
            ".config/hypr/hyprsunset.conf",
            ".config/hypr/autostart.lua",
            ".config/omarchy/extensions/menu/x.sh",
            ".config/omarchy/hooks/theme-set",
            ".bashrc",
            ".config/kitty/kitty.conf",
            ".config/ghostty/config",
            ".config/alacritty/alacritty.toml",
            ".config/tmux/tmux.conf",
            ".config/starship.toml",
            ".config/lazygit/config.yml",
            ".config/mpv/input.conf",
            ".config/nvim/init.lua",
            ".config/nvim/lua/plugins/x.lua",
        ] {
            assert!(m.runs_commands(p), "{p}");
        }
        // Shared tier, but it can still run commands.
        assert_eq!(m.tier(".config/hypr/bindings.lua"), Some(Tier::Shared));
        for p in [
            ".config/omarchy/shell.json",
            ".config/omarchy/branding/logo.txt",
            ".config/btop/btop.conf",
            ".config/git/config",
            ".config/mimeapps.list",
            ".XCompose",
            ".config/foot/foot.ini",
        ] {
            assert!(!m.runs_commands(p), "{p}");
        }
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

    #[test]
    fn secret_files_by_name_are_never_synced() {
        let m = Manifest::new(Choices::default());
        for p in [
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".docker/config.json",
            ".aws/credentials",
            ".kube/config",
            ".config/sops/age/keys.txt",
            ".gnupg/private-keys-v1.d/x.key",
        ] {
            assert_eq!(m.tier(p), Some(Tier::Never), "{p}");
        }
    }

    #[test]
    fn spots_the_shapes_secrets_come_in() {
        let secrets: &[(&str, &str)] = &[
            (
                "machine api.example.com login derek password hunter2",
                "a password",
            ),
            (
                "machine api.example.com\n  login derek\n  password hunter2\n",
                "a password",
            ),
            ("password correct-horse", "a password"),
            (
                "header = \"Authorization: Bearer abcdef0123456789\"",
                "a token",
            ),
            (
                "url = https://derek:s3cretpass@git.example.com/x.git",
                "a password in a link",
            ),
            (
                "url = redis://:s3cretpass@cache.example.com:6379",
                "a password in a link",
            ),
            (
                "{\"auth\": \"dXNlcjpwYXNzd29yZA==\"}",
                "a password or token",
            ),
            (
                "{\"registry\": {\"token\": \"abcdefghijkl\"}}",
                "a password or token",
            ),
            ("  \"api_key\" : \"0123456789ab\",", "a password or token"),
            ("{\"key\": \"Sup3rSecretValue42\"}", "a password or token"),
            ("export GITHUB_TOKEN=abc123def456ghi789", "a token"),
            (
                "export OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz0123456789",
                "an API key",
            ),
            (
                "export OPENAI_API_KEY='sk-proj-abcdefghijklmnopqrstuvwxyz'",
                "an API key",
            ),
            ("STRIPE=sk_live_4eC39HqLyjWDarjtT1zdp7dc", "a Stripe key"),
            ("sk_test_4eC39HqLyjWDarjtT1zdp7dc", "a Stripe key"),
            (
                "HF_TOKEN=hf_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
                "a Hugging Face token",
            ),
            (
                "//registry.npmjs.org/:_authToken=npm_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789",
                "an npm token",
            ),
            ("aws_access_key_id = ASIAIOSFODNN7EXAMPLE", "an AWS key"),
            ("aws_access_key_id = AKIAIOSFODNN7EXAMPLE", "an AWS key"),
            (
                "AGE-SECRET-KEY-1QYQSZQGPQYQSZQGPQYQSZQGPQYQSZQGPQYQSZQGPQYQSZQGPQYQS3QAEQ",
                "an age key",
            ),
            (
                "SLACK_BOT_TOKEN=xoxb-1234567890-abcdefghij",
                "a Slack token",
            ),
            ("xoxp-1234567890-abcdefghij", "a Slack token"),
            (
                "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",
                "a SendGrid key",
            ),
            (
                "password = pypi-AgEIcHlwaS5vcmcCJDAwMDAwMDAw",
                "a PyPI token",
            ),
            (
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
                "a token",
            ),
            (
                "token: eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
                "a password or token",
            ),
            (
                "jwt=\"eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c\"",
                "a token",
            ),
            ("cookie = 9f8Gq2LmZx4Tn7Wv1KpR3sYb6HcJ0eDa5UoI8", "a token"),
            (
                "fast_token=\"YXNkZmFzZGxmbnNkYWZoYXNkZmhrYWxm\"",
                "a password or token",
            ),
            (
                "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "a password or token",
            ),
        ];
        for (text, what) in secrets {
            assert_eq!(looks_secret(text.as_bytes()), Some(*what), "{text}");
        }
    }

    #[test]
    fn ordinary_config_lines_are_not_secrets() {
        let fine: &[&str] = &[
            "bind = SUPER, Return, exec, ghostty",
            "bindd = SUPER SHIFT, S, Screenshot region, exec, omarchy-cmd-screenshot",
            "o.bind(\"SUPER + SHIFT + J\", \"Equalize windows\", os.getenv(\"HOME\") .. \"/.local/bin/hypr-equalize\")",
            "dofile((os.getenv(\"OMARCHY_PATH\") or \"/usr/share/omarchy\") .. \"/default/hypr/bootstrap.lua\")",
            "hl.monitor({ output = \"desc:Samsung Electric Company LC34G55T HNTY600390\", mode = \"3440x1440@165\", position = \"0x0\", scale = 1 })",
            "background = \"~/.config/omarchy/current/theme/backgrounds/1-aGVsbG8gd29ybGQgZnJvbSBvbWFyY2h5.jpg\"",
            "wallpaper = 1-omarchy-wallpaper-Zm9yZXN0LWF0LWR1c2s.png",
            "id = 550e8400-e29b-41d4-a716-446655440000",
            "-- pinned at commit 3f2a9c8e1b7d6f5a4c3b2a1908f7e6d5c4b3a291",
            "# from 3f2a9c8e1b7d6f5a4c3b2a1908f7e6d5c4b3a2913f2a9c8e1b7d6f5a4c3b2a19",
            "col.active_border = rgba(33ccffee) rgba(00ff99ee) 45deg",
            "windowrule = float, class:^(org.pulseaudio.pavucontrol)$",
            "env = LIBVA_DRIVER_NAME,nvidia",
            "monitor=DP-1,2560x1440@144,0x0,1",
            "font_family = JetBrainsMono Nerd Font Mono",
            "set -g @plugin 'tmux-plugins/tmux-resurrect'",
            "map ctrl+shift+enter new_window_with_cwd",
            "{ \"nvim-treesitter/nvim-treesitter\", build = \":TSUpdate\" }",
            "vim.opt.clipboard = \"unnamedplus\"",
            "format = \"$directory$git_branch$character\"",
            "symbol = \"\u{f02a2} \"",
            "url = https://github.com/derekross/omarchy-calendar.git",
            "[url \"git@github.com:\"]",
            "export EDITOR=nvim",
            "export PATH=$HOME/.local/bin:$PATH",
            "export HISTCONTROL=ignoreboth:erasedups",
            "export NODE_OPTIONS=--max-old-space-size=4096",
            "export ANTHROPIC_MODEL=claude-sonnet-4-5-20250929",
            "export LS_COLORS=\"di=1;34:ln=1;36:so=1;35:pi=33:ex=1;32\"",
            "export TERM=xterm-256color",
            "export MANPAGER=\"sh -c 'col -bx | bat -l man -p'\"",
            "export npm_config_prefix=~/.npm-global",
            "export SSH_AUTH_SOCK=/run/user/1000/gnupg/S.gpg-agent.ssh",
            "export XCURSOR_THEME=Bibata-Modern-Classic",
            "header = \"Authorization: Bearer $TOKEN\"",
            "# Authorization: Bearer <token>",
            "desk-lamp = on\ntask-manager = btop\nSLOVAKIA = Bratislava\nasian-fusion = yes",
            "keybinding:\n  universal:\n    quit: q\n    return: <esc>",
            "customCommands:\n  - key: 'C'\n    command: git commit --no-verify",
            "{\"key\": \"SUPER+Return\", \"exec\": \"ghostty\"}",
            "        \"key\": \"showWhenEmpty\",",
            "{\"auth\": \"browser-login\"}",
            "$last_update_token = \"\\\"$last_update_token\\\"\";",
            "p.secret = secret.text.trim()",
            "token_file = \"~/.local/share/vdirsyncer/google_token\"",
            "o.bind(\"XF86KbdBrightnessDown\", \"Keyboard brightness down\", \"omarchy-brightness-keyboard down\", { locked = true, repeating = true })",
            "\"output\":\"release/build/libc-3cf23df31a163494/output\"",
            "password  required pam_unix.so",
            "api_key_name = \"OPENAI_API_KEY\"",
            "local token = vim.env.GITHUB_TOKEN",
            "--password-store=gnome-libsecret",
            "legacy_force_igpu_sha256=d604e7c4903829563e45fc52188fc5602c3f1bc66e247f0a2cc0a974ed6e57db",
            "if omarchy-hw-asus-expertbook-b9406 || omarchy-hw-asus-zenbook-ux5406aa; then",
            "searchableToken: searchableToken,",
            "{\"widgets\": [\"clock\", \"battery\", \"network\", \"workspaces\"]}",
            "-- github.com/basecamp/omarchy/blob/master/default/hypr/bindings/tiling.conf",
        ];
        for text in fine {
            assert_eq!(looks_secret(text.as_bytes()), None, "{text}");
        }
        // Long paths and identifiers with digits inside them.
        assert!(looks_secret(b"/usr/lib/jvm/java-17-openjdk/bin/java").is_none());
        assert!(looks_secret(b"nvidia-utils-580.82.09-2-x86_64.pkg.tar.zst").is_none());
        assert!(entropy("aaaa") == 0.0 && entropy("abcd") == 2.0);
    }
}
