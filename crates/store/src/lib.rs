#![cfg_attr(test, allow(clippy::panic, clippy::unwrap_used))]
#![deny(missing_docs)]

//! SQLite persistence for run-time settings and service authorisations.
//!
//! The service is the only writer. CLI reads are opened read-only so `config`
//! can explain a `db` source while `serve` holds the database.
//!
//! Tokens are never stored in plaintext: the `auth` table keeps a SHA-256
//! digest, a bound origin and the granted scopes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Database file below the data directory.
pub const DB_FILE: &str = "printkit.db";

/// Setting key holding the JSON array of whitelisted browser origins.
pub const ORIGINS_KEY: &str = "origins";

/// Applied in order; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/V1__init.sql"),
    include_str!("../migrations/V2__print_record.sql"),
    include_str!("../migrations/V3__calibration.sql"),
    include_str!("../migrations/V4__template.sql"),
    include_str!("../migrations/V5__token_revocation.sql"),
    include_str!("../migrations/V6__template_origin.sql"),
    include_str!("../migrations/V7__calibration_origin.sql"),
    include_str!("../migrations/V8__drop_template.sql"),
    include_str!("../migrations/V9__drop_calibration.sql"),
    include_str!("../migrations/V10__print_record_indexes.sql"),
    include_str!("../migrations/V11__print_record_origin_status_id.sql"),
];

/// Every failure this crate returns.
///
/// The variant names the stage that failed: filesystem setup in
/// [`Store::open`], a SQLite statement inside a query method, or the value
/// conversion performed on top of a row.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The data directory could not be created; raised by [`Store::open`]
    /// before it touches the database file.
    #[error("cannot create {path}: {source}")]
    CreateDir {
        /// Data directory passed to the failed create.
        path: String,
        /// Filesystem error behind the failure.
        #[source]
        source: std::io::Error,
    },
    /// SQLite refused to open the database file (missing parent, permissions,
    /// not a database); raised by [`Store::open`], [`Store::open_existing`]
    /// and [`Store::read_settings`].
    #[error("cannot open {path}: {source}")]
    Open {
        /// Database file path SQLite tried to open.
        path: String,
        /// SQLite error behind the failure.
        #[source]
        source: rusqlite::Error,
    },
    /// The automatic pre-V8 or pre-V9 backup could not be written; migration
    /// aborts and the original database is left untouched.
    #[error("cannot back up {path} to {backup}: {source}")]
    Backup {
        /// Source database path.
        path: String,
        /// Backup destination.
        backup: String,
        /// SQLite backup error.
        #[source]
        source: rusqlite::Error,
    },
    /// A unique temporary file for a pre-V8/pre-V9 backup could not be
    /// reserved or synced; the migration aborts and the original database is
    /// left untouched.
    #[error("cannot stage a temporary backup beside {path}: {source}")]
    BackupTemp {
        /// Database path the backup belongs to.
        path: String,
        /// Filesystem error behind the failure.
        #[source]
        source: std::io::Error,
    },
    /// A pre-V8/pre-V9 backup was written but could not be moved into place;
    /// the migration aborts and the original database is left untouched.
    #[error("cannot move the backup {backup} into place: {source}")]
    BackupMove {
        /// Backup destination that could not be created.
        backup: String,
        /// Filesystem error behind the failure.
        #[source]
        source: std::io::Error,
    },
    /// Unix only: chmod of the `0700` data directory or of the `0600`
    /// database, `-wal` and `-shm` files failed inside [`Store::open`].
    #[error("cannot restrict {path}: {source}")]
    Restrict {
        /// File or directory whose permissions were not changed.
        path: String,
        /// Filesystem error behind the failure.
        #[source]
        source: std::io::Error,
    },
    /// A statement failed for any other reason: migration, query, or a write
    /// whose busy retries were exhausted. Any method that locks the
    /// connection can return this.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A stored value is not the JSON the schema expects: the `origins` array
    /// in [`Store::origins`] / [`Store::add_origin`], or the tag array in
    /// [`Store::insert_print_record`]. `key` names the offending setting or
    /// column.
    #[error("setting {key} holds invalid JSON: {source}")]
    SettingJson {
        /// Setting name or pseudo-column (`print_record.tags`) holding the bad
        /// JSON.
        key: &'static str,
        /// serde error describing where the JSON diverged from the schema.
        #[source]
        source: serde_json::Error,
    },
    /// `PRAGMA user_version` is higher than the last migration this build
    /// knows, so an older binary met a newer database; raised by
    /// [`Store::open`] instead of downgrading the schema.
    #[error("database schema version {version} is newer than this build")]
    SchemaTooNew {
        /// `PRAGMA user_version` read from the file.
        version: i64,
    },
    /// Formatting a timestamp as RFC 3339 failed; raised by methods that stamp
    /// `created_at`, `updated_at` or `last_used_at`, and by
    /// [`Store::cleanup_print_records`] when it computes the cutoff.
    #[error("cannot format a timestamp: {0}")]
    Time(#[from] time::error::Format),
    /// A panic poisoned the connection mutex earlier, so this call could not
    /// lock the connection.
    #[error("store lock poisoned")]
    Poisoned,
}

/// One authorisation row. `scope` is a comma-separated list of scope names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthToken {
    /// Row id in `auth`; pass it to [`Store::revoke_token`] to revoke the
    /// token.
    pub id: i64,
    /// Origin the token is bound to; a browser handshake must present this
    /// same `Origin` header.
    pub origin: String,
    /// Comma-separated scope names as stored, e.g. `print` or `print,raw`.
    pub scope: String,
    /// `true` for script tokens: they are rejected when a browser `Origin`
    /// header is present.
    pub non_browser: bool,
}

/// The process-wide database handle.
#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    connection: Mutex<Connection>,
}

/// Open an existing database read-only, recovering a WAL database whose side
/// files were removed.
///
/// A WAL database without `-shm`/`-wal` opens read-only but fails its first
/// read with "unable to open database file". The recovery handle is opened
/// read-write so SQLite can recreate the side files, then `PRAGMA query_only`
/// keeps it read-only: write statements fail and no business row can change.
fn open_read_only_with_wal_recovery(db_path: &Path) -> Result<Connection, StoreError> {
    let read_only = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let read_write = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = match Connection::open_with_flags(db_path, read_only) {
        Ok(connection)
            if connection.pragma_update(None, "query_only", true).is_ok()
                && connection
                    .query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
                    .is_ok() =>
        {
            connection
        }
        _ => {
            let connection =
                Connection::open_with_flags(db_path, read_write).map_err(|source| {
                    StoreError::Open {
                        path: db_path.display().to_string(),
                        source,
                    }
                })?;
            connection.pragma_update(None, "query_only", true)?;
            connection
        }
    };
    Ok(connection)
}

/// Back up a pre-V8 database before the template table is dropped.
///
/// A new database (`user_version = 0`) and an already migrated database
/// (`>= 8`) are left alone. An existing usable backup is never overwritten; a
/// zero-length or unreadable leftover is replaced by a fresh copy.
fn backup_before_template_drop(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    backup_before_template_drop_with(connection, path, |from, to| fs::rename(from, to))
}

/// The pre-V8 backup body, with the final move injected so tests can force a
/// rename failure without racing the filesystem.
fn backup_before_template_drop_with<R>(
    connection: &Connection,
    path: &Path,
    rename: R,
) -> Result<(), StoreError>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if !(1..8).contains(&version) {
        return Ok(());
    }
    write_backup_atomically(connection, path, "v8", rename)
}

