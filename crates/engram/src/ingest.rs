use crate::embed::Embedder;
use crate::error::Result;
use crate::model::{MemoryDoc, NewDoc};
use crate::store::Store;
use regex::Regex;
use std::sync::OnceLock;

type IngestParts = (Vec<String>, Vec<Option<(i64, i64)>>, Vec<Vec<String>>);

/// Max characters (not bytes) per chunk — CJK-safe.
const MAX_CHUNK_CHARS: usize = 800;

/// Max characters per code chunk (code packs by line, not paragraph).
const MAX_CODE_CHUNK_CHARS: usize = 1500;

/// Token budget for code chunks — conservatively below the embed-model's 512-token ceiling.
/// Each emitted chunk must have `estimate_tokens(text) <= CODE_CHUNK_TOKEN_BUDGET`.
/// Could become embedder-aware in a future refactor (e.g. passed in from `GatewayEmbedder`).
const CODE_CHUNK_TOKEN_BUDGET: usize = 480;

/// Returns true if the character is in a CJK range (Unified Ideographs, Hiragana, Katakana,
/// Hangul Syllables). Used by both `estimate_tokens` and `chunk_code`.
#[inline]
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3040}'..='\u{309F}' // Hiragana
        | '\u{30A0}'..='\u{30FF}' // Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
    )
}

/// Heuristic token estimator: CJK characters (Unified Ideographs, Hiragana, Katakana, Hangul)
/// are each ≈1 token; all other characters are ≈0.25 token. Returns
/// `cjk_count + ceil(non_cjk_count / 4)`.
pub fn estimate_tokens(s: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for c in s.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// Code-aware chunking: pack consecutive lines up to a char cap and a token budget, never
/// splitting a line unless a single CJK-heavy line itself exceeds the token budget.
/// Returns `(chunk_text, start_line, end_line)` with 1-based inclusive line numbers.
///
/// Flush rules when packing:
///   - If adding a line would exceed `MAX_CODE_CHUNK_CHARS` chars OR `CODE_CHUNK_TOKEN_BUDGET`
///     estimated tokens, flush the current buffer first.
///
/// Oversized CJK-line rule:
///   - If a single line contains CJK characters AND its token estimate exceeds
///     `CODE_CHUNK_TOKEN_BUDGET`, it is hard-split on character boundaries into consecutive
///     pieces, each within budget. All pieces share the same `(start_line, end_line)`.
///   - Pure-ASCII lines that exceed the char cap continue to be emitted as a single chunk
///     (preserving prior behavior — the char cap dominates for ASCII content since
///     1500 chars ≈ 375 ASCII tokens, well below the 480-token budget).
pub fn chunk_code(content: &str) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = Vec::new();
    let mut cur = String::new();
    let mut cur_start = 1usize;
    let mut cur_lines = 0usize;
    let mut line_no = 0usize;

    let flush = |cur: &mut String,
                 cur_lines: &mut usize,
                 cur_start: usize,
                 end: usize,
                 out: &mut Vec<(String, usize, usize)>| {
        if *cur_lines > 0 {
            out.push((std::mem::take(cur), cur_start, end));
            *cur_lines = 0;
        }
    };

    for line in content.lines() {
        line_no += 1;
        let line_chars = line.chars().count();
        let line_tokens = estimate_tokens(line);
        let line_has_cjk = line.chars().any(is_cjk);

        if line_has_cjk && line_tokens > CODE_CHUNK_TOKEN_BUDGET {
            // Hard-split the oversized CJK line; flush any accumulated buffer first.
            flush(&mut cur, &mut cur_lines, cur_start, line_no - 1, &mut out);

            // Greedily accumulate characters into pieces, flushing each time adding the next
            // character would bring the piece to the budget.
            let mut piece = String::new();
            for ch in line.chars() {
                piece.push(ch);
                if estimate_tokens(&piece) >= CODE_CHUNK_TOKEN_BUDGET {
                    out.push((std::mem::take(&mut piece), line_no, line_no));
                }
            }
            if !piece.is_empty() {
                out.push((piece, line_no, line_no));
            }
            cur_start = line_no + 1;
        } else {
            // Normal line (ASCII, or CJK within budget): check both caps before packing.
            let add_char_len = line_chars + 1; // +1 for the newline we append
            let would_exceed_chars =
                cur_lines > 0 && cur.chars().count() + add_char_len > MAX_CODE_CHUNK_CHARS;
            let would_exceed_tokens =
                cur_lines > 0 && estimate_tokens(&cur) + line_tokens > CODE_CHUNK_TOKEN_BUDGET;

            if would_exceed_chars || would_exceed_tokens {
                flush(&mut cur, &mut cur_lines, cur_start, line_no - 1, &mut out);
            }
            if cur_lines == 0 {
                cur_start = line_no;
            }
            cur.push_str(line);
            cur.push('\n');
            cur_lines += 1;
        }
    }
    if cur_lines > 0 {
        out.push((cur, cur_start, line_no));
    }
    out
}

