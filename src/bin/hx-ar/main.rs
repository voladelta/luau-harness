//! `hx-ar`: autoresearch, a manager loop over `hx run` that improves an evaluator's score by changing a task's editable
//! files, and returns a portfolio of distinct good candidates rather than one best state (see `run`).
//!
//! It is a workflow built on the hx library, outside the harness core: each episode is an ordinary child `hx run` in a
//! throwaway workspace, and the agent there reaches this process only through an MCP server it serves on loopback
//! (`serve`). This process alone holds the run's state, runs the evaluators and writes the journal.
#[cfg(not(target_os = "linux"))]
compile_error!("hx-ar supports Linux only");

mod files;
mod run;
mod serve;

use anyhow::{Context, Result, bail};
use std::path::PathBuf;

const HELP: &str = "hx-ar: autoresearch over an hx workspace

USAGE:
    hx-ar --workspace DIR --prompt-file PROGRAM [run flags]   start a run
    hx-ar --workspace DIR --resume RUN                         continue a stopped run

The workspace supplies .harness/ar.toml (editable paths, evaluators, grader and holdout roots; covered by `hx trust`)
and must be a Git repository whose tree outside the editable paths is committed. PROGRAM is the task text every
episode gets. RUN is a run directory, or a bare run name in the workspace's autoresearch directory: `ar/` in the
`state` directory `hx paths` prints.

RUN FLAGS (passed to every episode's `hx run`):
    --model M, --base-url U, --key-env E, --reasoning R, --max-output N, --context-window N, --provider-only A,B,
    --pack A,B, --approval MODE, --yes
Episodes are limited in model turns by hx-ar itself, so --max-turns, --max-tokens, --max-wall, --rpi, --branch,
--trace and --ignore-config are refused.

ENVIRONMENT:
    HX_MODEL, HX_BASE_URL   defaults for --model and --base-url
    HX_AR_RETRY_SECS        base pause before retrying a batch whose every episode crashed (default 60)
";

/// Flags passed through to every episode's `hx run`, and whether each takes a value.
const PASSED: &[(&str, bool)] = &[
    ("--model", true),
    ("--base-url", true),
    ("--key-env", true),
    ("--reasoning", true),
    ("--max-output", true),
    ("--context-window", true),
    ("--provider-only", true),
    ("--pack", true),
    ("--approval", true),
    ("--yes", false),
];

/// The command line, parsed.
#[derive(Debug, Default, PartialEq)]
struct Cli {
    workspace: Option<String>,
    prompt_file: Option<String>,
    resume: Option<String>,
    /// The pass-through flags, as the episodes' `hx run` gets them.
    child: Vec<String>,
    help: bool,
    version: bool,
}

/// A command-line mistake: exits 2.
#[derive(Debug)]
struct Usage(String);
impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Usage {}
fn usage(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Usage(msg.into()))
}

fn parse(args: &[String]) -> Result<Cli> {
    let mut cli = Cli::default();
    let mut it = args.iter();
    while let Some(name) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .filter(|v| !v.starts_with("--"))
                .cloned()
                .ok_or_else(|| usage(format!("{name} requires a value")))
        };
        let slot = match name.as_str() {
            "-h" | "--help" => {
                cli.help = true;
                continue;
            }
            "-V" | "--version" => {
                cli.version = true;
                continue;
            }
            "--workspace" => &mut cli.workspace,
            "--prompt-file" => &mut cli.prompt_file,
            "--resume" => &mut cli.resume,
            flag => match PASSED.iter().find(|(f, _)| *f == flag) {
                Some((f, takes)) => {
                    if cli.child.iter().any(|c| c == f) {
                        bail!(usage(format!("{f} may only be supplied once")));
                    }
                    cli.child.push(f.to_string());
                    if *takes {
                        let v = value(f)?;
                        if matches!(*f, "--max-output" | "--context-window")
                            && !v.parse::<u64>().is_ok_and(|n| n > 0)
                        {
                            bail!(usage(format!("{f} must be a positive integer, got {v:?}")));
                        }
                        cli.child.push(v);
                    }
                    continue;
                }
                None if flag.starts_with("--") => {
                    bail!(usage(format!(
                        "{flag} cannot be passed to hx-ar (see `hx-ar --help`)"
                    )))
                }
                None => bail!(usage(format!(
                    "unexpected argument {flag:?}: the program is read from --prompt-file"
                ))),
            },
        };
        if slot.is_some() {
            bail!(usage(format!("{name} may only be supplied once")));
        }
        *slot = Some(value(name)?);
    }
    match (&cli.prompt_file, &cli.resume) {
        _ if cli.help || cli.version => {}
        (Some(_), Some(_)) => bail!(usage(
            "use --prompt-file for a new run or --resume RUN, not both"
        )),
        (None, None) => bail!(usage(
            "a new run requires --prompt-file PROGRAM (or --resume RUN)"
        )),
        (None, Some(_)) if !cli.child.is_empty() => bail!(usage(
            "a resumed run keeps the flags it was launched with; pass only --workspace and --resume"
        )),
        _ => {}
    }
    Ok(cli)
}

async fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse(&args)?;
    if cli.help {
        return hx::out!("{HELP}");
    }
    if cli.version {
        return hx::outln!("hx-ar {}", env!("CARGO_PKG_VERSION"));
    }
    let workspace = PathBuf::from(cli.workspace.as_deref().unwrap_or("."))
        .canonicalize()
        .context("--workspace")?;
    let dirs = hx::dirs::Dirs::from_env()?;
    let runs = run::runs_dir(&dirs, &workspace);
    hx::confine::note();
    let report = match (&cli.resume, &cli.prompt_file) {
        (Some(name), _) => {
            // A bare run name is a run in this workspace's autoresearch directory.
            let dir = if name.contains('/') {
                PathBuf::from(name)
            } else {
                runs.join(name)
            };
            run::resume(&workspace, &dirs, &dir).await?
        }
        (None, Some(file)) => {
            let program =
                std::fs::read_to_string(file).with_context(|| format!("--prompt-file {file}"))?;
            run::start(
                &workspace,
                &dirs,
                with_provider_limits(cli.child).await,
                program,
            )
            .await?
        }
        (None, None) => unreachable!("parse requires one of them"),
    };
    hx::outln!("{}", serde_json::to_string(&report)?)
}

/// Without `--context-window`, ask the provider once here, rather than in every episode, for the model's window and
/// (unless `--max-output` was given) the output cap it implies, as `hx run` would at launch.
async fn with_provider_limits(mut child: Vec<String>) -> Vec<String> {
    if child.iter().any(|a| a == "--context-window") {
        return child;
    }
    let Some(cfg) = run::llm_cfg(&child) else {
        return child;
    };
    if let Some((window, completion)) = hx::llm::provider_limits(&cfg).await {
        if !child.iter().any(|a| a == "--max-output") {
            let cap = hx::llm::default_max_output(Some(window), completion);
            child.extend(["--max-output".into(), cap.to_string()]);
        }
        child.extend(["--context-window".into(), window.to_string()]);
    }
    child
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // As in hx: a panic takes the crash path, which resume handles.
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        report(info);
        std::process::abort();
    }));
    if let Err(e) = hx::procs::init() {
        eprintln!("Error: cannot become the reaper of the processes hx-ar starts: {e}");
        return std::process::ExitCode::FAILURE;
    }
    // A signal stops the episode children (they stop their own commands) and the running evaluators.
    hx::procs::stop_on_signals();
    let status = match real_main().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) if e.is::<hx::ClosedStdout>() => {
            std::process::ExitCode::from(hx::CLOSED_STDOUT_STATUS)
        }
        Err(e) => {
            eprintln!("Error: {e}");
            for c in e.chain().skip(1) {
                eprintln!("  caused by: {c}");
            }
            std::process::ExitCode::from(if e.is::<Usage>() { 2 } else { 1 })
        }
    };
    hx::procs::shutdown();
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn run_flags_pass_through_and_episode_limits_are_refused() {
        let cli = parse(&argv(
            "--workspace w --prompt-file p --model m --yes --max-output 9",
        ))
        .unwrap();
        assert_eq!(cli.child, argv("--model m --yes --max-output 9"));
        assert_eq!(
            (cli.workspace.as_deref(), cli.prompt_file.as_deref()),
            (Some("w"), Some("p"))
        );
        for bad in [
            "--prompt-file p --max-turns 5",
            "--prompt-file p --rpi",
            "--prompt-file p --ignore-config",
            "--prompt-file p --max-output x",
            "--prompt-file p program",
            "--model m",
            "--prompt-file p --resume r",
            "--resume r --model m",
            "--prompt-file p --yes --yes",
        ] {
            assert!(parse(&argv(bad)).unwrap_err().is::<Usage>(), "{bad}");
        }
        assert!(parse(&argv("--resume r --workspace w")).is_ok());
        assert!(parse(&argv("--help")).unwrap().help);
    }
}
