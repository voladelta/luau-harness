//! The episodes' side of a run: an MCP server on loopback whose tools `eval`, `claim`, `claims` and `reset` the agent
//! calls as `mcp.ar.*`. Each episode's child `hx run` gets its own bearer token, which names the episode (its role, its
//! candidate, its workspace), so a call can act only for the episode it came from, and stops working when the child
//! has exited.
//!
//! The server speaks just enough of MCP's Streamable HTTP transport for hx's client: one JSON-RPC message per POST,
//! answered with a plain JSON body. Every handler runs to its end even when the client has gone, so what it journaled
//! stays true whether or not its answer arrived. `claim` and `reset` are idempotent: repeating one returns what the
//! first returned; repeating `eval` evaluates the same bytes again, which refines their score and never counts as an
//! improvement.
use crate::files::{self, capture, evaluate, load_snap, restore, save_snap};
use crate::run::{
    Claim, JUDGE_TOKENS, Journal, Record, Role, Row, Spec, claims, estimate, estimate_of, noisy,
    rows,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// The episode a token names: what its calls may do and where its files are.
#[derive(Clone, Debug)]
pub(crate) struct Ep {
    /// This attempt's label (`r1-w3`, or `r1-w3.2` for a retry).
    pub(crate) label: String,
    /// The planned episode it is an attempt of (`r1-w3`).
    pub(crate) plan: String,
    pub(crate) role: Role,
    /// The candidate an exploit worker improves, or an auditor audits (0 for research and explore).
    pub(crate) idea: u32,
    /// The snapshot the workspace starts from: an exploit worker's candidate's best state, which an improvement must
    /// beat, or the seed.
    pub(crate) start: String,
    /// The episode's throwaway workspace.
    pub(crate) ws: PathBuf,
}

impl Ep {
    /// The scope of this worker's claims: `explore` for every explore worker, `cN` for refinements of candidate N.
    fn scope(&self, what: &str) -> Result<String> {
        Ok(match self.role {
            Role::Explore => "explore".into(),
            Role::Exploit => format!("c{}", self.idea),
            Role::Research | Role::Audit => bail!("{what} is for explore and exploit workers"),
        })
    }

    /// The claim this worker holds: one accepted for this planned episode, by this attempt or an earlier one.
    fn claim<'a>(&self, records: &'a [Record]) -> Option<&'a Claim> {
        let scope = self.scope("claim").ok()?;
        claims(records).find(|c| c.scope == scope && plan_of(&c.worker) == self.plan)
    }

    /// The snapshot of this worker's best state so far: its last state that improved, else an exploit worker's starting
    /// state. An explore worker has none until its first valid state, which sets it even below the seed.
    fn best(&self, records: &[Record]) -> Option<String> {
        rows(records)
            .filter(|r| r.episode == self.label && r.improved)
            .last()
            .map(|r| r.snap.clone())
            .or_else(|| (self.role == Role::Exploit).then(|| self.start.clone()))
    }
}

/// The planned episode an attempt's label belongs to (`r1-w3.2` -> `r1-w3`).
fn plan_of(label: &str) -> &str {
    label.split('.').next().unwrap_or(label)
}

/// The model the claim judge asks: the episodes' own provider and model.
pub(crate) struct Judge {
    http: reqwest::Client,
    cfg: hx::llm::LlmCfg,
    key: String,
}

impl Judge {
    /// From the episodes' `hx run` flags; None when they name no model or its key is not set, and then every claim
    /// that passes the other checks is accepted.
    pub(crate) fn new(child_args: &[String]) -> Option<Judge> {
        let cfg = crate::run::llm_cfg(child_args)?;
        let key = cfg.key().ok()?;
        let http = hx::llm::client(&cfg).ok()?;
        Some(Judge { http, cfg, key })
    }
}

/// What the run loop and the episodes' MCP server share.
pub(crate) struct Shared {
    dir: PathBuf,
    spec: Spec,
    pub(crate) journal: Journal,
    judge: Option<Judge>,
    /// The server's URL, once it listens.
    url: OnceLock<String>,
    /// Token -> the episode it names, while its child runs.
    episodes: Mutex<HashMap<String, Arc<Ep>>>,
    /// Claims are judged one at a time, in arrival order (tokio's mutex is fair).
    claiming: tokio::sync::Mutex<()>,
    /// Evaluations of one state take turns, so a deterministic evaluator's result is reused by the next.
    scoring: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// An episode's token, served until this is dropped.
pub(crate) struct Served<'a> {
    shared: &'a Shared,
    token: String,
}

