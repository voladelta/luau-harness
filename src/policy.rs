//! Capability policy + macOS Seatbelt sandbox for model-reachable effects.
//!
//! Core guarantees (enforced here, in Rust, where no Luau code or hook can change them):
//!  * `sh` runs under Seatbelt: writes only to the workspace and a per-run temp dir, reads only from an allowlist,
//!    no network unless granted, a scrubbed environment, its own process group (killed on timeout), and a process cap.
//!  * `read`/`write`/`edit`/`search` check the *canonical* path against the same roots, never write through symlinks.
//!  * `.harness/` and `hx.lock` are protected: the model can read them but never write them (overrides, init, staging).
//!
//! User-owned config: `<workspace>/.harness/policy.toml` (protected from the model):
//!     [fs]  read = ["/extra/read/root"]   write = ["/extra/write/root"]
//!     [net] allow = true                   # Seatbelt cannot filter by host; this is all-or-nothing
//!     [gpu] allow = true                   # let `sh` use Metal (GPU renderers such as Skia); off by default
use crate::eventlog::EventLog;
use anyhow::{bail, Result};
use serde_json::json;
use std::sync::Arc;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize, Default)]
struct PolicyFile {
    #[serde(default)]
    fs: FsSection,
    #[serde(default)]
    net: NetSection,
    #[serde(default)]
    approval: ApprovalSection,
    #[serde(default)]
    budget: Budget,
    #[serde(default)]
    packs: PacksSection,
    #[serde(default)]
    gpu: GpuSection,
}
/// `[gpu] allow = true`: let `sh` open the GPU (Metal), e.g. for Skia/wgpu renderers. Off by default.
#[derive(Deserialize, Default)]
struct GpuSection {
    #[serde(default)]
    allow: bool,
}
#[derive(Deserialize, Default)]
struct PacksSection {
    #[serde(default)]
    enable: Vec<String>,
}

/// Limits for one run, shared with every subagent. None = unlimited. Cost needs a provider that reports it
/// (OpenRouter, Enclave); tokens (input + cache reads + cache writes + output) work everywhere.
#[derive(Deserialize, serde::Serialize, Default, Clone, Debug, PartialEq)]
pub struct Budget {
    pub max_cost: Option<f64>,
    pub max_tokens: Option<u64>,
    pub max_wall_secs: Option<f64>,
}
#[derive(Deserialize, Default)]
struct ApprovalSection {
    /// extra command substrings that count as external effects (need approval)
    #[serde(default)]
    ask: Vec<String>,
    /// commands the USER pre-approves (substring match; an irreversible command needs an exact full-command match)
    #[serde(default)]
    allow: Vec<String>,
    mode: Option<ApprovalMode>,
}
#[derive(Deserialize, Default)]
struct FsSection {
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}
#[derive(Deserialize, Default)]
struct NetSection {
    #[serde(default)]
    allow: bool,
}

/// What happens to a command that needs approval and is not pre-approved in `policy.toml`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Refuse it (the headless default).
    Deny,
    /// Ask on the terminal.
    Prompt,
}

impl std::str::FromStr for ApprovalMode {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "deny" => Ok(Self::Deny),
            "prompt" => Ok(Self::Prompt),
            _ => anyhow::bail!("approval mode must be deny or prompt (got {s:?})"),
        }
    }
}

/// The live policy: the frozen settings a run's manifest records, plus its journal.
pub struct Policy {
    cfg: Snapshot,
    pub log: Arc<EventLog>,
}

impl std::ops::Deref for Policy {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot { &self.cfg }
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
pub struct Snapshot {
    pub enabled: bool,
    pub workspace: PathBuf,
    pub tmp: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
    pub net: bool,
    pub gpu: bool,
    pub ask: Vec<String>,
    pub allow: Vec<String>,
    pub mode: ApprovalMode,
    pub budget: Budget,
    pub packs: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Class {
    Read,
    LocalWrite,
    External,
    Irreversible,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Read => "read",
            Class::LocalWrite => "local_write",
            Class::External => "external",
            Class::Irreversible => "irreversible",
        }
    }
}

