//! One owner for admission, immutable run configuration, receipts, and recovery.
use crate::{
    eventlog::{self, EventLog, LedgerKind},
    llm::{LlmCfg, Reply},
    modules::Registry,
    policy::{Budget, BudgetExceeded, Class, Policy, Snapshot},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

// Approval policy and temporary grants are frozen under this format; shell outcomes carry their approval source; the
// journal chain is keyed; `expect` records carry the file's previous hash and the temporary name.
// Workflow checkpoints record consumed owner decisions by their journal sequence numbers.
pub const FORMAT: u64 = 11;
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

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
    #[serde(default)]
    pub workflow: Option<crate::workflow::WorkflowConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Branch {
    pub parent: std::path::PathBuf,
    pub durable: Value,
}

/// Host-owned lifecycle, independent of the customizable controller's private state.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunState {
    Running {
        controller: Value,
    },
    /// The open owner gate is stored here and nowhere else.
    AwaitingReview {
        controller: Value,
        report: serde_json::Map<String, Value>,
        gate: crate::workflow::ReviewRequest,
    },
    Finished {
        report: serde_json::Map<String, Value>,
    },
}

impl RunState {
    /// The controller's private state; a finished run has none.
    pub fn controller(&self) -> Option<&Value> {
        match self {
            Self::Running { controller } | Self::AwaitingReview { controller, .. } => {
                Some(controller)
            }
            Self::Finished { .. } => None,
        }
    }

    fn controller_mut(&mut self) -> Option<&mut Value> {
        match self {
            Self::Running { controller } | Self::AwaitingReview { controller, .. } => {
                Some(controller)
            }
            Self::Finished { .. } => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(flatten)]
    pub run: RunState,
    pub durable: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<crate::workflow::ExecutionFrame>,
}

/// A checkpoint as its body blob stores it. The controller state's `messages` array, the one part of that state the
/// host reads, is stored one blob per message. The message hashes, in order, are stored in chunks of `eventlog::CHUNK`
/// (every chunk but the last is full), one blob per chunk, and the body names the chunks. A message or chunk that is unchanged
/// since an earlier checkpoint is never written again, so a run's blobs grow with its conversation, not with the
/// conversation times the number of checkpoints. The rest of the controller state stays in the body as it is.
/// `Checkpoint::write` and `Checkpoint::read` are the only split and join.
#[derive(Serialize, Deserialize)]
pub(crate) struct CheckpointBody {
    #[serde(flatten)]
    checkpoint: Checkpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_chunks: Option<Vec<String>>,
}

impl CheckpointBody {
    /// Parse a body blob. A body without message chunks whose controller state holds a `messages` array was written
    /// before messages had blobs of their own and is refused, never read as something it is not.
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self> {
        let body: Self = serde_json::from_slice(bytes).context("invalid checkpoint body")?;
        let state = body.checkpoint.run.controller();
        match (&body.message_chunks, state.and_then(|c| c.get("messages"))) {
            (Some(_), _) if state.is_none_or(|c| !c.is_object()) => {
                bail!("checkpoint body names messages but has no controller state")
            }
            (Some(_), Some(_)) => {
                bail!("checkpoint body holds messages both inline and by reference")
            }
            (None, Some(Value::Array(_))) => bail!(
                "checkpoint body holds its messages inline, as builds before message blobs wrote it; start a new run"
            ),
            _ => Ok(body),
        }
    }

    /// The controller state without its `messages` array.
    pub(crate) fn controller(&self) -> Option<&Value> {
        self.checkpoint.run.controller()
    }

    /// Where message `index` is listed: the chunk blob's hash, whether it is the last chunk, and the message's position
    /// in it. `None` when the controller state has no `messages` array or it is shorter.
    pub(crate) fn message_chunk(&self, index: usize) -> Option<(&str, bool, usize)> {
        let chunks = self.message_chunks.as_ref()?;
        let chunk = chunks.get(index / eventlog::CHUNK)?;
        Some((
            chunk,
            index / eventlog::CHUNK + 1 == chunks.len(),
            index % eventlog::CHUNK,
        ))
    }
}

impl Checkpoint {
    /// Store the checkpoint and return its body blob's hash. Message and chunk blobs are written and synced before the
    /// body that names them, and the body before this returns, so a journal record written afterwards never names a
    /// missing blob; a crash in between leaves only blobs that nothing names.
    pub fn write(mut self, log: &EventLog) -> Result<String> {
        let messages = match self.run.controller_mut().and_then(Value::as_object_mut) {
            Some(state) if state.get("messages").is_some_and(Value::is_array) => {
                state.remove("messages")
            }
            _ => None,
        };
        let message_chunks = match messages {
            Some(Value::Array(messages)) => {
                let hashes = messages
                    .iter()
                    .map(|m| log.blob(&serde_json::to_vec(m)?))
                    .collect::<Result<Vec<_>>>()?;
                Some(
                    hashes
                        .chunks(eventlog::CHUNK)
                        .map(|chunk| log.blob(&serde_json::to_vec(chunk)?))
                        .collect::<Result<Vec<_>>>()?,
                )
            }
            _ => None,
        };
        log.blob(&serde_json::to_vec(&CheckpointBody {
            checkpoint: self,
            message_chunks,
        })?)
    }

    /// Read the checkpoint whose body blob is `hash`, join its messages back in order, and validate it. A missing or
    /// corrupt blob is an error that names it.
    pub fn read(log_dir: &Path, hash: &str) -> Result<Self> {
        let body = CheckpointBody::parse(&eventlog::read_blob(log_dir, hash)?)
            .with_context(|| format!("checkpoint {hash}"))?;
        let mut checkpoint = body.checkpoint;
        if let Some(chunks) = body.message_chunks {
            let mut messages = vec![];
            for (c, chunk) in chunks.iter().enumerate() {
                let hashes = read_chunk(log_dir, chunk, c + 1 == chunks.len())
                    .with_context(|| format!("checkpoint {hash}"))?;
                for (i, message) in hashes.iter().enumerate() {
                    messages.push(read_message(log_dir, message).with_context(|| {
                        format!("checkpoint {hash}: message {}", c * eventlog::CHUNK + i)
                    })?);
                }
            }
            let state = checkpoint
                .run
                .controller_mut()
                .and_then(Value::as_object_mut)
                .expect("parse requires an object state when messages are named");
            state.insert("messages".into(), Value::Array(messages));
        }
        checkpoint
            .validate()
            .with_context(|| format!("checkpoint {hash}"))?;
        Ok(checkpoint)
    }

    /// The latest root-lane checkpoint in a journal's `records`, with its record's sequence.
    pub fn latest(log_dir: &Path, records: &[Value]) -> Result<Option<(u64, Self)>> {
        let Some(record) = records
            .iter()
            .rev()
            .find(|r| r["kind"] == "checkpoint" && r["lane"] == "root")
        else {
            return Ok(None);
        };
        let seq = record["seq"].as_u64().context("missing record sequence")?;
        let hash = record["blob"]
            .as_str()
            .context("checkpoint record without a blob")?;
        Ok(Some((
            seq,
            Self::read(log_dir, hash).with_context(|| format!("checkpoint at record {seq}"))?,
        )))
    }

    fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.durable.is_object(), "invalid durable checkpoint");
        crate::values::validate(&self.durable)?;
        match &self.run {
            RunState::Running { controller } | RunState::AwaitingReview { controller, .. } => {
                anyhow::ensure!(controller.is_object(), "controller state must be an object");
                crate::values::validate(controller)?;
            }
            RunState::Finished { report } => {
                crate::controller::command(&json!({"kind":"finish", "report":report}))?;
            }
        }
        Ok(())
    }
}

fn read_chunk(log_dir: &Path, hash: &str, last: bool) -> Result<Vec<String>> {
    eventlog::parse_chunk(&eventlog::read_blob(log_dir, hash)?, last)
        .with_context(|| format!("message chunk {hash}"))
}

/// One message of a checkpoint's conversation, from its blob.
pub(crate) fn read_message(log_dir: &Path, hash: &str) -> Result<Value> {
    serde_json::from_slice(&eventlog::read_blob(log_dir, hash)?)
        .with_context(|| format!("message blob {hash} is not JSON"))
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self> {
        // Luau modules and the prompt are restored from the manifest's registry, and FORMAT pins the journal's shape, so a
        // rebuilt binary resumes the same workflow. A changed host primitive surfaces as an error, never a replayed effect.
        let manifest = Self::inspect(path)?;
        if manifest.binary_sha != binary_sha()? {
            eprintln!("hx: note: this binary differs from the one that started the run");
        }
        Ok(manifest)
    }

    pub fn inspect(path: &Path) -> Result<Self> {
        let records = eventlog::load(path)?;
        let record = records
            .iter()
            .find(|r| r["kind"] == "manifest")
            .context("this journal has no manifest record; start a new run")?;
        let bytes = eventlog::read_blob(
            path.parent().context("log has no parent")?,
            record["blob"].as_str().context("manifest blob missing")?,
        )?;
        let manifest: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            manifest.format == FORMAT,
            "unsupported run format {} (this binary resumes format {FORMAT}); start a new run",
            manifest.format
        );
        if let Some(config) = &manifest.workflow {
            config.validate()?;
        }
        Ok(manifest)
    }
}

/// What the journal says about spend and open effects. The live supervisor, resume, reconciliation, `hx log verify` and
/// `hx recover` all build it with `Books::apply`, so they cannot disagree about what is open.
#[derive(Default)]
pub struct Books {
    pub usage: Usage,
    pub model_admissions: u64,
    /// Model requests not yet settled by a receipt, a definite rejection or a billing decision: their reserved tokens, by
    /// the `model_intent` record's seq.
    pub reservations: BTreeMap<u64, u64>,
    /// Mutating intents without an outcome or resolution; `uncertain` is the subset whose effect may or may not have acted.
    pub open: BTreeMap<u64, Value>,
    pub uncertain: std::collections::BTreeSet<u64>,
}

impl Books {
    /// The books a journal's records add up to.
    pub fn of(records: &[Value]) -> Result<Self> {
        let mut books = Self::default();
        for r in records {
            let seq = r["seq"].as_u64().context("missing record sequence")?;
            books
                .apply(seq, r["kind"].as_str().unwrap_or_default(), r)
                .with_context(|| format!("record {seq}"))?;
        }
        Ok(books)
    }

    /// Check a record against the books before it changes them: its fields, and that it settles something open.
    pub fn validate(&self, kind: &str, r: &Value) -> Result<()> {
        let of = || {
            r["of"]
                .as_u64()
                .filter(|id| *id > 0)
                .with_context(|| format!("{kind} record missing operation ID"))
        };
        match kind {
            "outcome" | "resolution" | "uncertain" => {
                let id = of()?;
                anyhow::ensure!(
                    self.open.contains_key(&id),
                    "{kind} record for #{id}, which is not an open intent"
                );
            }
            "llm" | "billing" => {
                let id = of()?;
                anyhow::ensure!(
                    self.reservations.contains_key(&id),
                    "{kind} record for #{id}, which is not an open model request"
                );
            }
            _ => {}
        }
        match kind {
            "model_intent" => anyhow::ensure!(
                r["reserved_tokens"].as_u64().is_some(),
                "missing reservation tokens"
            ),
            "billing" => anyhow::ensure!(
                r["tokens"].as_u64().is_some(),
                "billing record missing tokens"
            ),
            // Every shell command that ran says who approved it.
            "outcome" if r["ret"]["t"] == "sh" => anyhow::ensure!(
                r["approval"]
                    .as_str()
                    .and_then(crate::policy::ApprovalSource::parse)
                    .is_some(),
                "sh outcome record missing its approval source"
            ),
            _ => {}
        }
        Ok(())
    }

    /// Validate `r`, then apply it; an invalid record changes nothing.
    pub fn apply(&mut self, seq: u64, kind: &str, r: &Value) -> Result<()> {
        self.validate(kind, r)?;
        let of = r["of"].as_u64().unwrap_or_default();
        if kind == "llm" && r["status"] == 429 {
            self.usage.rate_limits += 1;
        }
        match kind {
            "intent" if r["class"] != "read" => {
                self.open.insert(seq, r.clone());
            }
            "outcome" | "resolution" => {
                self.open.remove(&of);
                self.uncertain.remove(&of);
            }
            "uncertain" => {
                self.uncertain.insert(of);
            }
            "model_intent" => {
                self.model_admissions += 1;
                self.reservations
                    .insert(seq, r["reserved_tokens"].as_u64().unwrap_or_default());
            }
            "llm" if r.get("error").is_none() => {
                self.reservations.remove(&of);
                let u = &mut self.usage;
                u.input += r["input"].as_u64().unwrap_or(0);
                u.cache_write += r["cache_write"].as_u64().unwrap_or(0);
                u.cache_read += r["cache_read"].as_u64().unwrap_or(0);
                u.output += r["output"].as_u64().unwrap_or(0);
                u.calls += 1;
            }
            // A failure that may have been billed stays reserved until its `billing` record.
            "llm" if r["billing_uncertain"] == false => {
                self.reservations.remove(&of);
            }
            "billing" => {
                self.reservations.remove(&of);
                self.usage.input += r["tokens"].as_u64().unwrap_or_default();
                self.usage.calls += 1;
            }
            _ => {}
        }
        Ok(())
    }

    /// After a crash nothing is running: every open effect is uncertain.
    pub fn crashed(&mut self) {
        self.uncertain.extend(self.open.keys().copied());
    }

    pub fn reserved(&self) -> u64 {
        self.reservations.values().sum()
    }
}

/// Append one ledger record and apply it to `books`. Every ledger record goes through here,
/// so books kept live and books folded from the journal agree. A record the books reject is never written, and stops
/// the run.
fn append(
    log: &EventLog,
    books: &mut Books,
    kind: LedgerKind,
    fields: Value,
    sync: bool,
) -> Result<u64> {
    books
        .validate(kind.name(), &fields)
        .map_err(|e| log.fail(format!("invalid journal record: {e}")))?;
    let seq = log.append_ledger(kind, fields.clone(), sync)?;
    books
        .apply(seq, kind.name(), &fields)
        .map_err(|e| log.fail(format!("journal ledger update failed: {e}")))?;
    Ok(seq)
}

/// How an uncertain operation ended, as the owner (`hx recover`) or the evidence (`reconcile`) decides it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decision {
    Complete,
    Abandon,
}

impl Decision {
    pub fn name(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Abandon => "abandon",
        }
    }
}

impl std::str::FromStr for Decision {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "complete" => Ok(Self::Complete),
            "abandon" => Ok(Self::Abandon),
            _ => bail!("decision must be complete or abandon"),
        }
    }
}

