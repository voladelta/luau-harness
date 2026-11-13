//! Append-only event log with a keyed hash chain: the source of truth for what the model did.
//!
//! One JSON object per line: { seq, ts_ms, session, kind, prev, ... }. `prev` is the HMAC-SHA256 of the previous line
//! under a per-user key (`KEY_FILE`, beside the trust store, outside every workspace), so a process that can write the
//! journal but cannot read the key cannot edit, insert or delete a record, or append a forged one, without `hx log
//! verify` and every reopen noticing. Removing records from the end leaves a valid chain and is not detected. Effects that
//! change the world are written write-ahead: an `intent` record (flushed and synced) BEFORE the effect, an `outcome`
//! record (`of` = the intent's seq) after. An intent with no outcome is an UNKNOWN outcome: the process died mid-effect,
//! and a human decides.
use anyhow::{Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Record kinds that change the books (`supervisor::Books`). They reach the journal only through
/// `EventLog::append_ledger`, which the supervisor's ledger calls after checking each record against its books;
/// `EventLog::write` refuses their names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerKind {
    Intent,
    Outcome,
    Resolution,
    Uncertain,
    ModelIntent,
    Llm,
    Billing,
}

impl LedgerKind {
    pub const ALL: [Self; 7] = [Self::Intent, Self::Outcome, Self::Resolution, Self::Uncertain, Self::ModelIntent, Self::Llm, Self::Billing];

    /// The record's `kind` in the journal.
    pub fn name(self) -> &'static str {
        match self {
            Self::Intent => "intent",
            Self::Outcome => "outcome",
            Self::Resolution => "resolution",
            Self::Uncertain => "uncertain",
            Self::ModelIntent => "model_intent",
            Self::Llm => "llm",
            Self::Billing => "billing",
        }
    }

    /// The ledger kind a journal `kind` names, if any.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// The journal key's file name in hx's configuration directory (`trust::store_dir`), which confined children cannot see
/// and file tools refuse.
pub const KEY_FILE: &str = "journal.key";

const GENESIS: &str = "genesis";

struct Inner {
    file: std::fs::File,
    seq: u64,
    prev: String,
}

pub struct EventLog {
    inner: Mutex<Inner>,
    failure: Mutex<Option<String>>,
    pub session: String,
    pub path: PathBuf,
    key: ChainKey,
    /// test hook: SIGKILL this process right after the Nth record is on disk (HX_TEST_CRASH_AFTER_RECORD)
    crash_after: Option<u64>,
    // A separate inode survives torn-tail repair. The lock is released only when the run drops its journal.
    _writer_lock: Option<std::fs::File>,
    owner_reconciliation: bool,
    /// Blobs this process wrote or verified, with the file's identity when it did. Checkpoints name every message of
    /// the conversation each time, so re-hashing every existing blob would make each checkpoint cost the whole
    /// conversation again; an unchanged identity means the file was neither replaced nor written since.
    known: Mutex<std::collections::HashMap<String, FileId>>,
}

/// What changes whenever a file's content can have changed: a replacement gets a new inode, and any write or
/// metadata change moves the change time, which, unlike the modification time, cannot be set back.
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileId {
    ino: u64,
    len: u64,
    ctime: (i64, i64),
}

impl FileId {
    fn of(path: &Path) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::symlink_metadata(path)?;
        Ok(Self { ino: m.ino(), len: m.len(), ctime: (m.ctime(), m.ctime_nsec()) })
    }
}

/// Take the journal's single-writer lock: an exclusive lock on `PATH.lock`, a file of its own so replacing the journal
/// cannot drop it. Fails at once when another process holds it; released when the returned file is dropped.
fn lock_writer(path: &Path) -> Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .custom_flags(libc::O_NOFOLLOW).open(path.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!("run is already open by another process: {}", path.display()),
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("lock {}", path.display())),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha_bytes(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// `create_dir_all` that also fsyncs the parent of every directory it creates, so the path to a new journal survives a
/// crash along with the journal.
fn create_dirs(dir: &Path) -> std::io::Result<()> {
    if dir.as_os_str().is_empty() || dir.is_dir() {
        return Ok(());
    }
    let parent = dir.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    create_dirs(parent)?;
    match std::fs::create_dir(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e),
        _ => {},
    }
    sync_dir(parent)
}

