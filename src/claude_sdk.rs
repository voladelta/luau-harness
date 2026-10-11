//! `--api claude-sdk`: use Claude through the `claude` CLI (the way the Claude Agent SDK does), so the calls draw on the
//! user's Claude plan instead of an API key. One long-lived `claude -p --input-format stream-json --output-format
//! stream-json` child holds the conversation; hx sends only the new user turn each time. The CLI cannot take custom tool
//! definitions, so a tool call is plain text: `<name>` + the single string argument + `</name>`, parsed back into a call here.
use crate::llm::{LlmCfg, Reply};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// idle children kept for reuse; the oldest is dropped (and killed) beyond this
const POOL_MAX: usize = 8;
static POOL: OnceLock<Mutex<Vec<Proc>>> = OnceLock::new();
static CALL_ID: AtomicU64 = AtomicU64::new(0);

struct Proc {
    _child: Child,
    stdin: ChildStdin,
    out: tokio::io::Lines<BufReader<ChildStdout>>,
    /// what this child was started with; a request with a different system prompt or model needs another child
    sys_sha: String,
    /// the user turns this child has seen, in order (its own replies sit between them)
    users: Vec<String>,
    /// total_cost_usd is cumulative per session; the last value seen, to report each turn's share
    last_cost: f64,
}

/// A tool the model can call in text: `<name>VALUE</name>` where VALUE is the tool's one string parameter.
struct TextTool {
    name: String,
    param: String,
}

fn text_tools(tools: &[Value]) -> Vec<TextTool> {
    tools
        .iter()
        .filter_map(|t| {
            let props = t["parameters"]["properties"].as_object()?;
            let (param, _) = props.iter().find(|(_, v)| v["type"] == "string")?;
            Some(TextTool { name: t["name"].as_str()?.to_string(), param: param.clone() })
        })
        .collect()
}

fn system_with_protocol(system: &str, tools: &[Value], tt: &[TextTool]) -> String {
    if tt.is_empty() {
        return system.to_string();
    }
    let mut s = format!("{system}\n\n# How to call tools\nYour tools are called with text, not native tool calling. To call one, end your reply with exactly one block, the tag named after the tool around its argument, and write nothing after the closing tag:\n");
    for t in tt {
        let desc = tools.iter().find(|x| x["name"] == t.name.as_str()).and_then(|x| x["description"].as_str()).unwrap_or("");
        s += &format!("\n<{n}>\n(the `{p}` argument, verbatim, no code fence)\n</{n}>\n{desc}\n", n = t.name, p = t.param);
    }
    if let Some(t) = tt.first() {
        s += &format!("\nExample reply: a sentence of what you are about to do, then\n<{n}>\n...the {p}...\n</{n}>\n", n = t.name, p = t.param);
    }
    s += "\nUse only this exact tag form: no <invoke>, no <function_calls>, no JSON wrapper. The harness runs the call and answers in the next user message as `[<tool> result]`. Never write results yourself. To finish, reply in plain text with no block.";
    s
}

/// One side of the wire conversation: its text and the images (`{mime, data}`) attached to it.
type Turn = (bool, String, Vec<Value>);

/// Neutral messages -> alternating (user, assistant) wire turns. Tool results and user messages between two assistant
/// turns merge into one user turn.
fn wire(msgs: &[Value], tt: &[TextTool]) -> Vec<Turn> {
    let mut names: HashMap<String, String> = HashMap::new();
    let mut out: Vec<Turn> = Vec::new();
    let mut push = |user: bool, t: String, imgs: Vec<Value>| match out.last_mut() {
        Some((u, s, im)) if *u == user => {
            s.push_str("\n\n");
            s.push_str(&t);
            im.extend(imgs);
        }
        _ => out.push((user, t, imgs)),
    };
    for m in msgs {
        match m["role"].as_str().unwrap_or("user") {
            "assistant" => {
                let mut t = m["content"].as_str().unwrap_or("").to_string();
                for c in m["calls"].as_array().into_iter().flatten() {
                    let name = c["name"].as_str().unwrap_or("");
                    names.insert(c["id"].as_str().unwrap_or("").to_string(), name.to_string());
                    if let Some(tool) = tt.iter().find(|x| x.name == name) {
                        let arg = c["input"][&tool.param].as_str().unwrap_or("");
                        t += &format!("\n<{name}>\n{arg}\n</{name}>");
                    }
                }
                push(false, t.trim_start().to_string(), vec![]);
            }
            "tool" => {
                let name = names.get(m["id"].as_str().unwrap_or("")).cloned().unwrap_or_else(|| "tool".into());
                push(true, format!("[{name} result]\n{}", m["content"].as_str().unwrap_or("")), vec![]);
            }
            _ => {
                let imgs = m["images"].as_array().cloned().unwrap_or_default();
                push(true, m["content"].as_str().unwrap_or("").to_string(), imgs);
            }
        }
    }
    out
}

