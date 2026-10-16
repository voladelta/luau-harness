//! One owner for admission, immutable run configuration, receipts, and recovery.
use crate::{eventlog::{self, Books, EventLog}, llm::{LlmCfg, Reply}, modules::Registry, policy::{Budget, Class, Policy, Snapshot}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::{SystemTime, UNIX_EPOCH}};

pub const FORMAT: u64 = 3;
pub fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64() }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u64,
    pub runtime: String,
    pub binary_sha: String,
    pub task: String,
    pub model: LlmCfg,
    pub max_turns: usize,
    pub packs: Vec<String>,
    pub budget: Budget,
    pub deadline: Option<f64>,
    pub policy: Snapshot,
    pub registry: Registry,
    #[serde(default)]
    pub branch: Option<Branch>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Branch { pub parent: std::path::PathBuf, pub durable: Value }

/// Host-owned lifecycle, independent of the customizable controller's private state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunState {
    Running { controller: Value },
    Finished { report: serde_json::Map<String, Value> },
}

#[derive(Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(flatten)]
    pub run: RunState,
    pub durable: Value,
}

impl Checkpoint {
    pub fn load(bytes: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(bytes)?;
        let checkpoint: Self = if value.get("status").is_some() {
            serde_json::from_value(value)?
        } else {
            // Compatibility with format-2 checkpoints written before the host owned terminal status.
            // Only this migration adapter knows the old standard controller's phase representation.
            let controller = value.get("controller").context("checkpoint missing controller")?;
            let run = if controller["phase"] == "finished" || controller["phase"]["kind"] == "finished" {
                RunState::Finished { report: controller["report"].as_object().context("checkpoint missing finish report")?.clone() }
            } else { RunState::Running { controller: controller.clone() } };
            Self { run, durable: value.get("durable").context("checkpoint missing durable data")?.clone() }
        };
        anyhow::ensure!(checkpoint.durable.is_object(), "invalid durable checkpoint");
        crate::values::validate(&checkpoint.durable)?;
        match &checkpoint.run {
            RunState::Running { controller } => {
                anyhow::ensure!(controller.is_object(), "controller state must be an object");
                crate::values::validate(controller)?;
            },
            RunState::Finished { report } => { crate::controller::command(&json!({"kind":"finish", "report":report}))?; },
        }
        Ok(checkpoint)
    }
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        // Luau modules and the prompt are restored from the manifest's registry, and FORMAT pins the journal's shape, so a
        // rebuilt binary resumes the same workflow. A changed host primitive surfaces as an error, never a replayed effect.
        let manifest = Self::inspect(path)?;
        if manifest.binary_sha != binary_sha()? { eprintln!("hx: note: this binary differs from the one that started the run"); }
        Ok(manifest)
    }

    pub fn inspect(path: &Path) -> Result<Self> {
        let records = eventlog::load(path)?;
        let record = records.iter().find(|r| r["kind"] == "manifest")
            .context("this journal has no manifest record; start a new run")?;
        let bytes = eventlog::read_blob(path.parent().context("log has no parent")?, record["blob"].as_str().context("manifest blob missing")?)?;
        let manifest: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(matches!(manifest.format, 2 | FORMAT), "unsupported run format {}", manifest.format);
        Ok(manifest)
    }
}

