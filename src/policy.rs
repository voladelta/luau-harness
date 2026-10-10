//! Capability policy + macOS Seatbelt sandbox for model-reachable effects.
//!
//! Core guarantees (enforced here, in Rust, where no Luau code or hook can change them):
//!  * `sh` runs under Seatbelt: writes only to the workspace and a per-run temp dir, reads only from an allowlist,
//!    no network unless granted, a scrubbed environment, its own process group (killed on timeout), and a process cap.
//!  * `read`/`write`/`edit`/`search` check the *canonical* path against the same roots, never write through symlinks.
//!  * `.harness/` and `hx.lock` are protected: the model can read them but never write them (grants, overrides, init).
//! User-owned config: `<workspace>/.harness/policy.toml` (protected from the model):
//!   [fs]  read = ["/extra/read/root"]   write = ["/extra/write/root"]
//!   [net] allow = true                   # Seatbelt cannot filter by host; this is all-or-nothing
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
}
#[derive(Deserialize, Default)]
struct PacksSection {
    #[serde(default)]
    enable: Vec<String>,
}

/// Limits for one run, shared with every subagent. None = unlimited. Cost needs a provider that reports it
/// (OpenRouter, Enclave); tokens (input + cache writes + output) work everywhere.
#[derive(Deserialize, Default, Clone, Debug)]
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
    /// "deny" (default, headless) or "prompt" (ask on the terminal)
    mode: Option<String>,
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

pub struct Policy {
    pub enabled: bool,
    pub workspace: PathBuf,
    pub tmp: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
    pub net: bool,
    pub log: Arc<EventLog>,
    ask: Vec<String>,
    allow: Vec<String>,
    pub mode: String,
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
    /// `enabled = false` turns every check and the sandbox off (explicit opt-out, used for `--no-sandbox`).
    pub fn new(workspace: &Path, enabled: bool, approval_mode: Option<String>, resume: Option<&Path>, net: Option<bool>) -> Result<Policy> {
        let workspace = std::fs::canonicalize(workspace)?;
        let tmp = std::env::temp_dir().join(format!("hx-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos()));
        std::fs::create_dir_all(&tmp)?;
        let tmp = std::fs::canonicalize(tmp)?;
        let mut p = Policy { enabled, workspace: workspace.clone(), tmp: tmp.clone(), read_roots: vec![], write_roots: vec![], protected: vec![], net: false, log: EventLog::off(), ask: vec![], allow: vec![], mode: "deny".into(), budget: Budget::default(), packs: vec![] };
        p.read_roots.extend([workspace.clone(), tmp.clone()]);
        p.write_roots.extend([workspace.clone(), tmp]);
        p.protected.extend([workspace.join(".harness"), workspace.join("hx.lock")]);
        for r in SYSTEM_READ {
            if let Some(c) = canon(r) {
                p.read_roots.push(c);
            }
        }
        p.read_roots.extend(toolchain_roots());
        if let Ok(t) = std::fs::read_to_string(workspace.join(".harness/policy.toml")) {
            let f: PolicyFile = toml::from_str(&t).map_err(|e| anyhow::anyhow!("bad .harness/policy.toml: {e}"))?;
            p.read_roots.extend(f.fs.read.iter().filter_map(canon));
            let w: Vec<PathBuf> = f.fs.write.iter().filter_map(canon).collect();
            p.read_roots.extend(w.clone());
            p.write_roots.extend(w);
            p.net = f.net.allow;
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
        if enabled {
            match resume {
                Some(path) => {
                    p.log = EventLog::reopen(path)?;
                    p.log.write("resume", json!({"sandbox": enabled, "net": p.net, "approval_mode": p.mode}), true);
                }
                None => {
                    p.log = EventLog::open(&workspace.join(".harness/log"))?;
                    p.log.write("session", json!({"workspace": workspace.to_string_lossy(), "sandbox": enabled, "net": p.net, "approval_mode": p.mode}), true);
                }
            }
        }
        p.read_roots.sort();
        p.read_roots.dedup();
        Ok(p)
    }

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
        if self.mode == "prompt" {
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
        Ok(c)
    }

    /// Resolve `p` for writing: lexically normalized, no symlink anywhere on the not-yet-existing tail or at the
    /// target, existing ancestor canonicalized, result inside a write root and outside every protected path.
    pub fn check_write(&self, p: &str) -> Result<PathBuf> {
        let full = normalize(&self.abs(p));
        if !self.enabled {
            return Ok(full);
        }
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
        if tail.is_empty() && std::fs::symlink_metadata(&existing)?.file_type().is_symlink() {
            bail!("policy: refusing to write through the symlink {p}");
        }
        let mut c = std::fs::canonicalize(&existing)?;
        for t in tail.iter().rev() {
            c.push(t);
        }
        if !Self::within(&c, &self.write_roots) {
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
        v
    }

    /// The Seatbelt profile for `sh`. Later rules win, so protected-path denies come after the write allows.
    pub fn seatbelt_profile(&self) -> String {
        self.seatbelt_profile_with(self.net, &[])
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
        s.push_str("(deny file-read* (regex #\"/(credentials(\\\\.toml)?|\\\\.netrc|id_rsa|id_ed25519|\\\\.npmrc|\\\\.pypirc)$\"))\n");
        for p in &self.protected {
            s.push_str(&format!("(deny file-write* (subpath {}))\n", q(p)));
        }
        if net {
            s.push_str("(allow network*)\n");
        } else {
            s.push_str("(deny network*)\n");
        }
        s
    }
}

impl Drop for Policy {
    fn drop(&mut self) {
        self.log.write("session_end", json!({}), true);
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
