//! Parse `git log` output into commit records for ingesting repo history as documents in the
//! `repo:<id>:history` namespace. The parsing is pure + unit-tested; the git invocation and the
//! HTTP POST live in `main.rs`.

use serde_json::json;

/// One commit's metadata + changed files.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitRecord {
    pub sha: String,
    pub author: String, // "Name <email>"
    pub email: String,
    pub timestamp: i64,
    pub subject: String,
    pub body: String,
    pub files: Vec<String>,
}

/// The `--pretty=format:` string (paired with `--name-only`) that [`parse_git_log`] expects.
/// Records are prefixed with SOH (\x01); header fields are separated by US (\x1f) and the header
/// is terminated by RS (\x1e), after which `--name-only` lists the changed files, one per line.
pub const GIT_LOG_FORMAT: &str = "\u{01}%H\u{1f}%an <%ae>\u{1f}%ae\u{1f}%ct\u{1f}%s\u{1f}%b\u{1e}";

/// Parse the output of
/// `git log --no-merges --name-only --pretty=format:GIT_LOG_FORMAT`.
pub fn parse_git_log(output: &str) -> Vec<CommitRecord> {
    let mut out = Vec::new();
    for rec in output.split('\u{01}') {
        if rec.trim().is_empty() {
            continue;
        }
        let (header, files_blob) = match rec.split_once('\u{1e}') {
            Some(x) => x,
            None => continue,
        };
        let f: Vec<&str> = header.split('\u{1f}').collect();
        if f.len() < 6 {
            continue;
        }
        let files: Vec<String> = files_blob
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();
        out.push(CommitRecord {
            sha: f[0].trim().to_string(),
            author: f[1].trim().to_string(),
            email: f[2].trim().to_string(),
            timestamp: f[3].trim().parse().unwrap_or(0),
            subject: f[4].trim().to_string(),
            body: f[5].trim().to_string(),
            files,
        });
    }
    out
}

impl CommitRecord {
    /// Document key: `commit:<sha>` (immutable, idempotent upsert).
    pub fn key(&self) -> String {
        format!("commit:{}", self.sha)
    }

    /// Document title: the subject (truncated), or the key if empty.
    pub fn title(&self) -> String {
        let s: String = self.subject.chars().take(72).collect();
        if s.is_empty() {
            self.key()
        } else {
            s
        }
    }

    /// Document content: subject + body + the changed-file list (so `path:` entities link to code).
    pub fn content(&self) -> String {
        let mut c = self.subject.clone();
        if !self.body.is_empty() {
            c.push_str("\n\n");
            c.push_str(&self.body);
        }
        if !self.files.is_empty() {
            c.push_str("\n\nChanged files:\n");
            c.push_str(&self.files.join("\n"));
        }
        c
    }

    /// Opaque `meta` carrying structured commit metadata.
    pub fn meta(&self) -> serde_json::Value {
        json!({
            "kind": "commit",
            "sha": self.sha,
            "ts": self.timestamp,
            "files": self.files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> String {
        // Two commits in the GIT_LOG_FORMAT shape, each followed by --name-only file lists.
        format!(
            "{0}a1b2c3{1}Ada <ada@x.io>{1}ada@x.io{1}1700000000{1}fix: take the write lock{1}body line one\nbody line two{2}\nsrc/store.rs\nsrc/tree.rs\n\n{0}d4e5f6{1}Bo <bo@y.io>{1}bo@y.io{1}1700000100{1}feat: add cosine{1}{2}\nsrc/vec.rs\n",
            "\u{01}", "\u{1f}", "\u{1e}"
        )
    }

    #[test]
    fn parses_two_commits_with_files() {
        let recs = parse_git_log(&sample());
        assert_eq!(recs.len(), 2);
        let c0 = &recs[0];
        assert_eq!(c0.sha, "a1b2c3");
        assert_eq!(c0.email, "ada@x.io");
        assert_eq!(c0.timestamp, 1_700_000_000);
        assert_eq!(c0.subject, "fix: take the write lock");
        assert!(c0.body.contains("body line one") && c0.body.contains("body line two"));
        assert_eq!(c0.files, vec!["src/store.rs", "src/tree.rs"]);
        // Empty body is fine.
        assert_eq!(recs[1].body, "");
        assert_eq!(recs[1].files, vec!["src/vec.rs"]);
    }

    #[test]
    fn commit_doc_fields_carry_files_and_meta() {
        let recs = parse_git_log(&sample());
        let c = &recs[0];
        assert_eq!(c.key(), "commit:a1b2c3");
        assert_eq!(c.title(), "fix: take the write lock");
        let content = c.content();
        assert!(content.contains("take the write lock"));
        assert!(content.contains("src/store.rs") && content.contains("src/tree.rs"));
        let meta = c.meta();
        assert_eq!(meta["kind"], "commit");
        assert_eq!(meta["sha"], "a1b2c3");
        assert_eq!(meta["ts"], 1_700_000_000);
        assert_eq!(meta["files"][0], "src/store.rs");
    }

    #[test]
    fn empty_log_is_no_commits() {
        assert!(parse_git_log("").is_empty());
        assert!(parse_git_log("\n\n").is_empty());
    }
}
