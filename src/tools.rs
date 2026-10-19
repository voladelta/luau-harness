//! Single-file, workspace-trusted tools. Definitions and endpoint grants are frozen; secrets never are.
use crate::{effects::{Effects, Ret}, modules::{Registry, Source}, policy::Class, supervisor::Tool, values};
use anyhow::{Context, Result, ensure};
use mlua::{Function, Lua, Table, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, path::Path, sync::Arc, time::{Duration, Instant}};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub url: String,
    #[serde(default = "get")]
    pub method: String,
    pub secret: Option<String>,
    #[serde(default = "header")]
    pub header: String,
    #[serde(default)]
    pub bearer: bool,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub read_only: bool,
}
fn get() -> String { "GET".into() }
fn header() -> String { "Authorization".into() }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Method { pub signature: String, pub description: String }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub http: BTreeMap<String, Endpoint>,
    pub capabilities: BTreeMap<String, Vec<String>>,
    pub tools: BTreeMap<String, Method>,
}

fn identifier(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        && !["and", "break", "do", "else", "elseif", "end", "false", "for", "function", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "continue"].contains(&name)
}

fn fields(table: &Table, allowed: &[&str]) -> Result<()> {
    ensure!(table.metatable().is_none(), "tool definitions must be plain tables");
    for pair in table.clone().pairs::<String, Value>() {
        let (key, _) = pair?;
        ensure!(allowed.contains(&key.as_str()), "unknown tool definition field {key:?}");
    }
    Ok(())
}

fn data<T: serde::de::DeserializeOwned + Default>(table: &Table, key: &str) -> Result<T> {
    match table.get::<Value>(key)? {
        Value::Nil => Ok(T::default()),
        value => Ok(serde_json::from_value(values::copy_out(value)?)?),
    }
}

pub fn encode_path_segment(value: &str) -> Result<String, &'static str> {
    if value.is_empty() {
        return Err("URL path parameter cannot be empty");
    }
    if value == "." || value.contains("..") {
        return Err("URL path parameter cannot contain '..'");
    }
    let mut encoded = String::with_capacity(value.len() * 3);
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.' || b == b'~' || b == b':' {
            encoded.push(b as char);
        } else {
            use std::fmt::Write;
            write!(&mut encoded, "%{:02X}", b).unwrap();
        }
    }
    Ok(encoded)
}

