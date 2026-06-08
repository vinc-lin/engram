use crate::error::Result;
use crate::model::MemoryDoc;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Metadata for a sealed summary node's markdown mirror.
pub struct NodeMeta<'a> {
    pub namespace: &'a str,
    pub tree_kind: &'a str,
    pub tree_key: &'a str,
    pub node_id: &'a str,
    pub level: i64,
    pub label: Option<&'a str>,
    pub body: &'a str,
    pub created_at: f64,
}

/// Write-only Obsidian-style markdown mirror. SQLite is the source of truth; this is for
/// human browsing (Obsidian graph view) + inspectability. Opt-in via `ENGRAM_VAULT_DIR`.
pub struct Vault {
    root: PathBuf,
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn yaml(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Body-only content hash already recorded in an existing file, if any.
fn existing_sha(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    content.lines().find_map(|l| {
        l.strip_prefix("content_sha: ")
            .map(|s| s.trim().to_string())
    })
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)?;
    Ok(())
}

impl Vault {
    pub fn new(dir: &str) -> Self {
        Self {
            root: PathBuf::from(dir),
        }
    }

    pub fn write_doc(&self, doc: &MemoryDoc) -> Result<()> {
        let sha = sha256_hex(&doc.content);
        let path = self
            .root
            .join(&doc.namespace)
            .join("docs")
            .join(format!("{}.md", slug(&doc.document_id)));
        if existing_sha(&path).as_deref() == Some(sha.as_str()) {
            return Ok(());
        }
        let taint = match doc.taint {
            crate::model::Taint::Internal => "internal",
            crate::model::Taint::ExternalSync => "external_sync",
        };
        let content = format!(
            "---\nnamespace: {}\ndocument_id: {}\nkey: {}\ntitle: {}\nauthor: {}\ntaint: {}\ncreated_at: {}\nupdated_at: {}\ncontent_sha: {}\n---\n{}\n",
            doc.namespace, doc.document_id, yaml(&doc.key), yaml(&doc.title), yaml(&doc.author),
            taint, doc.created_at, doc.updated_at, sha, doc.content,
        );
        write_atomic(&path, &content)
    }

    pub fn write_node(&self, m: NodeMeta) -> Result<()> {
        let sha = sha256_hex(m.body);
        let path = self
            .root
            .join(m.namespace)
            .join(m.tree_kind)
            .join(slug(m.tree_key))
            .join(format!("L{}-{}.md", m.level, slug(m.node_id)));
        if existing_sha(&path).as_deref() == Some(sha.as_str()) {
            return Ok(());
        }
        let content = format!(
            "---\nnamespace: {}\ntree_kind: {}\ntree_key: {}\nnode_id: {}\nlevel: {}\nlabel: {}\nsealed: true\ncreated_at: {}\ncontent_sha: {}\n---\n{}\n",
            m.namespace, m.tree_kind, yaml(m.tree_key), m.node_id, m.level,
            yaml(m.label.unwrap_or("")), m.created_at, sha, m.body,
        );
        write_atomic(&path, &content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_doc_and_node_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vault::new(dir.path().to_str().unwrap());
        let doc = crate::model::MemoryDoc {
            document_id: "id1".into(),
            namespace: "alice".into(),
            key: "k".into(),
            title: "T".into(),
            content: "hello body".into(),
            author: "a".into(),
            taint: crate::model::Taint::Internal,
            meta: None,
            created_at: 1.0,
            updated_at: 2.0,
        };
        v.write_doc(&doc).unwrap();
        let p = dir.path().join("alice/docs/id1.md");
        let txt = std::fs::read_to_string(&p).unwrap();
        assert!(txt.contains("namespace: alice"));
        assert!(txt.contains("document_id: id1"));
        assert!(txt.contains("content_sha: "));
        assert!(txt.ends_with("hello body\n"));
        v.write_doc(&doc).unwrap(); // unchanged → skip; file still identical
        assert_eq!(std::fs::read_to_string(&p).unwrap(), txt);

        v.write_node(NodeMeta {
            namespace: "alice",
            tree_kind: "topic",
            tree_key: "handle:bob",
            node_id: "n1",
            level: 1,
            label: Some("L"),
            body: "summary",
            created_at: 3.0,
        })
        .unwrap();
        let np = dir.path().join("alice/topic/handle_bob/L1-n1.md");
        let nt = std::fs::read_to_string(&np).unwrap();
        assert!(nt.contains("tree_kind: topic"));
        assert!(nt.ends_with("summary\n"));
    }
}