/// Reserve a unique, empty file for a `bak-before-<label>` backup.
///
/// The process id, a nanosecond timestamp and an attempt counter keep the
/// name unique even when two processes open the same data directory at once;
/// `create_new` makes the reservation race-free. The caller removes the file
/// on every failure path.
fn reserve_backup_temporary(path: &Path, label: &str) -> std::io::Result<PathBuf> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    for attempt in 0..8 {
        let candidate = path.with_file_name(format!(
            "{DB_FILE}.bak-before-{label}.{}.{stamp}.{attempt}.tmp",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => {
                drop(file);
                return Ok(candidate);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "cannot reserve a unique temporary backup name",
    ))
}

/// Flush a finished backup to disk before it is renamed into place.
fn sync_backup(path: &Path) -> std::io::Result<()> {
    fs::OpenOptions::new().write(true).open(path)?.sync_all()
}

/// A final backup counts as present only when it is a non-empty SQLite
/// database whose header is intact.
///
/// A crash can leave a zero-length or corrupt file under the final name.
/// Treating that as "already backed up" would keep an unusable file forever,
/// so such a leftover is replaced by a fresh copy instead.
///
/// The copy inherits the header journal flag of the live database, which is
/// WAL: such a file has no `-wal`/`-shm` side files and cannot be read through
/// a read-only handle, so the header is the strongest check available without
/// leaving side files behind. Rollback-journal files are additionally opened
/// read-only and asked for `schema_version`.
fn backup_is_usable(backup: &Path) -> bool {
    use std::io::Read as _;
    let Ok(metadata) = fs::metadata(backup) else {
        return false;
    };
    let Ok(mut file) = fs::File::open(backup) else {
        return false;
    };
    let mut header = [0u8; 100];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    if &header[..16] != b"SQLite format 3\0" {
        return false;
    }
    // A crash during a direct copy leaves the header claiming the full page
    // count while the file is short, so reject anything smaller than its own
    // header says.
    let page_size = match u16::from_be_bytes([header[16], header[17]]) {
        1 => 65_536,
        size => u64::from(size),
    };
    let page_count = u64::from(u32::from_be_bytes([
        header[28], header[29], header[30], header[31],
    ]));
    if page_size < 512 || page_count == 0 || metadata.len() < page_size * page_count {
        return false;
    }
    if header[18] == 2 || header[19] == 2 {
        return true;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(backup, flags) {
        Ok(connection) => connection
            .query_row("PRAGMA schema_version", [], |row| row.get::<_, i64>(0))
            .is_ok(),
        Err(_) => false,
    }
}

/// Write `<data_dir>/printkit.db.bak-before-<label>` without ever exposing a
/// half-written backup.
///
/// The copy goes to a uniquely reserved sibling temporary file (`create_new`),
/// is fsynced, restricted to `0600`, then renamed into place. An existing
/// usable backup is kept as is; a zero-length or unreadable leftover is
/// replaced atomically. Every failure removes the temporary and aborts the
/// migration before the dropping migration runs.
fn write_backup_atomically<R>(
    connection: &Connection,
    path: &Path,
    label: &str,
    rename: R,
) -> Result<(), StoreError>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let backup = path.with_file_name(format!("{DB_FILE}.bak-before-{label}"));
    if backup_is_usable(&backup) {
        return Ok(());
    }
    let temporary = match reserve_backup_temporary(path, label) {
        Ok(temporary) => temporary,
        Err(source) => {
            return Err(StoreError::BackupTemp {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if let Err(source) =
        connection.backup("main", &temporary, None::<fn(rusqlite::backup::Progress)>)
    {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::Backup {
            path: path.display().to_string(),
            backup: backup.display().to_string(),
            source,
        });
    }
    if let Err(source) = sync_backup(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::BackupTemp {
            path: path.display().to_string(),
            source,
        });
    }
    #[cfg(unix)]
    restrict_path(&temporary);
    // The leftover under the final name was rejected as unusable above.
    // Windows `rename` refuses to replace an existing file, so clear it first;
    // on POSIX the rename itself replaces it atomically.
    #[cfg(windows)]
    if backup.exists()
        && let Err(source) = fs::remove_file(&backup)
    {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::BackupMove {
            backup: backup.display().to_string(),
            source,
        });
    }
    if let Err(source) = rename(&temporary, &backup) {
        let _ = fs::remove_file(&temporary);
        return Err(StoreError::BackupMove {
            backup: backup.display().to_string(),
            source,
        });
    }
    Ok(())
}

/// Back up a database that still holds calibration rows before V9 drops the
/// table.
///
/// The backup is written once per data directory as
/// `<data_dir>/printkit.db.bak-before-v9`: a new database, an already migrated
/// database and a database whose `calibration` table is empty are left alone.
/// The copy is written to a uniquely reserved sibling temporary file and
/// renamed into place, so readers never see a half-written backup. An existing
/// usable backup is kept; a zero-length or unreadable leftover is replaced. A
/// failure aborts the migration before V9 runs and keeps the original file.
fn backup_before_calibration_drop(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    backup_before_calibration_drop_with(connection, path, |from, to| fs::rename(from, to))
}

/// The backup body, with the final move injected so tests can force a rename
/// failure without racing the filesystem.
fn backup_before_calibration_drop_with<R>(
    connection: &Connection,
    path: &Path,
    rename: R,
) -> Result<(), StoreError>
where
    R: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= 9 {
        return Ok(());
    }
    let table: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'calibration'",
        [],
        |row| row.get(0),
    )?;
    if table == 0 {
        return Ok(());
    }
    let rows: i64 =
        connection.query_row("SELECT count(*) FROM calibration", [], |row| row.get(0))?;
    if rows == 0 {
        return Ok(());
    }
    write_backup_atomically(connection, path, "v9", rename)
}

impl Store {
    /// Open (and migrate) the database in `data_dir`.
    pub fn open(data_dir: &Path) -> Result<Self, StoreError> {
        fs::create_dir_all(data_dir).map_err(|source| StoreError::CreateDir {
            path: data_dir.display().to_string(),
            source,
        })?;
        #[cfg(unix)]
        restrict_data_dir(data_dir)?;
        let path = data_dir.join(DB_FILE);
        let existed = path.is_file();
        let mut connection = Connection::open(&path).map_err(|source| StoreError::Open {
            path: path.display().to_string(),
            source,
        })?;
        configure(&connection)?;
        if existed {
            backup_before_template_drop(&connection, &path)?;
            backup_before_calibration_drop(&connection, &path)?;
        }
        migrate(&mut connection)?;
        #[cfg(unix)]
        restrict_database_files(&path)?;
        Ok(Self {
            path,
            connection: Mutex::new(connection),
        })
    }

    /// Open an existing database without writing anything.
    ///
    /// Returns `None` when the file does not exist yet, which is the normal
    /// state before the first `serve` run.
    ///
    /// The map is the whole `setting` table: setting key -> stored text value,
    /// ordered by key. Values are not parsed here, so JSON settings such as
    /// `origins` arrive as their raw string.
    pub fn read_settings(db_path: &Path) -> Result<Option<BTreeMap<String, String>>, StoreError> {
        if !db_path.is_file() {
            return Ok(None);
        }
        let connection = open_read_only_with_wal_recovery(db_path)?;
        connection.busy_timeout(Duration::from_millis(5_000))?;
        let mut statement = connection.prepare("SELECT key, value FROM setting")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut settings = BTreeMap::new();
        for row in rows {
            let (key, value) = row?;
            settings.insert(key, value);
        }
        Ok(Some(settings))
    }

    /// Filesystem path of the database file, `<data_dir>/printkit.db`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::Poisoned)
    }

    /// Value of one `setting` row, or `None` when the key was never written.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT value FROM setting WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Insert or replace one `setting` row; the value is stored verbatim as
    /// text, so callers keep the JSON encoding.
    pub fn set_setting(&self, key: &str, value: &str) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Whitelisted browser origins, sorted and de-duplicated.
    pub fn origins(&self) -> Result<Vec<String>, StoreError> {
        let Some(raw) = self.get_setting(ORIGINS_KEY)? else {
            return Ok(Vec::new());
        };
        if raw.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut origins: Vec<String> =
            serde_json::from_str(&raw).map_err(|source| StoreError::SettingJson {
                key: ORIGINS_KEY,
                source,
            })?;
        origins.sort();
        origins.dedup();
        Ok(origins)
    }

    /// Add an origin to the whitelist and return the updated list.
    pub fn add_origin(&self, origin: &str) -> Result<Vec<String>, StoreError> {
        self.mutate_origins(|origins| {
            if !origins.iter().any(|existing| existing == origin) {
                origins.push(origin.to_owned());
            }
        })
    }

    /// Remove an origin from the whitelist and return the updated list.
    ///
    /// Removing an origin that is not listed is not an error; the caller gets
    /// the unchanged list.
    pub fn remove_origin(&self, origin: &str) -> Result<Vec<String>, StoreError> {
        self.mutate_origins(|origins| origins.retain(|existing| existing != origin))
    }

    /// Read-modify-write under one connection lock, so two administrators
    /// cannot drop each other's update.
    fn mutate_origins<F>(&self, change: F) -> Result<Vec<String>, StoreError>
    where
        F: FnOnce(&mut Vec<String>),
    {
        let connection = self.connection()?;
        let raw: Option<String> = connection
            .query_row(
                "SELECT value FROM setting WHERE key = ?1",
                params![ORIGINS_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let mut origins: Vec<String> = match raw {
            Some(raw) if !raw.trim().is_empty() => {
                serde_json::from_str(&raw).map_err(|source| StoreError::SettingJson {
                    key: ORIGINS_KEY,
                    source,
                })?
            }
            _ => Vec::new(),
        };
        change(&mut origins);
        origins.sort();
        origins.dedup();
        let raw = serde_json::to_string(&origins).map_err(|source| StoreError::SettingJson {
            key: ORIGINS_KEY,
            source,
        })?;
        connection.execute(
            "INSERT INTO setting (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ORIGINS_KEY, raw],
        )?;
        Ok(origins)
    }

    /// Store a new authorisation and return its row id.
    ///
    /// `token_sha256` is the hex SHA-256 digest of the token — plaintext is
    /// never written. `origin` is the bound origin, `scope` a comma-separated
    /// scope list, and `non_browser` marks a script token. One `INSERT` writes
    /// all of these plus `created_at`.
    pub fn insert_token(
        &self,
        origin: &str,
        token_sha256: &str,
        scope: &str,
        non_browser: bool,
    ) -> Result<i64, StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO auth (origin, token_sha256, scope, non_browser, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![origin, token_sha256, scope, i64::from(non_browser), now()?],
        )?;
        Ok(connection.last_insert_rowid())
    }

    /// The live authorisation for a token digest, or `None` when the digest is
    /// unknown or the row was revoked.
    pub fn token_by_hash(&self, token_sha256: &str) -> Result<Option<AuthToken>, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, origin, scope, non_browser FROM auth \
                 WHERE token_sha256 = ?1 AND revoked_at IS NULL",
                params![token_sha256],
                |row| {
                    Ok(AuthToken {
                        id: row.get(0)?,
                        origin: row.get(1)?,
                        scope: row.get(2)?,
                        non_browser: row.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    /// Stamp `last_used_at` on one token so the admin list shows recent
    /// activity; unknown ids are ignored.
    pub fn touch_token(&self, id: i64) -> Result<(), StoreError> {
        let connection = self.connection()?;
        connection.execute(
            "UPDATE auth SET last_used_at = ?1 WHERE id = ?2",
            params![now()?, id],
        )?;
        Ok(())
    }

    /// Revoke one token by its row id. Returns `false` when the id is unknown
    /// or already revoked.
    pub fn revoke_token(&self, id: i64) -> Result<bool, StoreError> {
        let connection = self.connection()?;
        let updated = connection.execute(
            "UPDATE auth SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL",
            params![now()?, id],
        )?;
        Ok(updated > 0)
    }

    /// Revoke every token issued for one origin. Returns the number revoked.
    pub fn revoke_origin(&self, origin: &str) -> Result<u64, StoreError> {
        let connection = self.connection()?;
        let updated = connection.execute(
            "UPDATE auth SET revoked_at = ?1 WHERE origin = ?2 AND revoked_at IS NULL",
            params![now()?, origin],
        )?;
        Ok(u64::try_from(updated).unwrap_or(u64::MAX))
    }
}

/// One stored usage row, decoded from `print_record`.
pub struct PrintRecord {
    /// SQLite rowid; listing is ordered by it and a page cursor is the last id.
    pub id: i64,
    /// Trace id of the print job, joinable to the JSONL event stream.
    pub trace_id: String,
    /// Per-trace sequence number of the usage event.
    pub seq: i64,
    /// RFC 3339 UTC timestamp of the attempt.
    pub at: String,
    /// Origin of the submission: the browser origin, `cli` for the local CLI,
    /// or empty for a local session without one.
    pub origin: String,
    /// Template handle (`tpl-<id>`) when the job used a stored template.
    pub template: Option<String>,
    /// Tags attached to the job, decoded from their JSON array.
    pub tags: Vec<String>,
    /// Printer name, or `None` when the job never reached printing.
    pub printer: Option<String>,
    /// Pages per copy in the document.
    pub pages: u32,
    /// Copies requested.
    pub copies: u32,
    /// Final job status: `done`, `failed`, `cancelled` or `pending`.
    pub status: String,
    /// End-to-end duration in milliseconds, from request to finish.
    pub dur_ms: i64,
    /// `E_*` protocol code when the job failed, else `None`.
    pub error_code: Option<String>,
}

/// A row about to be inserted. Borrowed so the engine writes without cloning
/// a tag list it already owns.
#[derive(Debug, Clone, Copy)]
pub struct NewPrintRecord<'a> {
    /// Trace id the row belongs to; joins to the JSONL event stream.
    pub trace_id: &'a str,
    /// Sequence number within that trace; the store widens it to `i64`.
    pub seq: u64,
    /// RFC 3339 UTC timestamp, formatted by the caller to avoid a clock read
    /// per row.
    pub at: &'a str,
    /// Origin of the submission; see [`PrintRecord::origin`].
    pub origin: &'a str,
    /// Template handle used, when any.
    pub template: Option<&'a str>,
    /// Tags, serialised into a JSON array for the `tags` column.
    pub tags: &'a [String],
    /// Printer name, when the job reached printing.
    pub printer: Option<&'a str>,
    /// Pages per copy.
    pub pages: u32,
    /// Copies requested.
    pub copies: u32,
    /// `done`, `failed`, `cancelled` or `pending`.
    pub status: &'a str,
    /// End-to-end duration in milliseconds.
    pub dur_ms: u64,
    /// `E_*` code for a failure, else `None`.
    pub error_code: Option<&'a str>,
}

/// Aggregation dimension of `records.stats` and `printkit stats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsGroup {
    /// Group by tag; a row with several tags counts once per tag.
    Tag,
    /// Group by template handle; rows without one land in `unnamed`.
    Template,
    /// Group by printer name; rows without one land in `default`.
    Printer,
    /// Group by local calendar day of `at`, keyed `YYYY-MM-DD`.
    Day,
}

impl StatsGroup {
    /// Wire name used by `records.stats` and the CLI: `tag`, `template`,
    /// `printer` or `day`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tag => "tag",
            Self::Template => "template",
            Self::Printer => "printer",
            Self::Day => "day",
        }
    }
}

