//! Provider client, two wire formats, both with native tool calling: OpenAI-compatible Chat Completions (`completions`) and
//! the Responses API (`responses`). Any model on any compatible endpoint (OpenRouter, Enclave, local) works: nothing here
//! restricts the model or the upstream provider.
use anyhow::{bail, Result};
use serde_json::{json, Value};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LlmCfg {
    pub model: String,
    /// "responses" | "completions"
    pub api: String,
    pub base_url: Option<String>,
    pub key_env: Option<String>,
    /// OpenRouter `reasoning.effort`
    pub reasoning: Option<String>,
    /// default per-call output limit (a request can override it); a runaway reply costs at most this many tokens
    pub max_output: u64,
    /// the model's context window in tokens, when the user says so (`--context-window`); modules read it via `hx.model_info()`
    pub context_window: Option<u64>,
    /// OpenAI-style `prompt_cache_key` (the run's session id, so every turn and subagent routes to one cache)
    pub cache_key: Option<String>,
    /// OpenRouter only: restrict routing to these upstream providers, no fallbacks (keeps a prompt cache on one provider)
    pub provider_only: Vec<String>,
}

/// The provider answered but returned no usable generation. A transport loss is a plain error instead: only then is
/// it unknown whether a billable generation happened.
#[derive(Debug)]
pub struct Answered {
    pub status: u16,
    pub message: String,
    /// seconds from a `Retry-After` header
    pub retry_after: Option<f64>,
}

impl std::fmt::Display for Answered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}

impl std::error::Error for Answered {}

impl Answered {
    /// A 4xx rejects the request and a 503 refuses it before any generation, so nothing was billed. Other 5xx can
    /// follow a generation the upstream billed (a gateway timeout, say).
    pub fn unbilled(&self) -> bool { (400..500).contains(&self.status) || self.status == 503 }
    /// Worth one more attempt: rate limits, server errors and malformed 200 bodies. Other 4xx repeat identically.
    pub fn retryable(&self) -> bool { matches!(self.status, 408 | 429 | 200) || self.status >= 500 }
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}

impl LlmCfg {
    pub fn key(&self) -> Result<String> {
        let env = self.key_env.clone().unwrap_or_else(|| "OPENROUTER_API_KEY".into());
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
            r => match m["images"].as_array().filter(|a| !a.is_empty()) {
                // {role:user, content, images:[{mime, data}]} (the vision pack): text part, then one image_url part per image
                Some(imgs) => {
                    let mut parts = vec![json!({"type": "text", "text": m["content"].as_str().unwrap_or("")})];
                    parts.extend(imgs.iter().map(|i| json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", i["mime"].as_str().unwrap_or("image/png"), i["data"].as_str().unwrap_or(""))}})));
                    out.push(json!({"role": r, "content": parts}));
                }
                None => out.push(json!({"role": r, "content": m["content"].as_str().unwrap_or("")})),
            },
        }
    }
    if cache {
        // one moving breakpoint on the conversation prefix, plus the system prompt
        if let Some(last) = out.last_mut() {
            if let Some(parts) = last["content"].as_array_mut() {
                if let Some(p) = parts.last_mut() {
                    p["cache_control"] = cache_marker();
                }
            } else {
                let text = last["content"].as_str().unwrap_or_default().to_string();
                last["content"] = json!([{"type": "text", "text": text, "cache_control": cache_marker()}]);
            }
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
            r => match m["images"].as_array().filter(|a| !a.is_empty()) {
                Some(imgs) => {
                    let mut parts = vec![json!({"type": "input_text", "text": m["content"].as_str().unwrap_or("")})];
                    parts.extend(imgs.iter().map(|i| json!({"type": "input_image", "image_url": format!("data:{};base64,{}", i["mime"].as_str().unwrap_or("image/png"), i["data"].as_str().unwrap_or(""))})));
                    out.push(json!({"role": r, "content": parts}));
                }
                None => out.push(json!({"role": r, "content": m["content"].as_str().unwrap_or("")})),
            },
        }
    }
    out
}

/// `req`: { system?: string, messages: [neutral message], tools?: [{name, description, parameters}], max_tokens?: number }
pub async fn chat(http: &reqwest::Client, cfg: &LlmCfg, key: &str, req: &Value) -> Result<Reply> {
    if !matches!(cfg.api.as_str(), "responses" | "completions") {
        bail!("unknown --api {:?} (expected responses or completions)", cfg.api);
    }
    let responses = cfg.api == "responses";
    let system = req["system"].as_str().unwrap_or("");
    // Prompt caching is always on, never optional. Chat Completions gets `cache_control` markers
    // (OpenRouter honours them for Anthropic/Gemini and ignores them elsewhere, where caching is automatic); the Responses
    // API has no markers and caches automatically. OpenAI-style requests also carry `prompt_cache_key`.
    let cache = !responses;
    let max_tokens = req["max_tokens"].as_u64().unwrap_or(cfg.max_output).min(cfg.max_output);
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
    } else {
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
    };
    let base = cfg.base_url.clone().unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    let url = if responses { format!("{base}/responses") } else { format!("{base}/chat/completions") };
    // Generation can be billable even when its response is lost. The supervisor owns admission and recovery;
    // neither this adapter nor reqwest may silently dispatch a second attempt.
    let response = http.post(&url).bearer_auth(key).json(&body).send().await?;
    let status = response.status();
    let retry_after = response.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()?.trim().parse::<f64>().ok());
    let text = response.text().await?;
    let answered = |message: String| Answered { status: status.as_u16(), message, retry_after };
    if !status.is_success() { return Err(answered(format!("API {status}: {text}")).into()); }
    let resp: Value = serde_json::from_str(&text).map_err(|e| answered(format!("API returned a malformed body: {e}")))?;
    if resp.get("error").is_some_and(|error| !error.is_null()) { return Err(answered(format!("API error: {}", resp["error"])).into()); }

    let u = &resp["usage"];
    let mut out = Reply::default();
    if responses {
        let d = &u["input_tokens_details"];
        out.cache_read = d["cached_tokens"].as_u64().unwrap_or(0);
        out.cache_write = d["cache_write_tokens"].as_u64().unwrap_or(0);
        out.input = u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(out.cache_read + out.cache_write);
        out.output = u["output_tokens"].as_u64().unwrap_or(0);
        let items: Vec<Value> = resp["output"].as_array().cloned().unwrap_or_default();
        out.text = items.iter().filter(|o| o["type"] == "message").flat_map(|o| o["content"].as_array().into_iter().flatten()).filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
        out.calls = items.iter().filter(|o| o["type"] == "function_call").map(|o| call(o["call_id"].as_str().unwrap_or(""), o["name"].as_str().unwrap_or(""), o["arguments"].as_str().unwrap_or(""))).collect();
        out.raw = serde_json::to_string(&items)?;
        let cut = resp["incomplete_details"]["reason"].as_str() == Some("max_output_tokens");
        out.finish = Some(if cut { "length".into() } else { "stop".to_string() });
    } else {
        let d = &u["prompt_tokens_details"];
        out.cache_read = d["cached_tokens"].as_u64().unwrap_or(0);
        out.cache_write = d["cache_write_tokens"].as_u64().unwrap_or(0);
        out.input = u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(out.cache_read + out.cache_write);
        out.output = u["completion_tokens"].as_u64().unwrap_or(0);
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
    }
    Ok(out)
}
