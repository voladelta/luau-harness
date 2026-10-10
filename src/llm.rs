//! Provider client, three wire formats, all with native tool calling: Anthropic Messages (`anthropic`), OpenAI-compatible Chat Completions (`openai`) and
//! the Responses API (`responses`). Any model on any compatible endpoint (OpenRouter, Enclave, local) works: nothing here
//! restricts the model or the upstream provider.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Clone)]
pub struct LlmCfg {
    pub model: String,
    /// "anthropic" | "openai" | "responses"
    pub api: String,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    /// OpenRouter `reasoning.effort`
    pub reasoning: Option<String>,
    /// default per-call output limit (a request can override it); a runaway reply costs at most this many tokens
    pub max_output: u64,
    /// OpenAI-style `prompt_cache_key` (the run's session id, so every turn and subagent routes to one cache)
    pub cache_key: Option<String>,
    /// OpenRouter only: restrict routing to these upstream providers, no fallbacks (keeps a prompt cache on one provider)
    pub provider_only: Vec<String>,
}

#[derive(Default, Debug, Clone)]
pub struct Reply {
    pub text: String,
    /// tool calls the model made: [{id, name, input, error?}]; `input` is the parsed arguments object, `error` is set when they
    /// were not valid JSON (e.g. the reply was cut off mid-call)
    pub calls: Vec<Value>,
    /// the provider-native assistant message (JSON text), echoed back verbatim next turn so reasoning/thinking blocks survive
    pub raw: String,
    pub finish: Option<String>,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub cost: f64,
}

impl LlmCfg {
    /// Chat Completions or Responses: both default to OpenRouter and use bearer auth.
    fn openai_style(&self) -> bool {
        matches!(self.api.as_str(), "openai" | "responses")
    }

    pub fn key(&self) -> Result<String> {
        let env = self.key_env.clone().unwrap_or_else(|| if self.openai_style() { "OPENROUTER_API_KEY" } else { "ANTHROPIC_API_KEY" }.into());
        std::env::var(&env).map_err(|_| anyhow::anyhow!("{env} not set"))
    }
}

fn add_cache_key(b: &mut Value, cfg: &LlmCfg) {
    if let Some(k) = &cfg.cache_key {
        b["prompt_cache_key"] = json!(k);
    }
}

fn parse_args(s: &str) -> (Value, Option<String>) {
    if s.trim().is_empty() {
        return (json!({}), None);
    }
    match serde_json::from_str::<Value>(s) {
        Ok(v) if v.is_object() => (v, None),
        Ok(_) => (Value::Null, Some("arguments are not a JSON object".into())),
        Err(e) => (Value::Null, Some(format!("arguments are not valid JSON ({e})"))),
    }
}

fn call(id: &str, name: &str, args: &str) -> Value {
    let (input, error) = parse_args(args);
    let mut c = json!({"id": id, "name": name, "input": input});
    if let Some(e) = error {
        c["error"] = json!(e);
    }
    c
}

fn cache_marker() -> Value {
    json!({"type": "ephemeral"})
}

/// Neutral messages -> Chat Completions messages. Neutral forms: {role:user, content}, {role:assistant, content, calls, raw},
/// {role:tool, id, content}. `raw` (the provider's own message) wins over rebuilding from `content`/`calls`.
fn chat_messages(system: &Value, msgs: &[Value], cache: bool) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": [system]})];
    for m in msgs {
        match m["role"].as_str().unwrap_or("user") {
            "assistant" => {
                if let Some(raw) = m["raw"].as_str().and_then(|r| serde_json::from_str::<Value>(r).ok()) {
                    out.push(raw);
                    continue;
                }
                let mut a = json!({"role": "assistant", "content": m["content"].as_str().unwrap_or("")});
                let calls: Vec<Value> = m["calls"].as_array().into_iter().flatten().map(|c| json!({"id": c["id"], "type": "function", "function": {"name": c["name"], "arguments": c["input"].to_string()}})).collect();
                if !calls.is_empty() {
                    a["tool_calls"] = json!(calls);
                }
                out.push(a);
            }
            "tool" => out.push(json!({"role": "tool", "tool_call_id": m["id"], "content": m["content"].as_str().unwrap_or("")})),
            r => out.push(json!({"role": r, "content": m["content"].as_str().unwrap_or("")})),
        }
    }
    if cache {
        // one moving breakpoint on the conversation prefix, plus the system prompt
        if let Some(last) = out.last_mut() {
            let text = last["content"].as_str().unwrap_or_default().to_string();
            last["content"] = json!([{"type": "text", "text": text, "cache_control": cache_marker()}]);
        }
    }
    out
}

