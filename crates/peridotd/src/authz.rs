//! Who may ask the daemon for what.
//!
//! The socket is only reachable by the user's own processes, and Peridot
//! has no passphrase, so it cannot prove which program is calling. What it
//! can do: tell its own panel and command apart from everything else by
//! the caller's executable (readable for both), class every method by how
//! much it can do, and route the dangerous ones through the panel. Same
//! user code that wants to misbehave can still read the keyring or edit
//! `~/.config` directly; this stops accidents, sandboxed apps and lazy
//! misuse, and keeps Peridot's powers (Opal's signature, the sync channel
//! to your other computers, uploads) from being borrowed quietly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use opal_kit::ipc::Peer;

/// Which of the user's programs is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Caller {
    /// Peridot's bar panel (the shell's QML runs inside quickshell).
    Panel,
    /// The `peridot` command.
    Cli,
    /// Anything else, including a process whose executable can't be read.
    Other,
}

impl Caller {
    pub fn name(self) -> &'static str {
        match self {
            Caller::Panel => "the panel",
            Caller::Cli => "the peridot command",
            Caller::Other => "another program",
        }
    }
}

/// How much a method can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// Looks at state. Anyone; secrets are left out for `Other`.
    Read,
    /// Small, reversible, or already guarded by Opal's own prompts.
    Routine,
    /// Publishes as you, uploads, changes what syncs or where.
    Sensitive,
    /// Exports the key, runs installers, hands the identity to another
    /// computer, or stops syncing. The panel confirms; the command waits
    /// for the panel; others never.
    Dangerous,
}

/// Every method the daemon answers, with its class. Anything not listed
/// is refused before it reaches the API.
pub const METHODS: &[(&str, Class)] = &[
    // read
    ("status", Class::Read),
    ("opal.accounts", Class::Read),
    ("share.list", Class::Read),
    ("gallery.list", Class::Read),
    ("gallery.item", Class::Read),
    ("gallery.reviews", Class::Read),
    ("gallery.following", Class::Read),
    ("gallery.setups", Class::Read),
    ("gallery.setup.mine", Class::Read),
    ("gallery.setup.steps", Class::Read),
    ("items.defaults", Class::Read),
    ("identity.card", Class::Read),
    ("profile.get", Class::Read),
    ("contacts.resolve", Class::Read),
    ("relays.check", Class::Read),
    ("approvals.list", Class::Read),
    // routine
    ("sync.now", Class::Routine),
    ("sync.pause", Class::Routine),
    ("sync.auto_apply", Class::Routine),
    ("apply", Class::Routine),
    ("conflict.keep_local", Class::Routine),
    ("history.undo", Class::Routine),
    ("gallery.refresh", Class::Routine),
    ("gallery.like", Class::Routine),
    ("gallery.follow", Class::Routine),
    ("gallery.setup.like", Class::Routine),
    ("gallery.unreview", Class::Routine),
    ("share.sweep", Class::Routine),
    ("share.revoke", Class::Routine),
    ("offer.dismiss", Class::Routine),
    ("device.rename", Class::Routine),
    ("recovery.save_page", Class::Routine),
    // sensitive
    ("share.file", Class::Sensitive),
    ("share.text", Class::Sensitive),
    ("share.send", Class::Sensitive),
    ("gallery.review", Class::Sensitive),
    ("gallery.enable", Class::Sensitive),
    ("gallery.list_item", Class::Sensitive),
    ("gallery.setup.publish", Class::Sensitive),
    ("gallery.setup.remove", Class::Sensitive),
    ("profile.set", Class::Sensitive),
    ("items.set", Class::Sensitive),
    ("relays.set", Class::Sensitive),
    ("device.remove", Class::Sensitive),
    ("servers.audit", Class::Sensitive),
    ("pair.new", Class::Sensitive),
    ("pair.join", Class::Sensitive),
    ("pair.view", Class::Sensitive),
    ("pair.cancel", Class::Sensitive),
    ("setup.start_fresh", Class::Sensitive),
    ("setup.import", Class::Sensitive),
    ("setup.use_opal", Class::Sensitive),
    ("recovery.restore", Class::Sensitive),
    ("opal.pair", Class::Sensitive),
    // dangerous
    ("recovery.create", Class::Dangerous),
    ("identity.move.start", Class::Dangerous),
    ("identity.move.finish", Class::Dangerous),
    ("setup.leave", Class::Dangerous),
    ("pair.confirm", Class::Dangerous),
    ("gallery.install", Class::Dangerous),
    ("gallery.setup.install", Class::Dangerous),
    ("offer.accept", Class::Dangerous),
    ("approvals.answer", Class::Dangerous),
];

