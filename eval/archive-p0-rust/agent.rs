//! Throwaway P0 loop (P2 replaces it): model replies with one ```luau block; harness runs it and feeds back the output.
use crate::vm::Vm;
use anyhow::{bail, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SYSTEM: &str = include_str!("../prompts/system.md");

pub struct Config {
    pub model: String,
    pub max_turns: usize,
    pub trace: Option<PathBuf>,
    /// "anthropic" (Messages API) or "openai" (chat completions: OpenRouter, Enclave, local).
    pub api: String,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    pub reasoning: Option<String>, // OpenRouter reasoning.effort: low | medium | high
}

#[derive(Serialize, Default)]
pub struct Report {
    pub model: String,
    pub turns: usize,           // model calls
    pub code_turns: usize,      // turns that ran Luau
    pub luau_errors: usize,     // chunks that raised (compile or runtime)
    pub ops_per_turn: Vec<usize>,
    pub input_tokens: u64,      // uncached input
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,          // provider-reported, when available
    pub wall_secs: f64,
    pub truncated_turns: usize, // replies that were cut off, empty or had an unclosed code fence
    pub finished: bool,         // model ended with a plain-text answer (not cut off by max_turns)
    pub final_answer: String,
}

/// Fenced code blocks per CommonMark: an opening line of 3+ backticks (+ info string); it closes at a line made only
/// of at least as many backticks. A ``` inside a line of code (e.g. `x == "```"`) therefore does not close it.
/// Returns (info string, body, closed?) for each fence.
fn fences(text: &str) -> Vec<(String, String, bool)> {
    let mut out = Vec::new();
    let mut open: Option<(usize, String, Vec<&str>)> = None;
    for line in text.split('\n') {
        let t = line.trim_end();
        let ticks = t.trim_start().chars().take_while(|c| *c == '`').count();
        match &mut open {
            None if ticks >= 3 && !t.trim_start()[ticks..].contains('`') => {
                open = Some((ticks, t.trim_start()[ticks..].trim().to_string(), Vec::new()));
            }
            Some((n, _, _)) if ticks >= *n && t.trim_start().len() == ticks => {
                let (_, info, body) = open.take().unwrap();
                out.push((info, body.join("\n"), true));
            }
            Some((_, _, body)) => body.push(line),
            None => {}
        }
    }
    if let Some((_, info, body)) = open {
        out.push((info, body.join("\n"), false));
    }
    out
}

/// Pull the Luau chunk out of a reply. Accepts a closed ```luau fence, a model's native tool-call markup
/// (DeepSeek leaks `<｜DSML｜ parameter name="code">...`), or an unclosed fence whose tail is markup.
fn extract_block(text: &str) -> Option<(String, bool)> {
    let all = fences(text);
    let closed: Vec<String> = all.iter().filter(|(i, _, c)| *c && (i == "luau" || i == "lua")).map(|(_, b, _)| b.clone()).collect();
    if let Some(first) = closed.first() {
        return Some((first.clone(), closed.len() > 1));
    }
    if let Some(i) = text.find("DSML｜ parameter name=") {
        let tail = &text[i..];
        let start = tail.find('>')? + 1;
        let body = &tail[start..];
        let end = body.find("</｜DSML｜ parameter>").unwrap_or(body.len());
        let code = body[..end].trim();
        if !code.is_empty() {
            return Some((code.to_string(), false));
        }
    }
    // unclosed fence whose tail is tool-call markup junk (not a length cut)
    let (_, body, _) = all.iter().find(|(i, _, c)| !*c && (i == "luau" || i == "lua"))?;
    let end = body.find("</｜DSML").or_else(|| body.find("<｜DSML"))?;
    let code = body[..end].trim();
    (!code.is_empty()).then(|| (code.to_string(), false))
}

#[cfg(test)]
mod tests {
    use super::extract_block;

    #[test]
    fn plain_block() {
        let (c, multi) = extract_block("hi\n```luau\nprint(1)\n```\nbye").unwrap();
        assert_eq!((c.as_str(), multi), ("print(1)", false));
    }

    #[test]
    fn backticks_inside_code_do_not_close_the_fence() {
        let t = "```luau\nwrite(\"p.py\", [==[\nif line.strip() == \"```\":\n    x = 1\n]==])\nprint(1)\n```";
        let (c, _) = extract_block(t).unwrap();
        assert!(c.contains("x = 1") && c.ends_with("print(1)"), "{c}");
    }

    #[test]
    fn line_of_only_backticks_inside_needs_a_longer_fence() {
        let t = "````luau\nwrite(\"a.md\", [==[\n```\ncode\n```\n]==])\n````";
        let (c, _) = extract_block(t).unwrap();
        assert!(c.contains("code") && c.ends_with("]==])"), "{c}");
    }

    #[test]
    fn dsml_parameter_and_unclosed_fence_tail() {
        let d = "｜";
        let a = format!("<{d}DSML{d} invoke name=\"luau\">\n<{d}DSML{d} parameter name=\"code\" string=\"true\">\nprint(2)\n</{d}DSML{d} parameter>\n</{d}DSML{d} invoke>");
        assert_eq!(extract_block(&a).unwrap().0, "print(2)");
        let b = format!("```luau\nprint(3)\n</{d}DSML{d} parameter>\n</{d}DSML{d} invoke>");
        assert_eq!(extract_block(&b).unwrap().0, "print(3)");
    }

    #[test]
    fn truncated_fence_is_not_a_block() {
        assert!(extract_block("```luau\nwrite(\"x\", [==[\nhalf of a fi").is_none());
        assert!(extract_block("no code here").is_none());
    }
}

/// One-line-per-file listing of the workspace (gitignore-aware), so the model need not spend a turn on `ls`.
fn file_tree(root: &std::path::Path) -> String {
    let mut out = Vec::new();
    for e in ignore::WalkBuilder::new(root).require_git(false).sort_by_file_path(|a, b| a.cmp(b)).build().flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = e.path().strip_prefix(root).unwrap_or(e.path()).to_string_lossy().to_string();
        if rel.contains("__pycache__") || rel.starts_with(".git/") {
            continue;
        }
        let lines = std::fs::read_to_string(e.path()).map(|t| t.lines().count()).unwrap_or(0);
        out.push(format!("{rel} ({lines} lines)"));
        if out.len() >= 80 {
            out.push("…".into());
            break;
        }
    }
    out.join("\n")
}

