//! Model adapter and supervised agent/controller execution. Descendants share one admission ledger.
use crate::{controller::Controller, llm::{self, LlmCfg, Reply}, policy::{Budget, Policy}, supervisor::{Principal, RunState, Supervisor}, vm::Vm};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{future::Future, io::Write, path::PathBuf, pin::Pin, sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}}, time::{Duration, Instant}};

pub struct StartOptions {
    pub task: String,
    pub max_turns: usize,
    pub trace: Option<PathBuf>,
    pub budget: Budget,
    pub packs: Vec<String>,
    pub branch: Option<crate::supervisor::Branch>,
}

pub struct Host {
    pub llm: LlmCfg,
    pub workspace: PathBuf,
    pub policy: Arc<Policy>,
    pub supervisor: Arc<Supervisor>,
    pub principal: Principal,
    pub durable: Arc<Mutex<Value>>,
    pub lane: String,
    pub max_turns: usize,
    pub depth: usize,
    pub max_depth: usize,
    pub http: reqwest::Client,
    pub trace: Arc<Mutex<Option<std::fs::File>>>,
    pub key: String,
    pub packs: Vec<String>,
}

#[derive(serde::Deserialize)]
pub struct AgentOptions {
    #[serde(default = "default_agent_turns")]
    pub max_turns: usize,
    pub model: Option<String>,
    pub effort: Option<String>,
}

fn default_agent_turns() -> usize { 12 }

impl AgentOptions {
    pub fn parse(value: Value) -> Result<Self> {
        let options: Self = serde_json::from_value(if value.is_null() { json!({}) } else { value })
            .context("invalid agent options")?;
        if let Some(effort) = &options.effort {
            anyhow::ensure!(matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max"),
                "agent: effort must be low, medium, high, xhigh or max (got {effort:?})");
        }
        Ok(options)
    }

    pub fn from_lua(options: Option<mlua::Table>) -> mlua::Result<Self> {
        let value = options.map(|t| crate::values::copy_out(mlua::Value::Table(t))).transpose()?.unwrap_or(Value::Null);
        Self::parse(value).map_err(mlua::Error::external)
    }
}

impl Host {
    pub fn start(llm: LlmCfg, policy: Arc<Policy>, options: StartOptions) -> Result<Arc<Self>> {
        let StartOptions { task, max_turns, trace, budget: flags, mut packs, branch } = options;
        let pb = &policy.budget;
        let budget = Budget { max_cost: flags.max_cost.or(pb.max_cost), max_tokens: flags.max_tokens.or(pb.max_tokens), max_wall_secs: flags.max_wall_secs.or(pb.max_wall_secs) };
        for limit in [budget.max_cost, budget.max_wall_secs].into_iter().flatten() {
            anyhow::ensure!(limit.is_finite() && limit >= 0.0, "budgets must be finite and nonnegative");
        }
        for pack in &policy.packs { if !packs.contains(pack) { packs.push(pack.clone()); } }
        llm.key()?; // Reject missing credentials before committing a new run manifest.
        let supervisor = Supervisor::start(policy, llm, task, max_turns, packs, budget, branch)?;
        Self::from_supervisor(supervisor, trace)
    }

    pub fn resume(policy: Arc<Policy>, trace: Option<PathBuf>) -> Result<Arc<Self>> {
        Self::from_supervisor(Supervisor::resume(policy)?, trace)
    }

