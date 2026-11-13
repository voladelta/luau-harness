//! Structural sharing for stored state. A checkpoint or manifest is written as a tree of content-addressed blobs: every
//! subtree whose encoding is over `SPLIT` bytes becomes its own blob and its parent holds a reference, bottom-up. A
//! subtree that did not change between transitions (an earlier message, an image, a module's source) is then the same
//! blob, stored once; a transition writes only its changed path plus a node of references per list it grew.
use crate::eventlog::{self, EventLog};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value};
use std::{collections::{HashMap, HashSet}, path::Path};

/// The key of a reference object `{"$hx.blob": SHA}`. Stored object keys that start with `$` get one more `$`
/// (`escape`, undone by `unescape`), so data, including a model's own tool-call input, round-trips whatever its keys
/// are, and every reference in a stored tree was made by `store`: loading can never be steered to another blob.
pub const MARKER: &str = "$hx.blob";
/// A subtree whose encoding, with its own large subtrees already replaced, is over this many bytes is stored as a blob.
/// A reference costs about 80 bytes, so smaller subtrees stay inline.
pub const SPLIT: usize = 1024;
/// Nesting across references when loading. Stored data nests at most 64 levels (`values::MAX_DEPTH`) under a few
/// host levels; a deeper tree is damage, and must not exhaust the stack.
const MAX_DEPTH: usize = 256;

/// The hash a reference names; an object that holds the marker in any other shape is damage.
pub fn reference(value: &Value) -> Result<Option<&str>> {
    let Value::Object(map) = value else { return Ok(None) };
    let Some(sha) = map.get(MARKER) else { return Ok(None) };
    match sha.as_str() {
        Some(sha) if map.len() == 1 && sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(Some(sha)),
        _ => bail!("malformed stored reference"),
    }
}

/// Store `value` as a tree of blobs and return its root blob's hash. Every blob is durable (fsynced, directory
/// synced) before this returns, so a journal record naming the root commits the whole tree.
pub fn store(log: &EventLog, value: &Value) -> Result<String> {
    match split(Some(log), value, true)?.0 {
        Value::Object(mut map) => match map.remove(MARKER) { Some(Value::String(sha)) => Ok(sha), _ => unreachable!("the root is always stored") },
        _ => unreachable!("the root is always stored"),
    }
}

/// `value` as a parent stores it: a reference when it is large, else its inline copy (whose large parts are references).
/// For assembling a root around borrowed subtrees; `store_root` then writes it.
pub fn share(log: &EventLog, value: &Value) -> Result<Value> { Ok(split(Some(log), value, false)?.0) }

/// What `share` would put in a parent for this list, computed without writing: equal stored forms mean equal lists,
/// so a stored list is compared with a live one without reading it back.
pub fn digest_list(items: &[Value]) -> Result<Value> { Ok(split_items(None, items, false)?.0) }

/// Write a root whose fields came from `share`, and return its hash.
pub fn store_root(log: &EventLog, root: serde_json::Map<String, Value>) -> Result<String> {
    log.blob(&serde_json::to_vec(&root)?)
}

/// A blob's hash, written to `log` when there is one.
fn put(log: Option<&EventLog>, bytes: &[u8]) -> Result<String> {
    match log { Some(log) => log.blob(bytes), None => Ok(eventlog::sha_bytes(bytes)) }
}

fn split_items(log: Option<&EventLog>, items: &[Value], root: bool) -> Result<(Value, usize)> {
    let mut out = Vec::with_capacity(items.len());
    let mut len = 1 + items.len().max(1);
    for item in items {
        let (item, n) = split(log, item, false)?;
        len += n;
        out.push(item);
    }
    let node = Value::Array(out);
    if root || len > SPLIT { Ok((link(put(log, &serde_json::to_vec(&node)?)?), REF_LEN)) } else { Ok((node, len)) }
}

