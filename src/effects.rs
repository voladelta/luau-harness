//! Authorized effect dispatch with durable intents and receipts. Recovery never re-executes old chunks.
use crate::eventlog::{self, EventLog};
use crate::policy::{ApprovalRequired, ApprovalSource, Class, Policy};
use anyhow::Result;
use mlua::Lua;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use crate::supervisor::{Principal, Supervisor, Tool};
use crate::workflow_runtime::StageFacts;

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

/// `error` as a Luau error, keeping an `UnknownOutcome` or `ApprovalRequired` inside it recognisable by type (an
/// `anyhow::Error` boxed as it is would hide its type from `caused_by`).
pub fn effect_error(error: anyhow::Error) -> mlua::Error {
    let error = match error.downcast::<UnknownOutcome>() {
        Ok(error) => return mlua::Error::external(error),
        Err(error) => error,
    };
    match error.downcast::<ApprovalRequired>() {
        Ok(error) => mlua::Error::external(error),
        Err(error) => mlua::Error::external(error),
    }
}

/// Is `error`, or the error it wraps, an `E` raised from Rust?
fn caused_by<E: std::error::Error + 'static>(error: &mlua::Error) -> bool {
    match error {
        mlua::Error::ExternalError(error) => error.downcast_ref::<E>().is_some(),
        mlua::Error::CallbackError { cause, .. } | mlua::Error::WithContext { cause, .. } => caused_by::<E>(cause),
        _ => false,
    }
}

