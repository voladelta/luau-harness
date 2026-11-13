//! Staging: what the agent proposes, and the owner's review, accept, reject and rollback.
use super::approval::{snapshot, write_atomic, Location, Snapshot};
use super::{file_for, split_name, valid_package_name, Resolver};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Text as the owner's terminal shows it in a review: control characters (except newline and tab) and bidirectional
/// formatting characters are escaped, so model-written bytes cannot move the cursor, recolour or reorder what the
/// owner reads before answering.
pub(super) fn terminal_safe(text: &str) -> String {
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

/// Something staged, frozen as the owner reviewed it.
pub(super) enum Staged {
    Package { name: String, snapshot: Box<Snapshot>, replaces: bool },
    Module { module: String, new: String, base_sha: String },
}

/// What `hx accept` shows and then installs: `text` is rendered from the frozen bytes, and accepting installs exactly
/// those bytes.
pub struct Review {
    pub text: String,
    pub(super) staged: Staged,
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

    pub(super) fn module_file(module: &str) -> Result<(String, String)> {
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
