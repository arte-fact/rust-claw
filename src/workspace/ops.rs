use std::time::UNIX_EPOCH;

use serde::Serialize;

use super::{FILE_VIEW_MAX_BYTES, Workspace, WorkspaceError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub size: u64,
    pub modified: Option<u64>,
}

/// Lists a directory, classifying entries by their own type (symlinks are not
/// followed), directories first then case-insensitive by name.
pub fn list(ws: &Workspace, rel: &str) -> Result<Vec<Entry>, WorkspaceError> {
    let dir = ws.jail(rel)?;
    if !std::fs::symlink_metadata(&dir)?.is_dir() {
        return Err(WorkspaceError::NotADirectory);
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let link = std::fs::symlink_metadata(entry.path())?;
        let kind = if link.is_symlink() {
            EntryKind::Symlink
        } else if meta.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };
        entries.push(Entry {
            name: entry.file_name().to_string_lossy().into_owned(),
            kind,
            size: meta.len(),
            modified: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
        });
    }
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Dir)
            .cmp(&(b.kind != EntryKind::Dir))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(entries)
}

/// Reads a file as UTF-8 text, refusing binaries and anything over the view limit.
pub fn read_text(ws: &Workspace, rel: &str) -> Result<String, WorkspaceError> {
    let target = ws.jail(rel)?;
    let meta = std::fs::symlink_metadata(&target)?;
    if !meta.is_file() {
        return Err(WorkspaceError::NotAFile);
    }
    if meta.len() > FILE_VIEW_MAX_BYTES {
        return Err(WorkspaceError::TooLarge);
    }
    let bytes = std::fs::read(&target)?;
    if bytes.contains(&0) {
        return Err(WorkspaceError::Binary);
    }
    String::from_utf8(bytes).map_err(|_| WorkspaceError::Binary)
}

/// Reads raw bytes for download — no size cap, no binary check, but still a file.
pub fn read_bytes(ws: &Workspace, rel: &str) -> Result<Vec<u8>, WorkspaceError> {
    let target = ws.jail(rel)?;
    if !std::fs::symlink_metadata(&target)?.is_file() {
        return Err(WorkspaceError::NotAFile);
    }
    Ok(std::fs::read(&target)?)
}

/// Creates or overwrites a text file. Its parent directory must already exist.
pub fn write_text(ws: &Workspace, rel: &str, content: &str) -> Result<(), WorkspaceError> {
    write_bytes(ws, rel, content.as_bytes())
}

/// Creates or overwrites a file with raw bytes (uploads). Parent must already exist.
pub fn write_bytes(ws: &Workspace, rel: &str, bytes: &[u8]) -> Result<(), WorkspaceError> {
    let target = mutable_target(ws, rel)?;
    match target.parent() {
        Some(parent) if parent.is_dir() => {}
        _ => return Err(WorkspaceError::NotFound),
    }
    std::fs::write(&target, bytes)?;
    Ok(())
}

/// Creates a single new directory; its parent must exist and the path must be free.
pub fn mkdir(ws: &Workspace, rel: &str) -> Result<(), WorkspaceError> {
    let target = mutable_target(ws, rel)?;
    if target.exists() {
        return Err(WorkspaceError::Exists);
    }
    std::fs::create_dir(&target)?;
    Ok(())
}

/// Removes a file, or a directory and everything beneath it.
pub fn delete(ws: &Workspace, rel: &str) -> Result<(), WorkspaceError> {
    let target = mutable_target(ws, rel)?;
    let meta = std::fs::symlink_metadata(&target)?;
    if meta.is_dir() {
        std::fs::remove_dir_all(&target)?;
    } else {
        std::fs::remove_file(&target)?;
    }
    Ok(())
}

/// Moves/renames `from` to `to`; the source must exist and the destination must not.
pub fn rename(ws: &Workspace, from: &str, to: &str) -> Result<(), WorkspaceError> {
    let src = mutable_target(ws, from)?;
    let dst = mutable_target(ws, to)?;
    if !src.exists() {
        return Err(WorkspaceError::NotFound);
    }
    if dst.exists() {
        return Err(WorkspaceError::Exists);
    }
    std::fs::rename(&src, &dst)?;
    Ok(())
}

