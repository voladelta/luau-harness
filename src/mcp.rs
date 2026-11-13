//! MCP client: stdio and Streamable HTTP. Stdio servers are user-configured third-party processes, launched directly.
//! Process isolation is supplied externally. HTTP servers are remote; the `url` in the user-owned mcp.toml is the consent to reach them.
//!
//! `.harness/mcp.toml` (user-owned; file tools cannot write `.harness/`):
//!   [server.NAME]
//!   command = "/path/to/server"        args = ["--flag"]
//!   env = ["SOME_VAR"]                 # env vars passed through from the harness (default: none, so no secrets)
//!   read_only_tools = ["read_*", "list_*", "get_*", "search_*"]   # called without approval; everything else is an
//!                                                                  # external effect that goes through the approval gate
//!
//!   [server.REMOTE]                    # Streamable HTTP instead of a process (`command` xor `url`)
//!   url = "https://host/mcp"           # https only (http for localhost); redirects are not followed
//!   headers_env = { "x-api-key" = "SOME_VAR" }   # header name -> env var holding the value (never written in the file)
//!   bearer_env = "SOME_VAR"            # shorthand for Authorization: Bearer $SOME_VAR
//!   read_only_tools = [...]            # args/env do not apply to a remote server and are rejected
//!   For remote servers, the approval gate and recorded effects are the controls. No OAuth.
use crate::policy::Policy;
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

#[derive(Deserialize, serde::Serialize, Debug, Clone, Default)]
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
struct File {
    #[serde(default)]
    server: BTreeMap<String, ServerCfg>,
}

pub fn snapshot(root: &std::path::Path) -> Result<BTreeMap<String, ServerCfg>> {
    let path = root.join(".harness/mcp.toml");
    if !path.exists() { return Ok(BTreeMap::new()); }
    let file: File = toml::from_str(&std::fs::read_to_string(path)?)?;
    for (name, cfg) in &file.server { validate(name, cfg)?; }
    Ok(file.server)
}

/// The server process is gone or stopped answering: the cached connection must be dropped. `sent` says whether the
/// request may have reached the server: a lost answer after sending is uncertain, a failure to send is not.
#[derive(Debug)]
struct ConnLost { message: String, sent: bool }
impl std::fmt::Display for ConnLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for ConnLost {}

/// The request may have reached the server, which then gave no definite answer.
fn lost(message: impl Into<String>) -> anyhow::Error { anyhow::Error::new(ConnLost { message: message.into(), sent: true }) }
/// The connection failed before the request could reach the server.
fn unsent(message: impl Into<String>) -> anyhow::Error { anyhow::Error::new(ConnLost { message: message.into(), sent: false }) }

/// Did a failed request possibly act? Only when it may have reached the server without a definite answer; a JSON-RPC
/// error, an HTTP 4xx and every failure before sending are definite.
pub fn may_have_acted(error: &anyhow::Error) -> bool {
    error.downcast_ref::<ConnLost>().is_some_and(|c| c.sent)
}

struct Stdio {
    child: Child, // killed when dropped
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
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

enum Conn {
    Stdio(Stdio),
    Http(Http),
}

fn is_local(url: &reqwest::Url) -> bool {
    matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
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
    if !(url.scheme() == "https" || (url.scheme() == "http" && is_local(&url))) {
        bail!("MCP server {name}: url must be https (http only for localhost)");
    }
    Ok(())
}

pub struct Mcp {
    pub servers: BTreeMap<String, ServerCfg>,
    pol: Arc<Policy>,
    /// One slot per server, locked across its spawn and handshake, so a slow server stalls only its own callers.
    /// None: not connected (never, or the connection was lost); the next call connects afresh.
    conns: std::sync::Mutex<HashMap<String, Arc<Mutex<Option<Conn>>>>>,
}

/// Splits an SSE byte stream into events (each terminated by a blank line). The scan for a boundary resumes where the
/// last one stopped, so a large event arriving in small chunks costs linear time, not a rescan per chunk.
#[derive(Default)]
struct SseEvents { buf: Vec<u8>, scanned: usize }

impl SseEvents {
    fn next_event(&mut self) -> Option<Vec<u8>> {
        // A boundary may straddle the previous scan's end: back up by its longest form's length minus one.
        let from = self.scanned.saturating_sub(3);
        let end = (from..self.buf.len()).find_map(|i| {
            if self.buf[i..].starts_with(b"\n\n") {
                Some(i + 2)
            } else if self.buf[i..].starts_with(b"\r\n\r\n") {
                Some(i + 4)
            } else {
                None
            }
        });
        match end {
            Some(end) => { self.scanned = 0; Some(self.buf.drain(..end).collect()) },
            None => { self.scanned = self.buf.len(); None },
        }
    }
}

const MAX_BODY: usize = 16 << 20;
/// `tools/list` pages followed per server before the listing is reported as truncated.
pub const MAX_LIST_PAGES: usize = 20;

/// One byte allowance for the whole response, including ignored notifications and HTTP error bodies.
/// Check before appending; neither a missing Content-Length nor draining SSE events resets the allowance.
struct ResponseBudget { remaining: usize }

impl ResponseBudget {
    fn new() -> Self { Self { remaining: MAX_BODY } }

