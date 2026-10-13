//! Module resolution, package manifests, lockfile and grants.
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
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
    ("std", "checkpoint.luau", include_str!("../stdlib/std/checkpoint.luau")),
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
#[derive(Debug, Clone, serde::Serialize)]
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

#[derive(Debug, Deserialize, serde::Serialize, Default)]
struct Grants {
    #[serde(default)]
    grant: BTreeMap<String, BTreeMap<String, Vec<String>>>,
}

pub struct Resolver {
    pub root: PathBuf,
    checked: Mutex<HashSet<String>>,
}

pub struct Source {
    pub text: String,
    pub origin: String,
    /// true when this came from `.harness/override/` (the caller falls back to the built-in module if it is broken)
    pub is_override: bool,
    /// project packages: (name, capabilities) so the loader can run the code with only those primitives
    pub pkg: Option<(String, BTreeMap<String, Vec<String>>)>,
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
        Resolver { root, checked: Mutex::new(HashSet::new()) }
    }

    fn harness(&self) -> PathBuf {
        self.root.join(".harness")
    }

    pub fn embedded(pkg: &str, file: &str) -> Option<&'static str> {
        EMBEDDED.iter().find(|(p, n, _)| *p == pkg && *n == file).map(|(_, _, t)| *t)
    }

    pub fn resolve(&self, name: &str) -> Result<Source> {
        let trusted = crate::trust::is_trusted(&self.root);
        if name == "project/init" {
            if !trusted {
                bail!("no project init");
            }
            let p = self.harness().join("init.luau");
            return std::fs::read_to_string(&p)
                .map(|text| Source { text, origin: p.display().to_string(), is_override: false, pkg: None })
                .map_err(|_| anyhow!("no project init"));
        }
        // `base:std/printer` skips project overrides, so an override can wrap the module it replaces.
        let (skip_override, name) = match name.strip_prefix("base:") {
            Some(n) => (true, n),
            None => (false, name),
        };
        let (pkg, module) = split_name(name);
        let file = file_for(module);
        if let Some((tn, tdir)) = TEST_PKG.get()
            && pkg == tn && tdir.join(&file).is_file() {
                let man: Manifest = toml::from_str(&std::fs::read_to_string(tdir.join("hx.toml"))?)?;
                let p = tdir.join(&file);
                return Ok(Source { text: std::fs::read_to_string(&p)?, origin: p.display().to_string(), is_override: false, pkg: Some((pkg.to_string(), man.capabilities)) });
            }
        let over = self.harness().join("override").join(pkg).join(&file);
        if trusted && !skip_override && over.is_file() {
            return Ok(Source { text: std::fs::read_to_string(&over)?, origin: over.display().to_string(), is_override: true, pkg: None });
        }
        let pdir = self.harness().join("pkgs").join(pkg);
        if trusted && pdir.join(&file).is_file() {
            let man = self.check_package(pkg, &pdir)?;
            let p = pdir.join(&file);
            return Ok(Source { text: std::fs::read_to_string(&p)?, origin: p.display().to_string(), is_override: false, pkg: Some((pkg.to_string(), man.capabilities)) });
        }
        if let Some(t) = Self::embedded(pkg, &file) {
            return Ok(Source { text: t.to_string(), origin: format!("{pkg}:{file}"), is_override: false, pkg: None });
        }
        bail!("module not found: {name}")
    }

    /// Project packages must be locked (hash matches) and have every requested capability granted by the user.
    fn check_package(&self, pkg: &str, dir: &Path) -> Result<Manifest> {
        let man = Self::read_manifest(pkg, dir)?;
        // Mark a package as checked only once it has PASSED: a failed check must fail again on the next require.
        if !self.checked.lock().unwrap().contains(pkg) {
            self.verify(pkg, dir, &man)?;
            self.checked.lock().unwrap().insert(pkg.to_string());
        }
        Ok(man)
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
        let entry = lock.package.iter().find(|e| e.name == pkg);
        let hash = if entry.is_some() { hash_dir(dir)? } else { String::new() }; // unlocked: skip hashing (~/.agents/skills can be large)
        match entry {
            None => bail!("package {pkg} is not locked; review it, then run `hx approve {pkg}`"),
            Some(e) if e.hash != hash || e.version != man.package.version => bail!(
                "package {pkg} changed since it was approved (locked {}…, now {}…); review it and run `hx approve {pkg}` again",
                &e.hash[..8.min(e.hash.len())],
                &hash[..8]
            ),
            Some(_) => {}
        }
        let grants: Grants = read_toml(&self.harness().join("grants.toml"))?;
        let granted = grants.grant.get(pkg);
        for (cap, scopes) in &man.capabilities {
            for s in scopes {
                let ok = granted.and_then(|g| g.get(cap)).is_some_and(|g| g.iter().any(|x| x == s || x == "*"));
                if !ok {
                    bail!("package {pkg} requests capability {cap}={s} which has not been granted; run `hx approve {pkg}`");
                }
            }
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

    pub fn staged_dir(&self) -> PathBuf {
        self.harness().join("staged")
    }

    /// Write a package into the staging area (replacing a previous staging of the same name). Not active until accepted.
    pub fn stage(&self, name: &str, files: &[(String, String)]) -> Result<PathBuf> {
        let dir = self.staged_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        for (rel, text) in files {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap())?;
            std::fs::write(p, text)?;
        }
        Ok(dir)
    }

    pub fn list_staged(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.staged_dir()).map(|rd| rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    /// What a human reviews before accepting: manifest, capabilities, source, tests.
    pub fn review(&self, name: &str) -> Result<String> {
        let dir = self.staged_dir().join(name);
        if !dir.is_dir() {
            bail!("nothing staged as {name:?} (see `hx staged`)");
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

    /// `hx accept NAME`: activate a staged package: move it into pkgs/, lock its hash, grant the capabilities it requests.
    /// Only the user's CLI can reach this; no primitive exposes it.
    pub fn accept(&self, name: &str) -> Result<String> {
        let from = self.staged_dir().join(name);
        if !from.is_dir() {
            bail!("nothing staged as {name:?} (see `hx staged`)");
        }
        let to = self.harness().join("pkgs").join(name);
        std::fs::create_dir_all(self.harness().join("pkgs"))?;
        let _ = std::fs::remove_dir_all(&to);
        std::fs::rename(&from, &to)?;
        self.checked.lock().unwrap().remove(name);
        self.approve(name)
    }

    // ---------------------------------------------------------------------------------------------------------------
    // Harness-module upgrades (AC5): propose -> check -> gate (eval) -> accept (user) -> rollback.
    // A proposal is staged under .harness/proposals/<id>/ by the core; nothing is active until `hx upgrade accept`.
    // ---------------------------------------------------------------------------------------------------------------
    fn module_file(module: &str) -> Result<(String, String)> {
        let (pkg, m) = split_name(module);
        if pkg != "std" || m.is_empty() || !m.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            bail!("only built-in harness modules can be upgraded, e.g. std/printer (got {module:?})");
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

    pub fn proposals_dir(&self) -> PathBuf {
        self.harness().join("proposals")
    }

    pub fn propose(&self, module: &str, source: &str, reason: &str) -> Result<String> {
        Self::module_file(module)?;
        if source.trim().is_empty() || source.len() > 200_000 {
            bail!("a proposal needs 1 byte to 200 KB of Luau source");
        }
        let base = self.effective_source(module)?;
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let id = format!("{ts}-{}", &crate::eventlog::sha_bytes(source.as_bytes())[..8]);
        let dir = self.proposals_dir().join(&id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("base.luau"), &base)?;
        std::fs::write(dir.join("new.luau"), source)?;
        let meta = serde_json::json!({"id": id, "module": module, "reason": reason, "base_sha": crate::eventlog::sha_bytes(base.as_bytes()), "new_sha": crate::eventlog::sha_bytes(source.as_bytes()), "created": ts});
        std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
        Ok(id)
    }

    pub fn list_proposals(&self) -> Vec<serde_json::Value> {
        let mut v: Vec<serde_json::Value> = std::fs::read_dir(self.proposals_dir())
            .map(|rd| rd.flatten().filter_map(|e| std::fs::read_to_string(e.path().join("meta.json")).ok().and_then(|t| serde_json::from_str(&t).ok())).collect())
            .unwrap_or_default();
        v.sort_by_key(|m| m["created"].as_u64().unwrap_or(0));
        v
    }

    fn proposal_meta(&self, id: &str) -> Result<(PathBuf, serde_json::Value)> {
        let dir = self.proposals_dir().join(id);
        let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("meta.json")).map_err(|_| anyhow!("no proposal {id:?} (see `hx upgrade list`)"))?)?;
        Ok((dir, meta))
    }

    /// Structural check: the candidate loads, returns a table, and keeps every export of the module it replaces.
    pub fn check_proposal(&self, id: &str) -> Result<String> {
        let (dir, meta) = self.proposal_meta(id)?;
        let (base, new) = (std::fs::read_to_string(dir.join("base.luau"))?, std::fs::read_to_string(dir.join("new.luau"))?);
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
            let (proxy, module) = (proxy.clone(), meta["module"].as_str().unwrap_or("").to_string());
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
        let n = load("proposed module", &new, make_env(Some(b.clone()))?)?;
        let mut missing = vec![];
        for pair in b.pairs::<String, mlua::Value>() {
            let (k, _) = pair?;
            if matches!(n.get::<mlua::Value>(k.as_str())?, mlua::Value::Nil) {
                missing.push(k);
            }
        }
        if !missing.is_empty() {
            missing.sort();
            bail!("the proposed {} drops exports that other modules use: {}", meta["module"].as_str().unwrap_or("module"), missing.join(", "));
        }
        Ok("loads, returns a table, keeps every export".into())
    }

    pub fn show_proposal(&self, id: &str) -> Result<String> {
        let (dir, meta) = self.proposal_meta(id)?;
        let diff = std::process::Command::new("diff").args(["-u", "--label", "current", "--label", "proposed"]).arg(dir.join("base.luau")).arg(dir.join("new.luau")).output()?;
        let gate = std::fs::read_to_string(dir.join("gate.json")).unwrap_or_else(|_| "(gate not run: `python3 eval/gate.py ...`)".into());
        Ok(format!("proposal {id} for {}\nreason: {}\n\n{}\n\ngate: {gate}", meta["module"].as_str().unwrap_or("?"), meta["reason"].as_str().unwrap_or(""), String::from_utf8_lossy(&diff.stdout)))
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

    /// Activate a proposal. Requires a passing gate report for THIS exact candidate, unless forced.
    pub fn accept_upgrade(&self, id: &str, force: bool) -> Result<String> {
        let (dir, meta) = self.proposal_meta(id)?;
        let module = meta["module"].as_str().unwrap_or("").to_string();
        let new = std::fs::read_to_string(dir.join("new.luau"))?;
        let sha = crate::eventlog::sha_bytes(new.as_bytes());
        if sha != meta["new_sha"].as_str().unwrap_or("") {
            bail!("the staged candidate was modified after it was proposed");
        }
        // The candidate replaces the whole module and its gate compared it with the module in force at proposal time:
        // if that module has changed since (another upgrade, a rollback), accepting would silently drop the change.
        let current = crate::eventlog::sha_bytes(self.effective_source(&module)?.as_bytes());
        if Some(current.as_str()) != meta["base_sha"].as_str() {
            bail!("{module} has changed since this proposal was made; propose the change again against the current module");
        }
        self.check_proposal(id)?;
        if !force {
            let gate: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("gate.json")).map_err(|_| anyhow!("the eval gate has not been run for this proposal (python3 eval/gate.py --proposal {id} ...)"))?)?;
            if gate["candidate_sha"].as_str() != Some(sha.as_str()) {
                bail!("the gate report is for a different candidate; rerun the gate");
            }
            if gate["pass"].as_bool() != Some(true) {
                bail!("the eval gate FAILED for this proposal: {}", gate["reason"].as_str().unwrap_or("regression"));
            }
        }
        let target = self.override_path(&module)?;
        let vdir = self.versions_dir(&module);
        std::fs::create_dir_all(&vdir)?;
        let n = std::fs::read_dir(&vdir).map(|rd| rd.count()).unwrap_or(0) + 1;
        let previous = std::fs::read_to_string(&target).unwrap_or_else(|_| "<<none>>".to_string());
        std::fs::write(vdir.join(format!("{n:04}.luau")), &previous)?;
        std::fs::create_dir_all(target.parent().unwrap())?;
        std::fs::write(&target, &new)?;
        self.log_upgrade(serde_json::json!({"action": if force { "accept(force)" } else { "accept" }, "module": module, "proposal": id, "new_sha": sha, "version": n}))?;
        Ok(format!("activated {module} (version {n}); undo with `hx upgrade rollback {module}`"))
    }

    pub fn rollback_upgrade(&self, module: &str) -> Result<String> {
        let target = self.override_path(module)?;
        let vdir = self.versions_dir(module);
        let mut files: Vec<_> = std::fs::read_dir(&vdir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
        files.sort();
        let Some(last) = files.pop() else { bail!("nothing to roll back for {module}") };
        let previous = std::fs::read_to_string(&last)?;
        if previous == "<<none>>" {
            let _ = std::fs::remove_file(&target);
        } else {
            std::fs::write(&target, previous)?;
        }
        std::fs::remove_file(&last)?;
        self.log_upgrade(serde_json::json!({"action": "rollback", "module": module}))?;
        Ok(format!("rolled back {module} to {}", if files.is_empty() { "the built-in module" } else { "the previous version" }))
    }

    pub fn reject(&self, name: &str) -> Result<()> {
        std::fs::remove_dir_all(self.staged_dir().join(name))?;
        Ok(())
    }

    /// `hx approve <pkg>`: record the lock entry and grant exactly the capabilities the manifest requests.
    pub fn approve(&self, pkg: &str) -> Result<String> {
        let (dir, man) = self.locate(pkg)?;
        let hash = hash_dir(&dir)?;
        let lock_path = self.root.join("hx.lock");
        let mut entries = read_toml::<Lockfile>(&lock_path)?.package;
        entries.retain(|e| e.name != pkg);
        entries.push(LockEntry { name: pkg.into(), version: man.package.version.clone(), hash: hash.clone() });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        std::fs::write(&lock_path, toml::to_string(&Lockfile { package: entries })?)?;
        let gpath = self.harness().join("grants.toml");
        std::fs::create_dir_all(self.harness())?;
        let mut g: Grants = read_toml(&gpath)?;
        g.grant.insert(pkg.into(), man.capabilities.clone());
        std::fs::write(&gpath, toml::to_string(&g)?)?;
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
        assert!(err(r.check_package("foo", &dir)).contains("not locked"));
        // a failed check is not cached: after approval the same resolver passes
        let msg = r.approve("foo").unwrap();
        assert!(msg.starts_with("approved foo 1.0"), "{msg}");
        let lock = std::fs::read_to_string(root.join("hx.lock")).unwrap();
        assert!(lock.contains("name = \"foo\"") && lock.contains("version = \"1.0\""), "{lock}");
        assert!(std::fs::read_to_string(root.join(".harness/grants.toml")).unwrap().contains("[grant.foo]\nfs = [\"read\"]"));
        assert_eq!(r.check_package("foo", &dir).unwrap().capabilities["fs"], vec!["read"]);
        // any content change invalidates the lock
        std::fs::write(dir.join("init.luau"), "return { x = 2 }\n").unwrap();
        assert!(err(Resolver::new(root.clone()).check_package("foo", &dir)).contains("changed since it was approved"));
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
        std::fs::write(root.join(".harness/grants.toml"), "[grant.foo]\nfs = [\"write\"]\n").unwrap();
        assert!(err(r.verify("foo", &dir, &man)).contains("capability fs=read which has not been granted"));
        std::fs::write(root.join(".harness/grants.toml"), "").unwrap();
        assert!(err(r.verify("foo", &dir, &man)).contains("not been granted"));
        std::fs::write(root.join(".harness/grants.toml"), "[grant.foo]\nfs = [\"*\"]\n").unwrap();
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
    fn resolve_order_override_package_builtin() {
        let root = ws("resolve");
        // the launcher's consent to use workspace config; policy tests set the same value
        unsafe { std::env::set_var("HX_TRUST", "1") };
        let r = Resolver::new(root.clone());
        let s = r.resolve("std/printer").unwrap();
        assert_eq!((s.origin.as_str(), s.is_override, s.pkg.is_none()), ("std:printer.luau", false, true));
        assert_eq!(r.resolve("std").unwrap().origin, "std:init.luau");
        assert!(err(r.resolve("nope/x")).contains("module not found: nope/x"));
        assert!(err(r.resolve("project/init")).contains("no project init"));
        write(root.join(".harness/init.luau"), "-- init\n");
        assert_eq!(r.resolve("project/init").unwrap().text, "-- init\n");
        // an override wins, and `base:` skips it
        write(root.join(".harness/override/std/printer.luau"), "return {}\n");
        let s = r.resolve("std/printer").unwrap();
        assert!(s.is_override && s.text == "return {}\n");
        assert!(!r.resolve("base:std/printer").unwrap().is_override);
        // a project package resolves only once approved, and carries its capabilities
        install_pkg(&root, "foo");
        assert!(err(r.resolve("foo")).contains("not locked"));
        r.approve("foo").unwrap();
        let s = r.resolve("foo").unwrap();
        assert_eq!(s.pkg.unwrap().1["fs"], vec!["read"]);
        assert!(err(r.resolve("foo/missing")).contains("module not found"));
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
    fn only_builtin_modules_can_be_upgraded() {
        assert!(Resolver::module_file("std/printer").is_ok());
        for bad in ["std", "std/", "foo/printer", "std/../x", "std/Printer", "std/nope"] {
            assert!(Resolver::module_file(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn proposal_must_load_return_a_table_and_keep_exports() {
        let root = ws("check");
        let r = Resolver::new(root.clone());
        assert!(err(r.propose("std/printer", " ", "")).contains("1 byte to 200 KB"));
        let check = |src: &str| r.check_proposal(&r.propose("std/printer", src, "").unwrap());
        assert!(err(check("return {")).contains("does not compile"));
        assert!(err(check("error('boom')")).contains("fails when loaded"));
        assert!(err(check("return 1")).contains("must return a table"));
        assert!(err(check("return {}")).contains("drops exports"));
        // a wrapper that copies `base:` keeps every export
        assert!(check("local b = require('base:std/printer')\nlocal m = {}\nfor k, v in b do m[k] = v end\nreturn m\n").is_ok());
        assert!(err(r.check_proposal("nope")).contains("no proposal"));
        // model-written code runs in the user's CLI: a loop or a memory bomb must fail the check, not hang it
        assert!(err(check("while true do end")).contains("fails when loaded"));
        assert!(err(check("local t = {} for i = 1, 1e9 do t[i] = string.rep('x', 1024) .. i end return t")).contains("fails when loaded"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upgrade_accept_refuses_a_proposal_whose_base_is_no_longer_in_force() {
        let root = ws("stale");
        let r = Resolver::new(root.clone());
        let builtin = r.effective_source("std/printer").unwrap();
        let a = r.propose("std/printer", &format!("{builtin}\n-- A\n"), "").unwrap();
        let b = r.propose("std/printer", &format!("{builtin}\n-- B\n"), "").unwrap();
        r.accept_upgrade(&a, true).unwrap();
        // B was made against the built-in module; accepting it now would silently drop A
        assert!(err(r.accept_upgrade(&b, true)).contains("changed since this proposal was made"));
        assert!(r.effective_source("std/printer").unwrap().ends_with("-- A\n"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn upgrade_accept_needs_a_passing_gate_and_rolls_back() {
        let root = ws("upgrade");
        let r = Resolver::new(root.clone());
        let builtin = r.effective_source("std/printer").unwrap();
        let new = format!("{builtin}\n-- v2\n");
        let id = r.propose("std/printer", &new, "test").unwrap();
        assert_eq!(r.list_proposals()[0]["reason"], "test");
        let dir = r.proposals_dir().join(&id);
        let sha = crate::eventlog::sha_bytes(new.as_bytes());
        assert!(err(r.accept_upgrade(&id, false)).contains("gate has not been run"));
        std::fs::write(dir.join("gate.json"), r#"{"candidate_sha": "other", "pass": true}"#).unwrap();
        assert!(err(r.accept_upgrade(&id, false)).contains("different candidate"));
        std::fs::write(dir.join("gate.json"), format!(r#"{{"candidate_sha": "{sha}", "pass": false, "reason": "slower"}}"#)).unwrap();
        assert!(err(r.accept_upgrade(&id, false)).contains("FAILED for this proposal: slower"));
        std::fs::write(dir.join("gate.json"), format!(r#"{{"candidate_sha": "{sha}", "pass": true}}"#)).unwrap();
        assert!(r.accept_upgrade(&id, false).unwrap().contains("version 1"));
        assert_eq!(r.effective_source("std/printer").unwrap(), new);
        // a second (forced, gate-less) upgrade stacks on the first
        let newer = format!("{new}-- v3\n");
        let id2 = r.propose("std/printer", &newer, "").unwrap();
        assert!(r.accept_upgrade(&id2, true).unwrap().contains("version 2"));
        assert_eq!(r.effective_source("std/printer").unwrap(), newer);
        // a candidate edited after proposing is refused even when forced
        std::fs::write(r.proposals_dir().join(&id2).join("new.luau"), "return {}\n").unwrap();
        assert!(err(r.accept_upgrade(&id2, true)).contains("modified after it was proposed"));
        let log = std::fs::read_to_string(root.join(".harness/upgrades.jsonl")).unwrap();
        assert!(log.contains("\"accept\"") && log.contains("\"accept(force)\""));
        assert!(r.rollback_upgrade("std/printer").unwrap().ends_with("the previous version"));
        assert_eq!(r.effective_source("std/printer").unwrap(), new);
        assert!(r.rollback_upgrade("std/printer").unwrap().ends_with("the built-in module"));
        assert_eq!(r.effective_source("std/printer").unwrap(), builtin);
        assert!(!root.join(".harness/override/std/printer.luau").exists());
        assert!(err(r.rollback_upgrade("std/printer")).contains("nothing to roll back"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
