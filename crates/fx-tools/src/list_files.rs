use std::fs;

use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::resolve_existing;

const IGNORED_NAMES: &[&str] = &[
    ".git",
    ".zig-cache",
    "zig-out",
    "target",
    "node_modules",
    ".next",
    "dist",
    "build",
    "coverage",
];

#[derive(Clone, Copy, Debug, Default)]
pub struct ListFiles;

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default = "default_path")]
    path: String,
}

fn default_path() -> String {
    ".".into()
}

impl Tool for ListFiles {
    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List the immediate entries of a directory with bounded output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
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
        let requested = if input.path.trim().is_empty() {
            "."
        } else {
            input.path.trim()
        };
        Ok(vec![
            resolve_existing(&context.workspace_root, requested)?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input: Input = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        let requested = if input.path.trim().is_empty() {
            "."
        } else {
            input.path.trim()
        };
        crate::read_path_permission(self.name(), context, requested)
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
    let requested = if input.path.trim().is_empty() {
        "."
    } else {
        input.path.trim()
    };
    let resolved = resolve_existing(&context.workspace_root, requested)?;
    let source = fs::read_dir(&resolved.absolute).map_err(|error| {
        ToolError::Execution(format!("{}: {error}", resolved.absolute.display()))
    })?;
    let mut entries = Vec::new();
    let mut truncated = false;
    for entry in source {
        let entry = entry.map_err(|error| ToolError::Execution(error.to_string()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if IGNORED_NAMES.contains(&name.as_str()) {
            continue;
        }
        if entries.len() >= context.limits.max_list_entries {
            truncated = true;
            break;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        let suffix = if file_type.is_dir() {
            "/"
        } else if file_type.is_symlink() {
            "@"
        } else {
            ""
        };
        entries.push((name, suffix));
    }

    let display = if resolved.display.as_os_str().is_empty() {
        ".".into()
    } else {
        resolved.display.to_string_lossy()
    };
    let mut content = format!("{display}:\n");
    for (name, suffix) in &entries {
        content.push_str(&format!("- {name}{suffix}\n"));
    }
    if entries.is_empty() {
        content.push_str("(empty)\n");
    } else if truncated {
        content.push_str(&format!(
            "... and more entries (showing first {})\n",
            context.limits.max_list_entries
        ));
    }
    let original_bytes = content.len();
    Ok(ToolOutput {
        content,
        is_error: false,
        structured: None,
        original_bytes,
        truncated,
        durable_content: None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture() -> (PathBuf, ToolContext) {
        let root = std::env::temp_dir().join(format!("fx-list-tool-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("folder")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("file.txt"), "text").unwrap();
        let context = ToolContext::new(root.clone());
        (root, context)
    }

    #[test]
    fn lists_immediate_entries_and_filters_build_directories() {
        let (root, context) = fixture();
        let output = execute(&context, json!({})).unwrap();
        assert!(output.content.starts_with(".:\n"));
        assert!(output.content.contains("- folder/\n"));
        assert!(output.content.contains("- file.txt\n"));
        assert!(!output.content.contains(".git"));
        fs::remove_dir_all(root).unwrap();
    }
}
