//! Native primitives: read, write, edit, sh and search. Every effect goes through
//! `Effects` and every path through `Policy`; shell commands are confined to the policy's roots where Landlock exists.
use crate::effects::{rt, Effects, Ret};
use crate::policy::Class;
use crate::supervisor::Tool;
use anyhow::Result;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use ignore::{overrides::OverrideBuilder, WalkBuilder};
use mlua::{Lua, Table, Value};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const MAX_READ: u64 = 10 * 1024 * 1024;
const MAX_CAPTURE: usize = 1024 * 1024;
const DEFAULT_HITS: usize = 200;
const SH_TIMEOUT: f64 = 60.0;
const SH_TIMEOUT_MAX: f64 = 600.0;

/// The limits the system prompt states, by placeholder name (`{{MAX_READ_MB}}` in prompts/system.md). std/prompt fills
/// them in, so the prose the model reads comes from the constants that enforce it.
pub fn prompt_limits() -> serde_json::Value {
    serde_json::json!({
        "MAX_READ_MB": MAX_READ >> 20, "MAX_CAPTURE_MB": MAX_CAPTURE >> 20, "CAPTURE_HALF_KB": (MAX_CAPTURE / 2) >> 10, "DEFAULT_HITS": DEFAULT_HITS, "SH_TIMEOUT": SH_TIMEOUT, "SH_TIMEOUT_MAX": SH_TIMEOUT_MAX,
        "CHUNK_SECS": crate::vm::CHUNK_SECS, "VIEW_MAX": crate::vm::VIEW_MAX.0, "VIEW_MAX_MIN": crate::vm::VIEW_MAX.1, "VIEW_MAX_LIMIT": crate::vm::VIEW_MAX.2, "HISTORY_ENTRIES": crate::history::DEFAULT_LIMIT, "HISTORY_BYTES": crate::history::MAX_TEXT, "DATA_BYTES": crate::values::MAX_BYTES,
    })
}


/// Run blocking file work off the async worker threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> mlua::Result<T> + Send + 'static) -> mlua::Result<T> {
    tokio::task::spawn_blocking(f).await.map_err(mlua::Error::external)?
}

/// Replace a file's content all at once (temp file, fsync, rename), after journaling the content's hash. A crash leaves
/// the old or the new file, never a torn one, so resume can tell which from the disk (`supervisor::reconcile`).
fn replace_file(sup: &crate::supervisor::Supervisor, intent: Option<u64>, p: &Path, target: &crate::atomic_file::Target, bytes: &[u8]) -> Result<()> {
    sup.replace_file(intent, p, target, bytes)
}

/// Open a policy-checked canonical path for reading: every directory from `/` down is opened without following a
/// link, so a directory swapped for a symlink after the check fails the read instead of redirecting it.
fn read_checked(p: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    crate::atomic_file::Target::open(p, false)?.read(max)
}

pub fn opt_str(o: &Option<Table>, k: &str) -> Option<String> {
    o.as_ref().and_then(|t| t.get::<Option<String>>(k).ok().flatten())
}
pub fn bad(r: Ret) -> mlua::Error {
    rt(format!("unexpected native result {r:?}"))
}

/// Parse a primitive's options (`nil` for the defaults) into `T`. The table is copied out as plain data first (raw
/// access, no metamethods), so options are read once, before the effect starts, and an invalid one is an error.
pub fn options<T: DeserializeOwned + Default>(name: &str, value: Value) -> mlua::Result<T> {
    if value.is_nil() {
        return Ok(T::default());
    }
    let json = crate::values::copy_out(value).map_err(|e| rt(format!("{name}: options: {e}")))?;
    serde_json::from_value(json).map_err(|e| rt(format!("{name}: options: {e}")))
}

/// Copy a structured native result into Lua data.
pub fn json_value(lua: &Lua, r: Ret) -> mlua::Result<Value> {
    match r {
        Ret::Json(v) => crate::values::copy_in(lua, &v),
        r => Err(bad(r)),
    }
}

/// `read(path, opts)` options: `offset` and `limit` count lines, `offset` from 1.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ReadOptions {
    pub offset: Option<f64>,
    pub limit: Option<f64>,
}

/// Read a file's bytes exactly as stored (or the requested lines of it), under the read policy.
pub async fn read(fx: &Effects, lua: &Lua, path: String, opts: ReadOptions) -> mlua::Result<Vec<u8>> {
    let args = json!({"path": &path, "offset": opts.offset, "limit": opts.limit});
    let pol = fx.pol.clone();
    match fx.run(lua, Tool::Read, Class::Read, args, || blocking(move || read_file(&pol, &path, &opts).map(Ret::Bytes))).await? {
        Ret::Bytes(bytes) => Ok(bytes),
        r => Err(bad(r)),
    }
}

fn read_file(pol: &crate::policy::Policy, path: &str, opts: &ReadOptions) -> mlua::Result<Vec<u8>> {
    let p = pol.check_read(path).map_err(|e| rt(format!("read {path}: {e}")))?;
    let bytes = read_checked(&p, MAX_READ).map_err(|e| match e.kind() {
        std::io::ErrorKind::IsADirectory => rt(format!("read {path}: is a directory (use sh('ls') or search)")),
        _ => rt(format!("read {path}: {e}")),
    })?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Err(rt(format!("read {path}: binary file")));
    }
    if opts.offset.is_none() && opts.limit.is_none() {
        return Ok(bytes);
    }
    let start = opts.offset.unwrap_or(1.0).max(1.0) as usize - 1;
    let take = opts.limit.map(|l| l.max(0.0) as usize).unwrap_or(usize::MAX);
    Ok(bytes.split_inclusive(|b| *b == b'\n').skip(start).take(take).flatten().copied().collect())
}

/// `edit(path, old, new, opts)` options.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct EditOptions {
    all: bool,
}

/// Lines in `text` as `str::lines` counts them: a final line without a newline counts, an empty tail does not.
fn line_count(text: &[u8]) -> usize {
    text.split(|b| *b == b'\n').count() - usize::from(text.is_empty() || text.ends_with(b"\n"))
}

