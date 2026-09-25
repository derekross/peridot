//! Finding the files that sync: walk the manifest's targets (never all of
//! home), skip links, and check each file's contents before it's published.

use std::path::Path;

use serde::Serialize;

use crate::apply::{ApplyError, Home};
use crate::manifest::{Manifest, Target, is_binary, looks_secret};

/// Most files one scan looks at (a runaway nvim plugin folder shouldn't
/// turn into thousands of events).
pub const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 8;

/// Why a covered file isn't synced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Skip {
    /// A link, e.g. managed by Stow or chezmoi.
    Linked,
    /// Looks like it holds a secret.
    Secret {
        what: String,
    },
    Binary,
    TooBig,
    Unreadable {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub path: String,
    #[serde(flatten)]
    pub why: Skip,
}

/// Paths (relative to home) the manifest syncs, sorted, found under its
/// targets. Links are left out here; [`read_checked`] reports them.
pub fn syncable_paths(home: &Home, manifest: &Manifest) -> Vec<String> {
    let mut out = Vec::new();
    for target in manifest.targets() {
        match target {
            Target::File(p) => {
                if home
                    .path()
                    .join(&p)
                    .symlink_metadata()
                    .is_ok_and(|m| !m.is_dir())
                {
                    out.push(p);
                }
            }
            Target::Dir(d) => walk(home.path(), &d, 1, &mut out),
            Target::Tree(d) => walk(home.path(), &d, MAX_DEPTH, &mut out),
        }
        if out.len() >= MAX_FILES {
            break;
        }
    }
    out.retain(|p| manifest.syncs(p));
    out.sort();
    out.dedup();
    out.truncate(MAX_FILES);
    out
}

fn walk(home: &Path, rel_dir: &str, depth: usize, out: &mut Vec<String>) {
    if depth == 0 || out.len() >= MAX_FILES {
        return;
    }
    // A linked folder (e.g. `~/.config/nvim` managed by Stow) is left alone.
    if home
        .join(rel_dir)
        .symlink_metadata()
        .is_ok_and(|m| m.is_symlink())
    {
        return;
    }
    let Ok(entries) = std::fs::read_dir(home.join(rel_dir)) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let rel = if rel_dir.is_empty() {
            name
        } else {
            format!("{rel_dir}/{name}")
        };
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            // Never descend into linked folders (is_dir is false for links).
            walk(home, &rel, depth - 1, out);
        } else {
            out.push(rel);
        }
    }
}

/// Read a file for publishing: its contents, or why it's skipped. None if
/// it doesn't exist (any more).
pub fn read_checked(home: &Home, path: &str) -> Result<Option<Vec<u8>>, Skipped> {
    let skip = |why| Skipped {
        path: path.to_string(),
        why,
    };
    match home.read(path) {
        Ok(None) => Ok(None),
        Ok(Some(content)) => {
            if is_binary(&content) {
                Err(skip(Skip::Binary))
            } else if let Some(what) = looks_secret(&content) {
                Err(skip(Skip::Secret { what: what.into() }))
            } else {
                Ok(Some(content))
            }
        }
        Err(ApplyError::Symlink(_)) => Err(skip(Skip::Linked)),
        Err(ApplyError::TooBig(_)) => Err(skip(Skip::TooBig)),
        Err(e) => Err(skip(Skip::Unreadable {
            error: e.to_string(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Choices;

    #[test]
    fn finds_covered_files_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let h = dir.path();
        let put = |p: &str, c: &[u8]| {
            let full = h.join(p);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, c).unwrap();
        };
        put(".config/hypr/bindings.lua", b"bind");
        put(".config/hypr/monitors.lua", b"monitor");
        put(".config/hypr/autostart.lua", b"exec");
        put(".config/hypr/sub/deep.lua", b"not one level");
        put(".config/omarchy/extensions/omarchy-menu.jsonc", b"{}");
        put(".config/omarchy/plugins/x/manifest.json", b"{}");
        put(".XCompose", b"compose");
        put("Documents/private.txt", b"no");
        put(".ssh/id_ed25519", b"-----BEGIN");
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("k"), b"x").unwrap();
        std::fs::create_dir_all(h.join(".config/kitty")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("k"), h.join(".config/kitty/kitty.conf"))
            .unwrap();
        std::os::unix::fs::symlink(outside.path(), h.join(".config/tmux")).unwrap();

        let home = Home::open(h).unwrap();
        let paths = syncable_paths(&home, &Manifest::new(Choices::default()));
        assert_eq!(
            paths,
            [
                ".XCompose",
                ".config/hypr/bindings.lua",
                ".config/kitty/kitty.conf",
                ".config/omarchy/extensions/omarchy-menu.jsonc",
            ]
        );
        assert_eq!(
            read_checked(&home, ".config/kitty/kitty.conf")
                .unwrap_err()
                .why,
            Skip::Linked
        );
        assert_eq!(
            read_checked(&home, ".XCompose").unwrap().unwrap(),
            b"compose"
        );
        put(
            ".config/hypr/looknfeel.lua",
            b"-- api_key = abcdefghijklmnop",
        );
        assert!(matches!(
            read_checked(&home, ".config/hypr/looknfeel.lua")
                .unwrap_err()
                .why,
            Skip::Secret { .. }
        ));
        assert_eq!(read_checked(&home, ".config/hypr/gone.lua").unwrap(), None);
    }
}
