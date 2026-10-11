mod claude_sdk;
mod effects;
mod eventlog;
mod host;
mod llm;
mod mcp;
mod modules;
mod policy;
mod prims;
mod trust;
mod vm;

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
    test         run a package's tests/*.luau
    approve      lock a project package or skill and grant the capabilities it requests
    skills       list skills in .agents/skills and ~/.agents/skills and whether each is approved
    trust        review this workspace's .harness config and hx.lock, then trust it (untrusted config is ignored)
    staged       list staged packages
    accept       review and accept a staged package
    reject       discard a staged package
    upgrade      harness-module upgrade flow (propose|list|show|check|accept|rollback)
    log verify   check an event log's hash chain and list unknown-outcome effects

GLOBAL FLAGS:
    --workspace DIR   workspace root (default: .)
    --yolo            no sandbox, network, path limits or approval gate; .harness/ stays read-only (prints a warning)
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
        "exec" => "hx exec [--workspace DIR] [--pack A,B] [--net|--no-net] [--yolo] CHUNK...\n\nRun each Luau CHUNK in one session and print its result.\n\nExample:\n    hx exec 'print(1+1)' 'print(2)'\n",
        "run" => "hx run --prompt-file F --model M [flags]\n\nFLAGS:\n    --prompt-file F    task text (not needed with --resume of a log that recorded its task)\n    --model M          model id (or HX_MODEL)\n    --api A            openai (Chat Completions, default) | responses (Responses API) | claude-sdk (the `claude` CLI, uses your Claude plan; --model haiku|sonnet|opus) (or HX_API)\n    --base-url U       API base URL (or HX_BASE_URL)\n    --key-env E        env var holding the API key\n    --reasoning R      reasoning effort\n    --max-output N     max output tokens (default 16384)\n    --context-window N the model's context window in tokens (used by the std/compact pack)\n    --provider-only A,B  OpenRouter (--api openai): route only to these providers, no fallbacks\n    --max-turns N      turn limit (default 30)\n    --max-cost X --max-tokens N --max-wall SECS   budgets\n    --trace FILE       write a trace\n    --pack A,B         extra packs\n    --resume LOG       resume from an event log\n\nPrints a JSON report on stdout.\n\nExample:\n    hx run --prompt-file task.md --model claude-sonnet-5-5\n",
        "test" => "hx test PKG_DIR\n\nRun the package's tests/*.luau. Exits 1 if any test fails.\n",
        "approve" => "hx approve PKG\n\nLock a project package's hash and grant exactly the capabilities its manifest requests.\nPKG may also be a skill in .agents/skills or ~/.agents/skills (no hx.toml needed; no capabilities).\n",
        "skills" => "hx skills\n\nList skills in <workspace>/.agents/skills and ~/.agents/skills (project first) and whether each is approved.\nOnly approved, unchanged skills are visible to the model; approve with `hx approve NAME`.\n",
        "staged" => "hx staged\n\nList staged packages.\n",
        "trust" => "hx trust [--yes]\n\nShow this workspace's config (.harness/{policy,grants,mcp}.toml, .harness/init.luau, .harness/override/,\nhx.lock) and trust its exact content. Until then hx ignores it: a cloned repo cannot widen the sandbox, start MCP\nservers or pre-approve packages and skills. The record lives in ~/.config/hx/trust.toml.\nPrompts on a terminal; without one, --yes is required. HX_TRUST=1 trusts the workspace for one launch.\n",
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

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// A comma-separated flag value as a list (empty when the flag is absent).
fn list_arg(args: &[String], name: &str) -> Vec<String> {
    arg(args, name).map(|v| v.split(',').map(String::from).collect()).unwrap_or_default()
}

