//! What crosses an episode's boundary: snapshots of the editable files, the throwaway copies episodes and evaluators
//! run in, the evaluator itself, and the committed tree frozen at launch.
use super::*;
use base64::Engine as _;
use std::collections::BTreeMap;

/// Limits on the editable files of one snapshot: total bytes, file count and directory depth.
pub(crate) const MAX_SNAPSHOT: usize = 16 << 20;
pub(crate) const MAX_FILES: usize = 10_000;
pub(crate) const MAX_DEPTH: usize = 32;

/// Editable files, path -> bytes.
pub(super) type Files = BTreeMap<String, Vec<u8>>;

/// Every regular file under the editable paths of `root`, within the snapshot limits. Symlinks and special files are
/// skipped: they never leave an episode. Names must be UTF-8 so they are recreated exactly. Every entry is opened
/// relative to its parent's descriptor without following a link and judged by what was opened (fstat), so a process
/// that swaps a path for a symlink to grader or held-out data between the listing and the read gets nothing through.
pub(super) fn capture(root: &Path, spec: &Spec) -> Result<Files> {
    use std::os::fd::{AsRawFd, FromRawFd};
    /// `name` in `dir`, opened without following a link; None when it is gone, a symlink or not openable as a file.
    fn open_at(dir: &std::fs::File, name: &str, directory: bool) -> Result<Option<std::fs::File>> {
        let c = std::ffi::CString::new(name)?;
        let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | if directory { libc::O_DIRECTORY } else { 0 };
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), flags) };
        if fd >= 0 { return Ok(Some(unsafe { std::fs::File::from_raw_fd(fd) })); }
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ENOENT | libc::ELOOP | libc::ENOTDIR | libc::ENXIO | libc::EOPNOTSUPP) => Ok(None),
            _ => Err(e.into()),
        }
    }
    /// The names in an open directory.
    fn list(dir: &std::fs::File) -> Result<Vec<String>> {
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
    fn walk(parent: &std::fs::File, name: &str, rel: &str, depth: usize, out: &mut Files, total: &mut usize) -> Result<()> {
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
            out.insert(rel.to_string(), bytes);
        }
        Ok(())
    }
    let (mut out, mut total) = (BTreeMap::new(), 0);
    let top = std::fs::File::open(root)?;
    'paths: for e in &spec.editable {
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

/// Kill every process that still has its working directory or an open file under `dir` (agents' and evaluators'
/// detached leftovers), before the directory is deleted.
pub(super) fn reap(dir: &Path) {
    let Ok(out) = std::process::Command::new("lsof").args(["-t", "+D"]).arg(dir).stderr(std::process::Stdio::null()).output() else { return };
    let me = std::process::id() as i32;
    for pid in String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse::<i32>().ok()).filter(|p| *p != me) {
        unsafe { libc::kill(pid, libc::SIGKILL); }
    }
}

/// Make the editable paths of `root` hold exactly `files`.
pub(super) fn place(root: &Path, spec: &Spec, files: &Files) -> Result<()> {
    for e in &spec.editable {
        let path = root.join(e.trim_end_matches('/'));
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&path)?,
            Ok(_) => std::fs::remove_file(&path)?,
            Err(_) => {}
        }
    }
    for (rel, bytes) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// Store a snapshot content-addressed under `dir/snaps`; returns its id.
pub(super) fn save_snap(dir: &Path, files: &Files) -> Result<String> {
    let encoded: BTreeMap<&String, String> = files.iter()
        .map(|(k, v)| (k, base64::engine::general_purpose::STANDARD.encode(v))).collect();
    let bytes = serde_json::to_vec(&encoded)?;
    let id = crate::eventlog::sha_bytes(&bytes)[..16].to_string();
    let path = dir.join("snaps").join(format!("{id}.json"));
    // Written whole or not at all, so one that exists is complete (one torn by an older hx is shorter: rewritten).
    if std::fs::metadata(&path).map_or(true, |m| m.len() != bytes.len() as u64) {
        std::fs::create_dir_all(path.parent().unwrap())?;
        crate::atomic_file::replace_trusted(&path, &bytes)?;
    }
    Ok(id)
}

pub(super) fn load_snap(dir: &Path, id: &str) -> Result<Files> {
    let encoded: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(dir.join("snaps").join(format!("{id}.json")))?)?;
    encoded.into_iter().map(|(k, v)| Ok((k, base64::engine::general_purpose::STANDARD.decode(v)?))).collect()
}

/// A fresh private directory in the system temp dir; removed on drop.
pub(super) struct Scratch(pub(super) PathBuf);