/// The per-user secret that keys every journal's chain.
#[derive(Clone)]
struct ChainKey([u8; 32]);

impl ChainKey {
    fn path() -> Result<PathBuf> {
        crate::trust::store_dir().map(|d| d.join(KEY_FILE)).context("no HOME or XDG_CONFIG_HOME for the journal key")
    }

    /// The key that verifies existing journals. It is never created here: a new key could verify nothing.
    fn load() -> Result<Self> {
        let path = Self::path()?;
        Self::read(&path).with_context(|| format!("journal key {}", path.display()))
    }

    /// The key new journals are written with, created on first use.
    fn load_or_create() -> Result<Self> {
        let path = Self::path()?;
        match Self::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::create(&path),
            other => other,
        }
        .with_context(|| format!("journal key {}", path.display()))
    }

    fn read(path: &Path) -> std::io::Result<Self> {
        let mut bytes = Vec::with_capacity(33);
        std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path)?.take(33).read_to_end(&mut bytes)?;
        let key = bytes.try_into().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "not a 32-byte key"))?;
        Ok(Self(key))
    }

    /// 32 random bytes, mode 0600. They are written under a private temporary name and hard-linked into place, which
    /// fails if the key exists (like `O_EXCL`), so the key appears whole or not at all and concurrent first uses agree.
    fn create(path: &Path) -> std::io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = path.parent().expect("the key path has a parent directory");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let mut key = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
        let (tmp, mut file) = loop {
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let tmp = dir.join(format!(".{KEY_FILE}.{}-{n}", std::process::id()));
            match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp) {
                Ok(file) => break (tmp, file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        let linked = file.write_all(&key).and_then(|()| file.sync_all()).and_then(|()| std::fs::hard_link(&tmp, path));
        std::fs::remove_file(&tmp)?;
        match linked {
            Ok(()) => {
                sync_dir(dir)?;
                Ok(Self(key))
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Self::read(path),
            Err(e) => Err(e),
        }
    }

    /// The link to `line`: what the `prev` of the record after it must be.
    fn link(&self, line: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.0).expect("HMAC takes a key of any length");
        mac.update(line.as_bytes());
        hex(&mac.finalize().into_bytes())
    }
}

