//! Luau VM. Bindings load, then sandbox(true). Two environments sit on the read-only globals:
//!   * the SESSION env: the model's persistent variables (what `vars()` lists);
//!   * the MODULE env: where harness modules (std loop, printer, ...) run, with `require` and the `hx` API.
use crate::effects::{Effects, Ret};
use crate::host::{self, Host};
use crate::modules::Resolver;
use crate::policy::{Class, Policy};
use crate::prims;
use anyhow::Result;
use serde_json::json;
use futures::future::join_all;
use mlua::{Function, Lua, LuaSerdeExt, MultiValue, Table, Value, Variadic, VmState};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const PRELUDE: &str = r##"
function print(...)
	local n = select("#", ...)
	local parts = table.create(n)
	for i = 1, n do
		parts[i] = __format((select(i, ...)))
	end
	__emit(table.concat(parts, "\t"))
end

-- Non-deterministic builtins go through the effect recorder so a resumed run sees the same values.
do
	local real_time, real_clock, real_date, real_random = os.time, os.clock, os.date, math.random
	os.time = function(t)
		if t ~= nil then return real_time(t) end
		return __nd_value("time", real_time())
	end
	os.clock = function() return __nd_value("clock", real_clock()) end
	os.date = function(f, t)
		if t ~= nil then return real_date(f, t) end
		return __nd_value("date", real_date(f))
	end
	math.random = function(...) return __nd_value("random", real_random(...)) end
end

-- Nothing the model or a package runs may reach the real globals or load code around the loader.
getfenv = nil
setfenv = nil
loadstring = nil
require = nil

function search(pattern, opts)
	local rows, err = __search_raw(pattern, opts)
	if err then
		setmetatable(err, { __tostring = function(e) return e.kind .. ": " .. e.message .. " (hint: " .. e.hint .. ")" end })
		error(err, 2)
	end
	return rows
end
"##;

pub struct RunResult {
    pub output: String,
    pub values: Vec<Value>,
    pub error: Option<String>,
    pub ops: usize,
}

/// The model's persistent environment plus the budgets that apply to each chunk.
#[derive(Clone)]
pub struct Session {
    lua: Lua,
    log: Arc<crate::eventlog::EventLog>,
    depth: usize,
    env: Table,
    out: Arc<Mutex<String>>,
    ops: Arc<AtomicUsize>,
    deadline: Arc<Mutex<Option<Instant>>>,
    budget: Duration,
}

impl Session {
    /// Compile `code`; if its last line is an expression, return it so the value is captured.
    fn compile(&self, code: &str) -> mlua::Result<Function> {
        let load = |src: &str| self.lua.load(src).set_name("=chunk").set_environment(self.env.clone()).into_function();
        let trimmed = code.trim_end();
        if let Some(idx) = trimmed.rfind('\n') {
            let (head, last) = trimmed.split_at(idx + 1);
            // Only rewrite when `head` is a complete chunk by itself; otherwise the last line closes a block
            // (`end`) and `return end` would silently exit the loop/if body early.
            if self.lua.load(head).set_name("=chunk").into_function().is_ok()
                && let Ok(f) = load(&format!("{head}return {last}")) {
                    return Ok(f);
                }
        } else if let Ok(f) = load(&format!("return {trimmed}")) {
            return Ok(f);
        }
        load(trimmed)
    }

    pub async fn run(&self, code: &str) -> RunResult {
        self.out.lock().unwrap().clear();
        self.ops.store(0, Ordering::Relaxed);
        *self.deadline.lock().unwrap() = Some(Instant::now() + self.budget);
        let result = match self.compile(code) {
            Ok(f) => f.call_async::<MultiValue>(()).await,
            Err(e) => Err(e),
        };
        *self.deadline.lock().unwrap() = None;
        let output = std::mem::take(&mut *self.out.lock().unwrap());
        let (values, error) = match result {
            Ok(vals) => (vals.into_iter().filter(|v| !v.is_nil()).collect(), None),
            Err(e) => (vec![], Some(clean_error(&e))),
        };
        RunResult { output, values, error, ops: self.ops.load(Ordering::Relaxed) }
    }
}

fn clean_error(e: &mlua::Error) -> String {
    match e {
        mlua::Error::CallbackError { cause, traceback } => format!("{}\n{}", clean_error(cause), traceback.trim_end()),
        other => other.to_string(),
    }
}

