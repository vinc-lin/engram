use clap::{Parser, Subcommand};
use engram_index::{
    client::{delete_doc, post_doc, post_doc_with_meta},
    commits::{parse_git_log, GIT_LOG_FORMAT},
    hook::install_post_commit_hook,
    parse_diff_line, should_index, DiffEntry,
};
use reqwest::blocking::Client;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
};

// ── CLI definition ─────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "engram-index", about = "Index a git repo into engram")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Walk all tracked files and POST them to engram.
    Index(IndexArgs),
    /// Apply changes since last index run (falls back to full index).
    Reindex(IndexArgs),
    /// Ingest git history (commits) into the history namespace for `why` / `find_symbol`.
    IndexHistory(HistoryArgs),
    /// Install a post-commit git hook that calls `reindex`.
    InstallHook(HookArgs),
}

#[derive(clap::Args, Clone)]
struct IndexArgs {
    /// Path to the git repository root.
    path: PathBuf,

    #[arg(long)]
    namespace: String,

    /// Base URL of the engram server (env: ENGRAM_URL).
    #[arg(long, env = "ENGRAM_URL", default_value = "http://127.0.0.1:8088")]
    url: String,

    /// Bearer token (env: ENGRAM_TOKEN).
    #[arg(long, env = "ENGRAM_TOKEN", default_value = "")]
    token: String,

    /// Author field on every ingested doc.
    #[arg(long, default_value = "indexer")]
    author: String,

    /// Number of parallel upload threads.
    #[arg(long, default_value_t = 6)]
    concurrency: usize,
}

#[derive(clap::Args)]
struct HistoryArgs {
    /// Path to the git repository root.
    path: PathBuf,

    /// History namespace (convention: `repo:<id>:history`).
    #[arg(long)]
    namespace: String,

    /// Base URL of the engram server (env: ENGRAM_URL).
    #[arg(long, env = "ENGRAM_URL", default_value = "http://127.0.0.1:8088")]
    url: String,

    /// Bearer token (env: ENGRAM_TOKEN).
    #[arg(long, env = "ENGRAM_TOKEN", default_value = "")]
    token: String,

    /// Limit to the most recent N commits (0 = all).
    #[arg(long, default_value_t = 0)]
    max: usize,
}

#[derive(clap::Args)]
struct HookArgs {
    /// Path to the git repository root.
    path: PathBuf,

    #[arg(long)]
    namespace: String,

    /// Base URL written into the hook script (env: ENGRAM_URL).
    #[arg(long, env = "ENGRAM_URL", default_value = "http://127.0.0.1:8088")]
    url: String,
}

// ── entry point ────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::Index(args) => cmd_index(args, false),
        Commands::Reindex(args) => cmd_index(args, true),
        Commands::IndexHistory(args) => cmd_index_history(args),
        Commands::InstallHook(args) => cmd_install_hook(args),
    };
    if let Err(e) = result {
        eprintln!("engram-index: error: {e}");
        std::process::exit(1);
    }
}

// ── subcommands ────────────────────────────────────────────────────────────────

fn cmd_index(args: IndexArgs, incremental: bool) -> Result<(), String> {
    let repo = args
        .path
        .canonicalize()
        .map_err(|e| format!("cannot resolve path: {e}"))?;

    let client = make_client()?;

    if incremental {
        if let Some(last_sha) = read_last_sha(&repo) {
            let head_sha = git_head(&repo)?;
            if last_sha == head_sha {
                println!("engram-index: already up-to-date (HEAD {head_sha})");
                return Ok(());
            }
            return run_incremental(&repo, &args, &client, &last_sha);
        }
        // Fall through to full index
        eprintln!("engram-index: no last-sha found, falling back to full index");
    }

    run_full_index(&repo, &args, &client)
}

