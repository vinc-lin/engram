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

/// A consolidated summary-tree node (architecture / module digest).
#[derive(Debug, Clone)]
pub struct Digest {
    pub tree_key: String,
    pub label: Option<String>,
    pub body: String,
    pub score: f64,
}

// ---------------------------------------------------------------------------
// Backend trait + real HTTP impl
// ---------------------------------------------------------------------------

pub trait CodeSearch {
    fn search(&self, query: &str, limit: usize) -> Result<Vec<CodeHit>, String>;
    /// Repo-wide architecture digest (the `global` summary tree).
    fn get_architecture(&self, query: &str, limit: usize) -> Result<Vec<Digest>, String>;
    /// One directory's digest (`path` is a repo-relative directory = the module key).
    fn get_module(&self, path: &str, query: &str, limit: usize) -> Result<Vec<Digest>, String>;
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

/// Wire-shape of a tree digest node (extra fields like node_id/level/tree_kind are ignored).
#[derive(Deserialize)]
struct RawDigest {
    tree_key: String,
    label: Option<String>,
    body: String,
    score: f64,
}

pub struct HttpCodeSearch {
    pub url: String,
    pub token: String,
    pub namespace: String,
    pub client: reqwest::blocking::Client,
}

impl HttpCodeSearch {
    /// POST to a digest endpoint (code/architecture or code/module) and parse tree nodes.
    fn post_digests(&self, suffix: &str, body: Value) -> Result<Vec<Digest>, String> {
        let endpoint = format!("{}/v1/{}/{}", self.url, self.namespace, suffix);
        let resp = self
            .client
            .post(&endpoint)
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!(
                "HTTP {}: {}",
                status,
                resp.text().unwrap_or_default()
            ));
        }
        let raw: Vec<RawDigest> = resp.json().map_err(|e| e.to_string())?;
        Ok(raw
            .into_iter()
            .map(|d| Digest {
                tree_key: d.tree_key,
                label: d.label,
                body: d.body,
                score: d.score,
            })
            .collect())
    }
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

    fn get_architecture(&self, query: &str, limit: usize) -> Result<Vec<Digest>, String> {
        self.post_digests(
            "code/architecture",
            json!({ "query": query, "limit": limit }),
        )
    }

    fn get_module(&self, path: &str, query: &str, limit: usize) -> Result<Vec<Digest>, String> {
        self.post_digests(
            "code/module",
            json!({ "path": path, "query": query, "limit": limit }),
        )
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

pub fn format_digests(items: &[Digest]) -> String {
    if items.is_empty() {
        return "No digest available (the repo may not be consolidated yet).".to_string();
    }
    items
        .iter()
        .map(|d| {
            let head = d.label.clone().unwrap_or_else(|| d.tree_key.clone());
            let body: String = d.body.chars().take(600).collect();
            format!("[{}]  (score {:.2})\n{}", head, d.score, body)
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

fn get_architecture_tool_def() -> Value {
    json!({
        "name": "get_architecture",
        "description": "High-level architecture digest of the indexed repo (the consolidated 'global' summary tree). An optional query focuses it.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "optional focus query" },
                "limit": { "type": "integer", "description": "max digest nodes (default 10)" }
            }
        }
    })
}

fn get_module_tool_def() -> Value {
    json!({
        "name": "get_module",
        "description": "Architecture digest for one directory/module. 'path' is a repo-relative directory (e.g. 'src/state').",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "repo-relative directory (the module key)" },
                "query": { "type": "string", "description": "optional focus query" },
                "limit": { "type": "integer", "description": "max digest nodes (default 10)" }
            },
            "required": ["path"]
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
            "result": { "tools": [search_code_tool_def(), get_architecture_tool_def(), get_module_tool_def()] }
        })),

        "tools/call" => {
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let query = args.get("query").and_then(Value::as_str).unwrap_or("");
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;

            let result: Result<String, String> = match name {
                "search_code" => backend.search(query, limit).map(|h| format_hits(&h)),
                "get_architecture" => backend
                    .get_architecture(query, limit)
                    .map(|d| format_digests(&d)),
                "get_module" => {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                    backend
                        .get_module(path, query, limit)
                        .map(|d| format_digests(&d))
                }
                _ => {
                    return Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32602, "message": "Unknown tool" }
                    }))
                }
            };

            match result {
                Ok(text) => Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{ "type": "text", "text": text }],
                        "isError": false
                    }
                })),
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
        fn get_architecture(&self, _q: &str, _l: usize) -> Result<Vec<Digest>, String> {
            Ok(vec![Digest {
                tree_key: "global".into(),
                label: Some("auth · search · store".into()),
                body: "global architecture digest".into(),
                score: 0.0,
            }])
        }
        fn get_module(&self, path: &str, _q: &str, _l: usize) -> Result<Vec<Digest>, String> {
            Ok(vec![Digest {
                tree_key: path.into(),
                label: Some("module digest".into()),
                body: format!("digest for {path}"),
                score: 0.0,
            }])
        }
    }

    struct ErrorSearch(String);

    impl CodeSearch for ErrorSearch {
        fn search(&self, _query: &str, _limit: usize) -> Result<Vec<CodeHit>, String> {
            Err(self.0.clone())
        }
        fn get_architecture(&self, _q: &str, _l: usize) -> Result<Vec<Digest>, String> {
            Err(self.0.clone())
        }
        fn get_module(&self, _p: &str, _q: &str, _l: usize) -> Result<Vec<Digest>, String> {
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
    fn tools_list_includes_all_three_tools() {
        let req = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
        let resp = dispatch(&req, &no_hits()).unwrap();
        let names: Vec<&str> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"search_code"));
        assert!(names.contains(&"get_architecture"));
        assert!(names.contains(&"get_module"));
    }

    #[test]
    fn tools_call_get_architecture_formats_digest() {
        let req = json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "get_architecture", "arguments": {} }
        });
        let resp = dispatch(&req, &no_hits()).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("global architecture digest"));
    }

    #[test]
    fn tools_call_get_module_passes_path() {
        let req = json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": { "name": "get_module", "arguments": { "path": "src/state" } }
        });
        let resp = dispatch(&req, &no_hits()).unwrap();
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("digest for src/state"));
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
