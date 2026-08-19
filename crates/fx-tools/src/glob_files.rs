use std::path::Path;

use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::resolve_existing;

const MAX_PATTERN_BYTES: usize = 4_096;
const CANDIDATE_CAP: usize = 100_000;
const MAX_RELATIVE_PATH_BYTES: usize = 2_048;

#[derive(Clone, Copy, Debug, Default)]
pub struct GlobFiles;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Matches,
    Count,
}

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    mode: OutputMode,
}

fn default_path() -> String {
    ".".into()
}

impl Tool for GlobFiles {
    fn name(&self) -> &str {
        "glob_files"
    }

    fn description(&self) -> &str {
        "Find file paths matching a glob pattern, or count exact path matches."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "mode": { "type": "string", "enum": ["matches", "count"] }
            },
            "required": ["pattern"],
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
    if input.pattern.len() > MAX_PATTERN_BYTES {
        return Err(ToolError::InvalidArguments(format!(
            "glob_files field `pattern` must be at most {MAX_PATTERN_BYTES} bytes"
        )));
    }
    let resolved = resolve_existing(&context.workspace_root, &input.path)?;
    let matcher = compile(&input.pattern)?;
    let pattern_has_separator = input.pattern.contains('/');
    let include_hidden = input
        .pattern
        .split('/')
        .any(|component| component.starts_with('.'));

    let mut matches = Vec::new();
    let mut match_count = 0usize;
    let mut candidates = 0usize;
    let mut candidate_incomplete = false;
    let mut skipped_overlong = 0usize;

    if resolved.absolute.is_file() {
        let name = resolved.absolute.file_name().unwrap_or_default();
        if matches_candidate(&matcher, Path::new(name), pattern_has_separator) {
            match_count = 1;
            matches.push(resolved.display.to_string_lossy().into_owned());
        }
    } else {
        let mut builder = WalkBuilder::new(&resolved.absolute);
        builder
            .standard_filters(true)
            .hidden(!include_hidden)
            .follow_links(false);
        for entry in builder.build() {
            let entry = entry.map_err(|error| ToolError::Execution(error.to_string()))?;
            if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            if candidates >= CANDIDATE_CAP {
                candidate_incomplete = true;
                break;
            }
            candidates += 1;
            let relative = entry
                .path()
                .strip_prefix(&resolved.absolute)
                .unwrap_or(entry.path());
            let relative_text = relative.to_string_lossy();
            if relative_text.len() > MAX_RELATIVE_PATH_BYTES {
                skipped_overlong += 1;
                continue;
            }
            if !matches_candidate(&matcher, relative, pattern_has_separator) {
                continue;
            }
            match_count += 1;
            if input.mode == OutputMode::Matches && matches.len() < context.limits.max_list_entries
            {
                let display = if resolved.display.as_os_str().is_empty() {
                    relative.to_owned()
                } else {
                    resolved.display.join(relative)
                };
                matches.push(display.to_string_lossy().into_owned());
            }
        }
    }

    matches.sort_unstable();
    let output_truncated = input.mode == OutputMode::Matches && match_count > matches.len();
    let mut content = match input.mode {
        OutputMode::Count => format!("[glob] count {match_count} matches for {}\n", input.pattern),
        OutputMode::Matches if matches.is_empty() => {
            format!("[glob] no matches for {}\n", input.pattern)
        }
        OutputMode::Matches => {
            let mut text = format!("[glob] {} matches for {}\n", matches.len(), input.pattern);
            for path in &matches {
                text.push_str(&format!(" - {path}\n"));
            }
            if output_truncated {
                text.push_str(&format!(
                    "... truncated to first {} matches\n",
                    context.limits.max_list_entries
                ));
            }
            text
        }
    };
    if candidate_incomplete {
        content.push_str(&format!(
            "... candidate list may be incomplete; candidate cap {CANDIDATE_CAP} reached before all files were discovered\n"
        ));
    }
    if skipped_overlong > 0 {
        let suffix = if skipped_overlong == 1 { "" } else { "s" };
        content.push_str(&format!(
            "... skipped {skipped_overlong} overlong candidate path{suffix}\n"
        ));
    }
    let original_bytes = content.len();
    Ok(ToolOutput {
        content,
        is_error: false,
        structured: None,
        original_bytes,
        truncated: output_truncated || candidate_incomplete,
        durable_content: None,
    })
}

fn compile(pattern: &str) -> Result<GlobMatcher, ToolError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

fn matches_candidate(matcher: &GlobMatcher, path: &Path, pattern_has_separator: bool) -> bool {
    if pattern_has_separator {
        matcher.is_match(path)
    } else {
        path.file_name().is_some_and(|name| matcher.is_match(name))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn fixture(name: &str) -> (std::path::PathBuf, ToolContext) {
        let root = std::env::temp_dir().join(format!("fx-glob-tool-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/deep")).unwrap();
        fs::write(root.join("main.rs"), "").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        fs::write(root.join("src/deep/mod.rs"), "").unwrap();
        fs::write(root.join("src/readme.md"), "").unwrap();
        let context = ToolContext::new(root.clone());
        (root, context)
    }

    #[test]
    fn globstar_matches_root_and_nested_files() {
        let (root, context) = fixture("globstar");
        let output = execute(&context, json!({"pattern": "**/*.rs"})).unwrap();
        assert!(output.content.contains("main.rs"));
        assert!(output.content.contains("src/lib.rs"));
        assert!(output.content.contains("src/deep/mod.rs"));
        assert!(!output.content.contains("readme.md"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn count_is_not_limited_by_presentation_cap() {
        let (root, mut context) = fixture("count");
        context.limits.max_list_entries = 1;
        let output = execute(&context, json!({"pattern": "*.rs", "mode": "count"})).unwrap();
        assert_eq!(output.content, "[glob] count 3 matches for *.rs\n");
        fs::remove_dir_all(root).unwrap();
    }
}
