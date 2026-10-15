//! Append-only, hash-chained event log: the source of truth for what the model did.
//!
//! One JSON object per line: { seq, ts_ms, session, kind, prev, ... }. `prev` is the SHA-256 of the previous line, so
//! editing or deleting any earlier line breaks the chain (`hx log verify`). Effects that change the world are written
//! write-ahead: an `intent` record (flushed and synced) BEFORE the effect, an `outcome` record (`of` = the intent's
//! seq) after. An intent with no outcome is an UNKNOWN outcome: the process died mid-effect, and a human decides.
use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct Inner {
    file: Option<std::fs::File>,
    seq: u64,
    prev: String,
}

pub struct EventLog {
    inner: Mutex<Inner>,
    failure: Mutex<Option<String>>,
    pub session: String,
    pub path: Option<PathBuf>,
    /// test hook: SIGKILL this process right after the Nth record is on disk (HX_TEST_CRASH_AFTER_RECORD)
    crash_after: Option<u64>,
    // A separate inode survives torn-tail repair. The lock is released only when the run drops its journal.
    _writer_lock: Option<std::fs::File>,
    owner_reconciliation: bool,
}

fn lock_writer(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .custom_flags(libc::O_NOFOLLOW).open(path.with_extension("lock"))?;
    anyhow::ensure!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "run is already open by another process: {}", path.display());
    Ok(lock)
}

pub fn sha_bytes(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|b| format!("{b:02x}")).collect()
}

