//! Kernel confinement for the processes hx starts (`sh`, local MCP servers, autoresearch evaluators): on Linux, a
//! Landlock ruleset built from the run's policy limits which files a child may read and write. Network access is left
//! to the environment. Without Landlock (an old kernel, or a container that filters the syscalls) children run as
//! before, and the run says so once.
//!
//! Autoresearch children (evaluators and an episode's shell commands) are also isolated from each other and from every
//! later process (`Isolate`): candidate code runs next to grader and held-out data, and anything it leaves behind (a
//! daemon holding the data in its argv, a shared-memory segment, a socket) would carry that data to the next agent.
use std::ffi::CString;
use std::path::{Path, PathBuf};

/// What a child may touch. `hidden` paths are carved out of the read roots that contain them; `exposed` paths are
/// readable all the same (a run's own frozen skill files, which live in the hidden run store). `scoped` children (the
/// isolated ones) may also neither signal nor reach an abstract unix socket outside their Landlock domain (ABI 6).
#[derive(Debug, Default)]
pub struct Rules {
    pub read: Vec<PathBuf>,
    pub write: Vec<PathBuf>,
    pub hidden: Vec<PathBuf>,
    pub exposed: Vec<PathBuf>,
    pub scoped: bool,
}

/// Read-only for every child, in addition to the policy's read roots: shared libraries, process and device
/// information, the resolver configuration. A confined child cannot reach hx's own `/proc` entries (Landlock refuses
/// ptrace-style access to a process outside its domain), so `/proc` does not expose hx's environment; an isolated child
/// sees a `/proc` of its own.
const CHILD_READ: &[&str] = &["/lib", "/lib32", "/lib64", "/libx32", "/proc", "/sys", "/run/systemd/resolve"];
/// Devices every child may open for writing. `/dev/shm` is one directory for every process of the machine and outlives
/// them; ordinary children keep it, because POSIX shared memory and named semaphores (`shm_open`, `sem_open`, Python
/// multiprocessing locks, queues and pools, pytest-xdist) live only there. Isolated children get an empty private one
/// in their mount namespace; only under the cgroup fallback, which has no mount namespace, they go without it
/// (`without_shm`): there it would be a channel from candidate code running next to held-out data to a later shell.
const CHILD_WRITE: &[&str] = &["/dev/null", "/dev/zero", "/dev/full", "/dev/tty", "/dev/pts", SHM];
const SHM: &str = "/dev/shm";

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

/// A prepared ruleset, applied to a child between fork and exec (`apply`). `mounts`: the access to grant on the
/// private `/proc` and `/dev/shm` an isolated child mounts (read; read and write, or none for `/dev/shm`). Landlock
/// skips the mount point when it walks up from a mount, so the system's `/proc` granted in the parent does not cover the
/// child's: `apply` grants these on the child's own, after it mounted them.
pub struct Ruleset {
    #[cfg(target_os = "linux")]
    fd: std::os::fd::OwnedFd,
    mounts: Option<(u64, u64)>,
}

impl Rules {
    /// Add the system paths every child needs: libraries, `/proc`, `/sys`, the resolver configuration (read) and the
    /// shared devices such as `/dev/null` and `/dev/shm` (write).
    pub fn with_system(mut self) -> Self {
        self.read.extend(CHILD_READ.iter().map(PathBuf::from));
        self.write.extend(CHILD_WRITE.iter().map(PathBuf::from));
        self
    }

    /// Take `/dev/shm` away (neither readable nor writable), for children that may sit next to held-out data
    /// (`CHILD_WRITE`). Shared memory and named semaphores then fail with a permission error.
    pub fn without_shm(mut self) -> Self {
        self.write.retain(|p| p != Path::new(SHM));
        self.hidden.push(SHM.into());
        self
    }

    /// The rules of an isolated child (`Isolate`): scoped, and without the shared `/dev/shm` unless it gets a private
    /// one (namespaces).
    pub fn isolated(mut self) -> Self {
        self.scoped = true;
        if matches!(isolation(), Ok(Isolation::Namespaces)) { self } else { self.without_shm() }
    }