impl Drop for Served<'_> {
    fn drop(&mut self) {
        self.shared
            .episodes
            .lock()
            .expect("episode table lock poisoned")
            .remove(&self.token);
    }
}

impl Shared {
    pub(crate) fn new(dir: PathBuf, spec: Spec, journal: Journal, judge: Option<Judge>) -> Shared {
        Shared {
            dir,
            spec,
            journal,
            judge,
            url: OnceLock::new(),
            episodes: Mutex::default(),
            claiming: tokio::sync::Mutex::new(()),
            scoring: Mutex::default(),
        }
    }

    /// Listen on a free loopback port and serve the episodes' calls for as long as this process runs.
    pub(crate) async fn listen(self: &Arc<Self>) -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("cannot listen on loopback for the episodes' MCP server")?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        self.url
            .set(url)
            .map_err(|_| anyhow::anyhow!("the episodes' MCP server already listens"))?;
        let shared = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        tokio::spawn(connection(socket, shared.clone()));
                    }
                    Err(e) => eprintln!("hx-ar: MCP server: {e}"),
                }
            }
        });
        Ok(())
    }

    /// The URL episodes reach the server at.
    pub(crate) fn url(&self) -> Result<String> {
        self.url
            .get()
            .cloned()
            .context("the episodes' MCP server is not listening")
    }

    /// Serve `ep` under `token` until the returned guard is dropped.
    pub(crate) fn serve(&self, token: String, ep: Ep) -> Served<'_> {
        self.episodes
            .lock()
            .expect("episode table lock poisoned")
            .insert(token.clone(), Arc::new(ep));
        Served {
            shared: self,
            token,
        }
    }

    fn episode(&self, token: &str) -> Option<Arc<Ep>> {
        self.episodes
            .lock()
            .expect("episode table lock poisoned")
            .get(token)
            .cloned()
    }

    /// Append a record built from the journal so far, off the async threads.
    async fn append_with<T: Send + 'static>(
        self: &Arc<Self>,
        make: impl FnOnce(&[Record]) -> Result<(Record, T)> + Send + 'static,
    ) -> Result<T> {
        let shared = self.clone();
        tokio::task::spawn_blocking(move || shared.journal.append_with(make)).await?
    }
}

/// Largest request head and body the server reads.
const MAX_HEAD: u64 = 16 << 10;
const MAX_BODY: usize = 1 << 20;

/// Serve one keep-alive connection: requests in order until the client closes it or sends something unreadable.
async fn connection(socket: tokio::net::TcpStream, shared: Arc<Shared>) {
    let mut stream = BufReader::new(socket);
    loop {
        let request = match read_request(&mut stream).await {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(e) => {
                let _ = write_response(&mut stream, 400, &json!({"error": e.to_string()})).await;
                return;
            }
        };
        let (status, body) = respond(&shared, request).await;
        let written = match body {
            Some(body) => write_response(&mut stream, status, &body).await,
            None => write_empty(&mut stream, status).await,
        };
        if written.is_err() {
            return;
        }
    }
}

/// One HTTP request: its method, path, bearer token and body.
struct Request {
    method: String,
    path: String,
    token: Option<String>,
    body: Vec<u8>,
}

async fn read_request(stream: &mut BufReader<tokio::net::TcpStream>) -> Result<Option<Request>> {
    let mut head = String::new();
    let mut limited = (&mut *stream).take(MAX_HEAD);
    loop {
        let before = head.len();
        if limited.read_line(&mut head).await? == 0 {
            ensure!(head.is_empty(), "the request head was cut off");
            return Ok(None);
        }
        ensure!(head.ends_with('\n'), "the request head is too large");
        if head[before..].trim().is_empty() && before > 0 {
            break;
        }
    }
    let mut lines = head.lines();
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        start.next().unwrap_or_default().to_string(),
        start.next().unwrap_or_default().to_string(),
    );
    let (mut length, mut token) = (0usize, None);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().context("bad content-length")?,
            "authorization" => token = value.strip_prefix("Bearer ").map(String::from),
            "transfer-encoding" => bail!("chunked requests are not supported"),
            _ => {}
        }
    }
    ensure!(length <= MAX_BODY, "the request body is too large");
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    Ok(Some(Request {
        method,
        path,
        token,
        body,
    }))
}

