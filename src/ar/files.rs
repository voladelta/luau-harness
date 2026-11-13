//! What crosses an episode's boundary: snapshots of the editable files, the throwaway copies episodes and evaluators
//! run in, the evaluator itself, and the committed tree frozen at launch.
//!
//! The editable files of an episode's workspace are agent-controlled and can change while they are read, so every
//! file and directory under them is opened relative to an already opened directory, never through a symlink: no path
//! is checked and then resolved again.
use super::*;
use base64::Engine as _;
use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;

/// Limits on the editable files of one snapshot: total bytes, file count and directory depth.
pub(crate) const MAX_SNAPSHOT: usize = 16 << 20;
pub(crate) const MAX_FILES: usize = 10_000;
pub(crate) const MAX_DEPTH: usize = 32;

/// One editable file: its bytes and whether it is executable.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Content {
    pub(super) bytes: Vec<u8>,
    pub(super) exec: bool,
}

/// Editable files, path -> content.
pub(super) type Files = BTreeMap<String, Content>;

/// A file in a stored snapshot: base64 bytes, wrapped as `{"exec": ...}` when the file is executable. A plain file is
/// a bare string, so a snapshot without executables is stored (and named) exactly as before modes were kept.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Stored {
    Plain(String),
    Exec { exec: String },
}

fn cstr(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in file name"))
}

/// Open `name` in `dir` without following a symlink: a symlink fails with `ELOOP`. Non-blocking, so a FIFO cannot
/// stall the open.
fn open_in(dir: &File, name: &str, flags: libc::c_int, mode: libc::c_uint) -> io::Result<File> {
    let name = cstr(name)?;
    let flags = flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    // SAFETY: `name` is a NUL-terminated string that outlives the call, and `dir` is an open descriptor.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful `openat` returned a new descriptor that nothing else owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Open the directory `name` in `dir`, never through a symlink; with `create`, make it when it is missing.
fn open_dir_in(dir: &File, name: &str, create: bool) -> io::Result<File> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY;
    match open_in(dir, name, flags, 0) {
        Err(e) if create && e.kind() == io::ErrorKind::NotFound => {
            let c = cstr(name)?;
            // SAFETY: `c` is a NUL-terminated string that outlives the call, and `dir` is an open descriptor.
            if unsafe { libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o777) } < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::AlreadyExists {
                    return Err(e);
                }
            }
            open_in(dir, name, flags, 0)
        }
        other => other,
    }
}

/// The directory holding the relative path `rel` under `root`, opened component by component without following a
/// symlink, and the final name. With `create`, missing directories are made.
fn parent_of<'a>(root: &File, rel: &'a str, create: bool) -> io::Result<(File, &'a str)> {
    let (parents, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let mut dir = root.try_clone()?;
    for part in parents.split('/').filter(|p| !p.is_empty()) {
        dir = open_dir_in(&dir, part, create)?;
    }
    Ok((dir, name))
}

/// Errors that mean "nothing to capture here": a symlink, an entry removed while it was read, a socket, or a parent
/// that is not a directory.
fn absent(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP | libc::ENOENT | libc::ENXIO | libc::ENOTDIR))
}

/// The entry names of the open directory `dir`, read through the descriptor itself.
fn names(dir: &File) -> Result<Vec<String>> {
    // `/proc/self/fd/N` resolves to the open directory itself, not to whatever its path names now.
    std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?
        .map(|entry| {
            entry?.file_name().into_string().map_err(|n| anyhow::anyhow!("editable file name {n:?} is not UTF-8"))
        })
        .collect()
}

