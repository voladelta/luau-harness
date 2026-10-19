//! Generic durable stage boundaries. Workflow routing and prompts remain pure Luau.
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
        ensure!(frame.gate.is_none(), "stage is awaiting owner review");
        ensure!(
            frame.pending_submission.is_none(),
            "stage.submit is terminal: effects after submission are refused"
        );
        let stage = frame
            .active_stage
            .as_ref()
            .context("no active workflow stage")?;
        if stage.access == StageAccess::Inspect {
            ensure!(
                matches!(
                    tool,
                    Tool::Read | Tool::Search | Tool::History | Tool::View | Tool::Agent | Tool::McpList
                ) || matches!(tool, Tool::Http | Tool::McpCall) && class == Class::Read,
                "stage inspect access denies {}",
                tool.name()
            );
        }
        Ok(())
    }
    pub fn workflow_effect_metadata(&self, tool: Tool, fields: &mut Value) -> Result<()> {
        let stage = self
            .workflow
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|f| f.active_stage.clone());
        if let Some(stage) = stage {
            fields["workflow_stage"] = json!(stage.id);
            if matches!(tool, Tool::Sh) && stage.verify {
                let plan = self
                    .workflow
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|f| f.check_plan.clone())
                    .unwrap_or_default();
                match observe_checks(&self.policy.workspace, &plan) {
                    Ok(observation) => {
                        fields["project_observation"] = serde_json::to_value(observation)?
                    }
                    Err(error) if fields.get("ret").is_some() => {
                        fields["project_observation_error"] = json!(error.to_string())
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }
    pub fn submit_stage(&self, epoch: Option<u64>, mut payload: Value) -> Result<String> {
        self.check()?;
        validate_submission(&payload).context("stage.submit")?;
        ensure!(
            self.active_effects.load(Ordering::SeqCst) == 0,
            "stage.submit requires all descendant effects to finish"
        );
        let frame = self
            .workflow
            .lock()
            .unwrap()
            .clone()
            .context("stage.submit requires an active workflow")?;
        ensure!(
            epoch == Some(frame.stage_epoch),
            "stage.submit belongs to a stale stage"
        );
        ensure!(
            frame.pending_submission.is_none(),
            "stage.submit may only be called once"
        );
        ensure!(
            frame.gate.is_none(),
            "stage.submit cannot answer an owner gate"
        );
        let stage = frame
            .active_stage
            .as_ref()
            .context("stage.submit requires active stage")?;
        if stage.verify {
            let observation = observe_checks(
                &self.policy.workspace,
                &self
                    .workflow
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|f| f.check_plan.clone())
                    .unwrap_or_default(),
            )?;
            let records = eventlog::load(&self.policy.log.path)?;
            let result = check_result(
                &frame.check_plan,
                &payload,
                &records,
                &stage.id,
                &observation,
            )?;
            let mut findings = result.failed.clone();
            findings.extend(
                result
                    .unrun
                    .iter()
                    .map(|s| format!("Required check not verified: {s}")),
            );
            if frame.check_plan.is_empty() {
                findings.push("No executable acceptance checks were planned.".into());
            }
            if frame
                .observation
                .as_ref()
                .is_none_or(|o| o.digest != observation.digest)
            {
                findings.push("Project changed during verification; run a fresh verification after final edits.".into());
            }
            payload["verification"] =
                json!({"passed":findings.is_empty(),"findings":findings,"checks":result});
        } else {
            // Native facts cannot be forged by including a field with this name.
            payload.as_object_mut().unwrap().remove("verification");
        }
        let document = payload.as_object_mut().unwrap().remove("document").unwrap();
        let document_blob = self
            .policy
            .log
            .blob(document.as_str().unwrap().as_bytes())?;
        let id = format!(
            "submission-{}",
            eventlog::sha_bytes(&serde_json::to_vec(
                &json!({"stage":stage.id,"payload":payload,"blob":document_blob})
            )?)
        );
        let submission = Submission {
            id: id.clone(),
            stage_id: stage.id.clone(),
            payload,
            document_blob,
        };
        self.workflow
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .pending_submission = Some(submission);
        Ok(id)
    }
    pub fn finish_stage_chunk(&self, success: bool) -> Option<Value> {
        let mut frame = self.workflow.lock().unwrap();
        let frame = frame.as_mut()?;
        if !success {
            frame.pending_submission = None;
            return None;
        }
        frame
            .pending_submission
            .as_ref()
            .map(|s| json!({"id":s.id,"payload":s.payload}))
    }
}

