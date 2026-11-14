//! Packages and skills: the one reader of their directories (`snapshot`), and where they are found.
use super::{Manifest, PkgInfo, Resolver, valid_package_name};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Where a package's files live, which also decides how its manifest is read.
#[derive(Clone, Debug)]
pub enum Location {
    /// A directory with an `hx.toml` (`.harness/pkgs/NAME`, a package under test): Luau with its manifest's capabilities.
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

/// A package's files as read once, and the manifest parsed from them.
#[derive(Debug)]
pub struct Snapshot {
    pub location: Location,
    /// Relative path (`/`-separated) to content. Dot-entries are not part of a package.
    pub files: BTreeMap<String, Vec<u8>>,
    pub manifest: Manifest,
}

/// Upper bounds of one snapshot: a package is source and documents, not a data directory.
const SNAPSHOT_FILES: usize = 10_000;
const SNAPSHOT_BYTES: usize = 64 << 20;

/// Read a package or skill. This is the only reader of package directories: workspace trust, `hx skills`, the run's
/// registry and `hx test` all go through it.
///
/// The directory itself may be a symlink (a skill linked into `~/.agents/skills`): its target's content is what is
/// read, hashed and loaded. Inside it, entries whose name starts with `.` (`.git`, editor files) are skipped, and a
/// symlink or special file is an error, since its target would not be part of what the digest covers.
pub fn snapshot(location: Location) -> Result<Snapshot> {
    let dir = location.dir();
    let files = read_tree(dir)?;
    let manifest = match &location {
        Location::Package(_) => {
            let text = files
                .get("hx.toml")
                .with_context(|| format!("{}: missing hx.toml", dir.display()))?;
            let text = std::str::from_utf8(text)
                .with_context(|| format!("{}: hx.toml is not UTF-8", dir.display()))?;
            toml::from_str(text).map_err(|e| anyhow!("{}: bad hx.toml: {e}", dir.display()))?
        }
        Location::AgentSkill(_) => {
            let text = files
                .get("SKILL.md")
                .with_context(|| format!("{}: missing SKILL.md", dir.display()))?;
            Manifest::agent_skill(&String::from_utf8_lossy(text))
        }
    };
    Ok(Snapshot {
        files,
        manifest,
        location,
    })
}

fn read_tree(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    fn walk(
        dir: &Path,
        prefix: &str,
        out: &mut BTreeMap<String, Vec<u8>>,
        total: &mut usize,
    ) -> Result<()> {
        use std::{io::Read, os::unix::fs::OpenOptionsExt};
        for entry in
            std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .with_context(|| format!("non-UTF-8 file name in {}", dir.display()))?;
            if name.starts_with('.') {
                continue;
            }
            let rel = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(&entry.path(), &rel, out, total)?;
            } else if kind.is_file() {
                anyhow::ensure!(
                    out.len() < SNAPSHOT_FILES,
                    "{} has more than {SNAPSHOT_FILES} files",
                    root_hint(dir, prefix)
                );
                let mut bytes = Vec::new();
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(entry.path())
                    .and_then(|f| {
                        f.take((SNAPSHOT_BYTES - *total) as u64 + 1)
                            .read_to_end(&mut bytes)
                    })
                    .with_context(|| format!("cannot read {}", entry.path().display()))?;
                *total += bytes.len();
                anyhow::ensure!(
                    *total <= SNAPSHOT_BYTES,
                    "{} is larger than {} MiB",
                    root_hint(dir, prefix),
                    SNAPSHOT_BYTES >> 20
                );
                out.insert(rel, bytes);
            } else if kind.is_symlink() {
                bail!(
                    "{} is a symlink; a package or skill may not contain symlinks (their targets are not covered by trust): copy the file in instead",
                    entry.path().display()
                );
            } else {
                bail!("{} is not a regular file", entry.path().display());
            }
        }
        Ok(())
    }
    /// The package root, for messages about the whole tree.
    fn root_hint(dir: &Path, prefix: &str) -> String {
        let depth = if prefix.is_empty() {
            0
        } else {
            prefix.split('/').count()
        };
        dir.ancestors()
            .nth(depth)
            .unwrap_or(dir)
            .display()
            .to_string()
    }
    anyhow::ensure!(
        std::fs::metadata(root)
            .with_context(|| format!("cannot read {}", root.display()))?
            .is_dir(),
        "{} is not a directory",
        root.display()
    );
    let mut files = BTreeMap::new();
    walk(root, "", &mut files, &mut 0)?;
    Ok(files)
}

impl Snapshot {
    pub(super) fn is_agent_skill(&self) -> bool {
        matches!(self.location, Location::AgentSkill(_))
    }

    /// The Luau modules this package provides, as (module name, file, source). An agent skill provides none.
    pub(super) fn modules(&self, name: &str) -> Result<Vec<(String, String, String)>> {
        if self.is_agent_skill() {
            return Ok(Vec::new());
        }
        self.files
            .iter()
            .filter_map(|(file, bytes)| {
                file.strip_suffix(".luau")
                    .map(|module| (module, file, bytes))
            })
            .map(|(module, file, bytes)| {
                let text = String::from_utf8(bytes.clone())
                    .with_context(|| format!("package {name}: {file} is not UTF-8"))?;
                let module = if module == "init" {
                    name.to_string()
                } else {
                    format!("{name}/{module}")
                };
                Ok((module, file.clone(), text))
            })
            .collect()
    }