async fn write_response(
    stream: &mut BufReader<tokio::net::TcpStream>,
    status: u16,
    body: &Value,
) -> std::io::Result<()> {
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        reason(status),
        body.len()
    );
    let socket = stream.get_mut();
    socket.write_all(head.as_bytes()).await?;
    socket.write_all(body.as_bytes()).await?;
    socket.flush().await
}

async fn write_empty(
    stream: &mut BufReader<tokio::net::TcpStream>,
    status: u16,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-length: 0\r\n\r\n",
        reason(status)
    );
    let socket = stream.get_mut();
    socket.write_all(head.as_bytes()).await?;
    socket.flush().await
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    }
}

/// The answer to one request: a status and a JSON body (none for an accepted notification).
async fn respond(shared: &Arc<Shared>, request: Request) -> (u16, Option<Value>) {
    if request.path != "/mcp" {
        return (404, Some(json!({"error": "not found"})));
    }
    if request.method != "POST" {
        return (405, Some(json!({"error": "POST only"})));
    }
    let Some(ep) = request.token.as_deref().and_then(|t| shared.episode(t)) else {
        return (
            401,
            Some(json!({"error": "unknown or expired episode token"})),
        );
    };
    let Ok(message) = serde_json::from_slice::<Value>(&request.body) else {
        return (400, Some(json!({"error": "the body is not JSON"})));
    };
    // A notification or a response to nothing hx-ar asked: accepted, nothing to answer.
    let Some(id) = message.get("id").filter(|id| !id.is_null()).cloned() else {
        return (202, None);
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let answer = match message["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "hx-ar", "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_list()})),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default().to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Ok(match call(shared, &ep, &name, args).await {
                Ok(value) => json!({
                    "content": [{"type": "text", "text": value.to_string()}],
                    "structuredContent": value,
                }),
                Err(error) => json!({
                    "content": [{"type": "text", "text": format!("{error:#}")}],
                    "isError": true,
                }),
            })
        }
        other => Err(format!("method not found: {other}")),
    };
    let body = match answer {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": message}})
        }
    };
    (200, Some(body))
}

fn tool_list() -> Value {
    let text = |name: &str, about: &str| json!({"type": "object", "properties": {name: {"type": "string", "description": about}}, "required": [name]});
    json!([
        {"name": "eval", "description": "Score the editable files with the owner's frozen evaluator; returns {n, score, valid, se, improved, best, report}.",
         "inputSchema": text("note", "one line on what this state is")},
        {"name": "claim", "description": "Claim the idea or refinement you will build (two or three sentences); returns {ok, n, message}.",
         "inputSchema": text("idea", "the idea, in two or three sentences")},
        {"name": "claims", "description": "The claims made so far in your scope, which yours must differ from.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "reset", "description": "Put your best scored state back into the editable paths; returns {best}.",
         "inputSchema": {"type": "object", "properties": {}}},
    ])
}

/// A tool's string argument.
fn text_arg(args: &Value, name: &str) -> Result<String> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => bail!("{name} must be a string"),
    }
}

async fn call(shared: &Arc<Shared>, ep: &Arc<Ep>, tool: &str, args: Value) -> Result<Value> {
    match tool {
        "eval" => eval(shared, ep, text_arg(&args, "note")?).await,
        "claim" => claim(shared, ep, text_arg(&args, "idea")?).await,
        "claims" => listed(shared, ep),
        "reset" => reset(shared, ep).await,
        other => bail!("no tool named {other:?}"),
    }
}

