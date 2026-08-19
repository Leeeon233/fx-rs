use std::fs::{self, File, Metadata};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use atomic_write_file::AtomicWriteFile;
use fx_core::{
    BoxFuture, FileChangeReview, PermissionRequest, PreparedToolAction, PreparedToolCall,
    ReadEvidence, ToolContext, ToolEffect, ToolError, ToolOutput, ToolReview,
};
use memchr::memmem;
use sha2::{Digest, Sha256};

use crate::resolve_target;

pub(crate) const MAX_CONTENT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PATH_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MutationKind {
    Write,
    Edit,
}

#[derive(Debug)]
pub(crate) enum Mutation {
    Write(Vec<u8>),
    Edit {
        old_string: Vec<u8>,
        new_string: Vec<u8>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileIdentity {
    len: u64,
    modified_ns: i128,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    mode: u32,
}

#[derive(Clone, Debug)]
struct Snapshot {
    identity: FileIdentity,
    content_hash: [u8; 32],
    content: Vec<u8>,
}

#[derive(Debug)]
struct PreparedFileMutation {
    kind: MutationKind,
    raw_path: String,
    target: PathBuf,
    display_path: PathBuf,
    preimage: Option<Snapshot>,
    after: Vec<u8>,
}

pub(crate) fn prepare_file_mutation(
    context: &ToolContext,
    tool_name: &str,
    raw_path: String,
    mutation: Mutation,
) -> Result<PreparedToolCall, ToolError> {
    if raw_path.len() > MAX_PATH_BYTES {
        return Err(ToolError::InvalidArguments(
            "file mutation preparation failed: path exceeds the preparation limit".into(),
        ));
    }
    let resolved = resolve_target(&context.workspace_root, &raw_path)?;
    let preimage = read_snapshot(&resolved.absolute)?;
    let kind = match &mutation {
        Mutation::Write(_) => MutationKind::Write,
        Mutation::Edit { .. } => MutationKind::Edit,
    };
    let after = derive_postimage(preimage.as_ref(), mutation)?;
    let before = preimage.as_ref().map(|snapshot| snapshot.content.clone());
    let review = FileChangeReview {
        path: resolved.display.clone(),
        before,
        after: after.clone(),
    };
    let action = PreparedFileMutation {
        kind,
        raw_path,
        target: resolved.absolute.clone(),
        display_path: resolved.display,
        preimage,
        after,
    };

    Ok(PreparedToolCall::new(
        tool_name,
        vec![PermissionRequest::new(
            tool_name,
            resolved.absolute.display().to_string(),
            ToolEffect::Write,
        )],
        true,
        Some(ToolReview::FileChange(review)),
        action,
    ))
}

fn derive_postimage(preimage: Option<&Snapshot>, mutation: Mutation) -> Result<Vec<u8>, ToolError> {
    match mutation {
        Mutation::Write(content) => {
            if content.len() > MAX_CONTENT_BYTES {
                return Err(ToolError::InvalidArguments(
                    "write_file failed: content exceeds the 4 MiB preparation limit".into(),
                ));
            }
            Ok(content)
        }
        Mutation::Edit {
            old_string,
            new_string,
        } => {
            if old_string.len() > MAX_CONTENT_BYTES {
                return Err(ToolError::InvalidArguments(
                    "edit_file failed: old_string exceeds the 4 MiB preparation limit".into(),
                ));
            }
            if new_string.len() > MAX_CONTENT_BYTES {
                return Err(ToolError::InvalidArguments(
                    "edit_file failed: new_string exceeds the 4 MiB preparation limit".into(),
                ));
            }
            if old_string == new_string {
                return Err(ToolError::InvalidArguments(
                    "edit_file failed: old_string and new_string are identical".into(),
                ));
            }
            let before = preimage
                .ok_or_else(|| {
                    ToolError::Execution(
                        "file mutation preparation failed: approved filesystem identity changed"
                            .into(),
                    )
                })?
                .content
                .as_slice();
            let matches: Vec<_> = memmem::find_iter(before, &old_string).collect();
            match matches.as_slice() {
                [] => Err(ToolError::InvalidArguments(
                    "edit_file failed: old_string not found in file".into(),
                )),
                [position] => {
                    let after_len = before
                        .len()
                        .checked_sub(old_string.len())
                        .and_then(|len| len.checked_add(new_string.len()))
                        .ok_or_else(|| {
                            ToolError::InvalidArguments(
                                "edit_file failed: postimage exceeds the 4 MiB preparation limit"
                                    .into(),
                            )
                        })?;
                    if after_len > MAX_CONTENT_BYTES {
                        return Err(ToolError::InvalidArguments(
                            "edit_file failed: postimage exceeds the 4 MiB preparation limit"
                                .into(),
                        ));
                    }
                    let suffix = *position + old_string.len();
                    let mut after = Vec::with_capacity(after_len);
                    after.extend_from_slice(&before[..*position]);
                    after.extend_from_slice(&new_string);
                    after.extend_from_slice(&before[suffix..]);
                    Ok(after)
                }
                matches => Err(ToolError::InvalidArguments(format!(
                    "edit_file failed: old_string is not unique (found {} occurrences), provide more context",
                    matches.len()
                ))),
            }
        }
    }
}

impl PreparedFileMutation {
    fn apply(self, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        if self
            .preimage
            .as_ref()
            .is_some_and(|before| before.content == self.after)
            || (self.preimage.is_none() && self.after.is_empty())
        {
            return Ok(success_output(format!(
                "No changes to {}; it already contains the requested content",
                self.display_path.display()
            )));
        }

        self.verify_binding(context)?;
        if let Some(parent) = self.target.parent() {
            fs::create_dir_all(parent).map_err(|error| mutation_io_error(&self.target, error))?;
        }
        self.verify_binding(context)?;
        verify_preimage(&self.target, self.preimage.as_ref())?;

        let mut stage = AtomicWriteFile::open(&self.target)
            .map_err(|error| mutation_io_error(&self.target, error))?;
        for chunk in self.after.chunks(64 * 1024) {
            stage
                .write_all(chunk)
                .map_err(|error| mutation_io_error(&self.target, error))?;
        }
        stage
            .sync_all()
            .map_err(|error| mutation_io_error(&self.target, error))?;

        self.verify_binding(context)?;
        verify_preimage(&self.target, self.preimage.as_ref())?;
        stage
            .commit()
            .map_err(|error| mutation_io_error(&self.target, error))?;

        if let Some(store) = &context.read_evidence {
            let metadata = fs::metadata(&self.target)
                .map_err(|error| mutation_io_error(&self.target, error))?;
            store.record(
                self.target.clone(),
                ReadEvidence {
                    modified_ns: metadata_modified_ns(&metadata),
                    content_hash: Sha256::digest(&self.after).into(),
                    model_view_covers_full_file: true,
                    snapshot_covers_full_file: true,
                },
            );
        }

        Ok(success_output(format!(
            "{} {} ({} bytes)",
            match self.kind {
                MutationKind::Write => "wrote",
                MutationKind::Edit => "edited",
            },
            self.display_path.display(),
            self.after.len()
        )))
    }

    fn verify_binding(&self, context: &ToolContext) -> Result<(), ToolError> {
        let resolved = resolve_target(&context.workspace_root, &self.raw_path)?;
        if resolved.absolute != self.target {
            return Err(ToolError::Execution(
                "file mutation rejected because the approved path traversal changed".into(),
            ));
        }
        Ok(())
    }
}

impl PreparedToolAction for PreparedFileMutation {
    fn commit<'a>(
        self: Box<Self>,
        context: &'a ToolContext,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { self.apply(context) })
    }
}

fn read_snapshot(path: &Path) -> Result<Option<Snapshot>, ToolError> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(mutation_io_error(path, error)),
    };
    let metadata = file
        .metadata()
        .map_err(|error| mutation_io_error(path, error))?;
    if !metadata.is_file() {
        return Err(ToolError::Execution(format!(
            "file mutation preparation failed: {} is not a regular file",
            path.display()
        )));
    }
    let mut content = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_CONTENT_BYTES)
            .min(MAX_CONTENT_BYTES),
    );
    Read::by_ref(&mut file)
        .take((MAX_CONTENT_BYTES + 1) as u64)
        .read_to_end(&mut content)
        .map_err(|error| mutation_io_error(path, error))?;
    if content.len() > MAX_CONTENT_BYTES {
        return Err(ToolError::Execution(
            "file mutation preparation failed: preimage exceeds the 4 MiB preparation limit".into(),
        ));
    }
    let content_hash = Sha256::digest(&content).into();
    Ok(Some(Snapshot {
        identity: file_identity(&metadata),
        content_hash,
        content,
    }))
}

