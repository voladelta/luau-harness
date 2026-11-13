//! What crosses an episode's boundary: snapshots of the editable files, the throwaway copies episodes and evaluators
//! run in, the evaluator itself, and the committed tree frozen at launch.
use super::*;
use base64::Engine as _;
use crate::eventlog::{EventLog, read_blob};
use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;

/// Limits on the editable files of one snapshot: total bytes, file count and directory depth.
pub(crate) const MAX_SNAPSHOT: usize = 16 << 20;
pub(crate) const MAX_FILES: usize = 10_000;
pub(crate) const MAX_DEPTH: usize = 32;
/// Characters of a failed evaluator's stdout and of its stderr kept for the owner (`failures/`).
const FAILURE_CHARS: usize = 4_000;

/// One editable file: its permission bits (an evaluator may run it, so the executable bits matter) and its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) mode: u32,
    pub(crate) bytes: Vec<u8>,
}

/// Editable files, path -> entry.
pub(super) type Files = BTreeMap<String, Entry>;

/// `name` in `dir`, opened without following a link; None when it is gone, a symlink or not openable as that kind.
fn open_at(dir: &File, name: &str, directory: bool) -> Result<Option<File>> {
    let c = std::ffi::CString::new(name)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | if directory { libc::O_DIRECTORY } else { 0 };
    let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
    if fd >= 0 { return Ok(Some(unsafe { File::from_raw_fd(fd) })); }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR | libc::ENXIO | libc::EOPNOTSUPP) => Ok(None),
        _ => Err(e.into()),
    }
}

/// An absolute path opened component by component without following a link; None when one is missing or a link.
fn open_nofollow(path: &Path) -> Result<Option<File>> {
    let parts: Vec<&str> = path.strip_prefix("/")?.iter().map(|c| c.to_str().context("non-UTF-8 path")).collect::<Result<_>>()?;
    let mut at = File::open("/")?;
    for (i, part) in parts.iter().enumerate() {
        match open_at(&at, part, i + 1 < parts.len())? {
            Some(next) => at = next,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

/// Every regular file under the editable paths of `root`, with its permission bits, within the snapshot limits.
/// Symlinks and special files are skipped: they never leave an episode. Names must be UTF-8 so they are recreated
/// exactly. Every entry is opened relative to its parent's descriptor without following a link and judged by what was
/// opened (fstat), so a process that swaps a path for a symlink to grader or held-out data between the listing and the
/// read gets nothing through.
pub(super) fn capture(root: &Path, editable: &[String]) -> Result<Files> {
    /// The names in an open directory.
    fn list(dir: &File) -> Result<Vec<String>> {
        let fd = unsafe { libc::dup(dir.as_raw_fd()) };
        ensure!(fd >= 0, "{}", std::io::Error::last_os_error());
        let stream = unsafe { libc::fdopendir(fd) };
        if stream.is_null() {
            unsafe { libc::close(fd); }
            bail!("{}", std::io::Error::last_os_error());
        }
        let mut names = vec![];
        let result = (|| -> Result<()> {
            loop {
                let entry = unsafe { libc::readdir(stream) };
                if entry.is_null() { return Ok(()); }
                let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
                if matches!(name.to_bytes(), b"." | b"..") { continue; }
                names.push(name.to_str().map_err(|_| anyhow::anyhow!("editable file name {name:?} is not UTF-8"))?.to_string());
            }
        })();
        unsafe { libc::closedir(stream); }
        result.map(|_| names)
    }
    fn walk(parent: &File, name: &str, rel: &str, depth: usize, out: &mut Files, total: &mut usize) -> Result<()> {
        let Some(file) = open_at(parent, name, false)? else { return Ok(()) };
        let meta = file.metadata()?;
        if meta.is_dir() {
            ensure!(depth < MAX_DEPTH, "editable directories nest deeper than {MAX_DEPTH} levels");
            for name in list(&file)? {
                walk(&file, &name, &format!("{rel}/{name}"), depth + 1, out, total)?;
            }
        } else if meta.is_file() {
            ensure!(out.len() < MAX_FILES, "editable paths hold more than {MAX_FILES} files");
            // Read at most what is left of the limit, so a file that keeps growing cannot get past it.
            let left = MAX_SNAPSHOT - *total;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(file, left as u64 + 1), &mut bytes)?;
            ensure!(bytes.len() <= left, "editable files exceed {} MiB", MAX_SNAPSHOT >> 20);
            *total += bytes.len();
            out.insert(rel.to_string(), Entry { mode: meta.permissions().mode() & 0o777, bytes });
        }
        Ok(())
    }
    let (mut out, mut total) = (BTreeMap::new(), 0);
    let top = File::open(root)?;
    'paths: for e in editable {
        let e = e.trim_end_matches('/');
        let parts: Vec<&str> = e.split('/').collect();
        // A symlinked parent directory is skipped like any symlink: nothing is read through it.
        let mut dir = top.try_clone()?;
        for part in &parts[..parts.len() - 1] {
            match open_at(&dir, part, true)? {
                Some(next) => dir = next,
                None => continue 'paths,
            }
        }
        walk(&dir, parts[parts.len() - 1], e, 0, &mut out, &mut total)?;
    }
    Ok(out)
}

/// Make the editable paths of `root` hold exactly `files`: write each file whose bytes or mode differ and remove each
/// file `capture` would take that `files` lacks. Nothing else is touched: symlinks and special files that capture skips
/// stay, and so do directories. Nothing is reached through a symlink: directories on the way are opened (or created)
/// without following links, and a snapshot path that runs into a symlink or a directory is refused. A missing `root` is
/// created (a portfolio directory).
pub(super) fn place(root: &Path, editable: &[String], files: &Files) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    place_checked(&root, editable, files, &|rel| Ok(root.join(rel)))
}

