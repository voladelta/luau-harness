//! Workspace trust (like `direnv allow`). Everything hx treats as user-owned config lives inside the workspace:
//! every `.harness/*.toml` (hx's own `policy.toml` and `mcp.toml`, and those of tools built on hx), `.harness/init.luau`, `.harness/{override,tools}/`, the packages in
//! `.harness/pkgs/` and the agent skills in `.agents/skills/`. Whoever wrote the repo wrote them, and the agent's shell
//! commands can write `.harness/` too, so a cloned repo or an earlier run could widen file-tool access, start MCP
//! commands or install a package with capabilities. hx therefore uses that config only when the owner has trusted its
//! exact content: `hx trust` shows it, then records a hash per workspace in the trust store (`Dirs::trust_store`,
//! outside every repo and every root the model can reach), with a digest per file, package and skill so that a later
//! refusal can name what changed. `hx run` and `hx exec` refuse untrusted config unless `--ignore-config` asks to run
//! without it; other commands ignore it as if absent. A workspace with none of these files needs no trust.
//! `HX_TRUST=<hash>` in hx's own environment trusts that exact config for one launch (throwaway copies).
use crate::dirs::Dirs;
use crate::modules::{Extension, Resolver};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Config files whose content trust covers (paths relative to the workspace), besides every other `.harness/*.toml`,
/// plus whole trees: module overrides and local tools. Packages and agent skills are read as such (see
/// `Resolver::workspace_extensions`).
const FILES: [&str; 3] = [
    ".harness/policy.toml",
    ".harness/mcp.toml",
    ".harness/init.luau",
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

/// The workspace config, read once. Trust hashes exactly these bytes, and the run's policy and registry are built from
/// the same bytes, so nothing written between the trust check and the capture can slip in.
pub struct Config {
    /// Keyed by path relative to the workspace.
    entries: BTreeMap<String, Entry>,
    /// The workspace's packages, then its agent skills.
    extensions: Vec<Extension>,
}

/// SHA-256 over `(name, content)` pairs, each length-prefixed, in the order given. The framing makes the encoding
/// unambiguous: no other set of names and contents (bytes moved between names) shares a hash.
fn framed<'a>(items: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (name, body) in items {
        h.update((name.len() as u64).to_le_bytes());
        h.update(name.as_bytes());
        h.update((body.len() as u64).to_le_bytes());
        h.update(body);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl Config {
    /// Read every config file and tree, and every package and skill. A symlink anywhere in the files and trees
    /// (`.harness` itself, a file, or inside a tree) is recorded but not followed: its target is outside what the hash
    /// covers (and may be writable by the model), so such a config can never be trusted. A package or skill is read
    /// by `snapshot`, which hashes a linked skill directory's content and refuses links inside it.
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
        if kind("hx.lock").is_some() {
            static NOTICE: std::sync::Once = std::sync::Once::new();
            NOTICE.call_once(|| {
                eprintln!(
                    "note: hx.lock is ignored: workspace trust (`hx trust`) now covers packages and skills; you can delete it"
                )
            });
        }
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
        let extensions = Resolver::new(ws.to_path_buf()).workspace_extensions(!harness_link);
        Ok(Config {
            entries,
            extensions,
        })
    }

    /// The workspace's packages (first) and agent skills, as read.
    pub fn extensions(&self) -> &[Extension] {
        &self.extensions
    }

    /// Symlinked entries (they make the config untrustable).
    pub fn symlinks(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| matches!(e, Entry::Symlink))
            .map(|(rel, _)| rel.clone())
            .collect()
    }

    /// Every hashed item as (name, content): a file by its path, a symlink as `symlink:<path>` with no content, a
    /// package's or skill's files by their workspace paths, and one that cannot be read as `unreadable:<dir>`, which
    /// never loads (its reason is shown, not hashed: it may name absolute paths).
    fn items(&self) -> BTreeMap<String, &[u8]> {
        let mut items: BTreeMap<String, &[u8]> = self
            .entries
            .iter()
            .map(|(rel, entry)| match entry {
                Entry::File(bytes) => (rel.clone(), bytes.as_slice()),
                Entry::Symlink => (format!("symlink:{rel}"), &[][..]),
            })
            .collect();
        for ext in &self.extensions {
            match &ext.snapshot {
                Ok(snap) => items.extend(
                    snap.files
                        .iter()
                        .map(|(file, bytes)| (format!("{}/{file}", ext.rel), bytes.as_slice())),
                ),
                Err(_) => {
                    items.insert(format!("unreadable:{}", ext.rel), &[]);
                }
            }
        }
        items
    }

    /// Hash of the config (every item of `items`, framed, in name order), or None when there is none.
    pub fn hash(&self) -> Option<String> {
        let items = self.items();
        (!items.is_empty()).then(|| framed(items.iter().map(|(n, b)| (n.as_str(), *b))))
    }

    /// One digest per unit the owner thinks in: each config file (or symlink), and each package or skill directory.
    /// Recorded with the trust, so a refusal can name what changed.
    fn units(&self) -> BTreeMap<String, String> {
        let mut units: BTreeMap<String, String> = self
            .entries
            .iter()
            .map(|(rel, entry)| match entry {
                Entry::File(bytes) => (rel.clone(), framed([(rel.as_str(), bytes.as_slice())])),
                Entry::Symlink => (format!("symlink:{rel}"), framed([])),
            })
            .collect();
        for ext in &self.extensions {
            let digest = match &ext.snapshot {
                Ok(snap) => framed(snap.files.iter().map(|(f, b)| (f.as_str(), b.as_slice()))),
                Err(_) => "unreadable".into(),
            };
            units.insert(ext.rel.clone(), digest);
        }
        units
    }

    /// The bytes of one config file (`.harness/init.luau`, ...), if present.
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

    /// What `hx trust` shows before asking: every config file in full, then one line per package and skill with what
    /// it is granted (name, kind, declared capabilities, file count). Everything is escaped with `terminal_safe`.
    pub fn review(&self) -> String {
        let mut out = String::new();
        for (rel, entry) in &self.entries {
            match entry {
                Entry::File(bytes) => match std::str::from_utf8(bytes) {
                    Ok(t) => out.push_str(&format!(
                        "===== {}\n{}{}",
                        terminal_safe(rel),
                        terminal_safe(t),
                        if t.ends_with('\n') || t.is_empty() {
                            ""
                        } else {
                            "\n"
                        }
                    )),
                    Err(_) => out.push_str(&format!(
                        "===== {} (binary, {} bytes)\n",
                        terminal_safe(rel),
                        bytes.len()
                    )),
                },
                Entry::Symlink => out.push_str(&format!("===== symlink:{}\n", terminal_safe(rel))),
            }
        }
        if !self.extensions.is_empty() {
            out.push_str("===== packages and skills\n");
        }
        for ext in &self.extensions {
            let what = match &ext.snapshot {
                Ok(snap) if ext.rel.starts_with(".agents/") => format!(
                    "agent skill, no Luau and no capabilities{}, {}",
                    if snap.files.contains_key("hx.toml") {
                        " (its hx.toml is an ordinary file)"
                    } else {
                        ""
                    },
                    file_count(snap.files.len())
                ),
                Ok(snap) => {
                    let package = &snap.manifest.package;
                    let caps = snap.manifest.capabilities.to_map();
                    let caps = if caps.is_empty() {
                        "no capabilities".to_string()
                    } else {
                        let list: Vec<String> = caps
                            .iter()
                            .map(|(cap, scopes)| format!("{cap}={}", scopes.join("+")))
                            .collect();
                        format!("capabilities {}", list.join(" "))
                    };
                    format!(
                        "package, kind {}, version {}, {caps}, {}",
                        terminal_safe(package.kind.as_deref().unwrap_or("module")),
                        terminal_safe(&package.version),
                        file_count(snap.files.len())
                    )
                }
                Err(e) => format!("not loaded: {}", terminal_safe(e)),
            };
            out.push_str(&format!(
                "{:<24} {what} ({})\n",
                terminal_safe(&ext.name),
                terminal_safe(&ext.rel)
            ));
        }
        out
    }
}

