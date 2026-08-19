use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use atomic_write_file::AtomicWriteFile;
use fs4::TryLockError;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::ChildSnapshot;

const SCHEMA_VERSION: u32 = 1;
const MAX_STORE_BYTES: usize = 16 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct SubagentStore {
    root: PathBuf,
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invalid subagent root id")]
    InvalidId,
    #[error("subagent state is corrupt: {0}")]
    Corrupt(String),
    #[error("subagent state is unavailable: {0}")]
    Unavailable(String),
}

#[derive(Deserialize, Serialize)]
struct SnapshotFile {
    schema_version: u32,
    root_id: String,
    children: Vec<ChildSnapshot>,
}

impl SubagentStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub(crate) fn load(&self, root_id: &str) -> Result<Vec<ChildSnapshot>, StoreError> {
        validate_id(root_id)?;
        ensure_private_directory(&self.root)?;
        let _lock = lock_store(&self.root)?;
        let path = self.root.join(format!("{root_id}.json"));
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unavailable(&path, error)),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || usize::try_from(metadata.len()).unwrap_or(usize::MAX) > MAX_STORE_BYTES
        {
            return Err(StoreError::Corrupt(format!(
                "unsafe or oversized state file {}",
                path.display()
            )));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
        File::open(&path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|error| unavailable(&path, error))?;
        let snapshot: SnapshotFile = serde_json::from_slice(&bytes)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        if snapshot.schema_version != SCHEMA_VERSION || snapshot.root_id != root_id {
            return Err(StoreError::Corrupt("snapshot identity mismatch".into()));
        }
        Ok(snapshot.children)
    }

    pub(crate) fn save(
        &self,
        root_id: &str,
        children: Vec<ChildSnapshot>,
    ) -> Result<(), StoreError> {
        validate_id(root_id)?;
        ensure_private_directory(&self.root)?;
        let _lock = lock_store(&self.root)?;
        let path = self.root.join(format!("{root_id}.json"));
        reject_symlink_or_nonfile(&path)?;
        let bytes = serde_json::to_vec(&SnapshotFile {
            schema_version: SCHEMA_VERSION,
            root_id: root_id.to_owned(),
            children,
        })
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        if bytes.is_empty() || bytes.len() > MAX_STORE_BYTES {
            return Err(StoreError::Corrupt("snapshot exceeds 16 MiB".into()));
        }
        let mut stage = AtomicWriteFile::open(&path).map_err(|error| unavailable(&path, error))?;
        set_private_file(stage.as_file(), &path)?;
        stage
            .write_all(&bytes)
            .map_err(|error| unavailable(&path, error))?;
        stage.commit().map_err(|error| unavailable(&path, error))
    }
}

fn validate_id(id: &str) -> Result<(), StoreError> {
    if id.is_empty()
        || id.len() > 255
        || matches!(id, "." | "..")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Err(StoreError::InvalidId)
    } else {
        Ok(())
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(StoreError::Corrupt(format!(
                "unsafe directory {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|error| unavailable(path, error))?;
        }
        Err(error) => return Err(unavailable(path, error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| unavailable(path, error))?;
    }
    Ok(())
}

fn lock_store(root: &Path) -> Result<File, StoreError> {
    let path = root.join("subagent-control.lock");
    reject_symlink_or_nonfile(&path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&path)
        .map_err(|error| unavailable(&path, error))?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(StoreError::Unavailable("timed out waiting for lock".into()));
            }
            Err(TryLockError::Error(error)) => return Err(unavailable(&path, error)),
        }
    }
}

fn reject_symlink_or_nonfile(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            StoreError::Corrupt(format!("unsafe file {}", path.display())),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(unavailable(path, error)),
    }
}

fn set_private_file(file: &File, path: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| unavailable(path, error))?;
    }
    Ok(())
}

fn unavailable(path: &Path, error: io::Error) -> StoreError {
    StoreError::Unavailable(format!("{}: {error}", path.display()))
}
