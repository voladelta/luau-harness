//! `--api claude-sdk`: use Claude through the `claude` CLI (the way the Claude Agent SDK does), so the calls draw on the
//! user's Claude plan instead of an API key. One long-lived `claude -p --input-format stream-json --output-format
//! stream-json` child holds the conversation; hx sends only the new user turn each time. hx's tools reach the model as
//! native tools of an in-process "SDK" MCP server (`hx`): the CLI asks hx for `tools/list` and `tools/call` over the
//! control channel. A `tools/call` is not answered at once: `chat` returns it as the reply's tool call, parks the child with
//! the call outstanding, and the next `chat` (which carries the tool results) answers it and lets the model go on.
use crate::llm::{LlmCfg, Reply};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

/// With partial messages on, a working model streams deltas (thinking included) every few seconds; this much silence
/// means the call is stuck, and `chat` retries it once on a fresh child.
const IDLE: Duration = Duration::from_secs(180);
const STALL: &str = "claude-sdk: no output for";

/// idle children kept for reuse; the oldest is dropped (and killed) beyond this
const POOL_MAX: usize = 8;
static POOL: OnceLock<Mutex<Vec<Proc>>> = OnceLock::new();

struct Proc {
    _child: Child,
    stdin: ChildStdin,
    out: tokio::io::Lines<BufReader<ChildStdout>>,
    /// what this child was started with; a request with a different system prompt, model or tool set needs another child
    sys_sha: String,
    /// keys of the user / tool-result messages this child has been given, in order (its own replies sit between them)
    seen: Vec<String>,
    /// tool calls the model made that hx has not answered yet
    pending: Vec<Pending>,
    /// usage (input, cache_read, cache_write, output) of every message this child has streamed, by message id: kept for the
    /// child's life because a message's final output count (its message_delta) can arrive after hx stopped reading for
    /// the turn that message belongs to
    msgs: HashMap<String, [u64; 4]>,
    /// the message being streamed (from its message_start), so a late message_delta is credited to it
    cur_msg: String,
    /// the sum over `msgs` already reported; each turn reports the growth since
    reported: [u64; 4],
    /// total_cost_usd is cumulative per session; the last value seen, to report each turn's share
    last_cost: f64,
}

/// A `tools/call` the CLI is waiting on.
struct Pending {
    request_id: String,
    rpc_id: Value,
    tool_use_id: String,
}

const SERVER: &str = "hx";

/// The tools as an MCP `tools/list` result.
fn mcp_tools(tools: &[Value]) -> Value {
    json!(tools.iter().map(|t| json!({"name": t["name"], "description": t["description"], "inputSchema": t["parameters"]})).collect::<Vec<_>>())
}

/// One side of the wire conversation: its text and the images (`{mime, data}`) attached to it.
type Turn = (bool, String, Vec<Value>);

