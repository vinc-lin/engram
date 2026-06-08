/// Returns true if a file should be sent to engram, based solely on its
/// repo-relative path and byte size.  All the logic is pure (no I/O) so it
/// is trivially unit-testable.
pub fn should_index(relpath: &str, size_bytes: u64) -> bool {
    // 256 KiB hard ceiling
    if size_bytes > 256_000 {
        return false;
    }

    // Extract the basename (the last path component)
    let basename = relpath.rsplit('/').next().unwrap_or(relpath);

    // Skip known lockfiles
    const LOCKFILES: &[&str] = &[
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "Cargo.lock",
        "bun.lockb",
    ];
    if LOCKFILES.contains(&basename) {
        return false;
    }

    // Skip binary / asset extensions (suffix-based — handles dotfiles correctly)
    // Also skip .map and .min.js
    let lower = relpath.to_ascii_lowercase();

    // .min.js check (suffix)
    if lower.ends_with(".min.js") {
        return false;
    }

    // .map check
    if lower.ends_with(".map") {
        return false;
    }

    // Plain extension check
    const BINARY_EXTS: &[&str] = &[
        ".svg", ".png", ".gif", ".jpg", ".jpeg", ".ico", ".webp", ".woff", ".woff2", ".ttf",
        ".eot", ".pdf", ".mp3", ".mp4", ".wav",
    ];
    for ext in BINARY_EXTS {
        if lower.ends_with(ext) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── binary / asset extensions ───────────────────────────────────────────
    #[test]
    fn should_index_skips_png() {
        assert!(!should_index("assets/logo.png", 1_000));
    }

    #[test]
    fn should_index_skips_svg() {
        assert!(!should_index("icons/arrow.svg", 500));
    }

    #[test]
    fn should_index_skips_jpg() {
        assert!(!should_index("photo.jpg", 50_000));
    }

    #[test]
    fn should_index_skips_jpeg() {
        assert!(!should_index("photo.jpeg", 50_000));
    }

    #[test]
    fn should_index_skips_gif() {
        assert!(!should_index("anim.gif", 1_000));
    }

    #[test]
    fn should_index_skips_ico() {
        assert!(!should_index("favicon.ico", 1_024));
    }

    #[test]
    fn should_index_skips_webp() {
        assert!(!should_index("image.webp", 1_000));
    }

    #[test]
    fn should_index_skips_woff() {
        assert!(!should_index("font.woff", 20_000));
    }

    #[test]
    fn should_index_skips_woff2() {
        assert!(!should_index("font.woff2", 20_000));
    }

    #[test]
    fn should_index_skips_ttf() {
        assert!(!should_index("font.ttf", 20_000));
    }

    #[test]
    fn should_index_skips_eot() {
        assert!(!should_index("font.eot", 20_000));
    }

    #[test]
    fn should_index_skips_pdf() {
        assert!(!should_index("doc.pdf", 10_000));
    }

    #[test]
    fn should_index_skips_mp3() {
        assert!(!should_index("audio.mp3", 100_000));
    }

    #[test]
    fn should_index_skips_mp4() {
        assert!(!should_index("video.mp4", 100_000));
    }

    #[test]
    fn should_index_skips_wav() {
        assert!(!should_index("audio.wav", 100_000));
    }

    #[test]
    fn should_index_skips_map() {
        assert!(!should_index("bundle.js.map", 500_000));
    }

    #[test]
    fn should_index_skips_min_js() {
        assert!(!should_index("dist/app.min.js", 80_000));
    }

    // ── lockfiles ────────────────────────────────────────────────────────────
    #[test]
    fn should_index_skips_package_lock_json() {
        assert!(!should_index("package-lock.json", 50_000));
    }

    #[test]
    fn should_index_skips_pnpm_lock_yaml() {
        assert!(!should_index("pnpm-lock.yaml", 20_000));
    }

    #[test]
    fn should_index_skips_yarn_lock() {
        assert!(!should_index("yarn.lock", 30_000));
    }

    #[test]
    fn should_index_skips_cargo_lock() {
        assert!(!should_index("Cargo.lock", 10_000));
    }

    #[test]
    fn should_index_skips_bun_lockb() {
        assert!(!should_index("bun.lockb", 5_000));
    }

    // ── nested lockfile path ─────────────────────────────────────────────────
    #[test]
    fn should_index_skips_nested_lockfile() {
        assert!(!should_index("packages/app/package-lock.json", 50_000));
    }

    // ── oversized ────────────────────────────────────────────────────────────
    #[test]
    fn should_index_skips_oversized() {
        assert!(!should_index("large.rs", 256_001));
    }

    #[test]
    fn should_index_accepts_exactly_at_limit() {
        assert!(should_index("large.rs", 256_000));
    }

    // ── accepted files ────────────────────────────────────────────────────────
    #[test]
    fn should_index_accepts_ts_file() {
        assert!(should_index("src/app.ts", 5_000));
    }

    #[test]
    fn should_index_accepts_rs_file() {
        assert!(should_index("src/main.rs", 3_000));
    }

    #[test]
    fn should_index_accepts_js_file() {
        assert!(should_index("src/app.js", 5_000));
    }

    #[test]
    fn should_index_accepts_md_file() {
        assert!(should_index("README.md", 10_000));
    }

    #[test]
    fn should_index_accepts_toml_file() {
        assert!(should_index("Cargo.toml", 1_000));
    }

    #[test]
    fn should_index_accepts_yaml_file() {
        assert!(should_index(".github/workflows/ci.yml", 2_000));
    }

    #[test]
    fn should_index_accepts_file_with_no_extension() {
        assert!(should_index("Makefile", 5_000));
    }
}
