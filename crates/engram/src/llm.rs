use crate::error::{Error, Result};
use serde::Serialize;

/// Result of one chat/summarize call.
#[derive(Debug, Clone)]
pub struct ChatResult {
    pub text: String,
    pub usage_total: Option<u64>,
}

/// A chat-completion client. engram's only LLM use is folding N memory items into
/// one summary; errors bubble so the caller can fall back deterministically.
pub trait ChatClient: Send + Sync {
    fn summarize(&self, prompt: &str, max_tokens: usize) -> Result<ChatResult>;
}

/// Deterministic, network-free chat client for tests.
pub struct FakeChatClient {
    reply: Option<String>, // None → always errors
}
impl FakeChatClient {
    pub fn ok(reply: &str) -> Self {
        Self {
            reply: Some(reply.into()),
        }
    }
    pub fn failing() -> Self {
        Self { reply: None }
    }
}
impl ChatClient for FakeChatClient {
    fn summarize(&self, prompt: &str, _max_tokens: usize) -> Result<ChatResult> {
        match &self.reply {
            Some(r) => Ok(ChatResult {
                text: r.clone(),
                usage_total: Some(prompt.split_whitespace().count() as u64),
            }),
            None => Err(Error::Llm("fake failure".into())),
        }
    }
}

/// OpenAI-compatible chat client pointing at the litellm gateway. Routing through the
/// gateway is also what gives WSL reachability to Ollama (the WSL→Windows hairpin).
pub struct GatewayChatClient {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::blocking::Client,
}
impl GatewayChatClient {
    pub fn new(base_url: String, api_key: String, model: String, timeout_secs: u64) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            base_url,
            api_key,
            model,
            client,
        }
    }
}
impl ChatClient for GatewayChatClient {
    fn summarize(&self, prompt: &str, max_tokens: usize) -> Result<ChatResult> {
        #[derive(Serialize)]
        struct Msg<'a> {
            role: &'a str,
            content: &'a str,
        }
        #[derive(Serialize)]
        struct Req<'a> {
            model: &'a str,
            messages: Vec<Msg<'a>>,
            max_tokens: usize,
        }
        #[derive(serde::Deserialize)]
        struct Choice {
            message: RespMsg,
        }
        #[derive(serde::Deserialize)]
        struct RespMsg {
            content: String,
        }
        #[derive(serde::Deserialize)]
        struct Usage {
            total_tokens: Option<u64>,
        }
        #[derive(serde::Deserialize)]
        struct Resp {
            choices: Vec<Choice>,
            usage: Option<Usage>,
        }

        let url = format!("{}/v1/chat/completions", self.base_url);
        let resp: Resp = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&Req {
                model: &self.model,
                messages: vec![Msg {
                    role: "user",
                    content: prompt,
                }],
                max_tokens,
            })
            .send()
            .map_err(|e| Error::Llm(e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Llm(e.to_string()))?
            .json()
            .map_err(|e| Error::Llm(e.to_string()))?;
        let text = resp
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .ok_or_else(|| Error::Llm("no choices".into()))?;
        Ok(ChatResult {
            text,
            usage_total: resp.usage.and_then(|u| u.total_tokens),
        })
    }
}

/// llm-audit event. Field set mirrors the gateway's verified payload (the deployed
/// llm-audit accepts exactly these keys; `extra="forbid"`). engram posts one per LLM call.
#[derive(Debug, Clone, Serialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub app: String,
    pub persona: Option<String>,
    pub ts: String,
    pub received_at: String,
    pub model_id: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub turn: Option<i64>,
    pub query_text: Option<String>,
    pub retrieved_count: i64,
    pub usage_total: Option<u64>,
    pub raw_json: String,
    pub provider: String,
    pub op: String,
    pub cost_usd: f64,
    pub latency_ms: i64,
    pub status: String,
}

/// Context for an audited summarize call.
pub struct SummarizeCtx<'a> {
    pub namespace: &'a str,
    pub tree_kind: &'a str,
    pub tree_key: &'a str,
    pub level: i64,
    pub model: &'a str,
    pub provider: &'a str,
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl AuditEvent {
    /// Build from a summarize outcome. `op` is "chat" (the gateway's vocabulary);
    /// `app=engram` already implies summarize, and `query_text` carries the tree context.
    pub fn build(ctx: &SummarizeCtx, res: &Result<ChatResult>, latency_ms: i64) -> Self {
        let now = now_rfc3339();
        let (status, usage_total) = match res {
            Ok(r) => ("ok", r.usage_total),
            Err(_) => ("error", None),
        };
        AuditEvent {
            event_id: format!("engram:{}", uuid::Uuid::new_v4()),
            app: "engram".into(),
            persona: Some(ctx.namespace.to_string()),
            ts: now.clone(),
            received_at: now,
            model_id: ctx.model.to_string(),
            session_id: None,
            run_id: None,
            turn: None,
            query_text: Some(format!("{}:{}@L{}", ctx.tree_kind, ctx.tree_key, ctx.level)),
            retrieved_count: 0,
            usage_total,
            raw_json: "{}".into(),
            provider: ctx.provider.to_string(),
            op: "chat".into(),
            cost_usd: 0.0,
            latency_ms,
            status: status.into(),
        }
    }
}