async fn call(http: &reqwest::Client, cfg: &Config, key: &str, body: &Value) -> Result<Value> {
    let openai = cfg.api == "openai";
    let default = if openai { "https://openrouter.ai/api/v1" } else { "https://api.anthropic.com" };
    let base = cfg.base_url.clone().or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok().filter(|_| !openai)).unwrap_or(default.into());
    let url = if openai { format!("{base}/chat/completions") } else { format!("{base}/v1/messages") };
    for attempt in 0..6u32 {
        let req = http.post(&url).json(body);
        let req = if openai { req.bearer_auth(key) } else { req.header("x-api-key", key).header("anthropic-version", "2023-06-01") };
        let resp = req.send().await;
        match resp {
            Ok(r) if r.status().is_success() => return Ok(r.json().await?),
            Ok(r) if matches!(r.status().as_u16(), 429 | 500 | 502 | 503 | 529) => {}
            Ok(r) => bail!("API {}: {}", r.status(), r.text().await.unwrap_or_default()),
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
    }
    bail!("API unavailable after retries")
}

pub async fn run(workspace: PathBuf, task: &str, cfg: Config) -> Result<Report> {
    let openai = cfg.api == "openai";
    let key_env = cfg.key_env.clone().unwrap_or_else(|| if openai { "OPENROUTER_API_KEY" } else { "ANTHROPIC_API_KEY" }.into());
    let key = std::env::var(&key_env).map_err(|_| anyhow::anyhow!("{key_env} not set"))?;
    let http = reqwest::Client::builder().timeout(Duration::from_secs(300)).build()?;
    let vm = Vm::new(workspace.clone(), Duration::from_secs(120), 512 << 20)?;
    let mut trace = cfg.trace.as_ref().map(|p| std::fs::File::create(p)).transpose()?;
    let mut log = |s: &str| {
        if let Some(f) = trace.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    };

    let mut rep = Report { model: cfg.model.clone(), ..Default::default() };
    let t0 = Instant::now();
    let task = format!("{task}\n\n[files in the working directory]\n{}", file_tree(&workspace));
    let mut messages: Vec<Value> = vec![json!({"role": "user", "content": task})];
    log(&format!("### TASK\n{task}"));

    let mut consecutive_bad = 0;
    let mut no_block_nudges = 0;
    while rep.turns < cfg.max_turns {
        // Cache the system prompt and the whole conversation prefix (one moving breakpoint).
        let mut msgs = messages.clone();
        if let Some(last) = msgs.last_mut() {
            let text = last["content"].as_str().unwrap_or_default().to_string();
            last["content"] = json!([{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}]);
        }
        let body = if openai {
            let mut all = vec![json!({"role": "system", "content": [{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}]})];
            all.extend(msgs);
            let mut b = json!({"model": cfg.model, "max_tokens": 16384, "messages": all, "usage": {"include": true}});
            if let Some(e) = &cfg.reasoning {
                b["reasoning"] = json!({"effort": e});
            }
            if cfg.base_url.as_deref().is_some_and(|u| u.contains("openrouter")) {
                // keep the upstream provider fixed so cost and caching are comparable across runs
                b["provider"] = json!({"only": ["anthropic"], "allow_fallbacks": false});
            }
            b
        } else {
            json!({
                "model": cfg.model,
                "max_tokens": 16384,
                "system": [{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}],
                "messages": msgs,
            })
        };
        let resp = call(&http, &cfg, &key, &body).await?;
        rep.turns += 1;
        let u = &resp["usage"];
        if openai {
            let d = &u["prompt_tokens_details"];
            let (read, write) = (d["cached_tokens"].as_u64().unwrap_or(0), d["cache_write_tokens"].as_u64().unwrap_or(0));
            rep.input_tokens += u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(read + write);
            rep.cache_read_tokens += read;
            rep.cache_creation_tokens += write;
            rep.output_tokens += u["completion_tokens"].as_u64().unwrap_or(0);
            rep.cost_usd += u["cost"].as_f64().unwrap_or(0.0);
        } else {
            rep.input_tokens += u["input_tokens"].as_u64().unwrap_or(0);
            rep.output_tokens += u["output_tokens"].as_u64().unwrap_or(0);
            rep.cache_creation_tokens += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            rep.cache_read_tokens += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        }
        let text: String = if openai {
            resp["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string()
        } else {
            resp["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n")
        };
        log(&format!("### MODEL (turn {})\n{text}", rep.turns));
        let cut = resp["choices"][0]["finish_reason"].as_str() == Some("length") || resp["stop_reason"].as_str() == Some("max_tokens");
        let unclosed = text.matches("```").count() % 2 == 1 && extract_block(&text).is_none();
        if text.trim().is_empty() || cut || unclosed {
            // A truncated or empty reply is not a final answer: ask for a complete one instead of ending the run.
            rep.truncated_turns += 1;
            consecutive_bad += 1;
            if consecutive_bad > 3 {
                break;
            }
            let shown = if text.trim().is_empty() { "(empty reply)".to_string() } else { text.clone() };
            messages.push(json!({"role": "assistant", "content": shown}));
            let note = "Your last reply was cut off or empty, so nothing ran. Reply again with ONE complete ```luau block (keep it shorter: split big edits across turns), or a plain-text final answer if you are done.";
            log(&format!("### HARNESS\n{note}"));
            messages.push(json!({"role": "user", "content": note}));
            continue;
        }
        consecutive_bad = 0;
        messages.push(json!({"role": "assistant", "content": text}));

        let Some((code, multi)) = extract_block(&text) else {
            if rep.code_turns == 0 && no_block_nudges < 2 {
                // A "final answer" before any code ran is a malformed reply (e.g. pseudo tool-call markup): nudge.
                no_block_nudges += 1;
                rep.truncated_turns += 1;
                let note = "No ```luau block was found in your reply, so nothing ran. Reply with ONE ```luau block that starts the work.";
                log(&format!("### HARNESS\n{note}"));
                messages.push(json!({"role": "user", "content": note}));
                continue;
            }
            rep.finished = true;
            rep.final_answer = text;
            break;
        };
        let o = vm.run(&code).await;
        rep.code_turns += 1;
        rep.luau_errors += o.error as usize;
        rep.ops_per_turn.push(o.ops);
        let mut feedback = o.text;
        if multi {
            feedback.push_str("(note: only the first luau block was run; send one block per reply)\n");
        }
        log(&format!("### RESULT (ops={}, error={})\n{feedback}", o.ops, o.error));
        messages.push(json!({"role": "user", "content": feedback}));
    }
    rep.wall_secs = t0.elapsed().as_secs_f64();
    Ok(rep)
}