pub fn classify(method: &str) -> Option<Class> {
    METHODS.iter().find(|(m, _)| *m == method).map(|(_, c)| *c)
}

/// Executables that count as the panel and the command.
#[derive(Debug, Clone)]
pub struct Trust {
    pub panel_exes: Vec<PathBuf>,
    pub cli_exes: Vec<PathBuf>,
}

impl Trust {
    /// The defaults: quickshell for the panel, `peridot` next to this
    /// daemon (or in /usr/bin) for the command. `extra_panel` and
    /// `extra_cli` come from the daemon's development flags (tests).
    pub fn new(extra_panel: &[PathBuf], extra_cli: &[PathBuf]) -> Self {
        let mut panel_exes = vec![
            PathBuf::from("/usr/bin/quickshell"),
            PathBuf::from("/usr/bin/qs"),
            PathBuf::from("/usr/local/bin/quickshell"),
        ];
        panel_exes.extend_from_slice(extra_panel);
        let mut cli_exes = vec![PathBuf::from("/usr/bin/peridot")];
        if let Ok(me) = std::env::current_exe()
            && let Some(dir) = me.parent()
        {
            cli_exes.push(dir.join("peridot"));
        }
        cli_exes.extend_from_slice(extra_cli);
        Self {
            panel_exes,
            cli_exes,
        }
    }

    pub fn caller(&self, peer: &Peer) -> Caller {
        let Some(exe) = peer.exe.as_deref() else {
            return Caller::Other;
        };
        if self.panel_exes.iter().any(|p| same_file(p, exe)) {
            return Caller::Panel;
        }
        if self.cli_exes.iter().any(|p| same_file(p, exe)) {
            return Caller::Cli;
        }
        Caller::Other
    }
}

/// The same path, or the same file behind two paths (a symlinked
/// `~/.local/bin/peridot`).
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Per caller-class token buckets, and a lockout after repeated "no"s.
#[derive(Default)]
pub struct Limits {
    inner: Mutex<LimitsInner>,
}

#[derive(Default)]
struct LimitsInner {
    stamps: HashMap<(Caller, &'static str), Vec<u64>>,
    denials: u32,
    lockout_until: u64,
}

/// (bucket name, how many, per how many seconds)
fn budget(class: Class, method: &str) -> Option<(&'static str, usize, u64)> {
    Some(match (class, method) {
        (_, m) if m.starts_with("share.") && class != Class::Read => ("share", 20, 3600),
        (_, "pair.join") => ("pair.join", 5, 15 * 60),
        (Class::Sensitive, _) => ("sensitive", 30, 60),
        (Class::Dangerous, _) => ("dangerous", 10, 10 * 60),
        _ => return None,
    })
}

impl Limits {
    pub fn check(
        &self,
        caller: Caller,
        class: Class,
        method: &str,
        now: u64,
    ) -> anyhow::Result<()> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if class == Class::Dangerous && now < g.lockout_until {
            anyhow::bail!(
                "Peridot was told no several times; try again in {} minutes",
                (g.lockout_until - now).div_ceil(60)
            );
        }
        let Some((bucket, n, per)) = budget(class, method) else {
            return Ok(());
        };
        let stamps = g.stamps.entry((caller, bucket)).or_default();
        stamps.retain(|t| now.saturating_sub(*t) < per);
        if stamps.len() >= n {
            anyhow::bail!("too many requests; try again in a few minutes");
        }
        stamps.push(now);
        Ok(())
    }

