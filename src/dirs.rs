//! Where hx keeps its own files. Everything hx treats as authority or as a secret lives outside every workspace, in
//! directories the model can never reach: no file-tool root and no confined child's root may contain them.
//!
//! - configuration, `$XDG_CONFIG_HOME/hx/` (default `~/.config/hx/`): the trust store, the user's `secrets.toml`, and
//!   per workspace `workspaces/<name>-<id>/secrets.toml`;
//! - state, `$XDG_STATE_HOME/hx/` (default `~/.local/state/hx/`): per workspace `workspaces/<name>-<id>/log/` (journals,
//!   blobs, lock files, revocation markers, materialized skills) and `.../ar/` (autoresearch runs).
//!
//! `<name>` is the workspace directory's name made safe for a path, `<id>` the first 16 hex digits of the SHA-256 of its
//! canonical path. The environment is read once, at startup (`Dirs::from_env`), and the result passed down.
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// hx's configuration and state directories, canonical (the deepest existing ancestor resolved), so they compare
/// with the canonical paths the policy checks.
#[derive(Clone, Debug)]
pub struct Dirs {
    config: PathBuf,
    state: PathBuf,
}

/// One workspace's files under `Dirs`.
#[derive(Clone, Debug)]
pub struct WorkspaceDirs {
    /// Journals, blobs, lock files, revocation markers and the per-run `registry/` of materialized skills.
    pub log: PathBuf,
    /// Autoresearch runs.
    pub ar: PathBuf,
    /// The workspace's credentials for local tools.
    pub secrets: PathBuf,
}

impl Dirs {
    /// From `XDG_CONFIG_HOME` and `XDG_STATE_HOME` (a relative value is ignored, as the XDG specification says),
    /// else from `HOME`.
    pub fn from_env() -> Result<Self> {
        let base = |var: &str, default: &str| -> Result<PathBuf> {
            match std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
            {
                Some(dir) => Ok(dir),
                None => std::env::var_os("HOME")
                    .filter(|h| !h.is_empty())
                    .map(|h| PathBuf::from(h).join(default))
                    .with_context(|| {
                        format!("neither {var} nor HOME is set; hx needs one to find its own files")
                    }),
            }
        };
        Ok(Self::at(
            &base("XDG_CONFIG_HOME", ".config")?.join("hx"),
            &base("XDG_STATE_HOME", ".local/state")?.join("hx"),
        ))
    }

