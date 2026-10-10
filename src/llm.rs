//! Provider client, three wire formats: Anthropic Messages (`anthropic`), OpenAI-compatible Chat Completions (`openai`) and
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
}

#[derive(Default, Debug, Clone)]
pub struct Reply {
    pub text: String,
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

/// `req`: { system?: string, messages: [{role, content: string}], max_tokens?: number, cache?: bool }
pub async fn chat(http: &reqwest::Client, cfg: &LlmCfg, key: &str, req: &Value) -> Result<Reply> {
    if !matches!(cfg.api.as_str(), "anthropic" | "openai" | "responses") {
        bail!("unknown --api {:?} (expected anthropic, openai or responses)", cfg.api);
    }
    let openai = cfg.openai_style();
    let responses = cfg.api == "responses";
    let system = req["system"].as_str().unwrap_or("");
    let cache = req["cache"].as_bool().unwrap_or(true);
    let max_tokens = req["max_tokens"].as_u64().unwrap_or(cfg.max_output);
    let mut msgs: Vec<Value> = req["messages"].as_array().cloned().unwrap_or_default();
    if cache {
        // one moving breakpoint on the conversation prefix, plus the system prompt
        if let Some(last) = msgs.last_mut() {
            let text = last["content"].as_str().unwrap_or_default().to_string();
            last["content"] = json!([{"type": "text", "text": text, "cache_control": {"type": "ephemeral"}}]);
        }
    }
    let sys_block = if cache { json!({"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}) } else { json!({"type": "text", "text": system}) };
    let body = if responses {
        // Responses API: system text is `instructions`; plain string content (no Anthropic cache markers).
        let input: Vec<Value> = req["messages"].as_array().into_iter().flatten().map(|m| json!({"role": m["role"], "content": m["content"].as_str().unwrap_or_default()})).collect();
        let mut b = json!({"model": cfg.model, "max_output_tokens": max_tokens, "instructions": system, "input": input});
        if let Some(e) = &cfg.reasoning {
            b["reasoning"] = json!({"effort": e});
        }
        b
    } else if openai {
        let mut all = vec![json!({"role": "system", "content": [sys_block]})];
        all.extend(msgs);
        let mut b = json!({"model": cfg.model, "max_tokens": max_tokens, "messages": all, "usage": {"include": true}});
        if let Some(e) = &cfg.reasoning {
            b["reasoning"] = json!({"effort": e});
        }
        b
    } else {
        json!({"model": cfg.model, "max_tokens": max_tokens, "system": [sys_block], "messages": msgs})
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
        out.text = resp["output"].as_array().into_iter().flatten().filter(|o| o["type"] == "message").flat_map(|o| o["content"].as_array().into_iter().flatten()).filter(|c| c["type"] == "output_text").filter_map(|c| c["text"].as_str()).collect::<Vec<_>>().join("\n");
        let cut = resp["incomplete_details"]["reason"].as_str() == Some("max_output_tokens");
        out.finish = Some(if cut { "length".into() } else { "stop".to_string() });
    } else if openai {
        let d = &u["prompt_tokens_details"];
        out.cache_read = d["cached_tokens"].as_u64().unwrap_or(0);
        out.cache_write = d["cache_write_tokens"].as_u64().unwrap_or(0);
        out.input = u["prompt_tokens"].as_u64().unwrap_or(0).saturating_sub(out.cache_read + out.cache_write);
        out.output = u["completion_tokens"].as_u64().unwrap_or(0);
        out.cost = u["cost"].as_f64().unwrap_or(0.0);
        out.text = resp["choices"][0]["message"]["content"].as_str().unwrap_or_default().to_string();
        out.finish = resp["choices"][0]["finish_reason"].as_str().map(String::from);
    } else {
        out.input = u["input_tokens"].as_u64().unwrap_or(0);
        out.output = u["output_tokens"].as_u64().unwrap_or(0);
        out.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        out.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        out.text = resp["content"].as_array().into_iter().flatten().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n");
        out.finish = resp["stop_reason"].as_str().map(|s| if s == "max_tokens" { "length".into() } else { s.to_string() });
    }
    Ok(out)
}
