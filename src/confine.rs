//! Kernel confinement for the processes hx starts (`sh`, local MCP servers, autoresearch evaluators): on Linux, a
//! Landlock ruleset built from the run's policy limits which files a child may read and write. Network access is left
//! to the environment. Without Landlock (an old kernel, or a container that filters the syscalls) children run as
//! before, and the run says so once.
use std::path::{Path, PathBuf};

/// What a child may touch. `hidden` paths are carved out of the read roots that contain them.
#[derive(Debug, Default)]
pub struct Rules {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub hidden: Vec<PathBuf>,
}

/// Read-only for every child, in addition to the policy's read roots: shared libraries, process and device
/// information, the resolver configuration. A confined child cannot reach hx's own `/proc` entries (Landlock refuses
/// ptrace-style access to a process outside its domain), so `/proc` does not expose hx's environment.
const CHILD_READ: &[&str] = &["/lib", "/lib32", "/lib64", "/libx32", "/proc", "/sys", "/run/systemd/resolve"];
/// Devices every child may open for writing.
const CHILD_WRITE: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/tty", "/dev/pts", "/dev/shm"];

/// The kernel's Landlock ABI version, or why confinement is unavailable. Probed once per process.
pub fn status() -> Result<i32, &'static str> {
    static STATUS: std::sync::OnceLock<Result<i32, &'static str>> = std::sync::OnceLock::new();
    *STATUS.get_or_init(probe)
}

#[cfg(target_os = "linux")]
fn probe() -> Result<i32, &'static str> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // The check suites use this to exercise the fallback on a kernel that has Landlock.
    if std::env::var_os("HX_TEST_NO_LANDLOCK").is_some() { return Err("disabled by HX_TEST_NO_LANDLOCK"); }
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, LANDLOCK_CREATE_RULESET_VERSION) };
    if abi >= 1 { Ok(abi as i32) } else { Err("the kernel does not offer Landlock (or a container filters it)") }
}

#[cfg(not(target_os = "linux"))]
fn probe() -> Result<i32, &'static str> { Err("Landlock is Linux only") }

/// A one-line description of confinement for the session record and the run's stderr note.
pub fn describe() -> String {
    match status() {
        Ok(abi) => format!("landlock abi {abi}"),
        Err(why) => format!("off: {why}"),
    }
}

/// One stderr line at the start of a run whose children will run without kernel confinement.
pub fn note() {
    if let Err(why) = status() {
        eprintln!("hx: note: shell commands, MCP servers and evaluators run unconfined ({why}); isolate hx in your own sandbox");
    }
}