/// Replace `old` with `new` in `text`, byte for byte. Unless `all`, `old` must occur exactly once, counting
/// overlapping occurrences: in `"aaa"`, `"aa"` occurs twice. -> (new text, replacements)
fn replace_bytes(path: &str, text: &[u8], old: &[u8], new: &[u8], all: bool) -> mlua::Result<(Vec<u8>, usize)> {
    let finder = memchr::memmem::Finder::new(old);
    let hits: Vec<usize> = finder.find_iter(text).collect();
    let Some(&first) = hits.first() else {
        return Err(rt(format!("edit_not_found: text not found in {path}; read the file and copy the exact text (whitespace matters)")));
    };
    if !all && (hits.len() > 1 || finder.find(&text[first + 1..]).is_some()) {
        let times = if hits.len() > 1 { format!("{} times", hits.len()) } else { "twice (overlapping)".into() };
        return Err(rt(format!("edit_not_unique: text matches {times} in {path}; include more surrounding lines or pass {{all=true}}")));
    }
    let mut out = Vec::with_capacity(text.len() + hits.len() * new.len());
    let mut from = 0;
    for &at in &hits {
        out.extend_from_slice(&text[from..at]);
        out.extend_from_slice(new);
        from = at + old.len();
    }
    out.extend_from_slice(&text[from..]);
    Ok((out, hits.len()))
}

/// The search options of `search(pattern, opts)`.
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct SearchOptions {
    pub path: Option<String>,
    pub glob: Option<String>,
    pub max: Option<f64>,
    pub ignore_case: bool,
    pub literal: bool,
}

pub type Rows = Vec<(String, i64, String)>;

/// A search the pattern or glob made impossible; the prelude raises it as a `{kind, message, hint}` table.
pub struct SearchError {
    pub kind: String,
    pub message: String,
    pub hint: String,
}

impl SearchError {
    fn new(kind: &str, message: impl Into<String>, hint: &str) -> Self {
        Self { kind: kind.into(), message: message.into(), hint: hint.into() }
    }
}

/// The matching lines; `truncated` when a match beyond `max` exists and was left out.
pub struct Hits {
    pub rows: Rows,
    pub truncated: bool,
}

/// Search files under the read policy (`search(pattern, opts)`).
pub async fn search(fx: &Effects, lua: &Lua, pattern: String, opts: SearchOptions) -> mlua::Result<Result<Hits, SearchError>> {
    let base = opts.path.clone().unwrap_or_else(|| ".".into());
    let args = json!({"pattern": &pattern, "path": &base, "glob": opts.glob, "max": opts.max, "ignore_case": opts.ignore_case, "literal": opts.literal});
    let (pol, workflow) = (fx.pol.clone(), fx.supervisor.workflow_epoch().is_some());
    let ret = fx.run(lua, Tool::Search, Class::Read, args, || blocking(move || {
        let root = pol.check_read(&base).map_err(|e| rt(format!("search {base}: {e}")))?;
        Ok(match search_files(&pol.workspace, &root, &pattern, &opts, workflow, &pol.unreadable()) {
            Ok(Hits { rows, truncated }) => Ret::Rows { rows, truncated },
            Err(SearchError { kind, message, hint }) => Ret::SearchErr { kind, message, hint },
        })
    })).await?;
    match ret {
        Ret::Rows { rows, truncated } => Ok(Ok(Hits { rows, truncated })),
        Ret::SearchErr { kind, message, hint } => Ok(Err(SearchError { kind, message, hint })),
        r => Err(bad(r)),
    }
}

/// Search rows as a Lua array of `{file, line, text}`.
pub fn rows_table(lua: &Lua, rows: &[(String, i64, String)]) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(rows.len(), 0)?;
    for (file, line, text) in rows {
        let row = lua.create_table_with_capacity(0, 3)?;
        row.raw_set("file", file.as_str())?;
        row.raw_set("line", *line)?;
        row.raw_set("text", text.as_str())?;
        t.raw_push(row)?;
    }
    Ok(t)
}

/// What a subagent returned: `agent(...)` and a batch `agent` operation give the model this as `{answer, finished, turns}`.
pub struct AgentReply {
    pub answer: String,
    pub finished: bool,
    pub turns: u64,
}

impl AgentReply {
    pub fn into_lua(self, lua: &Lua) -> mlua::Result<Table> {
        let t = lua.create_table_with_capacity(0, 3)?;
        t.raw_set("answer", self.answer)?;
        t.raw_set("finished", self.finished)?;
        t.raw_set("turns", self.turns)?;
        Ok(t)
    }
}

/// Run `task` in `child` (made by `Host::child` with the principal it may have). Spawning a subagent changes nothing
/// by itself, so it records a call receipt, not an intent: a crash mid-agent leaves nothing for the owner to settle.
/// Each effect of the child has its own intent and receipt and goes through admission itself.
pub async fn agent(fx: &Effects, lua: &Lua, child: Arc<crate::host::Host>, task: String, options: &crate::host::AgentOptions) -> mlua::Result<AgentReply> {
    let args = json!({"task_sha": crate::eventlog::sha_bytes(task.as_bytes()), "preview": task.chars().take(120).collect::<String>(), "max_turns": options.max_turns, "model": options.model, "effort": options.effort});
    let ret = fx.run(lua, Tool::Agent, Class::Read, args, || async {
        child.log(&format!("### SUBAGENT (depth {}, model {}, effort {})\n{task}", child.depth, child.llm.model, child.llm.reasoning.as_deref().unwrap_or("default")));
        let rep = child.clone().run_task(task.clone(), options.max_turns).await.map_err(|e| rt(e.to_string()))?;
        Ok(Ret::Agent { answer: rep["final_answer"].as_str().unwrap_or("").to_string(), finished: rep["finished"].as_bool().unwrap_or(false), turns: rep["turns"].as_u64().unwrap_or(0) })
    }).await?;
    match ret {
        Ret::Agent { answer, finished, turns } => Ok(AgentReply { answer, finished, turns }),
        r => Err(bad(r)),
    }
}

