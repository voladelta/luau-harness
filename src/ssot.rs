//! String Seed of Thought (https://pub.sakana.ai/ssot/): deliberately different answers from one model. Each sample
//! gets fresh host entropy; the model lists the root-level dimensions along which answers differ, picks one option per
//! dimension from a different part of the seed, then answers. In blind tests on DeepSeek V4.1 Flash this, not the
//! paper's self-generated string, moved answers off the model's default. `examples/tools/ssot.lua` sends the same
//! instructions from a tool.
use crate::llm::{self, LlmCfg};
use serde_json::json;
use std::time::Duration;

/// Attempts per sample: rate limits and server errors are waited out, anything else gives up.
const ATTEMPTS: i32 = 5;

pub fn system(seed: &str) -> String {
    format!("You must produce exactly one unique and diverse answer. Your random string is \
<random_string>{seed}</random_string>. Within <thinking> and </thinking> tags: first list \
5 to 7 root-level dimensions along which answers to this task can differ fundamentally \
(not surface details), each with at least 6 options ranging from conventional to radical. \
Then pick one option per dimension using a different part of the random string \
(e.g. sum of character codes of a segment, mod the number of options). Commit to the \
picks even if unusual, and make them cohere.\n\
Then, provide your final answer, enclosed within <answer> and </answer> tags.")
}

/// Fresh OS entropy, so no earlier call or fixed PRNG state can predict the seed.
pub fn seed() -> std::io::Result<String> {
    use base64::Engine;
    let mut bytes = [0u8; 24];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// The answer in a reply. The last tagged answer wins: a model may quote the tags while reasoning. Untagged text after
/// `</thinking>` is an answer only when no `<answer>` was opened (an opened one without a close was cut off).
pub fn answer(text: &str) -> Option<String> {
    let tagged = text.match_indices("<answer>").filter_map(|(i, _)| {
        let rest = &text[i + "<answer>".len()..];
        rest.find("</answer>").map(|end| &rest[..end])
    }).last();
    let untagged = || (!text.contains("<answer>")).then(|| text.rsplit_once("</thinking>").map(|(_, a)| a)).flatten();
    tagged.or_else(untagged).map(str::trim).filter(|a| !a.is_empty()).map(String::from)
}

/// One sample for `prompt` from `cfg` without reasoning: the answer (None when the reply has none or the request
/// failed) and its cost.
pub async fn sample(http: &reqwest::Client, cfg: &LlmCfg, key: &str, prompt: &str) -> (Option<String>, f64) {
    let Ok(seed) = seed() else { return (None, 0.0) };
    let mut cfg = cfg.clone();
    cfg.reasoning = None; // no reasoning gave the most variety
    let req = json!({"system": system(&seed), "messages": [{"role": "user", "content": prompt}]});
    for attempt in 0..ATTEMPTS {
        match llm::chat(http, &cfg, key, &req).await {
            Ok(reply) => return (answer(&reply.text), reply.cost),
            Err(error) => match error.downcast_ref::<llm::Answered>() {
                Some(a) if a.retryable() && attempt + 1 < ATTEMPTS => {
                    // Back off with jitter so concurrent samples do not retry in lockstep; honour Retry-After.
                    let jitter = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_millis()) as f64 / 1000.0;
                    let wait = a.retry_after.unwrap_or(2f64.powi(attempt + 1)).clamp(1.0, 30.0) + jitter;
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
                _ => return (None, 0.0),
            },
        }
    }
    (None, 0.0)
}

#[cfg(test)]
mod tests {
    use super::answer;

    #[test]
    fn answers() {
        assert_eq!(answer("<thinking>x <answer>quoted</answer></thinking> <answer> real </answer>").as_deref(), Some("real"));
        assert_eq!(answer("<thinking>x</thinking>\n plain ").as_deref(), Some("plain"));
        assert_eq!(answer("<thinking>x</thinking><answer>cut off"), None);
        assert_eq!(answer("done"), None);
    }
}