/// Split content into chunks: paragraph-aware (`\n\n`), packed up to a char cap,
/// with hard char-splitting of any single oversized paragraph. Deterministic.
pub fn chunk(content: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;

    let flush = |cur: &mut String, cur_len: &mut usize, chunks: &mut Vec<String>| {
        if !cur.is_empty() {
            chunks.push(std::mem::take(cur));
            *cur_len = 0;
        }
    };

    for para in content.split("\n\n") {
        let p = para.trim();
        if p.is_empty() {
            continue;
        }
        let plen = p.chars().count();
        if plen > MAX_CHUNK_CHARS {
            flush(&mut cur, &mut cur_len, &mut chunks);
            let chars: Vec<char> = p.chars().collect();
            for piece in chars.chunks(MAX_CHUNK_CHARS) {
                chunks.push(piece.iter().collect());
            }
            continue;
        }
        if cur_len + plen + 1 > MAX_CHUNK_CHARS {
            flush(&mut cur, &mut cur_len, &mut chunks);
        }
        if !cur.is_empty() {
            cur.push('\n');
            cur_len += 1;
        }
        cur.push_str(p);
        cur_len += plen;
    }
    flush(&mut cur, &mut cur_len, &mut chunks);
    if chunks.is_empty() {
        let t = content.trim();
        if !t.is_empty() {
            chunks.push(t.to_string());
        }
    }
    chunks
}

/// Mechanical (regex) entity extraction → canonical ids
/// (`email:`, `url:`, `handle:`, `hashtag:`). Sorted + deduped.
/// Semantic (LLM) entities are a later plan.
pub fn extract_entities(text: &str) -> Vec<String> {
    static EMAIL: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static HANDLE: OnceLock<Regex> = OnceLock::new();
    static HASHTAG: OnceLock<Regex> = OnceLock::new();

    let email = EMAIL.get_or_init(|| Regex::new(r"[\w.+-]+@[\w-]+\.[\w.-]+").unwrap());
    let url = URL.get_or_init(|| Regex::new(r"https?://[^\s]+").unwrap());
    let handle = HANDLE.get_or_init(|| Regex::new(r"(?:^|[^\w@])@(\w+)").unwrap());
    let hashtag = HASHTAG.get_or_init(|| Regex::new(r"(?:^|[^\w#])#(\w+)").unwrap());

    let mut out: Vec<String> = Vec::new();
    for m in email.find_iter(text) {
        out.push(format!("email:{}", m.as_str().to_lowercase()));
    }
    // Mask URL spans before the handle/hashtag passes so `@name` / `#tag` *inside* a
    // URL (e.g. https://x.com/@alice#sec) aren't mis-extracted as social mentions.
    // Replacing each match with an equal-length run of spaces keeps later byte offsets valid.
    let mut masked = text.to_string();
    for m in url.find_iter(text) {
        out.push(format!("url:{}", m.as_str()));
        masked.replace_range(m.range(), &" ".repeat(m.as_str().len()));
    }
    for c in handle.captures_iter(&masked) {
        out.push(format!("handle:{}", c[1].to_lowercase()));
    }
    for c in hashtag.captures_iter(&masked) {
        out.push(format!("hashtag:{}", c[1].to_lowercase()));
    }
    out.sort();
    out.dedup();
    out
}

