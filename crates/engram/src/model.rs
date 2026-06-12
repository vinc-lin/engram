use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Taint {
    #[default]
    Internal,
    ExternalSync,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDoc {
    pub document_id: String,
    pub namespace: String,
    pub key: String,
    pub title: String,
    pub content: String,
    pub author: String,
    #[serde(default)]
    pub taint: Taint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    pub created_at: f64,
    pub updated_at: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDoc {
    pub key: String,
    pub title: String,
    pub content: String,
    pub author: String,
    #[serde(default)]
    pub taint: Taint,
    #[serde(default)]
    pub meta: Option<serde_json::Value>,
}

/// The semantic relationship carried by a directed code-graph edge.
/// Stored as the canonical uppercase strings "CALLS", "USES_TYPE", "IMPORTS"
/// in `code_edges.edge_kind` (TEXT NOT NULL).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EdgeKind {
    Calls,
    UsesType,
    Imports,
}

impl EdgeKind {
    /// Canonical uppercase string stored in SQLite and returned in API responses.
    pub fn as_str(&self) -> &'static str {
        match self {
            EdgeKind::Calls => "CALLS",
            EdgeKind::UsesType => "USES_TYPE",
            EdgeKind::Imports => "IMPORTS",
        }
    }

    /// Parse the canonical uppercase string. Returns `None` for any other input
    /// (case-sensitive — the DB stores exactly these strings).
    pub fn parse(s: &str) -> Option<EdgeKind> {
        match s {
            "CALLS" => Some(EdgeKind::Calls),
            "USES_TYPE" => Some(EdgeKind::UsesType),
            "IMPORTS" => Some(EdgeKind::Imports),
            _ => None,
        }
    }
}

/// One directed edge extracted from source text, before the write lock.
///
/// `src_doc_id` is intentionally absent: the edge is produced by `graph::extract_edges`
/// (which runs off-lock, before `commit_ingest` assigns a document id) and is passed into
/// `commit_ingest` as a slice. The store layer stamps `src_doc_id` from the freshly-assigned
/// doc id inside the transaction.
///
/// `dst_doc_id` is resolved lazily at query time via `chunk_entities` (cross-file resolution,
/// Option a) — it is NULL in the database until `graph_query` resolves it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawEdge {
    /// Target symbol in canonical entity form, e.g. `"sym:FooBar"`.
    pub dst_sym: String,
    pub edge_kind: EdgeKind,
    /// Source line number within the file (1-based), if known.
    pub src_line: Option<i64>,
    /// Extraction confidence in [0.0, 1.0].
    pub confidence: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taint_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&Taint::ExternalSync).unwrap(),
            "\"external_sync\""
        );
        assert_eq!(Taint::default(), Taint::Internal);
    }

    #[test]
    fn newdoc_defaults_taint_internal() {
        let n: NewDoc =
            serde_json::from_str(r#"{"key":"k","title":"t","content":"c","author":"alice"}"#)
                .unwrap();
        assert_eq!(n.taint, Taint::Internal);
    }

    #[test]
    fn meta_round_trips_and_defaults_none() {
        // absent → None
        let n: NewDoc =
            serde_json::from_str(r#"{"key":"k","title":"t","content":"c","author":"a"}"#).unwrap();
        assert!(n.meta.is_none());
        // present → preserved as opaque JSON
        let n2: NewDoc = serde_json::from_str(
            r#"{"key":"k","title":"t","content":"c","author":"a","meta":{"category":"fact","importance":0.8}}"#,
        ).unwrap();
        assert_eq!(n2.meta.as_ref().unwrap()["category"], "fact");
    }

    #[test]
    fn edge_kind_as_str_round_trips() {
        assert_eq!(EdgeKind::Calls.as_str(), "CALLS");
        assert_eq!(EdgeKind::UsesType.as_str(), "USES_TYPE");
        assert_eq!(EdgeKind::Imports.as_str(), "IMPORTS");

        assert_eq!(EdgeKind::parse("CALLS"), Some(EdgeKind::Calls));
        assert_eq!(EdgeKind::parse("USES_TYPE"), Some(EdgeKind::UsesType));
        assert_eq!(EdgeKind::parse("IMPORTS"), Some(EdgeKind::Imports));
        assert_eq!(EdgeKind::parse("calls"), None); // case-sensitive
        assert_eq!(EdgeKind::parse("UNKNOWN"), None);
    }

    #[test]
    fn raw_edge_fields_accessible() {
        let e = RawEdge {
            dst_sym: "sym:FooBar".into(),
            edge_kind: EdgeKind::Calls,
            src_line: Some(42),
            confidence: 0.9,
        };
        assert_eq!(e.dst_sym, "sym:FooBar");
        assert_eq!(e.edge_kind.as_str(), "CALLS");
        assert_eq!(e.src_line, Some(42));
        assert!((e.confidence - 0.9).abs() < 1e-6);
        // no src_doc_id field on RawEdge — verify by exhaustive struct literal above compiling
    }
}
