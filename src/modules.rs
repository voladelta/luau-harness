//! Module resolution, package manifests, the lockfile (approvals) and staging.
//!
//! `require("pkg")` loads `init.luau`; `require("pkg/mod")` loads `mod.luau`. Lookup order:
//!   1. `<ws>/.harness/override/std/<file>`   user override of a built-in file (e.g. the printer), no rebuild needed
//!   2. `<ws>/.harness/pkgs/<pkg>/<mod>.luau` project package (needs a lockfile entry for its exact content)
//!   3. the built-in `std` package embedded in the binary
//!
//! An override replaces a file of the built-in `std` package and nothing else, and no package may be named `std`, so
//! the three never compete for one name.
//!
//! Agent skills (the `.agents/skills` convention): `<ws>/.agents/skills/<name>/SKILL.md`, then `~/.agents/skills/<name>/SKILL.md`.
//! Their manifest comes from SKILL.md frontmatter; they carry no Luau and no capabilities (an `hx.toml` or `.luau` file
//! in one is an ordinary file), and are locked by `hx approve NAME` like any package. `.harness/pkgs/<name>` shadows them.
//!
//! Every package and skill is read by one function, `snapshot`: approval, `hx skills`, the run's registry and `hx
//! accept` all see the same files and the same digest.
//!
//! Everything under (1), (2), `project/init` and the agent skills is used only when the workspace config is trusted
//! (see trust.rs); otherwise only the built-in package resolves.
//!
//! `require("base:std/printer")` skips (1), so an override can wrap the module it replaces.
//! `require("project/init")` is `<ws>/.harness/init.luau`, run before the loop starts (hook registration etc.).
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use crate::supervisor::Capabilities;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Packages built into the binary: (package, file, text). Trusted like the binary itself.
pub const EMBEDDED: &[(&str, &str, &str)] = &[
    ("std", "hx.toml", include_str!("../stdlib/std/hx.toml")),
    ("std", "init.luau", include_str!("../stdlib/std/init.luau")),
    ("std", "loop.luau", include_str!("../stdlib/std/loop.luau")),
    ("std", "rpi.luau", include_str!("../stdlib/std/rpi.luau")),
    ("std", "rpi/common.md", include_str!("../stdlib/std/rpi/common.md")),
    ("std", "rpi/research_questions.md", include_str!("../stdlib/std/rpi/research_questions.md")),
    ("std", "rpi/research.md", include_str!("../stdlib/std/rpi/research.md")),
    ("std", "rpi/system_design.md", include_str!("../stdlib/std/rpi/system_design.md")),
    ("std", "rpi/program_design.md", include_str!("../stdlib/std/rpi/program_design.md")),
    ("std", "rpi/implementation.md", include_str!("../stdlib/std/rpi/implementation.md")),
    ("std", "rpi/verification.md", include_str!("../stdlib/std/rpi/verification.md")),
    ("std", "rpi/repair.md", include_str!("../stdlib/std/rpi/repair.md")),
    ("std", "rpi/PROVENANCE.md", include_str!("../stdlib/std/rpi/PROVENANCE.md")),
    ("std", "prompt.luau", include_str!("../stdlib/std/prompt.luau")),
    ("std", "printer.luau", include_str!("../stdlib/std/printer.luau")),
    ("std", "feedback.luau", include_str!("../stdlib/std/feedback.luau")),
    ("std", "hooks.luau", include_str!("../stdlib/std/hooks.luau")),
    ("std", "testlib.luau", include_str!("../stdlib/std/testlib.luau")),
    ("std", "packages.luau", include_str!("../stdlib/std/packages.luau")),
    ("std", "mcp.luau", include_str!("../stdlib/std/mcp.luau")),
    ("std", "authoring.luau", include_str!("../stdlib/std/authoring.luau")),
    ("std", "vision.luau", include_str!("../stdlib/std/vision.luau")),
    ("std", "compact.luau", include_str!("../stdlib/std/compact.luau")),
    ("std", "recheck.luau", include_str!("../stdlib/std/recheck.luau")),
    ("std", "system.md", include_str!("../prompts/system.md")),
];

/// A package's `hx.toml`. Unknown tables and unknown capabilities are load errors, so a typo cannot silently grant
/// nothing (or less than intended).
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub package: PackageInfo,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub tool: ToolInfo,
    /// `[exports]` and `[tests]`: written by older `promote()` manifests and read by nothing (the module is `init.luau`,
    /// tests are `tests/*.luau`). Accepted so approved packages keep loading.
    #[serde(default, rename = "exports")]
    _exports: Option<toml::Table>,
    #[serde(default, rename = "tests")]
    _tests: Option<toml::Table>,
}

