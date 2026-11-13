//! MCP client: stdio and Streamable HTTP. Stdio servers are user-configured third-party processes, started directly
//! under the run's Landlock file policy in their own process group, like shell commands; the group is killed when the
//! connection is dropped or hx is stopped. HTTP servers are remote; the `url` in the user-owned mcp.toml is the
//! consent to reach them.
//!
//! `.harness/mcp.toml` (user-owned; file tools cannot write `.harness/`; an unknown key is an error):
//!   [server.NAME]
//!   command = "/path/to/server"        args = ["--flag"]
//!   env = ["SOME_VAR"]                 # env vars passed through from the harness (default: none, so no secrets)
//!   read_only_tools = ["read_*", "list_*", "get_*", "search_*"]   # called without approval; everything else is an
//!                                                                  # external effect that goes through the approval gate
//!
//!   [server.REMOTE]                    # Streamable HTTP instead of a process (`command` xor `url`)
//!   url = "https://host/mcp"           # https only (http for loopback); redirects are not followed
//!   headers_env = { "x-api-key" = "SOME_VAR" }   # header name -> env var holding the value (never written in the file)
//!   bearer_env = "SOME_VAR"            # shorthand for Authorization: Bearer $SOME_VAR
//!   read_only_tools = [...]            # args/env do not apply to a remote server and are rejected
//!   For remote servers, the approval gate and recorded effects are the controls. No OAuth.
use crate::net::{self, BodyError, Delivery, ResponseBudget, Sse};
use crate::policy::Policy;
use crate::prims::GroupKill;
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

#[derive(Deserialize, serde::Serialize, Debug, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers_env: BTreeMap<String, String>,
    #[serde(default)]
    pub bearer_env: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub read_only_tools: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    server: BTreeMap<String, ServerCfg>,
}

pub fn snapshot(text: Option<&[u8]>) -> Result<BTreeMap<String, ServerCfg>> {
    let Some(text) = text else { return Ok(BTreeMap::new()) };
    let file: File = toml::from_str(std::str::from_utf8(text)?).map_err(|e| anyhow!(".harness/mcp.toml: {e}"))?;
    for (name, cfg) in &file.server { validate(name, cfg)?; }
    Ok(file.server)
}

/// The largest response hx reads for one request, across stdio frames, HTTP bodies, SSE events and error bodies.
const MAX_BODY: usize = 16 << 20;
/// How long one request may wait for its response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The most `tools/list` pages read from one server.
const MAX_LIST_PAGES: usize = 64;

/// A failed MCP request: how far it got, and whether the connection must be dropped so the next call starts afresh.
#[derive(Debug)]
pub struct McpError {
    pub delivery: Delivery,
    reset: bool,
    message: String,
}

impl McpError {
    /// Nothing reached the server, or the server rejected it unprocessed. The connection is not reused.
    fn not_sent(message: impl Into<String>) -> Self { McpError { delivery: Delivery::NotSent, reset: true, message: message.into() } }
    /// The server answered with an error; the connection stays usable.
    fn answered(message: impl Into<String>) -> Self { McpError { delivery: Delivery::Answered, reset: false, message: message.into() } }
    /// The request went out and its response is lost; the stream is out of step, so the connection is dropped.
    fn lost(message: impl Into<String>) -> Self { McpError { delivery: Delivery::Lost, reset: true, message: message.into() } }

    fn from_net(error: net::Error) -> Self { McpError { delivery: error.delivery, reset: true, message: error.message } }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(&self.message) }
}

impl std::error::Error for McpError {}

type McpResult<T> = std::result::Result<T, McpError>;

/// A lost or oversized response after the request went out.
fn body_lost(error: BodyError) -> McpError { McpError::lost(format!("MCP {error}")) }

/// One JSON-RPC message from the server, as it relates to the request with id `id`.
enum Incoming {
    /// The response to our request.
    Response(Value),
    /// A request from the server, which must be answered or a compliant server waits for it.
    Request { id: Value, method: String },
    /// A notification, a response to something else, or noise.
    Other,
}