    /// Build the ruleset now, in the parent. None when Landlock is unavailable; then the child runs unconfined.
    #[cfg(target_os = "linux")]
    pub fn prepare(&self) -> std::io::Result<Option<Ruleset>> {
        use landlock::{Access, AccessFs, PathBeneath, PathFd, Ruleset as Builder, RulesetAttr, RulesetCreatedAttr, Scope, ABI};
        if status().is_err() { return Ok(None); }
        // ABI 3 covers reading, writing, renaming across directories and truncation; older kernels get what they have.
        let abi = match status() { Ok(1) => ABI::V1, Ok(2) => ABI::V2, _ => ABI::V3 };
        let (read, write) = (AccessFs::from_read(abi), AccessFs::from_write(abi));
        let mut full = vec![];
        let mut list = vec![];
        let hidden: Vec<PathBuf> = self.hidden.iter().filter_map(|h| std::fs::canonicalize(h).ok()).collect();
        for root in self.read.iter().filter_map(|r| std::fs::canonicalize(r).ok()) {
            carve(&root, &hidden, &mut full, &mut list);
        }
        let err = |e: landlock::RulesetError| std::io::Error::other(e.to_string());
        let mut builder = Builder::default().handle_access(AccessFs::from_all(abi)).map_err(err)?;
        // Signals and abstract unix sockets stay inside the child's own domain where the kernel can scope them (ABI 6).
        if self.scoped && status().is_ok_and(|v| v >= 6) { builder = builder.scope(Scope::AbstractUnixSocket | Scope::Signal).map_err(err)?; }
        let mut ruleset = builder.create().map_err(err)?;
        let mut add = |path: &Path, access| -> std::io::Result<()> {
            // A path that does not exist (yet) grants nothing.
            if let Ok(fd) = PathFd::new(path) { ruleset.as_mut().add_rule(PathBeneath::new(fd, access)).map_err(err)?; }
            Ok(())
        };
        for p in full.iter().chain(&self.exposed) { add(p, read)?; }
        for p in &list { add(p, AccessFs::ReadDir.into())?; }
        // Write rules add no read access: a hidden file inside a write root stays unreadable. (Moving it to a readable
        // place is refused too: Landlock never lets a rename or link gain access.)
        for p in &self.write { add(p, write)?; }
        let fd: Option<std::os::fd::OwnedFd> = ruleset.into();
        let shm = if self.write.iter().any(|w| w == Path::new(SHM)) { (read | write).bits() } else { 0 };
        let mounts = (self.scoped && matches!(isolation(), Ok(Isolation::Namespaces))).then_some((read.bits(), shm));
        Ok(fd.map(|fd| Ruleset { fd, mounts }))
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
        /// `struct landlock_path_beneath_attr`.
        #[repr(C, packed)]
        struct Beneath { allowed_access: u64, parent_fd: i32 }
        const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;
        unsafe {
            for (path, access) in self.mounts.iter().flat_map(|(proc, shm)| [(c"/proc", *proc), (c"/dev/shm", *shm)]) {
                let fd = libc::open(path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
                if access == 0 || fd < 0 { if fd >= 0 { libc::close(fd); } continue; }
                let attr = Beneath { allowed_access: access, parent_fd: fd };
                let added = libc::syscall(libc::SYS_landlock_add_rule, self.fd.as_raw_fd(), LANDLOCK_RULE_PATH_BENEATH, &attr as *const Beneath, 0);
                libc::close(fd);
                if added != 0 { return Err(std::io::Error::last_os_error()); }
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 { return Err(std::io::Error::last_os_error()); }
            if libc::syscall(libc::SYS_landlock_restrict_self, self.fd.as_raw_fd(), 0) != 0 { return Err(std::io::Error::last_os_error()); }
        }
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    pub fn apply(&self) -> std::io::Result<()> { Ok(()) }
}

/// How autoresearch children are kept apart (`isolation`).
#[derive(Clone, Debug, PartialEq)]
pub enum Isolation {
    /// Fresh user, pid, mount and IPC namespaces per child (and network, for evaluators): its own `/proc` and
    /// `/dev/shm`, no view of other processes, and when its first process ends the kernel kills every process left in
    /// it, however it detached.
    Namespaces,
    /// No user namespaces (a kernel or container that forbids them): each child runs in a cgroup of its own below this
    /// one, killed whole through `cgroup.kill` when the child ends, and Landlock scopes its signals and abstract unix
    /// sockets where the kernel has ABI 6. Weaker: no private `/proc` (processes running at the same time see each
    /// other), no `/dev/shm` at all, and System V IPC objects outlive the child.
    Cgroup(PathBuf),
}

/// How autoresearch children are isolated here, or why they cannot be. Probed once per process by starting a child
/// the real way; `HX_TEST_ISOLATION` (`cgroup` or `none`) lets the check suites exercise the fallbacks.
pub fn isolation() -> Result<&'static Isolation, &'static str> {
    static ISOLATION: std::sync::OnceLock<Result<Isolation, String>> = std::sync::OnceLock::new();
    ISOLATION.get_or_init(probe_isolation).as_ref().map_err(String::as_str)
}

/// One line for notes and records: how autoresearch children are isolated.
pub fn describe_isolation() -> String {
    match isolation() {
        Ok(Isolation::Namespaces) => "namespaces".into(),
        Ok(Isolation::Cgroup(_)) => format!("cgroup{}", if status().is_ok_and(|v| v >= 6) { " + landlock scopes" } else { "" }),
        Err(why) => format!("off: {why}"),
    }
}

#[cfg(target_os = "linux")]
fn probe_isolation() -> Result<Isolation, String> {
    let forced = std::env::var("HX_TEST_ISOLATION").ok();
    let why = match forced.as_deref() {
        None => match probe_namespaces() { Ok(()) => return Ok(Isolation::Namespaces), Err(e) => e },
        Some(_) => "disabled by HX_TEST_ISOLATION".to_string(),
    };
    if forced.as_deref() == Some("none") { return Err(format!("no user namespaces ({why}), no cgroup fallback")); }
    cgroup_parent().map(Isolation::Cgroup).map_err(|cg| format!("no user namespaces ({why}) and no cgroup v2 with cgroup.kill to fall back on ({cg})"))
}

#[cfg(not(target_os = "linux"))]
fn probe_isolation() -> Result<Isolation, String> { Err("process isolation needs Linux namespaces or cgroups".into()) }

/// Start `sh` the way an isolated child starts and check that it is the first process of its own pid namespace.
#[cfg(target_os = "linux")]
fn probe_namespaces() -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let sandbox = std::sync::Arc::new(Isolate { net: true, overlay: None }.sandbox(&Isolation::Namespaces).map_err(|e| e.to_string())?);
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", "test \"$$\" = 1 && test -d /proc/1 && ! test -d /proc/3"]).stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    unsafe { cmd.pre_exec(move || sandbox.enter()); }
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("a namespaced probe exited with {s}")),
        Err(e) => Err(e.to_string()),
    }
}

