use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use breakd_scheduler::Snapshot;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StateStoreError {
    #[error("failed to access {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Snapshot>, StateStoreError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.path).map_err(|source| StateStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| StateStoreError::Parse {
                path: self.path.clone(),
                source,
            })
    }

    pub fn save(&self, snapshot: &Snapshot) -> Result<(), StateStoreError> {
        let parent = self.path.parent().ok_or_else(|| StateStoreError::Io {
            path: self.path.clone(),
            source: io::Error::new(io::ErrorKind::InvalidInput, "state path has no parent"),
        })?;
        fs::create_dir_all(parent).map_err(|source| StateStoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|source| {
            StateStoreError::Io {
                path: parent.to_path_buf(),
                source,
            }
        })?;

        let temporary = self.path.with_extension("json.tmp");
        let bytes =
            serde_json::to_vec_pretty(snapshot).expect("snapshot serialization is infallible");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|source| StateStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.write_all(&bytes)
            .map_err(|source| StateStoreError::Io {
                path: temporary.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| StateStoreError::Io {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &self.path).map_err(|source| StateStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        OpenOptions::new()
            .read(true)
            .open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| StateStoreError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        Ok(())
    }

    pub fn quarantine_corrupt(&self) -> Result<Option<PathBuf>, StateStoreError> {
        if !self.path.exists() {
            return Ok(None);
        }
        let quarantined = self.path.with_extension("json.corrupt");
        fs::rename(&self.path, &quarantined).map_err(|source| StateStoreError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(Some(quarantined))
    }
}

#[cfg(test)]
mod tests {
    use breakd_config::defaults;
    use breakd_scheduler::Scheduler;

    use super::*;

    #[test]
    fn snapshot_round_trip_is_atomic_and_private() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state/state.json");
        let store = StateStore::new(&path);
        let now = breakd_core::ClockSample {
            monotonic_ms: 1,
            boottime_ms: 1,
            wall_unix_ms: 1,
        };
        let scheduler = Scheduler::new(defaults(), "boot".into(), now, "socket".into());
        store.save(&scheduler.snapshot()).unwrap();
        assert_eq!(store.load().unwrap(), Some(scheduler.snapshot()));
        let mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
