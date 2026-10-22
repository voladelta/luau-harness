//! Data-only workflow protocol and bounded host validation. Stage routing belongs to Luau.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    pub name: String,
    pub version: u32,
    pub mode: String,
    pub max_repairs: u8,
}
impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            name: "rpi".into(),
            version: 1,
            mode: "auto".into(),
            max_repairs: 3,
        }
    }
}
impl WorkflowConfig {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.name == "rpi" && self.version == 1,
            "unsupported workflow configuration"
        );
        ensure!(
            matches!(self.mode.as_str(), "auto" | "manual"),
            "workflow mode must be auto or manual"
        );
        ensure!(self.max_repairs <= 3, "workflow repair limit exceeds three");
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StageAccess {
    Inspect,
    Workspace,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageSpec {
    pub id: String,
    pub name: String,
    pub access: StageAccess,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub instructions: String,
    #[serde(default)]
    pub verify: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_name: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRequest {
    pub gate_id: String,
    pub kind: String,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub questions: Vec<String>,
    #[serde(default)]
    pub check_ids: Vec<String>,
    pub decisions: Vec<String>,
    pub continuation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectObservation>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BoundaryNext {
    Stage { stage: StageSpec },
    Review { request: ReviewRequest },
    Finish { report: Map<String, Value> },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    pub id: String,
    pub submission: Option<String>,
    pub next: BoundaryNext,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Submission {
    pub id: String,
    pub stage_id: String,
    pub payload: Value,
    pub document_blob: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: String,
    pub name: String,
    pub document_blob: String,
    pub summary: String,
    pub submission: Value,
    pub stage_id: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OwnerInputRef {
    pub id: String,
    pub gate_id: String,
    pub decision: String,
    pub content_blob: Option<String>,
    #[serde(default)]
    pub check_ids: Vec<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
}
/// The stage whose effects are admitted, and its prepared submission. A submission cannot exist without its stage.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActiveStage {
    pub spec: StageSpec,
    #[serde(default)]
    pub submission: Option<Submission>,
}
/// No stage is active before the first boundary or while an owner gate is open: the open gate lives only in
/// `RunState::AwaitingReview`, so a paused run admits no stage effects by construction.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionFrame {
    pub stage: Option<ActiveStage>,
    pub artifacts: Vec<ArtifactRef>,
    pub owner_inputs: Vec<OwnerInputRef>,
    pub pending_event: Option<Value>,
    pub consumed_decisions: Vec<String>,
    pub stage_epoch: u64,
    pub check_plan: Vec<PlannedCheck>,
    pub evidence: Vec<CheckEvidence>,
    pub boundaries: BTreeMap<String, String>,
    pub observation: Option<ProjectObservation>,
    pub root_turns: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlannedCheck {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout: Option<f64>,
    #[serde(default)]
    pub requirements: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckEvidence {
    pub check_id: String,
    pub operation_id: u64,
    pub stage_id: String,
    pub project_digest: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectObservation {
    pub digest: String,
    pub inventory: BTreeMap<String, String>,
    pub root: PathBuf,
    #[serde(default)]
    pub extra_inputs: Vec<String>,
}

fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 200
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c)),
        "invalid workflow identifier"
    );
    Ok(())
}
fn list<'a>(value: &'a Value, name: &str) -> Result<Vec<&'a Value>> {
    match value.get(name) {
        None => Ok(vec![]),
        Some(Value::Array(v)) => Ok(v.iter().collect()),
        Some(Value::Object(v)) if v.is_empty() => Ok(vec![]),
        _ => anyhow::bail!("{name} must be an array"),
    }
}
pub fn validate_submission(value: &Value) -> Result<()> {
    ensure!(value.is_object(), "stage submission must be an object");
    ensure!(
        serde_json::to_vec(value)?.len() <= 2 * 1024 * 1024,
        "stage submission exceeds 2 MiB"
    );
    fn depth(value: &Value, remaining: usize) -> bool {
        if remaining == 0 {
            return false;
        }
        match value {
            Value::Array(a) => a.iter().all(|v| depth(v, remaining - 1)),
            Value::Object(o) => o.values().all(|v| depth(v, remaining - 1)),
            _ => true,
        }
    }
    ensure!(depth(value, 32), "stage submission exceeds depth limit");
    ensure!(
        matches!(
            value["outcome"].as_str(),
            Some("complete" | "needs_input" | "blocked")
        ),
        "invalid stage outcome"
    );
    ensure!(
        value["document"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty()),
        "stage document must be nonempty text"
    );
    ensure!(
        value["summary"].as_str().is_some_and(|s| s.len() <= 16_384),
        "stage summary must be text up to 16 KiB"
    );
    for q in list(value, "questions")? {
        ensure!(q.is_string(), "stage questions must be strings");
    }
    for evidence in list(value, "evidence")? {
        identifier(
            evidence["check_id"]
                .as_str()
                .context("evidence needs check_id")?,
        )?;
        ensure!(
            evidence["operation_id"].as_u64().is_some_and(|id| id > 0),
            "evidence needs operation_id"
        );
    }
    list(value, "findings")?;
    planned_checks(value)?;
    if value["outcome"] == "needs_input" {
        ensure!(
            !list(value, "questions")?.is_empty(),
            "needs_input requires questions"
        );
    }
    Ok(())
}
pub fn planned_checks(value: &Value) -> Result<Vec<PlannedCheck>> {
    let mut seen = BTreeSet::new();
    list(value, "checks")?
        .into_iter()
        .map(|v| {
            let mut v = v.clone();
            if v["requirements"].as_object().is_some_and(|o| o.is_empty()) {
                v["requirements"] = json!([]);
            }
            if v["inputs"].as_object().is_some_and(|o| o.is_empty()) {
                v["inputs"] = json!([]);
            }
            let check: PlannedCheck = serde_json::from_value(v).context("invalid planned check")?;
            identifier(&check.id)?;
            for input in &check.inputs {
                relative(Path::new(input))?;
            }
            ensure!(seen.insert(check.id.clone()), "duplicate check ID");
            ensure!(
                !check.command.trim().is_empty() && check.command.len() <= 65_536,
                "invalid check command"
            );
            ensure!(
                check
                    .timeout
                    .is_none_or(|t| t.is_finite() && (1.0..=600.0).contains(&t)),
                "invalid check timeout"
            );
            Ok(check)
        })
        .collect()
}
pub fn validate_review_decision(
    request: &ReviewRequest,
    decision: &str,
    input: Option<&str>,
) -> Result<()> {
    identifier(&request.gate_id)?;
    ensure!(
        request.decisions.iter().any(|d| d == decision),
        "decision is not permitted by this gate"
    );
    let valid = match decision {
        "approve" => matches!(request.kind.as_str(), "review" | "acceptance"),
        "revise" => matches!(request.kind.as_str(), "review" | "acceptance"),
        "answer" => request.kind == "input",
        "waive" => request.kind == "input" && !request.check_ids.is_empty(),
        _ => false,
    };
    ensure!(valid, "decision is invalid for this gate kind");
    if matches!(decision, "revise" | "answer" | "waive") {
        ensure!(
            input.is_some_and(|s| !s.trim().is_empty()),
            "decision requires feedback in --input-file"
        );
    }
    ensure!(
        input.is_none_or(|s| s.len() <= 1024 * 1024),
        "review input exceeds 1 MiB"
    );
    Ok(())
}
pub fn validate_check_receipt(
    check: &PlannedCheck,
    intent: &Value,
    outcome: &Value,
    stage_id: &str,
    observation: &ProjectObservation,
) -> Result<CheckEvidence> {
    let operation_id = intent["seq"]
        .as_u64()
        .context("check intent missing sequence")?;
    ensure!(
        intent["kind"] == "intent" && intent["tool"] == "sh",
        "evidence must reference a shell intent"
    );
    ensure!(
        outcome["kind"] == "outcome"
            && outcome["of"].as_u64() == Some(operation_id)
            && outcome["tool"] == "sh",
        "check outcome does not match intent"
    );
    let timed_out = outcome["ret"]["timed_out"].as_bool().unwrap_or(false);
    let code = outcome["ret"]["code"].as_i64().unwrap_or(-1);
    ensure!(
        outcome["ok"] == true && outcome["ret"]["t"] == "sh" && !timed_out && code == 0,
        if timed_out { "command timed out" } else { "command failed with non-zero exit code" }
    );
    ensure!(
        intent["workflow_stage"].as_str() == Some(stage_id),
        "check belongs to another stage"
    );
    ensure!(
        intent["project_observation"]["digest"].as_str() == Some(observation.digest.as_str()),
        "check evidence is stale for the current project (workspace digest changed during verification)"
    );
    ensure!(
        outcome["workflow_stage"].as_str() == Some(stage_id),
        "check outcome belongs to another stage"
    );
    ensure!(
        outcome["project_observation"]["digest"].as_str() == Some(observation.digest.as_str()),
        "check changed the observed project revision (command mutated workspace files)"
    );
    ensure!(
        intent["args"]["cmd"].as_str() == Some(check.command.as_str()),
        "check command differs from planned command"
    );
    let actual_timeout = intent["args"]["timeout"]
        .as_f64()
        .unwrap_or(60.0)
        .clamp(1.0, 600.0);
    ensure!(
        actual_timeout == check.timeout.unwrap_or(60.0),
        "check timeout differs from plan"
    );
    let cwd = |s: Option<&str>| -> Result<PathBuf> {
        let path = Path::new(s.unwrap_or("."));
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            observation.root.join(path)
        };
        let path = std::fs::canonicalize(path)?;
        ensure!(
            path.starts_with(&observation.root),
            "check cwd escapes project"
        );
        Ok(path)
    };
    ensure!(
        cwd(intent["args"]["cwd"].as_str())? == cwd(check.cwd.as_deref())?,
        "check cwd differs from plan"
    );
    Ok(CheckEvidence {
        check_id: check.id.clone(),
        operation_id,
        stage_id: stage_id.into(),
        project_digest: observation.digest.clone(),
    })
}