impl Manifest {
    /// The manifest of an agent skill: kind "skill", no capabilities, the description from SKILL.md frontmatter.
    fn agent_skill(skill_md: &str) -> Manifest {
        let mut m = Manifest::default();
        m.package.version = "agent-skill".into();
        m.package.kind = Some("skill".into());
        m.package.description = Some(frontmatter_description(skill_md));
        m
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct PackageInfo {
    /// The package's name; `hx test` uses it for a directory outside `.harness/pkgs` (else the directory name).
    pub name: Option<String>,
    pub version: String,
    /// "tool" (functions the model calls as tools.NAME), "skill" (SKILL.md + optional Luau), or empty (plain module)
    pub kind: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct ToolInfo {
    pub signature: Option<String>,
    pub doc: Option<String>,
}

/// An installed, locked and granted package, as shown to the model.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PkgInfo {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: String,
    pub signature: String,
    pub doc: String,
    /// skills: absolute directory and its files (relative), so SKILL.md's references can be read
    pub dir: String,
    pub files: Vec<String>,
}

/// A name the model can use as a Luau identifier (`deftool` names, and so promoted packages): lowercase letters,
/// digits and `_`, starting with a letter, at most 40 bytes.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A package or skill name: lowercase letters, digits, `_` and `-` (agent skills are conventionally hyphenated),
/// starting with a letter, at most 64 bytes, and not `std` or `project`, which name built-in modules. No `/`, `:` or
/// `.`, so a name can never reach another package's modules, the `base:`/`tool:` namespaces or a path outside its
/// directory.
pub fn valid_package_name(n: &str) -> bool {
    n.len() <= 64 && !matches!(n, "std" | "project") && n.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// `hx.lock`: one record per approved package: the exact content approved. The digest covers `hx.toml`, so approving
/// it grants exactly the capabilities that manifest requests; there is no separate grant list to keep in step.
#[derive(Debug, Deserialize, serde::Serialize, Default)]
struct Lockfile {
    #[serde(default)]
    package: Vec<LockEntry>,
}

#[derive(Debug, Deserialize, serde::Serialize, Clone)]
struct LockEntry {
    name: String,
    version: String,
    hash: String,
}

impl Lockfile {
    fn parse(bytes: Option<&[u8]>) -> Result<Lockfile> {
        let Some(bytes) = bytes else { return Ok(Lockfile::default()) };
        toml::from_str(std::str::from_utf8(bytes).context("hx.lock is not UTF-8")?).context("hx.lock is not valid")
    }

    /// The lock on disk, for the owner's commands (a run reads it through trust, see `Registry::capture`).
    fn read(path: &Path) -> Result<Lockfile> {
        match std::fs::read(path) {
            Ok(bytes) => Lockfile::parse(Some(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lockfile::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    fn entry(&self, name: &str) -> Option<&LockEntry> {
        self.package.iter().find(|e| e.name == name)
    }

    /// The snapshot of `location` when the lock approves exactly its content; else why not.
    fn approved(&self, name: &str, location: Location) -> Result<Snapshot> {
        // checked before reading anything: ~/.agents/skills can be large
        let entry = self.entry(name).ok_or_else(|| LockError::NotLocked(name.to_string()))?;
        let snap = snapshot(location)?;
        if entry.hash != snap.digest || entry.version != snap.manifest.package.version {
            return Err(LockError::Changed { name: name.to_string(), locked: entry.hash.clone(), now: snap.digest }.into());
        }
        Ok(snap)
    }
}

/// Why a package is not usable although it is installed.
#[derive(Debug)]
pub enum LockError {
    NotLocked(String),
    Changed { name: String, locked: String, now: String },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLocked(pkg) => write!(f, "package {pkg} is not locked; review it, then run `hx approve {pkg}`"),
            Self::Changed { name, locked, now } => write!(f,
                "package {name} changed since it was approved (locked {}…, now {}…); review it and run `hx approve {name}` again",
                locked.get(..8).unwrap_or(locked), now.get(..8).unwrap_or(now)),
        }
    }
}

impl std::error::Error for LockError {}

/// Where a package's files live, which also decides how its manifest is read.
#[derive(Clone, Debug)]
pub enum Location {
    /// A directory with an `hx.toml` (`.harness/pkgs/NAME`, a staged package, a package under test).
    Package(PathBuf),
    /// `.agents/skills/NAME` or `~/.agents/skills/NAME`: SKILL.md and plain files, no Luau, no capabilities.
    AgentSkill(PathBuf),
}

impl Location {
    pub fn dir(&self) -> &Path {
        match self {
            Self::Package(dir) | Self::AgentSkill(dir) => dir,
        }
    }
}

/// A package's files as read once, the digest over exactly those bytes, and the manifest parsed from them.
#[derive(Debug)]
pub struct Snapshot {
    pub location: Location,
    /// Relative path (`/`-separated) to content. Dot-entries are not part of a package.
    pub files: BTreeMap<String, Vec<u8>>,
    pub digest: String,
    pub manifest: Manifest,
}

/// Upper bounds of one snapshot: a package is source and documents, not a data directory.
const SNAPSHOT_FILES: usize = 10_000;
const SNAPSHOT_BYTES: usize = 64 << 20;

/// Read a package or skill. This is the only reader of package directories: approval, `hx skills`, the run's registry,
/// `hx test` and `hx accept` all go through it.
///
/// The directory itself may be a symlink (a skill linked into `~/.agents/skills`): its target's content is what is
/// read, hashed and approved. Inside it, entries whose name starts with `.` (`.git`, editor files) are skipped, and a
/// symlink or special file is an error, since its target would not be part of what the digest covers.
pub fn snapshot(location: Location) -> Result<Snapshot> {
    let dir = location.dir();
    let files = read_tree(dir)?;
    let manifest = match &location {
        Location::Package(_) => {
            let text = files.get("hx.toml").with_context(|| format!("{}: missing hx.toml", dir.display()))?;
            let text = std::str::from_utf8(text).with_context(|| format!("{}: hx.toml is not UTF-8", dir.display()))?;
            toml::from_str(text).map_err(|e| anyhow!("{}: bad hx.toml: {e}", dir.display()))?
        },
        Location::AgentSkill(_) => {
            let text = files.get("SKILL.md").with_context(|| format!("{}: missing SKILL.md", dir.display()))?;
            Manifest::agent_skill(&String::from_utf8_lossy(text))
        },
    };
    Ok(Snapshot { digest: digest(&files), files, manifest, location })
}

fn read_tree(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    fn walk(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Vec<u8>>, total: &mut usize) -> Result<()> {
        use std::{io::Read, os::unix::fs::OpenOptionsExt};
        for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_str().with_context(|| format!("non-UTF-8 file name in {}", dir.display()))?;
            if name.starts_with('.') {
                continue;
            }
            let rel = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(&entry.path(), &rel, out, total)?;
            } else if kind.is_file() {
                anyhow::ensure!(out.len() < SNAPSHOT_FILES, "{} has more than {SNAPSHOT_FILES} files", root_hint(dir, prefix));
                let mut bytes = Vec::new();
                std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(entry.path())
                    .and_then(|f| f.take((SNAPSHOT_BYTES - *total) as u64 + 1).read_to_end(&mut bytes))
                    .with_context(|| format!("cannot read {}", entry.path().display()))?;
                *total += bytes.len();
                anyhow::ensure!(*total <= SNAPSHOT_BYTES, "{} is larger than {} MiB", root_hint(dir, prefix), SNAPSHOT_BYTES >> 20);
                out.insert(rel, bytes);
            } else if kind.is_symlink() {
                bail!("{} is a symlink; a package or skill may not contain symlinks (their targets are not covered by its approval): copy the file in instead", entry.path().display());
            } else {
                bail!("{} is not a regular file", entry.path().display());
            }
        }
        Ok(())
    }
    /// The package root, for messages about the whole tree.
    fn root_hint(dir: &Path, prefix: &str) -> String {
        let depth = if prefix.is_empty() { 0 } else { prefix.split('/').count() };
        dir.ancestors().nth(depth).unwrap_or(dir).display().to_string()
    }
    anyhow::ensure!(std::fs::metadata(root).with_context(|| format!("cannot read {}", root.display()))?.is_dir(), "{} is not a directory", root.display());
    let mut files = BTreeMap::new();
    walk(root, "", &mut files, &mut 0)?;
    Ok(files)
}

/// The approval digest: SHA-256 over every file as length-prefixed path and content, in path order. The framing makes
/// the encoding unambiguous, so no other set of files (bytes moved between names or files) shares a digest.
fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(b"hx package v2\0");
    for (path, bytes) in files {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl Snapshot {
    fn is_agent_skill(&self) -> bool {
        matches!(self.location, Location::AgentSkill(_))
    }

    /// The Luau modules this package provides, as (module name, file, source). An agent skill provides none.
    fn modules(&self, name: &str) -> Result<Vec<(String, String, String)>> {
        if self.is_agent_skill() {
            return Ok(Vec::new());
        }
        self.files.iter().filter_map(|(file, bytes)| file.strip_suffix(".luau").map(|module| (module, file, bytes)))
            .map(|(module, file, bytes)| {
                let text = String::from_utf8(bytes.clone()).with_context(|| format!("package {name}: {file} is not UTF-8"))?;
                let module = if module == "init" { name.to_string() } else { format!("{name}/{module}") };
                Ok((module, file.clone(), text))
            })
            .collect()
    }

    fn info(&self, name: &str) -> PkgInfo {
        let package = &self.manifest.package;
        let kind = package.kind.clone().unwrap_or_default();
        PkgInfo {
            name: name.to_string(),
            version: package.version.clone(),
            description: package.description.clone().unwrap_or_default(),
            signature: self.manifest.tool.signature.clone().unwrap_or_default(),
            doc: self.manifest.tool.doc.clone().unwrap_or_default(),
            dir: String::new(),
            files: if kind == "skill" { self.files.keys().cloned().collect() } else { Vec::new() },
            kind,
        }
    }
}

pub struct Resolver {
    pub root: PathBuf,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Source {
    pub text: String,
    pub origin: String,
    /// true when this came from `.harness/override/` (the caller falls back to the built-in module if it is broken)
    pub is_override: bool,
    /// project packages: (name, capabilities) so the loader can run the code with only those primitives
    pub pkg: Option<(String, Capabilities)>,
}

/// An immutable, approved byte snapshot. Runtime resolution never goes back to mutable workspace files.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub tools: BTreeMap<String, crate::tools::Definition>,
    #[serde(default)]
    pub tool_errors: BTreeMap<String, String>,
    pub sources: BTreeMap<String, Source>,
    pub assets: BTreeMap<String, Vec<u8>>,
    pub packages: Vec<PkgInfo>,
    pub capabilities: BTreeMap<String, Capabilities>,
    pub mcp: BTreeMap<String, crate::mcp::ServerCfg>,
    pub rejected: BTreeMap<String, String>,
}

impl Registry {
    pub fn resolve(&self, name: &str) -> Result<Source> {
        self.sources.get(name).cloned().ok_or_else(|| {
            let package = name.split('/').next().unwrap_or(name);
            match self.rejected.get(package) { Some(error) => anyhow!("{error}"), None => anyhow!("module not found: {name}") }
        })
    }

    pub fn asset(&self, name: &str) -> Result<String> {
        String::from_utf8(self.assets.get(name).ok_or_else(|| anyhow!("no asset {name}"))?.clone())
            .map_err(Into::into)
    }

    /// Freeze everything a run may load: the built-in package, and, when the workspace config is trusted, the
    /// overrides, project init, MCP servers and local tools from the bytes trust covered, plus every package and agent
    /// skill whose snapshot the lock in those bytes approves.
    pub fn capture(root: &Path) -> Result<Self> {
        Self::build(root, crate::trust::trusted(root)?)
    }

    /// `capture` from config already read and judged: `config` is None when it may not be used.
    fn build(root: &Path, config: Option<crate::trust::Config>) -> Result<Self> {
        let mut registry = Registry::default();
        for (pkg, file, text) in EMBEDDED {
            let key = format!("{pkg}/{file}");
            registry.assets.insert(key.clone(), text.as_bytes().to_vec());
            if let Some(module) = file.strip_suffix(".luau") {
                let name = if module == "init" { pkg.to_string() } else { format!("{pkg}/{module}") };
                let base = Source { text: text.to_string(), origin: key, is_override: false, pkg: None };
                registry.sources.insert(format!("base:{name}"), base.clone());
                registry.sources.insert(name, base);
            }
        }
        let Some(config) = config else { return Ok(registry) };
        // Only files that replace a built-in file are overrides; anything else in the tree is not used.
        for (rel, bytes) in config.tree(".harness/override") {
            let Some(file) = rel.strip_prefix("std/") else { continue };
            if Resolver::embedded("std", file).is_none() {
                continue;
            }
            registry.assets.insert(rel.to_string(), bytes.to_vec());
            if let Some(module) = file.strip_suffix(".luau") {
                let name = if module == "init" { "std".to_string() } else { format!("std/{module}") };
                let text = String::from_utf8(bytes.to_vec()).with_context(|| format!(".harness/override/{rel} is not UTF-8"))?;
                registry.sources.insert(name, Source { text, origin: rel.to_string(), is_override: true, pkg: None });
            }
        }
        if let Some(init) = config.file(".harness/init.luau") {
            let text = String::from_utf8(init.to_vec()).context(".harness/init.luau is not UTF-8")?;
            registry.sources.insert("project/init".into(), Source { text, origin: "project/init".into(), is_override: false, pkg: None });
        }
        let lock = Lockfile::parse(config.file("hx.lock"));
        for (name, location) in Resolver::new(root.to_path_buf()).installed() {
            let approved = match &lock {
                _ if !valid_package_name(&name) => Err(anyhow!("{name:?} is not a valid package name (lowercase letters, digits, _ and -, starting with a letter; not std or project)")),
                Ok(lock) => lock.approved(&name, location),
                Err(e) => Err(anyhow!("{e:#}")),
            };
            match approved.and_then(|snap| registry.add(&name, &snap).map(|()| snap)) {
                Ok(snap) => registry.packages.push(snap.info(&name)),
                Err(error) => { registry.rejected.insert(name, error.to_string()); },
            }
        }
        registry.mcp = crate::mcp::snapshot(config.file(".harness/mcp.toml"))?;
        crate::tools::capture(root, config.tree(".harness/tools"), &mut registry)?;
        Ok(registry)
    }

    /// Add a package's modules, files and capabilities. Nothing is added when one of its modules is unusable.
    fn add(&mut self, name: &str, snap: &Snapshot) -> Result<()> {
        let modules = snap.modules(name)?;
        let caps = snap.manifest.capabilities.clone();
        self.capabilities.insert(name.to_string(), caps.clone());
        for (file, bytes) in &snap.files {
            self.assets.insert(format!("{name}/{file}"), bytes.clone());
        }
        for (module, file, text) in modules {
            self.sources.insert(module, Source { text, origin: format!("{name}/{file}"), is_override: false, pkg: Some((name.to_string(), caps.clone())) });
        }
        self.rejected.remove(name);
        Ok(())
    }

    /// `hx test DIR`: make an unapproved package loadable for this one registry (never a run's), so its tests can
    /// run inside its boundary.
    pub fn mount(&mut self, name: &str, snap: &Snapshot) -> Result<()> {
        self.add(name, snap)
    }

    /// The test files of a package in this registry: `tests/*.luau`, as (file name, source), sorted.
    pub fn tests(&self, name: &str) -> Result<Vec<(String, String)>> {
        let prefix = format!("{name}/tests/");
        self.assets.iter()
            .filter_map(|(key, bytes)| key.strip_prefix(prefix.as_str()).filter(|f| f.ends_with(".luau") && !f.contains('/')).map(|f| (f, bytes)))
            .map(|(file, bytes)| Ok((file.to_string(), String::from_utf8(bytes.clone()).with_context(|| format!("tests/{file} is not UTF-8"))?)))
            .collect()
    }

    pub fn materialize_skills(&mut self, log: &crate::eventlog::EventLog) -> Result<()> {
        let base = log.dir().join("registry").join(&log.session);
        for package in &mut self.packages {
            if package.kind != "skill" { continue; }
            let target = base.join(&package.name);
            for file in &package.files {
                let key = format!("{}/{file}", package.name);
                if let Some(bytes) = self.assets.get(&key) {
                    let path = target.join(file);
                    std::fs::create_dir_all(path.parent().expect("a joined file path has a parent"))?;
                    std::fs::write(path, bytes)?;
                }
            }
            package.dir = target.to_string_lossy().to_string();
        }
        Ok(())
    }
}

/// `tests/*.luau` of a directory on disk, as (file name, source), sorted.
pub fn test_files(dir: &Path) -> Result<Vec<(String, String)>> {
    let tests = dir.join("tests");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&tests).with_context(|| format!("cannot read {}", tests.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "luau") && path.is_file() {
            let name = path.file_name().expect("a directory entry has a name").to_string_lossy().to_string();
            out.push((name, std::fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?));
        }
    }
    out.sort();
    Ok(out)
}

/// What `hx test DIR` runs.
pub enum TestTarget {
    /// The tests of a project package, inside its boundary: `mount` is the unapproved snapshot to load, or None for an
    /// installed package, which must be approved and unchanged.
    Package { name: String, mount: Option<Box<Snapshot>> },
    /// Not a project package (the built-in std package's suite, or a directory without `hx.toml`): the tests run in
    /// the harness's module environment with the agent's authority, as `hx exec` would. `note` says so when the
    /// owner might expect otherwise.
    Harness { note: Option<String> },
}

/// The value of `description:` in SKILL.md frontmatter (plain, quoted, or a `>`/`|` block).
fn frontmatter_description(text: &str) -> String {
    let Some(fm) = text.strip_prefix("---\n").and_then(|t| t.split_once("\n---").map(|(f, _)| f)) else { return String::new() };
    let mut lines = fm.lines();
    while let Some(l) = lines.next() {
        let Some(v) = l.strip_prefix("description:") else { continue };
        let v = v.trim();
        if v.is_empty() || v.starts_with('>') || v.starts_with('|') {
            let block: Vec<&str> = lines.take_while(|l| l.starts_with(' ') || l.starts_with('\t') || l.is_empty()).map(str::trim).collect();
            return block.join(" ").trim().to_string();
        }
        return v.trim_matches(|c| c == '"' || c == '\'').to_string();
    }
    String::new()
}

fn file_for(module: &str) -> String {
    if module.is_empty() { "init.luau".into() } else { format!("{module}.luau") }
}

fn split_name(name: &str) -> (&str, &str) {
    name.split_once('/').unwrap_or((name, ""))
}

/// Text as the owner's terminal shows it in a review: control characters (except newline and tab) and bidirectional
/// formatting characters are escaped, so model-written bytes cannot move the cursor, recolour or reorder what the
/// owner reads before answering.
fn terminal_safe(text: &str) -> String {
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

/// Write a whole file so a crash leaves the old content or the new, never a truncated one.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().expect("an absolute file path has a parent"))?;
    crate::atomic_file::replace_trusted(path, bytes).with_context(|| format!("cannot write {}", path.display()))
}

/// Something staged, frozen as the owner reviewed it.
enum Staged {
    Package { name: String, snapshot: Box<Snapshot>, replaces: bool },
    Module { module: String, new: String, base_sha: String },
}

/// What `hx accept` shows and then installs: `text` is rendered from the frozen bytes, and accepting installs exactly
/// those bytes.
pub struct Review {
    pub text: String,
    staged: Staged,
}

impl Review {
    /// The confirmation question, naming what accepting replaces.
    pub fn question(&self) -> String {
        match &self.staged {
            Staged::Package { name, replaces: true, .. } => format!("accept {name}? it REPLACES the installed package {name}, and is locked and granted the capabilities above"),
            Staged::Package { name, .. } => format!("accept {name}? it will be locked and granted the capabilities above"),
            Staged::Module { module, .. } => format!("accept {module}? it replaces the module in force for new runs"),
        }
    }
}

/// `.harness/staged/std/<module>/meta.json`.
#[derive(serde::Serialize, Deserialize)]
struct ModuleMeta {
    reason: String,
    base_sha: String,
}

impl Resolver {
    pub fn new(root: PathBuf) -> Resolver {
        Resolver { root }
    }

    fn harness(&self) -> PathBuf {
        self.root.join(".harness")
    }

    pub fn embedded(pkg: &str, file: &str) -> Option<&'static str> {
        EMBEDDED.iter().find(|(p, n, _)| *p == pkg && *n == file).map(|(_, _, t)| *t)
    }

    /// `.agents/skills` roots, project first: `<ws>/.agents/skills`, then `~/.agents/skills`.
    fn agent_skill_roots(&self) -> Vec<PathBuf> {
        let mut v = vec![self.root.join(".agents/skills")];
        if let Ok(h) = std::env::var("HOME") {
            v.push(PathBuf::from(h).join(".agents/skills"));
        }
        v
    }

    fn agent_skill_dir(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() || name.starts_with('.') || name.contains('/') {
            return None;
        }
        self.agent_skill_roots().into_iter().map(|r| r.join(name)).find(|d| d.join("SKILL.md").is_file())
    }

    /// Where package `name` lives: `.harness/pkgs/<name>`, else an agent skill.
    fn locate(&self, name: &str) -> Result<Location> {
        let pdir = self.harness().join("pkgs").join(name);
        if pdir.is_dir() {
            return Ok(Location::Package(pdir));
        }
        if let Some(d) = self.agent_skill_dir(name) {
            return Ok(Location::AgentSkill(d));
        }
        bail!("no package or skill named {name} (looked in .harness/pkgs, .agents/skills, ~/.agents/skills)")
    }

    /// Every skill found under the `.agents/skills` roots: (name, dir), first root wins.
    pub fn agent_skills(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for root in self.agent_skill_roots() {
            let Ok(rd) = std::fs::read_dir(&root) else { continue };
            let mut names: Vec<String> = rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            names.sort();
            for n in names {
                if !out.iter().any(|(m, _)| *m == n) && self.agent_skill_dir(&n).is_some_and(|d| d.parent() == Some(root.as_path())) {
                    out.push((n.clone(), root.join(&n)));
                }
            }
        }
        out
    }

    /// Every installed package (`.harness/pkgs`, sorted), then every agent skill no package shadows. Dot-entries in
    /// `.harness/pkgs` (an interrupted `hx accept`) are not packages.
    fn installed(&self) -> Vec<(String, Location)> {
        let mut out: Vec<(String, Location)> = std::fs::read_dir(self.harness().join("pkgs")).into_iter().flatten().flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| (e.file_name().to_string_lossy().to_string(), Location::Package(e.path())))
            .filter(|(name, _)| !name.starts_with('.'))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, dir) in self.agent_skills() {
            if !out.iter().any(|(n, _)| *n == name) {
                out.push((name, Location::AgentSkill(dir)));
            }
        }
        out
    }

