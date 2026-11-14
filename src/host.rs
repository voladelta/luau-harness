//! Model adapter and supervised agent/controller execution. Descendants share one admission ledger.
use crate::{
    controller::{Command, Controller, Event},
    llm::{self, LlmCfg, Model, Reply},
    policy::{Budget, BudgetExceeded, Policy},
    supervisor::{Principal, RunState, Supervisor},
    vm::Vm,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    future::Future,
    io::Write,
    ops::ControlFlow,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// New requests after a 429 that generated nothing; other retryable failures get one.
const RATE_LIMIT_RETRIES: u32 = 8;

/// Seconds before retry number `retries + 1`: the provider's `Retry-After`, else 2, 4, 8 … s; always 1–30 s.
fn backoff(retries: u32, retry_after: Option<f64>) -> f64 {
    retry_after
        .unwrap_or(2f64.powi(retries as i32 + 1))
        .clamp(1.0, 30.0)
}

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
    /// What `chat` asks; every descendant shares it.
    pub model: Arc<dyn Model>,
    pub trace: Arc<Mutex<Option<std::fs::File>>>,
    pub packs: Vec<String>,
}

#[derive(serde::Deserialize)]
pub struct AgentOptions {
    #[serde(default = "default_agent_turns")]
    pub max_turns: usize,
    pub model: Option<String>,
    pub effort: Option<String>,
}

fn default_agent_turns() -> usize {
    12
}

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
            anyhow::ensure!(
                matches!(effort.as_str(), "low" | "medium" | "high" | "xhigh" | "max"),
                "agent: effort must be low, medium, high, xhigh or max (got {effort:?})"
            );
        }
        Ok(options)
    }

    pub fn from_lua(options: Option<mlua::Table>) -> mlua::Result<Self> {
        let value = options
            .map(|t| crate::values::copy_out(mlua::Value::Table(t)))
            .transpose()?
            .unwrap_or(Value::Null);
        Self::parse(value).map_err(mlua::Error::external)
    }
}

impl Host {
    /// Launch a new run against the provider `llm` names, over HTTP.
    pub fn start(llm: LlmCfg, policy: Arc<Policy>, options: StartOptions) -> Result<Arc<Self>> {
        Self::launch(llm, policy, options, None)
    }

    /// Launch a new run whose requests go to `model` instead of the provider `llm` names; `llm` is still the run's
    /// frozen model configuration.
    pub fn start_with(
        model: Arc<dyn Model>,
        llm: LlmCfg,
        policy: Arc<Policy>,
        options: StartOptions,
    ) -> Result<Arc<Self>> {
        Self::launch(llm, policy, options, Some(model))
    }

    fn launch(
        llm: LlmCfg,
        policy: Arc<Policy>,
        options: StartOptions,
        model: Option<Arc<dyn Model>>,
    ) -> Result<Arc<Self>> {
        let StartOptions {
            task,
            max_turns,
            trace,
            budget: flags,
            mut packs,
            branch,
            workflow,
        } = options;
        let pb = &policy.budget;
        let budget = Budget {
            max_tokens: flags.max_tokens.or(pb.max_tokens),
            max_wall_secs: flags.max_wall_secs.or(pb.max_wall_secs),
        };
        if let Some(limit) = budget.max_wall_secs {
            anyhow::ensure!(
                limit.is_finite() && limit >= 0.0,
                "budgets must be finite and nonnegative"
            );
        }
        for pack in &policy.packs {
            if !packs.contains(pack) {
                packs.push(pack.clone());
            }
        }
        if model.is_none() {
            llm.key()?; // Reject missing credentials before committing a new run manifest.
        }
        let launch = crate::supervisor::Launch {
            model: llm,
            task,
            max_turns,
            packs,
            budget,
            branch,
            workflow,
        };
        let supervisor = Supervisor::start(policy, launch)?;
        Self::from_supervisor(supervisor, trace, model)
    }

    /// Resume the run in `policy`'s journal against the provider its manifest names, over HTTP.
    pub fn resume(policy: Arc<Policy>, trace: Option<PathBuf>) -> Result<Arc<Self>> {
        Self::from_supervisor(Supervisor::resume(policy)?, trace, None)
    }

    /// Resume the run in `policy`'s journal with its requests going to `model`.
    pub fn resume_with(
        model: Arc<dyn Model>,
        policy: Arc<Policy>,
        trace: Option<PathBuf>,
    ) -> Result<Arc<Self>> {
        Self::from_supervisor(Supervisor::resume(policy)?, trace, Some(model))
    }