impl std::str::FromStr for StatsGroup {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "tag" => Ok(Self::Tag),
            "template" => Ok(Self::Template),
            "printer" => Ok(Self::Printer),
            "day" => Ok(Self::Day),
            _ => Err(()),
        }
    }
}

/// Filters shared by listing, statistics and CSV export.
///
/// `origin` is `None` for an administrator (all origins) and `Some` for a
/// `print` token, which is forced to its bound origin.
#[derive(Debug, Clone, Default)]
pub struct RecordFilter {
    /// Inclusive lower bound, RFC 3339.
    pub from: Option<String>,
    /// Exclusive upper bound, RFC 3339.
    pub to: Option<String>,
    /// Keep rows having at least one of these tags (any-of, not all-of).
    pub tags: Vec<String>,
    /// Exact printer name; `default` matches rows with no printer.
    pub printer: Option<String>,
    /// Exact `status` (`done`, `failed`, `cancelled` or `pending`).
    pub status: Option<String>,
    /// Exact origin; see the struct note for the admin/token distinction.
    pub origin: Option<String>,
    /// Rows below this id, for the next page.
    pub cursor: Option<i64>,
    /// Page size; [`Store::list_print_records`] clamps it to `1..=500`.
    pub limit: u32,
}

/// One statistics bucket. `sheets` is `SUM(pages * copies)` and `avg_ms` is
/// the average end-to-end duration; they are separate bases and are never
/// derived from each other.
#[derive(Debug, Clone, PartialEq)]
pub struct StatsBucket {
    /// Group value: tag name, template handle or `unnamed`, printer name or
    /// `default`, or a local `YYYY-MM-DD` date.
    pub key: String,
    /// Rows contributing to the bucket.
    pub count: i64,
    /// `SUM(pages * copies)`: printed sheets, not row count.
    pub sheets: i64,
    /// Mean `dur_ms` of the bucket, in milliseconds.
    pub avg_ms: f64,
}

/// One statistics query.
#[derive(Debug, Clone)]
pub struct StatsQuery {
    /// Dimension to group by.
    pub group: StatsGroup,
    /// Row filter applied before grouping.
    pub filter: RecordFilter,
}

/// Cards shown above the records table.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSummary {
    /// Sheets (`pages * copies`) recorded today by local calendar date, all
    /// statuses (legacy `todaySheets` behavior).
    pub today_sheets: i64,
    /// Deprecated compatibility field: successful print requests are counted
    /// by `today_done` (one per successful request). This retains
    /// `sum(pages * copies)` over today's done rows for old clients.
    pub today_done_sheets: i64,
    /// Count of today's `status = done` rows.
    pub today_done: i64,
    /// Count of today's `status = failed` rows.
    pub today_failed: i64,
    /// Count of today's `status = cancelled` rows.
    pub today_cancelled: i64,
    /// Matching rows.
    pub total: i64,
    /// Matching rows with `status = 'failed'`.
    pub failed: i64,
    /// Matching rows with `status = 'cancelled'`.
    pub cancelled: i64,
    /// Matching rows with `status = 'pending'`.
    pub pending: i64,
    /// Mean `dur_ms` over matching rows, in milliseconds.
    pub avg_ms: f64,
    /// Printer with the highest mean duration and that mean in milliseconds;
    /// `None` when the filter matches no rows.
    pub slowest_printer: Option<(String, f64)>,
}

/// What a cleanup pass removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CleanupReport {
    /// Rows removed because they were older than the retention window.
    pub expired: u64,
    /// Rows removed from the front of the table to stay under `max_rows`.
    pub overflow: u64,
}

/// Count and oldest timestamp of the usage table, for `doctor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordCount {
    /// Rows in `print_record`.
    pub count: i64,
    /// `min(at)` as RFC 3339; `None` when the table is empty.
    pub earliest: Option<String>,
}

impl Store {
    /// Open an existing database read-only for the CLI.
    ///
    /// Returns `None` before the first `serve` run created the file. WAL lets
    /// this read while `serve` writes.
    pub fn open_existing(db_path: &Path) -> Result<Option<Self>, StoreError> {
        if !db_path.is_file() {
            return Ok(None);
        }
        let connection = open_read_only_with_wal_recovery(db_path)?;
        connection.busy_timeout(Duration::from_millis(5_000))?;
        Ok(Some(Self {
            path: db_path.to_path_buf(),
            connection: Mutex::new(connection),
        }))
    }

    /// Write one usage row. Best effort at the call site: statistics must
    /// never fail a print.
    pub fn insert_print_record(&self, record: &NewPrintRecord<'_>) -> Result<i64, StoreError> {
        let tags =
            serde_json::to_string(record.tags).map_err(|source| StoreError::SettingJson {
                key: "print_record.tags",
                source,
            })?;
        let connection = self.connection()?;
        retry_busy(|| {
            connection.execute(
                "INSERT INTO print_record
                 (trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    record.trace_id,
                    i64::try_from(record.seq).unwrap_or(i64::MAX),
                    record.at,
                    record.origin,
                    record.template,
                    tags,
                    record.printer,
                    i64::from(record.pages),
                    i64::from(record.copies),
                    record.status,
                    i64::try_from(record.dur_ms).unwrap_or(i64::MAX),
                    record.error_code,
                ],
            )
        })?;
        Ok(connection.last_insert_rowid())
    }

    /// Expire old rows, then trim the table to the row limit. The two branches
    /// are independent: either can remove rows on its own.
    pub fn cleanup_print_records(
        &self,
        retention_days: u16,
        max_rows: u32,
    ) -> Result<CleanupReport, StoreError> {
        let cutoff = OffsetDateTime::now_utc() - time::Duration::days(i64::from(retention_days));
        let cutoff = cutoff.format(&Rfc3339)?;
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let expired = transaction.execute(
            "DELETE FROM print_record WHERE julianday(at) < julianday(?1)",
            params![cutoff],
        )?;
        let count: i64 =
            transaction.query_row("SELECT count(*) FROM print_record", [], |row| row.get(0))?;
        let excess = usize::try_from(count - i64::from(max_rows)).unwrap_or(0);
        let overflow = if excess > 0 {
            transaction.execute(
                "DELETE FROM print_record WHERE id IN
                 (SELECT id FROM print_record ORDER BY id ASC LIMIT ?1)",
                params![i64::try_from(excess).unwrap_or(i64::MAX)],
            )?
        } else {
            0
        };
        transaction.commit()?;
        Ok(CleanupReport {
            expired: u64::try_from(expired).unwrap_or(u64::MAX),
            overflow: u64::try_from(overflow).unwrap_or(u64::MAX),
        })
    }

