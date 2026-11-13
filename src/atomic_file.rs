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
        let dir = Dir::open(path.parent().unwrap(), create_parents)?.0;
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

    /// Unlink the target name itself (a link is removed, never followed).
    pub fn remove(&self) -> io::Result<()> {
        // SAFETY: `dir` is an open directory descriptor and `name` a NUL-terminated string; both outlive the call.
        if unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The target's whole content, opened through the pinned directory without following a link. Refuses a directory
    /// (`IsADirectory`), anything else that is not a regular file (a FIFO would block, a device may never end;
    /// `InvalidInput`) and content over `max` bytes (`FileTooLarge`), reading at most `max + 1` bytes either way.
    pub fn read(&self, max: u64) -> io::Result<Vec<u8>> {
        let file = self.existing()?;
        let meta = file.metadata()?;
        if meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "is a directory",
            ));
        }
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let too_large = |len: u64| {
            io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("file is {len} bytes, over the {max}-byte limit"),
            )
        };
        if meta.len() > max {
            return Err(too_large(meta.len()));
        }
        let mut bytes = Vec::new();
        file.take(max + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > max {
            return Err(too_large(bytes.len() as u64));
        }
        Ok(bytes)
    }

    /// The SHA-256 of the file's current content, or None when there is no file. A link or non-regular file is an error.
    pub fn sha256(&self) -> io::Result<Option<String>> {
        use sha2::Digest;
        let mut file = match self.existing() {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        let mut hasher = sha2::Sha256::new();
        let mut chunk = [0u8; 65536];
        loop {
            match file.read(&mut chunk)? {
                0 => break,
                n => hasher.update(&chunk[..n]),
            }
        }
        Ok(Some(
            hasher
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        ))
    }

    /// The permissions a replacement keeps: the existing file's, or None when there is none.
    fn permissions(&self) -> anyhow::Result<Option<std::fs::Permissions>> {
        match self.existing() {
            Ok(file) => {
                let meta = file.metadata()?;
                anyhow::ensure!(meta.is_file(), "not a regular file");
                Ok(Some(meta.permissions()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Replace the file's content through a temporary file with a fresh name.
    pub fn replace(&self, bytes: &[u8]) -> anyhow::Result<()> {
        let permissions = self.permissions()?;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let (tmp, file) = loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = CString::new(format!(".hx-tmp-{}-{n}", std::process::id())).unwrap();
            let result = create_temp(&self.dir, &tmp);
            match result {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        };
        self.commit(&tmp, file, bytes, permissions)
    }

    /// Replace the file's content through a temporary file named `tmp` in the same directory, which must not exist.
    /// The caller journals `tmp` first, so a crash before the rename leaves a file recovery knows to remove.
    pub fn replace_through(&self, tmp: &str, bytes: &[u8]) -> anyhow::Result<()> {
        let permissions = self.permissions()?;
        let tmp = cstr(std::ffi::OsStr::new(tmp))?;
        let file = create_temp(&self.dir, &tmp)?;
        self.commit(&tmp, file, bytes, permissions)
    }

    fn commit(
        &self,
        tmp: &CString,
        mut file: File,
        bytes: &[u8],
        permissions: Option<std::fs::Permissions>,
    ) -> anyhow::Result<()> {
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

/// A directory reached without following a link in any component, and the operations owner commands need inside it.
/// Each operation names one entry of this directory and follows no link at that entry either, so a process that can
/// write the tree (a shell command writing `.harness/`) cannot redirect a write, rename or removal elsewhere by
/// swapping a directory for a link while the operation runs.
#[derive(Debug)]
pub struct Dir(File);

const DIR_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// One entry name: a single path component, never `.`, `..` or empty.
fn entry(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a single file name"),
        ));
    }
    cstr(std::ffi::OsStr::new(name))
}

impl Dir {
    /// Open the absolute, normalized `path`, every component without following a link; with `create`, missing
    /// directories are made on the way.
    pub fn open(path: &Path, create: bool) -> io::Result<Dir> {
        let mut dir = Dir(File::open("/")?);
        for component in path.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(part) => dir = dir.child(&cstr(part)?, create)?,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "expected normalized path",
                    ));
                }
            }
        }
        Ok(dir)
    }

    fn child(&self, name: &CString, create: bool) -> io::Result<Dir> {
        // SAFETY: `self.0` is an open directory descriptor and `name` a NUL-terminated string; both outlive the call.
        let mut next =
            opened(unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), DIR_FLAGS) });
        if create
            && next
                .as_ref()
                .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
        {
            // SAFETY: as above.
            if unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o755) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            // SAFETY: as above.
            next = opened(unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), DIR_FLAGS) });
        }
        Ok(Dir(next?))
    }

    /// The subdirectory `name`, created when `create` and missing. A link there is an error.
    pub fn dir(&self, name: &str, create: bool) -> io::Result<Dir> {
        self.child(&entry(name)?, create)
    }

    /// Create the subdirectory `name`, which must not exist (`AlreadyExists` otherwise), and open it.
    pub fn create_dir(&self, name: &str) -> io::Result<Dir> {
        let name = entry(name)?;
        // SAFETY: `self.0` is an open directory descriptor and `name` a NUL-terminated string; both outlive the call.
        if unsafe { libc::mkdirat(self.0.as_raw_fd(), name.as_ptr(), 0o755) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.child(&name, false)
    }

    /// Create the file `name`, which must not exist (`AlreadyExists` otherwise, a link included), with `bytes`.
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let mut file = create_temp(&self.0, &entry(name)?)?;
        file.write_all(bytes)?;
        file.sync_all()
    }

    /// Append `bytes` to the file `name`, creating it when missing. A link there is an error.
    pub fn append(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        let name = entry(name)?;
        let flags = libc::O_WRONLY
            | libc::O_APPEND
            | libc::O_CREAT
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC;
        // SAFETY: `self.0` is an open directory descriptor and `name` a NUL-terminated string; both outlive the call.
        let mut file =
            opened(unsafe { libc::openat(self.0.as_raw_fd(), name.as_ptr(), flags, 0o666) })?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        file.write_all(bytes)
    }

    /// Rename the entry `from` of this directory to `to` in this directory, replacing what `to` names as rename(2) does.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (from, to) = (entry(from)?, entry(to)?);
        // SAFETY: `self.0` is an open directory descriptor and both names NUL-terminated strings; all outlive the call.
        if unsafe {
            libc::renameat(
                self.0.as_raw_fd(),
                from.as_ptr(),
                self.0.as_raw_fd(),
                to.as_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The entry `name` as a file target (see `Target`): read, replaced or removed without following a link.
    pub fn file(&self, name: &str) -> io::Result<Target> {
        Ok(Target {
            dir: self.0.try_clone()?,
            name: entry(name)?,
        })
    }

    /// The names in this directory, sorted.
    pub fn names(&self) -> io::Result<Vec<String>> {
        let mut names = std::fs::read_dir(self.proc_path())?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    /// Remove the entry `name` and, when it is a directory, everything in it. A link is removed, never followed.
    pub fn remove_all(&self, name: &str) -> io::Result<()> {
        let entry = entry(name)?;
        // SAFETY: `self.0` is an open directory descriptor and `entry` a NUL-terminated string; both outlive the call.
        if unsafe { libc::unlinkat(self.0.as_raw_fd(), entry.as_ptr(), 0) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EISDIR) {
            return Err(error);
        }
        // `/proc/self/fd/N` names this very directory however its path changes, and std's remove_dir_all walks below
        // the final component without following links.
        std::fs::remove_dir_all(self.proc_path().join(name))
    }

    fn proc_path(&self) -> std::path::PathBuf {
        Path::new("/proc/self/fd").join(self.0.as_raw_fd().to_string())
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

    #[test]
    fn directory_operations_follow_no_link() {
        let dir = workspace("dir");
        std::fs::create_dir(dir.join("elsewhere")).unwrap();
        std::fs::write(dir.join("elsewhere/kept"), "kept").unwrap();
        symlink(dir.join("elsewhere"), dir.join("linked")).unwrap();
        assert!(Dir::open(&dir.join("linked"), false).is_err());
        let base = Dir::open(&dir, false).unwrap();
        assert!(base.dir("linked", true).is_err());
        assert_eq!(
            base.write_new("linked", b"x").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            base.create_dir("linked").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(base.append("linked", b"x").is_err());
        assert!(base.dir("../elsewhere", false).is_err() && base.write_new("..", b"x").is_err());
        // removal takes the link, not what it points to
        base.remove_all("linked").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("elsewhere/kept")).unwrap(),
            "kept"
        );
        let made = base.create_dir("made").unwrap();
        made.dir("sub", true)
            .unwrap()
            .write_new("file", b"data")
            .unwrap();
        assert_eq!(made.names().unwrap(), ["sub"]);
        base.rename("made", "moved").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("moved/sub/file")).unwrap(),
            "data"
        );
        base.remove_all("moved").unwrap();
        assert_eq!(base.names().unwrap(), ["elsewhere"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