/// `eval`: snapshot the episode's editable files, score the snapshot, journal the evaluation. Explore and exploit
/// workers score only after their claim is accepted, and their states belong to their candidate.
async fn eval(shared: &Arc<Shared>, ep: &Arc<Ep>, note: String) -> Result<Value> {
    let claim = ep.claim(&shared.journal.records()).map(|c| c.n);
    let idea = match (ep.role, claim) {
        (Role::Explore | Role::Exploit, None) => {
            bail!("claim your idea with mcp.ar.claim{{idea = \"...\"}} before scoring it")
        }
        (Role::Explore, Some(n)) => n,
        (Role::Exploit, Some(_)) => ep.idea,
        (Role::Research | Role::Audit, _) => 0,
    };
    let (ws, dir, spec) = (ep.ws.clone(), shared.dir.clone(), shared.spec.clone());
    let (files, snap) = tokio::task::spawn_blocking(move || -> Result<_> {
        let files = capture(&ws, &spec)?;
        let snap = save_snap(&dir, &files)?;
        Ok((files, snap))
    })
    .await??;
    let turn = shared
        .scoring
        .lock()
        .expect("scoring table lock poisoned")
        .entry(snap.clone())
        .or_default()
        .clone();
    let _turn = turn.lock().await;
    // An evaluator that has never reported `se` in this run is deterministic: a state it already scored validly keeps
    // its stored result. A probe, or a state its candidate already has, gets that result and no new journal record; a
    // state only another candidate scored is recorded for this one, judged against its best, without running the
    // evaluator again.
    let records = shared.journal.records();
    let stored = if noisy(&records) {
        None
    } else {
        rows(&records).find(|r| r.snap == snap && r.valid).cloned()
    };
    if let Some(r) = stored.as_ref().filter(|_| {
        idea == 0 || rows(&records).any(|o| o.snap == snap && o.idea == idea && o.valid)
    }) {
        let best = ep
            .best(&records)
            .and_then(|b| estimate(&records, &b))
            .map(|e| e.mean);
        return Ok(
            json!({"n": r.n, "score": r.score, "valid": true, "se": null, "improved": false,
            "best": best, "report": r.report, "cached": true}),
        );
    }
    let scored = match &stored {
        Some(r) => files::Scored {
            score: r.score,
            valid: true,
            se: None,
            report: r.report.clone(),
        },
        None => evaluate(&shared.spec, &shared.dir, &files, &shared.spec.eval, false).await?,
    };
    let (ep_, spec) = (ep.clone(), shared.spec.clone());
    let (n, score, valid, se, improved, best, report) = shared
        .append_with(move |records| {
            let files::Scored {
                score,
                valid,
                se,
                report,
            } = scored;
            let mut row = Row {
                n: rows(records).count() + 1,
                episode: ep_.label.clone(),
                idea,
                note: note.chars().take(300).collect(),
                snap: snap.clone(),
                score,
                valid,
                se,
                improved: false,
                report,
            };
            // A state counts when it is new to this worker (evaluating the same bytes again refines their score, it is
            // not a step) and beats the worker's best by more than their combined standard error, so small steps add
            // up across rounds; `min_delta` is the bar against the seed, applied at the gate. The state a worker
            // starts from never improves it, so an explore worker's bar is set by the first state it builds.
            let best = ep_.best(records);
            let with_row = |s: &str| estimate_of(rows(records).chain([&row]), records, s);
            let improved = idea != 0
                && valid
                && snap != ep_.start
                && best.as_deref() != Some(snap.as_str())
                && with_row(&snap)
                    .is_some_and(|e| spec.beats(e, best.as_deref().and_then(&with_row), 0.0));
            let best = if improved { Some(snap.clone()) } else { best };
            let best_mean = best.as_deref().and_then(&with_row).map(|e| e.mean);
            row.improved = improved;
            let out = (
                row.n,
                row.score,
                row.valid,
                row.se,
                row.improved,
                best_mean,
                row.report.clone(),
            );
            Ok((Record::Evaluated(row), out))
        })
        .await?;
    let mut out = json!({"n": n, "score": score, "valid": valid, "se": se, "improved": improved,
        "best": best, "report": report});
    if stored.is_some() {
        out["cached"] = json!(true);
    }
    Ok(out)
}