    /// Row count and oldest `at` of the usage table, for `doctor`.
    pub fn print_record_count(&self) -> Result<RecordCount, StoreError> {
        let connection = self.connection()?;
        connection
            .query_row("SELECT count(*), min(at) FROM print_record", [], |row| {
                Ok(RecordCount {
                    count: row.get(0)?,
                    earliest: row.get(1)?,
                })
            })
            .map_err(StoreError::from)
    }

    /// Highest `seq` ever written to the usage table. A process seeds its
    /// sequence floor from this too, so `--log=usage-only` does not make the
    /// next start reuse numbers.
    pub fn max_print_record_seq(&self) -> Result<Option<u64>, StoreError> {
        let connection = self.connection()?;
        let value: Option<i64> = connection
            .query_row("SELECT max(seq) FROM print_record", [], |row| row.get(0))
            .optional()?
            .flatten();
        Ok(value.and_then(|value| u64::try_from(value).ok()))
    }

    /// One page of usage rows, newest (highest id) first.
    ///
    /// `filter` is applied as SQL; `limit` is clamped to `1..=500`. When
    /// `filter.cursor` is set only rows with a smaller id are returned, which
    /// is how CSV export and the records page walk to the next page.
    pub fn list_print_records(
        &self,
        filter: &RecordFilter,
    ) -> Result<Vec<PrintRecord>, StoreError> {
        let (where_sql, values) = filter_sql(filter);
        let limit = filter.limit.clamp(1, 500);
        let mut sql = format!(
            "SELECT id, trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code
             FROM print_record {where_sql}"
        );
        if filter.cursor.is_some() {
            sql.push_str(if where_sql.is_empty() {
                " WHERE"
            } else {
                " AND"
            });
            sql.push_str(" id < ?");
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let mut bindings: Vec<rusqlite::types::Value> = values;
        if let Some(cursor) = filter.cursor {
            bindings.push(rusqlite::types::Value::Integer(cursor));
        }
        bindings.push(rusqlite::types::Value::Integer(i64::from(limit)));
        let rows = statement.query_map(rusqlite::params_from_iter(bindings), row_to_record)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    /// One SQL `GROUP BY` per dimension; the protocol, the page and the CLI
    /// all call this function, which is why their numbers cannot drift.
    pub fn stats(&self, query: &StatsQuery) -> Result<Vec<StatsBucket>, StoreError> {
        let (where_sql, values) = filter_sql(&query.filter);
        let expression = match query.group {
            StatsGroup::Tag => "value",
            StatsGroup::Template => "coalesce(template, 'unnamed')",
            StatsGroup::Printer => "coalesce(printer, 'default')",
            StatsGroup::Day => "date(at, 'localtime')",
        };
        let from = match query.group {
            StatsGroup::Tag => "print_record, json_each(print_record.tags)".to_owned(),
            _ => "print_record".to_owned(),
        };
        let sql = format!(
            "SELECT {expression} AS bucket, count(*) AS n, coalesce(sum(pages * copies), 0) AS sheets,
                    coalesce(avg(dur_ms), 0.0) AS avg_ms
             FROM {from} {where_sql}
             GROUP BY bucket ORDER BY n DESC, bucket ASC"
        );
        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let rows = statement.query_map(rusqlite::params_from_iter(values), |row| {
            Ok(StatsBucket {
                key: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                count: row.get(1)?,
                sheets: row.get(2)?,
                avg_ms: row.get(3)?,
            })
        })?;
        let mut buckets = Vec::new();
        for row in rows {
            buckets.push(row?);
        }
        Ok(buckets)
    }

    /// Summary row for the records page; `today*` fields use the local calendar date.
    ///
    /// `today_done_sheets` is a deprecated compatibility count
    /// (`pages * copies`); `today_done` is the KPI (one per successful request).
    pub fn record_summary(&self, filter: &RecordFilter) -> Result<RecordSummary, StoreError> {
        let (where_sql, values) = filter_sql(filter);
        let sql = format!(
            "SELECT
                coalesce(sum(CASE WHEN date(at, 'localtime') = date('now', 'localtime')
                                  THEN pages * copies ELSE 0 END), 0) AS today_sheets,
                coalesce(sum(CASE WHEN date(at, 'localtime') = date('now', 'localtime')
                                       AND status = 'done'
                                  THEN pages * copies ELSE 0 END), 0) AS today_done_sheets,
                coalesce(sum(CASE WHEN date(at, 'localtime') = date('now', 'localtime')
                                       AND status = 'done'
                                  THEN 1 ELSE 0 END), 0) AS today_done,
                coalesce(sum(CASE WHEN date(at, 'localtime') = date('now', 'localtime')
                                       AND status = 'failed'
                                  THEN 1 ELSE 0 END), 0) AS today_failed,
                coalesce(sum(CASE WHEN date(at, 'localtime') = date('now', 'localtime')
                                       AND status = 'cancelled'
                                  THEN 1 ELSE 0 END), 0) AS today_cancelled,
                count(*) AS total,
                coalesce(sum(CASE WHEN status = 'failed' THEN 1 ELSE 0 END), 0) AS failed,
                coalesce(sum(CASE WHEN status = 'cancelled' THEN 1 ELSE 0 END), 0) AS cancelled,
                coalesce(sum(CASE WHEN status = 'pending' THEN 1 ELSE 0 END), 0) AS pending,
                coalesce(avg(dur_ms), 0.0) AS avg_ms
             FROM print_record {where_sql}"
        );
        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let (
            today_sheets,
            today_done_sheets,
            today_done,
            today_failed,
            today_cancelled,
            total,
            failed,
            cancelled,
            pending,
            avg_ms,
        ) = statement.query_row(rusqlite::params_from_iter(values), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, f64>(9)?,
            ))
        })?;

        let printer_sql = format!(
            "SELECT coalesce(printer, 'default') AS printer, avg(dur_ms) AS avg_ms
             FROM print_record {where_sql} GROUP BY printer ORDER BY avg_ms DESC LIMIT 1"
        );
        let mut statement = connection.prepare(&printer_sql)?;
        let (_, values) = filter_sql(filter);
        let slowest = statement
            .query_row(rusqlite::params_from_iter(values), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
            })
            .optional()?;
        Ok(RecordSummary {
            today_sheets,
            today_done_sheets,
            today_done,
            today_failed,
            today_cancelled,
            total,
            failed,
            cancelled,
            pending,
            avg_ms,
            slowest_printer: slowest,
        })
    }

    /// CSV with a UTF-8 BOM, columns fixed by the protocol contract.
    ///
    /// One bounded query writes the file; the old version paged 500 rows at a
    /// time and sorted the whole set in memory. At most 5000 of the newest rows
    /// are exported, so a retention backlog cannot make the response
    /// unbounded.
    pub fn records_csv(&self, filter: &RecordFilter) -> Result<String, StoreError> {
        let (where_sql, values) = filter_sql(filter);
        let sql = format!(
            "SELECT id, trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code
             FROM (SELECT * FROM print_record {where_sql} ORDER BY id DESC LIMIT {RECORDS_CSV_LIMIT}) AS page
             ORDER BY id ASC"
        );
        let connection = self.connection()?;
        let mut statement = connection.prepare(&sql)?;
        let mut csv = String::from("\u{feff}");
        csv.push_str(
            "at,origin,template,tags,printer,pages,copies,status,dur_ms,error_code,trace_id\n",
        );
        let rows = statement.query_map(rusqlite::params_from_iter(values), row_to_record)?;
        for record in rows {
            let record = record?;
            let fields = [
                record.at,
                record.origin,
                record.template.unwrap_or_default(),
                record.tags.join(";"),
                record.printer.unwrap_or_default(),
                record.pages.to_string(),
                record.copies.to_string(),
                record.status,
                record.dur_ms.to_string(),
                record.error_code.unwrap_or_default(),
                record.trace_id,
            ];
            csv.push_str(&fields.map(|field| csv_field(&field)).join(","));
            csv.push('\n');
        }
        Ok(csv)
    }
}

/// Rows one CSV export may contain. The default retention ceiling is 5000, so
/// a healthy table never hits this bound; it exists to keep memory bounded.
const RECORDS_CSV_LIMIT: usize = 5_000;

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<PrintRecord> {
    let tags: String = row.get(6)?;
    Ok(PrintRecord {
        id: row.get(0)?,
        trace_id: row.get(1)?,
        seq: row.get(2)?,
        at: row.get(3)?,
        origin: row.get(4)?,
        template: row.get(5)?,
        tags: serde_json::from_str(&tags).unwrap_or_default(),
        printer: row.get(7)?,
        pages: u32::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
        copies: u32::try_from(row.get::<_, i64>(9)?).unwrap_or(0),
        status: row.get(10)?,
        dur_ms: row.get(11)?,
        error_code: row.get(12)?,
    })
}

