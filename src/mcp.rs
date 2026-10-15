//! MCP client: stdio and Streamable HTTP. Stdio servers are user-configured third-party processes, run under their own
//! Seatbelt profile. HTTP servers are remote; the `url` in the user-owned mcp.toml is the consent to reach them.
//!
//! `.harness/mcp.toml` (user-owned; the model cannot write `.harness/`):
//!   [server.NAME]
//!   command = "/path/to/server"        args = ["--flag"]
//!   env = ["SOME_VAR"]                 # env vars passed through from the harness (default: none, so no secrets)
//!   write = ["/dir/it/may/write"]      # extra writable roots (besides the run's temp dir)
//!   net = false                        # all-or-nothing network, like `sh`
//!   read_only_tools = ["read_*", "list_*", "get_*", "search_*"]   # called without approval; everything else is an
//!                                                                  # external effect that goes through the approval gate
//!
//!   [server.REMOTE]                    # Streamable HTTP instead of a process (`command` xor `url`)
//!   url = "https://host/mcp"           # https only (http for localhost); redirects are not followed
//!   headers_env = { "x-api-key" = "SOME_VAR" }   # header name -> env var holding the value (never written in the file)
//!   bearer_env = "SOME_VAR"            # shorthand for Authorization: Bearer $SOME_VAR
//!   read_only_tools = [...]            # args/env/write/net do not apply to a remote server and are rejected
//!   No sandbox applies to a remote server: the approval gate and recorded effects are the controls. No OAuth.
use crate::policy::Policy;
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    pub write: Vec<String>,
    #[serde(default)]
    pub net: bool,
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

/// The server process is gone or stopped answering: the cached connection must be dropped.
#[derive(Debug)]
struct ConnLost(String);
impl std::fmt::Display for ConnLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConnLost {}

