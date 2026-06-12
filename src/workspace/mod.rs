pub mod ops;
pub mod path;

use std::path::{Path, PathBuf};

pub use ops::{Entry, EntryKind};

/// Largest file served to the inline text viewer; bigger files are download-only.
pub const FILE_VIEW_MAX_BYTES: u64 = 1 << 20;

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("path escapes the workspace")]
    Escape,
    #[error("no such file or directory")]
    NotFound,
    #[error("not a directory")]
    NotADirectory,
    #[error("not a file")]
    NotAFile,
    #[error("file is not valid UTF-8 text")]
    Binary,
    #[error("file is larger than the {FILE_VIEW_MAX_BYTES}-byte view limit")]
    TooLarge,
    #[error("the workspace root itself cannot be the target")]
    InvalidName,
    #[error("a file or directory already exists there")]
    Exists,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A canonicalized agent workspace root. Every path handed to `ops` is jailed to
/// stay within this root — unlike the agent's own file tools (`providers::native::
/// files`), which deliberately let absolute paths and `..` through. This is the
/// surface the web file browser drives, so it must not escape one agent's folder.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    /// Opens an existing workspace directory, canonicalizing it once so symlink
    /// checks downstream compare against a real path.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, WorkspaceError> {
        let root = dir.as_ref().canonicalize().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                WorkspaceError::NotFound
            } else {
                WorkspaceError::Io(err)
            }
        })?;
        if !root.is_dir() {
            return Err(WorkspaceError::NotADirectory);
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves `rel` (a workspace-relative path) to an absolute path proven to
    /// stay within the root. The target need not exist.
    pub fn jail(&self, rel: &str) -> Result<PathBuf, WorkspaceError> {
        path::jail(&self.root, rel)
    }
}