    /// The root host of `supervisor`'s run. Without `model`, it asks the manifest's provider over HTTP, with the key
    /// read now; a run that resumes finished or paused at an owner gate may have none yet, and reads it per request.
    fn from_supervisor(
        supervisor: Arc<Supervisor>,
        trace: Option<PathBuf>,
        model: Option<Arc<dyn Model>>,
    ) -> Result<Arc<Self>> {
        let m = supervisor
            .manifest
            .as_ref()
            .context("missing run manifest")?;
        let model = match model {
            Some(model) => model,
            None => {
                let key = if matches!(
                    &supervisor.restored,
                    Some(RunState::Finished { .. } | RunState::AwaitingReview { .. })
                ) {
                    m.model.key().ok()
                } else {
                    Some(m.model.key()?)
                };
                Arc::new(llm::Http::new(&m.model, key)?)
            }
        };
        let file = trace.map(std::fs::File::create).transpose()?;
        Ok(Arc::new(Self {
            llm: m.model.clone(),
            workspace: m.policy.workspace.clone(),
            policy: supervisor.policy.clone(),
            principal: Principal::Agent { read_only: false },
            durable: supervisor.durable.clone(),
            lane: "root".into(),
            max_turns: m.max_turns,
            depth: 0,
            max_depth: 2,
            model,
            trace: Arc::new(Mutex::new(file)),
            packs: m.packs.clone(),
            supervisor,
        }))
    }

    pub fn child(&self, options: &AgentOptions, principal: Principal) -> Result<Arc<Self>> {
        anyhow::ensure!(
            self.depth < self.max_depth,
            "agent depth limit ({}) reached",
            self.max_depth
        );
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut llm = self.llm.clone();
        if let Some(model) = &options.model {
            llm.model = model.clone();
        }
        if options.effort.is_some() {
            llm.reasoning = options.effort.clone();
        }
        Ok(Arc::new(Self {
            llm,
            workspace: self.workspace.clone(),
            policy: self.policy.clone(),
            supervisor: self.supervisor.clone(),
            principal,
            durable: Arc::new(Mutex::new(json!({}))),
            lane: format!("{}/{}", self.lane, NEXT.fetch_add(1, Ordering::Relaxed)),
            max_turns: self.max_turns,
            depth: self.depth + 1,
            max_depth: self.max_depth,
            model: self.model.clone(),
            trace: self.trace.clone(),
            packs: self.packs.clone(),
        }))
    }

    pub fn log(&self, line: &str) {
        if let Some(file) = self.trace.lock().unwrap().as_mut() {
            let _ = writeln!(file, "{line}");
        }
    }

    pub fn budget_status(&self) -> Value {
        self.supervisor.budget_status()
    }
    pub fn model_info(&self) -> Value {
        json!({"model": self.llm.model, "context_window": self.llm.context_window, "max_output": self.llm.max_output,
            "history": self.depth == 0, "autopilot": self.policy.autopilot, "confined": crate::confine::status().is_ok()})
    }

    pub(crate) fn report(&self, mut report: Value, wall_secs: f64) -> Value {
        let usage = self.supervisor.usage();
        report["model"] = json!(self.llm.model);
        report["input_tokens"] = json!(usage.input);
        report["cache_creation_tokens"] = json!(usage.cache_write);
        report["cache_read_tokens"] = json!(usage.cache_read);
        report["output_tokens"] = json!(usage.output);
        report["llm_calls"] = json!(usage.calls);
        report["rate_limits"] = json!(usage.rate_limits);
        report["wall_secs"] = json!(wall_secs);
        report["budget"] = self.budget_status();
        report["log"] = json!(self.policy.log.path);
        let uncertain = self.supervisor.uncertain_operations();
        if !uncertain.is_empty() {
            report["finished"] = json!(false);
            if report.get("stopped").is_none() {
                report["stopped"] = json!("unknown_outcome: owner reconciliation required");
            }
        }
        report["uncertain_operations"] = json!(uncertain);
        if let Some(workflow) = crate::workflow_runtime::root(self) {
            workflow.decorate_report(&mut report);
        }
        if report["ops_per_turn"] == json!({}) {
            report["ops_per_turn"] = json!([]);
        }
        report
    }

    pub async fn chat(&self, req: &Value) -> Result<Reply> {
        self.chat_as(&self.llm, req).await
    }