/// The `content` of a user message: a plain string, or text + base64 image blocks when there are images.
fn content(text: &str, imgs: &[Value]) -> Value {
    if imgs.is_empty() {
        return json!(text);
    }
    let mut parts = vec![json!({"type": "text", "text": text})];
    parts.extend(imgs.iter().map(|i| json!({"type": "image", "source": {"type": "base64", "media_type": i["mime"].as_str().unwrap_or("image/png"), "data": i["data"].as_str().unwrap_or("")}})));
    json!(parts)
}

/// What a user turn is remembered as for child reuse: its text plus a hash of each image.
fn turn_key(t: &Turn) -> String {
    t.2.iter().fold(t.1.clone(), |k, i| format!("{k}\n[image {}]", sha(i["data"].as_str().unwrap_or(""))))
}

fn sha(s: &str) -> String {
    crate::eventlog::sha_bytes(s.as_bytes())
}

async fn spawn(cfg: &LlmCfg, system: &str, max_tokens: u64) -> Result<Proc> {
    let dir = std::env::temp_dir().join("hx-claude-sdk");
    std::fs::create_dir_all(&dir)?;
    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--tools", "", "--setting-sources", "", "--strict-mcp-config", "--no-session-persistence", "--model", &cfg.model, "--system-prompt", system]);
    if let Some(e) = &cfg.reasoning {
        cmd.args(["--effort", e]);
    }
    // never bill an API key by accident: this backend exists to use the plan's login
    cmd.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN").env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", max_tokens.to_string());
    cmd.current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("could not start `claude` (is Claude Code installed and logged in?): {e}"))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("claude: no stdin"))?;
    let out = BufReader::new(child.stdout.take().ok_or_else(|| anyhow::anyhow!("claude: no stdout"))?).lines();
    Ok(Proc { _child: child, stdin, out, sys_sha: sha(&format!("{}\n{}\n{:?}", cfg.model, system, cfg.reasoning)), users: Vec::new(), last_cost: 0.0 })
}

