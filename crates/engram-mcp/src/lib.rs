// engram-mcp: MCP stdio server exposing search_code.
//
// Phase 1: stdio (newline-delimited JSON-RPC 2.0) transport + search_code tool only.
// Future phases: streamable-HTTP transport; tools get_architecture, why, find_symbol.

use serde::Deserialize;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CodeHit {
    pub path: String,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub snippet: String,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Backend trait + real HTTP impl
// ---------------------------------------------------------------------------

pub trait CodeSearch {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<CodeHit>, String>;
}

/// Wire-shape returned by the engram code-search endpoint.
#[derive(Deserialize)]
struct RawHit {
    path: String,
    line_start: Option<i64>,
    line_end: Option<i64>,
    snippet: String,
    score: f64,
}

pub struct HttpCodeSearch {
    pub url: String,
    pub token: String,
    pub namespace: String,
    pub client: reqwest::blocking::Client,
}

impl CodeSearch for HttpCodeSearch {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<CodeHit>, String> {
        let endpoint = format!("{}/v1/{}/code/search", self.url, self.namespace);
        let body = json!({ "query": query, "limit": limit });
        let resp = self
            .client
            .post(&endpoint)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().unwrap_or_default();
            return Err(format!("HTTP {}: {}", status, text));
        }

        let raw: Vec<RawHit> = resp.json().map_err(|e| e.to_string())?;
        Ok(raw
            .into_iter()
            .map(|h| CodeHit {
                path: h.path,
                line_start: h.line_start,
                line_end: h.line_end,
                snippet: h.snippet,
                score: h.score,
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

pub fn format_hits(hits: &[CodeHit]) -> String {
    if hits.is_empty() {
        return "No results.".to_string();
    }
    hits.iter()
        .map(|h| {
            let loc = match (h.line_start, h.line_end) {
                (Some(s), Some(e)) => format!("{}:{}-{}", h.path, s, e),
                (Some(s), None) => format!("{}:{}", h.path, s),
                _ => h.path.clone(),
            };
            let snippet: String = h.snippet.chars().take(400).collect();
            format!("{}  (score {:.2})\n{}", loc, h.score, snippet)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// Tool definition (constant, shared between list and dispatch)
// ---------------------------------------------------------------------------

fn search_code_tool_def() -> Value {
    json!({
        "name": "search_code",
        "description": "Semantic + keyword code search over an indexed repo. Returns ranked path:line locations with snippets.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "natural-language or keyword query"
                },
                "limit": {
                    "type": "integer",
                    "description": "max results (default 10)"
                }
            },
            "required": ["query"]
        }
    })
}

// ---------------------------------------------------------------------------
// Dispatch — the unit-tested core
// ---------------------------------------------------------------------------

/// Handle one parsed JSON-RPC message.
/// Returns `Some(response)` for requests; `None` for notifications (no `id`).
pub fn dispatch(req: &Value, backend: &dyn CodeSearch) -> Option<Value> {
    // Notifications have no `id` → produce no response.
    let id = req.get("id")?;

    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    match method {
        "initialize" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "engram-mcp", "version": "0.1.0" }
            }
        })),

        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": [search_code_tool_def()] }
        })),

        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");

            if name != "search_code" {
                return Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32602, "message": "Unknown tool" }
                }));
            }

            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;

            match backend.search(query, limit) {
                Ok(hits) => {
                    let text = format_hits(&hits);
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": text }],
                            "isError": false
                        }
                    }))
                }
                Err(e) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": e }],
                        "isError": true
                    }
                })),
            }
        }

        _ => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "Method not found" }
        })),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Fake backends ---

    struct FakeSearch(Vec<CodeHit>);

    impl CodeSearch for FakeSearch {
        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<CodeHit>, String> {
            Ok(self.0.clone())
        }
    }

    struct ErrorSearch(String);

    impl CodeSearch for ErrorSearch {
        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<CodeHit>, String> {
            Err(self.0.clone())
        }
    }

    fn fake_hit(path: &str, line_start: i64, line_end: i64, snippet: &str, score: f64) -> CodeHit {
        CodeHit {
            path: path.to_string(),
            line_start: Some(line_start),
            line_end: Some(line_end),
            snippet: snippet.to_string(),
            score,
        }
    }

    fn no_hits() -> FakeSearch {
        FakeSearch(vec![])
    }

    // --- dispatch tests ---

    #[test]
    fn initialize_returns_protocol_and_serverinfo() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        });
        let resp = dispatch(&req, &no_hits()).unwrap();
        let result = &resp["result"];
        assert_eq!(result["protocolVersion"], "2024-11-05");
        assert_eq!(result["serverInfo"]["name"], "engram-mcp");
    }

    #[test]
    fn tools_list_includes_search_code() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let resp = dispatch(&req, &no_hits()).unwrap();
        let tools = &resp["result"]["tools"];
        assert!(tools.is_array());
        let tool = &tools[0];
        assert_eq!(tool["name"], "search_code");
        let required = &tool["inputSchema"]["required"];
        assert!(
            required.as_array().unwrap().iter().any(|v| v == "query"),
            "inputSchema.required must contain 'query'"
        );
    }

    #[test]
    fn tools_call_search_code_formats_hits() {
        let hits = vec![
            fake_hit("src/foo.rs", 10, 20, "fn foo() { }", 0.92),
            fake_hit("src/bar.rs", 5, 7, "fn bar() { }", 0.75),
        ];
        let req = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": { "query": "foo bar", "limit": 5 }
            }
        });
        let resp = dispatch(&req, &FakeSearch(hits)).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("src/foo.rs:10-20"),
            "should contain first path:line"
        );
        assert!(
            text.contains("src/bar.rs:5-7"),
            "should contain second path:line"
        );
    }

    #[test]
    fn tools_call_backend_error_is_iserror() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "search_code",
                "arguments": { "query": "boom" }
            }
        });
        let resp = dispatch(&req, &ErrorSearch("connection refused".into())).unwrap();
        assert_eq!(resp["result"]["isError"], true);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("connection refused"));
    }

    #[test]
    fn unknown_method_returns_error() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "foo/bar",
            "params": {}
        });
        let resp = dispatch(&req, &no_hits()).unwrap();
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[test]
    fn notification_returns_none() {
        // notifications/initialized has no "id" field
        let req = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        assert!(dispatch(&req, &no_hits()).is_none());
    }

    #[test]
    fn unknown_tool_name_returns_invalid_params_error() {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "does_not_exist", "arguments": {} }
        });
        let resp = dispatch(&req, &no_hits()).unwrap();
        assert_eq!(resp["error"]["code"], -32602);
    }

    // --- format_hits tests ---

    #[test]
    fn format_hits_empty_returns_no_results() {
        assert_eq!(format_hits(&[]), "No results.");
    }

    #[test]
    fn format_hits_nonempty_contains_path_line_and_score() {
        let hits = vec![fake_hit("src/lib.rs", 1, 5, "pub fn entry()", 0.88)];
        let s = format_hits(&hits);
        assert!(s.contains("src/lib.rs:1-5"), "expected path:line range");
        assert!(s.contains("0.88"), "expected score");
        assert!(s.contains("pub fn entry()"), "expected snippet");
    }

    #[test]
    fn format_hits_truncates_long_snippet() {
        let long_snippet: String = "x".repeat(500);
        let hits = vec![CodeHit {
            path: "a.rs".into(),
            line_start: None,
            line_end: None,
            snippet: long_snippet,
            score: 0.5,
        }];
        let s = format_hits(&hits);
        // snippet portion should be at most 400 chars
        // the line before the snippet is "a.rs  (score 0.50)\n"
        let snippet_part = s.split('\n').nth(1).unwrap_or("");
        assert!(
            snippet_part.chars().count() <= 400,
            "snippet should be truncated to 400 chars"
        );
    }
}