/// This process's cgroup v2 directory, when a child cgroup with `cgroup.kill` (Linux 5.14) can be made in it.
#[cfg(target_os = "linux")]
fn cgroup_parent() -> Result<PathBuf, String> {
    let own = std::fs::read_to_string("/proc/self/cgroup").map_err(|e| e.to_string())?;
    let rel = own.lines().find_map(|l| l.strip_prefix("0::")).ok_or("no cgroup v2 hierarchy")?;
    let info = std::fs::read_to_string("/proc/self/mountinfo").map_err(|e| e.to_string())?;
    // `ID PARENT MAJ:MIN ROOT MOUNTPOINT OPTIONS... - FSTYPE SOURCE SUPEROPTIONS`
    let (root, at) = info.lines().find_map(|l| {
        let (left, right) = l.split_once(" - ")?;
        let f: Vec<&str> = left.split(' ').collect();
        (right.split(' ').next() == Some("cgroup2") && f.len() >= 5).then(|| (f[3].to_string(), f[4].to_string()))
    }).ok_or("no cgroup2 mount")?;
    let inner = rel.strip_prefix(root.trim_end_matches('/')).unwrap_or(rel).trim_start_matches('/');
    let dir = Path::new(&at).join(inner);
    drop(Cgroup::new(&dir).map_err(|e| format!("{}: {e}", dir.display()))?);
    Ok(dir)
}

