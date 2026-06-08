use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Write a post-commit hook that calls `engram-index reindex`.
///
/// Returns `Ok(true)` when the hook was written, `Ok(false)` when a hook
/// already existed (caller should warn the user), or an error on I/O failure.
pub fn install_post_commit_hook(repo_path: &Path, namespace: &str, url: &str) -> io::Result<bool> {
    let hooks_dir = repo_path.join(".git").join("hooks");
    fs::create_dir_all(&hooks_dir)?;

    let hook_path = hooks_dir.join("post-commit");
    if hook_path.exists() {
        return Ok(false);
    }

    // Resolve the canonical absolute path so the hook works from any cwd.
    let abs_repo = repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf());

    let script = format!(
        r#"#!/bin/sh
# Added by engram-index install-hook
engram-index reindex {abs} --namespace {ns} --url {url}
"#,
        abs = abs_repo.display(),
        ns = namespace,
        url = url,
    );

    fs::write(&hook_path, &script)?;
    fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755))?;
    Ok(true)
}
