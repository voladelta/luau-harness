//! Kernel confinement for the processes hx starts (`sh`, local MCP servers, autoresearch evaluators): a Landlock
//! ruleset built from the run's policy limits which files a child may read and write and, where the kernel offers
//! scopes, keeps it from signalling processes or reaching abstract Unix sockets outside its own domain. Network access
//! is left to the environment. Without Landlock (an old kernel, or a container that filters the syscalls) children run
//! unconfined, and the run says so once.
use std::path::{Path, PathBuf};

/// What a child may touch: everything beneath each read root (read-only) and each write root (write only; a path a
/// child should read and write is in both lists).
#[derive(Debug, Default)]
pub struct Rules {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
}

/// Read-only for every child, in addition to the policy's read roots: shared libraries, process and system
/// information, the resolver configuration and the harmless character devices. A confined child cannot reach hx's own
/// `/proc` entries (Landlock refuses ptrace-style access to a process outside its domain), so `/proc` does not expose
/// hx's environment. No terminal is listed: children run in their own session with stdin closed, and access to
/// `/dev/tty` or `/dev/pts` would let one paint over an approval prompt or read the owner's keystrokes.
const CHILD_READ: &[&str] = &[
    "/lib",
    "/lib32",
    "/lib64",
    "/libx32",
    "/proc",
    "/sys",
    "/run/systemd/resolve",
    "/dev/null",
    "/dev/zero",
    "/dev/full",
    "/dev/random",
    "/dev/urandom",
    "/dev/shm",
];
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
    if std::env::var_os("HX_TEST_NO_LANDLOCK").is_some() {
        return Err("disabled by HX_TEST_NO_LANDLOCK");
    }
    // SAFETY: with a null attribute pointer, size 0 and the VERSION flag the kernel only reports its ABI version.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<u8>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if abi >= 1 {
        Ok(abi as i32)
    } else {
        Err("the kernel does not offer Landlock (or a container filters it)")
    }
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
        eprintln!(
            "hx: note: shell commands, MCP servers and evaluators run unconfined ({why}); isolate hx in your own sandbox"
        );
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
        use landlock::{
            ABI, Access, AccessFs, PathBeneath, PathFd, Ruleset as Builder, RulesetAttr,
            RulesetCreatedAttr, Scope,
        };
        let Ok(version) = status() else {
            return Ok(None);
        };
        // Everything the running kernel can restrict: truncation (ABI 3), device ioctls (5), signals and abstract Unix
        // sockets to processes outside the child's domain (6), pathname Unix sockets (9). The crate drops what an
        // older kernel lacks.
        let abi = ABI::from(version);
        let (read, write) = (AccessFs::from_read(abi), AccessFs::from_write(abi));
        let err = |e: landlock::RulesetError| std::io::Error::other(e.to_string());
        let mut builder = Builder::default()
            .handle_access(AccessFs::from_all(abi))
            .map_err(err)?;
        let scopes = Scope::from_all(abi);
        if !scopes.is_empty() {
            builder = builder.scope(scopes).map_err(err)?;
        }
        let mut ruleset = builder.create().map_err(err)?;
        let mut add = |path: &Path, access| -> std::io::Result<()> {
            // A path that does not exist (yet) grants nothing.
            if let Ok(fd) = PathFd::new(path) {
                ruleset
                    .as_mut()
                    .add_rule(PathBeneath::new(fd, access))
                    .map_err(err)?;
            }
            Ok(())
        };
        for p in &self.read {
            add(p, read)?;
        }
        for p in &self.write {
            add(p, write)?;
        }
        let fd: Option<std::os::fd::OwnedFd> = ruleset.into();
        Ok(fd.map(Ruleset))
    }
}