/// Mechanical (regex) code entities → canonical ids: `sym:<name>` (definitions) and
/// `import:<target>` (import/use/require/include/from targets). Case-preserving (code is
/// case-sensitive). Sorted + deduped. Language-agnostic baseline; AST-precise symbols are a
/// later (tree-sitter) phase.
pub fn extract_code_entities(text: &str) -> Vec<String> {
    static DEF: OnceLock<Regex> = OnceLock::new();
    static IMPORT: OnceLock<Regex> = OnceLock::new();

    let def = DEF.get_or_init(|| {
        Regex::new(
            r"(?m)^\s*(?:pub\s+)?(?:async\s+)?(?:fn|def|class|struct|enum|interface|trait|type|func)\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .unwrap()
    });
    let import = IMPORT.get_or_init(|| {
        Regex::new(r#"(?m)^\s*(?:use|import|from|require|include|using)\s+([^\s;(){}'"]+)"#)
            .unwrap()
    });

    let mut out: Vec<String> = Vec::new();
    for c in def.captures_iter(text) {
        out.push(format!("sym:{}", &c[1]));
    }
    for c in import.captures_iter(text) {
        out.push(format!("import:{}", &c[1]));
    }
    out.sort();
    out.dedup();
    out
}

/// Hot ingest: chunk the content, embed every chunk **off the write lock**, extract mechanical
/// entities, then commit the doc + chunks + entities + the post-acquire job atomically.
pub fn ingest_document(
    store: &Store,
    embedder: &dyn Embedder,
    namespace: &str,
    new: &NewDoc,
) -> Result<MemoryDoc> {
    let is_code = new
        .meta
        .as_ref()
        .and_then(|m| m.get("kind"))
        .and_then(|k| k.as_str())
        == Some("file");

    let (chunk_texts, line_ranges, entities): IngestParts = if is_code {
        let pieces = chunk_code(&new.content);
        let texts: Vec<String> = pieces.iter().map(|(t, _, _)| t.clone()).collect();
        let ranges: Vec<Option<(i64, i64)>> = pieces
            .iter()
            .map(|(_, s, e)| Some((*s as i64, *e as i64)))
            .collect();
        let path_ent = format!("path:{}", new.key);
        let ents: Vec<Vec<String>> = texts
            .iter()
            .map(|t| {
                let mut es = extract_code_entities(t);
                es.push(path_ent.clone());
                es.sort();
                es.dedup();
                es
            })
            .collect();
        (texts, ranges, ents)
    } else {
        let texts = chunk(&new.content);
        let ranges = vec![None; texts.len()];
        let ents: Vec<Vec<String>> = texts.iter().map(|t| extract_entities(t)).collect();
        (texts, ranges, ents)
    };

    // Embed each chunk off-lock; skip any chunk whose embedding fails and warn.
    let mut kept_texts: Vec<String> = Vec::with_capacity(chunk_texts.len());
    let mut kept_ranges: Vec<Option<(i64, i64)>> = Vec::with_capacity(chunk_texts.len());
    let mut kept_entities: Vec<Vec<String>> = Vec::with_capacity(chunk_texts.len());
    let mut kept_embeddings: Vec<Vec<f32>> = Vec::with_capacity(chunk_texts.len());
    let mut skipped: usize = 0;

    for i in 0..chunk_texts.len() {
        match embedder.embed(&chunk_texts[i]) {
            Ok(emb) => {
                kept_texts.push(chunk_texts[i].clone());
                kept_ranges.push(line_ranges[i]);
                kept_entities.push(entities[i].clone());
                kept_embeddings.push(emb);
            }
            Err(_) => {
                skipped += 1;
            }
        }
    }

    if skipped > 0 {
        tracing::warn!(
            namespace = namespace,
            key = %new.key,
            skipped,
            total = chunk_texts.len(),
            "some chunks failed to embed and were skipped"
        );
    }

    if !chunk_texts.is_empty() && kept_texts.is_empty() {
        let n = chunk_texts.len();
        return Err(crate::error::Error::Embed(format!(
            "all {n} chunks failed to embed for {}",
            new.key
        )));
    }

    let sig = embedder.signature();
    store.commit_ingest(
        namespace,
        new,
        &kept_texts,
        &kept_embeddings,
        &kept_entities,
        &kept_ranges,
        &sig,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_is_one_chunk() {
        assert_eq!(chunk("hello world"), vec!["hello world".to_string()]);
        assert!(chunk("   ").is_empty());
    }

    #[test]
    fn paragraphs_split_and_pack() {
        let big = "x".repeat(900);
        let out = chunk(&format!("para one\n\npara two\n\n{big}"));
        assert_eq!(out.len(), 3);
        assert!(out[0].contains("para one") && out[0].contains("para two"));
        assert_eq!(out[1].chars().count(), 800);
        assert_eq!(out[2].chars().count(), 100);
    }

    #[test]
    fn cjk_safe() {
        let s = "漫画".repeat(500); // 1000 CJK chars, one paragraph
        let out = chunk(&s);
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|c| c.contains('\u{FFFD}')));
    }

    #[test]
    fn ingest_document_chunks_embeds_extracts_and_enqueues() {
        use crate::embed::{Embedder, HashEmbedder};
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let mk = |k: &str, c: &str| NewDoc {
            key: k.into(),
            title: k.into(),
            content: c.into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };

        // Two short paragraphs pack into one chunk; a post-acquire job is enqueued.
        let doc = ingest_document(
            &store,
            &e,
            "alice",
            &mk(
                "d1",
                "Contact @bob about the launch.\n\nSee https://x.com/a and email a@b.com",
            ),
        )
        .unwrap();
        let d1: Vec<_> = store
            .chunks_for_namespace("alice", &e.signature())
            .unwrap()
            .into_iter()
            .filter(|(d, _, _)| d == &doc.document_id)
            .collect();
        assert_eq!(d1.len(), 1);
        assert!(d1.iter().all(|(_, _, v)| v.len() == 32));
        let hits = store
            .docs_with_entities("alice", &["handle:bob".into(), "email:a@b.com".into()])
            .unwrap();
        assert_eq!(hits.get(&doc.document_id).copied().unwrap_or(0), 2);
        assert_eq!(
            store
                .job(&format!("alice:{}", doc.document_id))
                .unwrap()
                .unwrap()
                .0,
            "pending"
        );

        // A long single paragraph hard-splits into multiple chunks under one doc.
        let long = format!("@carol owns the launch. {}", "x".repeat(900));
        let doc2 = ingest_document(&store, &e, "alice", &mk("d2", &long)).unwrap();
        let d2: Vec<_> = store
            .chunks_for_namespace("alice", &e.signature())
            .unwrap()
            .into_iter()
            .filter(|(d, _, _)| d == &doc2.document_id)
            .collect();
        assert_eq!(d2.len(), 2);

        // Re-ingest d1 with different content replaces its chunks + entities, same id.
        let doc1b =
            ingest_document(&store, &e, "alice", &mk("d1", "totally different now")).unwrap();
        assert_eq!(doc1b.document_id, doc.document_id);
        let d1b: Vec<_> = store
            .chunks_for_namespace("alice", &e.signature())
            .unwrap()
            .into_iter()
            .filter(|(d, _, _)| d == &doc.document_id)
            .collect();
        assert_eq!(d1b.len(), 1);
        assert!(store
            .docs_with_entities("alice", &["handle:bob".into()])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn extracts_canonical_entities() {
        let got = extract_entities(
            "Ping Alice at Alice@Example.com or @aliceH about #LaunchDay see https://x.com/a",
        );
        assert!(got.contains(&"email:alice@example.com".to_string()));
        assert!(got.contains(&"handle:aliceh".to_string()));
        assert!(got.contains(&"hashtag:launchday".to_string()));
        assert!(got.contains(&"url:https://x.com/a".to_string()));
        assert!(!got.iter().any(|e| e == "handle:example.com"));
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(got, sorted);
    }

    #[test]
    fn entities_inside_urls_are_not_mentions() {
        let got = extract_entities("see https://x.com/@alice#sec then ping @bob about #launch");
        assert!(got.contains(&"url:https://x.com/@alice#sec".to_string()));
        assert!(got.contains(&"handle:bob".to_string()));
        assert!(got.contains(&"hashtag:launch".to_string()));
        assert!(!got.contains(&"handle:alice".to_string()));
        assert!(!got.contains(&"hashtag:sec".to_string()));
    }

    #[test]
    fn chunk_code_one_chunk_for_small_input() {
        let out = chunk_code("fn main() {\n    let x = 1;\n}\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, 1);
        assert_eq!(out[0].2, 3);
        assert!(out[0].0.contains("fn main()"));
        assert!(out[0].0.contains("let x = 1;"));
    }

    #[test]
    fn chunk_code_splits_on_char_cap_at_line_boundary() {
        let a = "a".repeat(800);
        let b = "b".repeat(800);
        let out = chunk_code(&format!("{a}\n{b}"));
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].1, out[0].2), (1, 1));
        assert_eq!((out[1].1, out[1].2), (2, 2));
        assert!(out[0].0.contains(&a));
        assert!(out[1].0.contains(&b));
    }

    #[test]
    fn chunk_code_oversized_single_line_is_its_own_chunk() {
        let big = "z".repeat(2000);
        let out = chunk_code(&format!("head\n{big}\ntail"));
        assert_eq!(out.len(), 3);
        assert_eq!((out[0].1, out[0].2), (1, 1));
        assert_eq!((out[1].1, out[1].2), (2, 2));
        assert_eq!((out[2].1, out[2].2), (3, 3));
        assert!(out[1].0.contains(&big));
    }

    #[test]
    fn chunk_code_empty_is_no_chunks() {
        assert!(chunk_code("").is_empty());
    }

    #[test]
    fn extract_code_entities_finds_defs_and_imports_case_preserving() {
        let text = "use crate::store::Store;\npub fn DoThing() {}\nstruct MyType;\n";
        let got = extract_code_entities(text);
        assert!(got.contains(&"import:crate::store::Store".to_string()));
        assert!(got.contains(&"sym:DoThing".to_string()));
        assert!(got.contains(&"sym:MyType".to_string()));
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(got, sorted);
    }

    #[test]
    fn extract_code_entities_handles_multiple_languages() {
        let py = "import os\ndef handler():\n    pass\nclass Worker:\n    pass\n";
        let got = extract_code_entities(py);
        assert!(got.contains(&"import:os".to_string()));
        assert!(got.contains(&"sym:handler".to_string()));
        assert!(got.contains(&"sym:Worker".to_string()));
    }

    #[test]
    fn extract_code_entities_empty_for_prose() {
        assert!(extract_code_entities("just some words, nothing to define").is_empty());
    }

    #[test]
    fn ingest_code_mode_sets_line_ranges_and_code_entities() {
        use crate::embed::HashEmbedder;
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let new = NewDoc {
            key: "src/lib.rs".into(),
            title: "src/lib.rs".into(),
            content: "use crate::store::Store;\npub fn run() {}\n".into(),
            author: "code".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file", "lang": "rust"})),
        };
        let doc = ingest_document(&store, &e, "repo:x", &new).unwrap();
        let rows = store.chunks_for_doc("repo:x", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].line_start, rows[0].line_end), (Some(1), Some(2)));
        assert!(rows[0].entities.contains(&"sym:run".to_string()));
        assert!(rows[0]
            .entities
            .contains(&"import:crate::store::Store".to_string()));
        assert!(rows[0].entities.contains(&"path:src/lib.rs".to_string()));
    }

    #[test]
    fn estimate_tokens_counts_cjk_heavier() {
        // Pure CJK: each char ≈ 1 token
        let cjk = "漫画作品の"; // 5 CJK chars
        let t = estimate_tokens(cjk);
        assert!(
            t >= 4 && t <= 6,
            "CJK token estimate should be near char count, got {t}"
        );

        // Pure ASCII: ≈ chars/4
        let ascii = "hello world!!!"; // 14 chars
        let t2 = estimate_tokens(ascii);
        assert!(
            t2 >= 3 && t2 <= 5,
            "ASCII token estimate should be ~chars/4, got {t2}"
        );

        // Mixed: large CJK string
        let cjk_long = "漫".repeat(480); // 480 CJK chars → ~480 tokens
        let t3 = estimate_tokens(&cjk_long);
        assert!(t3 >= 475 && t3 <= 485, "expected ~480, got {t3}");
    }

    #[test]
    fn chunk_code_cjk_stays_within_token_budget() {
        // Build a long CJK string across many lines, each line ~100 CJK chars
        let line = "語".repeat(100); // 100 CJK = ~100 tokens per line
        let content: String = (0..30)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join("\n"); // 30 lines, 3000 total CJK chars
        let out = chunk_code(&content);
        assert!(!out.is_empty());
        for (text, _s, _e) in &out {
            let tokens = estimate_tokens(text);
            assert!(
                tokens <= CODE_CHUNK_TOKEN_BUDGET,
                "chunk has {tokens} tokens, exceeds budget {CODE_CHUNK_TOKEN_BUDGET}"
            );
        }
    }

    #[test]
    fn chunk_code_oversized_cjk_line_is_split() {
        // Single line of 2000 CJK chars — no newlines
        let line = "字".repeat(2000);
        let out = chunk_code(&line);
        assert!(
            out.len() > 1,
            "expected multiple chunks for oversized CJK line, got {}",
            out.len()
        );
        for (text, s, e) in &out {
            assert_eq!(*s, 1, "start_line should be 1");
            assert_eq!(*e, 1, "end_line should be 1");
            let tokens = estimate_tokens(text);
            assert!(
                tokens <= CODE_CHUNK_TOKEN_BUDGET,
                "split piece has {tokens} tokens, exceeds budget"
            );
        }
    }

    #[test]
    fn ingest_prose_mode_unchanged_when_no_file_kind() {
        use crate::embed::HashEmbedder;
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();
        let e = HashEmbedder::new(32);
        let new = NewDoc {
            key: "note".into(),
            title: "note".into(),
            content: "ping @bob about the launch".into(),
            author: "a".into(),
            taint: Taint::Internal,
            meta: None,
        };
        let doc = ingest_document(&store, &e, "alice", &new).unwrap();
        let rows = store.chunks_for_doc("alice", &doc.document_id).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].line_start, rows[0].line_end), (None, None));
        assert!(rows[0].entities.contains(&"handle:bob".to_string()));
        assert!(rows[0].entities.iter().all(|e| !e.starts_with("sym:")));
    }

    // Test-only embedder that fails on a specific call number (1-based).
    // If `always_fail` is true, every call fails regardless of `fail_on_call`.
    struct FailNthEmbedder {
        inner: crate::embed::HashEmbedder,
        fail_on_call: usize,
        always_fail: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl crate::embed::Embedder for FailNthEmbedder {
        fn embed(&self, text: &str) -> crate::error::Result<Vec<f32>> {
            let n = self
                .calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                + 1;
            if self.always_fail || n == self.fail_on_call {
                return Err(crate::error::Error::Embed("boom".into()));
            }
            self.inner.embed(text)
        }
        fn signature(&self) -> String {
            self.inner.signature()
        }
        fn dim(&self) -> usize {
            self.inner.dim()
        }
    }

    /// A code-mode doc whose content is long enough to produce at least 2 chunks from
    /// `chunk_code`. The embedder fails on chunk 2; `ingest_document` should still return Ok
    /// and store (total_chunks - 1) chunks.
    #[test]
    fn ingest_skips_failed_chunk() {
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();

        // Build a code doc with content large enough to be split into ≥2 chunks by chunk_code.
        // Each chunk_code chunk is up to MAX_CODE_CHUNK_CHARS (1500) chars; two 800-char lines
        // guarantees exactly 2 chunks (800+1 > 1500? No: 800+800+newline = 1601 > 1500, so the
        // second line starts a new chunk). Actually: cur = "aaa...\n" (801 chars); adding
        // "bbb...\n" (801) would make 1602 > 1500, so flush. Two chunks.
        let line_a = "a".repeat(800);
        let line_b = "b".repeat(800);
        let content = format!("{line_a}\n{line_b}");

        // Verify chunk_code yields exactly 2 chunks with this content.
        let pieces = chunk_code(&content);
        let total = pieces.len();
        assert!(total >= 2, "need ≥2 chunks for this test, got {total}");

        let embedder = FailNthEmbedder {
            inner: crate::embed::HashEmbedder::new(32),
            fail_on_call: 2,
            always_fail: false,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let new = NewDoc {
            key: "src/big.rs".into(),
            title: "src/big.rs".into(),
            content,
            author: "bot".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file"})),
        };
        let doc = ingest_document(&store, &embedder, "ns", &new)
            .expect("ingest_document should succeed even when one chunk fails to embed");

        let rows = store.chunks_for_doc("ns", &doc.document_id).unwrap();
        assert_eq!(
            rows.len(),
            total - 1,
            "expected {} chunks (skipped 1 failed), got {}",
            total - 1,
            rows.len()
        );
    }

    /// A code-mode doc where EVERY chunk fails to embed. `ingest_document` should return Err.
    #[test]
    fn ingest_errors_when_all_chunks_fail() {
        use crate::model::{NewDoc, Taint};
        use crate::store::Store;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("t.db").to_str().unwrap()).unwrap();

        let line_a = "a".repeat(800);
        let line_b = "b".repeat(800);
        let content = format!("{line_a}\n{line_b}");

        let pieces = chunk_code(&content);
        assert!(pieces.len() >= 1, "need ≥1 chunk for this test");

        let embedder = FailNthEmbedder {
            inner: crate::embed::HashEmbedder::new(32),
            fail_on_call: 0, // unused when always_fail is true
            always_fail: true,
            calls: std::sync::atomic::AtomicUsize::new(0),
        };

        let new = NewDoc {
            key: "src/fail.rs".into(),
            title: "src/fail.rs".into(),
            content,
            author: "bot".into(),
            taint: Taint::Internal,
            meta: Some(serde_json::json!({"kind": "file"})),
        };
        let result = ingest_document(&store, &embedder, "ns", &new);
        assert!(
            result.is_err(),
            "expected Err when all chunks fail to embed, got Ok"
        );
    }
}