fn file_count(n: usize) -> String {
    if n == 1 {
        "1 file".into()
    } else {
        format!("{n} files")
    }
}

/// What the trust store holds: per workspace, the trusted hash and the digest of each unit (see `Config::units`).
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Store {
    #[serde(default)]
    workspaces: BTreeMap<String, String>,
    #[serde(default)]
    units: BTreeMap<String, BTreeMap<String, String>>,
}

fn read_store(dirs: &Dirs) -> Store {
    std::fs::read_to_string(dirs.trust_store())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

static IGNORE_CONFIG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `--ignore-config`: use none of this workspace's config for this launch, trusted or not.
pub fn ignore_config() {
    IGNORE_CONFIG.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Has the owner consented to exactly this config: there is none, `HX_TRUST` in hx's own environment names its hash,
/// or its hash is the one recorded? A config with symlinks never is.
fn consented(ws: &Path, dirs: &Dirs, config: &Config) -> bool {
    let Some(h) = config.hash() else {
        return true;
    };
    let consented = std::env::var("HX_TRUST").is_ok_and(|v| v == h)
        || read_store(dirs).workspaces.get(&key(ws)) == Some(&h);
    consented && config.symlinks().is_empty()
}

/// The workspace config, when hx may use it (see `consented`); never with `--ignore-config`. The returned bytes are
/// the ones the decision covered: build from them, never from a second read.
pub fn trusted(ws: &Path, dirs: &Dirs) -> Result<Option<Config>> {
    if IGNORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(None);
    }
    let config = Config::read(ws)?;
    Ok(consented(ws, dirs, &config).then_some(config))
}

/// May hx use this workspace's config? See `trusted`; a config that cannot be read is not trusted.
pub fn is_trusted(ws: &Path, dirs: &Dirs) -> bool {
    matches!(trusted(ws, dirs), Ok(Some(_)))
}

/// Hash of the config, or None when there is none (or it cannot be read).
pub fn config_hash(ws: &Path) -> Option<String> {
    Config::read(ws).ok()?.hash()
}

/// How the current config differs from the one last trusted, as `path (new|changed|removed)`; None when this
/// workspace was never trusted.
pub fn changes(ws: &Path, dirs: &Dirs, config: &Config) -> Option<Vec<String>> {
    let recorded = read_store(dirs).units.remove(&key(ws))?;
    let now = config.units();
    let mut out = Vec::new();
    for (unit, digest) in &now {
        match recorded.get(unit) {
            None => out.push(format!("{unit} (new)")),
            Some(d) if d != digest => out.push(format!("{unit} (changed)")),
            Some(_) => {}
        }
    }
    out.extend(
        recorded
            .keys()
            .filter(|unit| !now.contains_key(*unit))
            .map(|unit| format!("{unit} (removed)")),
    );
    Some(out)
}

/// `hx run` and `hx exec` refuse to start over workspace config the owner has not trusted, rather than silently running
/// without it, and say what changed since the last trust.
pub fn require_trusted(ws: &Path, dirs: &Dirs) -> Result<()> {
    if IGNORE_CONFIG.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    let config = Config::read(ws)?;
    if consented(ws, dirs, &config) {
        return Ok(());
    }
    let links = config.symlinks();
    let what = match changes(ws, dirs, &config) {
        _ if !links.is_empty() => format!(
            "has symlinks, which cannot be trusted (their targets are not covered): {}; replace them with real files",
            links.join(", ")
        ),
        Some(changed) if !changed.is_empty() => {
            format!("changed since you trusted it: {}", changed.join(", "))
        }
        _ => format!(
            "is not trusted ({})",
            config.units().into_keys().collect::<Vec<_>>().join(", ")
        ),
    };
    anyhow::bail!(
        "this workspace's config {}; review it and run `hx trust`, or pass --ignore-config to run without it",
        terminal_safe(&what)
    )
}

/// What `hx skills` says about one workspace skill.
pub struct State {
    trusted: bool,
    recorded: Option<BTreeMap<String, String>>,
    units: BTreeMap<String, String>,
}

impl State {
    pub fn of(&self, unit: &str) -> &'static str {
        if self.trusted {
            return "trusted";
        }
        match self.recorded.as_ref().map(|r| r.get(unit)) {
            Some(Some(digest)) if Some(digest) == self.units.get(unit) => {
                "not trusted (other config changed)"
            }
            Some(Some(_)) => "changed since trust",
            _ => "not trusted",
        }
    }
}

