//! Minimal, hardened loopback-only HTTP/1.1 server for MCP Streamable HTTP.
//! Reuses `quiet_sync_client::tokio` without pulling extra HTTP frameworks into
//! the desktop binary.

use super::mcp::McpServer;
use quiet_sync_client::tokio;
use quiet_sync_client::tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use quiet_sync_client::tokio::net::{TcpListener, TcpStream};
use quiet_sync_client::tokio::sync::watch;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct AuthState {
    pub token: String,
    pub port: u16,
}

pub async fn serve(
    listener: TcpListener,
    mcp: Arc<McpServer>,
    auth: Arc<RwLock<AuthState>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { break; }
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { continue };
                let mcp = mcp.clone();
                let auth = auth.clone();
                tokio::spawn(async move {
                    let _ = tokio::time::timeout(IO_TIMEOUT, handle_conn(stream, mcp, auth)).await;
                });
            }
        }
    }
}

struct ParsedReq {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

enum ParseErr {
    BadRequest(&'static str),
    PayloadTooLarge,
    Io,
}

/// Read one `\n`-terminated line without ever buffering more than `max_bytes`.
async fn read_line_limited<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R, max_bytes: usize) -> Result<String, ParseErr> {
    let mut buf = Vec::new();
    loop {
        let avail = reader.fill_buf().await.map_err(|_| ParseErr::Io)?;
        if avail.is_empty() {
            break;
        }
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if buf.len().saturating_add(take) > max_bytes {
                return Err(ParseErr::BadRequest("line exceeds limit"));
            }
            buf.extend_from_slice(&avail[..take]);
            reader.consume(take);
            break;
        }
        let n = avail.len();
        if buf.len().saturating_add(n) > max_bytes {
            return Err(ParseErr::BadRequest("line exceeds limit"));
        }
        buf.extend_from_slice(avail);
        reader.consume(n);
    }
    String::from_utf8(buf).map_err(|_| ParseErr::BadRequest("non-UTF-8 header line"))
}

async fn read_request(stream: &mut TcpStream) -> Result<ParsedReq, ParseErr> {
    let mut reader = BufReader::new(stream);
    let req_line = read_line_limited(&mut reader, MAX_HEADER_BYTES).await?;
    let n = req_line.len();
    if n == 0 {
        return Err(ParseErr::BadRequest("empty request line"));
    }
    let mut parts = req_line.trim_end_matches(['\r', '\n']).split_whitespace();
    let method = parts.next().ok_or(ParseErr::BadRequest("missing method"))?.to_string();
    let raw_target = parts.next().ok_or(ParseErr::BadRequest("missing target"))?;
    let _version = parts.next().ok_or(ParseErr::BadRequest("missing http version"))?;
    let path = raw_target.split('?').next().unwrap_or(raw_target).to_string();

    let mut headers = HashMap::new();
    let mut total_hdr_bytes = n;
    for _ in 0..MAX_HEADERS {
        let remaining = MAX_HEADER_BYTES.saturating_sub(total_hdr_bytes);
        if remaining == 0 {
            return Err(ParseErr::BadRequest("headers too large"));
        }
        let line = read_line_limited(&mut reader, remaining).await?;
        let got = line.len();
        if got == 0 { return Err(ParseErr::BadRequest("unexpected EOF in headers")); }
        total_hdr_bytes += got;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        } else {
            return Err(ParseErr::BadRequest("malformed header"));
        }
    }

    let is_chunked = headers
        .get("transfer-encoding")
        .is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));

    let mut body = Vec::new();
    if is_chunked {
        loop {
            let size_line = read_line_limited(&mut reader, 1024).await?;
            if size_line.is_empty() { return Err(ParseErr::BadRequest("unexpected EOF in chunked body")); }
            let hex = size_line.trim().split(';').next().unwrap_or("");
            let sz = usize::from_str_radix(hex, 16).map_err(|_| ParseErr::BadRequest("invalid chunk size"))?;
            if sz == 0 {
                // Consume trailing CRLF / trailer headers (bounded).
                let _ = read_line_limited(&mut reader, 4096).await;
                break;
            }
            if body.len().saturating_add(sz) > MAX_BODY_BYTES {
                return Err(ParseErr::PayloadTooLarge);
            }
            let start = body.len();
            body.resize(start + sz, 0);
            reader.read_exact(&mut body[start..]).await.map_err(|_| ParseErr::Io)?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf).await.map_err(|_| ParseErr::Io)?;
        }
    } else if let Some(cl_str) = headers.get("content-length") {
        let len: usize = cl_str.parse().map_err(|_| ParseErr::BadRequest("invalid Content-Length"))?;
        if len > MAX_BODY_BYTES {
            return Err(ParseErr::PayloadTooLarge);
        }
        if len > 0 {
            body.resize(len, 0);
            reader.read_exact(&mut body).await.map_err(|_| ParseErr::Io)?;
        }
    }

    Ok(ParsedReq { method, path, headers, body })
}

