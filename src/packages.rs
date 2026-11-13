//! Package calls cross a data-only boundary and get a fresh VM and an unforgeable Rust principal.
use crate::{effects::Effects, host::Host, prims, supervisor::{Principal, Supervisor}, values};
use mlua::{Lua, MultiValue, Table, Value};
use std::{collections::HashSet, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::{Duration, Instant}};

/// A fresh VM for one package invocation: (the VM, the package's module, the principal's active flag, the package
/// environment the module was loaded in).
fn worker(supervisor: Arc<Supervisor>, host: Option<Arc<Host>>, package: &str) -> mlua::Result<(Lua, Table, Arc<AtomicBool>, Table)> {
    let registry = supervisor.registry.clone();
    let mut caps = registry.capabilities.get(package).cloned().ok_or_else(|| mlua::Error::runtime("package not in frozen registry"))?;
    if let Some(host) = &host {
        caps.0.retain(|cap| host.principal.allows(*cap));
    }
    let active = Arc::new(AtomicBool::new(false));
    let principal = Principal::Package { name: package.to_string(), caps, active: active.clone() };
    let lua = Lua::new();
    let deadline = Arc::new(Mutex::new(Some(Instant::now() + Duration::from_secs(30))));
    let fx = Effects::supervised(supervisor.clone(), principal.clone(), host.as_ref().map(|h| h.depth).unwrap_or(0), deadline.clone());
    prims::install(&lua, fx.clone(), Arc::new(AtomicUsize::new(0))).map_err(mlua::Error::external)?;
    if let Some(name) = package.strip_prefix("tool:") {
        let def = registry.tools.get(name).ok_or_else(|| mlua::Error::runtime("tool not in frozen registry"))?;
        crate::tools::install_http(&lua, fx.clone(), name.to_string(), def.clone())?;
        crate::tools::install_codecs(&lua)?;
    }
    let g = lua.globals();
    g.set("__emit", lua.create_function(|_, _: String| Ok(()))?)?;
    g.set("__format", lua.create_function(|_, value: Value| value.to_string())?)?;
    let (lfx, lhost) = (fx.clone(), host.clone());
    g.set("llm", lua.create_async_function(move |lua, requests: Value| {
        let (host, fx) = (lhost.clone(), lfx.clone());
        async move {
            let input = values::copy_out(requests)?;
            let ret = fx.run(&lua, crate::supervisor::Tool::Llm, crate::policy::Class::Read, input.clone(), || async {
                let host = host.ok_or_else(|| mlua::Error::runtime("llm requires hx run"))?;
                Ok(crate::effects::Ret::Json(host.complete(input).await.map_err(mlua::Error::external)?))
            }).await?;
            prims::json_value(&lua, ret)
        }
    })?)?;
    if let Some(host) = host {
        let afx = fx.clone();
        g.set("agent", lua.create_async_function(move |lua, (task, opts): (String, Option<Table>)| {
            let (host, fx) = (host.clone(), afx.clone());
            async move {
                fx.supervisor.authorize(&fx.principal, crate::supervisor::Tool::Agent, crate::policy::Class::Read).map_err(mlua::Error::external)?;
                let options = crate::host::AgentOptions::from_lua(opts)?;
                let child = host.child(&options, fx.principal.clone()).map_err(mlua::Error::external)?;
                let ret = fx.run(&lua, crate::supervisor::Tool::Agent, crate::policy::Class::Read, serde_json::json!({"task":task, "max_turns":options.max_turns, "model":options.model, "effort":options.effort}), || async {
                    let rep = child.run_task(task, options.max_turns).await.map_err(mlua::Error::external)?;
                    Ok(crate::effects::Ret::Json(rep))
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?)?;
    } else {
        let fx = fx.clone();
        g.set("agent", lua.create_function(move |_, _: MultiValue| -> mlua::Result<()> {
            fx.supervisor.authorize(&fx.principal, crate::supervisor::Tool::Agent, crate::policy::Class::Read).map_err(mlua::Error::external)?;
            Err(mlua::Error::runtime("agent requires hx run"))
        })?)?;
    }
    lua.load(crate::vm::PRELUDE).exec()?;
    // Package imports can reach only this package's frozen source files. The cache dies with this invocation.
    let env = lua.create_table()?;
    env.set_metatable(Some(crate::vm::locked_env(&lua, &g)?))?;
    let loaded = lua.create_table()?;
    let loading = Arc::new(Mutex::new(HashSet::new()));
    let package = package.to_string();
    let (p, r, e, l, progress, initialization) = (package.clone(), registry, env.clone(), loaded, loading, active.clone());
    let require = lua.create_function(move |lua, name: String| {
        let name = if name == p || name.starts_with(&format!("{p}/")) { name } else {
            if name.contains('/') || name.contains(':') || name.contains("..") || name == "std" || name == "project" {
                return Err(mlua::Error::runtime("capability_denied: imports are restricted to this package"));
            }
            format!("{p}/{name}")
        };
        if let Value::Table(t) = l.raw_get::<Value>(name.as_str())? { return Ok(t); }
        let src = r.resolve(&name).map_err(mlua::Error::external)?;
        if src.pkg.as_ref().map(|(n, _)| n.as_str()) != Some(p.as_str()) {
            return Err(mlua::Error::runtime("capability_denied: package cannot import privileged modules"));
        }
        if !progress.lock().unwrap().insert(name.clone()) { return Err(mlua::Error::runtime("cyclic package import")); }
        let previous = initialization.swap(false, Ordering::AcqRel);
        let result = lua.load(&src.text).set_name(format!("=package:{name}")).set_environment(e.clone()).eval::<Table>();
        initialization.store(previous, Ordering::Release);
        progress.lock().unwrap().remove(&name);
        let mut module = result?;
        if name == p && let Some(tool) = p.strip_prefix("tool:") {
            let def = r.tools.get(tool).ok_or_else(|| mlua::Error::runtime("tool not in frozen registry"))?;
            module = crate::tools::exports(lua, module, def)?;
        }
        l.raw_set(name.as_str(), module.clone())?;
        Ok(module)
    })?;
    env.set("require", require.clone())?;
    lua.set_memory_limit(128 << 20)?;
    crate::vm::Budget {
        run_deadline: supervisor.manifest.as_ref().and_then(|m| m.deadline),
        deadline,
        waiting: Some(fx.waiting.clone()),
        exceeded: "package time budget exceeded",
    }
    .install(&lua);
    lua.sandbox(true)?;
    let module: Table = require.call(package)?;
    Ok((lua, module, active, env))
}

pub fn proxy(lua: &Lua, supervisor: Arc<Supervisor>, host: Option<Arc<Host>>, package: &str, caller: Arc<Effects>) -> mlua::Result<Table> {
    let (_worker, module, _active, _env) = worker(supervisor.clone(), host.clone(), package)?;
    struct ExportContext<'a> {
        supervisor: Arc<Supervisor>,
        host: Option<Arc<Host>>,
        package: &'a str,
        caller: Arc<Effects>,
    }
    fn exports(lua: &Lua, source: Table, ctx: &ExportContext<'_>,
        path: Vec<String>, stack: &mut HashSet<usize>, limit: &mut values::Limit) -> mlua::Result<Table> {
        if path.len() > 64 { return Err(mlua::Error::runtime("package exports exceed 64 levels")); }
        limit.charge(64, 1)?;
        if source.metatable().is_some() || !stack.insert(source.to_pointer() as usize) { return Err(mlua::Error::runtime("package exports must be acyclic plain tables")); }
        if source.clone().pairs::<Value, Value>().any(|pair| !matches!(pair, Ok((Value::String(_), _)))) {
            let data = values::copy_out(Value::Table(source.clone()))?;
            limit.charge(serde_json::to_vec(&data).map_err(mlua::Error::external)?.len(), 1)?;
            let Value::Table(out) = values::copy_in(lua, &data)? else { unreachable!() };
            stack.remove(&(source.to_pointer() as usize));
            return Ok(out);
        }
        let out = lua.create_table()?;
        for pair in source.clone().pairs::<String, Value>() {
            let (name, value) = pair?;
            limit.charge(64, 1)?;
            limit.string(&name)?;
            let mut route = path.clone(); route.push(name.clone());
            let value = match value {
                Value::Function(_) => {
                    let (s, h, p, caller) = (ctx.supervisor.clone(), ctx.host.clone(), ctx.package.to_string(), ctx.caller.clone());
                    Value::Function(lua.create_async_function(move |lua, args: MultiValue| {
                        let (s, h, p, route, caller) = (s.clone(), h.clone(), p.clone(), route.clone(), caller.clone());
                        async move {
                            let args = values::copy_many(args)?;
                            let (worker, module, active, _env) = worker(s, h, &p)?;
                            let mut function = Value::Table(module);
                            for name in route { function = match function { Value::Table(t) => t.raw_get(name.as_str())?, _ => return Err(mlua::Error::runtime("package export changed shape")) }; }
                            let Value::Function(function) = function else { return Err(mlua::Error::runtime("package export is not callable")); };
                            let args = args.iter().map(|v| if v.is_null() { Ok(Value::Nil) } else { values::copy_in(&worker, v) }).collect::<mlua::Result<Vec<_>>>()?;
                            active.store(true, Ordering::Release);
                            let _wait = caller.awaiting(&lua);
                            let returned: MultiValue = function.call_async(MultiValue::from_vec(args)).await?;
                            let copied = values::copy_many(returned)?;
                            copied.iter().map(|v| if v.is_null() { Ok(Value::Nil) } else { values::copy_in(&lua, v) }).collect::<mlua::Result<MultiValue>>()
                        }
                    })?)
                },
                Value::Table(t) => Value::Table(exports(lua, t, ctx, route, stack, limit)?),
                value => {
                    let data = values::copy_out(value)?;
                    limit.charge(serde_json::to_vec(&data).map_err(mlua::Error::external)?.len(), 0)?;
                    values::copy_in(lua, &data)?
                },
            };
            out.raw_set(name, value)?;
        }
        stack.remove(&(source.to_pointer() as usize));
        Ok(out)
    }
    let ctx = ExportContext { supervisor, host, package, caller };
    exports(lua, module, &ctx, Vec::new(), &mut HashSet::new(), &mut values::Limit::default())
}

/// What `hx test` prints: the failures, and the counts.
#[derive(Default)]
pub struct TestReport {
    pub passed: usize,
    pub failed: usize,
    pub text: String,
}

impl TestReport {
    /// Record one test file: its load error, or the cases `test.case` collected in `testlib.results`.
    pub fn record(&mut self, file: &str, ran: mlua::Result<()>, testlib: &Table) -> mlua::Result<()> {
        if let Err(e) = ran {
            self.failed += 1;
            self.text.push_str(&format!("FAIL {file}: load error: {e}\n"));
            return Ok(());
        }
        let results: Table = testlib.get("results")?;
        for r in results.sequence_values::<Table>() {
            let r = r?;
            let (case, ok): (String, bool) = (r.get("name")?, r.get("ok")?);
            if ok {
                self.passed += 1;
            } else {
                self.failed += 1;
                self.text.push_str(&format!("FAIL {file}: {case}: {}\n", r.get::<Option<String>>("err")?.unwrap_or_default()));
            }
        }
        Ok(())
    }
}

/// `hx test` of a package: each test file `(file name, source)` runs inside the package's boundary, in a package
/// worker whose principal holds exactly the capabilities the registry grants the package. The file sees the package's
/// environment (imports restricted to the package) plus `test`, the test library; its globals do not leak into the
/// next file.
pub async fn run_tests(supervisor: Arc<Supervisor>, package: &str, tests: &[(String, String)]) -> anyhow::Result<TestReport> {
    let mut report = TestReport::default();
    let (lua, _module, active, env) = match worker(supervisor, None, package) {
        Ok(worker) => worker,
        Err(e) => {
            for (file, _) in tests {
                report.failed += 1;
                report.text.push_str(&format!("FAIL {file}: load error: package {package} failed to load: {e}\n"));
            }
            return Ok(report);
        },
    };
    let testlib: Table = lua.load(crate::modules::Resolver::embedded("std", "testlib.luau").expect("std/testlib is embedded"))
        .set_name("=std/testlib").set_environment(env.clone()).eval()?;
    active.store(true, Ordering::Release);
    for (file, source) in tests {
        testlib.set("results", lua.create_table()?)?;
        let file_env = lua.create_table()?;
        file_env.set("test", testlib.clone())?;
        let mt = lua.create_table()?;
        mt.set("__index", env.clone())?;
        file_env.set_metatable(Some(mt))?;
        let ran = match lua.load(source).set_name(format!("={file}")).set_environment(file_env).into_function() {
            Ok(f) => f.call_async::<()>(()).await,
            Err(e) => Err(e),
        };
        report.record(file, ran, &testlib)?;
    }
    Ok(report)
}
