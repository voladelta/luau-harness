use anyhow::{Result, bail};
// Every library module under the name it had when it was declared here, so code in this file refers to it unchanged.
#[allow(unused_imports)]
use hx::{
    CLOSED_STDOUT_STATUS, ClosedStdout, atomic_file, batch, confine, controller, dirs, effects,
    eventlog, history, host, llm, mcp, modules, net, out, outln, packages, policy, prims,
    supervisor, tools, trust, values, vm, workflow, workflow_runtime,
};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug)]
enum CliArg<'a> {
    Flag(&'a str, Option<&'a str>),
    Positional(&'a str),
}

/// Parse options once per query, skipping option values and honoring `--` for literal prompts/chunks.
fn cli_args(args: &[String]) -> Vec<CliArg<'_>> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let mut parsed = Vec::new();
    let mut it = args.iter().skip(1).peekable();
    while let Some(name) = it.next() {
        if name == "--" {
            parsed.extend(it.map(|s| CliArg::Positional(s)));
            break;
        }
        if (name.starts_with("--") && !(command == "exec" && name.chars().any(char::is_whitespace)))
            || name == "-h"
        {
            let value = if flag_kind(command, name).is_some_and(|k| k != FlagKind::Switch)
                && it.peek().is_some_and(|v| !v.starts_with("--"))
            {
                it.next().map(String::as_str)
            } else {
                None
            };
            parsed.push(CliArg::Flag(name, value));
        } else {
            parsed.push(CliArg::Positional(name));
        }
    }
    parsed
}

fn arg(args: &[String], name: &str) -> Option<String> {
    cli_args(args).into_iter().find_map(|a| match a {
        CliArg::Flag(n, v) if n == name => v.map(String::from),
        _ => None,
    })
}