/// `place` into a canonical `root`, with each editable path resolved by `resolve` (an agent's workspace: the policy's
/// write check, so a reset goes where the agent's own `write` would).
pub(super) fn place_checked(root: &Path, editable: &[String], files: &Files, resolve: &dyn Fn(&str) -> Result<PathBuf>) -> Result<()> {
    let current = capture(root, editable)?;
    for rel in current.keys().filter(|rel| !files.contains_key(*rel)) {
        crate::atomic_file::Target::open(&resolve(rel)?, false)?.remove()?;
    }
    for (rel, entry) in files.iter().filter(|(rel, entry)| current.get(*rel) != Some(entry)) {
        let path = resolve(rel)?;
        if current.get(rel).is_none_or(|c| c.bytes != entry.bytes) {
            crate::atomic_file::Target::open(&path, true)?.replace(&entry.bytes)?;
        }
        let file = open_nofollow(&path)?.filter(|f| f.metadata().is_ok_and(|m| m.is_file()))
            .with_context(|| format!("{rel} is not a regular file"))?;
        file.set_permissions(std::fs::Permissions::from_mode(entry.mode & 0o777))?;
    }
    Ok(())
}

/// A file of a stored snapshot: its mode and the SHA-256 of its bytes.
#[derive(Serialize, Deserialize)]
struct Stored {
    mode: u32,
    blob: String,
}

/// Store a snapshot in the run's blob store, through the orchestrator's journal (only it writes the run): each file's
/// bytes once, content-addressed, however many snapshots hold them, then the manifest `{path: {mode, blob}}`, whose own
/// hash is the snapshot's id. Each blob is durable before the next is written, so a manifest names only blobs that are.
pub(super) fn save_snap(log: &EventLog, files: &Files) -> Result<String> {
    let mut stored = BTreeMap::new();
    for (rel, e) in files { stored.insert(rel, Stored { mode: e.mode & 0o777, blob: log.blob(&e.bytes)? }); }
    log.blob(&serde_json::to_vec(&stored)?)
}

/// A stored snapshot; a damaged manifest or blob is an error, never other bytes.
pub(super) fn load_snap(dir: &Path, id: &str) -> Result<Files> {
    let stored: BTreeMap<String, Stored> = serde_json::from_slice(&read_blob(dir, id)?).with_context(|| format!("snapshot {id}"))?;
    stored.into_iter().map(|(rel, s)| Ok((rel, Entry { mode: s.mode & 0o777, bytes: read_blob(dir, &s.blob)? }))).collect()
}

/// Files as they cross an episode's channel: `{path: {mode, data}}`, the bytes in base64.
pub(super) fn to_wire(files: &Files) -> Value {
    Value::Object(files.iter().map(|(rel, e)| (rel.clone(),
        json!({"mode": e.mode & 0o777, "data": base64::engine::general_purpose::STANDARD.encode(&e.bytes)}))).collect())
}

/// Files from an episode's channel, held to what `capture` could have produced: paths inside the editable paths without
/// `.`, `..` or empty components, and the snapshot limits.
pub(super) fn from_wire(editable: &[String], wire: &Value) -> Result<Files> {
    let map = wire.as_object().context("files must be an object")?;
    ensure!(map.len() <= MAX_FILES, "editable paths hold more than {MAX_FILES} files");
    let (mut out, mut total) = (Files::new(), 0);
    for (rel, v) in map {
        let parts: Vec<&str> = rel.split('/').collect();
        ensure!(parts.len() <= MAX_DEPTH + 8 && parts.iter().all(|c| !c.is_empty() && *c != "." && *c != "..")
            && editable.iter().any(|e| { let e = e.trim_end_matches('/'); rel == e || rel.starts_with(&format!("{e}/")) }),
            "{rel:?} is not an editable file");
        let bytes = base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().context("file data")?)?;
        total += bytes.len();
        ensure!(total <= MAX_SNAPSHOT, "editable files exceed {} MiB", MAX_SNAPSHOT >> 20);
        out.insert(rel.clone(), Entry { mode: v["mode"].as_u64().context("file mode")? as u32 & 0o777, bytes });
    }
    Ok(out)
}

