//! Kernel confinement for the processes hx starts (`sh`, local MCP servers, autoresearch evaluators): a Landlock
//! ruleset built from the run's policy limits which files a child may read and write and, where the kernel offers
//! scopes, keeps it from signalling processes or reaching abstract Unix sockets outside its own domain. Network access
//! is left to the environment. Without Landlock (an old kernel, or a container that filters the syscalls) children run
//! unconfined, and the run says so once.
use std::path::{Path, PathBuf};

/// What a child may touch. `hidden` paths are carved out of the read roots that contain them.
#[derive(Debug, Default)]
pub struct Rules {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub hidden: Vec<PathBuf>,
}

/// Read-only for every child, in addition to the policy's read roots: shared libraries, process and system
/// information, the resolver configuration and the harmless character devices. A confined child cannot reach hx's own
/// `/proc` entries (Landlock refuses ptrace-style access to a process outside its domain), so `/proc` does not expose
/// hx's environment. No terminal is listed: children run in their own session with stdin closed, and access to
/// `/dev/tty` or `/dev/pts` would let one paint over an approval prompt or read the owner's keystrokes.
const CHILD_READ: &[&str] = &["/lib", "/lib32", "/lib64", "/libx32", "/proc", "/sys", "/run/systemd/resolve",
    "/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/shm"];
/// Devices every child may open for writing (`/dev/shm` holds POSIX shared memory and semaphores).
const CHILD_WRITE: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/shm"];

/// The kernel's Landlock ABI version, or why confinement is unavailable. Probed once per process.
pub fn status() -> Result<i32, &'static str> {
    static STATUS: std::sync::OnceLock<Result<i32, &'static str>> = std::sync::OnceLock::new();
    *STATUS.get_or_init(probe)
}

fn probe() -> Result<i32, &'static str> {
    const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
    // The check suites use this to exercise the fallback on a kernel that has Landlock.
    if std::env::var_os("HX_TEST_NO_LANDLOCK").is_some() { return Err("disabled by HX_TEST_NO_LANDLOCK"); }
    // SAFETY: with a null attribute pointer, size 0 and the VERSION flag the kernel only reports its ABI version.
    let abi = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, LANDLOCK_CREATE_RULESET_VERSION) };
    if abi >= 1 { Ok(abi as i32) } else { Err("the kernel does not offer Landlock (or a container filters it)") }
}

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
pub struct Ruleset(std::os::fd::OwnedFd);

impl Rules {
    /// Add the system paths every child needs: libraries, `/proc`, `/sys`, the resolver configuration (read) and the
    /// shared devices such as `/dev/null` (write).
    pub fn with_system(mut self) -> Self {
        self.read.extend(CHILD_READ.iter().map(PathBuf::from));
        self.write.extend(CHILD_WRITE.iter().map(PathBuf::from));
        self
    }

