//! Workspace trust (like `direnv allow`). Everything hx treats as user-owned config lives inside the workspace:
//! every `.harness/*.toml` (hx's own `policy.toml` and `mcp.toml`, and those of tools built on hx), `.harness/init.luau`,
//! `.harness/{override,tools}/` and `hx.lock`. The model cannot
//! write them, but whoever wrote the repo can, so a cloned repo could widen file-tool access, start MCP commands or
//! pre-approve its own packages and skills. hx therefore uses that config only when the user has trusted its exact
//! content: `hx trust` records a hash per workspace in the trust store (`Dirs::trust_store`, outside every repo and
//! every root the model can reach). `hx run` and `hx exec` refuse untrusted config unless `--ignore-config` asks to run without it; other
//! commands ignore it as if absent. A workspace with none of these files needs no trust. `HX_TRUST=<hash>` in hx's own
//! environment trusts that exact config for one launch (throwaway copies).
use crate::dirs::Dirs;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Config files whose content trust covers (paths relative to the workspace), besides every other `.harness/*.toml`,
/// plus whole trees: module overrides and local tools.
const FILES: [&str; 4] = [
    ".harness/policy.toml",
    ".harness/mcp.toml",
    ".harness/init.luau",
    "hx.lock",
];
const TREES: [&str; 2] = [".harness/override", ".harness/tools"];

fn key(ws: &Path) -> String {
    std::fs::canonicalize(ws)
        .unwrap_or_else(|_| ws.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// One config entry: a regular file's bytes, or a symlink, whose target the hash cannot cover.
enum Entry {
    File(Vec<u8>),
    Symlink,
}

/// The workspace config, read once. Trust hashes exactly these bytes, and the run's registry is built from the same
/// bytes, so nothing written between the trust check and the capture can slip in.
pub struct Config {
    /// Keyed by path relative to the workspace.
    entries: BTreeMap<String, Entry>,
}

impl Config {
    /// Read every config file and tree. A symlink anywhere in the config (`.harness` itself, a file, or inside a tree)
    /// is recorded but not followed: its target is outside what the hash covers (and may be writable by the model), so
    /// such a config can never be trusted.
    pub fn read(ws: &Path) -> Result<Config> {
        fn add(entries: &mut BTreeMap<String, Entry>, ws: &Path, rel: String) -> Result<()> {
            let path = ws.join(&rel);
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => {
                    return Err(e).with_context(|| format!("cannot read workspace config {rel}"));
                }
            };
            if meta.file_type().is_symlink() {
                entries.insert(rel, Entry::Symlink);
            } else if meta.is_dir() {
                for entry in std::fs::read_dir(&path)
                    .with_context(|| format!("cannot read workspace config {rel}"))?
                {
                    let name = entry?.file_name();
                    let name = name.to_str().with_context(|| {
                        format!("non-UTF-8 file name in workspace config {rel}")
                    })?;
                    add(entries, ws, format!("{rel}/{name}"))?;
                }
            } else {
                anyhow::ensure!(
                    meta.is_file(),
                    "workspace config {rel} is not a regular file"
                );
                let bytes = std::fs::read(&path)
                    .with_context(|| format!("cannot read workspace config {rel}"))?;
                entries.insert(rel, Entry::File(bytes));
            }
            Ok(())
        }
        let kind = |rel: &str| {
            std::fs::symlink_metadata(ws.join(rel))
                .ok()
                .map(|m| m.file_type())
        };
        let mut entries = BTreeMap::new();
        let harness_link = kind(".harness").is_some_and(|t| t.is_symlink());
        if harness_link {
            entries.insert(".harness".to_string(), Entry::Symlink);
        }
        for f in FILES.into_iter().chain(TREES) {
            if harness_link && f.starts_with(".harness/") {
                continue;
            }
            // a directory where a file belongs, or a file where a tree belongs, is not config
            let misplaced = if FILES.contains(&f) {
                kind(f).is_some_and(|t| t.is_dir())
            } else {
                kind(f).is_some_and(|t| t.is_file())
            };
            if !misplaced {
                add(&mut entries, ws, f.to_string())?;
            }
        }
        // Tools built on hx keep their own config beside hx's, as `.harness/NAME.toml`: trust covers those too.
        if !harness_link && kind(".harness").is_some_and(|t| t.is_dir()) {
            for entry in std::fs::read_dir(ws.join(".harness")).context("cannot read .harness")? {
                let name = entry?.file_name();
                let rel = match name.to_str() {
                    Some(n) if n.ends_with(".toml") => format!(".harness/{n}"),
                    _ => continue,
                };
                if !entries.contains_key(&rel) && !kind(&rel).is_some_and(|t| t.is_dir()) {
                    add(&mut entries, ws, rel)?;
                }
            }
        }
        Ok(Config { entries })
    }

    /// The entries as `hx trust` lists them, sorted: a path, or `symlink:<path>`.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .entries
            .iter()
            .map(|(rel, entry)| match entry {
                Entry::File(_) => rel.clone(),
                Entry::Symlink => format!("symlink:{rel}"),
            })
            .collect();
        out.sort();
        out
    }

    /// Symlinked entries (they make the config untrustable).
    pub fn symlinks(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| matches!(e, Entry::Symlink))
            .map(|(rel, _)| rel.clone())
            .collect()
    }

    /// Hash of the config (length-framed name and content of every entry, in `names()` order), or None when there is
    /// none.
    pub fn hash(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        if self.entries.is_empty() {
            return None;
        }
        let bodies: BTreeMap<String, &[u8]> = self
            .entries
            .iter()
            .map(|(rel, entry)| match entry {
                Entry::File(bytes) => (rel.clone(), bytes.as_slice()),
                Entry::Symlink => (format!("symlink:{rel}"), &[][..]),
            })
            .collect();
        let mut h = Sha256::new();
        for (name, body) in bodies {
            h.update((name.len() as u64).to_le_bytes());
            h.update(name.as_bytes());
            h.update((body.len() as u64).to_le_bytes());
            h.update(body);
        }
        Some(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
    }

    /// The bytes of one config file (`.harness/init.luau`, `hx.lock`, ...), if present.
    pub fn file(&self, rel: &str) -> Option<&[u8]> {
        match self.entries.get(rel) {
            Some(Entry::File(bytes)) => Some(bytes),
            _ => None,
        }
    }

    /// The files of one config tree (`.harness/override`, ...), keyed by path relative to the tree.
    pub fn tree<'a>(&'a self, tree: &str) -> impl Iterator<Item = (&'a str, &'a [u8])> {
        let prefix = format!("{tree}/");
        self.entries
            .iter()
            .filter_map(move |(rel, entry)| match entry {
                Entry::File(bytes) => rel
                    .strip_prefix(prefix.as_str())
                    .map(|r| (r, bytes.as_slice())),
                Entry::Symlink => None,
            })
    }
}

