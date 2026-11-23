//! `plugins.toml`: parsing, atomic replacement and the host lock.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use printkit_plugin_api::PluginId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// File name inside the data directory.
pub const PLUGINS_FILE: &str = "plugins.toml";
/// Backup written before every save.
pub const PLUGINS_BACKUP: &str = "plugins.toml.bak";
/// One-time pre-core backup written by the D2 migration.
pub const PLUGINS_CORE_BACKUP: &str = "plugins.toml.bak-core";
/// Exclusive lock held by the service for its whole lifetime.
pub const PLUGINS_LOCK: &str = "plugins.lock";
/// Defaults version this build settles a file to.
pub const CURRENT_SETTLED: u32 = 2;

/// Which built-in plugins are on when a plugin has no row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    /// Every built-in plugin is on.
    All,
    /// No optional plugin is on; only the core keeps running.
    #[default]
    Core,
}

impl Preset {
    /// Whether a plugin without an explicit row starts enabled.
    pub fn enables_without_row(self) -> bool {
        matches!(self, Self::All)
    }
}

impl FromStr for Preset {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all" => Ok(Self::All),
            "core" => Ok(Self::Core),
            _ => Err(()),
        }
    }
}

/// One explicit user choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRow {
    /// Plugin id.
    pub id: PluginId,
    /// Whether the user enabled it.
    pub enabled: bool,
    /// External package version, when installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `user` or `bundled`, for external packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Tree hash of an installed external package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    /// Plugin configuration, validated against its schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<toml::Value>,
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsFile {
    /// File format version; only `1` is understood.
    #[serde(default = "file_version")]
    pub version: u32,
    /// The defaults version the file was last settled to.
    #[serde(default = "settled_version")]
    pub settled: u32,
    /// Default for plugins without a row.
    #[serde(default)]
    pub preset: Preset,
    /// Explicit rows, in file order.
    #[serde(default)]
    pub plugins: Vec<PluginRow>,
}

fn file_version() -> u32 {
    1
}

fn settled_version() -> u32 {
    0
}

impl Default for PluginsFile {
    fn default() -> Self {
        Self {
            version: 1,
            settled: CURRENT_SETTLED,
            preset: Preset::Core,
            plugins: Vec::new(),
        }
    }
}

impl PluginsFile {
    /// Empty file with the `core` preset.
    pub fn core() -> Self {
        Self {
            preset: Preset::Core,
            ..Self::default()
        }
    }

    /// The explicit row for one plugin.
    pub fn row(&self, id: &str) -> Option<&PluginRow> {
        self.plugins.iter().find(|row| row.id.as_str() == id)
    }

    /// The chosen value for a plugin before dependency resolution.
    pub fn chosen(&self, id: &str) -> bool {
        self.row(id)
            .map(|row| row.enabled)
            .unwrap_or_else(|| self.preset.enables_without_row())
    }

    /// Whether a user row exists for this plugin.
    pub fn has_row(&self, id: &str) -> bool {
        self.row(id).is_some()
    }
}

/// What the host loaded, including a broken file's diagnosis.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedFile {
    /// Parsed file, or a core-only fallback.
    pub file: PluginsFile,
    /// Why the main file could not be used; `None` when it parsed.
    pub broken: Option<String>,
    /// Whether `plugins.toml` existed at all.
    pub exists: bool,
}

impl LoadedFile {
    /// Resolve this file with one consistent broken-file rule.
    ///
    /// When the main file is broken, every consumer (service, CLI list,
    /// enable/disable) sees core-only, even if a readable `.bak` supplied the
    /// diagnosed content.
    pub fn registry(&self, specs: &[crate::builtins::BuiltinSpec]) -> crate::registry::Registry {
        match &self.broken {
            Some(reason) => crate::registry::Registry::broken(specs, reason.clone()),
            None => crate::registry::Registry::resolve(&self.file, specs),
        }
    }
}

/// Why `plugins.toml` cannot be read or written.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileError {
    /// Reading the data directory failed.
    #[error("cannot read {}: {detail}", path.display())]
    Read {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        detail: String,
    },
    /// Writing or replacing the file failed.
    #[error("cannot write {}: {detail}", path.display())]
    Write {
        /// File that failed.
        path: PathBuf,
        /// Underlying error.
        detail: String,
    },
    /// The host's exclusive lock is already held.
    #[error("the plugin file is locked by another process")]
    Locked,
}

fn read_file(path: &Path) -> Result<String, FileError> {
    fs::read_to_string(path).map_err(|source| FileError::Read {
        path: path.to_path_buf(),
        detail: source.to_string(),
    })
}

