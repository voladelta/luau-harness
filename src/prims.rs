//! Native primitives: read, write, edit, sh and search. Every effect goes through
//! `Effects` and every path through `Policy`; `sh` runs under Seatbelt.
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

/// Replace a file's content all at once (temp file, fsync, rename), after journaling the content's hash. A crash leaves
/// the old or the new file, never a torn one, so resume can tell which from the disk (`supervisor::reconcile`).
fn replace_file(sup: &crate::supervisor::Supervisor, intent: Option<u64>, p: &Path, target: &crate::atomic_file::Target, bytes: &[u8]) -> Result<()> {
    if let Some(id) = intent { sup.expect(id, p, &crate::eventlog::sha_bytes(bytes))?; }
    target.replace(bytes)
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
    rt(format!("unexpected native result {r:?}"))
}

/// Copy a structured native result into Lua data.
pub fn json_value(lua: &Lua, r: Ret) -> mlua::Result<Value> {
    match r {
        Ret::Json(v) => crate::values::copy_in(lua, &v),
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

    // read(path, {offset=1, limit=n}) -> string. offset/limit are in lines.
    let (f, o) = (fx.clone(), ops.clone());
    g.set("read", lua.create_function(move |lua, (path, opts): (String, Option<Table>)| {
        o.fetch_add(1, Ordering::Relaxed);
        let (offset, limit) = (opt_num(&opts, "offset"), opt_num(&opts, "limit"));
        let args = json!({"path": &path, "offset": offset, "limit": limit});
        let pol = f.pol.clone();
        match f.sync(lua, crate::supervisor::Tool::Read, Class::Read, args, |_| {
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
        let sup = f.supervisor.clone();
        match f.sync(lua, crate::supervisor::Tool::Write, Class::LocalWrite, args, |intent| {
            let p = pol.check_write(&path).map_err(|e| rt(format!("write {path}: {e}")))?;
            let target = crate::atomic_file::Target::open(&p, true).map_err(mlua::Error::external)?;
            replace_file(&sup, intent, &p, &target, &bytes).map_err(crate::effects::effect_error)?;
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
        let sup = f.supervisor.clone();
        match f.sync(lua, crate::supervisor::Tool::Edit, Class::LocalWrite, args, |intent| {
            let p = pol.check_write(&path).map_err(|e| rt(format!("edit {path}: {e}")))?;
            // edit reads the file, and whether `old` matched would reveal its content
            if pol.enabled && crate::policy::is_secret(&p) {
                return Err(rt(format!("edit {path}: policy: a credential file cannot be read")));
            }
            let target = crate::atomic_file::Target::open(&p, false).map_err(mlua::Error::external)?;
            let text = target.read_to_string().map_err(|e| rt(format!("edit {path}: {e}")))?;
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
            replace_file(&sup, intent, &p, &target, text.replace(&old, &new).as_bytes()).map_err(crate::effects::effect_error)?;
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
            let args = json!({"cmd": &cmd, "cwd": opt_str(&opts, "cwd"), "timeout": opt_num(&opts, "timeout")});
            let ret = f.run(&lua, crate::supervisor::Tool::Sh, class, args, || async {
                if let Err(why) = pol.approve(&cmd, class).await {
                    return Err(rt(format!("approval_required: {why}")));
                }
                let secs = opt_num(&opts, "timeout").unwrap_or(60.0).clamp(1.0, 600.0);
                let cwd = match opt_str(&opts, "cwd") {
                    Some(c) => pol.check_read(&c).map_err(|e| rt(format!("sh: cwd: {e}")))?,
                    None => pol.workspace.clone(),
                };
                let (so, se, status, timed_out) = shell(Some(&pol), &cmd, cwd, secs).await?;
                Ok(Ret::Sh { stdout: so, stderr: se, code: status, timed_out, operation_id:None })
            }).await?;
            match ret {
                Ret::Sh { stdout, stderr, code, timed_out, operation_id } => {
                    let t = lua.create_table()?;
                    t.set("stdout", lua.create_string(&stdout)?)?;
                    t.set("stderr", lua.create_string(&stderr)?)?;
                    t.set("code", code.unwrap_or(-1))?;
                    t.set("timed_out", timed_out)?;
                    t.set("operation_id", operation_id)?;
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
        match f.sync(lua, crate::supervisor::Tool::Search, Class::Read, args, |_| {
            pol.check_read(&base).map_err(|e| rt(format!("search {base}: {e}")))?;
            Ok(match search(&pol.workspace, &pat, &opts, pol.enabled, f.supervisor.workflow_epoch().is_some(), &pol.sealed)? {
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

// search(pattern, {path=".", glob="*.rs", max=200, ignore_case=false, literal=false}). With `skip_secrets` (the policy is on),
// credential files are not searched, as `read` refuses them.
fn search(root: &Path, pat: &str, opts: &Option<Table>, skip_secrets: bool, workflow: bool, sealed: &[PathBuf]) -> mlua::Result<Result<(Rows, bool), SearchErr>> {
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
    if workflow {
        wb.parents(false);
        let project=root.to_path_buf();
        let explicit=base.strip_prefix(root).unwrap_or(&base).to_path_buf();
        wb.filter_entry(move |entry| {
            let relative=entry.path().strip_prefix(&project).unwrap_or(entry.path());
            [".harness/log", ".agents/artifacts"].iter().all(|runtime|explicit.starts_with(runtime) || !relative.starts_with(runtime))
        });
    }
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
        if (skip_secrets && crate::policy::is_secret(path)) || sealed.iter().any(|s| path.starts_with(s)) {
            continue;
        }
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

/// Run `cmd` with bash under the run's OS policy (None: an owner command, outside the agent sandbox): own process
/// group, capped output, killed at `secs`.
pub async fn shell(pol: Option<&crate::policy::Policy>, cmd: &str, cwd: PathBuf, secs: f64) -> mlua::Result<(Vec<u8>, Vec<u8>, Option<i64>, bool)> {
    let mut command = if let Some(pol) = pol.filter(|p| p.enabled) {
        let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(pol.seatbelt_profile()).arg("/bin/bash").arg("-c").arg(&cmd);
        c.env_clear().envs(pol.env());
        c
    } else if let Some(profile) = pol.and_then(|p| p.yolo_profile()) {
        let mut c = tokio::process::Command::new("/usr/bin/sandbox-exec");
        c.arg("-p").arg(profile).arg("/bin/bash").arg("-c").arg(&cmd);
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
    Ok((so, se, status.map(i64::from), timed_out))
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
    let len = std::fs::metadata(&p).map_err(|e| format!("view {path}: {e}"))?.len();
    if len > MAX_IMAGE_FILE {
        return Err(format!("view {path}: file is {len} bytes, over the 20MB limit"));
    }
    let bytes = std::fs::read(&p).map_err(|e| format!("view {path}: {e}"))?;
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
mod receipt_tests {
    use super::*;

    #[tokio::test]
    async fn shell_receipt_identity_includes_the_command_after_character_2000() {
        let dir = std::env::temp_dir().join(format!("hx-long-command-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pol = Arc::new(crate::policy::Policy::new(&dir, true, None, None, None).unwrap());
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
        let _ = std::fs::remove_dir_all(dir);
    }
}
