//! In-process runs of the agent loop: a real `Policy`, `Supervisor`, journal and Luau VM over a temporary workspace,
//! with a scripted model (`scripted::Scripted`) in place of the provider. They drive `Host` through its public
//! surface (`start_with`, `resume_with`, `run_task`), so what they check is the loop, the controller and the
//! supervisor around each model call; the binary's edge (flags, exit codes, HTTP and SSE parsing) stays with the
//! check suites under `scripts/`.
pub(crate) mod scripted;

mod budget;
mod compact;
mod finish;
mod model_errors;
mod runs;
mod turns;

use crate::{
    dirs::TestDirs,
    host::{Host, StartOptions},
    llm::{DEFAULT_MAX_OUTPUT, LlmCfg},
    policy::{Budget, Policy},
};
use scripted::Scripted;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// The init file every workspace starts with: the opt-in finish review stays off unless a test turns it on.
const RECHECK_OFF: &str = "require(\"std/recheck\").enabled = false\n";

/// A temporary workspace with its own hx configuration and state directories; all removed on drop.
pub(crate) struct Workspace {
    dirs: TestDirs,
    base: PathBuf,
    pub root: PathBuf,
}

impl Workspace {
    /// An empty workspace whose `.harness/init.luau` turns the finish review off.
    pub(crate) fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("hx-loop-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("ws");
        std::fs::create_dir_all(root.join(".harness")).expect("a temporary workspace");
        let ws = Self {
            dirs: TestDirs::new(),
            base,
            root,
        };
        ws.write(".harness/init.luau", RECHECK_OFF);
        ws
    }

    /// Append `luau` to `.harness/init.luau`.
    pub(crate) fn init(self, luau: &str) -> Self {
        let path = self.root.join(".harness/init.luau");
        let mut init = std::fs::read_to_string(&path).expect("init.luau");
        init.push_str(luau);
        init.push('\n');
        std::fs::write(path, init).expect("init.luau");
        self
    }

    /// Write `contents` to `path` under the workspace, creating its directories.
    pub(crate) fn write(&self, path: &str, contents: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().expect("a file path")).expect("a directory");
        std::fs::write(path, contents).expect("a workspace file");
    }

    /// The trace file of this workspace's runs (outside the workspace, so it is not part of the file tree).
    fn trace(&self) -> PathBuf {
        self.base.join("trace.txt")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Launch settings; the defaults are those of `hx run --model mock --max-turns 30` with no budget.
#[derive(Clone)]
pub(crate) struct Launch {
    pub task: String,
    pub max_turns: usize,
    pub budget: Budget,
    pub context_window: Option<u64>,
    pub max_output: u64,
    /// `--yes`
    pub yes: bool,
}

impl Default for Launch {
    fn default() -> Self {
        Self {
            task: "go".into(),
            max_turns: 30,
            budget: Budget::default(),
            context_window: None,
            max_output: DEFAULT_MAX_OUTPUT,
            yes: false,
        }
    }
}

impl Launch {
    pub(crate) fn max_tokens(tokens: u64) -> Self {
        Self {
            budget: Budget {
                max_tokens: Some(tokens),
                max_wall_secs: None,
            },
            ..Self::default()
        }
    }

    pub(crate) fn max_wall(seconds: f64) -> Self {
        Self {
            budget: Budget {
                max_tokens: None,
                max_wall_secs: Some(seconds),
            },
            ..Self::default()
        }
    }

    fn cfg(&self) -> LlmCfg {
        LlmCfg {
            model: "mock".into(),
            base_url: None,
            key_env: None,
            reasoning: None,
            max_output: self.max_output,
            context_window: self.context_window,
            cache_key: Some("session".into()),
            provider_only: Vec::new(),
        }
    }
}

/// What a run left behind.
pub(crate) struct Ran {
    /// `run_task`'s result: the report, or the error `hx run` would print before exiting nonzero
    pub result: anyhow::Result<Value>,
    /// the run's journal
    pub records: Vec<Value>,
    pub log: PathBuf,
    /// the `--trace` file
    pub trace: String,
}

impl Ran {
    /// The report of a run that ended with one.
    #[track_caller]
    pub(crate) fn report(&self) -> &Value {
        match &self.result {
            Ok(report) => report,
            Err(error) => panic!("the run failed: {error:#}"),
        }
    }

    /// The error of a run that failed.
    #[track_caller]
    pub(crate) fn error(&self) -> String {
        match &self.result {
            Ok(report) => panic!("the run did not fail: {report}"),
            Err(error) => format!("{error:#}"),
        }
    }

    /// The journal records of `kind`.
    pub(crate) fn records(&self, kind: &str) -> Vec<&Value> {
        self.records.iter().filter(|r| r["kind"] == kind).collect()
    }

    /// Lines of the trace that are exactly `line`.
    pub(crate) fn trace_lines(&self, line: &str) -> usize {
        self.trace.lines().filter(|l| *l == line).count()
    }
}

/// Launch a new run in `ws` against `model` and run it to its end.
pub(crate) async fn run(ws: &Workspace, model: &Arc<Scripted>, launch: Launch) -> Ran {
    // the owner trusts the config the test wrote, as `hx trust` would
    crate::trust::record(&ws.root, &ws.dirs).expect("trusted workspace config");
    let policy = Arc::new(
        Policy::new(&ws.dirs, &ws.root, launch.yes, None, None)
            .expect("a policy for the workspace"),
    );
    let host = Host::start_with(
        model.clone(),
        launch.cfg(),
        policy,
        StartOptions {
            task: launch.task,
            max_turns: launch.max_turns,
            trace: Some(ws.trace()),
            budget: launch.budget,
            packs: Vec::new(),
            branch: None,
            workflow: None,
        },
    );
    finish(ws, host.expect("a started run")).await
}

/// Resume the run whose journal is `log` against `model` and run it to its end.
pub(crate) async fn resume(ws: &Workspace, model: &Arc<Scripted>, log: &Path) -> Ran {
    let policy = Arc::new(
        Policy::new(&ws.dirs, &ws.root, false, None, Some(log)).expect("a policy for the run"),
    );
    let host = Host::resume_with(model.clone(), policy, Some(ws.trace()));
    finish(ws, host.expect("a resumed run")).await
}

async fn finish(ws: &Workspace, host: Arc<Host>) -> Ran {
    let manifest = host.supervisor.manifest.as_ref().expect("a manifest");
    let task = manifest.task.clone();
    let log = host.policy.log.path.clone();
    let result = host.clone().run_task(task, host.max_turns).await;
    drop(host);
    Ran {
        result,
        records: crate::eventlog::load(&log).expect("the run's journal"),
        log,
        trace: std::fs::read_to_string(ws.trace()).unwrap_or_default(),
    }
}

/// The text of a neutral message's content (a string, or parts with text).
pub(crate) fn content(message: &Value) -> String {
    match &message["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect(),
        _ => String::new(),
    }
}

/// Whether a request's conversation contains `needle` anywhere.
pub(crate) fn mentions(body: &Value, needle: &str) -> bool {
    body.to_string().contains(needle)
}

/// The task a request works on: its first user message.
pub(crate) fn task_of(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.iter().find(|m| m["role"] == "user"))
        .map(content)
        .unwrap_or_default()
}

/// Whether the model has replied in this conversation before.
pub(crate) fn has_replied(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|m| m["role"] == "assistant"))
}
