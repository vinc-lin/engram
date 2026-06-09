// engram-mcp main: JSON-RPC 2.0 over stdio (default) or HTTP (set ENGRAM_MCP_HTTP=addr).
//
// Config (env vars; no flags to keep dependencies minimal):
//   ENGRAM_URL        — default: http://127.0.0.1:8088
//   ENGRAM_TOKEN      — Bearer token (required)
//   ENGRAM_NAMESPACE  — target namespace (required)
//   ENGRAM_MCP_HTTP   — if set (e.g. 127.0.0.1:8765), serve HTTP JSON-RPC instead of stdio

use std::io::{self, BufRead, Write};

use engram_mcp::{handle_message, HttpCodeSearch};

fn main() {
    let url = std::env::var("ENGRAM_URL").unwrap_or_else(|_| "http://127.0.0.1:8088".to_string());
    let token = std::env::var("ENGRAM_TOKEN").unwrap_or_else(|_| {
        eprintln!("engram-mcp: ENGRAM_TOKEN not set; requests will be unauthorised");
        String::new()
    });
    let namespace = std::env::var("ENGRAM_NAMESPACE").unwrap_or_else(|_| {
        eprintln!("engram-mcp: ENGRAM_NAMESPACE not set; defaulting to \"default\"");
        "default".to_string()
    });

    let backend = HttpCodeSearch {
        url,
        token,
        namespace,
        client: reqwest::blocking::Client::new(),
    };

    match std::env::var("ENGRAM_MCP_HTTP") {
        Ok(addr) if !addr.is_empty() => serve_http(&addr, &backend),
        _ => serve_stdio(&backend),
    }
}

/// Newline-delimited JSON-RPC over stdin/stdout.
fn serve_stdio(backend: &HttpCodeSearch) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("engram-mcp: stdin read error: {e}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(resp) = handle_message(&line, backend) {
            if writeln!(out, "{resp}").is_err() {
                break;
            }
            out.flush().ok();
        }
    }
}

/// Minimal HTTP JSON-RPC transport: POST one JSON-RPC request, get the JSON-RPC response.
fn serve_http(addr: &str, backend: &HttpCodeSearch) {
    let server = match tiny_http::Server::http(addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("engram-mcp: HTTP bind {addr} failed: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("engram-mcp: HTTP JSON-RPC on http://{addr}/ (POST a JSON-RPC body)");
    for mut req in server.incoming_requests() {
        let mut body = String::new();
        req.as_reader().read_to_string(&mut body).ok();
        let resp_text = handle_message(&body, backend).unwrap_or_default();
        let header =
            tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
        let response = tiny_http::Response::from_string(resp_text).with_header(header);
        req.respond(response).ok();
    }
}
