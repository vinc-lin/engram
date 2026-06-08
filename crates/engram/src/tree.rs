use crate::config::Config;
use crate::embed::Embedder;
use crate::error::Result;
use crate::jobs::JobProcessor;
use crate::llm::{summarize_audited, AuditSink, ChatClient, SummarizeCtx};
use crate::store::{Job, NewTreeNode, Store, TreeNode};
use crate::vault::Vault;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const MAX_CASCADE_DEPTH: i64 = 32;

/// Shared dependencies for one cold-pipeline pass.
pub struct TreeCtx<'a> {
    pub embedder: &'a dyn Embedder,
    pub chat: &'a dyn ChatClient,
    pub audit: &'a dyn AuditSink,
    pub cfg: &'a Config,
    pub vault: Option<&'a Vault>,
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

/// Approximate token count (chars/4, min 1) — no tokenizer dep (spec §7.1).
fn approx_tokens(s: &str) -> i64 {
    (s.chars().count() / 4).max(1) as i64
}

/// Append a leaf to a tree buffer and run the seal cascade on it.
#[allow(clippy::too_many_arguments)]
pub fn append_leaf(
    store: &Store,
    ctx: &TreeCtx,
    namespace: &str,
    tree_kind: &str,
    tree_key: &str,
    body: &str,
    doc_id: &str,
    embedding: &[f32],
) -> Result<()> {
    let sig = ctx.embedder.signature();
    let node = NewTreeNode {
        namespace,
        tree_kind,
        tree_key,
        level: 0,
        sealed: false,
        label: None,
        body,
        doc_id: Some(doc_id),
        token_count: approx_tokens(body),
        embedding,
        model_signature: &sig,
    };
    store.append_leaf_node(&node)?;
    seal_cascade(store, ctx, namespace, tree_kind, tree_key, 0)
}

fn gate_exceeded(cfg: &Config, level: i64, buf: &[TreeNode], now: f64) -> bool {
    if buf.is_empty() {
        return false;
    }
    let size = if level == 0 {
        buf.iter().map(|n| n.token_count).sum::<i64>() >= cfg.seal_input_token_budget as i64
    } else {
        buf.len() >= cfg.seal_fanout
    };
    let oldest = buf
        .iter()
        .map(|n| n.created_at)
        .fold(f64::INFINITY, f64::min);
    let stale = oldest <= now - cfg.seal_flush_age_secs;
    size || stale
}

/// Seal the buffer at `level` if its gate is exceeded, then cascade upward. Summarize runs
/// off the write lock; only the node/edge writes commit (atomically, in `seal_buffer`).
pub fn seal_cascade(
    store: &Store,
    ctx: &TreeCtx,
    namespace: &str,
    tree_kind: &str,
    tree_key: &str,
    level: i64,
) -> Result<()> {
    if level >= MAX_CASCADE_DEPTH {
        return Ok(());
    }
    let buf = store.unsealed_nodes(namespace, tree_kind, tree_key, level)?;
    if !gate_exceeded(ctx.cfg, level, &buf, now_secs()) {
        return Ok(());
    }
    let (summary, label) = summarize(ctx, namespace, tree_kind, tree_key, level, &buf);
    let emb = ctx.embedder.embed(&summary)?;
    let sig = ctx.embedder.signature();
    let parent = NewTreeNode {
        namespace,
        tree_kind,
        tree_key,
        level: level + 1,
        sealed: false,
        label: label.as_deref(),
        body: &summary,
        doc_id: None,
        token_count: approx_tokens(&summary),
        embedding: &emb,
        model_signature: &sig,
    };
    let child_ids: Vec<String> = buf.iter().map(|n| n.node_id.clone()).collect();
    let parent_id = store.seal_buffer(&parent, &child_ids)?;
    if let Some(v) = ctx.vault {
        v.write_node(crate::vault::NodeMeta {
            namespace,
            tree_kind,
            tree_key,
            node_id: &parent_id,
            level: level + 1,
            label: label.as_deref(),
            body: &summary,
            created_at: now_secs(),
        })?;
    }
    seal_cascade(store, ctx, namespace, tree_kind, tree_key, level + 1)
}