/// The trust state of `config`, for `hx skills`.
pub fn state(ws: &Path, dirs: &Dirs, config: &Config) -> State {
    State {
        trusted: consented(ws, dirs, config),
        recorded: read_store(dirs).units.remove(&key(ws)),
        units: config.units(),
    }
}

/// Record `config`, as read and shown, as trusted. Returns the hash (None: nothing to trust).
pub fn record_config(ws: &Path, dirs: &Dirs, config: &Config) -> Result<Option<String>> {
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
    let mut store = read_store(dirs);
    store.workspaces.insert(key(ws), h.clone());
    store.units.insert(key(ws), config.units());
    let body = toml::to_string(&store)?;
    crate::atomic_file::replace_trusted(&path,
        format!("# Workspaces whose config, packages and skills you trusted (`hx trust`). Delete a line under [workspaces] to revoke.\n{body}").as_bytes())?;
    Ok(Some(h))
}

/// Record the workspace's current config as trusted, as `hx trust --yes` would.
#[cfg(test)]
pub fn record(ws: &Path, dirs: &Dirs) -> Result<Option<String>> {
    record_config(ws, dirs, &Config::read(ws)?)
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
        assert_eq!(
            Config::read(&ws)
                .unwrap()
                .units()
                .into_keys()
                .collect::<Vec<_>>(),
            vec![".harness/tool.toml".to_string()]
        );
        let before = config_hash(&ws);
        std::fs::write(ws.join(".harness/tool.toml"), "a = 2\n").unwrap();
        assert_ne!(
            config_hash(&ws),
            before,
            "a tool's config is covered by trust"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_hash_frames_names_and_contents() {
        // Without length framing, `a\0x\0b\0y\0` is both one file a = "x\0b\0y" and two files a = "x", b = "y".
        assert_ne!(
            framed([("a", &b"x\0b\0y"[..])]),
            framed([("a", &b"x"[..]), ("b", &b"y"[..])])
        );
        assert_ne!(
            framed([("a", &b"xb"[..]), ("b", &b""[..])]),
            framed([("a", &b"x"[..]), ("b", &b"b"[..])])
        );
    }

    #[test]
    fn terminal_safe_escapes_controls_and_bidi_but_keeps_lines() {
        assert_eq!(
            terminal_safe("a\x1b[2Jb \u{202e}c\r\n\td"),
            "a\\u{1b}[2Jb \\u{202e}c\\u{d}\n\td"
        );
    }
}