    /// `hx skills`: every agent skill and whether the model can see it.
    pub fn skills_status(&self) -> Result<String> {
        let lock = Lockfile::read(&self.root.join("hx.lock"))?;
        let mut s = String::new();
        for (name, dir) in self.agent_skills() {
            let state = if self.harness().join("pkgs").join(&name).is_dir() {
                "shadowed by .harness/pkgs".to_string()
            } else if !valid_package_name(&name) {
                "error: not a valid skill name".to_string()
            } else {
                match lock.approved(&name, Location::AgentSkill(dir.clone())) {
                    Ok(_) => "approved".into(),
                    Err(e) => match e.downcast_ref::<LockError>() {
                        Some(LockError::NotLocked(_)) => "not approved".into(),
                        Some(LockError::Changed { .. }) => "changed since approval".into(),
                        None => format!("error: {}", terminal_safe(e.to_string().lines().next().unwrap_or(""))),
                    },
                }
            };
            s.push_str(&format!("{name:<28} {state:<24} {}\n", dir.display()));
        }
        if !crate::trust::is_trusted(&self.root) {
            s.insert_str(0, "workspace config not trusted: no skill is visible until you run `hx trust`\n");
        }
        Ok(if s.is_empty() { "no skills in .agents/skills or ~/.agents/skills\n".into() } else { s })
    }