    pub(super) fn info(&self, name: &str) -> PkgInfo {
        let package = &self.manifest.package;
        let kind = package.kind.clone().unwrap_or_default();
        PkgInfo {
            name: name.to_string(),
            version: package.version.clone(),
            description: package.description.clone().unwrap_or_default(),
            signature: self.manifest.tool.signature.clone().unwrap_or_default(),
            doc: self.manifest.tool.doc.clone().unwrap_or_default(),
            dir: String::new(),
            files: if kind == "skill" {
                self.files.keys().cloned().collect()
            } else {
                Vec::new()
            },
            kind,
        }
    }
}

/// A package or skill of the workspace, as workspace trust read it: trust hashes `snapshot`'s files, and a run loads
/// exactly those bytes. One that cannot be read (or has an invalid name) is kept with the reason, so it is reported,
/// never loaded.
#[derive(Debug)]
pub struct Extension {
    pub name: String,
    /// Its directory relative to the workspace: `.harness/pkgs/NAME` or `.agents/skills/NAME`.
    pub rel: String,
    pub snapshot: std::result::Result<Snapshot, String>,
}

impl Resolver {
    /// The workspace's packages (`.harness/pkgs/*`, sorted; skipped when `packages` is false), then its agent skills
    /// (`.agents/skills/*` with a SKILL.md, sorted), each read once. Dot-entries are neither. Shadowing is the
    /// registry's business: both are listed.
    pub fn workspace_extensions(&self, packages: bool) -> Vec<Extension> {
        let mut out = Vec::new();
        if packages {
            for (name, dir) in subdirs(&self.harness().join("pkgs")) {
                out.push(read(name, ".harness/pkgs", Location::Package(dir)));
            }
        }
        for (name, dir) in skill_dirs(&self.root.join(".agents/skills")) {
            out.push(read(name, ".agents/skills", Location::AgentSkill(dir)));
        }
        out
    }

    /// The user's own agent skills, `~/.agents/skills/*` (sorted): outside every workspace, so not part of its trust.
    pub fn user_skills() -> Vec<(String, PathBuf)> {
        std::env::var_os("HOME")
            .map(|home| skill_dirs(&PathBuf::from(home).join(".agents/skills")))
            .unwrap_or_default()
    }

    /// `hx skills`: every agent skill, the workspace's first, and whether the model can see it.
    pub fn skills_status(&self, dirs: &crate::dirs::Dirs) -> Result<String> {
        let config = crate::trust::Config::read(&self.root)?;
        let state = crate::trust::state(&self.root, dirs, &config);
        let packages: Vec<&str> = config
            .extensions()
            .iter()
            .filter(|e| e.rel.starts_with(".harness/"))
            .map(|e| e.name.as_str())
            .collect();
        let mut s = String::new();
        let mut seen: Vec<String> = Vec::new();
        for ext in config
            .extensions()
            .iter()
            .filter(|e| e.rel.starts_with(".agents/"))
        {
            let status = if packages.contains(&ext.name.as_str()) {
                "shadowed by .harness/pkgs".to_string()
            } else if let Err(e) = &ext.snapshot {
                format!(
                    "error: {}",
                    crate::trust::terminal_safe(e.lines().next().unwrap_or(""))
                )
            } else {
                state.of(&ext.rel).to_string()
            };
            s.push_str(&format!(
                "{:<28} {status:<24} {}\n",
                ext.name,
                self.root.join(&ext.rel).display()
            ));
            seen.push(ext.name.clone());
        }
        for (name, dir) in Self::user_skills() {
            let status = if packages.contains(&name.as_str()) || seen.contains(&name) {
                "shadowed by the workspace".to_string()
            } else if !valid_package_name(&name) {
                "error: not a valid skill name".to_string()
            } else {
                match snapshot(Location::AgentSkill(dir.clone())) {
                    Ok(_) => "user skill".to_string(),
                    Err(e) => format!(
                        "error: {}",
                        crate::trust::terminal_safe(e.to_string().lines().next().unwrap_or(""))
                    ),
                }
            };
            s.push_str(&format!("{name:<28} {status:<24} {}\n", dir.display()));
        }
        Ok(if s.is_empty() {
            "no skills in .agents/skills or ~/.agents/skills\n".into()
        } else {
            s
        })
    }
}

/// Read one extension, unless its name is invalid.
fn read(name: String, parent: &str, location: Location) -> Extension {
    let rel = format!("{parent}/{name}");
    let snapshot = if valid_package_name(&name) {
        snapshot(location).map_err(|e| format!("{e:#}"))
    } else {
        Err(format!(
            "{name:?} is not a valid package name (lowercase letters, digits, _ and -, starting with a letter; not std or project)"
        ))
    };
    Extension {
        name,
        rel,
        snapshot,
    }
}

/// The subdirectories of `dir` (a link to a directory counts), sorted by name; dot-entries and non-UTF-8 names are not
/// extensions.
fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    out.sort();
    out
}

/// The skill directories of a `.agents/skills` root: subdirectories with a SKILL.md.
fn skill_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    subdirs(root)
        .into_iter()
        .filter(|(_, dir)| dir.join("SKILL.md").is_file())
        .collect()
}
