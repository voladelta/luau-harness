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
    pub session: String,
    pub path: Option<PathBuf>,
    /// test hook: SIGKILL this process right after the Nth record is on disk (HX_TEST_CRASH_AFTER_RECORD)
    crash_after: Option<u64>,
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
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file: Some(file), seq: 0, prev: "genesis".into() }), session, path: Some(path), crash_after: crash_hook() }))
    }

    /// Continue an existing log after a crash: restore seq and the hash chain, drop a torn final line. A malformed line
    /// before the end is not a crash: the log is refused, never truncated (the records after it are evidence).
    pub fn reopen(path: &Path) -> Result<Arc<EventLog>> {
        let text = std::fs::read_to_string(path)?;
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
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file: Some(file), seq, prev }), session, path: Some(path.to_path_buf()), crash_after: crash_hook() }))
    }

    pub fn off() -> Arc<EventLog> {
        Arc::new(EventLog { inner: Mutex::new(Inner { file: None, seq: 0, prev: String::new() }), session: "off".into(), path: None, crash_after: None })
    }

    pub fn dir(&self) -> Option<PathBuf> {
        self.path.as_ref().and_then(|p| p.parent().map(Path::to_path_buf))
    }

    /// Store `bytes` content-addressed next to the log (write-once) and return its sha256.
    pub fn blob(&self, bytes: &[u8]) -> String {
        let h = sha_bytes(bytes);
        if let Some(d) = self.dir() {
            let dir = d.join("blobs");
            let _ = std::fs::create_dir_all(&dir);
            let f = dir.join(&h);
            if !f.exists() {
                // atomic: a crash mid-write must not leave a truncated blob under its final name
                let _ = write_atomic(&f, bytes);
            }
        }
        h
    }

    /// Append a record; `sync` forces it to disk (used for intents). Returns its seq.
    pub fn write(&self, kind: &str, fields: Value, sync: bool) -> u64 {
        let mut g = self.inner.lock().unwrap();
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
            let _ = f.write_all(format!("{line}\n").as_bytes());
            let _ = f.flush();
            if sync {
                let _ = f.sync_data();
            }
        }
        g.prev = sha_bytes(line.as_bytes());
        if self.crash_after == Some(seq) {
            if let Some(f) = g.file.as_mut() {
                let _ = f.sync_data();
            }
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        }
        seq
    }
}

pub struct Report {
    pub records: usize,
    pub chain_ok: bool,
    pub bad_line: Option<usize>,
    /// (seq, tool, summary) for every intent without an outcome
    pub unknown: Vec<(u64, String, String)>,
}

/// One complete record: its line exactly as written (the hash chain covers it) and the parsed value.
type Record<'a> = (&'a str, Value);

/// The records of a log, in order, with whether its final line is torn (a crash mid-write; that line is left out).
/// A malformed line anywhere else is damage, not a crash: Err(its 1-based line number).
fn parse(text: &str) -> std::result::Result<(Vec<Record<'_>>, bool), usize> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<Value>(line) {
            Ok(v) => out.push((*line, v)),
            Err(_) if i + 1 == lines.len() => return Ok((out, true)),
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
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// Verify the hash chain and list effects whose outcome is unknown.
pub fn verify(path: &Path) -> Result<Report> {
    let text = std::fs::read_to_string(path)?;
    let records = parse(&text).map_err(|n| anyhow::anyhow!("line {n} is not valid JSON"))?.0;
    let mut prev = "genesis".to_string();
    let mut intents: std::collections::BTreeMap<u64, (String, String)> = Default::default();
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
            Some("outcome") => {
                intents.remove(&v["of"].as_u64().unwrap_or(0));
            }
            _ => {}
        }
    }
    Ok(Report { records: n, chain_ok: bad.is_none(), bad_line: bad, unknown: intents.into_iter().map(|(s, (t, d))| (s, t, d)).collect() })
}

fn crash_hook() -> Option<u64> {
    std::env::var("HX_TEST_CRASH_AFTER_RECORD").ok().and_then(|v| v.parse().ok())
}

pub fn read_blob(log_dir: &Path, sha: &str) -> Result<Vec<u8>> {
    Ok(std::fs::read(log_dir.join("blobs").join(sha))?)
}

/// All complete records of a log (a torn final line from a crash is ignored; a malformed line before it is an error).
pub fn load(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)?;
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
            log.write("note", json!({"i": i}), false);
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
        assert_eq!(log.write("note", json!({}), false), 4);
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
        EventLog::reopen(&path).unwrap().write("note", json!({}), false);
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
}