/// Record `decision` for the open intent `id` and remove the temporary file a local write may have left (its name is
/// in the intent's `expect` record). `evidence` is hx's reason when hx decides; None means the owner decided.
pub fn resolve(
    log: &EventLog,
    books: &mut Books,
    records: &[Value],
    id: u64,
    decision: Decision,
    evidence: Option<&str>,
) -> Result<()> {
    let intent = books
        .open
        .get(&id)
        .with_context(|| format!("#{id} is not an open intent"))?;
    let mut fields = json!({"of": id, "decision": decision.name(), "tool": intent["tool"], "args_sha": intent["args_sha"]});
    if let Some(evidence) = evidence {
        fields["by"] = json!("hx");
        fields["evidence"] = json!(evidence);
    }
    append(log, books, LedgerKind::Resolution, fields, true)?;
    if let Some(expect) = records
        .iter()
        .find(|r| r["kind"] == "expect" && r["of"] == id)
    {
        remove_temp(expect)?;
    }
    Ok(())
}

/// The name a local write's temporary file has until it is renamed over its target: unique to the journal session and
/// intent, and journaled before the file exists, so recovery can remove it.
fn temp_name(session: &str, intent: u64) -> String {
    format!(".hx-tmp-{session}-{intent}")
}

fn remove_temp(expect: &Value) -> Result<()> {
    let (Some(path), Some(tmp)) = (expect["path"].as_str(), expect["tmp"].as_str()) else {
        return Ok(());
    };
    let path = Path::new(path)
        .parent()
        .context("expect record path has no parent")?
        .join(tmp);
    match crate::atomic_file::Target::open(&path, false).and_then(|t| t.remove()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("remove {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Settle what the evidence settles without the owner, after a crash:
/// - `write`/`edit` journal an `expect` record (the file's hash before and after) and replace the file atomically, so
///   no record means the file was never touched; otherwise the file's hash says whether it landed. A file that holds
///   neither, or cannot be read, is left for the owner.
/// - A model request whose response was lost is charged its reserved tokens.
///
/// Returns the books after settlement and how many records were written.
pub fn reconcile(log: &EventLog, records: &[Value]) -> Result<(Books, usize)> {
    let mut books = Books::of(records)?;
    let mut settled = 0;
    let open: Vec<(u64, Option<String>)> = books
        .open
        .iter()
        .map(|(&id, r)| (id, r["tool"].as_str().map(String::from)))
        .collect();
    for (id, tool) in open {
        if !matches!(tool.as_deref(), Some("write" | "edit")) {
            continue;
        }
        let (decision, evidence) = match records
            .iter()
            .find(|r| r["kind"] == "expect" && r["of"] == id)
        {
            None => (Decision::Abandon, "interrupted before the file was touched"),
            Some(e) => {
                let now = match std::fs::read(e["path"].as_str().unwrap_or_default()) {
                    Ok(bytes) => Some(eventlog::sha_bytes(&bytes)),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => continue,
                };
                if now.is_some() && now.as_deref() == e["sha256"].as_str() {
                    (Decision::Complete, "the file holds the intended content")
                } else if now.as_deref() == e["before"].as_str() {
                    (
                        Decision::Abandon,
                        if now.is_some() {
                            "the file holds its previous content"
                        } else {
                            "the file was never created"
                        },
                    )
                } else {
                    continue;
                }
            }
        };
        resolve(log, &mut books, records, id, decision, Some(evidence))?;
        settled += 1;
    }
    let reservations: Vec<(u64, u64)> = books
        .reservations
        .iter()
        .map(|(&id, &tokens)| (id, tokens))
        .collect();
    for (id, tokens) in reservations {
        append(
            log,
            &mut books,
            LedgerKind::Billing,
            bound_billing(id, tokens),
            true,
        )?;
        settled += 1;
    }
    Ok((books, settled))
}

/// A lost response's usage, charged at its reservation's bound.
fn bound_billing(of: u64, tokens: u64) -> Value {
    json!({"of": of, "tokens": tokens, "source": "hx", "evidence": "response lost; charged at the request's reserved upper bound"})
}

fn binary_sha() -> Result<String> {
    Ok(eventlog::sha_bytes(&std::fs::read(
        std::env::current_exe()?
    )?))
}

/// Every effect a principal can ask for, and the capability it needs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Read,
    Search,
    History,
    View,
    Write,
    Edit,
    Sh,
    Agent,
    McpList,
    McpCall,
    Promote,
    ProposeModule,
    Http,
    Llm,
}

impl Tool {
    /// The name journal records carry.
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Search => "search",
            Self::History => "history",
            Self::View => "view",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::Sh => "sh",
            Self::Agent => "agent",
            Self::McpList => "mcp_list",
            Self::McpCall => "mcp_call",
            Self::Promote => "promote",
            Self::ProposeModule => "propose_module",
            Self::Http => "http",
            Self::Llm => "llm",
        }
    }

    fn capability(self) -> Capability {
        match self {
            Self::Read | Self::Search | Self::History | Self::View => Capability::FsRead,
            Self::Write | Self::Edit | Self::Promote | Self::ProposeModule => Capability::FsWrite,
            Self::Sh => Capability::ProcSh,
            Self::Agent => Capability::LlmAgent,
            Self::McpList | Self::McpCall => Capability::McpCall,
            Self::Http => Capability::HttpRequest,
            Self::Llm => Capability::LlmChat,
        }
    }
}

/// Everything a principal can be granted: the capability each `Tool` needs. Package manifests and tools request them
/// as `{fs = ["read"], proc = ["sh"]}` (`*` for every scope of one); an unknown one is a load error, never a no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    FsRead,
    FsWrite,
    ProcSh,
    LlmAgent,
    LlmChat,
    McpCall,
    HttpRequest,
}

impl Capability {
    pub const ALL: [Self; 7] = [
        Self::FsRead,
        Self::FsWrite,
        Self::ProcSh,
        Self::LlmAgent,
        Self::LlmChat,
        Self::McpCall,
        Self::HttpRequest,
    ];

    /// The (capability, scope) pair manifests write.
    pub fn parts(self) -> (&'static str, &'static str) {
        match self {
            Self::FsRead => ("fs", "read"),
            Self::FsWrite => ("fs", "write"),
            Self::ProcSh => ("proc", "sh"),
            Self::LlmAgent => ("llm", "agent"),
            Self::LlmChat => ("llm", "chat"),
            Self::McpCall => ("mcp", "call"),
            Self::HttpRequest => ("http", "request"),
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (c, s) = self.parts();
        write!(f, "{c}.{s}")
    }
}

/// A set of capabilities, written as a map from capability to scopes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Capabilities(pub std::collections::BTreeSet<Capability>);

impl Capabilities {
    pub fn contains(&self, c: Capability) -> bool {
        self.0.contains(&c)
    }

    pub fn from_map(map: &BTreeMap<String, Vec<String>>) -> Result<Self> {
        let mut out = std::collections::BTreeSet::new();
        for (cap, scopes) in map {
            for scope in scopes {
                let found: Vec<_> = Capability::ALL
                    .into_iter()
                    .filter(|c| c.parts().0 == cap && (scope == "*" || c.parts().1 == scope))
                    .collect();
                anyhow::ensure!(
                    !found.is_empty(),
                    "unknown capability {cap}.{scope} (known: {})",
                    Capability::ALL.map(|c| c.to_string()).join(", ")
                );
                out.extend(found);
            }
        }
        Ok(Self(out))
    }