/// Settle what the evidence settles without the owner, after a crash:
/// - `write`/`edit` replace their file atomically after journaling an `expect` record, so no record means the file was
///   never touched; otherwise the file's hash says whether it landed. An unreadable file is left for the owner.
/// - `sh` that ran under Seatbelt with the network off and no approval-gated command could only write inside the
///   workspace and the run's temp dir, which the recovery turn can inspect: `unknown_local`.
/// - A model request whose reservation is a real bound (`bounded`) is charged at that bound.
///
/// Returns how many records were written.
pub fn reconcile(log: &EventLog, records: &[Value], manifest: &Manifest) -> Result<usize> {
    let books = Books::of(records)?;
    let confined = manifest.policy.enabled && !manifest.policy.net;
    let mut settled = 0;
    for (&id, intent) in &books.open {
        let (decision, evidence) = match intent["tool"].as_str() {
            Some("write" | "edit") => match records.iter().find(|r| r["kind"] == "expect" && r["of"] == id) {
                None => ("abandon", "interrupted before the file was touched"),
                Some(e) => match std::fs::read(e["path"].as_str().unwrap_or_default()) {
                    Ok(bytes) if eventlog::sha_bytes(&bytes) == e["sha256"] => ("complete", "the file holds the intended content"),
                    Ok(_) => ("abandon", "the file holds its previous content"),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => ("abandon", "the file was never created"),
                    Err(_) => continue,
                },
            },
            Some("sh") if confined && intent["class"] == "local_write" => ("unknown_local",
                "sandboxed with no network: it may have run, partly or fully, but could only change the workspace and the run's temp dir; inspect them before repeating it"),
            _ => continue,
        };
        log.write("resolution", json!({"of": id, "decision": decision, "tool": intent["tool"], "args_sha": intent["args_sha"], "by": "hx", "evidence": evidence}), true)?;
        settled += 1;
    }
    for (&id, reservation) in books.reservations.iter().filter(|(_, r)| r.bounded) {
        log.write("billing", bound_billing(id, reservation.tokens, reservation.cost), true)?;
        settled += 1;
    }
    Ok(settled)
}

/// A lost response's usage, charged at its reservation's bound.
fn bound_billing(of: u64, tokens: u64, cost: f64) -> Value {
    json!({"of": of, "tokens": tokens, "cost": cost, "source": "hx", "evidence": "response lost; charged at the request's reserved upper bound"})
}

fn binary_sha() -> Result<String> { Ok(eventlog::sha_bytes(&std::fs::read(std::env::current_exe()?)?)) }

/// Every effect a principal can ask for, and the capability it needs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool { Read, Search, View, Write, Edit, Sh, Agent, McpList, McpCall, Promote, ProposeModule, Http }

impl Tool {
    /// The name journal records carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read", Self::Search => "search", Self::View => "view", Self::Write => "write", Self::Edit => "edit",
            Self::Sh => "sh", Self::Agent => "agent", Self::McpList => "mcp_list", Self::McpCall => "mcp_call",
            Self::Promote => "promote", Self::ProposeModule => "propose_module",
            Self::Http => "http",
        }
    }

    fn capability(self) -> (&'static str, &'static str) {
        match self {
            Self::Read | Self::Search | Self::View => ("fs", "read"),
            Self::Write | Self::Edit | Self::Promote | Self::ProposeModule => ("fs", "write"),
            Self::Sh => ("proc", "sh"),
            Self::Agent => ("llm", "agent"),
            Self::McpList | Self::McpCall => ("mcp", "call"),
            Self::Http => ("http", "request"),
        }
    }
}

#[derive(Clone)]
pub enum Principal {
    Agent { read_only: bool },
    Package { name: String, caps: BTreeMap<String, Vec<String>>, active: Arc<AtomicBool> },
}

impl Principal {
    pub fn name(&self) -> &str { match self { Self::Agent { read_only: false } => "agent", Self::Agent { read_only: true } => "read-only agent", Self::Package { name, .. } => name } }
    pub(crate) fn allows(&self, cap: &str, scope: &str) -> bool {
        match self {
            Self::Agent { read_only } => !read_only || matches!((cap, scope), ("fs", "read") | ("llm", "agent")),
            Self::Package { caps, active, .. } => active.load(Ordering::Acquire) && caps.get(cap).is_some_and(|s| s.iter().any(|s| s == scope || s == "*")),
        }
    }

