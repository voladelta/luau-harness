//! Root-lane compacted context. Bodies stay in immutable checkpoints; only references are indexed.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    io::Read,
    path::Path,
    sync::{Arc, Mutex},
};

const MAX_SOURCE: usize = 64 << 20;
const CACHE_BYTES: usize = 8 << 20;
const MAX_RESPONSE: usize = 12 << 10;
const MAX_TEXT: usize = 4096;
const MAX_SCAN: usize = 128;

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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub version: u32,
    pub source: CheckpointRef,
    pub cut: usize,
    pub stage_id: Option<String>,
}

#[derive(Clone)]
struct Record {
    seq: u64,
    blob: String,
    archive: Archive,
}

#[derive(Default)]
struct Index {
    latest: Option<CheckpointRef>,
    records: Vec<Record>,
    cache: VecDeque<(String, Arc<Value>, usize)>,
}

#[derive(Default)]
pub struct History {
    index: Mutex<Index>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    q: Option<String>,
    before: Option<String>,
    limit: Option<usize>,
    id: Option<String>,
    offset: Option<usize>,
    bytes: Option<usize>,
}

impl Query {
    pub fn parse(value: Value) -> Result<Self> {
        let q: Self = serde_json::from_value(if value.is_null() { json!({}) } else { value })?;
        if q.id.is_some() {
            ensure!(
                q.q.is_none() && q.before.is_none() && q.limit.is_none(),
                "history: mixed read/list options"
            );
            ensure!(
                (1..=MAX_TEXT).contains(&q.bytes.unwrap_or(MAX_TEXT)),
                "history: bytes must be 1..4096"
            );
        } else {
            ensure!(
                q.offset.is_none() && q.bytes.is_none(),
                "history: offset/bytes require id"
            );
            ensure!(
                (1..=20).contains(&q.limit.unwrap_or(5)),
                "history: limit must be 1..20"
            );
            if let Some(text) = &q.q {
                ensure!(
                    !text.is_empty() && text.len() <= 1024,
                    "history: q must be 1..1024 bytes"
                );
            }
        }
        Ok(q)
    }
}

impl History {
    pub fn restore(records: &[Value]) -> Result<Self> {
        let history = Self::default();
        for r in records {
            if r["kind"] != "checkpoint" || r["lane"] != "root" {
                continue;
            }
            let reference = CheckpointRef {
                seq: r["seq"]
                    .as_u64()
                    .context("history: checkpoint sequence missing")?,
                blob: r["blob"]
                    .as_str()
                    .context("history: checkpoint blob missing")?
                    .into(),
            };
            let archive = r
                .get("archive")
                .map(|a| serde_json::from_value(a.clone()))
                .transpose()?;
            history.committed(reference, archive)?;
        }
        Ok(history)
    }

