//! Module resolution, package manifests, the lockfile (approvals) and staging.
//!
//! `require("pkg")` loads `init.luau`; `require("pkg/mod")` loads `mod.luau`. Lookup order (the first wins):
//!   1. `<ws>/.harness/override/<pkg>/<mod>.luau`   user override of any module (e.g. the printer), no rebuild needed;
//!                                                  the owner's explicit choice, so it also beats an approved package
//!   2. `<ws>/.harness/pkgs/<pkg>/<mod>.luau`       project package (needs a lockfile entry for its exact content)
//!   3. the built-in `std` package embedded in the binary
//!
//! Agent skills (the `.agents/skills` convention): `<ws>/.agents/skills/<name>/SKILL.md`, then `~/.agents/skills/<name>/SKILL.md`.
//! Their manifest always comes from SKILL.md frontmatter: they carry no Luau and no capabilities, so an `hx.toml` or
//! `.luau` file inside one is an ordinary file, never a manifest and never loaded. They are locked by `hx approve NAME`
//! like any package. `.harness/pkgs/<name>` shadows them.
//!
//! Approval, verification and capture all read a package once into a `Snapshot` (files, hash, manifest from those same
//! bytes), so what `hx approve` shows, what hx.lock records and what a run loads cannot differ.
//!
//! Everything under (1), (2), `project/init` and the agent skills is used only when the workspace config is trusted
//! (see trust.rs); otherwise only the built-in package resolves.
//!
//! `require("base:std/printer")` skips (1), so an override can wrap the module it replaces.
//! `require("project/init")` is `<ws>/.harness/init.luau`, run before the loop starts (hook registration etc.).
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
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

#[derive(Debug, Deserialize, Default)]
pub struct PackageInfo {
    /// The package's name (the `hx test` mount; installed packages are named by their directory).
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

/// `hx test DIR` mounts the package under test here so it can be required without being installed.
pub static TEST_PKG: std::sync::OnceLock<(String, PathBuf)> = std::sync::OnceLock::new();

/// `hx.lock`: one record per approved package: the exact content approved. The hash covers `hx.toml`, so approving it
/// grants exactly the capabilities that manifest requests; there is no separate grant list to keep in step.
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

/// Lock hashes carry this scheme prefix. Hashes written before it (unframed `path\0content\0`) let two different trees
/// hash alike, so they no longer verify: such a package is reported as needing `hx approve` again, never matched.
const HASH_SCHEME: &str = "v2:";

/// A package's files, relative path to bytes, read once.
type Files = BTreeMap<PathBuf, Vec<u8>>;

fn files_hash(files: &Files) -> String {
    format!("{HASH_SCHEME}{}", crate::trust::tree_hash(files.iter().map(|(p, b)| (p.as_os_str().as_encoded_bytes(), b.as_slice()))))
}

/// The hex digits of a lock hash, shortened for display.
fn short(hash: &str) -> &str {
    let h = hash.strip_prefix(HASH_SCHEME).unwrap_or(hash);
    &h[..8.min(h.len())]
}

/// A package or agent skill read once: what is hashed, approved, shown and loaded are these same bytes.
#[derive(Debug)]
struct Snapshot {
    name: String,
    dir: PathBuf,
    agent_skill: bool,
    files: Files,
    hash: String,
    man: Manifest,
}

/// Whether hx.lock approves a package as it is now.
#[derive(Debug)]
enum Approval {
    Approved(Snapshot),
    NotLocked,
    /// Locked under the old unframed hash: valid once, unverifiable now.
    Outdated,
    Changed { locked: String, now: String },
}

impl Approval {
    fn refusal(&self, pkg: &str) -> String {
        match self {
            Approval::Approved(_) => String::new(),
            Approval::NotLocked => format!("package {pkg} is not locked; review it, then run `hx approve {pkg}`"),
            Approval::Outdated => format!("package {pkg} was approved by an older hx whose content hash this version no longer accepts; review it and run `hx approve {pkg}` again"),
            Approval::Changed { locked, now } => format!("package {pkg} changed since it was approved (locked {}…, now {}…); review it and run `hx approve {pkg}` again", short(locked), short(now)),
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

    pub fn capture(root: &Path) -> Result<Self> {
        let resolver = Resolver::new(root.to_path_buf());
        let trusted = crate::trust::is_trusted(root);
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
        if trusted {
            // One pass: each package is read, hashed and checked against hx.lock once, and loaded from those bytes.
            let (approved, rejected) = resolver.approved()?;
            registry.rejected = rejected;
            'packages: for snap in approved {
                let name = snap.name.clone();
                let caps = snap.man.capabilities.clone();
                let mut sources = Vec::new();
                // agent skills carry no Luau: their .luau files are plain assets (see the module header)
                for (relative, bytes) in snap.files.iter().filter(|_| !snap.agent_skill) {
                    let file = relative.to_string_lossy().replace('\\', "/");
                    let Some(module) = file.strip_suffix(".luau") else { continue };
                    let Ok(text) = String::from_utf8(bytes.clone()) else {
                        registry.rejected.insert(name.clone(), format!("package {name}: {file} is not UTF-8 text"));
                        continue 'packages;
                    };
                    let module = if module == "init" { name.clone() } else { format!("{name}/{module}") };
                    sources.push((module, Source { text, origin: format!("{name}/{file}"), is_override: false, pkg: Some((name.clone(), caps.clone())) }));
                }
                registry.sources.extend(sources);
                registry.capabilities.insert(name.clone(), caps);
                registry.packages.push(Resolver::info(&snap));
                for (relative, bytes) in snap.files {
                    registry.assets.insert(format!("{name}/{}", relative.to_string_lossy().replace('\\', "/")), bytes);
                }
            }
            // After packages: an override is the owner's explicit choice and wins over a package module of the same name.
            let overrides = root.join(".harness/override");
            if overrides.is_dir() {
                for (relative, bytes) in frozen_files(&overrides)? {
                    let key = relative.to_string_lossy().replace('\\', "/");
                    // Only modules are decoded here; other files (e.g. std/system.md) stay bytes until used.
                    if let Some(name) = key.strip_suffix(".luau") {
                        let text = String::from_utf8(bytes.clone()).map_err(|_| anyhow!("module override .harness/override/{key} is not UTF-8 text"))?;
                        let name = name.strip_suffix("/init").unwrap_or(name).to_string();
                        registry.sources.insert(name, Source { text, origin: key.clone(), is_override: true, pkg: None });
                    }
                    registry.assets.insert(key, bytes);
                }
            }
            let init = root.join(".harness/init.luau");
            if init.is_file() {
                anyhow::ensure!(!std::fs::symlink_metadata(&init)?.file_type().is_symlink(), "project init must not be a symlink");
                registry.sources.insert("project/init".into(), Source { text: std::fs::read_to_string(init)?, origin: "project/init".into(), is_override: false, pkg: None });
            }
            registry.mcp = crate::mcp::snapshot(root)?;
            crate::tools::capture(root, &mut registry)?;
        }
        // Explicit developer test invocation is the sole unapproved package mount. It is never restored into a run.
        if let Some((name, dir)) = TEST_PKG.get().filter(|(name, _)| name != "std") {
            let man = Resolver::read_manifest(name, dir)?;
            registry.capabilities.insert(name.clone(), man.capabilities.clone());
            for (relative, bytes) in frozen_files(dir)? {
                let file = relative.to_string_lossy().replace('\\', "/");
                if let Some(module) = file.strip_suffix(".luau") {
                    let key = if module == "init" { name.clone() } else { format!("{name}/{module}") };
                    registry.sources.insert(key, Source { text: String::from_utf8(bytes)?, origin: file, is_override:false, pkg:Some((name.clone(), man.capabilities.clone())) });
                }
            }
        }
        Ok(registry)
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
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    std::fs::write(path, bytes)?;
                }
            }
            package.dir = target.to_string_lossy().to_string();
        }
        Ok(())
    }
}