    pub fn read_only(&self) -> Self {
        match self {
            Self::Agent { .. } => Self::Agent { read_only: true },
            Self::Package { name, active, .. } => {
                let mut restricted = BTreeMap::new();
                if self.allows("fs", "read") { restricted.insert("fs".into(), vec!["read".into()]); }
                if self.allows("llm", "agent") { restricted.insert("llm".into(), vec!["agent".into()]); }
                Self::Package { name: name.clone(), caps: restricted, active: active.clone() }
            },
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64, pub cache_write: u64, pub cache_read: u64, pub output: u64, pub cost: f64, pub calls: u64,
}

pub struct Supervisor {
    pub policy: Arc<Policy>,
    pub registry: Arc<Registry>,
    pub manifest: Option<Manifest>,
    books: Mutex<Books>,
    settled: tokio::sync::Notify,
    pub mutations: Arc<tokio::sync::Semaphore>,
    pub durable: Arc<Mutex<Value>>,
    pub restored: Option<RunState>,
    pub recovery: Option<Value>,
}

impl Supervisor {
    pub fn start(policy: Arc<Policy>, model: LlmCfg, task: String, max_turns: usize, packs: Vec<String>, budget: Budget, branch: Option<Branch>) -> Result<Arc<Self>> {
        let mut registry = Registry::capture(&policy.workspace)?;
        registry.materialize_skills(&policy.log)?;
        let manifest = Manifest { format: FORMAT, runtime: env!("CARGO_PKG_VERSION").into(), binary_sha: binary_sha()?,
            task, model, max_turns, packs, deadline: budget.max_wall_secs.map(|s| now() + s), budget,
            policy: policy.snapshot(), registry: registry.clone(), branch };
        let durable = manifest.branch.as_ref().map(|b| b.durable.clone()).unwrap_or_else(|| json!({}));
        anyhow::ensure!(durable.is_object(), "invalid branch durable data");
        crate::values::validate(&durable)?;
        let blob = policy.log.blob(&serde_json::to_vec(&manifest)?)?;
        policy.log.write("manifest", json!({"format": FORMAT, "blob": blob}), true)?;
        Ok(Arc::new(Self { policy, registry: Arc::new(registry), manifest: Some(manifest), books: Mutex::new(Books::default()), settled: tokio::sync::Notify::new(), mutations: Arc::new(tokio::sync::Semaphore::new(1)), durable: Arc::new(Mutex::new(durable)), restored: None, recovery: None }))
    }

    pub fn ephemeral(policy: Arc<Policy>, registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self { policy, registry, manifest: None, books: Mutex::new(Books::default()), settled: tokio::sync::Notify::new(), mutations: Arc::new(tokio::sync::Semaphore::new(1)),
            durable: Arc::new(Mutex::new(json!({}))), restored: None, recovery: None })
    }

    pub fn resume(policy: Arc<Policy>) -> Result<Arc<Self>> {
        let path = policy.log.path.as_path();
        let manifest = Manifest::load(path)?;
        let mut records = eventlog::load(path)?;
        if reconcile(&policy.log, &records, &manifest)? > 0 { records = eventlog::load(path)?; }
        let mut books = Books::of(&records)?;
        books.crashed();
        let (frontier, checkpoint) = match records.iter().rev().find(|r| r["kind"] == "checkpoint" && r["lane"] == "root") {
            Some(r) => (r["seq"].as_u64().context("missing record sequence")?,
                Some(Checkpoint::load(&eventlog::read_blob(path.parent().unwrap(), r["blob"].as_str().context("missing checkpoint blob")?)?)?)),
            None => (0, None),
        };
        let receipts: Vec<_> = records.iter().filter(|r| r["seq"].as_u64().unwrap_or(0) > frontier && matches!(r["kind"].as_str(), Some("chunk_begin" | "intent" | "outcome" | "call" | "llm" | "model_intent" | "resolution" | "uncertain" | "billing")))
            .cloned().collect();
        let mut durable = checkpoint.as_ref().map(|c| c.durable.clone())
            .or_else(|| manifest.branch.as_ref().map(|b| b.durable.clone())).unwrap_or_else(|| json!({}));
        if !durable.is_object() { bail!("invalid durable checkpoint"); }
        crate::values::validate(&durable)?;
        let restored = checkpoint.map(|c| c.run);
        let recovery = Some(json!({"receipts": receipts, "log_directory":path.parent(), "uncertain_operations": books.open.values().cloned().collect::<Vec<_>>(),
            "notice": "Recovered into a fresh Luau VM. Ordinary globals and functions are gone; the conversation is restored. Old chunks were NOT executed. Inspect receipts before doing further work: a resolution `unknown_local` means a sandboxed `sh` may have run, so check the workspace before repeating it. Uncertain mutations require the owner's `hx recover` decision. To keep data across a future restart, save JSON-like values with `durable.set(key, value)` and read them with `durable.get(key)`; a completed chunk commits them."}));
        policy.log.write("resume", json!({"format": FORMAT, "checkpoint": frontier}), true)?;
        // The registry is restored from bytes in the protected journal, never from the workspace.
        let mut registry = manifest.registry.clone();
        registry.materialize_skills(&policy.log)?;
        Ok(Arc::new(Self { policy, registry: Arc::new(registry), manifest: Some(manifest), books: Mutex::new(books), settled: tokio::sync::Notify::new(), mutations: Arc::new(tokio::sync::Semaphore::new(1)), durable: Arc::new(Mutex::new(std::mem::take(&mut durable))), restored, recovery }))
    }

    pub fn check(&self) -> Result<()> {
        self.policy.log.check()?;
        if self.manifest.as_ref().and_then(|m| m.deadline).is_some_and(|d| now() >= d) { bail!("budget_exceeded: absolute run deadline reached (downtime counts)"); }
        Ok(())
    }

    pub fn remaining(&self) -> Option<std::time::Duration> {
        self.manifest.as_ref().and_then(|m| m.deadline).map(|d| std::time::Duration::from_secs_f64((d - now()).max(0.0)))
    }

    pub fn authorize(&self, principal: &Principal, tool: Tool, class: Class) -> Result<()> {
        self.check()?;
        if let Principal::Package { active, .. } = principal {
            anyhow::ensure!(active.load(Ordering::Acquire), "capability_denied: package initialization must be pure");
        }
        let (cap, scope) = tool.capability();
        anyhow::ensure!(principal.allows(cap, scope), "capability_denied: {} lacks {cap}.{scope} for {}", principal.name(), tool.name());
        if class != Class::Read && !self.books.lock().unwrap().uncertain.is_empty() {
            bail!("unknown_outcome: unresolved mutations block new mutations; inspect receipts and ask the owner to run `hx recover LOG OP --decision complete|abandon --yes`");
        }
        Ok(())
    }

    pub fn checkpoint(&self, lane: &str, run: RunState, durable: &Value) -> Result<()> {
        self.policy.log.check()?;
        let blob = self.policy.log.blob(&serde_json::to_vec(&Checkpoint { run, durable: durable.clone() })?)?;
        self.policy.log.write("checkpoint", json!({"lane": lane, "blob": blob}), true)?;
        Ok(())
    }

    pub fn intent(&self, tool: Tool, class: Class, args: &Value, args_sha: &str, depth: usize, principal: &Principal) -> Result<u64> {
        self.record("intent", json!({"tool":tool.name(), "class":class.name(), "args":args,
            "args_sha":args_sha, "depth":depth, "principal":principal.name(),
            "summary":args.to_string().chars().take(300).collect::<String>()}), true)
    }

    /// Record the file content a local write is about to produce, before it touches the file. With atomic replacement
    /// the file then holds either its old content or exactly this, so recovery can read the outcome off the disk.
    pub fn expect(&self, intent: u64, path: &Path, sha: &str) -> Result<()> {
        self.policy.log.write("expect", json!({"of": intent, "path": path, "sha256": sha}), true)?;
        Ok(())
    }

    pub fn uncertain(&self, id: u64, tool: Tool, args: &Value, message: &str) -> Result<()> {
        self.record("uncertain", json!({"of":id, "tool":tool.name(), "args":args, "error":message}), true)?;
        Ok(())
    }

    pub fn uncertain_operations(&self) -> Vec<u64> { self.books.lock().unwrap().uncertain.iter().copied().collect() }

    /// Append a journal record and apply it to the books under one lock, so the live books are the journal's fold.
    pub fn record(&self, kind: &str, fields: Value, sync: bool) -> Result<u64> {
        self.record_in(&mut self.books.lock().unwrap(), kind, fields, sync)
    }

    fn record_in(&self, books: &mut Books, kind: &str, fields: Value, sync: bool) -> Result<u64> {
        let seq = self.policy.log.write(kind, fields.clone(), sync)?;
        books.apply(seq, kind, &fields).map_err(|e| self.policy.log.fail(format!("journal ledger update failed: {e}")))?;
        Ok(seq)
    }

    /// Admit one model request. A request that only the reservations of running requests keep out waits for them to
    /// settle instead of failing; uncertain reservations never settle by themselves, so they still stop admission.
    pub async fn reserve_model(&self, cfg: &LlmCfg, req: &Value, depth: usize) -> Result<u64> {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if let Some(seq) = self.try_reserve_model(cfg, req, depth)? { return Ok(seq); }
            settled.await;
        }
    }