impl EventLog {
    pub fn open(dir: &Path) -> Result<Arc<EventLog>> {
        let key = ChainKey::load_or_create()?;
        create_dirs(dir).with_context(|| format!("journal directory {}", dir.display()))?;
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
        // The journal's name must be durable before the first intent synced into it is.
        sync_dir(dir).with_context(|| format!("journal directory {}", dir.display()))?;
        let _writer_lock = Some(lock_writer(&path)?);
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file, seq: 0, prev: GENESIS.into() }), failure: Mutex::new(None), session, path, key, crash_after: crash_hook(), _writer_lock, owner_reconciliation: false, known: Mutex::default() }))
    }

    /// Continue an existing log after a crash: verify the chain with the key, restore seq and the chain, drop a torn
    /// final line. A malformed line before the end is not a crash: the log is refused, never truncated (the records after
    /// it are evidence).
    pub fn reopen(path: &Path) -> Result<Arc<EventLog>> {
        Self::reopen_mode(path, false)
    }

    /// Owner CLI only: revocation still blocks execution, but does not erase the ability to reconcile receipts. Only
    /// `resolution` and `billing` records can be appended.
    pub fn reconcile(path: &Path) -> Result<Arc<EventLog>> {
        Self::reopen_mode(path, true)
    }

    fn reopen_mode(path: &Path, owner_reconciliation: bool) -> Result<Arc<EventLog>> {
        let _writer_lock = Some(lock_writer(path)?);
        let key = ChainKey::load()?;
        let text = read_text(path)?;
        let chain = Chain::check(&text, &key).map_err(|e| anyhow::anyhow!("{}: {e}; not resuming a damaged log", path.display()))?;
        if let Some((line, why)) = &chain.broken {
            anyhow::bail!("{}: {why} at line {line}; not resuming a damaged log", path.display());
        }
        let seq = chain.records.len() as u64;
        // a torn tail is dropped; a complete final record that lost its newline gets one, so appends start a new line
        if chain.torn || !text.is_empty() && !text.ends_with('\n') {
            let keep: String = chain.records.iter().map(|(l, _)| format!("{l}\n")).collect();
            write_atomic(path, keep.as_bytes())?;
        }
        let file = std::fs::OpenOptions::new().append(true).open(path)?;
        let session = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        Ok(Arc::new(EventLog { inner: Mutex::new(Inner { file, seq, prev: chain.next }), failure: Mutex::new(None), session, path: path.to_path_buf(), key, crash_after: crash_hook(), _writer_lock, owner_reconciliation, known: Mutex::default() }))
    }

    /// Audit failures are fatal to the run, even if Luau catches the returned error.
    pub fn fail(&self, message: impl std::fmt::Display) -> anyhow::Error {
        let mut failure = self.failure.lock().unwrap();
        let message = failure.get_or_insert_with(|| message.to_string()).clone();
        anyhow::anyhow!(message)
    }

    pub fn check(&self) -> Result<()> {
        if !self.owner_reconciliation && self.path.with_extension("revoked").exists() {
            return Err(self.fail("run revoked by its owner"));
        }
        match self.failure.lock().unwrap().as_ref() {
            Some(message) => anyhow::bail!("{message}"),
            None => Ok(()),
        }
    }

    pub fn dir(&self) -> &Path {
        self.path.parent().expect("a journal path has a parent directory")
    }

    /// Store `bytes` content-addressed next to the log (write-once, synced) and return its sha256. A blob that already
    /// exists is reused once its content is verified; this process verifies each blob once, and again only if the
    /// file was replaced or written since.
    pub fn blob(&self, bytes: &[u8]) -> Result<String> {
        self.check()?;
        let h = sha_bytes(bytes);
        let dir = self.dir().join("blobs");
        create_dirs(&dir).map_err(|e| self.fail(format!("event log blob: {e}")))?;
        let f = dir.join(&h);
        let known = self.known.lock().unwrap().get(&h).copied();
        match FileId::of(&f) {
            Ok(id) if known == Some(id) => {},
            Ok(id) => {
                // The identity taken before the read: a change during it shows as a different identity next time.
                read_blob(self.dir(), &h).map_err(|e| self.fail(format!("{e:#}")))?;
                self.known.lock().unwrap().insert(h.clone(), id);
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // atomic: a crash mid-write must not leave a truncated blob under its final name
                write_atomic(&f, bytes).map_err(|e| self.fail(format!("event log blob: {e}")))?;
            },
            Err(e) => return Err(self.fail(format!("event log blob {}: {e}", f.display()))),
        }
        Ok(h)
    }

    /// Append a record of any kind but the ledger's (`LedgerKind`); `sync` forces it to disk. Returns its seq.
    pub fn write(&self, kind: &str, fields: Value, sync: bool) -> Result<u64> {
        anyhow::ensure!(LedgerKind::parse(kind).is_none(), "{kind} records change the books; the supervisor's ledger appends them");
        anyhow::ensure!(!self.owner_reconciliation, "owner reconciliation can only record resolutions or billing");
        self.append(kind, fields, sync)
    }

    /// Append a ledger record. Only `supervisor`'s ledger calls this, after checking the record against the books it
    /// then applies it to, so the live books stay the journal's fold.
    pub(crate) fn append_ledger(&self, kind: LedgerKind, fields: Value, sync: bool) -> Result<u64> {
        anyhow::ensure!(!self.owner_reconciliation || matches!(kind, LedgerKind::Resolution | LedgerKind::Billing),
            "owner reconciliation can only record resolutions or billing");
        self.append(kind.name(), fields, sync)
    }

    fn append(&self, kind: &str, fields: Value, sync: bool) -> Result<u64> {
        self.check()?;
        if !fields.as_object().is_some_and(|fields| ["seq", "prev", "kind", "session", "ts_ms"].iter().all(|key| !fields.contains_key(*key))) {
            return Err(self.fail("journal fields cannot replace record metadata"));
        }
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
        g.file.write_all(format!("{line}\n").as_bytes()).map_err(|e| self.fail(format!("event log write: {e}")))?;
        g.file.flush().map_err(|e| self.fail(format!("event log flush: {e}")))?;
        if sync {
            g.file.sync_data().map_err(|e| self.fail(format!("event log sync: {e}")))?;
        }
        g.prev = self.key.link(&line);
        if self.crash_after == Some(seq) {
            let _ = g.file.sync_data();
            // SAFETY: kill takes only integers and touches no memory of this process before it ends it.
            unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
        }
        Ok(seq)
    }
}

