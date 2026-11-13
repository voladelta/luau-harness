//! Sealed data descriptions, isolated execution contexts, and input-order publication.
use crate::{effects::{Effects, Ret}, host::{AgentOptions, Host}, policy::Class, prims, supervisor::{Principal, Tool}, values};
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

/// `fx` is the caller's: a batch agent records the same intent and receipt as `agent()`. Native operations count
/// in the caller's `ops`, like their unbatched forms.
pub async fn run(lua: &Lua, fx: Arc<Effects>, host: Option<Arc<Host>>, ops: Arc<AtomicUsize>, input: Json) -> Result<Json> {
    let supervisor = fx.supervisor.clone();
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
        let (supervisor, host, fx, ops, lua) = (supervisor.clone(), host.clone(), fx.clone(), ops.clone(), lua.clone());
        jobs.push(async move {
            let search = matches!(&operation, Operation::Native { op: "search", .. });
            let result: Result<Json> = match operation {
                Operation::Agent { task, options } => async {
                    let host = host.context("agent unavailable")?;
                    let child = host.child(&options, fx.principal.read_only())?;
                    let args = json!({"task_sha": crate::eventlog::sha_bytes(task.as_bytes()), "preview": task.chars().take(120).collect::<String>(), "max_turns": options.max_turns, "model": options.model, "effort": options.effort});
                    let mut report = None;
                    let slot = &mut report;
                    // A batch agent is read-only, so it is a read: receipted as a `call`, and still admitted while an
                    // uncertain mutation awaits the owner, when inspection is what the recovery notice asks for.
                    fx.run(&lua, Tool::Agent, Class::Read, args, move || async move {
                        child.log(&format!("### SUBAGENT (depth {}, model {}, effort {}, batch)\n{task}", child.depth, child.llm.model, child.llm.reasoning.as_deref().unwrap_or("default")));
                        let rep = child.run_task(task, options.max_turns).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
                        let ret = Ret::Agent { answer: rep["final_answer"].as_str().unwrap_or("").to_string(), finished: rep["finished"].as_bool().unwrap_or(false), turns: rep["turns"].as_u64().unwrap_or(0) };
                        *slot = Some(rep);
                        Ok(ret)
                    }).await.map_err(|e| anyhow::anyhow!("{e}"))?;
                    report.context("agent executor returned no report")
                }.await,
                // Each native operation runs in its own fresh VM under a read-only principal, on a blocking worker so
                // searches run side by side; the worker drives the async primitive, which may await (an owner prompt,
                // the prompt lock other operations hold, the run deadline).
                Operation::Native { op, args } => {
                    let runtime = tokio::runtime::Handle::current();
                    tokio::task::spawn_blocking(move || -> Result<Json> {
                        let lua = Lua::new();
                        let principal = host.as_ref().map(|h| h.principal.read_only()).unwrap_or(Principal::Agent { read_only: true });
                        let fx = Effects::supervised(supervisor, principal, host.as_ref().map(|h| h.depth).unwrap_or(0), Arc::default());
                        prims::install(&lua, fx, ops)?;
                        let function: Function = lua.globals().get(if op == "search" { "__search_raw" } else { "read" })?;
                        let args = args.iter().map(|v| if v.is_null() { Ok(Value::Nil) } else { values::copy_in(&lua, v) }).collect::<mlua::Result<Vec<_>>>()?;
                        let returned: MultiValue = runtime.block_on(function.call_async(MultiValue::from_vec(args)))?;
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
                    }).await.map_err(anyhow::Error::from).and_then(|r| r)
                },
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