fn incoming(v: Value, id: u64) -> Incoming {
    match (v.get("method").and_then(Value::as_str), v.get("id")) {
        (Some(method), Some(server_id)) if !server_id.is_null() => Incoming::Request { id: server_id.clone(), method: method.to_string() },
        (None, Some(response_id)) if response_id.as_u64() == Some(id) => Incoming::Response(v),
        _ => Incoming::Other,
    }
}

/// hx's answer to a server request: `ping` succeeds; hx offers no other client capability (sampling, roots,
/// elicitation), so everything else is "method not found".
fn answer(id: Value, method: &str) -> Value {
    if method == "ping" {
        json!({"jsonrpc": "2.0", "id": id, "result": {}})
    } else {
        json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}})
    }
}

/// A response -> its result, or the server's error as a definite failure.
fn result(v: Value, scrub: impl Fn(String) -> String) -> McpResult<Value> {
    match v.get("error") {
        Some(e) => Err(McpError::answered(scrub(format!("MCP error: {}", e["message"].as_str().unwrap_or("unknown"))))),
        None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
    }
}

struct Stdio {
    /// Kills the server's whole process group when dropped (a lost connection, a timeout, hx stopping). Declared
    /// before `child` so it drops first, while the group leader is not yet reaped and the group id cannot be reused.
    _group: Option<GroupKill>,
    /// Killed and reaped when dropped.
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
    /// How long one request may take, from the first byte written to its response (`REQUEST_TIMEOUT`).
    deadline: Duration,
}

impl Stdio {
    /// Write one frame. A failed write or flush means the server is gone (its stdin is closed): the frame was not read.
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        self.stdin.write_all((msg.to_string() + "\n").as_bytes()).await?;
        self.stdin.flush().await
    }

    async fn request(&mut self, method: &str, params: Value) -> McpResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        // A write into the pipe of a server that already exited can still succeed, so ask the kernel first.
        if let Ok(Some(status)) = self.child.try_wait() {
            return Err(McpError::not_sent(format!("MCP server is not running ({status})")));
        }
        let deadline = self.deadline;
        // The write is inside the deadline: a server that stops reading its stdin (blocked writing its stdout, say)
        // must not hold this call, and the server's lock, forever once the frame outgrows the pipe buffer.
        let work = async {
            self.send(&msg).await.map_err(|e| McpError::not_sent(format!("MCP server is not running ({e})")))?;
            let mut budget = ResponseBudget::new(MAX_BODY);
            loop {
                let line = budget.line(&mut self.out).await.map_err(body_lost)?;
                if line.is_empty() {
                    return Err(McpError::lost(format!("MCP server exited while handling {method}")));
                }
                let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
                match incoming(v, id) {
                    Incoming::Response(v) => return result(v, |m| m),
                    Incoming::Request { id: server_id, method: server_method } => {
                        self.send(&answer(server_id, &server_method)).await
                            .map_err(|e| McpError::lost(format!("MCP server stopped reading while handling {method} ({e})")))?;
                    },
                    Incoming::Other => {},
                }
            }
        };
        // On timeout the frame may have been read in part or whole, so the outcome is unknown; `lost` drops the
        // connection, which kills the server's group.
        tokio::time::timeout(deadline, work).await
            .map_err(|_| McpError::lost(format!("MCP server did not read and answer {method} within {}s", deadline.as_secs())))?
    }

    async fn notify(&mut self, method: &str) -> McpResult<()> {
        let deadline = self.deadline;
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match tokio::time::timeout(deadline, self.send(&msg)).await {
            Ok(sent) => sent.map_err(|e| McpError::not_sent(format!("MCP server is not running ({e})"))),
            Err(_) => Err(McpError::lost(format!("MCP server did not read {method} within {}s", deadline.as_secs()))),
        }
    }
}