/// `value` with its large subtrees replaced by references, and that replacement's exact encoded length. Only the inline
/// part is copied; a stored subtree is serialized once, for its blob.
fn split(log: Option<&EventLog>, value: &Value, root: bool) -> Result<(Value, usize)> {
    let (node, len) = match value {
        Value::Object(map) => {
            let mut out = Map::new();
            let mut len = 1 + map.len().max(1);
            for (key, item) in map {
                let (item, n) = split(log, item, false)?;
                let key = escape(key);
                len += encoded(&key) + 1 + n;
                out.insert(key.into_owned(), item);
            }
            (Value::Object(out), len)
        },
        Value::Array(items) => return split_items(log, items, root),
        Value::String(s) => {
            // Stored straight from the borrowed text: a large string (an image) is never copied to be replaced.
            let len = encoded(s);
            if root || len > SPLIT { return Ok((link(put(log, &serde_json::to_vec(s)?)?), REF_LEN)); }
            return Ok((value.clone(), len));
        },
        _ => { let len = encoded(value); (value.clone(), len) },
    };
    if root || len > SPLIT { Ok((link(put(log, &serde_json::to_vec(&node)?)?), REF_LEN)) } else { Ok((node, len)) }
}

/// A data key as stored: `$k` becomes `$$k`, so no stored data key equals `MARKER`.
fn escape(key: &str) -> std::borrow::Cow<'_, str> {
    if key.starts_with('$') { format!("${key}").into() } else { key.into() }
}

/// A stored key as data. A single-`$` key that is not a reference is damage.
pub fn unescape(key: &str) -> Result<&str> {
    match key.strip_prefix('$') {
        Some(rest) if rest.starts_with('$') => Ok(rest),
        Some(_) => bail!("malformed stored key {key:?}"),
        None => Ok(key),
    }
}

fn link(sha: String) -> Value { Value::Object(Map::from_iter([(MARKER.to_string(), Value::String(sha))])) }

/// `{"$hx.blob":"<64 hex>"}`
const REF_LEN: usize = 2 + MARKER.len() + 2 + 1 + 66;

/// The exact length of `value`'s compact JSON encoding, without building it.
fn encoded(value: &impl serde::Serialize) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> { self.0 += buf.len(); Ok(buf.len()) }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("JSON values encode");
    count.0
}

/// The tree stored under `sha`, references expanded. Every blob's content hash is verified as it is read; a subtree
/// stored once and referenced twice is read once.
pub fn load(dir: &Path, sha: &str) -> Result<Value> {
    expand_blob(dir, sha, &mut HashMap::new(), 0)
}

fn expand_blob(dir: &Path, sha: &str, seen: &mut HashMap<String, Value>, depth: usize) -> Result<Value> {
    if let Some(value) = seen.get(sha) { return Ok(value.clone()); }
    let bytes = eventlog::read_blob(dir, sha).with_context(|| format!("stored state blob {sha}"))?;
    let value = expand(dir, serde_json::from_slice(&bytes).with_context(|| format!("stored state blob {sha}: invalid JSON"))?, seen, depth)?;
    seen.insert(sha.into(), value.clone());
    Ok(value)
}

fn expand(dir: &Path, value: Value, seen: &mut HashMap<String, Value>, depth: usize) -> Result<Value> {
    ensure!(depth <= MAX_DEPTH, "stored state nests deeper than {MAX_DEPTH} levels");
    if let Some(sha) = reference(&value)? { return expand_blob(dir, sha, seen, depth + 1); }
    Ok(match value {
        Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| Ok((unescape(&k)?.to_string(), expand(dir, v, seen, depth + 1)?))).collect::<Result<_>>()?),
        Value::Array(items) => Value::Array(items.into_iter().map(|v| expand(dir, v, seen, depth + 1)).collect::<Result<_>>()?),
        value => value,
    })
}