    pub fn committed(&self, reference: CheckpointRef, archive: Option<Archive>) -> Result<()> {
        let mut index = self.index.lock().unwrap();
        if let Some(archive) = archive {
            ensure!(
                archive.version == 1 && archive.cut >= 3,
                "history: unsupported archive descriptor"
            );
            let source = index
                .latest
                .as_ref()
                .context("history: archive has no source checkpoint")?;
            ensure!(
                archive.source.seq == source.seq && archive.source.blob == source.blob,
                "history: archive source is not the preceding root checkpoint"
            );
            index.records.push(Record {
                seq: reference.seq,
                blob: reference.blob.clone(),
                archive,
            });
        }
        index.latest = Some(reference);
        Ok(())
    }

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
        let mut index = self.index.lock().unwrap();
        let source = index
            .latest
            .clone()
            .context("history_archive_invalid: no committed source")?;
        let mut budget = MAX_SOURCE;
        let saved = load(&mut index, dir, &source.blob, &mut budget, None)?;
        ensure!(
            saved["controller"]["messages"]
                .as_array()
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

    pub fn query(
        &self,
        dir: &Path,
        run: &str,
        query: Query,
        deadline: Option<f64>,
    ) -> Result<Value> {
        check_deadline(deadline)?;
        let mut index = self.index.lock().unwrap();
        if let Some(id) = &query.id {
            let (seq, ordinal) = parse_id(id)?;
            let pos = index
                .records
                .binary_search_by_key(&seq, |r| r.seq)
                .map_err(|_| anyhow::anyhow!("history: unknown or out-of-scope id"))?;
            let record = index.records[pos].clone();
            ensure!(
                ordinal == 0 || (2..record.archive.cut).contains(&ordinal),
                "history: unknown id"
            );
            let mut budget = MAX_SOURCE;
            let (role, text) = entry(&mut index, &record, ordinal, dir, &mut budget, deadline)?;
            let offset = query.offset.unwrap_or(0);
            ensure!(
                offset <= text.len() && text.is_char_boundary(offset),
                "history: invalid UTF-8 offset"
            );
            let mut end = floor_boundary(
                &text,
                offset
                    .saturating_add(query.bytes.unwrap_or(MAX_TEXT))
                    .min(text.len()),
            );
            ensure!(
                end > offset || offset == text.len(),
                "history: bytes too small for next character"
            );
            loop {
                let mut out = json!({"run":run,"lane":"root","id":id,"kind":if ordinal==0 {"compaction"} else {"message"},
                    "role":role,"representation":"public-transcript-v1","bytes":text.len(),"offset":offset,
                    "text":&text[offset..end],"truncated":end<text.len()});
                if end < text.len() {
                    out["next_offset"] = json!(end);
                }
                if serde_json::to_vec(&out)?.len() <= MAX_RESPONSE {
                    return Ok(out);
                }
                ensure!(end > offset, "history: response metadata too large");
                end = floor_boundary(&text, offset + (end - offset) / 2);
                ensure!(
                    end > offset || offset == text.len(),
                    "history: response cannot fit next character"
                );
            }
        }
        let last = index.records.last().map_or(0, |r| r.seq);
        let (frontier, mut seq, mut ordinal) = if let Some(cursor) = &query.before {
            decode_cursor(cursor, run, query.q.as_deref())?
        } else {
            (last, last, 0)
        };
        ensure!(frontier <= last, "history: cursor is ahead of archive");
        let mut out = json!({"run":run,"lane":"root","coverage":"recorded_compactions_only",
            "status":if last==0 {"unavailable"} else {"ok"},"entries":[],"scan_complete":true});
        if last == 0 {
            out["reason"] = json!("no_supported_archive_records");
            return Ok(out);
        }
        let mut entries = Vec::new();
        let mut examined = 0;
        let mut budget = MAX_SOURCE;
        while seq != 0 && entries.len() < query.limit.unwrap_or(5) && examined < MAX_SCAN {
            check_deadline(deadline)?;
            let pos = index
                .records
                .binary_search_by_key(&seq, |r| r.seq)
                .map_err(|_| anyhow::anyhow!("history: invalid cursor position"))?;
            ensure!(seq <= frontier, "history: invalid cursor frontier");
            let record = index.records[pos].clone();
            ensure!(
                ordinal == 0 || (2..record.archive.cut).contains(&ordinal),
                "history: invalid cursor ordinal"
            );
            // Never consume a candidate when its source will exceed this call's work budget.
            let blob = if ordinal == 0 {
                &record.blob
            } else {
                &record.archive.source.blob
            };
            let cost = std::fs::metadata(dir.join("blobs").join(blob))?.len() as usize;
            ensure!(cost <= MAX_SOURCE, "history: source read limit exceeded");
            if cost > budget {
                break;
            }
            let (role, text) = entry(&mut index, &record, ordinal, dir, &mut budget, deadline)?;
            let old_position = (seq, ordinal);
            let current = ordinal;
            if ordinal == 0 {
                ordinal = record.archive.cut - 1;
            } else if ordinal > 2 {
                ordinal -= 1;
            } else {
                seq = if pos > 0 {
                    index.records[pos - 1].seq
                } else {
                    0
                };
                ordinal = 0;
            }
            examined += 1;
            let found = match &query.q {
                Some(q) => text.find(q),
                None => Some(0),
            };
            if let Some(at) = found {
                let start = floor_boundary(&text, at.saturating_sub(80));
                let end = floor_boundary(&text, (start + 200).min(text.len()));
                let candidate = json!({"id":entry_id(record.seq,current),"kind":if current==0 {"compaction"} else {"message"},
                    "role":role,"stage_id":record.archive.stage_id,"bytes":text.len(),"snippet":&text[start..end],
                    "snippet_offset":start,"truncated":start>0 || end<text.len()});
                if serde_json::to_vec(&entries)?.len() + serde_json::to_vec(&candidate)?.len()
                    > MAX_RESPONSE - 1024
                {
                    (seq, ordinal) = old_position;
                    break;
                }
                entries.push(candidate);
            }
        }
        entries.reverse();
        out["entries"] = json!(entries);
        if seq != 0 {
            out["next"] = json!(encode_cursor(
                frontier,
                seq,
                ordinal,
                run,
                query.q.as_deref()
            ));
            out["scan_complete"] = json!(false);
        }
        ensure!(
            serde_json::to_vec(&out)?.len() <= MAX_RESPONSE,
            "history: response exceeds limit; request fewer entries"
        );
        Ok(out)
    }
}

pub(crate) fn summary(controller: &Value) -> Option<&str> {
    controller.get("worker").unwrap_or(controller)["previous_summary"].as_str()
}

fn check_deadline(deadline: Option<f64>) -> Result<()> {
    ensure!(
        deadline.is_none_or(|d| crate::supervisor::now() < d),
        "history: run deadline reached"
    );
    Ok(())
}

fn load(
    index: &mut Index,
    dir: &Path,
    hash: &str,
    budget: &mut usize,
    deadline: Option<f64>,
) -> Result<Arc<Value>> {
    check_deadline(deadline)?;
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "history: invalid blob hash"
    );
    let size = std::fs::metadata(dir.join("blobs").join(hash))?.len() as usize;
    ensure!(
        size <= MAX_SOURCE && size <= *budget,
        "history: source read limit exceeded"
    );
    *budget -= size;
    if let Some(pos) = index.cache.iter().position(|(h, _, _)| h == hash) {
        let item = index.cache.remove(pos).unwrap();
        let value = item.1.clone();
        index.cache.push_back(item);
        return Ok(value);
    }
    let mut file = std::fs::File::open(dir.join("blobs").join(hash))?;
    let mut data = Vec::with_capacity(size.min(1 << 20));
    let mut chunk = [0u8; 65536];
    loop {
        check_deadline(deadline)?;
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        ensure!(
            data.len() + n <= MAX_SOURCE,
            "history: source read limit exceeded"
        );
        data.extend_from_slice(&chunk[..n]);
    }
    ensure!(
        crate::eventlog::sha_bytes(&data) == hash,
        "history: content hash mismatch"
    );
    let value: Arc<Value> =
        Arc::new(serde_json::from_slice(&data).context("history: invalid checkpoint JSON")?);
    check_deadline(deadline)?;
    if data.len() <= CACHE_BYTES {
        while index.cache.iter().map(|(_, _, n)| n).sum::<usize>() + data.len() > CACHE_BYTES {
            index.cache.pop_front();
        }
        index
            .cache
            .push_back((hash.into(), value.clone(), data.len()));
    }
    Ok(value)
}

