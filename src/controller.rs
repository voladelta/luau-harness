//! A separate pure VM: workflow code can return commands but cannot execute effects.
use crate::{modules::Registry, values};
use anyhow::Result;
use mlua::{Function, Lua, MultiValue, Table, Value};
use serde_json::Value as Json;
use std::{sync::Arc, time::{Duration, Instant}};

#[derive(serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Next { archive: Option<crate::history::Directive> },
    NextCall,
    Model { request: Json, purpose: Option<String> },
    Run { code: String },
    Boundary { id: String, submission: Option<String>, next: crate::workflow::BoundaryNext },
    Finish { report: serde_json::Map<String, Json> },
}

pub fn validate_request(request: &Json) -> Result<()> {
    anyhow::ensure!(request.is_object() && request["system"].is_string(), "model request needs a string system prompt");
    let messages = request["messages"].as_array().ok_or_else(|| anyhow::anyhow!("model request needs a messages array"))?;
    anyhow::ensure!(!messages.is_empty(), "model request messages cannot be empty");
    for message in messages {
        anyhow::ensure!(matches!(message["role"].as_str(), Some("user" | "assistant" | "tool")), "model message has an invalid role");
        anyhow::ensure!(message["content"].is_string() || message["content"].is_array() || (message["role"] == "assistant" && message["content"].is_null()), "model message content must be text or an array");
    }
    if let Some(tools) = request.get("tools") {
        // Empty Luau tables have no array identity at the copied-data boundary.
        anyhow::ensure!(tools.is_array() || tools.as_object().is_some_and(|v| v.is_empty()), "model request tools must be an array");
        for tool in tools.as_array().into_iter().flatten() {
            anyhow::ensure!(tool["name"].is_string() && tool["parameters"].is_object(), "model tool needs name and parameters");
        }
    }
    if let Some(max) = request.get("max_tokens") { anyhow::ensure!(max.as_u64().is_some_and(|v| v > 0), "max_tokens must be a positive integer"); }
    Ok(())
}

fn normalize_lists(value: &mut Json) {
    if let Some(object)=value.as_object_mut() {
        for (key,value) in object.iter_mut() {
            if matches!(key.as_str(),"inputs"|"artifacts"|"questions"|"check_ids"|"decisions") && value.as_object().is_some_and(|v|v.is_empty()) { *value=serde_json::json!([]); }
            else {normalize_lists(value);}
        }
    } else if let Some(array)=value.as_array_mut() { for value in array {normalize_lists(value);} }
}

pub fn command(value: &Json) -> Result<Command> {
    let mut value=value.clone();
    if value["kind"]=="boundary" { normalize_lists(&mut value); }
    let command: Command = serde_json::from_value(value).map_err(|e| anyhow::anyhow!("invalid controller command: {e}"))?;
    match &command {
        Command::Model { request, purpose } => {
            anyhow::ensure!(matches!(purpose.as_deref(), None | Some("agent" | "compact")), "invalid model command purpose");
            validate_request(request)?;
        },
        Command::Boundary {id,submission,next} => { crate::workflow::Boundary{id:id.clone(),submission:submission.clone(),next:next.clone()}.validate()?; },
        Command::Finish { report } => {
            anyhow::ensure!(report.get("finished").is_some_and(Json::is_boolean) && report.get("final_answer").is_some_and(Json::is_string), "finish report needs finished and final_answer");
        },
        _ => {},
    }
    Ok(command)
}

pub struct Controller { lua: Lua, module: Table }