    /// What `hx test DIR` runs: see `TestTarget`. `dir` must be canonical.
    pub fn test_target(&self, dir: &Path) -> Result<TestTarget> {
        let inside_harness = dir.starts_with(self.harness());
        if !dir.join("hx.toml").exists() {
            anyhow::ensure!(!inside_harness, "{} has no hx.toml, so it is not a package", dir.display());
            let note = format!("{} has no hx.toml, so it is not a package: its tests run with the agent's authority, like `hx exec`", dir.display());
            return Ok(TestTarget::Harness { note: Some(note) });
        }
        if dir.parent() == Some(self.harness().join("pkgs").as_path()) {
            let name = dir.file_name().expect("a package directory has a name").to_string_lossy().to_string();
            anyhow::ensure!(valid_package_name(&name), "{name:?} is not a valid package name");
            return Ok(TestTarget::Package { name, mount: None });
        }
        let snap = snapshot(Location::Package(dir.to_path_buf()))?;
        let name = match &snap.manifest.package.name {
            Some(name) => name.clone(),
            None => dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        };
        if name == "std" && !inside_harness {
            return Ok(TestTarget::Harness { note: None });
        }
        anyhow::ensure!(valid_package_name(&name), "{name:?} is not a valid package name");
        Ok(TestTarget::Package { name, mount: Some(Box::new(snap)) })
    }

    /// Record `snap` as the approved content of `name` in `hx.lock`.
    fn lock(&self, name: &str, snap: &Snapshot) -> Result<()> {
        let lock_path = self.root.join("hx.lock");
        let mut entries = Lockfile::read(&lock_path)?.package;
        entries.retain(|e| e.name != name);
        entries.push(LockEntry { name: name.into(), version: snap.manifest.package.version.clone(), hash: snap.digest.clone() });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        write_atomic(&lock_path, toml::to_string(&Lockfile { package: entries })?.as_bytes())
    }