/// Neutral messages -> alternating (user, assistant) wire turns, as text. Tool results and user messages between two
/// assistant turns merge into one user turn.
fn wire(msgs: &[Value]) -> Vec<Turn> {
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
                    // only a transcript replay (a fresh child) renders calls as text; a live child holds its own
                    let arg = c["input"].as_object().and_then(|o| o.values().find_map(|v| v.as_str())).unwrap_or("");
                    t += &format!("\n[tool call {name}]\n{arg}");
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

/// One key per non-assistant message: its text and images, or the id of the tool call it answers.
fn msg_keys(msgs: &[Value]) -> Vec<String> {
    msgs.iter()
        .filter(|m| m["role"].as_str() != Some("assistant"))
        .map(|m| match m["role"].as_str() {
            Some("tool") => format!("tool:{}", m["id"].as_str().unwrap_or("")),
            _ => format!("u:{}", turn_key(&(true, m["content"].as_str().unwrap_or("").to_string(), m["images"].as_array().cloned().unwrap_or_default()))),
        })
        .collect()
}

fn sha(s: &str) -> String {
    crate::eventlog::sha_bytes(s.as_bytes())
}

async fn spawn(cfg: &LlmCfg, system: &str, max_tokens: u64, tools: &[Value], sys_sha: String) -> Result<Proc> {
    let dir = std::env::temp_dir().join("hx-claude-sdk");
    std::fs::create_dir_all(&dir)?;
    let mut cmd = Command::new("claude");
    cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages", "--tools", "", "--setting-sources", "", "--strict-mcp-config", "--no-session-persistence", "--model", &cfg.model, "--system-prompt", system]);
    if !tools.is_empty() {
        let allowed: Vec<String> = tools.iter().filter_map(|t| t["name"].as_str()).map(|n| format!("mcp__{SERVER}__{n}")).collect();
        cmd.args(["--allowedTools", &allowed.join(","), "--mcp-config", &json!({"mcpServers": {SERVER: {"type": "sdk", "name": SERVER}}}).to_string()]);
    }
    if let Some(e) = &cfg.reasoning {
        cmd.args(["--effort", e]);
    }
    // never bill an API key by accident: this backend exists to use the plan's login
    cmd.env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN").env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", max_tokens.to_string());
    cmd.current_dir(dir).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("could not start `claude` (is Claude Code installed and logged in?): {e}"))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow::anyhow!("claude: no stdin"))?;
    let out = BufReader::new(child.stdout.take().ok_or_else(|| anyhow::anyhow!("claude: no stdout"))?).lines();
    let mut p = Proc { _child: child, stdin, out, sys_sha, seen: Vec::new(), pending: Vec::new(), msgs: HashMap::new(), cur_msg: String::new(), reported: [0; 4], last_cost: 0.0 };
    if !tools.is_empty() {
        send(&mut p, &json!({"type": "control_request", "request_id": "hx-init", "request": {"subtype": "initialize", "sdkMcpServers": [SERVER]}})).await?;
    }
    Ok(p)
}

async fn send(p: &mut Proc, v: &Value) -> Result<()> {
    p.stdin.write_all(serde_json::to_string(v)?.as_bytes()).await?;
    p.stdin.write_all(b"\n").await?;
    p.stdin.flush().await?;
    Ok(())
}

/// Answer an MCP request the CLI sent over the control channel.
async fn mcp_reply(p: &mut Proc, request_id: &str, rpc_id: Option<&Value>, result: Value) -> Result<()> {
    let mut msg = json!({"jsonrpc": "2.0", "result": result});
    if let Some(id) = rpc_id {
        msg["id"] = id.clone();
    }
    send(p, &json!({"type": "control_response", "response": {"subtype": "success", "request_id": request_id, "response": {"mcp_response": msg}}})).await
}

/// What the model did since the last message hx sent.
#[derive(Default)]
struct Segment {
    text: String,
    calls: Vec<Value>,
    pending: Vec<Pending>,
    /// the `result` event, when the model finished instead of calling tools
    result: Option<Value>,
}

