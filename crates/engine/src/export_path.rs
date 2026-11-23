//! Confine `exportPdf` output to `<data_dir>/exports`.
//!
//! The documented contract is a relative path whose canonical form stays
//! under the exports prefix. The path is validated before any directory is
//! created, then walked one component at a time: existing symlinks must resolve
//! inside `exports/`, and every directory this function creates is
//! canonicalised and re-checked immediately. A user-supplied symlink therefore
//! cannot make `create_dir_all` build directories outside the prefix.

use std::path::{Component, Path, PathBuf};

/// Why an export path was rejected before a job paid for a render.
#[derive(Debug, thiserror::Error)]
pub enum ExportPathError {
    /// The requested path breaks the contract for `exports/`.
    #[error("export path {path:?}: {reason}")]
    Invalid {
        /// The path as the client supplied it.
        path: String,
        /// The rule that was violated, for the log line.
        reason: &'static str,
    },
    /// The filesystem refused to create or resolve part of the path.
    #[error("export path {path:?}: {source}")]
    Io {
        /// The path as the client supplied it.
        path: String,
        /// The underlying filesystem error.
        #[source]
        source: std::io::Error,
    },
}

impl ExportPathError {
    /// The `E_*` code: `E_SCHEMA` for a path rule violation, `E_JOB_FAILED`
    /// when the filesystem failed.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => "E_SCHEMA",
            Self::Io { .. } => "E_JOB_FAILED",
        }
    }
}

/// Resolve a relative export path to a file inside `<data_dir>/exports`.
///
/// Intermediate directories are created. The returned path is canonical for
/// its parent, so callers can write without resolving symlinks again.
pub fn resolve(data_dir: &Path, value: &str) -> Result<PathBuf, ExportPathError> {
    let invalid = |reason| ExportPathError::Invalid {
        path: value.to_owned(),
        reason,
    };
    if value.is_empty() {
        return Err(invalid("path must not be empty"));
    }
    if value.ends_with('/') {
        return Err(invalid("path must name a file"));
    }
    let request = Path::new(value);
    if request.is_absolute() {
        return Err(invalid("path must be relative"));
    }
    let mut relative = PathBuf::new();
    for component in request.components() {
        match component {
            Component::Normal(name) => relative.push(name),
            Component::CurDir => return Err(invalid("path must not contain .")),
            Component::ParentDir => return Err(invalid("path must not contain ..")),
            Component::RootDir | Component::Prefix(_) => {
                return Err(invalid("path must be relative"));
            }
        }
    }
    let Some(file_name) = relative.file_name() else {
        return Err(invalid("path must name a file"));
    };

    let exports = data_dir.join("exports");
    std::fs::create_dir_all(&exports).map_err(|source| ExportPathError::Io {
        path: value.to_owned(),
        source,
    })?;
    let canonical_data = std::fs::canonicalize(data_dir).map_err(|source| ExportPathError::Io {
        path: value.to_owned(),
        source,
    })?;
    let canonical_exports =
        std::fs::canonicalize(&exports).map_err(|source| ExportPathError::Io {
            path: value.to_owned(),
            source,
        })?;
    // A symlinked `exports/` would move the whole prefix elsewhere; the contract
    // names `<data_dir>/exports`, so it must be the real directory.
    if canonical_exports != canonical_data.join("exports") {
        return Err(invalid("exports/ is not a directory under the data dir"));
    }

    let mut canonical_parent = canonical_exports.clone();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            let next = canonical_parent.join(component);
            let existing = match std::fs::symlink_metadata(&next) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let resolved =
                        std::fs::canonicalize(&next).map_err(|source| ExportPathError::Io {
                            path: value.to_owned(),
                            source,
                        })?;
                    if !resolved.starts_with(&canonical_exports) {
                        return Err(invalid("path resolves outside exports/"));
                    }
                    if !resolved.is_dir() {
                        return Err(invalid("path component is not a directory"));
                    }
                    Some(resolved)
                }
                Ok(metadata) if metadata.is_dir() => Some(next.clone()),
                Ok(_) => return Err(invalid("path component is not a directory")),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(source) => {
                    return Err(ExportPathError::Io {
                        path: value.to_owned(),
                        source,
                    });
                }
            };
            canonical_parent = match existing {
                Some(path) => path,
                None => {
                    // Create exactly one level: a race that swaps the parent for
                    // a symlink makes `create_dir` fail or the re-check below
                    // reject the result.
                    match std::fs::create_dir(&next) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            let metadata = std::fs::symlink_metadata(&next).map_err(|source| {
                                ExportPathError::Io {
                                    path: value.to_owned(),
                                    source,
                                }
                            })?;
                            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                                return Err(invalid("path component is not a directory"));
                            }
                        }
                        Err(source) => {
                            return Err(ExportPathError::Io {
                                path: value.to_owned(),
                                source,
                            });
                        }
                    }
                    let created =
                        std::fs::canonicalize(&next).map_err(|source| ExportPathError::Io {
                            path: value.to_owned(),
                            source,
                        })?;
                    if !created.starts_with(&canonical_exports) {
                        return Err(invalid("path resolves outside exports/"));
                    }
                    created
                }
            };
        }
    }

    let target = canonical_parent.join(file_name);
    if let Ok(metadata) = std::fs::symlink_metadata(&target)
        && metadata.file_type().is_symlink()
    {
        let canonical_target = std::fs::canonicalize(&target)
            .map_err(|_| invalid("path resolves outside exports/"))?;
        if !canonical_target.starts_with(&canonical_exports) {
            return Err(invalid("path resolves outside exports/"));
        }
    }
    Ok(target)
}