/// A fresh private directory in the system temp dir; removed on drop. Nothing a process started in it can still be
/// running then: shell commands and evaluators run isolated (`crate::confine::Isolate`), so whatever they left behind
/// died with them.
pub(super) struct Scratch(pub(super) PathBuf);

/// The copies an earlier orchestrator left for attempt `label` of the run `run` (its directory name): direct,
/// non-symlink directories in the system temp dir named as `Scratch::episode` names them. Only these are removed on
/// resume; nothing in the journal names a path to delete.
pub(super) fn left_copies(run: &str, label: &str) -> Vec<PathBuf> {
    let Ok(base) = std::fs::canonicalize(std::env::temp_dir()) else { return vec![] };
    let tail = format!("-{}-{label}.noindex", run.trim_end_matches(".noindex"));
    std::fs::read_dir(&base).into_iter().flatten().flatten().filter(|e| {
        let name = e.file_name();
        let pid = name.to_str().and_then(|n| n.strip_prefix("hx-ar-ws-")).and_then(|n| n.strip_suffix(tail.as_str()));
        pid.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            && e.file_type().is_ok_and(|t| t.is_dir() && !t.is_symlink())
    }).map(|e| e.path()).collect()
}
/// The scratch directories this process holds, removed by `remove_scratch` when it is told to stop (it then runs no
/// destructors).
static LIVE: std::sync::Mutex<std::collections::BTreeSet<PathBuf>> = std::sync::Mutex::new(std::collections::BTreeSet::new());

impl Scratch {
    /// A new directory `hx-ar-KIND-PID-NANOS-SEQ.noindex` in the system temp dir.
    pub(super) fn new(kind: &str) -> Result<Scratch> {
        Scratch::new_in(&std::fs::canonicalize(std::env::temp_dir())?, kind)
    }

    fn new_in(parent: &Path, kind: &str) -> Result<Scratch> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Scratch::named(parent.join(format!("hx-ar-{kind}-{}-{nanos}-{seq}.noindex", std::process::id())))
    }

    /// An episode attempt's copy, `hx-ar-ws-PID-RUN-LABEL.noindex`: named from its run and label, so a resumed
    /// orchestrator finds what its predecessor left (`left_copies`), and from this process, so any later process
    /// removes it once this one is gone (`sweep_dead`).
    pub(super) fn episode(run: &str, label: &str) -> Result<Scratch> {
        let base = std::fs::canonicalize(std::env::temp_dir())?;
        Scratch::named(base.join(format!("hx-ar-ws-{}-{}-{label}.noindex", std::process::id(), run.trim_end_matches(".noindex"))))
    }

    fn named(path: PathBuf) -> Result<Scratch> {
        // `.noindex`: Spotlight skips it, so the copies a run churns through are never indexed.
        std::fs::create_dir(&path)?;
        LIVE.lock().unwrap().insert(path.clone());
        Ok(Scratch(path))
    }
}

/// Remove a scratch directory, first opening up the directory without permissions an overlay leaves in its work
/// directory (here or in an evaluation copy inside), which its owner may open again.
fn remove(dir: &Path) {
    let unlock = |d: &Path| { let _ = std::fs::set_permissions(d.join("work").join("work"), std::fs::Permissions::from_mode(0o700)); };
    unlock(dir);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() { unlock(&entry.path()); }
    let _ = std::fs::remove_dir_all(dir);
}

impl Drop for Scratch {
    fn drop(&mut self) {
        remove(&self.0);
        LIVE.lock().unwrap().remove(&self.0);
    }
}

/// Episode copies whose child journal (`COPY/log`) must reach the run before the copy goes, with where it goes: a child
/// cannot write the run directory, so its journal lives in its copy until the orchestrator keeps it.
static KEEP: std::sync::Mutex<std::collections::BTreeMap<PathBuf, PathBuf>> = std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Keep `copy`'s child journal at `to` if the copy is removed by `remove_scratch` (a stop) before it is kept normally.
pub(super) fn keep_on_stop(copy: &Path, to: &Path) { KEEP.lock().unwrap().insert(copy.to_path_buf(), to.to_path_buf()); }

