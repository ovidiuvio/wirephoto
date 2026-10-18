//! A library's files, reached only through a capability handle on its root (`cap-std`): every path is resolved
//! beneath the root by the operating system (`openat2(RESOLVE_BENEATH)` on Linux), so `..` and symlinks that
//! lead outside fail by construction.
//!
//! Paths here are relative to the root, `/`-separated, without a leading slash; the root itself is `""`.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use cap_std::fs::{Dir, MetadataExt, OpenOptions};

/// What wirephoto needs to know about a file or folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub is_dir: bool,
    pub is_file: bool,
    pub size: u64,
    pub mtime_ns: i64,
    pub dev: u64,
    pub ino: u64,
}

pub struct LibFs {
    pub root: PathBuf,
    dir: Dir,
}

/// Where trashed entries go, inside the library (a dotfolder, so indexing skips it).
pub const TRASH_DIR: &str = ".wirephoto-trash";

fn p(rel: &str) -> &Path {
    Path::new(if rel.is_empty() { "." } else { rel })
}

pub fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

pub fn parent_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("")
}

pub fn name_of(rel: &str) -> &str {
    rel.rsplit_once('/').map(|(_, n)| n).unwrap_or(rel)
}

/// A name an app may give a file or folder: one path component, not hidden, not reserved.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
}

fn stat_of(m: &cap_std::fs::Metadata) -> Stat {
    let mtime_ns = m
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(cap_std::time::SystemTime::from_std(std::time::UNIX_EPOCH))
                .ok()
        })
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    Stat {
        is_dir: m.is_dir(),
        is_file: m.is_file(),
        size: if m.is_file() { m.len() } else { 0 },
        mtime_ns,
        dev: m.dev(),
        ino: m.ino(),
    }
}

impl LibFs {
    pub fn open(root: &Path) -> io::Result<LibFs> {
        let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        Ok(LibFs {
            root: root.to_path_buf(),
            dir,
        })
    }

    /// The stat of an entry, without following a final symlink. `None` for symlinks and special files.
    pub fn stat(&self, rel: &str) -> io::Result<Option<Stat>> {
        let m = self.dir.symlink_metadata(p(rel))?;
        if m.file_type().is_symlink() || !(m.is_dir() || m.is_file()) {
            return Ok(None);
        }
        Ok(Some(stat_of(&m)))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.dir.symlink_metadata(p(rel)).is_ok()
    }

    /// A folder's children: (name, stat), skipping symlinks and special files.
    pub fn list(&self, rel: &str) -> io::Result<Vec<(String, Stat)>> {
        let mut out = Vec::new();
        for e in self.dir.read_dir(p(rel))? {
            let e = e?;
            let Ok(name) = e.file_name().into_string() else {
                continue;
            }; // not UTF-8: not addressable
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() || !(ft.is_dir() || ft.is_file()) {
                continue;
            }
            match e.metadata() {
                Ok(m) => out.push((name, stat_of(&m))),
                Err(_) => continue, // vanished meanwhile
            }
        }
        Ok(out)
    }

    pub fn open_file(&self, rel: &str) -> io::Result<std::fs::File> {
        Ok(self.dir.open(p(rel))?.into_std())
    }

    /// Up to `n` bytes from the start of a file.
    pub fn read_head(&self, rel: &str, n: usize) -> io::Result<Vec<u8>> {
        let mut f = self.open_file(rel)?;
        let mut buf = Vec::with_capacity(n);
        (&mut f).take(n as u64).read_to_end(&mut buf)?;
        Ok(buf)
    }

    pub fn read_all(&self, rel: &str) -> io::Result<Vec<u8>> {
        let mut f = self.open_file(rel)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    }

    /// A new temporary file next to `final_rel`, for an atomic write.
    pub fn create_temp(&self, final_rel: &str) -> io::Result<(String, std::fs::File)> {
        let dir = parent_of(final_rel);
        let tmp = join(dir, &format!(".wp-{}.tmp", crate::util::random_id("")));
        let f = self
            .dir
            .open_with(p(&tmp), OpenOptions::new().write(true).create_new(true))?;
        Ok((tmp, f.into_std()))
    }