/// What `hx log verify` checks without folding the books: the chain, the record sequence and the referenced blobs.
pub struct Report {
    pub records: usize,
    /// The first line that breaks the chain or the record sequence, and why.
    pub broken: Option<(usize, String)>,
    /// Blobs that records reference but that are missing or do not hold the content their name hashes.
    pub blob_errors: Vec<String>,
}

/// One complete record: its line exactly as written (the chain covers it) and the parsed value.
type Record<'a> = (&'a str, Value);

/// A crash can tear a UTF-8 character as well as JSON. Decode complete lines strictly; an undecodable,
/// unterminated tail is represented as invalid JSON so the normal torn-tail path discards only that tail.
fn read_text(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("journal {}", path.display()))?;
    let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let mut text = std::str::from_utf8(&bytes[..end]).with_context(|| format!("{}: invalid UTF-8", path.display()))?.to_string();
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

/// A log's records checked against the key.
struct Chain<'a> {
    records: Vec<Record<'a>>,
    torn: bool,
    /// The first line whose `prev`, `seq` or `kind` is wrong, and what is wrong with it.
    broken: Option<(usize, String)>,
    /// The `prev` the next appended record carries.
    next: String,
}

impl<'a> Chain<'a> {
    fn check(text: &'a str, key: &ChainKey) -> Result<Self> {
        let (records, torn) = parse(text).map_err(|n| anyhow::anyhow!("line {n} is not valid JSON"))?;
        let (mut next, mut broken) = (GENESIS.to_string(), None);
        for (i, (line, v)) in records.iter().enumerate() {
            let n = i + 1;
            let problem = if v["prev"].as_str() != Some(next.as_str()) {
                // Journals of earlier formats chained plain SHA-256: say so rather than call them damaged.
                let unkeyed = n == 2 && v["prev"].as_str() == Some(sha_bytes(records[0].0.as_bytes()).as_str());
                Some(if unkeyed { "hash chain is not keyed (an earlier journal format; start a new run)" } else { "hash chain broken" })
            } else if v["seq"].as_u64() != Some(n as u64) {
                Some("record sequence is not consecutive")
            } else if !v["kind"].is_string() {
                Some("record has no kind")
            } else {
                None
            };
            if let Some(problem) = problem {
                broken = Some((n, problem.to_string()));
                break;
            }
            next = key.link(line);
        }
        Ok(Self { records, torn, broken, next })
    }
}

/// Verify the chain and the record sequence with the journal key, and every blob a record references.
pub fn verify(path: &Path) -> Result<Report> {
    let key = ChainKey::load()?;
    let text = read_text(path)?;
    let chain = Chain::check(&text, &key).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    let dir = path.parent().context("a journal path has a parent directory")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut blob_errors = vec![];
    for (_, record) in &chain.records {
        for sha in blob_refs(record) {
            if !seen.insert(sha.to_string()) {
                continue;
            }
            let body = match read_blob(dir, sha) {
                Ok(body) => body,
                Err(e) => {
                    blob_errors.push(format!("record {}: {e:#}", record["seq"]));
                    continue;
                },
            };
            if record["kind"] != "checkpoint" || record["blob"] != sha {
                continue;
            }
            if let Err(e) = verify_messages(dir, &body, &mut seen) {
                blob_errors.push(format!("record {}: checkpoint {sha}: {e:#}", record["seq"]));
            }
        }
    }
    Ok(Report { records: chain.records.len(), broken: chain.broken, blob_errors })
}

