/// A single file-level change from `git diff --name-status -M`.
#[derive(Debug, PartialEq, Eq)]
pub enum DiffEntry {
    Added(String),
    Modified(String),
    Deleted(String),
    Renamed { old: String, new: String },
    Copied(String),
}

/// Parse one output line from `git diff --name-status -M <a>..<b>`.
///
/// Format reference:
///   `A\t<path>`            — added
///   `M\t<path>`            — modified
///   `D\t<path>`            — deleted
///   `R<score>\t<old>\t<new>` — renamed  (e.g. `R100\told.ts\tnew.ts`)
///   `C<score>\t<old>\t<new>` — copied   (treat as Added(new))
///
/// Returns `None` for blank lines or unrecognised formats (never panics).
pub fn parse_diff_line(line: &str) -> Option<DiffEntry> {
    let line = line.trim_end_matches('\n').trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }

    let parts: Vec<&str> = line.splitn(3, '\t').collect();
    match parts.as_slice() {
        // Two-field forms: A / M / D
        [status, path] => {
            let path = path.to_string();
            match *status {
                "A" => Some(DiffEntry::Added(path)),
                "M" => Some(DiffEntry::Modified(path)),
                "D" => Some(DiffEntry::Deleted(path)),
                _ => None,
            }
        }
        // Three-field forms: R<score> or C<score>
        [status, old, new] => {
            let (old, new) = (old.to_string(), new.to_string());
            if status.starts_with('R') {
                Some(DiffEntry::Renamed { old, new })
            } else if status.starts_with('C') {
                Some(DiffEntry::Copied(new))
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_line_added() {
        assert_eq!(
            parse_diff_line("A\tsrc/a.ts"),
            Some(DiffEntry::Added("src/a.ts".into()))
        );
    }

    #[test]
    fn parse_diff_line_modified() {
        assert_eq!(
            parse_diff_line("M\tsrc/b.ts"),
            Some(DiffEntry::Modified("src/b.ts".into()))
        );
    }

    #[test]
    fn parse_diff_line_deleted() {
        assert_eq!(
            parse_diff_line("D\tsrc/c.ts"),
            Some(DiffEntry::Deleted("src/c.ts".into()))
        );
    }

    #[test]
    fn parse_diff_line_renamed() {
        assert_eq!(
            parse_diff_line("R100\told.ts\tnew.ts"),
            Some(DiffEntry::Renamed {
                old: "old.ts".into(),
                new: "new.ts".into()
            })
        );
    }

    #[test]
    fn parse_diff_line_renamed_score_not_100() {
        assert_eq!(
            parse_diff_line("R85\tsrc/foo.rs\tsrc/bar.rs"),
            Some(DiffEntry::Renamed {
                old: "src/foo.rs".into(),
                new: "src/bar.rs".into()
            })
        );
    }

    #[test]
    fn parse_diff_line_copied() {
        assert_eq!(
            parse_diff_line("C100\toriginal.ts\tcopy.ts"),
            Some(DiffEntry::Copied("copy.ts".into()))
        );
    }

    #[test]
    fn parse_diff_line_blank_returns_none() {
        assert_eq!(parse_diff_line(""), None);
    }

    #[test]
    fn parse_diff_line_whitespace_only_returns_none() {
        assert_eq!(parse_diff_line("   "), None);
    }

    #[test]
    fn parse_diff_line_unknown_status_returns_none() {
        assert_eq!(parse_diff_line("X\tsomefile.txt"), None);
    }

    #[test]
    fn parse_diff_line_does_not_panic_on_garbage() {
        // Must not panic
        let _ = parse_diff_line("this is not a diff line at all");
    }
}
