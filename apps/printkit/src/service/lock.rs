//! The single-instance lock and the administrator token file.
//!
//! Both live in the data directory and are the only proof that a service run
//! owns the directory: `serve.lock` carries the pid and port, `admin-token`
//! carries the secret that `/admin` accepts.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

pub const LOCK_FILE: &str = "serve.lock";
/// The invisible inode whose OS lock proves ownership; its content is never
/// read, so a crash cannot leave a half-written proof of ownership.
pub const LOCK_INODE_FILE: &str = "serve.lock.lock";
pub const ADMIN_TOKEN_FILE: &str = "admin-token";
pub const SESSION_COOKIE: &str = "pk_admin";
pub const CSRF_COOKIE: &str = "pk_csrf";
pub const CSRF_HEADER: &str = "x-csrf-token";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another printkit serve owns {path} (pid {pid}, port {port})")]
    Running { path: String, pid: u32, port: u16 },
    /// The OS lock is held, but the diagnostic JSON is empty or malformed.
    #[error("another printkit serve owns {path}")]
    Held { path: String },
    #[error("cannot inspect {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
    port: u16,
    /// Random per-run id, so a reused pid cannot impersonate an old lock.
    #[serde(default)]
    instance: String,
    /// Unix start time in seconds; diagnostics only.
    #[serde(default)]
    started: String,
    #[serde(default)]
    executable: String,
}

/// Exclusive ownership of one data directory for one `serve` process.
///
/// The OS holds an advisory exclusive lock on `serve.lock.lock` for the whole
/// lifetime of [`ServeLock`]. `serve.lock` is diagnostics only and is replaced
/// atomically, so a reader never acts on a half-written pid/port body.
#[derive(Debug)]
pub struct ServeLock {
    diagnostics: PathBuf,
    pid: u32,
    _file: File,
}

impl ServeLock {
    pub fn acquire(data_dir: &Path, port: u16) -> Result<Self, LockError> {
        fs::create_dir_all(data_dir).map_err(|source| LockError::Io {
            path: data_dir.display().to_string(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
                LockError::Io {
                    path: data_dir.display().to_string(),
                    source,
                }
            })?;
        }
        let inode = data_dir.join(LOCK_INODE_FILE);
        let file = open_lock(&inode).map_err(|source| LockError::Io {
            path: inode.display().to_string(),
            source,
        })?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                let diagnostics = data_dir.join(LOCK_FILE);
                write_info(&diagnostics, port).map_err(|source| LockError::Io {
                    path: diagnostics.display().to_string(),
                    source,
                })?;
                Ok(Self {
                    diagnostics,
                    pid: std::process::id(),
                    _file: file,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let diagnostics = data_dir.join(LOCK_FILE);
                let existing = fs::read_to_string(&diagnostics).unwrap_or_default();
                Err(match serde_json::from_str::<LockInfo>(&existing) {
                    Ok(info) => LockError::Running {
                        path: diagnostics.display().to_string(),
                        pid: info.pid,
                        port: info.port,
                    },
                    Err(_) => LockError::Held {
                        path: diagnostics.display().to_string(),
                    },
                })
            }
            Err(source) => Err(LockError::Io {
                path: inode.display().to_string(),
                source,
            }),
        }
    }
}

impl Drop for ServeLock {
    fn drop(&mut self) {
        // We still hold the OS lock while replacing the diagnostics, so no
        // second owner can be active between the read and the unlink.
        let current = fs::read_to_string(&self.diagnostics).unwrap_or_default();
        if let Ok(info) = serde_json::from_str::<LockInfo>(&current)
            && info.pid == self.pid
        {
            let _ = fs::remove_file(&self.diagnostics);
        }
        // Closing `self._file` releases the OS lock.
    }
}