/// An isolated child: `net` takes its network away as well (an evaluator needs none, and without one it can hand
/// nothing to a listener elsewhere; an episode's shell keeps the environment's); `overlay` mounts its working tree.
#[derive(Clone, Debug, Default)]
pub struct Isolate {
    pub net: bool,
    pub overlay: Option<Overlay>,
}

/// An overlay mounted at `target` in the child's own mount namespace, which it then starts in: the read-only `lower`
/// layers (first on top) under the writable `upper`. An evaluator's tree then costs no copy of the frozen base.
#[derive(Clone, Debug)]
pub struct Overlay {
    pub lower: Vec<PathBuf>,
    pub upper: PathBuf,
    pub work: PathBuf,
    pub target: PathBuf,
}

impl Overlay {
    /// Can these paths be written into the mount options (`:`, `,` and `\` are separators there)?
    pub fn expressible(paths: &[&Path]) -> bool {
        paths.iter().all(|p| p.to_str().is_some_and(|s| !s.contains([':', ',', '\\'])))
    }
}

/// One child's isolation, prepared in the parent: `enter` applies it between fork and exec.
pub struct Sandbox {
    net: bool,
    shm: bool,
    uid_map: Vec<u8>,
    gid_map: Vec<u8>,
    /// The overlay's mount point and options.
    overlay: Option<(CString, CString)>,
    cgroup: Option<Cgroup>,
}

impl Isolate {
    /// Prepare one child's isolation; the error says why this system cannot isolate it.
    pub fn prepare(&self) -> std::io::Result<Sandbox> {
        let how = isolation().map_err(|why| std::io::Error::other(format!("autoresearch evaluators and episode shell commands \
run isolated, and this system cannot isolate them ({why}); run hx where unprivileged user namespaces are allowed")))?;
        self.sandbox(how)
    }

    fn sandbox(&self, how: &Isolation) -> std::io::Result<Sandbox> {
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let mut sandbox = Sandbox { net: self.net, shm: Path::new(SHM).is_dir(), uid_map: format!("{uid} {uid} 1").into_bytes(),
            gid_map: format!("{gid} {gid} 1").into_bytes(), overlay: None, cgroup: None };
        let cstr = |p: &Path| CString::new(p.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other);
        match how {
            Isolation::Namespaces => if let Some(o) = &self.overlay {
                let mut all: Vec<&Path> = o.lower.iter().map(PathBuf::as_path).collect();
                all.extend([o.upper.as_path(), &o.work, &o.target]);
                if !Overlay::expressible(&all) { return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "overlay paths cannot be expressed")); }
                let lower: Vec<String> = o.lower.iter().map(|p| p.display().to_string()).collect();
                let opts = format!("lowerdir={},upperdir={},workdir={}", lower.join(":"), o.upper.display(), o.work.display());
                sandbox.overlay = Some((cstr(&o.target)?, CString::new(opts).map_err(std::io::Error::other)?));
            },
            Isolation::Cgroup(parent) => {
                if self.overlay.is_some() { return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "an overlay needs a mount namespace")); }
                sandbox.cgroup = Some(Cgroup::new(parent)?);
            }
        }
        Ok(sandbox)
    }
}