fn frozen_files(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.file_name().is_some_and(|n| n == ".DS_Store") { continue; }
            let meta = std::fs::symlink_metadata(&path)?;
            anyhow::ensure!(!meta.file_type().is_symlink(), "frozen extensions may not contain symlinks: {}", path.display());
            if meta.is_dir() { walk(base, &path, out)?; }
            else { out.insert(path.strip_prefix(base)?.to_path_buf(), std::fs::read(&path)?); }
        }
        Ok(())
    }
    anyhow::ensure!(!std::fs::symlink_metadata(root)?.file_type().is_symlink(), "frozen extension root must not be a symlink");
    let mut files = BTreeMap::new(); walk(root, root, &mut files)?; Ok(files)
}

/// A TOML file, or its default when it does not exist yet.
fn read_toml<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(toml::from_str(&t)?),
        Err(_) => Ok(T::default()),
    }
}

fn file_for(module: &str) -> String {
    if module.is_empty() { "init.luau".into() } else { format!("{module}.luau") }
}

fn split_name(name: &str) -> (&str, &str) {
    name.split_once('/').unwrap_or((name, ""))
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

/// Text for the owner's terminal: control characters other than newline and tab are shown escaped, so staged content
/// cannot hide lines from the review (`\r`, cursor movement, clear-line sequences).
fn printable(text: &str) -> String {
    text.chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { c.escape_unicode().to_string() } else { c.to_string() }).collect()
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
        if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
            return None;
        }
        self.agent_skill_roots().into_iter().map(|r| r.join(name)).find(|d| d.join("SKILL.md").is_file())
    }

    /// The manifest, parsed from the snapshot's own bytes. A `.harness/pkgs` package declares it in hx.toml. An agent
    /// skill never does: kind "skill", no capabilities, description from SKILL.md frontmatter, whatever else it ships.
    fn manifest(name: &str, agent_skill: bool, files: &Files) -> Result<Manifest> {
        let text = |file: &str| -> Result<&str> {
            let bytes = files.get(Path::new(file)).ok_or_else(|| anyhow!("package {name}: missing {file}"))?;
            std::str::from_utf8(bytes).map_err(|_| anyhow!("package {name}: {file} is not UTF-8 text"))
        };
        if !agent_skill {
            return toml::from_str(text("hx.toml")?).map_err(|e| anyhow!("package {name}: bad hx.toml: {e}"));
        }
        let mut m = Manifest::default();
        m.package.version = "agent-skill".into();
        m.package.kind = Some("skill".into());
        m.package.description = Some(frontmatter_description(text("SKILL.md")?));
        Ok(m)
    }

    /// Where package `name` lives: `.harness/pkgs/<name>`, else an agent skill (true).
    fn locate(&self, name: &str) -> Result<(PathBuf, bool)> {
        // a package named std would replace built-in modules; that is what overrides are for
        anyhow::ensure!(name != "std", "std is the built-in package; replace one of its modules with an override instead");
        let pdir = self.harness().join("pkgs").join(name);
        if pdir.is_dir() && !name.is_empty() && !name.starts_with('.') && !name.contains(['/', '\\']) {
            return Ok((pdir, false));
        }
        if let Some(d) = self.agent_skill_dir(name) {
            return Ok((d, true));
        }
        bail!("no package or skill named {name} (looked in .harness/pkgs, .agents/skills, ~/.agents/skills)")
    }

    /// Read package `name` once: its files, their hash and its manifest.
    fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let (dir, agent_skill) = self.locate(name)?;
        let files = frozen_files(&dir).map_err(|e| anyhow!("package {name}: {e}"))?;
        let man = Self::manifest(name, agent_skill, &files)?;
        Ok(Snapshot { name: name.to_string(), hash: files_hash(&files), dir, agent_skill, files, man })
    }

    /// Does `lock` approve package `name` as it is on disk now? Its files are read only when hx.lock has an entry for it
    /// (~/.agents/skills can be large).
    fn approval(&self, lock: &Lockfile, name: &str) -> Result<Approval> {
        let Some(entry) = lock.package.iter().find(|e| e.name == name) else { return Ok(Approval::NotLocked) };
        if !entry.hash.starts_with(HASH_SCHEME) {
            return Ok(Approval::Outdated);
        }
        let snap = self.snapshot(name)?;
        Ok(if entry.hash != snap.hash || entry.version != snap.man.package.version {
            Approval::Changed { locked: entry.hash.clone(), now: snap.hash }
        } else {
            Approval::Approved(snap)
        })
    }

    /// Every package and agent skill hx.lock approves as it is now (what the model may use), and why each other
    /// `.harness/pkgs` package is refused (the error `require` reports for it). Unapproved agent skills are just absent.
    fn approved(&self) -> Result<(Vec<Snapshot>, BTreeMap<String, String>)> {
        let (mut out, mut rejected) = (Vec::new(), BTreeMap::new());
        if !crate::trust::is_trusted(&self.root) {
            return Ok((out, rejected)); // hx.lock is untrusted, so nothing it approves is visible
        }
        let mut names: Vec<String> = std::fs::read_dir(self.harness().join("pkgs")).into_iter().flatten().flatten()
            .filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect();
        names.sort();
        // A malformed hx.lock approves nothing; it does not stop runs that use no package.
        let lock: Lockfile = match read_toml(&self.root.join("hx.lock")) {
            Ok(lock) => lock,
            Err(e) => {
                rejected.extend(names.into_iter().map(|n| (n, format!("hx.lock is not valid TOML, so nothing is approved: {e}"))));
                return Ok((out, rejected));
            },
        };
        for name in names {
            match self.approval(&lock, &name) {
                Ok(Approval::Approved(snap)) => out.push(snap),
                Ok(refused) => { rejected.insert(name.clone(), refused.refusal(&name)); },
                Err(e) => { rejected.insert(name, e.to_string()); },
            }
        }
        for (name, _) in self.agent_skills() {
            if !self.harness().join("pkgs").join(&name).is_dir() && let Ok(Approval::Approved(snap)) = self.approval(&lock, &name) {
                out.push(snap);
            }
        }
        Ok((out, rejected))
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

    /// `hx skills`: every agent skill and whether the model can see it.
    pub fn skills_status(&self) -> String {
        let mut s = String::new();
        let lock = read_toml::<Lockfile>(&self.root.join("hx.lock"));
        for (name, dir) in self.agent_skills() {
            let state = if self.harness().join("pkgs").join(&name).is_dir() {
                "shadowed by .harness/pkgs".to_string()
            } else {
                match lock.as_ref().map_err(|e| anyhow!("hx.lock: {e}")).and_then(|lock| self.approval(lock, &name)) {
                    Ok(Approval::Approved(_)) => "approved".into(),
                    Ok(Approval::NotLocked) => "not approved".into(),
                    Ok(Approval::Outdated) => "re-approve (old hash)".into(),
                    Ok(Approval::Changed { .. }) => "changed since approval".into(),
                    Err(e) => format!("error: {}", e.to_string().lines().next().unwrap_or("")),
                }
            };
            s.push_str(&format!("{name:<28} {state:<24} {}\n", dir.display()));
        }
        if !crate::trust::is_trusted(&self.root) {
            s.insert_str(0, "workspace config not trusted: no skill is visible until you run `hx trust`\n");
        }
        if s.is_empty() { "no skills in .agents/skills or ~/.agents/skills".into() } else { s }
    }

    fn read_manifest(pkg: &str, dir: &Path) -> Result<Manifest> {
        toml::from_str(&std::fs::read_to_string(dir.join("hx.toml")).map_err(|_| anyhow!("package {pkg}: missing hx.toml"))?)
            .map_err(|e| anyhow!("package {pkg}: bad hx.toml: {e}"))
    }

    /// Is package `name` approved as it is now? The error says why not.
    #[cfg(test)]
    fn verify(&self, name: &str) -> Result<()> {
        match self.approval(&read_toml(&self.root.join("hx.lock"))?, name)? {
            Approval::Approved(_) => Ok(()),
            refused => bail!("{}", refused.refusal(name)),
        }
    }

    /// The name `hx test DIR` mounts a package under: `[package] name` in its hx.toml (parsed, not pattern-matched),
    /// else the directory's name; None without an hx.toml (nothing to mount).
    pub fn test_name(dir: &Path) -> Result<Option<String>> {
        let dirname = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if !dir.join("hx.toml").is_file() {
            return Ok(None);
        }
        Ok(Some(Self::read_manifest(&dirname, dir)?.package.name.unwrap_or(dirname)))
    }

    /// Every installed package that is locked and unchanged: what the model may use. Others are skipped.
    pub fn list_packages(&self) -> Vec<PkgInfo> {
        self.approved().map(|(snaps, _)| snaps.iter().map(Self::info).collect()).unwrap_or_default()
    }

    fn info(snap: &Snapshot) -> PkgInfo {
        let man = &snap.man;
        let kind = man.package.kind.clone().unwrap_or_default();
        let skill = kind == "skill";
        PkgInfo {
            name: snap.name.clone(),
            version: man.package.version.clone(),
            description: man.package.description.clone().unwrap_or_default(),
            signature: man.tool.signature.clone().unwrap_or_default(),
            doc: man.tool.doc.clone().unwrap_or_default(),
            dir: if skill { std::fs::canonicalize(&snap.dir).unwrap_or(snap.dir.clone()).display().to_string() } else { String::new() },
            files: if skill { snap.files.keys().map(|p| p.to_string_lossy().to_string()).collect() } else { vec![] },
            kind,
        }
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
        let dir = self.staged_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap())?;
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
        let base = crate::eventlog::sha_bytes(self.effective_source(module)?.as_bytes());
        let dir = self.staged_dir().join(module);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("new.luau"), source)?;
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&serde_json::json!({"reason": reason, "base_sha": base}))?)?;
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
        let valid = if name.contains('/') { Self::module_file(name).is_ok() } else { !name.is_empty() && !name.starts_with('.') && name != "std" };
        let dir = self.staged_dir().join(name);
        if !valid || !dir.is_dir() {
            bail!("nothing staged as {name:?} (see `hx staged`)");
        }
        Ok(dir)
    }

    fn is_module(name: &str) -> bool {
        name.contains('/')
    }

    /// What the user reviews before accepting, with the exact bytes it shows: every file of a package (text in full,
    /// anything else by size), or a module replacement's reason, structural check and diff against the module in force.
    pub fn review(&self, name: &str) -> Result<Reviewed> {
        let dir = self.staged(name)?;
        let files = frozen_files(&dir)?;
        let hash = files_hash(&files);
        let text = |file: &str| -> Result<String> {
            String::from_utf8(files.get(Path::new(file)).ok_or_else(|| anyhow!("staged {name} has no {file}"))?.clone()).map_err(|_| anyhow!("staged {name}: {file} is not UTF-8 text"))
        };
        if Self::is_module(name) {
            let meta: serde_json::Value = serde_json::from_str(&text("meta.json")?)?;
            let new = text("new.luau")?;
            let check = self.check_module(name, &new).unwrap_or_else(|e| format!("FAILS: {e}"));
            // diff the reviewed bytes (stdin) against the module in force (a private temporary copy)
            let current = tempfile_with(&self.effective_source(name)?)?;
            let mut diff = std::process::Command::new("diff").args(["-u", "--label", "current", "--label", "staged"]).arg(current.path()).arg("-")
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
            std::io::Write::write_all(&mut diff.stdin.take().unwrap(), new.as_bytes())?;
            let diff = diff.wait_with_output()?;
            let text = format!("== staged module {name} ==\nreason: {}\ncheck: {check}\n\n{}", printable(meta["reason"].as_str().unwrap_or("")), printable(&String::from_utf8_lossy(&diff.stdout)));
            return Ok(Reviewed { name: name.into(), text, files, hash });
        }
        let mut out = format!("== staged package {name}: {} files, review {}… ==\n", files.len(), short(&hash));
        for (rel, bytes) in &files {
            out.push_str(&format!("  {} ({} bytes)\n", printable(&rel.to_string_lossy()), bytes.len()));
        }
        for (rel, bytes) in &files {
            let rel = printable(&rel.to_string_lossy());
            match std::str::from_utf8(bytes) {
                Ok(t) => out.push_str(&format!("\n--- {rel} ---\n{}{}", printable(t), if t.ends_with('\n') { "" } else { "\n" })),
                Err(_) => out.push_str(&format!("\n--- {rel} --- ({} bytes, not text; never loaded as Luau)\n", bytes.len())),
            }
        }
        Ok(Reviewed { name: name.into(), text: out, files, hash })
    }

    /// `hx accept NAME`: activate what `review` showed. A package is installed into pkgs/ from the reviewed bytes and
    /// locked, granting the capabilities its reviewed manifest requests; a module replacement becomes the override.
    /// The staging area is writable by the agent's shell, so if it no longer holds exactly what was shown, nothing is
    /// activated. Only the user's CLI reaches this.
    pub fn accept(&self, reviewed: &Reviewed) -> Result<String> {
        let name = reviewed.name.as_str();
        let from = self.staged(name)?;
        if frozen_files(&from).map(|f| files_hash(&f)).ok().as_deref() != Some(reviewed.hash.as_str()) {
            bail!("staged {name} changed after it was shown for review; nothing was activated. Run `hx accept {name}` again to review what is staged now");
        }
        if Self::is_module(name) {
            let utf8 = |file: &str| reviewed.files.get(Path::new(file)).and_then(|b| String::from_utf8(b.clone()).ok()).ok_or_else(|| anyhow!("staged {name} has no {file}"));
            return self.accept_module(name, &from, &serde_json::from_str(&utf8("meta.json")?)?, &utf8("new.luau")?);
        }
        let man = Self::manifest(name, false, &reviewed.files)?;
        let to = self.harness().join("pkgs").join(name);
        let _ = std::fs::remove_dir_all(&to);
        for (rel, bytes) in &reviewed.files {
            let path = to.join(rel);
            std::fs::create_dir_all(path.parent().unwrap())?;
            std::fs::write(path, bytes)?;
        }
        self.lock(name, &man.package.version, &reviewed.hash)?;
        std::fs::remove_dir_all(&from)?;
        Ok(format!("approved {name} {} ({}…) from {} with capabilities {:?}", man.package.version, short(&reviewed.hash), to.display(), man.capabilities))
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
        Ok(std::fs::read_to_string(self.override_path(module)?).unwrap_or_else(|_| Self::embedded(&pkg, &file_for(&m)).unwrap().to_string()))
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

    fn accept_module(&self, module: &str, dir: &Path, meta: &serde_json::Value, new: &str) -> Result<String> {
        // The replacement covers the whole module and was written against the module in force when it was staged: if
        // that module has changed since (another accept, a rollback), accepting would silently drop the change.
        let current = crate::eventlog::sha_bytes(self.effective_source(module)?.as_bytes());
        if Some(current.as_str()) != meta["base_sha"].as_str() {
            bail!("{module} has changed since this replacement was staged; stage it again against the current module");
        }
        self.check_module(module, new)?;
        let target = self.override_path(module)?;
        let vdir = self.versions_dir(module);
        std::fs::create_dir_all(&vdir)?;
        // Each accept saves what it replaces: NNNN.luau holds the previous override, NNNN.none records that there was none.
        let n = std::fs::read_dir(&vdir).map(|rd| rd.count()).unwrap_or(0) + 1;
        match std::fs::read_to_string(&target) {
            Ok(previous) => std::fs::write(vdir.join(format!("{n:04}.luau")), previous)?,
            Err(_) => std::fs::write(vdir.join(format!("{n:04}.none")), "")?,
        }
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(&target, new)?;
        std::fs::remove_dir_all(dir)?;
        self.log_upgrade(serde_json::json!({"action": "accept", "module": module, "reason": meta["reason"], "new_sha": crate::eventlog::sha_bytes(new.as_bytes()), "version": n}))?;
        Ok(format!("activated {module} (version {n}); undo with `hx rollback {module}`"))
    }

    /// `hx rollback MODULE`: restore what the last accepted replacement of MODULE replaced.
    pub fn rollback(&self, module: &str) -> Result<String> {
        let target = self.override_path(module)?;
        let vdir = self.versions_dir(module);
        let mut files: Vec<_> = std::fs::read_dir(&vdir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        let Some(last) = files.pop() else { bail!("nothing to roll back for {module}") };
        let previous = if last.extension().is_some_and(|e| e == "none") { None } else { Some(std::fs::read_to_string(&last)?) };
        // Older versions marked "no override" with this text inside a .luau file.
        match previous.filter(|p| p != "<<none>>") {
            Some(previous) => std::fs::write(&target, previous)?,
            None => { let _ = std::fs::remove_file(&target); },
        }
        std::fs::remove_file(&last)?;
        self.log_upgrade(serde_json::json!({"action": "rollback", "module": module}))?;
        Ok(format!("rolled back {module} to {}", if files.is_empty() { "the built-in module" } else { "the previous version" }))
    }

    /// `hx approve <pkg>`: lock the package's exact content, which grants exactly the capabilities its manifest requests.
    /// The manifest shown is the one verification and capture use (for an agent skill: never its hx.toml).
    pub fn approve(&self, pkg: &str) -> Result<String> {
        let snap = self.snapshot(pkg)?;
        self.lock(pkg, &snap.man.package.version, &snap.hash)?;
        let inert = snap.agent_skill && snap.files.keys().any(|p| p == Path::new("hx.toml") || p.extension().is_some_and(|e| e == "luau"));
        Ok(format!("approved {pkg} {} ({}…) from {} with capabilities {:?}{}", snap.man.package.version, short(&snap.hash), snap.dir.display(), snap.man.capabilities,
            if inert { "\nnote: agent skills carry no Luau or capabilities: its hx.toml and .luau files are plain files, never loaded (move it to .harness/pkgs to make it a package)" } else { "" }))
    }

    /// Record `name` at `version` with content `hash` in hx.lock, replacing its previous entry.
    fn lock(&self, name: &str, version: &str, hash: &str) -> Result<()> {
        let lock_path = self.root.join("hx.lock");
        let mut entries = read_toml::<Lockfile>(&lock_path)?.package;
        entries.retain(|e| e.name != name);
        entries.push(LockEntry { name: name.into(), version: version.into(), hash: hash.into() });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        std::fs::write(&lock_path, toml::to_string(&Lockfile { package: entries })?)?;
        Ok(())
    }
}

/// What `hx accept` showed for a staged NAME and the exact bytes behind it (see `Resolver::review`).
pub struct Reviewed {
    pub name: String,
    pub text: String,
    files: Files,
    hash: String,
}

/// A private temporary file holding `text`, removed on drop.
struct TempFile(PathBuf);
impl TempFile { fn path(&self) -> &Path { &self.0 } }
impl Drop for TempFile { fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); } }
fn tempfile_with(text: &str) -> Result<TempFile> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let nonce: u64 = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos() as u64;
    let path = std::env::temp_dir().join(format!("hx-review-{}-{nonce:x}.luau", std::process::id()));
    std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?.write_all(text.as_bytes())?;
    Ok(TempFile(path))
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
        dir
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

    /// Review, then accept what was shown (the `hx accept` sequence without the prompt).
    fn acc(r: &Resolver, name: &str) -> Result<String> {
        r.accept(&r.review(name)?)
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
        assert!(err(r.verify("foo")).contains("not locked"));
        // Approval records the exact content, manifest (and so capabilities) included.
        let msg = r.approve("foo").unwrap();
        assert!(msg.starts_with("approved foo 1.0"), "{msg}");
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        assert!(lock.contains("name = \"foo\"") && lock.contains("version = \"1.0\""), "{lock}");
        assert!(!lock.contains("grants"), "{lock}");
        assert!(r.verify("foo").is_ok());
        // any content change invalidates the lock
        std::fs::write(dir.join("init.luau"), "return { x = 2 }\n").unwrap();
        assert!(err(r.verify("foo")).contains("changed since it was approved"));
        // so does a version that differs from the locked one
        std::fs::write(dir.join("hx.toml"), "[package]\nversion = \"2.0\"\n\n[capabilities]\nfs = [\"read\"]\n").unwrap();
        // lock the new content under the old version: only the version differs
        std::fs::write(root.join("hx.lock"), format!("[[package]]\nname = \"foo\"\nversion = \"1.0\"\nhash = {:?}\n", files_hash(&frozen_files(&dir).unwrap()))).unwrap();
        assert!(err(Resolver::new(root.clone()).verify("foo")).contains("changed since it was approved"));
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
        assert!(r.verify("foo").is_ok());
        // Requesting more is a manifest edit, so it changes the hash and needs a new approval.
        std::fs::write(dir.join("hx.toml"), "[package]\nversion = \"1.0\"\n\n[capabilities]\nfs = [\"read\", \"write\"]\n").unwrap();
        assert!(err(r.verify("foo")).contains("changed since it was approved"));
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
        install_pkg(&root, "std");
        assert!(err(r.approve("std")).contains("built-in package"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn approve_writes_valid_toml_for_any_name() {
        let root = ws("dotted");
        install_pkg(&root, "my.pkg");
        install_pkg(&root, "other");
        let r = Resolver::new(root.clone());
        r.approve("my.pkg").unwrap();
        r.approve("other").unwrap();
        assert!(r.verify("my.pkg").is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frozen_registry_resolves_overrides_packages_and_builtins() {
        let root = ws("resolve");
        unsafe { std::env::set_var("HX_TRUST", "1") };
        let r = Resolver::new(root.clone());
        let registry = Registry::capture(&root).unwrap();
        let source = registry.resolve("std/printer").unwrap();
        assert_eq!((source.origin.as_str(), source.is_override, source.pkg.is_none()), ("std/printer.luau", false, true));
        assert_eq!(registry.resolve("std").unwrap().origin, "std/init.luau");
        assert!(err(registry.resolve("nope/x")).contains("module not found: nope/x"));
        assert!(err(registry.resolve("project/init")).contains("module not found"));
        write(root.join(".harness/init.luau"), "-- init\n");
        write(root.join(".harness/override/std/printer.luau"), "return {}\n");
        install_pkg(&root, "foo");
        assert!(err(Registry::capture(&root).unwrap().resolve("foo")).contains("not locked"));
        r.approve("foo").unwrap();
        let registry = Registry::capture(&root).unwrap();
        assert_eq!(registry.resolve("project/init").unwrap().text, "-- init\n");
        let source = registry.resolve("std/printer").unwrap();
        assert!(source.is_override && source.text == "return {}\n");
        assert!(!registry.resolve("base:std/printer").unwrap().is_override);
        assert_eq!(registry.resolve("foo").unwrap().pkg.unwrap().1, crate::supervisor::Capabilities::from([crate::supervisor::Capability::FsRead]));
        assert!(err(registry.resolve("foo/missing")).contains("module not found"));
        write(root.join(".harness/override/std/printer.luau"), "return {changed=true}\n");
        assert_eq!(registry.resolve("std/printer").unwrap().text, "return {}\n");
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
        let review = r.review("foo").unwrap().text;
        assert!(review.contains("--- hx.toml ---") && review.contains("--- tests/a.luau ---\n-- t"), "{review}");
        assert!(err(r.review("nope")).contains("nothing staged"));
        assert!(acc(&r, "foo").unwrap().starts_with("approved foo 1.0"));
        // a malformed hx.lock approves nothing but does not fail capture (runs without packages still start)
        unsafe { std::env::set_var("HX_TRUST", "1") };
        std::fs::write(root.join("hx.lock"), "lock\n").unwrap();
        assert!(err(Registry::capture(&root).unwrap().resolve("foo")).contains("hx.lock is not valid TOML"));
        std::fs::remove_file(root.join("hx.lock")).unwrap();
        assert!(root.join(".harness/pkgs/foo/init.luau").is_file());
        assert_eq!(r.list_staged(), vec!["bar"]);
        // re-staging and accepting replaces the installed copy
        r.stage("foo", &files("1.1")).unwrap();
        assert!(acc(&r, "foo").unwrap().starts_with("approved foo 1.1"));
        assert!(err(acc(&r, "foo")).contains("nothing staged"));
        r.reject("bar").unwrap();
        assert!(r.list_staged().is_empty());
        assert!(r.reject("bar").is_err());
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
        let root = ws("names");
        let r = Resolver::new(root.clone());
        write(root.join(".harness/pkgs/real/hx.toml"), "[package]\nversion = \"1\"\n");
        for bad in ["", ".", "..", "../pkgs/real", "std", "std/../../pkgs/real", "a/b"] {
            assert!(err(r.reject(bad)).contains("nothing staged"), "{bad}");
        }
        assert!(root.join(".harness/pkgs/real/hx.toml").is_file());
        assert!(err(r.stage("std", &[])).contains("built-in package"));
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
            let review = r.review("std/printer").unwrap().text;
            let accepted = acc(&r, "std/printer");
            if accepted.is_ok() { r.rollback("std/printer").unwrap(); }
            (review, accepted)
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
    fn module_accept_refuses_a_replacement_whose_base_is_no_longer_in_force() {
        let root = ws("stale");
        let r = Resolver::new(root.clone());
        let builtin = r.effective_source("std/printer").unwrap();
        r.stage_module("std/printer", &format!("{builtin}\n-- A\n"), "").unwrap();
        acc(&r, "std/printer").unwrap();
        // staged against A, then A is rolled back: accepting now would silently drop the rollback
        r.stage_module("std/printer", &format!("{builtin}\n-- A\n-- B\n"), "").unwrap();
        r.rollback("std/printer").unwrap();
        assert!(err(acc(&r, "std/printer")).contains("changed since this replacement was staged"));
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
        assert!(acc(&r, "std/printer").unwrap().contains("version 1"));
        assert_eq!(r.effective_source("std/printer").unwrap(), new);
        assert_eq!(r.list_staged(), vec!["tool"]);
        // a second replacement stacks on the first
        let newer = format!("{new}-- v3\n");
        r.stage_module("std/printer", &newer, "").unwrap();
        assert!(acc(&r, "std/printer").unwrap().contains("version 2"));
        assert_eq!(r.effective_source("std/printer").unwrap(), newer);
        // rejecting a staged replacement leaves the module in force alone
        r.stage_module("std/printer", "return {}", "").unwrap();
        r.reject("std/printer").unwrap();
        assert!(err(acc(&r, "std/printer")).contains("nothing staged"));
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
    fn a_blob_cannot_be_split_into_other_files_under_the_same_approval() {
        let root = ws("split");
        let skill = root.join(".agents/skills/s");
        write(skill.join("SKILL.md"), "---\ndescription: s\n---\nx\n");
        // With unframed `path\0content\0` hashing, this one file hashed exactly like the three files below.
        let manifest = "[package]\nversion = \"agent-skill\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n";
        write(skill.join("assets/a.bin"), &format!("x\0hx.toml\0{manifest}\0init.luau\0return {{}}"));
        let r = Resolver::new(root.clone());
        r.approve("s").unwrap();
        assert!(r.verify("s").is_ok());
        write(skill.join("assets/a.bin"), "x");
        write(skill.join("hx.toml"), manifest);
        write(skill.join("init.luau"), "return {}");
        assert!(err(r.verify("s")).contains("changed since it was approved"));
        assert!(r.skills_status().contains("changed since approval"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_lock_entry_with_the_old_hash_asks_for_reapproval() {
        let root = ws("legacy");
        install_pkg(&root, "foo");
        write(root.join(".agents/skills/s/SKILL.md"), "---\ndescription: s\n---\nx\n");
        let old = "0".repeat(64);
        write(root.join("hx.lock"), &format!("[[package]]\nname = \"foo\"\nversion = \"1.0\"\nhash = \"{old}\"\n\n[[package]]\nname = \"s\"\nversion = \"agent-skill\"\nhash = \"{old}\"\n"));
        let r = Resolver::new(root.clone());
        assert!(err(r.verify("foo")).contains("approved by an older hx") && err(r.verify("foo")).contains("hx approve foo"));
        assert!(r.skills_status().contains("re-approve (old hash)"), "{}", r.skills_status());
        r.approve("foo").unwrap();
        assert!(r.verify("foo").is_ok());
        assert!(std::fs::read_to_string(root.join("hx.lock")).unwrap().contains("hash = \"v2:"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_agent_skill_never_takes_a_manifest_or_luau_from_its_files() {
        let root = ws("skillman");
        unsafe { std::env::set_var("HX_TRUST", "1") };
        let skill = root.join(".agents/skills/s");
        write(skill.join("SKILL.md"), "---\ndescription: helpful\n---\nx\n");
        write(skill.join("hx.toml"), "[package]\nversion = \"9\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n");
        write(skill.join("init.luau"), "return { run = function() return sh('id') end }\n");
        let r = Resolver::new(root.clone());
        // approve shows what verification and capture use: kind skill, no capabilities
        let msg = r.approve("s").unwrap();
        assert!(msg.starts_with("approved s agent-skill") && msg.contains("Capabilities({})") && msg.contains("never loaded"), "{msg}");
        assert!(r.verify("s").is_ok());
        let registry = Registry::capture(&root).unwrap();
        let info = registry.packages.iter().find(|p| p.name == "s").unwrap();
        assert_eq!((info.kind.as_str(), info.description.as_str()), ("skill", "helpful"));
        assert!(registry.capabilities["s"].0.is_empty());
        assert!(err(registry.resolve("s")).contains("module not found"));
        assert!(registry.assets.contains_key("s/init.luau"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn accept_shows_every_file_and_installs_only_the_reviewed_bytes() {
        let root = ws("review");
        let r = Resolver::new(root.clone());
        r.stage("t", &[("hx.toml".into(), "[package]\nversion = \"1\"\nkind = \"tool\"\n[capabilities]\nproc = [\"sh\"]\n".into()),
            ("init.luau".into(), "return require('t/helper')\n".into()), ("lib/helper.luau".into(), "return { go = function() return sh('evil') end }\n".into()),
            ("notes.txt".into(), "hidden\u{1b}[2Kline\rcover\n".into())]).unwrap();
        let reviewed = r.review("t").unwrap();
        assert!(reviewed.text.contains("--- lib/helper.luau ---\nreturn { go = function() return sh('evil') end }"), "{}", reviewed.text);
        // terminal control sequences are shown, not obeyed
        assert!(reviewed.text.contains("hidden\\u{1b}[2Kline\\u{d}cover"), "{}", reviewed.text);
        // the agent's shell rewrites the staging area between the printout and the answer: nothing is activated
        write(root.join(".harness/staged/t/lib/helper.luau"), "return { go = function() return sh('worse') end }\n");
        assert!(err(r.accept(&reviewed)).contains("changed after it was shown for review"));
        assert!(!root.join(".harness/pkgs/t").exists());
        let reviewed = r.review("t").unwrap();
        assert!(r.accept(&reviewed).unwrap().starts_with("approved t 1"));
        assert_eq!(std::fs::read_to_string(root.join(".harness/pkgs/t/lib/helper.luau")).unwrap(), "return { go = function() return sh('worse') end }\n");
        assert!(r.verify("t").is_ok());
        // a module replacement is bound the same way
        let builtin = r.effective_source("std/printer").unwrap();
        r.stage_module("std/printer", &format!("{builtin}\n-- shown\n"), "").unwrap();
        let reviewed = r.review("std/printer").unwrap();
        write(root.join(".harness/staged/std/printer/new.luau"), &format!("{builtin}\n-- swapped\n"));
        assert!(err(r.accept(&reviewed)).contains("changed after it was shown for review"));
        assert_eq!(r.effective_source("std/printer").unwrap(), builtin);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_override_beats_a_package_and_non_text_override_files_are_kept_as_bytes() {
        let root = ws("precedence");
        unsafe { std::env::set_var("HX_TRUST", "1") };
        install_pkg(&root, "foo");
        Resolver::new(root.clone()).approve("foo").unwrap();
        write(root.join(".harness/override/foo/init.luau"), "return { owner = true }\n");
        std::fs::create_dir_all(root.join(".harness/override/std")).unwrap();
        std::fs::write(root.join(".harness/override/std/logo.png"), [0x89, 0xff, 0xfe]).unwrap();
        let registry = Registry::capture(&root).unwrap();
        let foo = registry.resolve("foo").unwrap();
        assert!(foo.is_override && foo.text.contains("owner"));
        assert_eq!(registry.assets["std/logo.png"], vec![0x89, 0xff, 0xfe]);
        std::fs::write(root.join(".harness/override/std/printer.luau"), [0xff]).unwrap();
        assert!(err(Registry::capture(&root)).contains(".harness/override/std/printer.luau is not UTF-8"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_name_reads_the_manifest() {
        let root = ws("testname");
        write(root.join("p/hx.toml"), "[package]\n name   =   \"real\"\nversion = \"1\"\n");
        assert_eq!(Resolver::test_name(&root.join("p")).unwrap().as_deref(), Some("real"));
        write(root.join("q/hx.toml"), "[package]\nversion = \"1\"\n");
        assert_eq!(Resolver::test_name(&root.join("q")).unwrap().as_deref(), Some("q"));
        assert_eq!(Resolver::test_name(&root.join("none")).unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