pub fn install(lua: &Lua, fx: Arc<Effects>, ops: Arc<AtomicUsize>) -> Result<()> {
    let g = lua.globals();
    let pol = fx.pol.clone();

    // tmpdir() -> the run's private scratch directory (writable; the only writable place besides the project).
    let tmp = pol.tmp.to_string_lossy().to_string();
    g.set("tmpdir", lua.create_function(move |_, ()| Ok(tmp.clone()))?)?;

    // read(path, {offset=1, limit=n}) -> string. offset/limit are in lines.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("read", lua.create_async_function(move |lua, (path, opts): (String, Value)| {
        let (f, o) = (f.clone(), o.clone());
        let opts = options::<ReadOptions>("read", opts);
        async move {
            o.fetch_add(1, Ordering::Relaxed);
            lua.create_string(read(&f, &lua, path, opts?).await?)
        }
    })?)?;

    // write(path, content) -> bytes written. Creates parent directories.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("write", lua.create_async_function(move |lua, (path, content): (String, mlua::LuaString)| {
        let (f, o) = (f.clone(), o.clone());
        let bytes = content.as_bytes().to_vec();
        async move {
            o.fetch_add(1, Ordering::Relaxed);
            let args = json!({"path": &path, "bytes": bytes.len(), "sha256": crate::eventlog::sha_bytes(&bytes)});
            let (pol, sup) = (f.pol.clone(), f.supervisor.clone());
            let len = bytes.len();
            match f.run_with_intent(&lua, Tool::Write, Class::LocalWrite, args, |intent| blocking(move || {
                let p = pol.check_write(&path).map_err(|e| rt(format!("write {path}: {e}")))?;
                let target = crate::atomic_file::Target::open(&p, true).map_err(mlua::Error::external)?;
                replace_file(&sup, intent, &p, &target, &bytes).map_err(crate::effects::effect_error)?;
                Ok(Ret::Num(len as f64))
            })).await? {
                Ret::Num(n) => Ok(n as i64),
                r => Err(bad(r)),
            }
        }
    })?)?;

    // edit(path, old, new, {all=false}) -> "ok: N replacement(s), +A/-B lines". `old` must match exactly once unless all=true.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("edit", lua.create_async_function(move |lua, (path, old, new, opts): (String, mlua::LuaString, mlua::LuaString, Value)| {
        let (f, o) = (f.clone(), o.clone());
        let (old, new) = (old.as_bytes().to_vec(), new.as_bytes().to_vec());
        let opts = options::<EditOptions>("edit", opts);
        async move {
            o.fetch_add(1, Ordering::Relaxed);
            let all = opts?.all;
            if old.is_empty() {
                return Err(rt(format!("edit {path}: `old` is empty")));
            }
            let args = json!({"path": &path, "old_sha": crate::eventlog::sha_bytes(&old), "new_sha": crate::eventlog::sha_bytes(&new), "old_bytes": old.len(), "new_bytes": new.len(), "all": all});
            let (pol, sup) = (f.pol.clone(), f.supervisor.clone());
            match f.run_with_intent(&lua, Tool::Edit, Class::LocalWrite, args, |intent| blocking(move || {
                let p = pol.check_write(&path).map_err(|e| rt(format!("edit {path}: {e}")))?;
                // edit reads the file, and whether `old` matched would reveal its content
                if crate::policy::is_secret(&p) {
                    return Err(rt(format!("edit {path}: policy: a credential file cannot be read")));
                }
                let target = crate::atomic_file::Target::open(&p, false).map_err(|e| rt(format!("edit {path}: {e}")))?;
                let text = target.read(MAX_READ).map_err(|e| rt(format!("edit {path}: {e}")))?;
                let (edited, n) = replace_bytes(&path, &text, &old, &new, all)?;
                replace_file(&sup, intent, &p, &target, &edited).map_err(crate::effects::effect_error)?;
                let (a, b) = (line_count(&new) * n, line_count(&old) * n);
                Ok(Ret::Str(format!("ok: {n} replacement{}, +{a}/-{b} lines", if n == 1 { "" } else { "s" })))
            })).await? {
                Ret::Str(s) => Ok(s),
                r => Err(bad(r)),
            }
        }
    })?)?;

    // sh(cmd, {timeout=seconds, cwd=dir}) -> {stdout, stderr, code, timed_out}
    let (f, o) = (fx.clone(), ops.clone());
    g.set("sh", lua.create_async_function(move |lua, (cmd, opts): (String, Option<Table>)| {
        let (f, o) = (f.clone(), o.clone());
        async move {
            o.fetch_add(1, Ordering::Relaxed);
            let opts = ShOpts::parse(opts)?;
            let pol = f.pol.clone();
            let class = pol.classify_sh(&cmd);
            let args = json!({"cmd": &cmd, "cwd": &opts.cwd, "timeout": opts.timeout});
            let ret = f.run(&lua, crate::supervisor::Tool::Sh, class, args, || async {
                let approval = pol.approve(&cmd, class).await.map_err(|why| rt(format!("approval_required: {why}")))?;
                let cwd = match &opts.cwd {
                    Some(c) => pol.check_read(c).map_err(|e| rt(format!("sh: cwd: {e}")))?,
                    None => pol.workspace.clone(),
                };
                let (so, se, status, timed_out, truncated) = shell(&ShellEnv::agent(&pol)?, &cmd, cwd, opts.secs()).await?;
                Ok(Ret::Sh { stdout: so, stderr: se, code: status, timed_out, truncated, operation_id:None, approval })
            }).await?;
            match ret {
                Ret::Sh { stdout, stderr, code, timed_out, truncated, operation_id, .. } => {
                    let t = lua.create_table()?;
                    t.set("stdout", lua.create_string(&stdout)?)?;
                    t.set("stderr", lua.create_string(&stderr)?)?;
                    t.set("code", code.unwrap_or(-1))?;
                    t.set("timed_out", timed_out)?;
                    t.set("truncated", truncated)?;
                    t.set("operation_id", operation_id)?;
                    Ok(t)
                }
                r => Err(bad(r)),
            }
        }
    })?)?;

    // __search_raw(pattern, opts) -> rows, truncated | nil, nil, {kind, message, hint}; the prelude's `search` raises the
    // error table.
    let (f, o) = (fx.clone(), ops);
    g.set("__search_raw", lua.create_async_function(move |lua, (pattern, opts): (String, Value)| {
        let (f, o) = (f.clone(), o.clone());
        let opts = options::<SearchOptions>("search", opts);
        async move {
            o.fetch_add(1, Ordering::Relaxed);
            match search(&f, &lua, pattern, opts?).await? {
                Ok(Hits { rows, truncated }) => Ok((Value::Table(rows_table(&lua, &rows)?), Value::Boolean(truncated), Value::Nil)),
                Err(SearchError { kind, message, hint }) => {
                    let e = lua.create_table_with_capacity(0, 3)?;
                    e.raw_set("kind", kind)?;
                    e.raw_set("message", message)?;
                    e.raw_set("hint", hint)?;
                    Ok((Value::Nil, Value::Nil, Value::Table(e)))
                }
            }
        }
    })?)?;
    Ok(())
}