/// Read the child's output until the model either finishes (`result`) or has made every tool call of one assistant message.
async fn drive(p: &mut Proc, tools: &Value) -> Result<Segment> {
    let mut seg = Segment::default();
    let mut expected = 0usize;
    loop {
        let l = match tokio::time::timeout(IDLE, p.out.next_line()).await {
            Err(_) => bail!("{STALL} {} s", IDLE.as_secs()),
            Ok(r) => r?,
        };
        let Some(l) = l else { bail!("claude-sdk: the claude process exited (not logged in? run `claude` once to log in; or the plan's usage limit was reached)") };
        if let Some(path) = std::env::var_os("HX_SDK_TRACE") {
            // debugging aid: every line the claude child prints, so a slow call shows what it is doing
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{} {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0), l);
            }
        }
        let Ok(v) = serde_json::from_str::<Value>(&l) else { continue };
        match v["type"].as_str() {
            Some("rate_limit_event") if v["rate_limit_info"]["status"].as_str().is_some_and(|s| s == "rejected" || s == "blocked") => {
                bail!("claude-sdk: plan usage limit reached ({})", v["rate_limit_info"]);
            }
            Some("assistant") => {
                let m = &v["message"];
                let u = &m["usage"];
                {
                    let id = m["id"].as_str().unwrap_or("").to_string();
                    let e = p.msgs.entry(id).or_insert([0; 4]);
                    let new = [u["input_tokens"].as_u64().unwrap_or(0), u["cache_read_input_tokens"].as_u64().unwrap_or(0), u["cache_creation_input_tokens"].as_u64().unwrap_or(0), u["output_tokens"].as_u64().unwrap_or(0)];
                    for (x, n) in e.iter_mut().zip(new) {
                        *x = (*x).max(n);
                    }
                }
                for b in m["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("text") => {
                            if !seg.text.is_empty() {
                                seg.text.push('\n');
                            }
                            seg.text.push_str(b["text"].as_str().unwrap_or(""));
                        }
                        Some("tool_use") => {
                            // only hx's own tools reach hx (as tools/call); the CLI itself rejects any other name (e.g. a
                            // bare `luau` copied from a transcript replay), so waiting for it would hang this read
                            let Some(name) = b["name"].as_str().and_then(|n| n.strip_prefix(&format!("mcp__{SERVER}__"))) else { continue };
                            expected += 1;
                            seg.calls.push(json!({"id": b["id"], "name": name, "input": b["input"]}));
                        }
                        _ => {}
                    }
                }
            }
            Some("control_request") if v["request"]["subtype"] == "mcp_message" => {
                let rid = v["request_id"].as_str().unwrap_or("").to_string();
                let msg = &v["request"]["message"];
                let rpc = msg.get("id");
                match msg["method"].as_str().unwrap_or("") {
                    "initialize" => mcp_reply(p, &rid, rpc, json!({"protocolVersion": msg["params"]["protocolVersion"], "capabilities": {"tools": {}}, "serverInfo": {"name": SERVER, "version": "1"}})).await?,
                    "tools/list" => mcp_reply(p, &rid, rpc, json!({"tools": tools})).await?,
                    "tools/call" => seg.pending.push(Pending { request_id: rid, rpc_id: rpc.cloned().unwrap_or(Value::Null), tool_use_id: msg["params"]["_meta"]["claudecode/toolUseId"].as_str().unwrap_or("").to_string() }),
                    _ => mcp_reply(p, &rid, rpc, json!({})).await?,
                }
            }
            Some("stream_event") => {
                // the streamed `assistant` events carry the usage of a message still being written (output far too low);
                // message_start names the message, and its closing message_delta has the final output count
                let ev = &v["event"];
                match ev["type"].as_str() {
                    Some("message_start") => {
                        let m = &ev["message"];
                        p.cur_msg = m["id"].as_str().unwrap_or("").to_string();
                        let u = &m["usage"];
                        let e = p.msgs.entry(p.cur_msg.clone()).or_insert([0; 4]);
                        let new = [u["input_tokens"].as_u64().unwrap_or(0), u["cache_read_input_tokens"].as_u64().unwrap_or(0), u["cache_creation_input_tokens"].as_u64().unwrap_or(0), u["output_tokens"].as_u64().unwrap_or(0)];
                        for (x, n) in e.iter_mut().zip(new) {
                            *x = (*x).max(n);
                        }
                    }
                    Some("message_delta") => {
                        if let Some(o) = ev["usage"]["output_tokens"].as_u64() {
                            let e = p.msgs.entry(p.cur_msg.clone()).or_insert([0; 4]);
                            e[3] = e[3].max(o);
                        }
                    }
                    _ => {}
                }
            }
            Some("result") => {
                seg.result = Some(v);
                break;
            }
            _ => {}
        }
        if expected > 0 && seg.pending.len() >= expected {
            break;
        }
    }
    Ok(seg)
}

