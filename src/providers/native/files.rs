use std::path::{Path, PathBuf};

const DEFAULT_READ_LIMIT: usize = 2_000;

/// Relative paths resolve against the workspace; absolute paths pass through
/// (§11: the workspace scopes by convention — bash can reach anywhere anyway,
/// so pretending otherwise here would only confuse the model).
fn resolve(workspace: &Path, path: &str) -> PathBuf {
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace.join(candidate)
    }
}

/// Line-numbered read with optional 1-based offset and line limit.
pub fn read(workspace: &Path, path: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    let target = resolve(workspace, path);
    let content = match std::fs::read_to_string(&target) {
        Ok(content) => content,
        Err(err) => return format!("error: cannot read {path}: {err}"),
    };
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_READ_LIMIT).max(1);
    let total = content.lines().count();

    let mut out = String::new();
    for (number, line) in content
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .skip(start - 1)
        .take(limit)
    {
        out.push_str(&format!("{number:>6}\t{line}\n"));
    }
    if out.is_empty() {
        return format!("(empty — {path} has {total} lines)");
    }
    let last_shown = (start - 1 + limit).min(total);
    if last_shown < total {
        out.push_str(&format!("[… {} more lines …]", total - last_shown));
    }
    out.trim_end().to_owned()
}

/// Create or overwrite; parent directories are created.
pub fn write(workspace: &Path, path: &str, content: &str) -> String {
    let target = resolve(workspace, path);
    if let Some(parent) = target.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return format!("error: cannot create directories for {path}: {err}");
    }
    match std::fs::write(&target, content) {
        Ok(()) => format!("wrote {} bytes to {path}", content.len()),
        Err(err) => format!("error: cannot write {path}: {err}"),
    }
}

/// Exact-string replacement; the needle must match exactly once.
pub fn edit(workspace: &Path, path: &str, old_string: &str, new_string: &str) -> String {
    if old_string.is_empty() {
        return "error: old_string must not be empty".to_owned();
    }
    let target = resolve(workspace, path);
    let content = match std::fs::read_to_string(&target) {
        Ok(content) => content,
        Err(err) => return format!("error: cannot read {path}: {err}"),
    };
    match content.matches(old_string).count() {
        0 => format!("error: old_string not found in {path}"),
        1 => {
            let updated = content.replacen(old_string, new_string, 1);
            match std::fs::write(&target, updated) {
                Ok(()) => format!("edited {path}"),
                Err(err) => format!("error: cannot write {path}: {err}"),
            }
        }
        count => format!("error: old_string matches {count} times in {path}; it must be unique"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn read_numbers_lines_and_respects_offset_and_limit() {
        let tmp = workspace();
        std::fs::write(tmp.path().join("f.txt"), "alpha\nbeta\ngamma\ndelta\n").expect("write");

        assert_eq!(
            read(tmp.path(), "f.txt", None, None),
            "     1\talpha\n     2\tbeta\n     3\tgamma\n     4\tdelta"
        );
        assert_eq!(
            read(tmp.path(), "f.txt", Some(2), Some(2)),
            "     2\tbeta\n     3\tgamma\n[… 1 more lines …]"
        );
    }

    #[test]
    fn read_missing_file_is_an_error_string() {
        let tmp = workspace();
        assert!(read(tmp.path(), "nope.txt", None, None).starts_with("error:"));
    }

    #[test]
    fn write_creates_parent_directories() {
        let tmp = workspace();
        let result = write(tmp.path(), "deep/nested/file.txt", "content");
        assert_eq!(result, "wrote 7 bytes to deep/nested/file.txt");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("deep/nested/file.txt")).expect("read"),
            "content"
        );
    }

    #[test]
    fn edit_replaces_a_unique_match_only() {
        let tmp = workspace();
        std::fs::write(tmp.path().join("f.txt"), "one two two").expect("write");

        assert_eq!(edit(tmp.path(), "f.txt", "one", "ONE"), "edited f.txt");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("f.txt")).expect("read"),
            "ONE two two"
        );

        let ambiguous = edit(tmp.path(), "f.txt", "two", "2");
        assert!(ambiguous.contains("2 times"), "{ambiguous}");

        let missing = edit(tmp.path(), "f.txt", "absent", "x");
        assert!(missing.contains("not found"), "{missing}");

        let empty = edit(tmp.path(), "f.txt", "", "x");
        assert!(empty.contains("must not be empty"), "{empty}");
    }

    #[test]
    fn absolute_paths_pass_through_and_relative_paths_stay_in_the_workspace() {
        let tmp = workspace();
        let other = workspace();
        std::fs::write(other.path().join("abs.txt"), "absolute").expect("write");

        let absolute = other.path().join("abs.txt");
        assert!(
            read(tmp.path(), absolute.to_str().expect("utf8"), None, None).contains("absolute")
        );

        write(tmp.path(), "rel.txt", "relative");
        assert!(tmp.path().join("rel.txt").is_file());
    }
}
