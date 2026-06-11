use engram::api::{app, AppState};
use engram::config::Config;
use engram::embed::Embedder;
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

    // Embeddings route through the litellm gateway (centralized keys/egress, audited, reachable
    // from WSL). With ENGRAM_EMBED_FALLBACK, wrap it with a local Ollama fallback (R1): the
    // fallback serves the same model/dim and the wrapper keeps the primary's signature(), so a
    // failover never orphans chunks.
    let primary: Arc<dyn Embedder> = Arc::new(engram::embed::GatewayEmbedder::new(
        cfg.embed_url.clone(),
        cfg.gateway_key.clone(),
        cfg.embed_model.clone(),
        cfg.embed_dim,
        cfg.embed_timeout_secs,
    ));
    let embedder: Arc<dyn Embedder> = if cfg.embed_fallback {
        tracing::info!(
            "embedder: gateway primary + ollama fallback ({})",
            cfg.ollama_url
        );
        Arc::new(engram::embed::FallbackEmbedder::new(
            primary,
            Arc::new(engram::embed::OllamaEmbedder::new(
                cfg.ollama_url.clone(),
                cfg.embed_model.clone(),
                cfg.embed_dim,
            )),
        ))
    } else {
        primary
    };
    // Best-effort startup reachability probe (non-fatal). Runs on a blocking thread so the
    // blocking reqwest call never stalls the tokio runtime — the server starts immediately.
    {
        let probe = embedder.clone();
        tokio::task::spawn_blocking(move || match probe.embed("ping") {
            Ok(_) => tracing::info!("embedder reachable ({})", probe.signature()),
            Err(e) => tracing::warn!("embedder probe failed at startup: {e}"),
        });
    }

    let chat: Arc<dyn engram::llm::ChatClient> = Arc::new(GatewayChatClient::new(
        cfg.gateway_url.clone(),
        cfg.gateway_key.clone(),
        cfg.llm_model.clone(),
        cfg.llm_timeout_secs,
    ));
    let processor = Arc::new(TreeProcessor {
        embedder: embedder.clone(),
        chat: chat.clone(),
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
        chat,
    };
    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr).await?;
    tracing::info!("engram listening on {}", cfg.bind_addr);
    axum::serve(listener, app(state)).await?;
    Ok(())
}