    /// Explicit directories (tests, and the paths `from_env` found).
    pub fn at(config: &Path, state: &Path) -> Self {
        Self {
            config: resolve(config),
            state: resolve(state),
        }
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    pub fn state(&self) -> &Path {
        &self.state
    }

    /// The trust store (`crate::trust`).
    pub fn trust_store(&self) -> PathBuf {
        self.config.join("trust.toml")
    }

    /// The user's credentials for local tools, shared by every workspace.
    pub fn user_secrets(&self) -> PathBuf {
        self.config.join("secrets.toml")
    }

    /// The directories of `workspace` (canonicalized here when it exists).
    pub fn workspace(&self, workspace: &Path) -> WorkspaceDirs {
        let name = workspace_key(workspace);
        let state = self.state.join("workspaces").join(&name);
        WorkspaceDirs {
            log: state.join("log"),
            ar: state.join("ar"),
            secrets: self
                .config
                .join("workspaces")
                .join(name)
                .join("secrets.toml"),
        }
    }

    /// Is `path` (canonical or lexically normal) hx's own configuration or state, or inside it?
    pub fn owns(&self, path: &Path) -> bool {
        path.starts_with(&self.config) || path.starts_with(&self.state)
    }

    /// Which of hx's directories a root of the model's would reach, if any: one containing `root`, or one `root`
    /// contains. Returns a name for messages and the directory.
    pub fn overlap(&self, root: &Path) -> Option<(&'static str, &Path)> {
        [("configuration", self.config()), ("state", self.state())]
            .into_iter()
            .find(|(_, dir)| root.starts_with(dir) || dir.starts_with(root))
    }
}

/// `<name>-<id>` for a workspace, see the module documentation.
fn workspace_key(workspace: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canonical = std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let name: String = canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    let name = match name.trim_start_matches('.') {
        "" => "root".to_string(),
        n => n.to_string(),
    };
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let id: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("{name}-{id}")
}

/// `dir` with its deepest existing ancestor canonicalized (symlinks resolved): the policy compares canonical paths, so
/// `~/.config -> dotfiles/config` must not slip past it.
fn resolve(dir: &Path) -> PathBuf {
    let mut base = dir.to_path_buf();
    let mut rest = vec![];
    while std::fs::symlink_metadata(&base).is_err() {
        let (Some(name), Some(parent)) = (base.file_name(), base.parent()) else {
            return dir.to_path_buf();
        };
        rest.push(name.to_os_string());
        base = parent.to_path_buf();
    }
    let mut out = std::fs::canonicalize(&base).unwrap_or(base);
    out.extend(rest.iter().rev());
    out
}

/// Say once, on stderr, that files an earlier version of hx kept inside the workspace are no longer read, and where
/// their successors live. Nothing is moved.
pub fn note_leftovers(workspace: &Path, dirs: &WorkspaceDirs) {
    for (old, new) in [
        (".harness/secrets.toml", &dirs.secrets),
        (".harness/log", &dirs.log),
        (".harness/ar", &dirs.ar),
    ] {
        if std::fs::symlink_metadata(workspace.join(old)).is_ok() {
            eprintln!(
                "hx: note: {old} is left from an earlier version of hx and is not read; hx keeps it in {} now",
                new.display()
            );
        }
    }
}

/// A `Dirs` in a fresh temporary directory beside nothing else, removed when dropped: unit tests never touch the
/// developer's own `~/.config/hx` or `~/.local/state/hx`.
#[cfg(test)]
pub struct TestDirs {
    base: PathBuf,
    dirs: Dirs,
}

#[cfg(test)]
impl TestDirs {
    pub fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("hx-test-dirs-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("a temporary directory");
        let dirs = Dirs::at(&base.join("config/hx"), &base.join("state/hx"));
        Self { base, dirs }
    }
}

#[cfg(test)]
impl std::ops::Deref for TestDirs {
    type Target = Dirs;
    fn deref(&self) -> &Dirs {
        &self.dirs
    }
}

#[cfg(test)]
impl Drop for TestDirs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_directories_are_named_by_a_safe_name_and_the_canonical_path() {
        let dirs = TestDirs::new();
        let ws = dirs.base.join("my project!");
        std::fs::create_dir_all(&ws).unwrap();
        let a = dirs.workspace(&ws);
        let name = a
            .log
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            name.starts_with("my_project_-") && name.len() == "my_project_-".len() + 16,
            "{name}"
        );
        assert!(
            a.log.starts_with(dirs.state())
                && a.ar.starts_with(dirs.state())
                && a.secrets.starts_with(dirs.config())
        );
        // another spelling of the same directory is the same workspace; another directory is not
        assert_eq!(dirs.workspace(&ws.join("../my project!")).log, a.log);
        std::fs::create_dir_all(dirs.base.join("x/my project!")).unwrap();
        assert_ne!(dirs.workspace(&dirs.base.join("x/my project!")).log, a.log);
        assert!(
            dirs.workspace(Path::new("/"))
                .log
                .to_string_lossy()
                .contains("/workspaces/root-")
        );
    }

    #[test]
    fn overlap_is_either_direction() {
        let dirs = TestDirs::new();
        assert!(dirs.overlap(&dirs.base).is_some());
        assert!(dirs.overlap(Path::new("/")).is_some());
        assert_eq!(
            dirs.overlap(&dirs.state().join("workspaces")).map(|o| o.0),
            Some("state")
        );
        assert!(dirs.overlap(&dirs.base.join("elsewhere")).is_none());
        assert!(
            dirs.owns(&dirs.config().join("trust.toml")) && !dirs.owns(&dirs.base.join("configx"))
        );
    }

    #[test]
    fn leftovers_are_named_with_their_new_location() {
        let dirs = TestDirs::new();
        let ws = dirs.base.join("ws");
        std::fs::create_dir_all(ws.join(".harness/log")).unwrap();
        std::fs::write(ws.join(".harness/secrets.toml"), "K = \"v\"").unwrap();
        // The notice only prints; what matters is that nothing is moved or read.
        note_leftovers(&ws, &dirs.workspace(&ws));
        assert!(ws.join(".harness/log").is_dir() && ws.join(".harness/secrets.toml").is_file());
        assert!(!dirs.state().exists() && !dirs.config().exists());
    }
}