fn valid_loopback_host(host: &str, port: u16) -> bool {
    let h = host.trim().to_ascii_lowercase();
    h == "127.0.0.1"
        || h == format!("127.0.0.1:{port}")
        || h == "localhost"
        || h == format!("localhost:{port}")
        || h == "[::1]"
        || h == format!("[::1]:{port}")
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

async fn write_response(stream: &mut TcpStream, status: u16, extra_headers: &[(&str, &str)], content_type: Option<&str>, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    };
    let mut head = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n", body.len());
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    for (k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    if !body.is_empty() {
        let _ = stream.write_all(body).await;
    }
    let _ = stream.flush().await;
}

async fn handle_conn(mut stream: TcpStream, mcp: Arc<McpServer>, auth: Arc<RwLock<AuthState>>) {
    let req = match read_request(&mut stream).await {
        Ok(r) => r,
        Err(ParseErr::PayloadTooLarge) => {
            write_response(&mut stream, 413, &[], Some("application/json"), br#"{"error":"payload too large"}"#).await;
            return;
        }
        Err(ParseErr::BadRequest(msg)) => {
            let body = serde_json::json!({ "error": msg }).to_string();
            write_response(&mut stream, 400, &[], Some("application/json"), body.as_bytes()).await;
            return;
        }
        Err(ParseErr::Io) => return,
    };

    let (expected_token, port) = {
        let g = auth.read().unwrap_or_else(|e| e.into_inner());
        (g.token.clone(), g.port)
    };

    // 1. Block DNS rebinding via strict loopback Host header verification.
    let Some(host) = req.headers.get("host") else {
        write_response(&mut stream, 400, &[], Some("application/json"), br#"{"error":"missing Host header"}"#).await;
        return;
    };
    if !valid_loopback_host(host, port) {
        write_response(&mut stream, 403, &[], Some("application/json"), br#"{"error":"forbidden host"}"#).await;
        return;
    }

    // 2. Block browser cross-origin requests.
    if req.headers.contains_key("origin") {
        write_response(&mut stream, 403, &[], Some("application/json"), br#"{"error":"browser Origin header is not allowed"}"#).await;
        return;
    }

    // 3. Enforce Bearer token authentication with constant-time comparison.
    let provided_token = req
        .headers
        .get("authorization")
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim)
        .unwrap_or("");
    if expected_token.is_empty() || !constant_time_eq(provided_token.as_bytes(), expected_token.as_bytes()) {
        write_response(
            &mut stream,
            401,
            &[("WWW-Authenticate", "Bearer")],
            Some("application/json"),
            br#"{"error":"unauthorized"}"#,
        ).await;
        return;
    }

    // 4. Route endpoints.
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => {
            let body = serde_json::json!({ "ok": true, "app": "quiet.md", "version": env!("CARGO_PKG_VERSION") }).to_string();
            write_response(&mut stream, 200, &[], Some("application/json"), body.as_bytes()).await;
        }
        ("POST", "/mcp") => {
            let sid = req.headers.get("mcp-session-id").map(String::as_str);
            let reply = mcp.handle_post(sid, &req.body).await;
            let mut hdrs: Vec<(&str, &str)> = Vec::new();
            let sid_owned = reply.session_id.clone();
            if let Some(ref s) = sid_owned {
                hdrs.push(("Mcp-Session-Id", s.as_str()));
            }
            match reply.body {
                Some(v) => {
                    let bytes = serde_json::to_vec(&v).unwrap_or_default();
                    write_response(&mut stream, reply.status, &hdrs, Some("application/json"), &bytes).await;
                }
                None => {
                    write_response(&mut stream, reply.status, &hdrs, None, &[]).await;
                }
            }
        }
        ("GET", "/mcp") => {
            write_response(&mut stream, 405, &[("Allow", "POST, DELETE")], Some("application/json"), br#"{"error":"method not allowed"}"#).await;
        }
        ("DELETE", "/mcp") => {
            if let Some(sid) = req.headers.get("mcp-session-id") {
                mcp.close_session(sid);
            }
            write_response(&mut stream, 204, &[], None, &[]).await;
        }
        _ => {
            write_response(&mut stream, 404, &[], Some("application/json"), br#"{"error":"not found"}"#).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_api::mcp::{ApiBackend, ApiCall, BoxFuture};
    use fdl_core::agent::AgentError;
    use serde_json::{json, Value};

    struct MockBackend {
        read_only: bool,
    }

    impl ApiBackend for MockBackend {
        fn execute(&self, call: ApiCall) -> BoxFuture<'_, Result<Value, AgentError>> {
            Box::pin(async move {
                match call {
                    ApiCall::GetCurrentContext => Ok(json!({ "doc_id": "d1", "title": "Note", "revision": "0123456789abcdef" })),
                    ApiCall::CreateDoc { title, .. } => Ok(json!({ "doc_id": "new1", "title": title })),
                    _ => Ok(json!({ "ok": true })),
                }
            })
        }
        fn is_read_only(&self) -> bool { self.read_only }
    }

    async fn send_raw(port: u16, raw: &str) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(raw.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.unwrap();
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn security_guards_and_mcp_handshake_work() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let token = "secret-token-123".to_string();
            let auth = Arc::new(RwLock::new(AuthState { token: token.clone(), port }));
            let mcp = Arc::new(McpServer::new(Arc::new(MockBackend { read_only: true })));
            let (tx, rx) = watch::channel(false);
            let srv = tokio::spawn(serve(listener, mcp, auth, rx));

            // 1. Missing token -> 401
            let resp = send_raw(port, &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")).await;
            assert!(resp.starts_with("HTTP/1.1 401"), "{resp}");

            // 2. DNS rebinding host -> 403
            let resp = send_raw(port, &format!("GET /health HTTP/1.1\r\nHost: evil.example:{port}\r\nAuthorization: Bearer {token}\r\n\r\n")).await;
            assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");

            // 3. Browser Origin header -> 403
            let resp = send_raw(port, &format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: https://evil.example\r\nAuthorization: Bearer {token}\r\n\r\n")).await;
            assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");

            // 4. Valid MCP initialize -> 200 + Mcp-Session-Id
            let init_body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#;
            let resp = send_raw(port, &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{init_body}",
                init_body.len()
            )).await;
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert!(resp.contains("Mcp-Session-Id:"), "{resp}");
            assert!(resp.contains("\"name\":\"quiet-md\""), "{resp}");

            // 5. Read-only mode blocks write tool
            let write_body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"create_doc","arguments":{"title":"X"}}}"#;
            let resp = send_raw(port, &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{write_body}",
                write_body.len()
            )).await;
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert!(resp.contains("PERMISSION_DENIED"), "{resp}");

            // 6. Read tool succeeds
            let read_body = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"get_current_context","arguments":{}}}"#;
            let resp = send_raw(port, &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{read_body}",
                read_body.len()
            )).await;
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert!(resp.contains("\"doc_id\":\"d1\""), "{resp}");

            // 7. Deleted/expired Mcp-Session-Id returns HTTP 404
            let init_resp = send_raw(port, &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{init_body}",
                init_body.len()
            )).await;
            let sid = init_resp
                .lines()
                .find_map(|l| l.strip_prefix("Mcp-Session-Id:").map(str::trim))
                .unwrap()
                .to_string();
            let del_resp = send_raw(port, &format!(
                "DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nMcp-Session-Id: {sid}\r\n\r\n"
            )).await;
            assert!(del_resp.starts_with("HTTP/1.1 204"), "{del_resp}");
            let expired_resp = send_raw(port, &format!(
                "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nMcp-Session-Id: {sid}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{read_body}",
                read_body.len()
            )).await;
            assert!(expired_resp.starts_with("HTTP/1.1 404"), "{expired_resp}");

            // 8. Oversized header line without newline is bounded and rejected with 400
            let huge = "X".repeat(MAX_HEADER_BYTES + 128);
            let bad_resp = send_raw(port, &format!("GET /{huge} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")).await;
            assert!(bad_resp.starts_with("HTTP/1.1 400"), "{bad_resp}");

            let _ = tx.send(true);
            let _ = srv.await;
        });
    }
}
