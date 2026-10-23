//! Authorized effect dispatch with durable intents and receipts. Recovery never re-executes old chunks.
use crate::eventlog::{self, EventLog};
use crate::policy::{Class, Policy};
use anyhow::Result;
use mlua::Lua;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use crate::supervisor::{Principal, Supervisor, Tool};

pub fn rt(msg: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(msg.into())
}

/// An effect may have acted, so its durable intent must remain unresolved.
#[derive(Debug)]
pub struct UnknownOutcome(pub String);
impl std::fmt::Display for UnknownOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "unknown_outcome: {}", self.0) }
}
impl std::error::Error for UnknownOutcome {}

pub fn unknown(message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(UnknownOutcome(message.into()))
}

pub fn effect_error(error: anyhow::Error) -> mlua::Error {
    match error.downcast::<UnknownOutcome>() {
        Ok(error) => mlua::Error::external(error),
        Err(error) => mlua::Error::external(error),
    }
}

fn is_unknown(error: &mlua::Error) -> bool {
    match error {
        mlua::Error::ExternalError(error) => error.downcast_ref::<UnknownOutcome>().is_some(),
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => is_unknown(cause),
        _ => false,
    }
}

/// The recorded result of one effect.
#[derive(Clone, Debug)]
pub enum Ret {
    Num(f64),
    Str(String),
    Sh { stdout: Vec<u8>, stderr: Vec<u8>, code: Option<i64>, timed_out: bool, truncated: bool, operation_id: Option<u64> },
    Rows { rows: Vec<(String, i64, String)>, truncated: bool },
    SearchErr { kind: String, message: String, hint: String },
    Agent { answer: String, finished: bool, turns: u64 },
    /// generic structured result (MCP calls, promote, propose_module)
    Json(Value),
    Err(String),
}

impl Ret {
    pub fn to_json(&self, log: &EventLog) -> Result<Value> {
        Ok(match self {
            Ret::Num(v) => json!({"t": "num", "v": v}),
            Ret::Str(s) => json!({"t": "str", "blob": log.blob(s.as_bytes())?}),
            Ret::Sh { stdout, stderr, code, timed_out, truncated, .. } => json!({"t": "sh", "stdout": log.blob(stdout)?, "stderr": log.blob(stderr)?, "code": code, "timed_out": timed_out, "truncated": truncated}),
            Ret::Rows { rows, truncated } => json!({"t": "rows", "blob": log.blob(serde_json::to_string(rows).unwrap_or_default().as_bytes())?, "n": rows.len(), "truncated": truncated}),
            Ret::SearchErr { kind, message, hint } => json!({"t": "searcherr", "kind": kind, "message": message, "hint": hint}),
            Ret::Agent { answer, finished, turns } => json!({"t": "agent", "answer": log.blob(answer.as_bytes())?, "finished": finished, "turns": turns}),
            Ret::Json(v) => json!({"t": "json", "v": log.blob(serde_json::to_string(v).unwrap_or_default().as_bytes())?}),
            Ret::Err(m) => json!({"t": "err", "msg": m}),
        })
    }
}

pub struct Effects {
    pub pol: Arc<Policy>,
    pub depth: usize,
    pub supervisor: Arc<Supervisor>,
    pub principal: Principal,
    pub stage_epoch: Option<u64>,
    /// The chunk's time budget guards against Luau loops, not waiting: time spent awaiting an effect moves it forward.
    pub deadline: Arc<Mutex<Option<std::time::Instant>>>,
    /// Luau threads (by pointer) awaiting an effect, with a count per thread; the interrupt skips only those threads,
    /// so suspended native work does not spend the caller's Lua CPU allowance.
    /// The entry holds the thread itself so its address cannot be reused by another thread while it is marked.
    pub waiting: Arc<Mutex<HashMap<usize, (usize, mlua::Thread)>>>,
}

/// What one effect call is, for the log records around it.
#[derive(Clone, Copy)]
struct Call<'a> {
    tool: Tool,
    class: Class,
    args: &'a Value,
    sha: &'a str,
}