impl Controller {
    pub fn new(registry: Arc<Registry>, profile: Json, contributions: String, packs: &[String], run_deadline: Option<f64>, entrypoint: &str) -> Result<Self> {
        let lua = Lua::new();
        let g = lua.globals();
        g.set("print", lua.create_function(|_, args: MultiValue| { eprintln!("{}", args.into_iter().map(|v| v.to_string().unwrap_or_default()).collect::<Vec<_>>().join("\t")); Ok(()) })?)?;
        for name in ["_G", "os", "coroutine", "getfenv", "setfenv", "loadstring", "require"] { g.set(name, Value::Nil)?; }
        let math: Table = g.get("math")?;
        math.set("random", Value::Nil)?; math.set("randomseed", Value::Nil)?;
        let deadline = Arc::new(std::sync::Mutex::new(Instant::now() + Duration::from_secs(10)));
        let d = deadline.clone();
        lua.set_interrupt(move |_| {
            if run_deadline.is_some_and(|d| crate::supervisor::now() >= d) {
                return Err(mlua::Error::runtime("budget_exceeded: absolute run deadline reached during controller execution"));
            }
            if Instant::now() >= *d.lock().unwrap() { return Err(mlua::Error::runtime("controller time budget exceeded")); }
            Ok(mlua::VmState::Continue)
        });
        lua.set_memory_limit(128 << 20)?;
        let env = lua.create_table()?;
        let mt = lua.create_table()?; mt.set("__index", g.clone())?; env.set_metatable(Some(mt))?;
        let loaded = lua.create_table()?;
        let (r, e, l) = (registry.clone(), env.clone(), loaded);
        let require = lua.create_function(move |lua, name: String| {
            if let Ok(Value::Table(t)) = l.raw_get::<Value>(name.as_str()) { return Ok(t); }
            let source = r.resolve(&name).map_err(mlua::Error::external)?;
            if source.pkg.is_some() { return Err(mlua::Error::runtime("controller cannot import tool packages")); }
            let value: Value = lua.load(&source.text).set_name(format!("=controller:{name}")).set_environment(e.clone()).eval()?;
            let module = match value { Value::Table(t) => t, Value::Nil if name == "project/init" => lua.create_table()?, _ => return Err(mlua::Error::runtime("controller modules must return a table")) };
            l.raw_set(name.as_str(), module.clone())?;
            Ok(module)
        })?;
        env.set("require", require.clone())?;
        let hx = lua.create_table()?;
        let r = registry.clone();
        hx.set("asset", lua.create_function(move |_, name: String| r.asset(&name).map_err(mlua::Error::external))?)?;
        let p = profile.clone();
        hx.set("model_info", lua.create_function(move |lua, ()| values::copy_in(lua, &p))?)?;
        hx.set("net", profile["net"].as_bool().unwrap_or(false))?;
        hx.set("limits", values::copy_in(&lua, &crate::prims::prompt_limits())?)?;
        let names = registry.mcp.keys().cloned().collect::<Vec<_>>();
        hx.set("mcp_servers", lua.create_function(move |lua, ()| lua.create_sequence_from(names.clone()))?)?;
        hx.set("prompt_contributions", contributions)?;
        hx.set("log", lua.create_function(|_, line: String| { eprintln!("{line}"); Ok(()) })?)?;
        env.set("hx", hx)?;
        lua.sandbox(true)?;
        if registry.sources.contains_key("project/init") { require.call::<Table>("project/init")?; }
        // Controller packs are trusted pure std modules. Agent-facing packs are installed in the agent VM instead.
        for name in packs {
            if matches!(name.as_str(), "std/packages" | "std/mcp" | "std/authoring" | "std/vision" | "std/recheck" | "std/compact") { continue; }
            let module: Table = require.call(name.as_str())?;
            if let Ok(install) = module.get::<Function>("install") {
                let ctx = lua.create_table()?; ctx.set("hooks", require.call::<Table>("std/hooks")?)?;
                install.call::<()>(ctx)?;
            }
        }
        let module: Table = require.call(entrypoint)?;
        // Each transition gets a fresh interrupt deadline; no controller can hang effect admission.
        module.set("__deadline", lua.create_function(move |_, ()| { *deadline.lock().unwrap() = Instant::now() + Duration::from_secs(10); Ok(()) })?)?;
        Ok(Self { lua, module })
    }

    pub fn init(&self, cfg: &Json) -> Result<Json> {
        self.module.get::<Function>("__deadline")?.call::<()>(())?;
        let result: Value = self.module.get::<Function>("init")?.call(values::copy_in(&self.lua, cfg)?)?;
        let state = values::copy_out(result)?;
        anyhow::ensure!(state.is_object(), "controller state must be an object");
        Ok(state)
    }

    pub fn step(&self, state: &Json, event: &Json) -> Result<(Json, Json)> {
        self.module.get::<Function>("__deadline")?.call::<()>(())?;
        let result: MultiValue = self.module.get::<Function>("step")?.call((values::copy_in(&self.lua, state)?, values::copy_in(&self.lua, event)?))?;
        anyhow::ensure!(result.len() == 2, "controller step must return state and one command");
        let mut result = result.into_iter();
        Ok((values::copy_out(result.next().unwrap())?, values::copy_out(result.next().unwrap())?))
    }
}