/// Fold a buffer into one summary (LLM via gateway, audited) with a deterministic fallback.
fn summarize(
    ctx: &TreeCtx,
    namespace: &str,
    tree_kind: &str,
    tree_key: &str,
    level: i64,
    buf: &[TreeNode],
) -> (String, Option<String>) {
    let joined = buf
        .iter()
        .map(|n| n.body.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");
    let prompt = format!(
        "Consolidate these {} memory items into a single concise summary (at most {} tokens). \
         Preserve names, dates, and specifics.\n\n{}",
        buf.len(),
        ctx.cfg.max_summary_output_tokens,
        joined,
    );
    let sctx = SummarizeCtx {
        namespace,
        tree_kind,
        tree_key,
        level,
        model: &ctx.cfg.llm_model,
        provider: &ctx.cfg.llm_provider,
    };
    let summary = match summarize_audited(
        ctx.chat,
        ctx.audit,
        &sctx,
        &prompt,
        ctx.cfg.max_summary_output_tokens,
    ) {
        Ok(r) => r.text,
        Err(_) => fallback_summary(buf, ctx.cfg.max_summary_output_tokens * 4),
    };
    let label = label_for(tree_kind, buf, &summary);
    (summary, label)
}

/// Deterministic, LLM-free summary: concat child first-lines, truncated. Cascades never abort.
fn fallback_summary(buf: &[TreeNode], max_chars: usize) -> String {
    let mut s = String::new();
    for n in buf {
        let line = n.body.lines().next().unwrap_or("").trim();
        if !line.is_empty() {
            s.push_str(line);
            s.push_str("; ");
        }
        if s.chars().count() >= max_chars {
            break;
        }
    }
    s.chars().take(max_chars).collect()
}

fn label_for(tree_kind: &str, buf: &[TreeNode], summary: &str) -> Option<String> {
    match tree_kind {
        "source" => Some(
            summary
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect(),
        ),
        "global" => {
            let mut labels: Vec<String> = buf.iter().filter_map(|n| n.label.clone()).collect();
            labels.sort();
            labels.dedup();
            if labels.is_empty() {
                None
            } else {
                Some(labels.join(" · "))
            }
        }
        _ => None, // topic: the entity id is the label
    }
}

/// Cold pipeline for one document: fan its chunks out as leaves into the Source (by author),
/// Global, and per-entity Topic trees, sealing each touched buffer. Re-ingest first drops the
/// doc's prior unsealed leaves (sealed history is immutable). Optionally mirrors the doc.
pub fn process_doc(store: &Store, ctx: &TreeCtx, namespace: &str, document_id: &str) -> Result<()> {
    let doc = store.get_doc(namespace, document_id)?;
    let author = doc
        .as_ref()
        .map(|d| d.author.clone())
        .unwrap_or_else(|| "unknown".into());
    let chunks = store.chunks_for_doc(namespace, document_id)?;
    store.delete_unsealed_leaves_for_doc(namespace, document_id)?;
    for ch in &chunks {
        append_leaf(
            store,
            ctx,
            namespace,
            "source",
            &author,
            &ch.text,
            document_id,
            &ch.embedding,
        )?;
        append_leaf(
            store,
            ctx,
            namespace,
            "global",
            "global",
            &ch.text,
            document_id,
            &ch.embedding,
        )?;
        for e in &ch.entities {
            append_leaf(
                store,
                ctx,
                namespace,
                "topic",
                e,
                &ch.text,
                document_id,
                &ch.embedding,
            )?;
        }
    }
    if let (Some(v), Some(d)) = (ctx.vault, doc.as_ref()) {
        v.write_doc(d)?;
    }
    Ok(())
}

/// The job processor swapped in for `NoopProcessor` once the tree exists. Owns the
/// dependencies; builds a `TreeCtx` per call.
pub struct TreeProcessor {
    pub embedder: Arc<dyn Embedder>,
    pub chat: Arc<dyn ChatClient>,
    pub audit: Arc<dyn AuditSink>,
    pub cfg: Config,
    pub vault: Option<Vault>,
}

impl TreeProcessor {
    fn ctx(&self) -> TreeCtx<'_> {
        TreeCtx {
            embedder: self.embedder.as_ref(),
            chat: self.chat.as_ref(),
            audit: self.audit.as_ref(),
            cfg: &self.cfg,
            vault: self.vault.as_ref(),
        }
    }
}

