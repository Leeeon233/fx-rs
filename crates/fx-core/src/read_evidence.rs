use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

/// SHA-256 digest used by the original implementation for file snapshots.
pub type ContentHash = [u8; 32];

/// Evidence captured when a model-visible file read succeeds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadEvidence {
    pub modified_ns: i128,
    pub content_hash: ContentHash,
    pub model_view_covers_full_file: bool,
    pub snapshot_covers_full_file: bool,
}

/// Session-scoped read evidence port.
///
/// Hosts may replace the in-memory implementation when reads happen in a
/// remote workspace or need to be projected into a durable session log.
pub trait ReadEvidenceStore: Send + Sync {
    fn record(&self, path: PathBuf, evidence: ReadEvidence);
    fn lookup(&self, path: &Path) -> Option<ReadEvidence>;
    fn remove(&self, path: &Path);
}

/// Minimal in-process store used by the native CLI and tests.
#[derive(Debug, Default)]
pub struct MemoryReadEvidenceStore {
    entries: RwLock<HashMap<PathBuf, ReadEvidence>>,
}

impl ReadEvidenceStore for MemoryReadEvidenceStore {
    fn record(&self, path: PathBuf, evidence: ReadEvidence) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(path, evidence);
    }

    fn lookup(&self, path: &Path) -> Option<ReadEvidence> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
            .copied()
    }

    fn remove(&self, path: &Path) {
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_overwrites_and_removes_evidence() {
        let store = MemoryReadEvidenceStore::default();
        let path = PathBuf::from("/tmp/file.txt");
        let first = ReadEvidence {
            modified_ns: 1,
            content_hash: [1; 32],
            model_view_covers_full_file: true,
            snapshot_covers_full_file: true,
        };
        let second = ReadEvidence {
            modified_ns: 2,
            content_hash: [2; 32],
            model_view_covers_full_file: false,
            snapshot_covers_full_file: true,
        };

        store.record(path.clone(), first);
        assert_eq!(store.lookup(&path), Some(first));
        store.record(path.clone(), second);
        assert_eq!(store.lookup(&path), Some(second));
        store.remove(&path);
        assert_eq!(store.lookup(&path), None);
    }
}
