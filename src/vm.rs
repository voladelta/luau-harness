//! Luau VM. Bindings load, then sandbox(true). Two environments sit on the read-only globals:
//!   * the SESSION env: the model's persistent variables (what `vars()` lists);
//!   * the MODULE env: where harness modules (std loop, printer, ...) run, with `require` and the `hx` API.
use crate::effects::{Effects, Ret};
use crate::host::Host;
use crate::modules::Resolver;
use crate::policy::{Class, Policy};
use crate::prims;
use anyhow::{Context, Result};
use serde_json::json;
use mlua::{Function, Lua, MultiValue, Table, Value, Variadic, VmState};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

/// Luau CPU allowance of one agent chunk (time waiting on effects does not count).
pub const CHUNK_SECS: u64 = 120;
/// `view(path, {max = N})`: the long side images are scaled to, by default and at least / at most.
pub const VIEW_MAX: (u32, u32, u32) = (1024, 64, 2000);

pub(crate) const PRELUDE: &str = r##"
function print(...)
	local n = select("#", ...)
	local parts = table.create(n)
	for i = 1, n do
		parts[i] = __format((select(i, ...)))
	end
	__emit(table.concat(parts, "\t"))
end

-- Nothing the model or a package runs may reach the real globals or load code around the loader.
getfenv = nil
setfenv = nil
loadstring = nil
require = nil

function search(pattern, opts)
	local rows, truncated, err = __search_raw(pattern, opts)
	if err then
		setmetatable(err, { __tostring = function(e) return e.kind .. ": " .. e.message .. " (hint: " .. e.hint .. ")" end })
		error(err, 2)
	end
	return rows, truncated
end
"##;

pub struct RunResult {
    pub output: String,
    /// Bytes `print` wrote that `output` does not hold (see `Printed`).
    pub omitted: usize,
    pub values: Vec<Value>,
    pub error: Option<String>,
    pub ops: usize,
}

/// Bytes of `print` output one chunk keeps: the first and the last half. The buffer lives outside the Lua memory
/// limit, so it must be bounded by itself.
const PRINT_CAP: usize = 1 << 20;

/// What `print` wrote during one chunk: the first and last `PRINT_CAP / 2` bytes, and how many bytes there were.
#[derive(Default)]
struct Printed {
    head: Vec<u8>,
    tail: Vec<u8>,
    total: usize,
}

impl Printed {
    fn push(&mut self, bytes: &[u8]) {
        let half = PRINT_CAP / 2;
        self.total = self.total.saturating_add(bytes.len());
        let take = bytes.len().min(half - self.head.len());
        self.head.extend_from_slice(&bytes[..take]);
        let rest = &bytes[take..];
        if rest.len() >= half {
            self.tail.clear();
            self.tail.extend_from_slice(&rest[rest.len() - half..]);
        } else {
            self.tail.extend_from_slice(rest);
            // Trim only once the tail doubles, so appending stays amortized O(1).
            if self.tail.len() > 2 * half {
                self.tail.drain(..self.tail.len() - half);
            }
        }
    }

    /// The captured text (invalid UTF-8 replaced) and the number of bytes left out of its middle; empties the buffer.
    fn take(&mut self) -> (String, usize) {
        let Printed { mut head, tail, total } = std::mem::take(self);
        if total <= PRINT_CAP {
            head.extend_from_slice(&tail);
            return (String::from_utf8_lossy(&head).into_owned(), 0);
        }
        // Over the cap the head is full and the tail holds at least half; cut both on character boundaries.
        let tail = &tail[tail.len() - PRINT_CAP / 2..];
        let head = &head[..whole_prefix(&head)];
        let tail = &tail[tail.iter().take(3).take_while(|&&b| is_continuation(b)).count()..];
        let omitted = total - head.len() - tail.len();
        let text = format!(
            "{}\n…[hx: {omitted} bytes omitted from the middle of the printed output]…\n{}",
            String::from_utf8_lossy(head),
            String::from_utf8_lossy(tail)
        );
        (text, omitted)
    }
}