/// `claim`: claim the idea an explore worker will test, or the refinement an exploit worker will try. Claims are
/// served one at a time in arrival order. A claim is rejected only when two independent judge calls both find that it
/// shares the core approach of the same earlier claim in its scope, so a borderline idea gets the benefit of the
/// doubt; a judge that cannot answer accepts. One accepted claim per worker; a retried worker keeps the claim of its
/// earlier attempt.
async fn claim(shared: &Arc<Shared>, ep: &Arc<Ep>, idea: String) -> Result<Value> {
    let scope = ep.scope("claim")?;
    let idea: String = idea
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1_000)
        .collect();
    // Agents probe a function with a dummy argument ("x"); an accepted claim is final, so a non-description is refused
    // without using up the claim.
    ensure!(
        idea.split(' ').count() >= 8,
        "describe the idea in two or three sentences (at least 8 words): an accepted \
claim is final and is how other workers, auditors and the report know what you built"
    );
    let _turn = shared.claiming.lock().await;
    let records = shared.journal.records();
    if let Some(mine) = ep.claim(&records) {
        // The same call again (its answer may have been lost) gets the same answer.
        if mine.worker == ep.label && mine.idea == idea {
            return Ok(accepted(mine.n));
        }
        if mine.worker == ep.label {
            bail!(
                "this worker already claimed #{}: one idea per worker",
                mine.n
            )
        }
        return Ok(
            json!({"ok": true, "n": mine.n, "idea": mine.idea, "message": format!("Accepted as #{}: an earlier attempt \
of this worker claimed this idea before it was interrupted. Continue with it: {}", mine.n, mine.idea)}),
        );
    }
    let earlier: Vec<Claim> = claims(&records)
        .filter(|c| c.scope == scope)
        .cloned()
        .collect();
    // Plain text, so a printed result shows every claim (printers abbreviate nested tables).
    let listed: String = earlier
        .iter()
        .map(|c| format!("{}. {}\n", c.n, c.idea))
        .collect();
    let what = if ep.role == Role::Exploit {
        "refinement"
    } else {
        "core approach"
    };
    match judge(
        shared.judge.as_ref(),
        &earlier,
        &idea,
        ep.role == Role::Exploit,
    )
    .await
    {
        Judged::New => {}
        Judged::Same(i) => {
            return Ok(
                json!({"ok": false, "same_as": earlier[i].n, "message": format!("Rejected: the same \
{what} as #{} was claimed first. Claimed so far:\n{listed}Choose a different {what} and claim it.", earlier[i].n)}),
            );
        }
        Judged::Placeholder => {
            return Ok(
                json!({"ok": false, "message": format!("Rejected: this names no concrete \
{what}. A claim is final and is how everyone knows what you built: claim the {what} you will build. \
`mcp.ar.claims{{}}` lists what is taken. Claimed so far:\n{listed}")}),
            );
        }
    }
    let (worker, n) = (ep.label.clone(), earlier.len() as u32 + 1);
    shared
        .append_with(move |_| {
            Ok((
                Record::Claimed(Claim {
                    scope,
                    n,
                    idea,
                    worker,
                }),
                (),
            ))
        })
        .await?;
    Ok(accepted(n))
}

fn accepted(n: u32) -> Value {
    json!({"ok": true, "n": n, "message": format!("Accepted as #{n}. Build it now; mcp.ar.eval works from here on.")})
}

/// What two independent judge calls agree a new claim is.
pub(crate) enum Judged {
    New,
    /// The index of the earlier claim it duplicates: the same core approach for explore ideas, the same change for
    /// refinements of one candidate.
    Same(usize),
    /// No concrete approach at all: a placeholder, a probe or a test of the tool.
    Placeholder,
}

/// Judge a new claim against the earlier ones. Only an answer both calls agree on rejects it; anything else accepts.
async fn judge(judge: Option<&Judge>, earlier: &[Claim], idea: &str, refinement: bool) -> Judged {
    let Some(judge) = judge else {
        return Judged::New;
    };
    let listed: String = if earlier.is_empty() {
        "(none)\n".into()
    } else {
        earlier
            .iter()
            .map(|c| format!("{}. {}\n", c.n, c.idea))
            .collect()
    };
    let prompt = if refinement {
        format!(
            "Several researchers each claim one refinement of the same solution. Claimed so far:\n{listed}\n\
New claim:\n{idea}\n\nIs the new claim the same refinement as one already claimed? Same means the same change to the \
same part of the solution (the same rule, component or parameter, changed the same way); a change to a different part, \
or a different kind of change, is a different refinement even when both touch similar things. In one sentence, name \
what the new claim changes and the closest claimed one. Then end with a line 'Answer: N', where N is the matching \
claim's number, or 0 if it is a different refinement. If the new claim names no concrete change at all (a placeholder, \
a probe or a test of the tool), end with 'Answer: none' instead."
        )
    } else {
        format!(
            "Several researchers each claim one core approach to the same task. Claimed so far:\n{listed}\n\
New claim:\n{idea}\n\nDoes the new claim use the same core approach as one already claimed? Same core means the same main \
mechanism that decides what the solution does (the same kind of model, algorithm or signal); differing only in \
parameters, filters, exits, sizing or added gates still counts as the same, while a different main mechanism is a \
different core even when it shares ingredients. In one sentence, name the new claim's main mechanism and the closest \
claimed one. Then end with a line 'Answer: N', where N is the matching claim's number, or 0 if it is a genuinely \
different core approach. If the new claim names no concrete approach at all (a placeholder, a probe or a test of the \
tool), end with 'Answer: none' instead."
        )
    };
    let request = json!({"system": "You compare research ideas and answer exactly in the format asked.",
        "messages": [{"role": "user", "content": prompt}], "max_tokens": JUDGE_TOKENS});
    let ask = || hx::llm::chat(&judge.http, &judge.cfg, &judge.key, &request);
    let (a, b) = tokio::join!(ask(), ask());
    let pick = |reply: Result<hx::llm::Reply>| reply.ok().and_then(|r| answer_number(&r.text));
    match (pick(a), pick(b)) {
        (Some(a), Some(b)) if a == b && a == usize::MAX => Judged::Placeholder,
        (Some(a), Some(b)) if a == b && (1..=earlier.len()).contains(&a) => Judged::Same(a - 1),
        _ => Judged::New,
    }
}