fn filter_sql(filter: &RecordFilter) -> (String, Vec<rusqlite::types::Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut values = Vec::new();
    if let Some(from) = &filter.from {
        clauses.push("julianday(at) >= julianday(?)".to_owned());
        values.push(rusqlite::types::Value::Text(from.clone()));
    }
    if let Some(to) = &filter.to {
        clauses.push("julianday(at) < julianday(?)".to_owned());
        values.push(rusqlite::types::Value::Text(to.clone()));
    }
    if let Some(origin) = &filter.origin {
        clauses.push("origin = ?".to_owned());
        values.push(rusqlite::types::Value::Text(origin.clone()));
    }
    if let Some(printer) = &filter.printer {
        clauses.push("coalesce(printer, 'default') = ?".to_owned());
        values.push(rusqlite::types::Value::Text(printer.clone()));
    }
    if let Some(status) = &filter.status {
        clauses.push("status = ?".to_owned());
        values.push(rusqlite::types::Value::Text(status.clone()));
    }
    if !filter.tags.is_empty() {
        let placeholders = vec!["?"; filter.tags.len()].join(", ");
        clauses.push(format!(
            "EXISTS (SELECT 1 FROM json_each(print_record.tags) WHERE value IN ({placeholders}))"
        ));
        for tag in &filter.tags {
            values.push(rusqlite::types::Value::Text(tag.clone()));
        }
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_sql, values)
}

fn csv_field(value: &str) -> String {
    // Spreadsheet applications interpret these leading characters as formulas
    // even inside CSV cells. A leading apostrophe forces text without changing
    // the value that a CSV reader sees.
    let value = match value.as_bytes().first() {
        Some(b'=' | b'+' | b'-' | b'@' | b'\t' | b'\r') => format!("'{value}"),
        _ => value.to_owned(),
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

fn now() -> Result<String, StoreError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(StoreError::from)
}

#[cfg(unix)]
fn restrict_data_dir(data_dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
        StoreError::Restrict {
            path: data_dir.display().to_string(),
            source,
        }
    })
}

#[cfg(unix)]
fn restrict_path(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(unix)]
fn restrict_database_files(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::PermissionsExt;
    let mut files = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", path.display()));
        if side.exists() {
            files.push(side);
        }
    }
    for file in files {
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).map_err(|source| {
            StoreError::Restrict {
                path: file.display().to_string(),
                source,
            }
        })?;
    }
    Ok(())
}

/// Retry a write a few times when SQLite reports another writer is active.
fn retry_busy<T, F>(mut operation: F) -> Result<T, StoreError>
where
    F: FnMut() -> Result<T, rusqlite::Error>,
{
    let mut attempt = 0;
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy && attempt < 4 =>
            {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(25 * attempt));
            }
            Err(error) => return Err(StoreError::from(error)),
        }
    }
}

