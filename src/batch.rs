//! Sealed data descriptions of independent read-only operations, run concurrently and published in input order.
use crate::{effects::Effects, host::{AgentOptions, Host}, prims};
use anyhow::Result;
use mlua::{Lua, Table, Value};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value as Json};
use std::sync::{atomic::{AtomicUsize, Ordering}, Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor { op: String, args: Vec<Json> }

enum Operation {
    Read { path: String, options: prims::ReadOptions },
    Search { pattern: String, options: prims::SearchOptions },
    Agent { task: String, options: AgentOptions },
}

/// What one operation published: its value and, for a search, whether rows were left out.
enum Published {
    Bytes(Vec<u8>),
    Rows(prims::Hits),
    Agent(prims::AgentReply),
}

fn options<T: DeserializeOwned + Default>(op: &str, value: Option<Json>) -> Result<T> {
    match value {
        None | Some(Json::Null) => Ok(T::default()),
        Some(value) => serde_json::from_value(value).map_err(|e| anyhow::anyhow!("batch {op}: options: {e}")),
    }
}

/// Validate every descriptor before any operation is dispatched.
fn parse(input: Json, agents: bool) -> Result<Vec<Operation>> {
    let descriptors: Vec<Descriptor> = if input == json!({}) { vec![] } else { serde_json::from_value(input)? };
    anyhow::ensure!(descriptors.len() <= 32, "a batch may contain at most 32 operations");
    descriptors.into_iter().map(|Descriptor { op, args }| {
        anyhow::ensure!(matches!(op.as_str(), "read" | "search" | "agent"), "batch supports only read, search and read-only agent operations");
        let mut args = args.into_iter();
        let (Some(Json::String(first)), opts, None) = (args.next(), args.next(), args.next()) else {
            anyhow::bail!("batch arguments must be [string, optional options object]");
        };
        if let Some(opts) = &opts { anyhow::ensure!(opts.is_object() || opts.is_null(), "batch options must be an object"); }
        Ok(match op.as_str() {
            "read" => Operation::Read { path: first, options: options(&op, opts)? },
            "search" => {
                let options: prims::SearchOptions = options(&op, opts)?;
                options.cap().map_err(|e| anyhow::anyhow!("batch {e}"))?;
                Operation::Search { pattern: first, options }
            }
            _ => {
                anyhow::ensure!(agents, "batch agent requires hx run");
                Operation::Agent { task: first, options: AgentOptions::parse(opts.unwrap_or(Json::Null))? }
            }
        })
    }).collect()
}

/// Run a batch through the same effect functions as the single primitives (each operation authorized, recorded and
/// counted on its own), with agents read-only. -> `{ok, value | error[, truncated]}` per operation, in input order.
pub async fn run(fx: &Effects, lua: &Lua, host: Option<Arc<Host>>, ops: &AtomicUsize, input: Json) -> mlua::Result<Table> {
    let operations = parse(input, host.is_some()).map_err(mlua::Error::external)?;
    let jobs = operations.into_iter().map(|operation| {
        let host = host.clone();
        async move {
            ops.fetch_add(1, Ordering::Relaxed);
            match operation {
                Operation::Read { path, options } => prims::read(fx, lua, path, options).await.map(Published::Bytes),
                Operation::Search { pattern, options } => match prims::search(fx, lua, pattern, options).await? {
                    Ok(hits) => Ok(Published::Rows(hits)),
                    Err(e) => Err(mlua::Error::runtime(format!("{}: {}", e.kind, e.message))),
                },
                Operation::Agent { task, options } => {
                    let host = host.expect("parse admits agents only with a host");
                    let child = host.child(&options, fx.principal.read_only()).map_err(mlua::Error::external)?;
                    prims::agent(fx, lua, child, task, &options).await.map(Published::Agent)
                }
            }
        }
    });
    let results = futures::future::join_all(jobs).await;
    let out = lua.create_table_with_capacity(results.len(), 0)?;
    for result in results {
        let entry = lua.create_table()?;
        match result {
            Ok(published) => {
                entry.raw_set("ok", true)?;
                let value = match published {
                    Published::Bytes(bytes) => Value::String(lua.create_string(bytes)?),
                    Published::Rows(prims::Hits { rows, truncated }) => {
                        entry.raw_set("truncated", truncated)?;
                        Value::Table(prims::rows_table(lua, &rows)?)
                    }
                    Published::Agent(reply) => Value::Table(reply.into_lua(lua)?),
                };
                entry.raw_set("value", value)?;
            }
            Err(error) => {
                entry.raw_set("ok", false)?;
                entry.raw_set("error", error_text(&error))?;
            }
        }
        out.raw_push(entry)?;
    }
    Ok(out)
}

/// The message of an operation's error, without the callback wrapping and traceback mlua adds.
fn error_text(error: &mlua::Error) -> String {
    match error {
        mlua::Error::RuntimeError(m) => m.clone(),
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => error_text(cause),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_option_names_its_cause() {
        let error = parse(json!([{"op": "read", "args": ["f", {"bogus": 2}]}]), false).err().expect("an unknown option is refused");
        assert_eq!(error.to_string(), "batch read: options: unknown field `bogus`, expected `offset` or `limit`");
        let error = parse(json!([{"op": "search", "args": ["x", {"max": "lots"}]}]), false).err().expect("a mistyped option is refused");
        assert!(error.to_string().starts_with("batch search: options: invalid type: string \"lots\""), "{error}");
        let error = parse(json!([{"op": "search", "args": ["x", {"max": 1e9}]}]), false).err().expect("too many rows are refused");
        assert_eq!(error.to_string(), "batch search: options: max must be from 1 to 2000 rows, got 1000000000");
    }
}
