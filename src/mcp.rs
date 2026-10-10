//! MCP client (stdio). Servers are user-configured third-party processes, run under their own Seatbelt profile.
//!
//! `.harness/mcp.toml` (user-owned; the model cannot write `.harness/`):
//!   [server.NAME]
//!   command = "/path/to/server"        args = ["--flag"]
//!   env = ["SOME_VAR"]                 # env vars passed through from the harness (default: none, so no secrets)
//!   write = ["/dir/it/may/write"]      # extra writable roots (besides the run's temp dir)
//!   net = false                        # all-or-nothing network, like `sh`
//!   read_only_tools = ["read_*", "list_*", "get_*", "search_*"]   # called without approval; everything else is an
//!                                                                  # external effect that goes through the approval gate
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

#[derive(Deserialize, Clone, Default)]
pub struct ServerCfg {
    pub command: String,
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

/// The server process is gone or stopped answering: the cached connection must be dropped.
#[derive(Debug)]
struct ConnLost(String);
impl std::fmt::Display for ConnLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConnLost {}

struct Conn {
    #[allow(dead_code)] // kept alive; killed when dropped
    child: Child,
    stdin: ChildStdin,
    out: BufReader<ChildStdout>,
    next_id: u64,
}

pub struct Mcp {
    pub servers: BTreeMap<String, ServerCfg>,
    pol: Arc<Policy>,
    conns: Mutex<HashMap<String, Arc<Mutex<Conn>>>>,
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
    /// None when there is no `.harness/mcp.toml` (the common case: zero cost).
    pub fn load(pol: Arc<Policy>) -> Result<Option<Arc<Mcp>>> {
        let path = pol.workspace.join(".harness/mcp.toml");
        let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
        let f: File = toml::from_str(&text).map_err(|e| anyhow!("bad .harness/mcp.toml: {e}"))?;
        if f.server.is_empty() {
            return Ok(None);
        }
        Ok(Some(Arc::new(Mcp { servers: f.server, pol, conns: Mutex::new(HashMap::new()) })))
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
        let extra: Vec<PathBuf> = cfg.write.iter().filter_map(|w| std::fs::canonicalize(w).ok()).collect();
        let mut cmd = if self.pol.enabled {
            let mut c = Command::new("/usr/bin/sandbox-exec");
            c.arg("-p").arg(self.pol.seatbelt_profile_with(cfg.net, &extra)).arg(&cfg.command).args(&cfg.args);
            c.env_clear().envs(self.pol.env());
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
        let conn = Arc::new(Mutex::new(Conn { child, stdin, out, next_id: 1 }));
        // handshake
        {
            let mut c = conn.lock().await;
            Self::request_on(&mut c, "initialize", json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "hx", "version": "0.1"}})).await?;
            let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string() + "\n";
            c.stdin.write_all(note.as_bytes()).await?;
            c.stdin.flush().await?;
        }
        conns.insert(server.to_string(), conn.clone());
        Ok(conn)
    }

    async fn request_on(c: &mut Conn, method: &str, params: Value) -> Result<Value> {
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