impl Sandbox {
    /// Isolate the calling process; call it in `pre_exec`, after `setsid` and before Landlock. With namespaces, the
    /// process unshares, forks the child's first process (which returns here and goes on to exec) and stays behind as a
    /// stand-in that holds no descriptor and exits as that process does, so the parent's handle, timeout and process
    /// group still work. Async-signal-safe: system calls on what the parent prepared.
    #[cfg(target_os = "linux")]
    pub fn enter(&self) -> std::io::Result<()> {
        let last = std::io::Error::last_os_error;
        if let Some(cg) = &self.cgroup {
            use std::os::fd::AsRawFd;
            return if unsafe { libc::write(cg.procs.as_raw_fd(), b"0".as_ptr().cast(), 1) } == 1 { Ok(()) } else { Err(last()) };
        }
        let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWPID | libc::CLONE_NEWNS | libc::CLONE_NEWIPC | if self.net { libc::CLONE_NEWNET } else { 0 };
        if unsafe { libc::unshare(flags) } != 0 { return Err(last()); }
        // The same ids inside as outside: files keep their owner, and a non-root child has no capability after exec.
        write_proc(c"/proc/self/setgroups", b"deny", true)?;
        write_proc(c"/proc/self/uid_map", &self.uid_map, false)?;
        write_proc(c"/proc/self/gid_map", &self.gid_map, false)?;
        match unsafe { libc::fork() } {
            -1 => Err(last()),
            0 => self.init(),
            pid => stand_in(pid),
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn enter(&self) -> std::io::Result<()> { Err(std::io::Error::other("process isolation is Linux only")) }

    /// In the child's first process (pid 1 of its namespace): die with the stand-in, mount a private `/proc` and
    /// `/dev/shm` (and the overlay), bring up loopback in a private network, and give root no capabilities after exec,
    /// so it cannot unmount the private `/proc` to uncover the machine's (Landlock forbids mounts as well).
    #[cfg(target_os = "linux")]
    fn init(&self) -> std::io::Result<()> {
        let check = |r: libc::c_int| if r == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) };
        let none = std::ptr::null::<libc::c_char>();
        unsafe {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            check(libc::mount(none, c"/".as_ptr(), none, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()))?;
            check(libc::mount(c"proc".as_ptr(), c"/proc".as_ptr(), c"proc".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC, std::ptr::null()))?;
            if self.shm {
                check(libc::mount(c"tmpfs".as_ptr(), c"/dev/shm".as_ptr(), c"tmpfs".as_ptr(), libc::MS_NOSUID | libc::MS_NODEV, c"mode=1777".as_ptr().cast()))?;
            }
            if self.net { loopback_up(); }
            if let Some((target, opts)) = &self.overlay {
                check(libc::mount(c"overlay".as_ptr(), target.as_ptr(), c"overlay".as_ptr(), 0, opts.as_ptr().cast()))?;
                check(libc::chdir(target.as_ptr()))?;
            }
            libc::prctl(libc::PR_SET_SECUREBITS, (libc::SECBIT_NOROOT | libc::SECBIT_NOROOT_LOCKED) as libc::c_ulong, 0, 0, 0);
        }
        Ok(())
    }
}

/// Write `bytes` to a `/proc/self` file between fork and exec. An `optional` file may be missing (`setgroups` before
/// Linux 3.19).
#[cfg(target_os = "linux")]
fn write_proc(path: &std::ffi::CStr, bytes: &[u8], optional: bool) -> std::io::Result<()> {
    unsafe {
        let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return if optional && e.raw_os_error() == Some(libc::ENOENT) { Ok(()) } else { Err(e) };
        }
        let n = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
        let e = std::io::Error::last_os_error();
        libc::close(fd);
        if n == bytes.len() as isize { Ok(()) } else { Err(e) }
    }
}

/// The stand-in between hx and an isolated child's first process: it closes every descriptor (the spawner reads its
/// exec-error pipe and the command's output to their end, which must come from the child alone), waits, and exits as
/// the child did. Killing it kills the child (`PR_SET_PDEATHSIG`), and with it the namespace.
#[cfg(target_os = "linux")]
fn stand_in(pid: libc::pid_t) -> ! {
    unsafe {
        if libc::syscall(libc::SYS_close_range, 0u32, u32::MAX, 0u32) != 0 {
            for fd in 0..4096 { libc::close(fd); }
        }
        let mut status = 0;
        while libc::waitpid(pid, &mut status, 0) < 0 {
            if *libc::__errno_location() != libc::EINTR { libc::_exit(127); }
        }
        if libc::WIFEXITED(status) { libc::_exit(libc::WEXITSTATUS(status)); }
        let sig = libc::WTERMSIG(status);
        libc::signal(sig, libc::SIG_DFL);
        libc::kill(libc::getpid(), sig);
        libc::_exit(128 + sig)
    }
}

