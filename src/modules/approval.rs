//! Approval: the one package reader (`snapshot`), its digest, `hx.lock`, `hx approve` and `hx skills`.
use super::{Manifest, PkgInfo, Resolver, valid_package_name};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `hx.lock`: one record per approved package: the exact content approved. The digest covers `hx.toml`, so approving
/// it grants exactly the capabilities that manifest requests; there is no separate grant list to keep in step.
#[derive(Debug, Deserialize, serde::Serialize, Default)]
pub(super) struct Lockfile {
    #[serde(default)]
    package: Vec<LockEntry>,
}

/// One approval. `kind` says what the content was approved as, so content approved as an inert agent skill can never
/// load as a package (with its hx.toml's capabilities and its Luau) from `.harness/pkgs`. An entry without one was
/// written before approvals recorded it and matches nothing.
#[derive(Debug, Deserialize, serde::Serialize, Clone)]
struct LockEntry {
    name: String,
    version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<Kind>,
    hash: String,
}

/// What content is approved as. It decides how the content is read (see `Location`) and is bound into both the digest
/// and the lock entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Kind {
    /// A directory with an `hx.toml`: its Luau runs with the capabilities the manifest requests.
    Package,
    /// SKILL.md and plain files: no Luau, no capabilities.
    AgentSkill,
}

impl Kind {
    /// The digest's domain tag: the same files hash differently as a package and as a skill.
    fn domain(self) -> &'static [u8] {
        match self {
            Self::Package => b"hx package v3\0",
            Self::AgentSkill => b"hx agent-skill v3\0",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Package => "a package",
            Self::AgentSkill => "an agent skill",
        })
    }
}

impl Lockfile {
    pub(super) fn parse(bytes: Option<&[u8]>) -> Result<Lockfile> {
        let Some(bytes) = bytes else {
            return Ok(Lockfile::default());
        };
        toml::from_str(std::str::from_utf8(bytes).context("hx.lock is not UTF-8")?)
            .context("hx.lock is not valid")
    }

    /// The lock on disk, for the owner's commands (a run reads it through trust, see `Registry::capture`).
    pub(super) fn read(path: &Path) -> Result<Lockfile> {
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
    pub(super) fn approved(&self, name: &str, location: Location) -> Result<Snapshot> {
        // checked before reading anything: ~/.agents/skills can be large
        let entry = self
            .entry(name)
            .ok_or_else(|| LockError::NotLocked(name.to_string()))?;
        let kind = location.kind();
        if let Some(approved) = entry.kind.filter(|approved| *approved != kind) {
            return Err(LockError::OtherKind {
                name: name.to_string(),
                approved,
                now: kind,
            }
            .into());
        }
        let snap = snapshot(location)?;
        if entry.kind != Some(kind)
            || entry.hash != snap.digest
            || entry.version != snap.manifest.package.version
        {
            return Err(LockError::Changed {
                name: name.to_string(),
                locked: entry.hash.clone(),
                now: snap.digest,
            }
            .into());
        }
        Ok(snap)
    }
}

/// Why a package is not usable although it is installed.
#[derive(Debug)]
pub enum LockError {
    NotLocked(String),
    Changed {
        name: String,
        locked: String,
        now: String,
    },
    /// Approved as one kind of thing and now found as another (a skill copied into `.harness/pkgs`).
    OtherKind {
        name: String,
        approved: Kind,
        now: Kind,
    },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLocked(pkg) => write!(
                f,
                "package {pkg} is not locked; review it, then run `hx approve {pkg}`"
            ),
            Self::Changed { name, locked, now } => write!(
                f,
                "package {name} changed since it was approved (locked {}…, now {}…); review it and run `hx approve {name}` again",
                locked.get(..8).unwrap_or(locked),
                now.get(..8).unwrap_or(now)
            ),
            Self::OtherKind {
                name,
                approved,
                now,
            } => write!(
                f,
                "{name} was approved as {approved}, not as {now}; review it and run `hx approve {name}` again"
            ),
        }
    }
}

impl std::error::Error for LockError {}

/// Where a package's files live, which also decides how its manifest is read.
#[derive(Clone, Debug)]
pub enum Location {
    /// A directory with an `hx.toml` (`.harness/pkgs/NAME`, a package under test).
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