struct Http {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap, // auth headers, marked sensitive
    secrets: Vec<String>,                // resolved header values, scrubbed from error text
    session: Option<String>,             // Mcp-Session-Id
    version: Option<String>,             // negotiated protocol version, sent on every request after initialize
    next_id: u64,
}

impl Http {
    fn scrub(&self, mut t: String) -> String {
        for s in self.secrets.iter().filter(|s| !s.is_empty()) {
            t = t.replace(s.as_str(), "***");
        }
        t
    }

    fn post(&self, msg: &Value) -> reqwest::RequestBuilder {
        let mut req = self.client.post(&self.url).headers(self.headers.clone()).header("accept", "application/json, text/event-stream").json(msg);
        if let Some(s) = &self.session {
            req = req.header("mcp-session-id", s);
        }
        if let Some(v) = &self.version {
            req = req.header("mcp-protocol-version", v);
        }
        req
    }

    async fn request(&mut self, method: &str, params: Value) -> McpResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let v = self.exchange(&msg, Some(id)).await?.ok_or_else(|| McpError::lost(format!("MCP server sent no response to {method}")))?;
        result(v, |m| self.scrub(m))
    }

    async fn notify(&mut self, method: &str) -> McpResult<()> {
        self.exchange(&json!({"jsonrpc": "2.0", "method": method}), None).await.map(|_| ())
    }

    /// POST one JSON-RPC message. With `want` = the request id, returns the matching response (from a JSON body or an
    /// SSE stream, answering server requests on the way); without it (a notification or an answer) returns None.
    async fn exchange(&mut self, msg: &Value, want: Option<u64>) -> McpResult<Option<Value>> {
        let work = async {
            let mut resp = self.post(msg).send().await.map_err(|e| {
                let mut error = McpError::from_net(net::Error::send("MCP http request failed", e));
                error.message = self.scrub(error.message);
                error
            })?;
            let mut budget = ResponseBudget::new(MAX_BODY);
            let status = resp.status();
            if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                self.session = Some(s.to_string());
            }
            if status == reqwest::StatusCode::NOT_FOUND && self.session.is_some() {
                // The session is gone and the server did not process the request: start a new session.
                return Err(McpError::not_sent("MCP session expired (HTTP 404)"));
            }
            if !status.is_success() {
                let body = match budget.body(&mut resp).await {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(e) => format!("({e})"),
                };
                let body: String = self.scrub(body).chars().take(200).collect();
                let hint = if status.is_redirection() { " (redirects are not followed)" } else if status.as_u16() == 401 || status.as_u16() == 403 { " (check the credentials)" } else { "" };
                let message = self.scrub(format!("MCP server returned HTTP {}{hint}: {body}", status.as_u16()));
                return Err(McpError { delivery: Delivery::of_status(status), reset: false, message });
            }
            let Some(id) = want else { return Ok(None) };
            let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
            if ctype.starts_with("text/event-stream") {
                let mut sse = Sse::default();
                loop {
                    let chunk = resp.chunk().await.map_err(|e| McpError::lost(self.scrub(format!("MCP stream failed: {}", e.without_url()))))?;
                    let events = match &chunk {
                        Some(bytes) => {
                            budget.consume(bytes.len()).map_err(body_lost)?;
                            sse.push(bytes)
                        },
                        // the stream may end without the final blank line: try what is left
                        None => sse.finish(),
                    };
                    for data in events {
                        let Ok(v) = serde_json::from_str::<Value>(&data) else { continue };
                        match incoming(v, id) {
                            Incoming::Response(v) => return Ok(Some(v)),
                            Incoming::Request { id: server_id, method } => self.reply(answer(server_id, &method)).await?,
                            Incoming::Other => {},
                        }
                    }
                    if chunk.is_none() {
                        return Err(McpError::lost("MCP stream ended without a response"));
                    }
                }
            }
            let bytes = budget.body(&mut resp).await.map_err(body_lost)?;
            let v: Value = serde_json::from_slice(&bytes).map_err(|_| McpError::lost("MCP server sent an unreadable response"))?;
            match incoming(v, id) {
                Incoming::Response(v) => Ok(Some(v)),
                _ => Err(McpError::lost("MCP server answered a different request id")),
            }
        };
        tokio::time::timeout(REQUEST_TIMEOUT, work).await
            .map_err(|_| McpError::lost(format!("MCP server did not answer within {}s", REQUEST_TIMEOUT.as_secs())))?
    }

    /// Answer a server request made inside a response stream. Our own request is already out, so a failure here
    /// leaves its outcome unknown.
    async fn reply(&self, msg: Value) -> McpResult<()> {
        let resp = self.post(&msg).send().await.map_err(|e| McpError::lost(self.scrub(format!("MCP reply to a server request failed: {}", e.without_url()))))?;
        if !resp.status().is_success() {
            return Err(McpError::lost(format!("MCP server refused hx's reply to its request (HTTP {})", resp.status().as_u16())));
        }
        Ok(())
    }
}