fn positionals(args: &[String]) -> Vec<&str> {
    cli_args(args)
        .into_iter()
        .filter_map(|a| match a {
            CliArg::Positional(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// How many positional operands `command` takes, and its usage line for a wrong count. `run` and `exec` check their
/// own (a prompt, or any number of chunks).
fn operand_count(command: &str) -> Option<(usize, &'static str)> {
    Some(match command {
        "review" => (
            1,
            "usage: hx review LOG --gate ID --decision approve|revise|answer|waive [--input-file F]",
        ),
        "recover" => (
            2,
            "usage: hx recover LOG OP --decision complete|abandon [--yes]",
        ),
        "revoke" => (1, "usage: hx revoke LOG"),
        "test" => (1, "usage: hx test PKG_DIR"),
        "approve" => (1, "usage: hx approve PKG"),
        "skills" => (0, "usage: hx skills"),
        "trust" => (0, "usage: hx trust [--yes]"),
        "log" => (2, "usage: hx log verify LOG"),
        "paths" => (0, "usage: hx paths"),
        _ => return None,
    })
}

/// The positional operands of an owner command, wherever its flags stand, checked against `operand_count`.
fn operands(args: &[String]) -> Result<Vec<&str>> {
    let command = args.first().map(String::as_str).unwrap_or("");
    let found = positionals(args);
    if let Some((count, line)) = operand_count(command)
        && (found.len() != count || (command == "log" && found.first() != Some(&"verify")))
    {
        return Err(usage(line));
    }
    Ok(found)
}

fn task_text(args: &[String]) -> Result<Option<String>> {
    match arg(args, "--prompt-file") {
        Some(path) => Ok(Some(std::fs::read_to_string(path)?)),
        None => Ok(positionals(args).first().map(|p| p.to_string())),
    }
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
    trust        review this workspace's .harness config and hx.lock, then trust it (runs refuse untrusted config)
    log verify   check an event log's hash chain and list unknown-outcome effects
    paths        print where hx keeps this workspace's logs, state and secrets, and the trust store
";

const TOP_HELP_TAIL: &str = "    -h, --help            show help (also `hx <command> --help`)
    -V, --version         show version

EXAMPLES:
    hx exec --workspace . 'print(1+1)'
    hx run \"Fix the failing tests\" --yes --model claude-sonnet-5-5
    hx log verify RUN       (a run name, in this workspace's log directory; see `hx paths`)

ENVIRONMENT:
    HX_MODEL, HX_BASE_URL   defaults for the matching `run` flags
    HX_DEBUG=1              print the full error chain on failure
    HX_TRUST=HASH           trust exactly the workspace config with this hash (see `hx trust`) for one launch
";

const DEFAULT_MAX_TURNS: u64 = 200;
use llm::DEFAULT_MAX_OUTPUT;

#[derive(Clone, Copy, PartialEq)]
enum FlagKind {
    Switch,
    Text,
    Unsigned,
    Positive,
    Amount,
}

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

const fn flag(
    commands: &'static [&'static str],
    name: &'static str,
    kind: FlagKind,
    value: &'static str,
    help: &'static str,
) -> Flag {
    Flag {
        commands,
        name,
        kind,
        value,
        help,
        default: None,
    }
}

const FLAGS: &[Flag] = {
    use FlagKind::*;
    const RUN: &[&str] = &["run"];
    &[
        flag(
            &[],
            "--workspace",
            Text,
            "DIR",
            "workspace root (default: .)",
        ),
        flag(
            &["run", "exec", "test"],
            "--approval",
            Text,
            "MODE",
            "prompt (default) or deny: handle requests outside standing permissions",
        ),
        flag(
            &["run", "exec"],
            "--yes",
            Switch,
            "",
            "autopilot: approve ordinary path and tool requests for this run without asking (never irreversible commands)",
        ),
        flag(
            &["run", "exec"],
            "--ignore-config",
            Switch,
            "",
            "run without this workspace's .harness config and hx.lock (instead of refusing untrusted config)",
        ),
        flag(
            &["run", "exec"],
            "--pack",
            Text,
            "A,B",
            "extra packs: std/vision (view() shows images)",
        ),
        flag(
            RUN,
            "--resume",
            Text,
            "LOG",
            "restore frozen settings and checkpoint into a fresh VM; do not repeat flags (LOG: a run name or a path)",
        ),
        flag(
            RUN,
            "--branch",
            Text,
            "LOG",
            "start a new run with current approved configuration and inherited durable data",
        ),
        flag(RUN, "--prompt-file", Text, "F", "task text"),
        flag(RUN, "--model", Text, "M", "model id (default: HX_MODEL)"),
        flag(
            RUN,
            "--base-url",
            Text,
            "U",
            "API base URL (default: HX_BASE_URL, else OpenRouter)",
        ),
        flag(
            RUN,
            "--key-env",
            Text,
            "E",
            "environment variable holding the API key (default: OPENROUTER_API_KEY)",
        ),
        flag(RUN, "--reasoning", Text, "R", "reasoning effort"),
        Flag {
            default: Some(DEFAULT_MAX_OUTPUT),
            ..flag(
                RUN,
                "--max-output",
                Positive,
                "N",
                "max output tokens per turn; with --context-window, at most a quarter of it",
            )
        },
        flag(
            RUN,
            "--context-window",
            Positive,
            "N",
            "the model's context window; compaction starts before it (default: OpenRouter's listing)",
        ),
        flag(
            RUN,
            "--provider-only",
            Text,
            "A,B",
            "OpenRouter only: route to these providers, no fallbacks",
        ),
        Flag {
            default: Some(DEFAULT_MAX_TURNS),
            ..flag(RUN, "--max-turns", Unsigned, "N", "turn limit")
        },
        flag(
            RUN,
            "--max-tokens",
            Unsigned,
            "N",
            "token admission threshold",
        ),
        flag(
            RUN,
            "--max-wall",
            Amount,
            "SECS",
            "absolute deadline, in seconds",
        ),
        flag(RUN, "--trace", Text, "FILE", "write a readable trace"),
        flag(
            RUN,
            "--rpi",
            Switch,
            "",
            "research, design, implement, verify and repair automatically",
        ),
        flag(
            RUN,
            "--manual",
            Switch,
            "",
            "require RPI artifact review (requires --rpi)",
        ),
        flag(&["review"], "--gate", Text, "ID", "the gate to decide"),
        flag(
            &["review"],
            "--decision",
            Text,
            "D",
            "approve, revise, answer or waive",
        ),
        flag(
            &["review"],
            "--input-file",
            Text,
            "F",
            "feedback, answers or the waiver reason",
        ),
        flag(&["recover"], "--decision", Text, "D", "complete or abandon"),
        flag(
            &["recover", "trust"],
            "--yes",
            Switch,
            "",
            "confirm without a terminal prompt",
        ),
    ]
};

fn flag_kind(command: &str, name: &str) -> Option<FlagKind> {
    FLAGS
        .iter()
        .find(|f| f.name == name && (f.commands.is_empty() || f.commands.contains(&command)))
        .map(|f| f.kind)
}

/// Help lines for flags: global ones (`command` None) or those of one command.
fn flag_lines(command: Option<&str>) -> String {
    let mut out = String::new();
    for f in FLAGS.iter().filter(|f| match command {
        None => f.commands.is_empty(),
        Some(c) => f.commands.contains(&c),
    }) {
        let left = if f.value.is_empty() {
            f.name.to_string()
        } else {
            format!("{} {}", f.name, f.value)
        };
        let default = f
            .default
            .map(|d| format!(" (default: {d})"))
            .unwrap_or_default();
        out.push_str(&format!("    {left:<22}{}{default}\n", f.help));
    }
    out
}

fn top_help() -> String {
    format!(
        "{TOP_HELP}\nGLOBAL FLAGS:\n{}{TOP_HELP_TAIL}",
        flag_lines(None)
    )
}

/// `hx <command> --help`: the command's usage text with its flags from FLAGS.
fn command_help(cmd: &str) -> Option<String> {
    let text = help_for(cmd)?;
    let flags = flag_lines(Some(cmd));
    Some(if flags.is_empty() {
        text.to_string()
    } else {
        format!("{text}\nFLAGS:\n{flags}")
    })
}

fn help_for(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "exec" => {
            "hx exec [flags] CHUNK...\n\nRun each Luau CHUNK in one session and print its result.\n\nExample:\n    hx exec 'print(1+1)' 'print(2)'\n"
        }
        "run" => {
            "hx run PROMPT [--yes] [flags]\n       hx run --prompt-file F [flags]\n\nBudgets are admission thresholds; reservations include in-flight and uncertain generations.\n"
        }
        "review" => {
            "hx review LOG --gate ID --decision approve|revise|answer|waive [--input-file F]\n\nRecords a decision only; continue with hx run --resume LOG.\nLOG is a run name (its journal in this workspace's log directory, see `hx paths`) or a path to the journal.\n"
        }
        "recover" => {
            "hx recover LOG OP --decision complete|abandon [--yes]\n\nRecord an owner's reconciliation decision; never executes an operation.\nAfter abandon, the resumed agent can issue the operation again if it is still needed.\nLocal write/edit outcomes are read from the files on resume, model requests whose response was lost are charged their reserved tokens; none needs a decision.\nLOG is a run name (its journal in this workspace's log directory, see `hx paths`) or a path to the journal.\n"
        }
        "revoke" => {
            "hx revoke LOG\n\nStop admitting further operations for this run. In-flight external work cannot be rolled back.\nLOG is a run name (its journal in this workspace's log directory, see `hx paths`) or a path to the journal.\n"
        }
        "test" => {
            "hx test PKG_DIR\n\nRun the package's tests/*.luau inside its boundary, with only the capabilities its hx.toml requests.\nA package in .harness/pkgs must be approved; a directory without hx.toml runs with the agent's authority.\nExits 1 if any test fails.\n"
        }
        "approve" => {
            "hx approve PKG\n\nLock a project package's hash and grant exactly the capabilities its manifest requests.\nPKG may also be a skill in .agents/skills or ~/.agents/skills (no Luau, no capabilities; an hx.toml there is ignored).\n"
        }
        "skills" => {
            "hx skills\n\nList skills in <workspace>/.agents/skills and ~/.agents/skills (project first) and whether each is approved.\nOnly approved, unchanged skills are visible to the model; approve with `hx approve NAME`.\n"
        }
        "trust" => {
            "hx trust [--yes]\n\nShow this workspace's config (.harness/*.toml, .harness/init.luau, .harness/override/,\n.harness/tools/, hx.lock) and trust its exact content. Until then `hx run` and `hx exec` refuse to\nstart (pass --ignore-config to run without it): a cloned repo cannot widen file-tool access, start MCP servers or\npre-approve packages and skills. The record lives in hx's trust store (see `hx paths`).\nPrompts on a terminal; without one, --yes is required. HX_TRUST=HASH trusts exactly that config for one launch.\n"
        }
        "log" => {
            "hx log verify LOG\n\nCheck the hash chain and the session every record names, the blobs records reference and the recovery\naccounting, and list effects with an unknown outcome.\nLOG is a run name (its journal in this workspace's log directory, see `hx paths`) or a path to the journal.\nExit codes: 0 ok, 1 broken chain, bad blob or invalid recovery accounting, 3 unknown outcomes.\n"
        }
        "paths" => {
            "hx paths\n\nPrint where hx keeps this workspace's files, one `name: path` per line: log (journals, blobs, lock files,\nrevocation markers), state (the workspace's state directory, which holds log and the files of tools built on
hx), secrets (the workspace's credentials for local tools) and trust\n(the trust store). They live under $XDG_STATE_HOME/hx and $XDG_CONFIG_HOME/hx (default ~/.local/state/hx and\n~/.config/hx), outside every root the model can reach.\n"
        }
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
async fn main() -> std::process::ExitCode {
    // Every panic takes the crash path, which resume already handles: an executor that panics after its intent is
    // journaled must not leave the run going with an open intent nothing treats as uncertain, which is what mlua's
    // conversion of a callback panic into a Luau error would do. (`panic = "abort"` in the profile cannot do this:
    // Luau raises its errors as C++ exceptions through Rust frames, which an abort strategy turns into a crash.)
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
    prims::stop_on_signals();
    match real_main().await {
        Ok(status) => status,
        Err(e) if e.is::<ClosedStdout>() => std::process::ExitCode::from(CLOSED_STDOUT_STATUS),
        Err(e) => {
            report_error(&e);
            std::process::ExitCode::from(if e.is::<Usage>() { 2 } else { 1 })
        }
    }
}

/// Print an error for the owner: its message, then its causes (or the full chain with HX_DEBUG).
fn report_error(e: &anyhow::Error) {
    eprintln!("Error: {e}");
    if std::env::var_os("HX_DEBUG").is_some() {
        eprintln!("{e:?}");
    } else if e.chain().nth(1).is_some() {
        for c in e.chain().skip(1) {
            eprintln!("  caused by: {c}");
        }
        eprintln!("(set HX_DEBUG=1 for a full trace)");
    }
}

fn has(args: &[String], flag: &str) -> bool {
    cli_args(args)
        .into_iter()
        .any(|a| matches!(a, CliArg::Flag(n, _) if n == flag))
}

/// A comma-separated flag value as a list (empty when the flag is absent).
fn list_arg(args: &[String], name: &str) -> Vec<String> {
    arg(args, name)
        .map(|v| v.split(',').map(String::from).collect())
        .unwrap_or_default()
}

/// A numeric flag's value, None when absent. `validate_args` has already refused a missing or malformed value (a mistyped
/// `--max-tokens` must not silently run without a budget), so this only converts.
fn parsed<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>> {
    arg(args, name)
        .map(|v| {
            v.parse()
                .map_err(|_| usage(&format!("{name}: expected a number, got {v:?}")))
        })
        .transpose()
}

/// Reject malformed options before opening a journal, starting an MCP server, or contacting a provider.
fn validate_args(args: &[String]) -> Result<()> {
    let command = args[0].as_str();
    let mut seen = std::collections::BTreeSet::new();
    for argument in cli_args(args) {
        let CliArg::Flag(name, value) = argument else {
            continue;
        };
        let kind = flag_kind(command, name)
            .ok_or_else(|| usage(&format!("unknown option {name} for hx {command}")))?;
        if !seen.insert(name) {
            return Err(usage(&format!("{name} may only be supplied once")));
        }
        if matches!(kind, FlagKind::Switch) {
            continue;
        }
        let value = value.ok_or_else(|| usage(&format!("{name} requires a value")))?;
        match kind {
            FlagKind::Unsigned | FlagKind::Positive => {
                let number = value.parse::<u64>().map_err(|_| {
                    usage(&format!(
                        "{name}: expected a nonnegative integer, got {value:?}"
                    ))
                })?;
                if matches!(kind, FlagKind::Positive) && number == 0 {
                    return Err(usage(&format!("{name} must be positive")));
                }
            }
            FlagKind::Amount => {
                let number = value
                    .parse::<f64>()
                    .map_err(|_| usage(&format!("{name}: expected a number, got {value:?}")))?;
                if !number.is_finite() || number < 0.0 {
                    return Err(usage(&format!("{name} must be finite and nonnegative")));
                }
            }
            _ => {}
        }
    }
    if seen.contains("--resume")
        && let Some(flag) = seen
            .iter()
            .find(|flag| !matches!(**flag, "--resume" | "--workspace" | "--trace"))
    {
        return Err(usage(&format!(
            "{flag} cannot override a resumed manifest; use --branch LOG for a new configuration"
        )));
    }
    if seen.contains("--manual") && !seen.contains("--rpi") {
        return Err(usage("--manual requires --rpi"));
    }
    if command == "run" {
        let prompts = positionals(args);
        if prompts.len() > 1 {
            return Err(usage("supply one quoted prompt"));
        }
        if !prompts.is_empty() && seen.contains("--prompt-file") {
            return Err(usage("use a positional prompt or --prompt-file, not both"));
        }
        if !prompts.is_empty() && seen.contains("--resume") {
            return Err(usage(
                "a prompt cannot override a resumed manifest; use --branch LOG",
            ));
        }
        if prompts.first().is_some_and(|p| p.trim().is_empty()) {
            return Err(usage("prompt must not be empty"));
        }
        if prompts.is_empty()
            && !seen.contains("--prompt-file")
            && !seen.contains("--resume")
            && !seen.contains("--branch")
        {
            return Err(usage("a new run requires a prompt or --prompt-file"));
        }
    }
    if seen.contains("--yes") && seen.contains("--approval") {
        return Err(usage("--yes and --approval cannot be combined"));
    }
    vm::check_packs(&list_arg(args, "--pack")).map_err(|e| usage(&e.to_string()))?;
    operands(args)?;
    if command == "review" && (arg(args, "--gate").is_none() || arg(args, "--decision").is_none()) {
        return Err(usage(
            operand_count("review").expect("review takes operands").1,
        ));
    }
    if command == "recover" && arg(args, "--decision").is_none() {
        return Err(usage(
            operand_count("recover").expect("recover takes operands").1,
        ));
    }
    if let Some(mode) = arg(args, "--approval") {
        mode.parse::<policy::ApprovalMode>()
            .map_err(|e| usage(&e.to_string()))?;
    }
    Ok(())
}

/// Top-level help, version, or `<command> --help`: printed, and the caller is done.
fn print_info(args: &[String]) -> Result<bool> {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => out!("{}", top_help())?,
        Some("-V" | "-v" | "--version" | "version") => outln!("hx {}", env!("CARGO_PKG_VERSION"))?,
        Some(c) if has(args, "-h") || has(args, "--help") => match command_help(c) {
            Some(h) => out!("{h}")?,
            None => return Ok(false),
        },
        _ => return Ok(false),
    }
    Ok(true)
}

/// Run the command in `std::env::args` and return its exit status. Every path returns here rather than exiting, so
/// destructors run: a policy ends its journal session and removes its private temp directory.
async fn real_main() -> Result<std::process::ExitCode> {
    use std::process::ExitCode;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if print_info(&args)? {
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(command) = args.first().filter(|c| help_for(c).is_none()) {
        return Err(usage(&format!(
            "unknown command `{command}`\n\nRun `hx --help` for usage."
        )));
    }
    validate_args(&args)?;
    let workspace =
        PathBuf::from(arg(&args, "--workspace").unwrap_or_else(|| ".".into())).canonicalize()?;
    let dirs = dirs::Dirs::from_env()?;
    let own = dirs.workspace(&workspace);
    dirs::note_leftovers(&workspace, &own);
    let resume = arg(&args, "--resume").map(|r| run_log(&own, &r));
    // Only commands that run code need a policy (and with it a temp dir and a log session); admin commands do not.
    let mk_policy = || -> Result<Arc<policy::Policy>> {
        Ok(Arc::new(policy::Policy::new(
            &dirs,
            &workspace,
            has(&args, "--yes"),
            arg(&args, "--approval").map(|m| m.parse()).transpose()?,
            resume.as_deref(),
        )?))
    };
    if has(&args, "--ignore-config") {
        trust::ignore_config();
    }
    // A resumed run uses only its frozen manifest, so workspace config edited since launch neither changes nor blocks it.
    if matches!(args[0].as_str(), "exec" | "run") && !has(&args, "--resume") {
        trust::require_trusted(&workspace, &dirs)?;
    }
    let done = match args[0].as_str() {
        "exec" => cmd_exec(&args, mk_policy()?).await,
        "run" => {
            confine::note();
            cmd_run(&args, &own, mk_policy()?).await
        }
        "recover" => cmd_recover(&args, &own),
        "review" => {
            let (gate, decision) = (
                arg(&args, "--gate").expect("validated"),
                arg(&args, "--decision").expect("validated"),
            );
            workflow_runtime::review(
                &run_log(&own, operands(&args)?[0]),
                &gate,
                &decision,
                arg(&args, "--input-file").as_deref().map(Path::new),
            )
        }
        "revoke" => {
            let path = run_log(&own, operands(&args)?[0]);
            anyhow::ensure!(path.is_file(), "run log does not exist");
            std::fs::write(path.with_extension("revoked"), "revoked by owner\n")?;
            outln!("revoked {}", path.display())
        }
        "test" => return cmd_test(&args, &workspace, &dirs, mk_policy()?).await,
        "approve" => cmd_approve(&args, &workspace, &dirs),
        "trust" => cmd_trust(&args, &workspace, &dirs),
        // hx skills : agent skills on disk and their approval state
        "skills" => {
            out!(
                "{}",
                modules::Resolver::new(workspace).skills_status(&dirs)?
            )
        }
        "log" => return cmd_log_verify(&args, &own),
        "paths" => {
            outln!(
                "log: {}\nstate: {}\nsecrets: {}\ntrust: {}",
                own.log.display(),
                own.state.display(),
                own.secrets.display(),
                dirs.trust_store().display()
            )
        }
        other => unreachable!("help_for lists {other} but nothing dispatches it"),
    };
    done.map(|()| ExitCode::SUCCESS)
}

/// A LOG operand: a path, or a bare run name (no `/`; `.jsonl` optional) in the workspace's log directory. A journal
/// in the current directory is `./NAME.jsonl`.
fn run_log(own: &dirs::WorkspaceDirs, operand: &str) -> PathBuf {
    if operand.contains('/') {
        return PathBuf::from(operand);
    }
    own.log.join(format!(
        "{}.jsonl",
        operand.strip_suffix(".jsonl").unwrap_or(operand)
    ))
}

/// hx exec --workspace DIR 'chunk1' 'chunk2' : run chunks in one session (no API key needed; used for testing)
async fn cmd_exec(args: &[String], policy: Arc<policy::Policy>) -> Result<()> {
    let vm = vm::Vm::new(policy, Duration::from_secs(30), 256 << 20, None)?;
    vm.install_packs(&list_arg(args, "--pack")).await?;
    // The chunks: every argument after the command that is not a flag or its value.
    for chunk in positionals(args) {
        outln!("{}", vm.exec_text(chunk).await?)?;
    }
    Ok(())
}

/// `hx run` model settings: flags, then HX_* environment defaults.
fn llm_cfg(args: &[String], session: String) -> Result<llm::LlmCfg> {
    let context_window = parsed(args, "--context-window")?;
    Ok(llm::LlmCfg {
        model: arg(args, "--model")
            .or_else(|| std::env::var("HX_MODEL").ok())
            .ok_or_else(|| anyhow::anyhow!("--model or HX_MODEL required"))?,
        base_url: arg(args, "--base-url").or_else(|| std::env::var("HX_BASE_URL").ok()),
        key_env: arg(args, "--key-env"),
        reasoning: arg(args, "--reasoning"),
        max_output: parsed(args, "--max-output")?.unwrap_or_else(|| {
            context_window.map_or(DEFAULT_MAX_OUTPUT, |w| DEFAULT_MAX_OUTPUT.min(w / 4).max(1))
        }),
        context_window,
        cache_key: Some(session),
        provider_only: list_arg(args, "--provider-only"),
    })
}

/// Without `--context-window`, take the window (and the output cap, unless `--max-output` was given) that OpenRouter
/// lists for the model. A failure keeps today's defaults.
async fn with_provider_limits(mut cfg: llm::LlmCfg, args: &[String]) -> llm::LlmCfg {
    if cfg.context_window.is_some() {
        return cfg;
    }
    if let Some((window, completion)) = llm::provider_limits(&cfg).await {
        cfg.context_window = Some(window);
        if arg(args, "--max-output").is_none() {
            cfg.max_output = DEFAULT_MAX_OUTPUT
                .min(window / 4)
                .min(completion.unwrap_or(u64::MAX))
                .max(1);
        }
    }
    cfg
}

/// hx run --workspace DIR --prompt-file F --model M [--base-url U --key-env E --reasoning R] [--max-turns N] [--trace FILE]
async fn cmd_run(
    args: &[String],
    own: &dirs::WorkspaceDirs,
    policy: Arc<policy::Policy>,
) -> Result<()> {
    let trace = arg(args, "--trace").map(PathBuf::from);
    let host = if has(args, "--resume") {
        host::Host::resume(policy, trace)?
    } else {
        let inherited = arg(args, "--branch")
            .map(|path| -> Result<_> {
                let path = &run_log(own, &path);
                // The owner may branch a revoked run, so its parent is opened for reconciliation: hx can settle what the
                // evidence settles and bill lost model responses, and nothing else is written to it.
                let journal = eventlog::EventLog::reconcile(path)?;
                let manifest = supervisor::Manifest::inspect(path)?;
                let records = eventlog::load(path)?;
                let (books, _) = supervisor::reconcile(&journal, &records)?;
                anyhow::ensure!(
                    books.open.is_empty() && books.reservations.is_empty(),
                    "reconcile uncertain operations and model billing before branching"
                );
                let state = supervisor::Checkpoint::latest(journal.dir(), &records)?
                    .map(|(_, checkpoint)| checkpoint);
                Ok((journal, manifest, state))
            })
            .transpose()?;
        let task = task_text(args)?
            .or_else(|| inherited.as_ref().map(|(_, m, _)| m.task.clone()))
            .ok_or_else(|| usage("a new run requires a prompt or --prompt-file"))?;
        let cfg = with_provider_limits(llm_cfg(args, policy.log.session.clone())?, args).await;
        let max_turns = parsed(args, "--max-turns")?.unwrap_or(DEFAULT_MAX_TURNS as usize);
        let budget = policy::Budget {
            max_tokens: parsed(args, "--max-tokens")?,
            max_wall_secs: parsed(args, "--max-wall")?,
        };
        let branch = inherited
            .as_ref()
            .map(|(parent, manifest, state)| supervisor::Branch {
                parent: parent.path.clone(),
                durable: state
                    .as_ref()
                    .map(|s| s.durable.clone())
                    .or_else(|| manifest.branch.as_ref().map(|b| b.durable.clone()))
                    .unwrap_or_else(|| serde_json::json!({})),
            });
        if has(args, "--rpi") {
            anyhow::ensure!(
                workflow::git_inventory(&policy.workspace)?.is_some(),
                "--rpi requires a Git repository at workspace root ({}); initialize git with a .gitignore first",
                policy.workspace.display()
            );
            // Verification and acceptance observe the project; one that cannot be observed fails before any work.
            workflow::observe_project(&policy.workspace, &[])
                .map_err(|e| e.context("--rpi cannot observe the project"))?;
        }
        let host = host::Host::start(
            cfg,
            policy,
            host::StartOptions {
                task,
                max_turns,
                trace,
                budget,
                packs: list_arg(args, "--pack"),
                branch,
                workflow: has(args, "--rpi").then(|| workflow::WorkflowConfig {
                    name: "rpi".into(),
                    version: 1,
                    mode: if has(args, "--manual") {
                        "manual"
                    } else {
                        "auto"
                    }
                    .into(),
                    max_repairs: workflow::MAX_REPAIRS,
                }),
            },
        )?;
        if let Some((parent, _, _)) = inherited {
            host.policy
                .log
                .write("branch", serde_json::json!({"parent":parent.path}), true)?;
        }
        host
    };
    let task = host.supervisor.manifest.as_ref().unwrap().task.clone();
    let max_turns = host.max_turns;
    let report = host.run_task(task, max_turns).await?;
    outln!("{}", serde_json::to_string(&report)?)
}

fn cmd_recover(args: &[String], own: &dirs::WorkspaceDirs) -> Result<()> {
    let operands = operands(args)?;
    let path = &run_log(own, operands[0]);
    let id: u64 = operands[1].parse().map_err(|_| {
        usage(&format!(
            "operation ID: expected a number, got {:?}",
            operands[1]
        ))
    })?;
    let decision: supervisor::Decision = arg(args, "--decision")
        .expect("validated")
        .parse()
        .map_err(|e: anyhow::Error| usage(&e.to_string()))?;
    let log = eventlog::EventLog::reconcile(path)?;
    let records = eventlog::load(path)?;
    let mut books = supervisor::Books::of(&records)?;
    if !books.open.contains_key(&id) {
        let kind = records
            .iter()
            .find(|r| r["seq"] == id)
            .and_then(|r| r["kind"].as_str());
        let settled_by = |kind: &str| records.iter().any(|r| r["kind"] == kind && r["of"] == id);
        match kind {
            Some("intent") if settled_by("resolution") => {
                anyhow::bail!("operation already resolved")
            }
            Some("intent") => anyhow::bail!("operation already has an outcome"),
            Some("model_intent") => {
                return Err(usage(
                    "a model request is not an operation: resume or branch charges a lost response its reserved tokens",
                ));
            }
            _ => return Err(usage("not a mutating operation ID")),
        }
    }
    if !has(args, "--yes") {
        confirm(&format!(
            "Record {} for operation {id}? This is your assertion about its external state.",
            decision.name()
        ))?;
    }
    supervisor::resolve(&log, &mut books, &records, id, decision, None)?;
    outln!(
        "recorded {} for {id}; no operation was executed",
        decision.name()
    )
}

/// hx test PKG_DIR : run the package's tests/*.luau inside its boundary (see `modules::TestTarget`)
async fn cmd_test(
    args: &[String],
    workspace: &Path,
    dirs: &dirs::Dirs,
    policy: Arc<policy::Policy>,
) -> Result<std::process::ExitCode> {
    let dir = PathBuf::from(operands(args)?[0]).canonicalize()?;
    let report = match modules::Resolver::new(workspace.to_path_buf()).test_target(&dir)? {
        modules::TestTarget::Harness { note } => {
            if let Some(note) = note {
                eprintln!("note: {note}");
            }
            let tests = modules::test_files(&dir)?;
            vm::Vm::new(policy, Duration::from_secs(30), 256 << 20, None)?
                .run_tests(&tests)
                .await?
        }
        modules::TestTarget::Package { name, mount } => {
            let mut registry = modules::Registry::capture(workspace, dirs)?;
            match mount {
                Some(snapshot) => registry.mount(&name, &snapshot)?,
                // an installed package is tested as a run would load it: approved and unchanged
                None => {
                    registry.resolve(&name)?;
                }
            }
            let tests = registry.tests(&name)?;
            packages::run_tests(
                supervisor::Supervisor::ephemeral(policy, Arc::new(registry)),
                &name,
                &tests,
            )
            .await?
        }
    };
    out!("{}", report.text)?;
    outln!("{} passed, {} failed", report.passed, report.failed)?;
    Ok(if report.failed > 0 {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    })
}

/// hx approve PKG : lock a project package's hash and grant exactly the capabilities its manifest requests
fn cmd_approve(args: &[String], workspace: &Path, dirs: &dirs::Dirs) -> Result<()> {
    let pkg = operands(args)?[0];
    let rt = trust::Retrust::before(workspace, dirs);
    outln!(
        "{}",
        modules::Resolver::new(workspace.to_path_buf()).approve(pkg)?
    )?;
    rt.after()
}

/// hx trust [--yes] : review the workspace config and record its hash as trusted (outside the repo)
fn cmd_trust(args: &[String], workspace: &Path, dirs: &dirs::Dirs) -> Result<()> {
    let files = trust::config_files(workspace);
    if files.is_empty() {
        return outln!("nothing to trust: this workspace has no .harness config or hx.lock");
    }
    let links = trust::symlinks(workspace);
    if !links.is_empty() {
        bail!(
            "cannot trust this config: it has symlinks ({}), whose targets the trust hash does not cover; replace them with real files",
            links.join(", ")
        );
    }
    for f in &files {
        outln!("===== {f}")?;
        match std::fs::read_to_string(workspace.join(f)) {
            Ok(t) => out!("{t}{}", if t.ends_with('\n') { "" } else { "\n" })?,
            Err(_) => outln!("(binary or unreadable)")?,
        }
    }
    if trust::is_trusted(workspace, dirs) && std::env::var("HX_TRUST").is_err() {
        return outln!("already trusted");
    }
    outln!(
        "config hash: {} (HX_TRUST={0} trusts exactly this config for one launch)",
        trust::config_hash(workspace).unwrap_or_default()
    )?;
    if !has(args, "--yes") {
        confirm(
            "trust this config? hx will apply it (file-tool policy, MCP servers, packages, skills, overrides)",
        )?;
    }
    let h = trust::record(workspace, dirs)?.unwrap_or_default();
    outln!("trusted {} ({h})", workspace.display())
}

/// hx log verify LOG : check the chain and the sessions, the referenced blobs and the accounting, and list effects with
/// an unknown outcome (exit 1 damaged, 3 unknown)
fn cmd_log_verify(args: &[String], own: &dirs::WorkspaceDirs) -> Result<std::process::ExitCode> {
    let f = run_log(own, operands(args)?[1]);
    let r = eventlog::verify(&f)?;
    match &r.broken {
        None => outln!("{} records, hash chain intact", r.records)?,
        Some((line, why)) => outln!(
            "{} records, hash chain BROKEN at line {line}: {why}",
            r.records
        )?,
    }
    for error in &r.blob_errors {
        outln!("BAD BLOB: {error}")?;
    }
    let mut damaged = r.broken.is_some() || !r.blob_errors.is_empty();
    let mut unknown = false;
    if r.broken.is_none() {
        match supervisor::Books::of(&eventlog::load(&f)?) {
            Ok(books) => {
                for (seq, intent) in &books.open {
                    outln!(
                        "UNKNOWN OUTCOME: #{seq} {}: {}",
                        intent["tool"].as_str().unwrap_or("?"),
                        intent["summary"].as_str().unwrap_or("")
                    )?;
                }
                for seq in books.reservations.keys() {
                    outln!(
                        "UNKNOWN BILLING: #{seq} model request (charged its reserved tokens on resume)"
                    )?;
                }
                unknown = !books.open.is_empty() || !books.reservations.is_empty();
            }
            Err(error) => {
                outln!("INVALID RECOVERY ACCOUNTING: {error:#}")?;
                damaged = true;
            }
        }
    }
    Ok(std::process::ExitCode::from(if damaged {
        1
    } else if unknown {
        3
    } else {
        0
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn exec_chunks_skip_global_flags_and_their_values() {
        let a = argv("exec --workspace /w print(1) --pack a,b print(2) --yes print(3)");
        assert_eq!(positionals(&a), vec!["print(1)", "print(2)", "print(3)"]);
        assert!(positionals(&argv("exec")).is_empty());
    }

    /// Every command finds its operands with global flags before, between and after them, and refuses a wrong count
    /// or a global flag it would ignore.
    #[test]
    fn operands_are_found_wherever_global_flags_stand() {
        let commands: &[(&str, &[&str], &str)] = &[
            ("run", &["task"], "--model m"),
            ("exec", &["print(1)", "print(2)"], ""),
            ("review", &["LOG"], "--gate g --decision approve"),
            ("recover", &["LOG", "7"], "--decision complete"),
            ("revoke", &["LOG"], ""),
            ("test", &["DIR"], ""),
            ("approve", &["PKG"], ""),
            ("skills", &[], ""),
            ("trust", &[], ""),
            ("log", &["verify", "FILE"], ""),
            ("paths", &[], ""),
        ];
        for &(command, operands_, flags) in commands {
            let mut globals = vec!["--workspace", "/w"];
            if matches!(command, "run" | "exec" | "test") {
                globals.extend(["--approval", "deny"]);
            }
            let flags: Vec<&str> = flags.split_whitespace().collect();
            for split in 0..=operands_.len() {
                // global flags at `split`, command flags at the end and then at the start
                for flags_first in [false, true] {
                    let mut a = vec![command.to_string()];
                    if flags_first {
                        a.extend(flags.iter().map(|f| f.to_string()));
                    }
                    a.extend(operands_[..split].iter().map(|o| o.to_string()));
                    a.extend(globals.iter().map(|g| g.to_string()));
                    a.extend(operands_[split..].iter().map(|o| o.to_string()));
                    if !flags_first {
                        a.extend(flags.iter().map(|f| f.to_string()));
                    }
                    validate_args(&a).unwrap_or_else(|e| panic!("{a:?}: {e}"));
                    assert_eq!(positionals(&a), operands_, "{a:?}");
                    if command != "run" && command != "exec" {
                        assert_eq!(operands(&a).unwrap(), operands_, "{a:?}");
                    }
                }
            }
            if let Some((count, _)) = operand_count(command) {
                let mut extra = vec![command.to_string()];
                extra.extend(operands_.iter().map(|o| o.to_string()));
                extra.push("surplus".into());
                extra.extend(flags.iter().map(|f| f.to_string()));
                assert!(
                    validate_args(&extra).unwrap_err().is::<Usage>(),
                    "{extra:?}: {} operands only",
                    count
                );
            }
            if !matches!(command, "run" | "exec" | "test") {
                let mut a = vec![command.to_string(), "--approval".into(), "deny".into()];
                a.extend(operands_.iter().map(|o| o.to_string()));
                a.extend(flags.iter().map(|f| f.to_string()));
                assert!(
                    validate_args(&a).unwrap_err().is::<Usage>(),
                    "{a:?} accepted a flag it ignores"
                );
            }
        }
        for bad in [
            "log check FILE",
            "recover LOG --decision complete",
            "review --gate g --decision approve",
        ] {
            assert!(
                validate_args(&argv(bad)).unwrap_err().is::<Usage>(),
                "{bad}"
            );
        }
        let own = dirs::WorkspaceDirs {
            log: "/l".into(),
            state: "/st".into(),
            secrets: "/s".into(),
        };
        assert!(
            cmd_recover(&argv("recover LOG abc --decision complete"), &own)
                .unwrap_err()
                .is::<Usage>()
        );
    }

    #[test]
    fn only_optional_packs_can_be_enabled() {
        for ok in [
            "exec --pack std/vision",
            "run --model m --pack std/vision task",
        ] {
            validate_args(&argv(ok)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in [
            "std/compact",
            "std/recheck",
            "std/rpi",
            "std/packages",
            "std/loop",
            "mypkg",
        ] {
            for command in ["exec", "run --model m task"] {
                let a = argv(&format!("{command} --pack {bad}"));
                assert!(validate_args(&a).unwrap_err().is::<Usage>(), "{a:?}");
            }
        }
    }

    #[test]
    fn prompts_are_data_and_cannot_enable_flags() {
        let args = vec![
            "run".into(),
            "Fix --yes and --help parsing".into(),
            "--model".into(),
            "mock".into(),
        ];
        validate_args(&args).unwrap();
        assert_eq!(
            task_text(&args).unwrap().as_deref(),
            Some("Fix --yes and --help parsing")
        );
        assert!(!has(&args, "--yes"));
        let args = argv("run --model mock -- --yes");
        validate_args(&args).unwrap();
        assert!(!has(&args, "--yes"));
        assert_eq!(task_text(&args).unwrap().as_deref(), Some("--yes"));
        assert_eq!(arg(&argv("run -- --model mock"), "--model"), None);
        assert_eq!(positionals(&argv("exec -- --yes")), vec!["--yes"]);
    }

    #[test]
    fn flag_helpers() {
        let a = argv("run --pack a,b --max-turns 7 --max-wall x");
        assert_eq!(list_arg(&a, "--pack"), vec!["a", "b"]);
        assert!(list_arg(&a, "--provider-only").is_empty());
        assert_eq!(parsed::<u32>(&a, "--max-turns").unwrap(), Some(7));
        assert_eq!(parsed::<u32>(&a, "--max-tokens").unwrap(), None);
        // Validation, not parsing, refuses a bad or missing value: a mistyped budget is a usage error, not unlimited.
        for bad in [
            "run --max-wall x",
            "run --model m --max-wall",
            "run --max-wall --yes",
        ] {
            let e = validate_args(&argv(bad)).unwrap_err();
            assert!(
                e.is::<Usage>() && e.to_string().contains("--max-wall"),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn llm_cfg_reads_flags() {
        let a = argv("run --model m --max-output 99 --context-window 1000 --provider-only x,y");
        let c = llm_cfg(&a, "s".into()).unwrap();
        assert_eq!(
            (c.model.as_str(), c.max_output, c.context_window),
            ("m", 99, Some(1000))
        );
        assert_eq!(
            (c.provider_only, c.cache_key),
            (
                vec!["x".to_string(), "y".to_string()],
                Some("s".to_string())
            )
        );
        let cap = |a: &str| llm_cfg(&argv(a), "s".into()).unwrap().max_output;
        assert_eq!(cap("run --model m"), DEFAULT_MAX_OUTPUT);
        assert_eq!(cap("run --model m --context-window 128000"), 32000); // a small window caps the default
        assert_eq!(
            cap("run --model m --context-window 1000000"),
            DEFAULT_MAX_OUTPUT
        );
    }

    #[test]
    fn the_provider_listing_gives_the_window_and_output_cap() {
        let models = serde_json::json!({"data": [{"id": "a/m", "context_length": 128000, "top_provider": {"max_completion_tokens": 8192}},
            {"id": "b/m", "context_length": 1048576, "top_provider": {"max_completion_tokens": null}}]});
        assert_eq!(
            llm::listed_limits(&models, "a/m"),
            Some((128000, Some(8192)))
        );
        assert_eq!(llm::listed_limits(&models, "b/m"), Some((1048576, None)));
        assert_eq!(llm::listed_limits(&models, "c/m"), None);
        assert_eq!(
            llm::listed_limits(&serde_json::json!({"error": "x"}), "a/m"),
            None
        );
    }

    #[test]
    fn every_flag_is_documented_and_shown_in_help() {
        let docs = include_str!("../docs/cli.md");
        for f in FLAGS {
            assert!(
                docs.contains(&format!("`{}", f.name)),
                "docs/cli.md does not mention {}",
                f.name
            );
            let help = if f.commands.is_empty() {
                top_help()
            } else {
                command_help(f.commands[0]).unwrap()
            };
            assert!(
                help.contains(f.name),
                "help for {:?} does not list {}",
                f.commands,
                f.name
            );
        }
        assert!(
            command_help("run")
                .unwrap()
                .contains("turn limit (default: 200)")
        );
    }

    /// A number from the embedded Luau source: the literal after `prefix` (e.g. `M.trigger_ratio = `).
    fn luau_number(source: &str, prefix: &str) -> f64 {
        let at = source
            .find(prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in the Luau source"))
            + prefix.len();
        source[at..]
            .split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Limits that docs state in prose, checked against the constants that enforce them, as
    /// `every_flag_is_documented_and_shown_in_help` does for flags.
    #[test]
    fn documented_limits_match_the_code() {
        let mib = |n: usize| format!("{} MiB", n >> 20);
        let word = |n: usize| {
            [
                "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
                "ten",
            ]
            .get(n)
            .map(|w| w.to_string())
            .unwrap_or_else(|| {
                if n == 20 {
                    "twenty".into()
                } else {
                    panic!("no word for {n}")
                }
            })
        };
        let compact = modules::Resolver::embedded("std", "compact.luau").unwrap();
        let window = luau_number(compact, "M.context_window = ") as usize;
        let trigger = format!(
            "{}%",
            (luau_number(compact, "M.trigger_ratio = ") * 100.0).round()
        );
        let reserve_max = luau_number(compact, "M.reserve_tokens = ") as usize;
        let reserve_min = luau_number(compact, "math.max(") as usize;
        let retry_turns = luau_number(compact, "M.retry_turns = ") as usize;
        let reserve_share = match luau_number(compact, "math.floor(window / ") as usize {
            8 => "an eighth",
            4 => "a quarter",
            d => panic!("1/{d}"),
        };
        let (readme, cli, config) = (
            include_str!("../README.md"),
            include_str!("../docs/cli.md"),
            include_str!("../docs/configuration.md"),
        );
        let (runtime, extending, workflows) = (
            include_str!("../docs/runtime-v2.md"),
            include_str!("../docs/extending.md"),
            include_str!("../docs/workflows.md"),
        );
        let checks = [
            (
                "README.md",
                readme,
                format!(
                    "at {trigger} of the context window ({}M unless",
                    window / 1_000_000
                ),
            ),
            (
                "docs/cli.md",
                cli,
                format!("| `--context-window N` | {window} (assumed) |"),
            ),
            (
                "docs/cli.md",
                cli,
                format!("compaction starts at {trigger} of it"),
            ),
            (
                "docs/configuration.md",
                config,
                format!(
                    "At {trigger} of the context window ({}M is assumed",
                    window / 1_000_000
                ),
            ),
            (
                "docs/configuration.md",
                config,
                format!(
                    "{reserve_share} of the window, from {}k tokens up to {}k",
                    reserve_min / 1024,
                    reserve_max / 1024
                ),
            ),
            (
                "docs/configuration.md",
                config,
                format!(
                    "pauses for the next {} agent replies (`retry_turns`)",
                    word(retry_turns)
                ),
            ),
            (
                "docs/runtime-v2.md",
                runtime,
                format!(
                    "Copies stop during traversal at {} levels, {} nodes or a conservative {} encoded-size bound",
                    values::MAX_DEPTH,
                    values::MAX_NODES,
                    mib(values::MAX_BYTES)
                ),
            ),
            (
                "docs/runtime-v2.md",
                runtime,
                format!(
                    "under a {}-second CPU deadline and a {} memory limit",
                    word(tools::REGISTER_SECS as usize),
                    mib(tools::REGISTER_MEMORY)
                ),
            ),
            (
                "docs/extending.md",
                extending,
                format!(
                    "limited to {} second and {}",
                    word(tools::REGISTER_SECS as usize),
                    mib(tools::REGISTER_MEMORY)
                ),
            ),
            (
                "docs/extending.md",
                extending,
                format!(
                    "Requests have a {}-second timeout, an {} response limit",
                    tools::HTTP_TIMEOUT_SECS,
                    mib(tools::HTTP_MAX_BODY)
                ),
            ),
            (
                "docs/workflows.md",
                workflows,
                format!(
                    "at most {} repair cycles",
                    word(workflow::MAX_REPAIRS as usize)
                ),
            ),
            (
                "README.md",
                readme,
                format!(
                    "up to {} repair cycles",
                    word(workflow::MAX_REPAIRS as usize)
                ),
            ),
        ];
        for (name, text, phrase) in checks {
            assert!(
                text.contains(&phrase),
                "{name} should say {phrase:?} (the value in force)"
            );
        }
    }

    #[test]
    fn a_bare_run_name_is_a_journal_in_the_log_directory() {
        let own = dirs::WorkspaceDirs {
            log: "/state/log".into(),
            state: "/state".into(),
            secrets: "/cfg/s".into(),
        };
        assert_eq!(
            run_log(&own, "1700000000-42"),
            PathBuf::from("/state/log/1700000000-42.jsonl")
        );
        assert_eq!(
            run_log(&own, "1700000000-42.jsonl"),
            PathBuf::from("/state/log/1700000000-42.jsonl")
        );
        assert_eq!(
            run_log(&own, "./1700000000-42.jsonl"),
            PathBuf::from("./1700000000-42.jsonl")
        );
        assert_eq!(run_log(&own, "/x/y.jsonl"), PathBuf::from("/x/y.jsonl"));
    }

    #[test]
    fn print_info_handles_help_and_version_only() {
        assert!(print_info(&argv("")).unwrap());
        assert!(print_info(&argv("--version")).unwrap());
        assert!(print_info(&argv("run --help")).unwrap());
        assert!(
            !print_info(&argv("bogus --help")).unwrap(),
            "unknown command help falls through to the error"
        );
        assert!(!print_info(&argv("run --model m")).unwrap());
    }
}
