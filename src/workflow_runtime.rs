//! RPI's durable stage boundaries: submission authority, receipts, artifacts and owner gates. The host side is
//! specific to std/rpi; only stage routing and prompts live in Luau.
use crate::{
    eventlog,
    host::Host,
    policy::Class,
    supervisor::{Checkpoint, RunState, Supervisor, Tool},
    workflow::*,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::Path, sync::atomic::Ordering};

pub struct EffectGuard<'a>(&'a Supervisor);
impl<'a> EffectGuard<'a> {
    pub fn new(supervisor: &'a Supervisor) -> Self {
        supervisor.active_effects.fetch_add(1, Ordering::SeqCst);
        Self(supervisor)
    }
}
impl Drop for EffectGuard<'_> {
    fn drop(&mut self) {
        self.0.active_effects.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a root chunk left for the controller: its prepared submission, if the chunk succeeded and made one, and
/// whether `stage.submit` refused a payload during the chunk.
#[derive(Default)]
pub struct ChunkEnd {
    pub submission: Option<Value>,
    pub refused: bool,
}

/// What a resumed owner gate does next.
pub(crate) enum OwnerResume {
    /// No unconsumed decision is recorded: the run stays paused.
    Waiting,
    /// A decision was consumed; deliver this event to the controller.
    Deliver(Value),
    /// The recorded decision was consumed without effect (an approval gone stale); the gate stays open.
    Rejected(String),
}

impl Supervisor {
    pub fn workflow_epoch(&self) -> Option<u64> {
        self.workflow
            .lock()
            .unwrap()
            .as_ref()
            .map(|f| f.stage_epoch)
    }
    pub fn authorize_stage(&self, epoch: Option<u64>, tool: Tool, class: Class) -> Result<()> {
        let frame = self.workflow.lock().unwrap();
        let Some(frame) = frame.as_ref() else {
            return Ok(());
        };
        ensure!(
            epoch == Some(frame.stage_epoch),
            "stage effect belongs to a stale epoch"
        );
        let active = frame.stage.as_ref().context("no active workflow stage")?;
        ensure!(
            active.submission.is_none(),
            "stage.submit is terminal: effects after submission are refused"
        );
        let stage = &active.spec;
        if stage.access == StageAccess::Inspect {
            ensure!(
                matches!(
                    tool,
                    Tool::Read | Tool::Search | Tool::History | Tool::View | Tool::Agent | Tool::Llm | Tool::McpList
                ) || matches!(tool, Tool::Http | Tool::McpCall) && class == Class::Read,
                "stage inspect access denies {}",
                tool.name()
            );
        }
        Ok(())
    }
    /// Stage facts for an effect's intent or outcome record: the stage ID, and for a verification-stage shell command
    /// the project digest its receipt is checked against. Only the digest is stored; the inventory is recomputable.
    pub fn workflow_effect_metadata(&self, tool: Tool, fields: &mut Value) -> Result<()> {
        let (stage_id, plan) = {
            let guard = self.workflow.lock().unwrap();
            let Some((frame, active)) = guard.as_ref().and_then(|f| f.stage.as_ref().map(|a| (f, a))) else {
                return Ok(());
            };
            let plan = (matches!(tool, Tool::Sh) && active.spec.verify).then(|| frame.check_plan.clone());
            (active.spec.id.clone(), plan)
        };
        fields["workflow_stage"] = json!(stage_id);
        if let Some(plan) = plan {
            match observe_checks(&self.policy.workspace, &plan) {
                Ok(observation) => fields["project_digest"] = json!(observation.digest),
                Err(error) if fields.get("ret").is_some() => {
                    fields["project_observation_error"] = json!(format!("{error:#}"))
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// `stage.submit`: check the payload against the active stage and prepare the submission the chunk's boundary
    /// seals. Everything the model influences is checked here, so a bad payload is a refused call the model can
    /// correct rather than a failed boundary. A refusal also marks the stage, so the chunk reports `submission_error`.
    pub fn submit_stage(&self, epoch: Option<u64>, payload: Value) -> Result<String> {
        self.check()?;
        let result = self.prepare_submission(epoch, payload);
        if result.is_err()
            && let Some(active) = self
                .workflow
                .lock()
                .unwrap()
                .as_mut()
                .filter(|f| Some(f.stage_epoch) == epoch)
                .and_then(|f| f.stage.as_mut())
        {
            active.refused = true;
        }
        result
    }

    fn prepare_submission(&self, epoch: Option<u64>, mut payload: Value) -> Result<String> {
        let (stage, plan, started) = {
            let guard = self.workflow.lock().unwrap();
            let frame = guard
                .as_ref()
                .context("stage.submit requires an active workflow")?;
            ensure!(
                epoch == Some(frame.stage_epoch),
                "stage.submit belongs to a stale stage"
            );
            let active = frame
                .stage
                .as_ref()
                .context("stage.submit requires active stage")?;
            ensure!(
                active.submission.is_none(),
                "stage.submit may only be called once"
            );
            let started = frame.observation.as_ref().map(|o| o.digest.clone());
            (active.spec.clone(), frame.check_plan.clone(), started)
        };
        validate_submission(&payload).context("stage.submit")?;
        ensure!(
            self.active_effects.load(Ordering::SeqCst) == 0,
            "stage.submit requires all descendant effects to finish"
        );
        let object = payload
            .as_object_mut()
            .expect("validate_submission accepts only objects");
        // Native facts cannot be forged by including a field with this name.
        object.remove("verification");
        let Some(Value::String(mut document)) = object.remove("document") else {
            unreachable!("validate_submission requires a text document")
        };
        let complete = payload["outcome"] == "complete";
        let checks = planned_checks(&payload)?;
        if stage.plans_checks {
            if payload.get("checks").is_some() {
                document.push_str(&check_plan_section(&checks)?);
            }
        } else {
            ensure!(
                checks.is_empty(),
                "stage.submit: only program design plans checks; omit `checks` (the frozen plan is stage.checks)"
            );
        }
        if stage.verify {
            let observation = observe_checks(&self.policy.workspace, &plan)
                .context("stage.submit: the project cannot be observed")?;
            let root = std::fs::canonicalize(&self.policy.workspace)?;
            let records = eventlog::load(&self.policy.log.path)?;
            let result = check_result(&plan, &payload, &records, &stage.id, &root, &observation.digest)
                .context("stage.submit")?;
            let mut findings = result.failed.clone();
            findings.extend(
                result
                    .unrun
                    .iter()
                    .map(|s| format!("Required check not verified: {s}")),
            );
            if plan.is_empty() {
                findings.push("No executable acceptance checks were planned.".into());
            }
            match started {
                None => findings.push("The project could not be observed when verification started; run a fresh verification.".into()),
                Some(digest) if digest != observation.digest => findings.push("Project changed during verification; run a fresh verification after final edits.".into()),
                Some(_) => {}
            }
            payload["verification"] =
                json!({"passed":findings.is_empty(),"findings":findings,"checks":result});
        } else if stage.access == StageAccess::Workspace && complete {
            // Verification observes the project next; a project it cannot observe must be fixed while this stage can
            // still change it.
            observe_checks(&self.policy.workspace, &plan).context(
                "stage.submit: verification could not observe the project; fix this before submitting",
            )?;
        }
        let document_blob = self.policy.log.blob(document.as_bytes())?;
        let id = format!(
            "submission-{}",
            eventlog::sha_bytes(&serde_json::to_vec(
                &json!({"stage":stage.id,"payload":payload,"blob":document_blob})
            )?)
        );
        let mut guard = self.workflow.lock().unwrap();
        let active = guard
            .as_mut()
            .filter(|f| Some(f.stage_epoch) == epoch)
            .and_then(|f| f.stage.as_mut())
            .filter(|a| a.submission.is_none())
            .context("stage.submit: the stage changed while the submission was prepared")?;
        active.submission = Some(Submission {
            id: id.clone(),
            stage_id: stage.id,
            payload,
            document_blob,
        });
        Ok(id)
    }

    /// End a root chunk of the active stage. A failed chunk invalidates its prepared submission.
    pub fn finish_stage_chunk(&self, success: bool) -> ChunkEnd {
        let mut frame = self.workflow.lock().unwrap();
        let Some(active) = frame.as_mut().and_then(|f| f.stage.as_mut()) else {
            return ChunkEnd::default();
        };
        if !success {
            active.submission = None;
        }
        ChunkEnd {
            submission: active
                .submission
                .as_ref()
                .map(|s| json!({"id":s.id,"payload":s.payload})),
            refused: std::mem::take(&mut active.refused),
        }
    }
}

fn document_event(frame: &ExecutionFrame, log: &eventlog::EventLog) -> Result<serde_json::Map<String, Value>> {
    let artifacts = frame.artifacts.iter().map(|a| -> Result<Value> {
        let name = if a.stage_id == "request" {"request"} else {a.stage_id.rsplit_once('-').map_or(a.stage_id.as_str(),|(name,_)|name)};
        Ok(json!({"id":a.id,"name":name,"path":a.name,"summary":a.summary,"submission":a.submission,
            "document":String::from_utf8(eventlog::read_blob(log.dir(),&a.document_blob)?)?}))
    }).collect::<Result<Vec<_>>>()?;
    let owner_inputs = frame.owner_inputs.iter().map(|i| -> Result<Value> {
        let text = i.content_blob.as_ref().map(|b|eventlog::read_blob(log.dir(),b)).transpose()?.map(String::from_utf8).transpose()?.unwrap_or_default();
        Ok(json!({"id":i.id,"gate_id":i.gate_id,"decision":i.decision,"text":text,"check_ids":i.check_ids}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(serde_json::Map::from_iter([("artifacts".into(), json!(artifacts)), ("owner_inputs".into(), json!(owner_inputs))]))
}

impl Host {
    pub(crate) fn is_workflow_root(&self) -> bool {
        self.depth == 0
            && self
                .supervisor
                .manifest
                .as_ref()
                .is_some_and(|m| m.workflow.is_some())
    }

    /// Regenerate the readable projections under `.agents/artifacts/rpi-RUN/`. They are views of canonical journal
    /// blobs, so a failure is reported and never stops the run.
    pub(crate) fn project_workflow_artifacts(&self) {
        if !self.is_workflow_root() {
            return;
        }
        let latest: Vec<ArtifactRef> = match self.supervisor.workflow.lock().unwrap().as_ref() {
            // Keep only the latest projection for a stable view name.
            Some(frame) => {
                let mut latest = std::collections::BTreeMap::new();
                for artifact in &frame.artifacts {
                    latest.insert(&artifact.name, artifact);
                }
                latest.into_values().cloned().collect()
            }
            None => return,
        };
        for artifact in &latest {
            if let Err(error) = project_artifact(
                &self.workspace,
                &self.policy.log.session,
                self.policy.log.dir(),
                artifact,
            ) {
                let message = format!("artifact projection {} failed: {error:#}", artifact.name);
                eprintln!("hx: warning: {message}");
                self.log(&format!("### WARNING\n{message}"));
            }
        }
    }

    pub(crate) fn workflow_pause_report(&self, report: &serde_json::Map<String, Value>) -> Value {
        let mut report = Value::Object(report.clone());
        if let Err(error) = self.supervisor.check() {
            report["continuation_error"] = json!(error.to_string());
        }
        self.report(report, 0.0)
    }

    /// Consume the latest unconsumed owner decision recorded for the open gate, exactly once. An approval of an
    /// acceptance gate whose project changed since verification is consumed without effect, so the owner can record
    /// another decision for the same gate.
    pub(crate) fn resume_owner_decision(&self, request: &ReviewRequest) -> Result<OwnerResume> {
        let mut frame = self
            .supervisor
            .workflow
            .lock()
            .unwrap()
            .clone()
            .context("missing workflow frame")?;
        let records = eventlog::load(&self.policy.log.path)?;
        let decision = records.iter().rev().find(|r| {
            r["kind"] == "owner_decision"
                && r["gate_id"] == request.gate_id
                && !frame.consumed_decisions.contains(&r["seq"].to_string())
        });
        let Some(decision) = decision else {
            return Ok(OwnerResume::Waiting);
        };
        let kind = decision["decision"]
            .as_str()
            .context("owner decision without a decision")?;
        // A final acceptance reduction dispatches no model or workspace effects.
        if !(request.continuation == "finish" && kind == "approve") {
            self.supervisor.check()?;
        }
        let text = decision["content_blob"]
            .as_str()
            .map(|b| eventlog::read_blob(self.policy.log.dir(), b))
            .transpose()?
            .map(String::from_utf8)
            .transpose()?
            .unwrap_or_default();
        validate_review_decision(request, kind, Some(&text))?;
        let id = decision["seq"].to_string();
        if kind == "approve"
            && let Some(project) = &request.project
        {
            let stale = match observe_project(&self.workspace, &project.extra_inputs) {
                Ok(now) if now.digest == project.digest => None,
                Ok(_) => Some("the project changed after verification".to_string()),
                Err(error) => Some(format!("the project cannot be observed: {error:#}")),
            };
            if let Some(reason) = stale {
                frame.consumed_decisions.push(id);
                *self.supervisor.workflow.lock().unwrap() = Some(frame);
                return Ok(OwnerResume::Rejected(format!(
                    "approval of acceptance gate {} discarded: {reason}; record `revise` to repair and verify again",
                    request.gate_id
                )));
            }
        }
        frame.owner_inputs.push(OwnerInputRef {
            id: id.clone(),
            gate_id: request.gate_id.clone(),
            decision: kind.into(),
            content_blob: decision["content_blob"].as_str().map(String::from),
            check_ids: request.check_ids.clone(),
        });
        frame.consumed_decisions.push(id);
        let event = crate::controller::Event::OwnerDecision {
            document: document_event(&frame, &self.policy.log)?,
            decision: json!(kind),
            text,
            request: serde_json::to_value(request)?,
        }
        .to_json();
        frame.pending_event = Some(event.clone());
        *self.supervisor.workflow.lock().unwrap() = Some(frame);
        Ok(OwnerResume::Deliver(event))
    }

    /// Commit one controller boundary: seal the chunk's submission as an artifact, then start a stage, open a gate or
    /// finish. Everything model-controlled was checked by `stage.submit`, so an error here is a host or controller bug.
    pub(crate) fn apply_boundary(
        &self,
        boundary: Boundary,
        controller: &Value,
    ) -> Result<RunState> {
        ensure!(
            self.is_workflow_root(),
            "workflow boundary requires explicit root workflow selection"
        );
        boundary.validate()?;
        ensure!(
            self.supervisor.active_effects.load(Ordering::SeqCst) == 0,
            "boundary requires quiescent descendants"
        );
        let mut frame = self
            .supervisor
            .workflow
            .lock()
            .unwrap()
            .clone()
            .context("missing workflow frame")?;
        let hash = eventlog::sha_bytes(&serde_json::to_vec(&boundary)?);
        if let Some(old) = frame.boundaries.get(&boundary.id) {
            ensure!(old == &hash, "boundary ID reused with different contents");
            anyhow::bail!("boundary already committed; consume its pending delivery");
        }
        if frame.artifacts.is_empty() {
            let task = &self
                .supervisor
                .manifest
                .as_ref()
                .context("missing run manifest")?
                .task;
            frame.artifacts.push(ArtifactRef {
                id: "request".into(),
                name: "01-request.md".into(),
                document_blob: self.policy.log.blob(task.as_bytes())?,
                summary: "Original request".into(),
                submission: json!({}),
                stage_id: "request".into(),
            });
        }
        if let Some(id) = &boundary.submission {
            let active = frame.stage.as_mut().context("boundary missing active stage")?;
            let submission = active
                .submission
                .take()
                .context("boundary has no native submission")?;
            ensure!(
                &submission.id == id,
                "boundary submission ID does not match native result"
            );
            let stage = &active.spec;
            ensure!(
                stage.id == submission.stage_id,
                "submission belongs to another stage"
            );
            if stage.plans_checks && submission.payload.get("checks").is_some() {
                frame.check_plan = planned_checks(&submission.payload)?;
            }
            let artifact = ArtifactRef {
                id: submission.id,
                name: stage.artifact_name.clone(),
                document_blob: submission.document_blob,
                summary: submission.payload["summary"].as_str().unwrap_or("").into(),
                submission: submission.payload,
                stage_id: stage.id.clone(),
            };
            frame.artifacts.push(artifact);
        } else if let Some(active) = frame.stage.as_mut() {
            // A reducer may reject malformed phase-specific data and keep working, or stop without accepting it.
            active.submission = None;
        }
        frame.boundaries.insert(boundary.id, hash);
        let saved = match boundary.next {
            BoundaryNext::Stage { stage } => {
                for id in &stage.inputs {
                    ensure!(
                        frame.artifacts.iter().any(|a| &a.id == id),
                        "unknown stage input artifact {id}"
                    );
                }
                frame.stage_epoch += 1;
                let mut metadata = json!({"workspace":self.workspace,"files":crate::host::file_tree(&self.workspace),"check_plan":frame.check_plan});
                frame.observation = None;
                if stage.verify {
                    // An unobservable project does not stop the run: the stage is told why, and stage.submit refuses
                    // until the project can be observed.
                    match observe_checks(&self.workspace, &frame.check_plan) {
                        Ok(observation) => frame.observation = Some(observation),
                        Err(error) => metadata["project_error"] = json!(format!("{error:#}")),
                    }
                }
                frame.stage = Some(ActiveStage {
                    spec: stage.clone(),
                    submission: None,
                    refused: false,
                });
                let event = crate::controller::Event::BoundaryReady {
                    document: document_event(&frame, &self.policy.log)?,
                    stage: serde_json::to_value(&stage)?,
                    metadata,
                }
                .to_json();
                frame.pending_event = Some(event);
                RunState::Running {
                    controller: controller.clone(),
                }
            }
            BoundaryNext::Review { mut request } => {
                for id in &request.artifacts {
                    ensure!(
                        frame.artifacts.iter().any(|a| &a.id == id),
                        "unknown review artifact {id}"
                    );
                }
                if request.kind == "acceptance" {
                    request.project = frame.observation.clone();
                }
                let input = request.kind == "input";
                let mut report = json!({
                    "finished": false,
                    "final_answer": if input {"Owner input required."} else {"Artifact ready for review."},
                    "workflow": "rpi",
                    "status": if input {"needs_input"} else {"awaiting_review"},
                    "stage": frame.stage.as_ref().map(|a| &a.spec.name),
                    "gate": request,
                    "artifact_directory": self.workspace.join(".agents/artifacts").join(format!("rpi-{}", self.policy.log.session)),
                    "next_command": format!("hx review {} --gate {} --decision {}", self.policy.log.path.display(), request.gate_id,
                        if input {"answer --input-file answers.md"} else {"approve"}),
                });
                if !frame.check_plan.is_empty() {
                    // The owner approves the plan verification enforces, as frozen, not only the prose about it.
                    report["check_plan"] = json!(frame.check_plan);
                }
                frame.stage = None;
                frame.pending_event = None;
                let Value::Object(report) = report else {
                    unreachable!("json! object literal")
                };
                RunState::AwaitingReview {
                    controller: controller.clone(),
                    report,
                    gate: request,
                }
            }
            BoundaryNext::Finish { mut report } => {
                if report["finished"] == true {
                    let verified = frame
                        .artifacts
                        .iter()
                        .rev()
                        .find(|a| a.submission.get("verification").is_some())
                        .context("completed workflow has no verified artifact")?;
                    ensure!(
                        verified.submission["verification"]["passed"] == true,
                        "workflow cannot finish with failed check evidence"
                    );
                    ensure!(
                        self.supervisor.uncertain_operations().is_empty(),
                        "workflow cannot finish with uncertain effects"
                    );
                }
                report.insert(
                    "artifact_directory".into(),
                    json!(
                        self.workspace
                            .join(".agents/artifacts")
                            .join(format!("rpi-{}", self.policy.log.session))
                    ),
                );
                report.insert("artifact_refs".into(), json!(frame.artifacts));
                report.insert("owner_inputs".into(), json!(frame.owner_inputs));
                frame.stage = None;
                frame.pending_event = None;
                RunState::Finished { report }
            }
        };
        *self.supervisor.workflow.lock().unwrap() = Some(frame);
        Ok(saved)
    }
}

/// `hx review`: record an owner decision for the open gate. Recording never executes anything; `hx run --resume`
/// consumes the decision.
pub fn review(path: &Path, gate_id: &str, decision: &str, input_file: Option<&Path>) -> Result<()> {
    let log = eventlog::EventLog::reopen(path)?;
    let manifest = crate::supervisor::Manifest::inspect(path)?;
    ensure!(manifest.workflow.is_some(), "run has no review workflow");
    let records = eventlog::load(path)?;
    let (_, checkpoint) = Checkpoint::latest(log.dir(), &records)?.context("run has no checkpoint")?;
    let RunState::AwaitingReview { gate: request, .. } = checkpoint.run else {
        anyhow::bail!("run is not awaiting review");
    };
    let consumed = checkpoint
        .workflow
        .map(|f| f.consumed_decisions)
        .unwrap_or_default();
    ensure!(request.gate_id == gate_id, "stale or unknown review gate");
    let text = input_file
        .map(|p| -> Result<String> {
            let meta = std::fs::metadata(p)?;
            ensure!(meta.len() <= 1024 * 1024, "review input exceeds 1 MiB");
            Ok(std::fs::read_to_string(p)?)
        })
        .transpose()?;
    validate_review_decision(&request, decision, text.as_deref())?;
    if decision == "approve"
        && let Some(project) = &request.project
    {
        ensure!(
            observe_project(&manifest.policy.workspace, &project.extra_inputs)?.digest
                == project.digest,
            "acceptance gate is stale: project changed"
        );
    }
    let content_blob = text.as_ref().map(|s| log.blob(s.as_bytes())).transpose()?;
    // One unconsumed decision per gate. A consumed one (a stale approval discarded on resume) leaves the gate open.
    if let Some(old) = records.iter().find(|r| {
        r["kind"] == "owner_decision"
            && r["gate_id"] == gate_id
            && !consumed.contains(&r["seq"].to_string())
    }) {
        ensure!(
            old["decision"] == decision && old["content_blob"] == json!(content_blob),
            "conflicting decision already recorded"
        );
        outln!(
            "{}",
            json!({"recorded":true,"duplicate":true,"gate_id":gate_id})
        )?;
        return Ok(());
    }
    log.write("owner_decision",json!({"gate_id":gate_id,"decision":decision,"content_blob":content_blob,"artifacts":request.artifacts}),true)?;
    outln!(
        "{}",
        json!({"recorded":true,"gate_id":gate_id,"next_command":format!("hx run --resume {}",path.display())})
    )?;
    Ok(())
}
