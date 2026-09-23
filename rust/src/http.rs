//! Local HTTP endpoint for agents that don't spawn stdio MCP servers (OpenAI Agents SDK,
//! LangChain, custom scripts, anything that speaks HTTP). Runs inside the daemon.
//!
//!   POST /mcp              MCP Streamable HTTP (JSON responses; single message or batch)
//!   GET  /tools?format=    tool schemas: mcp | openai | openai-responses | anthropic
//!   POST /tools/<name>     call one tool with a JSON body of arguments (plain REST)
//!   GET  /health
//!
//! Security: bound to 127.0.0.1 only; every request needs `Authorization: Bearer <token>` from
//! ~/.reman/config.json; any request carrying a non-local `Origin` is refused (a web page in your
//! browser cannot reach it via DNS rebinding); the same folder boundary + secret redaction as the
//! stdio server (roots from config.json - none configured means nothing is visible).
use crate::daemon::{Daemon, log};
use crate::mcp;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

static STARTED: AtomicBool = AtomicBool::new(false);

/// Start the listener once (idempotent); returns false if it was already running.
pub fn start(d: Arc<Daemon>, port: u16) -> anyhow::Result<bool> {
    if STARTED.swap(true, Ordering::SeqCst) {
        return Ok(false);
    }
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            STARTED.store(false, Ordering::SeqCst);
            return Err(e.into());
        }
    };
    log(&format!("http endpoint on http://127.0.0.1:{port}/mcp"));
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let d = d.clone();
            std::thread::spawn(move || {
                let _ = handle(&d, conn);
            });
        }
    });
    Ok(true)
}

struct Req {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, k: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str())
    }
}

fn read_req(s: &TcpStream) -> anyhow::Result<Req> {
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query) = target.split_once('?').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((target, String::new()));
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
        if headers.len() > 100 {
            anyhow::bail!("too many headers");
        }
    }
    let len: usize = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")).and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    if len > 4 << 20 {
        anyhow::bail!("body too large");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Req { method, path, query, headers, body })
}

fn respond(mut s: &TcpStream, status: &str, body: Option<&Value>) {
    let payload = body.map(|b| serde_json::to_vec(b).unwrap_or_default()).unwrap_or_default();
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(&payload);
    let _ = s.flush();
}

fn local_origin(o: &str) -> bool {
    let o = o.to_ascii_lowercase();
    ["http://127.0.0.1", "http://localhost", "https://127.0.0.1", "https://localhost"]
        .iter()
        .any(|p| o == *p || o.starts_with(&format!("{p}:")) || o.starts_with(&format!("{p}/")))
}

/// Constant-time comparison so the token can't be guessed byte by byte from timing.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn handle(d: &Daemon, s: TcpStream) -> anyhow::Result<()> {
    let req = match read_req(&s) {
        Ok(r) => r,
        Err(_) => {
            respond(&s, "400 Bad Request", Some(&json!({"error": "bad request"})));
            return Ok(());
        }
    };
    if let Some(o) = req.header("origin") {
        if !local_origin(o) {
            respond(&s, "403 Forbidden", Some(&json!({"error": "cross-origin requests are not allowed"})));
            return Ok(());
        }
    }
    let settings = crate::settings::load();
    let Some(http) = settings.http.clone() else {
        respond(&s, "503 Service Unavailable", Some(&json!({"error": "http endpoint disabled (reman connect http)"})));
        return Ok(());
    };
    if req.path != "/health" {
        let ok = req.header("authorization").and_then(|a| a.strip_prefix("Bearer ")).is_some_and(|t| same(t.trim(), &http.token));
        if !ok {
            respond(&s, "401 Unauthorized", Some(&json!({"error": "missing or wrong bearer token (see ~/.reman/config.json)"})));
            return Ok(());
        }
    }
    let pol = mcp::Policy::from_settings();
    let mut call = |name: &str, args: &Value| -> anyhow::Result<Value> {
        if pol.roots.is_empty() {
            anyhow::bail!("no folders are shared with agents yet: run `reman connect http --root <dir>`");
        }
        mcp::call_tool(d, name, args, &pol)
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => respond(&s, "200 OK", Some(&json!({"ok": true, "server": "reman", "version": crate::config::VERSION}))),
        ("GET", "/tools") => {
            let fmt = req.query.split('&').find_map(|kv| kv.strip_prefix("format=")).unwrap_or("mcp");
            match mcp::tools_as(fmt) {
                Ok(v) => respond(&s, "200 OK", Some(&v)),
                Err(e) => respond(&s, "400 Bad Request", Some(&json!({"error": e.to_string()}))),
            }
        }
        ("POST", p) if p.starts_with("/tools/") => {
            let name = &p["/tools/".len()..];
            let args: Value = if req.body.is_empty() { json!({}) } else { serde_json::from_slice(&req.body).unwrap_or(json!({})) };
            match call(name, &args) {
                Ok(v) => respond(&s, "200 OK", Some(&v)),
                Err(e) => respond(&s, "400 Bad Request", Some(&json!({"error": e.to_string()}))),
            }
        }
        ("POST", "/mcp") => {
            let Ok(msg) = serde_json::from_slice::<Value>(&req.body) else {
                respond(&s, "400 Bad Request", Some(&json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}})));
                return Ok(());
            };
            let replies: Vec<Value> = match &msg {
                Value::Array(batch) => batch.iter().filter_map(|m| mcp::rpc(m, &mut call)).collect(),
                m => mcp::rpc(m, &mut call).into_iter().collect(),
            };
            match (msg.is_array(), replies.len()) {
                (_, 0) => respond(&s, "202 Accepted", None), // notifications only
                (false, _) => respond(&s, "200 OK", Some(&replies[0])),
                (true, _) => respond(&s, "200 OK", Some(&Value::Array(replies))),
            }
        }
        // no server-initiated stream: the tools are request/response
        ("GET", "/mcp") | ("DELETE", "/mcp") => respond(&s, "405 Method Not Allowed", Some(&json!({"error": "use POST"}))),
        _ => respond(&s, "404 Not Found", Some(&json!({"error": "not found", "routes": ["/mcp", "/tools", "/tools/<name>", "/health"]}))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_token_checks() {
        assert!(local_origin("http://localhost:3000"));
        assert!(local_origin("http://127.0.0.1"));
        assert!(!local_origin("http://localhost.evil.com"));
        assert!(!local_origin("https://example.com"));
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
    }
}
