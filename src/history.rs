//! Root-lane compaction history. A compaction's checkpoint record carries an `Archive` naming the root checkpoint
//! before it, and the messages the compaction removed are that checkpoint's, up to the cut. Nothing else is stored:
//! `history(...)` folds the journal's checkpoint records when it is called.
use crate::{
    eventlog::EventLog,
    supervisor::{Checkpoint, CheckpointBody},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::Path, sync::Mutex};

/// The controller's request to record a compaction that replaced messages `1..cut-1` (0-based) with a summary.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Directive {
    pub kind: String,
    pub cut: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRef {
    pub seq: u64,
    pub blob: String,
}

/// What a compaction's checkpoint record stores under `archive`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub version: u32,
    pub source: CheckpointRef,
    pub cut: usize,
    pub stage_id: Option<String>,
}

/// The latest root checkpoint: the source the next compaction cuts from.
#[derive(Default)]
pub struct History {
    latest: Mutex<Option<CheckpointRef>>,
}

/// `history(id, {from = i, to = j})`: with no id, the list of compactions; with one, messages `from..=to` of it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Query {
    #[serde(skip_deserializing)]
    pub id: Option<usize>,
    pub from: Option<usize>,
    pub to: Option<usize>,
}

impl Query {
    pub fn parse(id: Option<Value>, range: Option<Value>) -> Result<Self> {
        let mut query: Self = range
            .map(serde_json::from_value)
            .transpose()
            .context("history: the range is {from = i, to = j}")?
            .unwrap_or_default();
        query.id = id
            .map(serde_json::from_value)
            .transpose()
            .context("history: the id is a number from history()")?;
        ensure!(
            query.id.is_some() || query.from.is_none() && query.to.is_none(),
            "history: a range needs a compaction id"
        );
        Ok(query)
    }
}

impl History {
    pub fn restore(records: &[Value]) -> Result<Self> {
        let mut latest = None;
        for checkpoint in root_checkpoints(records) {
            latest = Some(checkpoint?.0);
        }
        Ok(Self {
            latest: Mutex::new(latest),
        })
    }

    pub fn committed(&self, reference: CheckpointRef) {
        *self.latest.lock().unwrap() = Some(reference);
    }

    /// Check that `after` replaces the first `directive.cut - 1` messages of `before`, the conversation of the latest
    /// root checkpoint, with a summary and keeps the rest unchanged, and return the descriptor to commit with it.
    pub fn prepare(
        &self,
        directive: &Directive,
        before: &[Value],
        after: &Value,
        stage_id: Option<String>,
        dir: &Path,
    ) -> Result<Archive> {
        ensure!(
            directive.kind == "compaction",
            "history_archive_invalid: unknown directive"
        );
        let cut = directive.cut;
        ensure!(
            cut >= 3 && cut <= before.len(),
            "history_archive_invalid: cut out of range"
        );
        let messages = after["messages"]
            .as_array()
            .context("history_archive_invalid: missing messages")?;
        ensure!(
            messages.len() == before.len() - cut + 2 && messages[1..] == before[cut - 1..],
            "history_archive_invalid: retained suffix differs"
        );
        ensure!(
            before[cut - 1]["role"] == "assistant",
            "history_archive_invalid: cut splits a tool exchange"
        );
        ensure!(
            messages[0]["role"] == "user"
                && messages[0]["content"].is_string()
                && summary(after).is_some_and(|s| !s.trim().is_empty()),
            "history_archive_invalid: missing summary"
        );
        let source = self
            .latest
            .lock()
            .unwrap()
            .clone()
            .context("history_archive_invalid: no committed source")?;
        let saved = Checkpoint::read(dir, &source.blob)
            .context("history_archive_invalid: source unreadable")?;
        ensure!(
            saved
                .run
                .controller()
                .and_then(|c| c["messages"].as_array())
                .map(Vec::as_slice)
                == Some(before),
            "history_archive_invalid: source differs"
        );
        Ok(Archive {
            version: 1,
            source,
            cut,
            stage_id,
        })
    }
}