/// Every regular file under the editable paths of `root`, within the snapshot limits. Symlinks and special files are
/// skipped: they never leave an episode, and nothing is read through them. Names must be UTF-8 so they are recreated
/// exactly.
pub(super) fn capture(root: &Path, spec: &Spec) -> Result<Files> {
    fn walk(file: File, rel: &str, depth: usize, out: &mut Files, total: &mut usize) -> Result<()> {
        let meta = file.metadata()?;
        if meta.is_dir() {
            ensure!(depth < MAX_DEPTH, "editable directories nest deeper than {MAX_DEPTH} levels");
            for name in names(&file)? {
                let child = match open_in(&file, &name, libc::O_RDONLY, 0) {
                    Err(e) if absent(&e) => continue,
                    other => other.with_context(|| format!("editable file {rel}/{name}"))?,
                };
                walk(child, &format!("{rel}/{name}"), depth + 1, out, total)?;
            }
        } else if meta.is_file() {
            ensure!(out.len() < MAX_FILES, "editable paths hold more than {MAX_FILES} files");
            // Read at most what is left of the limit, so a file that keeps growing cannot get past it.
            let left = MAX_SNAPSHOT - *total;
            let mut bytes = Vec::new();
            file.take(left as u64 + 1).read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= left, "editable files exceed {} MiB", MAX_SNAPSHOT >> 20);
            *total += bytes.len();
            out.insert(rel.to_string(), Content { bytes, exec: meta.permissions().mode() & 0o111 != 0 });
        }
        Ok(())
    }
    let top = File::open(root).with_context(|| format!("workspace {}", root.display()))?;
    let (mut out, mut total) = (Files::new(), 0);
    for e in &spec.editable {
        let e = e.trim_end_matches('/');
        let opened = parent_of(&top, e, false).and_then(|(dir, name)| open_in(&dir, name, libc::O_RDONLY, 0));
        match opened {
            Ok(file) => walk(file, e, 0, &mut out, &mut total)?,
            Err(err) if absent(&err) => {}
            Err(err) => return Err(err).with_context(|| format!("editable path {e}")),
        }
    }
    Ok(out)
}

/// The mode `mode` with the executable bits set where it can be read (`exec`), or cleared.
fn with_exec(mode: u32, exec: bool) -> u32 {
    if exec { mode | (mode & 0o444) >> 2 } else { mode & !0o111 }
}

/// Write `content` to `rel` under `root`, making missing directories, never through a symlink.
fn write_in(root: &File, rel: &str, content: &Content) -> Result<()> {
    let (dir, name) = parent_of(root, rel, true).with_context(|| format!("editable file {rel}"))?;
    let mut file = open_in(&dir, name, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, if content.exec { 0o777 } else { 0o666 })
        .with_context(|| format!("editable file {rel}"))?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "editable file {rel} is not a regular file");
    file.write_all(&content.bytes)?;
    let mode = meta.permissions().mode() & 0o7777;
    if with_exec(mode, content.exec) != mode {
        file.set_permissions(std::fs::Permissions::from_mode(with_exec(mode, content.exec)))?;
    }
    Ok(())
}

/// Remove the file `rel` under `root` (a symlink there is removed, never followed); a missing one is fine.
fn remove_in(root: &File, rel: &str) -> Result<()> {
    let (dir, name) = match parent_of(root, rel, false) {
        Err(e) if absent(&e) => return Ok(()),
        other => other.with_context(|| format!("editable file {rel}"))?,
    };
    let c = cstr(name)?;
    // SAFETY: `c` is a NUL-terminated string that outlives the call, and `dir` is an open descriptor.
    if unsafe { libc::unlinkat(dir.as_raw_fd(), c.as_ptr(), 0) } < 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e).with_context(|| format!("remove editable file {rel}"));
        }
    }
    Ok(())
}

/// Make the editable paths of the throwaway copy `root` hold exactly `files`, with their modes.
pub(super) fn place(root: &Path, spec: &Spec, files: &Files) -> Result<()> {
    std::fs::create_dir_all(root)?;
    for e in &spec.editable {
        let path = root.join(e.trim_end_matches('/'));
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path)?,
            Ok(_) => std::fs::remove_file(&path)?,
            Err(_) => {}
        }
    }
    let top = File::open(root)?;
    for (rel, content) in files {
        write_in(&top, rel, content)?;
    }
    Ok(())
}

/// Make the editable paths of a workspace someone else owns hold `result`, changing as little as possible: write only
/// files whose bytes or executable bit differ, and remove only files `before` held, `result` does not, and that still
/// hold what `before` recorded. Everything else there (files the run never saw, symlinks, other modes) is left alone.
pub(super) fn apply(root: &Path, spec: &Spec, before: &Files, result: &Files) -> Result<()> {
    let current = capture(root, spec)?;
    let top = File::open(root)?;
    for (rel, content) in result.iter().filter(|(rel, content)| current.get(*rel) != Some(content)) {
        write_in(&top, rel, content)?;
    }
    for (rel, old) in before.iter().filter(|(rel, _)| !result.contains_key(*rel)) {
        if current.get(rel) == Some(old) {
            remove_in(&top, rel)?;
        }
    }
    Ok(())
}