impl EventLog {
    pub fn open(dir: &Path) -> Result<Arc<EventLog>> {
        std::fs::create_dir_all(dir)?;
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?;
        // `{secs}-{pid}`, plus `-N` when another session of this process opened one in the same second
        let base = format!("{}-{}", ts.as_secs(), std::process::id());
        let mut n = 0;
        let (session, path, file) = loop {
            let session = if n == 0 { base.clone() } else { format!("{base}-{n}") };
            let path = dir.join(format!("{session}.jsonl"));
            match std::fs::OpenOptions::new().create_new(true).append(true).open(&path) {
                Ok(f) => break (session, path, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
                Err(e) => return Err(e.into()),
            }
        };
        let _writer_lock = Some(lock_writer(&path)?);
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file: Some(file), seq: 0, prev: "genesis".into() }), failure: Mutex::new(None), session, path: Some(path), crash_after: crash_hook(), _writer_lock, owner_reconciliation: false }))
    }

    /// Continue an existing log after a crash: restore seq and the hash chain, drop a torn final line. A malformed line
    /// before the end is not a crash: the log is refused, never truncated (the records after it are evidence).
    pub fn reopen(path: &Path) -> Result<Arc<EventLog>> {
        Self::reopen_mode(path, false)
    }

    /// Owner CLI only: revocation still blocks execution, but does not erase the ability to reconcile receipts.
    pub fn reconcile(path: &Path) -> Result<Arc<EventLog>> {
        Self::reopen_mode(path, true)
    }

    fn reopen_mode(path: &Path, owner_reconciliation: bool) -> Result<Arc<EventLog>> {
        let _writer_lock = Some(lock_writer(path)?);
        let text = read_text(path)?;
        let report = verify_text(&text)?;
        anyhow::ensure!(report.chain_ok, "{}: hash chain broken at line {}; not resuming a damaged log", path.display(), report.bad_line.unwrap_or(0));
        let (records, torn) = parse(&text).map_err(|n| anyhow::anyhow!("{}: line {n} is not valid JSON; not resuming a damaged log", path.display()))?;
        let (mut seq, mut prev) = (0u64, "genesis".to_string());
        for (line, v) in &records {
            seq = v["seq"].as_u64().unwrap_or(seq);
            prev = sha_bytes(line.as_bytes());
        }
        // a torn tail is dropped; a complete final record that lost its newline gets one, so appends start a new line
        if torn || !text.is_empty() && !text.ends_with('\n') {
            let keep: String = records.iter().map(|(l, _)| format!("{l}\n")).collect();
            write_atomic(path, keep.as_bytes())?;
        }
        let file = std::fs::OpenOptions::new().append(true).open(path)?;
        let session = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file: Some(file), seq, prev }), failure: Mutex::new(None), session, path: Some(path.to_path_buf()), crash_after: crash_hook(), _writer_lock, owner_reconciliation }))
    }

    pub fn off() -> Arc<EventLog> {
        Arc::new(EventLog { inner: Mutex::new(Inner { file: None, seq: 0, prev: String::new() }), failure: Mutex::new(None), session: "off".into(), path: None, crash_after: None, _writer_lock: None, owner_reconciliation: false })
    }

    /// Audit failures are fatal to the run, even if Luau catches the returned error.
    pub fn fail(&self, message: impl std::fmt::Display) -> anyhow::Error {
        let mut failure = self.failure.lock().unwrap();
        let message = failure.get_or_insert_with(|| message.to_string()).clone();
        anyhow::anyhow!(message)
    }

    pub fn check(&self) -> Result<()> {
        if !self.owner_reconciliation && self.path.as_ref().is_some_and(|p| p.with_extension("revoked").exists()) {
            return Err(self.fail("run revoked by its owner"));
        }
        match self.failure.lock().unwrap().as_ref() {
            Some(message) => anyhow::bail!("{message}"),
            None => Ok(()),
        }
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf))
    }


    /// Store `bytes` content-addressed next to the log (write-once) and return its sha256.
    pub fn blob(&self, bytes: &[u8]) -> Result<String> {
        self.check()?;
        let h = sha_bytes(bytes);
        if let Some(d) = self.dir() {
            let dir = d.join("blobs");
            std::fs::create_dir_all(&dir).map_err(|e| self.fail(format!("event log blob: {e}")))?;
            let f = dir.join(&h);
            if !f.exists() {
                // atomic: a crash mid-write must not leave a truncated blob under its final name
                write_atomic(&f, bytes).map_err(|e| self.fail(format!("event log blob: {e}")))?;
            } else {
                read_blob(&d, &h).map_err(|e| self.fail(e))?;
            }
        }
        Ok(h)
    }

    /// Append a record; `sync` forces it to disk (used for intents). Returns its seq.
    pub fn write(&self, kind: &str, fields: Value, sync: bool) -> Result<u64> {
        anyhow::ensure!(!self.owner_reconciliation || matches!(kind, "resolution" | "billing"), "owner reconciliation can only record resolutions or billing");
        self.check()?;
        let mut g = self.inner.lock().unwrap();
        self.check()?; // another writer may have failed while this one waited for the file
        g.seq += 1;
        let seq = g.seq;
        let mut obj = json!({"seq": seq, "ts_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0), "session": self.session, "kind": kind, "prev": g.prev});
        if let (Some(o), Value::Object(f)) = (obj.as_object_mut(), fields) {
            for (k, v) in f {
                o.insert(k, v);
            }
        }
        let line = serde_json::to_string(&obj).unwrap_or_default();
        if let Some(f) = g.file.as_mut() {
            f.write_all(format!("{line}\n").as_bytes()).map_err(|e| self.fail(format!("event log write: {e}")))?;
            f.flush().map_err(|e| self.fail(format!("event log flush: {e}")))?;
            if sync {
                f.sync_data().map_err(|e| self.fail(format!("event log sync: {e}")))?;
            }
        }
        g.prev = sha_bytes(line.as_bytes());
        if self.crash_after == Some(seq) {
            if let Some(f) = g.file.as_mut() {
                let _ = f.sync_data();
            }
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        }
        Ok(seq)
    }
}

pub struct Report {
    pub records: usize,
    pub chain_ok: bool,
    pub bad_line: Option<usize>,
    /// (seq, tool, summary) for every intent without an outcome
    pub unknown: Vec<(u64, String, String)>,
    pub unknown_models: Vec<u64>,
}

/// One complete record: its line exactly as written (the hash chain covers it) and the parsed value.
type Record<'a> = (&'a str, Value);

/// A crash can tear a UTF-8 character as well as JSON. Decode complete lines strictly; an undecodable,
/// unterminated tail is represented as invalid JSON so the normal torn-tail path discards only that tail.
fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut text = std::str::from_utf8(&bytes[..end])?.to_string();
    text.push_str(std::str::from_utf8(&bytes[end..]).unwrap_or("{torn UTF-8"));
    Ok(text)
}

/// The records of a log, in order, with whether its final line is torn (a crash mid-write; that line is left out).
/// A malformed line anywhere else is damage, not a crash: Err(its 1-based line number).
fn parse(text: &str) -> std::result::Result<(Vec<Record<'_>>, bool), usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push((*line, v)),
            Err(_) if i + 1 == lines.len() && !text.ends_with('\n') => return Ok((out, true)),
            Err(_) => return Err(i + 1),
        }
    }
    Ok((out, false))
}

