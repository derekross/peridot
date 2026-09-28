//! Reading and writing files in your home folder, safely.
//!
//! Synced paths come from your other devices, so they're treated as
//! untrusted: every open is resolved by the kernel (`openat2`) beneath your
//! home folder with symlinks refused, so a path can never escape home or be
//! redirected through a link. Files are replaced atomically (temp file,
//! fsync, rename) with a fixed mode: 0644, or 0755 only when the caller
//! asks for it (an existing file's exec bit never carries over).

use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use crate::manifest::{MAX_FILE_SIZE, is_clean_relative};

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("{0} isn't a safe path")]
    BadPath(String),
    #[error("{0} is a link (managed by a dotfile tool?); Peridot leaves it alone")]
    Symlink(String),
    #[error("{0} isn't a regular file")]
    NotAFile(String),
    #[error("{0} is too big to sync")]
    TooBig(String),
    #[error("{path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

type Result<T> = std::result::Result<T, ApplyError>;

const RESOLVE: ResolveFlags = ResolveFlags::BENEATH
    .union(ResolveFlags::NO_SYMLINKS)
    .union(ResolveFlags::NO_MAGICLINKS);

/// Your home folder, opened once; all paths are resolved beneath it.
pub struct Home {
    dir: OwnedFd,
    path: PathBuf,
}

impl Home {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let dir = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            dir,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether safe opens work here at all. A sandbox that blocks `openat2`
    /// (systemd's RestrictSUIDSGID= does, since it can't inspect the mode
    /// inside the call's struct) would otherwise make every file look
    /// unreadable.
    pub fn self_check(&self) -> std::result::Result<(), String> {
        match rustix::fs::openat2(
            &self.dir,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
            RESOLVE,
        ) {
            Ok(_) => Ok(()),
            Err(Errno::NOSYS) => Err("this computer's Peridot service can't open files safely: \
                 openat2 is blocked (a sandbox option such as RestrictSUIDSGID= in \
                 peridot.service). Reinstall Peridot to get the current service file."
                .into()),
            Err(e) => Err(format!("can't read your home folder: {e}")),
        }
    }

