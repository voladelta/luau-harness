mod effects;
mod eventlog;
mod host;
mod llm;
mod mcp;
mod modules;
mod policy;
mod prims;
mod vm;

use anyhow::{bail, Result};
use std::io::IsTerminal;
use std::path::PathBuf;
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
    test         run a package's tests/*.luau
    approve      lock a project package and grant the capabilities it requests
    staged       list staged packages
    accept       review and accept a staged package
    reject       discard a staged package
    upgrade      harness-module upgrade flow (propose|list|show|check|accept|rollback)
    log verify   check an event log's hash chain and list unknown-outcome effects

GLOBAL FLAGS:
    --workspace DIR   workspace root (default: .)
    --no-sandbox      disable the sandbox and path policy (prints a warning)
    --net | --no-net  allow or deny network access
    --approval MODE   approval mode
    --resume LOG      resume from an event log
    -h, --help        show help (also `hx <command> --help`)
    -V, --version     show version

EXAMPLES:
    hx exec --workspace . 'print(1+1)'
    hx run --prompt-file task.md --model claude-sonnet-5-5 --max-turns 20
    hx log verify .harness/run.jsonl

ENVIRONMENT:
    HX_MODEL, HX_API, HX_BASE_URL   defaults for the matching `run` flags
    HX_DEBUG=1                      print the full error chain on failure
";