    fn from_supervisor(supervisor: Arc<Supervisor>, trace: Option<PathBuf>) -> Result<Arc<Self>> {
        let m = supervisor.manifest.as_ref().context("missing run manifest")?;
        let key = m.model.key()?;
        let file = trace.map(std::fs::File::create).transpose()?;
        Ok(Arc::new(Self { llm: m.model.clone(), workspace: m.policy.workspace.clone(), policy: supervisor.policy.clone(),
            principal: Principal::Agent { read_only: false }, durable: supervisor.durable.clone(), lane: "root".into(),
            max_turns: m.max_turns, depth: 0, max_depth: 2,
            http: reqwest::Client::builder().timeout(Duration::from_secs(300)).redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never()).build()?,
            trace: Arc::new(Mutex::new(file)), key, packs: m.packs.clone(), supervisor }))
    }

    pub fn child(&self, options: &AgentOptions, principal: Principal) -> Result<Arc<Self>> {
        anyhow::ensure!(self.depth < self.max_depth, "agent depth limit ({}) reached", self.max_depth);
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut llm = self.llm.clone();
        // The launch-time price is for the run's model; another model's requests fall back to the unpriced reservation.
        if let Some(model) = options.model.as_ref().filter(|m| **m != llm.model) { llm.model = model.clone(); llm.price = None; }
        if options.effort.is_some() { llm.reasoning = options.effort.clone(); }
        Ok(Arc::new(Self { llm, workspace: self.workspace.clone(), policy: self.policy.clone(), supervisor: self.supervisor.clone(),
            principal, durable: Arc::new(Mutex::new(json!({}))), lane: format!("{}/{}", self.lane, NEXT.fetch_add(1, Ordering::Relaxed)),
            max_turns: self.max_turns, depth: self.depth + 1, max_depth: self.max_depth, http: self.http.clone(), trace: self.trace.clone(),
            key: self.key.clone(), packs: self.packs.clone() }))
    }

    pub fn log(&self, line: &str) {
        if let Some(file) = self.trace.lock().unwrap().as_mut() { let _ = writeln!(file, "{line}"); }
    }

    pub fn budget_status(&self) -> Value { self.supervisor.budget_status() }
    pub fn model_info(&self) -> Value {
        json!({"model": self.llm.model, "context_window": self.llm.context_window, "max_output": self.llm.max_output,
            "net": !self.policy.enabled || self.policy.net})
    }

    fn report(&self, mut report: Value, wall_secs: f64) -> Value {
        let usage = self.supervisor.usage();
        report["model"] = json!(self.llm.model);
        report["input_tokens"] = json!(usage.input);
        report["cache_creation_tokens"] = json!(usage.cache_write);
        report["cache_read_tokens"] = json!(usage.cache_read);
        report["output_tokens"] = json!(usage.output);
        report["cost_usd"] = json!(usage.cost);
        report["llm_calls"] = json!(usage.calls);
        report["wall_secs"] = json!(wall_secs);
        report["budget"] = self.budget_status();
        report["log"] = json!(self.policy.log.path);
        let uncertain = self.supervisor.uncertain_operations();
        if !uncertain.is_empty() {
            report["finished"] = json!(false);
            if report.get("stopped").is_none() { report["stopped"] = json!("unknown_outcome: owner reconciliation required"); }
        }
        report["uncertain_operations"] = json!(uncertain);
        if report["ops_per_turn"] == json!({}) { report["ops_per_turn"] = json!([]); }
        report
    }

    pub async fn chat(&self, req: &Value) -> Result<Reply> {
        crate::controller::validate_request(req)?;
        let mut retried = false;
        loop {
            let op = self.supervisor.reserve_model(&self.llm, req, self.depth).await?;
            let call = llm::chat(&self.http, &self.llm, &self.key, req);
            let result = match self.supervisor.remaining() {
                Some(remaining) => tokio::time::timeout(remaining, call).await.map_err(|_| anyhow::anyhow!("budget_exceeded: absolute run deadline reached during model request")).and_then(|r| r),
                None => call.await,
            };
            let error = match result {
                Ok(reply) => { self.supervisor.settle_model(op, &self.llm, &reply, self.depth)?; return Ok(reply); },
                Err(error) => error,
            };
            // An answer without a generation is classified. A lost response may have been billed: when its reservation
            // was a real bound it is charged at that bound and a new request may follow; otherwise it stays reserved for
            // the owner. The lost operation itself is never dispatched again.
            let answered = error.downcast_ref::<llm::Answered>();
            let closed = self.supervisor.fail_model(op, &self.llm, &error.to_string(), answered.is_some_and(|a| a.unbilled()), self.depth)?;
            let retry = match answered { Some(a) => a.retryable().then_some(a.retry_after), None => closed.then_some(None) };
            match retry.filter(|_| !retried) {
                Some(after) => {
                    let wait = after.unwrap_or(2.0).clamp(1.0, 30.0);
                    if self.supervisor.remaining().is_some_and(|r| r.as_secs_f64() <= wait) { return Err(error); }
                    self.log(&format!("### MODEL RETRY\n{error}"));
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    retried = true;
                },
                None => return Err(error),
            }
        }
    }

    pub fn run_task(self: Arc<Self>, task: String, max_turns: usize) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            let started = Instant::now();
            let result = self.clone().execute_task(task, max_turns).await;
            if result.is_err() && let Err(error) = self.supervisor.check()
                && error.to_string().starts_with("budget_exceeded:") {
                // Controller state is opaque; unavailable workflow counters must not masquerade as zero.
                let report = json!({"finished":false,"final_answer":"","stopped":error.to_string()});
                self.policy.log.write("budget", json!({"reason":error.to_string(),"depth":self.depth}), true)?;
                self.supervisor.checkpoint(&self.lane, RunState::Finished { report: report.as_object().unwrap().clone() }, &self.durable.lock().unwrap())?;
                return Ok(self.report(report, started.elapsed().as_secs_f64()));
            }
            result
        })
    }

    fn controller(&self) -> Result<Controller> {
        Controller::new(self.supervisor.registry.clone(), self.model_info(), String::new(), &self.packs,
            self.supervisor.manifest.as_ref().and_then(|m| m.deadline))
    }

    async fn execute_task(self: Arc<Self>, task: String, max_turns: usize) -> Result<Value> {
        self.policy.log.check()?;
        if self.depth == 0 && let Some(RunState::Finished { report }) = &self.supervisor.restored {
            return Ok(self.report(Value::Object(report.clone()), 0.0));
        }
        self.supervisor.check()?;
        let t0 = Instant::now();
        let vm = Vm::new(self.policy.clone(), Duration::from_secs(120), 512 << 20, Some(self.clone()))?;
        let mut packs = vec!["std/packages".into(), "std/mcp".into()];
        for name in &self.packs {
            if matches!(name.as_str(), "std/vision" | "std/authoring") && !packs.contains(name) { packs.push(name.clone()); }
        }
        vm.install_packs(&packs).await?;
        let controller = self.controller()?;
        let mut state = if self.depth == 0 {
            match &self.supervisor.restored {
                Some(RunState::Running { controller }) => controller.clone(),
                Some(RunState::Finished { .. }) => unreachable!("finished runs returned above"),
                None => controller.init(&json!({"max_turns": max_turns,
                    "task_message": format!("{task}\n\n[files in the working directory]\n{}", file_tree(&self.workspace))}))?,
            }
        } else {
            controller.init(&json!({"max_turns": max_turns, "task_message": task}))?
        };
        let mut event = if self.depth == 0 {
            self.supervisor.recovery.as_ref().map(|r| json!({"kind": "recover", "message": serde_json::to_string_pretty(r).unwrap()})).unwrap_or_else(|| json!({"kind":"next"}))
        } else { json!({"kind":"next"}) };
        let mut compacting = false;
        loop {
            self.supervisor.check()?;
            let previous_messages = state["messages"].as_array().cloned().unwrap_or_default();
            // A controller transition starts from frozen source and explicit data, just like a package call.
            // Captured Lua locals cannot become a second, uncheckpointed workflow state store.
            let controller = self.controller()?;
            let (next_state, mut command) = controller.step(&state, &event)?;
            self.supervisor.check()?;
            anyhow::ensure!(next_state.is_object(), "controller state must be an object");
            crate::controller::command(&command)?;
            state = next_state;
            // The trace reads only commands and the conversation, never the controller's private state.
            if std::mem::take(&mut compacting) {
                let failed = event["kind"] == "model_error" || event["reply"]["text"].as_str().unwrap_or("").trim().is_empty();
                let kept = state["messages"].as_array().map_or(0, Vec::len).saturating_sub(1);
                if failed { self.log("### COMPACT FAILED"); }
                else { self.log(&format!("### COMPACT\nsummarized {} messages, kept {kept}, summary committed", previous_messages.len().saturating_sub(kept + 1))); }
            }
            if let Some(messages) = state["messages"].as_array() {
                for (i, message) in messages.iter().enumerate() {
                    if previous_messages.get(i) != Some(message) && matches!(message["role"].as_str(), Some("user" | "tool")) {
                        let label = if message["role"] == "tool" { "RESULT" } else { "HARNESS" };
                        self.log(&format!("### {label}\n{}", message["content"].as_str().unwrap_or("")));
                    }
                }
            }
            if command["kind"] == "model" && command["purpose"] == "agent" {
                let request = vm.prepare_request(&command["request"])?;
                // Image notifications and other data-only additions belong to the controller checkpoint too.
                state["messages"] = request["messages"].clone();
                command["request"] = request;
            }
            compacting = command["kind"] == "model" && command["purpose"] == "compact";
            let command = crate::controller::command(&command)?;
            self.supervisor.check()?;
            let saved = match &command {
                crate::controller::Command::Finish { report } => RunState::Finished { report: report.clone() },
                _ => RunState::Running { controller: state.clone() },
            };
            self.supervisor.checkpoint(&self.lane, saved, &self.durable.lock().unwrap())?;
            match command {
                crate::controller::Command::Next => event = json!({"kind":"next"}),
                crate::controller::Command::NextCall => event = json!({"kind":"next_call"}),
                crate::controller::Command::Model { request, .. } => {
                    match self.chat(&request).await {
                        Ok(reply) => {
                            self.log(&format!("### MODEL\n{}", reply.text));
                            event = json!({"kind":"model_reply", "reply":reply});
                        },
                        Err(error) => {
                            let message = error.to_string();
                            let safe_context_rejection = ["API 400 ", "API 413 ", "API 422 "].iter().any(|prefix| message.starts_with(prefix));
                            event = json!({"kind":"model_error", "error":message, "safe_context_rejection":safe_context_rejection});
                        },
                    }
                },
                crate::controller::Command::Run { code } => {
                    self.supervisor.check()?;
                    self.log(&format!("### CODE\n{code}"));
                    self.policy.log.write("chunk_begin", json!({"lane":self.lane, "depth":self.depth, "code":code}), true)?;
                    let result = vm.run_chunk(&code).await?;
                    let notifications = vm.prepare_request(&json!({"system":"", "messages":[], "tools":[]}))?;
                    event = json!({"kind":"run_result", "result": result, "notifications":notifications["messages"]});
                },
                crate::controller::Command::Finish { report } => {
                    return Ok(self.report(Value::Object(report), t0.elapsed().as_secs_f64()));
                },
            }
        }
    }
}

/// A bounded, gitignore-aware listing used only to initialize the controller's saved conversation.
pub fn file_tree(root: &std::path::Path) -> String {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).require_git(false).sort_by_file_path(|a,b| a.cmp(b)).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) { continue; }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_string_lossy().to_string();
        if rel.contains("__pycache__") || rel.starts_with(".git/") || rel.starts_with(".harness/") { continue; }
        let lines = std::fs::read_to_string(entry.path()).map(|t| t.lines().count()).unwrap_or(0);
        out.push(format!("{rel} ({lines} lines)"));
        if out.len() >= 80 { out.push("…".into()); break; }
    }
    out.join("\n")
}
