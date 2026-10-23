//! What crosses an episode's boundary: snapshots of the editable files, the throwaway copies episodes and evaluators
//! run in, the evaluator itself, and the committed tree frozen at launch.
use super::*;
use base64::Engine as _;
use std::collections::BTreeMap;

/// Limits on the editable files of one snapshot: total bytes, file count and directory depth.
pub(super) const MAX_SNAPSHOT: usize = 16 << 20;
pub(super) const MAX_FILES: usize = 10_000;
pub(super) const MAX_DEPTH: usize = 32;

/// Editable files, path -> bytes.
pub(super) type Files = BTreeMap<String, Vec<u8>>;

/// Every regular file under the editable paths of `root`, within the snapshot limits. Symlinks and special files are
/// skipped: they never leave an episode. Names must be UTF-8 so they are recreated exactly.
pub(super) fn capture(root: &Path, spec: &Spec) -> Result<Files> {
    fn walk(path: &Path, rel: &str, depth: usize, out: &mut Files, total: &mut usize) -> Result<()> {
        let Ok(meta) = std::fs::symlink_metadata(path) else { return Ok(()) };
        if meta.is_dir() {
            ensure!(depth < MAX_DEPTH, "editable directories nest deeper than {MAX_DEPTH} levels");
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let name = entry.file_name().into_string().map_err(|n| anyhow::anyhow!("editable file name {n:?} is not UTF-8"))?;
                walk(&entry.path(), &format!("{rel}/{name}"), depth + 1, out, total)?;
            }
        } else if meta.is_file() {
            ensure!(out.len() < MAX_FILES, "editable paths hold more than {MAX_FILES} files");
            // Read at most what is left of the limit, so a file that keeps growing cannot get past it.
            let left = MAX_SNAPSHOT - *total;
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut std::io::Read::take(std::fs::File::open(path)?, left as u64 + 1), &mut bytes)?;
            ensure!(bytes.len() <= left, "editable files exceed {} MiB", MAX_SNAPSHOT >> 20);
            *total += bytes.len();
            out.insert(rel.to_string(), bytes);
        }
        Ok(())
    }
    let (mut out, mut total) = (BTreeMap::new(), 0);
    for e in &spec.editable {
        let e = e.trim_end_matches('/');
        // A symlinked parent directory is skipped like any symlink: nothing is read through it.
        let linked = Path::new(e).ancestors().skip(1).filter(|a| !a.as_os_str().is_empty())
            .any(|a| std::fs::symlink_metadata(root.join(a)).is_ok_and(|m| m.file_type().is_symlink()));
        if linked { continue; }
        walk(&root.join(e), e, 0, &mut out, &mut total)?;
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
    if !path.exists() {
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, bytes)?;
    }
    Ok(id)
}

pub(super) fn load_snap(dir: &Path, id: &str) -> Result<Files> {
    let encoded: BTreeMap<String, String> = serde_json::from_slice(&std::fs::read(dir.join("snaps").join(format!("{id}.json")))?)?;
    encoded.into_iter().map(|(k, v)| Ok((k, base64::engine::general_purpose::STANDARD.decode(v)?))).collect()
}

/// A fresh private directory in the system temp dir; removed on drop.
pub(super) struct Scratch(pub(super) PathBuf);
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

/// Copy the tree `from` to the new path `to` (APFS clones when available).
pub(super) fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    for flags in [&["-cR"][..], &["-R"][..]] {
        let status = std::process::Command::new("cp").args(flags).arg(from).arg(to)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status()?;
        if status.success() { return Ok(()); }
        let _ = std::fs::remove_dir_all(to);
    }
    bail!("could not copy {} to {}", from.display(), to.display())
}

/// Run an evaluator on `files` placed into a fresh copy of the frozen base. Returns (score, valid, se, report).
/// Failures of the evaluator become invalid results, not errors.
pub(super) async fn evaluate(spec: &Spec, run: &Path, files: &Files, cmd: &str) -> Result<(Option<f64>, bool, Option<f64>, Value)> {
    let scratch = Scratch::new("eval")?;
    let tree = scratch.0.join("tree");
    copy_tree(&run.join("base"), &tree)?;
    place(&tree, spec, files)?;
    let tmp = scratch.0.join("tmp");
    std::fs::create_dir(&tmp)?;
    let env = crate::prims::ShellEnv { env: Some(evaluator_env(&tmp)) };
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
    Ok((score, valid, se, obj.get("report").cloned().unwrap_or(Value::Null)))
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
        .filter_map(|j| crate::eventlog::Books::of(&crate::eventlog::load(&j).ok()?).ok())
        .map(|books| books.usage.rate_limits).sum()
}
