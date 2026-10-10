//! Native primitives: read, write, edit, sh, search (+ the non-determinism recorder). Every effect goes through
//! `Effects` (live or replayed) and every path through `Policy`; `sh` runs under Seatbelt.
use crate::effects::{rt, Effects, Ret};
use crate::policy::Class;
use anyhow::Result;
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};
use ignore::{overrides::OverrideBuilder, WalkBuilder};
use mlua::{Lua, Table, Value};
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


fn resolve(root: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { root.join(p) }
}

pub fn opt_str(o: &Option<Table>, k: &str) -> Option<String> {
    o.as_ref().and_then(|t| t.get::<Option<String>>(k).ok().flatten())
}
fn opt_num(o: &Option<Table>, k: &str) -> Option<f64> {
    o.as_ref().and_then(|t| t.get::<Option<f64>>(k).ok().flatten())
}
fn opt_bool(o: &Option<Table>, k: &str) -> bool {
    o.as_ref().and_then(|t| t.get::<Option<bool>>(k).ok().flatten()).unwrap_or(false)
}
pub fn bad(r: Ret) -> mlua::Error {
    rt(format!("replay: unexpected recorded result {r:?}"))
}

/// A recorded `Ret::Json` as a Lua value (anything else is a replay mismatch).
pub fn json_value(lua: &Lua, r: Ret) -> mlua::Result<Value> {
    match r {
        Ret::Json(v) => mlua::LuaSerdeExt::to_value(lua, &v),
        r => Err(bad(r)),
    }
}

pub fn rows_table(lua: &Lua, rows: &[(String, i64, String)], truncated: bool) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (file, line, text) in rows {
        let row = lua.create_table()?;
        row.set("file", file.as_str())?;
        row.set("line", *line)?;
        row.set("text", text.as_str())?;
        t.raw_push(row)?;
    }
    if truncated {
        let (mt, idx) = (lua.create_table()?, lua.create_table()?);
        idx.set("truncated", true)?;
        mt.set("__index", idx)?;
        t.set_metatable(Some(mt))?;
    }
    Ok(t)
}

