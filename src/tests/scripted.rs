//! A scripted model: answers each request with the next step of a script, like the check suites' mock provider
//! (`scripts/mock_llm.py`), but in-process. A reply is built as the Responses API object the provider would send and
//! parsed by the HTTP client's own parser, so finish reasons, usage and call parsing are the client's; failures are
//! the same typed errors the HTTP client returns.
use crate::llm::{self, Answered, LlmCfg, Model, Reply};
use crate::net;
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One scripted answer to a model request.
#[derive(Clone, Debug)]
pub(crate) enum Step {
    /// A generation.
    Reply(Output),
    /// The provider answered with an error status (`Retry-After` seconds, if any) and generated nothing usable.
    Status {
        status: u16,
        body: String,
        retry_after: Option<f64>,
    },
    /// The request was sent and its response lost: the generation may have been billed.
    Lost,
    /// The request never reached the provider.
    NotSent,
    /// No answer ever arrives.
    Hang,
    /// `then`, once `delay` has passed on tokio's clock (instant under a paused test clock).
    After { delay: Duration, then: Box<Step> },
    /// `then`, once as many requests as `barrier` counts are waiting on it: only requests in flight together get past.
    Together {
        barrier: Arc<tokio::sync::Barrier>,
        then: Box<Step>,
    },
}

/// A generation, as the provider reports it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Output {
    text: String,
    /// (tool name, arguments as JSON text)
    calls: Vec<(String, String)>,
    /// usage: input tokens (all of them, cached ones included), cached reads, cache writes, output tokens
    input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    /// cut off at the output cap
    cut: bool,
    reasoning: Option<String>,
}

/// A plain-text reply (usage: 10 input and 5 output tokens, as the mock provider reports).
pub(crate) fn text(text: &str) -> Step {
    Step::Reply(Output {
        text: text.into(),
        input: 10,
        output: 5,
        ..Output::default()
    })
}

/// A reply with one `luau` tool call running `code`.
pub(crate) fn luau(code: &str) -> Step {
    luau_calls(&[code])
}

/// A reply with one `luau` tool call per chunk, in one turn.
pub(crate) fn luau_calls(codes: &[&str]) -> Step {
    let mut step = text("");
    for code in codes {
        step = step.call("luau", &json!({ "code": code }).to_string());
    }
    step
}

/// The provider answered `status` with `body`.
pub(crate) fn status(status: u16, body: &str) -> Step {
    Step::Status {
        status,
        body: body.into(),
        retry_after: None,
    }
}

impl Step {
    fn output(&mut self) -> &mut Output {
        match self {
            Step::Reply(output) => output,
            other => panic!("not a reply: {other:?}"),
        }
    }

    /// Add a tool call with hand-made (even invalid) arguments.
    pub(crate) fn call(mut self, name: &str, arguments: &str) -> Self {
        self.output().calls.push((name.into(), arguments.into()));
        self
    }

    /// Set the reply text (beside its calls).
    pub(crate) fn with_text(mut self, text: &str) -> Self {
        self.output().text = text.into();
        self
    }

    /// Report `input` prompt tokens (the mock provider's `prompt_tokens`).
    pub(crate) fn tokens(mut self, input: u64) -> Self {
        self.output().input = input;
        self
    }

    /// Report this usage: input tokens in all (cached ones included), cached reads, cache writes, output tokens.
    pub(crate) fn usage(
        mut self,
        input: u64,
        cache_read: u64,
        cache_write: u64,
        output: u64,
    ) -> Self {
        let o = self.output();
        (o.input, o.cache_read, o.cache_write, o.output) = (input, cache_read, cache_write, output);
        self
    }

    /// Cut the reply off at the output cap (`finish: "length"`).
    pub(crate) fn cut(mut self) -> Self {
        self.output().cut = true;
        self
    }

    /// Add a reasoning summary item, which the loop echoes back on later turns.
    pub(crate) fn reasoning(mut self, summary: &str) -> Self {
        self.output().reasoning = Some(summary.into());
        self
    }

    /// Answer after `seconds` on tokio's clock.
    pub(crate) fn after(self, seconds: f64) -> Self {
        Step::After {
            delay: Duration::from_secs_f64(seconds),
            then: Box::new(self),
        }
    }

    /// Answer once every other request waiting on `barrier` has arrived.
    pub(crate) fn together(self, barrier: &Arc<tokio::sync::Barrier>) -> Self {
        Step::Together {
            barrier: barrier.clone(),
            then: Box::new(self),
        }
    }

