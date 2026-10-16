//! File replacement anchored to directory descriptors, never a checked path reopened later.
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    },
    path::{Component, Path},
};

fn cstr(s: &std::ffi::OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in file path"))
}

// All pointers passed to libc below are NUL-terminated CStrings held across the call;
// directory descriptors are owned Files, and each successful open transfers ownership once.
fn opened(fd: libc::c_int) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // The successful open returned a new descriptor owned by this File.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn create_temp(dir: &File, name: &CString) -> io::Result<File> {
    opened(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666,
        )
    })
}

pub struct Target {
    dir: File,
    name: CString,
}

impl Target {
    /// `path` must be the absolute, policy-resolved path. Do not canonicalize it again:
    /// every directory is opened without following links, including newly created parents.
    pub fn open(path: &Path, create_parents: bool) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected absolute path",
            ));
        }
        let name =
            cstr(path.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "expected file name")
            })?)?;
        let mut dir = File::open("/")?;
        for component in path.parent().unwrap().components() {
            let Component::Normal(part) = component else {
                if component == Component::RootDir {
                    continue;
                }
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected normalized path",
                ));
            };
            let part = cstr(part)?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let mut next = opened(unsafe { libc::openat(dir.as_raw_fd(), part.as_ptr(), flags) });
            if create_parents
                && next
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                let made = unsafe { libc::mkdirat(dir.as_raw_fd(), part.as_ptr(), 0o755) };
                if made < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                    return Err(io::Error::last_os_error());
                }
                next = opened(unsafe { libc::openat(dir.as_raw_fd(), part.as_ptr(), flags) });
            }
            dir = next?;
        }
        Ok(Self { dir, name })
    }

    fn existing(&self) -> io::Result<File> {
        opened(unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                self.name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        })
    }

    pub fn read_to_string(&self) -> io::Result<String> {
        let mut file = self.existing()?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    }

    pub fn replace(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let permissions = match self.existing() {
            Ok(file) => {
                let meta = file.metadata()?;
                anyhow::ensure!(meta.is_file(), "not a regular file");
                Some(meta.permissions())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (tmp, mut file) = loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = CString::new(format!(".hx-tmp-{}-{n}", std::process::id())).unwrap();
            let result = create_temp(&self.dir, &tmp);
            match result {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        let result = (|| -> io::Result<()> {
            file.write_all(bytes)?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            file.sync_all()?;
            if unsafe {
                libc::renameat(
                    self.dir.as_raw_fd(),
                    tmp.as_ptr(),
                    self.dir.as_raw_fd(),
                    self.name.as_ptr(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        if result.is_err() {
            unsafe {
                libc::unlinkat(self.dir.as_raw_fd(), tmp.as_ptr(), 0);
            }
        }
        result?;
        // The replacement has happened; a failed directory sync must never be reported as a definite non-effect.
        self.dir.sync_all().map_err(|e| {
            crate::effects::UnknownOutcome(format!("replacement directory sync failed: {e}"))
        })?;
        Ok(())
    }
}

/// Trusted journal paths may contain platform aliases such as macOS /var -> /private/var.
pub fn replace_trusted(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = std::fs::canonicalize(path.parent().unwrap())?;
    Target::open(&parent.join(path.file_name().unwrap()), false)?.replace(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn workspace(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-atomic-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn temporary_creation_never_reuses_an_existing_entry() {
        let dir = workspace("exclusive");
        std::fs::write(dir.join("original"), "unchanged").unwrap();
        symlink("original", dir.join("temp")).unwrap();
        let parent = File::open(&dir).unwrap();
        let name = CString::new("temp").unwrap();
        assert_eq!(
            create_temp(&parent, &name).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("original")).unwrap(),
            "unchanged"
        );
        std::fs::remove_file(dir.join("temp")).unwrap();
        create_temp(&parent, &name).unwrap();
        assert_eq!(
            create_temp(&parent, &name).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacement_creates_parents_and_preserves_existing_permissions() {
        let dir = workspace("replace");
        let path = dir.join("nested/file");
        let target = Target::open(&path, true).unwrap();
        target.replace(b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        target.replace(b"new").unwrap();
        assert_eq!(target.read_to_string().unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replacement_stays_in_the_opened_directory_when_its_path_changes() {
        let dir = workspace("pinned");
        std::fs::create_dir(dir.join("parent")).unwrap();
        let target = Target::open(&dir.join("parent/file"), false).unwrap();
        std::fs::rename(dir.join("parent"), dir.join("original")).unwrap();
        std::fs::create_dir(dir.join("parent")).unwrap();
        std::fs::write(dir.join("parent/file"), "other").unwrap();
        target.replace(b"expected").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("original/file")).unwrap(),
            "expected"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("parent/file")).unwrap(),
            "other"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn links_at_the_destination_or_in_parents_are_refused() {
        let dir = workspace("links");
        std::fs::write(dir.join("original"), "unchanged").unwrap();
        symlink("original", dir.join("link")).unwrap();
        assert!(
            Target::open(&dir.join("link"), false)
                .unwrap()
                .replace(b"new")
                .is_err()
        );
        symlink(&dir, dir.join("alias")).unwrap();
        assert!(Target::open(&dir.join("alias/file"), true).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("original")).unwrap(),
            "unchanged"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
