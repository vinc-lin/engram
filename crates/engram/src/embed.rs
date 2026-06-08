use crate::error::{Error, Result};

/// Embeds text into a fixed-dimension vector. The service owns embedding so all
/// stored vectors share one model signature.
pub trait Embedder: Send + Sync {
    fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn signature(&self) -> String;
    fn dim(&self) -> usize;
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
            client: reqwest::blocking::Client::new(),
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
        let resp: Resp = self
            .client
            .post(url)
            .json(&Req {
                model: &self.model,
                prompt: text,
            })
            .send()
            .map_err(|e| Error::Embed(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Embed(e.to_string()))?
            .json()
            .map_err(|e| Error::Embed(e.to_string()))?;
        Ok(resp.embedding)
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
    pub fn new(base_url: String, api_key: String, model: String, dim: usize) -> Self {
        Self {
            base_url,
            api_key,
            model,
            dim,
            client: reqwest::blocking::Client::new(),
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
        let resp: Resp = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&Req {
                model: &self.model,
                input: text,
            })
            .send()
            .map_err(|e| Error::Embed(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Embed(e.to_string()))?
            .json()
            .map_err(|e| Error::Embed(e.to_string()))?;
        resp.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| Error::Embed("no embedding data".into()))
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
    use super::*;

    fn dot(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
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
    fn gateway_embedder_signature_and_dim() {
        let e = GatewayEmbedder::new(
            "http://x".into(),
            "k".into(),
            "mxbai-embed-large".into(),
            1024,
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
        );
        let v = e.embed("hello").unwrap();
        assert_eq!(v.len(), 1024);
    }
}
