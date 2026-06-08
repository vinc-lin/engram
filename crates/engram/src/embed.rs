use std::time::Duration;

use crate::error::{Error, Result};

const EMBED_TIMEOUT_SECS: u64 = 30;
const EMBED_MAX_ATTEMPTS: usize = 3;
const EMBED_BACKOFF: Duration = Duration::from_millis(1500);

/// Embeds text into a fixed-dimension vector. The service owns embedding so all
/// stored vectors share one model signature.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn signature(&self) -> String;
    fn dim(&self) -> usize;

    /// Embed many texts at once. Default loops `embed`; impls may override for true batching.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// Deterministic, network-free embedder for tests: bag-of-words hashed into
/// `dim` buckets. Shared tokens → overlapping nonzero buckets → positive dot.
pub struct HashEmbedder {
    pub dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim }
    }
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

impl Embedder for HashEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = vec![0f32; self.dim];
        for tok in text.to_lowercase().split_whitespace() {
            let idx = (fnv1a(tok) as usize) % self.dim;
            v[idx] += 1.0;
        }
        Ok(v)
    }
    fn signature(&self) -> String {
        format!("hash:{}", self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

/// Retry `f` up to `max_attempts`, sleeping `backoff` between attempts, but only while the
/// returned error is flagged retryable. The closure returns Err((retryable, error)).
fn with_retries<T>(
    max_attempts: usize,
    backoff: Duration,
    mut f: impl FnMut() -> std::result::Result<T, (bool, crate::error::Error)>,
) -> crate::error::Result<T> {
    let mut attempt = 1;
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err((retryable, _e)) if retryable && attempt < max_attempts => {
                std::thread::sleep(backoff);
                attempt += 1;
            }
            Err((_, e)) => return Err(e),
        }
    }
}

fn is_retryable(e: &reqwest::Error) -> bool {
    // Retry transient failures: timeouts, connection errors, missing status, or 5xx.
    // Do NOT retry deterministic 4xx (e.g. a 400 from an oversized/odd input) or a body
    // decode error (a malformed 2xx body recurs on retry — futile latency).
    e.is_timeout()
        || e.is_connect()
        || (!e.is_decode() && e.status().is_none_or(|s| s.is_server_error()))
}

/// Real embedder backed by a local Ollama server (`POST /api/embeddings`).
pub struct OllamaEmbedder {
    base_url: String,
    model: String,
    dim: usize,
    client: reqwest::blocking::Client,
}

impl OllamaEmbedder {
    pub fn new(base_url: String, model: String, dim: usize) -> Self {
        Self {
            base_url,
            model,
            dim,
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(EMBED_TIMEOUT_SECS))
                .build()
                .expect("build embed client"),
        }
    }
}