fn verify_preimage(path: &Path, expected: Option<&Snapshot>) -> Result<(), ToolError> {
    let actual = read_snapshot(path)?;
    let fresh = match (expected, actual.as_ref()) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            expected.identity == actual.identity && expected.content_hash == actual.content_hash
        }
        _ => false,
    };
    if !fresh {
        return Err(ToolError::Execution(
            "file mutation rejected because the file changed after preview; make a new tool call for a fresh preview"
                .into(),
        ));
    }
    Ok(())
}

fn file_identity(metadata: &Metadata) -> FileIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        FileIdentity {
            len: metadata.len(),
            modified_ns: metadata_modified_ns(metadata),
            device: metadata.dev(),
            inode: metadata.ino(),
            mode: metadata.mode(),
        }
    }
    #[cfg(not(unix))]
    {
        FileIdentity {
            len: metadata.len(),
            modified_ns: metadata_modified_ns(metadata),
        }
    }
}

fn metadata_modified_ns(metadata: &Metadata) -> i128 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
        Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

fn mutation_io_error(path: &Path, error: std::io::Error) -> ToolError {
    ToolError::Execution(format!(
        "file mutation failed before commit: {}: {error}",
        path.display()
    ))
}

fn success_output(content: String) -> ToolOutput {
    let original_bytes = content.len();
    ToolOutput {
        content,
        is_error: false,
        structured: None,
        original_bytes,
        truncated: false,
        durable_content: None,
    }
}