fn err_text(e: &mlua::Error) -> String {
    match e {
        mlua::Error::RuntimeError(m) => m.clone(),
        other => other.to_string().lines().next().unwrap_or("").to_string(),
    }
}

fn args_sha(args: &Value) -> String {
    eventlog::sha_bytes(serde_json::to_string(args).unwrap_or_default().as_bytes())
}

impl Effects {
    /// Suspend the caller's Lua CPU clock across isolated worker/provider waits.
    pub fn awaiting(&self, lua: &Lua) -> AwaitGuard {
        AwaitGuard { _wait: WaitGuard::enter(&self.waiting, lua.current_thread()), deadline: self.deadline.clone(),
            initial: *self.deadline.lock().unwrap(), started: std::time::Instant::now() }
    }
    #[cfg(test)]
    pub fn new(pol: Arc<Policy>, depth: usize, deadline: Arc<Mutex<Option<std::time::Instant>>>) -> Arc<Effects> {
        let supervisor = Supervisor::ephemeral(pol.clone(), Arc::new(crate::modules::Registry::default()));
        Self::supervised(supervisor, Principal::Agent { read_only: false }, depth, deadline)
    }

    pub fn supervised(supervisor: Arc<Supervisor>, principal: Principal, depth: usize, deadline: Arc<Mutex<Option<std::time::Instant>>>) -> Arc<Self> {
        Arc::new(Self { pol: supervisor.policy.clone(), stage_epoch:supervisor.workflow_epoch(), depth, supervisor, principal, deadline, waiting: Arc::default() })
    }

    pub fn log(&self) -> &Arc<EventLog> {
        &self.pol.log
    }

    fn base(&self, sha: &str) -> Value {
        json!({"depth": self.depth, "args_sha": sha})
    }

    fn begin(&self, c: &Call) -> mlua::Result<Option<u64>> {
        let Call { tool, class, args, sha } = *c;
        (class != Class::Read).then(|| self.supervisor.intent(tool, class, args, sha, self.depth, &self.principal))
            .transpose().map_err(|e| rt(e.to_string()))
    }

    fn finish(&self, intent: Option<u64>, c: &Call, res: &mlua::Result<Ret>, ms: u64) -> mlua::Result<()> {
        let Call { tool, class, args, sha } = *c;
        if let (Some(id), Err(error)) = (intent, res) {
            let message = err_text(error);
            if is_unknown(error) {
                return self.supervisor.uncertain(id, tool, args, &message).map_err(mlua::Error::external);
            }
        }
        let ret = match res {
            Ok(r) => r.clone(),
            Err(e) => Ret::Err(err_text(e)),
        };
        let mut r = self.base(sha);
        r["tool"] = json!(tool.name());
        r["ok"] = json!(res.is_ok());
        r["ms"] = json!(ms);
        r["ret"] = ret.to_json(self.log()).map_err(|e| rt(e.to_string()))?;
        self.supervisor.workflow_effect_metadata(tool, &mut r).map_err(mlua::Error::external)?;
        if let Ret::Err(m) = &ret {
            r["detail"] = json!({"error": m.chars().take(300).collect::<String>()});
            if m.starts_with("approval_required") {
                r["denied"] = json!(true);
            }
        }
        match intent {
            Some(id) => {
                r["of"] = json!(id);
                self.supervisor.record("outcome", r, false)
            }
            None => {
                r["class"] = json!(class.name());
                r["args"] = args.clone();
                self.log().write("call", r, false)
            }
        }.map_err(|e| rt(e.to_string()))?;
        Ok(())
    }

