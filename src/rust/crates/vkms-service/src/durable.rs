//! A JSON file written atomically, with the previous copy kept as a backup.
//!
//! A half-written trust list after a power cut would leave a KVM whose paired machines are
//! unreadable -- and you cannot use that KVM to go and fix it, because reaching the other machines is
//! the thing it does. So the failure modes are explicit: fall back to the backup, and if both are
//! gone say so rather than silently presenting an empty list, which would look exactly like never
//! having paired anything.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Why a load did not simply return the primary file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoadOutcome {
    Loaded,

    /// No file yet. A first run.
    Absent,

    /// The primary was unreadable and the backup was used instead.
    RecoveredFromBackup,

    /// Both copies were unreadable.
    Unrecoverable,
}

#[derive(Debug)]
pub struct LoadResult<T> {
    pub outcome: LoadOutcome,
    pub value: Option<T>,
    pub error: Option<String>,
}

pub struct DurableFile {
    path: PathBuf,
    gate: Mutex<()>,
}

impl DurableFile {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), gate: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn backup_path(&self) -> PathBuf {
        with_suffix(&self.path, ".bak")
    }

    fn temp_path(&self) -> PathBuf {
        with_suffix(&self.path, ".tmp")
    }

    pub fn load<T: DeserializeOwned>(&self) -> LoadResult<T> {
        let _gate = self.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if !self.path.exists() && !self.backup_path().exists() {
            return LoadResult { outcome: LoadOutcome::Absent, value: None, error: None };
        }

        let primary_error = match read(&self.path) {
            Ok(value) => return LoadResult { outcome: LoadOutcome::Loaded, value: Some(value), error: None },
            Err(error) => error,
        };

        // The primary is damaged, which is exactly what the backup exists for.
        match read(&self.backup_path()) {
            Ok(value) => LoadResult { outcome: LoadOutcome::RecoveredFromBackup, value: Some(value), error: Some(primary_error) },
            Err(_) => LoadResult { outcome: LoadOutcome::Unrecoverable, value: None, error: Some(primary_error) },
        }
    }

    /// Writes `value`, replacing the previous copy and keeping it as the backup.
    pub fn save<T: Serialize>(&self, value: &T) -> io::Result<()> {
        let _gate = self.gate.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(directory) = self.path.parent() {
            fs::create_dir_all(directory)?;
        }

        // Written in full to a temporary file first, so a crash mid-write damages only that.
        let temporary = self.temp_path();
        fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;

        if self.path.exists() {
            // Atomic, and rotates the previous copy into the backup in the same step.
            vkms_win::files::replace_file(&self.path, &temporary, &self.backup_path())?;
        } else {
            fs::rename(&temporary, &self.path)?;

            // The first write gets a backup too, rather than waiting for the second. Otherwise a
            // machine that has just paired holds exactly one copy of the trust it depends on.
            fs::copy(&self.path, self.backup_path())?;
        }

        Ok(())
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn read<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;

    // A zero-length file is what an interrupted write leaves behind, and must not parse as "empty".
    if bytes.is_empty() {
        return Err("file is empty".into());
    }

    serde_json::from_slice(&bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Doc {
        name: String,
    }

    fn doc(name: &str) -> Doc {
        Doc { name: name.into() }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("vkms-durable-{}", uuid::Uuid::new_v4().simple()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> DurableFile {
            DurableFile::new(self.0.join("doc.json"))
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_missing_file_is_reported_as_absent_rather_than_empty() {
        let dir = TempDir::new();
        let result = dir.file().load::<Doc>();
        assert_eq!(LoadOutcome::Absent, result.outcome);
        assert!(result.value.is_none());
    }

    #[test]
    fn saving_keeps_the_previous_copy_as_a_backup() {
        let dir = TempDir::new();
        let file = dir.file();
        file.save(&doc("first")).unwrap();
        file.save(&doc("second")).unwrap();

        assert!(file.backup_path().exists());
        assert_eq!(Some(doc("second")), file.load().value);
    }

    #[test]
    fn the_very_first_save_also_leaves_a_backup() {
        let dir = TempDir::new();
        let file = dir.file();
        file.save(&doc("first")).unwrap();
        fs::write(file.path(), "{ corrupt").unwrap();

        let result = file.load::<Doc>();
        assert_eq!(LoadOutcome::RecoveredFromBackup, result.outcome);
        assert_eq!(Some(doc("first")), result.value);
    }

    #[test]
    fn a_corrupt_primary_falls_back_to_the_backup() {
        let dir = TempDir::new();
        let file = dir.file();
        file.save(&doc("original")).unwrap();
        file.save(&doc("current")).unwrap();
        fs::write(file.path(), "{ this is not json").unwrap();

        let result = file.load::<Doc>();
        assert_eq!(LoadOutcome::RecoveredFromBackup, result.outcome);
        assert_eq!(Some(doc("original")), result.value);
        assert!(result.error.is_some());
    }

    #[test]
    fn an_empty_primary_is_treated_as_corrupt() {
        let dir = TempDir::new();
        let file = dir.file();
        file.save(&doc("original")).unwrap();
        file.save(&doc("current")).unwrap();
        fs::write(file.path(), []).unwrap();

        assert_eq!(LoadOutcome::RecoveredFromBackup, file.load::<Doc>().outcome);
    }

    #[test]
    fn both_copies_unreadable_is_reported_rather_than_silently_empty() {
        let dir = TempDir::new();
        let file = dir.file();
        file.save(&doc("first")).unwrap();
        file.save(&doc("second")).unwrap();
        fs::write(file.path(), "corrupt").unwrap();
        fs::write(file.backup_path(), "also corrupt").unwrap();

        let result = file.load::<Doc>();
        assert_eq!(LoadOutcome::Unrecoverable, result.outcome);
        assert!(result.value.is_none());
        assert!(result.error.is_some());
    }

    #[test]
    fn no_temporary_file_is_left_behind() {
        let dir = TempDir::new();
        let file = dir.file();
        file.save(&doc("a")).unwrap();
        file.save(&doc("b")).unwrap();

        let leftovers = fs::read_dir(&dir.0).unwrap().filter_map(Result::ok).filter(|entry| {
            entry.path().extension().is_some_and(|extension| extension == "tmp")
        });
        assert_eq!(0, leftovers.count());
    }
}
