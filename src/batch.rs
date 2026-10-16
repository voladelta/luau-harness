//! Sealed data descriptions, isolated execution contexts, and input-order publication.
use crate::{effects::Effects, host::{AgentOptions, Host}, prims, supervisor::{Principal, Supervisor}, values};
use anyhow::{bail, Context, Result};
use mlua::{Function, Lua, MultiValue, Value};
use serde::Deserialize;
use serde_json::{json, Value as Json};
use std::sync::{Arc, atomic::AtomicUsize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor { op: String, args: Vec<Json> }

enum Operation {
    Native { op: &'static str, args: Vec<Json> },
    Agent { task: String, options: AgentOptions },
}

pub async fn run(supervisor: Arc<Supervisor>, host: Option<Arc<Host>>, input: Json) -> Result<Json> {
    let descriptors: Vec<Descriptor> = if input == json!({}) { vec![] } else { serde_json::from_value(input)? };
    anyhow::ensure!(descriptors.len() <= 32, "a batch may contain at most 32 operations");
    // Validate every descriptor before dispatching any of them.
    let operations = descriptors.into_iter().map(|operation| -> Result<Operation> {
        anyhow::ensure!(matches!(operation.op.as_str(), "read" | "search" | "agent"), "batch supports only read, search and read-only agent operations");
        anyhow::ensure!(!operation.args.is_empty() && operation.args.len() <= 2 && operation.args[0].is_string(), "batch arguments must be [string, optional options object]");
        if operation.args.len() == 2 { anyhow::ensure!(operation.args[1].is_object() || operation.args[1].is_null(), "batch options must be an object"); }
        if operation.op == "agent" {
            anyhow::ensure!(host.is_some(), "batch agent requires hx run");
            return Ok(Operation::Agent { task: operation.args[0].as_str().unwrap().to_string(),
                options: AgentOptions::parse(operation.args.get(1).cloned().unwrap_or(Json::Null))? });
        }
        Ok(Operation::Native { op: if operation.op == "read" { "read" } else { "search" }, args: operation.args })
    }).collect::<Result<Vec<_>>>()?;
    let mut jobs = Vec::new();
    for operation in operations {
        let (supervisor, host) = (supervisor.clone(), host.clone());
        jobs.push(async move {
            let search = matches!(&operation, Operation::Native { op: "search", .. });
            let result: Result<Json> = match operation {
                Operation::Agent { task, options } => async {
                    let host = host.context("agent unavailable")?;
                    let child = host.child(&options, host.principal.read_only())?;
                    child.run_task(task, options.max_turns).await
                }.await,
                Operation::Native { op, args } => tokio::task::spawn_blocking(move || -> Result<Json> {
                    let lua = Lua::new();
                    let principal = host.as_ref().map(|h| h.principal.read_only()).unwrap_or(Principal::Agent { read_only: true });
                    let fx = Effects::supervised(supervisor, principal, host.as_ref().map(|h| h.depth).unwrap_or(0), Arc::default());
                    prims::install(&lua, fx, Arc::new(AtomicUsize::new(0)))?;
                    let function: Function = lua.globals().get(if op == "search" { "__search_raw" } else { "read" })?;
                    let args = args.iter().map(|v| if v.is_null() { Ok(Value::Nil) } else { values::copy_in(&lua, v) }).collect::<mlua::Result<Vec<_>>>()?;
                    let returned: MultiValue = function.call(MultiValue::from_vec(args))?;
                    if op == "search" {
                        // search's native array has a metatable for its truncation flag; publish plain copied data.
                        let mut results = returned.into_iter();
                        let rows = results.next().unwrap_or(Value::Nil);
                        if let Some(Value::Table(error)) = results.next() { bail!("{}: {}", error.get::<String>("kind")?, error.get::<String>("message")?); }
                        if let Value::Table(t) = rows {
                            let truncated: bool = t.get("truncated").unwrap_or(false);
                            t.set_metatable(None)?;
                            return Ok(json!({"rows":values::copy_out(Value::Table(t))?, "truncated":truncated}));
                        }
                        bail!("search returned an invalid result");
                    }
                    Ok(values::copy_out(returned.into_iter().next().unwrap_or(Value::Nil))?)
                }).await.map_err(anyhow::Error::from).and_then(|r| r),
            };
            Ok::<_, anyhow::Error>(match result {
                Ok(value) if search => json!({"ok":true,"value":value["rows"],"truncated":value["truncated"]}),
                Ok(value) => json!({"ok":true,"value":value}),
                Err(error) => json!({"ok":false,"error":error.to_string()}),
            })
        });
    }
    let results = futures::future::join_all(jobs).await.into_iter().collect::<Result<Vec<_>>>()?;
    Ok(json!(results))
}
