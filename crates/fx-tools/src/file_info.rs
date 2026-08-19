use std::time::UNIX_EPOCH;

use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::resolve_existing;

#[derive(Clone, Copy, Debug, Default)]
pub struct FileInfo;

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
}

impl Tool for FileInfo {
    fn name(&self) -> &str {
        "file_info"
    }

    fn description(&self) -> &str {
        "Inspect file or directory metadata, including type, size, and modified time."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Read)
    }

    fn project_context_targets(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<std::path::PathBuf>, ToolError> {
        let input: Input = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        Ok(vec![
            resolve_existing(&context.workspace_root, &input.path)?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input: Input = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        crate::read_path_permission(self.name(), context, &input.path)
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { execute(context, arguments) })
    }
}

fn execute(context: &ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
    let input: Input = serde_json::from_value(arguments)
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    if input.path.trim().is_empty() {
        return Err(ToolError::InvalidArguments(
            "file_info field `path` must not be empty".into(),
        ));
    }
    let resolved = resolve_existing(&context.workspace_root, &input.path)?;
    let metadata = std::fs::metadata(&resolved.absolute)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let kind = if metadata.is_dir() {
        "directory"
    } else if metadata.is_file() {
        "file"
    } else if metadata.is_symlink() {
        "symlink"
    } else {
        "other"
    };
    let modified = metadata
        .modified()
        .map_err(|error| ToolError::Execution(error.to_string()))?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let timestamp = jiff::Timestamp::from_second(
        i64::try_from(modified.as_secs())
            .map_err(|_| ToolError::Execution("modified time is out of range".into()))?,
    )
    .map_err(|error| ToolError::Execution(error.to_string()))?;
    let display = if resolved.display.as_os_str().is_empty() {
        ".".into()
    } else {
        resolved.display.to_string_lossy()
    };
    let mut content = format!(
        "path: {display}\ntype: {kind}\nsize: {} bytes\nmodified: {}\n",
        metadata.len(),
        timestamp.strftime("%Y-%m-%dT%H:%M:%SZ")
    );
    if metadata.is_file()
        && let Some(extension) = resolved.absolute.extension()
    {
        content.push_str(&format!("extension: {}\n", extension.to_string_lossy()));
    }
    let original_bytes = content.len();
    Ok(ToolOutput {
        content,
        is_error: false,
        structured: None,
        original_bytes,
        truncated: false,
        durable_content: None,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn reports_stable_file_metadata_shape() {
        let root = std::env::temp_dir().join(format!("fx-info-tool-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("sample.rs"), "fn main() {}\n").unwrap();
        let context = ToolContext::new(root.clone());
        let output = execute(&context, json!({"path": "sample.rs"})).unwrap();
        assert!(
            output
                .content
                .starts_with("path: sample.rs\ntype: file\nsize: 13 bytes\n")
        );
        assert!(output.content.contains("modified: "));
        assert!(output.content.ends_with("extension: rs\n"));
        fs::remove_dir_all(root).unwrap();
    }
}