/// A recorded scratch path is not authority to kill processes and delete files: only a direct, non-symlink child of
/// the system temp directory with the name `Scratch::new("ws")` gives an episode's copy.
pub(super) fn valid_scratch(path: &Path) -> bool {
    let Ok(base) = std::fs::canonicalize(std::env::temp_dir()) else { return false };
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let shaped = name.strip_prefix("hx-ar-ws-").and_then(|n| n.strip_suffix(".noindex")).is_some_and(|n| {
        let parts: Vec<&str> = n.split('-').collect();
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 20 && p.bytes().all(|b| b.is_ascii_digit()))
    });
    shaped && path.parent() == Some(base.as_path())
        && std::fs::symlink_metadata(path).map_or(true, |m| m.is_dir() && !m.file_type().is_symlink())
}
impl Scratch {
    pub(super) fn new(kind: &str) -> Result<Scratch> {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // `.noindex`: Spotlight skips it, so the copies a run churns through are never indexed.
        let path = std::fs::canonicalize(std::env::temp_dir())?.join(format!("hx-ar-{kind}-{}-{nanos}-{seq}.noindex", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Scratch(path))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        reap(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
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

/// Run an evaluator on `files` placed into a fresh copy of the frozen base. Returns (score, valid, se, report).
/// Failures of the evaluator become invalid results, not errors. Where Landlock is available the evaluator may read
/// only the system, its scratch copy and its `grader` roots (plus `holdout` for the select evaluator), and write only
/// its scratch copy.
pub(super) async fn evaluate(spec: &Spec, run: &Path, files: &Files, cmd: &str, holdout: bool) -> Result<(Option<f64>, bool, Option<f64>, Value)> {
    let scratch = Scratch::new("eval")?;
    let tree = scratch.0.join("tree");
    copy_tree(&run.join("base"), &tree)?;
    place(&tree, spec, files)?;
    let tmp = scratch.0.join("tmp");
    std::fs::create_dir(&tmp)?;
    let mut read = crate::policy::system_read_roots();
    read.extend(spec.roots(holdout)?);
    read.push(scratch.0.clone());
    let rules = crate::confine::Rules { read, write: vec![scratch.0.clone()], hidden: vec![], exposed: vec![] }.with_system().without_shm();
    let env = crate::prims::ShellEnv { env: Some(evaluator_env(&tmp)), confine: rules.prepare()?.map(std::sync::Arc::new) };
    let (stdout, stderr, code, timed_out, _) = crate::prims::shell(&env, cmd, tree, spec.timeout).await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let stdout = String::from_utf8_lossy(&stdout);
    let tail = |s: &str| s.chars().rev().take(2000).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    let parsed = stdout.lines().rev().find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(Value::is_object);
    let Some(obj) = parsed.filter(|_| code == Some(0) && !timed_out) else {
        let why = if timed_out { "evaluator timed out".to_string() } else { format!("evaluator exit {code:?} or no JSON result line") };
        return Ok((None, false, None, json!({"error": why, "stdout": tail(&stdout), "stderr": tail(&String::from_utf8_lossy(&stderr))})));
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

/// How often a child without a report was rate-limited: the HTTP 429s its journals recorded.
pub(super) fn rate_limited(logs: &Path) -> u64 {
    std::fs::read_dir(logs).into_iter().flatten().flatten().map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter_map(|j| crate::eventlog::Books::of(&crate::eventlog::load(&j).ok()?).ok())
        .map(|books| books.usage.rate_limits).sum()
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
        let files = capture(&ws, &spec).unwrap();
        assert_eq!(files.keys().collect::<Vec<_>>(), vec!["pkg/a.txt", "pkg/sub/b.bin"]);
        assert_eq!(files["pkg/sub/b.bin"], vec![0u8, 255]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn snapshots_are_written_whole() {
        let dir = temp("snaps");
        let files = Files::from([("x.txt".to_string(), b"7\n".to_vec())]);
        let id = save_snap(&dir, &files).unwrap();
        // A snapshot torn by a killed writer is rewritten, not taken as stored.
        let path = dir.join("snaps").join(format!("{id}.json"));
        let whole = std::fs::read(&path).unwrap();
        std::fs::write(&path, &whole[..whole.len() / 2]).unwrap();
        assert_eq!(save_snap(&dir, &files).unwrap(), id);
        assert_eq!(load_snap(&dir, &id).unwrap(), files);
        assert_eq!(std::fs::read_dir(dir.join("snaps")).unwrap().count(), 1, "a temporary file was left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_episode_scratch_copies_are_cleaned_up() {
        let scratch = Scratch::new("ws").unwrap();
        assert!(valid_scratch(&scratch.0));
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let link = base.join(format!("hx-ar-ws-{}-1-999.noindex", std::process::id()));
        std::os::unix::fs::symlink(&scratch.0, &link).unwrap();
        assert!(!valid_scratch(&link), "a symlinked scratch path");
        let _ = std::fs::remove_file(&link);
        for bad in ["/home", "/", "/tmp/../home/hx-ar-ws-1-2-3.noindex"] { assert!(!valid_scratch(Path::new(bad)), "{bad}"); }
        assert!(!valid_scratch(&base.join("hx-ar-eval-1-2-3.noindex")));
        assert!(!valid_scratch(&base.join("hx-ar-ws-1-2.noindex")));
        assert!(!valid_scratch(&base.join("x").join("hx-ar-ws-1-2-3.noindex")));
        assert!(valid_scratch(&base.join("hx-ar-ws-1-2-3.noindex")), "a scratch copy that is already gone");
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