/// Neutral messages -> Anthropic Messages. Tool results become `tool_result` blocks of one user message; a user text that
/// follows them joins that message (tool_result blocks must come first).
fn anthropic_messages(msgs: &[Value], cache: bool) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut pending: Vec<Value> = Vec::new(); // content blocks of the user message being assembled
    fn flush(out: &mut Vec<Value>, pending: &mut Vec<Value>) {
        if !pending.is_empty() {
            out.push(json!({"role": "user", "content": std::mem::take(pending)}));
        }
    }
    for m in msgs {
        match m["role"].as_str().unwrap_or("user") {
            "assistant" => {
                flush(&mut out, &mut pending);
                if let Some(raw) = m["raw"].as_str().and_then(|r| serde_json::from_str::<Value>(r).ok()) {
                    out.push(json!({"role": "assistant", "content": raw}));
                    continue;
                }
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(t) = m["content"].as_str().filter(|t| !t.is_empty()) {
                    blocks.push(json!({"type": "text", "text": t}));
                }
                for c in m["calls"].as_array().into_iter().flatten() {
                    blocks.push(json!({"type": "tool_use", "id": c["id"], "name": c["name"], "input": c["input"]}));
                }
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            "tool" => pending.push(json!({"type": "tool_result", "tool_use_id": m["id"], "content": m["content"].as_str().unwrap_or("")})),
            _ => {
                pending.push(json!({"type": "text", "text": m["content"].as_str().unwrap_or("")}));
                flush(&mut out, &mut pending);
            }
        }
    }
    flush(&mut out, &mut pending);
    if cache {
        if let Some(b) = out.last_mut().and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut()) {
            b["cache_control"] = cache_marker();
        }
    }
    out
}

/// Neutral messages -> Responses API input items (the model's own output items are spliced back in verbatim).
fn responses_input(msgs: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in msgs {
        match m["role"].as_str().unwrap_or("user") {
            "assistant" => {
                if let Some(Value::Array(items)) = m["raw"].as_str().and_then(|r| serde_json::from_str::<Value>(r).ok()) {
                    out.extend(items);
                    continue;
                }
                if let Some(t) = m["content"].as_str().filter(|t| !t.is_empty()) {
                    out.push(json!({"role": "assistant", "content": t}));
                }
                for c in m["calls"].as_array().into_iter().flatten() {
                    out.push(json!({"type": "function_call", "call_id": c["id"], "name": c["name"], "arguments": c["input"].to_string()}));
                }
            }
            "tool" => out.push(json!({"type": "function_call_output", "call_id": m["id"], "output": m["content"].as_str().unwrap_or("")})),
            r => out.push(json!({"role": r, "content": m["content"].as_str().unwrap_or("")})),
        }
    }
    out
}