    /// One budgeted, journaled request to `cfg`: the run's provider, key and model, possibly with another reasoning effort.
    pub async fn chat_as(&self, cfg: &LlmCfg, req: &Value) -> Result<Reply> {
        crate::controller::validate_request(req)?;
        self.model.preflight(cfg)?;
        let mut retries = 0;
        loop {
            let op = self.supervisor.reserve_model(cfg, req, self.depth).await?;
            let call = self.model.send(cfg, req);
            let result = match self.supervisor.remaining() {
                Some(remaining) => tokio::time::timeout(remaining, call)
                    .await
                    .map_err(|_| {
                        BudgetExceeded("absolute run deadline reached during model request").into()
                    })
                    .and_then(|r| r),
                None => call.await,
            };
            let error = match result {
                Ok(reply) => {
                    self.supervisor.settle_model(op, cfg, &reply, self.depth)?;
                    return Ok(reply);
                }
                Err(error) => error,
            };
            // An answer without a generation is classified. A lost response may have been billed: it is charged its
            // reserved tokens and a new request may follow. The lost operation itself is never dispatched again.
            // A request that never reached the provider generated nothing either.
            let answered = error.downcast_ref::<llm::Answered>();
            let unsent = error
                .downcast_ref::<crate::net::Error>()
                .is_some_and(|e| e.delivery == crate::net::Delivery::NotSent);
            let unbilled = unsent || answered.is_some_and(|a| a.unbilled());
            let closed = self.supervisor.fail_model(
                op,
                cfg,
                &error.to_string(),
                answered.map(|a| a.status),
                unbilled,
                self.depth,
            )?;
            let retry = match answered {
                Some(a) => a.retryable().then_some(a.retry_after),
                None => closed.then_some(None),
            };
            // A rate limit generated nothing, so waiting it out is free: up to RATE_LIMIT_RETRIES new requests, backing
            // off when the provider names no delay. Anything else gets one.
            let limit = if answered.is_some_and(|a| a.status == 429 && a.unbilled()) {
                RATE_LIMIT_RETRIES
            } else {
                1
            };
            match retry.filter(|_| retries < limit) {
                Some(after) => {
                    let wait = backoff(retries, after);
                    if self
                        .supervisor
                        .remaining()
                        .is_some_and(|r| r.as_secs_f64() <= wait)
                    {
                        return Err(error);
                    }
                    self.log(&format!("### MODEL RETRY\n{error}"));
                    // test hook: the checks exercise retries, not the clock (HX_TEST_NO_BACKOFF)
                    if std::env::var_os("HX_TEST_NO_BACKOFF").is_none() {
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    }
                    retries += 1;
                }
                None => return Err(error),
            }
        }
    }

    /// Validate every completion, then run them concurrently. Results keep input order as `{ok, value}` or `{ok, error}`.
    pub async fn complete(&self, input: Value) -> Result<Value> {
        let requests: Vec<Completion> = serde_json::from_value(input)
            .context("llm takes a list of {system, prompt, effort?, max_tokens?}")?;
        anyhow::ensure!(
            (1..=32).contains(&requests.len()),
            "llm takes 1 to 32 requests"
        );
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
        let results = futures::future::join_all(jobs)
            .await
            .into_iter()
            .map(|reply| match reply {
                Ok(reply) => json!({"ok": true, "value": reply.text}),
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            })
            .collect();
        Ok(Value::Array(results))
    }