/// The copy's journal was kept (or the copy is gone): nothing to do on a stop.
pub(super) fn kept(copy: &Path) { KEEP.lock().unwrap().remove(copy); }

/// Remove every scratch directory this process holds; `crate::prims::stop` calls it on SIGTERM, SIGINT or SIGHUP, after
/// killing the commands that use them. An episode copy's child journal is kept with the run first, as when an episode
/// ends normally or resume closes it.
pub(crate) fn remove_scratch() {
    for (copy, to) in std::mem::take(&mut *KEEP.lock().unwrap()) {
        if copy.join("log").is_dir() && !to.exists() { let _ = copy_tree(&copy.join("log"), &to); }
    }
    let live = std::mem::take(&mut *LIVE.lock().unwrap());
    for dir in live { remove(&dir); }
}

/// Remove the episode and evaluation copies in the system temp dir whose process is gone (killed with SIGKILL, so it
/// removed nothing; a resume would clean only its own run's). Once per process, at its first evaluation.
fn sweep() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| if let Ok(base) = std::fs::canonicalize(std::env::temp_dir()) { sweep_dead(&base) });
}

fn sweep_dead(base: &Path) {
    for entry in std::fs::read_dir(base).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("hx-ar-eval-").or_else(|| n.strip_prefix("hx-ar-ws-")))
            .filter(|n| n.ends_with(".noindex")).and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<i32>().ok()).filter(|p| *p > 0) else { continue };
        let gone = unsafe { libc::kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone && entry.file_type().is_ok_and(|t| t.is_dir()) { remove(&entry.path()); }
    }
}

/// Copy the tree `from` to the new path `to`, as copy-on-write clones where the filesystem has them (APFS `cp -c`,
/// Btrfs and XFS `--reflink=auto`). Whether this system's `cp` takes the flag is learned on the first copy, so later
/// copies spawn `cp` once.
pub(super) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    use std::sync::atomic::{AtomicU8, Ordering};
    // 0: not known yet, 1: clones work, 2: plain copies only.
    static CLONES: AtomicU8 = AtomicU8::new(0);
    let clone: &[&str] = if cfg!(target_os = "macos") { &["-cR"] } else { &["-R", "--reflink=auto"] };
    let tries: &[&[&str]] = match CLONES.load(Ordering::Relaxed) { 1 => &[clone], 2 => &[&["-R"]], _ => &[clone, &["-R"]] };
    for flags in tries {
        let status = std::process::Command::new("cp").args(*flags).arg(from).arg(to)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?;
        if status.success() {
            let _ = CLONES.compare_exchange(0, if *flags == clone { 1 } else { 2 }, Ordering::Relaxed, Ordering::Relaxed);
            return Ok(());
        }
        let _ = std::fs::remove_dir_all(to);
    }
    bail!("could not copy {} to {}", from.display(), to.display())
}

/// Bytes of an evaluator's report as JSON kept whole; it goes into every ledger row and back to the agent.
const STORED_REPORT: usize = 16 << 10;

/// An evaluator's report, whole when its JSON is at most `STORED_REPORT` bytes; otherwise its scalar top-level fields
/// (the scores and counts an agent compares) and the start of the whole.
pub(super) fn cap_report(report: Value) -> Value {
    let text = report.to_string();
    if text.len() <= STORED_REPORT { return report; }
    let mut out = serde_json::Map::new();
    if let Value::Object(map) = &report {
        let scalar = |v: &Value| matches!(v, Value::Number(_) | Value::Bool(_)) || v.as_str().is_some_and(|s| s.len() <= 200);
        for (k, v) in map.iter().filter(|(k, v)| k.len() <= 100 && scalar(v)).take(100) { out.insert(k.clone(), v.clone()); }
    }
    out.insert("truncated".into(), json!(clip(&text, REPORT_CHARS)));
    Value::Object(out)
}

/// Evaluator runs a run may have at once (the orchestrator runs them all): the spec's `eval_jobs`, or half the CPUs and
/// at least one, so search evaluations side by side neither starve each other nor time out by load.
pub(super) fn eval_jobs(spec: &Spec) -> usize {
    spec.eval_jobs.filter(|n| *n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1)))
}

/// Mounting an evaluator's tree as an overlay failed once here: copy from then on.
static NO_OVERLAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The base's editable files, read once per run: a snapshot that lacks one cannot be an overlay on the base.
fn base_files(base: &Path, spec: &Spec) -> Result<Vec<String>> {
    static CACHE: std::sync::Mutex<Option<(PathBuf, Vec<String>)>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap();
    if let Some((at, files)) = cache.as_ref().filter(|(at, _)| at == base) { let _ = at; return Ok(files.clone()); }
    let files: Vec<String> = capture(base, &spec.editable)?.into_keys().collect();
    *cache = Some((base.to_path_buf(), files.clone()));
    Ok(files)
}