    /// Build the ruleset now, in the parent. None when Landlock is unavailable; then the child runs unconfined.
    pub fn prepare(&self) -> std::io::Result<Option<Ruleset>> {
        use landlock::{Access, AccessFs, PathBeneath, PathFd, Ruleset as Builder, RulesetAttr, RulesetCreatedAttr, Scope, ABI};
        let Ok(version) = status() else { return Ok(None) };
        // Everything the running kernel can restrict: truncation (ABI 3), device ioctls (5), signals and abstract Unix
        // sockets to processes outside the child's domain (6), pathname Unix sockets (9). The crate drops what an
        // older kernel lacks.
        let abi = ABI::from(version);
        let (read, write) = (AccessFs::from_read(abi), AccessFs::from_write(abi));
        let mut full = vec![];
        let mut list = vec![];
        let hidden: Vec<PathBuf> = self.hidden.iter().filter_map(|h| std::fs::canonicalize(h).ok()).collect();
        for root in self.read.iter().filter_map(|r| std::fs::canonicalize(r).ok()) {
            carve(&root, &hidden, &mut full, &mut list);
        }
        let err = |e: landlock::RulesetError| std::io::Error::other(e.to_string());
        let mut builder = Builder::default().handle_access(AccessFs::from_all(abi)).map_err(err)?;
        let scopes = Scope::from_all(abi);
        if !scopes.is_empty() { builder = builder.scope(scopes).map_err(err)?; }
        let mut ruleset = builder.create().map_err(err)?;
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
    pub fn apply(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: both calls take only integer arguments; the ruleset fd stays open for as long as `self` lives.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 { return Err(std::io::Error::last_os_error()); }
            if libc::syscall(libc::SYS_landlock_restrict_self, self.0.as_raw_fd(), 0) != 0 { return Err(std::io::Error::last_os_error()); }
        }
        Ok(())
    }
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
            // SAFETY: `apply` is async-signal-safe (two syscalls on an fd prepared before the fork).
            unsafe { cmd.pre_exec(move || r.apply()); }
            cmd.output().unwrap()
        };
        assert_eq!(run("README.md").stdout, b"PUBLIC");
        let secret = run(".harness/secrets.toml");
        assert!(!secret.status.success() && secret.stdout.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Run `script` with bash under `rules`; returns (exit code, stdout, stderr).
    fn confined(rules: Rules, script: &str) -> (Option<i32>, String, String) {
        use std::os::unix::process::CommandExt;
        let ruleset = std::sync::Arc::new(rules.prepare().unwrap().expect("Landlock is available"));
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg(script).stdin(std::process::Stdio::null());
        // SAFETY: `apply` is async-signal-safe (two syscalls on an fd prepared before the fork).
        unsafe { cmd.pre_exec(move || ruleset.apply()); }
        let out = cmd.output().unwrap();
        (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn system() -> Rules {
        Rules { read: vec!["/usr".into(), "/bin".into(), "/etc".into()], write: vec![], hidden: vec![] }.with_system()
    }

    #[test]
    fn a_confined_child_gets_the_harmless_devices_and_no_terminal() {
        if status().is_err() { eprintln!("skipped: Landlock unavailable"); return; }
        let (code, out, _) = confined(system(), "echo x > /dev/null && head -c 4 /dev/urandom | wc -c && head -c 2 /dev/zero | wc -c");
        assert_eq!((code, out.split_whitespace().collect::<Vec<_>>()), (Some(0), vec!["4", "2"]));
        for probe in ["exec 3>/dev/tty", "exec 3</dev/tty", "ls /dev/pts", "exec 3</dev/ptmx", "ls /dev"] {
            let (code, _, err) = confined(system(), probe);
            assert!(code != Some(0) && err.contains("Permission denied"), "{probe}: {code:?} {err}");
        }
    }

    #[test]
    fn a_confined_child_cannot_signal_or_reach_processes_outside_its_domain() {
        let Ok(abi) = status() else { eprintln!("skipped: Landlock unavailable"); return };
        if abi < 6 { eprintln!("skipped: Landlock ABI {abi} has no scopes"); return; }
        let mut victim = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let (code, _, err) = confined(system(), &format!("kill -TERM {}", victim.id()));
        assert!(code != Some(0) && err.contains("not permitted"), "{code:?} {err}");
        assert!(victim.try_wait().unwrap().is_none(), "the unrelated process was killed");
        victim.kill().unwrap();
        victim.wait().unwrap();
        // Inside its own domain a child still manages its own processes.
        let (code, _, err) = confined(system(), "sleep 30 & kill $!; wait $!; [ $? -eq 143 ]");
        assert_eq!(code, Some(0), "{err}");
        use std::os::linux::net::SocketAddrExt;
        let name = format!("hx-confine-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        let script = format!("python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect(\"\\0{name}\")'");
        let (code, _, err) = confined(system(), &script);
        assert!(code != Some(0) && err.contains("PermissionError"), "{code:?} {err}");
    }

}