fn is_continuation(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Length of `b` without a UTF-8 sequence cut off at its end.
fn whole_prefix(b: &[u8]) -> usize {
    for back in 1..=b.len().min(4) {
        let lead = b[b.len() - back];
        if !is_continuation(lead) {
            let width = match lead {
                0xF0.. => 4,
                0xE0.. => 3,
                0xC0.. => 2,
                _ => 1,
            };
            return if width > back { b.len() - back } else { b.len() };
        }
    }
    b.len()
}

/// Threads awaiting an effect, as `Effects::waiting` holds them.
type Waiting = Arc<Mutex<HashMap<usize, (usize, mlua::Thread)>>>;

/// Interrupts between two budget checks. Luau interrupts on every loop backedge and call, so most interrupts must
/// cost no more than a counter; a deadline is noticed at most this many interrupts late.
const INTERRUPT_STRIDE: u32 = 1024;

/// The time budgets one Lua state's interrupt enforces.
pub(crate) struct Budget {
    /// The run's absolute deadline on the `supervisor::now()` clock.
    pub run_deadline: Option<f64>,
    /// Deadline of the code running now (a chunk, a package call, a controller step); `None` while nothing is timed.
    pub deadline: Arc<Mutex<Option<Instant>>>,
    /// Threads awaiting an effect do not spend the allowance (see `Effects::awaiting`).
    pub waiting: Option<Waiting>,
    /// The error raised when `deadline` has passed.
    pub exceeded: &'static str,
}

impl Budget {
    fn check(&self, lua: &Lua) -> mlua::Result<VmState> {
        if self.run_deadline.is_some_and(|d| crate::supervisor::now() >= d) {
            return Err(mlua::Error::runtime("budget_exceeded: absolute run deadline reached"));
        }
        let deadline = *self.deadline.lock().unwrap();
        if deadline.is_none_or(|d| Instant::now() < d) {
            return Ok(VmState::Continue);
        }
        if let Some(waiting) = &self.waiting
            && waiting.lock().unwrap().contains_key(&(lua.current_thread().to_pointer() as usize))
        {
            return Ok(VmState::Continue);
        }
        Err(mlua::Error::runtime(self.exceeded))
    }

    /// Make this the interrupt of `lua`.
    pub(crate) fn install(self, lua: &Lua) {
        let ticks = std::cell::Cell::new(0u32);
        lua.set_interrupt(move |lua| {
            let tick = ticks.get().wrapping_add(1);
            ticks.set(tick);
            if !tick.is_multiple_of(INTERRUPT_STRIDE) {
                return Ok(VmState::Continue);
            }
            self.check(lua)
        });
    }
}

/// The metatable of every environment model, module and package code runs in. Reads fall through to a read-only
/// copy of `globals` that leaves out `_G` (the real globals) and `coroutine` (mlua's async callbacks need the
/// library, code must not get it), and `getmetatable` returns only "locked".
pub(crate) fn locked_env(lua: &Lua, globals: &Table) -> mlua::Result<Table> {
    let view = lua.create_table()?;
    for pair in globals.pairs::<Value, Value>() {
        let (key, value) = pair?;
        if matches!(&key, Value::String(s) if matches!(s.as_bytes().as_ref(), b"_G" | b"coroutine")) {
            continue;
        }
        view.raw_set(key, value)?;
    }
    view.set_readonly(true);
    let metatable = lua.create_table()?;
    metatable.raw_set("__index", view)?;
    metatable.raw_set("__metatable", "locked")?;
    Ok(metatable)
}

#[cfg(test)]
mod session_budget_tests {
    use super::*;

    #[tokio::test]
    async fn awaiting_an_effect_does_not_spend_the_luau_cpu_allowance() {
        let dir = std::env::temp_dir().join(format!("hx-chunk-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Arc::new(Policy::new(&dir, false, None, None, None).unwrap());
        let vm = Vm::new(policy, Duration::from_millis(30), 32 << 20, None).unwrap();
        let waited = vm.session.run("local r=sh('sleep 0.1; echo waited'); assert(r.code==0); print(r.stdout)").await;
        assert!(waited.error.is_none(), "{:?}", waited.error);
        assert!(waited.output.contains("waited"));
        let spun = vm.session.run("sh('sleep 0.1'); while true do end").await;
        assert!(spun.error.as_deref().is_some_and(|e| e.contains("time budget exceeded")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn isolated_package_wait_preserves_the_callers_cpu_allowance() {
        let dir = std::env::temp_dir().join(format!("hx-package-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Arc::new(Policy::new(&dir, false, None, None, None).unwrap());
        let vm = Vm::new(policy.clone(), Duration::from_millis(30), 32 << 20, None).unwrap();
        let mut registry = crate::modules::Registry::capture(&dir).unwrap();
        let caps = crate::supervisor::Capabilities::from([crate::supervisor::Capability::ProcSh]);
        registry.capabilities.insert("probe".into(), caps.clone());
        registry.sources.insert("probe".into(), crate::modules::Source {
            text: "return {run=function() return sh('sleep 0.1; echo waited').stdout end}".into(),
            origin: "probe/init.luau".into(), is_override: false, pkg: Some(("probe".into(), caps)),
        });
        let supervisor = crate::supervisor::Supervisor::ephemeral(policy, Arc::new(registry));
        let caller = vm.effects.clone();
        let proxy = crate::packages::proxy(&vm.lua, supervisor, None, "probe", caller).unwrap();
        vm.session.env.set("probe", proxy).unwrap();
        let waited = vm.session.run("assert(probe.run():find('waited')); print('ok')").await;
        assert!(waited.error.is_none(), "{:?}", waited.error);
        let spun = vm.session.run("probe.run(); while true do end").await;
        assert!(spun.error.as_deref().is_some_and(|e| e.contains("time budget exceeded")));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    fn vm(name: &str, budget: Duration, mem_limit: usize) -> (Vm, PathBuf) {
        let dir = std::env::temp_dir().join(format!("hx-boundary-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Arc::new(Policy::new(&dir, false, None, None, None).unwrap());
        (Vm::new(policy, budget, mem_limit, None).unwrap(), dir)
    }

    #[tokio::test]
    async fn a_result_that_is_not_utf8_still_reaches_the_model() {
        let (vm, dir) = vm("utf8", Duration::from_secs(5), 64 << 20);
        let r = vm.run_chunk("local s = 'café au lait'; print(s:sub(1, 4)); return s:sub(1, 4)").await.unwrap();
        let feedback = r["feedback"].as_str().unwrap();
        assert!(feedback.contains("=> caf\u{FFFD}"), "{feedback}");
        assert!(r["output"].as_str().unwrap().starts_with("caf\u{FFFD}"), "{r}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_value_with_raising_metamethods_is_summarised_without_running_them() {
        let (vm, dir) = vm("raise", Duration::from_secs(5), 64 << 20);
        let code = "print('before'); return setmetatable({}, {__index = function() error('strict table') end, __tostring = function() error('no') end})";
        let r = vm.run_chunk(code).await.unwrap();
        let feedback = r["feedback"].as_str().unwrap();
        assert!(feedback.starts_with("before\n=> {}"), "{feedback}");
        assert!(!feedback.contains("hx could not format"), "{feedback}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn feedback_that_cannot_be_composed_falls_back_to_output_and_error() {
        let result = RunResult { output: "before\n".into(), omitted: 7, values: vec![Value::Integer(1)], error: Some("chunk failed".into()), ops: 0 };
        let feedback = fallback_feedback(&result, &mlua::Error::runtime("printer broke\nstack traceback: ..."));
        assert_eq!(
            feedback,
            "before\nnote: 7 bytes printed in the middle of this chunk were dropped\nerror: chunk failed\n\
             (hx could not format this result: runtime error: printer broke; the returned values are not shown)\n"
        );
    }

    #[tokio::test]
    async fn harness_luau_between_chunks_runs_under_a_deadline() {
        let (vm, dir) = vm("compose-deadline", Duration::from_millis(300), 64 << 20);
        let t0 = Instant::now();
        let spin = vm.lua.load("while true do end").set_environment(vm.modenv.clone());
        let error = {
            let _harness = vm.session.harness();
            spin.exec().unwrap_err()
        };
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert!(error.to_string().contains("time budget exceeded"), "{error}");
        // the deadline is cleared again: harness code between chunks is not left on a stale clock
        assert!(vm.session.deadline.lock().unwrap().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_chunk_that_fills_memory_still_gets_feedback() {
        let (vm, dir) = vm("memory", Duration::from_secs(10), 32 << 20);
        let r = vm.run_chunk("keep = {}; for i = 1, 1e8 do keep[i] = string.rep('x', 64) .. i end").await.unwrap();
        let feedback = r["feedback"].as_str().unwrap();
        assert!(feedback.contains("error:") && feedback.contains("memory"), "{feedback}");
        assert!(!feedback.contains("hx could not format"), "{feedback}");
        // the model's data still fills the allowance; the next chunk's feedback and the request hooks still work
        let next = vm.exec_text("return #keep > 0").await.unwrap();
        assert!(next.starts_with("=> true\n") || next.contains("memory"), "{next}");
        let request = json!({"system": "s", "messages": [{"role": "user", "content": "hi"}], "tools": []});
        assert_eq!(vm.prepare_request(&request).unwrap()["messages"], request["messages"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn print_output_is_bounded_and_the_feedback_says_so() {
        let (vm, dir) = vm("print", Duration::from_secs(30), 64 << 20);
        let r = vm.session.run("for i = 1, 30000 do print(string.rep('é', 50)) end").await;
        let printed = 30000 * 101;
        assert!(r.output.len() < PRINT_CAP + 200 && r.omitted > printed - PRINT_CAP - 8, "{} {}", r.output.len(), r.omitted);
        assert!(!r.output.contains('\u{FFFD}'), "cut inside a character");
        assert!(r.output.contains(&format!("[hx: {} bytes omitted", r.omitted)));
        let text = vm.exec_text("for i = 1, 30000 do print(string.rep('é', 50)) end").await.unwrap();
        assert!(text.contains(&format!("note: {} bytes printed in the middle of this chunk were dropped", r.omitted)), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn printed_keeps_head_and_tail_on_character_boundaries() {
        let mut p = Printed::default();
        p.push(b"ab");
        p.push(b"\n");
        assert_eq!(p.take(), ("ab\n".to_string(), 0));
        let line = "€".repeat(1000); // 3 bytes per character, so the halves fall inside one
        for _ in 0..1000 {
            p.push(line.as_bytes());
        }
        let (text, omitted) = p.take();
        assert!(!text.contains('\u{FFFD}'));
        let kept: usize = text.split('\n').filter(|s| !s.contains("omitted")).map(str::len).sum();
        assert_eq!(kept + omitted, 3_000_000);
        assert_eq!(p.take(), (String::new(), 0));
    }

    #[test]
    fn a_failed_durable_set_keeps_the_previous_value() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let (vm, dir) = vm("durable", Duration::from_secs(10), 128 << 20);
        let code = "durable.set('k', {n = 1})
            local big = string.rep('x', 17 * 1024 * 1024)
            assert(not pcall(durable.set, 'k', big))
            assert(not pcall(durable.set, 'fresh', big))
            assert(durable.get('k').n == 1 and durable.get('fresh') == nil)";
        let r = rt.block_on(vm.session.run(code));
        assert!(r.error.is_none(), "{:?}", r.error);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_session_sandbox_holds() {
        let (vm, dir) = vm("sandbox", Duration::from_secs(5), 64 << 20);
        let probes = [
            "_G == nil and coroutine == nil and getfenv == nil and setfenv == nil",
            "loadstring == nil and require == nil and hx == nil",
            "not pcall(function() string.lower = nil end) and string.lower('A') == 'a'",
            "not pcall(function() getmetatable('').__index = {} end)",
            "not pcall(function() math.floor = nil end)",
            "not pcall(function() durable.get = nil end)",
        ];
        for probe in probes {
            let r = vm.session.run(&format!("return {probe}")).await;
            assert!(r.error.is_none() && matches!(r.values.as_slice(), [Value::Boolean(true)]), "{probe}: {:?}", r.error);
        }
        // a copied array's tag cannot be read, removed or reproduced
        crate::values::copy_in(&vm.lua, &json!([1, 2])).and_then(|a| vm.session.env.raw_set("arr", a)).unwrap();
        let r = vm.session.run("return getmetatable(arr) == 'native array' and not pcall(setmetatable, arr, nil)").await;
        assert!(matches!(r.values.as_slice(), [Value::Boolean(true)]), "{:?}", r.error);
        let forged = vm.session.run("durable.set('x', setmetatable({}, {__hx_array = true}))").await;
        assert!(forged.error.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// The model's persistent environment plus the budgets that apply to each chunk.
pub struct Session {
    lua: Lua,
    log: Arc<crate::eventlog::EventLog>,
    env: Table,
    out: Arc<Mutex<Printed>>,
    ops: Arc<AtomicUsize>,
    deadline: Arc<Mutex<Option<Instant>>>,
    budget: Duration,
    mem_limit: usize,
    /// chunks run so far (see `hx.retain`)
    chunks: Arc<AtomicU64>,
}

impl Session {
    /// Compile `code`; if its last line is an expression, return it so the value is captured.
    fn compile(&self, code: &str) -> mlua::Result<Function> {
        let load = |src: &str| self.lua.load(src).set_name("=chunk").set_environment(self.env.clone()).into_function();
        let trimmed = code.trim_end();
        if let Some(idx) = trimmed.rfind('\n') {
            let (head, last) = trimmed.split_at(idx + 1);
            // Only rewrite when `head` is a complete chunk by itself; otherwise the last line closes a block
            // (`end`) and `return end` would silently exit the loop/if body early.
            if self.lua.load(head).set_name("=chunk").into_function().is_ok()
                && let Ok(f) = load(&format!("{head}return {last}")) {
                    return Ok(f);
                }
        } else if let Ok(f) = load(&format!("return {trimmed}")) {
            return Ok(f);
        }
        load(trimmed)
    }

    pub async fn run(&self, code: &str) -> RunResult {
        self.out.lock().unwrap().take();
        self.ops.store(0, Ordering::Relaxed);
        self.chunks.fetch_add(1, Ordering::Relaxed);
        *self.deadline.lock().unwrap() = Some(Instant::now() + self.budget);
        let result = match self.compile(code) {
            Ok(f) => f.call_async::<MultiValue>(()).await,
            Err(e) => Err(e),
        };
        *self.deadline.lock().unwrap() = None;
        let (output, omitted) = self.out.lock().unwrap().take();
        let (values, error) = match result {
            Ok(vals) => (vals.into_iter().filter(|v| !v.is_nil()).collect(), None),
            Err(e) => (vec![], Some(clean_error(&e))),
        };
        RunResult { output, omitted, values, error, ops: self.ops.load(Ordering::Relaxed) }
    }

    /// Time and memory for harness Luau that runs between chunks (feedback, request hooks). It can run the model's
    /// metamethods, so it gets a deadline of its own; and the model may have filled its memory allowance, so it gets
    /// `HARNESS_HEADROOM` above what is in use. Both are restored when the guard drops.
    fn harness(&self) -> HarnessGuard<'_> {
        let used = self.lua.used_memory();
        self.lua.set_memory_limit(used.max(self.mem_limit).saturating_add(HARNESS_HEADROOM)).expect(MEMORY_CONTROL);
        *self.deadline.lock().unwrap() = Some(Instant::now() + self.budget.min(HARNESS_TIME));
        HarnessGuard { session: self }
    }
}

/// CPU allowance and memory headroom of the harness Luau around a chunk (see `Session::harness`).
const HARNESS_TIME: Duration = Duration::from_secs(10);
const HARNESS_HEADROOM: usize = 64 << 20;
const MEMORY_CONTROL: &str = "Vm::new set a memory limit, so the allocator tracks memory";

struct HarnessGuard<'a> {
    session: &'a Session,
}

impl Drop for HarnessGuard<'_> {
    fn drop(&mut self) {
        *self.session.deadline.lock().unwrap() = None;
        self.session.lua.set_memory_limit(self.session.mem_limit).expect(MEMORY_CONTROL);
    }
}

/// Characters of a chunk's output and error that `fallback_feedback` shows: std/printer's `OUTPUT_CAP`.
const FALLBACK_CHARS: usize = 12_000;

/// The feedback of a chunk whose std/feedback rendering failed: its output and error, cut in Rust, and why the
/// values are missing.
fn fallback_feedback(result: &RunResult, why: &mlua::Error) -> String {
    let mut text = clip(&result.output, FALLBACK_CHARS * 2 / 3);
    if result.omitted > 0 {
        text.push_str(&format!("note: {} bytes printed in the middle of this chunk were dropped\n", result.omitted));
    }
    if let Some(error) = &result.error {
        text.push_str(&format!("error: {}\n", clip(error, FALLBACK_CHARS / 3)));
    }
    let why = clean_error(why);
    let why = clip(why.lines().next().unwrap_or(""), 300);
    let lost = if result.values.is_empty() { "" } else { "; the returned values are not shown" };
    text.push_str(&format!("(hx could not format this result: {why}{lost})\n"));
    text
}

/// `s` cut to at most `max` characters: its start and end around a marker.
fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let half = max / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(n - half).collect();
    format!("{head}\n…[{} characters omitted]…\n{tail}", n - 2 * half)
}

fn clean_error(e: &mlua::Error) -> String {
    match e {
        mlua::Error::CallbackError { cause, traceback } => format!("{}\n{}", clean_error(cause), traceback.trim_end()),
        other => other.to_string(),
    }
}

#[derive(Clone)]
struct ToolDef {
    source: String,
    doc: String,
    signature: String,
    types: String,
    tests: String,
    caps: crate::supervisor::Capabilities,
}

fn opt_string(o: &Option<Table>, k: &str) -> String {
    prims::opt_str(o, k).unwrap_or_default()
}

fn stderr_print(_: &Lua, args: Variadic<Value>) -> mlua::Result<()> {
    eprintln!("{}", args.iter().map(|v| v.to_string().unwrap_or_default()).collect::<Vec<_>>().join("\t"));
    Ok(())
}

/// opts.caps = { fs = {"read"}, proc = {"sh"} }; an unknown capability is an error.
fn caps_of(o: &Option<Table>) -> mlua::Result<crate::supervisor::Capabilities> {
    let mut out = BTreeMap::new();
    if let Some(Value::Table(caps)) = o.as_ref().map(|t| t.get::<Value>("caps")).transpose()? {
        for pair in caps.pairs::<String, Vec<String>>() {
            let (k, v) = pair?;
            out.insert(k, v);
        }
    }
    crate::supervisor::Capabilities::from_map(&out).map_err(mlua::Error::external)
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn render_manifest(name: &str, d: &ToolDef) -> String {
    let mut t = format!("[package]\nname = {}\nversion = \"0.1.0\"\nkind = \"tool\"\ndescription = {}\n\n[tool]\nsignature = {}\ndoc = {}\n\n[capabilities]\n", toml_str(name), toml_str(&d.doc), toml_str(&d.signature), toml_str(&d.doc));
    for (k, v) in d.caps.to_map() {
        t.push_str(&format!("{k} = [{}]\n", v.iter().map(|x| toml_str(x)).collect::<Vec<_>>().join(", ")));
    }
    t
}

pub struct Vm {
    pub lua: Lua,
    pub session: Session,
    modenv: Table,
    tool_prompt: String,
    #[cfg(test)]
    effects: Arc<Effects>,
}

impl Vm {
    pub fn new(policy: Arc<Policy>, budget: Duration, mem_limit: usize, host: Option<Arc<Host>>) -> Result<Vm> {
        let root: PathBuf = policy.workspace.clone();
        let lua = Lua::new();
        let out = Arc::new(Mutex::new(Printed::default()));
        let ops = Arc::new(AtomicUsize::new(0));
        let deadline: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));
        let resolver = Arc::new(Resolver::new(root.clone()));
        let supervisor = match &host {
            Some(h) => h.supervisor.clone(),
            None => {
                let mut registry = crate::modules::Registry::capture(&root)?;
                registry.materialize_skills(&policy.log)?;
                crate::supervisor::Supervisor::ephemeral(policy.clone(), Arc::new(registry))
            },
        };
        let registry = supervisor.registry.clone();
        let g = lua.globals();
        let principal = host.as_ref().map(|h| h.principal.clone()).unwrap_or(crate::supervisor::Principal::Agent { read_only: false });
        let fx = Effects::supervised(supervisor.clone(), principal, host.as_ref().map(|h| h.depth).unwrap_or(0), deadline.clone());

        // --- session-side builtins (visible to the model) ---
        let session_env = lua.create_table()?;
        let buf = out.clone();
        g.set("__emit", lua.create_function(move |_, s: mlua::LuaString| {
            let mut b = buf.lock().unwrap();
            b.push(&s.as_bytes());
            b.push(b"\n");
            Ok(())
        })?)?;
        // Use the frozen std/printer module; fall back to tostring if it fails.
        let printer: Arc<Mutex<Option<Function>>> = Arc::new(Mutex::new(None));
        let require_cell: Arc<Mutex<Option<Function>>> = Arc::new(Mutex::new(None));
        let (pf, rc) = (printer.clone(), require_cell.clone());
        g.set("__format", lua.create_function(move |lua, v: Value| {
            if pf.lock().unwrap().is_none() {
                let req = rc.lock().unwrap().clone();
                if let Some(req) = req
                    && let Ok(Value::Table(m)) = req.call::<Value>("std/printer")
                        && let Ok(f) = m.get::<Function>("top") {
                            *pf.lock().unwrap() = Some(f);
                        }
            }
            let f = pf.lock().unwrap().clone();
            if let Some(f) = f
                && let Ok(s) = f.call::<mlua::LuaString>(v.clone()) {
                    return Ok(s.to_string_lossy());
                }
            Ok(lua.coerce_string(v.clone())?.map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| format!("<{}>", v.type_name())))
        })?)?;
        let env_for_vars = session_env.clone();
        let exposed: Arc<Mutex<std::collections::HashSet<String>>> = Arc::new(Mutex::new(Default::default()));
        let ex_vars = exposed.clone();
        g.set("vars", lua.create_function(move |lua, ()| {
            let t = lua.create_table()?;
            for (k, v) in env_for_vars.pairs::<Value, Value>().flatten() {
                if matches!(&k, Value::String(s) if !s.to_string_lossy().starts_with('_') && !ex_vars.lock().unwrap().contains(&*s.to_string_lossy())) {
                    t.set(k, v)?;
                }
            }
            Ok(t)
        })?)?;
        if let Some(h) = host.clone().filter(|h| h.depth == 0) {
            let hfx = fx.clone();
            let hops = ops.clone();
            g.set("history", lua.create_async_function(move |lua, input: Option<Value>| {
                let (h, fx, ops) = (h.clone(), hfx.clone(), hops.clone());
                async move {
                    let args = input.map(crate::values::copy_out).transpose()?.unwrap_or(json!({}));
                    let query = crate::history::Query::parse(args.clone()).map_err(mlua::Error::external)?;
                    ops.fetch_add(1, Ordering::Relaxed);
                    let result = fx.run(&lua, crate::supervisor::Tool::History, Class::Read, args, || async move {
                        h.policy.check_read(&h.policy.log.path.to_string_lossy()).map_err(mlua::Error::external)?;
                        let deadline = h.supervisor.manifest.as_ref().and_then(|m| m.deadline);
                        let value = tokio::task::spawn_blocking(move || h.supervisor.history.query(
                            h.policy.log.dir(), &h.policy.log.session, query, deadline)).await
                            .map_err(mlua::Error::external)?.map_err(mlua::Error::external)?;
                        Ok(Ret::Json(value))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
        }
        let (bs, bh, bfx, bops) = (supervisor.clone(), host.clone(), fx.clone(), ops.clone());
        g.set("batch", lua.create_async_function(move |lua, input: Value| {
            let (s, h, fx, ops) = (bs.clone(), bh.clone(), bfx.clone(), bops.clone());
            async move {
                s.authorize_stage(fx.stage_epoch,crate::supervisor::Tool::Read,Class::Read).map_err(mlua::Error::external)?;
                let _active = crate::workflow_runtime::EffectGuard::new(&s);
                let operations = crate::values::copy_out(input)?;
                let _wait = fx.awaiting(&lua);
                crate::batch::run(&fx, &lua, h, &ops, operations).await
            }
        })?)?;
        if host.as_ref().is_some_and(|h|h.is_workflow_root()) {
            let stage = lua.create_table()?;
            let frame = supervisor.workflow.lock().unwrap().clone().unwrap();
            if let Some(active) = frame.stage.as_ref().map(|a| &a.spec) {
                stage.set("id",active.id.clone())?;
                stage.set("name",active.name.clone())?;
                stage.set("checks",crate::values::copy_in(&lua,&serde_json::to_value(&frame.check_plan).map_err(mlua::Error::external)?)?)?;
            }
            let owner = supervisor.clone();
            let epoch = supervisor.workflow_epoch();
            stage.set("submit",lua.create_function(move |_,value:Value| {
                let payload=crate::values::copy_out(value)?;
                owner.submit_stage(epoch,payload).map_err(mlua::Error::external)
            })?)?;
            stage.set_readonly(true);
            g.set("stage",stage)?;
        }
        if policy.episode.is_some() && host.as_ref().is_some_and(|h| h.depth == 0) {
            // ar.eval(note) is the only way an autoresearch episode scores a state, ar.claim(idea) claims what a worker tests,
            // and ar.reset() restores the worker's best state (see ar.rs).
            let ar = lua.create_table()?;
            let afx = fx.clone();
            ar.set("eval", lua.create_async_function(move |lua, note: Option<String>| {
                let fx = afx.clone();
                async move {
                    let note = note.unwrap_or_default();
                    let result = fx.run(&lua, crate::supervisor::Tool::Sh, Class::LocalWrite, json!({"ar_eval": &note}), || async {
                        crate::ar::eval(&fx.pol, note.clone()).await.map(Ret::Json).map_err(|e| crate::effects::rt(format!("ar.eval: {e}")))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
            let afx = fx.clone();
            let ahost = host.clone().expect("autoresearch episodes run at depth 0 with a host");
            ar.set("claim", lua.create_async_function(move |lua, idea: String| {
                let (fx, host) = (afx.clone(), ahost.clone());
                async move {
                    let result = fx.run(&lua, crate::supervisor::Tool::Sh, Class::LocalWrite, json!({"ar_claim": &idea}), || async {
                        crate::ar::claim(&host, idea.clone()).await.map(Ret::Json).map_err(|e| crate::effects::rt(format!("ar.claim: {e}")))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
            let afx = fx.clone();
            let cpol = policy.clone();
            ar.set("claims", lua.create_function(move |lua, ()| {
                let listed = crate::ar::claims(&cpol).map_err(|e| crate::effects::rt(format!("ar.claims: {e}")))?;
                prims::json_value(lua, Ret::Json(listed))
            })?)?;
            ar.set("reset", lua.create_async_function(move |lua, ()| {
                let fx = afx.clone();
                async move {
                    let result = fx.run(&lua, crate::supervisor::Tool::Sh, Class::LocalWrite, json!({"ar_reset": true}), || async {
                        crate::ar::reset(&fx.pol).map(Ret::Json).map_err(|e| crate::effects::rt(format!("ar.reset: {e}")))
                    }).await?;
                    prims::json_value(&lua, result)
                }
            })?)?;
            ar.set_readonly(true);
            g.set("ar", ar)?;
        }
        let saved = host.as_ref().map(|h| h.durable.clone()).unwrap_or_else(|| supervisor.durable.clone());
        let durable = lua.create_table()?;
        let data = saved.clone();
        durable.set("get", lua.create_function(move |lua, key: String| {
            match data.lock().unwrap().get(&key) {
                Some(value) => crate::values::copy_in(lua, value),
                None => Ok(Value::Nil),
            }
        })?)?;
        let stage_owner=supervisor.clone();
        let stage_epoch=supervisor.workflow_epoch();
        durable.set("set", lua.create_function(move |_, (key, value): (String, Value)| {
            stage_owner.authorize_stage(stage_epoch,crate::supervisor::Tool::Read,Class::Read).map_err(mlua::Error::external)?;
            let value = crate::values::copy_out(value)?;
            let mut saved = saved.lock().unwrap();
            fn store(saved: &mut serde_json::Value) -> &mut serde_json::Map<String, serde_json::Value> {
                saved.as_object_mut().expect("the durable store is validated as an object when loaded")
            }
            let previous = store(&mut saved).insert(key.clone(), value);
            if let Err(e) = crate::values::validate(&saved) {
                match previous {
                    Some(previous) => store(&mut saved).insert(key, previous),
                    None => store(&mut saved).remove(&key),
                };
                return Err(e);
            }
            Ok(())
        })?)?;
        durable.set_readonly(true);
        g.set("durable", durable)?;
        if let Some(h) = host.clone() {
            let afx = fx.clone();
            g.set("agent", lua.create_async_function(move |lua, (task, opts): (String, Option<Table>)| {
                let (h, afx) = (h.clone(), afx.clone());
                async move {
                    let options = crate::host::AgentOptions::from_lua(opts)?;
                    let child = h.child(&options, afx.principal.clone()).map_err(mlua::Error::external)?;
                    prims::agent(&afx, &lua, child, task, &options).await?.into_lua(&lua)
                }
            })?)?;
        }
        prims::install(&lua, fx.clone(), ops.clone())?;

        // ---- MCP: frozen server configuration; every list/call records a receipt ----
        let mcp = crate::mcp::Mcp::frozen(policy.clone(), registry.mcp.clone())?;
        if let Some(m) = mcp.clone() {
            let (lfx, lm) = (fx.clone(), m.clone());
            g.set("__mcp_list", lua.create_async_function(move |lua, server: String| {
                let (fx, m) = (lfx.clone(), lm.clone());
                async move {
                    let ret = fx.run(&lua, crate::supervisor::Tool::McpList, Class::Read, json!({"server": server}), || async {
                        let tools = m.list_tools(&server).await.map_err(|e| mlua::Error::runtime(e.to_string()))?;
                        Ok(Ret::Json(serde_json::Value::Array(tools)))
                    }).await?;
                    prims::json_value(&lua, ret)
                }
            })?)?;
            let (cfx, cm) = (fx.clone(), m.clone());
            g.set("__mcp_call", lua.create_async_function(move |lua, (server, tool, args): (String, String, Value)| {
                let (fx, m) = (cfx.clone(), cm.clone());
                async move {
                    let args_json = crate::values::copy_out(args)?;
                    let class = if m.is_read_only(&server, &tool) { Class::Read } else { Class::External };
                    let a = json!({"server": server, "tool": tool, "args": args_json});
                    let ret = fx.run(&lua, crate::supervisor::Tool::McpCall, class, a, || async {
                        let call = crate::policy::Call::Mcp { server: &server, tool: &tool };
                        if let Err(why) = fx.pol.approve_call(call, &args_json, class).await {
                            return Err(mlua::Error::runtime(format!("approval_required: {why}")));
                        }
                        let r = m.request(&server, "tools/call", json!({"name": tool, "arguments": args_json})).await
                            .map_err(|e| crate::net::effect_error(class, e.delivery, e.to_string()))?;
                        Ok(Ret::Json(r))
                    }).await?;
                    prims::json_value(&lua, ret)
                }
            })?)?;
        }
        let mcp_names: Vec<String> = mcp.as_ref().map(|m| m.servers.keys().cloned().collect()).unwrap_or_default();
        lua.load(PRELUDE).set_name("=prelude").exec()?;

        lua.sandbox(true)?;
        lua.set_memory_limit(mem_limit)?;
        Budget {
            run_deadline: supervisor.manifest.as_ref().and_then(|m| m.deadline),
            deadline: deadline.clone(),
            waiting: Some(fx.waiting.clone()),
            exceeded: "time budget exceeded (infinite loop?)",
        }
        .install(&lua);
        let gmt = locked_env(&lua, &g)?;
        session_env.set_metatable(Some(gmt.clone()))?;

        // --- module env: require + hx API (not visible to the model's session) ---
        let modenv = lua.create_table()?;
        modenv.set_metatable(Some(gmt))?;
        let loaded = lua.create_table()?;
        let (res, menv, ld, supervisor_for_load, package_host, package_fx) = (registry.clone(), modenv.clone(), loaded.clone(), supervisor.clone(), host.clone(), fx.clone());
        let require = lua.create_function(move |lua, name: String| {
            let cached: Value = ld.get(name.as_str())?;
            if !cached.is_nil() {
                return Ok(cached);
            }
            let load = |src: &crate::modules::Source| -> mlua::Result<Value> {
                if let Some((pkg, _)) = &src.pkg {
                    if name != *pkg { return Err(mlua::Error::runtime("package submodules are private; call exported package methods")); }
                    return crate::packages::proxy(lua, supervisor_for_load.clone(), package_host.clone(), pkg, package_fx.clone()).map(Value::Table);
                }
                let env = menv.clone();
                let f = lua.load(&src.text).set_name(format!("={name}")).set_environment(env).into_function()?;
                f.call(())
            };
            let src = res.resolve(&name).map_err(|e| mlua::Error::runtime(e.to_string()))?;
            let v = match load(&src) {
                // A project override must load and return a table (every harness module does); otherwise fall back
                // to the built-in module so a typo in an override cannot take the harness down.
                Ok(v @ Value::Table(_)) => v,
                bad if src.is_override => {
                    let why = match bad {
                        Err(e) => e.to_string().lines().next().unwrap_or("").to_string(),
                        Ok(v) => format!("returned {} instead of a table", v.type_name()),
                    };
                    eprintln!("warning: override {} is broken ({why}); using the built-in {name}", src.origin);
                    let base = res.resolve(&format!("base:{name}")).map_err(|e| mlua::Error::runtime(e.to_string()))?;
                    load(&base)?
                }
                other => other?,
            };
            ld.set(name.as_str(), v.clone())?;
            Ok(v)
        })?;
        *require_cell.lock().unwrap() = Some(require.clone());
        modenv.set("require", require)?;
        modenv.set("print", lua.create_function(stderr_print)?)?;

        let hx = lua.create_table()?;
        // A pack adds functions/tables to the model's session (not to `vars()`): hx.expose("tests", tests)
        let (sess_env, ex_set) = (session_env.clone(), exposed.clone());
        hx.set("expose", lua.create_function(move |_, (name, v): (String, Value)| {
            sess_env.raw_set(name.as_str(), v)?;
            ex_set.lock().unwrap().insert(name);
            Ok(())
        })?)?;
        // hx.retain(v) -> n: keep a value the printer cut as `_cut[n]` in the session, so its notice can name it. The list
        // belongs to the latest chunk that cut something; the next chunk can read it until it cuts something itself.
        let chunks = Arc::new(AtomicU64::new(0));
        let (cut_env, cut_chunks, cut_seq) = (session_env.clone(), chunks.clone(), Arc::new(Mutex::new(u64::MAX)));
        hx.set("retain", lua.create_function(move |lua, v: Value| {
            let chunk = cut_chunks.load(Ordering::Relaxed);
            let mut seq = cut_seq.lock().unwrap();
            let list = match cut_env.raw_get::<Value>("_cut")? {
                Value::Table(t) if *seq == chunk => t,
                _ => { let t = lua.create_table()?; cut_env.raw_set("_cut", t.clone())?; *seq = chunk; t },
            };
            list.raw_push(v)?;
            Ok(list.raw_len())
        })?)?;
        let session = Session { lua: lua.clone(), log: policy.log.clone(), env: session_env, out, ops, deadline, budget, mem_limit, chunks };
        // hx.view_image(path, max) -> { path, width, height, orig_width, orig_height, mime, data } for the vision pack.
        // Record the live image identity once and return the same loaded bytes.
        let vfx = fx.clone();
        hx.set("view_image", lua.create_async_function(move |lua, (path, max): (String, Option<u32>)| {
            let vfx = vfx.clone();
            async move {
                let max = max.unwrap_or(VIEW_MAX.0).clamp(VIEW_MAX.1, VIEW_MAX.2);
                let pol = vfx.pol.clone();
                let mut live: Option<crate::prims::ViewImage> = None;
                vfx.run_sync(&lua, crate::supervisor::Tool::View, Class::Read, json!({"path": &path, "max": max}), |_| {
                    let v = crate::prims::load_view_image(&pol, &path, max).map_err(mlua::Error::runtime)?;
                    let meta = json!({"sha256": v.sha256, "width": v.width, "height": v.height}).to_string();
                    live = Some(v);
                    Ok(Ret::Str(meta))
                }).await?;
                let v = live.ok_or_else(|| mlua::Error::runtime("view executor returned no image"))?;
                let t = lua.create_table()?;
                t.set("path", path)?;
                t.set("width", v.width)?;
                t.set("height", v.height)?;
                t.set("orig_width", v.orig.0)?;
                t.set("orig_height", v.orig.1)?;
                t.set("mime", v.mime)?;
                t.set("data", v.data_b64)?;
                Ok(t)
            }
        })?)?;
        let h2 = host.clone();
        hx.set("log", lua.create_function(move |_, line: String| {
            match &h2 {
                Some(h) => h.log(&line),
                None => eprintln!("{line}"),
            }
            Ok(())
        })?)?;
        let rroot = registry.clone();
        hx.set("asset", lua.create_function(move |_, name: String| {
            rroot.asset(&name).map_err(mlua::Error::external)
        })?)?;
        if let Some(h) = host.clone() {
            let bh = h.clone();
            hx.set("budget", lua.create_function(move |lua, ()| crate::values::copy_in(lua, &bh.budget_status()))?)?;
            let mh = h.clone();
            hx.set("model_info", lua.create_function(move |lua, ()| crate::values::copy_in(lua, &mh.model_info()))?)?;
        }
        // ---- extension API: installed packages, agent-defined tools, staging for promotion ----
        let pres = registry.clone();
        hx.set("autopilot", policy.autopilot)?;
        hx.set("confined", crate::confine::status().is_ok())?;
        hx.set("limits", crate::values::copy_in(&lua, &prims::prompt_limits())?)?;
        hx.set("mcp_servers", lua.create_function(move |lua, ()| lua.create_sequence_from(mcp_names.clone()))?)?;
        hx.set("packages", lua.create_function(move |lua, ()| crate::values::copy_in(lua, &serde_json::to_value(&pres.packages).map_err(mlua::Error::external)?))?)?;
        let assets = registry.clone();
        hx.set("skill_asset", lua.create_function(move |_, (name, file): (String, String)| assets.asset(&format!("{name}/{file}")).map_err(mlua::Error::external))?)?;
        let defs: Arc<Mutex<HashMap<String, ToolDef>>> = Arc::new(Mutex::new(HashMap::new()));
        let d_defs = defs.clone();
        let deftool = lua.create_function(move |lua, (name, source, opts): (String, String, Option<Table>)| {
            if !crate::modules::valid_name(&name) {
                return Err(mlua::Error::runtime("deftool: the name must be lowercase letters, digits and _ (starting with a letter)"));
            }
            let caps = caps_of(&opts)?;
            lua.load(&source).into_function()?; // syntax check only; unapproved source must never execute
            d_defs.lock().unwrap().insert(name.clone(), ToolDef { source, doc: opt_string(&opts, "doc"), signature: opt_string(&opts, "signature"), types: opt_string(&opts, "types"), tests: opt_string(&opts, "tests"), caps });
            let definition = lua.create_table()?;
            definition.set("name", name)?; definition.set("active", false)?;
            Ok(definition)
        })?;
        let (p_defs, p_res, p_fx) = (defs.clone(), resolver.clone(), fx.clone());
        let promote = lua.create_async_function(move |lua, name: String| {
            let (defs, res, fx) = (p_defs.clone(), p_res.clone(), p_fx.clone());
            async move {
                let def = defs.lock().unwrap().get(&name).cloned().ok_or_else(|| mlua::Error::runtime(format!("promote: no tool named {name:?}; define it with deftool() first")))?;
                let args = json!({"name": name, "source_sha": crate::eventlog::sha_bytes(def.source.as_bytes()), "tests_sha": crate::eventlog::sha_bytes(def.tests.as_bytes())});
                let ret = fx.run(&lua, crate::supervisor::Tool::Promote, Class::LocalWrite, args, || async {
                    if def.tests.trim().is_empty() {
                        return Ok(Ret::Json(json!({"ok": false, "message": "a tool must have tests to be promoted: pass opts.tests to deftool()"})));
                    }
                    let types = format!("-- {}\n-- {}\n{}\n", def.signature, def.doc, def.types);
                    let files = vec![
                        ("hx.toml".to_string(), render_manifest(&name, &def)),
                        ("init.luau".to_string(), def.source.clone()),
                        ("types.d.luau".to_string(), types),
                        ("tests/t.luau".to_string(), def.tests.clone()),
                    ];
                    lua.load(&def.tests).into_function()?;
                    let dir = res.stage(&name, &files).map_err(mlua::Error::external)?;
                    Ok(Ret::Json(json!({
                        "ok": true,
                        "message": format!("staged tool '{name}' without executing unapproved code. Review it, run `hx test {}`, then `hx accept {name}`. It becomes available in a new run.", dir.display()),
                        "tested": false,
                    })))
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?;
        let (u_res, u_fx) = (resolver.clone(), fx.clone());
        let propose_module = lua.create_async_function(move |lua, (module, source, opts): (String, String, Option<Table>)| {
            let (res, fx) = (u_res.clone(), u_fx.clone());
            async move {
                let reason = opt_string(&opts, "reason");
                let args = json!({"module": module, "source_sha": crate::eventlog::sha_bytes(source.as_bytes()), "reason": reason});
                let ret = fx.run(&lua, crate::supervisor::Tool::ProposeModule, Class::LocalWrite, args, || async {
                    Ok(match res.stage_module(&module, &source, &reason) {
                        Ok(_) => Ret::Json(json!({"ok": true, "message": format!("staged a replacement for {module}. It is NOT active: the user reviews it and runs `hx accept {module}`; it applies to new runs.")})),
                        Err(e) => Ret::Json(json!({"ok": false, "message": e.to_string()})),
                    })
                }).await?;
                prims::json_value(&lua, ret)
            }
        })?;
        // the opt-in std/authoring pack exposes these to the model's session (hx.expose)
        let authoring = lua.create_table()?;
        for (n, f) in [("deftool", deftool), ("promote", promote), ("propose_module", propose_module)] {
            authoring.set(n, f)?;
        }
        hx.set("authoring", authoring)?;
        modenv.set("hx", hx)?;
        let lines = crate::tools::install(&lua, &session.env, supervisor, host, fx.clone())?;
        for name in registry.tools.keys() {
            if !session.env.raw_get::<Value>(name.as_str())?.is_nil() { exposed.lock().unwrap().insert(name.clone()); }
        }
        let tool_prompt = if lines.is_empty() { String::new() } else { format!("\n\n# Local tools (user-configured)\n{}", lines.join("\n")) };
        Ok(Vm { lua, session, modenv, tool_prompt, #[cfg(test)] effects: fx })
    }

    /// Install packs (`require(name).install(ctx)`) the way the loop does, for `hx exec --pack`.
    pub async fn install_packs(&self, packs: &[String]) -> Result<()> {
        let f = self
            .lua
            .load("for _, name in {...} do require(name).install({ hooks = require('std/hooks') }) end")
            .set_name("=host")
            .set_environment(self.modenv.clone())
            .into_function()?;
        f.call_async::<()>(MultiValue::from_iter(packs.iter().map(|p| Value::String(self.lua.create_string(p).unwrap())))).await?;
        Ok(())
    }

    /// Run a chunk in the session and render it exactly as the loop would (std/feedback).
    pub async fn exec_text(&self, code: &str) -> Result<String> {
        let (result, feedback) = self.run_with_feedback(code).await?;
        Ok(format!("{feedback}-- ops={} error={}", result.ops, result.error.is_some()))
    }

    /// Run a chunk in the session: its output, error, effect count and the feedback the model sees.
    pub async fn run_chunk(&self, code: &str) -> Result<serde_json::Value> {
        let (result, feedback) = self.run_with_feedback(code).await?;
        Ok(json!({"output":result.output, "error":result.error, "ops":result.ops, "feedback":feedback}))
    }

    /// Run a chunk and compose its feedback. Only a journal failure is an error: whatever the chunk returned, its
    /// result reaches the model as text.
    async fn run_with_feedback(&self, code: &str) -> Result<(RunResult, String)> {
        let result = self.session.run(code).await;
        self.session.log.check()?;
        let feedback = {
            let _harness = self.session.harness();
            self.compose(code, &result).await
        };
        let feedback = feedback.unwrap_or_else(|e| fallback_feedback(&result, &e));
        Ok((result, feedback))
    }

    /// std/feedback's rendering of a chunk's result. It runs the model's metamethods (`__index`, `__tostring`), so it
    /// runs under `Session::harness`, and its text may hold any bytes.
    async fn compose(&self, code: &str, result: &RunResult) -> mlua::Result<String> {
        let value = self.lua.create_table()?;
        value.set("output", result.output.as_str())?;
        value.set("omitted", result.omitted)?;
        value.set("values", self.lua.create_sequence_from(result.values.iter().cloned())?)?;
        value.set("error", result.error.as_deref())?;
        value.set("ops", result.ops)?;
        let compose = self.lua.load("local r, code = ...; return require('std/feedback').compose(r, code)")
            .set_name("=feedback").set_environment(self.modenv.clone()).into_function()?;
        let text: mlua::LuaString = compose.call_async((value, code)).await?;
        Ok(text.to_string_lossy())
    }

    /// Run the `before_model` hooks over a request and append the local tools to its system prompt.
    pub fn prepare_request(&self, request: &serde_json::Value) -> Result<serde_json::Value> {
        let prepared = {
            let _harness = self.session.harness();
            let req = crate::values::copy_in(&self.lua, request)?;
            let prepare = self.lua.load("local request = ...; return require('std/hooks').emit('before_model', request)")
                .set_name("=before_model").set_environment(self.modenv.clone()).into_function()?;
            prepare.call(req)?
        };
        let mut request = crate::values::copy_out(prepared).context("before_model hooks returned a request that is not data")?;
        if let Some(system) = request["system"].as_str() { request["system"] = json!(format!("{system}{}", self.tool_prompt)); }
        Ok(request)
    }

    /// Run test files `(file name, source)` in the module environment, with the agent's authority (`hx test` of the
    /// built-in package or a directory that is not a package; packages run theirs through `packages::run_tests`).
    pub async fn run_tests(&self, tests: &[(String, String)]) -> Result<crate::packages::TestReport> {
        let testlib: Table = self.modenv.get::<Function>("require")?.call("std/testlib")?;
        self.modenv.set("test", testlib.clone())?;
        let mut report = crate::packages::TestReport::default();
        for (name, src) in tests {
            testlib.set("results", self.lua.create_table()?)?;
            let ran = match self.lua.load(src).set_name(format!("={name}")).set_environment(self.modenv.clone()).into_function() {
                Ok(f) => f.call_async::<()>(()).await,
                Err(e) => Err(e),
            };
            report.record(name, ran, &testlib)?;
        }
        Ok(report)
    }
}
