//! Workspace trust (like `direnv allow`). Everything hx treats as user-owned config lives inside the workspace:
//! `.harness/{policy,mcp}.toml`, `.harness/init.luau`, `.harness/{override,tools}/` and `hx.lock`. The model cannot
//! write them, but whoever wrote the repo can, so a cloned repo could widen the sandbox, start MCP commands or
//! pre-approve its own packages and skills. hx therefore uses that config only when the user has trusted its exact
//! content: `hx trust` records a hash per workspace in `~/.config/hx/trust.toml` (outside every repo, protected from
//! the model). Untrusted config is ignored as if absent; a workspace with none of these files needs no trust.
//! `HX_TRUST=1` in hx's own environment trusts the workspace for that launch (tests, throwaway copies).
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Config files whose content trust covers (paths relative to the workspace), plus whole trees: module overrides and
/// the saved versions `hx rollback` restores. (`.harness/staged` is not covered: `hx accept` shows what it activates and
/// asks, and the agent stages there itself.)
const FILES: [&str; 4] = [".harness/policy.toml", ".harness/mcp.toml", ".harness/init.luau", "hx.lock"];
const TREES: [&str; 3] = [".harness/override", ".harness/versions", ".harness/tools"];

/// `$XDG_CONFIG_HOME/hx` or `~/.config/hx`.
pub fn store_dir() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => Some(PathBuf::from(x).join("hx")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/hx")),
    }
}

/// The store dir with its deepest existing ancestor canonicalized (symlinks resolved), for the protected-path list:
/// the policy compares canonical paths, so `~/.config -> dotfiles/config` must not slip past the protection.
pub fn store_dir_canonical() -> Option<PathBuf> {
    let d = store_dir()?;
    let mut base = d.clone();
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

fn store_file() -> Result<PathBuf> {
    store_dir().map(|d| d.join("trust.toml")).ok_or_else(|| anyhow!("no HOME or XDG_CONFIG_HOME for the trust store"))
}

fn key(ws: &Path) -> String {
    std::fs::canonicalize(ws).unwrap_or_else(|_| ws.to_path_buf()).to_string_lossy().to_string()
}

/// The config files present, relative to the workspace and sorted (tree files included). A symlink anywhere in the
/// config (`.harness` itself, a file, or inside a tree) is listed as `symlink:<path>`: its target is outside what the
/// hash covers (and may be writable by the model), so such a config can never be trusted.
pub fn config_files(ws: &Path) -> Vec<String> {
    let is_link = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink());
    let mut out: Vec<String> = Vec::new();
    if is_link(&ws.join(".harness")) {
        out.push("symlink:.harness".into());
    }
    for f in FILES {
        if is_link(&ws.join(f)) {
            out.push(format!("symlink:{f}"));
        } else if ws.join(f).is_file() {
            out.push(f.to_string());
        }
    }
    fn walk(dir: &Path, ws: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(ws).unwrap().to_string_lossy().to_string();
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.file_type().is_symlink() => out.push(format!("symlink:{rel}")),
                Ok(m) if m.is_dir() => walk(&p, ws, out),
                _ => out.push(rel),
            }
        }
    }
    for t in TREES {
        let d = ws.join(t);
        if is_link(&d) {
            out.push(format!("symlink:{t}"));
        } else {
            walk(&d, ws, &mut out);
        }
    }
    out.sort();
    out
}

/// Symlinked entries in the config (they make it untrustable).
pub fn symlinks(ws: &Path) -> Vec<String> {
    config_files(ws).into_iter().filter_map(|f| f.strip_prefix("symlink:").map(String::from)).collect()
}

/// Hash of the config (length-framed path + content of every file), or None when there is none.
pub fn config_hash(ws: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let files = config_files(ws);
    if files.is_empty() {
        return None;
    }
    let mut h = Sha256::new();
    for f in files {
        let body = if f.starts_with("symlink:") { Vec::new() } else { std::fs::read(ws.join(&f)).unwrap_or_default() };
        h.update((f.len() as u64).to_le_bytes());
        h.update(f.as_bytes());
        h.update((body.len() as u64).to_le_bytes());
        h.update(&body);
    }
    Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn read_store() -> BTreeMap<String, String> {
    store_file()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&t).ok())
        .and_then(|mut m| m.remove("workspaces"))
        .unwrap_or_default()
}

/// May hx use this workspace's config? Yes when there is none, when HX_TRUST=1, or when its hash was trusted.
pub fn is_trusted(ws: &Path) -> bool {
    if std::env::var("HX_TRUST").is_ok_and(|v| v == "1") {
        return true;
    }
    if !symlinks(ws).is_empty() {
        return false;
    }
    match config_hash(ws) {
        None => true,
        Some(h) => read_store().get(&key(ws)) == Some(&h),
    }
}

/// Record the workspace's current config as trusted. Returns the hash (None: nothing to trust).
pub fn record(ws: &Path) -> Result<Option<String>> {
    let links = symlinks(ws);
    if !links.is_empty() {
        anyhow::bail!("cannot trust a config with symlinks (their targets are not covered): {}; replace them with real files", links.join(", "));
    }
    let Some(h) = config_hash(ws) else { return Ok(None) };
    let path = store_file()?;
    let mut all = read_store();
    all.insert(key(ws), h.clone());
    std::fs::create_dir_all(path.parent().unwrap())?;
    let body = toml::to_string(&BTreeMap::from([("workspaces", all)]))?;
    std::fs::write(&path, format!("# Workspaces whose .harness config and hx.lock you trusted (`hx trust`). Delete a line to revoke.\n{body}"))?;
    Ok(Some(h))
}

/// For commands the user runs that change the config themselves (`hx approve`, `accept`, `rollback`):
/// keep a trusted workspace trusted, but never let them bless config the user has not trusted yet.
pub struct Retrust {
    ws: PathBuf,
    was: bool,
}

impl Retrust {
    pub fn before(ws: &Path) -> Retrust {
        // HX_TRUST=1 is consent for one launch only: it must not make the trust permanent here
        let stored = symlinks(ws).is_empty() && config_hash(ws).is_some_and(|h| read_store().get(&key(ws)) == Some(&h));
        Retrust { ws: ws.to_path_buf(), was: config_hash(ws).is_none() || stored }
    }

    pub fn after(self) -> Result<()> {
        if self.was {
            record(&self.ws)?;
        } else {
            eprintln!("note: this workspace's .harness config is not trusted, so hx ignores it; review it and run `hx trust`");
        }
        Ok(())
    }
}

/// One stderr line when a run or exec starts with config that is being ignored.
pub fn warn_if_untrusted(ws: &Path) {
    if !is_trusted(ws) {
        eprintln!(
            "warning: ignoring untrusted workspace config ({}); review it and run `hx trust` to use it",
            config_files(ws).join(", ")
        );
    }
}