fn artifact_name(stage: &StageSpec) -> String {
    if let Some(name) = &stage.artifact_name {
        return name.clone();
    }
    // This is a human-facing label supplied by the workflow, not a host transition rule.
    let safe: String = stage
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!(
        "{}-{}.md",
        stage.id.split('-').next_back().unwrap_or("1"),
        safe
    )
}
fn document_event(frame: &ExecutionFrame, log: &eventlog::EventLog) -> Result<Value> {
    let artifacts = frame.artifacts.iter().map(|a| -> Result<Value> {
        let name = if a.stage_id == "request" {"request"} else {a.stage_id.rsplit_once('-').map_or(a.stage_id.as_str(),|(name,_)|name)};
        Ok(json!({"id":a.id,"name":name,"path":a.name,"summary":a.summary,"submission":a.submission,
            "document":String::from_utf8(eventlog::read_blob(log.dir(),&a.document_blob)?)?}))
    }).collect::<Result<Vec<_>>>()?;
    let owner_inputs = frame.owner_inputs.iter().map(|i| -> Result<Value> {
        let text = i.content_blob.as_ref().map(|b|eventlog::read_blob(log.dir(),b)).transpose()?.map(String::from_utf8).transpose()?.unwrap_or_default();
        Ok(json!({"id":i.id,"gate_id":i.gate_id,"decision":i.decision,"text":text,"check_ids":i.check_ids}))
    }).collect::<Result<Vec<_>>>()?;
    Ok(json!({"artifacts":artifacts,"owner_inputs":owner_inputs}))
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

    pub(crate) fn project_workflow_artifacts(&self) -> Result<()> {
        if !self.is_workflow_root() {
            return Ok(());
        }
        if let Some(frame) = self.supervisor.workflow.lock().unwrap().as_ref() {
            // Keep only the latest projection for a stable view name.
            let mut latest = std::collections::BTreeMap::new();
            for artifact in &frame.artifacts {
                latest.insert(&artifact.name, artifact);
            }
            for artifact in latest.values() {
                project_artifact(
                    &self.workspace,
                    &self.policy.log.session,
                    self.policy.log.dir(),
                    artifact,
                )?;
            }
        }
        Ok(())
    }

    pub(crate) fn workflow_pause_report(&self, report: &serde_json::Map<String, Value>) -> Value {
        let mut report = Value::Object(report.clone());
        if let Err(error) = self.supervisor.check() {
            report["continuation_error"] = json!(error.to_string());
        }
        self.report(report, 0.0)
    }

    pub(crate) fn resume_owner_decision(&self) -> Result<Option<Value>> {
        let frame = self
            .supervisor
            .workflow
            .lock()
            .unwrap()
            .clone()
            .context("missing workflow frame")?;
        let Some(request) = frame.gate.clone() else {
            return Ok(None);
        };
        let records = eventlog::load(&self.policy.log.path)?;
        let decision = records.iter().rev().find(|r| {
            r["kind"] == "owner_decision"
                && r["gate_id"] == request.gate_id
                && !frame.consumed_decisions.contains(&r["seq"].to_string())
        });
        let Some(decision) = decision else {
            return Ok(None);
        };
        // A final acceptance reduction dispatches no model or workspace effects.
        if !(request.continuation == "finish" && decision["decision"] == "approve") {
            self.supervisor.check()?;
        }
        let text = decision["content_blob"]
            .as_str()
            .map(|b| eventlog::read_blob(self.policy.log.dir(), b))
            .transpose()?
            .map(String::from_utf8)
            .transpose()?
            .unwrap_or_default();
        validate_review_decision(
            &request,
            decision["decision"].as_str().unwrap_or(""),
            Some(&text),
        )?;
        if decision["decision"] == "approve"
            && let Some(project) = &request.project
        {
            ensure!(
                observe_project(&self.workspace, &[], &project.extra_inputs)?.digest
                    == project.digest,
                "acceptance gate is stale: project changed; request revision and re-verification"
            );
        }
        let mut next = frame;
        let id = decision["seq"].to_string();
        next.owner_inputs.push(OwnerInputRef {
            id: id.clone(),
            gate_id: request.gate_id.clone(),
            decision: decision["decision"].as_str().unwrap().into(),
            content_blob: decision["content_blob"].as_str().map(String::from),
            check_ids: request.check_ids.clone(),
            artifacts: request.artifacts.clone(),
        });
        next.consumed_decisions.push(id);
        next.gate = None;
        let mut event = document_event(&next, &self.policy.log)?;
        event["kind"] = json!("owner_decision");
        event["decision"] = decision["decision"].clone();
        event["text"] = json!(text);
        event["request"] = serde_json::to_value(request)?;
        next.pending_event = Some(event.clone());
        *self.supervisor.workflow.lock().unwrap() = Some(next);
        Ok(Some(event))
    }

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
        let mut sealed = None;
        if frame.artifacts.is_empty() {
            let task = &self.supervisor.manifest.as_ref().unwrap().task;
            frame.artifacts.push(ArtifactRef {
                id: "request".into(),
                name: "01-request.md".into(),
                document_blob: self.policy.log.blob(task.as_bytes())?,
                summary: "Original request".into(),
                submission: json!({}),
                stage_id: "request".into(),
                dependencies: vec![],
            });
        }
        if let Some(id) = &boundary.submission {
            let submission = frame
                .pending_submission
                .take()
                .context("boundary has no native submission")?;
            ensure!(
                &submission.id == id,
                "boundary submission ID does not match native result"
            );
            let stage = frame
                .active_stage
                .as_ref()
                .context("boundary missing active stage")?;
            ensure!(
                stage.id == submission.stage_id,
                "submission belongs to another stage"
            );
            let checks = planned_checks(&submission.payload)?;
            if stage.access == StageAccess::Inspect && submission.payload.get("checks").is_some() {
                frame.check_plan = checks.clone();
            }
            if stage.access == StageAccess::Workspace {
                ensure!(
                    checks.is_empty(),
                    "workspace stage cannot replace the accepted check plan"
                );
            }
            let artifact = ArtifactRef {
                id: submission.id.clone(),
                name: artifact_name(stage),
                document_blob: submission.document_blob,
                summary: submission.payload["summary"].as_str().unwrap_or("").into(),
                submission: submission.payload,
                stage_id: stage.id.clone(),
                dependencies: stage.inputs.clone(),
            };
            sealed = Some(json!({"id":artifact.id,"submission":artifact.id}));
            frame.artifacts.push(artifact);
        } else {
            // A reducer may reject malformed phase-specific data and keep working,
            // or stop without accepting it. It cannot carry a sealed VM into a new stage.
            frame.pending_submission = None;
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
                frame.active_stage = Some(stage.clone());
                frame.gate = None;
                frame.evidence.clear();
                frame.observation = if stage.verify {
                    Some(observe_checks(&self.workspace, &frame.check_plan)?)
                } else {
                    None
                };
                let mut event = document_event(&frame, &self.policy.log)?;
                event["kind"] = json!("boundary_ready");
                event["stage"] = serde_json::to_value(&stage)?;
                if let Some(sealed) = sealed {
                    event["sealed"] = sealed;
                }
                event["metadata"] = json!({"workspace":self.workspace,"files":crate::host::file_tree(&self.workspace),"check_plan":frame.check_plan,"project":frame.observation});
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
                let report = json!({"finished":false,"final_answer":if request.kind=="input" {"Owner input required."} else {"Artifact ready for review."},"workflow":"rpi","status":if request.kind=="input" {"needs_input"} else {"awaiting_review"},"stage":frame.active_stage.as_ref().map(|s|&s.name),"gate":request,"artifact_directory":self.workspace.join(".agents/artifacts").join(format!("rpi-{}",self.policy.log.session)),"next_command":format!("hx review {} --gate {} --decision {}",self.policy.log.path.display(),request.gate_id,if request.kind=="input" {"answer --input-file answers.md"} else {"approve"})});
                frame.gate = Some(request);
                frame.pending_event = None;
                RunState::AwaitingReview {
                    controller: controller.clone(),
                    report: report.as_object().unwrap().clone(),
                }
            }
            BoundaryNext::Finish { mut report } => {
                ensure!(
                    report.get("finished").is_some_and(Value::is_boolean)
                        && report.get("final_answer").is_some_and(Value::is_string),
                    "workflow finish report needs finished and final_answer"
                );
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
                frame.gate = None;
                frame.pending_event = None;
                RunState::Finished { report }
            }
        };
        *self.supervisor.workflow.lock().unwrap() = Some(frame);
        Ok(saved)
    }
}