const SAFE_GLOBALS: &[&str] = &[
    "assert", "error", "pcall", "xpcall", "select", "type", "typeof", "tostring", "tonumber", "pairs", "ipairs", "next", "unpack",
    "rawget", "rawset", "rawequal", "rawlen", "setmetatable", "getmetatable", "math", "string", "table", "utf8", "bit32", "buffer",
    "coroutine", "os", "parallel",
];

/// Each primitive with the (capability, scope) it needs.
const PRIM_CAPS: [(&str, &str, &str); 6] = [("read", "fs", "read"), ("search", "fs", "read"), ("write", "fs", "write"), ("edit", "fs", "write"), ("sh", "proc", "sh"), ("agent", "llm", "agent")];

fn has(caps: &BTreeMap<String, Vec<String>>, cap: &str, scope: &str) -> bool {
    caps.get(cap).is_some_and(|v| v.iter().any(|s| s == scope || s == "*"))
}

/// The environment a project package runs in: safe builtins, plus ONLY the primitives its manifest requested (which
/// `hx approve` granted). Everything else is a stub that raises `capability_denied`.
fn package_env(lua: &Lua, pkg: &str, caps: &BTreeMap<String, Vec<String>>, require: &Function, hx: &Table) -> mlua::Result<Table> {
    let g = lua.globals();
    let view = lua.create_table()?;
    for k in SAFE_GLOBALS {
        view.set(*k, g.get::<Value>(*k)?)?;
    }
    for (prim, cap, scope) in PRIM_CAPS {
        if has(caps, cap, scope) {
            view.set(prim, g.get::<Value>(prim)?)?;
        } else {
            let msg = format!("capability_denied: package '{pkg}' was not granted {cap}.{scope}, which {prim}() needs");
            view.set(prim, lua.create_function(move |_, _: mlua::MultiValue| -> mlua::Result<()> { Err(mlua::Error::runtime(msg.clone())) })?)?;
        }
    }
    if has(caps, "fs", "read") || has(caps, "fs", "write") {
        view.set("tmpdir", g.get::<Value>("tmpdir")?)?;
    }
    view.set("require", require.clone())?;
    view.set("print", lua.create_function(stderr_print)?)?;
    let pkg_hx = lua.create_table()?;
    pkg_hx.set("log", hx.get::<Value>("log")?)?;
    if has(caps, "llm", "chat") {
        pkg_hx.set("llm", hx.get::<Value>("llm")?)?;
    }
    view.set("hx", pkg_hx)?;
    let env = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__index", view)?;
    mt.set("__metatable", "locked")?;
    env.set_metatable(Some(mt))?;
    Ok(env)
}

