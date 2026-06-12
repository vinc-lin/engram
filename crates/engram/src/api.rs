use crate::embed::Embedder;
use crate::model::NewDoc;
use crate::retrieve;
use crate::store::Store;
use axum::{
    extract::{Path, Query, Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub token: String,
    pub embedder: Arc<dyn Embedder>,
    pub chat: Arc<dyn crate::llm::ChatClient>,
}

async fn health() -> &'static str {
    "ok"
}

pub fn app(state: AppState) -> Router {
    let protected = Router::new()
        .route("/v1/:namespace/docs", get(list_docs).post(ingest_doc))
        .route("/v1/:namespace/docs/:id", get(get_doc))
        .route(
            "/v1/:namespace/docs/by-key/:key",
            get(get_doc_by_key).delete(forget_doc_by_key),
        )
        .route("/v1/:namespace/query", post(query_docs))
        .route("/v1/:namespace/code/search", post(search_code_docs))
        .route("/v1/:namespace/recall", get(recall_docs))
        .route("/v1/:namespace/code/graph/callers", post(graph_callers))
        .route("/v1/:namespace/code/graph/callees", post(graph_callees))
        .route("/v1/:namespace/tree", post(tree_query))
        .route("/v1/:namespace/code/architecture", post(get_architecture))
        .route(
            "/v1/:namespace/code/architecture/rebuild",
            post(rebuild_architecture_handler),
        )
        .route("/v1/:namespace/code/module", post(get_module))
        .route(
            "/v1/:namespace/conventions/rebuild",
            post(rebuild_conventions_handler),
        )
        .route("/v1/:namespace/conventions", get(get_conventions_handler))
        .route_layer(middleware::from_fn_with_state(state.clone(), auth));

    Router::new()
        .route("/healthz", get(health))
        .merge(protected)
        .with_state(state)
}

async fn auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let ok = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == state.token)
        .unwrap_or(false);

    if !ok {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    next.run(request).await
}

