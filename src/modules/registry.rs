//! The frozen registry a run loads from (capture and resolve), and what `hx test` runs.
use super::snapshot::{Location, Snapshot, snapshot};
use super::{EMBEDDED, PkgInfo, Resolver, valid_package_name};
use crate::supervisor::Capabilities;
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
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

/// An immutable byte snapshot of everything a run may load. Runtime resolution never goes back to mutable workspace
/// files.
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
            match self.rejected.get(package) {
                Some(error) => anyhow!("{error}"),
                None => anyhow!("module not found: {name}"),
            }
        })
    }

    pub fn asset(&self, name: &str) -> Result<String> {
        String::from_utf8(
            self.assets
                .get(name)
                .ok_or_else(|| anyhow!("no asset {name}"))?
                .clone(),
        )
        .map_err(Into::into)
    }

    /// Freeze everything a run may load: the built-in package; when the workspace config is trusted, the overrides,
    /// project init, packages, agent skills, MCP servers and local tools from the bytes trust covered; and the user's
    /// own agent skills that no workspace package or skill shadows.
    pub fn capture(root: &Path, dirs: &crate::dirs::Dirs) -> Result<Self> {
        Self::build(root, crate::trust::trusted(root, dirs)?, dirs)
    }

    /// `capture` from config already read and judged: `config` is None when it may not be used.
    pub(super) fn build(
        root: &Path,
        config: Option<crate::trust::Config>,
        dirs: &crate::dirs::Dirs,
    ) -> Result<Self> {
        let mut registry = Registry::default();
        for (pkg, file, text) in EMBEDDED {
            let key = format!("{pkg}/{file}");
            registry
                .assets
                .insert(key.clone(), text.as_bytes().to_vec());
            if let Some(module) = file.strip_suffix(".luau") {
                let name = if module == "init" {
                    pkg.to_string()
                } else {
                    format!("{pkg}/{module}")
                };
                let base = Source {
                    text: text.to_string(),
                    origin: key,
                    is_override: false,
                    pkg: None,
                };
                registry
                    .sources
                    .insert(format!("base:{name}"), base.clone());
                registry.sources.insert(name, base);
            }
        }
        // names taken: a workspace package shadows a skill of its name, a workspace skill a user skill
        let mut taken = BTreeSet::new();
        if let Some(config) = config {
            registry.add_workspace(root, &config, dirs, &mut taken)?;
        }
        for (name, dir) in Resolver::user_skills() {
            if !taken.insert(name.clone()) {
                continue;
            }
            let snap = if valid_package_name(&name) {
                snapshot(Location::AgentSkill(dir))
            } else {
                Err(anyhow!("{name:?} is not a valid skill name"))
            };
            registry.load(&name, snap.map_err(|e| format!("{e:#}")).as_ref());
        }
        Ok(registry)
    }

    /// The trusted workspace config's part of `build`.
    fn add_workspace(
        &mut self,
        root: &Path,
        config: &crate::trust::Config,
        dirs: &crate::dirs::Dirs,
        taken: &mut BTreeSet<String>,
    ) -> Result<()> {
        // Only files that replace a built-in file are overrides; anything else in the tree is not used.
        for (rel, bytes) in config.tree(".harness/override") {
            let Some(file) = rel.strip_prefix("std/") else {
                continue;
            };
            if Resolver::embedded("std", file).is_none() {
                continue;
            }
            self.assets.insert(rel.to_string(), bytes.to_vec());
            if let Some(module) = file.strip_suffix(".luau") {
                let name = if module == "init" {
                    "std".to_string()
                } else {
                    format!("std/{module}")
                };
                let text = String::from_utf8(bytes.to_vec())
                    .with_context(|| format!(".harness/override/{rel} is not UTF-8"))?;
                self.sources.insert(
                    name,
                    Source {
                        text,
                        origin: rel.to_string(),
                        is_override: true,
                        pkg: None,
                    },
                );
            }
        }
        if let Some(init) = config.file(".harness/init.luau") {
            let text =
                String::from_utf8(init.to_vec()).context(".harness/init.luau is not UTF-8")?;
            self.sources.insert(
                "project/init".into(),
                Source {
                    text,
                    origin: "project/init".into(),
                    is_override: false,
                    pkg: None,
                },
            );
        }
        for ext in config.extensions() {
            if taken.insert(ext.name.clone()) {
                self.load(&ext.name, ext.snapshot.as_ref());
            }
        }
        self.mcp = crate::mcp::snapshot(config.file(".harness/mcp.toml"))?;
        crate::tools::capture(dirs, root, config.tree(".harness/tools"), self)?;
        Ok(())
    }

    /// Load one package or skill, or record why it is not loaded.
    fn load(&mut self, name: &str, snap: std::result::Result<&Snapshot, &String>) {
        let added = match snap {
            Ok(snap) => self
                .add(name, snap)
                .map(|()| snap.info(name))
                .map_err(|e| e.to_string()),
            Err(error) => Err(error.clone()),
        };
        match added {
            Ok(info) => self.packages.push(info),
            Err(error) => {
                self.rejected.insert(name.to_string(), error);
            }
        }
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
            self.sources.insert(
                module,
                Source {
                    text,
                    origin: format!("{name}/{file}"),
                    is_override: false,
                    pkg: Some((name.to_string(), caps.clone())),
                },
            );
        }
        self.rejected.remove(name);
        Ok(())
    }

    /// `hx test DIR`: make a package loadable for this one registry (never a run's), so its tests can run inside its
    /// boundary.
    pub fn mount(&mut self, name: &str, snap: &Snapshot) -> Result<()> {
        self.add(name, snap)
    }

    /// The test files of a package in this registry: `tests/*.luau`, as (file name, source), sorted.
    pub fn tests(&self, name: &str) -> Result<Vec<(String, String)>> {
        let prefix = format!("{name}/tests/");
        self.assets
            .iter()
            .filter_map(|(key, bytes)| {
                key.strip_prefix(prefix.as_str())
                    .filter(|f| f.ends_with(".luau") && !f.contains('/'))
                    .map(|f| (f, bytes))
            })
            .map(|(file, bytes)| {
                Ok((
                    file.to_string(),
                    String::from_utf8(bytes.clone())
                        .with_context(|| format!("tests/{file} is not UTF-8"))?,
                ))
            })
            .collect()
    }

    pub fn materialize_skills(&mut self, log: &crate::eventlog::EventLog) -> Result<()> {
        let base = log.dir().join("registry").join(&log.session);
        for package in &mut self.packages {
            if package.kind != "skill" {
                continue;
            }
            let target = base.join(&package.name);
            for file in &package.files {
                let key = format!("{}/{file}", package.name);
                if let Some(bytes) = self.assets.get(&key) {
                    let path = target.join(file);
                    std::fs::create_dir_all(
                        path.parent().expect("a joined file path has a parent"),
                    )?;
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
    for entry in
        std::fs::read_dir(&tests).with_context(|| format!("cannot read {}", tests.display()))?
    {
        let path = entry?.path();
        if path.extension().is_some_and(|x| x == "luau") && path.is_file() {
            let name = path
                .file_name()
                .expect("a directory entry has a name")
                .to_string_lossy()
                .to_string();
            out.push((
                name,
                std::fs::read_to_string(&path)
                    .with_context(|| format!("cannot read {}", path.display()))?,
            ));
        }
    }
    out.sort();
    Ok(out)
}

/// What `hx test DIR` runs.
pub enum TestTarget {
    /// The tests of a package, inside its boundary, loaded from this snapshot with its manifest's capabilities.
    Package {
        name: String,
        snapshot: Box<Snapshot>,
    },
    /// The built-in std package's suite, as compiled into this binary (`EMBEDDED_TESTS`), whatever the directory
    /// holds: it runs in the harness's module environment with the agent's authority, as `hx exec` would.
    Builtin,
    /// A directory without `hx.toml`, which is not a package: its tests run with the agent's authority, like `hx exec`,
    /// and `note` says so.
    Unpackaged { note: String },
}

/// This binary's own `stdlib/std`, for developing the built-in package: `hx test` runs the embedded suite there.
fn is_source_std(dir: &Path) -> bool {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("stdlib/std")
        .canonicalize()
        .is_ok_and(|source| source == dir)
}

impl Resolver {
    /// What `hx test DIR` runs: see `TestTarget`. `dir` must be canonical.
    pub fn test_target(&self, dir: &Path) -> Result<TestTarget> {
        if is_source_std(dir) {
            return Ok(TestTarget::Builtin);
        }
        let inside_harness = dir.starts_with(self.harness());
        if !dir.join("hx.toml").exists() {
            anyhow::ensure!(
                !inside_harness,
                "{} has no hx.toml, so it is not a package",
                dir.display()
            );
            let note = format!(
                "{} has no hx.toml, so it is not a package: its tests run with the agent's authority, like `hx exec`",
                dir.display()
            );
            return Ok(TestTarget::Unpackaged { note });
        }
        let snap = snapshot(Location::Package(dir.to_path_buf()))?;
        let dir_name = || {
            dir.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default()
        };
        // installed packages go by their directory's name, as a run loads them
        let name = match &snap.manifest.package.name {
            Some(name) if dir.parent() != Some(self.harness().join("pkgs").as_path()) => {
                name.clone()
            }
            _ => dir_name(),
        };
        // Any directory can claim the name, and one that looks like std is still an ordinary package.
        anyhow::ensure!(
            name != "std",
            "{} calls itself std, but only the built-in package is std (its suite is compiled into hx); give it another name",
            dir.display()
        );
        anyhow::ensure!(
            valid_package_name(&name),
            "{name:?} is not a valid package name"
        );
        Ok(TestTarget::Package {
            name,
            snapshot: Box::new(snap),
        })
    }
}
