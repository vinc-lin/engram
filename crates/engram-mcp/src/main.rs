// engram-mcp main: newline-delimited JSON-RPC 2.0 stdio transport.
//
// Config (env vars; no flags to keep dependencies minimal):
//   ENGRAM_URL        — default: http://127.0.0.1:8088
//   ENGRAM_TOKEN      — Bearer token (required)
//   ENGRAM_NAMESPACE  — target namespace (required)

use std::io::{self, BufRead, Write};

use engram_mcp::{dispatch, HttpCodeSearch};

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

    let client = reqwest::blocking::Client::new();
    let backend = HttpCodeSearch {
        url,
        token,
        namespace,
        client,
    };

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                eprintln!("engram-mcp: stdin read error: {}", e);
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                // Parse error with no id — skip (spec allows emitting -32700, but we skip
                // because we can't reliably extract an id).
                eprintln!("engram-mcp: JSON parse error (line skipped): {}", e);
                continue;
            }
        };

        if let Some(resp) = dispatch(&req, &backend) {
            let mut bytes = serde_json::to_vec(&resp).expect("response serialisation failed");
            bytes.push(b'\n');
            if let Err(e) = out.write_all(&bytes) {
                eprintln!("engram-mcp: stdout write error: {}", e);
                break;
            }
            out.flush().ok();
        }
    }
}