/// Where audit events go. `emit` never fails the caller — implementations swallow errors.
pub trait AuditSink: Send + Sync {
    fn emit(&self, ev: &AuditEvent);
}

/// No-op sink (when auditing is disabled).
pub struct NullAuditSink;
impl AuditSink for NullAuditSink {
    fn emit(&self, _ev: &AuditEvent) {}
}

/// Posts each event to llm-audit `/events`. Resilient: logs and swallows on failure so a
/// down audit sink never breaks consolidation.
pub struct HttpAuditSink {
    url: String,
    client: reqwest::blocking::Client,
}
impl HttpAuditSink {
    pub fn new(audit_url: String) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            url: format!("{}/events", audit_url.trim_end_matches('/')),
            client,
        }
    }
}
impl AuditSink for HttpAuditSink {
    fn emit(&self, ev: &AuditEvent) {
        let body = serde_json::json!({ "events": [ev] });
        if let Err(e) = self.client.post(&self.url).json(&body).send() {
            tracing::warn!("audit emit failed: {e}");
        }
    }
}

/// Summarize and audit: call the LLM, measure latency, emit one audit event (ok or error),
/// then return the LLM result so the caller can fall back on error.
pub fn summarize_audited(
    chat: &dyn ChatClient,
    audit: &dyn AuditSink,
    ctx: &SummarizeCtx,
    prompt: &str,
    max_tokens: usize,
) -> Result<ChatResult> {
    let start = std::time::Instant::now();
    let res = chat.summarize(prompt, max_tokens);
    let latency_ms = start.elapsed().as_millis() as i64;
    audit.emit(&AuditEvent::build(ctx, &res, latency_ms));
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_chat_is_deterministic() {
        let c = FakeChatClient::ok("SUMMARY");
        let r = c.summarize("fold these items", 100).unwrap();
        assert_eq!(r.text, "SUMMARY");
        assert_eq!(r.usage_total, Some(3));

        let err = FakeChatClient::failing();
        assert!(err.summarize("x", 10).is_err());
    }

    #[test]
    #[ignore = "requires the litellm gateway reachable with a valid ENGRAM_GATEWAY_KEY"]
    fn gateway_summarizes_live() {
        let key = std::env::var("ENGRAM_GATEWAY_KEY").unwrap_or_default();
        let c = GatewayChatClient::new("http://127.0.0.1:4000".into(), key, "qwen3".into(), 90);
        let r = c.summarize("Reply with exactly: OK", 32).unwrap();
        assert!(!r.text.is_empty());
    }

    use std::sync::Mutex;

    struct Capture(Mutex<Vec<AuditEvent>>);
    impl AuditSink for Capture {
        fn emit(&self, ev: &AuditEvent) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }

    fn ctx() -> SummarizeCtx<'static> {
        SummarizeCtx {
            namespace: "alice",
            tree_kind: "source",
            tree_key: "a",
            level: 0,
            model: "qwen3",
            provider: "ollama",
        }
    }

    #[test]
    fn summarize_audited_emits_ok_event_and_returns_text() {
        let chat = FakeChatClient::ok("S");
        let sink = Capture(Mutex::new(Vec::new()));
        let r = summarize_audited(&chat, &sink, &ctx(), "a b c", 100).unwrap();
        assert_eq!(r.text, "S");
        let evs = sink.0.lock().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].app, "engram");
        assert_eq!(evs[0].persona.as_deref(), Some("alice"));
        assert_eq!(evs[0].model_id, "qwen3");
        assert_eq!(evs[0].provider, "ollama");
        assert_eq!(evs[0].status, "ok");
        assert_eq!(evs[0].usage_total, Some(3));
        assert_eq!(evs[0].query_text.as_deref(), Some("source:a@L0"));
        assert!(evs[0].ts.contains('T')); // RFC3339
    }

    #[test]
    fn summarize_audited_emits_error_event_and_propagates() {
        let chat = FakeChatClient::failing();
        let sink = Capture(Mutex::new(Vec::new()));
        let r = summarize_audited(&chat, &sink, &ctx(), "x", 10);
        assert!(r.is_err());
        let evs = sink.0.lock().unwrap();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].status, "error");
        assert_eq!(evs[0].usage_total, None);
    }

    #[test]
    fn null_sink_is_noop() {
        NullAuditSink.emit(&AuditEvent::build(
            &ctx(),
            &Ok(ChatResult {
                text: "x".into(),
                usage_total: None,
            }),
            1,
        ));
    }
}