fn entry(
    index: &mut Index,
    record: &Record,
    ordinal: usize,
    dir: &Path,
    budget: &mut usize,
    deadline: Option<f64>,
) -> Result<(String, String)> {
    let hash = if ordinal == 0 {
        &record.blob
    } else {
        &record.archive.source.blob
    };
    let saved = load(index, dir, hash, budget, deadline)?;
    if ordinal == 0 {
        return Ok((
            "summary".into(),
            summary(&saved["controller"])
                .context("history: missing summary")?
                .into(),
        ));
    }
    let message = saved["controller"]["messages"]
        .get(ordinal - 1)
        .context("history: missing archived message")?;
    Ok((
        message["role"]
            .as_str()
            .context("history: invalid message role")?
            .into(),
        render(message)?,
    ))
}

fn render(message: &Value) -> Result<String> {
    let mut text = String::new();
    match &message["content"] {
        Value::String(s) => text.push_str(s),
        Value::Null => {}
        Value::Array(parts) => {
            for p in parts {
                if let Some(s) = p["text"].as_str() {
                    text.push_str(s);
                } else {
                    text.push_str("\n[media omitted]\n");
                }
            }
        }
        _ => bail!("history: invalid public content"),
    }
    for call in message["calls"].as_array().into_iter().flatten() {
        text.push_str(&format!(
            "\n[tool call {} id={}]\n",
            call["name"].as_str().unwrap_or("unknown"),
            call["id"].as_str().unwrap_or("unknown")
        ));
        if let Some(code) = call["input"]["code"].as_str() {
            if let Some(description) = call["input"]["description"].as_str() {
                text.push_str(description);
                text.push('\n');
            }
            text.push_str(code);
        } else {
            text.push_str(&serde_json::to_string(&call["input"])?);
        }
    }
    if message["images"].as_array().is_some_and(|a| !a.is_empty()) {
        text.push_str("\n[media omitted]\n");
    }
    Ok(text)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}
