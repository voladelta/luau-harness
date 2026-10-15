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

/// The recorded result of one effect.
#[derive(Clone, Debug)]
pub enum Ret {
    Num(f64),
    Str(String),
    Sh { stdout: Vec<u8>, stderr: Vec<u8>, code: Option<i64>, timed_out: bool },
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
            Ret::Sh { stdout, stderr, code, timed_out } => json!({"t": "sh", "stdout": log.blob(stdout)?, "stderr": log.blob(stderr)?, "code": code, "timed_out": timed_out}),
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
        Arc::new(Self { pol: supervisor.policy.clone(), depth, supervisor, principal, deadline, waiting: Arc::default() })
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
            if message.starts_with("unknown_outcome:") {
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
    pub fn sync(&self, _lua: &Lua, tool: Tool, class: Class, args: Value, f: impl FnOnce(Option<u64>) -> mlua::Result<Ret>) -> mlua::Result<Ret> {
        self.supervisor.authorize(&self.principal, tool, class).map_err(|e| rt(e.to_string()))?;
        let _permit = if class != Class::Read && tool != Tool::Agent {
            Some(self.supervisor.mutations.clone().try_acquire_owned().map_err(|_| rt("another mutation is in progress"))?)
        } else { None };
        let sha = args_sha(&args);
        let t0 = std::time::Instant::now();
        let call = Call { tool, class, args: &args, sha: &sha };
        let intent = self.begin(&call)?;
        let res = f(intent);
        self.finish(intent, &call, &res, t0.elapsed().as_millis() as u64)?;
        res
    }

    /// Authorize and record one asynchronous effect (sh, agent).
    pub async fn run<F, Fut>(&self, lua: &Lua, tool: Tool, class: Class, args: Value, f: F) -> mlua::Result<Ret>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = mlua::Result<Ret>>,
    {
        self.supervisor.authorize(&self.principal, tool, class).map_err(|e| rt(e.to_string()))?;
        let _permit = if class != Class::Read && tool != Tool::Agent {
            Some(self.supervisor.mutations.clone().acquire_owned().await.map_err(mlua::Error::external)?)
        } else { None };
        self.supervisor.check().map_err(mlua::Error::external)?;
        let sha = args_sha(&args);
        let t0 = std::time::Instant::now();
        let _wait = self.awaiting(lua);
        let call = Call { tool, class, args: &args, sha: &sha };
        let intent = self.begin(&call)?;
        let res = {
            match self.supervisor.remaining() {
                Some(remaining) => tokio::time::timeout(remaining, f()).await
                    .unwrap_or_else(|_| Err(rt("unknown_outcome: absolute run deadline interrupted this effect; it may have acted"))),
                None => f().await,
            }
        };
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