#[cfg(test)]
mod tests {
    use std::task::{Context, Poll, Waker};

    use fx_core::{MemoryReadEvidenceStore, ReadEvidenceStore, Tool, ToolPreparation};
    use serde_json::json;

    use super::*;
    use crate::{EditFile, WriteFile};

    fn fixture(name: &str) -> (PathBuf, ToolContext) {
        let root = std::env::temp_dir().join(format!("fx-mutation-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        (root.clone(), ToolContext::new(root))
    }

    fn prepared(preparation: ToolPreparation) -> PreparedToolCall {
        match preparation {
            ToolPreparation::Prepared(call) => call,
            ToolPreparation::Direct { .. } => panic!("expected prepared mutation"),
        }
    }

    fn commit(call: PreparedToolCall, context: &ToolContext) -> Result<ToolOutput, ToolError> {
        let mut future = call.commit(context);
        let mut task_context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut task_context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("filesystem mutation unexpectedly yielded"),
        }
    }

    #[test]
    fn write_prepares_review_and_creates_missing_parents_atomically() {
        let (root, context) = fixture("write-new");
        let call = prepared(
            WriteFile
                .prepare(
                    &context,
                    &json!({"path": "nested/file.txt", "content": "hello\n"}),
                )
                .unwrap(),
        );
        assert!(call.irreversible);
        assert_eq!(call.permission_requests[0].permission, "write_file");
        assert_eq!(
            call.review,
            Some(ToolReview::FileChange(FileChangeReview {
                path: PathBuf::from("nested/file.txt"),
                before: None,
                after: b"hello\n".to_vec(),
            }))
        );

        let output = commit(call, &context).unwrap();
        assert_eq!(output.content, "wrote nested/file.txt (6 bytes)");
        assert_eq!(fs::read(root.join("nested/file.txt")).unwrap(), b"hello\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_preimage_rejects_commit_without_overwriting() {
        let (root, context) = fixture("stale");
        let path = root.join("file.txt");
        fs::write(&path, "reviewed").unwrap();
        let call = prepared(
            WriteFile
                .prepare(
                    &context,
                    &json!({"path": "file.txt", "content": "approved"}),
                )
                .unwrap(),
        );
        fs::write(&path, "changed concurrently").unwrap();

        let error = commit(call, &context).unwrap_err();
        assert!(error.to_string().contains("changed after preview"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "changed concurrently");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_requires_one_exact_non_overlapping_occurrence() {
        let (root, context) = fixture("edit-unique");
        fs::write(root.join("file.txt"), "same same").unwrap();
        let error = EditFile
            .prepare(
                &context,
                &json!({
                    "path": "file.txt",
                    "old_string": "same",
                    "new_string": "different"
                }),
            )
            .unwrap_err();
        assert!(error.to_string().contains("found 2 occurrences"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn edit_commits_exact_replacement_and_refreshes_read_evidence() {
        let (root, mut context) = fixture("edit");
        let path = root.join("file.txt");
        fs::write(&path, "before middle after").unwrap();
        let evidence = std::sync::Arc::new(MemoryReadEvidenceStore::default());
        context.read_evidence = Some(evidence.clone());
        let call = prepared(
            EditFile
                .prepare(
                    &context,
                    &json!({
                        "path": "file.txt",
                        "old_string": "middle",
                        "new_string": "center"
                    }),
                )
                .unwrap(),
        );

        let output = commit(call, &context).unwrap();
        assert_eq!(output.content, "edited file.txt (19 bytes)");
        assert_eq!(fs::read_to_string(&path).unwrap(), "before center after");
        let record = evidence.lookup(&path.canonicalize().unwrap()).unwrap();
        assert!(record.model_view_covers_full_file);
        assert_eq!(
            record.content_hash,
            Sha256::digest(b"before center after")[..]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn overwrite_preserves_unix_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let (root, context) = fixture("mode");
        let path = root.join("script.sh");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o751)).unwrap();
        let call = prepared(
            WriteFile
                .prepare(&context, &json!({"path": "script.sh", "content": "new"}))
                .unwrap(),
        );
        commit(call, &context).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o751);
        fs::remove_dir_all(root).unwrap();
    }
}