/// N from the last `Answer: N` in a judge's reply, tolerating markdown such as `**Answer:** #2`; `Answer: none` is
/// `usize::MAX`.
fn answer_number(text: &str) -> Option<usize> {
    let at = text.to_ascii_lowercase().rfind("answer")?;
    let rest = text[at + "answer".len()..].trim_start_matches(['*', ':', ' ', '#', '\t']);
    if rest.to_ascii_lowercase().starts_with("none") {
        return Some(usize::MAX);
    }
    rest.chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()
}

/// `claims`: the claims made so far in this worker's scope, as plain text.
fn listed(shared: &Shared, ep: &Ep) -> Result<Value> {
    let scope = ep.scope("claims")?;
    let listed: String = claims(&shared.journal.records())
        .filter(|c| c.scope == scope)
        .map(|c| format!("{}. {}\n", c.n, c.idea))
        .collect();
    Ok(json!({"message": if listed.is_empty() {
        "No claims yet.".to_string()
    } else {
        format!("Claimed so far:\n{listed}")
    }}))
}

/// `reset`: put this worker's best scored state back into the editable paths (the state it started from when it has
/// not improved on it). Every directory on the way is opened without following a link, since the agent controls the
/// workspace.
async fn reset(shared: &Arc<Shared>, ep: &Arc<Ep>) -> Result<Value> {
    let records = shared.journal.records();
    let snap = ep.best(&records).unwrap_or_else(|| ep.start.clone());
    let (dir, spec, ws, snap_) = (
        shared.dir.clone(),
        shared.spec.clone(),
        ep.ws.clone(),
        snap.clone(),
    );
    tokio::task::spawn_blocking(move || restore(&ws, &spec, &load_snap(&dir, &snap_)?)).await??;
    let best = ep
        .best(&records)
        .and_then(|b| estimate(&records, &b))
        .map(|e| e.mean);
    Ok(json!({"best": best}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn judge_answers() {
        assert_eq!(answer_number("Mechanism: breakout.\nAnswer: 2"), Some(2));
        assert_eq!(answer_number("**Answer:** #3"), Some(3));
        assert_eq!(answer_number("answer: 0"), Some(0));
        assert_eq!(
            answer_number("It names no approach.\n**Answer:** none"),
            Some(usize::MAX)
        );
        assert_eq!(answer_number("I think 2 is closest."), None);
    }

    #[test]
    fn a_retried_worker_holds_its_earlier_attempt_s_claim() {
        let ep = |label: &str| Ep {
            label: label.into(),
            plan: plan_of(label).into(),
            role: Role::Explore,
            idea: 0,
            start: "seed".into(),
            ws: PathBuf::new(),
        };
        let records = [Record::Claimed(Claim {
            scope: "explore".into(),
            n: 4,
            idea: "an idea".into(),
            worker: "r1-w3".into(),
        })];
        assert_eq!(ep("r1-w3.2").claim(&records).map(|c| c.n), Some(4));
        assert!(ep("r1-w2").claim(&records).is_none());
        assert!(ep("r2-w3").claim(&records).is_none());
    }
}