/// The evaluator's tree as an overlay, where namespaces can mount one: the snapshot in a layer of its own over the frozen
/// base, writes going to a scratch layer, so an evaluation copies nothing of the base. A snapshot that deletes a file of
/// the base would need a whiteout, which only a privileged mount writes: that one is copied instead (None).
fn overlay(spec: &Spec, run: &Path, files: &Files, scratch: &Scratch) -> Result<Option<crate::confine::Overlay>> {
    if NO_OVERLAY.load(std::sync::atomic::Ordering::Relaxed) || !matches!(crate::confine::isolation(), Ok(crate::confine::Isolation::Namespaces)) {
        return Ok(None);
    }
    let base = run.join("base");
    if !base_files(&base, spec)?.iter().all(|f| files.contains_key(f)) { return Ok(None); }
    let overlay = crate::confine::Overlay { lower: vec![scratch.0.join("snap"), base], upper: scratch.0.join("upper"),
        work: scratch.0.join("work"), target: scratch.0.join("tree") };
    let paths: Vec<&Path> = overlay.lower.iter().chain([&overlay.upper, &overlay.work, &overlay.target]).map(PathBuf::as_path).collect();
    if !crate::confine::Overlay::expressible(&paths) { return Ok(None); }
    for dir in [&overlay.lower[0], &overlay.upper, &overlay.work, &overlay.target] { std::fs::create_dir(dir)?; }
    place(&overlay.lower[0], &spec.editable, files)?;
    Ok(Some(overlay))
}