    /// `hx approve <pkg>`: lock the package's exact content, which grants exactly the capabilities its manifest requests.
    pub fn approve(&self, pkg: &str) -> Result<String> {
        anyhow::ensure!(valid_package_name(pkg), "{pkg:?} is not a valid package name (lowercase letters, digits, _ and -, starting with a letter; not std or project)");
        let snap = snapshot(self.locate(pkg)?)?;
        self.lock(pkg, &snap)?;
        let dir = snap.location.dir();
        let mut out = format!("approved {pkg} {} ({}…) from {}", snap.manifest.package.version, &snap.digest[..8], dir.display());
        if let Ok(target) = std::fs::read_link(dir) {
            out.push_str(&format!(" (a link to {}; its content is what was approved)", target.display()));
        }
        if snap.is_agent_skill() {
            out.push_str(": an agent skill, so SKILL.md and its files only, with no Luau and no capabilities");
            if snap.files.contains_key("hx.toml") {
                out.push_str(" (its hx.toml is an ordinary file; move the skill to .harness/pkgs to use one)");
            }
        } else {
            out.push_str(&format!(" with capabilities {:?}", snap.manifest.capabilities));
        }
        Ok(out)
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Staging: everything the agent proposes waits under .harness/staged/ until the user accepts it there.
    //   <pkg>/                  a package (an agent-written tool): accepted into .harness/pkgs/<pkg>, locked and granted
    //   std/<module>/new.luau   a replacement for a built-in module: accepted into .harness/override/, rollback-able
    // Nothing staged is active, and nothing the agent can call accepts.
    // ---------------------------------------------------------------------------------------------------------------
    pub fn staged_dir(&self) -> PathBuf {
        self.harness().join("staged")
    }

    /// Stage a package (replacing a previous staging of the same name).
    pub fn stage(&self, name: &str, files: &[(String, String)]) -> Result<PathBuf> {
        anyhow::ensure!(name != "std", "std is the built-in package; stage a replacement for one of its modules instead");
        anyhow::ensure!(valid_package_name(name), "{name:?} is not a valid package name");
        let dir = self.staged_dir().join(name);
        remove_dir_if_present(&dir)?;
        for (rel, text) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().expect("a joined file path has a parent"))?;
            std::fs::write(p, text)?;
        }
        Ok(dir)
    }

    /// Stage a replacement for a built-in module (replacing a previous staging of it), recording the module it was
    /// written against.
    pub fn stage_module(&self, module: &str, source: &str, reason: &str) -> Result<PathBuf> {
        Self::module_file(module)?;
        if source.trim().is_empty() || source.len() > 200_000 {
            bail!("a module replacement needs 1 byte to 200 KB of Luau source");
        }
        let base_sha = crate::eventlog::sha_bytes(self.effective_source(module)?.as_bytes());
        let dir = self.staged_dir().join(module);
        remove_dir_if_present(&dir)?;
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("new.luau"), source)?;
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&ModuleMeta { reason: reason.to_string(), base_sha })?)?;
        Ok(dir)
    }

    /// Everything staged: packages, then module replacements as `std/<module>`.
    pub fn list_staged(&self) -> Vec<String> {
        let dirs = |d: PathBuf| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d).map(|rd| rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
            v.sort();
            v
        };
        let mut v: Vec<String> = dirs(self.staged_dir()).into_iter().filter(|n| n != "std").collect();
        v.extend(dirs(self.staged_dir().join("std")).into_iter().map(|m| format!("std/{m}")));
        v
    }

    /// The staging directory of NAME: a package name or `std/<module>`, never a path out of the staging area.
    fn staged(&self, name: &str) -> Result<PathBuf> {
        let valid = if name.contains('/') { Self::module_file(name).is_ok() } else { valid_package_name(name) };
        let dir = self.staged_dir().join(name);
        if !valid || !dir.is_dir() {
            bail!("nothing staged as {name:?} (see `hx staged`)");
        }
        Ok(dir)
    }

    /// The staged module replacement as stored: its source and metadata.
    fn staged_module(dir: &Path) -> Result<(String, ModuleMeta)> {
        let new = std::fs::read_to_string(dir.join("new.luau")).with_context(|| format!("cannot read {}/new.luau", dir.display()))?;
        let meta = std::fs::read_to_string(dir.join("meta.json")).with_context(|| format!("cannot read {}/meta.json", dir.display()))?;
        Ok((new, serde_json::from_str(&meta).context("bad meta.json")?))
    }

    /// What the user reviews before accepting, frozen: a package's every file (and what it replaces), or a module
    /// replacement's reason, structural check and diff against the module in force. `accept` installs exactly this.
    pub fn review(&self, name: &str) -> Result<Review> {
        let dir = self.staged(name)?;
        if name.contains('/') {
            let (new, meta) = Self::staged_module(&dir)?;
            let check = self.check_module(name, &new).unwrap_or_else(|e| format!("FAILS: {e}"));
            let diff = self.diff(name, &new)?;
            let text = format!("== staged module {name} ==\nreason: {}\ncheck: {}\n\n{}", terminal_safe(&meta.reason), terminal_safe(&check), terminal_safe(&diff));
            return Ok(Review { text, staged: Staged::Module { module: name.to_string(), new, base_sha: meta.base_sha } });
        }
        let snapshot = snapshot(Location::Package(dir))?;
        let installed = self.harness().join("pkgs").join(name);
        let replaces = std::fs::symlink_metadata(&installed).is_ok();
        let man = &snapshot.manifest;
        let mut text = format!("== staged package {name} ==\nversion {}, kind {:?}, capabilities {:?}\n{} files, digest {}\n",
            terminal_safe(&man.package.version), terminal_safe(man.package.kind.as_deref().unwrap_or("")), man.capabilities, snapshot.files.len(), snapshot.digest);
        if replaces {
            let version = snapshot_version(&installed);
            text.push_str(&format!("REPLACES the installed package {name} ({version})\n"));
        }
        // the manifest first, then every file the digest covers
        let files = snapshot.files.get_key_value("hx.toml").into_iter().chain(snapshot.files.iter().filter(|(f, _)| *f != "hx.toml"));
        for (file, bytes) in files {
            match std::str::from_utf8(bytes) {
                Ok(t) => text.push_str(&format!("\n--- {} ---\n{}\n", terminal_safe(file), terminal_safe(t))),
                Err(_) => text.push_str(&format!("\n--- {} (binary, {} bytes) ---\n", terminal_safe(file), bytes.len())),
            }
        }
        Ok(Review { text, staged: Staged::Package { name: name.to_string(), snapshot: Box::new(snapshot), replaces } })
    }

    /// `diff -u` of the module in force against `new`.
    fn diff(&self, module: &str, new: &str) -> Result<String> {
        use std::io::Write;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let current = self.effective_source(module)?;
        let path = std::env::temp_dir().join(format!("hx-review-{}-{}.luau", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?.write_all(current.as_bytes())?;
        let output = (|| {
            let mut diff = std::process::Command::new("diff").args(["-u", "--label", "current", "--label", "staged"]).arg(&path).arg("-")
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().context("cannot run diff")?;
            diff.stdin.take().expect("stdin is piped").write_all(new.as_bytes())?;
            Ok::<_, anyhow::Error>(diff.wait_with_output()?)
        })();
        std::fs::remove_file(&path)?;
        Ok(String::from_utf8_lossy(&output?.stdout).into_owned())
    }

    /// `hx accept NAME`: activate exactly what `review` showed. A package is installed into pkgs/ and locked, which
    /// grants the capabilities it requests; a module replacement becomes the override. Refused when the staged
    /// content changed after the review. Only the user's CLI reaches this.
    pub fn accept(&self, review: Review) -> Result<String> {
        match review.staged {
            Staged::Package { name, snapshot: snap, .. } => {
                let dir = self.staged(&name)?;
                let now = snapshot(Location::Package(dir.clone()));
                anyhow::ensure!(now.is_ok_and(|now| now.digest == snap.digest), "the staged {name} changed after it was shown; run `hx accept {name}` again to review it");
                let to = self.install(&name, &snap.files)?;
                self.lock(&name, &snap)?;
                std::fs::remove_dir_all(&dir)?;
                Ok(format!("approved {name} {} ({}…) into {} with capabilities {:?}", snap.manifest.package.version, &snap.digest[..8], to.display(), snap.manifest.capabilities))
            },
            Staged::Module { module, new, base_sha } => {
                let dir = self.staged(&module)?;
                let (now, meta) = Self::staged_module(&dir)?;
                anyhow::ensure!(now == new && meta.base_sha == base_sha, "the staged {module} changed after it was shown; run `hx accept {module}` again to review it");
                self.accept_module(&module, &dir, &new, &meta)
            },
        }
    }

    /// Install `files` as `.harness/pkgs/<name>`, replacing what is there: the new tree is written beside it and
    /// renamed into place, so a crash leaves the old package, the new one, or none, never a mixture.
    fn install(&self, name: &str, files: &BTreeMap<String, Vec<u8>>) -> Result<PathBuf> {
        let pkgs = self.harness().join("pkgs");
        let to = pkgs.join(name);
        let fresh = pkgs.join(format!(".{name}.new-{}", std::process::id()));
        let old = pkgs.join(format!(".{name}.old-{}", std::process::id()));
        remove_dir_if_present(&fresh)?;
        remove_dir_if_present(&old)?;
        for (rel, bytes) in files {
            let path = fresh.join(rel);
            std::fs::create_dir_all(path.parent().expect("a joined file path has a parent"))?;
            std::fs::write(&path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
        }
        if std::fs::symlink_metadata(&to).is_ok() {
            std::fs::rename(&to, &old).with_context(|| format!("cannot move {} aside", to.display()))?;
        }
        std::fs::rename(&fresh, &to).with_context(|| format!("cannot install {}", to.display()))?;
        remove_dir_if_present(&old)?;
        Ok(to)
    }

    pub fn reject(&self, name: &str) -> Result<()> {
        std::fs::remove_dir_all(self.staged(name)?)?;
        Ok(())
    }

    fn module_file(module: &str) -> Result<(String, String)> {
        let (pkg, m) = split_name(module);
        if pkg != "std" || m.is_empty() || !m.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            bail!("only built-in harness modules can be replaced, e.g. std/printer (got {module:?})");
        }
        if Self::embedded(pkg, &file_for(m)).is_none() {
            bail!("no built-in module {module}");
        }
        Ok((pkg.to_string(), m.to_string()))
    }

    fn override_path(&self, module: &str) -> Result<PathBuf> {
        let (pkg, m) = Self::module_file(module)?;
        Ok(self.harness().join("override").join(pkg).join(file_for(&m)))
    }

    /// The source in force right now: the active override, or the built-in module.
    pub fn effective_source(&self, module: &str) -> Result<String> {
        let (pkg, m) = Self::module_file(module)?;
        let path = self.override_path(module)?;
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::embedded(&pkg, &file_for(&m)).expect("module_file checked it is embedded").to_string()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// Structural check: the replacement loads, returns a table, and keeps every export of the module in force.
    fn check_module(&self, module: &str, new: &str) -> Result<String> {
        let base = self.effective_source(module)?;
        let lua = mlua::Lua::new();
        // the candidate is model-written and runs in the user's CLI: bound its time and memory like a session chunk
        lua.set_memory_limit(256 << 20)?;
        let allowance = if cfg!(test) { std::time::Duration::from_millis(200) } else { std::time::Duration::from_secs(10) };
        let deadline = std::time::Instant::now() + allowance;
        lua.set_interrupt(move |_| {
            if std::time::Instant::now() > deadline {
                return Err(mlua::Error::runtime("time limit exceeded (infinite loop?)"));
            }
            Ok(mlua::VmState::Continue)
        });
        // Loading may `require` other modules: they get a permissive stub so only this module's own shape is judged.
        // `base:<this module>` gets the REAL current module, so a wrapper that copies it keeps its exports.
        let proxy: mlua::Table = lua.load(r#"return setmetatable({}, { __index = function() return function() return nil end end, __call = function() return nil end })"#).eval()?;
        let make_env = |base: Option<mlua::Table>| -> Result<mlua::Table> {
            let (proxy, module) = (proxy.clone(), module.to_string());
            let req = lua.create_function(move |_, name: String| -> mlua::Result<mlua::Value> {
                match &base {
                    Some(b) if name == format!("base:{module}") => Ok(mlua::Value::Table(b.clone())),
                    _ => Ok(mlua::Value::Table(proxy.clone())),
                }
            })?;
            let env = lua.create_table()?;
            env.set("require", req)?;
            let mt = lua.create_table()?;
            mt.set("__index", lua.globals())?;
            env.set_metatable(Some(mt))?;
            Ok(env)
        };
        let load = |name: &str, src: &str, env: mlua::Table| -> Result<mlua::Table> {
            let v: mlua::Value = lua.load(src).set_name(format!("={name}")).set_environment(env).into_function().map_err(|e| anyhow!("{name} does not compile: {e}"))?.call(()).map_err(|e| anyhow!("{name} fails when loaded: {e}"))?;
            match v {
                mlua::Value::Table(t) => Ok(t),
                other => bail!("{name} must return a table (returned {})", other.type_name()),
            }
        };
        let b = load("current module", &base, make_env(None)?)?;
        let n = load("staged module", new, make_env(Some(b.clone()))?)?;
        let mut missing = vec![];
        for pair in b.pairs::<String, mlua::Value>() {
            let (k, _) = pair?;
            if matches!(n.get::<mlua::Value>(k.as_str())?, mlua::Value::Nil) {
                missing.push(k);
            }
        }
        if !missing.is_empty() {
            missing.sort();
            bail!("the staged {module} drops exports that other modules use: {}", missing.join(", "));
        }
        Ok("loads, returns a table, keeps every export".into())
    }

    fn versions_dir(&self, module: &str) -> PathBuf {
        self.harness().join("versions").join(module.replace('/', "__"))
    }

    fn log_upgrade(&self, mut rec: serde_json::Value) -> Result<()> {
        use std::io::Write;
        rec["ts"] = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs().into();
        writeln!(std::fs::OpenOptions::new().create(true).append(true).open(self.harness().join("upgrades.jsonl"))?, "{rec}")?;
        Ok(())
    }

    fn accept_module(&self, module: &str, dir: &Path, new: &str, meta: &ModuleMeta) -> Result<String> {
        // The replacement covers the whole module and was written against the module in force when it was staged: if
        // that module has changed since (another accept, a rollback), accepting would silently drop the change.
        let current = crate::eventlog::sha_bytes(self.effective_source(module)?.as_bytes());
        if current != meta.base_sha {
            bail!("{module} has changed since this replacement was staged; stage it again against the current module");
        }
        self.check_module(module, new)?;
        let target = self.override_path(module)?;
        let vdir = self.versions_dir(module);
        std::fs::create_dir_all(&vdir)?;
        // Each accept saves what it replaces: NNNN.luau holds the previous override, NNNN.none records that there was none.
        let n = std::fs::read_dir(&vdir)?.count() + 1;
        match std::fs::read(&target) {
            Ok(previous) => write_atomic(&vdir.join(format!("{n:04}.luau")), &previous)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_atomic(&vdir.join(format!("{n:04}.none")), b"")?,
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", target.display())),
        }
        write_atomic(&target, new.as_bytes())?;
        std::fs::remove_dir_all(dir)?;
        self.log_upgrade(serde_json::json!({"action": "accept", "module": module, "reason": meta.reason, "new_sha": crate::eventlog::sha_bytes(new.as_bytes()), "version": n}))?;
        Ok(format!("activated {module} (version {n}); undo with `hx rollback {module}`"))
    }

    /// `hx rollback MODULE`: restore what the last accepted replacement of MODULE replaced.
    pub fn rollback(&self, module: &str) -> Result<String> {
        let target = self.override_path(module)?;
        let vdir = self.versions_dir(module);
        let mut files: Vec<_> = match std::fs::read_dir(&vdir) {
            Ok(rd) => rd.map(|e| e.map(|e| e.path())).collect::<std::io::Result<_>>()?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", vdir.display())),
        };
        files.sort();
        let Some(last) = files.pop() else { bail!("nothing to roll back for {module}") };
        let previous = if last.extension().is_some_and(|e| e == "none") { None } else { Some(std::fs::read(&last)?) };
        // Older versions marked "no override" with this text inside a .luau file.
        let restored = match previous.filter(|p| p != b"<<none>>") {
            Some(previous) => { write_atomic(&target, &previous)?; "the previous version" },
            None => {
                match std::fs::remove_file(&target) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e).with_context(|| format!("cannot remove {}", target.display())),
                    _ => {},
                }
                "the built-in module"
            },
        };
        std::fs::remove_file(&last)?;
        self.log_upgrade(serde_json::json!({"action": "rollback", "module": module}))?;
        Ok(format!("rolled back {module} to {restored}"))
    }
}

/// The installed version of a package directory, for the review's replacement notice.
fn snapshot_version(dir: &Path) -> String {
    match snapshot(Location::Package(dir.to_path_buf())) {
        Ok(snap) => format!("version {}", terminal_safe(&snap.manifest.package.version)),
        Err(e) => format!("unreadable: {}", terminal_safe(&e.to_string())),
    }
}

fn remove_dir_if_present(dir: &Path) -> Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e).with_context(|| format!("cannot remove {}", dir.display())),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::frontmatter_description as d;
    use super::*;

    /// A fresh, empty workspace under the temp dir.
    fn ws(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-modules-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn write(path: PathBuf, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// `.harness/pkgs/<name>` with an fs=["read"] capability.
    fn install_pkg(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(".harness/pkgs").join(name);
        write(dir.join("hx.toml"), "[package]\nversion = \"1.0\"\n\n[capabilities]\nfs = [\"read\"]\n");
        write(dir.join("init.luau"), "return { x = 1 }\n");
        dir
    }

    /// The registry a run would capture once the owner trusts the workspace config as it is now.
    fn trusted_registry(root: &Path) -> Registry {
        Registry::build(root, Some(crate::trust::Config::read(root).unwrap())).unwrap()
    }

    /// The lock's verdict on a package directory.
    fn verdict(root: &Path, name: &str, dir: &Path) -> Result<Snapshot> {
        Lockfile::read(&root.join("hx.lock")).unwrap().approved(name, Location::Package(dir.to_path_buf()))
    }

    fn accept(r: &Resolver, name: &str) -> Result<String> {
        r.review(name).and_then(|review| r.accept(review))
    }

    fn err(r: Result<impl Sized>) -> String {
        match r {
            Ok(_) => panic!("expected an error"),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn frontmatter_description_forms() {
        assert_eq!(d("---\nname: a\ndescription: plain words\n---\nbody"), "plain words");
        assert_eq!(d("---\ndescription: \"quoted: yes\"\n---\n"), "quoted: yes");
        assert_eq!(d("---\ndescription: >\n  folded\n  lines\nname: a\n---\n"), "folded lines");
        assert_eq!(d("---\ndescription: |\n  block\n---\n"), "block");
        assert_eq!(d("no frontmatter"), "");
    }

    #[test]
    fn package_must_be_locked_and_unchanged() {
        let root = ws("lock");
        let dir = install_pkg(&root, "foo");
        let r = Resolver::new(root.clone());
        assert!(err(verdict(&root, "foo", &dir)).contains("not locked"));
        // Approval records the exact content, manifest (and so capabilities) included.
        let msg = r.approve("foo").unwrap();
        assert!(msg.starts_with("approved foo 1.0"), "{msg}");
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        assert!(lock.contains("name = \"foo\"") && lock.contains("version = \"1.0\""), "{lock}");
        assert!(!lock.contains("grants"), "{lock}");
        assert!(verdict(&root, "foo", &dir).is_ok());
        // any content change invalidates the lock
        std::fs::write(dir.join("init.luau"), "return { x = 2 }\n").unwrap();
        assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
        // so does a version that differs from the locked one
        std::fs::write(dir.join("hx.toml"), "[package]\nversion = \"2.0\"\n\n[capabilities]\nfs = [\"read\"]\n").unwrap();
        // lock the new content under the old version: only the version differs
        let digest = snapshot(Location::Package(dir.clone())).unwrap().digest;
        std::fs::write(root.join("hx.lock"), format!("[[package]]\nname = \"foo\"\nversion = \"1.0\"\nhash = {digest:?}\n")).unwrap();
        assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn capabilities_come_from_the_locked_manifest() {
        let root = ws("grants");
        let dir = install_pkg(&root, "foo");
        let r = Resolver::new(root.clone());
        r.approve("foo").unwrap();
        // A lock written before grants were dropped still parses; its extra table is ignored.
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        std::fs::write(root.join("hx.lock"), format!("{lock}\n[package.grants]\nfs = [\"read\"]\n")).unwrap();
        assert!(verdict(&root, "foo", &dir).is_ok());
        // Requesting more is a manifest edit, so it changes the digest and needs a new approval.
        std::fs::write(dir.join("hx.toml"), "[package]\nversion = \"1.0\"\n\n[capabilities]\nfs = [\"read\", \"write\"]\n").unwrap();
        assert!(err(verdict(&root, "foo", &dir)).contains("changed since it was approved"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_misspelt_capability_or_table_is_a_load_error() {
        let parse = |text: &str| toml::from_str::<Manifest>(text).map_err(|e| e.to_string());
        assert!(parse("[package]\nversion = \"1\"\n[capabilities]\nfs = [\"read\"]\nllm = [\"*\"]\n").unwrap().capabilities.contains(crate::supervisor::Capability::LlmChat));
        assert!(parse("[package]\nversion = \"1\"\n[capabilities]\nfs = [\"raed\"]\n").unwrap_err().contains("unknown capability fs.raed"));
        assert!(parse("[package]\nversion = \"1\"\n[capabilities]\nprocess = [\"sh\"]\n").unwrap_err().contains("unknown capability process.sh"));
        assert!(parse("[package]\nversion = \"1\"\n[capabilites]\nfs = [\"read\"]\n").is_err());
        // Older generated manifests carry [exports] and [tests]; they still load.
        assert!(parse("[package]\nversion = \"1\"\n[exports]\nmodule = \"init.luau\"\n[tests]\nfiles = [\"tests/*.luau\"]\n").is_ok());
    }

    #[test]
    fn approve_keeps_other_entries_and_rejects_unknown_names() {
        let root = ws("approve");
        install_pkg(&root, "foo");
        install_pkg(&root, "bar");
        let r = Resolver::new(root.clone());
        r.approve("foo").unwrap();
        r.approve("bar").unwrap();
        r.approve("foo").unwrap();
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        assert_eq!(lock.matches("[[package]]").count(), 2);
        assert!(lock.find("\"bar\"").unwrap() < lock.find("\"foo\"").unwrap(), "sorted by name: {lock}");
        assert!(err(r.approve("nope")).contains("no package or skill named nope"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_are_validated_and_std_cannot_be_shadowed() {
        let root = ws("names");
        for bad in ["std", "project", "my.pkg", "a:b", "Upper", "-x", ""] {
            assert!(!valid_package_name(bad), "{bad}");
        }
        assert!(valid_package_name("video-craft") && valid_package_name("tool_2"));
        let r = Resolver::new(root.clone());
        for bad in ["std", "my.pkg", "a:b"] {
            install_pkg(&root, bad);
            assert!(err(r.approve(bad)).contains("not a valid package name"), "{bad}");
        }
        std::fs::write(root.join("hx.lock"), "").unwrap();
        // an installed package named std neither replaces nor breaks the built-in package
        let registry = trusted_registry(&root);
        assert_eq!(registry.resolve("std").unwrap().origin, "std/init.luau");
        assert!(registry.resolve("std").unwrap().pkg.is_none());
        for bad in ["std", "my.pkg", "a:b"] {
            assert!(registry.rejected[bad].contains("not a valid package name"), "{bad}");
            assert!(!registry.capabilities.contains_key(bad), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frozen_registry_resolves_overrides_packages_and_builtins() {
        let root = ws("resolve");
        let r = Resolver::new(root.clone());
        let registry = trusted_registry(&root);
        let source = registry.resolve("std/printer").unwrap();
        assert_eq!((source.origin.as_str(), source.is_override, source.pkg.is_none()), ("std/printer.luau", false, true));
        assert_eq!(registry.resolve("std").unwrap().origin, "std/init.luau");
        assert!(err(registry.resolve("nope/x")).contains("module not found: nope/x"));
        assert!(err(registry.resolve("project/init")).contains("module not found"));
        write(root.join(".harness/init.luau"), "-- init\n");
        write(root.join(".harness/override/std/printer.luau"), "return {}\n");
        install_pkg(&root, "foo");
        assert!(err(trusted_registry(&root).resolve("foo")).contains("not locked"));
        r.approve("foo").unwrap();
        let registry = trusted_registry(&root);
        assert_eq!(registry.resolve("project/init").unwrap().text, "-- init\n");
        let source = registry.resolve("std/printer").unwrap();
        assert!(source.is_override && source.text == "return {}\n");
        assert!(!registry.resolve("base:std/printer").unwrap().is_override);
        assert_eq!(registry.resolve("foo").unwrap().pkg.unwrap().1, crate::supervisor::Capabilities::from([crate::supervisor::Capability::FsRead]));
        assert!(err(registry.resolve("foo/missing")).contains("module not found"));
        write(root.join(".harness/override/std/printer.luau"), "return {changed=true}\n");
        assert_eq!(registry.resolve("std/printer").unwrap().text, "return {}\n");
        // untrusted config: only the built-in package
        let untrusted = Registry::build(&root, None).unwrap();
        assert!(!untrusted.resolve("std/printer").unwrap().is_override && untrusted.resolve("foo").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn overrides_replace_built_in_files_only() {
        let root = ws("overrides");
        write(root.join(".harness/override/std/system.md"), "You are PLANTED.\n");
        write(root.join(".harness/override/std/nosuch.luau"), "return {}\n");
        write(root.join(".harness/override/foo/init.luau"), "return {}\n");
        install_pkg(&root, "foo");
        Resolver::new(root.clone()).approve("foo").unwrap();
        let registry = trusted_registry(&root);
        assert_eq!(registry.asset("std/system.md").unwrap(), "You are PLANTED.\n");
        assert!(registry.resolve("std/nosuch").is_err());
        // the package keeps its name: an override cannot replace it
        let foo = registry.resolve("foo").unwrap();
        assert!(!foo.is_override && foo.pkg.is_some(), "{foo:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_registry_is_built_from_the_bytes_trust_read() {
        let root = ws("trustbytes");
        install_pkg(&root, "foo");
        Resolver::new(root.clone()).approve("foo").unwrap();
        let config = crate::trust::Config::read(&root).unwrap();
        // hx.lock changes after trust read it: the registry still follows the bytes that were judged
        std::fs::write(root.join("hx.lock"), "").unwrap();
        let registry = Registry::build(&root, Some(config)).unwrap();
        assert!(registry.resolve("foo").is_ok());
        assert!(trusted_registry(&root).resolve("foo").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_digest_frames_paths_and_contents() {
        // Without length framing, `a\0x\0b\0y\0` is both one file a = "x\0b\0y" and two files a = "x", b = "y".
        let one = BTreeMap::from([("a".to_string(), b"x\0b\0y".to_vec())]);
        let two = BTreeMap::from([("a".to_string(), b"x".to_vec()), ("b".to_string(), b"y".to_vec())]);
        assert_ne!(digest(&one), digest(&two));
        let moved = BTreeMap::from([("a".to_string(), b"xb".to_vec()), ("b".to_string(), b"".to_vec())]);
        let kept = BTreeMap::from([("a".to_string(), b"x".to_vec()), ("b".to_string(), b"b".to_vec())]);
        assert_ne!(digest(&moved), digest(&kept));
    }

    #[test]
    fn an_agent_skill_has_no_luau_and_no_capabilities() {
        let root = ws("agentskill");
        let dir = root.join(".agents/skills/foo");
        write(dir.join("SKILL.md"), "---\ndescription: helpful\n---\nbody\n");
        write(dir.join("hx.toml"), "[package]\nversion = \"1\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n");
        write(dir.join("init.luau"), "return { run = function() return sh('id') end }\n");
        let msg = Resolver::new(root.clone()).approve("foo").unwrap();
        assert!(msg.contains("no Luau and no capabilities") && msg.contains("hx.toml is an ordinary file"), "{msg}");
        let registry = trusted_registry(&root);
        assert!(registry.capabilities["foo"].0.is_empty());
        assert!(registry.resolve("foo").is_err());
        let info = registry.packages.iter().find(|p| p.name == "foo").unwrap();
        assert_eq!((info.kind.as_str(), info.version.as_str(), info.description.as_str()), ("skill", "agent-skill", "helpful"));
        assert_eq!(info.files, ["SKILL.md", "hx.toml", "init.luau"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn one_walker_skips_dot_entries_follows_a_linked_root_and_refuses_inner_links() {
        let root = ws("walker");
        let real = root.join("elsewhere/g");
        write(real.join("SKILL.md"), "---\ndescription: g\n---\ng\n");
        write(real.join(".git/HEAD"), "ref: refs/heads/main\n");
        std::fs::create_dir_all(root.join(".agents/skills")).unwrap();
        std::os::unix::fs::symlink(&real, root.join(".agents/skills/g")).unwrap();
        let r = Resolver::new(root.clone());
        assert!(r.approve("g").unwrap().contains("a link to"));
        // what approve locked is what a run loads
        let registry = trusted_registry(&root);
        assert_eq!(registry.packages.iter().find(|p| p.name == "g").unwrap().files, ["SKILL.md"]);
        assert!(r.skills_status().unwrap().lines().any(|l| l.split_whitespace().take(2).eq(["g", "approved"])));
        // .git churn (git gc) does not un-approve it
        write(real.join(".git/objects/pack/p1"), "pack");
        assert!(trusted_registry(&root).packages.iter().any(|p| p.name == "g"));
        // a link inside is refused by approve and by the run alike
        std::os::unix::fs::symlink("/etc/hostname", real.join("leak.md")).unwrap();
        assert!(err(r.approve("g")).contains("may not contain symlinks"));
        let registry = trusted_registry(&root);
        assert!(registry.packages.is_empty() && registry.rejected["g"].contains("may not contain symlinks"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staged_package_review_accept_reject() {
        let root = ws("staged");
        let r = Resolver::new(root.clone());
        assert!(r.list_staged().is_empty());
        let files = |v: &str| vec![("hx.toml".to_string(), format!("[package]\nversion = \"{v}\"\n")), ("init.luau".to_string(), "return {}\n".to_string()), ("tests/a.luau".to_string(), "-- t\n".to_string())];
        r.stage("foo", &files("1.0")).unwrap();
        r.stage("bar", &files("1.0")).unwrap();
        assert_eq!(r.list_staged(), vec!["bar", "foo"]);
        let review = r.review("foo").unwrap();
        assert!(review.text.contains("--- hx.toml ---") && review.text.contains("--- tests/a.luau ---\n-- t"), "{}", review.text);
        assert!(!review.text.contains("REPLACES") && !review.question().contains("REPLACES"));
        assert!(err(r.review("nope")).contains("nothing staged"));
        assert!(r.accept(review).unwrap().starts_with("approved foo 1.0"));
        assert!(root.join(".harness/pkgs/foo/init.luau").is_file());
        assert_eq!(r.list_staged(), vec!["bar"]);
        // re-staging and accepting replaces the installed copy, and the review says so
        r.stage("foo", &files("1.1")).unwrap();
        let review = r.review("foo").unwrap();
        assert!(review.text.contains("REPLACES the installed package foo (version 1.0)") && review.question().contains("REPLACES"), "{}", review.text);
        assert!(r.accept(review).unwrap().starts_with("approved foo 1.1"));
        assert!(trusted_registry(&root).resolve("foo").is_ok());
        assert!(err(accept(&r, "foo")).contains("nothing staged"));
        r.reject("bar").unwrap();
        assert!(r.list_staged().is_empty());
        assert!(r.reject("bar").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accept_installs_exactly_what_the_review_showed() {
        let root = ws("frozen");
        let r = Resolver::new(root.clone());
        r.stage("foo", &[("hx.toml".into(), "[package]\nversion = \"1\"\n".into()), ("init.luau".into(), "return { f = function() return 1 end }\n".into()),
            ("notes/x.txt".into(), "clear\x1b[2Jscreen \u{202e}desrever\r".into())]).unwrap();
        std::fs::write(root.join(".harness/staged/foo/blob.bin"), [0xff, 0xfe, 0]).unwrap();
        let review = r.review("foo").unwrap();
        // every file is listed, and model bytes cannot drive the terminal
        assert!(review.text.contains("--- notes/x.txt ---") && review.text.contains("--- blob.bin (binary, 3 bytes) ---"), "{}", review.text);
        assert!(review.text.contains("clear\\u{1b}[2Jscreen \\u{202e}desrever\\u{d}"), "{}", review.text);
        assert!(!review.text.chars().any(|c| c.is_control() && c != '\n'));
        // the staged content changes between the review and the answer
        write(root.join(".harness/staged/foo/hx.toml"), "[package]\nversion = \"1\"\n[capabilities]\nproc = [\"sh\"]\n");
        assert!(err(r.accept(review)).contains("changed after it was shown"));
        assert!(!root.join(".harness/pkgs/foo").exists() && !root.join("hx.lock").exists());
        // reviewed again, it installs those exact bytes under the digest shown
        let review = r.review("foo").unwrap();
        let shown = match &review.staged { Staged::Package { snapshot, .. } => snapshot.digest.clone(), Staged::Module { .. } => unreachable!() };
        r.accept(review).unwrap();
        let installed = snapshot(Location::Package(root.join(".harness/pkgs/foo"))).unwrap();
        assert_eq!(installed.digest, shown);
        assert!(std::fs::read_to_string(root.join("hx.lock")).unwrap().contains(&shown));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_builtin_modules_can_be_replaced() {
        assert!(Resolver::module_file("std/printer").is_ok());
        for bad in ["std", "std/", "foo/printer", "std/../x", "std/Printer", "std/nope"] {
            assert!(Resolver::module_file(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn staged_names_cannot_leave_the_staging_area() {
        let root = ws("stagednames");
        let r = Resolver::new(root.clone());
        write(root.join(".harness/pkgs/real/hx.toml"), "[package]\nversion = \"1\"\n");
        for bad in ["", ".", "..", "../pkgs/real", "std", "std/../../pkgs/real", "a/b"] {
            assert!(err(r.reject(bad)).contains("nothing staged"), "{bad}");
        }
        assert!(root.join(".harness/pkgs/real/hx.toml").is_file());
        assert!(err(r.stage("std", &[])).contains("built-in package"));
        assert!(err(r.stage("../pkgs/real", &[])).contains("not a valid package name"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn staged_module_must_load_return_a_table_and_keep_exports() {
        let root = ws("check");
        let r = Resolver::new(root.clone());
        assert!(err(r.stage_module("std/printer", " ", "")).contains("1 byte to 200 KB"));
        assert!(err(r.stage_module("other/x", "return {}", "")).contains("only built-in"));
        let check = |src: &str| {
            r.stage_module("std/printer", src, "").unwrap();
            assert_eq!(r.list_staged(), vec!["std/printer"]);
            let review = r.review("std/printer").unwrap();
            let text = review.text.clone();
            let accepted = r.accept(review);
            if accepted.is_ok() { r.rollback("std/printer").unwrap(); }
            (text, accepted)
        };
        for (src, why) in [("return {", "does not compile"), ("error('boom')", "fails when loaded"), ("return 1", "must return a table"), ("return {}", "drops exports"),
            // model-written code runs in the user's CLI: a loop or a memory bomb must fail the check, not hang it
            ("while true do end", "fails when loaded"), ("local t = {} for i = 1, 1e9 do t[i] = string.rep('x', 1024) .. i end return t", "fails when loaded")] {
            let (review, accepted) = check(src);
            assert!(review.contains("check: FAILS") && review.contains(why), "{src}: {review}");
            assert!(err(accepted).contains(why), "{src}");
        }
        // a wrapper that copies `base:` keeps every export
        let (review, accepted) = check("local b = require('base:std/printer')\nlocal m = {}\nfor k, v in b do m[k] = v end\nreturn m\n");
        assert!(review.contains("check: loads, returns a table, keeps every export") && review.contains("+++ staged"), "{review}");
        assert!(accepted.is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_module_replacement_is_accepted_as_reviewed() {
        let root = ws("modfrozen");
        let r = Resolver::new(root.clone());
        let builtin = r.effective_source("std/printer").unwrap();
        r.stage_module("std/printer", &format!("{builtin}\n-- reviewed\n"), "why\x1b]0;title\x07").unwrap();
        let review = r.review("std/printer").unwrap();
        assert!(review.text.contains("reason: why\\u{1b}]0;title\\u{7}"), "{}", review.text);
        std::fs::write(root.join(".harness/staged/std/printer/new.luau"), format!("{builtin}\n-- swapped\n")).unwrap();
        assert!(err(r.accept(review)).contains("changed after it was shown"));
        assert_eq!(r.effective_source("std/printer").unwrap(), builtin);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn module_accept_refuses_a_replacement_whose_base_is_no_longer_in_force() {
        let root = ws("stale");
        let r = Resolver::new(root.clone());
        let builtin = r.effective_source("std/printer").unwrap();
        r.stage_module("std/printer", &format!("{builtin}\n-- A\n"), "").unwrap();
        accept(&r, "std/printer").unwrap();
        // staged against A, then A is rolled back: accepting now would silently drop the rollback
        r.stage_module("std/printer", &format!("{builtin}\n-- A\n-- B\n"), "").unwrap();
        r.rollback("std/printer").unwrap();
        assert!(err(accept(&r, "std/printer")).contains("changed since this replacement was staged"));
        assert_eq!(r.effective_source("std/printer").unwrap(), builtin);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn module_accept_activates_stacks_and_rolls_back() {
        let root = ws("upgrade");
        let r = Resolver::new(root.clone());
        let builtin = r.effective_source("std/printer").unwrap();
        let new = format!("{builtin}\n-- v2\n");
        r.stage_module("std/printer", &new, "test").unwrap();
        r.stage("tool", &[("hx.toml".into(), "[package]\nversion = \"1\"\n".into())]).unwrap();
        assert_eq!(r.list_staged(), vec!["tool", "std/printer"]);
        assert!(r.review("std/printer").unwrap().text.contains("reason: test"));
        assert!(accept(&r, "std/printer").unwrap().contains("version 1"));
        assert_eq!(r.effective_source("std/printer").unwrap(), new);
        assert_eq!(r.list_staged(), vec!["tool"]);
        // a second replacement stacks on the first
        let newer = format!("{new}-- v3\n");
        r.stage_module("std/printer", &newer, "").unwrap();
        assert!(accept(&r, "std/printer").unwrap().contains("version 2"));
        assert_eq!(r.effective_source("std/printer").unwrap(), newer);
        // rejecting a staged replacement leaves the module in force alone
        r.stage_module("std/printer", "return {}", "").unwrap();
        r.reject("std/printer").unwrap();
        assert!(err(accept(&r, "std/printer")).contains("nothing staged"));
        let log = std::fs::read_to_string(root.join(".harness/upgrades.jsonl")).unwrap();
        assert_eq!(log.matches("\"accept\"").count(), 2);
        assert!(r.rollback("std/printer").unwrap().ends_with("the previous version"));
        assert_eq!(r.effective_source("std/printer").unwrap(), new);
        assert!(r.rollback("std/printer").unwrap().ends_with("the built-in module"));
        assert_eq!(r.effective_source("std/printer").unwrap(), builtin);
        assert!(!root.join(".harness/override/std/printer.luau").exists());
        assert!(err(r.rollback("std/printer")).contains("nothing to roll back"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rollback_names_the_override_it_restores() {
        let root = ws("rollbackmsg");
        let r = Resolver::new(root.clone());
        // the owner's own override is in force before the first accepted replacement
        let mine = format!("{}\n-- mine\n", r.effective_source("std/printer").unwrap());
        write(root.join(".harness/override/std/printer.luau"), &mine);
        r.stage_module("std/printer", &format!("{mine}-- staged\n"), "").unwrap();
        accept(&r, "std/printer").unwrap();
        assert!(r.rollback("std/printer").unwrap().ends_with("the previous version"));
        assert_eq!(r.effective_source("std/printer").unwrap(), mine);
        let _ = std::fs::remove_dir_all(&root);
    }
}