/// Hashes per chunk of a chunked hash list, such as a checkpoint's messages. Appending to the list rewrites only its
/// last chunk, so storing a list that grew writes one hash per chunk plus at most this many, not one per item.
pub const CHUNK: usize = 64;

/// The hashes a chunk blob lists: `CHUNK` of them, or 1 to `CHUNK` for a list's `last` chunk.
pub fn parse_chunk(bytes: &[u8], last: bool) -> Result<Vec<String>> {
    let hashes: Vec<String> = serde_json::from_slice(bytes).context("chunk is not a list of hashes")?;
    anyhow::ensure!(hashes.len() == CHUNK || last && (1..=CHUNK).contains(&hashes.len()), "chunk of {} hashes", hashes.len());
    Ok(hashes)
}

/// Check the message chunks a checkpoint body names under `message_chunks`, and the message blobs they list
/// (`supervisor::CheckpointBody`). Stops at the first bad blob, which the error names.
fn verify_messages(dir: &Path, body: &[u8], seen: &mut std::collections::BTreeSet<String>) -> Result<()> {
    let body: Value = serde_json::from_slice(body).context("checkpoint body is not JSON")?;
    let chunks = match &body["message_chunks"] {
        Value::Null => return Ok(()),
        chunks => serde_json::from_value::<Vec<String>>(chunks.clone()).context("message chunks are not a list of hashes")?,
    };
    for (c, chunk) in chunks.iter().enumerate() {
        if !seen.insert(chunk.clone()) {
            continue;
        }
        let bytes = read_blob(dir, chunk)?;
        let messages = parse_chunk(&bytes, c + 1 == chunks.len()).with_context(|| format!("message chunk {chunk}"))?;
        for (i, message) in messages.into_iter().enumerate() {
            if seen.insert(message.clone()) {
                read_blob(dir, &message).with_context(|| format!("message {}", c * CHUNK + i))?;
            }
        }
    }
    Ok(())
}

/// The blobs a record names directly: manifest and checkpoint bodies, model replies and effect receipts.
fn blob_refs(record: &Value) -> impl Iterator<Item = &str> {
    let ret = &record["ret"];
    let receipt: &[&str] = match ret["t"].as_str() {
        Some("str" | "rows") => &["blob"],
        Some("sh") => &["stdout", "stderr"],
        Some("agent") => &["answer"],
        Some("json") => &["v"],
        _ => &[],
    };
    [&record["blob"], &record["reply"]].into_iter().chain(receipt.iter().map(move |k| &ret[*k])).filter_map(Value::as_str)
}

fn crash_hook() -> Option<u64> {
    std::env::var("HX_TEST_CRASH_AFTER_RECORD").ok().and_then(|v| v.parse().ok())
}

pub fn read_blob(log_dir: &Path, sha: &str) -> Result<Vec<u8>> {
    read_blob_bounded(log_dir, sha, usize::MAX, || Ok(()))
}

/// Read a blob of at most `max` bytes, calling `check` (a deadline) between chunks, and verify its content hash.
pub fn read_blob_bounded(log_dir: &Path, sha: &str, max: usize, mut check: impl FnMut() -> Result<()>) -> Result<Vec<u8>> {
    anyhow::ensure!(sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()), "invalid blob hash {sha:?}");
    let path = log_dir.join("blobs").join(sha);
    let mut file = std::fs::File::open(&path).with_context(|| format!("journal blob {}", path.display()))?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        check()?;
        let n = file.read(&mut chunk).with_context(|| format!("journal blob {}", path.display()))?;
        if n == 0 { break; }
        anyhow::ensure!(bytes.len() + n <= max, "blob {sha}: over the {max}-byte read limit");
        bytes.extend_from_slice(&chunk[..n]);
    }
    anyhow::ensure!(sha_bytes(&bytes) == sha, "blob {sha}: content hash mismatch");
    Ok(bytes)
}