    fn try_reserve_model(&self, cfg: &LlmCfg, req: &Value, depth: usize) -> Result<Option<u64>> {
        self.check()?;
        let mut books = self.books.lock().unwrap();
        let budget = self.manifest.as_ref().map(|m| &m.budget).unwrap_or(&self.policy.budget);
        let used = books.usage.input + books.usage.cache_write + books.usage.output;
        let (pending_tokens, pending_cost) = books.reserved();
        // This is a conservative admission allowance, not a tokenizer or provider billing guarantee.
        let bytes = serde_json::to_vec(req)?.len() as u64;
        let max_output = req["max_tokens"].as_u64().unwrap_or(cfg.max_output).min(cfg.max_output);
        let tokens = bytes.saturating_add(max_output);
        let over_tokens = budget.max_tokens.is_some_and(|m| used.saturating_add(pending_tokens) >= m);
        let over_cost = budget.max_cost.is_some_and(|m| books.usage.cost + pending_cost >= m);
        if (over_tokens || over_cost) && books.reservations.values().any(|r| r.running) { return Ok(None); }
        if over_tokens {
            self.policy.log.write("budget", json!({"reason":"budget_exceeded: token admission threshold reached","depth":depth}), true)?;
            bail!("budget_exceeded: token admission threshold reached");
        }
        if over_cost {
            self.policy.log.write("budget", json!({"reason":"budget_exceeded: cost admission threshold reached (including uncertain requests)","depth":depth}), true)?;
            bail!("budget_exceeded: cost admission threshold reached (including uncertain requests)");
        }
        // A priced model reserves its request's upper bound, so concurrent requests run side by side. Without a price
        // there is no defensible per-request dollar bound: one outstanding request reserves the remaining allowance and
        // concurrent requests queue behind it above. `bounded` says the reservation is a real bound, so a lost response
        // can be charged at it instead of waiting on the owner.
        let remaining = budget.max_cost.map(|m| (m - books.usage.cost - pending_cost).max(0.0));
        let (cost, bounded) = match (remaining, &cfg.price) {
            (Some(r), Some(p)) => (p.bound(bytes, max_output).min(r), true),
            (Some(r), None) => (r, false),
            (None, _) => (0.0, true),
        };
        let seq = self.record_in(&mut books, "model_intent", json!({"model": cfg.model, "depth": depth,
            "req_sha": eventlog::sha_bytes(&serde_json::to_vec(req)?), "reserved_tokens": tokens, "reserved_cost": cost, "bounded": bounded}), true)?;
        Ok(Some(seq))
    }