impl Ruleset {
    /// Restrict the calling process. Async-signal-safe: call it in `pre_exec`, between fork and exec.
    pub fn apply(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: both calls take only integer arguments; the ruleset fd stays open for as long as `self` lives.
        unsafe {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_landlock_restrict_self, self.0.as_raw_fd(), 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `script` with bash under `rules`; returns (exit code, stdout, stderr).
    fn confined(rules: Rules, script: &str) -> (Option<i32>, String, String) {
        use std::os::unix::process::CommandExt;
        let ruleset = std::sync::Arc::new(rules.prepare().unwrap().expect("Landlock is available"));
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c").arg(script).stdin(std::process::Stdio::null());
        // SAFETY: `apply` is async-signal-safe (two syscalls on an fd prepared before the fork).
        unsafe {
            cmd.pre_exec(move || ruleset.apply());
        }
        let out = cmd.output().unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    #[test]
    fn a_confined_child_reads_what_it_creates_and_nothing_outside_its_roots() {
        if status().is_err() {
            eprintln!("skipped: Landlock unavailable");
            return;
        }
        let dir = std::env::temp_dir().join(format!("hx-confine-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws")).unwrap();
        std::fs::create_dir_all(dir.join("owner")).unwrap();
        std::fs::write(dir.join("owner/secrets.toml"), "SECRET").unwrap();
        let ws = dir.join("ws");
        let rules = Rules {
            read: vec![ws.clone(), "/usr".into(), "/bin".into(), "/etc".into()],
            write: vec![ws.clone()],
        }
        .with_system();
        let script = format!(
            "cd {} && echo hi > new.txt && cat new.txt && mkdir -p d/e && echo x > d/e/f && cat d/e/f && cat ../owner/secrets.toml",
            ws.display()
        );
        let (code, out, err) = confined(rules, &script);
        assert_eq!(out, "hi\nx\n", "{err}");
        assert!(
            code != Some(0) && err.contains("Permission denied"),
            "{code:?} {err}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn system() -> Rules {
        Rules {
            read: vec!["/usr".into(), "/bin".into(), "/etc".into()],
            write: vec![],
        }
        .with_system()
    }

    #[test]
    fn a_confined_child_gets_the_harmless_devices_and_no_terminal() {
        if status().is_err() {
            eprintln!("skipped: Landlock unavailable");
            return;
        }
        let (code, out, _) = confined(
            system(),
            "echo x > /dev/null && head -c 4 /dev/urandom | wc -c && head -c 2 /dev/zero | wc -c",
        );
        assert_eq!(
            (code, out.split_whitespace().collect::<Vec<_>>()),
            (Some(0), vec!["4", "2"])
        );
        for probe in [
            "exec 3>/dev/tty",
            "exec 3</dev/tty",
            "ls /dev/pts",
            "exec 3</dev/ptmx",
            "ls /dev",
        ] {
            let (code, _, err) = confined(system(), probe);
            assert!(
                code != Some(0) && err.contains("Permission denied"),
                "{probe}: {code:?} {err}"
            );
        }
    }

    #[test]
    fn a_confined_child_cannot_signal_or_reach_processes_outside_its_domain() {
        let Ok(abi) = status() else {
            eprintln!("skipped: Landlock unavailable");
            return;
        };
        if abi < 6 {
            eprintln!("skipped: Landlock ABI {abi} has no scopes");
            return;
        }
        let mut victim = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (code, _, err) = confined(system(), &format!("kill -TERM {}", victim.id()));
        assert!(
            code != Some(0) && err.contains("not permitted"),
            "{code:?} {err}"
        );
        assert!(
            victim.try_wait().unwrap().is_none(),
            "the unrelated process was killed"
        );
        victim.kill().unwrap();
        victim.wait().unwrap();
        // Inside its own domain a child still manages its own processes.
        let (code, _, err) = confined(system(), "sleep 30 & kill $!; wait $!; [ $? -eq 143 ]");
        assert_eq!(code, Some(0), "{err}");
        use std::os::linux::net::SocketAddrExt;
        let name = format!("hx-confine-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        let script = format!(
            "python3 -c 'import socket; s = socket.socket(socket.AF_UNIX); s.connect(\"\\0{name}\")'"
        );
        let (code, _, err) = confined(system(), &script);
        assert!(
            code != Some(0) && err.contains("PermissionError"),
            "{code:?} {err}"
        );
    }
}