    fn consume(&mut self, bytes: usize) -> Result<()> {
        self.remaining = self.remaining.checked_sub(bytes)
            .ok_or_else(|| lost("MCP response too large"))?;
        Ok(())
    }

    fn append(&mut self, buffer: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
        self.consume(bytes.len())?;
        buffer.extend_from_slice(bytes);
        Ok(())
    }

    async fn line(&mut self, reader: &mut (impl AsyncBufRead + Unpin)) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        // One sentinel byte distinguishes an exact-sized frame from an oversized, newline-free stream.
        reader.take(self.remaining as u64 + 1).read_until(b'\n', &mut bytes).await
            .map_err(|e| lost(format!("MCP stdio read failed: {e}")))?;
        self.consume(bytes.len())?;
        Ok(bytes)
    }

    async fn body(&mut self, response: &mut reqwest::Response) -> Result<Vec<u8>> {
        if response.content_length().is_some_and(|n| n > self.remaining as u64) {
            return Err(lost("MCP response too large"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await
            .map_err(|e| lost(format!("MCP response read failed: {}", e.without_url())))? {
            self.append(&mut bytes, &chunk)?;
        }
        Ok(bytes)
    }
}

/// The JSON-RPC response with this id among the SSE events in `buf` (server requests and notifications are skipped).
fn sse_response(buf: &[u8], id: u64) -> Option<Value> {
    let text = String::from_utf8_lossy(buf).replace("\r\n", "\n");
    text.split("\n\n").find_map(|ev| {
        let data: Vec<&str> = ev.lines().filter_map(|l| l.strip_prefix("data:")).map(|d| d.strip_prefix(' ').unwrap_or(d)).collect();
        let v = serde_json::from_str::<Value>(&data.join("\n")).ok()?;
        (v.get("method").is_none() && v["id"].as_u64() == Some(id)).then_some(v)
    })
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
        Ok((!servers.is_empty()).then(|| Arc::new(Mcp { servers, pol, conns: Default::default() })))
    }
    pub fn is_read_only(&self, server: &str, tool: &str) -> bool {
        self.servers.get(server).is_some_and(|c| c.read_only_tools.iter().any(|p| glob(p, tool)))
    }

    /// Start a server (or open an HTTP client) and complete the handshake.
    async fn connect(&self, server: &str, cfg: &ServerCfg) -> Result<Conn> {
        let mut c = if cfg.url.is_some() { Self::connect_http(server, cfg)? } else { self.spawn_stdio(server, cfg)? };
        let version = if matches!(c, Conn::Http(_)) { "2025-06-18" } else { "2024-11-05" };
        let init = Self::request_on(&mut c, "initialize", json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "hx", "version": "0.1"}})).await?;
        if let Conn::Http(h) = &mut c {
            h.version = Some(init["protocolVersion"].as_str().unwrap_or(version).to_string());
        }
        Self::notify_on(&mut c, "notifications/initialized").await?;
        Ok(c)
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
        // The server gets the run's file policy as it stands at its start, like a shell command.
        if let Some(ruleset) = self.pol.confinement().prepare().map_err(|e| anyhow!("MCP server {server}: cannot prepare confinement: {e}"))? {
            unsafe { cmd.pre_exec(move || ruleset.apply()); }
        }
        let mut child = cmd.spawn().map_err(|e| anyhow!("cannot start MCP server {server}: {e}"))?;
        let stdin = child.stdin.take().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Ok(Conn::Stdio(Stdio { child, stdin, out, next_id: 1 }))
    }

    /// Auth headers are resolved from the environment here, in Rust: the model and Luau never see them.
    fn connect_http(server: &str, cfg: &ServerCfg) -> Result<Conn> {
        use reqwest::header::{HeaderName, HeaderValue};
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
        // no redirects: reqwest would forward a custom auth header (x-api-key) to another host
        let mut client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never());
        if cfg.url.as_deref().is_some_and(|u| u.starts_with("http://")) {
            client = client.no_proxy(); // localhost only (validated): never hand the key to a proxy in cleartext
        }
        let client = client.build()?;
        Ok(Conn::Http(Http { client, url: cfg.url.clone().unwrap(), headers, secrets, session: None, version: None, next_id: 1 }))
    }

    async fn request_on(c: &mut Conn, method: &str, params: Value) -> Result<Value> {
        match c {
            Conn::Stdio(s) => Self::stdio_request(s, method, params).await,
            Conn::Http(h) => Self::http_request(h, method, params).await,
        }
    }

    async fn notify_on(c: &mut Conn, method: &str) -> Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method});
        match c {
            Conn::Stdio(s) => Self::stdio_send(s, &(msg.to_string() + "\n")).await,
            Conn::Http(h) => Self::http_post(h, &msg, None).await.map(|_| ()),
        }
    }

    /// Write one message. A server that has exited, or whose stdin is closed (EPIPE), never received it: the connection
    /// is lost, but nothing was sent.
    async fn stdio_send(c: &mut Stdio, line: &str) -> Result<()> {
        if let Ok(Some(status)) = c.child.try_wait() {
            return Err(unsent(format!("MCP server exited ({status})")));
        }
        c.stdin.write_all(line.as_bytes()).await.map_err(|e| unsent(format!("MCP stdio write failed: {e}")))?;
        c.stdin.flush().await.map_err(|e| unsent(format!("MCP stdio write failed: {e}")))
    }

    async fn stdio_request(c: &mut Stdio, method: &str, params: Value) -> Result<Value> {
        let id = c.next_id;
        c.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string() + "\n";
        let work = async {
            Self::stdio_send(c, &line).await?;
            let mut budget = ResponseBudget::new();
            loop {
                let buf = budget.line(&mut c.out).await?;
                if buf.is_empty() {
                    return Err(lost(format!("MCP server exited while handling {method}")));
                }
                let Ok(v) = serde_json::from_slice::<Value>(&buf) else { continue };
                if v["id"].as_u64() != Some(id) {
                    continue; // notifications and server->client requests are ignored
                }
                if let Some(e) = v.get("error") {
                    bail!("MCP error {}: {}", e["code"], e["message"].as_str().unwrap_or("unknown"));
                }
                return Ok(v["result"].clone());
            }
        };
        tokio::time::timeout(Duration::from_secs(60), work).await
            .map_err(|_| lost(format!("MCP server did not answer {method} within 60s")))?
    }

    async fn http_request(h: &mut Http, method: &str, params: Value) -> Result<Value> {
        let id = h.next_id;
        h.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let v = Self::http_post(h, &msg, Some(id)).await?.ok_or_else(|| lost(format!("MCP server sent no response to {method}")))?;
        if let Some(e) = v.get("error") {
            bail!("MCP error {}: {}", e["code"], Self::scrub(h, e["message"].as_str().unwrap_or("unknown").to_string()));
        }
        // A server can echo the auth header it was sent; results are journaled and shown to the model.
        let mut result = v["result"].clone();
        Self::scrub_value(h, &mut result);
        Ok(result)
    }

    fn scrub(h: &Http, mut t: String) -> String {
        for s in h.secrets.iter().filter(|s| !s.is_empty()) {
            t = t.replace(s.as_str(), "***");
        }
        t
    }

    fn scrub_value(h: &Http, value: &mut Value) {
        match value {
            Value::String(s) => *s = Self::scrub(h, std::mem::take(s)),
            Value::Array(items) => items.iter_mut().for_each(|v| Self::scrub_value(h, v)),
            Value::Object(fields) => {
                *fields = std::mem::take(fields).into_iter().map(|(k, mut v)| { Self::scrub_value(h, &mut v); (Self::scrub(h, k), v) }).collect();
            },
            _ => {},
        }
    }

    /// POST one JSON-RPC message. With `want` = the request id, returns the matching response (from a JSON body or an
    /// SSE stream); without it (a notification) returns None.
    async fn http_post(h: &mut Http, msg: &Value, want: Option<u64>) -> Result<Option<Value>> {
        let gone = |h: &Http, t: String, sent: bool| anyhow::Error::new(ConnLost { message: Self::scrub(h, t), sent });
        let lost = |h: &Http, t: String| gone(h, t, true);
        let mut req = h.client.post(&h.url).headers(h.headers.clone()).header("accept", "application/json, text/event-stream").json(msg);
        if let Some(s) = &h.session {
            req = req.header("mcp-session-id", s);
        }
        if let Some(v) = &h.version {
            req = req.header("mcp-protocol-version", v);
        }
        let work = async {
            // A connection that was never made carried no request.
            let mut resp = req.send().await.map_err(|e| { let sent = !e.is_connect(); gone(h, format!("MCP http request failed: {}", e.without_url()), sent) })?;
            let mut budget = ResponseBudget::new();
            let status = resp.status();
            if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                h.session = Some(s.to_string());
            }
            if status == reqwest::StatusCode::NOT_FOUND && h.session.is_some() {
                // The server rejected the session before handling the request; a new session can send it again.
                return Err(gone(h, "MCP session expired (HTTP 404)".into(), false));
            }
            if !status.is_success() {
                let bytes = budget.body(&mut resp).await?;
                let body: String = Self::scrub(h, String::from_utf8_lossy(&bytes).into_owned()).chars().take(200).collect();
                let hint = if status.is_redirection() { " (redirects are not followed)" } else if status.as_u16() == 401 || status.as_u16() == 403 { " (check the credentials)" } else { "" };
                let message = Self::scrub(h, format!("MCP server returned HTTP {}{hint}: {body}", status.as_u16()));
                // A refusal (3xx/4xx) is definite; a 5xx, often a gateway giving up, may follow the action.
                if status.is_server_error() { return Err(lost(h, message)); }
                bail!("{message}");
            }
            let Some(id) = want else { return Ok(None) };
            let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
            if ctype.starts_with("text/event-stream") {
                let mut events = SseEvents::default();
                loop {
                    let Some(chunk) = resp.chunk().await.map_err(|e| lost(h, format!("MCP stream failed: {}", e.without_url())))? else {
                        // the stream may end without the final blank line: try what is left
                        events.buf.extend_from_slice(b"\n\n");
                        if let Some(v) = sse_response(&events.buf, id) {
                            return Ok(Some(v));
                        }
                        return Err(lost(h, "MCP stream ended without a response".into()));
                    };
                    budget.append(&mut events.buf, &chunk)?;
                    while let Some(ev) = events.next_event() {
                        if let Some(v) = sse_response(&ev, id) {
                            return Ok(Some(v));
                        }
                    }
                }
            }
            let bytes = budget.body(&mut resp).await?;
            let v: Value = serde_json::from_slice(&bytes)
                .map_err(|_| lost(h, "MCP server sent an unreadable response".into()))?;
            if v.get("method").is_some() || v["id"].as_u64() != Some(id) {
                return Err(lost(h, "MCP server answered a different request id".into()));
            }
            Ok(Some(v))
        };
        tokio::time::timeout(Duration::from_secs(60), work).await.map_err(|_| self::lost("MCP server did not answer within 60s"))?
    }

    /// One request on the server's connection, connecting first if there is none. A lost connection is dropped, so
    /// the next call starts a fresh server (or session). A cached connection found dead before the request was sent
    /// (the server exited, its session expired) is replaced at once and the request sent once on the new one.
    pub async fn request(&self, server: &str, method: &str, params: Value) -> Result<Value> {
        let cfg = self.servers.get(server).ok_or_else(|| anyhow!("no MCP server named {server:?} in .harness/mcp.toml"))?;
        let slot = self.conns.lock().unwrap().entry(server.to_string()).or_default().clone();
        let mut conn = slot.lock().await;
        let mut retried = false;
        loop {
            let fresh = conn.is_none();
            if fresh {
                // The handshake is not this request: whatever happened to it, this request was never sent.
                *conn = Some(self.connect(server, cfg).await.map_err(|e| anyhow!("{e:#}"))?);
            }
            let result = Self::request_on(conn.as_mut().unwrap(), method, params.clone()).await;
            if let Err(e) = &result && let Some(gone) = e.downcast_ref::<ConnLost>() {
                *conn = None;
                if !gone.sent && !fresh && !retried { retried = true; continue; }
            }
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stdio_limit_covers_newline_free_output_and_ignored_frames() {
        let bytes = vec![b'x'; MAX_BODY + 1];
        let mut reader = bytes.as_slice();
        let error = ResponseBudget::new().line(&mut reader).await.unwrap_err();
        assert!(error.is::<ConnLost>());
        assert!(error.to_string().contains("too large"));

        let mut budget = ResponseBudget { remaining: 8 };
        let mut reader = &b"ping\npong\n"[..];
        assert_eq!(budget.line(&mut reader).await.unwrap(), b"ping\n");
        assert!(budget.line(&mut reader).await.unwrap_err().is::<ConnLost>());
        let mut budget = ResponseBudget { remaining: 5 };
        assert_eq!(budget.line(&mut &b"done\n"[..]).await.unwrap(), b"done\n");
    }

    async fn chunked_http(status: u16, content_type: &str, body: Vec<u8>) -> Result<Value> {
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
        let mut http = Http { client: reqwest::Client::builder().no_proxy().build().unwrap(), url,
            headers: Default::default(), secrets: vec![], session: None, version: None, next_id: 1 };
        let result = Mcp::http_request(&mut http, "tools/list", json!({})).await;
        tokio::time::timeout(Duration::from_secs(5), server).await.unwrap().unwrap();
        result
    }

    #[tokio::test]
    async fn chunked_json_and_error_bodies_are_bounded_before_decoding() {
        assert_eq!(chunked_http(200, "application/json", br#"{"id":1,"result":{"ok":true}}"#.to_vec()).await.unwrap(), json!({"ok":true}));
        for status in [200, 502] {
            let error = chunked_http(status, "application/json", vec![b' '; MAX_BODY + 1]).await.unwrap_err();
            assert!(error.is::<ConnLost>() && error.to_string().contains("too large"), "{error}");
        }
    }

    #[tokio::test]
    async fn sse_notifications_cannot_reset_the_response_allowance() {
        let mut body = Vec::new();
        let event = format!("data: {{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"padding\":\"{}\"}}\n\n", "x".repeat(8192));
        while body.len() <= MAX_BODY { body.extend_from_slice(event.as_bytes()); }
        body.extend_from_slice(b"data: {\"id\":1,\"result\":{}}\n\n");
        let error = chunked_http(200, "text/event-stream", body).await.unwrap_err();
        assert!(error.is::<ConnLost>() && error.to_string().contains("too large"), "{error}");
    }

    #[tokio::test]
    async fn only_an_unanswered_sent_request_may_have_acted() {
        let rpc = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"bad arguments"}}"#.to_vec();
        let error = chunked_http(200, "application/json", rpc).await.unwrap_err();
        assert!(!may_have_acted(&error) && error.to_string().contains("-32602"), "{error}");
        assert!(!may_have_acted(&chunked_http(400, "application/json", b"{}".to_vec()).await.unwrap_err()));
        assert!(may_have_acted(&chunked_http(502, "text/plain", b"gateway".to_vec()).await.unwrap_err()));
        assert!(may_have_acted(&chunked_http(200, "application/json", b"{\"id\":1".to_vec()).await.unwrap_err()));
        // Nothing listens on this port: the request never left.
        let port = { let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); l.local_addr().unwrap().port() };
        let mut http = Http { client: reqwest::Client::builder().no_proxy().build().unwrap(), url: format!("http://127.0.0.1:{port}/mcp"),
            headers: Default::default(), secrets: vec![], session: None, version: None, next_id: 1 };
        let error = Mcp::http_request(&mut http, "tools/call", json!({})).await.unwrap_err();
        assert!(error.is::<ConnLost>() && !may_have_acted(&error), "{error}");
    }

    #[test]
    fn sse_boundaries_are_found_across_chunks_without_rescanning() {
        let mut events = SseEvents::default();
        let mut found = vec![];
        for chunk in b"data: a\r\n\r\ndata: b\n\ndata: c".chunks(3) {
            events.buf.extend_from_slice(chunk);
            while let Some(ev) = events.next_event() { found.push(ev); }
        }
        assert_eq!(found, [b"data: a\r\n\r\n".to_vec(), b"data: b\n\n".to_vec()]);
        assert_eq!(events.buf, b"data: c");
        // One 8 MiB event in 4 KiB chunks: a rescan per chunk is quadratic (minutes in a debug build).
        let started = std::time::Instant::now();
        let mut events = SseEvents::default();
        let body = [b"data: ".as_slice(), &vec![b'x'; 8 << 20], b"\n\n"].concat();
        let mut n = 0;
        for chunk in body.chunks(4096) {
            events.buf.extend_from_slice(chunk);
            while events.next_event().is_some() { n += 1; }
        }
        assert_eq!(n, 1);
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    }

    /// A stdio server: answers `quit` and exits, answers `bad` with a JSON-RPC error, waits SLOW seconds before its
    /// handshake, and otherwise returns its pid.
    const SERVER: &str = r#"
import json, os, sys, time
for line in sys.stdin:
    m = json.loads(line)
    if "id" not in m: continue
    if m["method"] == "initialize": time.sleep(float(os.environ.get("SLOW", "0")))
    name = m.get("params", {}).get("name")
    if name == "bad":
        print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "error": {"code": -32602, "message": "bad arguments"}}), flush=True); continue
    print(json.dumps({"jsonrpc": "2.0", "id": m["id"], "result": {"pid": os.getpid()}}), flush=True)
    if name == "quit": sys.exit(0)