/// A numeric flag: None when absent, a usage error when present but not a number (a mistyped `--max-cost` must not
/// silently run without a budget).
fn parsed<T: std::str::FromStr>(args: &[String], name: &str) -> Result<Option<T>> {
    match arg(args, name) {
        None => Ok(None),
        Some(v) => v.parse().map(Some).map_err(|_| usage(&format!("{name}: expected a number, got {v:?}"))),
    }
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

/// Top-level help, version, or `<command> --help`: printed, and the caller is done.
fn print_info(args: &[String]) -> bool {
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => print!("{TOP_HELP}"),
        Some("-V" | "-v" | "--version" | "version") => println!("hx {}", env!("CARGO_PKG_VERSION")),
        Some(c) if args.iter().skip(1).any(|a| a == "-h" || a == "--help") => match help_for(c) {
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
    let workspace = PathBuf::from(arg(&args, "--workspace").unwrap_or_else(|| ".".into())).canonicalize()?;
    // The sandbox, path policy and approval gate are ON by default; `--yolo` is an explicit opt-out (and says so on stderr).
    let sandbox = !has(&args, "--yolo");
    if !sandbox {
        eprintln!("warning: --yolo: no sandbox, network open, no path limits, no approval gate (.harness/ and hx.lock stay read-only)");
    }
    // Only commands that run code need a policy (and with it a temp dir and a log session); admin commands do not.
    let mk_policy = || -> Result<Arc<policy::Policy>> { Ok(Arc::new(policy::Policy::new(&workspace, sandbox, arg(&args, "--approval"), arg(&args, "--resume").map(PathBuf::from).as_deref(), net_flag(&args))?)) };
    match args[0].as_str() {
        "exec" => {
            trust::warn_if_untrusted(&workspace);
            cmd_exec(&args, mk_policy()?).await
        }
        "run" => {
            trust::warn_if_untrusted(&workspace);
            cmd_run(&args, workspace.clone(), mk_policy()?).await
        }
        "test" => cmd_test(&args, &workspace, mk_policy()?).await,
        "approve" => cmd_approve(&args, &workspace),
        "trust" => cmd_trust(&args, &workspace),
        // hx skills : agent skills on disk and their approval state
        "skills" => {
            print!("{}", modules::Resolver::new(workspace).skills_status());
            Ok(())
        }
        "upgrade" => cmd_upgrade(&args, &workspace),
        // hx staged | hx accept NAME [--yes] | hx reject NAME : the user's side of promotion (nothing the agent can call does this)
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
        "log" if args.get(1).map(String::as_str) == Some("verify") => cmd_log_verify(&args),
        _ => Err(usage("usage: hx log verify FILE (see `hx log --help`)")),
    }
}

/// The Luau chunks of `hx exec`: every argument after the command that is not a global flag or its value.
fn exec_chunks(args: &[String]) -> Vec<&str> {
    let mut out = vec![];
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--workspace" | "--pack" | "--approval" | "--resume" => {
                it.next();
            }
            "--net" | "--no-net" | "--yolo" => {}
            c => out.push(c),
        }
    }
    out
}

/// hx exec --workspace DIR 'chunk1' 'chunk2' : run chunks in one session (no API key needed; used for testing)
async fn cmd_exec(args: &[String], policy: Arc<policy::Policy>) -> Result<()> {
    let vm = vm::Vm::new(policy, Duration::from_secs(30), 256 << 20, None, None)?;
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
        api: arg(args, "--api").or_else(|| std::env::var("HX_API").ok()).unwrap_or_else(|| "openai".into()),
        base_url: arg(args, "--base-url").or_else(|| std::env::var("HX_BASE_URL").ok()),
        key_env: arg(args, "--key-env"),
        reasoning: arg(args, "--reasoning"),
        max_output: parsed(args, "--max-output")?.unwrap_or(16384),
        context_window: parsed(args, "--context-window")?,
        cache_key: Some(session),
        provider_only: list_arg(args, "--provider-only"),
    })
}

