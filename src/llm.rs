//! Responses API client with native tool calling for any compatible endpoint.
use anyhow::Result;
use serde_json::{json, Value};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LlmCfg {
    pub model: String,
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
    /// the provider-native output items (JSON text), echoed back verbatim next turn so reasoning survives
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

/// Tool-call syntax that reached the reply text instead of a structured call (the route failed to parse the model's call).
/// DeepSeek's own markers only: their fullwidth bars do not occur in ordinary prose.
const TOOL_MARKUP: [&str; 2] = ["｜DSML｜", "<｜tool▁call"];

/// `req`: { system?: string, messages: [neutral message], tools?: [{name, description, parameters}], max_tokens?: number }
pub async fn chat(http: &reqwest::Client, cfg: &LlmCfg, key: &str, req: &Value) -> Result<Reply> {
    let system = req["system"].as_str().unwrap_or("");
    // Responses caches automatically; prompt_cache_key keeps the run on one cache.
    let max_tokens = req["max_tokens"].as_u64().unwrap_or(cfg.max_output).min(cfg.max_output);
    let msgs: Vec<Value> = req["messages"].as_array().cloned().unwrap_or_default();
    let tools: Vec<Value> = req["tools"].as_array().cloned().unwrap_or_default();
    let mut body = json!({"model": cfg.model, "max_output_tokens": max_tokens, "instructions": system, "input": responses_input(&msgs)});
    if !tools.is_empty() {
        body["tools"] = json!(tools.iter().map(|t| json!({"type": "function", "name": t["name"], "description": t["description"], "parameters": t["parameters"]})).collect::<Vec<_>>());
    }
    if let Some(e) = &cfg.reasoning {
        body["reasoning"] = json!({"effort": e});
    }
    if !cfg.provider_only.is_empty() {
        body["provider"] = json!({"only": cfg.provider_only, "allow_fallbacks": false});
    }
    add_cache_key(&mut body, cfg);
    // Streamed, so a long reasoning reply keeps the connection busy: the client's read timeout bounds silence, not
    // the whole generation. Only the final response object is used.
    body["stream"] = json!(true);
    let base = cfg.base_url.clone().unwrap_or_else(|| "https://openrouter.ai/api/v1".into());
    let url = format!("{base}/responses");
    // Generation can be billable even when its response is lost. The supervisor owns admission and recovery;
    // neither this adapter nor reqwest may silently dispatch a second attempt.
    let mut response = http.post(&url).bearer_auth(key).json(&body).send().await?;
    let status = response.status();
    let retry_after = response.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()?.trim().parse::<f64>().ok());
    let answered = |message: String| Answered { status: status.as_u16(), message, retry_after };
    let sse = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/event-stream"));
    if !status.is_success() || !sse {
        let text = response.text().await?;
        if !status.is_success() { return Err(answered(format!("API {status}: {text}")).into()); }
        // a provider that ignores `stream` answers with the plain response object
        let resp: Value = serde_json::from_str(&text).map_err(|e| answered(format!("API returned a malformed body: {e}")))?;
        return reply(resp).map_err(|m| answered(m).into());
    }
    let mut events = Sse::default();
    while let Some(chunk) = response.chunk().await? {
        for data in events.push(&chunk) {
            let Ok(event) = serde_json::from_str::<Value>(&data) else { continue }; // e.g. a `[DONE]` sentinel
            match event["type"].as_str().unwrap_or("") {
                "response.completed" | "response.incomplete" => return reply(event["response"].clone()).map_err(|m| answered(m).into()),
                "response.failed" => return Err(answered(format!("API error: {}", event["response"]["error"])).into()),
                "error" => return Err(answered(format!("API error: {}", event.get("error").unwrap_or(&event))).into()),
                _ => {},
            }
        }
    }
    // The stream ended without a final response: like a lost connection, the generation may have been billed.
    anyhow::bail!("the response stream ended before the response completed")
}

/// Server-sent events: bytes in, the `data` of each complete event out.
#[derive(Default)]
struct Sse {
    buf: Vec<u8>,
    /// bytes of `buf` already searched for an event end
    scanned: usize,
}

impl Sse {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        // A line ends with \n or \r\n; JSON data never holds a raw \r, so dropping every \r leaves \n endings only.
        self.buf.extend(bytes.iter().filter(|&&b| b != b'\r'));
        let mut out = Vec::new();
        // An event ends at a blank line. The final response is one large event, so resume the search where it stopped.
        while let Some(end) = self.buf.get(self.scanned.saturating_sub(1)..).and_then(|b| b.windows(2).position(|w| w == b"\n\n")) {
            let end = self.scanned.saturating_sub(1) + end;
            let event = String::from_utf8_lossy(&self.buf[..end]).into_owned();
            self.buf.drain(..end + 2);
            self.scanned = 0;
            let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
            if !data.is_empty() { out.push(data.join("\n")); }
        }
        self.scanned = self.buf.len();
        out
    }
}

#[cfg(test)]
mod sse_tests {
    use super::Sse;

    #[test]
    fn events_split_across_chunks_and_line_endings() {
        let mut sse = Sse::default();
        assert!(sse.push(b"event: response.created\ndata: {\"a\"").is_empty());
        assert_eq!(sse.push(b":1}\n\n: comment\n\ndata: x\r\n\r\ndata: one\ndata: two\n"), vec!["{\"a\":1}", "x"]);
        assert_eq!(sse.push(b"\n"), vec!["one\ntwo"]);
        assert!(sse.push(b"data: partial\r\n\r").is_empty());
        assert_eq!(sse.push(b"\n"), vec!["partial"]);
    }
}

/// A finished Responses API response object -> Reply; Err is the message of a provider error.
fn reply(resp: Value) -> std::result::Result<Reply, String> {
    if resp.get("error").is_some_and(|error| !error.is_null()) { return Err(format!("API error: {}", resp["error"])); }

    let u = &resp["usage"];
    let mut out = Reply::default();
    let d = &u["input_tokens_details"];
    out.cache_read = d["cached_tokens"].as_u64().unwrap_or(0);
    out.cache_write = d["cache_write_tokens"].as_u64().unwrap_or(0);
    out.input = u["input_tokens"].as_u64().unwrap_or(0).saturating_sub(out.cache_read + out.cache_write);
    out.output = u["output_tokens"].as_u64().unwrap_or(0);
    let items: Vec<Value> = resp["output"].as_array().cloned().unwrap_or_default();
    out.text = items.iter().filter(|o| o["type"] == "message").flat_map(|o| o["content"].as_array().into_iter().flatten()).filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
    out.calls = items.iter().filter(|o| o["type"] == "function_call").map(|o| call(o["call_id"].as_str().unwrap_or(""), o["name"].as_str().unwrap_or(""), o["arguments"].as_str().unwrap_or(""))).collect();
    out.raw = serde_json::to_string(&items).map_err(|e| e.to_string())?;
    let cut = resp["incomplete_details"]["reason"].as_str() == Some("max_output_tokens");
    let markup = out.calls.is_empty() && TOOL_MARKUP.iter().any(|m| out.text.contains(m));
    out.finish = Some(if cut { "length" } else if markup { "tool_markup" } else { "stop" }.into());
    Ok(out)
}