pub fn install(lua: &Lua, fx: Arc<Effects>, ops: Arc<AtomicUsize>) -> Result<()> {
    let g = lua.globals();
    let pol = fx.pol.clone();

    // tmpdir() -> the run's private scratch directory (writable; the only writable place besides the project).
    let tmp = pol.tmp.to_string_lossy().to_string();
    g.set("tmpdir", lua.create_function(move |_, ()| Ok(tmp.clone()))?)?;

    // __nd_value(kind, live) -> the recorded value on replay, else `live` (recorded). Wraps os.time/clock/date/math.random.
    let f = fx.clone();
    g.set("__nd_value", lua.create_function(move |lua, (kind, live): (String, Value)| {
        let r = match &live {
            Value::Number(n) => Ret::Num(*n),
            Value::Integer(i) => Ret::Num(*i as f64),
            Value::String(s) => Ret::Str(s.to_string_lossy().to_string()),
            _ => Ret::Nil,
        };
        match f.sync(lua, "nd", Class::Read, json!({"kind": kind}), || Ok(r))? {
            Ret::Num(n) if n.fract() == 0.0 && n.abs() < 1e15 => Ok(Value::Integer(n as i64)),
            Ret::Num(n) => Ok(Value::Number(n)),
            Ret::Str(s) => Ok(Value::String(lua.create_string(&s)?)),
            _ => Ok(Value::Nil),
        }
    })?)?;

    // read(path, {offset=1, limit=n}) -> string. offset/limit are in lines.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("read", lua.create_function(move |lua, (path, opts): (String, Option<Table>)| {
        o.fetch_add(1, Ordering::Relaxed);
        let (offset, limit) = (opt_num(&opts, "offset"), opt_num(&opts, "limit"));
        let args = json!({"path": &path, "offset": offset, "limit": limit});
        let pol = f.pol.clone();
        match f.sync(lua, "read", Class::Read, args, || {
            let p = pol.check_read(&path).map_err(|e| rt(format!("read {path}: {e}")))?;
            let meta = std::fs::metadata(&p).map_err(|e| rt(format!("read {path}: {e}")))?;
            if meta.is_dir() {
                return Err(rt(format!("read {path}: is a directory (use sh('ls') or search)")));
            }
            if meta.len() > MAX_READ {
                return Err(rt(format!("read {path}: file is {} bytes, over the 10MB limit", meta.len())));
            }
            let bytes = std::fs::read(&p).map_err(|e| rt(format!("read {path}: {e}")))?;
            if bytes.iter().take(8192).any(|b| *b == 0) {
                return Err(rt(format!("read {path}: binary file")));
            }
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if offset.is_none() && limit.is_none() {
                return Ok(Ret::Str(text));
            }
            let start = offset.unwrap_or(1.0).max(1.0) as usize - 1;
            let take = limit.map(|l| l.max(0.0) as usize).unwrap_or(usize::MAX);
            Ok(Ret::Str(text.split_inclusive('\n').skip(start).take(take).collect::<String>()))
        })? {
            Ret::Str(s) => Ok(s),
            r => Err(bad(r)),
        }
    })?)?;

    // write(path, content) -> bytes written. Creates parent directories.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("write", lua.create_function(move |lua, (path, content): (String, mlua::LuaString)| {
        o.fetch_add(1, Ordering::Relaxed);
        let bytes = content.as_bytes().to_vec();
        let args = json!({"path": &path, "bytes": bytes.len(), "sha256": crate::eventlog::sha_bytes(&bytes)});
        let pol = f.pol.clone();
        match f.sync(lua, "write", Class::LocalWrite, args, || {
            let p = pol.check_write(&path).map_err(|e| rt(format!("write {path}: {e}")))?;
            if let Some(dir) = p.parent() {
                std::fs::create_dir_all(dir).map_err(|e| rt(format!("write {path}: {e}")))?;
            }
            std::fs::write(&p, &bytes).map_err(|e| rt(format!("write {path}: {e}")))?;
            Ok(Ret::Num(bytes.len() as f64))
        })? {
            Ret::Num(n) => Ok(n as i64),
            r => Err(bad(r)),
        }
    })?)?;

    // edit(path, old, new, {all=false}) -> "ok: N replacement(s), +A/-B lines". `old` must match exactly once unless all=true.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("edit", lua.create_function(move |lua, (path, old, new, opts): (String, mlua::LuaString, mlua::LuaString, Option<Table>)| {
        o.fetch_add(1, Ordering::Relaxed);
        let (old, new) = (old.to_string_lossy().to_string(), new.to_string_lossy().to_string());
        let all = opt_bool(&opts, "all");
        let args = json!({"path": &path, "old_sha": crate::eventlog::sha_bytes(old.as_bytes()), "new_sha": crate::eventlog::sha_bytes(new.as_bytes()), "old_bytes": old.len(), "new_bytes": new.len(), "all": all});
        let pol = f.pol.clone();
        match f.sync(lua, "edit", Class::LocalWrite, args, || {
            let p = pol.check_write(&path).map_err(|e| rt(format!("edit {path}: {e}")))?;
            let text = std::fs::read_to_string(&p).map_err(|e| rt(format!("edit {path}: {e}")))?;
            if old.is_empty() {
                return Err(rt(format!("edit {path}: `old` is empty")));
            }
            let n = text.matches(&old).count();
            if n == 0 {
                return Err(rt(format!("edit_not_found: text not found in {path}; read the file and copy the exact text (whitespace matters)")));
            }
            if n > 1 && !all {
                return Err(rt(format!("edit_not_unique: text matches {n} times in {path}; include more surrounding lines or pass {{all=true}}")));
            }
            std::fs::write(&p, text.replace(&old, &new)).map_err(|e| rt(format!("edit {path}: {e}")))?;
            let (a, b) = (new.lines().count() * n, old.lines().count() * n);
            Ok(Ret::Str(format!("ok: {n} replacement{}, +{a}/-{b} lines", if n == 1 { "" } else { "s" })))
        })? {
            Ret::Str(s) => Ok(s),
            r => Err(bad(r)),
        }
    })?)?;

    // sh(cmd, {timeout=seconds, cwd=dir}) -> {stdout, stderr, code, timed_out}
    let (f, o) = (fx.clone(), ops.clone());
    g.set("sh", lua.create_async_function(move |lua, (cmd, opts): (String, Option<Table>)| {
        let (f, o) = (f.clone(), o.clone());
        async move {
            o.fetch_add(1, Ordering::Relaxed);
            let pol = f.pol.clone();
            let class = pol.classify_sh(&cmd);
            let args = json!({"cmd": cmd.chars().take(2000).collect::<String>(), "cwd": opt_str(&opts, "cwd"), "timeout": opt_num(&opts, "timeout")});
            let ret = f.run(&lua, "sh", class, args, || async {
                if let Err(why) = pol.approve(&cmd, class).await {
                    return Err(rt(format!("approval_required: {why}")));
                }
                let secs = opt_num(&opts, "timeout").unwrap_or(60.0).clamp(1.0, 600.0);
                let cwd = match opt_str(&opts, "cwd") {
                    Some(c) => pol.check_read(&c).map_err(|e| rt(format!("sh: cwd: {e}")))?,
                    None => pol.workspace.clone(),
                };
                let mut command = if pol.enabled {
                    let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
                    c.arg("-p").arg(pol.seatbelt_profile()).arg("/bin/bash").arg("-c").arg(&cmd);
                    c.env_clear().envs(pol.env());
                    c
                } else {
                    let mut c = tokio::process::Command::new("bash");
                    c.arg("-c").arg(&cmd);
                    c
                };
                let nproc_cap = user_process_count() + 300;
                command
                    .current_dir(cwd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                // Own process group (killed as a whole), and a cap on how many processes this user may have, so a fork
                // bomb inside `sh` hits EAGAIN instead of taking the machine down.
                unsafe {
                    command.pre_exec(move || {
                        libc::setsid();
                        let lim = libc::rlimit { rlim_cur: nproc_cap, rlim_max: nproc_cap };
                        libc::setrlimit(libc::RLIMIT_NPROC, &lim);
                        Ok(())
                    });
                }
                let mut child = command.spawn().map_err(|e| rt(format!("sh: spawn failed: {e}")))?;
                let group = child.id().map(|p| GroupKill(p as i32)); // killpg on drop: also covers cancellation
                let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
                async fn drain<R: tokio::io::AsyncRead + Unpin>(mut s: R) -> Vec<u8> {
                    let mut buf = Vec::new();
                    let _ = (&mut s).take(MAX_CAPTURE as u64).read_to_end(&mut buf).await;
                    // keep reading (and discarding) so the child never blocks on a full pipe
                    let _ = tokio::io::copy(&mut s, &mut tokio::io::sink()).await;
                    buf
                }
                let (out_h, err_h) = (tokio::spawn(drain(out)), tokio::spawn(drain(err)));
                let (status, timed_out) = match tokio::time::timeout(Duration::from_secs_f64(secs), child.wait()).await {
                    Ok(st) => (st.ok().and_then(|s| s.code()), false),
                    Err(_) => (None, true),
                };
                // The command is over: kill stragglers and background jobs now. That also closes the pipes they held,
                // so the readers finish; whatever was printed before a timeout is kept.
                drop(group);
                let grace = Duration::from_secs(2);
                let so = tokio::time::timeout(grace, out_h).await.ok().and_then(|r| r.ok()).unwrap_or_default();
                let se = tokio::time::timeout(grace, err_h).await.ok().and_then(|r| r.ok()).unwrap_or_default();
                Ok(Ret::Sh { stdout: so, stderr: se, code: status.map(|c| c as i64), timed_out })
            }).await?;
            match ret {
                Ret::Sh { stdout, stderr, code, timed_out } => {
                    let t = lua.create_table()?;
                    t.set("stdout", lua.create_string(&stdout)?)?;
                    t.set("stderr", lua.create_string(&stderr)?)?;
                    t.set("code", code.unwrap_or(-1))?;
                    t.set("timed_out", timed_out)?;
                    Ok(t)
                }
                r => Err(bad(r)),
            }
        }
    })?)?;

    // __search_raw(pattern, opts) -> rows | nil, {kind, message, hint}; wrapped by the prelude so errors are tables.
    let (f, o) = (fx.clone(), ops);
    g.set("__search_raw", lua.create_function(move |lua, (pat, opts): (String, Option<Table>)| {
        o.fetch_add(1, Ordering::Relaxed);
        let base = opt_str(&opts, "path").unwrap_or_else(|| ".".into());
        let args = json!({"pattern": &pat, "path": &base, "glob": opt_str(&opts, "glob"), "max": opt_num(&opts, "max"), "ignore_case": opt_bool(&opts, "ignore_case"), "literal": opt_bool(&opts, "literal")});
        let pol = f.pol.clone();
        match f.sync(lua, "search", Class::Read, args, || {
            pol.check_read(&base).map_err(|e| rt(format!("search {base}: {e}")))?;
            Ok(match search(&pol.workspace, &pat, &opts)? {
                Ok((rows, truncated)) => Ret::Rows { rows, truncated },
                Err((kind, message, hint)) => Ret::SearchErr { kind: kind.into(), message, hint: hint.into() },
            })
        })? {
            Ret::Rows { rows, truncated } => Ok((Value::Table(rows_table(lua, &rows, truncated)?), Value::Nil)),
            Ret::SearchErr { kind, message, hint } => {
                let e = lua.create_table()?;
                e.set("kind", kind)?;
                e.set("message", message)?;
                e.set("hint", hint)?;
                Ok((Value::Nil, Value::Table(e)))
            }
            r => Err(bad(r)),
        }
    })?)?;
    Ok(())
}