/// The recorded result of one effect.
#[derive(Clone, Debug)]
pub enum Ret {
    Num(f64),
    Str(String),
    /// File content, byte-exact (journaled like `Str`).
    Bytes(Vec<u8>),
    /// `approval` is who approved the command (`Policy::approve`); the outcome record carries it.
    Sh { stdout: Vec<u8>, stderr: Vec<u8>, code: Option<i64>, timed_out: bool, truncated: bool, operation_id: Option<u64>, approval: ApprovalSource },
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
            Ret::Bytes(b) => json!({"t": "str", "blob": log.blob(b)?}),
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
    pub deadline: Arc<crate::vm::Deadline>,
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
            initial: self.deadline.get(), started: crate::vm::coarse_now() }
    }
    #[cfg(test)]
    pub fn new(pol: Arc<Policy>, depth: usize, deadline: Arc<crate::vm::Deadline>) -> Arc<Effects> {
        let supervisor = Supervisor::ephemeral(pol.clone(), Arc::new(crate::modules::Registry::default()));
        Self::supervised(supervisor, Principal::Agent { read_only: false }, depth, deadline)
    }

    pub fn supervised(supervisor: Arc<Supervisor>, principal: Principal, depth: usize, deadline: Arc<crate::vm::Deadline>) -> Arc<Self> {
        Arc::new(Self { pol: supervisor.policy.clone(), stage_epoch:supervisor.workflow_epoch(), depth, supervisor, principal, deadline, waiting: Arc::default() })
    }

    pub fn log(&self) -> &Arc<EventLog> {
        &self.pol.log
    }

    fn base(&self, sha: &str) -> Value {
        json!({"depth": self.depth, "args_sha": sha})
    }

    fn begin(&self, c: &Call, stage: Option<&StageFacts>) -> mlua::Result<Option<u64>> {
        let Call { tool, class, args, sha } = *c;
        (class != Class::Read).then(|| self.supervisor.intent(tool, class, args, sha, self.depth, &self.principal, stage))
            .transpose().map_err(|e| rt(e.to_string()))
    }

    fn finish(&self, intent: Option<u64>, c: &Call, res: &mlua::Result<Ret>, ms: u64, stage: Option<&StageFacts>) -> mlua::Result<()> {
        let Call { tool, class, args, sha } = *c;
        if let (Some(id), Err(error)) = (intent, res) {
            let message = err_text(error);
            if caused_by::<UnknownOutcome>(error) {
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
        if let Ret::Sh { approval, .. } = &ret { r["approval"] = json!(approval.name()); }
        if let Some(stage) = stage { stage.record(&mut r); }
        if let Ret::Err(m) = &ret {
            r["detail"] = json!({"error": m.chars().take(300).collect::<String>()});
        }
        if res.as_ref().is_err_and(caused_by::<ApprovalRequired>) {
            r["denied"] = json!(true);
        }
        match intent {
            Some(id) => {
                r["of"] = json!(id);
                self.supervisor.record(crate::eventlog::LedgerKind::Outcome, r, false)
            }
            None => {
                r["class"] = json!(class.name());
                r["args"] = args.clone();
                self.log().write("call", r, false)
            }
        }.map_err(|e| rt(e.to_string()))?;
        Ok(())
    }

    /// Authorize and record one asynchronous effect (sh, agent).
    pub async fn run<F, Fut>(&self, lua: &Lua, tool: Tool, class: Class, args: Value, f: F) -> mlua::Result<Ret>
    where F: FnOnce() -> Fut, Fut: Future<Output = mlua::Result<Ret>> {
        self.run_with_intent(lua, tool, class, args, |_| f()).await
    }

    /// Authorize and record one asynchronous effect; `f` receives the durable intent's ID (None for reads).
    ///
    /// Starts with `yield_first`, so nothing is authorized, queued or journaled where the caller cannot yield.
    pub async fn run_with_intent<F, Fut>(&self, lua: &Lua, tool: Tool, class: Class, args: Value, f: F) -> mlua::Result<Ret>
    where
        F: FnOnce(Option<u64>) -> Fut,
        Fut: Future<Output = mlua::Result<Ret>>,
    {
        yield_first().await;
        self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
        self.supervisor.authorize(&self.principal, tool, class).map_err(|e| rt(e.to_string()))?;
        let _permit = if class != Class::Read {
            Some(self.supervisor.mutations.clone().acquire_owned().await.map_err(mlua::Error::external)?)
        } else { None };
        // State may change while queued behind another mutation.
        self.supervisor.authorize_stage(self.stage_epoch, tool, class).map_err(mlua::Error::external)?;
        self.supervisor.authorize(&self.principal, tool, class).map_err(mlua::Error::external)?;
        let sha = args_sha(&args);
        let t0 = std::time::Instant::now();
        let _wait = self.awaiting(lua);
        let call = Call { tool, class, args: &args, sha: &sha };
        let intent_stage = if class != Class::Read { self.supervisor.intent_stage(tool).await.map_err(mlua::Error::external)? } else { None };
        let intent = self.begin(&call, intent_stage.as_ref())?;
        let _active = crate::workflow_runtime::EffectGuard::new(&self.supervisor);
        let authorization = async {
            let path = match tool {
                Tool::Sh => args["cwd"].as_str(),
                Tool::Read | Tool::Search | Tool::View | Tool::Write | Tool::Edit => args["path"].as_str(),
                _ => None,
            };
            if let Some(path) = path {
                self.pol.authorize_path(path, matches!(tool, Tool::Write | Tool::Edit)).await.map_err(effect_error)?;
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
        let outcome_stage = self.supervisor.outcome_stage(tool, intent_stage.as_ref()).await.map_err(mlua::Error::external)?;
        self.finish(intent, &call, &res, t0.elapsed().as_millis() as u64, outcome_stage.as_ref())?;
        res
    }
}

/// The first await of an async binding that acts, before it does anything but parse its arguments.
///
/// mlua polls a binding's future once inside the call and yields the Luau thread while it is pending. Where the
/// thread cannot yield (a callback C code runs without a continuation: a `string.gsub` replacement function, a
/// `table.sort` comparator, a metamethod), that yield fails with "attempt to yield across metamethod/C-call
/// boundary" and the future is never polled again, only dropped when Luau collects it. This await makes that one
/// poll return before the binding has dispatched, authorized, queued or journaled anything, or taken a guard.
pub async fn yield_first() {
    tokio::task::yield_now().await;
}

/// Marks a thread as awaiting an effect; unmarks on drop, so an abandoned future (a coroutine left suspended inside
/// `sh` and later collected) cannot leave a stale mark behind.
pub struct AwaitGuard {
    _wait: WaitGuard,
    deadline: Arc<crate::vm::Deadline>,
    initial: Option<u64>,
    started: u64,
}
impl Drop for AwaitGuard {
    fn drop(&mut self) {
        if let Some(initial) = self.initial {
            self.deadline.extend_to(initial.saturating_add(crate::vm::coarse_now().saturating_sub(self.started)));
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

    fn effects(tag: &str) -> (Arc<Effects>, std::path::PathBuf, crate::dirs::TestDirs) {
        let dir = std::env::temp_dir().join(format!("hx-effects-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = crate::dirs::TestDirs::new();
        let pol = Policy::new(&dirs, &dir, false, None, None, None).unwrap();
        (Effects::new(Arc::new(pol), 0, Arc::default()), dir, dirs)
    }

    #[tokio::test]
    async fn queued_mutation_rechecks_uncertain_state_before_dispatch() {
        let (fx, dir, _dirs) = effects("queued");
        let lua = Lua::new();
        let permit = fx.supervisor.mutations.clone().acquire_owned().await.unwrap();
        let id = fx.supervisor.intent(Tool::McpCall, Class::External, &json!({}), "hash", 0, &fx.principal, None).unwrap();
        let executed = std::cell::Cell::new(false);
        let queued = fx.run(&lua, Tool::Sh, Class::LocalWrite, json!({}), || async {
            executed.set(true);
            Ok(Ret::Num(1.0))
        });
        tokio::pin!(queued);
        // Two rounds: the first poll only yields; the second passes authorization and queues for the permit.
        for _ in 0..2 {
            tokio::select! {
                biased;
                _ = &mut queued => panic!("mutation must wait for its permit"),
                _ = tokio::task::yield_now() => {},
            }
        }
        fx.supervisor.uncertain(id, Tool::McpCall, &json!({}), "lost response").unwrap();
        drop(permit);
        assert!(queued.await.is_err());
        assert!(!executed.get());
        assert!(fx.run(&lua, Tool::Read, Class::Read, json!({}), || async { Ok(Ret::Num(0.0)) }).await.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn uncertainty_uses_error_type_instead_of_message_text() {
        let (fx, dir, _dirs) = effects("typed");
        let lua = Lua::new();
        let result = fx.run(&lua, Tool::Write, Class::LocalWrite, json!({}), || async { Err(rt("unknown_outcome: ordinary error text")) }).await;
        assert!(result.is_err());
        assert!(fx.supervisor.uncertain_operations().is_empty());
        let result = fx.run(&lua, Tool::Write, Class::LocalWrite, json!({}), || async { Err(unknown("response lost")) }).await;
        assert!(result.is_err());
        assert_eq!(fx.supervisor.uncertain_operations().len(), 1);
        assert_eq!(crate::supervisor::Books::of(&crate::eventlog::load(&fx.log().path).unwrap()).unwrap().open.len(), 1);
        drop(fx);
        let _ = std::fs::remove_dir_all(dir);
    }
}
