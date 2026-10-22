//! Model adapter and supervised agent/controller execution. Descendants share one admission ledger.
use crate::{controller::Controller, llm::{self, LlmCfg, Reply}, policy::{Budget, Policy}, supervisor::{Principal, RunState, Supervisor}, vm::Vm};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{future::Future, io::Write, path::PathBuf, pin::Pin, sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}}, time::{Duration, Instant}};

/// New requests after a 429 that generated nothing; other retryable failures get one.
const RATE_LIMIT_RETRIES: u32 = 8;

pub struct StartOptions {
    pub task: String,
    pub max_turns: usize,
    pub trace: Option<PathBuf>,
    pub budget: Budget,
    pub packs: Vec<String>,
    pub branch: Option<crate::supervisor::Branch>,
    pub workflow: Option<crate::workflow::WorkflowConfig>,
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

/// One single-shot completion for `llm{...}`: no tools, no history, the run's provider and model.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    system: String,
    prompt: String,
    /// absent: the run's effort; "none": send no reasoning setting; otherwise low..max
    effort: Option<String>,
    max_tokens: Option<u64>,
}

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
        let StartOptions { task, max_turns, trace, budget: flags, mut packs, branch, workflow } = options;
        let pb = &policy.budget;
        let budget = Budget { max_cost: flags.max_cost.or(pb.max_cost), max_tokens: flags.max_tokens.or(pb.max_tokens), max_wall_secs: flags.max_wall_secs.or(pb.max_wall_secs) };
        for limit in [budget.max_cost, budget.max_wall_secs].into_iter().flatten() {
            anyhow::ensure!(limit.is_finite() && limit >= 0.0, "budgets must be finite and nonnegative");
        }
        for pack in &policy.packs { if !packs.contains(pack) { packs.push(pack.clone()); } }
        llm.key()?; // Reject missing credentials before committing a new run manifest.
        let supervisor = Supervisor::start_workflow(policy, llm, task, max_turns, packs, budget, branch, workflow)?;
        Self::from_supervisor(supervisor, trace)
    }

    pub fn resume(policy: Arc<Policy>, trace: Option<PathBuf>) -> Result<Arc<Self>> {
        Self::from_supervisor(Supervisor::resume(policy)?, trace)
    }

    fn from_supervisor(supervisor: Arc<Supervisor>, trace: Option<PathBuf>) -> Result<Arc<Self>> {
        let m = supervisor.manifest.as_ref().context("missing run manifest")?;
        let key = if matches!(&supervisor.restored,Some(RunState::Finished{..}|RunState::AwaitingReview{..})) {m.model.key().unwrap_or_default()} else {m.model.key()?};
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
            "history": self.depth == 0,
            "net": !self.policy.enabled || self.policy.net})
    }

    pub(crate) fn report(&self, mut report: Value, wall_secs: f64) -> Value {
        let usage = self.supervisor.usage();
        report["model"] = json!(self.llm.model);
        report["input_tokens"] = json!(usage.input);
        report["cache_creation_tokens"] = json!(usage.cache_write);
        report["cache_read_tokens"] = json!(usage.cache_read);
        report["output_tokens"] = json!(usage.output);
        report["cost_usd"] = json!(usage.cost);
        report["llm_calls"] = json!(usage.calls);
        report["rate_limits"] = json!(usage.rate_limits);
        report["wall_secs"] = json!(wall_secs);
        report["budget"] = self.budget_status();
        report["log"] = json!(self.policy.log.path);
        let uncertain = self.supervisor.uncertain_operations();
        if !uncertain.is_empty() {
            report["finished"] = json!(false);
            if report.get("stopped").is_none() { report["stopped"] = json!("unknown_outcome: owner reconciliation required"); }
        }
        report["uncertain_operations"] = json!(uncertain);
        if self.is_workflow_root() {
            report["workflow"]=json!("rpi");
            if report.get("status").is_none() {report["status"]=json!(if report["finished"]==true {"completed"} else {"stopped"});}
            if let Some(frame)=self.supervisor.workflow.lock().unwrap().as_ref() {
                if report.get("stage").is_none() {report["stage"]=json!(frame.active_stage.as_ref().map(|s|&s.name));}
                report["root_model_admissions"]=json!(frame.root_turns);
                report["artifact_directory"]=json!(self.workspace.join(".agents/artifacts").join(format!("rpi-{}",self.policy.log.session)));
            }
        }
        if report["ops_per_turn"] == json!({}) { report["ops_per_turn"] = json!([]); }
        report
    }

    pub async fn chat(&self, req: &Value) -> Result<Reply> { self.chat_as(&self.llm, req).await }

    /// One budgeted, journaled request to `cfg`: the run's provider, key and model, possibly with another reasoning effort.
    pub async fn chat_as(&self, cfg: &LlmCfg, req: &Value) -> Result<Reply> {
        crate::controller::validate_request(req)?;
        let mut retries = 0;
        let key = if self.key.is_empty() {cfg.key()?} else {self.key.clone()};
        loop {
            let op = self.supervisor.reserve_model(cfg, req, self.depth).await?;
            let call = llm::chat(&self.http, cfg, &key, req);
            let result = match self.supervisor.remaining() {
                Some(remaining) => tokio::time::timeout(remaining, call).await.map_err(|_| anyhow::anyhow!("budget_exceeded: absolute run deadline reached during model request")).and_then(|r| r),
                None => call.await,
            };
            let error = match result {
                Ok(reply) => { self.supervisor.settle_model(op, cfg, &reply, self.depth)?; return Ok(reply); },
                Err(error) => error,
            };
            // An answer without a generation is classified. A lost response may have been billed: when its reservation
            // was a real bound it is charged at that bound and a new request may follow; otherwise it stays reserved for
            // the owner. The lost operation itself is never dispatched again.
            let answered = error.downcast_ref::<llm::Answered>();
            let closed = self.supervisor.fail_model(op, cfg, &error.to_string(), answered.map(|a| a.status), answered.is_some_and(|a| a.unbilled()), self.depth)?;
            let retry = match answered { Some(a) => a.retryable().then_some(a.retry_after), None => closed.then_some(None) };
            // A rate limit generated nothing, so waiting it out is free: up to RATE_LIMIT_RETRIES new requests, backing
            // off when the provider names no delay. Anything else gets one.
            let limit = if answered.is_some_and(|a| a.status == 429 && a.unbilled()) { RATE_LIMIT_RETRIES } else { 1 };
            match retry.filter(|_| retries < limit) {
                Some(after) => {
                    let wait = after.unwrap_or(2f64.powi(retries as i32 + 1)).clamp(1.0, 30.0);
                    if self.supervisor.remaining().is_some_and(|r| r.as_secs_f64() <= wait) { return Err(error); }
                    self.log(&format!("### MODEL RETRY\n{error}"));
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    retries += 1;
                },
                None => return Err(error),
            }
        }
    }

    /// Validate every completion, then run them concurrently. Results keep input order as `{ok, value}` or `{ok, error}`.
    pub async fn complete(&self, input: Value) -> Result<Value> {
        let requests: Vec<Completion> = serde_json::from_value(input).context("llm takes a list of {system, prompt, effort?, max_tokens?}")?;
        anyhow::ensure!((1..=32).contains(&requests.len()), "llm takes 1 to 32 requests");
        let jobs = requests.into_iter().map(|c| -> Result<_> {
            let mut cfg = self.llm.clone();
            match c.effort.as_deref() {
                None => {},
                Some("none") => cfg.reasoning = None,
                Some(effort @ ("low" | "medium" | "high" | "xhigh" | "max")) => cfg.reasoning = Some(effort.into()),
                Some(other) => anyhow::bail!("llm: effort must be none, low, medium, high, xhigh or max (got {other:?})"),
            }
            let mut req = json!({"system": c.system, "messages": [{"role": "user", "content": c.prompt}]});
            if let Some(max) = c.max_tokens { req["max_tokens"] = json!(max); }
            crate::controller::validate_request(&req)?;
            Ok(async move { self.chat_as(&cfg, &req).await })
        }).collect::<Result<Vec<_>>>()?;
        let results = futures::future::join_all(jobs).await.into_iter().map(|reply| match reply {
            Ok(reply) => json!({"ok": true, "value": reply.text}),
            Err(error) => json!({"ok": false, "error": error.to_string()}),
        }).collect();
        Ok(Value::Array(results))
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

    pub(crate) fn controller(&self) -> Result<Controller> {
        Controller::new(self.supervisor.registry.clone(), self.model_info(), String::new(), &self.packs,
            self.supervisor.manifest.as_ref().and_then(|m| m.deadline), if self.is_workflow_root() {"std/rpi"} else {"std/loop"})
    }

    async fn execute_task(self: Arc<Self>, task: String, max_turns: usize) -> Result<Value> {
        self.policy.log.check()?;
        let workflow=self.is_workflow_root();
        self.project_workflow_artifacts()?;
        if self.depth == 0 && let Some(RunState::Finished { report }) = &self.supervisor.restored {
            return Ok(self.report(Value::Object(report.clone()), 0.0));
        }
        let owner_event = if workflow && let Some(RunState::AwaitingReview{report,..})=&self.supervisor.restored {
            match self.resume_owner_decision() {
                Ok(Some(event))=>Some(event),
                Ok(None)=>return Ok(self.workflow_pause_report(report)),
                Err(error) if error.to_string().starts_with("budget_exceeded")=>return Ok(self.workflow_pause_report(report)),
                Err(error)=>return Err(error),
            }
        } else {None};
        if owner_event.is_none() {self.supervisor.check()?;}
        let t0 = Instant::now();
        let administrative = owner_event.is_some();
        let controller = if administrative {Controller::new(self.supervisor.registry.clone(),self.model_info(),String::new(),&self.packs,None,"std/rpi")?} else {self.controller()?};
        let mut state = if self.depth == 0 {
            match &self.supervisor.restored {
                Some(RunState::Running { controller }|RunState::AwaitingReview{controller,..}) => controller.clone(),
                Some(RunState::Finished { .. }) => unreachable!("finished runs returned above"),
                None => controller.init(&json!({"max_turns": max_turns,"workflow":self.supervisor.manifest.as_ref().and_then(|m|m.workflow.as_ref()),
                    "task_message": format!("{task}\n\n[files in the working directory]\n{}", file_tree(&self.workspace))}))?,
            }
        } else {controller.init(&json!({"max_turns": max_turns, "task_message": task}))?};
        let mut recovery = if self.depth==0 {self.supervisor.recovery.as_ref().map(|r|json!({"kind":"recover","message":serde_json::to_string_pretty(r).unwrap()}))} else {None};
        let pending=if workflow {self.supervisor.workflow.lock().unwrap().as_ref().and_then(|f|f.pending_event.clone())} else {None};
        let mut event=owner_event.or(pending).or_else(||recovery.take()).unwrap_or_else(||json!({"kind":"next"}));
        let mut vm:Option<Vm>=None;
        let mut compacting=false;
        loop {
            let admin=workflow && matches!(event["kind"].as_str(),Some("owner_decision"|"boundary_ready"));
            if admin {self.policy.log.check()?;} else {self.supervisor.check()?;}
            let previous_messages=state["messages"].as_array().cloned().unwrap_or_default();
            let was_compacting = compacting;
            let source_stage = self.supervisor.workflow.lock().unwrap().as_ref().and_then(|f| f.active_stage.as_ref().map(|s| s.id.clone()));
            let controller=if admin {Controller::new(self.supervisor.registry.clone(),self.model_info(),String::new(),&self.packs,None,"std/rpi")?} else {self.controller()?};
            let (next_state,mut command)=controller.step(&state,&event)?;
            ensure_object(&next_state)?;
            crate::controller::command(&command)?;
            state=next_state;
            if workflow && event["kind"]=="run_result" && command["kind"]!="boundary" {self.supervisor.finish_stage_chunk(false);}
            if workflow && admin {self.supervisor.workflow.lock().unwrap().as_mut().unwrap().pending_event=None;}
            if std::mem::take(&mut compacting) {
                let failed=event["kind"]=="model_error" || event["reply"]["text"].as_str().unwrap_or("").trim().is_empty();
                let kept=state["messages"].as_array().map_or(0,Vec::len).saturating_sub(1);
                self.log(&if failed {"### COMPACT FAILED".into()} else {format!("### COMPACT\nsummarized {} messages, kept {kept}, summary committed",previous_messages.len().saturating_sub(kept+1))});
            }
            if let Some(messages)=state["messages"].as_array() {
                for (i,message) in messages.iter().enumerate() {
                    if previous_messages.get(i)!=Some(message) && matches!(message["role"].as_str(),Some("user"|"tool")) {
                        self.log(&format!("### {}\n{}",if message["role"]=="tool" {"RESULT"} else {"HARNESS"},message["content"].as_str().unwrap_or("")));
                    }
                }
            }
            if command["kind"]=="model" || command["kind"]=="run" {
                self.supervisor.check()?;
                if vm.is_none() {
                    let fresh=Vm::new(self.policy.clone(),Duration::from_secs(120),512<<20,Some(self.clone()))?;
                    let mut packs=vec!["std/packages".into(),"std/mcp".into()];
                    for name in &self.packs {if matches!(name.as_str(),"std/vision"|"std/authoring") && !packs.contains(name) {packs.push(name.clone());}}
                    fresh.install_packs(&packs).await?;vm=Some(fresh);
                }
            }
            if command["kind"]=="model" && command["purpose"]=="agent" {
                let request=vm.as_ref().unwrap().prepare_request(&command["request"])?;
                state["messages"]=request["messages"].clone();
                if workflow {state["worker"]["messages"]=request["messages"].clone();}
                command["request"]=request;
            }
            compacting=command["kind"]=="model" && command["purpose"]=="compact";
            let command=crate::controller::command(&command)?;
            let archive = if let crate::controller::Command::Next {archive:Some(directive)} = &command {
                anyhow::ensure!(self.depth == 0, "history_archive_invalid: only root history is supported");
                anyhow::ensure!(was_compacting && event["kind"] == "model_reply" && event["reply"]["text"].as_str().is_some_and(|s| !s.trim().is_empty()),
                    "history_archive_invalid: no successful compaction reply");
                anyhow::ensure!(crate::history::summary(&state) == event["reply"]["text"].as_str(), "history_archive_invalid: summary differs from reply");
                Some(self.supervisor.history.prepare(directive, &previous_messages, &state, source_stage, self.policy.log.dir())?)
            } else {None};
            let saved=match &command {
                crate::controller::Command::Boundary{id,submission,next}=>self.apply_boundary(crate::workflow::Boundary{id:id.clone(),submission:submission.clone(),next:next.clone()},&state)?,
                crate::controller::Command::Finish{report}=>{anyhow::ensure!(!workflow,"RPI must finish through a durable workflow boundary");RunState::Finished{report:report.clone()}},
                _=>RunState::Running{controller:state.clone()},
            };
            if workflow && matches!(&command,crate::controller::Command::Model{..}) {
                let at_limit={
                    let mut store=self.supervisor.workflow.lock().unwrap();
                    let frame=store.as_mut().unwrap();
                    if frame.root_turns >= self.max_turns as u64 {true} else {frame.root_turns+=1;false}
                };
                if at_limit {
                    let report=json!({"finished":false,"final_answer":"","status":"stopped","workflow":"rpi","stopped":"max_turns: run-wide model admission limit reached"});
                    self.supervisor.checkpoint(&self.lane,RunState::Finished{report:report.as_object().unwrap().clone()},&self.durable.lock().unwrap())?;
                    return Ok(self.report(report,t0.elapsed().as_secs_f64()));
                }
            }
            self.supervisor.checkpoint_archived(&self.lane,saved.clone(),&self.durable.lock().unwrap(),archive)?;
            if let crate::controller::Command::Boundary{..}=&command {
                vm=None;self.project_workflow_artifacts()?;
                match saved {
                    RunState::Finished{report}=>return Ok(self.report(Value::Object(report),t0.elapsed().as_secs_f64())),
                    RunState::AwaitingReview{report,..}=>return Ok(self.report(Value::Object(report),t0.elapsed().as_secs_f64())),
                    _=>{event=self.supervisor.workflow.lock().unwrap().as_ref().unwrap().pending_event.clone().context("boundary delivery missing")?;continue;},
                }
            }
            match command {
                crate::controller::Command::Next{..}=>event=recovery.take().unwrap_or_else(||json!({"kind":"next"})),
                crate::controller::Command::NextCall=>event=json!({"kind":"next_call"}),
                crate::controller::Command::Model{request,..}=>{
                    match self.chat(&request).await {
                        Ok(reply)=>{self.log(&format!("### MODEL\n{}",reply.text));event=json!({"kind":"model_reply","reply":reply});},
                        Err(error)=>{let message=error.to_string();let safe_context_rejection=["API 400 ","API 413 ","API 422 "].iter().any(|p|message.starts_with(p));event=json!({"kind":"model_error","error":message,"safe_context_rejection":safe_context_rejection});},
                    }
                },
                crate::controller::Command::Run{code}=>{
                    self.supervisor.check()?;self.log(&format!("### CODE\n{code}"));
                    self.policy.log.write("chunk_begin",json!({"lane":self.lane,"depth":self.depth,"code":code}),true)?;
                    let mut result=vm.as_ref().unwrap().run_chunk(&code).await?;
                    if workflow {
                        if let Some(submission)=self.supervisor.finish_stage_chunk(result["error"].is_null()) {result["submission"]=submission;}
                        if result["error"].as_str().is_some_and(|e|e.contains("stage.submit") || e.contains("submission")) {result["submission_error"]=json!(true);}
                    }
                    let notifications=vm.as_ref().unwrap().prepare_request(&json!({"system":"","messages":[],"tools":[]}))?;
                    event=json!({"kind":"run_result","result":result,"notifications":notifications["messages"]});
                },
                crate::controller::Command::Finish{report}=>return Ok(self.report(Value::Object(report),t0.elapsed().as_secs_f64())),
                crate::controller::Command::Boundary{..}=>unreachable!(),
            }
        }
    }

}

/// A bounded, gitignore-aware listing used only to initialize the controller's saved conversation.
pub fn file_tree(root: &std::path::Path) -> String {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).require_git(false).parents(false).sort_by_file_path(|a,b| a.cmp(b)).build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) { continue; }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path()).to_string_lossy().to_string();
        if rel.contains("__pycache__") || rel.starts_with(".git/") || rel.starts_with(".harness/") { continue; }
        let lines = std::fs::read_to_string(entry.path()).map(|t| t.lines().count()).unwrap_or(0);
        out.push(format!("{rel} ({lines} lines)"));
        if out.len() >= 80 { out.push("…".into()); break; }
    }
    out.join("\n")
}

fn ensure_object(value:&Value)->Result<()> {anyhow::ensure!(value.is_object(),"controller state must be an object");Ok(())}