const MAX_FILES: usize = 100_000;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILE: u64 = 32 * 1024 * 1024;
fn relative(path: &Path) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty()
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "project inventory path must be relative and normalized"
    );
    Ok(())
}
fn excluded(path: &Path, exclusions: &[PathBuf]) -> bool {
    path.starts_with(".git")
        || path.starts_with(".harness")
        || path.starts_with(".agents/artifacts")
        || exclusions.iter().any(|e| path.starts_with(e))
}
/// Fixed Git metadata command, with bounded output and a five second deadline.
fn git_output(root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    let mut command = Command::new("git");
    // User shell Git overrides must not redirect the host inventory to another index.
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command.env_remove(key);
        }
    }
    let mut child = match command
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            anyhow::bail!("project Git inventory timed out");
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let bytes = reader
        .join()
        .map_err(|_| anyhow::anyhow!("Git inventory reader failed"))??;
    ensure!(
        bytes.len() <= 8 * 1024 * 1024,
        "project inventory exceeds 8 MiB"
    );
    if !status.success() {
        ensure!(
            !root.ancestors().any(|p| p.join(".git").exists()),
            "Git project inventory failed"
        );
        return Ok(None);
    }
    Ok(Some(bytes))
}
pub(crate) fn git_inventory(root: &Path) -> Result<Option<Vec<PathBuf>>> {
    use std::os::unix::ffi::OsStringExt;
    let Some(top) = git_output(root, &["rev-parse", "--show-toplevel"])? else {
        return Ok(None);
    };
    let top = top.strip_suffix(b"\n").unwrap_or(&top);
    let top = PathBuf::from(std::ffi::OsString::from_vec(top.to_vec()));
    // A workspace nested inside another repository owns its own inventory. In
    // particular, a parent ignore rule must not make that inventory empty.
    if std::fs::canonicalize(top)? != root {
        return Ok(None);
    }
    let Some(bytes) = git_output(
        root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )?
    else {
        anyhow::bail!("Git project inventory unavailable");
    };
    Ok(Some(
        bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(std::ffi::OsString::from_vec(s.to_vec())))
            .collect(),
    ))
}
/// Observe exactly the same bounded inventory for verification receipts and owner acceptance.
/// Tracked ignored/deleted files remain in scope. Build outputs ignored by Git are omitted.
pub fn observe_project(
    root: &Path,
    exclusions: &[PathBuf],
    extra_inputs: &[String],
) -> Result<ProjectObservation> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let root = std::fs::canonicalize(root)?;
    let exclusions: Vec<PathBuf> = exclusions
        .iter()
        .map(|p| p.strip_prefix(&root).unwrap_or(p).to_path_buf())
        .collect();
    let mut paths: BTreeSet<PathBuf> = match git_inventory(&root)? {
        Some(paths) => paths.into_iter().collect(),
        None => {
            let exclude = exclusions.clone();
            let base = root.clone();
            let mut builder = ignore::WalkBuilder::new(&root);
            builder
                .hidden(false)
                .parents(false)
                .follow_links(false)
                .require_git(false)
                .filter_entry(move |e| {
                    e.path()
                        .strip_prefix(&base)
                        .is_ok_and(|p| p.as_os_str().is_empty() || !excluded(p, &exclude))
                });
            let mut paths = BTreeSet::new();
            for entry in builder.build() {
                let entry = entry.context("project inventory traversal")?;
                if entry.file_type().is_some_and(|t| !t.is_dir()) {
                    paths.insert(entry.path().strip_prefix(&root)?.to_path_buf());
                }
                ensure!(
                    paths.len() <= MAX_FILES,
                    "project inventory exceeds {MAX_FILES} files"
                );
            }
            paths
        }
    };
    for input in extra_inputs {
        let p = Path::new(input);
        relative(p)?;
        ensure!(
            !excluded(p, &exclusions),
            "explicit check input is excluded from project observation"
        );
        paths.insert(p.to_path_buf());
    }
    ensure!(
        paths.len() <= MAX_FILES,
        "project inventory exceeds {MAX_FILES} files"
    );
    let mut inventory = BTreeMap::new();
    let mut total = 0u64;
    for path in paths {
        relative(&path)?;
        if excluded(&path, &exclusions) {
            continue;
        }
        let name = path
            .to_str()
            .context("project inventory requires UTF-8 file paths")?
            .to_string();
        let absolute = root.join(&path);
        // Resolve parents only: hash a symlink's identity without following it.
        if let Some(parent) = absolute.parent() {
            if parent.exists() {
                ensure!(
                    std::fs::canonicalize(parent)?.starts_with(&root),
                    "inventory parent escapes project"
                );
            }
        }
        let meta = match std::fs::symlink_metadata(&absolute) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                inventory.insert(name, "deleted".into());
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let hash = if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&absolute)?;
            let resolved = absolute.parent().unwrap().join(&target);
            if let Ok(resolved) = std::fs::canonicalize(&resolved) {
                ensure!(
                    resolved.starts_with(&root),
                    "inventory symlink escapes project"
                );
            } else {
                ensure!(
                    !target.is_absolute()
                        && target
                            .components()
                            .all(|c| !matches!(c, Component::ParentDir)),
                    "unresolved inventory symlink escapes scope"
                );
            }
            format!(
                "symlink:{}",
                crate::eventlog::sha_bytes(target.as_os_str().as_encoded_bytes())
            )
        } else if meta.is_file() {
            ensure!(
                meta.len() <= MAX_FILE,
                "project observation file exceeds 32 MiB: {name}"
            );
            total = total
                .checked_add(meta.len())
                .context("project observation size overflow")?;
            ensure!(total <= MAX_BYTES, "project observation exceeds 256 MiB");
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&absolute)?;
            let before = file.metadata()?;
            ensure!(before.is_file(), "project entry changed type");
            let mut bytes = Vec::new();
            file.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_FILE,
                "project file grew beyond observation bound"
            );
            let after = std::fs::symlink_metadata(&absolute)?;
            ensure!(
                before.ino() == after.ino()
                    && before.len() == after.len()
                    && before.mtime() == after.mtime()
                    && before.mtime_nsec() == after.mtime_nsec(),
                "project changed during observation: {name}"
            );
            format!(
                "file:{:o}:{}",
                before.mode() & 0o111,
                crate::eventlog::sha_bytes(&bytes)
            )
        } else {
            anyhow::bail!("unsupported project inventory entry: {name}");
        };
        inventory.insert(name, hash);
    }
    let digest = crate::eventlog::sha_bytes(&serde_json::to_vec(&inventory)?);
    Ok(ProjectObservation {
        digest,
        inventory,
        root,
        extra_inputs: extra_inputs.to_vec(),
    })
}

