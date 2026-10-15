//! Provider client, two wire formats, both with native tool calling: OpenAI-compatible Chat Completions (`openai`) and
//! the Responses API (`responses`). Any model on any compatible endpoint (OpenRouter, Enclave, local) works: nothing here
//! restricts the model or the upstream provider.
use anyhow::{bail, Result};
use serde_json::{json, Value};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LlmCfg {
    pub model: String,
    /// "openai" | "responses" | "claude-sdk"
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
    /// this model's price, looked up at launch when a dollar threshold is set; bounds each request's reservation
    #[serde(default)]
    pub price: Option<Price>,
}

/// USD per token (and per request), from the endpoint's model list.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Price {
    /// the base price and each listed override (long-context and time-of-day tiers)
    pub tiers: Vec<Tier>,
    pub request: f64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Tier {
    /// the tier applies from this many prompt tokens (0: always possible, e.g. a time-of-day tier)
    pub min_prompt_tokens: u64,
    /// the highest of the prompt and cache-write prices
    pub input: f64,
    pub output: f64,
}

impl Price {
    /// An upper bound on one request's cost: every request byte billed as an input token, plus the full output cap, at
    /// the highest tier the request could fall in. A request has no more prompt tokens than bytes, so a tier whose
    /// threshold exceeds the byte count cannot apply.
    pub fn bound(&self, request_bytes: u64, max_output: u64) -> f64 {
        let (input, output) = self.tiers.iter().filter(|t| t.min_prompt_tokens <= request_bytes)
            .fold((0.0_f64, 0.0_f64), |(i, o), t| (i.max(t.input), o.max(t.output)));
        request_bytes as f64 * input + max_output as f64 * output + self.request
    }
}

/// The model's price from `{base}/models` (OpenRouter publishes `pricing` there). None when the endpoint has no
/// usable price for it, e.g. a router with variable pricing.
pub async fn fetch_price(cfg: &LlmCfg) -> Option<Price> {
    if cfg.api == "claude-sdk" { return None; }
    let base = cfg.base_url.clone().unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build().ok()?;
    // OpenRouter's model list is public; send the key when there is one, for endpoints that require it.
    let mut get = http.get(format!("{base}/models"));
    if let Ok(key) = cfg.key() { get = get.bearer_auth(key); }
    let list: Value = get.send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
    let model = list["data"].as_array()?.iter().find(|m| m["id"] == cfg.model.as_str())?;
    price_of(&model["pricing"])
}

/// Each tier's prices, an override inheriting what it does not list from the base. A negative price means variable: None.
fn price_of(pricing: &Value) -> Option<Price> {
    let num = |t: &Value, k: &str| -> Option<Option<f64>> {
        let v = &t[k];
        match v.as_str().and_then(|s| s.trim().parse::<f64>().ok()).or_else(|| v.as_f64()).filter(|p| p.is_finite()) {
            Some(p) if p < 0.0 => None,
            other => Some(other.or_else(|| { let b = &pricing[k]; b.as_str().and_then(|s| s.trim().parse::<f64>().ok()).or_else(|| b.as_f64()) })),
        }
    };
    let tier = |t: &Value| -> Option<Tier> {
        let input = [num(t, "prompt")?, num(t, "input_cache_write")?, num(t, "input_cache_write_1h")?];
        Some(Tier { min_prompt_tokens: t["min_prompt_tokens"].as_u64().unwrap_or(0),
            input: input[0]?.max(input[1].unwrap_or(0.0)).max(input[2].unwrap_or(0.0)), output: num(t, "completion")?? })
    };
    let tiers = std::iter::once(pricing).chain(pricing["overrides"].as_array().into_iter().flatten()).map(tier).collect::<Option<Vec<_>>>()?;
    Some(Price { tiers, request: num(pricing, "request")?.unwrap_or(0.0) })
}

#[cfg(test)]
mod price_tests {
    use super::*;

    #[test]
    fn price_tiers_inherit_the_base_and_bound_by_reachable_tiers() {
        let haiku = json!({"prompt": "0.0000001", "completion": "0.0000005", "input_cache_write": "0.000000125", "input_cache_write_1h": "0.0000002",
            "overrides": [{"min_prompt_tokens": 100000, "prompt": "0.0000005", "completion": "0.0000025", "input_cache_write_1h": "0.000001"}]});
        let p = price_of(&haiku).unwrap();
        assert_eq!(p.tiers, vec![Tier { min_prompt_tokens: 0, input: 0.0000002, output: 0.0000005 },
            Tier { min_prompt_tokens: 100000, input: 0.000001, output: 0.0000025 }]);
        // a short request cannot reach the long-context tier; a long one can
        assert!((p.bound(1000, 1000) - (1000.0 * 0.0000002 + 1000.0 * 0.0000005)).abs() < 1e-12);
        assert!((p.bound(200000, 1000) - (200000.0 * 0.000001 + 1000.0 * 0.0000025)).abs() < 1e-12);
        // a time-of-day tier has no threshold, so it always counts
        let peak = price_of(&json!({"prompt": "0.000001", "completion": "0.000002", "request": "0.01",
            "overrides": [{"utc_days": ["monday"], "prompt": "0.000002", "completion": "0.000004"}]})).unwrap();
        assert!((peak.bound(10, 10) - (10.0 * 0.000002 + 10.0 * 0.000004 + 0.01)).abs() < 1e-12);
        assert_eq!(price_of(&json!({"prompt": "-1", "completion": "-1"})), None);
        assert_eq!(price_of(&json!({"prompt": "0.000001"})), None);
    }
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
    pub cost: f64,
}

impl LlmCfg {
    pub fn key(&self) -> Result<String> {
        if self.api == "claude-sdk" {
            return Ok(String::new()); // the claude CLI holds the login
        }
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
    if cfg.api == "claude-sdk" {
        return crate::claude_sdk::chat(cfg, req).await;
    }
    if !matches!(cfg.api.as_str(), "openai" | "responses") {
        bail!("unknown --api {:?} (expected openai, responses or claude-sdk)", cfg.api);
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
    if resp.get("error").is_some() { return Err(answered(format!("API error: {}", resp["error"])).into()); }

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
    } else {
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
    }
    Ok(out)
}