/// Write the PDF atomically: a temporary sibling is renamed over the target.
///
/// The target is checked for a symlink immediately before the rename; on Unix
/// the rename replaces that symlink instead of following it, so a race cannot
/// redirect the bytes outside `exports/`.
pub(crate) fn write_atomic(target: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write as _;

    let Some(parent) = target.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "export target has no parent",
        ));
    };
    if let Ok(metadata) = std::fs::symlink_metadata(target)
        && metadata.file_type().is_symlink()
    {
        std::fs::remove_file(target)?;
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    match temp.persist(target) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Windows cannot replace an existing file with rename. Removing the
            // target here is still safe: `resolve` and the symlink check above
            // have confined it to exports/.
            std::fs::remove_file(target)?;
            error
                .file
                .persist(target)
                .map(|_| ())
                .map_err(|error| error.error)
        }
        Err(error) => Err(error.error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn data_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn resolves_relative_path_inside_exports() {
        let dir = data_dir();
        let target = resolve(dir.path(), "quotes/2026-11.pdf").unwrap();
        assert!(target.starts_with(fs::canonicalize(dir.path().join("exports")).unwrap()));
        assert_eq!(target.file_name().unwrap(), "2026-11.pdf");
    }

    #[test]
    fn rejects_traversal_and_absolute_paths() {
        let dir = data_dir();
        for value in [
            "../escape.pdf",
            "a/../../escape.pdf",
            "/tmp/escape.pdf",
            "./a.pdf",
        ] {
            let error = resolve(dir.path(), value).unwrap_err();
            assert!(matches!(error, ExportPathError::Invalid { .. }), "{value}");
        }
    }

    #[test]
    fn rejects_directories_and_empty_paths() {
        let dir = data_dir();
        for value in ["", "folder/", ".."] {
            let error = resolve(dir.path(), value).unwrap_err();
            assert!(matches!(error, ExportPathError::Invalid { .. }), "{value}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_parent_symlink_without_creating_directories_outside() {
        let dir = data_dir();
        let outside = tempfile::tempdir().unwrap();
        let exports = dir.path().join("exports");
        fs::create_dir_all(&exports).unwrap();
        std::os::unix::fs::symlink(outside.path(), exports.join("link")).unwrap();
        let error = resolve(dir.path(), "link/new/nested/escape.pdf").unwrap_err();
        assert!(matches!(error, ExportPathError::Invalid { .. }), "{error}");
        assert!(
            !outside.path().join("new").exists(),
            "resolve created directories outside exports/"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_an_exports_symlink_that_leaves_the_data_dir() {
        let dir = data_dir();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("exports")).unwrap();
        let error = resolve(dir.path(), "file.pdf").unwrap_err();
        assert!(matches!(error, ExportPathError::Invalid { .. }), "{error}");
        assert!(!outside.path().join("file.pdf").exists());
    }

    #[test]
    fn creates_nested_directories_inside_exports() {
        let dir = data_dir();
        let target = resolve(dir.path(), "a/b/report.pdf").unwrap();
        assert_eq!(target.file_name().unwrap(), "report.pdf");
        assert!(target.parent().unwrap().is_dir());
        assert!(target.starts_with(fs::canonicalize(dir.path().join("exports")).unwrap()));
    }

    #[test]
    fn rejects_a_regular_file_in_the_parent_path() {
        let dir = data_dir();
        let exports = dir.path().join("exports");
        fs::create_dir_all(&exports).unwrap();
        fs::write(exports.join("blocker"), b"file").unwrap();
        let error = resolve(dir.path(), "blocker/report.pdf").unwrap_err();
        assert!(matches!(error, ExportPathError::Invalid { .. }), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_target_symlink_that_leaves_exports() {
        let dir = data_dir();
        let outside = tempfile::tempdir().unwrap();
        let exports = dir.path().join("exports");
        fs::create_dir_all(&exports).unwrap();
        std::os::unix::fs::symlink(outside.path().join("escape.pdf"), exports.join("out.pdf"))
            .unwrap();
        let error = resolve(dir.path(), "out.pdf").unwrap_err();
        assert!(matches!(error, ExportPathError::Invalid { .. }));
    }
    #[cfg(unix)]
    #[test]
    fn atomic_write_replaces_a_symlink_instead_of_following_it() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let exports = dir.path().join("exports");
        std::fs::create_dir_all(&exports).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, b"do not touch").unwrap();
        let target = exports.join("quote.pdf");
        symlink(&outside, &target).unwrap();
        super::write_atomic(&target, b"%PDF-1.7").unwrap();
        assert_eq!(std::fs::read(&outside).unwrap(), b"do not touch");
        assert_eq!(std::fs::read(&target).unwrap(), b"%PDF-1.7");
    }
}