/// Write `bytes` to `path` through a temp file and a rename, so readers see the old or the new content, never a part.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Verify the hash chain and list effects whose outcome is unknown.
pub fn verify(path: &Path) -> Result<Report> {
    let text = read_text(path)?;
    verify_text(&text)
}

fn verify_text(text: &str) -> Result<Report> {
    let records = parse(text).map_err(|n| anyhow::anyhow!("line {n} is not valid JSON"))?.0;
    let mut prev = "genesis".to_string();
    let mut intents: std::collections::BTreeMap<u64, (String, String)> = Default::default();
    let mut models = std::collections::BTreeSet::new();
    let (mut n, mut bad) = (0, None);
    for (i, (line, v)) in records.into_iter().enumerate() {
        if v["prev"].as_str() != Some(prev.as_str()) && bad.is_none() {
            bad = Some(i + 1);
        }
        prev = sha_bytes(line.as_bytes());
        n += 1;
        match v["kind"].as_str() {
            Some("intent") => {
                intents.insert(v["seq"].as_u64().unwrap_or(0), (v["tool"].as_str().unwrap_or("?").into(), v["summary"].as_str().unwrap_or("").into()));
            }
            Some("outcome" | "resolution") => {
                intents.remove(&v["of"].as_u64().unwrap_or(0));
            }
            Some("model_intent") => { models.insert(v["seq"].as_u64().unwrap_or(0)); }
            Some("llm") if v.get("error").is_none() => { models.remove(&v["of"].as_u64().unwrap_or(0)); }
            Some("billing") => { models.remove(&v["of"].as_u64().unwrap_or(0)); }
            _ => {}
        }
    }
    Ok(Report { records: n, chain_ok: bad.is_none(), bad_line: bad, unknown: intents.into_iter().map(|(s, (t, d))| (s, t, d)).collect(), unknown_models: models.into_iter().collect() })
}

fn crash_hook() -> Option<u64> {
    std::env::var("HX_TEST_CRASH_AFTER_RECORD").ok().and_then(|v| v.parse().ok())
}

pub fn read_blob(log_dir: &Path, sha: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "invalid blob hash {sha:?}");
    let bytes = std::fs::read(log_dir.join("blobs").join(sha))?;
    anyhow::ensure!(sha_bytes(&bytes) == sha, "blob {sha}: content hash mismatch");
    Ok(bytes)
}

/// All complete records of a log (a torn final line from a crash is ignored; a malformed line before it is an error).
pub fn load(path: &Path) -> Result<Vec<Value>> {
    let text = read_text(path)?;
    let report = verify_text(&text)?;
    anyhow::ensure!(report.chain_ok, "{}: hash chain broken at line {}", path.display(), report.bad_line.unwrap_or(0));
    let records = parse(&text).map_err(|n| anyhow::anyhow!("{}: line {n} is not valid JSON", path.display()))?.0;
    Ok(records.into_iter().map(|(_, v)| v).collect())
}

#[cfg(test)]
mod open_tests {
    use super::*;

