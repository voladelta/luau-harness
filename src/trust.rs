//! Workspace trust (like `direnv allow`). Everything hx treats as user-owned config lives inside the workspace:
//! `.harness/{policy,mcp}.toml`, `.harness/init.luau`, `.harness/{override,tools}/` and `hx.lock`. The model cannot
//! write them, but whoever wrote the repo can, so a cloned repo could widen file-tool access, start MCP commands or
//! pre-approve its own packages and skills. hx therefore uses that config only when the user has trusted its exact
//! content: `hx trust` records a hash per workspace in `~/.config/hx/trust.toml` (outside every repo, protected from
//! the model). `hx run` and `hx exec` refuse untrusted config unless `--ignore-config` asks to run without it; other
//! commands ignore it as if absent. A workspace with none of these files needs no trust. `HX_TRUST=<hash>` in hx's own
//! environment trusts that exact config for one launch (throwaway copies); `HX_TRUST=1`, which trusted any config, is
//! deprecated and still accepted for one release.
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Config files whose content trust covers (paths relative to the workspace), plus whole trees: module overrides and
/// the saved versions `hx rollback` restores. (`.harness/staged` is not covered: the agent stages there itself, and
/// `hx accept` shows every staged file, asks, and installs exactly the bytes it showed.)
const FILES: [&str; 5] = [".harness/policy.toml", ".harness/mcp.toml", ".harness/init.luau", ".harness/ar.toml", "hx.lock"];
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

/// The one hash of a file tree, for trust and package approval alike: SHA-256 over (path, content) pairs in the given
/// order, each part prefixed with its length. Without the framing, bytes could move between a path and a neighbouring
/// file's content (a blob holding `\0hx.toml\0...` hashed like a real hx.toml), so two different trees could match.
pub fn tree_hash<'a>(entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (path, body) in entries {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path);
        h.update((body.len() as u64).to_le_bytes());
        h.update(body);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash of the config (length-framed path + content of every file), or None when there is none.
pub fn config_hash(ws: &Path) -> Option<String> {
    let files = config_files(ws);
    if files.is_empty() {
        return None;
    }
    let bodies: Vec<Vec<u8>> = files.iter().map(|f| if f.starts_with("symlink:") { Vec::new() } else { std::fs::read(ws.join(f)).unwrap_or_default() }).collect();
    Some(tree_hash(files.iter().zip(&bodies).map(|(f, b)| (f.as_bytes(), b.as_slice()))))
}

fn read_store() -> BTreeMap<String, String> {
    store_file()
        .ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&t).ok())
        .and_then(|mut m| m.remove("workspaces"))
        .unwrap_or_default()
}

static IGNORE_CONFIG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `--ignore-config`: use none of this workspace's config for this launch, trusted or not.
pub fn ignore_config() {
    IGNORE_CONFIG.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// May hx use this workspace's config? Yes when there is none, when `HX_TRUST` names its hash (or is the deprecated
/// `1`), or when its hash was trusted; never with `--ignore-config`.
pub fn is_trusted(ws: &Path) -> bool {
    if IGNORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    let Some(h) = config_hash(ws) else { return true };
    match std::env::var("HX_TRUST").as_deref() {
        Ok("1") => {
            static NOTED: std::sync::Once = std::sync::Once::new();
            NOTED.call_once(|| eprintln!("hx: note: HX_TRUST=1 is deprecated and will stop working in the next release; set HX_TRUST to the config hash `hx trust` shows"));
            return true;
        },
        Ok(v) if v == h && symlinks(ws).is_empty() => return true,
        _ => {},
    }
    symlinks(ws).is_empty() && read_store().get(&key(ws)) == Some(&h)
}

/// `hx run` and `hx exec` refuse to start over workspace config the user has not trusted, rather than silently
/// running without it.
pub fn require_trusted(ws: &Path) -> Result<()> {
    if IGNORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed) || is_trusted(ws) {
        return Ok(());
    }
    anyhow::bail!(
        "this workspace's config is not trusted ({}); review it and run `hx trust`, or pass --ignore-config to run without it",
        config_files(ws).join(", ")
    )
}

/// Record the workspace's current config as trusted. Returns the hash (None: nothing to trust).
pub fn record(ws: &Path) -> Result<Option<String>> {
    let links = symlinks(ws);
    if !links.is_empty() {
        anyhow::bail!("cannot trust a config with symlinks (their targets are not covered): {}; replace them with real files", links.join(", "));
    }
    let Some(h) = config_hash(ws) else { return Ok(None) };
    let path = store_file()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    // Concurrent `hx trust` runs would otherwise lose each other's entries: read-modify-write under an exclusive lock,
    // replacing the file atomically.
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
        .custom_flags(libc::O_NOFOLLOW).open(path.with_extension("lock"))?;
    anyhow::ensure!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0, "cannot lock the trust store");
    let mut all = read_store();
    all.insert(key(ws), h.clone());
    let body = toml::to_string(&BTreeMap::from([("workspaces", all)]))?;
    crate::atomic_file::replace_trusted(&path,
        format!("# Workspaces whose .harness config and hx.lock you trusted (`hx trust`). Delete a line to revoke.\n{body}").as_bytes())?;
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
        // HX_TRUST is consent for one launch only: it must not make the trust permanent here
        let stored = symlinks(ws).is_empty() && config_hash(ws).is_some_and(|h| read_store().get(&key(ws)) == Some(&h));
        Retrust { ws: ws.to_path_buf(), was: config_hash(ws).is_none() || stored }
    }

    pub fn after(self) -> Result<()> {
        if self.was {
            record(&self.ws)?;
        } else {
            eprintln!("note: this workspace's .harness config is not trusted, so `hx run` refuses it; review it and run `hx trust`");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_hash_frames_lengths() {
        // Unframed `path\0content\0` hashing made these two trees equal: one file whose content spells a second file.
        let one = tree_hash([(&b"assets/a.bin"[..], &b"x\0hx.toml\0kind = \"tool\""[..])]);
        let two = tree_hash([(&b"assets/a.bin"[..], &b"x"[..]), (&b"hx.toml"[..], &b"kind = \"tool\""[..])]);
        assert_ne!(one, two);
        assert_ne!(tree_hash([(&b"ab"[..], &b"c"[..])]), tree_hash([(&b"a"[..], &b"bc"[..])]));
    }

    #[test]
    fn config_hash_is_stable_across_the_shared_helper() {
        // Trust records written before the helper existed must stay valid: same framing, same bytes.
        let ws = std::env::temp_dir().join(format!("hx-trust-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        std::fs::write(ws.join(".harness/policy.toml"), "[budget]\nmax_tokens = 1\n").unwrap();
        std::fs::write(ws.join("hx.lock"), "").unwrap();
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for (f, body) in [(".harness/policy.toml", &b"[budget]\nmax_tokens = 1\n"[..]), ("hx.lock", &b""[..])] {
            h.update((f.len() as u64).to_le_bytes()); h.update(f.as_bytes());
            h.update((body.len() as u64).to_le_bytes()); h.update(body);
        }
        let expected: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(config_hash(&ws), Some(expected));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
