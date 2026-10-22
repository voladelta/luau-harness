mod ar;
mod atomic_file;
mod effects;
mod eventlog;
mod host;
mod history;
mod llm;
mod ssot;
mod mcp;
mod modules;
mod policy;
mod prims;
mod trust;
mod vm;
mod supervisor;
mod controller;
mod values;
mod packages;
mod batch;
mod tools;
mod workflow;
mod workflow_runtime;

use anyhow::{bail, Result};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

const TOP_HELP: &str = "hx: Luau code-mode agent harness

USAGE:
    hx <command> [flags]

COMMANDS:
    exec         run Luau chunks in one session (no API key needed)
    run          run an agent task
    review       record an RPI review decision without executing work
    recover      record an owner's decision about an uncertain operation
    revoke       stop admitting operations for a run
    test         run a package's tests/*.luau
    approve      lock a project package or skill and grant the capabilities it requests
    skills       list skills in .agents/skills and ~/.agents/skills and whether each is approved
    trust        review this workspace's .harness config and hx.lock, then trust it (untrusted config is ignored)
    staged       list what the agent staged: packages and harness-module replacements
    accept       review and activate something staged
    reject       discard something staged
    rollback     undo the last accepted replacement of a harness module
    log verify   check an event log's hash chain and list unknown-outcome effects
";

const TOP_HELP_TAIL: &str = "    -h, --help            show help (also `hx <command> --help`)
    -V, --version         show version

EXAMPLES:
    hx exec --workspace . 'print(1+1)'
    hx run --prompt-file task.md --model claude-sonnet-5-5 --max-turns 20
    hx log verify .harness/run.jsonl

ENVIRONMENT:
    HX_MODEL, HX_API, HX_BASE_URL   defaults for the matching `run` flags
    HX_DEBUG=1                      print the full error chain on failure
";

const DEFAULT_MAX_TURNS: u64 = 200;
const DEFAULT_MAX_OUTPUT: u64 = 16384;

#[derive(Clone, Copy, PartialEq)]
enum FlagKind { Switch, Text, Unsigned, Positive, Amount }

/// One command-line flag. This table is the only place flags are declared: validation, `hx exec`'s separation of flags
/// from code, and every help screen read it.
struct Flag {
    /// Commands that accept it; empty means every command (a global flag).
    commands: &'static [&'static str],
    name: &'static str,
    kind: FlagKind,
    value: &'static str,
    help: &'static str,
    default: Option<u64>,
}

const fn flag(commands: &'static [&'static str], name: &'static str, kind: FlagKind, value: &'static str, help: &'static str) -> Flag {
    Flag { commands, name, kind, value, help, default: None }
}

const FLAGS: &[Flag] = {
    use FlagKind::*;
    const RUN: &[&str] = &["run"];
    &[
        flag(&[], "--workspace", Text, "DIR", "workspace root (default: .)"),
        flag(&[], "--yolo", Switch, "", "no sandbox, network, path limits or approval gate; .harness/ stays read-only (prints a warning)"),
        flag(&[], "--net", Switch, "", "allow network access"),
        flag(&[], "--no-net", Switch, "", "deny network access"),
        flag(&[], "--approval", Text, "MODE", "deny (default) or prompt: what happens to a command that needs approval"),
        flag(&["run", "exec"], "--pack", Text, "A,B", "extra packs: std/authoring (agent-written tools), std/vision (view() shows images)"),
        flag(RUN, "--resume", Text, "LOG", "restore frozen settings and checkpoint into a fresh VM; do not repeat flags"),
        flag(RUN, "--branch", Text, "LOG", "start a new run with current approved configuration and inherited durable data"),
        flag(RUN, "--prompt-file", Text, "F", "task text"),
        flag(RUN, "--model", Text, "M", "model id (default: HX_MODEL)"),
        flag(RUN, "--api", Text, "A", "responses (default, or HX_API) or completions"),
        flag(RUN, "--base-url", Text, "U", "API base URL (default: HX_BASE_URL, else OpenRouter)"),
        flag(RUN, "--key-env", Text, "E", "environment variable holding the API key (default: OPENROUTER_API_KEY)"),
        flag(RUN, "--reasoning", Text, "R", "reasoning effort"),
        Flag { default: Some(DEFAULT_MAX_OUTPUT), ..flag(RUN, "--max-output", Positive, "N", "max output tokens per turn") },
        flag(RUN, "--context-window", Positive, "N", "the model's context window; compaction starts before it"),
        flag(RUN, "--provider-only", Text, "A,B", "OpenRouter only: route to these providers, no fallbacks"),
        Flag { default: Some(DEFAULT_MAX_TURNS), ..flag(RUN, "--max-turns", Unsigned, "N", "turn limit") },
        flag(RUN, "--max-tokens", Unsigned, "N", "token admission threshold"),
        flag(RUN, "--max-wall", Amount, "SECS", "absolute deadline, in seconds"),
        flag(RUN, "--trace", Text, "FILE", "write a readable trace"),
        flag(RUN, "--rpi", Switch, "", "research, design, implement, verify and repair automatically"),
        flag(RUN, "--manual", Switch, "", "require RPI artifact review (requires --rpi)"),
        flag(RUN, "--ar", Switch, "", "autoresearch over .harness/ar.toml until progress stops (--ar --resume .harness/ar/RUN continues one)"),
        flag(&["review"], "--gate", Text, "ID", "the gate to decide"),
        flag(&["review"], "--decision", Text, "D", "approve, revise, answer or waive"),
        flag(&["review"], "--input-file", Text, "F", "feedback, answers or the waiver reason"),
        flag(&["recover"], "--decision", Text, "D", "complete or abandon"),
        flag(&["recover", "trust", "accept"], "--yes", Switch, "", "confirm without a terminal prompt"),
    ]
};

