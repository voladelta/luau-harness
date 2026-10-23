//! Luau VM. Bindings load, then sandbox(true). Two environments sit on the read-only globals:
//!   * the SESSION env: the model's persistent variables (what `vars()` lists);
//!   * the MODULE env: where harness modules (std loop, printer, ...) run, with `require` and the `hx` API.
use crate::effects::{Effects, Ret};
use crate::host::Host;
use crate::modules::Resolver;
use crate::policy::{Class, Policy};
use crate::prims;
use anyhow::Result;
use serde_json::json;
use mlua::{Function, Lua, MultiValue, Table, Value, Variadic, VmState};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

/// Luau CPU allowance of one agent chunk (time waiting on effects does not count).
pub const CHUNK_SECS: u64 = 120;

pub(crate) const PRELUDE: &str = r##"
function print(...)
	local n = select("#", ...)
	local parts = table.create(n)
	for i = 1, n do
		parts[i] = __format((select(i, ...)))
	end
	__emit(table.concat(parts, "\t"))
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

#[cfg(test)]
mod session_budget_tests {
    use super::*;

    #[tokio::test]
    async fn awaiting_an_effect_does_not_spend_the_luau_cpu_allowance() {
        let dir = std::env::temp_dir().join(format!("hx-chunk-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Arc::new(Policy::new(&dir, false, None, None, None).unwrap());
        let vm = Vm::new(policy, Duration::from_millis(30), 32 << 20, None).unwrap();
        let waited = vm.session.run("local r=sh('sleep 0.1; echo waited'); assert(r.code==0); print(r.stdout)").await;
        assert!(waited.error.is_none(), "{:?}", waited.error);
        assert!(waited.output.contains("waited"));
        let spun = vm.session.run("sh('sleep 0.1'); while true do end").await;
        assert!(spun.error.as_deref().is_some_and(|e| e.contains("time budget exceeded")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn isolated_package_wait_preserves_the_callers_cpu_allowance() {
        let dir = std::env::temp_dir().join(format!("hx-package-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Arc::new(Policy::new(&dir, false, None, None, None).unwrap());
        let vm = Vm::new(policy.clone(), Duration::from_millis(30), 32 << 20, None).unwrap();
        let mut registry = crate::modules::Registry::capture(&dir).unwrap();
        let caps = BTreeMap::from([("proc".into(), vec!["sh".into()])]);
        registry.capabilities.insert("probe".into(), caps.clone());
        registry.sources.insert("probe".into(), crate::modules::Source {
            text: "return {run=function() return sh('sleep 0.1; echo waited').stdout end}".into(),
            origin: "probe/init.luau".into(), is_override: false, pkg: Some(("probe".into(), caps)),
        });
        let supervisor = crate::supervisor::Supervisor::ephemeral(policy, Arc::new(registry));
        let caller = vm.effects.clone();
        let proxy = crate::packages::proxy(&vm.lua, supervisor, None, "probe", caller).unwrap();
        vm.session.env.set("probe", proxy).unwrap();
        let waited = vm.session.run("assert(probe.run():find('waited')); print('ok')").await;
        assert!(waited.error.is_none(), "{:?}", waited.error);
        let spun = vm.session.run("probe.run(); while true do end").await;
        assert!(spun.error.as_deref().is_some_and(|e| e.contains("time budget exceeded")));
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The model's persistent environment plus the budgets that apply to each chunk.
pub struct Session {
    lua: Lua,
    log: Arc<crate::eventlog::EventLog>,
    env: Table,
    out: Arc<Mutex<String>>,
    ops: Arc<AtomicUsize>,
    deadline: Arc<Mutex<Option<Instant>>>,
    budget: Duration,
    /// chunks run so far (see `hx.retain`)
    chunks: Arc<AtomicU64>,
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
        self.chunks.fetch_add(1, Ordering::Relaxed);
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
    tool_prompt: String,
    #[cfg(test)]
    effects: Arc<Effects>,
}

impl Vm {
    pub fn new(policy: Arc<Policy>, budget: Duration, mem_limit: usize, host: Option<Arc<Host>>) -> Result<Vm> {
        let root: PathBuf = policy.workspace.clone();
        let lua = Lua::new();
        let out = Arc::new(Mutex::new(String::new()));
        let ops = Arc::new(AtomicUsize::new(0));
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let resolver = Arc::new(Resolver::new(root.clone()));
        let supervisor = match &host {
            Some(h) => h.supervisor.clone(),
            None => {
                let mut registry = crate::modules::Registry::capture(&root)?;
                registry.materialize_skills(&policy.log)?;
                crate::supervisor::Supervisor::ephemeral(policy.clone(), Arc::new(registry))
            },
        };
        let registry = supervisor.registry.clone();
        let g = lua.globals();
        let principal = host.as_ref().map(|h| h.principal.clone()).unwrap_or(crate::supervisor::Principal::Agent { read_only: false });
        let fx = Effects::supervised(supervisor.clone(), principal, host.as_ref().map(|h| h.depth).unwrap_or(0), deadline.clone());

        // --- session-side builtins (visible to the model) ---
        let session_env = lua.create_table()?;
        let buf = out.clone();
        g.set("__emit", lua.create_function(move |_, s: mlua::LuaString| {
            let mut b = buf.lock().unwrap();
            b.push_str(&s.to_string_lossy());
            b.push('\n');
            Ok(())
        })?)?;
        // Use the frozen std/printer module; fall back to tostring if it fails.
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
        if let Some(h) = host.clone().filter(|h| h.depth == 0) {
            let hfx = fx.clone();
            let hops = ops.clone();
            g.set("history", lua.create_async_function(move |lua, input: Option<Value>| {
                let (h, fx, ops) = (h.clone(), hfx.clone(), hops.clone());
                async move {
                    let args = input.map(crate::values::copy_out).transpose()?.unwrap_or(json!({}));
                    let query = crate::history::Query::parse(args.clone()).map_err(mlua::Error::external)?;
                    ops.fetch_add(1, Ordering::Relaxed);
                    let result = fx.run(&lua, crate::supervisor::Tool::History, Class::Read, args, || async move {
                        h.policy.check_read(&h.policy.log.path.to_string_lossy()).map_err(mlua::Error::external)?;
                        let deadline = h.supervisor.manifest.as_ref().and_then(|m| m.deadline);
                        let value = tokio::task::spawn_blocking(move || h.supervisor.history.query(
                            h.policy.log.dir(), &h.policy.log.session, query, deadline)).await
                            .map_err(mlua::Error::external)?.map_err(mlua::Error::external)?;
                        Ok(Ret::Json(value))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
        }
        let (bs, bh, bfx) = (supervisor.clone(), host.clone(), fx.clone());
        g.set("batch", lua.create_async_function(move |lua, input: Value| {
            let (s, h, fx) = (bs.clone(), bh.clone(), bfx.clone());
            async move {
                s.authorize_stage(fx.stage_epoch,crate::supervisor::Tool::Read,Class::Read).map_err(mlua::Error::external)?;
                let _active = crate::workflow_runtime::EffectGuard::new(&s);
                let operations = crate::values::copy_out(input)?;
                let _wait = fx.awaiting(&lua);
                let results = crate::batch::run(s.clone(), h, operations).await.map_err(mlua::Error::external)?;
                crate::values::copy_in(&lua, &results)
            }
        })?)?;
        if host.as_ref().is_some_and(|h|h.is_workflow_root()) {
            let stage = lua.create_table()?;
            let frame = supervisor.workflow.lock().unwrap().clone().unwrap();
            if let Some(active) = frame.stage.as_ref().map(|a| &a.spec) {
                stage.set("id",active.id.clone())?;
                stage.set("name",active.name.clone())?;
                stage.set("checks",crate::values::copy_in(&lua,&serde_json::to_value(&frame.check_plan).map_err(mlua::Error::external)?)?)?;
            }
            let owner = supervisor.clone();
            let epoch = supervisor.workflow_epoch();
            stage.set("submit",lua.create_function(move |_,value:Value| {
                let payload=crate::values::copy_out(value)?;
                owner.submit_stage(epoch,payload).map_err(mlua::Error::external)
            })?)?;
            stage.set_readonly(true);
            g.set("stage",stage)?;
        }
        if policy.episode.is_some() && host.as_ref().is_some_and(|h| h.depth == 0) {
            // ar.eval(note) is the only way an autoresearch episode scores a state, ar.claim(idea) claims what a worker tests,
            // and ar.reset() restores the worker's best state (see ar.rs).
            let ar = lua.create_table()?;
            let afx = fx.clone();
            ar.set("eval", lua.create_async_function(move |lua, note: Option<String>| {
                let fx = afx.clone();
                async move {
                    let note = note.unwrap_or_default();
                    let result = fx.run(&lua, crate::supervisor::Tool::Sh, Class::LocalWrite, json!({"ar_eval": &note}), || async {
                        crate::ar::eval(&fx.pol, note.clone()).await.map(Ret::Json).map_err(|e| crate::effects::rt(format!("ar.eval: {e}")))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
            let afx = fx.clone();
            let ahost = host.clone().expect("autoresearch episodes run at depth 0 with a host");
            ar.set("claim", lua.create_async_function(move |lua, idea: String| {
                let (fx, host) = (afx.clone(), ahost.clone());
                async move {
                    let result = fx.run(&lua, crate::supervisor::Tool::Sh, Class::LocalWrite, json!({"ar_claim": &idea}), || async {
                        crate::ar::claim(&host, idea.clone()).await.map(Ret::Json).map_err(|e| crate::effects::rt(format!("ar.claim: {e}")))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
            let afx = fx.clone();
            let cpol = policy.clone();
            ar.set("claims", lua.create_function(move |lua, ()| {
                let listed = crate::ar::claims(&cpol).map_err(|e| crate::effects::rt(format!("ar.claims: {e}")))?;
                prims::json_value(lua, Ret::Json(listed))
            })?)?;
            ar.set("reset", lua.create_async_function(move |lua, ()| {
                let fx = afx.clone();
                async move {
                    let result = fx.run(&lua, crate::supervisor::Tool::Sh, Class::LocalWrite, json!({"ar_reset": true}), || async {
                        crate::ar::reset(&fx.pol).map(Ret::Json).map_err(|e| crate::effects::rt(format!("ar.reset: {e}")))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
            ar.set_readonly(true);
            g.set("ar", ar)?;
        }
        let saved = host.as_ref().map(|h| h.durable.clone()).unwrap_or_else(|| supervisor.durable.clone());
        let durable = lua.create_table()?;
        let data = saved.clone();
        durable.set("get", lua.create_function(move |lua, key: String| {
            match data.lock().unwrap().get(&key) {
                Some(value) => crate::values::copy_in(lua, value),
                None => Ok(Value::Nil),
            }
        })?)?;
        let stage_owner=supervisor.clone();
        let stage_epoch=supervisor.workflow_epoch();
        durable.set("set", lua.create_function(move |_, (key, value): (String, Value)| {
            stage_owner.authorize_stage(stage_epoch,crate::supervisor::Tool::Read,Class::Read).map_err(mlua::Error::external)?;
            let value = crate::values::copy_out(value)?;
            let mut saved = saved.lock().unwrap();
            let mut candidate = saved.clone();
            candidate.as_object_mut().unwrap().insert(key, value);
            crate::values::validate(&candidate)?;
            *saved = candidate;
            Ok(())
        })?)?;
        durable.set_readonly(true);
        g.set("durable", durable)?;
        if let Some(h) = host.clone() {
            let afx = fx.clone();
            g.set("agent", lua.create_async_function(move |lua, (task, opts): (String, Option<Table>)| {
                let (h, afx) = (h.clone(), afx.clone());
                async move {
                    let options = crate::host::AgentOptions::from_lua(opts)?;
                    let child = h.child(&options, afx.principal.clone()).map_err(mlua::Error::external)?;
                    let args = serde_json::json!({"task_sha": crate::eventlog::sha_bytes(task.as_bytes()), "preview": task.chars().take(120).collect::<String>(), "max_turns": options.max_turns, "model": options.model, "effort": options.effort});
                    // A subagent records an intent and receipt; recovery never redispatches it.
                    let ret = afx.run(&lua, crate::supervisor::Tool::Agent, Class::LocalWrite, args, || async {
                        child.log(&format!("### SUBAGENT (depth {}, model {}, effort {})\n{task}", child.depth, child.llm.model, child.llm.reasoning.as_deref().unwrap_or("default")));
                        let rep = child.run_task(task.clone(), options.max_turns).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
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

        // ---- MCP: frozen server configuration; every list/call records a receipt ----
        let mcp = crate::mcp::Mcp::frozen(policy.clone(), registry.mcp.clone())?;
        if let Some(m) = mcp.clone() {
            let (lfx, lm) = (fx.clone(), m.clone());
            g.set("__mcp_list", lua.create_async_function(move |lua, server: String| {
                let (fx, m) = (lfx.clone(), lm.clone());
                async move {
                    let ret = fx.run(&lua, crate::supervisor::Tool::McpList, Class::Read, json!({"server": server}), || async {
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
                    let args_json = crate::values::copy_out(args)?;
                    let class = if m.is_read_only(&server, &tool) { Class::Read } else { Class::External };
                    let text = args_json.to_string();
                    let a = json!({"server": server, "tool": tool, "args": args_json});
                    let ret = fx.run(&lua, crate::supervisor::Tool::McpCall, class, a, || async {
                        let cmd = format!("mcp {server}.{tool} {}", text.chars().take(500).collect::<String>());
                        if let Err(why) = fx.pol.approve(&cmd, class).await {
                            return Err(mlua::Error::runtime(format!("approval_required: {why}")));
                        }
                        let r = m.request(&server, "tools/call", json!({"name": tool, "arguments": args_json})).await
                            .map_err(|e| if class == Class::Read { mlua::Error::runtime(e.to_string()) } else { crate::effects::unknown(format!("MCP request failed; the server may have acted: {e}")) })?;
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
        let (dl, waiting, run_supervisor) = (deadline.clone(), fx.waiting.clone(), supervisor.clone());
        lua.set_interrupt(move |lua| {
            if run_supervisor.remaining().is_some_and(|d| d.is_zero()) {
                return Err(mlua::Error::runtime("budget_exceeded: absolute run deadline reached"));
            }
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

        // mlua needs its private coroutine library to build async callbacks. Neither model code nor modules receive
        // that library (or _G, which would expose it) through their environment.
        let view = lua.create_table()?;
        for pair in g.pairs::<Value, Value>() {
            let (key, value) = pair?;
            if matches!(&key, Value::String(s) if matches!(s.to_str()?.as_ref(), "_G" | "coroutine")) { continue; }
            view.raw_set(key, value)?;
        }
        view.set_readonly(true);
        let gmt = lua.create_table()?;
        gmt.set("__index", view)?;
        gmt.set("__metatable", "locked")?;
        session_env.set_metatable(Some(gmt.clone()))?;

        // --- module env: require + hx API (not visible to the model's session) ---
        let modenv = lua.create_table()?;
        modenv.set_metatable(Some(gmt))?;
        let loaded = lua.create_table()?;
        let (res, menv, ld, supervisor_for_load, package_host, package_fx) = (registry.clone(), modenv.clone(), loaded.clone(), supervisor.clone(), host.clone(), fx.clone());
        let require = lua.create_function(move |lua, name: String| {
            let cached: Value = ld.get(name.as_str())?;
            if !cached.is_nil() {
                return Ok(cached);
            }
            let load = |src: &crate::modules::Source| -> mlua::Result<Value> {
                if let Some((pkg, _)) = &src.pkg {
                    if name != *pkg { return Err(mlua::Error::runtime("package submodules are private; call exported package methods")); }
                    return crate::packages::proxy(lua, supervisor_for_load.clone(), package_host.clone(), pkg, package_fx.clone()).map(Value::Table);
                }
                let env = menv.clone();
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
        modenv.set("print", lua.create_function(stderr_print)?)?;

        let hx = lua.create_table()?;
        // A pack adds functions/tables to the model's session (not to `vars()`): hx.expose("tests", tests)
        let (sess_env, ex_set) = (session_env.clone(), exposed.clone());
        hx.set("expose", lua.create_function(move |_, (name, v): (String, Value)| {
            sess_env.raw_set(name.as_str(), v)?;
            ex_set.lock().unwrap().insert(name);
            Ok(())
        })?)?;
        // hx.retain(v) -> n: keep a value the printer cut as `_cut[n]` in the session, so its notice can name it. The list
        // belongs to the latest chunk that cut something; the next chunk can read it until it cuts something itself.
        let chunks = Arc::new(AtomicU64::new(0));
        let (cut_env, cut_chunks, cut_seq) = (session_env.clone(), chunks.clone(), Arc::new(Mutex::new(u64::MAX)));
        hx.set("retain", lua.create_function(move |lua, v: Value| {
            let chunk = cut_chunks.load(Ordering::Relaxed);
            let mut seq = cut_seq.lock().unwrap();
            let list = match cut_env.raw_get::<Value>("_cut")? {
                Value::Table(t) if *seq == chunk => t,
                _ => { let t = lua.create_table()?; cut_env.raw_set("_cut", t.clone())?; *seq = chunk; t },
            };
            list.raw_push(v)?;
            Ok(list.raw_len())
        })?)?;
        let session = Session { lua: lua.clone(), log: policy.log.clone(), env: session_env, out, ops, deadline, budget, chunks };
        // hx.view_image(path, max) -> { path, width, height, orig_width, orig_height, mime, data } for the vision pack.
        // Record the live image identity once and return the same loaded bytes.
        let vfx = fx.clone();
        hx.set("view_image", lua.create_async_function(move |lua, (path, max): (String, Option<u32>)| {
            let vfx = vfx.clone();
            async move {
                let max = max.unwrap_or(1024).clamp(64, 2048);
                let pol = vfx.pol.clone();
                let mut live: Option<crate::prims::ViewImage> = None;
                vfx.run_sync(&lua, crate::supervisor::Tool::View, Class::Read, json!({"path": &path, "max": max}), |_| {
                    let v = crate::prims::load_view_image(&pol, &path, max).map_err(mlua::Error::runtime)?;
                    let meta = json!({"sha256": v.sha256, "width": v.width, "height": v.height}).to_string();
                    live = Some(v);
                    Ok(Ret::Str(meta))
                }).await?;
                let v = live.ok_or_else(|| mlua::Error::runtime("view executor returned no image"))?;
                let t = lua.create_table()?;
                t.set("path", path)?;
                t.set("width", v.width)?;
                t.set("height", v.height)?;
                t.set("orig_width", v.orig.0)?;
                t.set("orig_height", v.orig.1)?;
                t.set("mime", v.mime)?;
                t.set("data", v.data_b64)?;
                Ok(t)
            }
        })?)?;
        let h2 = host.clone();
        hx.set("log", lua.create_function(move |_, line: String| {
            match &h2 {
                Some(h) => h.log(&line),
                None => eprintln!("{line}"),
            }
            Ok(())
        })?)?;
        let rroot = registry.clone();
        hx.set("asset", lua.create_function(move |_, name: String| {
            rroot.asset(&name).map_err(mlua::Error::external)
        })?)?;
        if let Some(h) = host.clone() {
            let bh = h.clone();
            hx.set("budget", lua.create_function(move |lua, ()| crate::values::copy_in(lua, &bh.budget_status()))?)?;
            let mh = h.clone();
            hx.set("model_info", lua.create_function(move |lua, ()| crate::values::copy_in(lua, &mh.model_info()))?)?;
        }
        // ---- extension API: installed packages, agent-defined tools, staging for promotion ----
        let pres = registry.clone();
        hx.set("autopilot", policy.autopilot)?;
        hx.set("limits", crate::values::copy_in(&lua, &prims::prompt_limits())?)?;
        hx.set("mcp_servers", lua.create_function(move |lua, ()| lua.create_sequence_from(mcp_names.clone()))?)?;
        hx.set("packages", lua.create_function(move |lua, ()| crate::values::copy_in(lua, &serde_json::to_value(&pres.packages).map_err(mlua::Error::external)?))?)?;
        let assets = registry.clone();
        hx.set("skill_asset", lua.create_function(move |_, (name, file): (String, String)| assets.asset(&format!("{name}/{file}")).map_err(mlua::Error::external))?)?;
        let defs: Arc<Mutex<HashMap<String, ToolDef>>> = Arc::new(Mutex::new(HashMap::new()));
        let d_defs = defs.clone();
        let deftool = lua.create_function(move |lua, (name, source, opts): (String, String, Option<Table>)| {
            if !valid_name(&name) {
                return Err(mlua::Error::runtime("deftool: the name must be lowercase letters, digits and _ (starting with a letter)"));
            }
            let caps = caps_of(&opts)?;
            lua.load(&source).into_function()?; // syntax check only; unapproved source must never execute
            d_defs.lock().unwrap().insert(name.clone(), ToolDef { source, doc: opt_string(&opts, "doc"), signature: opt_string(&opts, "signature"), types: opt_string(&opts, "types"), tests: opt_string(&opts, "tests"), caps });
            let definition = lua.create_table()?;
            definition.set("name", name)?; definition.set("active", false)?;
            Ok(definition)
        })?;
        let (p_defs, p_res, p_fx) = (defs.clone(), resolver.clone(), fx.clone());
        let promote = lua.create_async_function(move |lua, name: String| {
            let (defs, res, fx) = (p_defs.clone(), p_res.clone(), p_fx.clone());
            async move {
                let def = defs.lock().unwrap().get(&name).cloned().ok_or_else(|| mlua::Error::runtime(format!("promote: no tool named {name:?}; define it with deftool() first")))?;
                let args = json!({"name": name, "source_sha": crate::eventlog::sha_bytes(def.source.as_bytes()), "tests_sha": crate::eventlog::sha_bytes(def.tests.as_bytes())});
                let ret = fx.run(&lua, crate::supervisor::Tool::Promote, Class::LocalWrite, args, || async {
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
                    lua.load(&def.tests).into_function()?;
                    let dir = res.stage(&name, &files).map_err(mlua::Error::external)?;
                    Ok(Ret::Json(json!({
                        "ok": true,
                        "message": format!("staged tool '{name}' without executing unapproved code. Review it, run `hx test {}`, then `hx accept {name}`. It becomes available in a new run.", dir.display()),
                        "tested": false,
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
                let ret = fx.run(&lua, crate::supervisor::Tool::ProposeModule, Class::LocalWrite, args, || async {
                    Ok(match res.stage_module(&module, &source, &reason) {
                        Ok(_) => Ret::Json(json!({"ok": true, "message": format!("staged a replacement for {module}. It is NOT active: the user reviews it and runs `hx accept {module}`; it applies to new runs.")})),
                        Err(e) => Ret::Json(json!({"ok": false, "message": e.to_string()})),
                    })
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?;
        // the opt-in std/authoring pack exposes these to the model's session (hx.expose)
        let authoring = lua.create_table()?;
        for (n, f) in [("deftool", deftool), ("promote", promote), ("propose_module", propose_module)] {
            authoring.set(n, f)?;
        }
        hx.set("authoring", authoring)?;
        modenv.set("hx", hx)?;
        let lines = crate::tools::install(&lua, &session.env, supervisor, host, fx.clone())?;
        for name in registry.tools.keys() {
            if !session.env.raw_get::<Value>(name.as_str())?.is_nil() { exposed.lock().unwrap().insert(name.clone()); }
        }
        let tool_prompt = if lines.is_empty() { String::new() } else { format!("\n\n# Local tools (user-configured)\n{}", lines.join("\n")) };
        Ok(Vm { lua, session, modenv, tool_prompt, #[cfg(test)] effects: fx })
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
        self.session.log.check()?;
        Ok(format!("{text}-- ops={} error={}", r.ops, r.error.is_some()))
    }

    pub async fn run_chunk(&self, code: &str) -> Result<serde_json::Value> {
        let result = self.session.run(code).await;
        self.session.log.check()?;
        let value = self.lua.create_table()?;
        value.set("output", result.output.clone())?;
        value.set("values", self.lua.create_sequence_from(result.values)?)?;
        value.set("error", result.error.clone())?;
        value.set("ops", result.ops)?;
        let formatter = self.lua.load("local r, code = ...; return require('std/feedback').compose(r, code)")
            .set_environment(self.modenv.clone()).into_function()?;
        let feedback: String = formatter.call_async((value, code.to_string())).await?;
        Ok(json!({"output":result.output, "error":result.error, "ops":result.ops, "feedback":feedback}))
    }

    pub fn prepare_request(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let req = crate::values::copy_in(&self.lua, request)?;
        let prepare = self.lua.load("local request = ...; return require('std/hooks').emit('before_model', request)")
            .set_environment(self.modenv.clone()).into_function()?;
        let mut request = crate::values::copy_out(prepare.call(req)?)?;
        if let Some(system) = request["system"].as_str() { request["system"] = json!(format!("{system}{}", self.tool_prompt)); }
        Ok(request)
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