impl Embedder for OllamaEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            prompt: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            embedding: Vec<f32>,
        }

        let url = format!("{}/api/embeddings", self.base_url);
        with_retries(EMBED_MAX_ATTEMPTS, EMBED_BACKOFF, || {
            let resp: Resp = self
                .client
                .post(&url)
                .json(&Req {
                    model: &self.model,
                    prompt: text,
                })
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| (is_retryable(&e), Error::Embed(e.to_string())))?
                .json()
                .map_err(|e| (is_retryable(&e), Error::Embed(e.to_string())))?;
            Ok(resp.embedding)
        })
    }
    fn signature(&self) -> String {
        format!("ollama:{}:{}", self.model, self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

/// Embedder backed by the litellm gateway's OpenAI-compatible `/v1/embeddings`. This is the
/// production path: it routes through the gateway (centralized keys/egress, audited, and
/// reachable from WSL — which can't hit the Windows Ollama directly).
pub struct GatewayEmbedder {
    base_url: String,
    api_key: String,
    model: String,
    dim: usize,
    client: reqwest::blocking::Client,
}

impl GatewayEmbedder {
    pub fn new(
        base_url: String,
        api_key: String,
        model: String,
        dim: usize,
        timeout_secs: u64,
    ) -> Self {
        Self {
            base_url,
            api_key,
            model,
            dim,
            client: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .build()
                .expect("build embed client"),
        }
    }
}

impl Embedder for GatewayEmbedder {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            input: &'a str,
        }
        #[derive(serde::Deserialize)]
        struct EmbData {
            embedding: Vec<f32>,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<EmbData>,
        }

        let url = format!("{}/v1/embeddings", self.base_url);
        with_retries(EMBED_MAX_ATTEMPTS, EMBED_BACKOFF, || {
            let resp: Resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&Req {
                    model: &self.model,
                    input: text,
                })
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| (is_retryable(&e), Error::Embed(e.to_string())))?
                .json()
                .map_err(|e| (is_retryable(&e), Error::Embed(e.to_string())))?;
            resp.data
                .into_iter()
                .next()
                .map(|d| d.embedding)
                .ok_or_else(|| (false, Error::Embed("no embedding data".into())))
        })
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        #[derive(serde::Serialize)]
        struct Req<'a> {
            model: &'a str,
            input: &'a [&'a str],
        }
        #[derive(serde::Deserialize)]
        struct EmbData {
            index: usize,
            embedding: Vec<f32>,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            data: Vec<EmbData>,
        }

        let url = format!("{}/v1/embeddings", self.base_url);
        let n = texts.len();
        with_retries(EMBED_MAX_ATTEMPTS, EMBED_BACKOFF, || {
            let resp: Resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&Req {
                    model: &self.model,
                    input: texts,
                })
                .send()
                .and_then(|r| r.error_for_status())
                .map_err(|e| (is_retryable(&e), Error::Embed(e.to_string())))?
                .json()
                .map_err(|e| (is_retryable(&e), Error::Embed(e.to_string())))?;

            if resp.data.is_empty() {
                return Err((false, Error::Embed("no embedding data".into())));
            }
            if resp.data.len() != n {
                return Err((
                    false,
                    Error::Embed(format!(
                        "batch length mismatch: sent {n}, got {}",
                        resp.data.len()
                    )),
                ));
            }

            // Sort by the index field returned by litellm so order matches input.
            let mut data = resp.data;
            data.sort_by_key(|d| d.index);
            Ok(data.into_iter().map(|d| d.embedding).collect())
        })
    }

    fn signature(&self) -> String {
        format!("gateway:{}:{}", self.model, self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn with_retries_succeeds_after_transient() {
        let calls = Cell::new(0usize);
        let result = with_retries(EMBED_MAX_ATTEMPTS, Duration::ZERO, || {
            let n = calls.get() + 1;
            calls.set(n);
            if n < 3 {
                Err((true, Error::Embed("x".into())))
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn with_retries_stops_on_non_retryable() {
        let calls = Cell::new(0usize);
        let result: crate::error::Result<()> =
            with_retries(EMBED_MAX_ATTEMPTS, Duration::ZERO, || {
                calls.set(calls.get() + 1);
                Err((false, Error::Embed("fatal".into())))
            });
        assert!(result.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn with_retries_gives_up_after_max() {
        let calls = Cell::new(0usize);
        let result: crate::error::Result<()> =
            with_retries(EMBED_MAX_ATTEMPTS, Duration::ZERO, || {
                calls.set(calls.get() + 1);
                Err((true, Error::Embed("transient".into())))
            });
        assert!(result.is_err());
        assert_eq!(calls.get(), EMBED_MAX_ATTEMPTS);
    }

    #[test]
    fn deterministic_and_sized() {
        let e = HashEmbedder::new(64);
        let a = e.embed("hello world").unwrap();
        let b = e.embed("hello world").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_eq!(e.dim(), 64);
    }

    #[test]
    fn shared_tokens_overlap() {
        let e = HashEmbedder::new(64);
        let q = e.embed("rust memory service").unwrap();
        let related = e.embed("a memory service in rust").unwrap();
        let unrelated = e.embed("bananas grow on trees").unwrap();
        assert!(dot(&q, &related) > dot(&q, &unrelated));
    }

    #[test]
    #[ignore = "requires a running Ollama with bge-m3 pulled"]
    fn ollama_embeds_1024() {
        let e = OllamaEmbedder::new("http://127.0.0.1:11434".into(), "bge-m3".into(), 1024);
        let v = e.embed("hello").unwrap();
        assert_eq!(v.len(), 1024);
    }

    #[test]
    fn hash_embed_batch_matches_embed() {
        let e = HashEmbedder::new(64);
        let batch = e.embed_batch(&["a", "b"]).unwrap();
        let single_a = e.embed("a").unwrap();
        let single_b = e.embed("b").unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], single_a);
        assert_eq!(batch[1], single_b);
    }

    #[test]
    fn gateway_embedder_signature_and_dim() {
        let e = GatewayEmbedder::new(
            "http://x".into(),
            "k".into(),
            "mxbai-embed-large".into(),
            1024,
            30,
        );
        assert_eq!(e.dim(), 1024);
        assert_eq!(e.signature(), "gateway:mxbai-embed-large:1024");
    }

    #[test]
    #[ignore = "requires the litellm gateway reachable with a valid ENGRAM_GATEWAY_KEY"]
    fn gateway_embeds_via_litellm() {
        let key = std::env::var("ENGRAM_GATEWAY_KEY").unwrap_or_default();
        let e = GatewayEmbedder::new(
            "http://127.0.0.1:4000".into(),
            key,
            "mxbai-embed-large".into(),
            1024,
            30,
        );
        let v = e.embed("hello").unwrap();
        assert_eq!(v.len(), 1024);
    }
}