fn open_lock(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Replace `serve.lock` with a complete JSON body: write a sibling temp file,
/// fsync it, then rename it over the diagnostic path.
fn write_info(diagnostics: &Path, port: u16) -> std::io::Result<()> {
    let info = LockInfo {
        pid: std::process::id(),
        port,
        instance: random_secret().unwrap_or_else(|_| "unknown".to_owned()),
        started: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs().to_string())
            .unwrap_or_default(),
        executable: std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    };
    let text = serde_json::to_string(&info).map_err(std::io::Error::other)?;
    let temporary = diagnostics.with_extension("tmp");
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&temporary, diagnostics)
}
/// Make the token readable by its owner only.
#[cfg(unix)]
fn restrict_token_file(path: &Path) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Windows counterpart of `chmod 0600`: a protected DACL for the current user.
#[cfg(windows)]
fn restrict_token_file(path: &Path) -> std::io::Result<()> {
    printkit_platform_win::restrict_to_current_user(path)
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("cannot access {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read system randomness: {0}")]
    Random(#[from] getrandom::Error),
    #[error("{path} is empty; delete it and restart to generate a new token")]
    EmptyToken { path: String },
}

/// The administrator token from `<data_dir>/admin-token`, created on first
/// start readable by the owner only (mode 0600 on Unix, a user-only DACL on
/// Windows).
#[derive(Debug)]
pub struct AdminToken {
    value: String,
}

impl AdminToken {
    pub fn load_or_create(data_dir: &Path) -> Result<Self, SecretError> {
        fs::create_dir_all(data_dir).map_err(|source| SecretError::Io {
            path: data_dir.display().to_string(),
            source,
        })?;
        let path = data_dir.join(ADMIN_TOKEN_FILE);
        let value = match fs::read_to_string(&path) {
            Ok(text) => text.trim().to_owned(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let value = random_secret()?;
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(0o600);
                let mut file = options.open(&path).map_err(|source| SecretError::Io {
                    path: path.display().to_string(),
                    source,
                })?;
                file.write_all(value.as_bytes())
                    .and_then(|()| file.write_all(b"\n"))
                    .and_then(|()| file.sync_all())
                    .map_err(|source| SecretError::Io {
                        path: path.display().to_string(),
                        source,
                    })?;
                value
            }
            Err(source) => {
                return Err(SecretError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        if value.is_empty() {
            return Err(SecretError::EmptyToken {
                path: path.display().to_string(),
            });
        }
        restrict_token_file(&path).map_err(|source| SecretError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Ok(Self { value })
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

/// 32 bytes from the operating system, URL-safe base64 without padding.
pub fn random_secret() -> Result<String, SecretError> {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// A printable token handed to a business page exactly once.
pub fn new_access_token() -> Result<String, SecretError> {
    Ok(format!("pk_{}", random_secret()?))
}

/// The stored form of a token. Plaintext never reaches SQLite.
pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Compare two secrets without an early exit on the first differing byte.
pub fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn admin_token_file_is_created_private_and_stable() {
        let temp = tempfile::tempdir().unwrap();
        let first = AdminToken::load_or_create(temp.path()).unwrap();
        let mode = fs::metadata(temp.path().join(ADMIN_TOKEN_FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let second = AdminToken::load_or_create(temp.path()).unwrap();
        assert_eq!(first.value(), second.value());
    }

    #[cfg(unix)]
    #[test]
    fn loose_permissions_are_tightened_on_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(ADMIN_TOKEN_FILE);
        fs::write(&path, "kept\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let token = AdminToken::load_or_create(temp.path()).unwrap();
        assert_eq!(token.value(), "kept");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn hashes_are_hex_and_comparisons_are_length_safe() {
        let hash = hash_token("pk_value");
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(constant_time_eq("same", "same"));
        assert!(!constant_time_eq("same", "samf"));
        assert!(!constant_time_eq("short", "longer"));
    }

    #[test]
    fn a_live_pid_keeps_the_lock() {
        let temp = tempfile::tempdir().unwrap();
        let first = ServeLock::acquire(temp.path(), 19080).unwrap();
        let error = ServeLock::acquire(temp.path(), 19081).unwrap_err();
        assert!(matches!(error, LockError::Running { .. }));
        drop(first);
        assert!(!temp.path().join(LOCK_FILE).exists());
    }

    #[test]
    fn a_dead_pid_is_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(LOCK_FILE);
        fs::write(&path, r#"{"pid":999999,"port":19080}"#).unwrap();
        let lock = ServeLock::acquire(temp.path(), 19081).unwrap();
        let info: LockInfo = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(info.pid, std::process::id());
        drop(lock);
    }
}
