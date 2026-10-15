//! Module resolution, package manifests, the lockfile (approvals and grants) and staging.
//!
//! `require("pkg")` loads `init.luau`; `require("pkg/mod")` loads `mod.luau`. Lookup order:
//!   1. `<ws>/.harness/override/<pkg>/<mod>.luau`   user override of any module (e.g. the printer), no rebuild needed
//!   2. `<ws>/.harness/pkgs/<pkg>/<mod>.luau`       project package (needs a lockfile entry + granted capabilities)
//!   3. the built-in `std` package embedded in the binary
//!
//! Agent skills (the `.agents/skills` convention): `<ws>/.agents/skills/<name>/SKILL.md`, then `~/.agents/skills/<name>/SKILL.md`.
//! They need no hx.toml (the manifest comes from SKILL.md frontmatter), carry no Luau and no capabilities, and are locked
//! by `hx approve NAME` like any package. `.harness/pkgs/<name>` shadows them.
//!
//! Everything under (1), (2), `project/init` and the agent skills is used only when the workspace config is trusted
//! (see trust.rs); otherwise only the built-in package resolves.
//!
//! `require("base:std/printer")` skips (1), so an override can wrap the module it replaces.
//! `require("project/init")` is `<ws>/.harness/init.luau`, run before the loop starts (hook registration etc.).
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Packages built into the binary: (package, file, text). Trusted like the binary itself.
pub const EMBEDDED: &[(&str, &str, &str)] = &[
    ("std", "hx.toml", include_str!("../stdlib/std/hx.toml")),
    ("std", "init.luau", include_str!("../stdlib/std/init.luau")),
    ("std", "loop.luau", include_str!("../stdlib/std/loop.luau")),
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

#[derive(Debug, Deserialize, Default)]
pub struct Manifest {
    pub package: PackageInfo,
    #[serde(default)]
    pub capabilities: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub tool: ToolInfo,
}

#[derive(Debug, Deserialize, Default)]
pub struct PackageInfo {
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

/// `hx.lock`: one record per approved package: the exact content approved and the capabilities granted to it.
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
    /// capability -> granted scopes (`"*"` grants every scope)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    grants: BTreeMap<String, Vec<String>>,
}

impl LockEntry {
    /// The first requested capability scope this entry does not grant.
    fn ungranted<'a>(&self, requested: &'a BTreeMap<String, Vec<String>>) -> Option<(&'a str, &'a str)> {
        requested.iter().flat_map(|(cap, scopes)| scopes.iter().map(move |s| (cap.as_str(), s.as_str())))
            .find(|(cap, s)| !self.grants.get(*cap).is_some_and(|g| g.iter().any(|x| x == s || x == "*")))
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
    pub pkg: Option<(String, BTreeMap<String, Vec<String>>)>,
}

/// An immutable, approved byte snapshot. Runtime resolution never goes back to mutable workspace files.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Registry {
    pub sources: BTreeMap<String, Source>,
    pub assets: BTreeMap<String, Vec<u8>>,
    pub packages: Vec<PkgInfo>,
    pub capabilities: BTreeMap<String, BTreeMap<String, Vec<String>>>,
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
            let overrides = root.join(".harness/override");
            if overrides.is_dir() {
                for (relative, bytes) in frozen_files(&overrides)? {
                    let key = relative.to_string_lossy().replace('\\', "/");
                    let text = String::from_utf8(bytes.clone())?;
                    registry.assets.insert(key.clone(), bytes);
                    if let Some(name) = key.strip_suffix(".luau") {
                        let name = name.strip_suffix("/init").unwrap_or(name).to_string();
                        registry.sources.insert(name, Source { text, origin: key, is_override: true, pkg: None });
                    }
                }
            }
            let init = root.join(".harness/init.luau");
            if init.is_file() {
                anyhow::ensure!(!std::fs::symlink_metadata(&init)?.file_type().is_symlink(), "project init must not be a symlink");
                registry.sources.insert("project/init".into(), Source { text: std::fs::read_to_string(init)?, origin: "project/init".into(), is_override: false, pkg: None });
            }
            for entry in std::fs::read_dir(root.join(".harness/pkgs")).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if let Ok(man) = Resolver::read_manifest(&name, &entry.path()) {
                    if let Err(error) = resolver.verify(&name, &entry.path(), &man) { registry.rejected.insert(name, error.to_string()); }
                }
            }
            for info in resolver.list_packages() {
                let lock: Lockfile = read_toml(&root.join("hx.lock"))?;
                let (dir, _) = resolver.locate(&info.name)?;
                let files = match frozen_files(&dir) {
                    Ok(files) => files,
                    Err(error) => { registry.rejected.insert(info.name.clone(), error.to_string()); continue; },
                };
                // Capabilities and metadata must come from the same bytes whose approval hash we validate.
                let man: Manifest = match files.get(Path::new("hx.toml")) {
                    Some(bytes) => toml::from_str(std::str::from_utf8(bytes)?)?,
                    None => {
                        let text = files.get(Path::new("SKILL.md")).context("skill has no frozen SKILL.md")?;
                        let mut man = Manifest::default();
                        man.package.version = "agent-skill".into(); man.package.kind = Some("skill".into());
                        man.package.description = Some(frontmatter_description(std::str::from_utf8(text)?)); man
                    },
                };
                // Hash the bytes actually captured, not a second filesystem read.
                let mut hash = Sha256::new();
                for (relative, bytes) in &files {
                    hash.update(relative.to_string_lossy().as_bytes()); hash.update([0]);
                    hash.update(bytes); hash.update([0]);
                }
                let digest: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
                let entry = lock.package.iter().find(|p| p.name == info.name && p.hash == digest && p.version == man.package.version)
                    .with_context(|| format!("package {} changed while its approved snapshot was captured", info.name))?;
                if let Some((cap, scope)) = entry.ungranted(&man.capabilities) { bail!("package {} requests ungranted {cap}.{scope}", info.name); }
                registry.capabilities.insert(info.name.clone(), man.capabilities.clone());
                let mut info = info;
                info.version = man.package.version.clone(); info.kind = man.package.kind.clone().unwrap_or_default();
                info.description = man.package.description.clone().unwrap_or_default();
                info.signature = man.tool.signature.clone().unwrap_or_default(); info.doc = man.tool.doc.clone().unwrap_or_default();
                if info.kind == "skill" { info.files = files.keys().map(|p| p.to_string_lossy().to_string()).collect(); }
                for (relative, bytes) in files {
                    let file = relative.to_string_lossy().replace('\\', "/");
                    let key = format!("{}/{file}", info.name);
                    registry.assets.insert(key.clone(), bytes.clone());
                    if let Some(module) = file.strip_suffix(".luau") {
                        let name = if module == "init" { info.name.clone() } else { format!("{}/{module}", info.name) };
                        registry.sources.insert(name, Source { text: String::from_utf8(bytes)?, origin: key, is_override: false, pkg: Some((info.name.clone(), man.capabilities.clone())) });
                    }
                }
                registry.packages.push(info);
            }
            registry.mcp = crate::mcp::snapshot(root)?;
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