/// Commands that reach outside the machine or cannot be undone. A tripwire for the obvious cases; the sandbox (no
/// network, writes confined to the project) is the real boundary.
const EXTERNAL: &[&str] = &["git push", "npm publish", "cargo publish", "twine upload", "docker push", "ssh ", "scp ", "rsync ", "kubectl apply", "kubectl delete", "terraform apply", "gh pr create", "gh release", "curl -X POST", "curl -X PUT", "curl -X DELETE", "aws ", "gcloud ", "sendmail"];
const IRREVERSIBLE: &[&str] = &["git push --force", "git push -f", "rm -rf /", "rm -rf ~", "mkfs", "dd if="];

const SYSTEM_READ: &[&str] = &["/usr", "/bin", "/sbin", "/System", "/Library", "/private/etc", "/dev", "/opt", "/Applications", "/private/var/db"];
/// Seatbelt rules for Metal: the GPU driver user clients and the shader compiler service.
const GPU_RULES: &str = "(allow iokit-open)\n(allow iokit-get-properties)\n(allow mach-lookup (global-name \"com.apple.MTLCompilerService\") (global-name-regex #\"^com\\.apple\\.(metal|gpu|iosurface|CARenderServer|cvmsServ)\"))\n(allow file-read* file-write* (regex #\"^/private/var/folders/[^/]+/[^/]+/C/com\\.apple\\.(metal|gpuarchiver)\"))\n";
/// Credential files no model-reachable read may open, even inside a read root (e.g. `~/.cargo/credentials.toml`): `read`,
/// `search` and `view` refuse them and the Seatbelt profile denies them to `sh`.
const SECRET_FILES: &[&str] = &["credentials", "credentials.toml", "secrets.toml", ".netrc", ".npmrc", ".pypirc", "id_rsa", "id_ed25519"];

/// Is `path` (by file name) one of the credential files in SECRET_FILES?
pub fn is_secret(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| SECRET_FILES.contains(&n))
}

const ENV_ALLOW: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER", "LOGNAME", "SHELL"];
const TOOLS: &[&str] = &["python3", "python", "pip3", "node", "npm", "npx", "git", "cargo", "rustc", "go", "uv", "ruby", "java", "make", "gcc", "clang", "deno", "bun"];

fn canon(p: impl AsRef<Path>) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok()
}

/// Lexically normalize an absolute path (resolve `.` and `..` without touching the filesystem).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