enum Conn {
    Stdio(Stdio),
    Http(Http),
}

impl Conn {
    async fn request(&mut self, method: &str, params: Value) -> McpResult<Value> {
        match self {
            Conn::Stdio(s) => s.request(method, params).await,
            Conn::Http(h) => h.request(method, params).await,
        }
    }

    async fn notify(&mut self, method: &str) -> McpResult<()> {
        match self {
            Conn::Stdio(s) => s.notify(method).await,
            Conn::Http(h) => h.notify(method).await,
        }
    }
}

fn validate(name: &str, c: &ServerCfg) -> Result<()> {
    let Some(u) = &c.url else {
        if c.command.is_empty() {
            bail!("MCP server {name}: set `command` (stdio) or `url` (HTTP)");
        }
        if !c.headers_env.is_empty() || c.bearer_env.is_some() {
            bail!("MCP server {name}: headers_env/bearer_env only apply to a `url` server");
        }
        return Ok(());
    };
    if !c.command.is_empty() {
        bail!("MCP server {name}: `command` and `url` are mutually exclusive");
    }
    if !c.args.is_empty() || !c.env.is_empty() {
        bail!("MCP server {name}: args/env do not apply to a `url` server");
    }
    let url = reqwest::Url::parse(u).map_err(|e| anyhow!("MCP server {name}: bad url: {e}"))?;
    if !(url.scheme() == "https" || (url.scheme() == "http" && net::is_loopback(&url))) {
        bail!("MCP server {name}: url must be https (http only for localhost)");
    }
    Ok(())
}

/// The configured servers of one VM. Each server has its own connection slot, so one server starting up (up to the
/// request timeout) never delays calls to another; calls to one server are serialized.
pub struct Mcp {
    pub servers: BTreeMap<String, ServerCfg>,
    pol: Arc<Policy>,
    conns: BTreeMap<String, Mutex<Option<Conn>>>,
}

/// `*` wildcards only: `read_*`, `*_file`, `get_*_info`.
pub fn glob(pat: &str, s: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == s;
    }
    let (mut pos, last) = (0usize, parts.len() - 1);
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            if !s.starts_with(part) {
                return false;
            }
            pos = part.len();
        } else if i == last {
            return s.len() >= pos + part.len() && s.ends_with(part);
        } else {
            match s[pos..].find(part) {
                Some(f) => pos += f + part.len(),
                None => return false,
            }
        }
    }
    true
}

impl Mcp {
    pub fn frozen(pol: Arc<Policy>, servers: BTreeMap<String, ServerCfg>) -> Result<Option<Arc<Mcp>>> {
        for (name, cfg) in &servers { validate(name, cfg)?; }
        let conns = servers.keys().map(|name| (name.clone(), Mutex::new(None))).collect();
        Ok((!servers.is_empty()).then(|| Arc::new(Mcp { servers, pol, conns })))
    }

    pub fn is_read_only(&self, server: &str, tool: &str) -> bool {
        self.servers.get(server).is_some_and(|c| c.read_only_tools.iter().any(|p| glob(p, tool)))
    }