/// The config files present, as `hx trust` lists them (see `Config::names`). An unreadable config lists as empty.
pub fn config_files(ws: &Path) -> Vec<String> {
    Config::read(ws).map(|c| c.names()).unwrap_or_default()
}

/// Symlinked entries in the config (they make it untrustable).
pub fn symlinks(ws: &Path) -> Vec<String> {
    Config::read(ws).map(|c| c.symlinks()).unwrap_or_default()
}

/// Hash of the config, or None when there is none (or it cannot be read).
pub fn config_hash(ws: &Path) -> Option<String> {
    Config::read(ws).ok()?.hash()
}

fn read_store(dirs: &Dirs) -> BTreeMap<String, String> {
    std::fs::read_to_string(dirs.trust_store())
        .ok()
        .and_then(|t| toml::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&t).ok())
        .and_then(|mut m| m.remove("workspaces"))
        .unwrap_or_default()
}

static IGNORE_CONFIG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `--ignore-config`: use none of this workspace's config for this launch, trusted or not.
pub fn ignore_config() {
    IGNORE_CONFIG.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The workspace config, when hx may use it: when there is none, when `HX_TRUST` in hx's own environment names its
/// hash, or when its hash was trusted; never with `--ignore-config` or with symlinks in it. The returned bytes are the
/// ones the decision covered: build from them, never from a second read.
pub fn trusted(ws: &Path, dirs: &Dirs) -> Result<Option<Config>> {
    if IGNORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(None);
    }
    let config = Config::read(ws)?;
    let Some(h) = config.hash() else {
        return Ok(Some(config));
    };
    let consented = std::env::var("HX_TRUST").is_ok_and(|v| v == h)
        || read_store(dirs).get(&key(ws)) == Some(&h);
    Ok((consented && config.symlinks().is_empty()).then_some(config))
}

