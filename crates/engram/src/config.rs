#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: String,
    pub bind_addr: String,
    pub auth_token: String,
    pub ollama_url: String,
    pub embed_model: String,
    pub embed_dim: usize,
    pub embed_timeout_secs: u64,
    // Plan 4
    pub gateway_url: String,
    pub gateway_key: String,
    pub llm_model: String,
    pub llm_provider: String,
    pub llm_timeout_secs: u64,
    pub audit_url: String,
    pub jobs_workers: usize,
    pub jobs_poll_ms: u64,
    pub jobs_max_attempts: i64,
    pub seal_input_token_budget: usize,
    pub seal_fanout: usize,
    pub seal_flush_age_secs: f64,
    pub stale_sweep_secs: u64,
    pub max_summary_output_tokens: usize,
    pub vault_dir: Option<String>,
    /// Wrap the gateway embedder with a local Ollama fallback (R1). Default false.
    pub embed_fallback: bool,
    /// Run cold-path consolidation for code-mode docs (R2). Default false — code trees are unread
    /// (search_code reads chunks) until Phase 2 ships get_architecture/get_module.
    pub consolidate_code: bool,
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Self {
        Config {
            db_path: get("ENGRAM_DB").unwrap_or_else(|| "engram.db".into()),
            bind_addr: get("ENGRAM_BIND").unwrap_or_else(|| "127.0.0.1:8088".into()),
            auth_token: get("ENGRAM_TOKEN").unwrap_or_else(|| "dev-token".into()),
            ollama_url: get("ENGRAM_OLLAMA_URL").unwrap_or_else(|| "http://127.0.0.1:11434".into()),
            embed_model: get("ENGRAM_EMBED_MODEL").unwrap_or_else(|| "bge-m3".into()),
            embed_dim: get("ENGRAM_EMBED_DIM")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1024),
            embed_timeout_secs: get("ENGRAM_EMBED_TIMEOUT_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            gateway_url: get("ENGRAM_GATEWAY_URL")
                .unwrap_or_else(|| "http://127.0.0.1:4000".into()),
            gateway_key: get("ENGRAM_GATEWAY_KEY").unwrap_or_default(),
            llm_model: get("ENGRAM_LLM_MODEL").unwrap_or_else(|| "qwen3".into()),
            llm_provider: get("ENGRAM_LLM_PROVIDER").unwrap_or_else(|| "ollama".into()),
            llm_timeout_secs: get("ENGRAM_LLM_TIMEOUT_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(90),
            audit_url: get("ENGRAM_AUDIT_URL").unwrap_or_else(|| "http://127.0.0.1:8383".into()),
            jobs_workers: get("ENGRAM_JOBS_WORKERS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(2),
            jobs_poll_ms: get("ENGRAM_JOBS_POLL_MS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(500),
            jobs_max_attempts: get("ENGRAM_JOBS_MAX_ATTEMPTS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5),
            seal_input_token_budget: get("ENGRAM_SEAL_INPUT_TOKEN_BUDGET")
                .and_then(|s| s.parse().ok())
                .unwrap_or(50_000),
            seal_fanout: get("ENGRAM_SEAL_FANOUT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(10),
            seal_flush_age_secs: get("ENGRAM_SEAL_FLUSH_AGE_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(604_800.0),
            stale_sweep_secs: get("ENGRAM_STALE_SWEEP_SECS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600),
            max_summary_output_tokens: get("ENGRAM_MAX_SUMMARY_OUTPUT_TOKENS")
                .and_then(|s| s.parse().ok())
                .unwrap_or(5000),
            vault_dir: get("ENGRAM_VAULT_DIR"),
            embed_fallback: env_bool(get("ENGRAM_EMBED_FALLBACK"), false),
            consolidate_code: env_bool(get("ENGRAM_CONSOLIDATE_CODE"), false),
        }
    }
}

/// Parse a boolean env value (true/1/yes/on vs false/0/no/off); unrecognized/absent → `default`.
fn env_bool(v: Option<String>, default: bool) -> bool {
    v.and_then(|s| match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    })
    .unwrap_or(default)
}

/// Whether code chunking flushes at definition boundaries (Phase F symbol-split). Process-global
/// cached read of `ENGRAM_CODE_SYMBOL_SPLIT` (default true). Kept in config.rs so the env var
/// stays registered here even though `chunk_code` is a free function with no `Config` access.
pub fn code_symbol_split() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("ENGRAM_CODE_SYMBOL_SPLIT")
            .ok()
            .and_then(|s| match s.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Some(true),
                "false" | "0" | "no" | "off" => Some(false),
                _ => None,
            })
            .unwrap_or(true)
    })
}

/// Whether code search applies the path-type ranking prior (Phase F: down-weight docs/tests/config
/// so source files rank above them on near-ties). Cached read of `ENGRAM_CODE_PATH_PRIOR` (default true).
pub fn code_path_prior() -> bool {
    use std::sync::OnceLock;
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        std::env::var("ENGRAM_CODE_PATH_PRIOR")
            .ok()
            .and_then(|s| match s.to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Some(true),
                "false" | "0" | "no" | "off" => Some(false),
                _ => None,
            })
            .unwrap_or(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_unset() {
        let c = Config::from_vars(|_| None);
        assert_eq!(c.db_path, "engram.db");
        assert_eq!(c.bind_addr, "127.0.0.1:8088");
        assert_eq!(c.auth_token, "dev-token");
        assert_eq!(c.ollama_url, "http://127.0.0.1:11434");
        assert_eq!(c.embed_model, "bge-m3");
        assert_eq!(c.embed_dim, 1024);
        assert_eq!(c.embed_timeout_secs, 30);
    }

    #[test]
    fn reads_overrides() {
        let c = Config::from_vars(|k| match k {
            "ENGRAM_DB" => Some("/tmp/x.db".into()),
            "ENGRAM_TOKEN" => Some("secret".into()),
            _ => None,
        });
        assert_eq!(c.db_path, "/tmp/x.db");
        assert_eq!(c.auth_token, "secret");
        assert_eq!(c.bind_addr, "127.0.0.1:8088");
    }

    #[test]
    fn plan4_defaults_and_overrides() {
        let c = Config::from_vars(|_| None);
        assert_eq!(c.gateway_url, "http://127.0.0.1:4000");
        assert_eq!(c.llm_model, "qwen3");
        assert_eq!(c.llm_provider, "ollama");
        assert_eq!(c.audit_url, "http://127.0.0.1:8383");
        assert_eq!(c.jobs_workers, 2);
        assert_eq!(c.jobs_max_attempts, 5);
        assert_eq!(c.seal_input_token_budget, 50_000);
        assert_eq!(c.seal_fanout, 10);
        assert_eq!(c.seal_flush_age_secs, 604_800.0);
        assert!(c.gateway_key.is_empty());
        assert!(c.vault_dir.is_none());
        assert!(!c.embed_fallback);
        assert!(!c.consolidate_code);

        let c2 = Config::from_vars(|k| match k {
            "ENGRAM_LLM_MODEL" => Some("deepseek-chat".into()),
            "ENGRAM_SEAL_FANOUT" => Some("3".into()),
            "ENGRAM_VAULT_DIR" => Some("/tmp/v".into()),
            "ENGRAM_CONSOLIDATE_CODE" => Some("true".into()),
            _ => None,
        });
        assert_eq!(c2.llm_model, "deepseek-chat");
        assert_eq!(c2.seal_fanout, 3);
        assert_eq!(c2.vault_dir.as_deref(), Some("/tmp/v"));
        assert!(c2.consolidate_code);
    }
}