async fn ingest_doc(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(new): Json<NewDoc>,
) -> Response {
    // Ingest embeds chunks via a blocking HTTP client — run it off the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        crate::ingest::ingest_document(&state.store, state.embedder.as_ref(), &namespace, &new)
    })
    .await;
    match result {
        Ok(Ok(doc)) => (StatusCode::CREATED, Json(doc)).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_doc(
    State(state): State<AppState>,
    Path((namespace, id)): Path<(String, String)>,
) -> Response {
    match state.store.get_doc(&namespace, &id) {
        Ok(Some(doc)) => Json(doc).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn get_doc_by_key(
    State(state): State<AppState>,
    Path((namespace, key)): Path<(String, String)>,
) -> Response {
    match state.store.get_by_key(&namespace, &key) {
        Ok(Some(doc)) => Json(doc).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn forget_doc_by_key(
    State(state): State<AppState>,
    Path((namespace, key)): Path<(String, String)>,
) -> Response {
    match state.store.delete_doc_by_key(&namespace, &key) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn list_docs(State(state): State<AppState>, Path(namespace): Path<String>) -> Response {
    match state.store.list_namespace(&namespace, 100) {
        Ok(docs) => Json(docs).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct QueryReq {
    query: String,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct GraphReq {
    sym: Option<String>,
    path: Option<String>,
    max_depth: Option<usize>,
    limit: Option<usize>,
    min_confidence: Option<f32>,
}

async fn query_docs(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<QueryReq>,
) -> Response {
    // query embeds the query text via a blocking HTTP client — run it off the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        retrieve::query(
            &state.store,
            state.embedder.as_ref(),
            &namespace,
            &req.query,
            req.limit.unwrap_or(10),
        )
    })
    .await;
    match result {
        Ok(Ok(hits)) => Json(hits).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn search_code_docs(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<QueryReq>,
) -> Response {
    // search_code embeds the query via a blocking HTTP client — run it off the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        retrieve::search_code(
            &state.store,
            state.embedder.as_ref(),
            &namespace,
            &req.query,
            req.limit.unwrap_or(10),
        )
    })
    .await;
    match result {
        Ok(Ok(hits)) => Json(hits).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct RecallParams {
    limit: Option<usize>,
}

async fn recall_docs(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Query(params): Query<RecallParams>,
) -> Response {
    match retrieve::recall(&state.store, &namespace, params.limit.unwrap_or(10)) {
        Ok(hits) => Json(hits).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn graph_callers(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<GraphReq>,
) -> Response {
    // graph_query reads only SQLite — no embed/LLM calls, no spawn_blocking needed.
    let sym = match req.sym {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, "sym is required").into_response(),
    };
    // Edges store dst_sym as "sym:<Name>"; accept a bare name from callers and normalize so
    // both "process_doc" and "sym:process_doc" resolve to the same edges.
    let sym = if sym.starts_with("sym:") {
        sym
    } else {
        format!("sym:{sym}")
    };
    let max_depth = req
        .max_depth
        .unwrap_or(2)
        .min(crate::config::code_graph_max_depth());
    let limit = req.limit.unwrap_or(20);
    let min_conf = req.min_confidence.unwrap_or(0.0);
    match crate::graph_query::callers(&state.store, &namespace, &sym, max_depth, limit) {
        Ok(mut hops) => {
            if min_conf > 0.0 {
                hops.retain(|h| h.confidence >= min_conf);
            }
            Json(hops).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn graph_callees(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<GraphReq>,
) -> Response {
    let path = match req.path {
        Some(p) => p,
        None => return (StatusCode::BAD_REQUEST, "path is required").into_response(),
    };
    let max_depth = req
        .max_depth
        .unwrap_or(2)
        .min(crate::config::code_graph_max_depth());
    let limit = req.limit.unwrap_or(20);
    let min_conf = req.min_confidence.unwrap_or(0.0);
    match crate::graph_query::callees(&state.store, &namespace, &path, max_depth, limit) {
        Ok(mut hops) => {
            if min_conf > 0.0 {
                hops.retain(|h| h.confidence >= min_conf);
            }
            Json(hops).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct TreeReq {
    query: String,
    tree_kind: Option<String>,
    tree_key: Option<String>,
    max_depth: Option<usize>,
    limit: Option<usize>,
}

async fn tree_query(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<TreeReq>,
) -> Response {
    // drill_down embeds the query text via a blocking HTTP client — run it off the async runtime.
    let result = tokio::task::spawn_blocking(move || {
        retrieve::drill_down(
            &state.store,
            state.embedder.as_ref(),
            &namespace,
            &req.query,
            req.tree_kind.as_deref(),
            req.tree_key.as_deref(),
            req.max_depth.unwrap_or(3),
            req.limit.unwrap_or(10),
        )
    })
    .await;
    match result {
        Ok(Ok(hits)) => Json(hits).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ArchReq {
    query: Option<String>,
    depth: Option<usize>,
    limit: Option<usize>,
}

/// The repo architecture digest. Serves the cached **single-pass** digest from `<ns>:meta` (key
/// `architecture`, built by `POST .../architecture/rebuild`) when present — the digest path no
/// longer relies on the consolidation-tree fold. Falls back to a `global`-tree drill only when no
/// single-pass digest has been built (backward compatibility).
async fn get_architecture(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<ArchReq>,
) -> Response {
    let result =
        tokio::task::spawn_blocking(move || -> crate::error::Result<Vec<retrieve::TreeHit>> {
            let meta_ns = crate::conventions::meta_namespace(&namespace);
            if let Some(doc) = state.store.get_by_key(&meta_ns, "architecture")? {
                return Ok(vec![retrieve::digest_hit(doc, "architecture".to_string())]);
            }
            retrieve::drill_down(
                &state.store,
                state.embedder.as_ref(),
                &namespace,
                req.query.as_deref().unwrap_or(""),
                Some("global"),
                Some("global"),
                req.depth.unwrap_or(3),
                req.limit.unwrap_or(10),
            )
        })
        .await;
    match result {
        Ok(Ok(hits)) => Json(hits).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ModuleReq {
    path: String,
    query: Option<String>,
    depth: Option<usize>,
    limit: Option<usize>,
}

/// One directory's digest — `path` is the module key (a repo-relative directory). Serves the cached
/// single-pass digest from `<ns>:meta` (key `module:<path>`) when present, else falls back to a
/// `module`-tree drill.
async fn get_module(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<ModuleReq>,
) -> Response {
    let result =
        tokio::task::spawn_blocking(move || -> crate::error::Result<Vec<retrieve::TreeHit>> {
            let meta_ns = crate::conventions::meta_namespace(&namespace);
            let key = format!("module:{}", req.path.trim_end_matches('/'));
            if let Some(doc) = state.store.get_by_key(&meta_ns, &key)? {
                return Ok(vec![retrieve::digest_hit(doc, key)]);
            }
            retrieve::drill_down(
                &state.store,
                state.embedder.as_ref(),
                &namespace,
                req.query.as_deref().unwrap_or(""),
                Some("module"),
                Some(&req.path),
                req.depth.unwrap_or(3),
                req.limit.unwrap_or(10),
            )
        })
        .await;
    match result {
        Ok(Ok(hits)) => Json(hits).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct DigestRebuildReq {
    /// `None` = whole-repo architecture digest; `Some(dir)` = a module digest scoped to a directory.
    module: Option<String>,
    max_tokens: Option<usize>,
}

/// Build the **single-pass** architecture digest (whole-repo, or a module when `module` is set) and
/// cache it at `<ns>:meta` (LLM, off the runtime). This is the digest path's replacement for the
/// consolidation-tree fold — see `eval/RESULTS_distillation.md`.
async fn rebuild_architecture_handler(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
    Json(req): Json<DigestRebuildReq>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        crate::conventions::rebuild_architecture_digest(
            &state.store,
            state.chat.as_ref(),
            state.embedder.as_ref(),
            &namespace,
            req.module.as_deref(),
            req.max_tokens.unwrap_or(2000),
        )
    })
    .await;
    match result {
        Ok(Ok(doc)) => Json(doc).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Phase 3b: (re)build the conventions doc from config files + digests (LLM, off the runtime).
async fn rebuild_conventions_handler(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        crate::conventions::rebuild_conventions(
            &state.store,
            state.chat.as_ref(),
            state.embedder.as_ref(),
            &namespace,
            2000,
        )
    })
    .await;
    match result {
        Ok(Ok(doc)) => Json(doc).into_response(),
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Phase 3b: read the conventions doc from the `<ns>:meta` namespace.
async fn get_conventions_handler(
    State(state): State<AppState>,
    Path(namespace): Path<String>,
) -> Response {
    let meta_ns = crate::conventions::meta_namespace(&namespace);
    match state.store.get_by_key(&meta_ns, "conventions") {
        Ok(Some(doc)) => Json(doc).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no conventions (run rebuild)").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Like `spawn`, but returns the store + embedder so a test can drive the cold pipeline
    /// synchronously (deterministic, no background worker timing).
    pub async fn spawn_full() -> (
        String,
        Store,
        std::sync::Arc<crate::embed::HashEmbedder>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open(path.to_str().unwrap()).unwrap();
        let embedder = std::sync::Arc::new(crate::embed::HashEmbedder::new(64));
        let state = AppState {
            store: store.clone(),
            token: "secret".into(),
            embedder: embedder.clone(),
            chat: std::sync::Arc::new(crate::llm::FakeChatClient::ok(
                "- use rustfmt for formatting (evidence: rustfmt.toml)",
            )),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });
        (format!("http://{}", addr), store, embedder, dir)
    }

    #[tokio::test]
    async fn tree_endpoint_drills_global() {
        let (base, store, embedder, _d) = spawn_full().await;
        for (k, c) in [
            ("d1", "rust memory service"),
            ("d2", "bananas grow on trees"),
        ] {
            let new = crate::model::NewDoc {
                key: k.into(),
                title: k.into(),
                content: c.into(),
                author: "a".into(),
                taint: crate::model::Taint::Internal,
                meta: None,
            };
            crate::ingest::ingest_document(&store, embedder.as_ref(), "alice", &new).unwrap();
        }
        let proc = crate::tree::TreeProcessor {
            embedder: embedder.clone(),
            chat: std::sync::Arc::new(crate::llm::FakeChatClient::ok("S")),
            audit: std::sync::Arc::new(crate::llm::NullAuditSink),
            cfg: crate::config::Config::from_vars(|_| None),
            vault: None,
        };
        while crate::jobs::worker_tick(&store, &proc, 5).unwrap() {}

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/v1/alice/tree"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"query": "rust memory"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let hits: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0]["body"].as_str().unwrap().contains("rust"));
    }

    #[tokio::test]
    async fn architecture_and_module_endpoints_return_digests() {
        let (base, store, embedder, _d) = spawn_full().await;
        for (k, c) in [
            (
                "src/state/search.rs",
                "fn bm25_score() {}\nfn idf_term() {}",
            ),
            (
                "src/state/vector.rs",
                "fn cosine_sim() {}\nfn dot_prod() {}",
            ),
        ] {
            let new = crate::model::NewDoc {
                key: k.into(),
                title: k.into(),
                content: c.into(),
                author: "code".into(),
                taint: crate::model::Taint::Internal,
                meta: Some(serde_json::json!({ "kind": "file" })),
            };
            crate::ingest::ingest_document(&store, embedder.as_ref(), "repo:demo", &new).unwrap();
        }
        // Build the code trees (consolidation gated on for code).
        let mut cfg = crate::config::Config::from_vars(|_| None);
        cfg.consolidate_code = true;
        let proc = crate::tree::TreeProcessor {
            embedder: embedder.clone(),
            chat: std::sync::Arc::new(crate::llm::FakeChatClient::ok("S")),
            audit: std::sync::Arc::new(crate::llm::NullAuditSink),
            cfg,
            vault: None,
        };
        while crate::jobs::worker_tick(&store, &proc, 5).unwrap() {}

        let client = reqwest::Client::new();
        // get_architecture (no query) → the global digest.
        let resp = client
            .post(format!("{base}/v1/repo:demo/code/architecture"))
            .bearer_auth("secret")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let arch: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(!arch.is_empty(), "architecture digest should be non-empty");

        // get_module for the src/state directory.
        let resp = client
            .post(format!("{base}/v1/repo:demo/code/module"))
            .bearer_auth("secret")
            .json(&serde_json::json!({ "path": "src/state" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let m: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(
            !m.is_empty(),
            "module digest for src/state should be non-empty"
        );
        assert!(m.iter().all(|h| h["tree_kind"] == "module"));
    }

    #[tokio::test]
    async fn conventions_rebuild_and_get() {
        let (base, store, embedder, _d) = spawn_full().await;
        let new = crate::model::NewDoc {
            key: "rustfmt.toml".into(),
            title: "rustfmt.toml".into(),
            content: "max_width = 100\n".into(),
            author: "code".into(),
            taint: crate::model::Taint::Internal,
            meta: Some(serde_json::json!({ "kind": "file" })),
        };
        crate::ingest::ingest_document(&store, embedder.as_ref(), "repo:demo", &new).unwrap();

        let client = reqwest::Client::new();
        // Before rebuild → 404.
        let resp = client
            .get(format!("{base}/v1/repo:demo/conventions"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        // Rebuild (uses the FakeChatClient wired into spawn_full).
        let resp = client
            .post(format!("{base}/v1/repo:demo/conventions/rebuild"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // Now GET returns the conventions doc.
        let resp = client
            .get(format!("{base}/v1/repo:demo/conventions"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let doc: serde_json::Value = resp.json().await.unwrap();
        assert!(doc["content"].as_str().unwrap().contains("rustfmt"));
    }

    #[tokio::test]
    async fn code_search_endpoint_returns_path_line() {
        let (base, store, embedder, _d) = spawn_full().await;
        for (k, c) in [
            (
                "store.rs",
                "fn acquire_write_lock(&self) {\n    let guard = self.write.lock();\n    // single writer mutex connection\n}",
            ),
            (
                "embed.rs",
                "fn embed(text: &str) -> Vec<f32> {\n    hash_tokens_into_vector(text)\n}",
            ),
        ] {
            let new = crate::model::NewDoc {
                key: k.into(),
                title: k.into(),
                content: c.into(),
                author: "a".into(),
                taint: crate::model::Taint::Internal,
                meta: Some(serde_json::json!({ "kind": "file" })),
            };
            crate::ingest::ingest_document(&store, embedder.as_ref(), "repo:demo", &new).unwrap();
        }

        let client = reqwest::Client::new();
        let resp = client
            .post(format!("{base}/v1/repo:demo/code/search"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"query": "acquire write lock mutex"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let hits: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(!hits.is_empty(), "expected at least one code hit");
        // Top hit is the file we targeted, and line ranges survive the round-trip.
        assert_eq!(hits[0]["path"].as_str().unwrap(), "store.rs");
        assert!(hits[0]["line_start"].is_number());
        assert!(hits[0]["line_end"].is_number());
    }

    pub async fn spawn() -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let store = Store::open(path.to_str().unwrap()).unwrap();
        let state = AppState {
            store,
            token: "secret".into(),
            embedder: std::sync::Arc::new(crate::embed::HashEmbedder::new(64)),
            chat: std::sync::Arc::new(crate::llm::FakeChatClient::ok("conventions")),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app(state)).await.unwrap();
        });
        (format!("http://{}", addr), dir)
    }

    #[tokio::test]
    async fn healthz_ok() {
        let (base, _d) = spawn().await;
        let resp = reqwest::get(format!("{base}/healthz")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn ingest_then_fetch() {
        let (base, _d) = spawn().await;
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "key": "k1", "title": "T", "content": "Hello", "author": "alice"
        });

        let created = client
            .post(format!("{base}/v1/alice/docs"))
            .bearer_auth("secret")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), 201);
        let doc: serde_json::Value = created.json().await.unwrap();
        let id = doc["document_id"].as_str().unwrap().to_string();
        assert_eq!(doc["namespace"], "alice");

        let got = client
            .get(format!("{base}/v1/alice/docs/{id}"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(got.status(), 200);
        assert_eq!(
            got.json::<serde_json::Value>().await.unwrap()["content"],
            "Hello"
        );

        let list = client
            .get(format!("{base}/v1/alice/docs"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        let arr: Vec<serde_json::Value> = list.json().await.unwrap();
        assert_eq!(arr.len(), 1);

        // unknown id → 404
        let miss = client
            .get(format!("{base}/v1/alice/docs/nope"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(miss.status(), 404);
    }

    #[tokio::test]
    async fn auth_rejects_and_accepts() {
        let (base, _d) = spawn().await;
        let url = format!("{base}/v1/alice/docs");
        let client = reqwest::Client::new();

        // no token → 401
        let r1 = client.get(&url).send().await.unwrap();
        assert_eq!(r1.status(), 401);

        // wrong token → 401
        let r2 = client.get(&url).bearer_auth("nope").send().await.unwrap();
        assert_eq!(r2.status(), 401);

        // right token → not 401
        let r3 = client.get(&url).bearer_auth("secret").send().await.unwrap();
        assert_ne!(r3.status(), 401);
    }

    #[tokio::test]
    async fn query_endpoint_ranks() {
        let (base, _d) = spawn().await;
        let client = reqwest::Client::new();
        for (k, c) in [("d1", "rust memory service"), ("d2", "bananas and trees")] {
            client
                .post(format!("{base}/v1/alice/docs"))
                .bearer_auth("secret")
                .json(&serde_json::json!({"key": k, "title": k, "content": c, "author": "a"}))
                .send()
                .await
                .unwrap();
        }
        let resp = client
            .post(format!("{base}/v1/alice/query"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"query": "rust memory"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let hits: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["key"], "d1");
        assert!(hits[0]["score"].as_f64().unwrap() >= hits[1]["score"].as_f64().unwrap());
    }

    #[tokio::test]
    async fn query_endpoint_graph_boost() {
        let (base, _d) = spawn().await;
        let client = reqwest::Client::new();
        for (k, c) in [
            ("d1", "weekly release ping @carol"),
            ("d2", "weekly release notes"),
        ] {
            let r = client
                .post(format!("{base}/v1/alice/docs"))
                .bearer_auth("secret")
                .json(&serde_json::json!({"key": k, "title": k, "content": c, "author": "a"}))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 201);
        }
        let resp = client
            .post(format!("{base}/v1/alice/query"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"query": "release @carol"}))
            .send()
            .await
            .unwrap();
        let hits: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(hits[0]["key"], "d1");
        assert!(hits[0]["graph"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn meta_and_by_key_get_delete() {
        let (base, _d) = spawn().await;
        let client = reqwest::Client::new();
        // POST a doc with meta
        let r = client
            .post(format!("{base}/v1/alice/docs"))
            .bearer_auth("secret")
            .json(
                &serde_json::json!({"key":"m1","title":"t","content":"alpha","author":"op",
                                      "meta":{"category":"fact","importance":0.9}}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        assert_eq!(
            r.json::<serde_json::Value>().await.unwrap()["meta"]["category"],
            "fact"
        );

        // by-key GET returns the meta
        let g = client
            .get(format!("{base}/v1/alice/docs/by-key/m1"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(g.status(), 200);
        assert_eq!(
            g.json::<serde_json::Value>().await.unwrap()["meta"]["importance"],
            0.9
        );

        // query also carries meta through
        let q = client
            .post(format!("{base}/v1/alice/query"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"query":"alpha"}))
            .send()
            .await
            .unwrap();
        let hits: Vec<serde_json::Value> = q.json().await.unwrap();
        assert_eq!(hits[0]["meta"]["category"], "fact");

        // DELETE by-key → 204, then gone
        let d = client
            .delete(format!("{base}/v1/alice/docs/by-key/m1"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(d.status(), 204);
        let g2 = client
            .get(format!("{base}/v1/alice/docs/by-key/m1"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        assert_eq!(g2.status(), 404);
    }

    #[tokio::test]
    async fn recall_endpoint_recent_first() {
        let (base, _d) = spawn().await;
        let client = reqwest::Client::new();
        for (k, c) in [("d1", "first"), ("d2", "second")] {
            client
                .post(format!("{base}/v1/alice/docs"))
                .bearer_auth("secret")
                .json(&serde_json::json!({"key": k, "title": k, "content": c, "author": "a"}))
                .send()
                .await
                .unwrap();
        }
        let resp = client
            .get(format!("{base}/v1/alice/recall?limit=10"))
            .bearer_auth("secret")
            .send()
            .await
            .unwrap();
        let hits: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert_eq!(hits[0]["key"], "d2");
    }

    /// End-to-end code-graph test. Seeds a 3-file caller→callee graph directly via
    /// `store.commit_ingest(.., &raw_edges, ..)` with hand-built `RawEdge`s (NOT via the
    /// `ENGRAM_CODE_GRAPH_EXTRACT` env gate — that gate is a process-global `OnceLock`, so toggling
    /// it in-process is flaky across the test binary; the gated ingest→extract path is validated
    /// separately by graph_eval.py in a fresh process). Then hits the live `/code/graph/callers`
    /// and `/code/graph/callees` HTTP endpoints and asserts the right paths/hops come back,
    /// including a cross-file caller-of-caller resolved through `chunk_entities`.
    #[tokio::test]
    async fn graph_callers_and_callees_end_to_end() {
        use crate::embed::Embedder;
        use crate::model::{EdgeKind, NewDoc, RawEdge, Taint};

        let (base, store, embedder, _d) = spawn_full().await;
        let sig = embedder.signature();

        // Helper: ingest one code file with its defining sym entity + outbound edges.
        let seed = |key: &str, content: &str, def_sym: &str, edges: Vec<RawEdge>| {
            let new = NewDoc {
                key: key.into(),
                title: key.into(),
                content: content.into(),
                author: "code".into(),
                taint: Taint::Internal,
                meta: Some(serde_json::json!({ "kind": "file" })),
            };
            let emb_vec = embedder.embed(content).unwrap();
            store
                .commit_ingest(
                    "ns",
                    &new,
                    &[content.to_string()],
                    &[emb_vec],
                    &[vec![def_sym.to_string()]],
                    &[Some((1, 1))],
                    &edges,
                    &sig,
                )
                .unwrap()
        };

        // leaf.rs defines `leaf`, calls nothing.
        let leaf_doc = seed("src/leaf.rs", "fn leaf() {}", "sym:leaf", vec![]);
        // middle.rs defines `middle`, calls `leaf` (cross-file edge → resolves to leaf.rs).
        let _middle_doc = seed(
            "src/middle.rs",
            "fn middle() { leaf(); }",
            "sym:middle",
            vec![RawEdge {
                dst_sym: "sym:leaf".into(),
                edge_kind: EdgeKind::Calls,
                src_line: Some(1),
                confidence: 0.9,
            }],
        );
        // top.rs defines `top`, calls `middle` (so top is a caller-of-caller of leaf).
        let _top_doc = seed(
            "src/top.rs",
            "fn top() { middle(); }",
            "sym:top",
            vec![RawEdge {
                dst_sym: "sym:middle".into(),
                edge_kind: EdgeKind::Calls,
                src_line: Some(1),
                confidence: 0.9,
            }],
        );

        let client = reqwest::Client::new();

        // callers(sym:leaf, depth=2): direct caller middle.rs (depth 1) plus the cross-file
        // caller-of-caller top.rs (depth 2), reached by expanding middle.rs's own sym: defs.
        let resp = client
            .post(format!("{base}/v1/ns/code/graph/callers"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"sym": "sym:leaf", "max_depth": 2, "limit": 10}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let hops: Vec<serde_json::Value> = resp.json().await.unwrap();
        let paths: Vec<&str> = hops.iter().map(|h| h["path"].as_str().unwrap()).collect();
        assert!(
            paths.contains(&"src/middle.rs"),
            "expected direct caller src/middle.rs; got {paths:?}"
        );
        assert!(
            paths.contains(&"src/top.rs"),
            "expected cross-file caller-of-caller src/top.rs; got {paths:?}"
        );
        // The direct caller hop is at depth 1, the caller-of-caller at depth 2.
        let middle_hop = hops.iter().find(|h| h["path"] == "src/middle.rs").unwrap();
        assert_eq!(middle_hop["depth"].as_u64().unwrap(), 1);
        assert_eq!(middle_hop["edge_kind"].as_str().unwrap(), "CALLS");
        let top_hop = hops.iter().find(|h| h["path"] == "src/top.rs").unwrap();
        assert_eq!(top_hop["depth"].as_u64().unwrap(), 2);

        // min_confidence filter is post-BFS: 0.95 drops the 0.9 edges → empty.
        let resp = client
            .post(format!("{base}/v1/ns/code/graph/callers"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"sym": "sym:leaf", "max_depth": 2, "min_confidence": 0.95}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let filtered: Vec<serde_json::Value> = resp.json().await.unwrap();
        assert!(
            filtered.is_empty(),
            "min_confidence 0.95 should drop the 0.9 caller edges; got {filtered:?}"
        );

        // BARE sym name (no "sym:" prefix) must be normalized server-side and resolve to the
        // same callers as the prefixed form — previously this returned silently-empty results.
        let resp = client
            .post(format!("{base}/v1/ns/code/graph/callers"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"sym": "leaf", "max_depth": 2, "limit": 10}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bare_hops: Vec<serde_json::Value> = resp.json().await.unwrap();
        let bare_paths: Vec<&str> = bare_hops
            .iter()
            .map(|h| h["path"].as_str().unwrap())
            .collect();
        assert!(
            bare_paths.contains(&"src/middle.rs"),
            "bare sym 'leaf' should resolve the direct caller src/middle.rs; got {bare_paths:?}"
        );

        // missing `sym` → 400.
        let resp = client
            .post(format!("{base}/v1/ns/code/graph/callers"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"max_depth": 2}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // callees(src/middle.rs): outbound CALLS to sym:leaf, resolved cross-file via
        // chunk_entities to leaf.rs's document_id.
        let resp = client
            .post(format!("{base}/v1/ns/code/graph/callees"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"path": "src/middle.rs", "max_depth": 2, "limit": 10}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let hops: Vec<serde_json::Value> = resp.json().await.unwrap();
        let leaf_hop = hops
            .iter()
            .find(|h| h["sym"] == "sym:leaf")
            .expect("expected callee sym:leaf");
        assert_eq!(
            leaf_hop["path"].as_str().unwrap(),
            "src/leaf.rs",
            "callee should resolve cross-file to leaf.rs"
        );
        assert_eq!(
            leaf_hop["document_id"].as_str().unwrap(),
            leaf_doc.document_id,
            "callee document_id should be leaf.rs's doc id (resolved via chunk_entities)"
        );

        // missing `path` → 400.
        let resp = client
            .post(format!("{base}/v1/ns/code/graph/callees"))
            .bearer_auth("secret")
            .json(&serde_json::json!({"max_depth": 2}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }
}