/// Directories no search descends into unless the search starts inside them: Git's object store and the harness's
/// own state.
const INTERNAL_DIRS: [&str; 2] = [".git", ".harness"];

struct Collect<'a> {
    rows: &'a mut Rows,
    file: String,
    cap: usize,
    /// A match past `cap` was seen (and not collected).
    over: bool,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _s: &grep::searcher::Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.rows.len() >= self.cap {
            self.over = true;
            return Ok(false);
        }
        let text = String::from_utf8_lossy(m.bytes()).trim_end().to_string();
        let text: String = if text.chars().count() > 300 { text.chars().take(300).collect::<String>() + "…" } else { text };
        self.rows.push((self.file.clone(), m.line_number().unwrap_or(0) as i64, text));
        Ok(true)
    }
}

/// Search the files under `base` (canonical and policy-checked) for `pattern`, gitignore-aware, skipping binary
/// files, credential files, sealed paths and `INTERNAL_DIRS`. Rows name files relative to `root`.
fn search_files(root: &Path, base: &Path, pat: &str, opts: &SearchOptions, workflow: bool, sealed: &[PathBuf]) -> Result<Hits, SearchError> {
    if !opts.literal {
        for tok in ["(?=", "(?!", "(?<=", "(?<!"] {
            if pat.contains(tok) {
                return Err(SearchError::new("unsupported_regex",
                    format!("lookaround `{tok}` is not supported (Rust regex engine)"),
                    "search for the fixed part and filter the rows in Luau, or use two searches"));
            }
        }
        let b = pat.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            if b[i] == b'\\' {
                if (b'1'..=b'9').contains(&b[i + 1]) {
                    return Err(SearchError::new("unsupported_regex", "backreferences are not supported",
                        "match each part separately and compare in Luau"));
                }
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(opts.ignore_case)
        .fixed_strings(opts.literal)
        .build(pat)
        .map_err(|e| SearchError::new("invalid_regex", e.to_string(), "escape special characters or pass literal=true"))?;
    let cap = opts.max.map(|m| m.max(1.0) as usize).unwrap_or(DEFAULT_HITS);
    let mut wb = WalkBuilder::new(base);
    wb.hidden(false).follow_links(false).require_git(false).sort_by_file_path(|a, b| a.cmp(b));
    if workflow {
        wb.parents(false);
    }
    let (project, explicit) = (root.to_path_buf(), base.strip_prefix(root).unwrap_or(base).to_path_buf());
    wb.filter_entry(move |entry| {
        if entry.depth() > 0 && INTERNAL_DIRS.iter().any(|d| entry.file_name() == *d) {
            return false;
        }
        // In a workflow, run artifacts are searched only when asked for by path.
        let relative = entry.path().strip_prefix(&project).unwrap_or(entry.path());
        !workflow || [".harness/log", ".agents/artifacts"].iter().all(|runtime| explicit.starts_with(runtime) || !relative.starts_with(runtime))
    });
    if let Some(glob) = &opts.glob {
        let hint = "use gitignore-style globs like *.rs or src/**/*.ts";
        let mut ob = OverrideBuilder::new(base);
        ob.add(glob).map_err(|e| SearchError::new("invalid_glob", e.to_string(), hint))?;
        wb.overrides(ob.build().map_err(|e| SearchError::new("invalid_glob", e.to_string(), hint))?);
    }
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut rows: Rows = Vec::new();
    for entry in wb.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if crate::policy::is_secret(path) || sealed.iter().any(|s| path.starts_with(s)) {
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string();
        let mut sink = Collect { rows: &mut rows, file: rel, cap, over: false };
        // An unreadable file is skipped like a binary one.
        let _ = searcher.search_path(&matcher, path, &mut sink);
        if sink.over {
            return Ok(Hits { rows, truncated: true });
        }
    }
    Ok(Hits { rows, truncated: false })
}

/// `sh`'s options, read once when the call is made. Raw access runs no metamethod, so reading them can neither loop
/// nor run model code, and nothing touches a Lua value once the command is under way.
#[derive(Debug, Default, PartialEq)]
struct ShOpts {
    /// Seconds, as requested (finite); `secs` applies the default and the bounds.
    timeout: Option<f64>,
    cwd: Option<String>,
}

impl ShOpts {
    /// An unknown key or a value of the wrong type is an error, so a typo or a bad value never silently becomes the
    /// default.
    fn parse(opts: Option<Table>) -> mlua::Result<ShOpts> {
        let mut parsed = ShOpts::default();
        let Some(opts) = opts else { return Ok(parsed) };
        for pair in opts.pairs::<Value, Value>() {
            let (key, value) = pair?;
            let Value::String(name) = &key else {
                return Err(rt(format!("sh: option keys are names (timeout, cwd), got a {}", key.type_name())));
            };
            match (&*name.to_string_lossy(), value) {
                ("timeout", Value::Number(n)) if n.is_finite() => parsed.timeout = Some(n),
                ("timeout", Value::Integer(n)) => parsed.timeout = Some(n as f64),
                ("timeout", Value::Number(n)) => return Err(rt(format!("sh: timeout must be a finite number of seconds, got {n}"))),
                ("timeout", v) => return Err(rt(format!("sh: timeout must be a number of seconds, got a {}", v.type_name()))),
                ("cwd", Value::String(s)) => parsed.cwd = Some(s.to_str()?.to_owned()),
                ("cwd", v) => return Err(rt(format!("sh: cwd must be a string, got a {}", v.type_name()))),
                (other, _) => return Err(rt(format!("sh: unknown option `{other}` (options: timeout, cwd)"))),
            }
        }
        Ok(parsed)
    }

    /// The command's time limit in seconds: the default when none was given, clamped to [1, SH_TIMEOUT_MAX].
    fn secs(&self) -> f64 {
        self.timeout.unwrap_or(SH_TIMEOUT).clamp(1.0, SH_TIMEOUT_MAX)
    }
}

/// A shell command's environment (None: inherited) and its kernel confinement (None: unconfined).
pub struct ShellEnv {
    pub env: Option<Vec<(String, String)>>,
    pub confine: Option<Arc<crate::confine::Ruleset>>,
}
impl ShellEnv {
    /// The agent's `sh`: scrub credentials by default and in every autoresearch episode, and confine it to the run's
    /// current file policy (grants approved so far included).
    pub fn agent(pol: &crate::policy::Policy) -> mlua::Result<ShellEnv> {
        let confine = pol.confinement().prepare().map_err(|e| rt(format!("sh: cannot prepare confinement: {e}")))?;
        Ok(ShellEnv { env: Some(pol.env()), confine: confine.map(Arc::new) })
    }
}

/// Make `command` start its child in a new session, so the child leads its own process group and `GroupKill` can take
/// down everything it starts (except processes that leave the group themselves), and, when `ruleset` is given, under
/// that Landlock ruleset. The ruleset is applied last, after `setsid`.
pub(crate) fn confine_command(command: &mut tokio::process::Command, ruleset: Option<Arc<crate::confine::Ruleset>>) {
    // SAFETY: the closure runs in the forked child before exec, where only async-signal-safe calls are allowed. It
    // calls setsid(2) and `Ruleset::apply` (prctl(2) and landlock_restrict_self(2)), and on failure reads errno into
    // an `io::Error`, which does not allocate. It takes no lock and does not allocate: the Arc was moved in by the
    // parent and is only borrowed here.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(ruleset) = &ruleset {
                ruleset.apply()?;
            }
            Ok(())
        });
    }
}