    pub(super) fn kind(&self) -> Kind {
        match self {
            Self::Package(_) => Kind::Package,
            Self::AgentSkill(_) => Kind::AgentSkill,
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

/// Read a package or skill. This is the only reader of package directories: approval, `hx skills`, the run's registry
/// and `hx test` all go through it.
///
/// The directory itself may be a symlink (a skill linked into `~/.agents/skills`): its target's content is what is
/// read, hashed and approved. Inside it, entries whose name starts with `.` (`.git`, editor files) are skipped, and a
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
        digest: digest(location.kind(), &files),
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
                    "{} is a symlink; a package or skill may not contain symlinks (their targets are not covered by its approval): copy the file in instead",
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

/// The approval digest: SHA-256 over the kind's domain tag, then every file as length-prefixed path and content, in path
/// order. The framing makes the encoding unambiguous, so no other set of files (bytes moved between names or files)
/// shares a digest, and the tag keeps a skill's digest from ever matching a package's.
pub(super) fn digest(kind: Kind, files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut h = Sha256::new();
    h.update(kind.domain());
    for (path, bytes) in files {
        h.update((path.len() as u64).to_le_bytes());
        h.update(path.as_bytes());
        h.update((bytes.len() as u64).to_le_bytes());
        h.update(bytes);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
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

/// Write a whole file so a crash leaves the old content or the new, never a truncated one. `path` lies under the
/// canonical workspace; no link on the way to it, or at it, is followed, and missing directories are created.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    crate::atomic_file::Target::open(path, true)
        .map_err(anyhow::Error::from)
        .and_then(|file| file.replace(bytes))
        .with_context(|| format!("cannot write {}", path.display()))
}

impl Resolver {
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
        self.agent_skill_roots()
            .into_iter()
            .map(|r| r.join(name))
            .find(|d| d.join("SKILL.md").is_file())
    }

    /// Where package `name` lives: `.harness/pkgs/<name>`, else an agent skill.
    pub(super) fn locate(&self, name: &str) -> Result<Location> {
        let pdir = self.harness().join("pkgs").join(name);
        if pdir.is_dir() {
            return Ok(Location::Package(pdir));
        }
        if let Some(d) = self.agent_skill_dir(name) {
            return Ok(Location::AgentSkill(d));
        }
        bail!(
            "no package or skill named {name} (looked in .harness/pkgs, .agents/skills, ~/.agents/skills)"
        )
    }

    /// Every skill found under the `.agents/skills` roots: (name, dir), first root wins.
    pub fn agent_skills(&self) -> Vec<(String, PathBuf)> {
        let mut out: Vec<(String, PathBuf)> = Vec::new();
        for root in self.agent_skill_roots() {
            let Ok(rd) = std::fs::read_dir(&root) else {
                continue;
            };
            let mut names: Vec<String> = rd
                .flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect();
            names.sort();
            for n in names {
                if !out.iter().any(|(m, _)| *m == n)
                    && self
                        .agent_skill_dir(&n)
                        .is_some_and(|d| d.parent() == Some(root.as_path()))
                {
                    out.push((n.clone(), root.join(&n)));
                }
            }
        }
        out
    }

    /// Every installed package (`.harness/pkgs`, sorted), then every agent skill no package shadows. Dot-entries in
    /// `.harness/pkgs` are not packages.
    pub(super) fn installed(&self) -> Vec<(String, Location)> {
        let mut out: Vec<(String, Location)> = std::fs::read_dir(self.harness().join("pkgs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| {
                (
                    e.file_name().to_string_lossy().to_string(),
                    Location::Package(e.path()),
                )
            })
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
    pub fn skills_status(&self, dirs: &crate::dirs::Dirs) -> Result<String> {
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
                        Some(LockError::Changed { .. } | LockError::OtherKind { .. }) => {
                            "changed since approval".into()
                        }
                        None => format!(
                            "error: {}",
                            crate::trust::terminal_safe(e.to_string().lines().next().unwrap_or(""))
                        ),
                    },
                }
            };
            s.push_str(&format!("{name:<28} {state:<24} {}\n", dir.display()));
        }
        if !crate::trust::is_trusted(&self.root, dirs) {
            s.insert_str(
                0,
                "workspace config not trusted: no skill is visible until you run `hx trust`\n",
            );
        }
        Ok(if s.is_empty() {
            "no skills in .agents/skills or ~/.agents/skills\n".into()
        } else {
            s
        })
    }

    /// Record `snap` as the approved content of `name` in `hx.lock`.
    pub(super) fn lock(&self, name: &str, snap: &Snapshot) -> Result<()> {
        let lock_path = self.root.join("hx.lock");
        let mut entries = Lockfile::read(&lock_path)?.package;
        entries.retain(|e| e.name != name);
        entries.push(LockEntry {
            name: name.into(),
            version: snap.manifest.package.version.clone(),
            kind: Some(snap.location.kind()),
            hash: snap.digest.clone(),
        });
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        write_atomic(
            &lock_path,
            toml::to_string(&Lockfile { package: entries })?.as_bytes(),
        )
    }

    /// `hx approve <pkg>`: lock the package's exact content, which grants exactly the capabilities its manifest requests.
    pub fn approve(&self, pkg: &str) -> Result<String> {
        anyhow::ensure!(
            valid_package_name(pkg),
            "{pkg:?} is not a valid package name (lowercase letters, digits, _ and -, starting with a letter; not std or project)"
        );
        let snap = snapshot(self.locate(pkg)?)?;
        self.lock(pkg, &snap)?;
        let dir = snap.location.dir();
        let mut out = format!(
            "approved {pkg} {} ({}…) from {}",
            snap.manifest.package.version,
            &snap.digest[..8],
            dir.display()
        );
        if let Ok(target) = std::fs::read_link(dir) {
            out.push_str(&format!(
                " (a link to {}; its content is what was approved)",
                target.display()
            ));
        }
        if snap.is_agent_skill() {
            out.push_str(": an agent skill, so SKILL.md and its files only, with no Luau and no capabilities");
            if snap.files.contains_key("hx.toml") {
                out.push_str(" (its hx.toml is an ordinary file)");
            }
        } else {
            out.push_str(&format!(
                " with capabilities {:?}",
                snap.manifest.capabilities
            ));
        }
        Ok(out)
    }
}