    pub fn to_map(&self) -> BTreeMap<&'static str, Vec<&'static str>> {
        let mut map: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for c in &self.0 {
            let (cap, scope) = c.parts();
            map.entry(cap).or_default().push(scope);
        }
        map
    }
}

impl<const N: usize> From<[Capability; N]> for Capabilities {
    fn from(caps: [Capability; N]) -> Self {
        Self(caps.into_iter().collect())
    }
}

impl Serialize for Capabilities {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_map().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Capabilities {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        Self::from_map(&BTreeMap::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone)]
pub enum Principal {
    Agent {
        read_only: bool,
    },
    Package {
        name: String,
        caps: Capabilities,
        active: Arc<AtomicBool>,
    },
}

/// What a read-only principal (a batch's subagent) keeps.
const READ_ONLY: [Capability; 3] = [
    Capability::FsRead,
    Capability::LlmAgent,
    Capability::LlmChat,
];

impl Principal {
    pub fn name(&self) -> &str {
        match self {
            Self::Agent { read_only: false } => "agent",
            Self::Agent { read_only: true } => "read-only agent",
            Self::Package { name, .. } => name,
        }
    }
    pub(crate) fn allows(&self, cap: Capability) -> bool {
        match self {
            Self::Agent { read_only } => !read_only || READ_ONLY.contains(&cap),
            Self::Package { caps, active, .. } => {
                active.load(Ordering::Acquire) && caps.contains(cap)
            }
        }
    }

