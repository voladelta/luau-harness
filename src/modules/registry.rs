//! The frozen registry a run loads from (capture and resolve), and what `hx test` runs.
use super::approval::{Lockfile, Location, Snapshot, snapshot};
use super::{valid_package_name, PkgInfo, Resolver, EMBEDDED};
use crate::supervisor::Capabilities;
use anyhow::{anyhow, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

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
    pub(super) fn build(root: &Path, config: Option<crate::trust::Config>) -> Result<Self> {
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

impl Resolver {
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
}