type SearchErr = (&'static str, String, &'static str);
type Rows = Vec<(String, i64, String)>;

struct Collect<'a> {
    rows: &'a mut Rows,
    file: String,
    cap: usize,
}

impl Sink for Collect<'_> {
    type Error = std::io::Error;
    fn matched(&mut self, _s: &grep::searcher::Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.rows.len() >= self.cap {
            return Ok(false);
        }
        let text = String::from_utf8_lossy(m.bytes()).trim_end().to_string();
        let text: String = if text.chars().count() > 300 { text.chars().take(300).collect::<String>() + "…" } else { text };
        self.rows.push((self.file.clone(), m.line_number().unwrap_or(0) as i64, text));
        Ok(self.rows.len() < self.cap)
    }
}

// search(pattern, {path=".", glob="*.rs", max=200, ignore_case=false, literal=false})
fn search(root: &Path, pat: &str, opts: &Option<Table>) -> mlua::Result<Result<(Rows, bool), SearchErr>> {
    let literal = opt_bool(opts, "literal");
    if !literal {
        for tok in ["(?=", "(?!", "(?<=", "(?<!"] {
            if pat.contains(tok) {
                return Ok(Err(("unsupported_regex",
                    format!("lookaround `{tok}` is not supported (Rust regex engine)"),
                    "search for the fixed part and filter the rows in Luau, or use two searches")));
            }
        }
        let b = pat.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
            if b[i] == b'\\' {
                if (b'1'..=b'9').contains(&b[i + 1]) {
                    return Ok(Err(("unsupported_regex", "backreferences are not supported".into(),
                        "match each part separately and compare in Luau")));
                }
                i += 2;
            } else {
                i += 1;
            }
        }
    }
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(opt_bool(opts, "ignore_case"))
        .fixed_strings(literal)
        .build(pat)
    {
        Ok(m) => m,
        Err(e) => return Ok(Err(("invalid_regex", e.to_string(), "escape special characters or pass literal=true"))),
    };
    let base = resolve(root, &opt_str(opts, "path").unwrap_or_else(|| ".".into()));
    let cap = opt_num(opts, "max").map(|m| m.max(1.0) as usize).unwrap_or(DEFAULT_HITS);
    let mut wb = WalkBuilder::new(&base);
    wb.follow_links(false).require_git(false).sort_by_file_path(|a, b| a.cmp(b));
    if let Some(glob) = opt_str(opts, "glob") {
        let mut ob = OverrideBuilder::new(&base);
        if let Err(e) = ob.add(&glob) {
            return Ok(Err(("invalid_glob", e.to_string(), "use gitignore-style globs like *.rs or src/**/*.ts")));
        }
        match ob.build() {
            Ok(o) => { wb.overrides(o); }
            Err(e) => return Ok(Err(("invalid_glob", e.to_string(), "use gitignore-style globs"))),
        }
    }
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .build();
    let mut rows: Rows = Vec::new();
    let mut capped = false;
    for entry in wb.build().flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy().to_string();
        let mut sink = Collect { rows: &mut rows, file: rel, cap };
        let _ = searcher.search_path(&matcher, path, &mut sink);
        if rows.len() >= cap {
            capped = true;
            break;
        }
    }
    Ok(Ok((rows, capped)))
}

/// Processes currently owned by this user (the baseline for the fork-bomb cap).
fn user_process_count() -> u64 {
    let uid = unsafe { libc::getuid() };
    std::process::Command::new("/bin/ps")
        .args(["-u", &uid.to_string(), "-o", "pid="])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count() as u64)
        .unwrap_or(500)
}

/// Kills the whole process group of an `sh` command when dropped.
struct GroupKill(i32);
impl Drop for GroupKill {
    fn drop(&mut self) {
        unsafe { libc::killpg(self.0, libc::SIGKILL) };
    }
}
