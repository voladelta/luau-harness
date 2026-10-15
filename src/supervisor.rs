//! One owner for admission, immutable run configuration, receipts, and recovery.
use crate::{eventlog::{self, EventLog}, llm::{LlmCfg, Reply}, modules::Registry, policy::{Budget, Class, Policy, Snapshot}};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}}, time::{SystemTime, UNIX_EPOCH}};

pub const FORMAT: u64 = 2;
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
        anyhow::ensure!(manifest.format == FORMAT, "unsupported run format {}", manifest.format);
        Ok(manifest)
    }
}

/// Settle what the evidence settles without the owner, after a crash:
/// - `write`/`edit` replace their file atomically after journaling an `expect` record, so no record means the file was
///   never touched; otherwise the file's hash says whether it landed. An unreadable file is left for the owner.
/// - `sh` that ran under Seatbelt with the network off and no approval-gated command could only write inside the
///   workspace and the run's temp dir, which the recovery turn can inspect: `unknown_local`.
/// - A model request whose reservation is a real bound (`bounded`) is charged at that bound.
/// Returns how many records were written.
pub fn reconcile(log: &EventLog, records: &[Value], manifest: &Manifest) -> Result<usize> {
    let closed: std::collections::BTreeSet<u64> = records.iter()
        .filter(|r| matches!(r["kind"].as_str(), Some("outcome" | "resolution"))).filter_map(|r| r["of"].as_u64()).collect();
    let confined = manifest.policy.enabled && !manifest.policy.net;
    let mut settled = 0;
    for intent in records.iter().filter(|r| r["kind"] == "intent") {
        let Some(id) = intent["seq"].as_u64().filter(|id| !closed.contains(id)) else { continue };
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
    for intent in records.iter().filter(|r| r["kind"] == "model_intent" && r["bounded"] == true) {
        let Some(id) = intent["seq"].as_u64() else { continue };
        if records.iter().any(|r| r["of"] == id && eventlog::settles_model(r)) { continue; }
        log.write("billing", bound_billing(id, intent["reserved_tokens"].as_u64().unwrap_or(0), intent["reserved_cost"].as_f64().unwrap_or(0.0)), true)?;
        settled += 1;
    }
    Ok(settled)
}

/// A lost response's usage, charged at its reservation's bound.
fn bound_billing(of: u64, tokens: u64, cost: f64) -> Value {
    json!({"of": of, "tokens": tokens, "cost": cost, "source": "hx", "evidence": "response lost; charged at the request's reserved upper bound"})
}

fn binary_sha() -> Result<String> { Ok(eventlog::sha_bytes(&std::fs::read(std::env::current_exe()?)?)) }

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

#[derive(Default)]
struct Ledger {
    usage: Usage,
    // In-flight and uncertain generations remain reserved until a receipt, a definite rejection or owner reconciliation.
    pending: BTreeMap<u64, (u64, f64, bool)>,
    // The subset of `pending` whose request is still running and will settle on its own.
    inflight: std::collections::BTreeSet<u64>,
}

pub struct Supervisor {
    pub policy: Arc<Policy>,
    pub registry: Arc<Registry>,
    pub manifest: Option<Manifest>,
    ledger: Mutex<Ledger>,
    unresolved: Mutex<BTreeMap<u64, Value>>,
    settled: tokio::sync::Notify,
    pub mutations: Arc<tokio::sync::Semaphore>,
    pub durable: Arc<Mutex<Value>>,
    pub restored: Option<Value>,
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
        Ok(Arc::new(Self { policy, registry: Arc::new(registry), manifest: Some(manifest), ledger: Mutex::new(Ledger::default()),
            unresolved: Mutex::new(BTreeMap::new()), settled: tokio::sync::Notify::new(), mutations: Arc::new(tokio::sync::Semaphore::new(1)), durable: Arc::new(Mutex::new(durable)), restored: None, recovery: None }))
    }

    pub fn ephemeral(policy: Arc<Policy>, registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self { policy, registry, manifest: None, ledger: Mutex::new(Ledger::default()), unresolved: Mutex::new(BTreeMap::new()), settled: tokio::sync::Notify::new(), mutations: Arc::new(tokio::sync::Semaphore::new(1)),
            durable: Arc::new(Mutex::new(json!({}))), restored: None, recovery: None })
    }

    pub fn resume(policy: Arc<Policy>) -> Result<Arc<Self>> {
        let path = policy.log.path.as_deref().context("resume needs a journal")?;
        let manifest = Manifest::load(path)?;
        let mut records = eventlog::load(path)?;
        if reconcile(&policy.log, &records, &manifest)? > 0 { records = eventlog::load(path)?; }
        let mut ledger = Ledger::default();
        let mut unresolved = BTreeMap::new();
        let mut checkpoint = None;
        let mut frontier = 0;
        for r in &records {
            let seq = r["seq"].as_u64().context("missing record sequence")?;
            match r["kind"].as_str() {
                Some("intent") if r["class"] != "read" => { unresolved.insert(seq, r.clone()); },
                Some("outcome") => { unresolved.remove(&r["of"].as_u64().context("outcome missing operation ID")?); },
                Some("resolution") => { unresolved.remove(&r["of"].as_u64().context("resolution missing operation ID")?); },
                Some("model_intent") => { ledger.pending.insert(seq, (r["reserved_tokens"].as_u64().context("missing reservation")?, r["reserved_cost"].as_f64().context("missing reservation")?, r["bounded"] == true)); },
                Some("llm") if r.get("error").is_some() && r["billing_uncertain"] == false => {
                    ledger.pending.remove(&r["of"].as_u64().context("model receipt missing operation ID")?);
                },
                Some("llm") if r.get("error").is_none() => {
                    ledger.pending.remove(&r["of"].as_u64().context("model receipt missing operation ID")?);
                    ledger.usage.input += r["input"].as_u64().unwrap_or(0);
                    ledger.usage.cache_write += r["cache_write"].as_u64().unwrap_or(0);
                    ledger.usage.cache_read += r["cache_read"].as_u64().unwrap_or(0);
                    ledger.usage.output += r["output"].as_u64().unwrap_or(0);
                    ledger.usage.cost += r["cost"].as_f64().unwrap_or(0.0);
                    ledger.usage.calls += 1;
                },
                Some("billing") => {
                    ledger.pending.remove(&r["of"].as_u64().context("billing decision missing operation ID")?);
                    ledger.usage.input += r["tokens"].as_u64().context("billing decision missing tokens")?;
                    ledger.usage.cost += r["cost"].as_f64().context("billing decision missing cost")?;
                    ledger.usage.calls += 1;
                },
                Some("checkpoint") if r["lane"] == "root" => {
                    let data = eventlog::read_blob(path.parent().unwrap(), r["blob"].as_str().context("missing checkpoint blob")?)?;
                    checkpoint = Some(serde_json::from_slice::<Value>(&data)?); frontier = seq;
                },
                _ => {},
            }
        }
        let receipts: Vec<_> = records.iter().filter(|r| r["seq"].as_u64().unwrap_or(0) > frontier && matches!(r["kind"].as_str(), Some("chunk_begin" | "intent" | "outcome" | "call" | "llm" | "model_intent" | "resolution" | "uncertain" | "billing")))
            .cloned().collect();
        let mut durable = checkpoint.as_ref().map(|c| c["durable"].clone())
            .or_else(|| manifest.branch.as_ref().map(|b| b.durable.clone())).unwrap_or_else(|| json!({}));
        if !durable.is_object() { bail!("invalid durable checkpoint"); }
        crate::values::validate(&durable)?;
        let restored = checkpoint.map(|c| c["controller"].clone());
        let recovery = Some(json!({"receipts": receipts, "log_directory":path.parent(), "uncertain_operations": unresolved.values().cloned().collect::<Vec<_>>(),
            "notice": "Recovered into a fresh Luau VM. Ordinary globals and functions are gone; the conversation is restored. Old chunks were NOT executed. Inspect receipts before doing further work: a resolution `unknown_local` means a sandboxed `sh` may have run, so check the workspace before repeating it. Uncertain mutations require the owner's `hx recover` decision. To keep data across a future restart, save JSON-like values with `durable.set(key, value)` and read them with `durable.get(key)`; a completed chunk commits them."}));
        policy.log.write("resume", json!({"format": FORMAT, "checkpoint": frontier}), true)?;
        // The registry is restored from bytes in the protected journal, never from the workspace.
        let mut registry = manifest.registry.clone();
        registry.materialize_skills(&policy.log)?;
        Ok(Arc::new(Self { policy, registry: Arc::new(registry), manifest: Some(manifest), ledger: Mutex::new(ledger),
            unresolved: Mutex::new(unresolved), settled: tokio::sync::Notify::new(), mutations: Arc::new(tokio::sync::Semaphore::new(1)), durable: Arc::new(Mutex::new(std::mem::take(&mut durable))), restored, recovery }))
    }

    pub fn check(&self) -> Result<()> {
        self.policy.log.check()?;
        if self.manifest.as_ref().and_then(|m| m.deadline).is_some_and(|d| now() >= d) { bail!("budget_exceeded: absolute run deadline reached (downtime counts)"); }
        Ok(())
    }

    pub fn remaining(&self) -> Option<std::time::Duration> {
        self.manifest.as_ref().and_then(|m| m.deadline).map(|d| std::time::Duration::from_secs_f64((d - now()).max(0.0)))
    }

    pub fn authorize(&self, principal: &Principal, tool: &str, class: Class) -> Result<()> {
        self.check()?;
        if let Principal::Package { active, .. } = principal {
            anyhow::ensure!(active.load(Ordering::Acquire), "capability_denied: package initialization must be pure");
        }
        let need = match tool {
            "read" | "search" | "view" => Some(("fs", "read")),
            "write" | "edit" => Some(("fs", "write")),
            "sh" => Some(("proc", "sh")),
            "agent" => Some(("llm", "agent")),
            "mcp_list" | "mcp_call" => Some(("mcp", "call")),
            "nd" => None,
            "tree" => Some(("fs", "read")),
            "promote" | "propose_module" => Some(("fs", "write")),
            _ => bail!("unregistered effect: {tool}"),
        };
        if let Some((cap, scope)) = need {
            anyhow::ensure!(principal.allows(cap, scope), "capability_denied: {} lacks {cap}.{scope} for {tool}", principal.name());
        }
        if class != Class::Read && !self.unresolved.lock().unwrap().is_empty() {
            bail!("unknown_outcome: unresolved mutations block new mutations; inspect receipts and ask the owner to run `hx recover LOG OP --decision complete|abandon --yes`");
        }
        Ok(())
    }

    pub fn checkpoint(&self, lane: &str, controller: &Value, durable: &Value) -> Result<()> {
        self.policy.log.check()?;
        let blob = self.policy.log.blob(&serde_json::to_vec(&json!({"controller": controller, "durable": durable}))?)?;
        self.policy.log.write("checkpoint", json!({"lane": lane, "blob": blob}), true)?;
        Ok(())
    }

    pub fn intent(&self, tool: &str, class: Class, args: &Value, args_sha: &str, depth: usize, principal: &Principal) -> Result<u64> {
        self.policy.log.write("intent", json!({"tool":tool, "class":class.name(), "args":args,
            "args_sha":args_sha, "depth":depth, "principal":principal.name(),
            "summary":args.to_string().chars().take(300).collect::<String>()}), true)
    }

    /// Record the file content a local write is about to produce, before it touches the file. With atomic replacement
    /// the file then holds either its old content or exactly this, so recovery can read the outcome off the disk.
    pub fn expect(&self, intent: u64, path: &Path, sha: &str) -> Result<()> {
        self.policy.log.write("expect", json!({"of": intent, "path": path, "sha256": sha}), true)?;
        Ok(())
    }

    pub fn uncertain(&self, id: u64, tool: &str, args: &Value, message: &str) -> Result<()> {
        let record = json!({"of":id, "tool":tool, "args":args, "error":message});
        self.policy.log.write("uncertain", record.clone(), true)?;
        self.unresolved.lock().unwrap().insert(id, record);
        Ok(())
    }

    pub fn uncertain_operations(&self) -> Vec<u64> { self.unresolved.lock().unwrap().keys().copied().collect() }

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
        let mut ledger = self.ledger.lock().unwrap();
        let budget = self.manifest.as_ref().map(|m| &m.budget).unwrap_or(&self.policy.budget);
        let used = ledger.usage.input + ledger.usage.cache_write + ledger.usage.output;
        let pending_tokens: u64 = ledger.pending.values().map(|v| v.0).sum();
        let pending_cost: f64 = ledger.pending.values().map(|v| v.1).sum();
        // This is a conservative admission allowance, not a tokenizer or provider billing guarantee.
        let bytes = serde_json::to_vec(req)?.len() as u64;
        let max_output = req["max_tokens"].as_u64().unwrap_or(cfg.max_output).min(cfg.max_output);
        let tokens = bytes.saturating_add(max_output);
        let over_tokens = budget.max_tokens.is_some_and(|m| used.saturating_add(pending_tokens) >= m);
        let over_cost = budget.max_cost.is_some_and(|m| ledger.usage.cost + pending_cost >= m);
        if (over_tokens || over_cost) && !ledger.inflight.is_empty() { return Ok(None); }
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
        let remaining = budget.max_cost.map(|m| (m - ledger.usage.cost - pending_cost).max(0.0));
        let (cost, bounded) = match (remaining, cfg.price) {
            (Some(r), Some(p)) => (p.bound(bytes, max_output).min(r), true),
            (Some(r), None) => (r, false),
            (None, _) => (0.0, true),
        };
        let seq = self.policy.log.write("model_intent", json!({"model": cfg.model, "depth": depth,
            "req_sha": eventlog::sha_bytes(&serde_json::to_vec(req)?), "reserved_tokens": tokens, "reserved_cost": cost, "bounded": bounded}), true)?;
        ledger.pending.insert(seq, (tokens, cost, bounded));
        ledger.inflight.insert(seq);
        Ok(Some(seq))
    }

    /// A failed request: a definite rejection releases its reservation; a possibly billed one with a bounded reservation
    /// is charged at that bound; otherwise it stays reserved as uncertain billing. Returns whether the reservation closed.
    pub fn fail_model(&self, op: u64, cfg: &LlmCfg, error: &str, unbilled: bool, depth: usize) -> Result<bool> {
        let bounded = self.ledger.lock().unwrap().pending.get(&op).copied().filter(|p| p.2 && !unbilled);
        let result = self.policy.log.write("llm", json!({"of": op, "depth": depth, "model": cfg.model, "error": error, "billing_uncertain": !unbilled}), true)
            .and_then(|_| bounded.map(|(tokens, cost, _)| self.policy.log.write("billing", bound_billing(op, tokens, cost), true)).transpose());
        let mut l = self.ledger.lock().unwrap();
        l.inflight.remove(&op);
        let closed = result.is_ok() && (unbilled || bounded.is_some());
        if closed { l.pending.remove(&op); }
        if let (Ok(_), Some((tokens, cost, _))) = (&result, bounded) { l.usage.input += tokens; l.usage.cost += cost; l.usage.calls += 1; }
        drop(l);
        self.settled.notify_waiters();
        result.map(|_| closed)
    }

    pub fn settle_model(&self, op: u64, cfg: &LlmCfg, reply: &Reply, depth: usize) -> Result<()> {
        let written = (|| -> Result<()> {
            let blob = self.policy.log.blob(&serde_json::to_vec(reply)?)?;
            self.policy.log.write("llm", json!({"of": op, "model": cfg.model, "depth": depth, "reply": blob, "input": reply.input,
                "cache_read": reply.cache_read, "cache_write": reply.cache_write, "output": reply.output, "cost": reply.cost, "finish": reply.finish}), true)?;
            Ok(())
        })();
        let mut l = self.ledger.lock().unwrap();
        l.inflight.remove(&op);
        if written.is_ok() {
            l.pending.remove(&op);
            l.usage.input += reply.input; l.usage.cache_write += reply.cache_write; l.usage.cache_read += reply.cache_read;
            l.usage.output += reply.output; l.usage.cost += reply.cost; l.usage.calls += 1;
        }
        drop(l);
        self.settled.notify_waiters();
        written
    }

    pub fn usage(&self) -> Usage { self.ledger.lock().unwrap().usage.clone() }

    pub fn budget_status(&self) -> Value {
        let l = self.ledger.lock().unwrap();
        let b = self.manifest.as_ref().map(|m| &m.budget).unwrap_or(&self.policy.budget);
        json!({"cost": l.usage.cost, "max_cost": b.max_cost, "tokens": l.usage.input + l.usage.cache_write + l.usage.output,
            "max_tokens": b.max_tokens, "reserved_tokens": l.pending.values().map(|v| v.0).sum::<u64>(),
            "reserved_cost": l.pending.values().map(|v| v.1).sum::<f64>(), "max_wall": b.max_wall_secs,
            "deadline": self.manifest.as_ref().and_then(|m| m.deadline), "mode": "admission_threshold"})
    }
}
