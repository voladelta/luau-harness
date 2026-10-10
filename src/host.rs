//! Host: shared state for one harness run (model client, usage totals, trace), and subagent spawning.
use crate::effects::{LlmRec, ReplayData};
use crate::llm::{self, LlmCfg, Reply};
use crate::policy::Policy;
use crate::vm::Vm;
use anyhow::Result;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Usage {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    pub cost: f64,
    pub calls: u64,
}

pub struct Host {
    pub llm: LlmCfg,
    pub workspace: PathBuf,
    pub policy: Arc<Policy>,
    pub max_turns: usize,
    pub depth: usize,
    pub max_depth: usize,
    pub usage: Arc<Mutex<Usage>>,
    pub http: reqwest::Client,
    pub trace: Arc<Mutex<Option<std::fs::File>>>,
    pub key: String,
    pub budget: crate::policy::Budget,
    pub packs: Vec<String>,
    pub started: Instant,
    /// resume: queues for the top-level VM, the logged model replies, and where the blobs live
    replay: Mutex<Option<ReplayData>>,
    replay_llm: Mutex<VecDeque<LlmRec>>,
    replay_dir: Mutex<Option<PathBuf>>,
}

impl Host {
    pub fn new(llm: LlmCfg, workspace: PathBuf, policy: Arc<Policy>, max_turns: usize, trace: Option<PathBuf>, budget_flags: crate::policy::Budget, packs: Vec<String>) -> Result<Arc<Host>> {
        // flags (the launching user) beat policy.toml
        let pb = policy.budget.clone();
        let policy_budget = crate::policy::Budget { max_cost: budget_flags.max_cost.or(pb.max_cost), max_tokens: budget_flags.max_tokens.or(pb.max_tokens), max_wall_secs: budget_flags.max_wall_secs.or(pb.max_wall_secs) };
        let key = llm.key()?;
        let all_packs: Vec<String> = packs.into_iter().chain(policy.packs.clone()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        let file = trace.map(std::fs::File::create).transpose()?;
        Ok(Arc::new(Host {
            llm,
            workspace,
            policy,
            max_turns,
            depth: 0,
            max_depth: 2,
            usage: Arc::new(Mutex::new(Usage::default())),
            http: reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?,
            trace: Arc::new(Mutex::new(file)),
            key,
            budget: policy_budget,
            packs: all_packs,
            started: Instant::now(),
            replay: Mutex::new(None),
            replay_llm: Mutex::new(VecDeque::new()),
            replay_dir: Mutex::new(None),
        }))
    }

    /// Resume from a crashed run's log: the loop re-runs against the recorded model replies and effect results.
    pub fn set_replay(&self, mut rd: ReplayData) {
        *self.replay_llm.lock().unwrap() = std::mem::take(&mut rd.llm);
        *self.replay_dir.lock().unwrap() = Some(rd.dir.clone());
        *self.replay.lock().unwrap() = Some(rd);
    }

    pub fn child(&self) -> Arc<Host> {
        Arc::new(Host {
            llm: self.llm.clone(),
            workspace: self.workspace.clone(),
            policy: self.policy.clone(),
            max_turns: self.max_turns,
            depth: self.depth + 1,
            max_depth: self.max_depth,
            usage: self.usage.clone(),
            http: self.http.clone(),
            trace: self.trace.clone(),
            key: self.key.clone(),
            budget: self.budget.clone(),
            packs: self.packs.clone(),
            started: self.started,
            replay: Mutex::new(None),
            replay_llm: Mutex::new(VecDeque::new()),
            replay_dir: Mutex::new(None),
        })
    }

    pub fn log(&self, line: &str) {
        if let Some(f) = self.trace.lock().unwrap().as_mut() {
            let pad = "  ".repeat(self.depth);
            let _ = writeln!(f, "{}", line.lines().map(|l| format!("{pad}{l}")).collect::<Vec<_>>().join("\n"));
        }
    }

    /// Err(reason) when a limit is already used up. Checked before every LIVE model call (replayed replies are free).
    pub fn check_budget(&self) -> std::result::Result<(), String> {
        let u = self.usage.lock().unwrap();
        let b = &self.budget;
        if let Some(m) = b.max_cost.filter(|m| u.cost >= *m) {
            return Err(format!("budget_exceeded: cost ${:.4} reached the ${m:.4} limit", u.cost));
        }
        let tokens = u.input + u.cache_write + u.output;
        if let Some(m) = b.max_tokens.filter(|m| tokens >= *m) {
            return Err(format!("budget_exceeded: {tokens} tokens reached the {m} token limit"));
        }
        let wall = self.started.elapsed().as_secs_f64();
        if let Some(m) = b.max_wall_secs.filter(|m| wall >= *m) {
            return Err(format!("budget_exceeded: wall clock {wall:.0}s reached the {m:.0}s limit"));
        }
        Ok(())
    }

    /// Used and limit for each budget, for modules (`hx.budget()`).
    pub fn budget_status(&self) -> Value {
        let u = self.usage.lock().unwrap();
        json!({"cost": u.cost, "max_cost": self.budget.max_cost, "tokens": u.input + u.cache_write + u.output, "max_tokens": self.budget.max_tokens, "wall": self.started.elapsed().as_secs_f64(), "max_wall": self.budget.max_wall_secs})
    }

    pub async fn chat(&self, req: &Value) -> Result<Reply> {
        let req_sha = crate::eventlog::sha_bytes(serde_json::to_string(req)?.as_bytes());
        let logged = self.replay_llm.lock().unwrap().pop_front();
        let r = if let Some(rec) = logged {
            // replay: the model's earlier reply is returned as-is (no API call, no new log record)
            if rec.req_sha != req_sha {
                anyhow::bail!("replay diverged: the request to the model differs from the logged one (was the prompt, a module or the printer changed since the crash?)");
            }
            let dir = self.replay_dir.lock().unwrap().clone().unwrap_or_default();
            let v = &rec.rec;
            let body: Value = serde_json::from_slice(&crate::eventlog::read_blob(&dir, v["reply"].as_str().unwrap_or(""))?)?;
            Reply {
                text: body["text"].as_str().unwrap_or("").to_string(),
                calls: body["calls"].as_array().cloned().unwrap_or_default(),
                raw: body["raw"].as_str().unwrap_or("").to_string(),
                finish: v["finish"].as_str().map(String::from),
                input: v["input"].as_u64().unwrap_or(0),
                cache_read: v["cache_read"].as_u64().unwrap_or(0),
                cache_write: v["cache_write"].as_u64().unwrap_or(0),
                output: v["output"].as_u64().unwrap_or(0),
                cost: v["cost"].as_f64().unwrap_or(0.0),
            }
        } else {
            if let Err(why) = self.check_budget() {
                self.policy.log.write("budget", json!({"depth": self.depth, "reason": why}), true);
                anyhow::bail!(why);
            }
            let r = llm::chat(&self.http, &self.llm, &self.key, req).await?;
            // the reply blob holds everything the loop reads: the text, the tool calls and the raw assistant message
            let reply = self.policy.log.blob(json!({"text": r.text, "calls": r.calls, "raw": r.raw}).to_string().as_bytes());
            self.policy.log.write("llm", json!({"model": self.llm.model, "depth": self.depth, "req_sha": req_sha, "reply": reply, "input": r.input, "cache_read": r.cache_read, "cache_write": r.cache_write, "output": r.output, "cost": r.cost, "finish": r.finish}), true);
            r
        };
        let mut u = self.usage.lock().unwrap();
        u.input += r.input;
        u.cache_write += r.cache_write;
        u.cache_read += r.cache_read;
        u.output += r.output;
        u.cost += r.cost;
        u.calls += 1;
        Ok(r)
    }

    /// Run the std loop (or its project override) for `task` in a fresh VM. Boxed: subagents recurse through here.
    pub fn run_task(self: Arc<Self>, task: String, max_turns: usize) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            let t0 = Instant::now();
            let replay = self.replay.lock().unwrap().take();
            if replay.is_none() && self.depth == 0 {
                let blob = self.policy.log.blob(task.as_bytes());
                self.policy.log.write("task", json!({"blob": blob, "max_turns": max_turns, "model": self.llm.model, "depth": 0}), true);
            }
            let vm = Vm::new(self.policy.clone(), Duration::from_secs(120), 512 << 20, Some(self.clone()), replay.map(|r| (r.queues, r.dir)))?;
            let mut rep = vm.run_loop(&task, max_turns, &self.packs).await?;
            let u = self.usage.lock().unwrap();
            rep["model"] = json!(self.llm.model);
            rep["input_tokens"] = json!(u.input);
            rep["cache_creation_tokens"] = json!(u.cache_write);
            rep["cache_read_tokens"] = json!(u.cache_read);
            rep["output_tokens"] = json!(u.output);
            rep["cost_usd"] = json!(u.cost);
            rep["llm_calls"] = json!(u.calls);
            rep["wall_secs"] = json!(t0.elapsed().as_secs_f64());
            Ok(rep)
        })
    }
}

/// One line per file (gitignore-aware) so the model need not spend a turn on `ls`.
pub fn file_tree(root: &Path) -> String {
    let mut out = Vec::new();
    for e in ignore::WalkBuilder::new(root).require_git(false).sort_by_file_path(|a, b| a.cmp(b)).build().flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().to_string();
        if rel.contains("__pycache__") || rel.starts_with(".git/") || rel.starts_with(".harness/") {
            continue;
        }
        let lines = std::fs::read_to_string(e.path()).map(|t| t.lines().count()).unwrap_or(0);
        out.push(format!("{rel} ({lines} lines)"));
        if out.len() >= 80 {
            out.push("…".into());
            break;
        }
    }
    out.join("\n")
}