    /// Start the server (or open the HTTP session) and complete the handshake.
    async fn connect(&self, server: &str, cfg: &ServerCfg) -> Result<Conn> {
        let mut conn = if cfg.url.is_some() { Self::connect_http(server, cfg)? } else { self.spawn_stdio(server, cfg)? };
        let version = if matches!(conn, Conn::Http(_)) { "2025-06-18" } else { "2024-11-05" };
        let init = conn.request("initialize", json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "hx", "version": env!("CARGO_PKG_VERSION")}})).await?;
        if let Conn::Http(h) = &mut conn {
            h.version = Some(init["protocolVersion"].as_str().unwrap_or(version).to_string());
        }
        conn.notify("notifications/initialized").await?;
        Ok(conn)
    }

    fn spawn_stdio(&self, server: &str, cfg: &ServerCfg) -> Result<Conn> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args);
        cmd.env_clear().envs(self.pol.env());
        for k in &cfg.env {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.current_dir(&self.pol.workspace).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
        // The server gets the run's file policy as it stands at its start, like a shell command, and its own process
        // group, so a wrapper (`npx`, `uvx`) cannot leave the real server running when the connection is dropped.
        let ruleset = self.pol.confinement().prepare().map_err(|e| anyhow!("MCP server {server}: cannot prepare confinement: {e}"))?;
        crate::prims::confine_command(&mut cmd, ruleset.map(Arc::new));
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start MCP server {server}: {e}"))?;
        let group = GroupKill::new(&child);
        let stdin = child.stdin.take().expect("stdin is piped");
        let out = BufReader::new(child.stdout.take().expect("stdout is piped"));
        Ok(Conn::Stdio(Stdio { _group: group, child, stdin, out, next_id: 1, deadline: REQUEST_TIMEOUT }))
    }

    /// Auth headers are resolved from the environment here, in Rust: the model and Luau never see them.
    fn connect_http(server: &str, cfg: &ServerCfg) -> Result<Conn> {
        use reqwest::header::{HeaderName, HeaderValue};
        let url = cfg.url.clone().ok_or_else(|| anyhow!("MCP server {server}: no url"))?;
        let mut pairs: Vec<(String, String, String)> = cfg.headers_env.iter().map(|(h, v)| (h.clone(), v.clone(), String::new())).collect();
        if let Some(v) = &cfg.bearer_env {
            pairs.push(("authorization".into(), v.clone(), "Bearer ".into()));
        }
        let (mut headers, mut secrets) = (reqwest::header::HeaderMap::new(), vec![]);
        for (name, var, prefix) in pairs {
            let val = std::env::var(&var).map_err(|_| anyhow!("MCP server {server}: env var {var} (for header {name}) is not set"))?;
            let hn = HeaderName::from_bytes(name.as_bytes()).map_err(|_| anyhow!("MCP server {server}: bad header name {name:?}"))?;
            if matches!(hn.as_str(), "accept" | "content-type" | "content-length" | "host" | "mcp-session-id" | "mcp-protocol-version") {
                bail!("MCP server {server}: header {name} is set by the client and cannot be overridden");
            }
            let mut hv = HeaderValue::from_str(&format!("{prefix}{val}")).map_err(|_| anyhow!("MCP server {server}: env var {var} is not a valid header value"))?;
            hv.set_sensitive(true);
            headers.insert(hn, hv);
            secrets.push(val);
        }
        let client = net::client(&reqwest::Url::parse(&url)?).build()?;
        Ok(Conn::Http(Http { client, url, headers, secrets, session: None, version: None, next_id: 1 }))
    }

    /// One request on `server`'s connection, connecting first if there is none. A connection that has failed is
    /// dropped, so the next call starts a fresh server or session.
    async fn request_on(&self, slot: &mut Option<Conn>, server: &str, method: &str, params: Value) -> McpResult<Value> {
        let conn = match slot {
            Some(conn) => conn,
            None => {
                let cfg = self.servers.get(server).ok_or_else(|| McpError::not_sent(format!("no MCP server named {server:?} in .harness/mcp.toml")))?;
                let conn = self.connect(server, cfg).await.map_err(|e| McpError::not_sent(format!("MCP server {server} unavailable: {e}")))?;
                slot.insert(conn)
            },
        };
        let result = conn.request(method, params).await;
        if result.as_ref().is_err_and(|e| e.reset) {
            *slot = None;
        }
        result
    }

    pub async fn request(&self, server: &str, method: &str, params: Value) -> McpResult<Value> {
        let slot = self.conns.get(server).ok_or_else(|| McpError::not_sent(format!("no MCP server named {server:?} in .harness/mcp.toml")))?;
        let mut slot = slot.lock().await;
        let reused = slot.is_some();
        match self.request_on(&mut slot, server, method, params.clone()).await {
            // A cached connection that turned out dead (the server exited, the session expired) never saw the
            // request, so it is safe to send it once more on a fresh one.
            Err(e) if reused && e.delivery == Delivery::NotSent => self.request_on(&mut slot, server, method, params).await,
            result => result,
        }
    }

    /// Every tool `server` offers, following `nextCursor` pagination.
    pub async fn list_tools(&self, server: &str) -> McpResult<Vec<Value>> {
        let (mut tools, mut cursor) = (Vec::new(), None::<String>);
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor { Some(c) => json!({"cursor": c}), None => json!({}) };
            let page = self.request(server, "tools/list", params).await?;
            tools.extend(page["tools"].as_array().into_iter().flatten().cloned());
            match page.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() && cursor.as_deref() != Some(next) => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }
        Err(McpError::answered(format!("MCP server {server} lists more than {MAX_LIST_PAGES} pages of tools")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    async fn chunked_http(status: u16, content_type: &str, body: Vec<u8>) -> McpResult<Value> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let headers = format!("HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(socket);
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                if line == "\r\n" { break; }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).await.unwrap();
            let mut socket = reader.into_inner();
            socket.write_all(headers.as_bytes()).await.unwrap();
            for chunk in body.chunks(8192) {
                if socket.write_all(format!("{:x}\r\n", chunk.len()).as_bytes()).await.is_err()
                    || socket.write_all(chunk).await.is_err() || socket.write_all(b"\r\n").await.is_err() { return; }
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        });
        let client = net::client(&reqwest::Url::parse(&url).unwrap()).build().unwrap();
        let mut http = Http { client, url, headers: Default::default(), secrets: vec![], session: None, version: None, next_id: 1 };
        let result = http.request("tools/list", json!({})).await;
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
        result
    }

    #[tokio::test]
    async fn a_server_that_stops_reading_cannot_block_a_request_past_its_deadline() {
        // `sleep` never reads its stdin: a frame larger than the pipe buffer blocks the write.
        let mut child = Command::new("sleep").arg("1000").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
            .kill_on_drop(true).spawn().unwrap();
        let (stdin, out) = (child.stdin.take().unwrap(), BufReader::new(child.stdout.take().unwrap()));
        let mut stdio = Stdio { _group: None, child, stdin, out, next_id: 1, deadline: Duration::from_secs(1) };
        let call = stdio.request("tools/call", json!({"name": "put", "arguments": {"text": "x".repeat(300_000)}}));
        let error = tokio::time::timeout(Duration::from_secs(20), call).await.expect("the request outlived its deadline").unwrap_err();
        assert_eq!((error.delivery, error.reset), (Delivery::Lost, true), "{error}");
    }

    fn too_large(error: &McpError) -> bool { error.delivery == Delivery::Lost && error.reset && error.to_string().contains("too large") }

    #[tokio::test]
    async fn chunked_json_and_error_bodies_are_bounded_before_decoding() {
        assert_eq!(chunked_http(200, "application/json", br#"{"id":1,"result":{"ok":true}}"#.to_vec()).await.unwrap(), json!({"ok":true}));
        let error = chunked_http(200, "application/json", vec![b' '; MAX_BODY + 1]).await.unwrap_err();
        assert!(too_large(&error), "{error}");
        // An error status settles the outcome however much of its body arrives; the body is cut off, not trusted.
        let error = chunked_http(400, "application/json", vec![b' '; MAX_BODY + 1]).await.unwrap_err();
        assert!(error.delivery == Delivery::Answered && error.to_string().contains("too large"), "{error}");
    }

    #[tokio::test]
    async fn sse_notifications_cannot_reset_the_response_allowance() {
        let mut body = Vec::new();
        let event = format!("data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"padding\":\"{}\"}}\n\n", "x".repeat(8192));
        while body.len() <= MAX_BODY { body.extend_from_slice(event.as_bytes()); }
        body.extend_from_slice(b"data: {\"id\":1,\"result\":{}}\n\n");
        let error = chunked_http(200, "text/event-stream", body).await.unwrap_err();
        assert!(too_large(&error), "{error}");
    }

    #[tokio::test]
    async fn a_large_sse_response_is_read_in_linear_time() {
        let text = "x".repeat(15 << 20);
        let body = format!("data: {}\n\n", json!({"jsonrpc": "2.0", "id": 1, "result": {"text": text}})).into_bytes();
        let started = std::time::Instant::now();
        let result = chunked_http(200, "text/event-stream", body).await.unwrap();
        assert_eq!(result["text"].as_str().map(str::len), Some(text.len()));
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_json_rpc_error_is_a_definite_answer_and_keeps_the_connection() {
        let error = chunked_http(200, "application/json", br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Invalid params"}}"#.to_vec()).await.unwrap_err();
        assert_eq!((error.delivery, error.reset), (Delivery::Answered, false));
        assert_eq!(error.to_string(), "MCP error: Invalid params");
        for status in [401, 503] {
            assert_eq!(chunked_http(status, "text/plain", b"no".to_vec()).await.unwrap_err().delivery, Delivery::Answered, "{status}");
        }
        assert_eq!(chunked_http(502, "text/plain", b"upstream".to_vec()).await.unwrap_err().delivery, Delivery::Lost);
    }

    #[test]
    fn server_requests_are_told_apart_from_our_response() {
        let ping = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        assert!(matches!(incoming(ping, 1), Incoming::Request { ref id, ref method } if *id == json!(1) && method == "ping"));
        assert!(matches!(incoming(json!({"jsonrpc": "2.0", "method": "notifications/progress"}), 1), Incoming::Other));
        assert!(matches!(incoming(json!({"jsonrpc": "2.0", "id": 2, "result": {}}), 1), Incoming::Other));
        assert!(matches!(incoming(json!({"jsonrpc": "2.0", "id": 1, "result": {}}), 1), Incoming::Response(_)));
        assert_eq!(answer(json!("s1"), "ping"), json!({"jsonrpc": "2.0", "id": "s1", "result": {}}));
        assert_eq!(answer(json!(4), "sampling/createMessage")["error"]["code"], -32601);
    }

    #[test]
    fn unknown_config_keys_are_rejected() {
        let error = toml::from_str::<File>("[server.web]\nurl = \"https://example.com/mcp\"\nbearer-env = \"KEY\"\n").err().expect("typo accepted");
        assert!(error.to_string().contains("bearer-env"), "{error}");
        assert!(toml::from_str::<File>("[servers.web]\nurl = \"https://example.com/mcp\"\n").is_err());
        assert!(toml::from_str::<File>("[server.web]\nurl = \"https://example.com/mcp\"\nbearer_env = \"KEY\"\n").is_ok());
    }

    #[test]
    fn glob_matches_only_where_wildcards_allow() {
        assert!(glob("read_*", "read_file"));
        assert!(!glob("read_*", "write_file"));
        assert!(glob("*_file", "read_file"));
        assert!(!glob("*_file", "read_files"));
        assert!(glob("get_*_info", "get_user_info"));
        assert!(!glob("get_*_info", "get_info"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exact2"));
        assert!(!glob("a*a", "a"));
    }
}