    /// The panel (or the person) said no to a dangerous request.
    pub fn denied(&self, now: u64) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.denials += 1;
        if g.denials >= 3 {
            g.denials = 0;
            g.lockout_until = now + 10 * 60;
        }
    }

    pub fn allowed(&self) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.denials = 0;
    }
}

/// What a dangerous method is about to do, for the approval card.
pub fn describe(method: &str, params: &serde_json::Value) -> String {
    match method {
        "recovery.create" => "show your recovery kit (the key, encrypted, and six words)".into(),
        "identity.move.start" => "prepare your key for moving into Opal".into(),
        "identity.move.finish" => {
            "hand your identity to Opal and sign through it from now on".into()
        }
        "setup.leave" => "stop syncing on this computer and forget its identity".into(),
        "pair.confirm" => "confirm a pairing and send your sync key to another computer".into(),
        "gallery.install" => format!(
            "install {}",
            params["url"].as_str().unwrap_or("a theme or plugin")
        ),
        "gallery.setup.install" => format!(
            "run one step of someone's setup ({})",
            params["step"]["url"]
                .as_str()
                .or(params["step"]["name"].as_str())
                .unwrap_or("a theme or plugin")
        ),
        "offer.accept" => format!(
            "install or switch to {}",
            params["name"]
                .as_str()
                .unwrap_or("what another computer has")
        ),
        m => m.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_api_method_is_classified_and_nothing_twice() {
        let src = include_str!("api.rs");
        let mut missing = Vec::new();
        for line in src.lines() {
            let t = line.trim();
            if let Some(rest) = t.strip_prefix('"')
                && let Some(end) = rest.find("\" =>")
            {
                let m = &rest[..end];
                if (m.contains('.') || m == "status" || m == "apply") && classify(m).is_none() {
                    missing.push(m.to_string());
                }
            }
        }
        assert!(missing.is_empty(), "unclassified methods: {missing:?}");
        let mut names: Vec<&str> = METHODS.iter().map(|(m, _)| *m).collect();
        names.sort();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len(), "a method is listed twice");
    }

    #[test]
    fn callers_are_told_apart_by_executable() {
        let trust = Trust::new(&[PathBuf::from("/tmp/x/quickshell")], &[]);
        let mut peer = Peer::default();
        assert_eq!(trust.caller(&peer), Caller::Other);
        peer.exe = Some(PathBuf::from("/usr/bin/quickshell"));
        assert_eq!(trust.caller(&peer), Caller::Panel);
        peer.exe = Some(PathBuf::from("/tmp/x/quickshell"));
        assert_eq!(trust.caller(&peer), Caller::Panel);
        peer.exe = Some(PathBuf::from("/usr/bin/peridot"));
        assert_eq!(trust.caller(&peer), Caller::Cli);
        peer.exe = Some(PathBuf::from("/usr/bin/python3"));
        assert_eq!(trust.caller(&peer), Caller::Other);
    }

    #[test]
    fn limits_and_lockout() {
        let l = Limits::default();
        for _ in 0..10 {
            l.check(Caller::Panel, Class::Dangerous, "setup.leave", 100)
                .unwrap();
        }
        assert!(
            l.check(Caller::Panel, Class::Dangerous, "setup.leave", 100)
                .is_err()
        );
        assert!(
            l.check(Caller::Cli, Class::Dangerous, "setup.leave", 100)
                .is_ok(),
            "buckets are per caller"
        );
        assert!(l.check(Caller::Other, Class::Read, "status", 100).is_ok());
        l.denied(200);
        l.denied(200);
        assert!(l.check(Caller::Cli, Class::Dangerous, "x", 200).is_ok());
        l.denied(200);
        let e = l
            .check(Caller::Cli, Class::Dangerous, "x", 200)
            .unwrap_err()
            .to_string();
        assert!(e.contains("told no"), "{e}");
        assert!(
            l.check(Caller::Cli, Class::Dangerous, "x", 200 + 601)
                .is_ok()
        );
    }
}