/// Jails `rel` and refuses to let a mutating op target the workspace root itself.
fn mutable_target(ws: &Workspace, rel: &str) -> Result<std::path::PathBuf, WorkspaceError> {
    let target = ws.jail(rel)?;
    if target == ws.root() {
        return Err(WorkspaceError::InvalidName);
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let ws = Workspace::open(tmp.path()).expect("open");
        (tmp, ws)
    }

    #[test]
    fn list_orders_directories_first_then_by_name() {
        let (_tmp, ws) = workspace();
        std::fs::write(ws.root().join("zeta.txt"), "z").expect("write");
        std::fs::write(ws.root().join("Alpha.txt"), "a").expect("write");
        std::fs::create_dir(ws.root().join("src")).expect("mkdir");

        let names: Vec<_> = list(&ws, "")
            .expect("list")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["src", "Alpha.txt", "zeta.txt"]);
    }

    #[test]
    fn list_classifies_kinds_without_following_symlinks() {
        let (_tmp, ws) = workspace();
        std::fs::create_dir(ws.root().join("d")).expect("mkdir");
        std::fs::write(ws.root().join("f"), "x").expect("write");
        std::os::unix::fs::symlink(ws.root().join("d"), ws.root().join("l")).expect("symlink");

        let kinds: Vec<_> = list(&ws, "")
            .expect("list")
            .into_iter()
            .map(|e| (e.name, e.kind))
            .collect();
        assert!(kinds.contains(&("d".to_owned(), EntryKind::Dir)));
        assert!(kinds.contains(&("f".to_owned(), EntryKind::File)));
        assert!(kinds.contains(&("l".to_owned(), EntryKind::Symlink)));
    }

    #[test]
    fn read_text_returns_contents_and_rejects_binary() {
        let (_tmp, ws) = workspace();
        std::fs::write(ws.root().join("ok.txt"), "hello\nworld").expect("write");
        std::fs::write(ws.root().join("bin"), [0u8, 159, 146, 150]).expect("write");

        assert_eq!(read_text(&ws, "ok.txt").expect("text"), "hello\nworld");
        assert!(matches!(read_text(&ws, "bin"), Err(WorkspaceError::Binary)));
    }

    #[test]
    fn read_text_rejects_oversize_files() {
        let (_tmp, ws) = workspace();
        let big = vec![b'a'; (FILE_VIEW_MAX_BYTES + 1) as usize];
        std::fs::write(ws.root().join("big.txt"), &big).expect("write");
        assert!(matches!(
            read_text(&ws, "big.txt"),
            Err(WorkspaceError::TooLarge)
        ));
    }

    #[test]
    fn write_then_read_roundtrips_and_requires_existing_parent() {
        let (_tmp, ws) = workspace();
        write_text(&ws, "note.md", "body").expect("write");
        assert_eq!(read_text(&ws, "note.md").expect("read"), "body");
        assert!(matches!(
            write_text(&ws, "missing/note.md", "x"),
            Err(WorkspaceError::NotFound)
        ));
    }

    #[test]
    fn write_bytes_roundtrips_binary_through_read_bytes() {
        let (_tmp, ws) = workspace();
        let blob = [0u8, 1, 2, 255, 254];
        write_bytes(&ws, "upload.bin", &blob).expect("write");
        assert_eq!(read_bytes(&ws, "upload.bin").expect("read"), blob);
    }

    #[test]
    fn mkdir_creates_and_refuses_existing() {
        let (_tmp, ws) = workspace();
        mkdir(&ws, "sub").expect("mkdir");
        assert!(ws.root().join("sub").is_dir());
        assert!(matches!(mkdir(&ws, "sub"), Err(WorkspaceError::Exists)));
    }

    #[test]
    fn delete_removes_files_and_directory_trees() {
        let (_tmp, ws) = workspace();
        std::fs::create_dir(ws.root().join("tree")).expect("mkdir");
        std::fs::write(ws.root().join("tree/child"), "x").expect("write");
        delete(&ws, "tree").expect("delete");
        assert!(!ws.root().join("tree").exists());
    }

    #[test]
    fn rename_moves_when_destination_is_free() {
        let (_tmp, ws) = workspace();
        std::fs::write(ws.root().join("old.txt"), "x").expect("write");
        rename(&ws, "old.txt", "new.txt").expect("rename");
        assert!(ws.root().join("new.txt").is_file());
        assert!(!ws.root().join("old.txt").exists());
        std::fs::write(ws.root().join("a"), "1").expect("write");
        assert!(matches!(
            rename(&ws, "a", "new.txt"),
            Err(WorkspaceError::Exists)
        ));
    }

    #[test]
    fn mutating_the_root_itself_is_rejected() {
        let (_tmp, ws) = workspace();
        assert!(matches!(delete(&ws, ""), Err(WorkspaceError::InvalidName)));
        assert!(matches!(delete(&ws, "."), Err(WorkspaceError::InvalidName)));
    }
}