fn flag_kind(command: &str, name: &str) -> Option<FlagKind> {
    FLAGS.iter().find(|f| f.name == name && (f.commands.is_empty() || f.commands.contains(&command))).map(|f| f.kind)
}

/// Help lines for flags: global ones (`command` None) or those of one command.
fn flag_lines(command: Option<&str>) -> String {
    let mut out = String::new();
    for f in FLAGS.iter().filter(|f| match command { None => f.commands.is_empty(), Some(c) => f.commands.contains(&c) }) {
        let left = if f.value.is_empty() { f.name.to_string() } else { format!("{} {}", f.name, f.value) };
        let default = f.default.map(|d| format!(" (default: {d})")).unwrap_or_default();
        out.push_str(&format!("    {left:<22}{}{default}\n", f.help));
    }
    out
}

fn top_help() -> String {
    format!("{TOP_HELP}\nGLOBAL FLAGS:\n{}{TOP_HELP_TAIL}", flag_lines(None))
}

/// `hx <command> --help`: the command's usage text with its flags from FLAGS.
fn command_help(cmd: &str) -> Option<String> {
    let text = help_for(cmd)?;
    let flags = flag_lines(Some(cmd));
    Some(if flags.is_empty() { text.to_string() } else { format!("{text}\nFLAGS:\n{flags}") })
}

fn help_for(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "exec" => "hx exec [flags] CHUNK...\n\nRun each Luau CHUNK in one session and print its result.\n\nExample:\n    hx exec 'print(1+1)' 'print(2)'\n",
        "run" => "hx run --prompt-file F --model M [flags]\n\nBudgets are admission thresholds; reservations include in-flight and uncertain generations.\n",
        "review" => "hx review LOG --gate ID --decision approve|revise|answer|waive [--input-file F]\n\nRecords a decision only; continue with hx run --resume LOG.\n",
        "recover" => "hx recover LOG OP --decision complete|abandon [--yes]\n\nRecord an owner's reconciliation decision; never executes an operation.\nAfter abandon, the resumed agent can issue the operation again if it is still needed.\nLocal write/edit outcomes are read from the files on resume, sandboxed offline sh is settled as unknown_local, and model requests whose response was lost are charged their reserved tokens; none needs a decision.\n",
        "revoke" => "hx revoke LOG\n\nStop admitting further operations for this run. In-flight external work cannot be rolled back.\n",
        "test" => "hx test PKG_DIR\n\nRun the package's tests/*.luau. Exits 1 if any test fails.\n",
        "approve" => "hx approve PKG\n\nLock a project package's hash and grant exactly the capabilities its manifest requests.\nPKG may also be a skill in .agents/skills or ~/.agents/skills (no hx.toml needed; no capabilities).\n",
        "skills" => "hx skills\n\nList skills in <workspace>/.agents/skills and ~/.agents/skills (project first) and whether each is approved.\nOnly approved, unchanged skills are visible to the model; approve with `hx approve NAME`.\n",
        "staged" => "hx staged\n\nList what the agent staged: packages (NAME) and harness-module replacements (std/MODULE).\n",
        "trust" => "hx trust [--yes]\n\nShow this workspace's config (.harness/{policy,mcp}.toml, .harness/init.luau, .harness/override/,\n.harness/tools/, hx.lock) and trust its exact content. Until then hx ignores it: a cloned repo cannot widen the sandbox, start MCP\nservers or pre-approve packages and skills. The record lives in ~/.config/hx/trust.toml.\nPrompts on a terminal; without one, --yes is required. HX_TRUST=1 trusts the workspace for one launch.\n",
        "accept" => "hx accept NAME [--yes]\n\nReview something staged, then activate it. A package is locked in hx.lock and granted the capabilities\nit requests; a module replacement (std/MODULE) must load and keep every export, and becomes the override.\nPrompts on a terminal; without one, --yes is required.\n",
        "reject" => "hx reject NAME\n\nDiscard something staged.\n",
        "rollback" => "hx rollback MODULE\n\nRestore what the last accepted replacement of MODULE (e.g. std/printer) replaced.\n",
        "log" => "hx log verify FILE\n\nCheck the hash chain and list effects with an unknown outcome.\nExit codes: 0 ok, 1 corrupt chain or invalid recovery accounting, 3 unknown outcomes.\n",
        _ => return None,
    })
}