"#;

    fn servers(tag: &str, slow: &[&str]) -> (Arc<Mcp>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("hx-mcp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pol = Arc::new(Policy::new(&dir, false, None, None, None).unwrap());
        let cfg = |slow: bool| ServerCfg { command: "python3".into(), args: vec!["-c".into(), if slow { SERVER.replace("\"0\"", "\"2\"") } else { SERVER.into() }], ..Default::default() };
        let servers = ["a", "b"].iter().map(|s| (s.to_string(), cfg(slow.contains(s)))).collect();
        (Mcp::frozen(pol, servers).unwrap().unwrap(), dir)
    }

    #[tokio::test]
    async fn an_exited_stdio_server_is_respawned_and_errors_keep_the_connection() {
        let (mcp, dir) = servers("respawn", &[]);
        let call = |name: &str| mcp.request("a", "tools/call", json!({"name": name, "arguments": {}}));
        let first = call("quit").await.unwrap()["pid"].as_u64();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let second = call("x").await.unwrap()["pid"].as_u64();
        assert!(first.is_some() && second.is_some() && first != second);
        let error = call("bad").await.unwrap_err();
        assert!(!may_have_acted(&error) && error.to_string().contains("bad arguments"), "{error}");
        assert_eq!(call("x").await.unwrap()["pid"].as_u64(), second, "a JSON-RPC error keeps the connection");
        drop(mcp);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_slow_handshake_does_not_stall_other_servers() {
        let (mcp, dir) = servers("slow", &["a"]);
        let started = std::time::Instant::now();
        let (slow, fast) = (mcp.request("a", "tools/list", json!({})), async {
            let r = mcp.request("b", "tools/list", json!({})).await;
            (r, started.elapsed())
        });
        let (slow, (fast, fast_took)) = tokio::join!(slow, fast);
        assert!(slow.is_ok() && fast.is_ok());
        assert!(fast_took < Duration::from_millis(1500) && started.elapsed() >= Duration::from_secs(2), "{fast_took:?}");
        drop(mcp);
        let _ = std::fs::remove_dir_all(dir);
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