    /// A failed request: a definite rejection releases its reservation; a possibly billed one with a bounded reservation
    /// is charged at that bound; otherwise it stays reserved as uncertain billing. Returns whether the reservation closed.
    pub fn fail_model(&self, op: u64, cfg: &LlmCfg, error: &str, unbilled: bool, depth: usize) -> Result<bool> {
        let mut books = self.books.lock().unwrap();
        let bound = books.reservations.get(&op).copied().filter(|r| r.bounded && !unbilled);
        let result = self.record_in(&mut books, "llm", json!({"of": op, "depth": depth, "model": cfg.model, "error": error, "billing_uncertain": !unbilled}), true)
            .and_then(|_| bound.map(|r| self.record_in(&mut books, "billing", bound_billing(op, r.tokens, r.cost), true)).transpose());
        Self::stop_running(&mut books, op);
        drop(books);
        self.settled.notify_waiters();
        result.map(|_| unbilled || bound.is_some())
    }

    pub fn settle_model(&self, op: u64, cfg: &LlmCfg, reply: &Reply, depth: usize) -> Result<()> {
        let mut books = self.books.lock().unwrap();
        let written = self.policy.log.blob(&serde_json::to_vec(reply)?).and_then(|blob| self.record_in(&mut books, "llm", json!({"of": op, "model": cfg.model,
            "depth": depth, "reply": blob, "input": reply.input, "cache_read": reply.cache_read, "cache_write": reply.cache_write,
            "output": reply.output, "cost": reply.cost, "finish": reply.finish}), true));
        Self::stop_running(&mut books, op);
        drop(books);
        self.settled.notify_waiters();
        written.map(|_| ())
    }