fn help_for(cmd: &str) -> Option<&'static str> {
    Some(match cmd {
        "exec" => "hx exec [--workspace DIR] [--pack A,B] [--net|--no-net] [--no-sandbox] CHUNK...\n\nRun each Luau CHUNK in one session and print its result.\n\nExample:\n    hx exec 'print(1+1)' 'print(2)'\n",
        "run" => "hx run --prompt-file F --model M [flags]\n\nFLAGS:\n    --prompt-file F    task text (not needed with --resume of a log that recorded its task)\n    --model M          model id (or HX_MODEL)\n    --api A            anthropic (Messages, default) | openai (Chat Completions) | responses (Responses API) (or HX_API)\n    --base-url U       API base URL (or HX_BASE_URL)\n    --key-env E        env var holding the API key\n    --reasoning R      reasoning effort\n    --max-output N     max output tokens (default 16384)\n    --provider-only A,B  OpenRouter (--api openai): route only to these providers, no fallbacks\n    --max-turns N      turn limit (default 30)\n    --max-cost X --max-tokens N --max-wall SECS   budgets\n    --trace FILE       write a trace\n    --pack A,B         extra packs\n    --resume LOG       resume from an event log\n\nPrints a JSON report on stdout.\n\nExample:\n    hx run --prompt-file task.md --model claude-sonnet-5-5\n",
        "test" => "hx test PKG_DIR\n\nRun the package's tests/*.luau. Exits 1 if any test fails.\n",
        "approve" => "hx approve PKG\n\nLock a project package's hash and grant exactly the capabilities its manifest requests.\n",
        "staged" => "hx staged\n\nList staged packages.\n",
        "accept" => "hx accept NAME [--yes]\n\nReview a staged package, then lock it and grant its capabilities.\nPrompts on a terminal; without one, --yes is required.\n",
        "reject" => "hx reject NAME\n\nDiscard a staged package.\n",
        "upgrade" => "hx upgrade propose MODULE FILE [--reason R]\nhx upgrade list|show ID|check ID\nhx upgrade accept ID [--yes] [--force]\nhx upgrade rollback MODULE\n",
        "log" => "hx log verify FILE\n\nCheck the hash chain and list effects with an unknown outcome.\nExit codes: 0 ok, 1 tampered, 3 unknown outcomes.\n",
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

async fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => {
            print!("{TOP_HELP}");
            return Ok(());
        }
        Some("-V" | "-v" | "--version" | "version") => {
            println!("hx {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some(c) if args.iter().skip(1).any(|a| a == "-h" || a == "--help") => {
            if let Some(h) = help_for(c) {
                print!("{h}");
                return Ok(());
            }
        }
        _ => {}
    }
    if args.first().is_some_and(|c| help_for(c).is_none()) {
        eprintln!("Error: unknown command `{}`\n\nRun `hx --help` for usage.", args[0]);
        std::process::exit(2);
    }
    let workspace = PathBuf::from(arg(&args, "--workspace").unwrap_or_else(|| ".".into())).canonicalize()?;
    // The sandbox and path policy are ON by default; `--no-sandbox` is an explicit opt-out (and says so on stderr).
    let sandbox = !args.iter().any(|a| a == "--no-sandbox");
    if !sandbox {
        eprintln!("warning: --no-sandbox: sh and file primitives are NOT restricted");
    }
    // Only commands that run code need a policy (and with it a temp dir and a log session); admin commands do not.
    let mk_policy = || -> Result<std::sync::Arc<policy::Policy>> { Ok(std::sync::Arc::new(policy::Policy::new(&workspace, sandbox, arg(&args, "--approval"), arg(&args, "--resume").map(PathBuf::from).as_deref(), if args.iter().any(|a| a == "--net") { Some(true) } else if args.iter().any(|a| a == "--no-net") { Some(false) } else { None })?)) };
    match args.first().map(String::as_str) {
        // hx exec --workspace DIR 'chunk1' 'chunk2'  : run chunks in one session (no API key needed; used for testing)
        Some("exec") => {
            let policy = mk_policy()?;
            let vm = vm::Vm::new(policy.clone(), Duration::from_secs(30), 256 << 20, None, None)?;
            let packs: Vec<String> = arg(&args, "--pack").map(|v| v.split(',').map(String::from).collect()).unwrap_or_default();
            let mut all = vec!["std/packages".to_string(), "std/mcp".to_string()];
            all.extend(packs);
            vm.install_packs(&all).await?;
            let mut it = args.iter().skip(1);
            while let Some(a) = it.next() {
                if matches!(a.as_str(), "--workspace" | "--pack" | "--approval") {
                    it.next();
                    continue;
                }
                if matches!(a.as_str(), "--net" | "--no-net" | "--no-sandbox") {
                    continue;
                }
                println!("{}", vm.exec_text(a).await?);
            }
            Ok(())
        }
        // hx run --workspace DIR --prompt-file F --model M [--api openai --base-url U --key-env E --reasoning R] [--max-turns N] [--trace FILE]
        Some("run") => {
            let policy = mk_policy()?;
            let resume = arg(&args, "--resume").map(PathBuf::from);
            let replay = resume.as_ref().map(|p| effects::load_replay(p)).transpose()?;
            let task = match (&replay, arg(&args, "--prompt-file")) {
                (Some(r), _) if r.task.is_some() => r.task.clone().unwrap(),
                (_, Some(f)) => std::fs::read_to_string(f)?,
                _ => bail!("--prompt-file required (or --resume LOG for a log that recorded its task)"),
            };
            let cfg = llm::LlmCfg {
                model: arg(&args, "--model").or_else(|| std::env::var("HX_MODEL").ok()).ok_or_else(|| anyhow::anyhow!("--model or HX_MODEL required"))?,
                api: arg(&args, "--api").or_else(|| std::env::var("HX_API").ok()).unwrap_or_else(|| "anthropic".into()),
                base_url: arg(&args, "--base-url").or_else(|| std::env::var("HX_BASE_URL").ok()),
                key_env: arg(&args, "--key-env"),
                reasoning: arg(&args, "--reasoning"),
                max_output: arg(&args, "--max-output").and_then(|v| v.parse().ok()).unwrap_or(16384),
                cache_key: Some(policy.log.session.clone()),
                provider_only: arg(&args, "--provider-only").map(|v| v.split(',').map(String::from).collect()).unwrap_or_default(),
            };
            let max_turns = arg(&args, "--max-turns").and_then(|s| s.parse().ok()).unwrap_or(30);
            let budget = policy::Budget { max_cost: arg(&args, "--max-cost").and_then(|v| v.parse().ok()), max_tokens: arg(&args, "--max-tokens").and_then(|v| v.parse().ok()), max_wall_secs: arg(&args, "--max-wall").and_then(|v| v.parse().ok()) };
            let host = host::Host::new(cfg, workspace, policy.clone(), max_turns, arg(&args, "--trace").map(PathBuf::from), budget, arg(&args, "--pack").map(|v| v.split(',').map(String::from).collect()).unwrap_or_default())?;
            if let Some(r) = replay {
                host.set_replay(r);
            }
            let report = host.run_task(task, max_turns).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        // hx test PKG_DIR : run the package's tests/*.luau
        Some("test") => {
            let policy = mk_policy()?;
            let dir = PathBuf::from(args.get(1).ok_or_else(|| usage("usage: hx test PKG_DIR"))?).canonicalize()?;
            // Mount the package under test so `require("name")` finds it without installing or locking it, but ONLY for a
            // directory outside .harness/pkgs (e.g. a staged tool). Installed packages keep their lock and grant checks.
            let installed = dir.parent().is_some_and(|p| p == workspace.join(".harness/pkgs"));
            if installed {
                // fall through: resolved normally via .harness/pkgs (lock + grants enforced)
            } else if let Ok(t) = std::fs::read_to_string(dir.join("hx.toml"))
                && let Some(name) = t.lines().find_map(|l| l.trim().strip_prefix("name = ").map(|v| v.trim_matches('"').to_string())) {
                    let _ = modules::TEST_PKG.set((name, dir.clone()));
                }
            let vm = vm::Vm::new(policy.clone(), Duration::from_secs(30), 256 << 20, None, None)?;
            let (pass, fail, report) = vm.run_tests(&dir).await?;
            print!("{report}");
            println!("{pass} passed, {fail} failed");
            if fail > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        // hx approve PKG : lock a project package's hash and grant exactly the capabilities its manifest requests
        Some("approve") => {
            let pkg = args.get(1).ok_or_else(|| usage("usage: hx approve PKG"))?;
            println!("{}", modules::Resolver::new(workspace).approve(pkg)?);
            Ok(())
        }
        // hx upgrade propose|list|show|check|accept|rollback : the harness-module upgrade flow (AC5)
        Some("upgrade") => {
            let r = modules::Resolver::new(workspace);
            let sub = args.get(1).map(String::as_str).unwrap_or("");
            let id = || args.get(2).cloned().ok_or_else(|| usage(&format!("usage: hx upgrade {sub} ID")));
            match sub {
                "propose" => {
                    let (m, f) = (args.get(2).ok_or_else(|| usage("usage: hx upgrade propose MODULE FILE [--reason R]"))?, args.get(3).ok_or_else(|| usage("usage: hx upgrade propose MODULE FILE"))?);
                    println!("{}", r.propose(m, &std::fs::read_to_string(f)?, &arg(&args, "--reason").unwrap_or_default())?);
                }
                "list" => {
                    for p in r.list_proposals() {
                        println!("{}  {}  {}", p["id"].as_str().unwrap_or(""), p["module"].as_str().unwrap_or(""), p["reason"].as_str().unwrap_or(""));
                    }
                }
                "show" => println!("{}", r.show_proposal(&id()?)?),
                "check" => println!("{}", r.check_proposal(&id()?)?),
                "accept" => {
                    let id = id()?;
                    println!("{}", r.show_proposal(&id)?);
                    if !args.iter().any(|a| a == "--yes") {
                        confirm("activate this change?")?;
                    }
                    println!("{}", r.accept_upgrade(&id, args.iter().any(|a| a == "--force"))?);
                }
                "rollback" => println!("{}", r.rollback_upgrade(args.get(2).ok_or_else(|| usage("usage: hx upgrade rollback MODULE"))?)?),
                _ => return Err(usage("usage: hx upgrade propose|list|show|check|accept|rollback ...")),
            }
            Ok(())
        }
        // hx staged | hx accept NAME [--yes] | hx reject NAME : the user's side of promotion (nothing the agent can call does this)
        Some("staged") => {
            let r = modules::Resolver::new(workspace);
            let l = r.list_staged();
            println!("{}", if l.is_empty() { "nothing staged".to_string() } else { l.join("\n") });
            Ok(())
        }
        Some("reject") => {
            let n = args.get(1).ok_or_else(|| usage("usage: hx reject NAME"))?;
            modules::Resolver::new(workspace).reject(n)?;
            println!("rejected {n}");
            Ok(())
        }
        Some("accept") => {
            let n = args.get(1).ok_or_else(|| usage("usage: hx accept NAME [--yes]"))?;
            let r = modules::Resolver::new(workspace);
            println!("{}", r.review(n)?);
            if !args.iter().any(|a| a == "--yes") {
                confirm(&format!("accept {n}? it will be locked and granted the capabilities above"))?;
            }
            println!("{}", r.accept(n)?);
            Ok(())
        }
        // hx log verify FILE : check the hash chain and list effects with an unknown outcome (exit 1 tampered, 3 unknown)
        Some("log") if args.get(1).map(String::as_str) == Some("verify") => {
            let f = PathBuf::from(args.get(2).ok_or_else(|| usage("usage: hx log verify FILE"))?);
            let r = eventlog::verify(&f)?;
            println!("{} records, hash chain {}", r.records, if r.chain_ok { "intact".to_string() } else { format!("BROKEN at line {}", r.bad_line.unwrap_or(0)) });
            for (seq, tool, summary) in &r.unknown {
                println!("UNKNOWN OUTCOME: #{seq} {tool}: {summary}");
            }
            if !r.chain_ok {
                std::process::exit(1);
            }
            if !r.unknown.is_empty() {
                std::process::exit(3);
            }
            Ok(())
        }
        _ => Err(usage("usage: hx log verify FILE (see `hx log --help`)")),
    }
}