    pub fn run_task(
        self: Arc<Self>,
        task: String,
        max_turns: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            let started = Instant::now();
            let result = self.clone().execute_task(task, max_turns).await;
            if result.is_err()
                && let Err(error) = self.supervisor.check()
                && error.is::<BudgetExceeded>()
            {
                // Controller state is opaque; unavailable workflow counters must not masquerade as zero.
                let report =
                    json!({"finished":false,"final_answer":"","stopped":error.to_string()});
                self.policy.log.write(
                    "budget",
                    json!({"reason":error.to_string(),"depth":self.depth}),
                    true,
                )?;
                self.supervisor.checkpoint(
                    &self.lane,
                    RunState::Finished {
                        report: report.as_object().unwrap().clone(),
                    },
                    &self.durable.lock().unwrap(),
                )?;
                return Ok(self.report(report, started.elapsed().as_secs_f64()));
            }
            result
        })
    }

    /// The controller: the run's workflow's (named by the manifest) in the root agent, else std/loop. An
    /// administrative step dispatches nothing, so it runs without the run deadline.
    fn controller(&self, administrative: bool) -> Result<Controller> {
        let entry = crate::workflow_runtime::root(self)
            .map_or_else(|| "std/loop".into(), |w| w.controller_entry());
        let deadline = self.supervisor.manifest.as_ref().and_then(|m| m.deadline);
        Controller::new(
            self.supervisor.registry.clone(),
            self.model_info(),
            String::new(),
            deadline.filter(|_| !administrative),
            &entry,
        )
    }

    async fn execute_task(self: Arc<Self>, task: String, max_turns: usize) -> Result<Value> {
        self.policy.log.check()?;
        // The workflow's hook, in a root agent whose run has one; every use below is a boundary it needs.
        let workflow = crate::workflow_runtime::root(&self);
        if let Some(workflow) = &workflow {
            workflow.project_artifacts();
        }
        if self.depth == 0
            && let Some(RunState::Finished { report }) = &self.supervisor.restored
        {
            return Ok(self.report(Value::Object(report.clone()), 0.0));
        }
        let owner_event = match workflow.as_ref().map(|w| w.open_gate()).transpose()? {
            Some(ControlFlow::Break(paused)) => return Ok(paused),
            Some(ControlFlow::Continue(event)) => event,
            None => None,
        };
        if owner_event.is_none() {
            self.supervisor.check()?;
        }
        let t0 = Instant::now();
        let controller = self.controller(owner_event.is_some())?;
        let mut state = if self.depth == 0 {
            match &self.supervisor.restored {
                Some(RunState::Running { controller }|RunState::AwaitingReview{controller,..}) => controller.clone(),
                Some(RunState::Finished { .. }) => unreachable!("finished runs returned above"),
                None => controller.init(&json!({"max_turns": max_turns,"workflow":self.supervisor.manifest.as_ref().and_then(|m|m.workflow.as_ref()),
                    "task_message": format!("{task}\n\n[files in the working directory]\n{}", file_tree(&self.workspace))}))?,
            }
        } else {
            controller.init(&json!({"max_turns": max_turns, "task_message": task}))?
        };
        let mut recovery = if self.depth == 0 {
            self.supervisor
                .recovery
                .as_ref()
                .filter(|r| recovery_due(self.supervisor.restored.as_ref(), r))
                .map(|r| {
                    Event::Recover {
                        message: serde_json::to_string_pretty(r).unwrap(),
                    }
                    .to_json()
                })
        } else {
            None
        };
        let pending = workflow.as_ref().and_then(|w| w.pending_event());
        let mut event = owner_event
            .or(pending)
            .or_else(|| recovery.take())
            .unwrap_or_else(|| Event::Next.to_json());
        let mut vm: Option<Vm> = None;
        let mut compacting = false;
        // The system prompt of the last agent request as sent (VM hooks and local tools applied). A summarizer request
        // reuses it so it is a byte prefix of that request and the provider's cached prefix applies.
        let mut agent_system: Option<Value> = None;
        loop {
            let admin = workflow.as_ref().is_some_and(|w| w.administrative(&event));
            if admin {
                self.policy.log.check()?;
            } else {
                self.supervisor.check()?;
            }
            let previous_messages = state["messages"].as_array().cloned().unwrap_or_default();
            let was_compacting = compacting;
            let source_stage = workflow.as_ref().and_then(|w| w.stage_id());
            let controller = self.controller(admin)?;
            let (next_state, command) = controller.step(&state, &event)?;
            ensure_object(&next_state)?;
            let mut command = crate::controller::command(&command)?;
            state = next_state;
            if let Some(workflow) = &workflow {
                workflow.after_step(&event, &command, admin);
            }
            if std::mem::take(&mut compacting) {
                let failed = state["messages"].get(0) == previous_messages.first();
                let kept = state["messages"]
                    .as_array()
                    .map_or(0, Vec::len)
                    .saturating_sub(1);
                self.log(&if failed {
                    "### COMPACT FAILED".into()
                } else {
                    format!(
                        "### COMPACT\nsummarized {} messages, kept {kept}, summary committed",
                        previous_messages.len().saturating_sub(kept + 1)
                    )
                });
            }
            if let Some(messages) = state["messages"].as_array() {
                for (i, message) in messages.iter().enumerate() {
                    if previous_messages.get(i) != Some(message)
                        && matches!(message["role"].as_str(), Some("user" | "tool"))
                    {
                        self.log(&format!(
                            "### {}\n{}",
                            if message["role"] == "tool" {
                                "RESULT"
                            } else {
                                "HARNESS"
                            },
                            message["content"].as_str().unwrap_or("")
                        ));
                    }
                }
            }
            if matches!(command, Command::Model { .. } | Command::Run { .. }) {
                self.supervisor.check()?;
                if vm.is_none() {
                    let fresh = Vm::new(
                        self.policy.clone(),
                        Duration::from_secs(crate::vm::CHUNK_SECS),
                        512 << 20,
                        Some(self.clone()),
                    )?;
                    fresh.install_packs(&self.packs).await?;
                    vm = Some(fresh);
                }
            }
            if let Command::Model { request, purpose } = &mut command {
                match purpose.as_deref() {
                    Some("agent") => {
                        // VM hooks and local tools shape the request as sent; validate it again after them.
                        let prepared = vm.as_ref().unwrap().prepare_request(request)?;
                        crate::controller::validate_request(&prepared)?;
                        state["messages"] = prepared["messages"].clone();
                        agent_system = Some(prepared["system"].clone());
                        *request = prepared;
                    }
                    Some("compact") if request["replay"] == true => {
                        if let Some(system) = &agent_system {
                            request["system"] = system.clone();
                        }
                    }
                    _ => {}
                }
            }
            compacting = matches!(&command,Command::Model{purpose:Some(p),..} if p=="compact");
            let archive = if let Command::Next {
                archive: Some(directive),
            } = &command
            {
                anyhow::ensure!(
                    self.depth == 0,
                    "history_archive_invalid: only root history is supported"
                );
                anyhow::ensure!(
                    was_compacting
                        && event["kind"] == "model_reply"
                        && event["reply"]["text"]
                            .as_str()
                            .is_some_and(|s| !s.trim().is_empty()),
                    "history_archive_invalid: no successful compaction reply"
                );
                anyhow::ensure!(
                    crate::history::summary(&state) == event["reply"]["text"].as_str(),
                    "history_archive_invalid: summary differs from reply"
                );
                Some(self.supervisor.history.prepare(
                    directive,
                    &previous_messages,
                    &state,
                    source_stage,
                    self.policy.log.dir(),
                )?)
            } else {
                None
            };
            let saved = match &command {
                Command::Boundary {
                    id,
                    submission,
                    next,
                } => workflow
                    .as_ref()
                    .context("workflow boundary requires explicit root workflow selection")?
                    .apply_boundary(
                        crate::workflow::Boundary {
                            id: id.clone(),
                            submission: submission.clone(),
                            next: next.clone(),
                        },
                        &state,
                    )?,
                Command::Finish { report } => {
                    if let Some(workflow) = &workflow {
                        workflow.refuse_finish()?;
                    }
                    RunState::Finished {
                        report: report.clone(),
                    }
                }
                _ => RunState::Running {
                    controller: state.clone(),
                },
            };
            self.supervisor.checkpoint_archived(
                &self.lane,
                saved.clone(),
                &self.durable.lock().unwrap(),
                archive,
            )?;
            if let (Command::Boundary { .. }, Some(workflow)) = (&command, &workflow) {
                vm = None;
                workflow.project_artifacts();
                match saved {
                    RunState::Finished { report } => {
                        return Ok(self.report(Value::Object(report), t0.elapsed().as_secs_f64()));
                    }
                    RunState::AwaitingReview { report, .. } => {
                        return Ok(self.report(Value::Object(report), t0.elapsed().as_secs_f64()));
                    }
                    _ => {
                        event = workflow
                            .pending_event()
                            .context("boundary delivery missing")?;
                        continue;
                    }
                }
            }
            match command {
                Command::Next { .. } => {
                    event = recovery.take().unwrap_or_else(|| Event::Next.to_json())
                }
                Command::NextCall => event = Event::NextCall.to_json(),
                Command::Model { request, .. } => {
                    match self.chat(&request).await {
                        Ok(reply) => {
                            self.log(&format!("### MODEL\n{}", reply.text));
                            // the anchor's estimate covers the system prompt as sent, so its scale is not skewed by VM additions
                            let system_chars =
                                agent_system.as_ref().and_then(Value::as_str).map(str::len);
                            event = Event::ModelReply {
                                reply: serde_json::to_value(&reply)?,
                                system_chars,
                            }
                            .to_json();
                        }
                        Err(error) => {
                            let safe_context_rejection = error
                                .downcast_ref::<llm::Answered>()
                                .is_some_and(llm::Answered::may_be_context_overflow);
                            event = Event::ModelError {
                                error: error.to_string(),
                                safe_context_rejection,
                            }
                            .to_json();
                        }
                    }
                }
                Command::Run { code } => {
                    self.supervisor.check()?;
                    self.log(&format!("### CODE\n{code}"));
                    self.policy.log.write(
                        "chunk_begin",
                        json!({"lane":self.lane,"depth":self.depth,"code":code}),
                        true,
                    )?;
                    let mut result = vm.as_ref().unwrap().run_chunk(&code).await?;
                    if let Some(workflow) = &workflow {
                        workflow.end_chunk(&mut result);
                    }
                    let notifications = vm
                        .as_ref()
                        .unwrap()
                        .prepare_request(&json!({"system":"","messages":[],"tools":[]}))?;
                    event = Event::RunResult {
                        result,
                        notifications: notifications["messages"].clone(),
                    }
                    .to_json();
                }
                Command::Finish { report } => {
                    return Ok(self.report(Value::Object(report), t0.elapsed().as_secs_f64()));
                }
                Command::Boundary { .. } => unreachable!(),
            }
        }
    }
}