impl Policy {
    /// `enabled = false` is `--yolo`: no Seatbelt confinement, path roots or approval gate. Protected paths (`.harness/`,
    /// `hx.lock`, the trust store) stay read-only to the model, so a yolo run cannot rewrite config later runs apply.
    pub fn new(workspace: &Path, enabled: bool, approval_mode: Option<ApprovalMode>, resume: Option<&Path>, net: Option<bool>) -> Result<Policy> {
        let workspace = std::fs::canonicalize(workspace)?;
        if let Some(path) = resume {
            // Lock before reading any recovery state or repairing the log. Mutable workspace configuration is never
            // consulted for a resumed run: the exact authority approved at launch is part of its manifest.
            let log = EventLog::reopen(path)?;
            let manifest = crate::supervisor::Manifest::load(path)?;
            anyhow::ensure!(manifest.policy.workspace == workspace, "resume workspace differs from the saved run");
            let mut p = manifest.policy;
            validate_scratch(&p.tmp)?;
            std::fs::create_dir_all(&p.tmp)?;
            // A journal may have been moved since launch. Its actual files and artifacts remain model-read-only.
            let actual = std::fs::canonicalize(path)?;
            p.protected.extend([actual.clone(), actual.with_extension("lock"), actual.with_extension("revoked"),
                actual.parent().unwrap().join("blobs"), actual.parent().unwrap().join("registry")]);
            // Newly introduced aliases must not turn frozen protected paths into writable paths.
            let aliases: Vec<_> = p.protected.iter().filter_map(canon).collect();
            p.protected.extend(aliases);
            log.check()?;
            return Ok(Policy { cfg: p, log });
        }
        let modules_sha = crate::eventlog::sha_bytes(&serde_json::to_vec(crate::modules::EMBEDDED)?);
        let tmp = std::env::temp_dir().join(format!("hx-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos()));
        std::fs::create_dir_all(&tmp)?;
        let tmp = std::fs::canonicalize(tmp)?;
        let mut p = Snapshot { enabled, workspace: workspace.clone(), tmp: tmp.clone(), read_roots: vec![], write_roots: vec![], protected: vec![], net: false, gpu: false, ask: vec![], allow: vec![], mode: ApprovalMode::Deny, budget: Budget::default(), packs: vec![] };
        p.read_roots.extend([workspace.clone(), tmp.clone()]);
        p.write_roots.extend([workspace.clone(), tmp]);
        p.protected.extend([workspace.join(".harness"), workspace.join("hx.lock")]);
        for r in SYSTEM_READ {
            if let Some(c) = canon(r) {
                p.read_roots.push(c);
            }
        }
        p.read_roots.extend(toolchain_roots());
        // the trust store must never be writable by the model, even if the user's write roots cover ~/.config
        if let Some(d) = crate::trust::store_dir_canonical() {
            p.protected.push(d);
        }
        // check_write resolves aliases before comparing. Protect the resolved configuration too, including an
        // untrusted workspace's symlinked .harness or hx.lock and --yolo runs without root confinement.
        let canonical_protected: Vec<_> = p.protected.iter().filter_map(canon).collect();
        p.protected.extend(canonical_protected);
        let trusted = crate::trust::is_trusted(&workspace);
        if let Some(t) = std::fs::read_to_string(workspace.join(".harness/policy.toml")).ok().filter(|_| trusted) {
            let f: PolicyFile = toml::from_str(&t).map_err(|e| anyhow::anyhow!("bad .harness/policy.toml: {e}"))?;
            p.read_roots.extend(f.fs.read.iter().filter_map(canon));
            let w: Vec<PathBuf> = f.fs.write.iter().filter_map(canon).collect();
            p.read_roots.extend(w.clone());
            p.write_roots.extend(w);
            p.net = f.net.allow;
            p.gpu = f.gpu.allow;
            p.budget = f.budget;
            p.packs = f.packs.enable;
            p.ask = f.approval.ask;
            p.allow = f.approval.allow;
            if let Some(m) = f.approval.mode {
                p.mode = m;
            }
        }
        if let Some(m) = approval_mode {
            p.mode = m; // the launching user's choice beats the file
        }
        if let Some(n) = net {
            p.net = n; // so does --net / --no-net
        }
        // Approved skills may live outside the workspace (~/.agents/skills); their files must be readable (read-only).
        for pk in crate::modules::Resolver::new(workspace.clone()).list_packages() {
            if pk.kind == "skill" && !pk.dir.is_empty() {
                p.read_roots.push(PathBuf::from(pk.dir));
            }
        }
        // The audit log is independent of the sandbox: `--yolo` drops confinement, not the record of what ran.
        let log = EventLog::open(&workspace.join(".harness/log"))?;
        log.write("session", json!({"workspace": workspace.to_string_lossy(), "tmp": p.tmp.to_string_lossy(), "sandbox": enabled,
            "net": p.net, "approval_mode": p.mode, "config_sha": crate::trust::config_hash(&workspace), "trusted": trusted, "modules_sha": modules_sha}), true)?;
        p.read_roots.sort();
        p.read_roots.dedup();
        Ok(Policy { cfg: p, log })
    }

    pub fn snapshot(&self) -> Snapshot { self.cfg.clone() }

    /// Effect class of a shell command.
    pub fn classify_sh(&self, cmd: &str) -> Class {
        if IRREVERSIBLE.iter().any(|p| cmd.contains(p)) {
            Class::Irreversible
        } else if EXTERNAL.iter().any(|p| cmd.contains(p)) || self.ask.iter().any(|p| cmd.contains(p.as_str())) {
            Class::External
        } else {
            Class::LocalWrite
        }
    }

    /// The approval gate. Returns the approval source, or the reason it was refused. Lives here, in Rust: no Luau
    /// code, hook or file the model can write can approve anything (`policy.toml` is protected).
    pub async fn approve(&self, cmd: &str, class: Class) -> std::result::Result<&'static str, String> {
        if !self.enabled || matches!(class, Class::Read | Class::LocalWrite) {
            return Ok("not-required");
        }
        let preapproved = if class == Class::Irreversible { self.allow.iter().any(|a| a == cmd) } else { self.allow.iter().any(|a| cmd.contains(a.as_str())) };
        if preapproved {
            return Ok("policy-allow");
        }
        if self.mode == ApprovalMode::Prompt {
            let msg = format!("\n[approval needed] {} command:\n  {cmd}\napprove? [y/N] ", class.name());
            let yes = tokio::task::spawn_blocking(move || {
                use std::io::{BufRead, Write};
                let Ok(mut tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else { return false };
                let _ = tty.write_all(msg.as_bytes());
                let mut line = String::new();
                let _ = std::io::BufReader::new(&tty).read_line(&mut line);
                matches!(line.trim(), "y" | "Y" | "yes")
            })
            .await
            .unwrap_or(false);
            return if yes { Ok("prompt") } else { Err("the user declined".into()) };
        }
        Err(format!(
            "this is an {} effect and needs the user's approval; it was NOT run. Tell the user what you wanted to do instead of retrying (approvals are granted in .harness/policy.toml or interactively, never by the agent)",
            class.name()
        ))
    }

    fn within(path: &Path, roots: &[PathBuf]) -> bool {
        roots.iter().any(|r| path.starts_with(r))
    }

    /// Resolve `p` (relative to the workspace) for reading; the canonical path must be inside a read root.
    pub fn check_read(&self, p: &str) -> Result<PathBuf> {
        let full = self.abs(p);
        if !self.enabled {
            return Ok(full);
        }
        let c = std::fs::canonicalize(&full).map_err(|e| anyhow::anyhow!("{p}: {e}"))?;
        if !Self::within(&c, &self.read_roots) {
            bail!("policy: reading {p} is outside the allowed roots");
        }
        if is_secret(&c) {
            bail!("policy: {p} is a credential file and cannot be read");
        }
        Ok(c)
    }

    /// Resolve `p` for writing: lexically normalized, no symlink anywhere on the not-yet-existing tail or at the
    /// target, existing ancestor canonicalized, result inside a write root and outside every protected path.
    pub fn check_write(&self, p: &str) -> Result<PathBuf> {
        let full = normalize(&self.abs(p));
        let mut existing = full.clone();
        let mut tail: Vec<std::ffi::OsString> = vec![];
        while std::fs::symlink_metadata(&existing).is_err() {
            match (existing.file_name().map(|n| n.to_os_string()), existing.parent().map(|x| x.to_path_buf())) {
                (Some(n), Some(par)) => {
                    tail.push(n);
                    existing = par;
                }
                _ => bail!("policy: cannot resolve {p}"),
            }
        }
        if self.enabled && tail.is_empty() && std::fs::symlink_metadata(&existing)?.file_type().is_symlink() {
            bail!("policy: refusing to write through the symlink {p}");
        }
        let mut c = std::fs::canonicalize(&existing)?;
        for t in tail.iter().rev() {
            c.push(t);
        }
        if self.enabled && !Self::within(&c, &self.write_roots) {
            bail!("policy: writing {p} is outside the allowed roots");
        }
        if Self::within(&c, &self.protected) {
            bail!("policy: {p} is protected (harness configuration cannot be modified by the model)");
        }
        Ok(c)
    }

    pub fn abs(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() { p.to_path_buf() } else { self.workspace.join(p) }
    }

    /// Environment passed to `sh`: a small allowlist, HOME and TMPDIR pointing at the run's temp dir. No API keys.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = ENV_ALLOW.iter().filter_map(|k| std::env::var(k).ok().map(|x| (k.to_string(), x))).collect();
        let t = self.tmp.to_string_lossy().to_string();
        v.push(("HOME".into(), t.clone()));
        v.push(("TMPDIR".into(), t));
        // HOME is the temp dir, so rustup/cargo would not find the user's toolchain; point them at the real (readable) dirs.
        // Writes there (registry cache) still need `[fs] write` in policy.toml.
        let home = std::env::var("HOME").unwrap_or_default();
        for (var, dir) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
            let val = std::env::var(var).ok().or_else(|| canon(Path::new(&home).join(dir)).map(|p| p.to_string_lossy().to_string()));
            if let Some(val) = val {
                v.push((var.into(), val));
            }
        }
        v
    }