/// All complete records of a log, after verifying the chain with the journal key (a torn final line from a crash is
/// ignored; a malformed line before it, a broken chain or a sequence gap is an error).
pub fn load(path: &Path) -> Result<Vec<Value>> {
    let key = ChainKey::load()?;
    let text = read_text(path)?;
    let chain = Chain::check(&text, &key).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
    if let Some((line, why)) = chain.broken {
        anyhow::bail!("{}: {why} at line {line}", path.display());
    }
    Ok(chain.records.into_iter().map(|(_, v)| v).collect())
}

/// Write `bytes` to `path` through a temp file and a rename, so readers see the old or the new content, never a part.
fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    crate::atomic_file::replace_trusted(path, bytes)
}

#[cfg(test)]
mod open_tests {
    use super::*;

    /// Append `record` to the log at `path` with a valid link under `key`, the way a forger holding the key would.
    fn append_linked(path: &Path, mut record: Value, link: impl Fn(&str) -> String) -> String {
        let mut text = std::fs::read_to_string(path).unwrap();
        record["prev"] = json!(link(text.lines().last().unwrap()));
        text.push_str(&format!("{record}\n"));
        std::fs::write(path, &text).unwrap();
        text
    }

    #[test]
    fn a_record_appended_without_the_key_breaks_the_chain() {
        let path = three_records("forged");
        let grant = json!({"seq": 4, "kind": "path_grant", "session": "x", "ts_ms": 0, "grant": {"path": "/", "write": true}});
        let text = append_linked(&path, grant, |line| sha_bytes(line.as_bytes()));
        let report = verify(&path).unwrap();
        assert_eq!(report.broken.as_ref().map(|b| b.0), Some(4));
        assert!(EventLog::reopen(&path).is_err());
        assert!(load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text, "a refused log is left as it is");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_unkeyed_journal_is_named_as_an_earlier_format() {
        let path = three_records("unkeyed");
        let text = std::fs::read_to_string(&path).unwrap();
        let first = text.lines().next().unwrap().to_string();
        let mut second: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        second["prev"] = json!(sha_bytes(first.as_bytes()));
        std::fs::write(&path, format!("{first}\n{second}\n")).unwrap();
        let error = format!("{:#}", load(&path).unwrap_err());
        assert!(error.contains("not keyed") && error.contains("line 2"), "{error}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_keyed_chain_does_not_hide_a_sequence_gap() {
        let path = three_records("invalid-sequence");
        let key = ChainKey::load().unwrap();
        let text = append_linked(&path, json!({"seq": 5, "kind": "note"}), |line| key.link(line));
        let report = verify(&path).unwrap();
        assert_eq!(report.broken, Some((4, "record sequence is not consecutive".into())));
        assert!(EventLog::reopen(&path).is_err());
        assert!(load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_writer_lock_is_exclusive_does_not_wait_and_ends_with_its_file() {
        let dir = std::env::temp_dir().join(format!("hx-eventlog-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.jsonl");
        let held = lock_writer(&path).unwrap();
        let err = lock_writer(&path).unwrap_err().to_string();
        assert!(err.contains("already open by another process"), "{err}");
        drop(held);
        assert!(lock_writer(&path).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ledger_kinds_are_refused_outside_the_ledger() {
        let path = three_records("ledger-kinds");
        let log = EventLog::reopen(&path).unwrap();
        for kind in LedgerKind::ALL {
            assert_eq!(LedgerKind::parse(kind.name()), Some(kind));
            assert!(log.write(kind.name(), json!({"of": 1, "tokens": 1}), false).is_err(), "{kind:?}");
        }
        assert_eq!(LedgerKind::parse("note"), None);
        assert_eq!(load(&path).unwrap().len(), 3);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn verify_reports_missing_and_corrupt_blobs() {
        let path = three_records("blob-refs");
        let log = EventLog::reopen(&path).unwrap();
        let (kept, lost, bad) = (log.blob(br#"{"durable":{}}"#).unwrap(), log.blob(b"lost").unwrap(), log.blob(b"bad").unwrap());
        log.write("checkpoint", json!({"lane": "root", "blob": kept}), false).unwrap();
        log.write("call", json!({"ret": {"t": "sh", "stdout": lost, "stderr": bad}}), false).unwrap();
        assert!(verify(&path).unwrap().blob_errors.is_empty());
        let blobs = path.parent().unwrap().join("blobs");
        std::fs::remove_file(blobs.join(&lost)).unwrap();
        std::fs::write(blobs.join(&bad), "changed").unwrap();
        let errors = verify(&path).unwrap().blob_errors;
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains(&lost) && errors[1].contains("hash mismatch"), "{errors:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn the_key_is_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let path = ChainKey::path().unwrap();
        let key = ChainKey::load_or_create().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
        assert_eq!(ChainKey::load().unwrap().0, key.0);
    }

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
        log.path.clone()
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
        assert!(r.broken.is_none() && r.records == 4, "{} records, chain {:?}", r.records, r.broken);
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
        assert!(r.broken.is_none() && r.records == 4, "{} records, chain {:?}", r.records, r.broken);
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
        assert!(r.broken.is_none() && r.records == 4);
        torn.push(b'\n');
        std::fs::write(&path, &torn).unwrap();
        assert!(EventLog::reopen(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), torn);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_intent_write_prevents_the_effect_even_if_the_error_is_caught() {
        let path = three_records("write-failure");
        let mut pol = crate::policy::Policy::new(path.parent().unwrap(), false, None, None, None).unwrap();
        pol.log = Arc::new(EventLog {
            inner: Mutex::new(Inner { file: std::fs::File::open(&path).unwrap(), seq: 0, prev: "genesis".into() }),
            failure: Mutex::new(None),
            session: "read-only".into(), path: path.clone(), key: ChainKey::load().unwrap(), crash_after: None, _writer_lock: None, owner_reconciliation: false,
            known: Mutex::default(),
        });
        let fx = crate::effects::Effects::new(Arc::new(pol), 0, Arc::default());
        let lua = mlua::Lua::new();
        let mut executed = false;
        for _ in 0..2 {
            let result = fx.run_sync(&lua, crate::supervisor::Tool::Write, crate::policy::Class::LocalWrite, json!({}), |_| {
                executed = true;
                Ok(crate::effects::Ret::Num(0.0))
            }).await;
            assert!(result.is_err(), "a failed WAL must be fatal before executing the effect");
        }
        assert!(!executed);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn failed_outcome_blob_keeps_an_unknown_intent_and_blocks_later_effects() {
        let path = three_records("blob-failure");
        let pol = crate::policy::Policy::new(path.parent().unwrap(), false, None, None, None).unwrap();
        let log_path = pol.log.path.clone();
        std::fs::write(log_path.parent().unwrap().join("blobs"), "not a directory").unwrap();
        let fx = crate::effects::Effects::new(Arc::new(pol), 0, Arc::default());
        let lua = mlua::Lua::new();
        let mut executions = 0;
        for _ in 0..2 {
            let result = fx.run_sync(&lua, crate::supervisor::Tool::Write, crate::policy::Class::LocalWrite, json!({}), |_| {
                executions += 1;
                Ok(crate::effects::Ret::Str("result".into()))
            }).await;
            assert!(result.is_err());
        }
        assert_eq!(executions, 1, "the first effect precedes the outcome failure; the second must not execute");
        assert!(verify(&log_path).unwrap().broken.is_none());
        let books = crate::supervisor::Books::of(&load(&log_path).unwrap()).unwrap();
        assert_eq!(books.open.len(), 1, "the failed outcome must remain recoverable as unknown");
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

    #[test]
    fn a_verified_blob_is_reused_until_its_file_changes() {
        let path = three_records("blob-reuse");
        let log = EventLog::reopen(&path).unwrap();
        let hash = log.blob(b"original").unwrap();
        let file = path.parent().unwrap().join("blobs").join(&hash);
        // the second store verifies the existing file; the third trusts that verification
        assert_eq!(log.blob(b"original").unwrap(), hash);
        assert!(log.known.lock().unwrap().contains_key(&hash));
        assert_eq!(log.blob(b"original").unwrap(), hash);
        // an in-place rewrite of the same length moves the change time, so the blob is verified again
        std::fs::OpenOptions::new().write(true).open(&file).unwrap().write_all(b"0riginal").unwrap();
        assert!(log.blob(b"original").is_err(), "deduplication must not reuse a blob changed after verification");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