/// A command-line usage mistake: reported like any error but exits 2 (not 1).
#[derive(Debug)]
struct Usage(String);
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Usage {}
fn usage(msg: &str) -> anyhow::Error {
    anyhow::Error::new(Usage(msg.into()))
}

/// Ask a y/N question on stderr. Never blocks a script: without a tty on stdin the caller must pass `--yes`.
fn confirm(question: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        bail!("{question}\nstdin is not a terminal; pass --yes to confirm non-interactively");
    }
    eprint!("{question} [y/N] ");
    let mut l = String::new();
    std::io::stdin().read_line(&mut l)?;
    if !matches!(l.trim(), "y" | "Y" | "yes") {
        bail!("not accepted");
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    prims::stop_on_signals();
    if let Err(e) = real_main().await {
        eprintln!("Error: {e}");
        if std::env::var_os("HX_DEBUG").is_some() {
            eprintln!("{e:?}");
        } else if e.chain().nth(1).is_some() {
            for c in e.chain().skip(1) {
                eprintln!("  caused by: {c}");
            }
            eprintln!("(set HX_DEBUG=1 for a full trace)");
        }
        std::process::exit(if e.is::<Usage>() { 2 } else { 1 });
    }
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// A comma-separated flag value as a list (empty when the flag is absent).
fn list_arg(args: &[String], name: &str) -> Vec<String> {
    arg(args, name).map(|v| v.split(',').map(String::from).collect()).unwrap_or_default()
}

/// A numeric flag's value, None when absent. `validate_args` has already refused a missing or malformed value (a mistyped
/// `--max-tokens` must not silently run without a budget), so this only converts.
fn parsed<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>> {
    arg(args, name).map(|v| v.parse().map_err(|_| usage(&format!("{name}: expected a number, got {v:?}")))).transpose()
}

/// `--net` / `--no-net`, or None to let the policy decide.
fn net_flag(args: &[String]) -> Option<bool> {
    if has(args, "--net") {
        Some(true)
    } else if has(args, "--no-net") {
        Some(false)
    } else {
        None
    }
}

/// Reject malformed options before opening a journal, starting an MCP server, or contacting a provider.
fn validate_args(args: &[String]) -> Result<()> {
    let command = args[0].as_str();
    let mut seen = std::collections::BTreeSet::new();
    let mut it = args.iter().skip(1);
    while let Some(name) = it.next() {
        if !name.starts_with("--") { continue; }
        // A quoted Luau chunk may start with a multiline comment.
        if command == "exec" && name.chars().any(char::is_whitespace) { continue; }
        let kind = flag_kind(command, name).ok_or_else(|| usage(&format!("unknown option {name} for hx {command}")))?;
        if !seen.insert(name.as_str()) { return Err(usage(&format!("{name} may only be supplied once"))); }
        if matches!(kind, FlagKind::Switch) { continue; }
        let value = it.next().filter(|v| !v.starts_with("--")).ok_or_else(|| usage(&format!("{name} requires a value")))?;
        match kind {
            FlagKind::Unsigned | FlagKind::Positive => {
                let number = value.parse::<u64>().map_err(|_| usage(&format!("{name}: expected a nonnegative integer, got {value:?}")))?;
                if matches!(kind, FlagKind::Positive) && number == 0 { return Err(usage(&format!("{name} must be positive"))); }
            },
            FlagKind::Amount => {
                let number = value.parse::<f64>().map_err(|_| usage(&format!("{name}: expected a number, got {value:?}")))?;
                if !number.is_finite() || number < 0.0 { return Err(usage(&format!("{name} must be finite and nonnegative"))); }
            },
            _ => {},
        }
    }
    if seen.contains("--net") && seen.contains("--no-net") { return Err(usage("--net and --no-net are mutually exclusive")); }
    if seen.contains("--resume")
        && let Some(flag) = seen.iter().find(|flag| !matches!(**flag, "--resume" | "--workspace" | "--trace" | "--ar")) {
        return Err(usage(&format!("{flag} cannot override a resumed manifest; use --branch LOG for a new configuration")));
    }
    if seen.contains("--manual") && !seen.contains("--rpi") { return Err(usage("--manual requires --rpi")); }
    if seen.contains("--ar") && let Some(flag) = seen.iter().find(|f| matches!(**f, "--rpi" | "--branch" | "--trace" | "--max-tokens" | "--max-wall" | "--max-turns")) {
        return Err(usage(&format!("{flag} cannot be combined with --ar")));
    }
    if seen.contains("--ar") && !seen.contains("--resume") && !seen.contains("--prompt-file") {
        return Err(usage("--ar requires --prompt-file (the program), or --resume RUN_DIR"));
    }
    if list_arg(args, "--pack").iter().any(|p| p == "std/rpi" || p.starts_with("std/rpi/")) {
        return Err(usage("RPI must be activated explicitly with --rpi, not --pack"));
    }
    if command == "review" && (args.get(1).is_none_or(|s| s.starts_with("--")) || arg(args,"--gate").is_none() || arg(args,"--decision").is_none()) {
        return Err(usage("usage: hx review LOG --gate ID --decision approve|revise|answer|waive [--input-file F]"));
    }
    if let Some(api) = arg(args, "--api") && !matches!(api.as_str(), "responses" | "completions") {
        return Err(usage(&format!("unknown --api {api:?} (expected responses or completions)")));
    }
    if let Some(mode) = arg(args, "--approval") {
        mode.parse::<policy::ApprovalMode>().map_err(|e| usage(&e.to_string()))?;
    }
    Ok(())
}

/// Top-level help, version, or `<command> --help`: printed, and the caller is done.
fn print_info(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => print!("{}", top_help()),
        Some("-V" | "-v" | "--version" | "version") => println!("hx {}", env!("CARGO_PKG_VERSION")),
        Some(c) if args.iter().skip(1).any(|a| a == "-h" || a == "--help") => match command_help(c) {
            Some(h) => print!("{h}"),
            None => return false,
        },
        _ => return false,
    }
    true
}

async fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if print_info(&args) {
        return Ok(());
    }
    if args.first().is_some_and(|c| help_for(c).is_none()) {
        eprintln!("Error: unknown command `{}`\n\nRun `hx --help` for usage.", args[0]);
        std::process::exit(2);
    }
    validate_args(&args)?;
    let workspace = PathBuf::from(arg(&args, "--workspace").unwrap_or_else(|| ".".into())).canonicalize()?;
    // The sandbox, path policy and approval gate are ON by default; `--yolo` is an explicit opt-out (and says so on stderr).
    let sandbox = !has(&args, "--yolo");
    if !sandbox {
        eprintln!("warning: --yolo: no sandbox, network open, no path limits, no approval gate (.harness/ and hx.lock stay read-only)");
    }
    // Only commands that run code need a policy (and with it a temp dir and a log session); admin commands do not.
    let mk_policy = |episode| -> Result<Arc<policy::Policy>> { Ok(Arc::new(policy::Policy::new(&workspace, sandbox, arg(&args, "--approval").map(|m| m.parse()).transpose()?, arg(&args, "--resume").map(PathBuf::from).as_deref(), net_flag(&args), episode)?)) };
    match args[0].as_str() {
        "exec" => {
            trust::warn_if_untrusted(&workspace);
            cmd_exec(&args, mk_policy(None)?).await
        }
        "run" if has(&args, "--ar") => {
            trust::warn_if_untrusted(&workspace);
            cmd_ar(&args, &workspace).await
        }
        "run" => {
            trust::warn_if_untrusted(&workspace);
            // A child run of an autoresearch episode: the episode becomes part of its frozen policy.
            cmd_run(&args, workspace.clone(), mk_policy(ar::load_episode()?)?).await
        }
        "recover" => cmd_recover(&args),
        "review" => workflow_runtime::review(Path::new(&args[1]), &arg(&args,"--gate").unwrap(), &arg(&args,"--decision").unwrap(), arg(&args,"--input-file").as_deref().map(Path::new)),
        "revoke" => {
            let path = PathBuf::from(args.get(1).ok_or_else(|| usage("usage: hx revoke LOG"))?);
            anyhow::ensure!(path.is_file(), "run log does not exist");
            std::fs::write(path.with_extension("revoked"), "revoked by owner\n")?;
            println!("revoked {}", path.display());
            Ok(())
        },
        "test" => cmd_test(&args, &workspace, mk_policy(None)?).await,
        "approve" => cmd_approve(&args, &workspace),
        "trust" => cmd_trust(&args, &workspace),
        // hx skills : agent skills on disk and their approval state
        "skills" => {
            print!("{}", modules::Resolver::new(workspace).skills_status());
            Ok(())
        }
        // hx staged | accept NAME [--yes] | reject NAME | rollback MODULE : the user's side of staging (nothing the agent can call does this)
        "staged" => {
            let l = modules::Resolver::new(workspace).list_staged();
            println!("{}", if l.is_empty() { "nothing staged".to_string() } else { l.join("\n") });
            Ok(())
        }
        "reject" => {
            let n = args.get(1).ok_or_else(|| usage("usage: hx reject NAME"))?;
            modules::Resolver::new(workspace).reject(n)?;
            println!("rejected {n}");
            Ok(())
        }
        "accept" => cmd_accept(&args, &workspace),
        "rollback" => cmd_rollback(&args, &workspace),
        "log" if args.get(1).map(String::as_str) == Some("verify") => cmd_log_verify(&args),
        _ => Err(usage("usage: hx log verify FILE (see `hx log --help`)")),
    }
}