/// `req` is the same shape `llm::chat` takes.
pub async fn chat(cfg: &LlmCfg, req: &Value) -> Result<Reply> {
    let tools: Vec<Value> = req["tools"].as_array().cloned().unwrap_or_default();
    let tt = text_tools(&tools);
    let system = system_with_protocol(req["system"].as_str().unwrap_or(""), &tools, &tt);
    let max_tokens = req["max_tokens"].as_u64().unwrap_or(cfg.max_output);
    let msgs: Vec<Value> = req["messages"].as_array().cloned().unwrap_or_default();
    let w = wire(&msgs, &tt);
    let users: Vec<String> = w.iter().filter(|t| t.0).map(turn_key).collect();
    if users.is_empty() {
        bail!("claude-sdk: the request has no user message");
    }
    if w.last().is_some_and(|t| !t.0) {
        bail!("claude-sdk: the request must end with a user message");
    }
    let sys_sha = sha(&format!("{}\n{}\n{:?}", cfg.model, system, cfg.reasoning));

    // a child that has seen exactly the earlier user turns just gets the new one; otherwise start over with the transcript
    let pool = POOL.get_or_init(|| Mutex::new(Vec::new()));
    let reused = {
        let mut p = pool.lock().await;
        p.iter().position(|c| c.sys_sha == sys_sha && c.users[..] == users[..users.len() - 1]).map(|i| p.remove(i))
    };
    let (mut proc, prompt) = match reused {
        Some(p) => (p, content(&w[w.len() - 1].1, &w[w.len() - 1].2)),
        None => {
            let p = spawn(cfg, &system, max_tokens).await?;
            let last = &w[w.len() - 1];
            let prompt = if w.len() == 1 {
                content(&last.1, &last.2)
            } else {
                // earlier images are not replayed (a note stands in); the last turn's images are sent for real
                let mut t = String::from("The conversation so far (continue it as the assistant; your next reply answers the last user turn):\n");
                for (i, (u, s, im)) in w.iter().enumerate() {
                    let note = if i + 1 < w.len() && !im.is_empty() { format!("\n[{} image(s) omitted from this replay]", im.len()) } else { String::new() };
                    t += &format!("\n[{}]\n{s}{note}\n", if *u { "user" } else { "assistant" });
                }
                content(&t, &last.2)
            };
            (p, prompt)
        }
    };

    let line = serde_json::to_string(&json!({"type": "user", "message": {"role": "user", "content": prompt}}))?;
    proc.stdin.write_all(line.as_bytes()).await?;
    proc.stdin.write_all(b"\n").await?;
    proc.stdin.flush().await?;

    let result = loop {
        let l = match tokio::time::timeout(Duration::from_secs(900), proc.out.next_line()).await {
            Err(_) => bail!("claude-sdk: no reply within 15 minutes"),
            Ok(r) => r?,
        };
        let Some(l) = l else { bail!("claude-sdk: the claude process exited (not logged in? run `claude` once to log in; or the plan's usage limit was reached)") };
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        match v["type"].as_str() {
            Some("rate_limit_event") if v["rate_limit_info"]["status"].as_str().is_some_and(|s| s == "rejected" || s == "blocked") => {
                bail!("claude-sdk: plan usage limit reached ({})", v["rate_limit_info"]);
            }
            Some("result") => break v,
            _ => {}
        }
    };
    if result["is_error"].as_bool().unwrap_or(false) || result["subtype"].as_str() != Some("success") {
        bail!("claude-sdk: {}", result["result"].as_str().unwrap_or("the claude CLI reported an error"));
    }

    proc.users = users;
    let total = result["total_cost_usd"].as_f64().unwrap_or(0.0);
    let u = &result["usage"];
    let mut out = Reply {
        input: u["input_tokens"].as_u64().unwrap_or(0),
        cache_read: u["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write: u["cache_creation_input_tokens"].as_u64().unwrap_or(0),
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cost: (total - proc.last_cost).max(0.0),
        ..Default::default()
    };
    proc.last_cost = total;
    {
        let mut p = pool.lock().await;
        p.push(proc);
        if p.len() > POOL_MAX {
            p.remove(0);
        }
    }

    let text = result["result"].as_str().unwrap_or("").to_string();
    let cut = result["stop_reason"].as_str() == Some("max_tokens");
    out.finish = Some(if cut { "length".into() } else { "stop".into() });
    // the earliest opening tag of a known tool starts the call
    let found = tt.iter().filter_map(|t| text.find(&format!("<{}>", t.name)).map(|i| (i, t))).min_by_key(|(i, _)| *i);
    match found {
        None => match (invoke(&text, &tt), text.find("<invoke"), tt.first()) {
            // the model's habitual `<invoke name="tool"><parameter name="arg">..` form
            (Some((i, t, arg)), _, _) => {
                out.text = text[..i].trim_end().to_string();
                out.calls.push(json!({"id": format!("call_{}", CALL_ID.fetch_add(1, Ordering::Relaxed)), "name": t.name, "input": {t.param.clone(): unfence(&arg)}}));
            }
            // an unparseable attempt is a failed call (the loop says so and asks again), not a final answer
            (None, Some(i), Some(t)) => {
                out.text = text[..i].trim_end().to_string();
                out.calls.push(json!({"id": format!("call_{}", CALL_ID.fetch_add(1, Ordering::Relaxed)), "name": t.name, "input": null, "error": format!("malformed tool call; write <{n}>...</{n}> exactly", n = t.name)}));
            }
            _ => out.text = text,
        },
        Some((i, t)) => {
            out.text = text[..i].trim_end().to_string();
            let open = format!("<{}>", t.name);
            let body = &text[i + open.len()..];
            let id = format!("call_{}", CALL_ID.fetch_add(1, Ordering::Relaxed));
            match body.find(&format!("</{}>", t.name)) {
                Some(j) => out.calls.push(json!({"id": id, "name": t.name, "input": {t.param.clone(): unfence(&body[..j])}})),
                None => {
                    out.finish = Some("length".into());
                    out.calls.push(json!({"id": id, "name": t.name, "input": null, "error": "the tool call was cut off before its closing tag"}));
                }
            }
        }
    }
    out.raw = String::new();
    Ok(out)
}

/// The argument verbatim, minus the surrounding newlines and a code fence the model added anyway.
fn unfence(s: &str) -> String {
    let mut t = s.trim_matches('\n');
    // a `<parameter name="x">..</parameter>` wrapper inside the tool tag
    if let Some(r) = t.trim().strip_prefix("<parameter") && let Some(gt) = r.find('>') && let Some(inner) = r[gt + 1..].trim_end().strip_suffix("</parameter>") {
        t = inner.trim_matches('\n');
    }
    if let Some(rest) = t.trim().strip_prefix("```") {
        if let Some(nl) = rest.find('\n') {
            let body = &rest[nl + 1..];
            if let Some(b) = body.trim_end().strip_suffix("```") {
                return b.trim_end_matches('\n').to_string();
            }
        }
    }
    t.to_string()
}

/// `<invoke name="TOOL"> <parameter name="PARAM">VALUE</parameter> </invoke>` -> (start, tool, VALUE).
fn invoke<'a>(text: &str, tt: &'a [TextTool]) -> Option<(usize, &'a TextTool, String)> {
    let start = text.find("<invoke")?;
    let t = tt.iter().find(|t| text[start..].starts_with(&format!("<invoke name=\"{}\"", t.name)))?;
    let open = format!("<parameter name=\"{}\">", t.param);
    let a = text[start..].find(&open)? + start + open.len();
    let b = text[a..].find("</parameter>")? + a;
    Some((start, t, text[a..b].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tt() -> Vec<TextTool> {
        vec![TextTool { name: "luau".into(), param: "code".into() }]
    }

    #[test]
    fn unfence_strips_fences_and_parameter_wrapper() {
        assert_eq!(unfence("\nprint(1)\n"), "print(1)");
        assert_eq!(unfence("```luau\nprint(1)\n```"), "print(1)");
        assert_eq!(unfence("<parameter name=\"code\">\nprint(1)\n</parameter>"), "print(1)");
        assert_eq!(unfence("  indented()\n"), "  indented()");
    }

    #[test]
    fn invoke_form_is_read() {
        let t = tt();
        let (i, tool, arg) = invoke("go\n<invoke name=\"luau\"><parameter name=\"code\">x()</parameter></invoke>", &t).unwrap();
        assert_eq!((i, tool.name.as_str(), arg.as_str()), (3, "luau", "x()"));
        assert!(invoke("<invoke\n{\"cmd\":\"true\"}\n</invoke>", &t).is_none());
    }

    #[test]
    fn wire_merges_tool_results_and_names_them() {
        let msgs = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "doing", "calls": [{"id": "c1", "name": "luau", "input": {"code": "f()"}}]}),
            json!({"role": "tool", "id": "c1", "content": "ok"}),
            json!({"role": "user", "content": "nudge"}),
        ];
        let w = wire(&msgs, &tt());
        assert_eq!(w.len(), 3);
        assert_eq!(w[1].1, "doing\n<luau>\nf()\n</luau>");
        assert_eq!(w[2].1, "[luau result]\nok\n\nnudge");
    }

    #[test]
    fn images_become_base64_blocks_and_change_the_turn_key() {
        let img = json!({"mime": "image/jpeg", "data": "QUJD"});
        let msgs = vec![json!({"role": "user", "content": "look", "images": [img]})];
        let w = wire(&msgs, &tt());
        assert_eq!(w[0].2.len(), 1);
        let c = content(&w[0].1, &w[0].2);
        assert_eq!(c[0], json!({"type": "text", "text": "look"}));
        assert_eq!(c[1], json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}}));
        assert_eq!(content("hi", &[]), json!("hi"));
        let other = wire(&[json!({"role": "user", "content": "look", "images": [{"mime": "image/jpeg", "data": "WFla"}]})], &tt());
        assert_ne!(turn_key(&w[0]), turn_key(&other[0]));
    }

    /// Live: needs a logged-in `claude` CLI. `cargo test live_image -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_image_reaches_the_model() {
        // 16x16 solid red PNG
        let png = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAF0lEQVR4nGP4z8BAEiJN9aiGUQ1DSgMAkPn/Afnh+ngAAAAASUVORK5CYII=";
        let cfg = LlmCfg { model: "haiku".into(), api: "claude-sdk".into(), base_url: None, key_env: None, reasoning: None, max_output: 200, context_window: None, cache_key: None, provider_only: vec![] };
        let m1 = json!({"role": "user", "content": "What single color fills the attached image? Answer with one word.", "images": [{"mime": "image/png", "data": png}]});
        let r = chat(&cfg, &json!({"system": "Answer briefly.", "messages": [m1.clone()]})).await.unwrap();
        println!("turn 1: {}", r.text);
        assert!(r.text.to_lowercase().contains("red"), "{}", r.text);
        // second turn reuses the child (same earlier turn key) and a fresh child must replay with the image still described
        let m2 = json!({"role": "assistant", "content": r.text});
        let m3 = json!({"role": "user", "content": "And what was the image's size in pixels, if you can tell? One short sentence."});
        let r2 = chat(&cfg, &json!({"system": "Answer briefly.", "messages": [m1, m2, m3]})).await.unwrap();
        println!("turn 2: {}", r2.text);
        assert!(!r2.text.is_empty());
    }
}