fn entry_id(seq: u64, ordinal: usize) -> String {
    if ordinal == 0 {
        format!("h:{seq}:c")
    } else {
        format!("h:{seq}:m:{ordinal}")
    }
}
fn parse_id(id: &str) -> Result<(u64, usize)> {
    let parts: Vec<_> = id.split(':').collect();
    match parts.as_slice() {
        ["h", seq, "c"] => Ok((seq.parse()?, 0)),
        ["h", seq, "m", ordinal] => {
            let n = ordinal.parse()?;
            ensure!(n >= 2, "history: invalid message id");
            Ok((seq.parse()?, n))
        }
        _ => bail!("history: invalid id"),
    }
}

fn fingerprint(run: &str, q: Option<&str>) -> String {
    crate::eventlog::sha_bytes(&serde_json::to_vec(&(run, "root", q)).unwrap())[..32].into()
}
fn encode_cursor(frontier: u64, seq: u64, ordinal: usize, run: &str, q: Option<&str>) -> String {
    format!("1:{frontier:x}:{seq:x}:{ordinal:x}:{}", fingerprint(run, q))
}
fn decode_cursor(cursor: &str, run: &str, q: Option<&str>) -> Result<(u64, u64, usize)> {
    ensure!(cursor.len() <= 96, "history: invalid cursor");
    let parts: Vec<_> = cursor.split(':').collect();
    let ["1", frontier, seq, ordinal, hash] = parts.as_slice() else {
        bail!("history: invalid cursor")
    };
    ensure!(
        *hash == fingerprint(run, q),
        "history: cursor run/query mismatch; resend q"
    );
    Ok((
        u64::from_str_radix(frontier, 16)?,
        u64::from_str_radix(seq, 16)?,
        usize::from_str_radix(ordinal, 16)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eventlog::EventLog;
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
            f.save(json!({"messages":f.messages}), None);
            f
        }
        fn save(&self, controller: Value, archive: Option<Archive>) {
            let blob = self
                .log
                .blob(&serde_json::to_vec(&json!({"controller":controller})).unwrap())
                .unwrap();
            let mut fields = json!({"lane":"root","blob":blob});
            if let Some(a) = &archive {
                fields["archive"] = json!(a);
            }
            let seq = self.log.write("checkpoint", fields, true).unwrap();
            self.history
                .committed(CheckpointRef { seq, blob }, archive)
                .unwrap();
        }
        fn compact(&mut self, cut: usize, summary: &str) {
            let mut next = vec![json!({"role":"user","content":format!("task + {summary}")})];
            next.extend_from_slice(&self.messages[cut - 1..]);
            let state = json!({"messages":next,"previous_summary":summary});
            let archive = self
                .history
                .prepare(
                    &Directive {
                        kind: "compaction".into(),
                        cut,
                    },
                    &self.messages,
                    &state,
                    Some("implementation-1".into()),
                    &self.dir,
                )
                .unwrap();
            self.save(state, Some(archive));
            self.messages = next;
        }
        fn query(&self, q: Value) -> Result<Value> {
            self.history.query(&self.dir, "run", Query::parse(q)?, None)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn message(role: &str, text: &str) -> Value {
        json!({"role":role,"content":text})
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
    fn repeated_compactions_preserve_occurrences_and_restore_index() {
        let mut f = Fixture::new(conversation("same exact text"));
        assert_eq!(f.query(json!({})).unwrap()["status"], "unavailable");
        f.compact(4, "summary one");
        let pinned = f.query(json!({"limit":1})).unwrap();
        f.messages[2]["content"] = json!("same exact text");
        f.messages
            .extend([message("assistant", "last"), message("tool", "done")]);
        f.save(
            json!({"messages":f.messages,"previous_summary":"summary one"}),
            None,
        );
        f.compact(4, "summary two");
        let remaining = f
            .query(json!({"before":pinned["next"],"limit":20}))
            .unwrap();
        assert_eq!(remaining["entries"].as_array().unwrap().len(), 2);
        assert!(remaining.get("next").is_none());
        let hits = f.query(json!({"q":"same exact text"})).unwrap();
        let entries = hits["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_ne!(entries[0]["id"], entries[1]["id"]);
        let all = f.query(json!({"limit":20})).unwrap();
        assert_eq!(all["entries"].as_array().unwrap().len(), 6);
        let restored = History::restore(&crate::eventlog::load(&f.log.path).unwrap()).unwrap();
        assert_eq!(
            restored
                .query(
                    &f.dir,
                    "run",
                    Query::parse(json!({"q":"same exact text"})).unwrap(),
                    None
                )
                .unwrap(),
            hits
        );
    }

    #[test]
    fn large_utf8_and_escaped_entries_are_reconstructed_with_bounded_pages() {
        let text = "é🙂\n\u{1}\"\\".repeat(3000);
        let mut f = Fixture::new(conversation(&text));
        f.compact(4, "brief");
        let hits = f.query(json!({"q":"é🙂"})).unwrap();
        let id = hits["entries"][0]["id"].as_str().unwrap();
        let mut output = String::new();
        let mut offset = 0;
        loop {
            let page = f.query(json!({"id":id,"offset":offset})).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_RESPONSE);
            let part = page["text"].as_str().unwrap();
            assert!(part.len() <= MAX_TEXT);
            output.push_str(part);
            if let Some(next) = page["next_offset"].as_u64() {
                assert!(next as usize > offset);
                offset = next as usize;
            } else {
                break;
            }
        }
        assert_eq!(output, text);
        assert!(f.query(json!({"id":id,"offset":1})).is_err());
        assert!(f.query(json!({"id":id,"bytes":1})).is_err());
        assert_eq!(
            f.query(json!({"id":id,"offset":text.len()})).unwrap()["text"],
            ""
        );
    }

    #[test]
    fn search_continuations_progress_and_bind_query_and_run() {
        let mut messages = vec![message("user", "task")];
        for i in 0..150 {
            messages.push(message("assistant", "call"));
            messages.push(message("tool", if i == 0 { "needle" } else { "ordinary" }));
        }
        messages.extend([message("assistant", "keep"), message("tool", "keep")]);
        let cut = messages.len() - 1;
        let mut f = Fixture::new(messages);
        f.compact(cut, "summary");
        let mut q = json!({"q":"needle"});
        let mut found = 0;
        let mut pages = 0;
        loop {
            let page = f.query(q.clone()).unwrap();
            pages += 1;
            found += page["entries"].as_array().unwrap().len();
            if let Some(cursor) = page["next"].as_str() {
                assert!(cursor.len() <= 96);
                assert!(f.query(json!({"q":"other","before":cursor})).is_err());
                q["before"] = json!(cursor);
                let restored =
                    History::restore(&crate::eventlog::load(&f.log.path).unwrap()).unwrap();
                assert!(
                    restored
                        .query(
                            &f.dir,
                            "another-run",
                            Query::parse(q.clone()).unwrap(),
                            None
                        )
                        .is_err()
                );
                assert_eq!(
                    restored
                        .query(&f.dir, "run", Query::parse(q.clone()).unwrap(), None)
                        .unwrap(),
                    f.query(q.clone()).unwrap()
                );
            } else {
                break;
            }
            assert!(pages < 5);
        }
        assert!(pages >= 3);
        assert_eq!(found, 1);
    }

    #[test]
    fn validation_rejects_bad_transitions_and_private_fields_are_not_rendered() {
        let f = Fixture::new(conversation("result"));
        let state = json!({"messages":[message("user","summary"),message("assistant","wrong")],"previous_summary":"summary"});
        assert!(
            f.history
                .prepare(
                    &Directive {
                        kind: "compaction".into(),
                        cut: 4
                    },
                    &f.messages,
                    &state,
                    None,
                    &f.dir
                )
                .is_err()
        );
        let public=render(&json!({"content":[{"type":"text","text":"visible"},{"type":"image","data":"SECRET"}],
            "raw":"PRIVATE","images":[{"data":"SECRET"}],"calls":[{"name":"luau","id":"c","input":{"code":"print(42)"}}]})).unwrap();
        assert!(public.contains("visible") && public.contains("print(42)"));
        assert!(!public.contains("PRIVATE") && !public.contains("SECRET"));
        for q in [
            json!({"q":""}),
            json!({"limit":0}),
            json!({"id":"x","q":"q"}),
            json!({"run":"other"}),
        ] {
            assert!(Query::parse(q).is_err());
        }
    }

    #[test]
    fn missing_corrupt_oversized_sources_and_expired_reads_fail_explicitly() {
        let mut f = Fixture::new(conversation("original"));
        f.compact(4, "brief");
        let page = f.query(json!({"q":"original"})).unwrap();
        let id = page["entries"][0]["id"].as_str().unwrap();
        let records = crate::eventlog::load(&f.log.path).unwrap();
        let source = records
            .iter()
            .find_map(|r| r["archive"]["source"]["blob"].as_str())
            .unwrap();
        let path = f.dir.join("blobs").join(source);
        let saved = std::fs::read(&path).unwrap();
        let read = || {
            History::restore(&records).unwrap().query(
                &f.dir,
                "run",
                Query::parse(json!({"id":id})).unwrap(),
                None,
            )
        };
        std::fs::write(&path, b"corrupted").unwrap();
        assert!(read().unwrap_err().to_string().contains("hash mismatch"));
        std::fs::remove_file(&path).unwrap();
        assert!(read().is_err());
        let sparse = std::fs::File::create(&path).unwrap();
        sparse.set_len(MAX_SOURCE as u64 + 1).unwrap();
        assert!(
            read()
                .unwrap_err()
                .to_string()
                .contains("source read limit")
        );
        drop(sparse);
        std::fs::write(&path, saved).unwrap();
        assert_eq!(read().unwrap()["text"], "original");
        assert!(
            f.history
                .query(
                    &f.dir,
                    "run",
                    Query::parse(json!({})).unwrap(),
                    Some(crate::supervisor::now() - 1.0)
                )
                .unwrap_err()
                .to_string()
                .contains("deadline")
        );
        assert!(f.query(json!({"id":"h:999999:m:2"})).is_err());
    }
}
