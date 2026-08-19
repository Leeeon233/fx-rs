//! Provider-neutral, file-backed credential storage.
//!
//! Each provider gets one independently locked file under
//! `~/.fx/credentials`. Providers own the meaning and refresh lifecycle of the
//! credential; this crate owns only safe persistence and process coordination.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use atomic_write_file::AtomicWriteFile;
use fs4::TryLockError;
use fx_provider::{Credential, CredentialLease, CredentialStore, ProviderError};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const FORMAT_VERSION: u32 = 1;
const MAX_CREDENTIAL_BYTES: u64 = 64 * 1024;
const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct FileCredentialStore {
    root: PathBuf,
    lock_timeout: Duration,
}

impl FileCredentialStore {
    pub fn from_home(home: impl AsRef<Path>) -> Self {
        Self::new(home.as_ref().join(".fx").join("credentials"))
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            lock_timeout: DEFAULT_LOCK_TIMEOUT,
        }
    }

    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn acquire(&self, provider_id: &str) -> Result<FileCredentialLease<'_>, ProviderError> {
        validate_provider_id(provider_id)?;
        ensure_private_directory(&self.root)?;
        let locks = self.root.join(".locks");
        ensure_private_directory(&locks)?;
        let lock_path = locks.join(format!("{provider_id}.lock"));
        let lock = open_private_lock(&lock_path)?;
        let deadline = Instant::now() + self.lock_timeout;
        loop {
            match fs4::FileExt::try_lock(&lock) {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryLockError::WouldBlock) => {
                    return Err(ProviderError::CredentialStore(format!(
                        "timed out locking credential for `{provider_id}`"
                    )));
                }
                Err(TryLockError::Error(error)) => {
                    return Err(store_io("locking credential", error));
                }
            }
        }
        let path = self.root.join(format!("{provider_id}.json"));
        let credential = read_credential(&path)?;
        Ok(FileCredentialLease {
            _store: self,
            _lock: lock,
            path,
            credential,
        })
    }
}

impl CredentialStore for FileCredentialStore {
    fn lock<'a>(
        &'a self,
        provider_id: &str,
    ) -> Result<Box<dyn CredentialLease + 'a>, ProviderError> {
        Ok(Box::new(self.acquire(provider_id)?))
    }
}

struct FileCredentialLease<'a> {
    _store: &'a FileCredentialStore,
    _lock: File,
    path: PathBuf,
    credential: Option<Credential>,
}

impl CredentialLease for FileCredentialLease<'_> {
    fn credential(&self) -> Option<&Credential> {
        self.credential.as_ref()
    }

    fn replace(&mut self, credential: Credential) -> Result<(), ProviderError> {
        write_credential(&self.path, &credential)?;
        self.credential = Some(credential);
        Ok(())
    }

    fn delete(&mut self) -> Result<(), ProviderError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ProviderError::CredentialStore(format!(
                    "credential path `{}` is not a regular file",
                    self.path.display()
                )));
            }
            Ok(_) => fs::remove_file(&self.path)
                .map_err(|error| store_io("deleting credential", error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(store_io("inspecting credential", error)),
        }
        self.credential = None;
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredCredential {
    version: u32,
    credential: Credential,
}

fn read_credential(path: &Path) -> Result<Option<Credential>, ProviderError> {
    let file = match open_read_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(store_io("opening credential", error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| store_io("inspecting credential", error))?;
    if !metadata.is_file() || !private_file_permissions(&metadata) {
        return Err(ProviderError::CredentialStore(format!(
            "credential `{}` is not a private regular file",
            path.display()
        )));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| store_io("reading credential", error))?;
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(ProviderError::CredentialStore(format!(
            "credential `{}` exceeds 64 KiB",
            path.display()
        )));
    }
    let stored: StoredCredential = serde_json::from_slice(&bytes).map_err(|_| {
        ProviderError::CredentialStore(format!(
            "credential `{}` is corrupt or has an unsupported schema",
            path.display()
        ))
    })?;
    if stored.version != FORMAT_VERSION {
        return Err(ProviderError::CredentialStore(format!(
            "credential `{}` uses unsupported version {}",
            path.display(),
            stored.version
        )));
    }
    Ok(Some(stored.credential))
}

fn write_credential(path: &Path, credential: &Credential) -> Result<(), ProviderError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProviderError::CredentialStore(format!(
            "credential `{}` is a symbolic link",
            path.display()
        )));
    }
    let stored = StoredCredential {
        version: FORMAT_VERSION,
        credential: credential.clone(),
    };
    let mut bytes = Zeroizing::new(
        serde_json::to_vec(&stored)
            .map_err(|error| ProviderError::CredentialStore(error.to_string()))?,
    );
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(ProviderError::CredentialStore(
            "serialized credential exceeds 64 KiB".into(),
        ));
    }
    let mut stage =
        AtomicWriteFile::open(path).map_err(|error| store_io("staging credential", error))?;
    set_private_file(stage.as_file())?;
    stage
        .write_all(&bytes)
        .map_err(|error| store_io("writing credential", error))?;
    stage
        .commit()
        .map_err(|error| store_io("committing credential", error))
}

fn validate_provider_id(value: &str) -> Result<(), ProviderError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(ProviderError::CredentialStore(format!(
            "unsafe provider id `{value}`"
        )))
    }
}

fn ensure_private_directory(path: &Path) -> Result<(), ProviderError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ProviderError::CredentialStore(format!(
                "credential directory `{}` is not a directory",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|error| store_io("creating credential directory", error))?;
        }
        Err(error) => return Err(store_io("inspecting credential directory", error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| store_io("securing credential directory", error))?;
    }
    Ok(())
}

fn open_private_lock(path: &Path) -> Result<File, ProviderError> {
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(ProviderError::CredentialStore(format!(
            "credential lock `{}` is a symbolic link",
            path.display()
        )));
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|error| store_io("opening credential lock", error))?;
    set_private_file(&file)?;
    Ok(file)
}

fn open_read_no_follow(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn set_private_file(file: &File) -> Result<(), ProviderError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| store_io("securing credential file", error))?;
    }
    Ok(())
}

fn private_file_permissions(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o077 == 0
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        true
    }
}

fn store_io(operation: &str, error: std::io::Error) -> ProviderError {
    ProviderError::CredentialStore(format!("{operation}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "fx-auth-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn key(value: &str) -> Credential {
        Credential::ApiKey {
            secret: value.into(),
            attributes: BTreeMap::new(),
        }
    }

    #[test]
    fn isolates_provider_credentials_and_deletes_only_one() {
        let root = temporary("isolation");
        let store = FileCredentialStore::new(&root);
        store.lock("alpha").unwrap().replace(key("a")).unwrap();
        store.lock("beta").unwrap().replace(key("b")).unwrap();
        assert!(store.lock("alpha").unwrap().credential().is_some());
        store.lock("alpha").unwrap().delete().unwrap();
        assert!(store.lock("alpha").unwrap().credential().is_none());
        assert!(store.lock("beta").unwrap().credential().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_credentials_fail_closed() {
        let root = temporary("corrupt");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("alpha.json");
        fs::write(&path, b"not-json").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let error = FileCredentialStore::new(&root).lock("alpha").err().unwrap();
        assert!(error.to_string().contains("corrupt"));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stored_files_and_directories_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary("mode");
        let store = FileCredentialStore::new(&root);
        store.lock("alpha").unwrap().replace(key("a")).unwrap();
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(root.join("alpha.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }
}
