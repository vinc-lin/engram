use engram::api::{app, AppState};
use engram::config::Config;
use engram::jobs::spawn_workers;
use engram::llm::{GatewayChatClient, HttpAuditSink};
use engram::store::Store;
use engram::tree::{spawn_sweeper, TreeProcessor};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let cfg = Config::from_env();
    let store = Store::open(&cfg.db_path)?;
    store.requeue_running()?; // crash recovery

    // Embeddings route through the litellm gateway (same as summaries): centralized
    // keys/egress, audited, and reachable from WSL. (OllamaEmbedder remains in
    // `embed` for direct same-host use / fallback.)
    let embedder = Arc::new(engram::embed::GatewayEmbedder::new(
        cfg.gateway_url.clone(),
        cfg.gateway_key.clone(),
        cfg.embed_model.clone(),
        cfg.embed_dim,
    ));

    let processor = Arc::new(TreeProcessor {
        embedder: embedder.clone(),
        chat: Arc::new(GatewayChatClient::new(
            cfg.gateway_url.clone(),
            cfg.gateway_key.clone(),
            cfg.llm_model.clone(),
            cfg.llm_timeout_secs,
        )),
        audit: Arc::new(HttpAuditSink::new(cfg.audit_url.clone())),
        cfg: cfg.clone(),
        vault: cfg.vault_dir.clone().map(|d| engram::vault::Vault::new(&d)),
    });

    let stop = Arc::new(AtomicBool::new(false));
    let _workers = spawn_workers(
        store.clone(),
        processor.clone(),
        cfg.jobs_workers,
        cfg.jobs_poll_ms,
        cfg.jobs_max_attempts,
        stop.clone(),
    );
    let _sweeper = spawn_sweeper(
        store.clone(),
        processor.clone(),
        cfg.stale_sweep_secs,
        stop.clone(),
    );
    tracing::info!(
        "engram: {} worker(s) + stale sweeper running",
        cfg.jobs_workers
    );

    let state = AppState {
        store,
        token: cfg.auth_token,
        embedder,
    };
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!("engram listening on {}", cfg.bind_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