/// How long the output readers keep reading after the command ends (and its group is killed) before they give up on
/// a pipe that a process outside the group still holds open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Run `cmd` with bash: own process group, Landlock ruleset when given, capped output, killed at `secs`.
/// -> (stdout, stderr, exit code, timed out, either stream cut to its first and last MAX_CAPTURE / 2 bytes)
///
/// The exit code is the command's status, or 128 + the signal number when a signal killed it (bash's convention);
/// None when it timed out. `secs` must be a finite, non-negative number; anything else is an error and nothing runs.
pub async fn shell(env: &ShellEnv, cmd: &str, cwd: PathBuf, secs: f64) -> mlua::Result<(Vec<u8>, Vec<u8>, Option<i64>, bool, bool)> {
    let limit = Duration::try_from_secs_f64(secs).map_err(|e| rt(format!("sh: timeout {secs}: {e}")))?;
    let mut command = tokio::process::Command::new("bash");
    command.arg("-c").arg(cmd);
    if let Some(vars) = &env.env { command.env_clear().envs(vars.iter().cloned()); }
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    confine_command(&mut command, env.confine.clone());
    let mut child = command.spawn().map_err(|e| rt(format!("sh: spawn failed: {e}")))?;
    let group = GroupKill::new(&child); // killpg on drop: also covers cancellation
    let out = child.stdout.take().expect("stdout is piped");
    let err = child.stderr.take().expect("stderr is piped");
    /// The whole stream, or its first and last MAX_CAPTURE / 2 bytes around a marker (the end is where a failing
    /// test or build says why); reads to EOF so the child never blocks on a full pipe, or until the cutoff once one
    /// is set, keeping what it has read.
    async fn drain<R: tokio::io::AsyncRead + Unpin>(mut s: R, mut cutoff: tokio::sync::watch::Receiver<Option<tokio::time::Instant>>) -> (Vec<u8>, bool) {
        let half = MAX_CAPTURE / 2;
        let (mut head, mut tail, mut total) = (Vec::new(), Vec::new(), 0usize);
        let mut chunk = vec![0u8; 64 * 1024];
        let stop = async move {
            // The sender lives until it has set the cutoff, so a closed channel means the caller is gone.
            match cutoff.wait_for(Option::is_some).await.ok().and_then(|at| *at) {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(stop);
        loop {
            let n = tokio::select! {
                read = s.read(&mut chunk) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                },
                () = &mut stop => break,
            };
            total += n;
            let take = n.min(half - head.len());
            head.extend_from_slice(&chunk[..take]);
            tail.extend_from_slice(&chunk[take..n]);
            if tail.len() > 2 * half { tail.drain(..tail.len() - half); }
        }
        if total <= MAX_CAPTURE { head.append(&mut tail); return (head, false) }
        let omitted = total - head.len() - half;
        head.extend_from_slice(format!("\n…[hx: {omitted} bytes omitted from the middle of this output]…\n").as_bytes());
        head.extend_from_slice(&tail[tail.len() - half..]);
        (head, true)
    }
    let (cutoff, until) = tokio::sync::watch::channel(None);
    let wait = async move {
        let status = tokio::time::timeout(limit, child.wait()).await;
        // The command is over: kill stragglers and background jobs now. That also closes the pipes they held, so the
        // readers finish; a process that left the group may still hold one, so the readers stop at a shared cutoff.
        drop(group);
        cutoff.send_replace(Some(tokio::time::Instant::now() + DRAIN_GRACE));
        status
    };
    let (status, (so, cut_out), (se, cut_err)) = tokio::join!(wait, drain(out, until.clone()), drain(err, until));
    let (code, timed_out) = match status {
        Ok(status) => (Some(exit_code(status.map_err(|e| rt(format!("sh: waiting for the command failed: {e}")))?)), false),
        Err(_) => (None, true),
    };
    Ok((so, se, code, timed_out, cut_out || cut_err))
}

/// A reaped child's exit status as a shell reports it: the exit code, or 128 + the signal that killed it.
fn exit_code(status: std::process::ExitStatus) -> i64 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => i64::from(code),
        (None, Some(signal)) => 128 + i64::from(signal),
        (None, None) => unreachable!("a reaped child either exited or was killed by a signal"),
    }
}

/// Kills a child's whole process group with SIGKILL when dropped (which also covers a cancelled future), and until
/// then lists the group so `stop` kills it when hx is told to stop.
///
/// Contract: the child was started through `confine_command`, so it leads its own process group. Create the guard
/// right after spawning and drop it before reaping the child or promptly after: once the leader is reaped and every
/// member has exited, the kernel may reuse the group id. A process that leaves the group (`setsid`, `setpgid`) is
/// not killed.
pub(crate) struct GroupKill(i32);

