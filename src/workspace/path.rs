use std::path::{Component, Path, PathBuf};

use super::WorkspaceError;

/// Resolves a workspace-relative path against an already-canonical `root`,
/// rejecting anything that escapes it. Two layers: a lexical pass that collapses
/// `.`/`..` and refuses to climb above the root, then a real-path check that
/// canonicalizes the deepest existing ancestor so an in-workspace symlink pointing
/// outside is caught. The target itself need not exist (callers create files).
pub fn jail(root: &Path, rel: &str) -> Result<PathBuf, WorkspaceError> {
    let candidate = Path::new(rel);
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(WorkspaceError::Escape);
                }
            }
            Component::RootDir | Component::Prefix(_) => return Err(WorkspaceError::Escape),
        }
    }
    let joined = root.join(&normalized);
    verify_within(root, &joined)?;
    Ok(joined)
}

fn verify_within(root: &Path, joined: &Path) -> Result<(), WorkspaceError> {
    let mut ancestor = joined;
    loop {
        match ancestor.canonicalize() {
            Ok(real) => {
                return if real.starts_with(root) {
                    Ok(())
                } else {
                    Err(WorkspaceError::Escape)
                };
            }
            Err(_) => {
                ancestor = ancestor.parent().ok_or(WorkspaceError::Escape)?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn valid_relative_paths_resolve_under_root() {
        let tmp = root();
        let real = tmp.path().canonicalize().expect("canon");
        std::fs::create_dir_all(real.join("a/b")).expect("mkdir");

        for (input, expected_suffix) in [("a", "a"), ("a/b", "a/b"), ("./a/./b", "a/b")] {
            let resolved = jail(&real, input).expect(input);
            assert_eq!(resolved, real.join(expected_suffix), "input={input}");
        }
    }

    #[test]
    fn empty_path_is_the_root_itself() {
        let tmp = root();
        let real = tmp.path().canonicalize().expect("canon");
        assert_eq!(jail(&real, "").expect("empty"), real);
    }

    #[test]
    fn interior_dotdot_that_stays_inside_is_allowed() {
        let tmp = root();
        let real = tmp.path().canonicalize().expect("canon");
        std::fs::create_dir_all(real.join("a")).expect("mkdir");
        assert_eq!(jail(&real, "a/../a").expect("interior"), real.join("a"));
    }

    #[test]
    fn escaping_paths_are_rejected() {
        let tmp = root();
        let real = tmp.path().canonicalize().expect("canon");
        for bad in ["..", "../etc", "a/../../b", "/etc/passwd", "a/../.."] {
            assert!(
                matches!(jail(&real, bad), Err(WorkspaceError::Escape)),
                "bad={bad}"
            );
        }
    }

    #[test]
    fn symlink_pointing_outside_is_rejected() {
        let tmp = root();
        let outside = root();
        let real = tmp.path().canonicalize().expect("canon");
        let outside_real = outside.path().canonicalize().expect("canon");
        std::fs::write(outside_real.join("secret.txt"), "x").expect("write");
        std::os::unix::fs::symlink(&outside_real, real.join("link")).expect("symlink");

        assert!(matches!(jail(&real, "link"), Err(WorkspaceError::Escape)));
        assert!(matches!(
            jail(&real, "link/secret.txt"),
            Err(WorkspaceError::Escape)
        ));
    }
}