#[derive(Clone)]
struct ToolDef {
    source: String,
    doc: String,
    signature: String,
    types: String,
    tests: String,
    caps: BTreeMap<String, Vec<String>>,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 40 && n.chars().next().is_some_and(|c| c.is_ascii_lowercase()) && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn opt_string(o: &Option<Table>, k: &str) -> String {
    prims::opt_str(o, k).unwrap_or_default()
}

fn stderr_print(_: &Lua, args: Variadic<Value>) -> mlua::Result<()> {
    eprintln!("{}", args.iter().map(|v| v.to_string().unwrap_or_default()).collect::<Vec<_>>().join("\t"));
    Ok(())
}

/// opts.caps = { fs = {"read"}, proc = {"sh"} }
fn caps_of(o: &Option<Table>) -> mlua::Result<BTreeMap<String, Vec<String>>> {
    let mut out = BTreeMap::new();
    if let Some(Value::Table(caps)) = o.as_ref().map(|t| t.get::<Value>("caps")).transpose()? {
        for pair in caps.pairs::<String, Vec<String>>() {
            let (k, v) = pair?;
            out.insert(k, v);
        }
    }
    Ok(out)
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn render_manifest(name: &str, d: &ToolDef) -> String {
    let mut t = format!("[package]\nname = {}\nversion = \"0.1.0\"\nkind = \"tool\"\ndescription = {}\n\n[tool]\nsignature = {}\ndoc = {}\n\n[capabilities]\n", toml_str(name), toml_str(&d.doc), toml_str(&d.signature), toml_str(&d.doc));
    for (k, v) in &d.caps {
        t.push_str(&format!("{k} = [{}]\n", v.iter().map(|x| toml_str(x)).collect::<Vec<_>>().join(", ")));
    }
    t.push_str("\n[exports]\nmodule = \"init.luau\"\n\n[tests]\nfiles = [\"tests/*.luau\"]\n");
    t
}

pub struct Vm {
    pub lua: Lua,
    pub session: Session,
    modenv: Table,
}

impl Vm {
    pub fn new(policy: Arc<Policy>, budget: Duration, mem_limit: usize, host: Option<Arc<Host>>, replay: Option<(std::collections::HashMap<String, std::collections::VecDeque<crate::effects::Rec>>, PathBuf)>) -> Result<Vm> {
        let root: PathBuf = policy.workspace.clone();
        let lua = Lua::new();
        let out = Arc::new(Mutex::new(String::new()));
        let ops = Arc::new(AtomicUsize::new(0));
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let resolver = Arc::new(Resolver::new(root.clone()));
        let g = lua.globals();
        let fx = Effects::new(policy.clone(), host.as_ref().map(|h| h.depth).unwrap_or(0), replay, deadline.clone());

        // --- session-side builtins (visible to the model) ---
        let session_env = lua.create_table()?;
        let buf = out.clone();
        g.set("__emit", lua.create_function(move |_, s: mlua::LuaString| {
            let mut b = buf.lock().unwrap();
            b.push_str(&s.to_string_lossy());
            b.push('\n');
            Ok(())
        })?)?;
        // The printer is the std/printer module's `top` (hot-reloadable); fall back to tostring if it fails.
        let printer: Arc<Mutex<Option<Function>>> = Arc::new(Mutex::new(None));
        let require_cell: Arc<Mutex<Option<Function>>> = Arc::new(Mutex::new(None));
        let (pf, rc) = (printer.clone(), require_cell.clone());
        g.set("__format", lua.create_function(move |lua, v: Value| {
            if pf.lock().unwrap().is_none() {
                let req = rc.lock().unwrap().clone();
                if let Some(req) = req
                    && let Ok(Value::Table(m)) = req.call::<Value>("std/printer")
                        && let Ok(f) = m.get::<Function>("top") {
                            *pf.lock().unwrap() = Some(f);
                        }
            }
            let f = pf.lock().unwrap().clone();
            if let Some(f) = f
                && let Ok(s) = f.call::<String>(v.clone()) {
                    return Ok(s);
                }
            Ok(lua.coerce_string(v.clone())?.map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| format!("<{}>", v.type_name())))
        })?)?;
        let env_for_vars = session_env.clone();
        let exposed: Arc<Mutex<std::collections::HashSet<String>>> = Arc::new(Mutex::new(Default::default()));
        let ex_vars = exposed.clone();
        g.set("vars", lua.create_function(move |lua, ()| {
            let t = lua.create_table()?;
            for (k, v) in env_for_vars.pairs::<Value, Value>().flatten() {
                if matches!(&k, Value::String(s) if !s.to_string_lossy().starts_with('_') && !ex_vars.lock().unwrap().contains(&*s.to_string_lossy())) {
                    t.set(k, v)?;
                }
            }
            Ok(t)
        })?)?;
        let pfx = fx.clone();
        g.set("parallel", lua.create_async_function(move |lua, fns: Table| {
            let pfx = pfx.clone();
            async move {
                // each branch is its own Lua thread with its own replay queue: "0.1", "0.2", ... under the caller's id
                let parent = pfx.tid(&lua);
                let mut threads = Vec::new();
                for (i, f) in fns.sequence_values::<Function>().enumerate() {
                    let th = lua.create_thread(f?)?;
                    pfx.register_thread(&th, format!("{parent}.{}", i + 1));
                    threads.push(th);
                }
                let handles: Vec<_> = threads.to_vec();
                let futs: Vec<_> = threads.into_iter().map(|t| t.into_async::<MultiValue>(())).collect::<mlua::Result<_>>()?;
                let results = join_all(futs).await;
                for th in &handles {
                    pfx.unregister_thread(th);
                }
                let out = lua.create_table()?;
                for (i, r) in results.into_iter().enumerate() {
                    match r {
                        Ok(vals) => out.set(i + 1, vals.into_iter().next().unwrap_or(Value::Nil))?,
                        Err(e) => {
                            let t = lua.create_table()?;
                            t.set("error", clean_error(&e))?;
                            out.set(i + 1, t)?;
                        }
                    }
                }
                Ok(out)
            }
        })?)?;
        if let Some(h) = host.clone() {
            let afx = fx.clone();
            g.set("agent", lua.create_async_function(move |lua, (task, opts): (String, Option<Table>)| {
                let (h, afx) = (h.clone(), afx.clone());
                async move {
                    if h.depth >= h.max_depth {
                        return Err(mlua::Error::runtime(format!("agent depth limit ({}) reached", h.max_depth)));
                    }
                    let turns = opts.as_ref().and_then(|o| o.get::<Option<usize>>("max_turns").ok().flatten()).unwrap_or(12);
                    let args = serde_json::json!({"task_sha": crate::eventlog::sha_bytes(task.as_bytes()), "preview": task.chars().take(120).collect::<String>(), "max_turns": turns});
                    // a subagent is an effect: replayed from the log on resume, never re-run
                    let ret = afx.run(&lua, "agent", Class::LocalWrite, args, || async {
                        let child = h.child();
                        child.log(&format!("### SUBAGENT (depth {})\n{task}", child.depth));
                        let rep = child.run_task(task.clone(), turns).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
                        Ok(Ret::Agent { answer: rep["final_answer"].as_str().unwrap_or("").to_string(), finished: rep["finished"].as_bool().unwrap_or(false), turns: rep["turns"].as_u64().unwrap_or(0) })
                    }).await?;
                    match ret {
                        Ret::Agent { answer, finished, turns } => {
                            let t = lua.create_table()?;
                            t.set("answer", answer)?;
                            t.set("finished", finished)?;
                            t.set("turns", turns)?;
                            Ok(t)
                        }
                        r => Err(prims::bad(r)),
                    }
                }
            })?)?;
        }
        prims::install(&lua, fx.clone(), ops.clone())?;

        // ---- MCP: servers from .harness/mcp.toml; every list/call is a recorded, replayable effect ----
        let mcp = crate::mcp::Mcp::load(policy.clone())?;
        if let Some(m) = mcp.clone() {
            let (lfx, lm) = (fx.clone(), m.clone());
            g.set("__mcp_list", lua.create_async_function(move |lua, server: String| {
                let (fx, m) = (lfx.clone(), lm.clone());
                async move {
                    let ret = fx.run(&lua, "mcp_list", Class::Read, json!({"server": server}), || async {
                        let r = m.request(&server, "tools/list", json!({})).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
                        Ok(Ret::Json(r["tools"].clone()))
                    }).await?;
                    prims::json_value(&lua, ret)
                }
            })?)?;
            let (cfx, cm) = (fx.clone(), m.clone());
            g.set("__mcp_call", lua.create_async_function(move |lua, (server, tool, args): (String, String, Value)| {
                let (fx, m) = (cfx.clone(), cm.clone());
                async move {
                    let args_json: serde_json::Value = lua.from_value(args)?;
                    let class = if m.is_read_only(&server, &tool) { Class::Read } else { Class::External };
                    let text = args_json.to_string();
                    let a = json!({"server": server, "tool": tool, "args": args_json});
                    let ret = fx.run(&lua, "mcp_call", class, a, || async {
                        let cmd = format!("mcp {server}.{tool} {}", text.chars().take(500).collect::<String>());
                        if let Err(why) = fx.pol.approve(&cmd, class).await {
                            return Err(mlua::Error::runtime(format!("approval_required: {why}")));
                        }
                        let r = m.request(&server, "tools/call", json!({"name": tool, "arguments": args_json})).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
                        Ok(Ret::Json(r))
                    }).await?;
                    prims::json_value(&lua, ret)
                }
            })?)?;
        }
        let mcp_names: Vec<String> = mcp.as_ref().map(|m| m.servers.keys().cloned().collect()).unwrap_or_default();
        lua.load(PRELUDE).set_name("=prelude").exec()?;

        lua.sandbox(true)?;
        lua.set_memory_limit(mem_limit)?;
        let (dl, waiting) = (deadline.clone(), fx.waiting.clone());
        lua.set_interrupt(move |lua| {
            {
                let w = waiting.lock().unwrap();
                if !w.is_empty() && w.contains_key(&(lua.current_thread().to_pointer() as usize)) {
                    return Ok(VmState::Continue); // this thread awaits sh/agent/mcp: their own timeouts apply
                }
            }
            if let Some(t) = *dl.lock().unwrap()
                && Instant::now() > t {
                    return Err(mlua::Error::runtime("time budget exceeded (infinite loop?)"));
                }
            Ok(VmState::Continue)
        });

        let gmt = lua.create_table()?;
        gmt.set("__index", lua.globals())?;
        session_env.set_metatable(Some(gmt.clone()))?;

        // --- module env: require + hx API (not visible to the model's session) ---
        let modenv = lua.create_table()?;
        modenv.set_metatable(Some(gmt))?;
        let loaded = lua.create_table()?;
        let hx_cell: Arc<Mutex<Option<Table>>> = Arc::new(Mutex::new(None));
        let pkg_envs: Arc<Mutex<HashMap<String, Table>>> = Arc::new(Mutex::new(HashMap::new()));
        let (res, menv, ld, hxc, pe, rc2) = (resolver.clone(), modenv.clone(), loaded.clone(), hx_cell.clone(), pkg_envs.clone(), require_cell.clone());
        let require = lua.create_function(move |lua, name: String| {
            let cached: Value = ld.get(name.as_str())?;
            if !cached.is_nil() {
                return Ok(cached);
            }
            let load = |src: &crate::modules::Source| -> mlua::Result<Value> {
                let env = match &src.pkg {
                    Some((pkg, caps)) => {
                        let cached = pe.lock().unwrap().get(pkg).cloned();
                        match cached {
                            Some(e) => e,
                            None => {
                                let hx = hxc.lock().unwrap().clone().ok_or_else(|| mlua::Error::runtime("hx api not ready"))?;
                                let req = rc2.lock().unwrap().clone().ok_or_else(|| mlua::Error::runtime("require not ready"))?;
                                let e = package_env(lua, pkg, caps, &req, &hx)?;
                                pe.lock().unwrap().insert(pkg.clone(), e.clone());
                                e
                            }
                        }
                    }
                    None => menv.clone(),
                };
                let f = lua.load(&src.text).set_name(format!("={name}")).set_environment(env).into_function()?;
                f.call(())
            };
            let src = res.resolve(&name).map_err(|e| mlua::Error::runtime(e.to_string()))?;
            let v = match load(&src) {
                // A project override must load and return a table (every harness module does); otherwise fall back
                // to the built-in module so a typo in an override cannot take the harness down.
                Ok(v @ Value::Table(_)) => v,
                bad if src.is_override => {
                    let why = match bad {
                        Err(e) => e.to_string().lines().next().unwrap_or("").to_string(),
                        Ok(v) => format!("returned {} instead of a table", v.type_name()),
                    };
                    eprintln!("warning: override {} is broken ({why}); using the built-in {name}", src.origin);
                    let base = res.resolve(&format!("base:{name}")).map_err(|e| mlua::Error::runtime(e.to_string()))?;
                    load(&base)?
                }
                other => other?,
            };
            ld.set(name.as_str(), v.clone())?;
            Ok(v)
        })?;
        *require_cell.lock().unwrap() = Some(require.clone());
        modenv.set("require", require)?;
        // hot reload: drop a module from the cache; the next require loads the current source. Session variables survive.
        let (ld, pf) = (loaded.clone(), printer.clone());
        modenv.set("reload", lua.create_function(move |_, name: String| {
            ld.set(name.as_str(), Value::Nil)?;
            if name == "std/printer" {
                *pf.lock().unwrap() = None;
            }
            Ok(())
        })?)?;
        modenv.set("print", lua.create_function(stderr_print)?)?;

        let hx = lua.create_table()?;
        // A pack adds functions/tables to the model's session (not to `vars()`): hx.expose("tests", tests)
        let (sess_env, ex_set) = (session_env.clone(), exposed.clone());
        hx.set("expose", lua.create_function(move |_, (name, v): (String, Value)| {
            sess_env.raw_set(name.as_str(), v)?;
            ex_set.lock().unwrap().insert(name);
            Ok(())
        })?)?;
        let session = Session { lua: lua.clone(), log: policy.log.clone(), depth: fx.depth, env: session_env, out, ops, deadline, budget };
        let s = session.clone();
        hx.set("run", lua.create_async_function(move |lua, code: String| {
            let s = s.clone();
            async move {
                let r = s.run(&code).await;
                s.log.write("chunk", json!({"code_sha256": crate::eventlog::sha_bytes(code.as_bytes()), "code_bytes": code.len(), "ok": r.error.is_none(), "ops": r.ops, "depth": s.depth}), false);
                let t = lua.create_table()?;
                t.set("output", r.output)?;
                t.set("values", lua.create_sequence_from(r.values)?)?;
                t.set("error", r.error)?;
                t.set("ops", r.ops)?;
                Ok(t)
            }
        })?)?;
        // hx.view_image(path, max) -> { path, width, height, orig_width, orig_height, mime, data } for the vision pack.
        // Recorded like `read` (path + sha256 + size); on replay the file is read again and must still match.
        let vfx = fx.clone();
        hx.set("view_image", lua.create_function(move |lua, (path, max): (String, Option<u32>)| {
            let max = max.unwrap_or(1024).clamp(64, 2048);
            let pol = vfx.pol.clone();
            let mut live: Option<crate::prims::ViewImage> = None;
            let rec = vfx.sync(lua, "view", Class::Read, json!({"path": &path, "max": max}), || {
                let v = crate::prims::load_view_image(&pol, &path, max).map_err(mlua::Error::runtime)?;
                let meta = json!({"sha256": v.sha256, "width": v.width, "height": v.height}).to_string();
                live = Some(v);
                Ok(Ret::Str(meta))
            })?;
            let v = match live {
                Some(v) => v,
                None => {
                    let v = crate::prims::load_view_image(&pol, &path, max).map_err(mlua::Error::runtime)?;
                    let want = match &rec { Ret::Str(s) => serde_json::from_str::<serde_json::Value>(s).ok().and_then(|m| m["sha256"].as_str().map(String::from)), _ => None };
                    if want.as_deref() != Some(v.sha256.as_str()) {
                        return Err(mlua::Error::runtime(format!("view {path}: replay diverged (the file changed since it was recorded)")));
                    }
                    v
                }
            };
            let t = lua.create_table()?;
            t.set("path", path)?;
            t.set("width", v.width)?;
            t.set("height", v.height)?;
            t.set("orig_width", v.orig.0)?;
            t.set("orig_height", v.orig.1)?;
            t.set("mime", v.mime)?;
            t.set("data", v.data_b64)?;
            Ok(t)
        })?)?;
        let h2 = host.clone();
        hx.set("log", lua.create_function(move |_, line: String| {
            match &h2 {
                Some(h) => h.log(&line),
                None => eprintln!("{line}"),
            }
            Ok(())
        })?)?;
        let rroot = root.clone();
        hx.set("asset", lua.create_function(move |_, name: String| {
            // "std/system.md": project override first, then the embedded file
            let (pkg, file) = name.split_once('/').unwrap_or(("std", &name));
            let over = rroot.join(".harness/override").join(pkg).join(file);
            if let Some(t) = std::fs::read_to_string(&over).ok().filter(|_| crate::trust::is_trusted(&rroot)) {
                return Ok(t);
            }
            Resolver::embedded(pkg, file).map(String::from).ok_or_else(|| mlua::Error::runtime(format!("no asset {name}")))
        })?)?;
        // The file listing is an environment read: recorded, so a resumed run builds the identical first message.
        let (troot, tfx) = (root.clone(), fx.clone());
        hx.set("tree", lua.create_function(move |lua, ()| {
            match tfx.sync(lua, "tree", Class::Read, serde_json::json!({}), || Ok(Ret::Str(host::file_tree(&troot))))? {
                Ret::Str(s) => Ok(s),
                r => Err(prims::bad(r)),
            }
        })?)?;
        if let Some(h) = host {
            let bh = h.clone();
            hx.set("budget", lua.create_function(move |lua, ()| lua.to_value(&bh.budget_status()))?)?;
            let mh = h.clone();
            hx.set("model_info", lua.create_function(move |lua, ()| lua.to_value(&mh.model_info()))?)?;
            hx.set("llm", lua.create_async_function(move |lua, req: Value| {
                let h = h.clone();
                async move {
                    let json: serde_json::Value = lua.from_value(req)?;
                    let r = h.chat(&json).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
                    let t = lua.create_table()?;
                    t.set("text", r.text)?;
                    t.set("calls", lua.to_value(&r.calls)?)?;
                    t.set("raw", r.raw)?;
                    t.set("finish", r.finish)?;
                    t.set("input", r.input)?;
                    t.set("output", r.output)?;
                    t.set("cache_read", r.cache_read)?;
                    t.set("cache_write", r.cache_write)?;
                    t.set("cost", r.cost)?;
                    Ok(t)
                }
            })?)?;
        }
        // ---- extension API: installed packages, agent-defined tools, staging for promotion ----
        let pres = resolver.clone();
        hx.set("mcp_servers", lua.create_function(move |lua, ()| lua.create_sequence_from(mcp_names.clone()))?)?;
        hx.set("packages", lua.create_function(move |lua, ()| lua.to_value(&pres.list_packages()))?)?;
        let defs: Arc<Mutex<HashMap<String, ToolDef>>> = Arc::new(Mutex::new(HashMap::new()));
        let (d_defs, d_hx, d_req, d_env, d_ex) = (defs.clone(), hx.clone(), require_cell.clone(), session.env.clone(), exposed.clone());
        let deftool = lua.create_function(move |lua, (name, source, opts): (String, String, Option<Table>)| {
            if !valid_name(&name) {
                return Err(mlua::Error::runtime("deftool: the name must be lowercase letters, digits and _ (starting with a letter)"));
            }
            let caps = caps_of(&opts)?;
            let req = d_req.lock().unwrap().clone().ok_or_else(|| mlua::Error::runtime("deftool: harness not ready"))?;
            // the tool runs with ONLY the capabilities it declares (default none), exactly as it will once promoted
            let env = package_env(lua, &name, &caps, &req, &d_hx)?;
            let module: Value = lua.load(&source).set_name(format!("=tool:{name}")).set_environment(env).into_function()?.call(())?;
            if !matches!(module, Value::Table(_)) {
                return Err(mlua::Error::runtime("deftool: the source must `return` a table of functions"));
            }
            d_defs.lock().unwrap().insert(name.clone(), ToolDef { source, doc: opt_string(&opts, "doc"), signature: opt_string(&opts, "signature"), types: opt_string(&opts, "types"), tests: opt_string(&opts, "tests"), caps });
            let tools = match d_env.raw_get::<Value>("tools")? {
                Value::Table(t) => t,
                _ => {
                    let t = lua.create_table()?;
                    d_env.raw_set("tools", t.clone())?;
                    d_ex.lock().unwrap().insert("tools".into());
                    t
                }
            };
            tools.raw_set(name.as_str(), module.clone())?;
            Ok(module)
        })?;
        let (p_defs, p_res, p_fx, p_ws) = (defs.clone(), resolver.clone(), fx.clone(), root.clone());
        let promote = lua.create_async_function(move |lua, name: String| {
            let (defs, res, fx, ws) = (p_defs.clone(), p_res.clone(), p_fx.clone(), p_ws.clone());
            async move {
                let def = defs.lock().unwrap().get(&name).cloned().ok_or_else(|| mlua::Error::runtime(format!("promote: no tool named {name:?}; define it with deftool() first")))?;
                let args = json!({"name": name, "source_sha": crate::eventlog::sha_bytes(def.source.as_bytes()), "tests_sha": crate::eventlog::sha_bytes(def.tests.as_bytes())});
                let ret = fx.run(&lua, "promote", Class::LocalWrite, args, || async {
                    if def.tests.trim().is_empty() {
                        return Ok(Ret::Json(json!({"ok": false, "message": "a tool must have tests to be promoted: pass opts.tests to deftool()"})));
                    }
                    let types = format!("-- {}\n-- {}\n{}\n", def.signature, def.doc, def.types);
                    let files = vec![
                        ("hx.toml".to_string(), render_manifest(&name, &def)),
                        ("init.luau".to_string(), def.source.clone()),
                        ("types.d.luau".to_string(), types),
                        ("tests/t.luau".to_string(), def.tests.clone()),
                    ];
                    let dir = res.stage(&name, &files).map_err(|e| mlua::Error::runtime(e.to_string()))?;
                    let exe = std::env::current_exe().map_err(|e| mlua::Error::runtime(e.to_string()))?;
                    let out = tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        tokio::process::Command::new(exe).arg("test").arg(&dir).arg("--workspace").arg(&ws).stdin(std::process::Stdio::null()).output(),
                    )
                    .await;
                    let (ok, text) = match out {
                        Ok(Ok(o)) => (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
                        Ok(Err(e)) => (false, e.to_string()),
                        Err(_) => (false, "tests timed out after 60s".into()),
                    };
                    if !ok {
                        let _ = res.reject(&name); // a tool whose tests fail is not left staged
                    }
                    let tail: String = text.chars().rev().take(1500).collect::<Vec<_>>().into_iter().rev().collect();
                    Ok(Ret::Json(json!({
                        "ok": ok,
                        "message": if ok { format!("staged tool '{name}'. It is NOT active yet: the user must review and run `hx accept {name}`.") } else { "the tool's tests failed; nothing was staged".to_string() },
                        "output": tail,
                    })))
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?;
        let (u_res, u_fx) = (resolver.clone(), fx.clone());
        let propose_module = lua.create_async_function(move |lua, (module, source, opts): (String, String, Option<Table>)| {
            let (res, fx) = (u_res.clone(), u_fx.clone());
            async move {
                let reason = opt_string(&opts, "reason");
                let args = json!({"module": module, "source_sha": crate::eventlog::sha_bytes(source.as_bytes()), "reason": reason});
                let ret = fx.run(&lua, "propose_module", Class::LocalWrite, args, || async {
                    Ok(match res.propose(&module, &source, &reason) {
                        Ok(id) => Ret::Json(json!({"ok": true, "id": id, "message": format!("proposal {id} staged. It is NOT active: it must pass the eval gate and the user must run `hx upgrade accept {id}`.")})),
                        Err(e) => Ret::Json(json!({"ok": false, "message": e.to_string()})),
                    })
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?;
        for (n, f) in [("deftool", deftool), ("promote", promote), ("propose_module", propose_module)] {
            session.env.raw_set(n, f)?;
            exposed.lock().unwrap().insert(n.into());
        }
        *hx_cell.lock().unwrap() = Some(hx.clone());
        modenv.set("hx", hx)?;
        Ok(Vm { lua, session, modenv })
    }

    /// Run the std loop (or the project's override of it) and return its report.
    pub async fn run_loop(&self, task: &str, max_turns: usize, packs: &[String]) -> Result<serde_json::Value> {
        let f = self
            .lua
            .load("local cfg = ...\nreturn require('std/loop').run(cfg)")
            .set_name("=host")
            .set_environment(self.modenv.clone())
            .into_function()?;
        let cfg = self.lua.create_table()?;
        cfg.set("task", task)?;
        cfg.set("max_turns", max_turns)?;
        cfg.set("packs", self.lua.create_sequence_from(packs.iter().cloned())?)?;
        let rep: Value = f.call_async(cfg).await?;
        Ok(self.lua.from_value(rep)?)
    }

    /// Install packs (`require(name).install(ctx)`) the way the loop does, for `hx exec --pack`.
    pub async fn install_packs(&self, packs: &[String]) -> Result<()> {
        let f = self
            .lua
            .load("for _, name in {...} do require(name).install({ hooks = require('std/hooks') }) end")
            .set_name("=host")
            .set_environment(self.modenv.clone())
            .into_function()?;
        f.call_async::<()>(MultiValue::from_iter(packs.iter().map(|p| Value::String(self.lua.create_string(p).unwrap())))).await?;
        Ok(())
    }

    /// Run a chunk in the session and render it exactly as the loop would (std/feedback).
    pub async fn exec_text(&self, code: &str) -> Result<String> {
        let r = self.session.run(code).await;
        let t = self.lua.create_table()?;
        t.set("output", r.output)?;
        t.set("values", self.lua.create_sequence_from(r.values)?)?;
        t.set("error", r.error.clone())?;
        t.set("ops", r.ops)?;
        let f = self
            .lua
            .load("local res, code = ...\nreturn require('std/feedback').compose(res, code)")
            .set_name("=host")
            .set_environment(self.modenv.clone())
            .into_function()?;
        let text: String = f.call_async((t, code.to_string())).await?;
        Ok(format!("{text}-- ops={} error={}", r.ops, r.error.is_some()))
    }

    /// Run every tests/*.luau file of a package directory; returns (passed, failed, report).
    pub async fn run_tests(&self, pkg_dir: &std::path::Path) -> Result<(usize, usize, String)> {
        let testlib: Table = self.modenv.get::<Function>("require")?.call("std/testlib")?;
        self.modenv.set("test", testlib.clone())?;
        let mut files: Vec<_> = std::fs::read_dir(pkg_dir.join("tests"))?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "luau")).collect();
        files.sort();
        let (mut pass, mut fail, mut report) = (0, 0, String::new());
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            let src = std::fs::read_to_string(&f)?;
            testlib.set("results", self.lua.create_table()?)?;
            let f = self.lua.load(&src).set_name(format!("={name}")).set_environment(self.modenv.clone()).into_function();
            let res = match f {
                Ok(f) => f.call_async::<()>(()).await,
                Err(e) => Err(e),
            };
            if let Err(e) = res {
                fail += 1;
                report.push_str(&format!("FAIL {name}: load error: {e}\n"));
                continue;
            }
            let results: Table = testlib.get("results")?;
            for r in results.sequence_values::<Table>().flatten() {
                let (case, ok): (String, bool) = (r.get("name")?, r.get("ok")?);
                if ok {
                    pass += 1;
                } else {
                    fail += 1;
                    report.push_str(&format!("FAIL {name}: {case}: {}\n", r.get::<String>("err").unwrap_or_default()));
                }
            }
        }
        Ok((pass, fail, report))
    }
}