/// `req`: { system?: string, messages: [neutral message], tools?: [{name, description, parameters}], max_tokens?: number }
pub async fn chat(http: &reqwest::Client, cfg: &LlmCfg, key: &str, req: &Value) -> Result<Reply> {
    if !matches!(cfg.api.as_str(), "anthropic" | "openai" | "responses") {
        bail!("unknown --api {:?} (expected anthropic, openai or responses)", cfg.api);
    }
    let openai = cfg.openai_style();
    let responses = cfg.api == "responses";
    let system = req["system"].as_str().unwrap_or("");
    // Prompt caching is always on, never optional. Anthropic Messages and Chat Completions get `cache_control` markers
    // (OpenRouter honours them for Anthropic/Gemini and ignores them elsewhere, where caching is automatic); the Responses
    // API has no markers and caches automatically. OpenAI-style requests also carry `prompt_cache_key`.
    let cache = !responses;
    let max_tokens = req["max_tokens"].as_u64().unwrap_or(cfg.max_output);
    let msgs: Vec<Value> = req["messages"].as_array().cloned().unwrap_or_default();
    let tools: Vec<Value> = req["tools"].as_array().cloned().unwrap_or_default();
    let sys_block = if cache { json!({"type": "text", "text": system, "cache_control": cache_marker()}) } else { json!({"type": "text", "text": system}) };
    let body = if responses {
        let mut b = json!({"model": cfg.model, "max_output_tokens": max_tokens, "instructions": system, "input": responses_input(&msgs)});
        if !tools.is_empty() {
            b["tools"] = json!(tools.iter().map(|t| json!({"type": "function", "name": t["name"], "description": t["description"], "parameters": t["parameters"]})).collect::<Vec<_>>());
        }
        if let Some(e) = &cfg.reasoning {
            b["reasoning"] = json!({"effort": e});
        }
        add_cache_key(&mut b, cfg);
        b
    } else if openai {
        let mut b = json!({"model": cfg.model, "max_tokens": max_tokens, "messages": chat_messages(&sys_block, &msgs, cache), "usage": {"include": true}});
        if !tools.is_empty() {
            b["tools"] = json!(tools.iter().map(|t| json!({"type": "function", "function": {"name": t["name"], "description": t["description"], "parameters": t["parameters"]}})).collect::<Vec<_>>());
        }
        if let Some(e) = &cfg.reasoning {
            b["reasoning"] = json!({"effort": e});
        }
        if !cfg.provider_only.is_empty() {
            b["provider"] = json!({"only": cfg.provider_only, "allow_fallbacks": false});
        }
        add_cache_key(&mut b, cfg);
        b
    } else {
        let mut b = json!({"model": cfg.model, "max_tokens": max_tokens, "system": [sys_block], "messages": anthropic_messages(&msgs, cache)});
        if !tools.is_empty() {
            b["tools"] = json!(tools.iter().map(|t| json!({"name": t["name"], "description": t["description"], "input_schema": t["parameters"]})).collect::<Vec<_>>());
        }
        b
    };
    let default = if openai { "https://openrouter.ai/api/v1" } else { "https://api.anthropic.com" };
    let base = cfg.base_url.clone().or_else(|| std::env::var("ANTHROPIC_BASE_URL").ok().filter(|_| !openai)).unwrap_or(default.into());
    let url = if responses { format!("{base}/responses") } else if openai { format!("{base}/chat/completions") } else { format!("{base}/v1/messages") };
    let mut resp: Option<Value> = None;
    for attempt in 0..6u32 {
        let r = http.post(&url).json(&body);
        let r = if openai { r.bearer_auth(key) } else { r.header("x-api-key", key).header("anthropic-version", "2023-06-01") };
        match r.send().await {
            Ok(r) if r.status().is_success() => {
                resp = Some(r.json().await?);
                break;
            }
            Ok(r) if matches!(r.status().as_u16(), 429 | 500 | 502 | 503 | 529) => {}
            Ok(r) => bail!("API {}: {}", r.status(), r.text().await.unwrap_or_default()),
            Err(e) if e.is_timeout() || e.is_connect() => {}
            Err(e) => bail!("API request failed: {e}"),
        }
        if attempt < 5 {
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
    }
    let Some(resp) = resp else { bail!("API unavailable after retries") };

    let u = &resp["usage"];
    let mut out = Reply::default();
    if responses {
        let d = &u["input_tokens_details"];
        out.cache_read = d["cached_tokens"].as_u64().unwrap_or(0);
        out.cache_write = d["cache_write_tokens"].as_u64().unwrap_or(0);
        out.input = u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(out.cache_read + out.cache_write);
        out.output = u["output_tokens"].as_u64().unwrap_or(0);
        out.cost = u["cost"].as_f64().unwrap_or(0.0);
        let items: Vec<Value> = resp["output"].as_array().cloned().unwrap_or_default();
        out.text = items.iter().filter(|o| o["type"] == "message").flat_map(|o| o["content"].as_array().into_iter().flatten()).filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
        out.calls = items.iter().filter(|o| o["type"] == "function_call").map(|o| call(o["call_id"].as_str().unwrap_or(""), o["name"].as_str().unwrap_or(""), o["arguments"].as_str().unwrap_or(""))).collect();
        out.raw = serde_json::to_string(&items)?;
        let cut = resp["incomplete_details"]["reason"].as_str() == Some("max_output_tokens");
        out.finish = Some(if cut { "length".into() } else { "stop".to_string() });
    } else if openai {
        let d = &u["prompt_tokens_details"];
        out.cache_read = d["cached_tokens"].as_u64().unwrap_or(0);
        out.cache_write = d["cache_write_tokens"].as_u64().unwrap_or(0);
        out.input = u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(out.cache_read + out.cache_write);
        out.output = u["completion_tokens"].as_u64().unwrap_or(0);
        out.cost = u["cost"].as_f64().unwrap_or(0.0);
        let msg = &resp["choices"][0]["message"];
        out.text = msg["content"].as_str().unwrap_or_default().to_string();
        out.calls = msg["tool_calls"].as_array().into_iter().flatten().map(|c| call(c["id"].as_str().unwrap_or(""), c["function"]["name"].as_str().unwrap_or(""), c["function"]["arguments"].as_str().unwrap_or(""))).collect();
        // echo back only the fields the API defines for an assistant message (plus the reasoning fields providers return)
        let mut keep = serde_json::Map::new();
        for k in ["role", "content", "tool_calls", "reasoning", "reasoning_content", "reasoning_details"] {
            if !msg[k].is_null() {
                keep.insert(k.into(), msg[k].clone());
            }
        }
        out.raw = serde_json::to_string(&Value::Object(keep))?;
        out.finish = resp["choices"][0]["finish_reason"].as_str().map(String::from);
    } else {
        out.input = u["input_tokens"].as_u64().unwrap_or(0);
        out.output = u["output_tokens"].as_u64().unwrap_or(0);
        out.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        out.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let blocks: Vec<Value> = resp["content"].as_array().cloned().unwrap_or_default();
        out.text = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
        out.calls = blocks.iter().filter(|b| b["type"] == "tool_use").map(|b| {
            let mut c = json!({"id": b["id"], "name": b["name"], "input": b["input"]});
            if !b["input"].is_object() {
                c["error"] = json!("arguments are not a JSON object");
            }
            c
        }).collect();
        out.raw = serde_json::to_string(&blocks)?;
        out.finish = resp["stop_reason"].as_str().map(|s| if s == "max_tokens" { "length".into() } else { s.to_string() });
    }
    Ok(out)
}