    /// Authorize and record one synchronous effect.
    /// `f` receives the durable intent's ID (None for reads).
    #[cfg(test)]
    pub fn sync(&self, _lua: &Lua, tool: Tool, class: Class, args: Value, f: impl FnOnce(Option<u64>) -> mlua::Result<Ret>) -> mlua::Result<Ret> {
        self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
        self.supervisor.authorize(&self.principal, tool, class).map_err(|e| rt(e.to_string()))?;
        let _permit = if class != Class::Read && tool != Tool::Agent {
            Some(self.supervisor.mutations.clone().try_acquire_owned().map_err(|_| rt("another mutation is in progress"))?)
        } else { None };
        self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
        self.supervisor.authorize(&self.principal, tool, class).map_err(mlua::Error::external)?;
        let sha = args_sha(&args);
        let t0 = std::time::Instant::now();
        let call = Call { tool, class, args: &args, sha: &sha };
        let intent = self.begin(&call)?;
        let _active = crate::workflow_runtime::EffectGuard::new(&self.supervisor);
        let res = f(intent);
        self.finish(intent, &call, &res, t0.elapsed().as_millis() as u64)?;
        res
    }

    /// Async authorization around a synchronous file executor. The intent ID still reaches atomic writes.
    pub async fn run_sync(&self, lua: &Lua, tool: Tool, class: Class, args: Value,
        f: impl FnOnce(Option<u64>) -> mlua::Result<Ret>) -> mlua::Result<Ret> {
        self.run_inner(lua, tool, class, args, |intent| async move { f(intent) }).await
    }

    /// Authorize and record one asynchronous effect (sh, agent).
    pub async fn run<F, Fut>(&self, lua: &Lua, tool: Tool, class: Class, args: Value, f: F) -> mlua::Result<Ret>
    where F: FnOnce() -> Fut, Fut: Future<Output = mlua::Result<Ret>> {
        self.run_inner(lua, tool, class, args, |_| f()).await
    }

    /// Authorize and record one asynchronous effect (sh, agent).
    async fn run_inner<F, Fut>(&self, lua: &Lua, tool: Tool, class: Class, args: Value, f: F) -> mlua::Result<Ret>
    where
        F: FnOnce(Option<u64>) -> Fut,
        Fut: Future<Output = mlua::Result<Ret>>,
    {
        self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
        self.supervisor.authorize(&self.principal, tool, class).map_err(|e| rt(e.to_string()))?;
        let _permit = if class != Class::Read && tool != Tool::Agent {
            Some(self.supervisor.mutations.clone().acquire_owned().await.map_err(mlua::Error::external)?)
        } else { None };
        // State may change while queued behind another mutation.
        self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
        self.supervisor.authorize(&self.principal, tool, class).map_err(mlua::Error::external)?;
        let sha = args_sha(&args);
        let t0 = std::time::Instant::now();
        let _wait = self.awaiting(lua);
        let call = Call { tool, class, args: &args, sha: &sha };
        let intent = self.begin(&call)?;
        let _active = crate::workflow_runtime::EffectGuard::new(&self.supervisor);
        let authorization = async {
            let path = match tool {
                Tool::Sh => args["cwd"].as_str(),
                Tool::Read | Tool::Search | Tool::View | Tool::Write | Tool::Edit => args["path"].as_str(),
                _ => None,
            };
            if let Some(path) = path {
                if tool == Tool::Edit && crate::policy::is_secret(std::path::Path::new(path)) {
                    return Err(rt("edit: policy: a credential file cannot be read"));
                }
                self.pol.authorize_path(path, matches!(tool, Tool::Write | Tool::Edit)).await.map_err(mlua::Error::external)?;
            }
            Ok(())
        };
        let allowed = match self.supervisor.remaining() {
            Some(remaining) => tokio::time::timeout(remaining, authorization).await
                .unwrap_or_else(|_| Err(rt("budget_exceeded: absolute run deadline reached while awaiting permission; effect was NOT run"))),
            None => authorization.await,
        };
        let mut res = match allowed {
            Err(e) => Err(e),
            Ok(()) => {
                // Recheck after an owner wait: a run may have been revoked or a stage may have ended.
                let dispatch = async {
                    self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
                    self.supervisor.authorize(&self.principal, tool, class).map_err(mlua::Error::external)?;
                    f(intent).await
                };
                match self.supervisor.remaining() {
                    Some(remaining) => tokio::time::timeout(remaining, dispatch).await
                        .unwrap_or_else(|_| Err(unknown("absolute run deadline interrupted this effect; it may have acted"))),
                    None => dispatch.await,
                }
            },
        };
        if let Ok(Ret::Sh {operation_id,..}) = &mut res { *operation_id = intent; }
        self.finish(intent, &call, &res, t0.elapsed().as_millis() as u64)?;
        res
    }
}