/// Bring up `lo` in a fresh network namespace, so local servers and sockets inside an evaluator still work.
#[cfg(target_os = "linux")]
fn loopback_up() {
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if fd < 0 { return; }
        // `struct ifreq`: the name, then the flags as a short.
        let mut req = [0u8; 40];
        req[..2].copy_from_slice(b"lo");
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, req.as_mut_ptr()) == 0 {
            let flags = i16::from_ne_bytes([req[16], req[17]]) | libc::IFF_UP as i16;
            req[16..18].copy_from_slice(&flags.to_ne_bytes());
            libc::ioctl(fd, libc::SIOCSIFFLAGS, req.as_ptr());
        }
        libc::close(fd);
    }
}

/// The cgroup of one isolated child (the fallback without namespaces): the child moves itself in before exec, and
/// dropping this kills everything still in it and removes it.
pub struct Cgroup {
    dir: PathBuf,
    procs: std::fs::File,
}

impl Cgroup {
    fn new(parent: &Path) -> std::io::Result<Cgroup> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir = parent.join(format!("hx-{}-{}", std::process::id(), SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir(&dir)?;
        match std::fs::OpenOptions::new().write(true).open(dir.join("cgroup.procs")) {
            Ok(procs) if dir.join("cgroup.kill").exists() => Ok(Cgroup { dir, procs }),
            other => {
                let _ = std::fs::remove_dir(&dir);
                Err(other.err().unwrap_or_else(|| std::io::Error::other("no cgroup.kill (Linux 5.14)")))
            }
        }
    }
}

impl Drop for Cgroup {
    fn drop(&mut self) {
        let _ = std::fs::write(self.dir.join("cgroup.kill"), "1");
        // The kill is asynchronous; the directory goes once the cgroup is empty.
        for _ in 0..200 {
            if std::fs::read_to_string(self.dir.join("cgroup.events")).map_or(true, |e| e.contains("populated 0")) { break; }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carving_grants_siblings_and_lists_ancestors() {
        let dir = std::env::temp_dir().join(format!("hx-carve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".harness/pkgs")).unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join(".harness/secrets.toml"), "k").unwrap();
        std::fs::write(dir.join("README.md"), "r").unwrap();
        std::os::unix::fs::symlink("/etc", dir.join("link")).unwrap();
        let (mut full, mut list) = (vec![], vec![]);
        carve(&dir, &[dir.join(".harness/secrets.toml")], &mut full, &mut list);
        full.sort();
        assert_eq!(full, vec![dir.join(".harness/pkgs"), dir.join("README.md"), dir.join("src")]);
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
        let rules = Rules { read: vec![dir.clone(), "/usr".into(), "/bin".into(), "/etc".into()], write: vec![], hidden: vec![dir.join(".harness/secrets.toml")], exposed: vec![], scoped: false }.with_system();
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

    /// Run `script` with `sh` in `sandbox` (and `ruleset`), returning its stdout.
    #[cfg(target_os = "linux")]
    fn run_in(sandbox: Sandbox, ruleset: Option<Ruleset>, script: &str) -> String {
        use std::os::unix::process::CommandExt;
        let (sandbox, ruleset) = (std::sync::Arc::new(sandbox), std::sync::Arc::new(ruleset));
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script).stdin(std::process::Stdio::null());
        let (s, r) = (sandbox.clone(), ruleset.clone());
        unsafe { cmd.pre_exec(move || { libc::setsid(); s.enter()?; if let Some(r) = r.as_ref() { r.apply()?; } Ok(()) }); }
        let out = cmd.output().unwrap();
        drop(cmd);
        drop(sandbox);
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[cfg(target_os = "linux")]
    fn running(marker: &str) -> bool {
        std::fs::read_dir("/proc").unwrap().flatten().any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c.starts_with(marker.as_bytes())))
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn namespaced_children_see_no_other_process_and_leave_none() {
        if !matches!(isolation(), Ok(Isolation::Namespaces)) { eprintln!("skipped: no user namespaces"); return; }
        let marker = format!("hx-ns-held-{}", std::process::id());
        let sandbox = || Isolate { net: true, overlay: None }.prepare().unwrap();
        // One child leaves a detached process holding a secret in its argv and a file in its /dev/shm, and lists its
        // own /proc; once it has ended, nothing of it runs.
        let first = run_in(sandbox(), None, &format!("(setsid bash -c 'exec -a {marker} sleep 300' >/dev/null 2>&1 &); \
echo s > /dev/shm/hx-ns-{0}; sleep 0.2; ls /proc | grep -c '^[0-9]'; grep -l '^[h]{1}' /proc/[0-9]*/cmdline | wc -l", std::process::id(), &marker[1..]));
        let counts: Vec<u32> = first.split_whitespace().map(|n| n.parse().unwrap()).collect();
        assert!(counts[0] <= 6 && counts[1] == 1, "a namespaced child sees other processes, or its own leftover never ran: {first}");
        assert!(!running(&marker), "a detached process outlived its namespace");
        assert!(!Path::new(&format!("/dev/shm/hx-ns-{}", std::process::id())).exists(), "/dev/shm is private");
        // While a process of this one runs, a second child cannot see it.
        let mut held = std::process::Command::new("bash").args(["-c", &format!("exec -a {marker} sleep 30")]).spawn().unwrap();
        for _ in 0..500 { if running(&marker) { break } std::thread::sleep(std::time::Duration::from_millis(10)); }
        let seen = run_in(sandbox(), None, &format!("cat /proc/[0-9]*/cmdline 2>/dev/null | tr '\\0' ' ' | grep -c '[h]{}'; kill -0 {} 2>&1 | grep -c 'No such'", &marker[1..], held.id()));
        held.kill().unwrap();
        held.wait().unwrap();
        assert_eq!(seen.split_whitespace().collect::<Vec<_>>(), ["0", "1"], "{seen}");
        // An evaluator's network is its own: loopback only.
        let net = run_in(sandbox(), None, "cat /proc/net/dev | grep -c ':'");
        assert_eq!(net.trim(), "1", "only lo in a private network");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_cgroup_fallback_kills_what_a_child_leaves() {
        let Ok(parent) = cgroup_parent() else { eprintln!("skipped: no cgroup v2 with cgroup.kill"); return; };
        let marker = format!("hx-cg-held-{}", std::process::id());
        let sandbox = Isolate { net: true, overlay: None }.sandbox(&Isolation::Cgroup(parent.clone())).unwrap();
        let cgroup = sandbox.cgroup.as_ref().unwrap().dir.clone();
        let ran = run_in(sandbox, None, &format!("(setsid bash -c 'exec -a {marker} sleep 300' >/dev/null 2>&1 &); sleep 0.2; grep -l '^[h]{}' /proc/[0-9]*/cmdline | wc -l", &marker[1..]));
        assert_eq!(ran.trim(), "1", "the leftover never ran");
        assert!(!running(&marker), "a detached process outlived its cgroup");
        assert!(!cgroup.exists(), "the child's cgroup is removed");
        assert!(Isolate { net: false, overlay: Some(Overlay { lower: vec![], upper: "/u".into(), work: "/w".into(), target: "/t".into() }) }
            .sandbox(&Isolation::Cgroup(parent)).is_err(), "an overlay needs namespaces");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn isolated_children_share_no_dev_shm() {
        if status().is_err() || !Path::new("/dev/shm").is_dir() { eprintln!("skipped: Landlock or /dev/shm unavailable"); return; }
        let planted = PathBuf::from(format!("/dev/shm/hx-confine-planted-{}", std::process::id()));
        std::fs::write(&planted, "LEFT").unwrap();
        let rules = Rules { read: vec!["/usr".into(), "/bin".into(), "/etc".into(), "/dev".into()], write: vec![], hidden: vec![], exposed: vec![], scoped: false }.with_system().without_shm();
        let ruleset = std::sync::Arc::new(rules.prepare().unwrap().unwrap());
        let script = format!("echo x > /dev/shm/hx-confine-{}; cat {}; echo ok > /dev/null && echo NULL", std::process::id(), planted.display());
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        use std::os::unix::process::CommandExt;
        let r = ruleset.clone();
        unsafe { cmd.pre_exec(move || r.apply()); }
        let out = cmd.output().unwrap().stdout;
        let _ = std::fs::remove_file(&planted);
        let wrote = std::fs::remove_file(format!("/dev/shm/hx-confine-{}", std::process::id())).is_ok();
        assert_eq!((String::from_utf8_lossy(&out).as_ref(), wrote), ("NULL\n", false));
    }
}