fn configure(connection: &Connection) -> Result<(), StoreError> {
    // Set the lock wait before switching journal modes: concurrent first
    // starts must not fail with "database is locked" during the WAL switch.
    connection.execute_batch("PRAGMA busy_timeout=5000; PRAGMA synchronous=NORMAL;")?;
    // WAL keeps CLI read-only opens working while `serve` writes.
    let _journal_mode: String =
        connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let start = usize::try_from(version).unwrap_or(MIGRATIONS.len());
    if start > MIGRATIONS.len() {
        return Err(StoreError::SchemaTooNew { version });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(start) {
        let next = i64::try_from(index + 1).unwrap_or(version);
        let transaction = connection.transaction()?;
        transaction.execute_batch(sql)?;
        transaction.pragma_update(None, "user_version", next)?;
        transaction.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(dir: &Path) -> Store {
        Store::open(dir).unwrap()
    }

    #[test]
    fn concurrent_origin_updates_do_not_lose_entries() {
        let temp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(open(temp.path()));
        let mut handles = Vec::new();
        for index in 0..8 {
            let store = std::sync::Arc::clone(&store);
            handles.push(std::thread::spawn(move || {
                store
                    .add_origin(&format!("http://127.0.0.1:{index}"))
                    .unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let origins = store.origins().unwrap();
        assert_eq!(origins.len(), 8, "{origins:?}");
    }

    #[test]
    fn migration_records_its_version_and_creates_tables() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let connection = store.connection().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 3, "V9 drops the retired table");
    }

    #[test]
    fn revoked_tokens_stop_matching_and_keep_their_row() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let id = store
            .insert_token("https://app.example", "hash", "print", false)
            .unwrap();
        assert!(store.token_by_hash("hash").unwrap().is_some());
        assert!(store.revoke_token(id).unwrap());
        assert!(store.token_by_hash("hash").unwrap().is_none());
        assert!(!store.revoke_token(id).unwrap());
        let connection = store.connection().unwrap();
        let rows: i64 = connection
            .query_row(
                "SELECT count(*) FROM auth WHERE id = ?1 AND revoked_at IS NOT NULL",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// A version-7 database with one template row and `calibration_rows`
    /// calibration rows, as left by an older build before V8/V9 run.
    fn pre_v8_database(path: &Path, calibration_rows: usize) {
        let connection = Connection::open(path).unwrap();
        for (index, sql) in MIGRATIONS.iter().take(7).enumerate() {
            connection.execute_batch(sql).unwrap();
            connection
                .pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        connection
            .execute(
                "INSERT INTO template (origin, name, document_json, created_at, updated_at)
                 VALUES ('', 'invoice', '{}', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        for index in 0..calibration_rows {
            connection
                .execute(
                    "INSERT INTO calibration
                         (origin, printer_name, paper_name, path_kind, dx_mm, dy_mm, scale, updated_at)
                     VALUES (?1, 'laser', 'A4', 'webview', 1.0, 0.0, 1.0, '2026-01-01T00:00:00Z')",
                    params![format!("origin-{index}")],
                )
                .unwrap();
        }
    }

    fn backup_rows(path: &Path, table: &str) -> i64 {
        let backup = Connection::open(path).unwrap();
        backup
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[cfg(unix)]
    fn backup_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn backup_inode(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().ino()
    }

    #[test]
    fn migration_drops_template_and_writes_a_pre_v8_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);

        let store = open(temp.path());
        let connection = store.connection().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
        let template_tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'template'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(template_tables, 0);
        drop(connection);

        let backup_path = temp.path().join(format!("{DB_FILE}.bak-before-v8"));
        assert!(backup_path.is_file(), "pre-V8 backup missing");
        assert_eq!(backup_rows(&backup_path, "template"), 1);
        #[cfg(unix)]
        assert_eq!(backup_mode(&backup_path), 0o600);
    }

    #[test]
    fn a_new_database_does_not_write_a_pre_v8_backup() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        drop(store);
        assert!(
            !temp
                .path()
                .join(format!("{DB_FILE}.bak-before-v8"))
                .exists()
        );
    }

    #[test]
    fn an_existing_backup_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);
        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v8"));
        let connection = Connection::open(&backup).unwrap();
        connection
            .execute_batch("CREATE TABLE marker (value TEXT);")
            .unwrap();
        connection.pragma_update(None, "user_version", 42).unwrap();
        drop(connection);
        let size = std::fs::metadata(&backup).unwrap().len();

        let store = open(temp.path());
        drop(store);
        assert_eq!(std::fs::metadata(&backup).unwrap().len(), size);
        let existing = Connection::open(&backup).unwrap();
        let marker_tables: i64 = existing
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_tables, 1, "a usable backup must be kept");
    }

    #[test]
    fn a_written_wal_flagged_backup_counts_as_existing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);
        let connection = Connection::open(&path).unwrap();
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode, "wal");
        backup_before_template_drop(&connection, &path).unwrap();

        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v8"));
        // The copy inherits the WAL header flag without -wal/-shm side files.
        let header = std::fs::read(&backup).unwrap();
        assert_eq!((header[18], header[19]), (2, 2), "expected a WAL header");
        #[cfg(unix)]
        let inode = backup_inode(&backup);

        backup_before_template_drop(&connection, &path).unwrap();
        #[cfg(unix)]
        assert_eq!(
            backup_inode(&backup),
            inode,
            "an existing backup must not be replaced"
        );
        assert_eq!(backup_rows(&backup, "template"), 1);
    }

    #[test]
    fn a_zero_length_pre_v8_backup_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);
        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v8"));
        std::fs::write(&backup, "").unwrap();

        let store = open(temp.path());
        drop(store);
        assert!(
            std::fs::metadata(&backup).unwrap().len() > 0,
            "a zero-length leftover must be replaced"
        );
        assert_eq!(backup_rows(&backup, "template"), 1);
    }

    #[test]
    fn an_unreadable_pre_v8_backup_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);
        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v8"));
        std::fs::write(&backup, "sentinel").unwrap();

        let store = open(temp.path());
        drop(store);
        assert_eq!(backup_rows(&backup, "template"), 1);
    }

    #[test]
    fn a_truncated_pre_v8_backup_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);
        let connection = Connection::open(&path).unwrap();
        connection
            .query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
            .unwrap();
        backup_before_template_drop(&connection, &path).unwrap();
        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v8"));
        let full = std::fs::read(&backup).unwrap();
        assert!(full.len() > 4096, "the source must span several pages");
        // Keep the header with its full page count but drop the tail pages.
        std::fs::write(&backup, &full[..4096]).unwrap();

        backup_before_template_drop(&connection, &path).unwrap();
        assert!(
            std::fs::metadata(&backup).unwrap().len() > 4096,
            "a truncated leftover must be replaced"
        );
        assert_eq!(backup_rows(&backup, "template"), 1);
    }

    #[test]
    fn a_stale_fixed_name_temporary_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 1);
        let stale_v8 = temp.path().join(format!("{DB_FILE}.bak-before-v8.tmp"));
        let stale_v9 = temp.path().join(format!("{DB_FILE}.bak-before-v9.tmp"));
        std::fs::write(&stale_v8, "half-written").unwrap();
        std::fs::write(&stale_v9, "half-written").unwrap();

        let store = open(temp.path());
        drop(store);
        for (label, table) in [("v8", "template"), ("v9", "calibration")] {
            let backup = temp.path().join(format!("{DB_FILE}.bak-before-{label}"));
            assert!(backup.is_file(), "pre-{label} backup missing");
            assert_eq!(backup_rows(&backup, table), 1);
        }
        // The old fixed-name leftovers are never read as backups; they stay
        // untouched instead of being mistaken for a finished copy.
        assert_eq!(std::fs::read_to_string(&stale_v8).unwrap(), "half-written");
        assert_eq!(std::fs::read_to_string(&stale_v9).unwrap(), "half-written");
    }

    #[test]
    fn a_rename_failure_keeps_the_pre_v8_database_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v8_database(&path, 0);
        let connection = Connection::open(&path).unwrap();
        let temporary = std::sync::Mutex::new(None);
        let error = backup_before_template_drop_with(&connection, &path, |from, _| {
            *temporary.lock().unwrap() = Some(from.to_path_buf());
            Err(std::io::Error::other("rename blocked"))
        })
        .unwrap_err();
        assert!(matches!(error, StoreError::BackupMove { .. }), "{error:?}");
        let temporary = temporary.into_inner().unwrap().unwrap();
        assert!(
            !temporary.exists(),
            "the temporary backup must be removed: {}",
            temporary.display()
        );
        assert!(
            !temp
                .path()
                .join(format!("{DB_FILE}.bak-before-v8"))
                .exists(),
            "no final backup may appear after a failed rename"
        );
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'template'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "the original table must survive");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 7);
    }

    #[test]
    fn a_backup_failure_aborts_migration() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("nested").join(DB_FILE);
        std::fs::create_dir(temp.path().join("nested")).unwrap();
        let connection = Connection::open(&db).unwrap();
        connection.pragma_update(None, "user_version", 7).unwrap();
        let missing_parent = temp.path().join("missing").join(DB_FILE);
        let error = backup_before_template_drop(&connection, &missing_parent).unwrap_err();
        assert!(matches!(error, StoreError::BackupTemp { .. }), "{error:?}");
    }

    fn pre_v9_database(path: &Path, calibration_rows: usize) {
        let connection = Connection::open(path).unwrap();
        for (index, sql) in MIGRATIONS.iter().take(8).enumerate() {
            connection.execute_batch(sql).unwrap();
            connection
                .pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        for index in 0..calibration_rows {
            connection
                .execute(
                    "INSERT INTO calibration
                         (origin, printer_name, paper_name, path_kind, dx_mm, dy_mm, scale, updated_at)
                     VALUES (?1, 'laser', 'A4', 'webview', 1.0, 0.0, 1.0, '2026-01-01T00:00:00Z')",
                    params![format!("origin-{index}")],
                )
                .unwrap();
        }
    }

    #[test]
    fn a_pre_v9_database_with_calibration_rows_is_backed_up() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v9_database(&path, 1);
        drop(open(temp.path()));

        let backup_path = temp.path().join(format!("{DB_FILE}.bak-before-v9"));
        assert!(backup_path.is_file(), "pre-V9 backup missing");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&backup_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the backup must stay owner-only");
        }
        let backup = Connection::open(&backup_path).unwrap();
        let rows: i64 = backup
            .query_row("SELECT count(*) FROM calibration", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);

        let live = Connection::open(&path).unwrap();
        let tables: i64 = live
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'calibration'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 0);
        let version: i64 = live
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
    }

    #[test]
    fn a_rename_failure_keeps_the_original_database_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v9_database(&path, 1);
        let connection = Connection::open(&path).unwrap();
        let temporary = std::sync::Mutex::new(None);
        let error = backup_before_calibration_drop_with(&connection, &path, |from, _| {
            *temporary.lock().unwrap() = Some(from.to_path_buf());
            Err(std::io::Error::other("rename blocked"))
        })
        .unwrap_err();
        assert!(matches!(error, StoreError::BackupMove { .. }), "{error:?}");
        let temporary = temporary.into_inner().unwrap().unwrap();
        assert!(
            !temporary.exists(),
            "the temporary backup must be removed: {}",
            temporary.display()
        );
        assert!(
            !temp
                .path()
                .join(format!("{DB_FILE}.bak-before-v9"))
                .exists(),
            "no final backup may appear after a failed rename"
        );
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'calibration'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "the original table must survive");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
    }

    #[test]
    fn backup_temporary_names_are_unique_and_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join(DB_FILE);
        let first = reserve_backup_temporary(&base, "v9").unwrap();
        let second = reserve_backup_temporary(&base, "v9").unwrap();
        assert_ne!(first, second, "two reservations must not share a name");
        assert!(first.is_file() && second.is_file());
        let error = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&first)
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }

    #[test]
    fn a_pre_v9_database_without_calibration_rows_writes_no_backup() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v9_database(&path, 0);
        drop(open(temp.path()));
        assert!(
            !temp
                .path()
                .join(format!("{DB_FILE}.bak-before-v9"))
                .exists()
        );
    }

    #[test]
    fn a_new_database_does_not_write_a_pre_v9_backup() {
        let temp = tempfile::tempdir().unwrap();
        drop(open(temp.path()));
        assert!(
            !temp
                .path()
                .join(format!("{DB_FILE}.bak-before-v9"))
                .exists()
        );
    }

    #[test]
    fn an_existing_calibration_backup_is_not_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v9_database(&path, 1);
        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v9"));
        let connection = Connection::open(&backup).unwrap();
        connection
            .execute_batch("CREATE TABLE marker (value TEXT);")
            .unwrap();
        drop(connection);
        let size = std::fs::metadata(&backup).unwrap().len();

        drop(open(temp.path()));
        assert_eq!(std::fs::metadata(&backup).unwrap().len(), size);
        let existing = Connection::open(&backup).unwrap();
        let marker_tables: i64 = existing
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker_tables, 1, "a usable backup must be kept");
    }

    #[test]
    fn a_zero_length_calibration_backup_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v9_database(&path, 1);
        let backup = temp.path().join(format!("{DB_FILE}.bak-before-v9"));
        std::fs::write(&backup, "").unwrap();

        drop(open(temp.path()));
        assert!(
            std::fs::metadata(&backup).unwrap().len() > 0,
            "a zero-length leftover must be replaced"
        );
        assert_eq!(backup_rows(&backup, "calibration"), 1);
        #[cfg(unix)]
        assert_eq!(backup_mode(&backup), 0o600);
    }

    #[test]
    fn a_calibration_backup_failure_aborts_migration() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        pre_v9_database(&path, 1);
        let connection = Connection::open(&path).unwrap();
        let missing = temp.path().join("missing").join(DB_FILE);
        let error =
            backup_before_calibration_drop_with(&connection, &missing, |_, _| Ok(())).unwrap_err();
        assert!(matches!(error, StoreError::BackupTemp { .. }), "{error:?}");
        let tables: i64 = connection
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'calibration'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "the original table must survive a failed backup");
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 8);
    }

    #[test]
    fn settings_round_trip_and_are_readable_without_write_access() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        store.set_setting("log_level", "debug").unwrap();
        let settings = Store::read_settings(store.path()).unwrap().unwrap();
        assert_eq!(settings.get("log_level").map(String::as_str), Some("debug"));
        let missing = Store::read_settings(&temp.path().join("absent.db")).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn read_only_recovery_refuses_writes_and_keeps_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        store.set_setting("log_level", "debug").unwrap();
        store
            .insert_print_record(&record(&[], "https://read.example", 5))
            .unwrap();
        let rows = store.print_record_count().unwrap().count;
        let path = store.path().to_path_buf();
        drop(store);

        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(connection);
        for suffix in ["-wal", "-shm"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            let _ = fs::remove_file(side);
        }

        let reader = Store::open_existing(&path).unwrap().unwrap();
        assert_eq!(reader.print_record_count().unwrap().count, rows);
        assert_eq!(
            Store::read_settings(&path)
                .unwrap()
                .unwrap()
                .get("log_level")
                .map(String::as_str),
            Some("debug")
        );
        let refused = reader.insert_print_record(&record(&[], "https://read.example", 9));
        assert!(
            refused.is_err(),
            "a recovery handle must refuse writes: {refused:?}"
        );
        assert!(reader.set_setting("log_level", "trace").is_err());
        drop(reader);

        let reopened = open(temp.path());
        assert_eq!(reopened.print_record_count().unwrap().count, rows);
        assert_eq!(
            reopened.get_setting("log_level").unwrap().as_deref(),
            Some("debug")
        );
    }

    #[test]
    fn origins_are_sorted_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        store.add_origin("https://b.example").unwrap();
        store.add_origin("https://a.example").unwrap();
        let origins = store.add_origin("https://b.example").unwrap();
        assert_eq!(
            origins,
            vec![
                "https://a.example".to_owned(),
                "https://b.example".to_owned()
            ]
        );
        assert_eq!(store.origins().unwrap(), origins);
    }

    #[test]
    fn origins_can_be_removed_idempotently() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        store.add_origin("https://a.example").unwrap();
        store.add_origin("https://b.example").unwrap();
        let origins = store.remove_origin("https://a.example").unwrap();
        assert_eq!(origins, vec!["https://b.example".to_owned()]);
        assert_eq!(store.origins().unwrap(), origins);
        assert_eq!(
            store.remove_origin("https://a.example").unwrap(),
            origins,
            "removing a missing origin is idempotent"
        );
    }

    #[test]
    fn tokens_are_looked_up_by_hash_and_touch_their_last_use() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let id = store
            .insert_token("https://a.example", "abcd", "print", false)
            .unwrap();
        let token = store.token_by_hash("abcd").unwrap().unwrap();
        assert_eq!(token.id, id);
        assert_eq!(token.origin, "https://a.example");
        assert_eq!(token.scope, "print");
        assert!(!token.non_browser);
        assert!(store.token_by_hash("nope").unwrap().is_none());
        store.touch_token(id).unwrap();
        let connection = store.connection().unwrap();
        let touched: Option<String> = connection
            .query_row(
                "SELECT last_used_at FROM auth WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(touched.is_some());
    }

    /// Current instant expressed in the host's local UTC offset, so test
    /// fixtures carry a local calendar date regardless of the runner timezone.
    fn local_now(store: &Store) -> OffsetDateTime {
        let connection = store.connection().unwrap();
        let minutes: f64 = connection
            .query_row(
                "SELECT (julianday('now', 'localtime') - julianday('now')) * 1440",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let offset = time::UtcOffset::from_whole_seconds((minutes * 60.0).round() as i32)
            .expect("a whole-minute UTC offset");
        OffsetDateTime::now_utc().to_offset(offset)
    }

    fn record<'a>(tags: &'a [String], origin: &'a str, dur_ms: u64) -> NewPrintRecord<'a> {
        NewPrintRecord {
            trace_id: "trace",
            seq: 1,
            at: "2026-11-08T10:00:00Z",
            origin,
            template: Some("template"),
            tags,
            printer: Some("printer"),
            pages: 1,
            copies: 2,
            status: "done",
            dur_ms,
            error_code: None,
        }
    }

    #[test]
    fn the_summary_counts_cancelled_rows_separately() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let tags = Vec::new();
        store
            .insert_print_record(&record(&tags, "https://a.example", 10))
            .unwrap();
        let mut failed = record(&tags, "https://a.example", 20);
        failed.status = "failed";
        store.insert_print_record(&failed).unwrap();
        let mut cancelled = record(&tags, "https://a.example", 30);
        cancelled.status = "cancelled";
        store.insert_print_record(&cancelled).unwrap();
        let summary = store.record_summary(&RecordFilter::default()).unwrap();
        assert_eq!(summary.total, 3);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.cancelled, 1);
    }
    #[test]
    fn summary_counts_todays_done_sheets_and_statuses_without_changing_legacy_today() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let tags = Vec::new();
        let now = local_now(&store);
        let today = now.format(&Rfc3339).unwrap();
        let yesterday = (now - time::Duration::hours(24)).format(&Rfc3339).unwrap();

        let mut done = record(&tags, "https://a.example", 10);
        done.at = &today;
        done.pages = 2;
        done.copies = 3;
        store.insert_print_record(&done).unwrap();

        let mut failed = record(&tags, "https://a.example", 20);
        failed.at = &today;
        failed.status = "failed";
        store.insert_print_record(&failed).unwrap();

        let mut cancelled = record(&tags, "https://b.example", 30);
        cancelled.at = &today;
        cancelled.status = "cancelled";
        cancelled.pages = 2;
        store.insert_print_record(&cancelled).unwrap();

        let mut pending = record(&tags, "https://a.example", 40);
        pending.at = &today;
        pending.status = "pending";
        pending.copies = 1;
        store.insert_print_record(&pending).unwrap();

        let mut old = record(&tags, "https://a.example", 50);
        old.at = &yesterday;
        old.pages = 4;
        store.insert_print_record(&old).unwrap();

        let summary = store.record_summary(&RecordFilter::default()).unwrap();
        // Legacy field: every status, pages*copies. 6 + 2 + 4 + 1; yesterday excluded.
        assert_eq!(summary.today_sheets, 13);
        assert_eq!(summary.today_done_sheets, 6);
        assert_eq!(summary.today_done, 1);
        assert_eq!(summary.today_failed, 1);
        assert_eq!(summary.today_cancelled, 1);
        assert_eq!(summary.total, 5);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.cancelled, 1);
        assert_eq!(summary.pending, 1);

        let filtered = store
            .record_summary(&RecordFilter {
                origin: Some("https://a.example".to_owned()),
                ..RecordFilter::default()
            })
            .unwrap();
        // Rows: done (6 sheets), failed (2), pending (1), yesterday excluded.
        assert_eq!(filtered.today_sheets, 9);
        assert_eq!(filtered.today_done_sheets, 6);
        assert_eq!(filtered.today_done, 1);
        assert_eq!(filtered.today_failed, 1);
        assert_eq!(filtered.today_cancelled, 0);
        assert_eq!(filtered.pending, 1);
        assert_eq!(filtered.total, 4);
    }

    #[test]
    fn summary_filters_apply_to_today_done_counters() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let now = local_now(&store);
        let today = now.format(&Rfc3339).unwrap();
        let yesterday = (now - time::Duration::hours(24)).format(&Rfc3339).unwrap();
        let tag_a = vec!["tag-a".to_owned()];
        let tag_b = vec!["tag-b".to_owned()];

        let mut done = record(&tag_a, "https://a.example", 10);
        done.at = &today;
        done.printer = Some("printer-1");
        done.pages = 2;
        done.copies = 3;
        store.insert_print_record(&done).unwrap();

        let mut failed = record(&tag_b, "https://a.example", 20);
        failed.at = &today;
        failed.status = "failed";
        failed.printer = Some("printer-2");
        store.insert_print_record(&failed).unwrap();

        let mut cancelled = record(&tag_b, "https://b.example", 30);
        cancelled.at = &today;
        cancelled.status = "cancelled";
        cancelled.printer = Some("printer-2");
        store.insert_print_record(&cancelled).unwrap();

        let mut pending = record(&tag_a, "https://a.example", 40);
        pending.at = &today;
        pending.status = "pending";
        pending.printer = Some("printer-1");
        pending.copies = 1;
        store.insert_print_record(&pending).unwrap();

        let mut old = record(&tag_a, "https://a.example", 50);
        old.at = &yesterday;
        old.printer = Some("printer-1");
        old.pages = 4;
        store.insert_print_record(&old).unwrap();

        let by_printer = store
            .record_summary(&RecordFilter {
                printer: Some("printer-1".to_owned()),
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(by_printer.today_sheets, 7); // done 6 + pending 1
        assert_eq!(by_printer.today_done_sheets, 6);
        assert_eq!(by_printer.today_done, 1);
        assert_eq!(by_printer.today_failed, 0);
        assert_eq!(by_printer.today_cancelled, 0);
        assert_eq!(by_printer.pending, 1);
        assert_eq!(by_printer.total, 3);

        let by_status = store
            .record_summary(&RecordFilter {
                status: Some("done".to_owned()),
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(by_status.today_sheets, 6);
        assert_eq!(by_status.today_done_sheets, 6);
        assert_eq!(by_status.today_done, 1);
        assert_eq!(by_status.pending, 0);
        assert_eq!(by_status.total, 2);

        let by_tag = store
            .record_summary(&RecordFilter {
                tags: vec!["tag-a".to_owned()],
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(by_tag.today_sheets, 7);
        assert_eq!(by_tag.today_done_sheets, 6);
        assert_eq!(by_tag.today_done, 1);
        assert_eq!(by_tag.pending, 1);
        assert_eq!(by_tag.total, 3);
    }

    #[test]
    fn summary_counts_one_successful_request_once() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let now = local_now(&store);
        let today = now.format(&Rfc3339).unwrap();
        let yesterday = (now - time::Duration::hours(24)).format(&Rfc3339).unwrap();
        let tags: Vec<String> = Vec::new();

        let mut done = record(&tags, "https://a.example", 10);
        done.at = &today;
        done.pages = 3;
        done.copies = 3;
        store.insert_print_record(&done).unwrap();

        let mut failed = record(&tags, "https://a.example", 20);
        failed.at = &today;
        failed.status = "failed";
        failed.pages = 3;
        failed.copies = 3;
        store.insert_print_record(&failed).unwrap();

        let mut cancelled = record(&tags, "https://a.example", 30);
        cancelled.at = &today;
        cancelled.status = "cancelled";
        cancelled.pages = 3;
        cancelled.copies = 3;
        store.insert_print_record(&cancelled).unwrap();

        let mut old = record(&tags, "https://a.example", 40);
        old.at = &yesterday;
        old.pages = 3;
        old.copies = 3;
        store.insert_print_record(&old).unwrap();

        let summary = store.record_summary(&RecordFilter::default()).unwrap();
        assert_eq!(summary.today_sheets, 27);
        assert_eq!(
            summary.today_done_sheets, 9,
            "deprecated compatibility field"
        );
        assert_eq!(summary.today_done, 1, "one successful request counts once");
        assert_eq!(summary.today_failed, 1);
        assert_eq!(summary.today_cancelled, 1);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.total, 4);
    }
    #[test]
    fn summary_is_zero_on_an_empty_table() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let summary = store.record_summary(&RecordFilter::default()).unwrap();
        assert_eq!(summary.today_sheets, 0);
        assert_eq!(summary.today_done_sheets, 0);
        assert_eq!(summary.today_done, 0);
        assert_eq!(summary.today_failed, 0);
        assert_eq!(summary.today_cancelled, 0);
        assert_eq!(summary.pending, 0);
        assert_eq!(summary.total, 0);
    }

    #[test]
    fn tag_statistics_count_occurrences_and_average_one_record_each() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let first = vec!["a".to_owned(), "b".to_owned()];
        let second = vec!["a".to_owned()];
        store
            .insert_print_record(&record(&first, "https://a.example", 10))
            .unwrap();
        store
            .insert_print_record(&record(&second, "https://a.example", 30))
            .unwrap();
        let buckets = store
            .stats(&StatsQuery {
                group: StatsGroup::Tag,
                filter: RecordFilter::default(),
            })
            .unwrap();
        let a = buckets.iter().find(|bucket| bucket.key == "a").unwrap();
        assert_eq!(a.count, 2);
        assert_eq!(a.sheets, 4);
        assert!((a.avg_ms - 20.0).abs() < f64::EPSILON);
        let b = buckets.iter().find(|bucket| bucket.key == "b").unwrap();
        assert_eq!(b.count, 1);
        assert!((b.avg_ms - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn origin_filter_is_the_only_difference_between_print_and_admin_reads() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let tags = Vec::new();
        store
            .insert_print_record(&record(&tags, "https://a.example", 10))
            .unwrap();
        store
            .insert_print_record(&record(&tags, "https://b.example", 20))
            .unwrap();
        let all = store
            .list_print_records(&RecordFilter {
                limit: 10,
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(all.len(), 2);
        let filtered = store
            .list_print_records(&RecordFilter {
                origin: Some("https://a.example".to_owned()),
                limit: 10,
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].origin, "https://a.example");
        let first = store
            .list_print_records(&RecordFilter {
                limit: 1,
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(first.len(), 1);
        let cursor = first[0].id;
        let second_page = store
            .list_print_records(&RecordFilter {
                cursor: Some(cursor),
                limit: 1,
                ..RecordFilter::default()
            })
            .unwrap();
        assert_eq!(second_page.len(), 1);
        assert!(second_page[0].id < cursor);
    }

    #[test]
    fn cleanup_removes_expired_rows_then_applies_the_row_limit() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let tags = Vec::new();
        let mut old = record(&tags, "https://a.example", 10);
        old.at = "2020-01-01T00:00:00Z";
        old.trace_id = "old";
        store.insert_print_record(&old).unwrap();
        for _ in 0..5 {
            store
                .insert_print_record(&record(&tags, "https://a.example", 10))
                .unwrap();
        }
        let report = store.cleanup_print_records(90, 3).unwrap();
        assert_eq!(report.expired, 1);
        assert_eq!(report.overflow, 2);
        assert_eq!(store.print_record_count().unwrap().count, 3);
    }

    fn bulk_records(store: &Store, rows: i64) {
        let mut connection = store.connection().unwrap();
        let tx = connection.transaction().unwrap();
        tx.execute(
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < ?1)
             INSERT INTO print_record
                 (trace_id, seq, at, origin, template, tags, printer, pages, copies, status, dur_ms, error_code)
             SELECT printf('trace-%d', i), i, '2026-11-01T00:00:00Z',
                    CASE i % 3 WHEN 0 THEN 'https://a.example' WHEN 1 THEN 'https://b.example' ELSE 'https://c.example' END,
                    CASE i % 5 WHEN 0 THEN 'quote' ELSE NULL END,
                    '[]',
                    CASE i % 7 WHEN 0 THEN 'printer-a' WHEN 1 THEN 'printer-b' ELSE NULL END,
                    1, 1,
                    CASE i % 4 WHEN 0 THEN 'failed' WHEN 1 THEN 'cancelled' WHEN 2 THEN 'pending' ELSE 'done' END,
                    10, NULL
             FROM n",
            rusqlite::params![rows],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn query_plan(store: &Store, sql: &str) -> Vec<String> {
        let connection = store.connection().unwrap();
        let mut statement = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    }

    #[test]
    fn upgrading_a_v10_database_adds_the_origin_status_id_index() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        let connection = Connection::open(&path).unwrap();
        for (index, sql) in MIGRATIONS.iter().take(10).enumerate() {
            connection.execute_batch(sql).unwrap();
            connection
                .pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        drop(connection);
        let store = open(temp.path());
        let connection = store.connection().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
        let indexes: Vec<String> = connection
            .prepare("PRAGMA index_list(print_record)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(indexes.contains(&"print_record_origin_status_id".to_owned()));
        assert!(indexes.contains(&"print_record_origin_status_printer_id".to_owned()));
    }

    #[test]
    fn upgrading_a_v9_database_adds_the_records_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DB_FILE);
        let connection = Connection::open(&path).unwrap();
        for (index, sql) in MIGRATIONS.iter().take(9).enumerate() {
            connection.execute_batch(sql).unwrap();
            connection
                .pragma_update(None, "user_version", index as i64 + 1)
                .unwrap();
        }
        drop(connection);
        let store = open(temp.path());
        let connection = store.connection().unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
        let indexes: Vec<String> = connection
            .prepare("PRAGMA index_list(print_record)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(indexes.contains(&"print_record_origin_status_printer_id".to_owned()));
        assert!(indexes.contains(&"print_record_status_printer_id".to_owned()));
        assert!(indexes.contains(&"print_record_printer_id".to_owned()));
        assert!(indexes.contains(&"print_record_at_julianday".to_owned()));
        assert!(!indexes.contains(&"print_record_origin".to_owned()));
    }

    #[test]
    fn records_csv_is_ordered_bounded_and_keeps_5000_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        bulk_records(&store, 5_000);
        let csv = store.records_csv(&RecordFilter::default()).unwrap();
        assert!(csv.starts_with("\u{feff}at,origin,template,tags,printer,pages,copies,status,dur_ms,error_code,trace_id\n"));
        assert_eq!(csv.lines().count(), 5_001, "header plus every retained row");
        let first = csv.lines().nth(1).unwrap();
        let last = csv.lines().last().unwrap();
        assert!(first.ends_with("trace-1"), "ascending order: {first}");
        assert!(last.ends_with("trace-5000"), "ascending order: {last}");
        assert!(csv.contains("printer-a"));
        assert!(csv.contains("cancelled"));
    }

    #[test]
    fn records_csv_is_bounded_to_the_newest_5000_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        bulk_records(&store, 5_001);
        let csv = store.records_csv(&RecordFilter::default()).unwrap();
        assert_eq!(csv.lines().count(), 5_001, "header plus 5000 rows");
        let first = csv.lines().nth(1).unwrap();
        let last = csv.lines().last().unwrap();
        assert!(
            first.ends_with("trace-2"),
            "the oldest row is dropped: {first}"
        );
        assert!(
            last.ends_with("trace-5001"),
            "the newest row is kept: {last}"
        );
    }

    #[test]
    fn records_filters_use_the_migration_indexes() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        bulk_records(&store, 50_000);

        let origin_status = query_plan(
            &store,
            "SELECT id FROM print_record WHERE origin = 'https://a.example' AND status = 'done' ORDER BY id DESC LIMIT 500",
        );
        assert!(
            origin_status
                .iter()
                .any(|detail| detail.contains("print_record_origin_status_id")),
            "the origin+status query must use the covering index: {origin_status:?}"
        );
        assert!(
            !origin_status
                .iter()
                .any(|detail| detail.contains("TEMP B-TREE")),
            "origin+status must not sort in a temporary tree: {origin_status:?}"
        );
        let status_printer = query_plan(
            &store,
            "SELECT id FROM print_record WHERE status = 'done' AND coalesce(printer, 'default') = 'printer-a' ORDER BY id DESC LIMIT 500",
        );
        assert!(
            status_printer
                .iter()
                .any(|detail| detail.contains("print_record_status_printer_id"))
                || status_printer
                    .iter()
                    .any(|detail| detail.contains("print_record_printer_id")),
            "{status_printer:?}"
        );
        let at_range = query_plan(
            &store,
            "SELECT count(*) FROM print_record WHERE julianday(at) >= julianday('2026-11-01') AND julianday(at) < julianday('2026-11-02')",
        );
        assert!(
            at_range
                .iter()
                .any(|detail| detail.contains("print_record_at_julianday")),
            "{at_range:?}"
        );

        let started = std::time::Instant::now();
        let page = store
            .list_print_records(&RecordFilter {
                origin: Some("https://a.example".to_owned()),
                status: Some("done".to_owned()),
                limit: 500,
                ..RecordFilter::default()
            })
            .unwrap();
        let summary = store.record_summary(&RecordFilter::default()).unwrap();
        let stats = store
            .stats(&StatsQuery {
                filter: RecordFilter::default(),
                group: StatsGroup::Printer,
            })
            .unwrap();
        assert!(!page.is_empty());
        assert!(summary.total > 0);
        assert!(!stats.is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "50k-row queries took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn csv_has_the_contract_columns_and_a_bom() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let tags = vec!["a".to_owned(), "b".to_owned()];
        store
            .insert_print_record(&record(&tags, "https://a.example", 10))
            .unwrap();
        let csv = store.records_csv(&RecordFilter::default()).unwrap();
        assert!(csv.starts_with('\u{feff}'));
        assert!(csv.contains(
            "at,origin,template,tags,printer,pages,copies,status,dur_ms,error_code,trace_id"
        ));
        assert!(csv.contains("a;b"));
    }

    #[test]
    fn csv_formula_prefixes_are_neutralized() {
        for (input, expected) in [
            ("=cmd", "'=cmd"),
            ("+1", "'+1"),
            ("-2", "'-2"),
            ("@SUM", "'@SUM"),
            ("\tcmd", "'\tcmd"),
            ("\rcmd", "\"'\rcmd\""),
            ("=cmd,next", "\"'=cmd,next\""),
        ] {
            assert_eq!(csv_field(input), expected, "{input:?}");
        }
    }

    #[test]
    fn records_csv_neutralizes_formula_fields() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        let tags = vec!["\tcmd".to_owned()];
        let mut row = record(&tags, "=cmd", 10);
        row.template = Some("+1");
        row.printer = Some("-2");
        row.error_code = Some("@SUM");
        store.insert_print_record(&row).unwrap();
        let csv = store.records_csv(&RecordFilter::default()).unwrap();
        for field in [",'=cmd,", ",'+1,", ",'-2,", ",'\tcmd,", ",'@SUM,"] {
            assert!(csv.contains(field), "missing {field:?} in {csv:?}");
        }
        assert!(csv.contains("=cmd") && csv.contains("+1") && csv.contains("-2"));
        assert!(csv.contains("@SUM") && csv.contains("\tcmd"));
    }

    #[test]
    fn corrupt_origin_setting_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let store = open(temp.path());
        store.set_setting(ORIGINS_KEY, "not-json").unwrap();
        let error = store.origins().unwrap_err();
        assert!(matches!(error, StoreError::SettingJson { .. }));
    }
}