impl JobProcessor for TreeProcessor {
    fn process(&self, store: &Store, job: &Job) -> Result<()> {
        process_doc(store, &self.ctx(), &job.namespace, &job.document_id)
    }
}

impl TreeProcessor {
    pub fn sweep(&self, store: &Store) -> Result<()> {
        sweep_stale(store, &self.ctx())
    }
}

/// Seal every buffer whose oldest unsealed node is older than the flush age (spec §11). The
/// freshly-created parent is not itself stale, so single-node cascades stop after one seal.
pub fn sweep_stale(store: &Store, ctx: &TreeCtx) -> Result<()> {
    let older_than = now_secs() - ctx.cfg.seal_flush_age_secs;
    for (ns, kind, key, level) in store.due_stale_buffers(older_than)? {
        seal_cascade(store, ctx, &ns, &kind, &key, level)?;
    }
    Ok(())
}

/// Background thread: run `sweep_stale` every `interval_secs` until `stop` (in 1s slices so
/// shutdown is responsive).
pub fn spawn_sweeper(
    store: Store,
    processor: Arc<TreeProcessor>,
    interval_secs: u64,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            for _ in 0..interval_secs.max(1) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if let Err(e) = processor.sweep(&store) {
                tracing::warn!("stale sweep: {e}");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::embed::{Embedder, HashEmbedder};
    use crate::llm::{FakeChatClient, NullAuditSink};
    use crate::store::Store;

    fn temp() -> (Store, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        (store, dir)
    }

    fn small_cfg() -> Config {
        let mut c = Config::from_vars(|_| None);
        c.seal_input_token_budget = 1; // any leaf seals L0
        c.seal_fanout = 2; // two L1 nodes → seal L2
        c.seal_flush_age_secs = 1e12; // never stale in this test
        c
    }

    #[test]
    fn cascade_seals_and_summarizes() {
        let (store, _d) = temp();
        let e = HashEmbedder::new(8);
        let chat = FakeChatClient::ok("S");
        let audit = NullAuditSink;
        let cfg = small_cfg();
        let ctx = TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg,
            vault: None,
        };
        let emb = e.embed("anything").unwrap();

        append_leaf(
            &store,
            &ctx,
            "alice",
            "source",
            "auth",
            "first leaf body",
            "d1",
            &emb,
        )
        .unwrap();
        // budget=1 → L0 sealed; one L1 node (<fanout) → stop
        assert_eq!(
            store.tree_top_nodes("alice", "source", "auth").unwrap()[0].level,
            1
        );

        append_leaf(
            &store,
            &ctx,
            "alice",
            "source",
            "auth",
            "second leaf body",
            "d2",
            &emb,
        )
        .unwrap();
        let top = store.tree_top_nodes("alice", "source", "auth").unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].level, 2); // two L1 nodes → sealed to L2
        assert_eq!(top[0].body, "S"); // FakeChat summary
        assert_eq!(top[0].label.as_deref(), Some("S")); // source label = summary first line
        assert_eq!(store.children_of(&top[0].node_id).unwrap().len(), 2);
    }

    #[test]
    fn summarize_falls_back_when_llm_errors() {
        let (store, _d) = temp();
        let e = HashEmbedder::new(8);
        let chat = FakeChatClient::failing();
        let audit = NullAuditSink;
        let cfg = small_cfg();
        let ctx = TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg,
            vault: None,
        };
        let emb = e.embed("x").unwrap();
        append_leaf(
            &store,
            &ctx,
            "alice",
            "global",
            "global",
            "alpha beta",
            "d1",
            &emb,
        )
        .unwrap();
        let top = store.tree_top_nodes("alice", "global", "global").unwrap();
        assert_eq!(top[0].level, 1);
        assert!(top[0].body.contains("alpha beta")); // deterministic fallback (concat of child bodies)
    }

    #[test]
    fn process_doc_fans_out_to_source_global_topic() {
        let (store, _d) = temp();
        let e = HashEmbedder::new(16);
        let new = crate::model::NewDoc {
            key: "k".into(),
            title: "t".into(),
            content: "ping @bob about rust".into(),
            author: "agentX".into(),
            taint: crate::model::Taint::Internal,
            meta: None,
        };
        crate::ingest::ingest_document(&store, &e, "alice", &new).unwrap();
        let job = store.claim_job().unwrap().unwrap();

        let chat = FakeChatClient::ok("S");
        let audit = NullAuditSink;
        let cfg = Config::from_vars(|_| None); // big gates → leaves only
        let ctx = TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg,
            vault: None,
        };
        process_doc(&store, &ctx, &job.namespace, &job.document_id).unwrap();

        assert_eq!(
            store
                .unsealed_nodes("alice", "source", "agentX", 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .unsealed_nodes("alice", "global", "global", 0)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .unsealed_nodes("alice", "topic", "handle:bob", 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn tree_processor_drains_a_job() {
        let (store, _d) = temp();
        let e = std::sync::Arc::new(HashEmbedder::new(16));
        let new = crate::model::NewDoc {
            key: "k".into(),
            title: "t".into(),
            content: "hello world".into(),
            author: "a".into(),
            taint: crate::model::Taint::Internal,
            meta: None,
        };
        crate::ingest::ingest_document(&store, e.as_ref(), "alice", &new).unwrap();
        let proc = TreeProcessor {
            embedder: e.clone(),
            chat: std::sync::Arc::new(FakeChatClient::ok("S")),
            audit: std::sync::Arc::new(NullAuditSink),
            cfg: Config::from_vars(|_| None),
            vault: None,
        };
        assert!(crate::jobs::worker_tick(&store, &proc, 5).unwrap());
        assert_eq!(
            store
                .unsealed_nodes("alice", "global", "global", 0)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn sweep_seals_stale_buffers() {
        let (store, _d) = temp();
        let e = HashEmbedder::new(8);
        let chat = FakeChatClient::ok("S");
        let audit = NullAuditSink;
        let mut cfg = Config::from_vars(|_| None);
        cfg.seal_input_token_budget = 1_000_000; // size gates won't fire
        cfg.seal_fanout = 1_000_000;
        cfg.seal_flush_age_secs = 100.0;
        let ctx = TreeCtx {
            embedder: &e,
            chat: &chat,
            audit: &audit,
            cfg: &cfg,
            vault: None,
        };
        let emb = e.embed("x").unwrap();
        append_leaf(
            &store,
            &ctx,
            "alice",
            "global",
            "global",
            "leaf body one",
            "d1",
            &emb,
        )
        .unwrap();
        append_leaf(
            &store,
            &ctx,
            "alice",
            "global",
            "global",
            "leaf body two",
            "d2",
            &emb,
        )
        .unwrap();
        assert_eq!(
            store
                .unsealed_nodes("alice", "global", "global", 0)
                .unwrap()
                .len(),
            2
        ); // no seal yet

        store.touch_unsealed_created_at("alice", 0.0).unwrap(); // backdate → stale
        sweep_stale(&store, &ctx).unwrap();
        assert!(store
            .unsealed_nodes("alice", "global", "global", 0)
            .unwrap()
            .is_empty());
        let l1 = store
            .unsealed_nodes("alice", "global", "global", 1)
            .unwrap();
        assert_eq!(l1.len(), 1);
        assert_eq!(store.children_of(&l1[0].node_id).unwrap().len(), 2);
    }
}