/// Include ignored input files explicitly declared by the accepted check plan.
pub fn observe_checks(root: &Path, plan: &[PlannedCheck]) -> Result<ProjectObservation> {
    let inputs: BTreeSet<String> = plan.iter().flat_map(|c| c.inputs.iter().cloned()).collect();
    observe_project(root, &[], &inputs.into_iter().collect::<Vec<_>>())
}

/// Materialize a verified journal blob under the workflow's dedicated projection directory.
/// Existing files are replaceable only when their identity was recorded by this projection ledger.
pub fn project_artifact(
    root: &Path,
    session: &str,
    log_dir: &Path,
    artifact: &ArtifactRef,
) -> Result<PathBuf> {
    identifier(session)?;
    relative(Path::new(&artifact.name))?;
    ensure!(
        Path::new(&artifact.name).components().count() == 1 && artifact.name.ends_with(".md"),
        "artifact name must be one Markdown filename"
    );
    let root = std::fs::canonicalize(root)?;
    let directory = root
        .join(".agents/artifacts")
        .join(format!("rpi-{session}"));
    let path = directory.join(&artifact.name);
    let bytes = crate::eventlog::read_blob(log_dir, &artifact.document_blob)?;
    let target = crate::atomic_file::Target::open(&path, true)?;
    let ledger_path = directory.join(format!(".{}.projection", artifact.name));
    let ledger = crate::atomic_file::Target::open(&ledger_path, false)?;
    let prior = match ledger.read_to_string() {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    if prior.is_none() {
        match target.read_to_string() {
            Ok(text) => ensure!(
                crate::eventlog::sha_bytes(text.as_bytes()) == artifact.document_blob,
                "refusing to overwrite unowned artifact projection"
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    } else {
        ensure!(
            prior
                .as_ref()
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
            "invalid artifact projection ledger"
        );
    }
    // The ledger is a recoverability hint, never authority for model inputs or approval.
    ledger.replace(artifact.document_blob.as_bytes())?;
    target.replace(&bytes)?;
    Ok(path)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VerificationSummary {
    pub passed: Vec<String>,
    pub failed: Vec<String>,
    pub unrun: Vec<String>,
    pub evidence: Vec<CheckEvidence>,
}
pub fn check_result(
    plan: &[PlannedCheck],
    payload: &Value,
    records: &[Value],
    stage_id: &str,
    observation: &ProjectObservation,
) -> Result<VerificationSummary> {
    let mut result = VerificationSummary::default();
    let mut supplied = BTreeMap::new();
    for e in list(payload, "evidence")? {
        let check_id = e["check_id"]
            .as_str()
            .context("evidence missing check ID")?;
        ensure!(
            plan.iter().any(|c| c.id == check_id),
            "evidence references unknown check {check_id}"
        );
        ensure!(
            supplied
                .insert(
                    check_id,
                    e["operation_id"]
                        .as_u64()
                        .context("evidence missing operation ID")?
                )
                .is_none(),
            "duplicate evidence check ID"
        );
    }
    for check in plan {
        let Some(operation_id) = supplied.get(check.id.as_str()) else {
            result.unrun.push(check.id.clone());
            continue;
        };
        let intent = records
            .iter()
            .find(|r| r["seq"].as_u64() == Some(*operation_id))
            .context("unknown check operation")?;
        let outcome = records
            .iter()
            .find(|r| r["kind"] == "outcome" && r["of"].as_u64() == Some(*operation_id))
            .context("check has no recorded outcome")?;
        match validate_check_receipt(check, intent, outcome, stage_id, observation) {
            Ok(evidence) => {
                result.passed.push(check.id.clone());
                result.evidence.push(evidence);
            }
            Err(err) => result.failed.push(format!("{}: {err}", check.id)),
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn workspace(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-workflow-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }
    #[test]
    fn submission_rejects_unsupported_and_unbounded_payloads() {
        let mut v = json!({"outcome":"complete","document":"# Complete","summary":"done","questions":{},"checks":{},"evidence":{}});
        validate_submission(&v).unwrap();
        v["outcome"] = json!("needs_input");
        assert!(validate_submission(&v).is_err());
        v["questions"] = json!(["Which account?"]);
        validate_submission(&v).unwrap();
        v["evidence"] = json!([{"check_id":"unit","operation_id":0}]);
        assert!(validate_submission(&v).is_err());
        v["evidence"] = json!([]);
        v["document"] = json!("a".repeat(2 * 1024 * 1024));
        assert!(validate_submission(&v).is_err());
    }
    #[test]
    fn observation_excludes_logs_and_artifacts_and_tracks_source_and_links() {
        let root = workspace("observation");
        std::fs::write(root.join("app.js"), "one").unwrap();
        let a = observe_project(&root, &[], &[]).unwrap();
        std::fs::create_dir_all(root.join(".harness/log")).unwrap();
        std::fs::write(root.join(".harness/log/run"), "log").unwrap();
        std::fs::create_dir_all(root.join(".agents/artifacts/rpi-run")).unwrap();
        std::fs::write(root.join(".agents/artifacts/rpi-run/one.md"), "doc").unwrap();
        assert_eq!(a.digest, observe_project(&root, &[], &[]).unwrap().digest);
        std::fs::write(root.join("app.js"), "two").unwrap();
        assert_ne!(a.digest, observe_project(&root, &[], &[]).unwrap().digest);
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape")).unwrap();
        assert!(observe_project(&root, &[], &[]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn nested_workspace_ignores_parent_rules_but_honors_local_ignores() {
        let parent = workspace("nested-inventory");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&parent)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(parent.join(".gitignore"), ".harness/\n").unwrap();
        let root = parent.join(".harness/live");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.js"), "one").unwrap();
        std::fs::write(root.join(".gitignore"), "build\n").unwrap();
        std::fs::write(root.join("build"), "generated").unwrap();
        let first = observe_project(&root, &[], &[]).unwrap();
        assert!(first.inventory.contains_key("app.js"));
        assert!(!first.inventory.contains_key("build"));
        std::fs::write(root.join("app.js"), "two").unwrap();
        assert_ne!(
            first.digest,
            observe_project(&root, &[], &[]).unwrap().digest
        );
        std::fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn git_observation_retains_ignored_tracked_and_deleted_files() {
        use std::process::Command;
        let root = workspace("git");
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join("tracked"), "one").unwrap();
        assert!(
            Command::new("git")
                .args(["add", "tracked"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join(".gitignore"), "tracked\nbuild\n").unwrap();
        std::fs::write(root.join("build"), "ignored").unwrap();
        let a = observe_project(&root, &[], &[]).unwrap();
        assert!(a.inventory.contains_key("tracked"));
        assert!(!a.inventory.contains_key("build"));
        let explicit = observe_project(&root, &[], &["build".into()]).unwrap();
        assert!(explicit.inventory.contains_key("build"));
        assert_eq!(explicit.extra_inputs, vec!["build".to_string()]);
        std::fs::write(root.join("build"), "changed ignored input").unwrap();
        assert_ne!(
            explicit.digest,
            observe_project(&root, &[], &explicit.extra_inputs)
                .unwrap()
                .digest
        );
        assert_eq!(a.digest, observe_project(&root, &[], &[]).unwrap().digest);
        std::fs::remove_file(root.join("tracked")).unwrap();
        let b = observe_project(&root, &[], &[]).unwrap();
        assert_ne!(a.digest, b.digest);
        assert_eq!(b.inventory["tracked"], "deleted");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn evidence_requires_success_exact_invocation_and_fresh_revision() {
        let root = workspace("evidence");
        let observation = observe_project(&root, &[], &[]).unwrap();
        let check = PlannedCheck {
            id: "unit".into(),
            command: "bun test".into(),
            cwd: None,
            timeout: None,
            requirements: vec![],
            inputs: vec![],
        };
        let mut intent = json!({"kind":"intent","seq":4,"tool":"sh","args":{"cmd":"bun test","cwd":null,"timeout":null},"workflow_stage":"verify","project_observation":observation});
        let mut outcome = json!({"kind":"outcome","of":4,"tool":"sh","ok":true,"ret":{"t":"sh","code":0,"timed_out":false},"workflow_stage":"verify","project_observation":observation});
        validate_check_receipt(&check, &intent, &outcome, "verify", &observation).unwrap();
        outcome["ret"]["code"] = json!(1);
        assert!(validate_check_receipt(&check, &intent, &outcome, "verify", &observation).is_err());
        outcome["ret"]["code"] = json!(0);
        outcome["ret"]["timed_out"] = json!(true);
        assert!(validate_check_receipt(&check, &intent, &outcome, "verify", &observation).is_err());
        outcome["ret"]["timed_out"] = json!(false);
        intent["args"]["cmd"] = json!("true");
        assert!(validate_check_receipt(&check, &intent, &outcome, "verify", &observation).is_err());
        intent["args"]["cmd"] = json!("bun test");
        intent["project_observation"]["digest"] = json!("old");
        assert!(validate_check_receipt(&check, &intent, &outcome, "verify", &observation).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn projection_refuses_unowned_files_and_regenerates_owned_views() {
        let root = workspace("projection");
        let log = crate::eventlog::EventLog::open(&root.join(".harness/log")).unwrap();
        let blob = log.blob(b"# Canonical").unwrap();
        let artifact = ArtifactRef {
            id: "request".into(),
            name: "01-request.md".into(),
            document_blob: blob,
            summary: "request".into(),
            submission: json!({}),
            stage_id: "request".into(),
            dependencies: vec![],
        };
        let path = root.join(".agents/artifacts/rpi-test/01-request.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "foreign").unwrap();
        assert!(project_artifact(&root, "test", log.dir(), &artifact).is_err());
        std::fs::remove_file(&path).unwrap();
        project_artifact(&root, "test", log.dir(), &artifact).unwrap();
        std::fs::write(&path, "edited projection").unwrap();
        project_artifact(&root, "test", log.dir(), &artifact).unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "# Canonical");
        let mut invalid = artifact;
        invalid.name = "../outside.md".into();
        assert!(project_artifact(&root, "test", log.dir(), &invalid).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

impl Boundary {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.id)?;
        if let Some(id) = &self.submission {
            identifier(id)?;
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= 2 * 1024 * 1024,
            "workflow boundary exceeds 2 MiB"
        );
        match &self.next {
            BoundaryNext::Stage { stage } => {
                identifier(&stage.id)?;
                if let Some(name) = &stage.artifact_name {
                    relative(Path::new(name))?;
                    ensure!(
                        Path::new(name).components().count() == 1 && name.ends_with(".md"),
                        "artifact_name must be one Markdown filename"
                    );
                }
                ensure!(
                    !stage.name.trim().is_empty() && stage.name.len() <= 200,
                    "invalid stage name"
                );
                ensure!(
                    !stage.instructions.trim().is_empty()
                        && stage.instructions.len() <= 1024 * 1024,
                    "invalid stage instructions"
                );
                ensure!(stage.inputs.len() <= 1000, "too many stage input artifacts");
                for id in &stage.inputs {
                    identifier(id)?;
                }
            }
            BoundaryNext::Review { request } => {
                identifier(&request.gate_id)?;
                ensure!(
                    matches!(request.kind.as_str(), "review" | "input" | "acceptance"),
                    "invalid review gate kind"
                );
                identifier(&request.continuation)?;
                ensure!(
                    !request.decisions.is_empty(),
                    "review gate must permit a decision"
                );
                let mut decisions = BTreeSet::new();
                for decision in &request.decisions {
                    ensure!(decisions.insert(decision), "duplicate gate decision");
                    // Feedback is validated at decision recording; here validate the declared action shape.
                    validate_review_decision(request, decision, Some("gate validation"))?;
                }
                for id in request.artifacts.iter().chain(&request.check_ids) {
                    identifier(id)?;
                }
            }
            BoundaryNext::Finish { report } => {
                ensure!(
                    report.get("finished").is_some_and(Value::is_boolean)
                        && report.get("final_answer").is_some_and(Value::is_string),
                    "workflow finish needs finished and final_answer"
                );
            }
        }
        Ok(())
    }
}
