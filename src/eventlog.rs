//! Append-only, hash-chained event log: the source of truth for what the model did.
//!
//! One JSON object per line: { seq, ts_ms, session, kind, prev, ... }. `prev` is the SHA-256 of the previous line, so
//! editing or deleting any earlier line breaks the chain (`hx log verify`). Effects that change the world are written
//! write-ahead: an `intent` record (flushed and synced) BEFORE the effect, an `outcome` record (`of` = the intent's
//! seq) after. An intent with no outcome is an UNKNOWN outcome: the process died mid-effect, and a human decides.
use anyhow::{bail, Result};
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
        let session = format!("{}-{}", ts.as_secs(), std::process::id());
        let path = dir.join(format!("{session}.jsonl"));
        let file = std::fs::OpenOptions::new().create_new(true).append(true).open(&path)?;
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file: Some(file), seq: 0, prev: "genesis".into() }), session, path: Some(path), crash_after: crash_hook() }))
    }

    /// Continue an existing log after a crash: restore seq and the hash chain, drop a torn final line.
    pub fn reopen(path: &Path) -> Result<Arc<EventLog>> {
        let text = std::fs::read_to_string(path)?;
        let mut keep = String::new();
        let (mut seq, mut prev) = (0u64, "genesis".to_string());
        for line in text.lines() {
            match serde_json::from_str::<Value>(line) {
                Ok(v) => {
                    seq = v["seq"].as_u64().unwrap_or(seq);
                    prev = sha_bytes(line.as_bytes());
                    keep.push_str(line);
                    keep.push('\n');
                }
                Err(_) => break,
            }
        }
        std::fs::write(path, &keep)?;
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
                let _ = std::fs::write(f, bytes);
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

/// Verify the hash chain and list effects whose outcome is unknown.
pub fn verify(path: &Path) -> Result<Report> {
    let text = std::fs::read_to_string(path)?;
    let mut prev = "genesis".to_string();
    let mut intents: std::collections::BTreeMap<u64, (String, String)> = Default::default();
    let (mut n, mut bad) = (0, None);
    for (i, line) in text.lines().enumerate() {
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) if i + 1 == text.lines().count() => break, // a torn final line = crash mid-write; not tampering
            Err(_) => bail!("line {} is not valid JSON", i + 1),
        };
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

/// All complete records of a log (a torn final line from a crash is ignored).
pub fn load(path: &Path) -> Result<Vec<Value>> {
    let text = std::fs::read_to_string(path)?;
    Ok(text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}