    #[test]
    fn two_sessions_in_the_same_second_get_distinct_logs() {
        let dir = std::env::temp_dir().join(format!("hx-eventlog-{}", std::process::id()));
        let (a, b) = (super::EventLog::open(&dir).unwrap(), super::EventLog::open(&dir).unwrap());
        assert_ne!(a.session, b.session);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A log with three records, written the normal way.
    fn three_records(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hx-eventlog-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let log = EventLog::open(&dir).unwrap();
        for i in 0..3 {
            log.write("note", json!({"i": i}), false).unwrap();
        }
        log.path.clone().unwrap()
    }

    #[test]
    fn reopen_drops_only_a_torn_final_line_and_keeps_the_chain() {
        let path = three_records("torn");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"seq\": 4, \"kin");
        std::fs::write(&path, &text).unwrap();
        let log = EventLog::reopen(&path).unwrap();
        assert_eq!(log.write("note", json!({}), false).unwrap(), 4);
        let r = verify(&path).unwrap();
        assert!(r.chain_ok && r.records == 4, "{} records, chain {}", r.records, r.chain_ok);
        assert_eq!(load(&path).unwrap().len(), 4);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn reopen_after_a_lost_final_newline_appends_on_a_new_line() {
        let path = three_records("nonl");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.trim_end_matches('\n')).unwrap();
        EventLog::reopen(&path).unwrap().write("note", json!({}), false).unwrap();
        let r = verify(&path).unwrap();
        assert!(r.chain_ok && r.records == 4, "{} records, chain {}", r.records, r.chain_ok);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_line_before_the_end_is_refused_not_truncated() {
        let path = three_records("corrupt");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = text.lines().collect();
        lines[1] = "not json";
        let bad = lines.join("\n") + "\n";
        std::fs::write(&path, &bad).unwrap();
        assert!(EventLog::reopen(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bad, "resume must not delete the records after the bad line");
        assert!(load(&path).is_err());
        assert!(verify(&path).is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn complete_corrupt_final_line_is_not_a_torn_write() {
        let path = three_records("bad-final");
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("not json\n");
        std::fs::write(&path, &text).unwrap();
        assert!(EventLog::reopen(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn torn_utf8_is_recovered_but_complete_invalid_utf8_is_preserved() {
        let path = three_records("utf8-tail");
        let original = std::fs::read(&path).unwrap();
        let mut torn = original.clone();
        torn.extend_from_slice(b"{\"cmd\":\"\xe2");
        std::fs::write(&path, &torn).unwrap();
        assert_eq!(load(&path).unwrap().len(), 3);
        EventLog::reopen(&path).unwrap().write("note", json!({}), false).unwrap();
        let r = verify(&path).unwrap();
        assert!(r.chain_ok && r.records == 4);
        torn.push(b'\n');
        std::fs::write(&path, &torn).unwrap();
        assert!(EventLog::reopen(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), torn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_intent_write_prevents_the_effect_even_if_the_error_is_caught() {
        let path = three_records("write-failure");
        let mut pol = crate::policy::Policy::new(path.parent().unwrap(), true, None, None, None).unwrap();
        pol.log = Arc::new(EventLog {
            inner: Mutex::new(Inner { file: Some(std::fs::File::open(&path).unwrap()), seq: 0, prev: "genesis".into() }),
            failure: Mutex::new(None),
            session: "read-only".into(), path: Some(path.clone()), crash_after: None, _writer_lock: None, owner_reconciliation: false,
        });
        let fx = crate::effects::Effects::new(Arc::new(pol), 0, Arc::default());
        let lua = mlua::Lua::new();
        let mut executed = false;
        for _ in 0..2 {
            let result = fx.sync(&lua, "write", crate::policy::Class::LocalWrite, json!({}), || {
                executed = true;
                Ok(crate::effects::Ret::Num(0.0))
            });
            assert!(result.is_err(), "a failed WAL must be fatal before executing the effect");
        }
        assert!(!executed);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn failed_outcome_blob_keeps_an_unknown_intent_and_blocks_later_effects() {
        let path = three_records("blob-failure");
        let pol = crate::policy::Policy::new(path.parent().unwrap(), true, None, None, None).unwrap();
        let log_path = pol.log.path.clone().unwrap();
        std::fs::write(log_path.parent().unwrap().join("blobs"), "not a directory").unwrap();
        let fx = crate::effects::Effects::new(Arc::new(pol), 0, Arc::default());
        let lua = mlua::Lua::new();
        let mut executions = 0;
        for _ in 0..2 {
            let result = fx.sync(&lua, "write", crate::policy::Class::LocalWrite, json!({}), || {
                executions += 1;
                Ok(crate::effects::Ret::Str("result".into()))
            });
            assert!(result.is_err());
        }
        assert_eq!(executions, 1, "the first effect precedes the outcome failure; the second must not execute");
        let report = verify(&log_path).unwrap();
        assert!(report.chain_ok);
        assert_eq!(report.unknown.len(), 1, "the failed outcome must remain recoverable as unknown");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn blob_hash_and_path_are_verified() {
        let path = three_records("blob-hash");
        let log = EventLog::reopen(&path).unwrap();
        let hash = log.blob(b"original").unwrap();
        assert_eq!(read_blob(path.parent().unwrap(), &hash).unwrap(), b"original");
        std::fs::write(path.parent().unwrap().join("blobs").join(&hash), "corrupted").unwrap();
        assert!(read_blob(path.parent().unwrap(), &hash).is_err());
        assert!(log.blob(b"original").is_err(), "deduplication must not reuse a corrupted existing blob");
        assert!(read_blob(path.parent().unwrap(), "../outside").is_err());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
