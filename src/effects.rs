//! Effects: every model-reachable primitive goes through here, live or replayed.
//!
//! Live: log an intent (write-ahead, synced) for effects that change things, run, log an outcome carrying the result.
//! Replay (resume after a crash): the logged results are returned in order and nothing is executed. An effect whose
//! intent has no outcome is an UNKNOWN OUTCOME: it is never re-run; the model gets an `unknown_outcome` error and
//! decides. Replay is per Lua thread (`parallel{}` branches get their own queue), and every replayed call is checked
//! against the logged tool + argument hash, so a changed harness fails loudly instead of replaying wrong results.
use crate::eventlog::{self, EventLog};
use crate::policy::{Class, Policy};
use anyhow::Result;
use mlua::Lua;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub fn rt(msg: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(msg.into())
}

/// The recorded result of one effect.
#[derive(Clone, Debug)]
pub enum Ret {
    Nil,
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
    pub fn to_json(&self, log: &EventLog) -> Value {
        match self {
            Ret::Nil => json!({"t": "nil"}),
            Ret::Num(v) => json!({"t": "num", "v": v}),
            Ret::Str(s) => json!({"t": "str", "blob": log.blob(s.as_bytes())}),
            Ret::Sh { stdout, stderr, code, timed_out } => json!({"t": "sh", "stdout": log.blob(stdout), "stderr": log.blob(stderr), "code": code, "timed_out": timed_out}),
            Ret::Rows { rows, truncated } => json!({"t": "rows", "blob": log.blob(serde_json::to_string(rows).unwrap_or_default().as_bytes()), "n": rows.len(), "truncated": truncated}),
            Ret::SearchErr { kind, message, hint } => json!({"t": "searcherr", "kind": kind, "message": message, "hint": hint}),
            Ret::Agent { answer, finished, turns } => json!({"t": "agent", "answer": log.blob(answer.as_bytes()), "finished": finished, "turns": turns}),
            Ret::Json(v) => json!({"t": "json", "v": log.blob(serde_json::to_string(v).unwrap_or_default().as_bytes())}),
            Ret::Err(m) => json!({"t": "err", "msg": m}),
        }
    }

    pub fn from_json(v: &Value, dir: &Path) -> Result<Ret> {
        let blob = |k: &str| eventlog::read_blob(dir, v[k].as_str().unwrap_or(""));
        Ok(match v["t"].as_str().unwrap_or("") {
            "nil" => Ret::Nil,
            "num" => Ret::Num(v["v"].as_f64().unwrap_or(0.0)),
            "str" => Ret::Str(String::from_utf8_lossy(&blob("blob")?).into_owned()),
            "sh" => Ret::Sh { stdout: blob("stdout")?, stderr: blob("stderr")?, code: v["code"].as_i64(), timed_out: v["timed_out"].as_bool().unwrap_or(false) },
            "rows" => Ret::Rows { rows: serde_json::from_slice(&blob("blob")?)?, truncated: v["truncated"].as_bool().unwrap_or(false) },
            "searcherr" => Ret::SearchErr { kind: v["kind"].as_str().unwrap_or("").into(), message: v["message"].as_str().unwrap_or("").into(), hint: v["hint"].as_str().unwrap_or("").into() },
            "agent" => Ret::Agent { answer: String::from_utf8_lossy(&blob("answer")?).into_owned(), finished: v["finished"].as_bool().unwrap_or(false), turns: v["turns"].as_u64().unwrap_or(0) },
            "json" => Ret::Json(serde_json::from_slice(&blob("v")?)?),
            "err" => Ret::Err(v["msg"].as_str().unwrap_or("").into()),
            other => anyhow::bail!("unknown recorded result type {other:?}"),
        })
    }
}

pub enum RecState {
    Done(Value),
    Unknown,
}

pub struct Rec {
    pub tool: String,
    pub args_sha: String,
    pub state: RecState,
    pub intent: Option<u64>,
}

pub struct LlmRec {
    pub req_sha: String,
    pub rec: Value,
}

pub struct ReplayData {
    pub queues: HashMap<String, VecDeque<Rec>>,
    pub llm: VecDeque<LlmRec>,
    pub task: Option<String>,
    pub dir: PathBuf,
}