/// `ar.reset()`'s write: make the editable paths of the agent's workspace `root` hold `files`, through no symlink.
/// `check` vets each relative path first (the policy's write check).
pub(super) fn restore(root: &Path, spec: &Spec, files: &Files, check: impl Fn(&str) -> Result<()>) -> Result<()> {
    let top = File::open(root)?;
    for rel in capture(root, spec)?.keys().filter(|rel| !files.contains_key(*rel)) {
        check(rel)?;
        remove_in(&top, rel)?;
    }
    for (rel, content) in files {
        check(rel)?;
        write_in(&top, rel, content)?;
    }
    Ok(())
}

/// Store a snapshot content-addressed under `dir/snaps`; returns its id.
pub(super) fn save_snap(dir: &Path, files: &Files) -> Result<String> {
    let encoded: BTreeMap<&String, Stored> = files.iter().map(|(k, c)| {
        let data = base64::engine::general_purpose::STANDARD.encode(&c.bytes);
        (k, if c.exec { Stored::Exec { exec: data } } else { Stored::Plain(data) })
    }).collect();
    let bytes = serde_json::to_vec(&encoded)?;
    let id = crate::eventlog::sha_bytes(&bytes)[..16].to_string();
    let path = dir.join("snaps").join(format!("{id}.json"));
    if !path.exists() {
        std::fs::create_dir_all(dir.join("snaps"))?;
        std::fs::write(path, bytes)?;
    }
    Ok(id)
}

pub(super) fn load_snap(dir: &Path, id: &str) -> Result<Files> {
    let path = dir.join("snaps").join(format!("{id}.json"));
    let encoded: BTreeMap<String, Stored> = serde_json::from_slice(&std::fs::read(&path)?).with_context(|| format!("snapshot {id}"))?;
    encoded.into_iter().map(|(k, stored)| {
        let (data, exec) = match stored { Stored::Plain(d) => (d, false), Stored::Exec { exec } => (exec, true) };
        Ok((k, Content { bytes: base64::engine::general_purpose::STANDARD.decode(data)?, exec }))
    }).collect()
}

/// Kill every process that still has its working directory, executable or an open file under `dir` (agents' and
/// evaluators' detached leftovers), before the directory is deleted. Repeats until a pass finds none, so a process
/// that forked while being looked for is caught too.
pub(super) fn reap(dir: &Path) {
    const PASSES: usize = 100;
    let me = std::process::id();
    let inside = |link: PathBuf| std::fs::read_link(link).is_ok_and(|target| target.starts_with(dir));
    for _ in 0..PASSES {
        let Ok(procs) = std::fs::read_dir("/proc") else { return };
        let mut found = false;
        for pid in procs.flatten().filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok()).filter(|p| *p != me) {
            let proc = PathBuf::from(format!("/proc/{pid}"));
            let holds = inside(proc.join("cwd")) || inside(proc.join("exe"))
                || std::fs::read_dir(proc.join("fd")).into_iter().flatten().flatten().any(|fd| inside(fd.path()));
            if holds {
                // SAFETY: kill(2) takes plain integers; a pid that has exited meanwhile only makes it fail.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                found = true;
            }
        }
        if !found {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Scratch roots in the system temp directory are named `hx-ar-PID-NANOS`, one per process that needs throwaway
/// copies, and hold a `lock` file that process keeps locked while it lives.
const ROOT_PREFIX: &str = "hx-ar-";

/// This process's scratch root and how many scratch directories in it are in use.
struct Root {
    path: PathBuf,
    /// Held locked until the root is removed; `sweep` leaves a locked root alone.
    _lock: File,
    users: usize,
}
static ROOT: std::sync::Mutex<Option<Root>> = std::sync::Mutex::new(None);

/// Lock `file` exclusively; with `wait` false, fail at once when another holds it.
fn flock(file: &File, wait: bool) -> bool {
    let op = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
    // SAFETY: flock(2) on an open descriptor this function borrows.
    unsafe { libc::flock(file.as_raw_fd(), op) == 0 }
}

/// Create this process's scratch root, locked before it gets the name `sweep` looks for.
fn new_root() -> Result<Root> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let path = std::fs::canonicalize(std::env::temp_dir())?.join(format!("{ROOT_PREFIX}{}-{nanos}", std::process::id()));
    std::fs::create_dir(&path).with_context(|| format!("scratch directory {}", path.display()))?;
    let lock = File::create(path.join("lock.new"))?;
    ensure!(flock(&lock, true), "could not lock {}", path.display());
    std::fs::rename(path.join("lock.new"), path.join("lock"))?;
    Ok(Root { path, _lock: lock, users: 0 })
}

/// A fresh private directory inside this process's scratch root. Removed (after its leftover processes are killed) on
/// drop; when this process ends without dropping it (stopped by a signal, killed), `sweep` in a later run removes it.
pub(super) struct Scratch(pub(super) PathBuf);
impl Scratch {
    pub(super) fn new(kind: &str) -> Result<Scratch> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let mut root = ROOT.lock().expect("scratch root lock poisoned");
        if root.is_none() {
            *root = Some(new_root()?);
        }
        let root = root.as_mut().expect("scratch root was just created");
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = root.path.join(format!("{kind}-{seq}"));
        std::fs::create_dir(&path)?;
        root.users += 1;
        Ok(Scratch(path))
    }
}

