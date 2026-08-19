use std::fs::{self, File, Metadata};
use std::io;
use std::path::Path;

use atomic_write_file::AtomicWriteFile;
use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{resolve_existing, resolve_target};

#[derive(Clone, Copy, Debug, Default)]
pub struct CreateFolder;

#[derive(Clone, Copy, Debug, Default)]
pub struct CopyFile;

#[derive(Clone, Copy, Debug, Default)]
pub struct RenameFile;

#[derive(Clone, Copy, Debug, Default)]
pub struct DeleteFile;

#[derive(Clone, Debug, Deserialize)]
struct PathInput {
    path: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CopyInput {
    source: String,
    destination: String,
}

#[derive(Clone, Debug, Deserialize)]
struct RenameInput {
    old_path: String,
    new_path: String,
}

impl Tool for CreateFolder {
    fn name(&self) -> &str {
        "create_folder"
    }

    fn description(&self) -> &str {
        "Create a directory and any missing parent directories."
    }

    fn input_schema(&self) -> Value {
        one_path_schema("path")
    }

    fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Write)
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<std::path::PathBuf>, ToolError> {
        let input: PathInput = decode(arguments)?;
        Ok(vec![
            resolve_target(&context.workspace_root, &input.path)?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input: PathInput = decode(arguments)?;
        let target = resolve_target(&context.workspace_root, &input.path)?;
        Ok(vec![permission(self.name(), &target.absolute)])
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: PathInput = decode(&arguments)?;
            let target = resolve_target(&context.workspace_root, &input.path)?;
            let display = display_path(&target.display);
            match fs::metadata(&target.absolute) {
                Ok(metadata) if metadata.is_dir() => {
                    return Ok(output(format!("directory already exists: {display}")));
                }
                Ok(_) => {
                    return Err(ToolError::Execution(format!(
                        "create_folder failed: target exists and is not a directory: {display}"
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error("create_folder failed", &target.absolute, error));
                }
            }
            fs::create_dir_all(&target.absolute)
                .map_err(|error| io_error("create_folder failed", &target.absolute, error))?;
            Ok(output(format!("created {display}")))
        })
    }
}

impl Tool for CopyFile {
    fn name(&self) -> &str {
        "copy_file"
    }

    fn description(&self) -> &str {
        "Atomically copy a regular file, replacing an existing file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string" },
                "destination": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["source", "destination"],
            "additionalProperties": false
        })
    }

    fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Write)
    }

    fn irreversible(&self, _: &Value) -> Result<bool, ToolError> {
        Ok(true)
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<std::path::PathBuf>, ToolError> {
        let input: CopyInput = decode(arguments)?;
        Ok(vec![
            resolve_existing(&context.workspace_root, &input.source)?.absolute,
            resolve_target(&context.workspace_root, &input.destination)?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input: CopyInput = decode(arguments)?;
        let source = resolve_existing(&context.workspace_root, &input.source)?;
        let destination = resolve_target(&context.workspace_root, &input.destination)?;
        Ok(vec![
            PermissionRequest::new(
                self.name(),
                source.absolute.display().to_string(),
                ToolEffect::Read,
            ),
            permission(self.name(), &destination.absolute),
        ])
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: CopyInput = decode(&arguments)?;
            let source = resolve_existing(&context.workspace_root, &input.source)?;
            let destination = resolve_target(&context.workspace_root, &input.destination)?;
            let source_display = display_path(&source.display);
            let destination_display = display_path(&destination.display);
            if source.absolute == destination.absolute {
                return Ok(output(format!(
                    "copied {source_display} -> {destination_display}"
                )));
            }

            let mut source_file = File::open(&source.absolute)
                .map_err(|error| io_error("copy_file failed", &source.absolute, error))?;
            let source_metadata = source_file
                .metadata()
                .map_err(|error| io_error("copy_file failed", &source.absolute, error))?;
            if !source_metadata.is_file() {
                return Err(ToolError::Execution(format!(
                    "copy_file failed: {source_display}"
                )));
            }
            validate_copy_destination(&destination.absolute, &source_display)?;
            if let Some(parent) = destination.absolute.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| io_error("copy_file failed", parent, error))?;
            }
            verify_target_binding(context, &input.destination, &destination.absolute)?;

            let mut stage = AtomicWriteFile::open(&destination.absolute)
                .map_err(|error| io_error("copy_file failed", &destination.absolute, error))?;
            io::copy(&mut source_file, &mut stage)
                .map_err(|error| io_error("copy_file failed", &destination.absolute, error))?;
            stage
                .set_permissions(source_metadata.permissions())
                .map_err(|error| io_error("copy_file failed", &destination.absolute, error))?;
            stage
                .sync_all()
                .map_err(|error| io_error("copy_file failed", &destination.absolute, error))?;
            verify_target_binding(context, &input.destination, &destination.absolute)?;
            stage
                .commit()
                .map_err(|error| io_error("copy_file failed", &destination.absolute, error))?;
            if let Some(store) = &context.read_evidence {
                store.remove(&destination.absolute);
            }
            Ok(output(format!(
                "copied {source_display} -> {destination_display}"
            )))
        })
    }
}