/// A bounded, gitignore-aware listing of at most 80 files with their sizes, used only to initialize the controller's
/// saved conversation. Hidden files and directories (`.git`, `.harness` among them) are left out. It stats each
/// listed file and reads none.
pub fn file_tree(root: &std::path::Path) -> String {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .require_git(false)
        .parents(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .to_string();
        if rel.contains("__pycache__") {
            continue;
        }
        match entry.metadata() {
            Ok(meta) => out.push(format!("{rel} ({} bytes)", meta.len())),
            Err(_) => out.push(rel),
        }
        if out.len() >= 80 {
            out.push("…".into());
            break;
        }
    }
    out.join("\n")
}

/// Whether a resumed root run gets the recovery turn: only when work was cut off. A run restored while running was
/// (its controller may be waiting for a model reply or a chunk result that never comes, and the recovery turn is what
/// returns it to a ready state); a run with no checkpoint was when receipts or uncertain operations exist. A run paused
/// at an owner gate (or finished) stopped on purpose; its next stage starts in a fresh VM and has nothing to recover.
fn recovery_due(restored: Option<&RunState>, recovery: &Value) -> bool {
    let listed = |key: &str| {
        recovery[key]
            .as_array()
            .is_some_and(|items| !items.is_empty())
    };
    match restored {
        Some(RunState::Running { .. }) => true,
        Some(RunState::AwaitingReview { .. } | RunState::Finished { .. }) => false,
        None => listed("receipts") || listed("uncertain_operations"),
    }
}