/// Marks a thread as awaiting an effect; unmarks on drop, so an abandoned future (a coroutine left suspended inside
/// `sh` and later collected) cannot leave a stale mark behind.
pub struct AwaitGuard {
    _wait: WaitGuard,
    deadline: Arc<Mutex<Option<std::time::Instant>>>,
    initial: Option<std::time::Instant>,
    started: std::time::Instant,
}
impl Drop for AwaitGuard {
    fn drop(&mut self) {
        if let Some(initial) = self.initial {
            let mut deadline = self.deadline.lock().unwrap();
            if let Some(current) = *deadline { *deadline = Some(current.max(initial + self.started.elapsed())); }
        }
    }
}

struct WaitGuard {
    map: Arc<Mutex<HashMap<usize, (usize, mlua::Thread)>>>,
    key: usize,
}

impl WaitGuard {
    fn enter(map: &Arc<Mutex<HashMap<usize, (usize, mlua::Thread)>>>, th: mlua::Thread) -> WaitGuard {
        let key = th.to_pointer() as usize;
        map.lock().unwrap().entry(key).or_insert((0, th)).0 += 1;
        WaitGuard { map: map.clone(), key }
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        let mut w = self.map.lock().unwrap();
        if let Some(e) = w.get_mut(&self.key) {
            e.0 -= 1;
            if e.0 == 0 {
                w.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn effects(tag: &str) -> (Arc<Effects>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hx-effects-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pol = Policy::new(&dir, false, None, None, None).unwrap();
        (Effects::new(Arc::new(pol), 0, Arc::default()), dir)
    }

    #[tokio::test]
    async fn queued_mutation_rechecks_uncertain_state_before_dispatch() {
        let (fx, dir) = effects("queued");
        let lua = Lua::new();
        let permit = fx.supervisor.mutations.clone().acquire_owned().await.unwrap();
        let id = fx.supervisor.intent(Tool::McpCall, Class::External, &json!({}), "hash", 0, &fx.principal).unwrap();
        let executed = std::cell::Cell::new(false);
        let queued = fx.run(&lua, Tool::Sh, Class::LocalWrite, json!({}), || async {
            executed.set(true);
            Ok(Ret::Num(1.0))
        });
        tokio::pin!(queued);
        tokio::select! {
            biased;
            _ = &mut queued => panic!("mutation must wait for its permit"),
            _ = tokio::task::yield_now() => {},
        }
        fx.supervisor.uncertain(id, Tool::McpCall, &json!({}), "lost response").unwrap();
        drop(permit);
        assert!(queued.await.is_err());
        assert!(!executed.get());
        assert!(fx.sync(&lua, Tool::Read, Class::Read, json!({}), |_| Ok(Ret::Num(0.0))).is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uncertainty_uses_error_type_instead_of_message_text() {
        let (fx, dir) = effects("typed");
        let lua = Lua::new();
        let result = fx.sync(&lua, Tool::Write, Class::LocalWrite, json!({}), |_| Err(rt("unknown_outcome: ordinary error text")));
        assert!(result.is_err());
        assert!(fx.supervisor.uncertain_operations().is_empty());
        let result = fx.sync(&lua, Tool::Write, Class::LocalWrite, json!({}), |_| Err(unknown("response lost")));
        assert!(result.is_err());
        assert_eq!(fx.supervisor.uncertain_operations().len(), 1);
        assert_eq!(crate::eventlog::verify(&fx.log().path).unwrap().unknown.len(), 1);
        drop(fx);
        let _ = std::fs::remove_dir_all(dir);
    }
}