impl Tool for RenameFile {
    fn name(&self) -> &str {
        "rename_file"
    }

    fn description(&self) -> &str {
        "Rename a file, replacing an existing regular destination."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "old_path": { "type": "string" },
                "new_path": { "type": "string" },
                "overwrite": { "type": "boolean" }
            },
            "required": ["old_path", "new_path"],
            "additionalProperties": false
        })
    }

    fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Write)
    }

    fn irreversible(&self, _: &Value) -> Result<bool, ToolError> {
        Ok(true)
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<std::path::PathBuf>, ToolError> {
        let input: RenameInput = decode(arguments)?;
        Ok(vec![
            resolve_existing(&context.workspace_root, &input.old_path)?.absolute,
            resolve_target(&context.workspace_root, &input.new_path)?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input: RenameInput = decode(arguments)?;
        let source = resolve_existing(&context.workspace_root, &input.old_path)?;
        let destination = resolve_target(&context.workspace_root, &input.new_path)?;
        Ok(vec![
            permission(self.name(), &source.absolute),
            permission(self.name(), &destination.absolute),
        ])
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: RenameInput = decode(&arguments)?;
            let source = resolve_existing(&context.workspace_root, &input.old_path)?;
            let destination = resolve_target(&context.workspace_root, &input.new_path)?;
            let source_display = display_path(&source.display);
            let destination_display = display_path(&destination.display);
            if source.absolute == destination.absolute {
                return Ok(output(format!(
                    "renamed {source_display} -> {destination_display}"
                )));
            }
            if let Some(parent) = destination.absolute.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| io_error("rename_file failed", parent, error))?;
            }
            verify_target_binding(context, &input.new_path, &destination.absolute)?;
            fs::rename(&source.absolute, &destination.absolute)
                .map_err(|error| io_error("rename_file failed", &source.absolute, error))?;
            if let Some(store) = &context.read_evidence {
                store.remove(&source.absolute);
                store.remove(&destination.absolute);
            }
            Ok(output(format!(
                "renamed {source_display} -> {destination_display}"
            )))
        })
    }
}

impl Tool for DeleteFile {
    fn name(&self) -> &str {
        "delete_file"
    }

    fn description(&self) -> &str {
        "Delete a file or an empty directory."
    }

    fn input_schema(&self) -> Value {
        one_path_schema("path")
    }

    fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Write)
    }

    fn irreversible(&self, _: &Value) -> Result<bool, ToolError> {
        Ok(true)
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<std::path::PathBuf>, ToolError> {
        let input: PathInput = decode(arguments)?;
        Ok(vec![
            resolve_existing(&context.workspace_root, &input.path)?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input: PathInput = decode(arguments)?;
        let target = resolve_existing(&context.workspace_root, &input.path)?;
        Ok(vec![permission(self.name(), &target.absolute)])
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: PathInput = decode(&arguments)?;
            let target = resolve_existing(&context.workspace_root, &input.path)?;
            let display = display_path(&target.display);
            let metadata = fs::metadata(&target.absolute)
                .map_err(|error| io_error("delete_file failed", &target.absolute, error))?;
            if metadata.is_dir() {
                fs::remove_dir(&target.absolute).map_err(|error| {
                    if error.kind() == io::ErrorKind::DirectoryNotEmpty {
                        ToolError::Execution(format!(
                            "delete_file failed: directory not empty: {display}"
                        ))
                    } else {
                        io_error("delete_file failed", &target.absolute, error)
                    }
                })?;
            } else {
                fs::remove_file(&target.absolute)
                    .map_err(|error| io_error("delete_file failed", &target.absolute, error))?;
            }
            if let Some(store) = &context.read_evidence {
                store.remove(&target.absolute);
            }
            Ok(output(format!("deleted {display}")))
        })
    }
}