pub fn validate_endpoint_url(url_str: &str, has_secret: bool) -> Result<reqwest::Url> {
    ensure!(!url_str.contains('?') && !url_str.contains('#'), "endpoint URL cannot contain query or fragment");
    ensure!(url_str.starts_with("https://") || url_str.starts_with("http://"), "endpoint must be an HTTP(S) URL");
    let scheme_end = url_str.find("://").unwrap() + 3;
    let path_start = url_str[scheme_end..].find('/').map(|i| scheme_end + i).unwrap_or(url_str.len());
    let authority = &url_str[scheme_end..path_start];
    ensure!(!authority.contains('{') && !authority.contains('}'), "endpoint origin cannot contain placeholders: {url_str}");

    let base_origin_str = format!("{}/", &url_str[..path_start]);
    let base_url = reqwest::Url::parse(&base_origin_str).context("invalid endpoint origin URL")?;
    ensure!(matches!(base_url.scheme(), "https" | "http"), "endpoint must be an HTTP(S) URL");
    ensure!(base_url.host_str().is_some() && !base_url.host_str().unwrap().is_empty(), "endpoint must have a valid host");
    ensure!(base_url.username().is_empty() && base_url.password().is_none(), "endpoint cannot contain credentials in URL");
    if has_secret {
        ensure!(base_url.scheme() == "https" || matches!(base_url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")), "authenticated endpoints require HTTPS (except loopback)");
    }

    let path = &url_str[path_start..];
    ensure!(!path.contains("/../") && !path.ends_with("/..") && path != "/..", "endpoint URL cannot contain '..'");
    let mut in_placeholder = false;
    let mut placeholder_name = String::new();
    let mut dummy_path = String::new();

    for c in path.chars() {
        if c == '{' {
            ensure!(!in_placeholder, "nested '{{' in endpoint URL path");
            in_placeholder = true;
            placeholder_name.clear();
        } else if c == '}' {
            ensure!(in_placeholder, "unmatched '}}' in endpoint URL path");
            in_placeholder = false;
            ensure!(
                !placeholder_name.is_empty() && placeholder_name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
                "invalid placeholder '{{{placeholder_name}}}' in endpoint URL"
            );
            dummy_path.push_str("segment");
        } else if in_placeholder {
            placeholder_name.push(c);
        } else {
            dummy_path.push(c);
        }
    }
    ensure!(!in_placeholder, "unclosed '{{' in endpoint URL path");

    let dummy_full_url = format!("{}{}", &url_str[..path_start], dummy_path);
    let dummy_parsed = reqwest::Url::parse(&dummy_full_url).context("invalid endpoint URL path")?;
    ensure!(dummy_parsed.origin() == base_url.origin(), "endpoint URL altered origin");
    ensure!(dummy_parsed.username().is_empty() && dummy_parsed.password().is_none(), "endpoint URL cannot contain credentials");
    ensure!(dummy_parsed.query().is_none() && dummy_parsed.fragment().is_none(), "endpoint URL cannot contain query or fragment");

    Ok(base_url)
}

pub fn definition(table: &Table) -> Result<Definition> {
    fields(table, &["http", "capabilities", "tools"])?;
    let http: BTreeMap<String, Endpoint> = data(table, "http")?;
    let mut capabilities: BTreeMap<String, Vec<String>> = data(table, "capabilities")?;
    for (cap, scopes) in &capabilities {
        for scope in scopes {
            ensure!(matches!((cap.as_str(), scope.as_str()), ("fs", "read" | "write") | ("proc", "sh") | ("llm", "agent")), "unsupported tool capability {cap}.{scope}");
        }
    }
    for (name, endpoint) in &http {
        ensure!(identifier(name), "invalid endpoint name {name:?}");
        let base_url = validate_endpoint_url(&endpoint.url, endpoint.secret.is_some())?;
        ensure!(matches!(endpoint.method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE"), "unsupported HTTP method");
        reqwest::header::HeaderName::from_bytes(endpoint.header.as_bytes())?;
        if let Some(secret) = &endpoint.secret {
            ensure!(!secret.is_empty() && secret.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'), "invalid secret name");
            ensure!(base_url.scheme() == "https" || matches!(base_url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")), "authenticated endpoints require HTTPS (except loopback)");
        }
    }
    if !http.is_empty() { capabilities.insert("http".into(), vec!["request".into()]); }
    let methods: Table = table.get("tools")?;
    ensure!(methods.metatable().is_none(), "tools must be a plain table");
    let mut tools = BTreeMap::new();
    for pair in methods.pairs::<String, Table>() {
        let (name, method) = pair?;
        ensure!(identifier(&name), "invalid tool method {name:?}");
        fields(&method, &["signature", "description", "run"])?;
        method.get::<Function>("run").context("tool method needs a run function")?;
        let signature: String = method.get("signature")?;
        let description: String = method.get("description")?;
        ensure!(signature.starts_with(&format!("{name}(")) || signature.starts_with(&format!("{name}{{")), "signature must start with {name}( or {name}{{");
        ensure!(signature.len() <= 2000 && description.len() <= 4000 && !description.trim().is_empty(), "tool signature/description is empty or too long");
        tools.insert(name, Method { signature, description });
    }
    ensure!(!tools.is_empty(), "tool namespace must export at least one method");
    Ok(Definition { http, capabilities, tools })
}

fn inspect(text: &str) -> Result<Definition> {
    let lua = Lua::new();
    for key in ["_G", "os", "coroutine", "getfenv", "setfenv", "loadstring", "require", "print"] { lua.globals().set(key, Value::Nil)?; }
    let math: Table = lua.globals().get("math")?;
    math.set("random", Value::Nil)?; math.set("randomseed", Value::Nil)?;
    lua.set_memory_limit(16 << 20)?;
    let deadline = Instant::now() + Duration::from_secs(1);
    lua.set_interrupt(move |_| {
        if Instant::now() >= deadline { return Err(mlua::Error::runtime("tool registration time budget exceeded")); }
        Ok(mlua::VmState::Continue)
    });
    lua.sandbox(true)?;
    definition(&lua.load(text).set_name("=tool registration").eval::<Table>()?)
}

/// Environment > workspace > user config. No recursive scans or shell evaluation of credential files.
fn secret(root: &Path, name: &str) -> Result<String> {
    if let Ok(value) = std::env::var(name) && !value.trim().is_empty() { return Ok(value); }
    let mut paths = vec![root.join(".harness/secrets.toml")];
    if let Some(dir) = crate::trust::store_dir() { paths.push(dir.join("secrets.toml")); }
    for path in paths {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(meta.file_type().is_file(), "secrets file must be a regular file: {}", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => anyhow::bail!("cannot inspect secrets file {}", path.display()),
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => anyhow::bail!("cannot read secrets file {}", path.display()),
        };
        // Parse errors can quote their input, so never propagate TOML's diagnostic here.
        let secrets: BTreeMap<String, String> = toml::from_str(&text).map_err(|_| anyhow::anyhow!("invalid secrets file {} (expected NAME = \"value\")", path.display()))?;
        if let Some(value) = secrets.get(name).filter(|v| !v.trim().is_empty()) { return Ok(value.clone()); }
    }
    anyhow::bail!("missing credential {name}; set it in the environment, .harness/secrets.toml, or the hx user secrets.toml")
}

pub fn credentials(root: &Path, def: &Definition) -> Result<()> {
    for endpoint in def.http.values() {
        if let Some(name) = &endpoint.secret {
            let value = secret(root, name)?;
            reqwest::header::HeaderValue::from_str(&value).map_err(|_| anyhow::anyhow!("credential {name} is not a valid HTTP header value"))?;
        }
    }
    Ok(())
}

pub fn capture(root: &Path, registry: &mut Registry) -> Result<()> {
    let dir = root.join(".harness/tools");
    if !dir.exists() { return Ok(()); }
    ensure!(!std::fs::symlink_metadata(&dir)?.file_type().is_symlink(), "tools directory must not be a symlink");
    let mut files = std::fs::read_dir(dir)?.map(|entry| entry.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    for path in files {
        if !path.extension().is_some_and(|e| e == "lua" || e == "luau") { continue; }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let result = (|| -> Result<(String, Definition)> {
            ensure!(identifier(&name), "invalid tool namespace {name:?}");
            ensure!(!["tools", "skills", "skill", "mcp", "deftool", "promote", "propose_module", "view", "hx", "agent"].contains(&name.as_str()), "reserved tool namespace {name}");
            ensure!(!registry.tools.contains_key(&name) && !registry.tool_errors.contains_key(&name), "duplicate tool namespace {name}");
            ensure!(std::fs::symlink_metadata(&path)?.file_type().is_file(), "tool must be a regular file, not a symlink");
            let text = std::fs::read_to_string(&path)?;
            let def = inspect(&text)?;
            credentials(root, &def)?;
            Ok((text, def))
        })();
        match result {
            Ok((text, def)) => {
                let id = format!("tool:{name}");
                registry.capabilities.insert(id.clone(), def.capabilities.clone());
                registry.sources.insert(id.clone(), Source { text, origin: format!(".harness/tools/{}", path.file_name().unwrap().to_string_lossy()), is_override: false, pkg: Some((id, def.capabilities.clone())) });
                registry.tools.insert(name, def);
            },
            Err(error) => {
                // A duplicate invalidates both files, independently of directory enumeration order.
                registry.tools.remove(&name);
                registry.sources.remove(&format!("tool:{name}"));
                registry.capabilities.remove(&format!("tool:{name}"));
                registry.tool_errors.insert(name, error.to_string());
            },
        }
    }
    Ok(())
}

pub fn exports(lua: &Lua, table: Table, expected: &Definition) -> mlua::Result<Table> {
    let actual = definition(&table).map_err(mlua::Error::external)?;
    if actual != *expected { return Err(mlua::Error::runtime("tool definition changed shape after registration")); }
    let methods: Table = table.get("tools")?;
    let exports = lua.create_table()?;
    for name in expected.tools.keys() { exports.set(name.as_str(), methods.get::<Table>(name.as_str())?.get::<Function>("run")?)?; }
    Ok(exports)
}

pub fn install_http(lua: &Lua, fx: Arc<Effects>, namespace: String, def: Definition) -> mlua::Result<()> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).user_agent("openrouter-tool/1.0 (luau-harness)").redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never()).build().map_err(mlua::Error::external)?;
    lua.globals().set("http", lua.create_async_function(move |lua, (endpoint, args): (String, Value)| {
        let (fx, client, namespace, def) = (fx.clone(), client.clone(), namespace.clone(), def.clone());
        async move {
            let spec = def.http.get(&endpoint).ok_or_else(|| mlua::Error::runtime("undeclared HTTP endpoint"))?;
            let mut args = values::copy_out(args)?;
            if !args.is_object() { return Err(mlua::Error::runtime("http arguments must be an object")); }
            let key = spec.secret.as_ref().map(|name| secret(&fx.pol.workspace, name)).transpose().map_err(mlua::Error::external)?;
            let base_url = validate_endpoint_url(&spec.url, spec.secret.is_some()).map_err(mlua::Error::external)?;
            let expected_origin = base_url.origin();
            let orig_args = args.clone();
            let mut url_str = spec.url.clone();
            if url_str.contains('{') {
                if let Some(obj) = args.as_object_mut() {
                    let mut placeholders = Vec::new();
                    let mut in_p = false;
                    let mut cur = String::new();
                    for c in spec.url.chars() {
                        if c == '{' {
                            in_p = true;
                            cur.clear();
                        } else if c == '}' {
                            in_p = false;
                            placeholders.push(cur.clone());
                        } else if in_p {
                            cur.push(c);
                        }
                    }
                    let mut seen = std::collections::BTreeSet::new();
                    for name in placeholders {
                        if !seen.insert(name.clone()) { continue; }
                        let placeholder = format!("{{{name}}}");
                        let v = obj.remove(&name).ok_or_else(|| mlua::Error::runtime(format!("missing required URL parameter '{name}'")))?;
                        let val_str = match v {
                            serde_json::Value::String(s) => s,
                            serde_json::Value::Number(n) => n.to_string(),
                            serde_json::Value::Bool(b) => b.to_string(),
                            _ => return Err(mlua::Error::runtime("URL parameters must be strings, numbers or booleans")),
                        };
                        let encoded_segment = encode_path_segment(&val_str).map_err(mlua::Error::runtime)?;
                        url_str = url_str.replace(&placeholder, &encoded_segment);
                    }
                }
            }
            if url_str.contains('{') || url_str.contains('}') {
                return Err(mlua::Error::runtime("unresolved placeholder in URL"));
            }
            let mut url = reqwest::Url::parse(&url_str).map_err(mlua::Error::external)?;
            if url.origin() != expected_origin {
                return Err(mlua::Error::runtime(format!(
                    "request URL origin {} does not match fixed endpoint origin {}",
                    url.origin().ascii_serialization(),
                    expected_origin.ascii_serialization()
                )));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(mlua::Error::runtime("request URL cannot contain credentials"));
            }
            if spec.method == "GET" {
                let query = args.as_object().unwrap().iter().map(|(k, v)| {
                    let value = match v { serde_json::Value::String(s) => s.clone(), serde_json::Value::Bool(_) | serde_json::Value::Number(_) => v.to_string(), _ => return Err(mlua::Error::runtime("GET parameters must be strings, numbers or booleans")) };
                    Ok((k.clone(), value))
                }).collect::<mlua::Result<Vec<_>>>()?;
                url.query_pairs_mut().extend_pairs(&query);
            }
            let mut request = client.request(spec.method.parse().map_err(mlua::Error::external)?, url);
            if spec.method != "GET" { request = request.json(&args); }
            if let Some(key) = &key {
                let formatted = if spec.bearer {
                    if key.starts_with("Bearer ") { key.clone() } else { format!("Bearer {key}") }
                } else {
                    key.clone()
                };
                let mut header = reqwest::header::HeaderValue::from_str(&formatted).map_err(|_| mlua::Error::runtime("invalid credential header value"))?;
                header.set_sensitive(true);
                request = request.header(&spec.header, header);
            }
            let class = if spec.read_only { Class::Read } else { Class::External };
            let ret = fx.run(&lua, Tool::Http, class, json!({"namespace":namespace, "endpoint":endpoint, "arguments":orig_args}), || async {
                let label = format!("http {namespace}.{endpoint}");
                fx.pol.approve(&label, class).await.map_err(|e| mlua::Error::runtime(format!("approval_required: {e}")))?;
                let mut response = request.send().await.map_err(|e| unusable(class, format!("HTTP transport failed: {}", e.without_url())))?;
                let status = response.status();
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.map_err(|_| unusable(class, "HTTP response interrupted"))? {
                    if body.len() + chunk.len() > 8 << 20 { return Err(unusable(class, "HTTP response exceeds 8 MiB")); }
                    body.extend_from_slice(&chunk);
                }
                let text = String::from_utf8_lossy(&body);
                if !status.is_success() {
                    let mut excerpt = text.into_owned();
                    if let Some(key) = &key {
                        let encoded = serde_json::to_string(key).map_err(mlua::Error::external)?;
                        excerpt = excerpt.replace(&encoded[1..encoded.len()-1], "[redacted]").replace(key, "[redacted]");
                    }
                    let message = format!("HTTP {}: {}", status.as_u16(), excerpt.chars().take(1000).collect::<String>());
                    return Err(if status.is_client_error() { mlua::Error::runtime(message) } else { unusable(class, message) });
                }
                let mut result = if spec.raw {
                    let ct = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
                    let is_text = (ct.starts_with("text/") || ct.contains("json") || ct.contains("yaml") || ct.contains("markdown") || ct.is_empty())
                        && !ct.starts_with("audio/") && !ct.starts_with("image/") && !ct.starts_with("video/")
                        && std::str::from_utf8(&body).is_ok();
                    if is_text {
                        json!({"text": String::from_utf8_lossy(&body).into_owned(), "media_type": ct})
                    } else {
                        use base64::Engine;
                        let media_type = if ct.is_empty() { "application/octet-stream" } else { ct };
                        json!({
                            "base64": base64::engine::general_purpose::STANDARD.encode(&body),
                            "media_type": media_type
                        })
                    }
                } else {
                    serde_json::from_str(&text).map_err(|_| unusable(class, "HTTP response is not valid JSON"))?
                };
                if let Some(key) = &key { redact(&mut result, key); }
                values::validate(&result).map_err(|e| unusable(class, e.to_string()))?;
                Ok(Ret::Json(result))
            }).await?;
            crate::prims::json_value(&lua, ret)
        }
    })?)
}

// A mutation with an unreadable response may have happened. Keep its intent open for owner reconciliation.
fn unusable(class: Class, message: impl AsRef<str>) -> mlua::Error {
    if class == Class::Read { mlua::Error::runtime(message.as_ref()) } else { crate::effects::unknown(message.as_ref()) }
}

fn redact(value: &mut serde_json::Value, key: &str) {
    match value {
        serde_json::Value::String(s) => *s = s.replace(key, "[redacted]"),
        serde_json::Value::Array(items) => { for item in items { redact(item, key); } },
        serde_json::Value::Object(fields) => {
            *fields = std::mem::take(fields).into_iter().map(|(name, mut value)| {
                redact(&mut value, key);
                (name.replace(key, "[redacted]"), value)
            }).collect();
        },
        _ => {},
    }
}

/// Bind all methods together, rejecting collisions before touching the session environment.
pub fn install(lua: &Lua, env: &Table, supervisor: Arc<crate::supervisor::Supervisor>, host: Option<Arc<crate::host::Host>>, caller: Arc<Effects>) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for (name, error) in &supervisor.registry.tool_errors { eprintln!("warning: tool {name} registration failed: {error}"); }
    for (name, def) in &supervisor.registry.tools {
        let result = (|| -> Result<Table> {
            ensure!(env.get::<Value>(name.as_str())?.is_nil(), "namespace {name} is already in use");
            credentials(&supervisor.policy.workspace, def)?;
            Ok(crate::packages::proxy(lua, supervisor.clone(), host.clone(), &format!("tool:{name}"), caller.clone())?)
        })();
        match result {
            Ok(table) => {
                table.set_readonly(true);
                env.raw_set(name.as_str(), table)?;
                for method in def.tools.values() { lines.push(format!("- {name}.{}  -- {}", method.signature, method.description)); }
            },
            Err(error) => eprintln!("warning: tool {name} registration failed: {error}"),
        }
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_origin_equality() {
        let u1 = reqwest::Url::parse("https://openrouter.ai/").unwrap();
        let u2 = reqwest::Url::parse("https://openrouter.ai/a/b/c?x=1#frag").unwrap();
        assert_eq!(u1.origin(), u2.origin());
    }

    #[test]
    fn test_validate_endpoint_url_rejects_host_placeholders() {
        assert!(validate_endpoint_url("https://{host}/api", false).is_err());
        assert!(validate_endpoint_url("https://{host}:8080/api", false).is_err());
        assert!(validate_endpoint_url("https://api.{domain}.com/v1", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai:{port}/api", false).is_err());
        assert!(validate_endpoint_url("https://{user}@{host}/api", false).is_err());
    }

    #[test]
    fn test_validate_endpoint_url_rejects_queries_and_fragments() {
        assert!(validate_endpoint_url("https://openrouter.ai/api?q={search}", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/api#frag", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/api#{frag}", false).is_err());
    }

    #[test]
    fn test_validate_endpoint_url_rejects_invalid_placeholders() {
        assert!(validate_endpoint_url("https://openrouter.ai/{a/b}/foo", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/{}/foo", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/{unclosed", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/foo}bar", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/{nested{p}}", false).is_err());
        assert!(validate_endpoint_url("https://openrouter.ai/../foo", false).is_err());
    }

    #[test]
    fn test_validate_endpoint_url_accepts_valid_endpoints() {
        assert!(validate_endpoint_url("https://openrouter.ai/api/v1/credits", false).is_ok());
        assert!(validate_endpoint_url("https://openrouter.ai/{provider}/{model_id}/llms.txt", true).is_ok());
        assert!(validate_endpoint_url("https://openrouter.ai/api/v1/videos/{job_id}", true).is_ok());
        assert!(validate_endpoint_url("http://127.0.0.1:8080/api", true).is_ok());
        assert!(validate_endpoint_url("http://localhost:3000/{id}", true).is_ok());
        // Non-loopback HTTP with secret is rejected
        assert!(validate_endpoint_url("http://insecure.example.com/api", true).is_err());
    }

    #[test]
    fn test_encode_path_segment() {
        assert_eq!(encode_path_segment("gpt-4o").unwrap(), "gpt-4o");
        assert_eq!(encode_path_segment("claude-3.5-sonnet").unwrap(), "claude-3.5-sonnet");
        assert_eq!(encode_path_segment("laguna-s-2.1:free").unwrap(), "laguna-s-2.1:free");
        assert_eq!(encode_path_segment("foo/bar").unwrap(), "foo%2Fbar");
        assert_eq!(encode_path_segment("@evil.com").unwrap(), "%40evil.com");
        assert_eq!(encode_path_segment("?key=val").unwrap(), "%3Fkey%3Dval");
        assert_eq!(encode_path_segment("#frag").unwrap(), "%23frag");
        assert_eq!(encode_path_segment("foo bar").unwrap(), "foo%20bar");
        assert_eq!(encode_path_segment(r"foo\bar").unwrap(), "foo%5Cbar");

        assert!(encode_path_segment("").is_err());
        assert!(encode_path_segment(".").is_err());
        assert!(encode_path_segment("..").is_err());
        assert!(encode_path_segment("foo/../bar").is_err());
        assert!(encode_path_segment("../../etc/passwd").is_err());
    }
}