/// A prepared ruleset, applied to a child between fork and exec (`apply`).
pub struct Ruleset(#[cfg(target_os = "linux")] std::os::fd::OwnedFd);

impl Rules {
    /// Add the system paths every child needs: libraries, `/proc`, `/sys`, the resolver configuration (read) and the
    /// shared devices such as `/dev/null` (write).
    pub fn with_system(mut self) -> Self {
        self.read.extend(CHILD_READ.iter().map(PathBuf::from));
        self.write.extend(CHILD_WRITE.iter().map(PathBuf::from));
        self
    }

    /// Build the ruleset now, in the parent. None when Landlock is unavailable; then the child runs unconfined.
    #[cfg(target_os = "linux")]
    pub fn prepare(&self) -> std::io::Result<Option<Ruleset>> {
        use landlock::{Access, AccessFs, PathBeneath, PathFd, Ruleset as Builder, RulesetAttr, RulesetCreatedAttr, ABI};
        if status().is_err() { return Ok(None); }
        // ABI 3 covers reading, writing, renaming across directories and truncation; older kernels get what they have.
        let abi = ABI::V3;
        let (read, write) = (AccessFs::from_read(abi), AccessFs::from_write(abi));
        let mut full = vec![];
        let mut list = vec![];
        let hidden: Vec<PathBuf> = self.hidden.iter().filter_map(|h| std::fs::canonicalize(h).ok()).collect();
        for root in self.read.iter().filter_map(|r| std::fs::canonicalize(r).ok()) {
            carve(&root, &hidden, &mut full, &mut list);
        }
        let err = |e: landlock::RulesetError| std::io::Error::other(e.to_string());
        let mut ruleset = Builder::default().handle_access(AccessFs::from_all(abi)).map_err(err)?.create().map_err(err)?;
        let mut add = |path: &Path, access| -> std::io::Result<()> {
            // A path that does not exist (yet) grants nothing.
            if let Ok(fd) = PathFd::new(path) { ruleset.as_mut().add_rule(PathBeneath::new(fd, access)).map_err(err)?; }
            Ok(())
        };
        for p in &full { add(p, read)?; }
        for p in &list { add(p, AccessFs::ReadDir.into())?; }
        // Write rules add no read access: a hidden file inside a write root stays unreadable. (Moving it to a readable
        // place is refused too: Landlock never lets a rename or link gain access.)
        for p in &self.write { add(p, write)?; }
        let fd: Option<std::os::fd::OwnedFd> = ruleset.into();
        Ok(fd.map(Ruleset))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn prepare(&self) -> std::io::Result<Option<Ruleset>> { Ok(None) }
}

/// Grant `root` for reading, except `hidden` paths beneath it. Landlock only adds access, so a directory on the way
/// to a hidden path gets listing access only and each of its other entries is granted separately; an entry created in
/// such a directory after the ruleset was built is readable from the next command on. Symlinked entries are skipped:
/// granting one would grant its target.
fn carve(root: &Path, hidden: &[PathBuf], full: &mut Vec<PathBuf>, list: &mut Vec<PathBuf>) {
    if hidden.iter().any(|h| root.starts_with(h)) { return; }
    if !hidden.iter().any(|h| h.starts_with(root)) {
        full.push(root.to_path_buf());
        return;
    }
    list.push(root.to_path_buf());
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| !t.is_symlink()) { carve(&entry.path(), hidden, full, list); }
    }
}

impl Ruleset {
    /// Restrict the calling process. Async-signal-safe: call it in `pre_exec`, between fork and exec.
    #[cfg(target_os = "linux")]
    pub fn apply(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 { return Err(std::io::Error::last_os_error()); }
            if libc::syscall(libc::SYS_landlock_restrict_self, self.0.as_raw_fd(), 0) != 0 { return Err(std::io::Error::last_os_error()); }
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn apply(&self) -> std::io::Result<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carving_grants_siblings_and_lists_ancestors() {
        let dir = std::env::temp_dir().join(format!("hx-carve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".harness/log")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".harness/secrets.toml"), "k").unwrap();
        std::fs::write(dir.join("README.md"), "r").unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("link")).unwrap();
        let (mut full, mut list) = (vec![], vec![]);
        carve(&dir, &[dir.join(".harness/secrets.toml")], &mut full, &mut list);
        full.sort();
        assert_eq!(full, vec![dir.join(".harness/log"), dir.join("README.md"), dir.join("src")]);
        list.sort();
        assert_eq!(list, vec![dir.clone(), dir.join(".harness")]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_confined_child_cannot_read_a_hidden_file() {
        if status().is_err() { eprintln!("skipped: Landlock unavailable"); return; }
        let dir = std::env::temp_dir().join(format!("hx-confine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".harness")).unwrap();
        std::fs::write(dir.join(".harness/secrets.toml"), "SECRET").unwrap();
        std::fs::write(dir.join("README.md"), "PUBLIC").unwrap();
        let rules = Rules { read: vec![dir.clone(), "/usr".into(), "/bin".into(), "/etc".into()], write: vec![], hidden: vec![dir.join(".harness/secrets.toml")] }.with_system();
        let ruleset = std::sync::Arc::new(rules.prepare().unwrap().unwrap());
        let run = |file: &str| {
            let r = ruleset.clone();
            let mut cmd = std::process::Command::new("cat");
            cmd.arg(dir.join(file));
            use std::os::unix::process::CommandExt;
            unsafe { cmd.pre_exec(move || r.apply()); }
            cmd.output().unwrap()
        };
        assert_eq!(run("README.md").stdout, b"PUBLIC");
        let secret = run(".harness/secrets.toml");
        assert!(!secret.status.success() && secret.stdout.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