struct Stdio {
    #[allow(dead_code)] // kept alive; killed when dropped
    child: Child,
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
    if !c.args.is_empty() || !c.env.is_empty() || !c.write.is_empty() || c.net {
        bail!("MCP server {name}: args/env/write/net do not apply to a `url` server (it is not a sandboxed process)");
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
    conns: Mutex<HashMap<String, Arc<Mutex<Conn>>>>,
}

/// Index just past the first complete SSE event (terminated by a blank line), if any.
fn sse_event_end(buf: &[u8]) -> Option<usize> {
    (0..buf.len()).find_map(|i| {
        if buf[i..].starts_with(b"\n\n") {
            Some(i + 2)
        } else if buf[i..].starts_with(b"\r\n\r\n") {
            Some(i + 4)
        } else {
            None
        }
    })
}

const MAX_BODY: usize = 16 << 20;

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
        Ok((!servers.is_empty()).then(|| Arc::new(Mcp { servers, pol, conns: Mutex::new(HashMap::new()) })))
    }
    pub fn is_read_only(&self, server: &str, tool: &str) -> bool {
        self.servers.get(server).is_some_and(|c| c.read_only_tools.iter().any(|p| glob(p, tool)))
    }

    async fn conn(&self, server: &str) -> Result<Arc<Mutex<Conn>>> {
        let mut conns = self.conns.lock().await;
        if let Some(c) = conns.get(server) {
            return Ok(c.clone());
        }
        let cfg = self.servers.get(server).ok_or_else(|| anyhow!("no MCP server named {server:?} in .harness/mcp.toml"))?;
        let conn = if cfg.url.is_some() { Self::connect_http(server, cfg)? } else { self.spawn_stdio(server, cfg)? };
        let conn = Arc::new(Mutex::new(conn));
        // handshake
        {
            let mut c = conn.lock().await;
            let version = if matches!(*c, Conn::Http(_)) { "2025-06-18" } else { "2024-11-05" };
            let init = Self::request_on(&mut c, "initialize", json!({"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "hx", "version": "0.1"}})).await?;
            if let Conn::Http(h) = &mut *c {
                h.version = Some(init["protocolVersion"].as_str().unwrap_or(version).to_string());
            }
            Self::notify_on(&mut c, "notifications/initialized").await?;
        }
        conns.insert(server.to_string(), conn.clone());
        Ok(conn)
    }

    fn spawn_stdio(&self, server: &str, cfg: &ServerCfg) -> Result<Conn> {
        let extra: Vec<PathBuf> = cfg.write.iter().filter_map(|w| std::fs::canonicalize(w).ok()).collect();
        let mut cmd = if self.pol.enabled {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(self.pol.seatbelt_profile_with(cfg.net, &extra)).arg(&cfg.command).args(&cfg.args);
            c.env_clear().envs(self.pol.env());
            c
        } else if let Some(profile) = self.pol.yolo_profile() {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(profile).arg(&cfg.command).args(&cfg.args);
            c
        } else {
            let mut c = Command::new(&cfg.command);
            c.args(&cfg.args);
            c
        };
        for k in &cfg.env {
            if let Ok(v) = std::env::var(k) {
                cmd.env(k, v);
            }
        }
        cmd.current_dir(&self.pol.workspace).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).kill_on_drop(true);
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
            Conn::Stdio(s) => {
                s.stdin.write_all((msg.to_string() + "\n").as_bytes()).await?;
                s.stdin.flush().await?;
                Ok(())
            }
            Conn::Http(h) => Self::http_post(h, &msg, None).await.map(|_| ()),
        }
    }

    async fn stdio_request(c: &mut Stdio, method: &str, params: Value) -> Result<Value> {
        let id = c.next_id;
        c.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string() + "\n";
        c.stdin.write_all(line.as_bytes()).await?;
        c.stdin.flush().await?;
        loop {
            let mut buf = String::new();
            let n = tokio::time::timeout(Duration::from_secs(60), c.out.read_line(&mut buf)).await.map_err(|_| anyhow::Error::new(ConnLost(format!("MCP server did not answer {method} within 60s"))))??;
            if n == 0 {
                return Err(anyhow::Error::new(ConnLost(format!("MCP server exited while handling {method}"))));
            }
            let Ok(v) = serde_json::from_str::<Value>(buf.trim()) else { continue };
            if v["id"].as_u64() != Some(id) {
                continue; // notifications and server->client requests are ignored
            }
            if let Some(e) = v.get("error") {
                bail!("MCP error: {}", e["message"].as_str().unwrap_or("unknown"));
            }
            return Ok(v["result"].clone());
        }
    }

    async fn http_request(h: &mut Http, method: &str, params: Value) -> Result<Value> {
        let id = h.next_id;
        h.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let v = Self::http_post(h, &msg, Some(id)).await?.ok_or_else(|| anyhow::Error::new(ConnLost(format!("MCP server sent no response to {method}"))))?;
        if let Some(e) = v.get("error") {
            bail!("MCP error: {}", Self::scrub(h, e["message"].as_str().unwrap_or("unknown").to_string()));
        }
        Ok(v["result"].clone())
    }

    fn scrub(h: &Http, mut t: String) -> String {
        for s in h.secrets.iter().filter(|s| !s.is_empty()) {
            t = t.replace(s.as_str(), "***");
        }
        t
    }

    /// POST one JSON-RPC message. With `want` = the request id, returns the matching response (from a JSON body or an
    /// SSE stream); without it (a notification) returns None.
    async fn http_post(h: &mut Http, msg: &Value, want: Option<u64>) -> Result<Option<Value>> {
        let lost = |h: &Http, t: String| anyhow::Error::new(ConnLost(Self::scrub(h, t)));
        let mut req = h.client.post(&h.url).headers(h.headers.clone()).header("accept", "application/json, text/event-stream").json(msg);
        if let Some(s) = &h.session {
            req = req.header("mcp-session-id", s);
        }
        if let Some(v) = &h.version {
            req = req.header("mcp-protocol-version", v);
        }
        let work = async {
            let mut resp = req.send().await.map_err(|e| lost(h, format!("MCP http request failed: {}", e.without_url())))?;
            let status = resp.status();
            if let Some(s) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                h.session = Some(s.to_string());
            }
            if status == reqwest::StatusCode::NOT_FOUND && h.session.is_some() {
                return Err(lost(h, "MCP session expired (HTTP 404)".into()));
            }
            if !status.is_success() {
                let body: String = Self::scrub(h, resp.text().await.unwrap_or_default()).chars().take(200).collect();
                let hint = if status.is_redirection() { " (redirects are not followed)" } else if status.as_u16() == 401 || status.as_u16() == 403 { " (check the credentials)" } else { "" };
                bail!("{}", Self::scrub(h, format!("MCP server returned HTTP {}{hint}: {body}", status.as_u16())));
            }
            let Some(id) = want else { return Ok(None) };
            let ctype = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
            if ctype.starts_with("text/event-stream") {
                let mut buf: Vec<u8> = vec![];
                loop {
                    let Some(chunk) = resp.chunk().await.map_err(|e| lost(h, format!("MCP stream failed: {}", e.without_url())))? else {
                        // the stream may end without the final blank line: try what is left
                        buf.extend_from_slice(b"\n\n");
                        if let Some(v) = sse_response(&buf, id) {
                            return Ok(Some(v));
                        }
                        return Err(lost(h, "MCP stream ended without a response".into()));
                    };
                    buf.extend_from_slice(&chunk);
                    if buf.len() > MAX_BODY {
                        return Err(lost(h, "MCP response too large".into()));
                    }
                    while let Some(end) = sse_event_end(&buf) {
                        let ev: Vec<u8> = buf.drain(..end).collect();
                        if let Some(v) = sse_response(&ev, id) {
                            return Ok(Some(v));
                        }
                    }
                }
            }
            if resp.content_length().is_some_and(|n| n > MAX_BODY as u64) {
                return Err(lost(h, "MCP response too large".into()));
            }
            let v: Value = resp.json().await.map_err(|e| lost(h, format!("MCP server sent an unreadable response: {}", e.without_url())))?;
            if v.get("method").is_some() || v["id"].as_u64() != Some(id) {
                return Err(lost(h, "MCP server answered a different request id".into()));
            }
            Ok(Some(v))
        };
        tokio::time::timeout(Duration::from_secs(60), work).await.map_err(|_| anyhow::Error::new(ConnLost("MCP server did not answer within 60s".into())))?
    }

    pub async fn request(&self, server: &str, method: &str, params: Value) -> Result<Value> {
        let conn = self.conn(server).await?;
        let mut c = conn.lock().await;
        match Self::request_on(&mut c, method, params).await {
            Ok(v) => Ok(v),
            Err(e) => {
                if e.is::<ConnLost>() {
                    drop(c);
                    self.conns.lock().await.remove(server); // the next call starts a fresh server
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::glob;

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