/// Owner CLI only. Recording and execution remain separate operations.
pub fn review(path: &Path, gate_id: &str, decision: &str, input_file: Option<&Path>) -> Result<()> {
    let log = eventlog::EventLog::reopen(path)?;
    let manifest = crate::supervisor::Manifest::inspect(path)?;
    ensure!(manifest.workflow.is_some(), "run has no review workflow");
    let records = eventlog::load(path)?;
    let latest = records
        .iter()
        .rev()
        .find(|r| r["kind"] == "checkpoint" && r["lane"] == "root")
        .context("run has no checkpoint")?;
    let checkpoint = Checkpoint::load(&eventlog::read_blob(
        log.dir(),
        latest["blob"].as_str().context("checkpoint blob missing")?,
    )?)?;
    ensure!(
        matches!(checkpoint.run, RunState::AwaitingReview { .. }),
        "run is not awaiting review"
    );
    let frame = checkpoint.workflow.context("missing workflow frame")?;
    let request = frame.gate.context("missing active review gate")?;
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
            observe_project(&manifest.policy.workspace, &[], &project.extra_inputs)?.digest
                == project.digest,
            "acceptance gate is stale: project changed"
        );
    }
    let content_blob = text.as_ref().map(|s| log.blob(s.as_bytes())).transpose()?;
    if let Some(old) = records
        .iter()
        .find(|r| r["kind"] == "owner_decision" && r["gate_id"] == gate_id)
    {
        ensure!(
            old["decision"] == decision && old["content_blob"] == json!(content_blob),
            "conflicting decision already recorded"
        );
        println!(
            "{}",
            json!({"recorded":true,"duplicate":true,"gate_id":gate_id})
        );
        return Ok(());
    }
    log.write("owner_decision",json!({"gate_id":gate_id,"decision":decision,"content_blob":content_blob,"artifacts":request.artifacts}),true)?;
    println!(
        "{}",
        json!({"recorded":true,"gate_id":gate_id,"next_command":format!("hx run --resume {}",path.display())})
    );
    Ok(())
}