/// Remove a scratch directory of this process, and the root with the last one.
fn release(path: &Path) {
    reap(path);
    let _ = std::fs::remove_dir_all(path);
    let mut root = ROOT.lock().expect("scratch root lock poisoned");
    if let Some(r) = root.as_mut() {
        r.users -= 1;
        if r.users == 0 {
            let _ = std::fs::remove_file(r.path.join("lock"));
            let _ = std::fs::remove_dir(&r.path);
            *root = None;
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let path = std::mem::take(&mut self.0);
        // Killing leftovers and deleting a tree is blocking work: off the async worker threads when there are some.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(move || release(&path))),
            Err(_) => release(&path),
        }
    }
}

/// Remove the scratch roots of processes that ended without removing them: those whose lock no live process holds.
pub(super) fn sweep() {
    let Ok(tmp) = std::fs::canonicalize(std::env::temp_dir()) else { return };
    let Ok(entries) = std::fs::read_dir(&tmp) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_str().is_some_and(|n| n.starts_with(ROOT_PREFIX))
            || !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let path = entry.path();
        let Ok(dir) = File::open(&path) else { continue };
        let Ok(lock) = open_in(&dir, "lock", libc::O_RDONLY, 0) else { continue };
        if flock(&lock, false) {
            reap(&path);
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Copy the tree `from` to the new path `to`, keeping modes and symlinks (reflinked where the filesystem can).
pub(super) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    let out = std::process::Command::new("cp").args(["-a", "--reflink=auto", "--"]).arg(from).arg(to)
        .stdin(std::process::Stdio::null()).output().context("cp")?;
    ensure!(out.status.success(), "could not copy {} to {}: {}", from.display(), to.display(),
        String::from_utf8_lossy(&out.stderr).trim());
    Ok(())
}

/// What one evaluation said: its score, whether it is valid, its standard error and its report.
pub(super) struct Scored {
    pub(super) score: Option<f64>,
    pub(super) valid: bool,
    pub(super) se: Option<f64>,
    pub(super) report: Value,
}

/// Read roots the search evaluator gets: the system and toolchain directories every child reads, and `grader`. A
/// `holdout` root inside one of them would be readable by it, so that is refused.
pub(super) fn search_read_roots(spec: &Spec) -> Result<Vec<PathBuf>> {
    let mut read = crate::policy::system_read_roots();
    read.extend(spec.roots(false)?);
    let read = crate::confine::Rules { read, write: vec![], hidden: vec![] }.with_system().read;
    for h in spec.holdout_roots()? {
        if let Some(r) = read.iter().find(|r| h.starts_with(r)) {
            bail!("ar.toml: holdout root {} lies inside {}, which the search evaluator may read", h.display(), r.display());
        }
    }
    Ok(read)
}

/// Run an evaluator on `files` placed into a fresh copy of the frozen base. Failures of the evaluator become invalid
/// results, not errors. Where Landlock is available the evaluator may read only the system, its scratch copy and its
/// `grader` roots (plus `holdout` for the select evaluator), and write only its scratch copy.
pub(super) async fn evaluate(spec: &Spec, run: &Path, files: &Files, cmd: &str, holdout: bool) -> Result<Scored> {
    let (spec_, run, files_) = (spec.clone(), run.to_path_buf(), files.clone());
    let (scratch, rules) = tokio::task::spawn_blocking(move || -> Result<_> {
        let mut read = if holdout {
            let mut read = crate::policy::system_read_roots();
            read.extend(spec_.roots(true)?);
            read
        } else {
            search_read_roots(&spec_)?
        };
        let scratch = Scratch::new("eval")?;
        copy_tree(&run.join("base"), &scratch.0.join("tree"))?;
        place(&scratch.0.join("tree"), &spec_, &files_)?;
        std::fs::create_dir(scratch.0.join("tmp"))?;
        read.push(scratch.0.clone());
        let rules = crate::confine::Rules { read, write: vec![scratch.0.clone()], hidden: vec![] }.with_system();
        Ok((scratch, rules.prepare()?.map(std::sync::Arc::new)))
    }).await??;
    let env = crate::prims::ShellEnv { env: Some(evaluator_env(&scratch.0.join("tmp"))), confine: rules };
    let (stdout, stderr, code, timed_out, _) = crate::prims::shell(&env, cmd, scratch.0.join("tree"), spec.timeout).await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(scratch);
    let stdout = String::from_utf8_lossy(&stdout);
    let tail = |s: &str| s.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    let parsed = stdout.lines().rev().find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(Value::is_object);
    let Some(obj) = parsed.filter(|_| code == Some(0) && !timed_out) else {
        let why = if timed_out { "evaluator timed out".to_string() } else { format!("evaluator exit {code:?} or no JSON result line") };
        let report = json!({"error": why, "stdout": tail(&stdout), "stderr": tail(&String::from_utf8_lossy(&stderr))});
        return Ok(Scored { score: None, valid: false, se: None, report });
    };
    let score = obj["score"].as_f64().filter(|s| s.is_finite());
    let valid = score.is_some() && obj["valid"].as_bool() != Some(false);
    let se = obj["se"].as_f64().filter(|s| s.is_finite() && *s >= 0.0);
    Ok(Scored { score, valid, se, report: obj.get("report").cloned().unwrap_or(Value::Null) })
}

/// The evaluator's environment: no credentials, nothing inherited beyond what locates tools.
pub(super) fn evaluator_env(tmp: &Path) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = ["PATH", "HOME", "LANG", "LC_ALL", "USER", "RUSTUP_HOME", "CARGO_HOME"].iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v))).collect();
    env.push(("TMPDIR".into(), tmp.to_string_lossy().to_string()));
    env
}