/// Rebuild the replay queues from a log: top-level (depth 0) effects per Lua thread, and the model's replies in order.
pub fn load_replay(path: &Path) -> Result<ReplayData> {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut queues: HashMap<String, Vec<Rec>> = HashMap::new();
    let mut pos: HashMap<u64, (String, usize)> = HashMap::new();
    let (mut llm, mut task) = (VecDeque::new(), None);
    for r in eventlog::load(path)? {
        if r["depth"].as_u64().unwrap_or(0) != 0 {
            continue;
        }
        let tid = r["tid"].as_str().unwrap_or("0").to_string();
        let mk = |state, intent| Rec { tool: r["tool"].as_str().unwrap_or("").into(), args_sha: r["args_sha"].as_str().unwrap_or("").into(), state, intent };
        match r["kind"].as_str() {
            Some("task") if task.is_none() => task = Some(String::from_utf8_lossy(&eventlog::read_blob(&dir, r["blob"].as_str().unwrap_or(""))?).into_owned()),
            Some("intent") => {
                let q = queues.entry(tid.clone()).or_default();
                pos.insert(r["seq"].as_u64().unwrap_or(0), (tid, q.len()));
                q.push(mk(RecState::Unknown, r["seq"].as_u64()));
            }
            Some("outcome") => {
                if let (Some((t, i)), false) = (pos.get(&r["of"].as_u64().unwrap_or(0)), r["ret"].is_null()) {
                    queues.get_mut(t).unwrap()[*i].state = RecState::Done(r["ret"].clone());
                }
            }
            Some("call") => queues.entry(tid).or_default().push(mk(RecState::Done(r["ret"].clone()), None)),
            Some("llm") => llm.push_back(LlmRec { req_sha: r["req_sha"].as_str().unwrap_or("").into(), rec: r.clone() }),
            _ => {}
        }
    }
    Ok(ReplayData { queues: queues.into_iter().map(|(k, v)| (k, v.into())).collect(), llm, task, dir })
}

type Queues = (HashMap<String, VecDeque<Rec>>, PathBuf);

pub struct Effects {
    pub pol: Arc<Policy>,
    pub depth: usize,
    replay: Mutex<Option<Queues>>,
    threads: Mutex<HashMap<usize, String>>,
}

/// What one effect call is, for the log records around it.
#[derive(Clone, Copy)]
struct Call<'a> {
    tool: &'a str,
    class: Class,
    args: &'a Value,
    tid: &'a str,
    sha: &'a str,
}

enum Replayed {
    Done(Ret),
    Unknown(Option<u64>),
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
    pub fn new(pol: Arc<Policy>, depth: usize, replay: Option<Queues>) -> Arc<Effects> {
        Arc::new(Effects { pol, depth, replay: Mutex::new(replay), threads: Mutex::new(HashMap::new()) })
    }

    pub fn log(&self) -> &Arc<EventLog> {
        &self.pol.log
    }

    /// The id of the Lua thread running now: "0" for the chunk itself, "0.1" for the 2nd `parallel` branch, ...
    pub fn tid(&self, lua: &Lua) -> String {
        let p = lua.current_thread().to_pointer() as usize;
        self.threads.lock().unwrap().get(&p).cloned().unwrap_or_else(|| "0".into())
    }

    pub fn register_thread(&self, th: &mlua::Thread, tid: String) {
        self.threads.lock().unwrap().insert(th.to_pointer() as usize, tid);
    }

    pub fn unregister_thread(&self, th: &mlua::Thread) {
        self.threads.lock().unwrap().remove(&(th.to_pointer() as usize));
    }

    fn replayed(&self, tid: &str, tool: &str, sha: &str) -> mlua::Result<Option<Replayed>> {
        let mut g = self.replay.lock().unwrap();
        let Some((queues, dir)) = g.as_mut() else { return Ok(None) };
        let Some(rec) = queues.get_mut(tid).and_then(|q| q.pop_front()) else { return Ok(None) };
        if rec.tool != tool || rec.args_sha != sha {
            return Err(rt(format!(
                "replay diverged: the log has a `{}` call here but this run made a `{tool}` call with different arguments (was the harness, prompt or a module changed since the crash?)",
                rec.tool
            )));
        }
        Ok(Some(match rec.state {
            RecState::Done(v) => Replayed::Done(Ret::from_json(&v, dir).map_err(|e| rt(format!("replay: {e}")))?),
            RecState::Unknown => Replayed::Unknown(rec.intent),
        }))
    }