/// Run an evaluator on `files` placed into a fresh tree of the frozen base. Returns (score, valid, se, report).
/// Failures of the evaluator become invalid results, not errors. The evaluator runs isolated, with no network
/// (`crate::confine::Isolate`): every process it starts is gone when this returns, and it sees no other process. Where
/// Landlock is available it may read only the system, its scratch directory and its `grader` roots (plus `holdout`
/// for the select evaluator), and write only its scratch directory. Candidate code running inside it can read what it
/// reads, so a failed evaluator's output is never returned (that would carry grader or held-out files to the agent):
/// the result says only how it failed, and the output is kept for the owner in the run's `failures/SNAP.txt`.
pub(super) async fn evaluate(spec: &Spec, run: &Path, snap: &str, files: &Files, cmd: &str, holdout: bool) -> Result<(Option<f64>, bool, Option<f64>, Value)> {
    sweep();
    let scratch = Scratch::new("eval")?;
    let tree = scratch.0.join("tree");
    let tmp = scratch.0.join("tmp");
    std::fs::create_dir(&tmp)?;
    let mut read = crate::policy::system_read_roots();
    read.extend(spec.roots(holdout)?);
    read.push(scratch.0.clone());
    let rules = crate::confine::Rules { read, write: vec![scratch.0.clone()], hidden: vec![], exposed: vec![], scoped: false }.with_system().isolated();
    let confine = rules.prepare()?.map(std::sync::Arc::new);
    let mut layers = overlay(spec, run, files, &scratch)?;
    let (stdout, stderr, code, timed_out, _) = loop {
        if layers.is_none() {
            let _ = std::fs::remove_dir(&tree);
            copy_tree(&run.join("base"), &tree)?;
            place(&tree, &spec.editable, files)?;
        }
        let env = crate::prims::ShellEnv { env: Some(evaluator_env(&tmp)), confine: confine.clone(),
            isolate: Some(crate::confine::Isolate { net: true, overlay: layers.clone() }) };
        match crate::prims::shell(&env, cmd, tree.clone(), spec.timeout).await {
            Err(e) if layers.is_some() => {
                eprintln!("hx ar: an evaluator's tree cannot be mounted as an overlay here ({e}); copying the base instead");
                NO_OVERLAY.store(true, std::sync::atomic::Ordering::Relaxed);
                layers = None;
            }
            result => break result.map_err(|e| anyhow::anyhow!("{e}"))?,
        }
    };
    let stdout = String::from_utf8_lossy(&stdout);
    let parsed = stdout.lines().rev().find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(Value::is_object);
    let Some(obj) = parsed.filter(|_| code == Some(0) && !timed_out) else {
        let why = match code {
            _ if timed_out => format!("the evaluator timed out after {}s", spec.timeout),
            Some(0) => "the evaluator's last line is not a JSON object".to_string(),
            Some(c) => format!("the evaluator exited with status {c}"),
            None => "the evaluator was killed by a signal".to_string(),
        };
        let tail = |s: &str| s.chars().rev().take(FAILURE_CHARS).collect::<Vec<_>>().into_iter().rev().collect::<String>();
        let kept = run.join("failures");
        let _ = std::fs::create_dir_all(&kept).and_then(|_| std::fs::write(kept.join(format!("{snap}.txt")),
            format!("{why}\n--- stdout (end)\n{}\n--- stderr (end)\n{}\n", tail(&stdout), tail(&String::from_utf8_lossy(&stderr)))));
        return Ok((None, false, None, json!({"error": format!("{why}; its output is kept for the owner, not shown to agents")})));
    };
    let score = obj["score"].as_f64().filter(|s| s.is_finite());
    let valid = score.is_some() && obj["valid"].as_bool() != Some(false);
    let se = obj["se"].as_f64().filter(|s| s.is_finite() && *s >= 0.0);
    Ok((score, valid, se, cap_report(obj.get("report").cloned().unwrap_or(Value::Null))))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-ar-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    #[test]
    fn capture_reads_files_and_never_follows_a_link() {
        let dir = temp("capture");
        let (ws, secret) = (dir.join("ws"), dir.join("holdout"));
        std::fs::create_dir_all(ws.join("pkg/sub")).unwrap();
        std::fs::create_dir_all(ws.join("other")).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("answers.txt"), "SECRET").unwrap();
        std::fs::write(ws.join("pkg/a.txt"), "a").unwrap();
        std::fs::write(ws.join("pkg/sub/b.bin"), [0u8, 255]).unwrap();
        std::os::unix::fs::symlink(secret.join("answers.txt"), ws.join("pkg/link.txt")).unwrap();
        std::os::unix::fs::symlink(&secret, ws.join("pkg/linkdir")).unwrap();
        std::os::unix::fs::symlink(&secret, ws.join("linked")).unwrap();
        let spec: Spec = toml::from_str("editable = [\"pkg/\", \"linked/answers.txt\", \"missing/x\", \"other/none\"]\neval = \"true\"").unwrap();
        let files = capture(&ws, &spec.editable).unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), vec!["pkg/a.txt", "pkg/sub/b.bin"]);
        assert_eq!(files["pkg/sub/b.bin"].bytes, vec![0u8, 255]);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn entry(mode: u32, bytes: &str) -> Entry { Entry { mode, bytes: bytes.as_bytes().to_vec() } }

    #[test]
    fn snapshots_share_blobs_and_cross_the_channel_checked() {
        let dir = temp("snaps");
        std::fs::write(dir.join("journal.jsonl"), "").unwrap();
        let log = EventLog::reopen(&dir.join("journal.jsonl")).unwrap();
        let files = Files::from([("x.txt".to_string(), entry(0o644, "7\n")), ("run.sh".to_string(), entry(0o755, "#!/bin/sh\n"))]);
        let id = save_snap(&log, &files).unwrap();
        assert_eq!(save_snap(&log, &files).unwrap(), id);
        assert_eq!(load_snap(&dir, &id).unwrap(), files, "bytes and modes come back");
        // The manifest names modes and blobs; no file is stored as base64 in it.
        let manifest: Value = serde_json::from_slice(&read_blob(&dir, &id).unwrap()).unwrap();
        assert_eq!(manifest["run.sh"]["mode"], json!(0o755));
        // A second snapshot that changes one file stores one more blob (and its manifest), and a mode change only a manifest.
        let mut next = files.clone();
        next.insert("x.txt".into(), entry(0o644, "8\n"));
        next.insert("run.sh".into(), entry(0o644, "#!/bin/sh\n"));
        let other = save_snap(&log, &next).unwrap();
        assert_ne!(other, id, "a mode is part of the snapshot");
        assert_eq!(std::fs::read_dir(dir.join("blobs")).unwrap().count(), 5);
        // A damaged blob is an error, never other bytes.
        std::fs::write(dir.join("blobs").join(crate::eventlog::sha_bytes(b"7\n")), "9\n").unwrap();
        assert!(load_snap(&dir, &id).unwrap_err().to_string().contains("mismatch"));
        // What crosses the channel comes back exactly, and only editable files within the limits do.
        let editable = vec!["x.txt".to_string(), "pkg/".to_string()];
        let sent = Files::from([("x.txt".to_string(), entry(0o755, "1")), ("pkg/a/b".to_string(), entry(0o600, "\u{0}"))]);
        assert_eq!(from_wire(&editable, &to_wire(&sent)).unwrap(), sent);
        for bad in ["y.txt", "pkg/../y", "pkg//a", "pkg/./a", "/x.txt", "x.txt/"] {
            assert!(from_wire(&editable, &json!({bad: {"mode": 420, "data": ""}})).is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn place_restores_modes_and_touches_only_captured_files() {
        let dir = temp("place");
        let ws = dir.join("ws");
        std::fs::create_dir_all(ws.join("pkg/sub")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        std::fs::write(ws.join("pkg/run.sh"), "old").unwrap();
        std::fs::write(ws.join("pkg/extra.txt"), "x").unwrap();
        std::fs::write(ws.join("pkg/same.txt"), "same").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", ws.join("pkg/link")).unwrap();
        std::os::unix::fs::symlink(dir.join("elsewhere"), ws.join("pkg/linkdir")).unwrap();
        let spec: Spec = toml::from_str("editable = [\"pkg\"]\neval = \"true\"").unwrap();
        let before = std::fs::metadata(ws.join("pkg/same.txt")).unwrap().modified().unwrap();
        let files = Files::from([("pkg/run.sh".to_string(), entry(0o755, "new")), ("pkg/same.txt".to_string(), entry(0o644, "same")),
            ("pkg/sub/deep/n.txt".to_string(), entry(0o600, "n"))]);
        place(&ws, &spec.editable, &files).unwrap();
        assert_eq!(capture(&ws, &spec.editable).unwrap(), files, "exactly the snapshot, modes included");
        assert!(std::fs::symlink_metadata(ws.join("pkg/link")).unwrap().file_type().is_symlink(), "a skipped symlink stays");
        assert!(std::fs::symlink_metadata(ws.join("pkg/linkdir")).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::metadata(ws.join("pkg/same.txt")).unwrap().modified().unwrap(), before, "an unchanged file is not rewritten");
        // A snapshot path through a symlinked directory is refused, and nothing is written through it.
        let through = Files::from([("pkg/linkdir/planted.txt".to_string(), entry(0o644, "p"))]);
        assert!(place(&ws, &spec.editable, &through).is_err());
        assert!(!dir.join("elsewhere/planted.txt").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A run directory with a frozen base for evaluator tests.
    fn run_with_base(tag: &str, base: &[(&str, &str, u32)]) -> (PathBuf, Spec) {
        let run = temp(tag);
        for (rel, text, mode) in base {
            let path = run.join("base").join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(*mode)).unwrap();
        }
        (run, toml::from_str("editable = [\"pkg\"]\neval = \"true\"\ntimeout = 20").unwrap())
    }

    fn isolated() -> bool {
        let ok = crate::confine::isolation().is_ok();
        if !ok { eprintln!("skipped: no process isolation here"); }
        ok
    }

    #[tokio::test]
    async fn evaluators_run_with_modes_alone_and_leave_nothing_behind() {
        if !isolated() { return; }
        let (run, spec) = run_with_base("eval-iso", &[("pkg/run.sh", "#!/bin/sh\necho base\n", 0o755), ("pkg/gone.txt", "g", 0o644), ("data.txt", "d", 0o644)]);
        let marker = format!("hx-eval-leftover-{}", std::process::id());
        let script = format!("#!/bin/sh\n(setsid bash -c 'exec -a {marker} sleep 300' >/dev/null 2>&1 &)\nsleep 0.2\nn=$(ls -d /proc/[0-9]* | wc -l)\n\
h=$(grep -l '^[h]{}' /proc/[0-9]*/cmdline | wc -l)\necho \"{{\\\"score\\\": $n, \\\"report\\\": {{\\\"held\\\": $h, \\\"gone\\\": $(test -e pkg/gone.txt && echo 1 || echo 0), \\\"data\\\": \\\"$(cat data.txt)\\\"}}}}\"\n", &marker[1..]);
        // With every base file kept, the tree is an overlay; deleting one makes it a copy. Both see the same.
        for keep in [true, false] {
            let mut files = Files::from([("pkg/run.sh".to_string(), Entry { mode: 0o755, bytes: script.clone().into_bytes() })]);
            if keep { files.insert("pkg/gone.txt".into(), entry(0o644, "g")); }
            let (score, valid, _, report) = evaluate(&spec, &run, "s", &files, "./pkg/run.sh", false).await.unwrap();
            assert!(valid, "{report}");
            assert!(score.unwrap() <= 6.0 && report["held"] == 1, "the evaluator sees only its own processes, its leftover among them: {score:?} {report}");
            assert_eq!((report["gone"].clone(), report["data"].clone()), (json!(keep as u8), json!("d")), "{report}");
            let left = std::fs::read_dir("/proc").unwrap().flatten().filter(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c.starts_with(marker.as_bytes()))).count();
            assert_eq!(left, 0, "a process the evaluator left behind is still running");
        }
        assert!(std::fs::read_dir(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap().flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with(&format!("hx-ar-eval-{}-", std::process::id())) || LIVE.lock().unwrap().contains(&e.path())),
            "an evaluation copy was left behind");
        let _ = std::fs::remove_dir_all(run);
    }

    #[tokio::test]
    async fn a_failed_evaluator_returns_no_output() {
        if !isolated() { return; }
        let (run, spec) = run_with_base("eval-fail", &[("pkg/a.txt", "a", 0o644)]);
        let files = Files::from([("pkg/a.txt".to_string(), entry(0o644, "a"))]);
        let (score, valid, _, report) = evaluate(&spec, &run, "s", &files, "echo GRADER-SECRET-OUT; echo GRADER-SECRET-ERR >&2; exit 3", false).await.unwrap();
        assert_eq!((score, valid), (None, false));
        assert!(!report.to_string().contains("GRADER-SECRET") && report["error"].as_str().unwrap().contains("status 3"), "{report}");
        let kept = std::fs::read_to_string(run.join("failures/s.txt")).unwrap();
        assert!(kept.contains("GRADER-SECRET-OUT") && kept.contains("GRADER-SECRET-ERR"), "the owner keeps the output: {kept}");
        let (_, valid, _, report) = evaluate(&spec, &run, "s", &files, "echo '{\"score\": 1, \"report\": {\"ok\": true}}' ; exit 1", false).await.unwrap();
        assert!(!valid && report.get("ok").is_none(), "{report}");
        let _ = std::fs::remove_dir_all(run);
    }

    #[test]
    fn scratch_copies_go_on_stop_and_after_a_kill() {
        let held = Scratch::new("eval").unwrap();
        std::fs::create_dir_all(held.0.join("work/work")).unwrap();
        std::fs::set_permissions(held.0.join("work/work"), std::fs::Permissions::from_mode(0)).unwrap();
        let path = held.0.clone();
        std::mem::forget(held);
        // What `remove_scratch` removes (tests share the process, so this one removes only its own).
        assert!(LIVE.lock().unwrap().remove(&path), "a scratch copy is registered for removal on stop");
        remove(&path);
        assert!(!path.exists(), "a stopping process removes its scratch copies");
        // A copy whose process was killed outright is removed by a later process's first evaluation.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead = child.id();
        child.wait().unwrap();
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let orphans = [base.join(format!("hx-ar-eval-{dead}-1-1.noindex")), base.join(format!("hx-ar-ws-{dead}-1-1.noindex"))];
        let mine = base.join(format!("hx-ar-ws-{}-1-1.noindex", std::process::id()));
        for dir in orphans.iter().chain([&mine]) { std::fs::create_dir_all(dir.join("tree")).unwrap(); }
        sweep_dead(&base);
        assert!(orphans.iter().all(|o| !o.exists()) && mine.exists(), "only copies of a process that is gone are removed");
        let _ = std::fs::remove_dir_all(mine);
    }

    #[test]
    fn a_resumed_run_finds_only_its_own_episode_copies() {
        let (run, other) = (format!("1-{}.noindex", std::process::id()), format!("2-{}.noindex", std::process::id()));
        let mine = Scratch::episode(&run, "r1-w3.2").unwrap();
        let theirs = Scratch::episode(&other, "r1-w3.2").unwrap();
        let sibling = Scratch::episode(&run, "r1-w3").unwrap();
        assert_eq!(left_copies(&run, "r1-w3.2"), vec![mine.0.clone()]);
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let link = base.join(format!("hx-ar-ws-1-{}-r1-w4.noindex", run.trim_end_matches(".noindex")));
        std::os::unix::fs::symlink(&mine.0, &link).unwrap();
        assert!(left_copies(&run, "r1-w4").is_empty(), "a symlinked copy is not removed through");
        let _ = std::fs::remove_file(&link);
        drop((theirs, sibling));
        let gone = mine.0.clone();
        drop(mine);
        assert!(!gone.exists() && left_copies(&run, "r1-w3.2").is_empty());
        let spec: Spec = toml::from_str("editable = [\"x\"]\neval = \"true\"").unwrap();
        assert!(eval_jobs(&spec) >= 1);
    }

    #[test]
    fn large_reports_keep_their_scores() {
        let small = json!({"score": 1, "notes": "fine"});
        assert_eq!(cap_report(small.clone()), small);
        let big = cap_report(json!({"sharpe": 1.5, "valid": true, "name": "x", "trades": vec![0; 100_000]}));
        assert!(big.to_string().len() < 2 * REPORT_CHARS, "{}", big.to_string().len());
        assert_eq!((big["sharpe"].clone(), big["valid"].clone(), big["name"].clone()), (json!(1.5), json!(true), json!("x")));
        assert!(big["trades"].is_null() && big["truncated"].as_str().unwrap().ends_with("(truncated)"));
        assert!(cap_report(json!(vec!["y".repeat(100); 1000]))["truncated"].is_string());
    }
}
