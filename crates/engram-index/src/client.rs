use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::blocking::Client;
use serde_json::json;
use std::time::Duration;

/// Build the URL for `DELETE /v1/{namespace}/docs/by-key/{encoded_key}`.
///
/// The namespace is placed verbatim (colons are valid URL path characters and
/// the server treats `repo:<id>` as-is).  The key (a repo-relative path that
/// may contain slashes and spaces) is percent-encoded so the entire key lands
/// in one path segment.
pub fn build_by_key_url(base_url: &str, namespace: &str, key: &str) -> String {
    // Encode the key as a single path segment: every non-alphanumeric byte is
    // percent-encoded, including '/' and ' '.
    let encoded_key = utf8_percent_encode(key, NON_ALPHANUMERIC).to_string();
    let base = base_url.trim_end_matches('/');
    format!("{}/v1/{}/docs/by-key/{}", base, namespace, encoded_key)
}

// ── HTTP helpers ──────────────────────────────────────────────────────────────

/// POST a document to engram.  Retries up to 3 times on transport errors or 5xx.
pub fn post_doc(
    client: &Client,
    base_url: &str,
    namespace: &str,
    token: &str,
    key: &str,
    content: &str,
    author: &str,
) -> Result<(), String> {
    let url = format!("{}/v1/{}/docs", base_url.trim_end_matches('/'), namespace);
    let body = json!({
        "key": key,
        "title": key,
        "content": content,
        "author": author,
        "meta": { "kind": "file" }
    });

    retry(3, || {
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(|e| RetryableError::Transport(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else if status.is_server_error() {
            Err(RetryableError::Server(status.as_u16()))
        } else {
            Err(RetryableError::Client(status.as_u16()))
        }
    })
}

/// DELETE a document by key.  Not retried on 404 (idempotent anyway).
pub fn delete_doc(
    client: &Client,
    base_url: &str,
    namespace: &str,
    token: &str,
    key: &str,
) -> Result<(), String> {
    let url = build_by_key_url(base_url, namespace, key);

    retry(3, || {
        let resp = client
            .delete(&url)
            .bearer_auth(token)
            .timeout(Duration::from_secs(30))
            .send()
            .map_err(|e| RetryableError::Transport(e.to_string()))?;

        let status = resp.status();
        if status.is_success() || status.as_u16() == 404 {
            Ok(())
        } else if status.is_server_error() {
            Err(RetryableError::Server(status.as_u16()))
        } else {
            Err(RetryableError::Client(status.as_u16()))
        }
    })
}

// ── retry internals ───────────────────────────────────────────────────────────

enum RetryableError {
    Transport(String),
    Server(u16),
    Client(u16),
}

fn retry<F>(max_attempts: u32, mut f: F) -> Result<(), String>
where
    F: FnMut() -> Result<(), RetryableError>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match f() {
            Ok(()) => return Ok(()),
            Err(RetryableError::Client(code)) => {
                return Err(format!("HTTP {code} (client error, no retry)"));
            }
            Err(RetryableError::Transport(msg)) if attempt >= max_attempts => {
                return Err(format!("transport error after {attempt} attempt(s): {msg}"));
            }
            Err(RetryableError::Server(code)) if attempt >= max_attempts => {
                return Err(format!(
                    "HTTP {code} (server error) after {attempt} attempt(s)"
                ));
            }
            Err(_) => {
                let secs = attempt; // 1s, 2s
                std::thread::sleep(Duration::from_secs(secs as u64));
            }
        }
    }
}

// ── unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_key_url_encodes_spaces() {
        let url = build_by_key_url("http://127.0.0.1:8088", "repo:myrepo", "src/a b.ts");
        // Slashes and spaces must be encoded (dots are also encoded by NON_ALPHANUMERIC)
        assert!(url.contains("%2F"), "slash not encoded; got: {url}");
        assert!(url.contains("%20"), "space not encoded; got: {url}");
        assert!(url.starts_with("http://127.0.0.1:8088/v1/repo:myrepo/docs/by-key/"));
        // The "src" and "a b" parts should appear (possibly with more encoding)
        assert!(url.contains("src"), "got: {url}");
    }

    #[test]
    fn by_key_url_encodes_slashes_in_key() {
        let url = build_by_key_url("http://host", "repo:x", "deep/nested/file.rs");
        // All slashes in the key become %2F so it's a single path segment
        assert!(url.contains("%2F"), "slash not encoded; got: {url}");
        // The words should appear in the url
        assert!(url.contains("deep"), "got: {url}");
        assert!(url.contains("nested"), "got: {url}");
        assert!(url.contains("file"), "got: {url}");
    }

    #[test]
    fn by_key_url_namespace_colon_not_encoded() {
        let url = build_by_key_url("http://host", "repo:abc", "file.rs");
        // The colon in the namespace must NOT be percent-encoded
        assert!(url.contains("/v1/repo:abc/"), "got: {url}");
    }

    #[test]
    fn by_key_url_trailing_slash_stripped_from_base() {
        let url1 = build_by_key_url("http://host/", "repo:x", "f.rs");
        let url2 = build_by_key_url("http://host", "repo:x", "f.rs");
        assert_eq!(url1, url2);
    }
}