fn ensure_object(value: &Value) -> Result<()> {
    anyhow::ensure!(value.is_object(), "controller state must be an object");
    Ok(())
}

#[cfg(test)]
mod recovery_tests {
    use super::{RunState, recovery_due};
    use serde_json::json;
    #[test]
    fn only_cut_off_work_is_recovered() {
        let running = RunState::Running {
            controller: json!({}),
        };
        let paused = RunState::AwaitingReview { controller: json!({}), report: Default::default(),
            gate: serde_json::from_value(json!({"gate_id":"g","kind":"review","decisions":["approve"],"continuation":"next_stage"})).unwrap() };
        let interrupted = json!({"receipts":[{"kind":"chunk_begin"}],"uncertain_operations":[]});
        let uncertain = json!({"receipts":[],"uncertain_operations":[{"of":3}]});
        let clean = json!({"receipts":[],"uncertain_operations":[]});
        assert!(recovery_due(Some(&running), &interrupted) && recovery_due(Some(&running), &clean));
        assert!(
            recovery_due(None, &interrupted)
                && recovery_due(None, &uncertain)
                && !recovery_due(None, &clean)
        );
        assert!(
            !recovery_due(Some(&paused), &interrupted),
            "an owner gate is a pause, not a crash"
        );
    }
}

#[cfg(test)]
mod backoff_tests {
    #[test]
    fn retries_back_off_exponentially_within_bounds_and_honour_retry_after() {
        let schedule: Vec<f64> = (0..6).map(|n| super::backoff(n, None)).collect();
        assert_eq!(schedule, [2.0, 4.0, 8.0, 16.0, 30.0, 30.0]);
        assert_eq!(super::backoff(3, Some(5.0)), 5.0);
        assert_eq!(super::backoff(0, Some(0.0)), 1.0);
        assert_eq!(super::backoff(0, Some(120.0)), 30.0);
    }
}