    /// A request whose receipt could not be journaled is no longer running but stays reserved: the journal failure is
    /// fatal to admission, and resume treats the reservation as uncertain.
    fn stop_running(books: &mut Books, op: u64) {
        if let Some(r) = books.reservations.get_mut(&op) { r.running = false; }
    }

    pub fn usage(&self) -> Usage { self.books.lock().unwrap().usage.clone() }

    pub fn budget_status(&self) -> Value {
        let books = self.books.lock().unwrap();
        let (reserved_tokens, reserved_cost) = books.reserved();
        let b = self.manifest.as_ref().map(|m| &m.budget).unwrap_or(&self.policy.budget);
        json!({"cost": books.usage.cost, "max_cost": b.max_cost, "tokens": books.usage.input + books.usage.cache_write + books.usage.output,
            "max_tokens": b.max_tokens, "reserved_tokens": reserved_tokens,
            "reserved_cost": reserved_cost, "max_wall": b.max_wall_secs,
            "deadline": self.manifest.as_ref().and_then(|m| m.deadline), "mode": "admission_threshold"})
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn old_checkpoints_migrate_without_losing_reports_or_saved_data() {
        for phase in [json!("finished"), json!({"kind":"finished"})] {
            let bytes = serde_json::to_vec(&json!({"controller":{"phase":phase,
                "report":{"finished":true,"final_answer":"done"}}, "durable":{"saved":7}})).unwrap();
            let checkpoint = Checkpoint::load(&bytes).unwrap();
            assert_eq!(checkpoint.durable["saved"], 7);
            let RunState::Finished { report } = checkpoint.run else { panic!("lost terminal status") };
            assert_eq!(report["final_answer"], "done");
        }
        let checkpoint = Checkpoint::load(br#"{"controller":{"custom":3},"durable":{}}"#).unwrap();
        assert!(matches!(checkpoint.run, RunState::Running { controller } if controller["custom"] == 3));
        assert!(Checkpoint::load(br#"{"status":"finished","report":{"finished":true},"durable":{}}"#).is_err());
    }
}