/// May hx use this workspace's config? See `trusted`; a config that cannot be read is not trusted.
pub fn is_trusted(ws: &Path, dirs: &Dirs) -> bool {
    matches!(trusted(ws, dirs), Ok(Some(_)))
}

/// `hx run` and `hx exec` refuse to start over workspace config the user has not trusted, rather than silently
/// running without it.
pub fn require_trusted(ws: &Path, dirs: &Dirs) -> Result<()> {
    if IGNORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed) || is_trusted(ws, dirs) {
        return Ok(());
    }
    anyhow::bail!(
        "this workspace's config is not trusted ({}); review it and run `hx trust`, or pass --ignore-config to run without it",
        config_files(ws).join(", ")
    )
}

/// Record the workspace's current config as trusted. Returns the hash (None: nothing to trust).
pub fn record(ws: &Path, dirs: &Dirs) -> Result<Option<String>> {
    let config = Config::read(ws)?;
    let links = config.symlinks();
    if !links.is_empty() {
        anyhow::bail!(
            "cannot trust a config with symlinks (their targets are not covered): {}; replace them with real files",
            links.join(", ")
        );
    }
    let Some(h) = config.hash() else {
        return Ok(None);
    };
    let path = dirs.trust_store();
    std::fs::create_dir_all(dirs.config())?;
    // Concurrent `hx trust` runs would otherwise lose each other's entries: read-modify-write under an exclusive lock,
    // replacing the file atomically.
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.with_extension("lock"))?;
    lock.lock().context("cannot lock the trust store")?;
    let mut all = read_store(dirs);
    all.insert(key(ws), h.clone());
    let body = toml::to_string(&BTreeMap::from([("workspaces", all)]))?;
    crate::atomic_file::replace_trusted(&path,
        format!("# Workspaces whose .harness config and hx.lock you trusted (`hx trust`). Delete a line to revoke.\n{body}").as_bytes())?;
    Ok(Some(h))
}

/// For commands the user runs that change the config themselves (`hx approve`):
/// keep a trusted workspace trusted, but never let them bless config the user has not trusted yet.
pub struct Retrust<'a> {
    ws: PathBuf,
    dirs: &'a Dirs,
    was: bool,
}

impl<'a> Retrust<'a> {
    pub fn before(ws: &Path, dirs: &'a Dirs) -> Self {
        // HX_TRUST is consent for one launch only: it must not make the trust permanent here
        let hash = Config::read(ws)
            .ok()
            .filter(|c| c.symlinks().is_empty())
            .map(|c| c.hash());
        let was = match hash {
            Some(None) => true,
            Some(Some(h)) => read_store(dirs).get(&key(ws)) == Some(&h),
            None => false,
        };
        Retrust {
            ws: ws.to_path_buf(),
            dirs,
            was,
        }
    }

    pub fn after(self) -> Result<()> {
        if self.was {
            record(&self.ws, self.dirs)?;
        } else {
            eprintln!(
                "note: this workspace's .harness config is not trusted, so `hx run` refuses it; review it and run `hx trust`"
            );
        }
        Ok(())
    }
}

/// Text as the owner's terminal shows it in a review: control characters (except newline and tab) and bidirectional
/// formatting characters are escaped, so bytes someone else wrote cannot move the cursor, recolour or reorder what the
/// owner reads before answering.
pub fn terminal_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if (c.is_control() && c != '\n' && c != '\t') || bidi {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_toml_file_in_harness_is_config() {
        let ws = std::env::temp_dir().join(format!("hx-trust-toml-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&ws);
        std::fs::create_dir_all(ws.join(".harness/notes.toml")).unwrap();
        std::fs::write(ws.join(".harness/tool.toml"), "a = 1\n").unwrap();
        std::fs::write(ws.join(".harness/readme.md"), "not config").unwrap();
        assert_eq!(config_files(&ws), vec![".harness/tool.toml".to_string()]);
        let before = config_hash(&ws);
        std::fs::write(ws.join(".harness/tool.toml"), "a = 2\n").unwrap();
        assert_ne!(
            config_hash(&ws),
            before,
            "a tool's config is covered by trust"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }
}
