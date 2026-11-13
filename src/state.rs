//! Where run state lives: `$XDG_STATE_HOME/hx` (default `~/.local/state/hx`), one directory per workspace keyed by its
//! canonical path (`projects/<name>-<hash12>/`), holding `log/` (journals, blobs, registry, locks and revocation
//! markers) and `ar/RUN/` (autoresearch runs). It used to live in the workspace's `.harness/`, where the agent's own
//! shell could write it (Landlock cannot protect a subtree of a writable tree): append to or corrupt its journal, read
//! its manifest, rewrite what `--ar --resume` trusts. Outside every workspace, it is sealed from file tools and hidden
//! from confined children like the trust store, and no write root may overlap either (`guarded`).
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The state root: `$XDG_STATE_HOME/hx`, else `~/.local/state/hx`, with its deepest existing ancestor canonicalized so
/// the policy's canonical comparisons see through `~/.local -> elsewhere`. Unit tests default to a temp directory, so
/// `cargo test` never writes the user's state.
pub fn root() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None if cfg!(test) => std::env::temp_dir().join("hx-test-state"),
        None => PathBuf::from(std::env::var_os("HOME")?).join(".local/state"),
    };
    resolved(&base.join("hx"))
}

/// `path` with its deepest existing ancestor canonicalized (symlinks resolved) and the missing rest appended.
pub fn resolved(path: &Path) -> Option<PathBuf> {
    let mut base = path.to_path_buf();
    let mut rest = Vec::new();
    while !base.exists() {
        rest.push(base.file_name()?.to_os_string());
        base = base.parent()?.to_path_buf();
    }
    let mut c = std::fs::canonicalize(&base).ok()?;
    for r in rest.into_iter().rev() {
        c.push(r);
    }
    Some(c)
}

/// This workspace's state directory: `<root>/projects/<name>-<hash12>`, a readable name plus the first 12 hex digits
/// of the SHA-256 of its canonical path (two checkouts named `app` stay apart).
pub fn workspace_dir(workspace: &Path) -> Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let ws = std::fs::canonicalize(workspace).with_context(|| format!("workspace {}", workspace.display()))?;
    let name: String = ws.file_name().map(|n| n.to_string_lossy().chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).take(40).collect()).unwrap_or_default();
    let name = if name.is_empty() { "root".into() } else { name };
    let root = root().context("no HOME or XDG_STATE_HOME for hx's run state")?;
    Ok(root.join("projects").join(format!("{name}-{}", &crate::eventlog::sha_bytes(ws.as_os_str().as_bytes())[..12])))
}

/// Journals of this workspace's runs (with `blobs/` and `registry/`).
pub fn log_dir(workspace: &Path) -> Result<PathBuf> { Ok(workspace_dir(workspace)?.join("log")) }

/// This workspace's autoresearch runs.
pub fn ar_dir(workspace: &Path) -> Result<PathBuf> { Ok(workspace_dir(workspace)?.join("ar")) }

/// Directories no child may write and no write root may overlap: the state root and the trust store. Landlock only
/// adds access, so a write root that contains one would let every shell command write it, and one inside it would
/// grant writes there that hiding (which removes reads only) cannot take back.
pub fn guarded() -> Vec<PathBuf> {
    root().into_iter().chain(crate::trust::store_dir_canonical()).collect()
}

/// The guarded directory `root` (a write root, canonical) overlaps, if any.
pub fn overlap(root: &Path) -> Option<PathBuf> {
    guarded().into_iter().find(|g| g.starts_with(root) || root.starts_with(g))
}

/// A LOG argument: an existing path as given, else a run id of this workspace (`1731500000-1234` names
/// `<state>/log/1731500000-1234.jsonl`).
pub fn resolve_log(workspace: &Path, arg: &str) -> PathBuf {
    let given = PathBuf::from(arg);
    if given.exists() || arg.contains('/') { return given; }
    let id = arg.strip_suffix(".jsonl").unwrap_or(arg);
    log_dir(workspace).map(|d| d.join(format!("{id}.jsonl"))).ok().filter(|p| p.is_file()).unwrap_or(given)
}

/// An autoresearch RUN argument: an existing directory as given, else a run id of this workspace.
pub fn resolve_ar(workspace: &Path, arg: &str) -> PathBuf {
    let given = PathBuf::from(arg);
    if given.exists() || arg.contains('/') { return given; }
    ar_dir(workspace).map(|d| d.join(arg)).ok().filter(|p| p.is_dir()).unwrap_or(given)
}

/// One line on stderr when an owner command opens a journal from before the run store moved: it sits in a workspace's
/// `.harness/log`, which that workspace's shell commands could write.
pub fn note_legacy(journal: &Path) {
    if journal.parent().is_some_and(|d| d.ends_with(".harness/log")) {
        eprintln!("hx: note: {} lives in the workspace and is not protected from shell commands; new runs keep their journals in {}",
            journal.display(), root().map(|r| r.display().to_string()).unwrap_or_else(|| "~/.local/state/hx".into()));
    }
}

/// `hx runs`: this workspace's journals and autoresearch runs, oldest first, one per line with its path.
pub fn list(workspace: &Path) -> Result<String> {
    let dir = workspace_dir(workspace)?;
    let mut out = format!("runs of {} (state in {})\n", std::fs::canonicalize(workspace)?.display(), dir.display());
    let entries = |d: PathBuf| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d).into_iter().flatten().flatten().map(|e| e.path()).collect();
        v.sort();
        v
    };
    let mut n = 0;
    for path in entries(dir.join("log")).into_iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")) {
        let task = crate::supervisor::Manifest::inspect(&path).map(|m| m.task.lines().next().unwrap_or("").chars().take(60).collect::<String>());
        let state = if path.with_extension("revoked").exists() { "revoked" } else if task.is_err() { "no manifest" } else { "run" };
        out.push_str(&format!("{}  {state}  {}\n", path.display(), task.unwrap_or_default()));
        n += 1;
    }
    for path in entries(dir.join("ar")).into_iter().filter(|p| p.is_dir()) {
        let state = if path.join("report.json").exists() { "autoresearch, finished" } else { "autoresearch, unfinished" };
        out.push_str(&format!("{}  {state}\n", path.display()));
        n += 1;
    }
    if n == 0 { out.push_str("no runs yet\n"); }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspaces_get_distinct_readable_directories_outside_themselves() {
        let base = std::env::temp_dir().join(format!("hx-state-test-{}", std::process::id()));
        let (a, b) = (base.join("one/app"), base.join("two/app"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let (da, db) = (workspace_dir(&a).unwrap(), workspace_dir(&b).unwrap());
        assert_ne!(da, db);
        assert!(da.file_name().unwrap().to_string_lossy().starts_with("app-") && da.starts_with(root().unwrap().join("projects")));
        assert!(!da.starts_with(&base), "the state of a workspace never lives inside it");
        assert_eq!(da, workspace_dir(&a.join("../app")).unwrap(), "keyed by the canonical path");
        assert!(overlap(&a).is_none());
        assert_eq!(overlap(Path::new("/")), root());
        assert_eq!(overlap(&da.join("log")), root(), "a root inside the state directory overlaps it too");
        let _ = std::fs::remove_dir_all(base);
    }
}