/// Answer `query` from the journal `log`: blocking file work, for a worker thread.
pub fn query(log: &EventLog, query: Query) -> Result<Value> {
    let dir = log.dir();
    let compactions = compactions(&crate::eventlog::load(&log.path)?)?;
    let Some(id) = query.id else {
        let list = compactions.iter().enumerate().map(|(i, (at, archive))| {
            let body = CheckpointBody::parse(&crate::eventlog::read_blob(dir, &at.blob)?)?;
            let text = body.controller().and_then(summary);
            let mut entry = json!({"id": i + 1, "messages": archive.cut - 2,
                "summary": text.context("history: compaction without a summary")?});
            if let Some(stage) = &archive.stage_id {
                entry["stage"] = json!(stage);
            }
            Ok(entry)
        });
        return fit(list, |_| {
            anyhow::anyhow!("history: the list of compactions is over hx.limits.DATA_BYTES")
        });
    };
    let (_, archive) = id
        .checked_sub(1)
        .and_then(|i| compactions.get(i))
        .with_context(|| {
            format!(
                "history: no compaction {id}; history() lists {}",
                compactions.len()
            )
        })?;
    let count = archive.cut - 2;
    let (from, to) = (
        query.from.unwrap_or(1),
        query.to.unwrap_or(count).min(count),
    );
    ensure!(
        (1..=to).contains(&from),
        "history: compaction {id} removed messages 1..{count}; from = {from} is outside them"
    );
    let source = Checkpoint::read(dir, &archive.source.blob)?;
    let messages = source
        .run
        .controller()
        .and_then(|c| c["messages"].as_array())
        .filter(|m| m.len() >= archive.cut)
        .context("history: the source checkpoint is shorter than the cut")?;
    fit(messages[from..=to].iter().map(render), |n| match n {
        0 => anyhow::anyhow!(
            "history: message {from} of compaction {id} alone is over hx.limits.DATA_BYTES"
        ),
        n => anyhow::anyhow!(
            "history: messages {from}..{to} of compaction {id} are over hx.limits.DATA_BYTES; read them in ranges, starting with history({id}, {{from = {from}, to = {}}})",
            from + n - 1
        ),
    })
}

/// The root checkpoint records of a journal, in order, with their archive descriptors.
fn root_checkpoints(
    records: &[Value],
) -> impl Iterator<Item = Result<(CheckpointRef, Option<Archive>)>> {
    records
        .iter()
        .filter(|r| r["kind"] == "checkpoint" && r["lane"] == "root")
        .map(|r| {
            let reference: CheckpointRef =
                serde_json::from_value(json!({"seq": r["seq"], "blob": r["blob"]}))
                    .context("history: malformed checkpoint record")?;
            let archive = r
                .get("archive")
                .map(|a| serde_json::from_value(a.clone()))
                .transpose()?;
            Ok((reference, archive))
        })
}

/// The recorded compactions, oldest first: each one's own checkpoint and its descriptor, which must name the root
/// checkpoint just before it.
fn compactions(records: &[Value]) -> Result<Vec<(CheckpointRef, Archive)>> {
    let mut previous: Option<CheckpointRef> = None;
    let mut out = vec![];
    for checkpoint in root_checkpoints(records) {
        let (reference, archive) = checkpoint?;
        if let Some(archive) = archive {
            ensure!(
                archive.version == 1 && archive.cut >= 3,
                "history: unsupported archive descriptor"
            );
            ensure!(
                previous
                    .as_ref()
                    .is_some_and(|p| p.seq == archive.source.seq && p.blob == archive.source.blob),
                "history: archive source is not the preceding root checkpoint"
            );
            out.push((reference.clone(), archive));
        }
        previous = Some(reference);
    }
    Ok(out)
}

/// `items` as one array, or `over(n)` when item `n` takes the array past the data boundary (`values::MAX_BYTES`).
fn fit(
    items: impl Iterator<Item = Result<Value>>,
    over: impl FnOnce(usize) -> anyhow::Error,
) -> Result<Value> {
    let mut limit = crate::values::Limit::default();
    let mut out = vec![];
    for (n, item) in items.enumerate() {
        let item = item?;
        if crate::values::validate_with_limit(&item, 1, &mut limit).is_err() {
            return Err(over(n));
        }
        out.push(item);
    }
    Ok(Value::Array(out))
}

pub(crate) fn summary(controller: &Value) -> Option<&str> {
    controller.get("worker").unwrap_or(controller)["previous_summary"].as_str()
}

/// A saved message as the model reads it back: role, text, and the code of its tool calls. Media payloads and
/// provider replay or reasoning fields are left out.
fn render(message: &Value) -> Result<Value> {
    let mut text = String::new();
    match &message["content"] {
        Value::String(s) => text.push_str(s),
        Value::Null => {}
        Value::Array(parts) => {
            for p in parts {
                text.push_str(p["text"].as_str().unwrap_or("\n[media omitted]\n"));
            }
        }
        _ => bail!("history: invalid message content"),
    }
    for image in message["images"].as_array().into_iter().flatten() {
        let dim = |k: &str| image[k].as_u64().map_or("?".to_string(), |n| n.to_string());
        let path = image["path"].as_str().unwrap_or("?");
        text.push_str(&format!(
            "\n[image {path} {}x{} not kept; view() it again to look]\n",
            dim("width"),
            dim("height")
        ));
    }
    let mut out = json!({
        "role": message["role"].as_str().context("history: message without a role")?,
        "text": text,
    });
    if let Some(calls) = message["calls"].as_array().filter(|c| !c.is_empty()) {
        let code: Vec<String> = calls
            .iter()
            .map(|call| match call["input"]["code"].as_str() {
                Some(code) => code.to_string(),
                None => call["input"].to_string(),
            })
            .collect();
        out["code"] = json!(code.join("\n"));
    }
    Ok(out)
}