    /// The Seatbelt profile for `sh`. Later rules win, so protected-path denies come after the write allows.
    pub fn seatbelt_profile(&self) -> String {
        self.seatbelt_profile_with(self.net, &[])
    }

    /// `--yolo` profile: everything allowed except writes to protected paths. None where Seatbelt is unavailable.
    pub fn yolo_profile(&self) -> Option<String> {
        if !Path::new("/usr/bin/sandbox-exec").exists() {
            return None;
        }
        let q = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
        let mut s = String::from("(version 1)\n(allow default)\n");
        for p in &self.protected {
            s.push_str(&format!("(deny file-write* (subpath {}))\n", q(p)));
        }
        Some(s)
    }

    /// Same profile with an explicit network setting and extra writable roots (used for MCP server processes).
    pub fn seatbelt_profile_with(&self, net: bool, extra_write: &[PathBuf]) -> String {
        let q = |p: &Path| format!("\"{}\"", p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
        let mut s = String::from("(version 1)\n(deny default)\n(import \"system.sb\")\n");
        s.push_str("(allow process-exec*)\n(allow process-fork)\n(allow signal (target same-sandbox))\n(allow sysctl-read)\n(allow file-read-metadata)\n");
        for r in &self.read_roots {
            s.push_str(&format!("(allow file-read* (subpath {}))\n", q(r)));
        }
        s.push_str("(allow file-write* (literal \"/dev/null\") (literal \"/dev/zero\") (literal \"/dev/dtracehelper\") (regex #\"^/dev/tty\") (regex #\"^/dev/fd/\"))\n");
        // Apple's /usr/bin/git shim (xcrun) caches under the system user temp dir regardless of TMPDIR.
        s.push_str("(allow file-read* file-write* (regex #\"^/private/var/folders/[^/]+/[^/]+/T/xcrun_db\"))\n");
        for w in self.write_roots.iter().chain(extra_write) {
            s.push_str(&format!("(allow file-write* (subpath {}))\n", q(w)));
        }
        // `\.` in the profile: SBPL hands the #"..." string to the regex engine as is
        let names: Vec<String> = SECRET_FILES.iter().map(|n| n.replace('.', "\\.")).collect();
        s.push_str(&format!("(deny file-read* (regex #\"/({})$\"))\n", names.join("|")));
        for p in &self.protected {
            s.push_str(&format!("(deny file-write* (subpath {}))\n", q(p)));
        }
        if self.gpu {
            s.push_str(GPU_RULES);
        }
        if net {
            s.push_str("(allow network*)\n");
        } else {
            s.push_str("(deny network*)\n");
        }
        s
    }
}

/// A logged path is not authority to remove arbitrary directories on shutdown. Scratch paths must be direct,
/// non-symlink children of this user's system temp directory with the exact name generated by Policy::new.
fn validate_scratch(path: &Path) -> Result<()> {
    let base = std::fs::canonicalize(std::env::temp_dir())?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let valid_name = name.strip_prefix("hx-").and_then(|s| s.split_once('-')).is_some_and(|(pid, nanos)| {
        !pid.is_empty() && !nanos.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && nanos.bytes().all(|b| b.is_ascii_digit())
            && pid.parse::<u32>().is_ok_and(|p| p > 0) && nanos.parse::<u32>().is_ok_and(|n| n < 1_000_000_000)
    });
    anyhow::ensure!(valid_name && path.parent() == Some(base.as_path()), "invalid recorded scratch directory: {}", path.display());
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(meta.is_dir() && !meta.file_type().is_symlink(), "invalid recorded scratch directory: {}", path.display());
    }
    Ok(())
}