fn decode<T: for<'de> Deserialize<'de>>(arguments: &Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

fn permission(name: &str, target: &Path) -> PermissionRequest {
    PermissionRequest::new(name, target.display().to_string(), ToolEffect::Write)
}

fn one_path_schema(field: &str) -> Value {
    json!({
        "type": "object",
        "properties": { field: { "type": "string" } },
        "required": [field],
        "additionalProperties": false
    })
}

fn display_path(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".into()
    } else {
        path.display().to_string()
    }
}

fn output(content: String) -> ToolOutput {
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

fn io_error(operation: &str, path: &Path, error: io::Error) -> ToolError {
    ToolError::Execution(format!("{operation}: {} ({error})", path.display()))
}

fn verify_target_binding(
    context: &ToolContext,
    raw_path: &str,
    expected: &Path,
) -> Result<(), ToolError> {
    let resolved = resolve_target(&context.workspace_root, raw_path)?;
    if resolved.absolute != expected {
        return Err(ToolError::Execution(
            "filesystem target changed after permission evaluation".into(),
        ));
    }
    Ok(())
}

fn validate_copy_destination(path: &Path, source_display: &str) -> Result<(), ToolError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error("copy_file failed", path, error)),
    };
    if !metadata.is_file() || !metadata_is_writable(&metadata) {
        return Err(ToolError::Execution(format!(
            "copy_file failed: {source_display}"
        )));
    }
    Ok(())
}

fn metadata_is_writable(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o222 != 0
    }
    #[cfg(not(unix))]
    {
        !metadata.permissions().readonly()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::task::{Context, Poll, Waker};

    use super::*;

    fn fixture(name: &str) -> (PathBuf, ToolContext) {
        let root = std::env::temp_dir().join(format!("fx-direct-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        (root.clone(), ToolContext::new(root))
    }

    fn run(
        tool: &dyn Tool,
        context: &ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        let mut future = tool.execute(context, arguments);
        let mut task_context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut task_context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("direct filesystem tool unexpectedly yielded"),
        }
    }

    #[test]
    fn create_folder_is_recursive_and_idempotent() {
        let (root, context) = fixture("create");
        let first = run(&CreateFolder, &context, json!({"path": "nested/dir"})).unwrap();
        let second = run(&CreateFolder, &context, json!({"path": "nested/dir"})).unwrap();
        assert_eq!(first.content, "created nested/dir");
        assert_eq!(second.content, "directory already exists: nested/dir");
        assert!(root.join("nested/dir").is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_is_atomic_recursive_and_allows_same_path() {
        let (root, context) = fixture("copy");
        fs::write(root.join("source.txt"), "source\n").unwrap();
        let copied = run(
            &CopyFile,
            &context,
            json!({"source": "source.txt", "destination": "nested/dest.txt", "overwrite": false}),
        )
        .unwrap();
        let same = run(
            &CopyFile,
            &context,
            json!({"source": "source.txt", "destination": "source.txt"}),
        )
        .unwrap();
        assert_eq!(copied.content, "copied source.txt -> nested/dest.txt");
        assert_eq!(same.content, "copied source.txt -> source.txt");
        assert_eq!(fs::read(root.join("nested/dest.txt")).unwrap(), b"source\n");
        assert_eq!(fs::read(root.join("source.txt")).unwrap(), b"source\n");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rename_and_delete_follow_safe_final_symlinks_like_zig() {
        use std::os::unix::fs::symlink;

        let (root, context) = fixture("symlink-targets");
        fs::write(root.join("target.txt"), "target").unwrap();
        symlink("target.txt", root.join("link.txt")).unwrap();
        let renamed = run(
            &RenameFile,
            &context,
            json!({"old_path": "link.txt", "new_path": "moved.txt"}),
        )
        .unwrap();
        assert_eq!(renamed.content, "renamed target.txt -> moved.txt");
        assert!(
            root.join("link.txt")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(root.join("moved.txt")).unwrap(), b"target");

        symlink("moved.txt", root.join("delete-link.txt")).unwrap();
        let deleted = run(&DeleteFile, &context, json!({"path": "delete-link.txt"})).unwrap();
        assert_eq!(deleted.content, "deleted moved.txt");
        assert!(!root.join("moved.txt").exists());
        assert!(
            root.join("delete-link.txt")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_refuses_nonempty_directory() {
        let (root, context) = fixture("delete-nonempty");
        fs::create_dir(root.join("dir")).unwrap();
        fs::write(root.join("dir/file"), "x").unwrap();
        let error = run(&DeleteFile, &context, json!({"path": "dir"})).unwrap_err();
        assert!(error.to_string().contains("directory not empty: dir"));
        fs::remove_dir_all(root).unwrap();
    }
}