fn cmd_install_hook(args: HookArgs) -> Result<(), String> {
    let repo = args
        .path
        .canonicalize()
        .map_err(|e| format!("cannot resolve path: {e}"))?;

    match install_post_commit_hook(&repo, &args.namespace, &args.url)
        .map_err(|e| format!("failed to install hook: {e}"))?
    {
        true => println!(
            "engram-index: installed post-commit hook in {}",
            repo.display()
        ),
        false => {
            eprintln!(
                "engram-index: WARNING — a post-commit hook already exists at {}/.git/hooks/post-commit",
                repo.display()
            );
            eprintln!(
                "  Add this line manually:  engram-index reindex {} --namespace {} --url {}",
                repo.display(),
                args.namespace,
                args.url,
            );
        }
    }
    Ok(())
}

// ── history index ──────────────────────────────────────────────────────────────

fn cmd_index_history(args: HistoryArgs) -> Result<(), String> {
    let repo = args
        .path
        .canonicalize()
        .map_err(|e| format!("cannot resolve path: {e}"))?;
    let client = make_client()?;
    let output = git_log_history(&repo, args.max)?;
    let commits = parse_git_log(&output);
    let total = commits.len();
    let mut ok = 0usize;
    let mut failed = 0usize;
    for c in &commits {
        let author = if c.email.is_empty() {
            "git"
        } else {
            c.email.as_str()
        };
        match post_doc_with_meta(
            &client,
            &args.url,
            &args.namespace,
            &args.token,
            &c.key(),
            &c.title(),
            &c.content(),
            author,
            c.meta(),
        ) {
            Ok(()) => ok += 1,
            Err(e) => {
                eprintln!("  FAIL {}: {e}", c.key());
                failed += 1;
            }
        }
    }
    println!("engram-index: history total={total} ok={ok} failed={failed}");
    if failed > 0 {
        return Err(format!("{failed} commit(s) failed to index"));
    }
    Ok(())
}

// ── full index ─────────────────────────────────────────────────────────────────

fn run_full_index(repo: &Path, args: &IndexArgs, client: &Client) -> Result<(), String> {
    let files = git_ls_files(repo)?;
    let total = files.len();

    let counters = Arc::new(Counters::default());
    let work_queue: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(files));

    let threads: Vec<_> = (0..args.concurrency)
        .map(|_| {
            let queue = Arc::clone(&work_queue);
            let counters = Arc::clone(&counters);
            let client = client.clone();
            let repo = repo.to_path_buf();
            let args = args.clone();

            thread::spawn(move || loop {
                let relpath = {
                    let mut q = queue.lock().unwrap();
                    q.pop()
                };
                let relpath = match relpath {
                    Some(p) => p,
                    None => break,
                };

                let abs = repo.join(&relpath);
                let size = fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);

                if !should_index(&relpath, size) {
                    counters.skipped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }

                let content = match fs::read(&abs) {
                    Ok(bytes) => match String::from_utf8(bytes) {
                        Ok(s) => s,
                        Err(_) => {
                            counters.skipped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    },
                    Err(_) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                };

                match post_doc(
                    &client,
                    &args.url,
                    &args.namespace,
                    &args.token,
                    &relpath,
                    &content,
                    &args.author,
                ) {
                    Ok(()) => {
                        counters.ok.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("  FAIL {relpath}: {e}");
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for t in threads {
        t.join().ok();
    }

    let ok = counters.ok.load(Ordering::Relaxed);
    let skipped = counters.skipped.load(Ordering::Relaxed);
    let failed = counters.failed.load(Ordering::Relaxed);
    println!("engram-index: total={total} ok={ok} skipped={skipped} failed={failed}");

    if failed > 0 {
        return Err(format!("{failed} file(s) failed to index"));
    }

    write_last_sha(repo, &git_head(repo)?)?;
    Ok(())
}

// ── incremental index ──────────────────────────────────────────────────────────

fn run_incremental(
    repo: &Path,
    args: &IndexArgs,
    client: &Client,
    last_sha: &str,
) -> Result<(), String> {
    let diff_output = git_diff_name_status(repo, last_sha)?;
    let entries: Vec<DiffEntry> = diff_output.lines().filter_map(parse_diff_line).collect();

    let total = entries.len();
    let mut ok = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for entry in entries {
        match entry {
            DiffEntry::Added(path) | DiffEntry::Modified(path) => {
                let abs = repo.join(&path);
                let size = fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                if !should_index(&path, size) {
                    skipped += 1;
                    continue;
                }
                let content = match fs::read(&abs) {
                    Ok(b) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    },
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                };
                match post_doc(
                    client,
                    &args.url,
                    &args.namespace,
                    &args.token,
                    &path,
                    &content,
                    &args.author,
                ) {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        eprintln!("  FAIL {path}: {e}");
                        failed += 1;
                    }
                }
            }
            DiffEntry::Deleted(path) => {
                match delete_doc(client, &args.url, &args.namespace, &args.token, &path) {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        eprintln!("  FAIL DELETE {path}: {e}");
                        failed += 1;
                    }
                }
            }
            DiffEntry::Renamed { old, new } => {
                // Delete old key, post new file
                let _ = delete_doc(client, &args.url, &args.namespace, &args.token, &old);
                let abs = repo.join(&new);
                let size = fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                if !should_index(&new, size) {
                    skipped += 1;
                    continue;
                }
                let content = match fs::read(&abs) {
                    Ok(b) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    },
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                };
                match post_doc(
                    client,
                    &args.url,
                    &args.namespace,
                    &args.token,
                    &new,
                    &content,
                    &args.author,
                ) {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        eprintln!("  FAIL {new}: {e}");
                        failed += 1;
                    }
                }
            }
            DiffEntry::Copied(path) => {
                let abs = repo.join(&path);
                let size = fs::metadata(&abs).map(|m| m.len()).unwrap_or(0);
                if !should_index(&path, size) {
                    skipped += 1;
                    continue;
                }
                let content = match fs::read(&abs) {
                    Ok(b) => match String::from_utf8(b) {
                        Ok(s) => s,
                        Err(_) => {
                            skipped += 1;
                            continue;
                        }
                    },
                    Err(_) => {
                        failed += 1;
                        continue;
                    }
                };
                match post_doc(
                    client,
                    &args.url,
                    &args.namespace,
                    &args.token,
                    &path,
                    &content,
                    &args.author,
                ) {
                    Ok(()) => ok += 1,
                    Err(e) => {
                        eprintln!("  FAIL {path}: {e}");
                        failed += 1;
                    }
                }
            }
        }
    }

    println!("engram-index: total={total} ok={ok} skipped={skipped} failed={failed}");
    if failed > 0 {
        return Err(format!("{failed} file(s) failed"));
    }

    write_last_sha(repo, &git_head(repo)?)?;
    Ok(())
}