/// Hash of every file in a package directory (relative path + content, sorted), tests included.
pub fn hash_dir(dir: &Path) -> Result<String> {
    let mut files = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            // symlink_metadata: a symlinked directory is not walked (a link to / would hash the disk); see below
            if std::fs::symlink_metadata(&p)?.is_dir() {
                walk(&p, base, out)?;
            } else if p.file_name().is_some_and(|n| n != ".DS_Store") {
                out.push(p.strip_prefix(base).unwrap().to_path_buf());
            }
        }
        Ok(())
    }
    walk(dir, dir, &mut files)?;
    files.sort();
    let mut h = Sha256::new();
    for f in files {
        h.update(f.to_string_lossy().as_bytes());
        h.update([0]);
        let full = dir.join(&f);
        // a symlink contributes its target path, not the target's content (reads through it are refused anyway)
        match std::fs::read_link(&full) {
            Ok(t) => h.update(format!("symlink:{}", t.display()).as_bytes()),
            Err(_) => h.update(std::fs::read(&full)?),
        }
        h.update([0]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
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

/// Files under `dir`, relative and sorted (skips dotfiles).
fn list_files(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if p.is_dir() {
                walk(&p, base, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
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

    /// A skill without hx.toml: kind "skill", no capabilities, description from SKILL.md frontmatter.
    fn agent_skill_manifest(dir: &Path) -> Manifest {
        let text = std::fs::read_to_string(dir.join("SKILL.md")).unwrap_or_default();
        let mut m = Manifest::default();
        m.package.version = "agent-skill".into();
        m.package.kind = Some("skill".into());
        m.package.description = Some(frontmatter_description(&text));
        m
    }

    /// Where package `name` lives and its manifest: `.harness/pkgs/<name>`, else an agent skill.
    fn locate(&self, name: &str) -> Result<(PathBuf, Manifest)> {
        let pdir = self.harness().join("pkgs").join(name);
        if pdir.is_dir() {
            return Ok((pdir.clone(), Self::read_manifest(name, &pdir)?));
        }
        if let Some(d) = self.agent_skill_dir(name) {
            return Ok((d.clone(), Self::agent_skill_manifest(&d)));
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

    /// `hx skills`: every agent skill and whether the model can see it.
    pub fn skills_status(&self) -> String {
        let mut s = String::new();
        for (name, dir) in self.agent_skills() {
            let state = if self.harness().join("pkgs").join(&name).is_dir() {
                "shadowed by .harness/pkgs".to_string()
            } else {
                match self.verify(&name, &dir, &Self::agent_skill_manifest(&dir)) {
                    Ok(()) => "approved".into(),
                    Err(e) if e.to_string().contains("not locked") => "not approved".into(),
                    Err(e) if e.to_string().contains("changed since it was approved") => "changed since approval".into(),
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

    fn verify(&self, pkg: &str, dir: &Path, man: &Manifest) -> Result<()> {
        let lock: Lockfile = read_toml(&self.root.join("hx.lock"))?;
        let Some(entry) = lock.package.iter().find(|e| e.name == pkg) else {
            bail!("package {pkg} is not locked; review it, then run `hx approve {pkg}`")
        };
        let hash = hash_dir(dir)?; // only for locked packages: ~/.agents/skills can be large
        if entry.hash != hash || entry.version != man.package.version {
            bail!(
                "package {pkg} changed since it was approved (locked {}…, now {}…); review it and run `hx approve {pkg}` again",
                &entry.hash[..8.min(entry.hash.len())],
                &hash[..8]
            );
        }
        if let Some((cap, s)) = entry.ungranted(&man.capabilities) {
            bail!("package {pkg} requests capability {cap}={s} which has not been granted in hx.lock; run `hx approve {pkg}`");
        }
        Ok(())
    }

    /// Every installed package that is locked, unchanged and fully granted: what the model may use. Others are skipped.
    pub fn list_packages(&self) -> Vec<PkgInfo> {
        let mut out = Vec::new();
        if !crate::trust::is_trusted(&self.root) {
            return out; // hx.lock is untrusted, so nothing it approves is visible
        }
        let mut dirs: Vec<_> = std::fs::read_dir(self.harness().join("pkgs")).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        dirs.sort();
        for dir in dirs {
            let name = dir.file_name().unwrap().to_string_lossy().to_string();
            let Ok(man) = Self::read_manifest(&name, &dir) else { continue };
            if self.verify(&name, &dir, &man).is_err() {
                continue;
            }
            out.push(Self::info(name, &dir, &man));
        }
        for (name, dir) in self.agent_skills() {
            if out.iter().any(|p| p.name == name) || self.harness().join("pkgs").join(&name).is_dir() {
                continue;
            }
            let man = Self::agent_skill_manifest(&dir);
            if self.verify(&name, &dir, &man).is_ok() {
                out.push(Self::info(name, &dir, &man));
            }
        }
        out
    }

    fn info(name: String, dir: &Path, man: &Manifest) -> PkgInfo {
        let kind = man.package.kind.clone().unwrap_or_default();
        let skill = kind == "skill";
        PkgInfo {
            name,
            version: man.package.version.clone(),
            description: man.package.description.clone().unwrap_or_default(),
            signature: man.tool.signature.clone().unwrap_or_default(),
            doc: man.tool.doc.clone().unwrap_or_default(),
            dir: if skill { std::fs::canonicalize(dir).unwrap_or(dir.into()).display().to_string() } else { String::new() },
            files: if skill { list_files(dir) } else { vec![] },
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

    /// What the user reviews before accepting: a package's manifest, source and tests; or a module replacement's
    /// reason, structural check and diff against the module in force.
    pub fn review(&self, name: &str) -> Result<String> {
        let dir = self.staged(name)?;
        if Self::is_module(name) {
            let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)?;
            let new = std::fs::read_to_string(dir.join("new.luau"))?;
            let check = self.check_module(name, &new).unwrap_or_else(|e| format!("FAILS: {e}"));
            let current = self.effective_source(name)?;
            let mut diff = std::process::Command::new("diff").args(["-u", "--label", "current", "--label", "staged", "-"]).arg(dir.join("new.luau"))
                .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn()?;
            std::io::Write::write_all(&mut diff.stdin.take().unwrap(), current.as_bytes())?;
            let diff = diff.wait_with_output()?;
            return Ok(format!("== staged module {name} ==\nreason: {}\ncheck: {check}\n\n{}", meta["reason"].as_str().unwrap_or(""), String::from_utf8_lossy(&diff.stdout)));
        }
        let mut out = format!("== staged package {name} ==\n");
        for rel in ["hx.toml", "init.luau", "types.d.luau"] {
            if let Ok(t) = std::fs::read_to_string(dir.join(rel)) {
                out.push_str(&format!("\n--- {rel} ---\n{t}\n"));
            }
        }
        if let Ok(rd) = std::fs::read_dir(dir.join("tests")) {
            for e in rd.flatten() {
                out.push_str(&format!("\n--- tests/{} ---\n{}\n", e.file_name().to_string_lossy(), std::fs::read_to_string(e.path()).unwrap_or_default()));
            }
        }
        Ok(out)
    }

    /// `hx accept NAME`: activate what is staged as NAME. A package moves into pkgs/ and is locked and granted the
    /// capabilities it requests; a module replacement becomes the override. Only the user's CLI reaches this.
    pub fn accept(&self, name: &str) -> Result<String> {
        let from = self.staged(name)?;
        if Self::is_module(name) {
            return self.accept_module(name, &from);
        }
        let to = self.harness().join("pkgs").join(name);
        std::fs::create_dir_all(self.harness().join("pkgs"))?;
        let _ = std::fs::remove_dir_all(&to);
        std::fs::rename(&from, &to)?;
        self.approve(name)
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
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
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

    fn accept_module(&self, module: &str, dir: &Path) -> Result<String> {
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json"))?)?;
        let new = std::fs::read_to_string(dir.join("new.luau"))?;
        // The replacement covers the whole module and was written against the module in force when it was staged: if
        // that module has changed since (another accept, a rollback), accepting would silently drop the change.
        let current = crate::eventlog::sha_bytes(self.effective_source(module)?.as_bytes());
        if Some(current.as_str()) != meta["base_sha"].as_str() {
            bail!("{module} has changed since this replacement was staged; stage it again against the current module");
        }
        self.check_module(module, &new)?;
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
        std::fs::write(&target, &new)?;
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

    /// `hx approve <pkg>`: record the lock entry and grant exactly the capabilities the manifest requests.
    pub fn approve(&self, pkg: &str) -> Result<String> {
        let (dir, man) = self.locate(pkg)?;
        let hash = hash_dir(&dir)?;
        let lock_path = self.root.join("hx.lock");
        let mut entries = read_toml::<Lockfile>(&lock_path)?.package;
        entries.retain(|e| e.name != pkg);
        entries.push(LockEntry { name: pkg.into(), version: man.package.version.clone(), hash: hash.clone(), grants: man.capabilities.clone() });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        std::fs::write(&lock_path, toml::to_string(&Lockfile { package: entries })?)?;
        Ok(format!("approved {pkg} {} ({}…) from {} with capabilities {:?}", man.package.version, &hash[..8], dir.display(), man.capabilities))
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
    fn package_must_be_locked_unchanged_and_granted() {
        let root = ws("lock");
        let dir = install_pkg(&root, "foo");
        let r = Resolver::new(root.clone());
        assert!(err(r.verify("foo", &dir, &Resolver::read_manifest("foo", &dir).unwrap())).contains("not locked"));
        // Approval records the exact content and capabilities.
        let msg = r.approve("foo").unwrap();
        assert!(msg.starts_with("approved foo 1.0"), "{msg}");
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        assert!(lock.contains("name = \"foo\"") && lock.contains("version = \"1.0\""), "{lock}");
        assert!(lock.contains("[package.grants]\nfs = [\"read\"]"), "{lock}");
        assert!(r.verify("foo", &dir, &Resolver::read_manifest("foo", &dir).unwrap()).is_ok());
        // any content change invalidates the lock
        std::fs::write(dir.join("init.luau"), "return { x = 2 }\n").unwrap();
        assert!(err(r.verify("foo", &dir, &Resolver::read_manifest("foo", &dir).unwrap())).contains("changed since it was approved"));
        // so does a version that differs from the locked one
        std::fs::write(dir.join("hx.toml"), "[package]\nversion = \"2.0\"\n\n[capabilities]\nfs = [\"read\"]\n").unwrap();
        let man = Resolver::read_manifest("foo", &dir).unwrap();
        // lock the new content under the old version: only the version differs
        std::fs::write(root.join("hx.lock"), format!("[[package]]\nname = \"foo\"\nversion = \"1.0\"\nhash = {:?}\n", hash_dir(&dir).unwrap())).unwrap();
        assert!(err(Resolver::new(root.clone()).verify("foo", &dir, &man)).contains("changed since it was approved"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ungranted_capability_is_refused_and_wildcard_allowed() {
        let root = ws("grants");
        let dir = install_pkg(&root, "foo");
        let r = Resolver::new(root.clone());
        r.approve("foo").unwrap();
        let man = Resolver::read_manifest("foo", &dir).unwrap();
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        let with = |grants: &str| std::fs::write(root.join("hx.lock"), lock.replace("[package.grants]\nfs = [\"read\"]\n", grants)).unwrap();
        with("[package.grants]\nfs = [\"write\"]\n");
        assert!(err(r.verify("foo", &dir, &man)).contains("capability fs=read which has not been granted"));
        with("");
        assert!(err(r.verify("foo", &dir, &man)).contains("not been granted"));
        with("[package.grants]\nfs = [\"*\"]\n");
        assert!(r.verify("foo", &dir, &man).is_ok());
        let _ = std::fs::remove_dir_all(&root);
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
    fn approve_writes_valid_toml_for_any_name() {
        let root = ws("dotted");
        let dir = install_pkg(&root, "my.pkg");
        install_pkg(&root, "other");
        let r = Resolver::new(root.clone());
        r.approve("my.pkg").unwrap();
        r.approve("other").unwrap();
        let man = Resolver::read_manifest("my.pkg", &dir).unwrap();
        assert!(r.verify("my.pkg", &dir, &man).is_ok());
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
        assert_eq!(registry.resolve("foo").unwrap().pkg.unwrap().1["fs"], vec!["read"]);
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
        let review = r.review("foo").unwrap();
        assert!(review.contains("--- hx.toml ---") && review.contains("--- tests/a.luau ---\n-- t"), "{review}");
        assert!(err(r.review("nope")).contains("nothing staged"));
        assert!(r.accept("foo").unwrap().starts_with("approved foo 1.0"));
        assert!(root.join(".harness/pkgs/foo/init.luau").is_file());
        assert_eq!(r.list_staged(), vec!["bar"]);
        // re-staging and accepting replaces the installed copy
        r.stage("foo", &files("1.1")).unwrap();
        assert!(r.accept("foo").unwrap().starts_with("approved foo 1.1"));
        assert!(err(r.accept("foo")).contains("nothing staged"));
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
            let review = r.review("std/printer").unwrap();
            let accepted = r.accept("std/printer");
            if let Ok(_) = &accepted { r.rollback("std/printer").unwrap(); }
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
        r.accept("std/printer").unwrap();
        // staged against A, then A is rolled back: accepting now would silently drop the rollback
        r.stage_module("std/printer", &format!("{builtin}\n-- A\n-- B\n"), "").unwrap();
        r.rollback("std/printer").unwrap();
        assert!(err(r.accept("std/printer")).contains("changed since this replacement was staged"));
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
        assert!(r.review("std/printer").unwrap().contains("reason: test"));
        assert!(r.accept("std/printer").unwrap().contains("version 1"));
        assert_eq!(r.effective_source("std/printer").unwrap(), new);
        assert_eq!(r.list_staged(), vec!["tool"]);
        // a second replacement stacks on the first
        let newer = format!("{new}-- v3\n");
        r.stage_module("std/printer", &newer, "").unwrap();
        assert!(r.accept("std/printer").unwrap().contains("version 2"));
        assert_eq!(r.effective_source("std/printer").unwrap(), newer);
        // rejecting a staged replacement leaves the module in force alone
        r.stage_module("std/printer", "return {}", "").unwrap();
        r.reject("std/printer").unwrap();
        assert!(err(r.accept("std/printer")).contains("nothing staged"));
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
}