/// Check that the tree under `sha` is complete: every blob it references exists and matches its hash. Blobs in `seen`
/// were checked already (trees of one run share most of theirs). Returns how many blobs this call read.
pub fn verify(dir: &Path, sha: &str, seen: &mut HashSet<String>) -> Result<usize> {
    let mut todo = vec![(sha.to_string(), 0)];
    let mut read = 0;
    while let Some((sha, depth)) = todo.pop() {
        if !seen.insert(sha.clone()) { continue; }
        ensure!(depth <= MAX_DEPTH, "stored state nests deeper than {MAX_DEPTH} levels");
        let value: Value = serde_json::from_slice(&eventlog::read_blob(dir, &sha).with_context(|| format!("blob {sha}"))?)
            .with_context(|| format!("blob {sha}: invalid JSON"))?;
        read += 1;
        let mut stack = vec![&value];
        while let Some(v) = stack.pop() {
            if let Some(child) = reference(v)? { todo.push((child.to_string(), depth + 1)); continue; }
            match v { Value::Object(map) => stack.extend(map.values()), Value::Array(items) => stack.extend(items), _ => {} }
        }
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn log(tag: &str) -> std::sync::Arc<EventLog> {
        let dir = std::env::temp_dir().join(format!("hx-cas-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        EventLog::open(&dir).unwrap()
    }

    #[test]
    fn large_subtrees_are_stored_once_and_expand_exactly() {
        let log = log("roundtrip");
        let image = "i".repeat(100_000);
        let message = |n: usize| json!({"role":"tool","content":format!("{n}{}", "x".repeat(2000)),"images":[{"data":image,"path":"a.png"}]});
        let small = json!({"status":"running","controller":{"n":1,"messages":[{"role":"user","content":"hi"}]},"durable":{}});
        let sha = store(&log, &small).unwrap();
        assert_eq!(load(log.dir(), &sha).unwrap(), small, "a small tree is one blob");
        let mut state = json!({"status":"running","controller":{"messages":[message(0), message(1)],"weird":{"\u{1}k\"":[1.5,null,true]}},"durable":{"same":image}});
        let first = store(&log, &state).unwrap();
        let bytes = || std::fs::read_dir(log.dir().join("blobs")).unwrap().map(|e| e.unwrap().metadata().unwrap().len()).sum::<u64>();
        let before = bytes();
        state["controller"]["messages"].as_array_mut().unwrap().push(message(2));
        let second = store(&log, &state).unwrap();
        // the new message's text and the path to it; the image is stored once for all three messages and the durable copy
        assert!(serde_json::to_vec(&state).unwrap().len() > 400_000);
        assert!(bytes() - before < 2000 + 3 * SPLIT as u64, "{}", bytes() - before);
        assert_eq!(load(log.dir(), &second).unwrap(), state);
        assert_ne!(load(log.dir(), &first).unwrap(), state);
        let root: Value = serde_json::from_slice(&eventlog::read_blob(log.dir(), &second).unwrap()).unwrap();
        assert!(serde_json::to_vec(&root).unwrap().len() < SPLIT, "{root}");
        assert!(verify(log.dir(), &second, &mut HashSet::new()).unwrap() >= 5);
        let _ = std::fs::remove_dir_all(log.dir());
    }

    #[test]
    fn dollar_keys_round_trip_and_damage_is_detected() {
        let log = log("markers");
        // A model can put the marker in its own tool-call input: stored as data, never followed as a reference.
        for data in [json!({"a":{MARKER:"0".repeat(64)}}), json!([{MARKER:"x","other":1}]), json!({"$$k":{"$schema":"y".repeat(3000)}})] {
            let sha = store(&log, &data).unwrap();
            assert_eq!(load(log.dir(), &sha).unwrap(), data);
            assert!(verify(log.dir(), &sha, &mut HashSet::new()).is_ok());
        }
        assert!(unescape("$schema").is_err() && unescape("$$schema").unwrap() == "$schema" && unescape("k").unwrap() == "k");
        let state = json!({"messages":["m".repeat(5000), "n".repeat(5000)]});
        let sha = store(&log, &state).unwrap();
        let blobs = log.dir().join("blobs");
        let leaf = eventlog::sha_bytes(&serde_json::to_vec(&"m".repeat(5000)).unwrap());
        std::fs::write(blobs.join(&leaf), "\"tampered\"").unwrap();
        assert!(load(log.dir(), &sha).unwrap_err().to_string().contains(&leaf));
        assert!(verify(log.dir(), &sha, &mut HashSet::new()).is_err());
        std::fs::remove_file(blobs.join(&leaf)).unwrap();
        assert!(load(log.dir(), &sha).is_err());
        assert!(reference(&json!({MARKER: "ab"})).is_err());
        let _ = std::fs::remove_dir_all(log.dir());
    }
}