    pub fn read_only(&self) -> Self {
        match self {
            Self::Agent { .. } => Self::Agent { read_only: true },
            Self::Package { name, active, .. } => {
                let caps =
                    Capabilities(READ_ONLY.into_iter().filter(|c| self.allows(*c)).collect());
                Self::Package {
                    name: name.clone(),
                    caps,
                    active: active.clone(),
                }
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    pub calls: u64,
    /// Requests the provider refused with HTTP 429.
    #[serde(default)]
    pub rate_limits: u64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
            .saturating_add(self.output)
    }
}

pub struct Supervisor {
    pub history: crate::history::History,
    pub policy: Arc<Policy>,
    pub registry: Arc<Registry>,
    pub manifest: Option<Manifest>,
    books: Mutex<Books>,
    settled: tokio::sync::Notify,
    pub mutations: Arc<tokio::sync::Semaphore>,
    pub durable: Arc<Mutex<Value>>,
    pub restored: Option<RunState>,
    pub recovery: Option<Value>,
    pub workflow: Mutex<Option<crate::workflow::ExecutionFrame>>,
    pub active_effects: std::sync::atomic::AtomicUsize,
}

/// What a new run is launched with. `Supervisor::start` freezes it into the run's manifest.
pub struct Launch {
    pub model: LlmCfg,
    pub task: String,
    pub max_turns: usize,
    pub packs: Vec<String>,
    pub budget: Budget,
    pub branch: Option<Branch>,
    pub workflow: Option<crate::workflow::WorkflowConfig>,
}

impl Supervisor {
    /// Start a new run: capture the registry, write the manifest, and return the supervisor that owns its journal.
    pub fn start(policy: Arc<Policy>, launch: Launch) -> Result<Arc<Self>> {
        let Launch {
            model,
            task,
            max_turns,
            packs,
            budget,
            branch,
            workflow,
        } = launch;
        if let Some(config) = &workflow {
            config.validate()?;
        }
        crate::vm::check_packs(&packs)?;
        let mut registry = Registry::capture(&policy.workspace, &policy.dirs)?;
        registry.materialize_skills(&policy.log)?;
        let manifest = Manifest {
            format: FORMAT,
            runtime: env!("CARGO_PKG_VERSION").into(),
            binary_sha: binary_sha()?,
            task,
            model,
            max_turns,
            packs,
            deadline: budget.max_wall_secs.map(|s| now() + s),
            budget,
            policy: policy.snapshot(),
            registry: registry.clone(),
            branch,
            workflow: workflow.clone(),
        };
        let durable = manifest
            .branch
            .as_ref()
            .map(|b| b.durable.clone())
            .unwrap_or_else(|| json!({}));
        anyhow::ensure!(durable.is_object(), "invalid branch durable data");
        crate::values::validate(&durable)?;
        let blob = policy.log.blob(&serde_json::to_vec(&manifest)?)?;
        policy
            .log
            .write("manifest", json!({"format": FORMAT, "blob": blob}), true)?;
        Ok(Arc::new(Self {
            policy,
            history: crate::history::History::default(),
            registry: Arc::new(registry),
            manifest: Some(manifest),
            books: Mutex::new(Books::default()),
            settled: tokio::sync::Notify::new(),
            mutations: Arc::new(tokio::sync::Semaphore::new(1)),
            durable: Arc::new(Mutex::new(durable)),
            restored: None,
            recovery: None,
            workflow: Mutex::new(workflow.map(|_| crate::workflow::ExecutionFrame::default())),
            active_effects: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    pub fn ephemeral(policy: Arc<Policy>, registry: Arc<Registry>) -> Arc<Self> {
        Arc::new(Self {
            policy,
            history: crate::history::History::default(),
            registry,
            manifest: None,
            books: Mutex::new(Books::default()),
            settled: tokio::sync::Notify::new(),
            mutations: Arc::new(tokio::sync::Semaphore::new(1)),
            durable: Arc::new(Mutex::new(json!({}))),
            restored: None,
            recovery: None,
            workflow: Mutex::new(None),
            active_effects: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn resume(policy: Arc<Policy>) -> Result<Arc<Self>> {
        let path = policy.log.path.as_path();
        let manifest = Manifest::load(path)?;
        let mut records = eventlog::load(path)?;
        let (mut books, settled) = reconcile(&policy.log, &records)?;
        if settled > 0 {
            records = eventlog::load(path)?;
        }
        books.crashed();
        let (frontier, checkpoint) = match Checkpoint::latest(policy.log.dir(), &records)? {
            Some((seq, checkpoint)) => (seq, Some(checkpoint)),
            None => (0, None),
        };
        let receipts: Vec<_> = records
            .iter()
            .filter(|r| {
                r["seq"].as_u64().unwrap_or(0) > frontier
                    && matches!(
                        r["kind"].as_str(),
                        Some(
                            "chunk_begin"
                                | "intent"
                                | "outcome"
                                | "call"
                                | "llm"
                                | "model_intent"
                                | "resolution"
                                | "uncertain"
                                | "billing"
                        )
                    )
            })
            .cloned()
            .collect();
        let mut durable = checkpoint
            .as_ref()
            .map(|c| c.durable.clone())
            .or_else(|| manifest.branch.as_ref().map(|b| b.durable.clone()))
            .unwrap_or_else(|| json!({}));
        if !durable.is_object() {
            bail!("invalid durable checkpoint");
        }
        crate::values::validate(&durable)?;
        let workflow = checkpoint
            .as_ref()
            .and_then(|c| c.workflow.clone())
            .or_else(|| {
                manifest
                    .workflow
                    .as_ref()
                    .map(|_| crate::workflow::ExecutionFrame::default())
            });
        let restored = checkpoint.map(|c| c.run);
        let recovery = Some(
            json!({"receipts": receipts, "log_directory":path.parent(), "uncertain_operations": books.open.values().cloned().collect::<Vec<_>>(),
            "notice": "Recovered into a fresh Luau VM. Ordinary globals and functions are gone; the conversation is restored. Old chunks were NOT executed. Inspect receipts before doing further work. Uncertain mutations require the owner's `hx recover` decision. To keep data across a future restart, save JSON-like values with `durable.set(key, value)` and read them with `durable.get(key)`; a completed chunk commits them."}),
        );
        policy.log.write(
            "resume",
            json!({"format": FORMAT, "checkpoint": frontier}),
            true,
        )?;
        // The registry is restored from bytes in the protected journal, never from the workspace.
        let mut registry = manifest.registry.clone();
        registry.materialize_skills(&policy.log)?;
        Ok(Arc::new(Self {
            policy,
            history: crate::history::History::restore(&records)?,
            registry: Arc::new(registry),
            manifest: Some(manifest),
            books: Mutex::new(books),
            settled: tokio::sync::Notify::new(),
            mutations: Arc::new(tokio::sync::Semaphore::new(1)),
            durable: Arc::new(Mutex::new(std::mem::take(&mut durable))),
            restored,
            recovery,
            workflow: Mutex::new(workflow),
            active_effects: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    pub fn check(&self) -> Result<()> {
        self.policy.log.check()?;
        if self
            .manifest
            .as_ref()
            .and_then(|m| m.deadline)
            .is_some_and(|d| now() >= d)
        {
            return Err(BudgetExceeded("absolute run deadline reached (downtime counts)").into());
        }
        Ok(())
    }

    pub fn remaining(&self) -> Option<std::time::Duration> {
        self.manifest
            .as_ref()
            .and_then(|m| m.deadline)
            .map(|d| std::time::Duration::from_secs_f64((d - now()).max(0.0)))
    }

    pub fn authorize(&self, principal: &Principal, tool: Tool, class: Class) -> Result<()> {
        self.check()?;
        if let Principal::Package { active, .. } = principal {
            anyhow::ensure!(
                active.load(Ordering::Acquire),
                "capability_denied: package initialization must be pure"
            );
        }
        let cap = tool.capability();
        anyhow::ensure!(
            principal.allows(cap),
            "capability_denied: {} lacks {cap} for {}",
            principal.name(),
            tool.name()
        );
        if class != Class::Read && !self.books.lock().unwrap().uncertain.is_empty() {
            bail!(
                "unknown_outcome: unresolved mutations block new mutations; inspect receipts and ask the owner to run `hx recover LOG OP --decision complete|abandon --yes`"
            );
        }
        Ok(())
    }

    pub fn checkpoint(&self, lane: &str, run: RunState, durable: &Value) -> Result<()> {
        self.checkpoint_archived(lane, run, durable, None)
    }

    pub fn checkpoint_archived(
        &self,
        lane: &str,
        run: RunState,
        durable: &Value,
        archive: Option<crate::history::Archive>,
    ) -> Result<()> {
        self.policy.log.check()?;
        let workflow = if lane == "root" {
            self.workflow.lock().unwrap().clone()
        } else {
            None
        };
        let blob = Checkpoint {
            run,
            durable: durable.clone(),
            workflow,
        }
        .write(&self.policy.log)?;
        let mut fields = json!({"lane": lane, "blob": blob});
        if let Some(archive) = &archive {
            fields["archive"] = serde_json::to_value(archive)?;
        }
        let seq = self.policy.log.write("checkpoint", fields, true)?;
        if lane == "root" {
            self.history
                .committed(crate::history::CheckpointRef { seq, blob }, archive)?;
        }
        Ok(())
    }

    /// Journal the intent of an effect. `stage` holds the workflow facts its receipt carries, observed by the caller
    /// off the async threads.
    #[allow(clippy::too_many_arguments)]
    pub fn intent(
        &self,
        tool: Tool,
        class: Class,
        args: &Value,
        args_sha: &str,
        depth: usize,
        principal: &Principal,
        stage: Option<&crate::workflow_runtime::StageFacts>,
    ) -> Result<u64> {
        let mut fields = json!({"tool":tool.name(), "class":class.name(), "args":args,
            "args_sha":args_sha, "depth":depth, "principal":principal.name(),
            "summary":args.to_string().chars().take(300).collect::<String>()});
        if let Some(stage) = stage {
            stage.record(&mut fields);
        }
        self.record(LedgerKind::Intent, fields, true)
    }

    /// Replace the file `target` (at `path`) with `bytes` for the write intent `intent`. First journal an `expect`
    /// record: the file's hash now (`before`, null when absent), the hash it is about to have, and the temporary file's
    /// name. With atomic replacement the file then holds either `before` or `sha256`, so recovery reads the outcome off
    /// the disk, and removes the temporary file if the rename never happened.
    pub fn replace_file(
        &self,
        intent: Option<u64>,
        path: &Path,
        target: &crate::atomic_file::Target,
        bytes: &[u8],
    ) -> Result<()> {
        let Some(intent) = intent else {
            return target.replace(bytes);
        };
        let before = target
            .sha256()
            .with_context(|| format!("read {}", path.display()))?;
        let tmp = temp_name(&self.policy.log.session, intent);
        self.policy.log.write("expect", json!({"of": intent, "path": path, "before": before, "sha256": eventlog::sha_bytes(bytes), "tmp": tmp}), true)?;
        target.replace_through(&tmp, bytes)
    }

    pub fn uncertain(&self, id: u64, tool: Tool, args: &Value, message: &str) -> Result<()> {
        self.record(
            LedgerKind::Uncertain,
            json!({"of":id, "tool":tool.name(), "args":args, "error":message}),
            true,
        )?;
        Ok(())
    }

    pub fn uncertain_operations(&self) -> Vec<u64> {
        self.books
            .lock()
            .unwrap()
            .uncertain
            .iter()
            .copied()
            .collect()
    }

    /// Append a journal record and apply it to the books under one lock, so the live books are the journal's fold.
    pub fn record(&self, kind: LedgerKind, fields: Value, sync: bool) -> Result<u64> {
        self.record_in(&mut self.books.lock().unwrap(), kind, fields, sync)
    }

    fn record_in(
        &self,
        books: &mut Books,
        kind: LedgerKind,
        fields: Value,
        sync: bool,
    ) -> Result<u64> {
        append(&self.policy.log, books, kind, fields, sync)
    }

    /// Admit one model request. A request that only the reservations of running requests keep out waits for them to
    /// settle instead of failing. (A reservation outlives its request only when its receipt could not be journaled,
    /// which stops admission by itself, or across a crash, after which resume bills it before admitting anything.)
    pub async fn reserve_model(&self, cfg: &LlmCfg, req: &Value, depth: usize) -> Result<u64> {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if let Some(seq) = self.try_reserve_model(cfg, req, depth)? {
                return Ok(seq);
            }
            match self.remaining() {
                Some(remaining) => {
                    if tokio::time::timeout(remaining, settled).await.is_err() {
                        self.check()?;
                    }
                }
                None => settled.await,
            }
        }
    }

    fn try_reserve_model(&self, cfg: &LlmCfg, req: &Value, depth: usize) -> Result<Option<u64>> {
        self.check()?;
        let mut books = self.books.lock().unwrap();
        if let Some(manifest) = self.manifest.as_ref().filter(|m| m.workflow.is_some()) {
            anyhow::ensure!(
                books.model_admissions < manifest.max_turns as u64,
                BudgetExceeded(
                    "run-wide max_turns admission threshold reached (including descendants and retries)"
                )
            );
        }
        let budget = self
            .manifest
            .as_ref()
            .map(|m| &m.budget)
            .unwrap_or(&self.policy.budget);
        let used = books.usage.total_tokens();
        let pending_tokens = books.reserved();
        // This is a conservative admission allowance, not a tokenizer or provider billing guarantee.
        let bytes = serde_json::to_vec(req)?.len() as u64;
        let max_output = req["max_tokens"]
            .as_u64()
            .unwrap_or(cfg.max_output)
            .min(cfg.max_output);
        let tokens = bytes.saturating_add(max_output);
        let over_tokens = budget
            .max_tokens
            .is_some_and(|m| used.saturating_add(pending_tokens) >= m);
        if over_tokens && !books.reservations.is_empty() {
            return Ok(None);
        }
        if over_tokens {
            let exceeded = BudgetExceeded("token admission threshold reached");
            self.policy.log.write(
                "budget",
                json!({"reason":exceeded.to_string(),"depth":depth}),
                true,
            )?;
            return Err(exceeded.into());
        }
        let seq = self.record_in(
            &mut books,
            LedgerKind::ModelIntent,
            json!({"model": cfg.model, "depth": depth,
            "req_sha": eventlog::sha_bytes(&serde_json::to_vec(req)?), "reserved_tokens": tokens}),
            true,
        )?;
        Ok(Some(seq))
    }

    /// A failed request: a definite rejection releases its reservation; a possibly billed one is charged its reserved
    /// tokens. Returns whether the reservation closed.
    pub fn fail_model(
        &self,
        op: u64,
        cfg: &LlmCfg,
        error: &str,
        status: Option<u16>,
        unbilled: bool,
        depth: usize,
    ) -> Result<bool> {
        let mut books = self.books.lock().unwrap();
        let bound = books.reservations.get(&op).filter(|_| !unbilled).copied();
        let result = self.record_in(&mut books, LedgerKind::Llm, json!({"of": op, "depth": depth, "model": cfg.model, "error": error, "status": status, "billing_uncertain": !unbilled}), true)
            .and_then(|_| bound.map(|tokens| self.record_in(&mut books, LedgerKind::Billing, bound_billing(op, tokens), true)).transpose());
        drop(books);
        self.settled.notify_waiters();
        result.map(|_| unbilled || bound.is_some())
    }

    pub fn settle_model(&self, op: u64, cfg: &LlmCfg, reply: &Reply, depth: usize) -> Result<()> {
        let mut books = self.books.lock().unwrap();
        let written = self.policy.log.blob(&serde_json::to_vec(reply)?).and_then(|blob| self.record_in(&mut books, LedgerKind::Llm, json!({"of": op, "model": cfg.model,
            "depth": depth, "reply": blob, "input": reply.input, "cache_read": reply.cache_read, "cache_write": reply.cache_write,
            "output": reply.output, "finish": reply.finish}), true));
        drop(books);
        self.settled.notify_waiters();
        written.map(|_| ())
    }

    pub fn usage(&self) -> Usage {
        self.books.lock().unwrap().usage.clone()
    }

    pub fn budget_status(&self) -> Value {
        let books = self.books.lock().unwrap();
        let reserved_tokens = books.reserved();
        let b = self
            .manifest
            .as_ref()
            .map(|m| &m.budget)
            .unwrap_or(&self.policy.budget);
        json!({"tokens": books.usage.total_tokens(), "max_tokens": b.max_tokens, "reserved_tokens": reserved_tokens,
            "max_wall": b.max_wall_secs,
            "deadline": self.manifest.as_ref().and_then(|m| m.deadline), "mode": "admission_threshold"})
    }
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    fn model() -> LlmCfg {
        serde_json::from_value(json!({"model":"mock","max_output":100,
            "base_url":null,"key_env":null,"reasoning":null,"context_window":null,"cache_key":null,"provider_only":[]})).unwrap()
    }

    fn launch(budget: Budget) -> Launch {
        Launch {
            model: model(),
            task: "test".into(),
            max_turns: 1,
            packs: vec![],
            budget,
            branch: None,
            workflow: None,
        }
    }

    #[tokio::test]
    async fn lost_responses_are_charged_their_reserved_tokens() {
        for (tag, limit) in [("capped", Some(102)), ("uncapped", None)] {
            let dir = std::env::temp_dir().join(format!("hx-billing-{tag}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let dirs = crate::dirs::TestDirs::new();
            let policy = Arc::new(Policy::new(&dirs, &dir, false, None, None, None).unwrap());
            let cfg = model();
            let supervisor = Supervisor::start(
                policy,
                launch(Budget {
                    max_tokens: limit,
                    ..Budget::default()
                }),
            )
            .unwrap();
            let op = supervisor.reserve_model(&cfg, &json!({}), 0).await.unwrap();
            assert_eq!(supervisor.books.lock().unwrap().reservations[&op], 102);
            assert!(
                supervisor
                    .fail_model(op, &cfg, "lost response", None, false, 0)
                    .unwrap()
            );
            let replay = Books::of(&eventlog::load(&supervisor.policy.log.path).unwrap()).unwrap();
            assert_eq!(supervisor.usage().total_tokens(), 102);
            assert_eq!(replay.usage.total_tokens(), 102);
            assert!(replay.reservations.is_empty());
            if limit.is_some() {
                assert!(supervisor.reserve_model(&cfg, &json!({}), 0).await.is_err());
            }
            drop(supervisor);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[tokio::test]
    async fn queued_model_admission_expires_without_a_settlement() {
        let dir =
            std::env::temp_dir().join(format!("hx-admission-deadline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = crate::dirs::TestDirs::new();
        let policy = Arc::new(Policy::new(&dirs, &dir, false, None, None, None).unwrap());
        let cfg = model();
        let supervisor = Supervisor::start(
            policy,
            launch(Budget {
                max_tokens: Some(102),
                max_wall_secs: Some(0.5),
            }),
        )
        .unwrap();
        let first = supervisor.reserve_model(&cfg, &json!({}), 0).await.unwrap();
        let queued = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            supervisor.reserve_model(&cfg, &json!({}), 1),
        )
        .await
        .expect("queued admission must not wait for an unsettled request after its deadline");
        assert!(
            queued
                .unwrap_err()
                .to_string()
                .contains("absolute run deadline")
        );
        assert!(
            supervisor
                .books
                .lock()
                .unwrap()
                .reservations
                .contains_key(&first)
        );
        drop(supervisor);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn token_reservations_are_validated() {
        let mut books = Books::default();
        for id in [1, 2] {
            books
                .apply(id, "model_intent", &json!({"reserved_tokens":10}))
                .unwrap();
        }
        assert_eq!(books.reserved(), 20);
        for invalid in [json!(null), json!(-1), json!(1.5), json!("10")] {
            assert!(
                books
                    .apply(3, "model_intent", &json!({"reserved_tokens":invalid}))
                    .is_err()
            );
        }
        assert_eq!(books.reservations.len(), 2);
    }

    #[test]
    fn malformed_accounting_does_not_partially_update_books() {
        let mut books = Books::default();
        books
            .apply(1, "model_intent", &json!({"reserved_tokens":10}))
            .unwrap();
        assert!(
            books
                .apply(2, "billing", &json!({"of": 1, "tokens": -8}))
                .is_err()
        );
        assert_eq!(books.reserved(), 10);
        assert_eq!(books.usage.input, 0);
        assert_eq!(books.usage.calls, 0);
    }

    #[test]
    fn receipts_must_settle_something_open() {
        let mut books = Books::default();
        books
            .apply(
                1,
                "intent",
                &json!({"class": "local_write", "tool": "write"}),
            )
            .unwrap();
        books
            .apply(2, "model_intent", &json!({"reserved_tokens": 10}))
            .unwrap();
        for (kind, of) in [
            ("outcome", 2),
            ("resolution", 7),
            ("uncertain", 2),
            ("llm", 1),
            ("billing", 1),
        ] {
            assert!(
                books
                    .apply(3, kind, &json!({"of": of, "tokens": 1}))
                    .is_err(),
                "{kind} of #{of}"
            );
        }
        books
            .apply(3, "outcome", &json!({"of": 1, "ret": {"t": "num"}}))
            .unwrap();
        assert!(
            books.apply(4, "resolution", &json!({"of": 1})).is_err(),
            "an intent settles once"
        );
    }

    fn journal(tag: &str) -> (Arc<Supervisor>, std::path::PathBuf, crate::dirs::TestDirs) {
        let dir = std::env::temp_dir().join(format!("hx-ledger-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dirs = crate::dirs::TestDirs::new();
        let policy = Arc::new(Policy::new(&dirs, &dir, false, None, None, None).unwrap());
        (
            Supervisor::start(policy, launch(Budget::default())).unwrap(),
            std::fs::canonicalize(dir).unwrap(),
            dirs,
        )
    }

    #[test]
    fn invalid_accounting_is_rejected_before_append_and_stops_admission() {
        let (supervisor, dir, _dirs) = journal("invalid-append");
        let path = supervisor.policy.log.path.clone();
        let before = std::fs::read(&path).unwrap();
        assert!(
            supervisor
                .record(LedgerKind::Billing, json!({"of": 1}), true)
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(supervisor.policy.log.check().is_err());
        assert!(
            supervisor
                .policy
                .log
                .write("note", json!({}), false)
                .is_err()
        );
        drop(supervisor);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A write intent whose file was replaced (or not) before a crash: `before` and `after` are the content the
    /// `expect` record names, `on_disk` what the file holds at recovery.
    fn settle(tag: &str, before: Option<&str>, on_disk: Option<&str>) -> (Option<String>, bool) {
        let (supervisor, dir, _dirs) = journal(tag);
        let file = dir.join("note.txt");
        if let Some(text) = before {
            std::fs::write(&file, text).unwrap();
        }
        let id = supervisor
            .intent(
                Tool::Write,
                Class::LocalWrite,
                &json!({"path": "note.txt"}),
                "sha",
                0,
                &Principal::Agent { read_only: false },
                None,
            )
            .unwrap();
        let target = crate::atomic_file::Target::open(&file, false).unwrap();
        // Journal the expectation as replace_file does, then stop where a crash before the rename would.
        let tmp = temp_name(&supervisor.policy.log.session, id);
        supervisor
            .policy
            .log
            .write(
                "expect",
                json!({"of": id, "path": file, "before": target.sha256().unwrap(),
            "sha256": eventlog::sha_bytes(b"after"), "tmp": tmp}),
                true,
            )
            .unwrap();
        std::fs::write(dir.join(&tmp), "after").unwrap();
        match on_disk {
            Some(text) => std::fs::write(&file, text).unwrap(),
            None => {
                let _ = std::fs::remove_file(&file);
            }
        }
        // Reconcile through the run's own journal: a reopen could race a child forked by a concurrent test, which
        // holds the writer lock until it execs.
        let path = supervisor.policy.log.path.clone();
        let log = supervisor.policy.log.clone();
        drop(supervisor);
        let (books, _) = reconcile(&log, &eventlog::load(&path).unwrap()).unwrap();
        let decision = eventlog::load(&path)
            .unwrap()
            .iter()
            .find(|r| r["kind"] == "resolution")
            .map(|r| r["decision"].as_str().unwrap().to_string());
        assert_eq!(books.open.is_empty(), decision.is_some());
        let tmp_left = dir.join(&tmp).exists();
        drop(log);
        let _ = std::fs::remove_dir_all(dir);
        (decision, tmp_left)
    }

    #[test]
    fn interrupted_writes_settle_only_from_states_the_journal_names() {
        assert_eq!(
            settle("old", Some("old"), Some("old")),
            (Some("abandon".into()), false)
        );
        assert_eq!(
            settle("new", Some("old"), Some("after")),
            (Some("complete".into()), false)
        );
        assert_eq!(
            settle("absent", None, None),
            (Some("abandon".into()), false)
        );
        // Anything else changed the file after hx journaled the write: only the owner can say what happened.
        assert_eq!(settle("other", Some("old"), Some("other")), (None, true));
        assert_eq!(settle("deleted", Some("old"), None), (None, true));
    }

    #[test]
    fn owner_reconciliation_can_bill_a_revoked_run() {
        let (supervisor, dir, _dirs) = journal("revoked-billing");
        let cfg = model();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let op = rt
            .block_on(supervisor.reserve_model(&cfg, &json!({}), 0))
            .unwrap();
        let path = supervisor.policy.log.path.clone();
        drop(supervisor);
        std::fs::write(path.with_extension("revoked"), "").unwrap();
        assert!(
            reconcile(
                &EventLog::reopen(&path).unwrap(),
                &eventlog::load(&path).unwrap()
            )
            .is_err()
        );
        let (books, settled) = reconcile(
            &EventLog::reconcile(&path).unwrap(),
            &eventlog::load(&path).unwrap(),
        )
        .unwrap();
        assert_eq!(
            (settled, books.reservations.len(), books.usage.input),
            (1, 0, 102)
        );
        assert!(
            eventlog::load(&path)
                .unwrap()
                .iter()
                .any(|r| r["kind"] == "billing" && r["of"] == op)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoints_require_explicit_lifecycle_and_valid_data() {
        let (supervisor, dir, _dirs) = journal("checkpoint-validation");
        let log = &supervisor.policy.log;
        let read = |body: &[u8]| Checkpoint::read(log.dir(), &log.blob(body).unwrap());
        let checkpoint =
            read(br#"{"status":"running","controller":{"custom":3},"durable":{"saved":7}}"#)
                .unwrap();
        assert_eq!(checkpoint.durable["saved"], 7);
        assert!(
            matches!(checkpoint.run, RunState::Running { controller } if controller["custom"] == 3)
        );
        let message = log.blob(br#"{"role":"user","content":"task"}"#).unwrap();
        let chunk = log.blob(format!(r#"["{message}"]"#).as_bytes()).unwrap();
        for invalid in [
            br#"{"controller":{},"durable":{}}"#.to_vec(),
            br#"{"status":"finished","report":{"finished":true},"durable":{}}"#.to_vec(),
            br#"{"status":"running","controller":[],"durable":{}}"#.to_vec(),
            br#"{"status":"running","controller":{},"durable":[]}"#.to_vec(),
            // the shape builds before message blobs wrote: never read as a conversation of references
            br#"{"status":"running","controller":{"messages":[]},"durable":{}}"#.to_vec(),
            format!(r#"{{"status":"running","controller":{{"messages":[]}},"durable":{{}},"message_chunks":["{chunk}"]}}"#).into_bytes(),
            format!(r#"{{"status":"finished","report":{{"finished":true,"final_answer":""}},"durable":{{}},"message_chunks":["{chunk}"]}}"#).into_bytes(),
        ] {
            assert!(read(&invalid).is_err(), "{}", String::from_utf8_lossy(&invalid));
        }
        drop(supervisor);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn running(messages: Value) -> Checkpoint {
        Checkpoint {
            run: RunState::Running {
                controller: json!({"messages": messages, "turns": 3, "custom": {"nested": [1, 2.5, null]}}),
            },
            durable: json!({"saved": "é"}),
            workflow: None,
        }
    }

    fn total_blob_bytes(dir: &Path) -> u64 {
        std::fs::read_dir(dir.join("blobs"))
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum()
    }

    #[test]
    fn checkpoints_round_trip_with_one_blob_per_message() {
        let (supervisor, dir, _dirs) = journal("checkpoint-round-trip");
        let log = &supervisor.policy.log;
        let image = "iVBORw0KGgo".repeat(2000);
        let messages = json!([
            {"role": "user", "content": "naïve 🙂 \u{0} \"quoted\" \\ end"},
            {"role": "assistant", "content": null, "calls": [{"id": "c1", "name": "luau", "input": {"code": "print('日本')"}}]},
            {"role": "tool", "content": [{"type": "text", "text": "seen"}, {"type": "image", "data": image}], "images": [{"path": "a.png", "width": 2, "height": 1}]},
            {"role": "user", "content": "naïve 🙂 \u{0} \"quoted\" \\ end"},
        ]);
        let mut states = vec![running(messages.clone())];
        // A state whose `messages` is not an array is the controller's own business and stays in the body as it is.
        states.push(Checkpoint {
            run: RunState::Running {
                controller: json!({"messages": {"not": "a list"}}),
            },
            durable: json!({}),
            workflow: None,
        });
        states.push(Checkpoint {
            run: RunState::Running {
                controller: json!({"other": 1}),
            },
            durable: json!({}),
            workflow: None,
        });
        states.push(Checkpoint {
            run: RunState::Finished {
                report: json!({"finished": true, "final_answer": "done"})
                    .as_object()
                    .unwrap()
                    .clone(),
            },
            durable: json!({}),
            workflow: None,
        });
        for state in states {
            let expected = serde_json::to_value(&state).unwrap();
            let hash = state.write(log).unwrap();
            assert_eq!(
                serde_json::to_value(Checkpoint::read(log.dir(), &hash).unwrap()).unwrap(),
                expected
            );
        }
        let body = CheckpointBody::parse(
            &eventlog::read_blob(
                log.dir(),
                &Checkpoint::write(running(messages.clone()), log).unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        let chunk = eventlog::parse_chunk(
            &eventlog::read_blob(log.dir(), body.message_chunk(0).unwrap().0).unwrap(),
            true,
        )
        .unwrap();
        // Identical messages at different positions share one blob; the chunk keeps both positions.
        assert_eq!(chunk.len(), 4);
        assert_eq!(chunk[0], chunk[3]);
        assert_eq!(
            body.message_chunk(3).map(|(_, last, i)| (last, i)),
            Some((true, 3))
        );
        assert!(body.message_chunk(eventlog::CHUNK).is_none());
        assert!(body.controller().unwrap().get("messages").is_none());
        assert_eq!(body.controller().unwrap()["turns"], 3);
        drop(supervisor);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn checkpoint_blobs_grow_with_the_conversation_not_with_the_number_of_checkpoints() {
        let (supervisor, dir, _dirs) = journal("checkpoint-growth");
        let log = &supervisor.policy.log;
        let before = total_blob_bytes(log.dir());
        let mut messages = vec![];
        let mut last = 0;
        for turn in 0..100 {
            messages.push(json!({"role": "assistant", "content": format!("turn {turn}"), "calls": [{"id": format!("c{turn}"), "name": "luau", "input": {"code": "print(1)"}}]}));
            messages
                .push(json!({"role": "tool", "content": format!("{turn}:{}", "x".repeat(4000))}));
            for _ in 0..3 {
                let state = running(json!(messages));
                last = serde_json::to_vec(&state).unwrap().len();
                supervisor
                    .checkpoint("root", state.run, &state.durable)
                    .unwrap();
            }
        }
        let written = (total_blob_bytes(log.dir()) - before) as usize;
        // 300 checkpoints of the whole state would write about 150 times the final state; message blobs, plus a body
        // and a last chunk of hashes per checkpoint, stay under twice it.
        assert!(
            written < 2 * last,
            "{written} bytes of blobs for a final state of {last}"
        );
        let (_, restored) = Checkpoint::latest(log.dir(), &eventlog::load(&log.path).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.run.controller().unwrap()["messages"],
            json!(messages)
        );
        drop(supervisor);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_crash_after_the_blobs_but_before_the_record_resumes_the_previous_checkpoint() {
        let (supervisor, dir, _dirs) = journal("checkpoint-crash");
        let log = supervisor.policy.log.clone();
        let first = running(json!([{"role": "user", "content": "task"}]));
        supervisor
            .checkpoint("root", first.run.clone(), &first.durable)
            .unwrap();
        // The next checkpoint's message, chunk and body blobs reach the disk; the process dies before its record.
        Checkpoint::write(running(json!([{"role": "user", "content": "task"}, {"role": "assistant", "content": "lost"}])), &log).unwrap();
        let path = log.path.clone();
        assert!(
            eventlog::verify(&path).unwrap().blob_errors.is_empty(),
            "unreferenced blobs are not damage"
        );
        drop(supervisor);
        drop(log);
        let policy = Arc::new(Policy::new(&_dirs, &dir, false, None, Some(&path), None).unwrap());
        let resumed = Supervisor::resume(policy).unwrap();
        assert!(
            matches!(&resumed.restored, Some(RunState::Running { controller }) if controller["messages"] == json!([{"role": "user", "content": "task"}]))
        );
        drop(resumed);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_missing_or_corrupt_message_blob_fails_resume_and_verify_naming_the_blob() {
        for corrupt in [false, true] {
            let (supervisor, dir, _dirs) = journal(if corrupt {
                "checkpoint-corrupt"
            } else {
                "checkpoint-missing"
            });
            let log = supervisor.policy.log.clone();
            let state = running(
                json!([{"role": "user", "content": "task"}, {"role": "assistant", "content": "unique reply"}]),
            );
            supervisor
                .checkpoint("root", state.run, &state.durable)
                .unwrap();
            let path = log.path.clone();
            let hash = eventlog::sha_bytes(
                &serde_json::to_vec(&json!({"role": "assistant", "content": "unique reply"}))
                    .unwrap(),
            );
            let blob = log.dir().join("blobs").join(&hash);
            assert!(eventlog::verify(&path).unwrap().blob_errors.is_empty());
            if corrupt {
                std::fs::write(&blob, "changed").unwrap();
            } else {
                std::fs::remove_file(&blob).unwrap();
            }
            let errors = eventlog::verify(&path).unwrap().blob_errors;
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(
                errors[0].contains(&hash) && errors[0].contains("message 1"),
                "{errors:?}"
            );
            drop(supervisor);
            drop(log);
            let policy =
                Arc::new(Policy::new(&_dirs, &dir, false, None, Some(&path), None).unwrap());
            let error = format!(
                "{:#}",
                Supervisor::resume(policy).err().expect("resume must fail")
            );
            assert!(error.contains(&hash), "{error}");
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
