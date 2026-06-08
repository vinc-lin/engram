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
        .route("/v1/:namespace/tree", post(tree_query))
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
}