    /// Add a `Retry-After` of `seconds` to an error status.
    pub(crate) fn retry_after(mut self, seconds: f64) -> Self {
        match &mut self {
            Step::Status { retry_after, .. } => *retry_after = Some(seconds),
            other => panic!("not a status: {other:?}"),
        }
        self
    }
}

impl Output {
    /// The finished Responses API object; call ids are `call_{index}_{k}`, as the mock provider names them.
    fn response(&self, index: usize) -> Value {
        let mut items = vec![json!({"type": "reasoning", "summary": self.reasoning.iter()
            .map(|s| json!({"type": "summary_text", "text": s})).collect::<Vec<_>>()})];
        if !self.text.is_empty() || self.calls.is_empty() {
            items.push(json!({"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": self.text}]}));
        }
        for (k, (name, arguments)) in self.calls.iter().enumerate() {
            items.push(json!({"type": "function_call", "call_id": format!("call_{index}_{k}"), "name": name,
                "arguments": arguments}));
        }
        let mut response = json!({"error": null, "output": items, "usage": {"input_tokens": self.input,
            "output_tokens": self.output, "input_tokens_details": {"cached_tokens": self.cache_read,
            "cache_write_tokens": self.cache_write}}});
        if self.cut {
            response["incomplete_details"] = json!({"reason": "max_output_tokens"});
        }
        response
    }
}

/// A request the scripted model received.
#[derive(Clone, Debug)]
pub(crate) struct Request {
    pub cfg: LlmCfg,
    /// the neutral request: {system, messages, tools?, max_tokens?}
    pub body: Value,
    /// when it arrived, on tokio's clock (exact under a paused test clock)
    pub at: tokio::time::Instant,
}

type Answer = dyn Fn(&Value, usize) -> Step + Send + Sync;

/// The scripted model. Every request is recorded, failures included.
pub(crate) struct Scripted {
    answer: Box<Answer>,
    seen: Mutex<Vec<Request>>,
}

impl Scripted {
    /// Serve `steps` in order; the last one answers every later request.
    pub(crate) fn replies(steps: Vec<Step>) -> Arc<Self> {
        assert!(!steps.is_empty(), "a script needs at least one step");
        Self::by(move |_, index| steps[index.min(steps.len() - 1)].clone())
    }

    /// Answer each request with `answer(request, index)`, `index` counting requests from 0.
    pub(crate) fn by(answer: impl Fn(&Value, usize) -> Step + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            answer: Box::new(answer),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// Every request received so far, in arrival order.
    pub(crate) fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// The bodies of every request received so far.
    pub(crate) fn bodies(&self) -> Vec<Value> {
        self.requests().into_iter().map(|r| r.body).collect()
    }
}

impl Model for Scripted {
    fn send<'a>(&'a self, cfg: &'a LlmCfg, req: &'a Value) -> BoxFuture<'a, anyhow::Result<Reply>> {
        let (index, step) = {
            let mut seen = self.seen.lock().unwrap();
            seen.push(Request {
                cfg: cfg.clone(),
                body: req.clone(),
                at: tokio::time::Instant::now(),
            });
            (seen.len() - 1, (self.answer)(req, seen.len() - 1))
        };
        Box::pin(answer(step, index))
    }
}

/// Wait out `step`'s delays and rendezvous, then answer it as the HTTP client would.
async fn answer(mut step: Step, index: usize) -> anyhow::Result<Reply> {
    loop {
        step = match step {
            Step::After { delay, then } => {
                tokio::time::sleep(delay).await;
                *then
            }
            Step::Together { barrier, then } => {
                barrier.wait().await;
                *then
            }
            Step::Reply(output) => {
                return llm::reply(output.response(index)).map_err(|message| {
                    Answered {
                        status: 200,
                        message,
                        retry_after: None,
                    }
                    .into()
                });
            }
            Step::Status {
                status,
                body,
                retry_after,
            } => {
                let code = reqwest::StatusCode::from_u16(status).expect("a valid status code");
                return Err(Answered {
                    status,
                    message: format!("API {code}: {body}"),
                    retry_after,
                }
                .into());
            }
            Step::Lost => {
                return Err(anyhow::anyhow!(
                    "the response stream ended before the response completed"
                ));
            }
            Step::NotSent => {
                return Err(net::Error::new(
                    net::Delivery::NotSent,
                    "model request failed: connection refused",
                )
                .into());
            }
            Step::Hang => return std::future::pending().await,
        }
    }
}