    /// Makes a finished temporary file visible as `final_rel`, failing with `AlreadyExists` if that name
    /// exists, in which case the temporary file is left for the caller to try another name or remove. Never
    /// replaces: a hard link where the filesystem has them, else a rename that refuses to replace
    /// ([`Self::rename_new`]).
    pub fn publish_new(&self, tmp: &str, final_rel: &str) -> io::Result<()> {
        match self.dir.hard_link(p(tmp), &self.dir, p(final_rel)) {
            Ok(()) => {
                let _ = self.dir.remove_file(p(tmp));
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
            Err(_) => self.rename_new(tmp, final_rel),
        }
    }

    pub fn remove_file(&self, rel: &str) -> io::Result<()> {
        self.dir.remove_file(p(rel))
    }

    pub fn remove_all(&self, rel: &str) -> io::Result<()> {
        let m = self.dir.symlink_metadata(p(rel))?;
        if m.is_dir() {
            self.dir.remove_dir_all(p(rel))
        } else {
            self.dir.remove_file(p(rel))
        }
    }

    pub fn create_dir(&self, rel: &str) -> io::Result<()> {
        self.dir.create_dir(p(rel))
    }

    pub fn create_dir_all(&self, rel: &str) -> io::Result<()> {
        self.dir.create_dir_all(p(rel))
    }

    /// Renames within the library, failing with `AlreadyExists` if the target exists. The filesystem itself
    /// refuses to replace (`renameat2(RENAME_NOREPLACE)` on Linux, `renameatx_np(RENAME_EXCL)` on macOS), so a
    /// file arriving at `to` meanwhile is never overwritten. The parents are opened through the confined root
    /// and only the final names are passed. Where the filesystem cannot (some network mounts), it checks first.
    pub fn rename_new(&self, from: &str, to: &str) -> io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::fs::{RenameFlags, renameat_with};
            use rustix::io::Errno;
            let from_dir = self.dir.open_dir(p(parent_of(from)))?;
            let to_dir = self.dir.open_dir(p(parent_of(to)))?;
            match renameat_with(
                &from_dir,
                name_of(from),
                &to_dir,
                name_of(to),
                RenameFlags::NOREPLACE,
            ) {
                Ok(()) => return Ok(()),
                Err(Errno::EXIST) => return Err(io::ErrorKind::AlreadyExists.into()),
                Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP | Errno::OPNOTSUPP) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if self.exists(to) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        self.dir.rename(p(from), &self.dir, p(to))
    }

    /// Copies a file or a folder tree to `to` in `dest` (possibly another library). Same-filesystem copies use
    /// the platform's fastest path (clones/reflinks where the filesystem has them).
    pub fn copy_to(&self, from: &str, dest: &LibFs, to: &str) -> io::Result<()> {
        if dest.exists(to) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let m = self.dir.symlink_metadata(p(from))?;
        if m.is_dir() {
            dest.dir.create_dir(p(to))?;
            for (name, st) in self.list(from)? {
                if name.starts_with('.') && !st.is_file && !st.is_dir {
                    continue;
                }
                self.copy_to(&join(from, &name), dest, &join(to, &name))?;
            }
            Ok(())
        } else {
            // Into a temporary file, then published without replacing anything.
            let (tmp, file) = dest.create_temp(to)?;
            drop(file);
            let res = self
                .dir
                .copy(p(from), &dest.dir, p(&tmp))
                .and_then(|_| dest.publish_new(&tmp, to));
            if res.is_err() {
                let _ = dest.dir.remove_file(p(&tmp));
            }
            res
        }
    }

    pub fn fsync_dir(&self, rel: &str) {
        if let Ok(d) = self.dir.open(p(rel)) {
            let _ = d.into_std().sync_all();
        }
    }
}

/// Writes all of `data` and flushes it to disk.
pub fn write_all_sync(f: &mut std::fs::File, data: &[u8]) -> io::Result<()> {
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_and_publishes_never_replace() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("lib");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/x"), b"x").unwrap();
        std::fs::write(root.join("a/y"), b"y").unwrap();
        let fs = LibFs::open(&root).unwrap();
        let e = fs.rename_new("a/x", "a/y").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(root.join("a/y")).unwrap(), b"y");
        fs.rename_new("a/x", "z").unwrap();
        assert_eq!(std::fs::read(root.join("z")).unwrap(), b"x");
        let (tmp, mut f) = fs.create_temp("a/y").unwrap();
        write_all_sync(&mut f, b"new").unwrap();
        assert_eq!(
            fs.publish_new(&tmp, "a/y").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(fs.exists(&tmp), "kept for another name");
        fs.publish_new(&tmp, "a/y-1").unwrap();
        assert_eq!(std::fs::read(root.join("a/y")).unwrap(), b"y");
        assert_eq!(std::fs::read(root.join("a/y-1")).unwrap(), b"new");
        assert_eq!(
            fs.copy_to("a/y-1", &fs, "a/y").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        fs.copy_to("a/y-1", &fs, "a/y-2").unwrap();
        assert_eq!(std::fs::read(root.join("a/y-2")).unwrap(), b"new");
    }

    #[test]
    fn names() {
        assert!(valid_name("DSC01234.ARW"));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(".."));
        assert!(!valid_name(""));
    }

    #[test]
    fn confinement() {
        let t = tempfile::tempdir().unwrap();
        let lib = t.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::fs::write(t.path().join("secret"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.path().join("secret"), lib.join("link")).unwrap();
        let fs = LibFs::open(&lib).unwrap();
        assert!(fs.open_file("../secret").is_err());
        #[cfg(unix)]
        {
            assert!(fs.open_file("link").is_err());
            assert!(fs.list("").unwrap().is_empty(), "symlinks are not listed");
        }
    }
}