impl GroupKill {
    /// None when the child has already been reaped (it has no id any more).
    pub(crate) fn new(child: &tokio::process::Child) -> Option<Self> {
        let group = i32::try_from(child.id()?).expect("a pid fits in pid_t");
        GROUPS.lock().unwrap().insert(group);
        Some(GroupKill(group))
    }
}

impl Drop for GroupKill {
    fn drop(&mut self) {
        GROUPS.lock().unwrap().remove(&self.0);
        // SAFETY: killpg(2) takes plain integers and has no memory-safety preconditions.
        unsafe { libc::killpg(self.0, libc::SIGKILL) };
    }
}

/// The process groups of this process's running commands and the child runs it started. A process that is told to stop
/// runs no destructors, so `stop` takes them down itself; otherwise they would outlive it.
static GROUPS: std::sync::Mutex<std::collections::BTreeSet<i32>> = std::sync::Mutex::new(std::collections::BTreeSet::new());
static CHILDREN: std::sync::Mutex<std::collections::BTreeSet<i32>> = std::sync::Mutex::new(std::collections::BTreeSet::new());

/// A child run, asked to stop (and so to stop its own commands) when this process stops.
pub struct Child(i32);

impl Child {
    pub fn track(pid: u32) -> Self {
        CHILDREN.lock().unwrap().insert(pid as i32);
        Child(pid as i32)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        CHILDREN.lock().unwrap().remove(&self.0);
    }
}

/// Kill every running command's process group, ask every child run to stop, and exit.
pub fn stop(code: i32) -> ! {
    for group in GROUPS.lock().unwrap().iter() { unsafe { libc::killpg(*group, libc::SIGKILL) }; }
    for child in CHILDREN.lock().unwrap().iter() { unsafe { libc::kill(*child, libc::SIGTERM) }; }
    std::process::exit(code)
}

/// Stop on SIGINT, SIGTERM or SIGHUP, taking every process this one started down with it.
pub fn stop_on_signals() {
    use tokio::signal::unix::{SignalKind, signal};
    let watch = |kind: SignalKind, code: i32| {
        if let Ok(mut s) = signal(kind) {
            tokio::spawn(async move { if s.recv().await.is_some() { stop(code) } });
        }
    };
    watch(SignalKind::interrupt(), 130);
    watch(SignalKind::terminate(), 143);
    watch(SignalKind::hangup(), 129);
}

/// An image for the model: re-encoded so its long side is at most `max` px.
pub struct ViewImage {
    pub width: u32,
    pub height: u32,
    pub orig: (u32, u32),
    pub mime: &'static str,
    pub data_b64: String,
    pub sha256: String,
}

const MAX_IMAGE_FILE: u64 = 20 * 1024 * 1024;

/// Load `path` (under the read policy) for `view`: PNG, JPEG, WebP or GIF (first frame), at most 20 MB.
/// JPEG stays JPEG; everything else becomes PNG. An image already within `max` is passed through unchanged.
pub fn load_view_image(pol: &crate::policy::Policy, path: &str, max: u32) -> std::result::Result<ViewImage, String> {
    use base64::Engine;
    use image::{GenericImageView, ImageFormat};
    let p = pol.check_read(path).map_err(|e| format!("view {path}: {e}"))?;
    let bytes = read_checked(&p, MAX_IMAGE_FILE).map_err(|e| format!("view {path}: {e}"))?;
    let fmt = image::guess_format(&bytes).map_err(|_| format!("view {path}: not an image (PNG, JPEG, WebP or GIF)"))?;
    if !matches!(fmt, ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif) {
        return Err(format!("view {path}: unsupported format {fmt:?} (PNG, JPEG, WebP or GIF)"));
    }
    let img = image::load_from_memory_with_format(&bytes, fmt).map_err(|e| format!("view {path}: {e}"))?;
    let (w, h) = img.dimensions();
    let sha256 = crate::eventlog::sha_bytes(&bytes);
    let jpeg = fmt == ImageFormat::Jpeg;
    let mime = if jpeg { "image/jpeg" } else { "image/png" };
    let (out, (ow, oh)) = if w.max(h) <= max && matches!(fmt, ImageFormat::Png | ImageFormat::Jpeg) {
        (bytes, (w, h))
    } else {
        let small = if w.max(h) > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
        let mut buf = std::io::Cursor::new(Vec::new());
        if jpeg {
            small.to_rgb8().write_to(&mut buf, ImageFormat::Jpeg).map_err(|e| format!("view {path}: {e}"))?;
        } else {
            small.write_to(&mut buf, ImageFormat::Png).map_err(|e| format!("view {path}: {e}"))?;
        }
        let d = small.dimensions();
        (buf.into_inner(), d)
    };
    Ok(ViewImage { width: ow, height: oh, orig: (w, h), mime, data_b64: base64::engine::general_purpose::STANDARD.encode(out), sha256 })
}

#[cfg(test)]
mod prompt_limit_tests {
    #[test]
    fn every_limit_the_shipped_prompt_names_has_a_value() {
        let text = crate::modules::Resolver::embedded("std", "system.md").unwrap();
        let limits = super::prompt_limits();
        let mut named = 0;
        for part in text.split("{{").skip(1) {
            let name = part.split("}}").next().unwrap();
            if !name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) { continue; } // `{{file: string}}` is prose
            named += 1;
            // STR_TOP and ROWS belong to the printer, an overridable Luau module, so std/prompt adds them.
            assert!(limits.get(name).is_some() || matches!(name, "STR_TOP" | "ROWS"), "system.md names {{{{{name}}}}}, which has no value");
        }
        assert!(named >= 7, "placeholders went missing: {named}");
    }
}

#[cfg(test)]
mod receipt_tests {
    use super::*;