    fn base(&self, tid: &str, sha: &str) -> Value {
        json!({"tid": tid, "depth": self.depth, "args_sha": sha})
    }

    fn unknown(&self, tool: &str, tid: &str, sha: &str, intent: Option<u64>) -> mlua::Error {
        let msg = format!("unknown_outcome: the process died while this {tool} was running; it may or may not have taken effect. Check the current state before retrying. It was NOT run again automatically.");
        let mut o = self.base(tid, sha);
        o["of"] = json!(intent);
        o["tool"] = json!(tool);
        o["ok"] = json!(false);
        o["unknown_resolved"] = json!(true);
        o["ret"] = Ret::Err(msg.clone()).to_json(self.log());
        self.log().write("outcome", o, false);
        rt(msg)
    }

    fn into_result(r: Ret) -> mlua::Result<Ret> {
        match r {
            Ret::Err(m) => Err(rt(m)),
            other => Ok(other),
        }
    }

    fn begin(&self, c: &Call) -> Option<u64> {
        let Call { tool, class, args, tid, sha } = *c;
        (class != Class::Read).then(|| {
            let mut r = self.base(tid, sha);
            r["tool"] = json!(tool);
            r["class"] = json!(class.name());
            r["summary"] = json!(args.to_string().chars().take(300).collect::<String>());
            r["args"] = args.clone();
            self.log().write("intent", r, true)
        })
    }

    fn finish(&self, intent: Option<u64>, c: &Call, res: &mlua::Result<Ret>, ms: u64) {
        let Call { tool, class, args, tid, sha } = *c;
        let ret = match res {
            Ok(r) => r.clone(),
            Err(e) => Ret::Err(err_text(e)),
        };
        let mut r = self.base(tid, sha);
        r["tool"] = json!(tool);
        r["ok"] = json!(res.is_ok());
        r["ms"] = json!(ms);
        r["ret"] = ret.to_json(self.log());
        if let Ret::Err(m) = &ret {
            r["detail"] = json!({"error": m.chars().take(300).collect::<String>()});
            if m.starts_with("approval_required") {
                r["denied"] = json!(true);
            }
        }
        match intent {
            Some(id) => {
                r["of"] = json!(id);
                self.log().write("outcome", r, false)
            }
            None => {
                r["class"] = json!(class.name());
                r["args"] = args.clone();
                self.log().write("call", r, false)
            }
        };
    }

    /// A synchronous effect. `f` runs only when this is not a replay.
    pub fn sync(&self, lua: &Lua, tool: &str, class: Class, args: Value, f: impl FnOnce() -> mlua::Result<Ret>) -> mlua::Result<Ret> {
        let (tid, sha) = (self.tid(lua), args_sha(&args));
        match self.replayed(&tid, tool, &sha)? {
            Some(Replayed::Done(r)) => return Self::into_result(r),
            Some(Replayed::Unknown(i)) => return Err(self.unknown(tool, &tid, &sha, i)),
            None => {}
        }
        let t0 = std::time::Instant::now();
        let call = Call { tool, class, args: &args, tid: &tid, sha: &sha };
        let intent = self.begin(&call);
        let res = f();
        self.finish(intent, &call, &res, t0.elapsed().as_millis() as u64);
        res
    }

    /// An asynchronous effect (sh, agent). `f` runs only when this is not a replay.
    pub async fn run<F, Fut>(&self, lua: &Lua, tool: &str, class: Class, args: Value, f: F) -> mlua::Result<Ret>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = mlua::Result<Ret>>,
    {
        let (tid, sha) = (self.tid(lua), args_sha(&args));
        match self.replayed(&tid, tool, &sha)? {
            Some(Replayed::Done(r)) => return Self::into_result(r),
            Some(Replayed::Unknown(i)) => return Err(self.unknown(tool, &tid, &sha, i)),
            None => {}
        }
        let t0 = std::time::Instant::now();
        let call = Call { tool, class, args: &args, tid: &tid, sha: &sha };
        let intent = self.begin(&call);
        let res = f().await;
        self.finish(intent, &call, &res, t0.elapsed().as_millis() as u64);
        res
    }
}