/// `history(id?, range?)`, in the root agent's session only: the compactions so far, read from the journal.
pub fn install(cx: &crate::vm::Context) -> Result<()> {
    use crate::{effects::Ret, policy::Class, prims};
    use std::sync::atomic::Ordering;
    let Some(h) = cx.host.cloned().filter(|h| h.depth == 0) else {
        return Ok(());
    };
    let hfx = cx.fx.clone();
    let hops = cx.ops.clone();
    cx.lua.globals().set(
        "history",
        cx.lua.create_async_function(
            move |lua, (id, range): (Option<mlua::Value>, Option<mlua::Value>)| {
                let (h, fx, ops) = (h.clone(), hfx.clone(), hops.clone());
                async move {
                    let id = id.map(crate::values::copy_out).transpose()?;
                    let range = range.map(crate::values::copy_out).transpose()?;
                    let query = Query::parse(id, range).map_err(mlua::Error::external)?;
                    let args = json!(query);
                    ops.fetch_add(1, Ordering::Relaxed);
                    let result = fx
                        .run(
                            &lua,
                            crate::supervisor::Tool::History,
                            Class::Read,
                            args,
                            || async move {
                                let value = tokio::task::spawn_blocking(move || {
                                    crate::history::query(&h.policy.log, query)
                                })
                                .await
                                .map_err(mlua::Error::external)?
                                .map_err(mlua::Error::external)?;
                                Ok(Ret::Json(value))
                            },
                        )
                        .await?;
                    prims::json_value(&lua, result)
                }
            },
        )?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Fixture {
        dir: std::path::PathBuf,
        log: Arc<EventLog>,
        history: History,
        messages: Vec<Value>,
    }

    impl Fixture {
        fn new(messages: Vec<Value>) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "hx-history-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let log = EventLog::open(&dir).unwrap();
            let f = Self {
                dir,
                log,
                history: History::default(),
                messages,
            };
            f.save(json!({"messages": f.messages}), None);
            f
        }

        fn save(&self, controller: Value, archive: Option<Archive>) {
            let blob = Checkpoint {
                run: crate::supervisor::RunState::Running { controller },
                durable: json!({}),
                workflow: None,
            }
            .write(&self.log)
            .unwrap();
            let mut fields = json!({"lane": "root", "blob": blob});
            if let Some(a) = &archive {
                fields["archive"] = json!(a);
            }
            let seq = self.log.write("checkpoint", fields, true).unwrap();
            self.history.committed(CheckpointRef { seq, blob });
        }

        fn compact(&mut self, cut: usize, summary: &str) -> Result<()> {
            let mut next = vec![json!({"role": "user", "content": format!("task + {summary}")})];
            next.extend_from_slice(&self.messages[cut - 1..]);
            let state = json!({"messages": next, "previous_summary": summary});
            let directive = Directive {
                kind: "compaction".into(),
                cut,
            };
            let archive =
                self.history
                    .prepare(&directive, &self.messages, &state, None, &self.dir)?;
            self.save(state, Some(archive));
            self.messages = next;
            Ok(())
        }

        fn query(&self, id: Option<usize>, range: Value) -> Result<Value> {
            query(&self.log, Query::parse(id.map(|i| json!(i)), Some(range))?)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn message(role: &str, text: &str) -> Value {
        json!({"role": role, "content": text})
    }

    fn conversation(text: &str) -> Vec<Value> {
        vec![
            message("user", "task"),
            message("assistant", "run test"),
            message("tool", text),
            message("assistant", "next"),
            message("tool", "ok"),
        ]
    }

    #[test]
    fn compactions_list_and_read_their_removed_messages_after_a_restore() {
        let mut f = Fixture::new(conversation("first output"));
        assert_eq!(f.query(None, json!({})).unwrap(), json!([]));
        f.compact(4, "summary one").unwrap();
        f.messages.extend([
            message("assistant", "last"),
            message("tool", "second output"),
        ]);
        f.save(
            json!({"messages": f.messages, "previous_summary": "summary one"}),
            None,
        );
        f.compact(4, "summary two").unwrap();
        assert_eq!(
            f.query(None, json!({})).unwrap(),
            json!([
                {"id": 1, "messages": 2, "summary": "summary one"},
                {"id": 2, "messages": 2, "summary": "summary two"},
            ])
        );
        assert_eq!(
            f.query(Some(1), json!({})).unwrap(),
            json!([{"role": "assistant", "text": "run test"}, {"role": "tool", "text": "first output"}])
        );
        // The second compaction cut after its wrapped first message, which belongs to the first compaction.
        assert_eq!(
            f.query(Some(2), json!({"from": 2})).unwrap(),
            json!([{"role": "tool", "text": "ok"}])
        );
        let records = crate::eventlog::load(&f.log.path).unwrap();
        let latest = History::restore(&records)
            .unwrap()
            .latest
            .into_inner()
            .unwrap();
        assert_eq!(latest.unwrap().seq, records.last().unwrap()["seq"]);
        for (id, range) in [
            (0, json!({})),
            (3, json!({})),
            (1, json!({"from": 3})),
            (1, json!({"to": 0})),
        ] {
            assert!(f.query(Some(id), range).is_err());
        }
        assert!(f.query(None, json!({"from": 1})).is_err());
        assert!(f.query(Some(1), json!({"bytes": 1})).is_err());
    }

    #[test]
    fn a_range_over_the_data_boundary_names_the_range_that_fits() {
        // A checkpoint's conversation is itself held to the data boundary, but rendering a non-code tool input as JSON
        // escapes it again: two calls that fit in the conversation do not fit in one answer.
        let quotes = "\"".repeat(3 << 20);
        let call = json!({"role": "assistant", "content": "",
            "calls": [{"name": "other", "id": "c", "input": {"s": quotes}}]});
        let messages = vec![
            message("user", "task"),
            call.clone(),
            message("tool", "ok"),
            call,
            message("tool", "ok"),
            message("assistant", "keep"),
            message("tool", "keep"),
        ];
        let mut f = Fixture::new(messages);
        f.compact(6, "summary").unwrap();
        let error = f.query(Some(1), json!({})).unwrap_err().to_string();
        assert!(error.contains("history(1, {from = 1, to = 2})"), "{error}");
        let rest = f.query(Some(1), json!({"from": 3})).unwrap();
        assert_eq!(rest.as_array().map(Vec::len), Some(2));
        assert!(rest[0]["code"].as_str().unwrap().len() > 2 * quotes.len());
    }

    #[test]
    fn validation_rejects_bad_transitions_and_private_fields_are_not_rendered() {
        let mut f = Fixture::new(conversation("result"));
        let state = json!({"messages": [message("user", "summary"), message("assistant", "wrong")], "previous_summary": "summary"});
        let directive = Directive {
            kind: "compaction".into(),
            cut: 4,
        };
        assert!(
            f.history
                .prepare(&directive, &f.messages, &state, None, &f.dir)
                .is_err()
        );
        assert!(f.compact(2, "summary").is_err());
        let public = render(&json!({"role": "assistant",
            "content": [{"type": "text", "text": "visible"}, {"type": "image", "data": "SECRET"}],
            "raw": "PRIVATE", "images": [{"data": "SECRET", "path": "f.png", "width": 10, "height": 5}],
            "calls": [{"name": "luau", "id": "c", "input": {"code": "print(42)"}}]}))
        .unwrap();
        assert_eq!(public["code"], "print(42)");
        let text = public["text"].as_str().unwrap();
        assert!(text.contains("visible") && text.contains("[image f.png 10x5 not kept"));
        assert!(!public.to_string().contains("PRIVATE") && !public.to_string().contains("SECRET"));
    }

    #[test]
    fn a_missing_or_corrupt_source_fails_explicitly() {
        let mut f = Fixture::new(conversation("original"));
        f.compact(4, "brief").unwrap();
        let records = crate::eventlog::load(&f.log.path).unwrap();
        let source = records
            .iter()
            .find_map(|r| r["archive"]["source"]["blob"].as_str())
            .unwrap();
        let path = f.dir.join("blobs").join(source);
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"corrupted").unwrap();
        assert!(
            format!("{:#}", f.query(Some(1), json!({})).unwrap_err()).contains("hash mismatch")
        );
        std::fs::remove_file(&path).unwrap();
        assert!(f.query(Some(1), json!({})).is_err());
        std::fs::write(&path, saved).unwrap();
        assert_eq!(f.query(Some(1), json!({})).unwrap()[1]["text"], "original");
    }
}