    #[tokio::test]
    async fn shell_receipt_identity_includes_the_command_after_character_2000() {
        let dir = std::env::temp_dir().join(format!("hx-long-command-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pol = Arc::new(crate::policy::Policy::new(&dir, false, None, None, None).unwrap());
        let path = pol.log.path.clone();
        let prefix = format!(": #{}\n", "x".repeat(2100));
        let original = format!("print(sh({:?}).code)", format!("{prefix}echo original"));
        let changed = format!("print(sh({:?}).code)", format!("{prefix}echo changed"));
        let vm = crate::vm::Vm::new(pol.clone(), Duration::from_secs(5), 32 << 20, None).unwrap();
        let control = vm.session.run(&original).await;
        assert!(control.error.is_none(), "{:?}", control.error);
        assert_eq!(control.output, "0\n", "the control shell command must actually execute successfully");
        let result = vm.session.run(&changed).await;
        assert!(result.error.is_none());
        let records = crate::eventlog::load(&path).unwrap();
        let intents: Vec<_> = records.iter().filter(|r| r["kind"] == "intent" && r["tool"] == "sh").collect();
        assert_eq!(intents.len(), 2);
        assert_ne!(intents[0]["args_sha"], intents[1]["args_sha"], "full command suffix participates in operation identity");
        let outcomes: Vec<_> = records.iter().filter(|r| r["kind"] == "outcome" && r["ret"]["t"] == "sh").collect();
        assert!(outcomes.len() == 2 && outcomes.iter().all(|r| r["approval"] == "not-required"), "each shell outcome records who approved it");
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;

    fn workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-prims-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn vm(dir: &Path) -> crate::vm::Vm {
        let pol = Arc::new(crate::policy::Policy::new(dir, false, None, None, None).unwrap());
        crate::vm::Vm::new(pol, Duration::from_secs(10), 32 << 20, None).unwrap()
    }

    #[test]
    fn edit_uniqueness_counts_overlapping_occurrences() {
        let text = b"end\nend\nend\n";
        assert!(replace_bytes("f", text, b"end\nend", b"X", false).is_err_and(|e| e.to_string().contains("edit_not_unique")));
        assert!(replace_bytes("f", b"aaa", b"aa", b"b", false).is_err());
        assert_eq!(replace_bytes("f", b"aaa", b"aa", b"b", true).unwrap(), (b"ba".to_vec(), 1));
        assert_eq!(replace_bytes("f", b"a\xffb", b"\xff", b"\x80", false).unwrap(), (b"a\x80b".to_vec(), 1));
        assert!(replace_bytes("f", b"abc", b"x", b"y", false).is_err_and(|e| e.to_string().contains("edit_not_found")));
        assert_eq!((line_count(b""), line_count(b"a"), line_count(b"a\n"), line_count(b"a\nb")), (0, 1, 1, 2));
    }

    #[tokio::test]
    async fn read_write_and_edit_are_byte_exact() {
        let dir = workspace("bytes");
        std::fs::write(dir.join("f"), b"a\xffb\nend\n").unwrap();
        let vm = vm(&dir);
        let r = vm.session.run(r#"local s = read("f"); assert(#s == 8 and s:byte(2) == 255)
            assert(read("f", {offset = 2}) == "end\n")
            edit("f", "\255", "\128"); write("g", "\254\n")"#).await;
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(std::fs::read(dir.join("f")).unwrap(), b"a\x80b\nend\n");
        assert_eq!(std::fs::read(dir.join("g")).unwrap(), b"\xfe\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn read_refuses_what_is_not_a_regular_file() {
        let dir = workspace("fifo");
        assert!(std::process::Command::new("mkfifo").arg(dir.join("pipe")).status().unwrap().success());
        let vm = vm(&dir);
        let r = vm.session.run(r#"local ok, e = pcall(read, "pipe"); assert(not ok and tostring(e):find("not a regular file", 1, true), tostring(e))
            local ok2, e2 = pcall(read, "."); assert(not ok2 and tostring(e2):find("is a directory", 1, true), tostring(e2))"#).await;
        assert!(r.error.is_none(), "{:?}", r.error);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn options_are_plain_data_read_once() {
        let dir = workspace("options");
        std::fs::write(dir.join("f"), "x\n").unwrap();
        let vm = vm(&dir);
        let r = vm.session.run(r#"local trap = setmetatable({}, {__index = function() error("metamethod ran") end})
            local ok, e = pcall(read, "f", trap); assert(not ok and not tostring(e):find("metamethod ran", 1, true), tostring(e))
            assert(not pcall(read, "f", {offest = 2}))
            assert(not pcall(search, "x", {ignore_case = "yes"}))
            assert(not pcall(edit, "f", "x", "y", {al = true}))"#).await;
        assert!(r.error.is_none(), "{:?}", r.error);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_is_truncated_only_when_a_match_was_left_out() {
        let dir = workspace("cap");
        std::fs::write(dir.join("a"), "hit\nhit\n").unwrap();
        std::fs::write(dir.join("b"), "hit\n").unwrap();
        let search = |max: f64| {
            let opts = SearchOptions { max: Some(max), ..Default::default() };
            search_files(&dir, &dir, "hit", &opts, false, &[]).ok().unwrap()
        };
        let exact = search(3.0);
        assert_eq!((exact.rows.len(), exact.truncated), (3, false));
        let capped = search(2.0);
        assert_eq!((capped.rows.len(), capped.truncated), (2, true));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_covers_hidden_paths_except_git_and_harness() {
        let dir = workspace("hidden");
        for f in [".github/ci.yml", ".git/config", ".harness/x", ".envrc", "src/.keep", "credentials"] {
            let p = dir.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "needle\n").unwrap();
        }
        let opts = SearchOptions { literal: true, ..Default::default() };
        let files: Vec<String> = search_files(&dir, &dir, "needle", &opts, false, &[]).ok().unwrap().rows.into_iter().map(|r| r.0).collect();
        assert_eq!(files, [".envrc", ".github/ci.yml", "src/.keep"]);
        let inside = search_files(&dir, &dir.join(".harness"), "needle", &opts, false, &[]).ok().unwrap();
        assert_eq!(inside.rows.len(), 1, "a search that starts inside .harness covers it");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_effect_where_luau_cannot_yield_fails_before_it_exists() {
        let dir = workspace("yield");
        let pol = Arc::new(crate::policy::Policy::new(&dir, false, None, None, None).unwrap());
        let log = pol.log.path.clone();
        let vm = crate::vm::Vm::new(pol, Duration::from_secs(10), 32 << 20, None).unwrap();
        let r = vm.session.run(r#"local ok, e = pcall(string.gsub, "a", "a", function() return sh("touch ran").stdout end)
            assert(not ok and tostring(e):find("cannot run inside a callback that cannot yield", 1, true), tostring(e))
            assert(not pcall(table.sort, {2, 1}, function() return write("w", "x") > 0 end))
            assert(sh("echo again").stdout == "again\n")"#);
        let r = tokio::time::timeout(Duration::from_secs(20), r).await.expect("a later mutation must not hang");
        assert!(r.error.is_none(), "{:?}", r.error);
        assert!(!dir.join("ran").exists() && !dir.join("w").exists());
        let records = crate::eventlog::load(&log).unwrap();
        let outcomes: std::collections::HashSet<_> = records.iter().filter_map(|r| r["of"].as_u64()).collect();
        assert!(records.iter().filter(|r| r["kind"] == "intent").all(|r| outcomes.contains(&r["seq"].as_u64().unwrap())));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_read_never_follows_a_directory_swapped_for_a_symlink() {
        let dir = workspace("swap");
        let outside = workspace("swap-outside");
        std::fs::create_dir_all(dir.join("d")).unwrap();
        std::fs::write(dir.join("d/f"), "inside").unwrap();
        std::fs::write(outside.join("f"), "OUTSIDE").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("l")).unwrap();
        let pol = crate::policy::Policy::new(&dir, false, None, None, None).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let (d, l, stop) = (std::ffi::CString::new(dir.join("d").into_os_string().into_encoded_bytes()).unwrap(),
                std::ffi::CString::new(dir.join("l").into_os_string().into_encoded_bytes()).unwrap(), stop.clone());
            std::thread::spawn(move || while !stop.load(Ordering::Relaxed) {
                // SAFETY: both arguments are NUL-terminated paths that outlive the call.
                unsafe { libc::renameat2(libc::AT_FDCWD, d.as_ptr(), libc::AT_FDCWD, l.as_ptr(), libc::RENAME_EXCHANGE) };
            })
        };
        let leaked = (0..3000).filter(|_| read_file(&pol, "d/f", &ReadOptions::default()).is_ok_and(|b| b == b"OUTSIDE")).count();
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert_eq!(leaked, 0);
        let _ = (std::fs::remove_dir_all(dir), std::fs::remove_dir_all(outside));
    }
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    fn env() -> ShellEnv {
        ShellEnv { env: None, confine: None }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-shell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn options_are_typed_and_read_raw() {
        let lua = Lua::new();
        let parse = |src: &str| ShOpts::parse(lua.load(src).eval::<Option<Table>>().unwrap());
        assert_eq!(parse("nil").unwrap(), ShOpts::default());
        assert_eq!(parse("{timeout = 5, cwd = 'sub'}").unwrap(), ShOpts { timeout: Some(5.0), cwd: Some("sub".into()) });
        for bad in ["{timeout = 0/0}", "{timeout = math.huge}", "{timeout = 'abc'}", "{timeout = '5'}", "{cwd = 1}", "{tiemout = 5}", "{5}"] {
            assert!(parse(bad).is_err(), "{bad} must be rejected");
        }
        // An __index that never returns is never called.
        let looping = parse("setmetatable({}, {__index = function() while true do end end})").unwrap();
        assert_eq!(looping, ShOpts::default());
        assert_eq!(ShOpts { timeout: Some(-3.0), cwd: None }.secs(), 1.0);
        assert_eq!(ShOpts { timeout: Some(1e9), cwd: None }.secs(), SH_TIMEOUT_MAX);
    }

    #[tokio::test]
    async fn a_bad_timeout_is_an_error_before_anything_runs() {
        let dir = scratch("nan");
        for secs in [f64::NAN, -1.0, f64::INFINITY] {
            assert!(shell(&env(), "touch ran", dir.clone(), secs).await.is_err());
        }
        assert!(!dir.join("ran").exists(), "the command must not run");
        let pol = Arc::new(crate::policy::Policy::new(&dir, false, None, None, None).unwrap());
        let vm = crate::vm::Vm::new(pol, Duration::from_secs(5), 32 << 20, None).unwrap();
        let r = vm.session.run("print(pcall(sh, 'touch ran', {timeout = 0/0}))").await;
        assert!(r.error.is_none() && r.output.starts_with("false"), "{:?} {}", r.error, r.output);
        assert!(!dir.join("ran").exists(), "the command must not run");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn options_with_a_looping_index_do_not_escape_the_chunk_deadline() {
        let dir = scratch("index");
        let pol = Arc::new(crate::policy::Policy::new(&dir, false, None, None, None).unwrap());
        let vm = crate::vm::Vm::new(pol, Duration::from_secs(5), 32 << 20, None).unwrap();
        let code = "print(sh('echo hi', setmetatable({}, {__index = function() while true do end end})).stdout)";
        let r = tokio::time::timeout(Duration::from_secs(30), vm.session.run(code)).await.expect("the chunk must finish");
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.output, "hi\n\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn death_by_signal_is_128_plus_the_signal() {
        let dir = scratch("signal");
        let (_, _, code, timed_out, _) = shell(&env(), "echo x; kill -TERM $$", dir.clone(), 10.0).await.unwrap();
        assert_eq!((code, timed_out), (Some(128 + 15), false));
        let (_, _, code, timed_out, _) = shell(&env(), "exit 3", dir.clone(), 10.0).await.unwrap();
        assert_eq!((code, timed_out), (Some(3), false));
        let (_, _, code, timed_out, _) = shell(&env(), "sleep 10", dir.clone(), 1.0).await.unwrap();
        assert_eq!((code, timed_out), (None, true));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn output_survives_a_descendant_that_left_the_group_and_holds_the_pipe() {
        let dir = scratch("escape");
        let t0 = std::time::Instant::now();
        // The fifo makes the command wait until the escapee has its own session, so it is not killed with the group.
        let cmd = "mkfifo f; setsid sh -c 'echo > f; exec sleep 6.4321' & read < f; echo hello; echo oops >&2";
        let (so, se, code, timed_out, _) = shell(&env(), cmd, dir.clone(), 10.0).await.unwrap();
        let elapsed = t0.elapsed();
        let _ = std::process::Command::new("pkill").args(["-f", "sleep 6.4321"]).status();
        assert_eq!((so.as_slice(), se.as_slice(), code, timed_out), (&b"hello\n"[..], &b"oops\n"[..], Some(0), false));
        assert!(elapsed < DRAIN_GRACE * 2, "both readers share one cutoff, took {elapsed:?}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