/// A new child given the whole conversation: as is when it is one user turn, else as a transcript replay.
async fn fresh(cfg: &LlmCfg, system: &str, max_tokens: u64, tools: &[Value], sys_sha: String, msgs: &[Value]) -> Result<Proc> {
    let mut p = spawn(cfg, system, max_tokens, tools, sys_sha).await?;
    let w = wire(msgs);
    let last = &w[w.len() - 1];
    let prompt = if w.len() == 1 {
        content(&last.1, &last.2)
    } else {
        // earlier images are not replayed (a note stands in); the last turn's images are sent for real
        let mut t = String::from("The conversation so far (continue it as the assistant; your next reply answers the last user turn). The [tool call] lines are a record of earlier calls; make new calls with your tool, not as text:\n");
        for (i, (u, s, im)) in w.iter().enumerate() {
            let note = if i + 1 < w.len() && !im.is_empty() { format!("\n[{} image(s) omitted from this replay]", im.len()) } else { String::new() };
            t += &format!("\n[{}]\n{s}{note}\n", if *u { "user" } else { "assistant" });
        }
        content(&t, &last.2)
    };
    send(&mut p, &json!({"type": "user", "message": {"role": "user", "content": prompt}})).await?;
    Ok(p)
}

/// `req` is the same shape `llm::chat` takes.
pub async fn chat(cfg: &LlmCfg, req: &Value) -> Result<Reply> {
    let tools: Vec<Value> = req["tools"].as_array().cloned().unwrap_or_default();
    let tools_list = mcp_tools(&tools);
    let system = req["system"].as_str().unwrap_or("");
    let max_tokens = req["max_tokens"].as_u64().unwrap_or(cfg.max_output).min(cfg.max_output);
    let msgs: Vec<Value> = req["messages"].as_array().cloned().unwrap_or_default();
    let keys = msg_keys(&msgs);
    if keys.is_empty() {
        bail!("claude-sdk: the request has no user message");
    }
    if msgs.last().is_some_and(|m| m["role"].as_str() == Some("assistant")) {
        bail!("claude-sdk: the request must end with a user message");
    }
    // the messages after the last assistant message: new input for a child that has seen everything before them
    let trailing: Vec<Value> = msgs.iter().rev().take_while(|m| m["role"].as_str() != Some("assistant")).cloned().collect::<Vec<_>>().into_iter().rev().collect();
    let sys_sha = sha(&format!("{}\n{}\n{:?}\n{}", cfg.model, system, cfg.reasoning, tools_list));

    let pool = POOL.get_or_init(|| Mutex::new(Vec::new()));
    let reused = {
        let mut p = pool.lock().await;
        p.iter()
            .position(|c| {
                let n = c.seen.len();
                if c.sys_sha != sys_sha || n + trailing.len() != keys.len() || c.seen[..] != keys[..n] {
                    return false;
                }
                let tail = &keys[n..];
                if c.pending.is_empty() {
                    tail.iter().all(|k| k.starts_with("u:"))
                } else {
                    tail.len() == c.pending.len() && c.pending.iter().all(|q| tail.contains(&format!("tool:{}", q.tool_use_id)))
                }
            })
            .map(|i| p.remove(i))
    };
    let mut proc = match reused {
        Some(mut p) => {
            if p.pending.is_empty() {
                let w = wire(&trailing);
                let prompt = content(&w[0].1, &w[0].2);
                send(&mut p, &json!({"type": "user", "message": {"role": "user", "content": prompt}})).await?;
            } else {
                for q in std::mem::take(&mut p.pending) {
                    let out = trailing.iter().find(|m| m["id"].as_str() == Some(&q.tool_use_id)).and_then(|m| m["content"].as_str()).unwrap_or("");
                    mcp_reply(&mut p, &q.request_id, Some(&q.rpc_id), json!({"content": [{"type": "text", "text": out}]})).await?;
                }
            }
            p
        }
        None => fresh(cfg, system, max_tokens, &tools, sys_sha.clone(), &msgs).await?,
    };
    proc.seen = keys.clone();

    // A stalled child may already have generated billable output. Recovery belongs to the supervisor.
    let seg = drive(&mut proc, &tools_list).await?;
    let mut out = Reply { finish: Some("stop".into()), ..Default::default() };
    if let Some(result) = &seg.result {
        if result["is_error"].as_bool().unwrap_or(false) || result["subtype"].as_str() != Some("success") {
            bail!("claude-sdk: {}", result["result"].as_str().unwrap_or("the claude CLI reported an error"));
        }
        // the session's cost is cumulative and only reported with a result, so a turn that ends in tool calls reports none
        let total = result["total_cost_usd"].as_f64().unwrap_or(0.0);
        out.cost = (total - proc.last_cost).max(0.0);
        proc.last_cost = total;
        if result["stop_reason"].as_str() == Some("max_tokens") {
            out.finish = Some("length".into());
        }
        out.text = if seg.calls.is_empty() { result["result"].as_str().unwrap_or("").to_string() } else { seg.text };
    } else {
        out.text = seg.text;
    }
    // this turn's usage: what the child's messages add up to now, less what earlier turns already reported
    let mut total = [0u64; 4];
    for m in proc.msgs.values() {
        for (t, x) in total.iter_mut().zip(m) {
            *t += x;
        }
    }
    let mut usage = [0u64; 4];
    for ((u, t), r) in usage.iter_mut().zip(total).zip(proc.reported) {
        *u = t.saturating_sub(r);
    }
    proc.reported = total;
    (out.input, out.cache_read, out.cache_write, out.output) = (usage[0], usage[1], usage[2], usage[3]);
    out.calls = seg.calls;
    proc.pending = seg.pending;
    {
        let mut p = pool.lock().await;
        p.push(proc);
        if p.len() > POOL_MAX {
            p.remove(0);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_merges_tool_results_and_names_them() {
        let msgs = vec![
            json!({"role": "user", "content": "task"}),
            json!({"role": "assistant", "content": "doing", "calls": [{"id": "c1", "name": "luau", "input": {"code": "f()"}}]}),
            json!({"role": "tool", "id": "c1", "content": "ok"}),
            json!({"role": "user", "content": "nudge"}),
        ];
        let w = wire(&msgs);
        assert_eq!(w.len(), 3);
        assert_eq!(w[1].1, "doing\n[tool call luau]\nf()");
        assert_eq!(w[2].1, "[luau result]\nok\n\nnudge");
    }

    #[test]
    fn images_become_base64_blocks_and_change_the_turn_key() {
        let img = json!({"mime": "image/jpeg", "data": "QUJD"});
        let msgs = vec![json!({"role": "user", "content": "look", "images": [img]})];
        let w = wire(&msgs);
        assert_eq!(w[0].2.len(), 1);
        let c = content(&w[0].1, &w[0].2);
        assert_eq!(c[0], json!({"type": "text", "text": "look"}));
        assert_eq!(c[1], json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}}));
        assert_eq!(content("hi", &[]), json!("hi"));
        let other = wire(&[json!({"role": "user", "content": "look", "images": [{"mime": "image/jpeg", "data": "WFla"}]})]);
        assert_ne!(turn_key(&w[0]), turn_key(&other[0]));
    }

    /// Live: needs a logged-in `claude` CLI. `cargo test live_image -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn live_image_reaches_the_model() {
        // 16x16 solid red PNG
        let png = "iVBORw0KGgoAAAANSUhEUgAAABAAAAAQCAIAAACQkWg2AAAAF0lEQVR4nGP4z8BAEiJN9aiGUQ1DSgMAkPn/Afnh+ngAAAAASUVORK5CYII=";
        let cfg = LlmCfg { model: "haiku".into(), api: "claude-sdk".into(), base_url: None, key_env: None, reasoning: None, max_output: 200, context_window: None, cache_key: None, provider_only: vec![], price: None };
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
