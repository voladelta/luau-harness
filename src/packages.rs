//! Package calls cross a data-only boundary and get a fresh VM and an unforgeable Rust principal.
use crate::{effects::Effects, host::Host, prims, supervisor::{Principal, Supervisor}, values};
use mlua::{Lua, MultiValue, Table, Value};
use std::{collections::HashSet, sync::{Arc, Mutex, atomic::{AtomicBool, AtomicUsize, Ordering}}, time::{Duration, Instant}};

fn worker(supervisor: Arc<Supervisor>, host: Option<Arc<Host>>, package: &str) -> mlua::Result<(Lua, Table, Arc<AtomicBool>)> {
    let registry = supervisor.registry.clone();
    let mut caps = registry.capabilities.get(package).cloned().ok_or_else(|| mlua::Error::runtime("package not in frozen registry"))?;
    if let Some(host) = &host {
        for (cap, scopes) in &mut caps { scopes.retain(|scope| host.principal.allows(cap, scope)); }
    }
    let active = Arc::new(AtomicBool::new(false));
    let principal = Principal::Package { name: package.to_string(), caps, active: active.clone() };
    let lua = Lua::new();
    let deadline = Arc::new(Mutex::new(Some(Instant::now() + Duration::from_secs(30))));
    let fx = Effects::supervised(supervisor.clone(), principal.clone(), host.as_ref().map(|h| h.depth).unwrap_or(0), deadline.clone());
    prims::install(&lua, fx.clone(), Arc::new(AtomicUsize::new(0))).map_err(mlua::Error::external)?;
    let g = lua.globals();
    g.set("__emit", lua.create_function(|_, _: String| Ok(()))?)?;
    g.set("__format", lua.create_function(|_, value: Value| value.to_string())?)?;
    if let Some(host) = host {
        let afx = fx.clone();
        g.set("agent", lua.create_async_function(move |lua, (task, opts): (String, Option<Table>)| {
            let (host, fx) = (host.clone(), afx.clone());
            async move {
                fx.supervisor.authorize(&fx.principal, "agent", crate::policy::Class::LocalWrite).map_err(mlua::Error::external)?;
                if host.depth >= host.max_depth { return Err(mlua::Error::runtime("agent depth limit reached")); }
                let model = prims::opt_str(&opts, "model"); let effort = prims::opt_str(&opts, "effort");
                let turns = opts.as_ref().and_then(|o| o.get::<Option<usize>>("max_turns").ok().flatten()).unwrap_or(12);
                let child = host.child_with(model.clone(), effort.clone(), fx.principal.clone());
                let ret = fx.run(&lua, "agent", crate::policy::Class::LocalWrite, serde_json::json!({"task":task, "max_turns":turns, "model":model, "effort":effort}), || async {
                    let rep = child.run_task(task, turns).await.map_err(mlua::Error::external)?;
                    Ok(crate::effects::Ret::Json(rep))
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?)?;
    } else {
        let fx = fx.clone();
        g.set("agent", lua.create_function(move |_, _: MultiValue| -> mlua::Result<()> {
            fx.supervisor.authorize(&fx.principal, "agent", crate::policy::Class::LocalWrite).map_err(mlua::Error::external)?;
            Err(mlua::Error::runtime("agent requires hx run"))
        })?)?;
    }
    lua.load(crate::vm::PRELUDE).exec()?;
    // Package imports can reach only this package's frozen source files. The cache dies with this invocation.
    let env = lua.create_table()?;
    let view = lua.create_table()?;
    for pair in g.pairs::<Value, Value>() {
        let (key, value) = pair?;
        if matches!(&key, Value::String(s) if matches!(s.to_str()?.as_ref(), "_G" | "coroutine")) { continue; }
        view.raw_set(key, value)?;
    }
    view.set_readonly(true);
    let mt = lua.create_table()?; mt.set("__index", view)?; mt.set("__metatable", "locked")?; env.set_metatable(Some(mt))?;
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
        let module = result?;
        l.raw_set(name.as_str(), module.clone())?;
        Ok(module)
    })?;
    env.set("require", require.clone())?;
    lua.set_memory_limit(128 << 20)?;
    let d = deadline;
    let waiting = fx.waiting.clone();
    lua.set_interrupt(move |lua| {
        if supervisor.remaining().is_some_and(|remaining| remaining.is_zero()) { return Err(mlua::Error::runtime("budget_exceeded: absolute run deadline reached")); }
        if waiting.lock().unwrap().contains_key(&(lua.current_thread().to_pointer() as usize)) { return Ok(mlua::VmState::Continue); }
        if d.lock().unwrap().is_some_and(|deadline| Instant::now() >= deadline) { return Err(mlua::Error::runtime("package time budget exceeded")); }
        Ok(mlua::VmState::Continue)
    });
    lua.sandbox(true)?;
    let module: Table = require.call(package)?;
    Ok((lua, module, active))
}

pub fn proxy(lua: &Lua, supervisor: Arc<Supervisor>, host: Option<Arc<Host>>, package: &str, caller: Arc<Effects>) -> mlua::Result<Table> {
    let (_worker, module, _active) = worker(supervisor.clone(), host.clone(), package)?;
    fn exports(lua: &Lua, source: Table, supervisor: Arc<Supervisor>, host: Option<Arc<Host>>, package: &str,
        path: Vec<String>, stack: &mut HashSet<usize>, caller: Arc<Effects>, limit: &mut values::Limit) -> mlua::Result<Table> {
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
                    let (s, h, p, caller) = (supervisor.clone(), host.clone(), package.to_string(), caller.clone());
                    Value::Function(lua.create_async_function(move |lua, args: MultiValue| {
                        let (s, h, p, route, caller) = (s.clone(), h.clone(), p.clone(), route.clone(), caller.clone());
                        async move {
                            let args = values::copy_many(args)?;
                            let (worker, module, active) = worker(s, h, &p)?;
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
                Value::Table(t) => Value::Table(exports(lua, t, supervisor.clone(), host.clone(), package, route, stack, caller.clone(), limit)?),
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
    exports(lua, module, supervisor, host, package, Vec::new(), &mut HashSet::new(), caller, &mut values::Limit::default())
}