fn parse(text: &str) -> Result<PluginsFile, String> {
    let file: PluginsFile = toml::from_str(text).map_err(|error| error.to_string())?;
    if file.version != 1 {
        return Err(format!(
            "file version {} is newer than this build (1)",
            file.version
        ));
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in &file.plugins {
        if !ids.insert(row.id.as_str()) {
            return Err(format!("duplicate plugin row: {}", row.id));
        }
    }
    Ok(file)
}

/// Load `plugins.toml`, falling back to the backup and then to core-only.
///
/// A broken main file never stops the core: the caller starts with the `core`
/// preset and refuses writes until an operator repairs the file.
pub fn load(dir: &Path) -> LoadedFile {
    let path = dir.join(PLUGINS_FILE);
    let text = match read_file(&path) {
        Ok(text) => text,
        Err(error) => {
            if !path.exists() {
                return LoadedFile {
                    file: PluginsFile::core(),
                    broken: None,
                    exists: false,
                };
            }
            return LoadedFile {
                file: PluginsFile::core(),
                broken: Some(error.to_string()),
                exists: true,
            };
        }
    };
    match parse(&text) {
        Ok(mut file) => {
            if file.settled < CURRENT_SETTLED {
                file.preset = Preset::Core;
                file.settled = CURRENT_SETTLED;
                for row in &mut file.plugins {
                    row.enabled = false;
                }
            }
            LoadedFile {
                file,
                broken: None,
                exists: true,
            }
        }
        Err(reason) => {
            let backup = dir.join(PLUGINS_BACKUP);
            if let Ok(text) = read_file(&backup)
                && let Ok(file) = parse(&text)
            {
                return LoadedFile {
                    file,
                    broken: Some(format!(
                        "{PLUGINS_FILE} is broken ({reason}); using {PLUGINS_BACKUP}"
                    )),
                    exists: true,
                };
            }
            LoadedFile {
                file: PluginsFile::core(),
                broken: Some(format!("{PLUGINS_FILE} is broken: {reason}")),
                exists: true,
            }
        }
    }
}

/// D2-B: migrate a pre-core file to `preset = "core"`, settled 2, rows off.
///
/// A backup is written before the rewrite; an existing `.bak-core` is never
/// overwritten. A broken or missing file is left alone: the caller already
/// falls back to core-only and refuses writes.
pub fn migrate_core(dir: &Path) -> Result<bool, FileError> {
    let path = dir.join(PLUGINS_FILE);
    let text = match read_file(&path) {
        Ok(text) => text,
        Err(error) => {
            if !path.exists() {
                return Ok(false);
            }
            return Err(error);
        }
    };
    let mut file = match parse(&text) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    if file.settled >= CURRENT_SETTLED {
        return Ok(false);
    }
    let core_backup = dir.join(PLUGINS_CORE_BACKUP);
    if !core_backup.exists() {
        fs::copy(&path, &core_backup).map_err(|error| write_error(&core_backup, error))?;
    }
    file.preset = Preset::Core;
    file.settled = CURRENT_SETTLED;
    for row in &mut file.plugins {
        row.enabled = false;
    }
    save(dir, &file)?;
    Ok(true)
}

fn write_error(path: &Path, source: impl std::fmt::Display) -> FileError {
    FileError::Write {
        path: path.to_path_buf(),
        detail: source.to_string(),
    }
}

/// Replace `plugins.toml` atomically and keep one previous copy.
pub fn save(dir: &Path, file: &PluginsFile) -> Result<(), FileError> {
    if file.version != 1 {
        return Err(write_error(
            &dir.join(PLUGINS_FILE),
            "the in-memory file version is not 1",
        ));
    }
    fs::create_dir_all(dir).map_err(|error| write_error(dir, error))?;
    let target = dir.join(PLUGINS_FILE);
    let backup = dir.join(PLUGINS_BACKUP);
    if target.exists() {
        fs::copy(&target, &backup).map_err(|error| write_error(&backup, error))?;
    }
    let text = toml::to_string_pretty(file).map_err(|error| write_error(&target, error))?;
    let temporary = dir.join(format!("{PLUGINS_FILE}.tmp"));
    {
        let mut handle = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| write_error(&temporary, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = handle.set_permissions(fs::Permissions::from_mode(0o600));
        }
        handle
            .write_all(text.as_bytes())
            .map_err(|error| write_error(&temporary, error))?;
        handle
            .sync_all()
            .map_err(|error| write_error(&temporary, error))?;
    }
    fs::rename(&temporary, &target).map_err(|error| write_error(&target, error))?;
    #[cfg(unix)]
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// An exclusive hold on `plugins.lock`.
///
/// The service keeps one for its lifetime; the CLI only tries and falls back to
/// the administrator API when the lock is busy.
#[derive(Debug)]
pub struct PluginLock {
    file: File,
    path: PathBuf,
}

impl PluginLock {
    /// Block until this process owns the lock.
    pub fn acquire(dir: &Path) -> Result<Self, FileError> {
        fs::create_dir_all(dir).map_err(|error| write_error(dir, error))?;
        let path = dir.join(PLUGINS_LOCK);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| write_error(&path, error))?;
        fs2::FileExt::lock_exclusive(&file).map_err(|error| FileError::Write {
            path: path.clone(),
            detail: error.to_string(),
        })?;
        Ok(Self { file, path })
    }

    /// Try once; `Ok(None)` means another process holds the lock.
    pub fn try_acquire(dir: &Path) -> Result<Option<Self>, FileError> {
        fs::create_dir_all(dir).map_err(|error| write_error(dir, error))?;
        let path = dir.join(PLUGINS_LOCK);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| write_error(&path, error))?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => Ok(Some(Self { file, path })),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(error) => Err(FileError::Write {
                path,
                detail: error.to_string(),
            }),
        }
    }

    /// The locked path, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PluginLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PluginsFile {
        PluginsFile {
            version: 1,
            settled: CURRENT_SETTLED,
            preset: Preset::All,
            plugins: vec![PluginRow {
                id: PluginId::new("observability").unwrap(),
                enabled: false,
                version: None,
                source: None,
                integrity: None,
                config: None,
            }],
        }
    }

    #[test]
    fn a_missing_file_is_the_core_preset() {
        let temp = tempfile::tempdir().unwrap();
        let loaded = load(temp.path());
        assert!(!loaded.exists);
        assert!(loaded.broken.is_none());
        assert_eq!(loaded.file.preset, Preset::Core);
        assert!(!loaded.file.chosen("observability"));
    }

    #[test]
    fn a_file_round_trips() {
        let temp = tempfile::tempdir().unwrap();
        save(temp.path(), &sample()).unwrap();
        let loaded = load(temp.path());
        assert!(loaded.broken.is_none(), "{loaded:?}");
        assert_eq!(loaded.file, sample());
        assert!(!loaded.file.chosen("observability"));
        assert!(loaded.file.chosen("records"));
    }

    #[test]
    fn a_broken_main_file_uses_the_backup_and_starts_core_only() {
        let temp = tempfile::tempdir().unwrap();
        save(temp.path(), &sample()).unwrap();
        fs::copy(
            temp.path().join(PLUGINS_FILE),
            temp.path().join(PLUGINS_BACKUP),
        )
        .unwrap();
        fs::write(temp.path().join(PLUGINS_FILE), "not toml").unwrap();
        let loaded = load(temp.path());
        assert!(loaded.broken.is_some());
        assert_eq!(loaded.file, sample(), "the backup was not used");
        let core_only = loaded.registry(crate::builtins::builtins());
        assert!(!core_only.active.active("records"));
    }

    #[test]
    fn without_a_backup_a_broken_file_is_core_only() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(PLUGINS_FILE), "version = 9").unwrap();
        let loaded = load(temp.path());
        assert!(loaded.broken.is_some());
        assert_eq!(loaded.file.preset, Preset::Core);
        assert!(!loaded.file.chosen("observability"));
    }

    #[test]
    fn the_lock_is_exclusive() {
        let temp = tempfile::tempdir().unwrap();
        let held = PluginLock::try_acquire(temp.path()).unwrap().unwrap();
        assert!(PluginLock::try_acquire(temp.path()).unwrap().is_none());
        drop(held);
        assert!(PluginLock::try_acquire(temp.path()).unwrap().is_some());
    }

    #[test]
    fn a_pre_core_file_is_backed_up_and_settled_to_core() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(PLUGINS_FILE);
        fs::write(
            &path,
            "version = 1\nsettled = 1\npreset = \"all\"\n[[plugins]]\nid = \"observability\"\nenabled = true\n",
        )
        .unwrap();
        assert!(migrate_core(temp.path()).unwrap());
        let core_backup = fs::read_to_string(temp.path().join(PLUGINS_CORE_BACKUP)).unwrap();
        assert!(core_backup.contains("preset = \"all\""), "{core_backup}");
        let loaded = load(temp.path());
        assert_eq!(loaded.file.preset, Preset::Core);
        assert_eq!(loaded.file.settled, CURRENT_SETTLED);
        assert!(!loaded.file.chosen("observability"));
        // A second run is a no-op and never rewrites the core backup.
        assert!(!migrate_core(temp.path()).unwrap());
        assert_eq!(
            fs::read_to_string(temp.path().join(PLUGINS_CORE_BACKUP)).unwrap(),
            core_backup
        );
    }

    #[test]
    fn a_broken_file_is_not_migrated() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(PLUGINS_FILE), "version = 2").unwrap();
        assert!(!migrate_core(temp.path()).unwrap());
        assert!(!temp.path().join(PLUGINS_CORE_BACKUP).exists());
        assert_eq!(load(temp.path()).file.preset, Preset::Core);
    }

    #[test]
    fn writes_leave_a_backup_of_the_previous_file() {
        let temp = tempfile::tempdir().unwrap();
        save(temp.path(), &PluginsFile::default()).unwrap();
        let mut next = sample();
        next.preset = Preset::Core;
        save(temp.path(), &next).unwrap();
        let backup = fs::read_to_string(temp.path().join(PLUGINS_BACKUP)).unwrap();
        assert!(backup.contains("preset = \"core\""), "{backup}");
    }
}