/// The Luau chunks of `hx exec`: every argument after the command that is not a global flag or its value.
fn exec_chunks(args: &[String]) -> Vec<&str> {
    let mut out = vec![];
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match flag_kind("exec", a) {
            Some(FlagKind::Switch) => {},
            Some(_) => { it.next(); },
            None => out.push(a.as_str()),
        }
    }
    out
}

/// hx exec --workspace DIR 'chunk1' 'chunk2' : run chunks in one session (no API key needed; used for testing)
async fn cmd_exec(args: &[String], policy: Arc<policy::Policy>) -> Result<()> {
    let vm = vm::Vm::new(policy, Duration::from_secs(30), 256 << 20, None)?;
    let mut packs = vec!["std/packages".to_string(), "std/mcp".to_string()];
    packs.extend(list_arg(args, "--pack"));
    vm.install_packs(&packs).await?;
    for chunk in exec_chunks(args) {
        println!("{}", vm.exec_text(chunk).await?);
    }
    Ok(())
}

/// `hx run` model settings: flags, then HX_* environment defaults.
fn llm_cfg(args: &[String], session: String) -> Result<llm::LlmCfg> {
    Ok(llm::LlmCfg {
        model: arg(args, "--model").or_else(|| std::env::var("HX_MODEL").ok()).ok_or_else(|| anyhow::anyhow!("--model or HX_MODEL required"))?,
        api: arg(args, "--api").or_else(|| std::env::var("HX_API").ok()).unwrap_or_else(|| "responses".into()),
        base_url: arg(args, "--base-url").or_else(|| std::env::var("HX_BASE_URL").ok()),
        key_env: arg(args, "--key-env"),
        reasoning: arg(args, "--reasoning"),
        max_output: parsed(args, "--max-output")?.unwrap_or(DEFAULT_MAX_OUTPUT),
        context_window: parsed(args, "--context-window")?,
        cache_key: Some(session),
        provider_only: list_arg(args, "--provider-only"),
    })
}