/// hx run --workspace DIR --prompt-file F --model M [--api openai --base-url U --key-env E --reasoning R] [--max-turns N] [--trace FILE]
async fn cmd_run(args: &[String], workspace: PathBuf, policy: Arc<policy::Policy>) -> Result<()> {
    let replay = arg(args, "--resume").map(|p| effects::load_replay(Path::new(&p))).transpose()?;
    let task = match (replay.as_ref().and_then(|r| r.task.clone()), arg(args, "--prompt-file")) {
        (Some(t), _) => t,
        (None, Some(f)) => std::fs::read_to_string(f)?,
        (None, None) => bail!("--prompt-file required (or --resume LOG for a log that recorded its task)"),
    };
    let cfg = llm_cfg(args, policy.log.session.clone())?;
    let max_turns = parsed(args, "--max-turns")?.unwrap_or(30);
    let budget = policy::Budget { max_cost: parsed(args, "--max-cost")?, max_tokens: parsed(args, "--max-tokens")?, max_wall_secs: parsed(args, "--max-wall")? };
    let host = host::Host::new(cfg, workspace, policy, max_turns, arg(args, "--trace").map(PathBuf::from), budget, list_arg(args, "--pack"))?;
    if let Some(r) = replay {
        host.set_replay(r);
    }
    let report = host.run_task(task, max_turns).await?;
    println!("{}", serde_json::to_string(&report)?);
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
    let vm = vm::Vm::new(policy, Duration::from_secs(30), 256 << 20, None, None)?;
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

/// hx upgrade propose|list|show|check|accept|rollback : the harness-module upgrade flow (AC5)
fn cmd_upgrade(args: &[String], workspace: &Path) -> Result<()> {
    let r = modules::Resolver::new(workspace.to_path_buf());
    let sub = args.get(1).map(String::as_str).unwrap_or("");
    let id = || args.get(2).cloned().ok_or_else(|| usage(&format!("usage: hx upgrade {sub} ID")));
    match sub {
        "propose" => {
            let (m, f) = (args.get(2).ok_or_else(|| usage("usage: hx upgrade propose MODULE FILE [--reason R]"))?, args.get(3).ok_or_else(|| usage("usage: hx upgrade propose MODULE FILE"))?);
            println!("{}", r.propose(m, &std::fs::read_to_string(f)?, &arg(args, "--reason").unwrap_or_default())?);
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
            if !has(args, "--yes") {
                confirm("activate this change?")?;
            }
            let rt = trust::Retrust::before(workspace);
            println!("{}", r.accept_upgrade(&id, has(args, "--force"))?);
            rt.after()?;
        }
        "rollback" => {
            let rt = trust::Retrust::before(workspace);
            println!("{}", r.rollback_upgrade(args.get(2).ok_or_else(|| usage("usage: hx upgrade rollback MODULE"))?)?);
            rt.after()?;
        }
        _ => return Err(usage("usage: hx upgrade propose|list|show|check|accept|rollback ...")),
    }
    Ok(())
}

/// hx accept NAME [--yes] : review a staged package, then lock and grant it
fn cmd_accept(args: &[String], workspace: &Path) -> Result<()> {
    let n = args.get(1).ok_or_else(|| usage("usage: hx accept NAME [--yes]"))?;
    let r = modules::Resolver::new(workspace.to_path_buf());
    println!("{}", r.review(n)?);
    if !has(args, "--yes") {
        confirm(&format!("accept {n}? it will be locked and granted the capabilities above"))?;
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
    if !r.chain_ok {
        std::process::exit(1);
    }
    if !r.unknown.is_empty() {
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
        let a = argv("exec --workspace /w print(1) --pack a,b --net print(2) --approval auto --yolo --resume l.jsonl --no-net print(3)");
        assert_eq!(exec_chunks(&a), vec!["print(1)", "print(2)", "print(3)"]);
        assert!(exec_chunks(&argv("exec")).is_empty());
    }

    #[test]
    fn flag_helpers() {
        let a = argv("run --pack a,b --max-turns 7 --max-cost x --net");
        assert_eq!(list_arg(&a, "--pack"), vec!["a", "b"]);
        assert!(list_arg(&a, "--provider-only").is_empty());
        assert_eq!(parsed::<u32>(&a, "--max-turns").unwrap(), Some(7));
        assert_eq!(parsed::<u32>(&a, "--max-tokens").unwrap(), None);
        let e = parsed::<f64>(&a, "--max-cost").unwrap_err();
        assert!(e.is::<Usage>() && e.to_string().contains("--max-cost"), "a mistyped budget is a usage error, not unlimited: {e}");
        assert_eq!(net_flag(&a), Some(true));
        assert_eq!(net_flag(&argv("run --no-net")), Some(false));
        assert_eq!(net_flag(&argv("run")), None);
    }

    #[test]
    fn llm_cfg_reads_flags() {
        let a = argv("run --model m --api responses --max-output 99 --context-window 1000 --provider-only x,y");
        let c = llm_cfg(&a, "s".into()).unwrap();
        assert_eq!((c.model.as_str(), c.api.as_str(), c.max_output, c.context_window), ("m", "responses", 99, Some(1000)));
        assert_eq!((c.provider_only, c.cache_key), (vec!["x".to_string(), "y".to_string()], Some("s".to_string())));
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