    /// The file's contents, or None if it doesn't exist.
    pub fn read(&self, rel: &str) -> Result<Option<Vec<u8>>> {
        check(rel)?;
        let fd = match rustix::fs::openat2(
            &self.dir,
            rel,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
            RESOLVE,
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) | Err(Errno::NOTDIR) => return Ok(None),
            Err(Errno::LOOP) | Err(Errno::XDEV) => return Err(ApplyError::Symlink(rel.into())),
            Err(e) => return Err(io(rel, e)),
        };
        let st = rustix::fs::fstat(&fd).map_err(|e| io(rel, e))?;
        if FileType::from_raw_mode(st.st_mode) != FileType::RegularFile {
            return Err(ApplyError::NotAFile(rel.into()));
        }
        if st.st_size as u64 > MAX_FILE_SIZE {
            return Err(ApplyError::TooBig(rel.into()));
        }
        let mut out = Vec::new();
        std::fs::File::from(fd)
            .take(MAX_FILE_SIZE + 1)
            .read_to_end(&mut out)
            .map_err(|e| ApplyError::Io {
                path: rel.into(),
                source: e,
            })?;
        if out.len() as u64 > MAX_FILE_SIZE {
            return Err(ApplyError::TooBig(rel.into()));
        }
        Ok(Some(out))
    }

    /// Is this path (or a folder on the way to it) a symlink?
    pub fn is_linked(&self, rel: &str) -> bool {
        matches!(self.read(rel), Err(ApplyError::Symlink(_)))
    }

    /// Replace (or create) a file atomically, mode 0644, or 0755 when
    /// `executable` (for hooks; the file's previous mode is never kept).
    pub fn write(&self, rel: &str, content: &[u8], executable: bool) -> Result<()> {
        let mode = if executable { 0o755 } else { 0o644 };
        self.write_mode(rel, content, Mode::from_bits_truncate(mode))
    }

    /// Replace (or create) a file only its owner can read (0600): undo
    /// backups, which hold whatever a synced file held.
    pub fn write_private(&self, rel: &str, content: &[u8]) -> Result<()> {
        self.write_mode(rel, content, Mode::from_bits_truncate(0o600))
    }

    fn write_mode(&self, rel: &str, content: &[u8], mode: Mode) -> Result<()> {
        check(rel)?;
        if content.len() as u64 > MAX_FILE_SIZE {
            return Err(ApplyError::TooBig(rel.into()));
        }
        let (parent, name) = split(rel);
        let dir = self.open_dir(parent, true)?;
        let exists = match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => match FileType::from_raw_mode(st.st_mode) {
                FileType::RegularFile => true,
                FileType::Symlink => return Err(ApplyError::Symlink(rel.into())),
                _ => return Err(ApplyError::NotAFile(rel.into())),
            },
            Err(Errno::NOENT) => false,
            Err(e) => return Err(io(rel, e)),
        };
        let tmp = format!(
            ".{name}.peridot-{}",
            hex::encode(crate::crypto::random_bytes::<6>())
        );
        let fd = match rustix::fs::openat(
            &dir,
            tmp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode,
        ) {
            Ok(fd) => fd,
            // A folder we may not add to, holding a file we may write: the
            // daemon's sandbox mounts the home folder read-only and lets
            // through only the files it syncs there (`~/.bashrc`), which
            // can be written but not replaced. Then in place, with the undo
            // backup as the safety net.
            Err(Errno::ROFS | Errno::ACCESS | Errno::PERM | Errno::BUSY) if exists => {
                return self.write_in_place(&dir, rel, name, content, mode);
            }
            Err(e) => return Err(io(rel, e)),
        };
        let result = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::from(fd);
            f.write_all(content)?;
            // umask may have trimmed the mode; set it exactly.
            rustix::fs::fchmod(f.as_fd(), mode)?;
            f.sync_all()?;
            rustix::fs::renameat(&dir, tmp.as_str(), &dir, name)?;
            rustix::fs::fsync(&dir)?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = rustix::fs::unlinkat(&dir, tmp.as_str(), AtFlags::empty());
            return Err(ApplyError::Io {
                path: rel.into(),
                source: e,
            });
        }
        Ok(())
    }

    /// Overwrite an existing regular file where a temporary file can't be
    /// made next to it: write, cut to length, fsync.
    fn write_in_place(
        &self,
        dir: &OwnedFd,
        rel: &str,
        name: &str,
        content: &[u8],
        mode: Mode,
    ) -> Result<()> {
        let fd = rustix::fs::openat(
            dir,
            name,
            OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io(rel, e))?;
        let result = (|| -> std::io::Result<()> {
            let mut f = std::fs::File::from(fd);
            f.write_all(content)?;
            f.set_len(content.len() as u64)?;
            rustix::fs::fchmod(f.as_fd(), mode)?;
            f.sync_all()?;
            Ok(())
        })();
        result.map_err(|e| ApplyError::Io {
            path: rel.into(),
            source: e,
        })
    }

    /// Delete a file (not a link, not a folder). Missing is fine.
    pub fn remove(&self, rel: &str) -> Result<()> {
        check(rel)?;
        let (parent, name) = split(rel);
        let dir = match self.open_dir(parent, false) {
            Ok(d) => d,
            Err(ApplyError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(st) => match FileType::from_raw_mode(st.st_mode) {
                FileType::RegularFile => {}
                FileType::Symlink => return Err(ApplyError::Symlink(rel.into())),
                _ => return Err(ApplyError::NotAFile(rel.into())),
            },
            Err(Errno::NOENT) => return Ok(()),
            Err(e) => return Err(io(rel, e)),
        }
        rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(|e| io(rel, e))
    }

    /// Open a folder beneath home one component at a time (so every step
    /// is checked), creating missing ones (0755) if asked.
    fn open_dir(&self, rel: &str, create: bool) -> Result<OwnedFd> {
        let mut dir = rustix::fs::openat(
            &self.dir,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| io(rel, e))?;
        if rel.is_empty() {
            return Ok(dir);
        }
        for part in rel.split('/') {
            let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let next = match rustix::fs::openat2(&dir, part, flags, Mode::empty(), RESOLVE) {
                Ok(fd) => fd,
                Err(Errno::NOENT) if create => {
                    match rustix::fs::mkdirat(&dir, part, Mode::from_bits_truncate(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(io(rel, e)),
                    }
                    rustix::fs::openat2(&dir, part, flags, Mode::empty(), RESOLVE)
                        .map_err(|e| io(rel, e))?
                }
                Err(Errno::LOOP) | Err(Errno::XDEV) => return Err(ApplyError::Symlink(rel.into())),
                Err(Errno::NOTDIR) => {
                    // A link to a folder shows up as "not a folder" here.
                    let linked = rustix::fs::statat(&dir, part, AtFlags::SYMLINK_NOFOLLOW)
                        .is_ok_and(|st| FileType::from_raw_mode(st.st_mode) == FileType::Symlink);
                    return Err(if linked {
                        ApplyError::Symlink(rel.into())
                    } else {
                        ApplyError::NotAFile(rel.into())
                    });
                }
                Err(e) => return Err(io(rel, e)),
            };
            dir = next;
        }
        Ok(dir)
    }
}

fn check(rel: &str) -> Result<()> {
    if is_clean_relative(rel) {
        Ok(())
    } else {
        Err(ApplyError::BadPath(rel.into()))
    }
}

fn split(rel: &str) -> (&str, &str) {
    rel.rsplit_once('/').unwrap_or(("", rel))
}

fn io(path: &str, e: Errno) -> ApplyError {
    ApplyError::Io {
        path: path.into(),
        source: e.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn home() -> (tempfile::TempDir, Home) {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        (dir, home)
    }

    #[test]
    fn a_file_in_a_folder_we_may_not_add_to_is_written_in_place() {
        let (dir, home) = home();
        std::fs::write(dir.path().join(".bashrc"), b"old contents, longer").unwrap();
        // The folder refuses new entries (no temporary file), the file
        // itself is writable: the sandbox's view of the home folder.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = home.write(".bashrc", b"new", false);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        r.unwrap();
        assert_eq!(std::fs::read(dir.path().join(".bashrc")).unwrap(), b"new");
        // Making a file there is still refused, plainly.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = home.write(".XCompose", b"x", false);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(r.is_err());
    }

    #[test]
    fn writes_reads_and_removes_inside_home() {
        let (dir, home) = home();
        assert_eq!(home.read(".config/hypr/bindings.lua").unwrap(), None);
        home.write(".config/hypr/bindings.lua", b"bind", false)
            .unwrap();
        assert_eq!(
            home.read(".config/hypr/bindings.lua").unwrap().unwrap(),
            b"bind"
        );
        let meta = std::fs::metadata(dir.path().join(".config/hypr/bindings.lua")).unwrap();
        assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
        home.write(".config/hypr/bindings.lua", b"bind 2", false)
            .unwrap();
        assert_eq!(
            home.read(".config/hypr/bindings.lua").unwrap().unwrap(),
            b"bind 2"
        );
        // No temp files left behind.
        let left: Vec<_> = std::fs::read_dir(dir.path().join(".config/hypr"))
            .unwrap()
            .collect();
        assert_eq!(left.len(), 1);
        home.remove(".config/hypr/bindings.lua").unwrap();
        assert_eq!(home.read(".config/hypr/bindings.lua").unwrap(), None);
        home.remove(".config/nothing/here").unwrap();
    }

    #[test]
    fn refuses_paths_that_escape_home() {
        let (_dir, home) = home();
        for bad in ["../x", "/etc/passwd", "a/../../x", "", "a//b", "./a"] {
            assert!(
                matches!(home.write(bad, b"x", false), Err(ApplyError::BadPath(_))),
                "{bad}"
            );
            assert!(
                matches!(home.read(bad), Err(ApplyError::BadPath(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn never_follows_symlinks() {
        let (dir, home) = home();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("target"), b"outside").unwrap();
        // A linked file (e.g. managed by Stow).
        std::fs::create_dir_all(dir.path().join(".config/kitty")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("target"),
            dir.path().join(".config/kitty/kitty.conf"),
        )
        .unwrap();
        assert!(matches!(
            home.read(".config/kitty/kitty.conf"),
            Err(ApplyError::Symlink(_))
        ));
        assert!(home.is_linked(".config/kitty/kitty.conf"));
        assert!(matches!(
            home.write(".config/kitty/kitty.conf", b"x", false),
            Err(ApplyError::Symlink(_))
        ));
        assert!(matches!(
            home.remove(".config/kitty/kitty.conf"),
            Err(ApplyError::Symlink(_))
        ));
        // A linked folder on the way.
        std::os::unix::fs::symlink(outside.path(), dir.path().join(".config/foot")).unwrap();
        assert!(matches!(
            home.write(".config/foot/foot.ini", b"x", false),
            Err(ApplyError::Symlink(_))
        ));
        assert!(!outside.path().join("foot.ini").exists());
        // The same for a private write (backups).
        assert!(matches!(
            home.write_private(".config/kitty/kitty.conf", b"x"),
            Err(ApplyError::Symlink(_))
        ));
        assert!(matches!(
            home.write_private(".config/foot/foot.ini", b"x"),
            Err(ApplyError::Symlink(_))
        ));
        assert!(!outside.path().join("foot.ini").exists());
        assert_eq!(
            std::fs::read(outside.path().join("target")).unwrap(),
            b"outside"
        );
    }

    #[test]
    fn the_mode_is_fixed_and_an_exec_bit_never_survives() {
        let (dir, home) = home();
        let mode = |name: &str| {
            std::fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        let p = dir.path().join("hook");
        std::fs::write(&p, b"#!/bin/sh").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o4777)).unwrap();
        home.write("hook", b"#!/bin/sh\necho hi", false).unwrap();
        assert_eq!(mode("hook"), 0o644, "setuid and exec bits are gone");
        // Only an explicit request makes it executable, and only 0755.
        home.write("hook", b"#!/bin/sh\necho hi", true).unwrap();
        assert_eq!(mode("hook"), 0o755);
        home.write("hook", b"#!/bin/sh\necho again", false).unwrap();
        assert_eq!(mode("hook"), 0o644, "the exec bit isn't kept from before");
        // Private writes are 0600, even over a wide-open file.
        std::fs::write(dir.path().join("saved"), b"old").unwrap();
        std::fs::set_permissions(
            dir.path().join("saved"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        home.write_private("saved", b"backup").unwrap();
        assert_eq!(mode("saved"), 0o600);
        assert_eq!(home.read("saved").unwrap().unwrap(), b"backup");
        home.write_private("deep/er/saved", b"backup").unwrap();
        assert_eq!(mode("deep/er/saved"), 0o600);
    }

    #[test]
    fn refuses_folders_and_oversized_files() {
        let (dir, home) = home();
        std::fs::create_dir_all(dir.path().join("afolder")).unwrap();
        assert!(matches!(home.read("afolder"), Err(ApplyError::NotAFile(_))));
        assert!(matches!(
            home.write("afolder", b"x", false),
            Err(ApplyError::NotAFile(_))
        ));
        let big = vec![b'a'; MAX_FILE_SIZE as usize + 1];
        assert!(matches!(
            home.write("big", &big, false),
            Err(ApplyError::TooBig(_))
        ));
        std::fs::write(dir.path().join("big"), &big).unwrap();
        assert!(matches!(home.read("big"), Err(ApplyError::TooBig(_))));
    }
}