// ── git helpers ────────────────────────────────────────────────────────────────

fn git_ls_files(repo: &Path) -> Result<Vec<String>, String> {
    let out = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "ls-files"])
        .output()
        .map_err(|e| format!("git ls-files failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git ls-files error: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn git_head(repo: &Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("git rev-parse HEAD failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn git_diff_name_status(repo: &Path, last_sha: &str) -> Result<String, String> {
    let range = format!("{last_sha}..HEAD");
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &repo.to_string_lossy(),
            "diff",
            "--name-status",
            "-M",
            &range,
        ])
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_log_history(repo: &Path, max: usize) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args([
        "-C",
        &repo.to_string_lossy(),
        "log",
        "--no-merges",
        "--name-only",
        &format!("--pretty=format:{GIT_LOG_FORMAT}"),
    ]);
    if max > 0 {
        cmd.arg(format!("-n{max}"));
    }
    let out = cmd.output().map_err(|e| format!("git log failed: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// ── last-sha marker ────────────────────────────────────────────────────────────

fn marker_path(repo: &Path) -> PathBuf {
    repo.join(".git").join("engram").join("last-sha")
}

fn read_last_sha(repo: &Path) -> Option<String> {
    let path = marker_path(repo);
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn write_last_sha(repo: &Path, sha: &str) -> Result<(), String> {
    let path = marker_path(repo);
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    fs::write(&path, sha).map_err(|e| e.to_string())?;
    Ok(())
}

// ── misc ───────────────────────────────────────────────────────────────────────

fn make_client() -> Result<Client, String> {
    Client::builder()
        .build()
        .map_err(|e| format!("failed to build HTTP client: {e}"))
}

#[derive(Default)]
struct Counters {
    ok: AtomicUsize,
    skipped: AtomicUsize,
    failed: AtomicUsize,
}