impl Drop for Policy {
    fn drop(&mut self) {
        let _ = self.log.write("session_end", json!({}), true);
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// Directories under $HOME that hold the user's toolchains (so `python3`, `git`, `cargo` work in the sandbox).
fn toolchain_roots() -> Vec<PathBuf> {
    let home = std::env::var("HOME").ok().and_then(canon);
    let mut out = vec![];
    let Some(home) = home else { return out };
    let path = std::env::var("PATH").unwrap_or_default();
    for t in TOOLS {
        for dir in std::env::split_paths(&path) {
            if let Some(real) = canon(dir.join(t)) {
                if real.starts_with(&home) {
                    // <root>/bin/tool -> <root>; otherwise the containing directory
                    let parent = real.parent().map(Path::to_path_buf).unwrap_or_default();
                    let root = if parent.file_name().is_some_and(|n| n == "bin") { parent.parent().map(Path::to_path_buf).unwrap_or(parent) } else { parent };
                    if root != home {
                        out.push(root);
                    }
                }
                break;
            }
        }
    }
    for d in [".cargo", ".rustup", ".local/share/uv", ".nvm", ".pyenv", ".rbenv"] {
        if let Some(c) = canon(home.join(d)) {
            out.push(c);
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_cannot_adopt_and_delete_an_unrelated_scratch_directory() {
        let dir = std::env::temp_dir().join(format!("hx-policy-scratch-test-{}", std::process::id()));
        let victim = dir.join("unrelated");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("sentinel"), "KEEP").unwrap();
        let log = EventLog::open(&dir.join("log")).unwrap();
        log.write("session", json!({"workspace": dir, "tmp": victim}), true).unwrap();
        let result = Policy::new(&dir, true, None, Some(&log.path), None);
        let accepted = result.is_ok();
        drop(result);
        assert!(victim.join("sentinel").exists(), "Policy::drop deleted unrelated data");
        assert!(!accepted, "an arbitrary logged path is not an owned scratch directory");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scratch_validation_rejects_symlinks_nested_and_invalid_names() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let scratch = base.join(format!("hx-{}-999999999", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(validate_scratch(&scratch).is_ok());
        for invalid in [scratch.join("child"), base.join("hx-0-1"), base.join("hx-1-1000000000"), base.join("hx-1-1-extra"), PathBuf::from("hx-1-1")] {
            assert!(validate_scratch(&invalid).is_err(), "{}", invalid.display());
        }
        std::fs::remove_dir(&scratch).unwrap();
        std::os::unix::fs::symlink(&base, &scratch).unwrap();
        assert!(validate_scratch(&scratch).is_err());
        std::fs::remove_file(scratch).unwrap();
    }

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(normalize(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(normalize(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[test]
    fn classify_sh_orders_irreversible_before_external() {
        let dir = std::env::temp_dir().join(format!("hx-policy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = Policy::new(&dir, true, None, None, None).unwrap();
        assert_eq!(p.classify_sh("git push --force origin main"), Class::Irreversible);
        assert_eq!(p.classify_sh("git push origin main"), Class::External);
        assert_eq!(p.classify_sh("ls -la"), Class::LocalWrite);
        // writes: inside the workspace ok, protected and escaping paths refused
        assert!(p.check_write("src/new.txt").is_ok());
        assert!(p.check_write(".harness/policy.toml").is_err());
        assert!(p.check_write("hx.lock").is_err());
        assert!(p.check_write("../escape.txt").is_err());
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gpu_rules_only_when_policy_allows() {
        let base = std::env::temp_dir().join(format!("hx-policy-gpu-{}", std::process::id()));
        let (off, on) = (base.join("off"), base.join("on"));
        std::fs::create_dir_all(&off).unwrap();
        std::fs::create_dir_all(on.join(".harness")).unwrap();
        std::fs::write(on.join(".harness/policy.toml"), "[gpu]\nallow = true\n").unwrap();
        // the launcher's consent to use workspace config (modules tests set the same value; no test needs it unset)
        unsafe { std::env::set_var("HX_TRUST", "1") };
        assert!(!Policy::new(&off, true, None, None, None).unwrap().seatbelt_profile().contains("iokit-open"));
        let prof = Policy::new(&on, true, None, None, None).unwrap().seatbelt_profile();
        assert!(prof.contains("(allow iokit-open)") && prof.contains("MTLCompilerService"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