/// Git, configured so a repository's own config cannot run commands (owner context, launch only).
pub(super) fn git(workspace: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "core.untrackedCache=false"])
        .args(args).current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output().context("git")?;
    ensure!(out.status.success(), "git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    Ok(out.stdout)
}

/// Paths that differ from HEAD outside the editable set (owner's `.harness/` excepted).
pub(super) fn uncommitted(workspace: &Path, spec: &Spec) -> Result<Vec<String>> {
    let out = git(workspace, &["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    let mut bad = vec![];
    let mut entries = out.split(|b| *b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let entry = String::from_utf8_lossy(entry);
        let (status, path) = entry.split_at(3.min(entry.len()));
        let mut paths = vec![path.to_string()];
        // A rename or copy names its source as the next entry; both must be in scope.
        if status.starts_with('R') || status.starts_with('C') {
            paths.extend(entries.next().map(|e| String::from_utf8_lossy(e).to_string()));
        }
        for p in paths {
            let inside = spec.editable.iter().any(|e| p == *e || p.starts_with(&format!("{}/", e.trim_end_matches('/'))));
            if !p.starts_with(".harness/") && !inside { bad.push(p); }
        }
    }
    Ok(bad)
}

/// Freeze HEAD's tree (without `.harness`) into `run/base`, byte for byte: blobs are read directly, so export
/// attributes (`export-ignore`, `export-subst`) cannot change what the evaluator sees. Submodules are not included.
pub(super) fn freeze(workspace: &Path, run: &Path) -> Result<String> {
    use std::io::{BufRead, Read, Write};
    let commit = String::from_utf8(git(workspace, &["rev-parse", "HEAD"])?)?.trim().to_string();
    let listing = git(workspace, &["ls-tree", "-r", "-z", "--full-tree", &commit])?;
    let entries: Vec<(String, String, String)> = listing.split(|b| *b == 0).filter(|e| !e.is_empty()).map(|e| {
        let e = String::from_utf8(e.to_vec()).map_err(|_| anyhow::anyhow!("non-UTF-8 path in the committed tree"))?;
        let (meta, path) = e.split_once('\t').context("ls-tree entry")?;
        let mut parts = meta.split(' ');
        let (mode, kind, sha) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        Ok((mode.to_string(), if kind == "blob" { sha.to_string() } else { String::new() }, path.to_string()))
    }).collect::<Result<_>>()?;
    let base = run.join("base");
    std::fs::create_dir_all(&base)?;
    let mut child = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "cat-file", "--batch"])
        .current_dir(workspace).env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    for (mode, sha, path) in entries.iter().filter(|(_, sha, _)| !sha.is_empty()) {
        if path == ".harness" || path.starts_with(".harness/") {
            continue;
        }
        ensure!(!path.split('/').any(|c| c == ".." || c.is_empty()), "unsafe path {path:?} in the committed tree");
        writeln!(input, "{sha}")?;
        input.flush()?;
        let mut header = String::new();
        output.read_line(&mut header)?;
        let size: usize = header.trim().rsplit(' ').next().and_then(|n| n.parse().ok()).context("cat-file header")?;
        let mut bytes = vec![0; size + 1];
        output.read_exact(&mut bytes)?;
        bytes.pop();
        let target = base.join(path);
        if let Some(parent) = target.parent() { std::fs::create_dir_all(parent)?; }
        if mode == "120000" {
            std::os::unix::fs::symlink(String::from_utf8_lossy(&bytes).to_string(), &target)?;
        } else {
            std::fs::write(&target, &bytes)?;
            if mode == "100755" {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
        }
    }
    drop(input);
    ensure!(child.wait()?.success(), "git cat-file failed");
    Ok(commit)
}

/// Copy an episode workspace's journals into the run as `episodes/LABEL.log`; returns that directory.
pub(super) fn keep_logs(ws: &Path, episodes: &Path, label: &str) -> Result<PathBuf> {
    let logs = episodes.join(format!("{label}.log"));
    if ws.join(".harness/log").is_dir() && !logs.exists() {
        std::fs::create_dir_all(episodes)?;
        copy_tree(&ws.join(".harness/log"), &logs)?;
    }
    Ok(logs)
}

/// How often a child without a report was rate-limited: the HTTP 429s its journals recorded.
pub(super) fn rate_limited(logs: &Path) -> u64 {
    std::fs::read_dir(logs).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|j| crate::supervisor::Books::of(&crate::eventlog::load(&j).ok()?).ok())
        .map(|books| books.usage.rate_limits).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-artest-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn spec(editable: &[&str]) -> Spec {
        Spec { editable: editable.iter().map(|e| e.to_string()).collect(), eval: "true".into(), select: None, grader: vec![],
            holdout: vec![], direction: "max".into(), timeout: 1.0, min_delta: 0.0 }
    }

    #[test]
    fn capture_never_reads_through_a_symlink_swapped_in_while_it_walks() {
        let (ws, secret) = (dir("swap"), dir("swap-secret"));
        std::fs::write(secret.join("secret.txt"), "HELD-OUT").unwrap();
        std::fs::create_dir(ws.join("pkg")).unwrap();
        std::fs::write(ws.join("pkg/a.txt"), "0").unwrap();
        // A symlinked editable directory, or one below a symlinked parent, is skipped.
        symlink(&secret, ws.join("link")).unwrap();
        assert!(capture(&ws, &spec(&["link", "link/secret.txt"])).unwrap().is_empty());
        // An agent's process keeps swapping the editable directory with a symlink to held-out data.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flipper = {
            let (ws, secret, stop) = (ws.clone(), secret.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::fs::rename(ws.join("pkg"), ws.join("pkg.d")).unwrap();
                    symlink(&secret, ws.join("pkg")).unwrap();
                    std::fs::remove_file(ws.join("pkg")).unwrap();
                    std::fs::rename(ws.join("pkg.d"), ws.join("pkg")).unwrap();
                }
            })
        };
        let spec = spec(&["pkg"]);
        for _ in 0..3000 {
            let files = capture(&ws, &spec).unwrap();
            assert!(files.keys().all(|k| k == "pkg/a.txt"), "{:?}", files.keys());
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        flipper.join().unwrap();
        let _ = std::fs::remove_dir_all(&ws);
        let _ = std::fs::remove_dir_all(&secret);
    }

    #[test]
    fn executable_bits_survive_snapshot_and_restore() {
        let (ws, run, copy) = (dir("exec"), dir("exec-run"), dir("exec-copy"));
        std::fs::create_dir(ws.join("tool")).unwrap();
        std::fs::write(ws.join("tool/run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(ws.join("tool/run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(ws.join("tool/data"), "1").unwrap();
        let spec = spec(&["tool"]);
        let files = capture(&ws, &spec).unwrap();
        assert!(files["tool/run.sh"].exec && !files["tool/data"].exec);
        let loaded = load_snap(&run, &save_snap(&run, &files).unwrap()).unwrap();
        assert_eq!(loaded, files);
        place(&copy, &spec, &loaded).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o111;
        assert_ne!(mode(&copy.join("tool/run.sh")), 0);
        assert_eq!(mode(&copy.join("tool/data")), 0);
        // A snapshot without executables is stored as plain strings, as before modes were kept.
        let plain = Files::from([("x".to_string(), Content { bytes: b"0\n".to_vec(), exec: false })]);
        let id = save_snap(&run, &plain).unwrap();
        assert_eq!(std::fs::read_to_string(run.join("snaps").join(format!("{id}.json"))).unwrap(), r#"{"x":"MAo="}"#);
        for d in [ws, run, copy] { let _ = std::fs::remove_dir_all(d); }
    }

    #[test]
    fn apply_changes_only_what_the_result_changed() {
        let ws = dir("apply");
        std::fs::create_dir(ws.join("tool")).unwrap();
        for (name, text) in [("same", "s"), ("changed", "old"), ("gone", "g"), ("edited", "owner's")] {
            std::fs::write(ws.join("tool").join(name), text).unwrap();
        }
        std::fs::set_permissions(ws.join("tool/same"), std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink("same", ws.join("tool/link")).unwrap();
        let spec = spec(&["tool"]);
        let file = |text: &str| Content { bytes: text.as_bytes().to_vec(), exec: false };
        let before = Files::from([("tool/same".into(), Content { bytes: b"s".to_vec(), exec: true }),
            ("tool/changed".into(), file("old")), ("tool/gone".into(), file("g")), ("tool/edited".into(), file("seed"))]);
        let mut result = before.clone();
        result.insert("tool/changed".into(), file("new"));
        result.remove("tool/gone");
        result.remove("tool/edited");
        result.insert("tool/new".into(), Content { bytes: b"n".to_vec(), exec: true });
        let inode = |name: &str| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(ws.join("tool").join(name)).unwrap());
        let same = inode("same");
        apply(&ws, &spec, &before, &result).unwrap();
        let read = |name: &str| std::fs::read_to_string(ws.join("tool").join(name)).ok();
        assert_eq!((read("changed").as_deref(), read("gone"), read("edited").as_deref()), (Some("new"), None, Some("owner's")));
        assert_eq!(std::fs::metadata(ws.join("tool/same")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(inode("same"), same);
        assert!(std::fs::symlink_metadata(ws.join("tool/link")).unwrap().file_type().is_symlink());
        assert_ne!(std::fs::metadata(ws.join("tool/new")).unwrap().permissions().mode() & 0o111, 0);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn reap_kills_every_process_left_in_a_directory() {
        let d = dir("reap");
        let mut child = std::process::Command::new("sh").args(["-c", "sleep 300 & sleep 300"]).current_dir(&d).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(200));
        reap(&d);
        let status = child.wait().unwrap();
        assert!(!status.success());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sweep_removes_the_scratch_roots_of_ended_processes_only() {
        let tmp = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let (dead, live) = (tmp.join(format!("{ROOT_PREFIX}0-{}-dead", std::process::id())),
            tmp.join(format!("{ROOT_PREFIX}0-{}-live", std::process::id())));
        for d in [&dead, &live] {
            std::fs::create_dir_all(d.join("eval-0/tree")).unwrap();
            std::fs::write(d.join("lock"), "").unwrap();
        }
        let held = File::open(live.join("lock")).unwrap();
        assert!(flock(&held, false));
        let scratch = Scratch::new("ws").unwrap();
        sweep();
        assert!(!dead.exists() && live.exists() && scratch.0.exists());
        drop(held);
        sweep();
        assert!(!live.exists());
        let path = scratch.0.clone();
        drop(scratch);
        assert!(!path.exists());
    }
}