/// hx run --workspace DIR --prompt-file F --model M [--api responses --base-url U --key-env E --reasoning R] [--max-turns N] [--trace FILE]
async fn cmd_run(args: &[String], workspace: PathBuf, policy: Arc<policy::Policy>) -> Result<()> {
    let _ = workspace;
    let trace = arg(args, "--trace").map(PathBuf::from);
    let host = if has(args, "--resume") {
        host::Host::resume(policy, trace)?
    } else {
        let inherited = arg(args, "--branch").map(|path| -> Result<_> {
            let journal = eventlog::EventLog::reopen(Path::new(&path))?;
            let manifest = supervisor::Manifest::inspect(Path::new(&path))?;
            supervisor::reconcile(&journal, &eventlog::load(Path::new(&path))?, &manifest)?;
            let recovery = eventlog::verify(Path::new(&path))?;
            anyhow::ensure!(recovery.unknown.is_empty() && recovery.unknown_models.is_empty(), "reconcile uncertain operations and model billing before branching");
            let records = eventlog::load(Path::new(&path))?;
            let state = records.iter().rev().find(|r| r["kind"] == "checkpoint" && r["lane"] == "root")
                .map(|r| eventlog::read_blob(Path::new(&path).parent().unwrap(), r["blob"].as_str().unwrap())).transpose()?
                .map(|b| supervisor::Checkpoint::load(&b)).transpose()?;
            Ok((journal, manifest, state))
        }).transpose()?;
        let task = match arg(args, "--prompt-file") {
            Some(path) => std::fs::read_to_string(path)?,
            None => inherited.as_ref().map(|(_, m, _)| m.task.clone()).ok_or_else(|| usage("--prompt-file required for a new run"))?,
        };
        let cfg = llm_cfg(args, policy.log.session.clone())?;
        let max_turns = parsed(args, "--max-turns")?.unwrap_or(DEFAULT_MAX_TURNS as usize);
        let budget = policy::Budget { max_tokens: parsed(args, "--max-tokens")?, max_wall_secs: parsed(args, "--max-wall")? };
        let branch = inherited.as_ref().map(|(parent, manifest, state)| supervisor::Branch {
            parent: parent.path.clone(),
            durable: state.as_ref().map(|s| s.durable.clone())
                .or_else(|| manifest.branch.as_ref().map(|b| b.durable.clone())).unwrap_or_else(|| serde_json::json!({})),
        });
        if has(args, "--rpi") {
            anyhow::ensure!(
                workflow::git_inventory(&policy.workspace)?.is_some(),
                "--rpi requires a Git repository at workspace root ({}); initialize git with a .gitignore first",
                policy.workspace.display()
            );
        }
        let host = host::Host::start(cfg, policy, host::StartOptions {
            task, max_turns, trace, budget, packs: list_arg(args, "--pack"), branch,
            workflow: has(args,"--rpi").then(|| workflow::WorkflowConfig { name:"rpi".into(),version:1,mode:if has(args,"--manual") {"manual"} else {"auto"}.into(),max_repairs:3 }),
        })?;
        if let Some((parent, _, _)) = inherited {
            host.policy.log.write("branch", serde_json::json!({"parent":parent.path}), true)?;
        }
        host
    };
    let task = host.supervisor.manifest.as_ref().unwrap().task.clone();
    let max_turns = host.max_turns;
    let report = host.run_task(task, max_turns).await?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

/// hx run --ar: every flag except the program passes through to the episodes' child runs.
async fn cmd_ar(args: &[String], workspace: &Path) -> Result<()> {
    if let Some(dir) = arg(args, "--resume") {
        return ar::orchestrate(workspace, None, Some(PathBuf::from(dir))).await;
    }
    let mut child = vec![];
    let mut it = args.iter().skip(1);
    while let Some(name) = it.next() {
        let kind = flag_kind("run", name);
        let value = if matches!(kind, Some(FlagKind::Switch)) { None } else { it.next() };
        if matches!(name.as_str(), "--ar" | "--prompt-file" | "--workspace") { continue; }
        child.push(name.clone());
        child.extend(value.cloned());
    }
    let program = std::fs::read_to_string(arg(args, "--prompt-file").unwrap())?;
    ar::orchestrate(workspace, Some((child, program)), None).await
}

fn cmd_recover(args: &[String]) -> Result<()> {
    let path = Path::new(args.get(1).ok_or_else(|| usage("usage: hx recover LOG OP --decision complete|abandon"))?);
    let id: u64 = args.get(2).ok_or_else(|| usage("missing operation ID"))?.parse()?;
    let decision = arg(args, "--decision").ok_or_else(|| usage("missing --decision"))?;
    anyhow::ensure!(matches!(decision.as_str(), "complete" | "abandon"), "decision must be complete or abandon");
    let log = eventlog::EventLog::reconcile(path)?;
    let records = eventlog::load(path)?;
    let intent = records.iter().find(|r| r["seq"] == id && r["kind"] == "intent").ok_or_else(|| usage("not a mutating operation ID"))?;
    anyhow::ensure!(!records.iter().any(|r| r["kind"] == "outcome" && r["of"] == id), "operation already has an outcome");
    anyhow::ensure!(!records.iter().any(|r| r["kind"] == "resolution" && r["of"] == id), "operation already resolved");
    if !has(args, "--yes") { confirm(&format!("Record {decision} for operation {id}? This is your assertion about its external state."))?; }
    log.write("resolution", serde_json::json!({"of":id, "decision":decision, "tool":intent["tool"], "args_sha":intent["args_sha"]}), true)?;
    println!("recorded {decision} for {id}; no operation was executed");
    Ok(())
}

/// hx test PKG_DIR : run the package's tests/*.luau
async fn cmd_test(args: &[String], workspace: &Path, policy: Arc<policy::Policy>) -> Result<()> {
    let dir = PathBuf::from(args.get(1).ok_or_else(|| usage("usage: hx test PKG_DIR"))?).canonicalize()?;
    // Mount the package under test so `require("name")` finds it without installing or locking it, but ONLY for a
    // directory outside .harness/pkgs (e.g. a staged tool). Installed packages keep their lock and grant checks
    // (they resolve normally via .harness/pkgs).
    let installed = dir.parent().is_some_and(|p| p == workspace.join(".harness/pkgs"));
    if !installed
        && let Ok(t) = std::fs::read_to_string(dir.join("hx.toml"))
        && let Some(name) = t.lines().find_map(|l| l.trim().strip_prefix("name = ").map(|v| v.trim_matches('"').to_string()))
    {
        let _ = modules::TEST_PKG.set((name, dir.clone()));
    }
    let vm = vm::Vm::new(policy, Duration::from_secs(30), 256 << 20, None)?;
    let (pass, fail, report) = vm.run_tests(&dir).await?;
    print!("{report}");
    println!("{pass} passed, {fail} failed");
    if fail > 0 {
        std::process::exit(1);
    }
    Ok(())
}

/// hx approve PKG : lock a project package's hash and grant exactly the capabilities its manifest requests
fn cmd_approve(args: &[String], workspace: &Path) -> Result<()> {
    let pkg = args.get(1).ok_or_else(|| usage("usage: hx approve PKG"))?;
    let rt = trust::Retrust::before(workspace);
    println!("{}", modules::Resolver::new(workspace.to_path_buf()).approve(pkg)?);
    rt.after()
}

/// hx trust [--yes] : review the workspace config and record its hash as trusted (outside the repo)
fn cmd_trust(args: &[String], workspace: &Path) -> Result<()> {
    let files = trust::config_files(workspace);
    if files.is_empty() {
        println!("nothing to trust: this workspace has no .harness config or hx.lock");
        return Ok(());
    }
    let links = trust::symlinks(workspace);
    if !links.is_empty() {
        bail!("cannot trust this config: it has symlinks ({}), whose targets the trust hash does not cover; replace them with real files", links.join(", "));
    }
    for f in &files {
        println!("===== {f}");
        match std::fs::read_to_string(workspace.join(f)) {
            Ok(t) => print!("{t}{}", if t.ends_with('\n') { "" } else { "\n" }),
            Err(_) => println!("(binary or unreadable)"),
        }
    }
    if trust::is_trusted(workspace) && std::env::var("HX_TRUST").is_err() {
        println!("already trusted");
        return Ok(());
    }
    if !has(args, "--yes") {
        confirm("trust this config? hx will apply it (sandbox, MCP servers, packages, skills, overrides)")?;
    }
    let h = trust::record(workspace)?.unwrap_or_default();
    println!("trusted {} ({}…)", workspace.display(), &h[..12.min(h.len())]);
    Ok(())
}

/// hx rollback MODULE : undo the last accepted replacement of a harness module
fn cmd_rollback(args: &[String], workspace: &Path) -> Result<()> {
    let module = args.get(1).ok_or_else(|| usage("usage: hx rollback MODULE"))?;
    let rt = trust::Retrust::before(workspace);
    println!("{}", modules::Resolver::new(workspace.to_path_buf()).rollback(module)?);
    rt.after()
}

/// hx accept NAME [--yes] : review something staged, then activate it
fn cmd_accept(args: &[String], workspace: &Path) -> Result<()> {
    let n = args.get(1).ok_or_else(|| usage("usage: hx accept NAME [--yes]"))?;
    let r = modules::Resolver::new(workspace.to_path_buf());
    println!("{}", r.review(n)?);
    if !has(args, "--yes") {
        confirm(&format!("accept {n}? {}", if n.contains('/') { "it replaces the module in force for new runs" } else { "it will be locked and granted the capabilities above" }))?;
    }
    let rt = trust::Retrust::before(workspace);
    println!("{}", r.accept(n)?);
    rt.after()
}

/// hx log verify FILE : check the hash chain and list effects with an unknown outcome (exit 1 tampered, 3 unknown)
fn cmd_log_verify(args: &[String]) -> Result<()> {
    let f = PathBuf::from(args.get(2).ok_or_else(|| usage("usage: hx log verify FILE"))?);
    let r = eventlog::verify(&f)?;
    println!("{} records, hash chain {}", r.records, if r.chain_ok { "intact".to_string() } else { format!("BROKEN at line {}", r.bad_line.unwrap_or(0)) });
    for (seq, tool, summary) in &r.unknown {
        println!("UNKNOWN OUTCOME: #{seq} {tool}: {summary}");
    }
    for seq in &r.unknown_models { println!("UNKNOWN BILLING: #{seq} model request (charged its reserved tokens on resume)"); }
    if let Some(error) = &r.recovery_error { println!("INVALID RECOVERY ACCOUNTING: {error}"); }
    if !r.chain_ok || r.recovery_error.is_some() {
        std::process::exit(1);
    }
    if !r.unknown.is_empty() || !r.unknown_models.is_empty() {
        std::process::exit(3);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn exec_chunks_skip_global_flags_and_their_values() {
        let a = argv("exec --workspace /w print(1) --pack a,b --net print(2) --approval prompt --yolo --no-net print(3)");
        assert_eq!(exec_chunks(&a), vec!["print(1)", "print(2)", "print(3)"]);
        assert!(exec_chunks(&argv("exec")).is_empty());
    }

    #[test]
    fn flag_helpers() {
        let a = argv("run --pack a,b --max-turns 7 --max-wall x --net");
        assert_eq!(list_arg(&a, "--pack"), vec!["a", "b"]);
        assert!(list_arg(&a, "--provider-only").is_empty());
        assert_eq!(parsed::<u32>(&a, "--max-turns").unwrap(), Some(7));
        assert_eq!(parsed::<u32>(&a, "--max-tokens").unwrap(), None);
        // Validation, not parsing, refuses a bad or missing value: a mistyped budget is a usage error, not unlimited.
        for bad in ["run --max-wall x", "run --model m --max-wall", "run --max-wall --net"] {
            let e = validate_args(&argv(bad)).unwrap_err();
            assert!(e.is::<Usage>() && e.to_string().contains("--max-wall"), "{bad}: {e}");
        }
        assert_eq!(net_flag(&a), Some(true));
        assert_eq!(net_flag(&argv("run --no-net")), Some(false));
        assert_eq!(net_flag(&argv("run")), None);
    }

    #[test]
    fn llm_cfg_reads_flags() {
        let a = argv("run --model m --api completions --max-output 99 --context-window 1000 --provider-only x,y");
        let c = llm_cfg(&a, "s".into()).unwrap();
        assert_eq!((c.model.as_str(), c.api.as_str(), c.max_output, c.context_window), ("m", "completions", 99, Some(1000)));
        assert_eq!((c.provider_only, c.cache_key), (vec!["x".to_string(), "y".to_string()], Some("s".to_string())));
        let a_def = argv("run --model m");
        let c_def = llm_cfg(&a_def, "s".into()).unwrap();
        assert_eq!(c_def.api, "responses");
    }

    #[test]
    fn every_flag_is_documented_and_shown_in_help() {
        let docs = include_str!("../docs/cli.md");
        for f in FLAGS {
            assert!(docs.contains(&format!("`{}", f.name)), "docs/cli.md does not mention {}", f.name);
            let help = if f.commands.is_empty() { top_help() } else { command_help(f.commands[0]).unwrap() };
            assert!(help.contains(f.name), "help for {:?} does not list {}", f.commands, f.name);
        }
        assert!(command_help("run").unwrap().contains("turn limit (default: 200)"));
    }

    #[test]
    fn print_info_handles_help_and_version_only() {
        assert!(print_info(&argv("")));
        assert!(print_info(&argv("--version")));
        assert!(print_info(&argv("run --help")));
        assert!(!print_info(&argv("bogus --help")), "unknown command help falls through to the error");
        assert!(!print_info(&argv("run --model m")));
    }
}
