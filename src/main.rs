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
use std::path::PathBuf;
use std::time::Duration;

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let workspace = PathBuf::from(arg(&args, "--workspace").unwrap_or_else(|| ".".into())).canonicalize()?;
    // The sandbox and path policy are ON by default; `--no-sandbox` is an explicit opt-out (and says so on stderr).
    let sandbox = !args.iter().any(|a| a == "--no-sandbox");
    if !sandbox {
        eprintln!("warning: --no-sandbox: sh and file primitives are NOT restricted");
    }
    let policy = std::sync::Arc::new(policy::Policy::new(&workspace, sandbox, arg(&args, "--approval"), arg(&args, "--resume").map(PathBuf::from).as_deref(), if args.iter().any(|a| a == "--net") { Some(true) } else if args.iter().any(|a| a == "--no-net") { Some(false) } else { None })?);
    match args.first().map(String::as_str) {
        // hx exec --workspace DIR 'chunk1' 'chunk2'  : run chunks in one session (no API key needed; used for testing)
        Some("exec") => {
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
            let dir = PathBuf::from(args.get(1).ok_or_else(|| anyhow::anyhow!("usage: hx test PKG_DIR"))?).canonicalize()?;
            // Mount the package under test so `require("name")` finds it without installing or locking it, but ONLY for a
            // directory outside .harness/pkgs (e.g. a staged tool). Installed packages keep their lock and grant checks.
            let installed = dir.parent().is_some_and(|p| p == workspace.join(".harness/pkgs"));
            if installed {
                // fall through: resolved normally via .harness/pkgs (lock + grants enforced)
            } else if let Ok(t) = std::fs::read_to_string(dir.join("hx.toml")) {
                if let Some(name) = t.lines().find_map(|l| l.trim().strip_prefix("name = ").map(|v| v.trim_matches('"').to_string())) {
                    let _ = modules::TEST_PKG.set((name, dir.clone()));
                }
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
            let pkg = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: hx approve PKG"))?;
            println!("{}", modules::Resolver::new(workspace).approve(pkg)?);
            Ok(())
        }
        // hx upgrade propose|list|show|check|accept|rollback : the harness-module upgrade flow (AC5)
        Some("upgrade") => {
            let r = modules::Resolver::new(workspace);
            let sub = args.get(1).map(String::as_str).unwrap_or("");
            let id = || args.get(2).cloned().ok_or_else(|| anyhow::anyhow!("usage: hx upgrade {sub} ID"));
            match sub {
                "propose" => {
                    let (m, f) = (args.get(2).ok_or_else(|| anyhow::anyhow!("usage: hx upgrade propose MODULE FILE [--reason R]"))?, args.get(3).ok_or_else(|| anyhow::anyhow!("usage: hx upgrade propose MODULE FILE"))?);
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
                        use std::io::Write;
                        print!("activate this change? [y/N] ");
                        std::io::stdout().flush()?;
                        let mut l = String::new();
                        std::io::stdin().read_line(&mut l)?;
                        if !matches!(l.trim(), "y" | "Y" | "yes") {
                            bail!("not accepted");
                        }
                    }
                    println!("{}", r.accept_upgrade(&id, args.iter().any(|a| a == "--force"))?);
                }
                "rollback" => println!("{}", r.rollback_upgrade(args.get(2).ok_or_else(|| anyhow::anyhow!("usage: hx upgrade rollback MODULE"))?)?),
                _ => bail!("usage: hx upgrade propose|list|show|check|accept|rollback ..."),
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
            let n = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: hx reject NAME"))?;
            modules::Resolver::new(workspace).reject(n)?;
            println!("rejected {n}");
            Ok(())
        }
        Some("accept") => {
            let n = args.get(1).ok_or_else(|| anyhow::anyhow!("usage: hx accept NAME [--yes]"))?;
            let r = modules::Resolver::new(workspace);
            println!("{}", r.review(n)?);
            if !args.iter().any(|a| a == "--yes") {
                use std::io::Write;
                print!("accept {n}? it will be locked and granted the capabilities above [y/N] ");
                std::io::stdout().flush()?;
                let mut l = String::new();
                std::io::stdin().read_line(&mut l)?;
                if !matches!(l.trim(), "y" | "Y" | "yes") {
                    bail!("not accepted");
                }
            }
            println!("{}", r.accept(n)?);
            Ok(())
        }
        // hx log verify FILE : check the hash chain and list effects with an unknown outcome (exit 1 tampered, 3 unknown)
        Some("log") if args.get(1).map(String::as_str) == Some("verify") => {
            let f = PathBuf::from(args.get(2).ok_or_else(|| anyhow::anyhow!("usage: hx log verify FILE"))?);
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
        _ => bail!("usage: hx exec|run|test|approve|staged|accept|reject|upgrade|log verify ..."),
    }
}
