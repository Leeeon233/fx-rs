use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use memchr::memmem;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::resolve_existing;

const OUTPUT_CAP: usize = 200;
const COLLECTION_CAP: usize = 2_000;
const CONTEXT_LINES_CAP: usize = 5;
const FILE_BYTE_CAP: u64 = 200 * 1024;
const CANDIDATE_CAP: usize = 100_000;

#[derive(Clone, Copy, Debug, Default)]
pub struct GrepFiles;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OutputMode {
    #[default]
    Matches,
    FilesWithMatches,
    Count,
}

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    case_insensitive: bool,
    #[serde(default)]
    mode: OutputMode,
    #[serde(default = "default_head_limit")]
    head_limit: usize,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    context_lines: usize,
}

#[derive(Clone, Debug)]
struct Match {
    absolute_path: PathBuf,
    display_path: String,
    line_number: usize,
    line: String,
}

#[derive(Default)]
struct SearchResult {
    matches: Vec<Match>,
    matching_lines: usize,
    matching_files: usize,
    collection_truncated: bool,
    candidate_incomplete: bool,
}

fn default_path() -> String {
    ".".into()
}

fn default_head_limit() -> usize {
    OUTPUT_CAP
}

impl Tool for GrepFiles {
    fn name(&self) -> &str {
        "grep_files"
    }

    fn description(&self) -> &str {
        "Search text files for a literal substring with bounded, paginated output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string" },
                "path": { "type": "string" },
                "include": { "type": "string" },
                "case_insensitive": { "type": "boolean" },
                "mode": { "type": "string", "enum": ["matches", "files_with_matches", "count"] },
                "head_limit": { "type": "integer", "minimum": 1 },
                "offset": { "type": "integer", "minimum": 0 },
                "context_lines": { "type": "integer", "minimum": 0, "maximum": CONTEXT_LINES_CAP }
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
    let mut input: Input = serde_json::from_value(arguments)
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    if input.head_limit == 0 {
        return Err(ToolError::InvalidArguments(
            "grep_files field `head_limit` must be a positive integer".into(),
        ));
    }
    input.head_limit = input.head_limit.min(OUTPUT_CAP);
    input.context_lines = input.context_lines.min(CONTEXT_LINES_CAP);

    let root = resolve_existing(&context.workspace_root, &input.path)?;
    let include = input.include.as_deref().map(compile_glob).transpose()?;
    let mut result = SearchResult::default();
    if root.absolute.is_file() {
        if include
            .as_ref()
            .is_none_or(|matcher| matcher.is_match(root.absolute.file_name().unwrap_or_default()))
        {
            scan_file(
                &root.absolute,
                root.display.to_string_lossy().into_owned(),
                &input,
                &mut result,
            )?;
        }
    } else if root.absolute.is_dir() {
        scan_directory(&root, &input, include.as_ref(), &mut result)?;
    } else {
        return Err(ToolError::Execution(format!(
            "not a regular file or directory: {}",
            root.absolute.display()
        )));
    }

    let mut content = match input.mode {
        OutputMode::Matches => format_matches(context, &input, &result),
        OutputMode::FilesWithMatches => format_files(context, &input, &result),
        OutputMode::Count => format!(
            "[grep] count {} matching lines in {} files for {}\n",
            result.matching_lines, result.matching_files, input.pattern
        ),
    };
    if result.collection_truncated {
        content.push_str(&format!(
            "... match collection cap reached at {COLLECTION_CAP} matches before all candidate files were scanned\n"
        ));
    }
    if result.candidate_incomplete {
        content.push_str(&format!(
            "... candidate list may be incomplete; candidate cap {CANDIDATE_CAP} reached before all files were discovered\n"
        ));
    }
    let original_bytes = content.len();
    Ok(ToolOutput {
        content,
        is_error: false,
        structured: None,
        original_bytes,
        truncated: result.collection_truncated || result.candidate_incomplete,
        durable_content: None,
    })
}

fn scan_directory(
    root: &crate::ResolvedPath,
    input: &Input,
    include: Option<&GlobMatcher>,
    result: &mut SearchResult,
) -> Result<(), ToolError> {
    let mut candidates = 0usize;
    let mut builder = WalkBuilder::new(&root.absolute);
    builder
        .standard_filters(true)
        .hidden(true)
        .follow_links(false);
    for entry in builder.build() {
        let entry = entry.map_err(|error| ToolError::Execution(error.to_string()))?;
        if entry.depth() == 0 || !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if candidates >= CANDIDATE_CAP {
            result.candidate_incomplete = true;
            break;
        }
        candidates += 1;
        let relative = entry
            .path()
            .strip_prefix(&root.absolute)
            .unwrap_or(entry.path());
        if include.is_some_and(|matcher| {
            !matches_include(
                matcher,
                relative,
                input.include.as_deref().unwrap_or_default(),
            )
        }) {
            continue;
        }
        let display = if root.display.as_os_str().is_empty() {
            relative.to_owned()
        } else {
            root.display.join(relative)
        };
        scan_file(
            entry.path(),
            display.to_string_lossy().into_owned(),
            input,
            result,
        )?;
        if result.collection_truncated && input.mode != OutputMode::Count {
            break;
        }
    }
    Ok(())
}

fn scan_file(
    path: &Path,
    display_path: String,
    input: &Input,
    result: &mut SearchResult,
) -> Result<(), ToolError> {
    let metadata = fs::metadata(path).map_err(|error| ToolError::Execution(error.to_string()))?;
    if metadata.len() > FILE_BYTE_CAP {
        return Ok(());
    }
    let bytes = fs::read(path).map_err(|error| ToolError::Execution(error.to_string()))?;
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(());
    };
    if text.contains('\0') {
        return Ok(());
    }

    let mut file_matched = false;
    for (index, line) in text.split_terminator('\n').enumerate() {
        if !contains(
            line.as_bytes(),
            input.pattern.as_bytes(),
            input.case_insensitive,
        ) {
            continue;
        }
        result.matching_lines += 1;
        if !file_matched {
            file_matched = true;
            result.matching_files += 1;
        }
        if input.mode == OutputMode::Count {
            continue;
        }
        if result.matches.len() >= COLLECTION_CAP {
            result.collection_truncated = true;
            return Ok(());
        }
        result.matches.push(Match {
            absolute_path: path.to_owned(),
            display_path: display_path.clone(),
            line_number: index + 1,
            line: line.to_owned(),
        });
    }
    Ok(())
}

fn contains(haystack: &[u8], needle: &[u8], case_insensitive: bool) -> bool {
    if !case_insensitive {
        return memmem::find(haystack, needle).is_some();
    }
    if needle.is_empty() {
        return true;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, ToolError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

fn matches_include(matcher: &GlobMatcher, path: &Path, raw: &str) -> bool {
    if raw.contains('/') {
        matcher.is_match(path)
    } else {
        path.file_name().is_some_and(|name| matcher.is_match(name))
    }
}

fn format_matches(context: &ToolContext, input: &Input, result: &SearchResult) -> String {
    let limit = input.head_limit.min(context.limits.max_list_entries);
    let start = input.offset.min(result.matches.len());
    let end = start.saturating_add(limit).min(result.matches.len());
    let emitted = end - start;
    let mut output = if result.matches.is_empty() {
        format!("[grep] no matches for {}\n", input.pattern)
    } else if emitted == 0 {
        format!(
            "[grep] no matches for {} at offset {} ({} total matches)\n",
            input.pattern,
            input.offset,
            result.matches.len()
        )
    } else if start == 0 && end == result.matches.len() {
        format!("[grep] {emitted} matches for {}\n", input.pattern)
    } else {
        format!(
            "[grep] {emitted} matches for {} (showing {}-{} of {})\n",
            input.pattern,
            start + 1,
            end,
            result.matches.len()
        )
    };
    for found in &result.matches[start..end] {
        write_match_with_context(
            &mut output,
            found,
            input.context_lines,
            context.limits.max_read_file_line_bytes,
        );
    }
    if end < result.matches.len() {
        output.push_str(&format!(
            "... more matches available; use offset {end} to continue\n"
        ));
    }
    output
}

fn write_match_with_context(
    output: &mut String,
    found: &Match,
    context_lines: usize,
    line_cap: usize,
) {
    if context_lines > 0
        && let Ok(content) = fs::read_to_string(&found.absolute_path)
    {
        let lines: Vec<_> = content.split_terminator('\n').collect();
        let match_index = found.line_number - 1;
        let first = match_index.saturating_sub(context_lines);
        for (index, line) in lines[first..match_index].iter().enumerate() {
            let line_number = first + index + 1;
            output.push_str(&format!(
                "   {}:{line_number}- {}\n",
                found.display_path,
                clip(line, line_cap)
            ));
        }
        output.push_str(&format!(
            " - {}:{}: {}\n",
            found.display_path,
            found.line_number,
            clip(&found.line, line_cap)
        ));
        let after_end = (match_index + 1 + context_lines).min(lines.len());
        for (index, line) in lines[match_index + 1..after_end].iter().enumerate() {
            let line_number = match_index + index + 2;
            output.push_str(&format!(
                "   {}:{line_number}- {}\n",
                found.display_path,
                clip(line, line_cap)
            ));
        }
        return;
    }
    output.push_str(&format!(
        " - {}:{}: {}\n",
        found.display_path,
        found.line_number,
        clip(&found.line, line_cap)
    ));
}

fn clip(line: &str, cap: usize) -> String {
    if line.len() <= cap {
        return line.into();
    }
    let mut end = cap;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &line[..end])
}

fn format_files(context: &ToolContext, input: &Input, result: &SearchResult) -> String {
    let mut seen = HashSet::new();
    let files: Vec<_> = result
        .matches
        .iter()
        .filter_map(|found| {
            seen.insert(found.display_path.as_str())
                .then_some(found.display_path.as_str())
        })
        .collect();
    let limit = input.head_limit.min(context.limits.max_list_entries);
    let start = input.offset.min(files.len());
    let end = start.saturating_add(limit).min(files.len());
    let emitted = end - start;
    let mut output = if files.is_empty() {
        format!("[grep] no files with matches for {}\n", input.pattern)
    } else if emitted == 0 {
        format!(
            "[grep] no files with matches for {} at offset {} ({} total files)\n",
            input.pattern,
            input.offset,
            files.len()
        )
    } else if start == 0 && end == files.len() {
        format!(
            "[grep] {emitted} files with matches for {}\n",
            input.pattern
        )
    } else {
        format!(
            "[grep] {emitted} files with matches for {} (showing {}-{} of {})\n",
            input.pattern,
            start + 1,
            end,
            files.len()
        )
    };
    for path in &files[start..end] {
        output.push_str(&format!(" - {path}\n"));
    }
    if end < files.len() {
        output.push_str(&format!(
            "... more files available; use offset {end} to continue\n"
        ));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (PathBuf, ToolContext) {
        let root = std::env::temp_dir().join(format!("fx-grep-tool-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/one.rs"), "before\nNeedle here\nafter\n").unwrap();
        fs::write(root.join("src/two.rs"), "needle one\nneedle two\n").unwrap();
        fs::write(root.join("src/skip.md"), "needle markdown\n").unwrap();
        let context = ToolContext::new(root.clone());
        (root, context)
    }

    #[test]
    fn searches_literal_text_with_include_and_ascii_case_fold() {
        let (root, context) = fixture("matches");
        let output = execute(
            &context,
            json!({
                "pattern": "needle",
                "include": "*.rs",
                "case_insensitive": true,
                "context_lines": 1
            }),
        )
        .unwrap();
        assert!(output.content.starts_with("[grep] 3 matches for needle\n"));
        assert!(output.content.contains("src/one.rs:2: Needle here"));
        assert!(output.content.contains("src/one.rs:1- before"));
        assert!(!output.content.contains("skip.md"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn count_mode_is_exact_beyond_presentation_limit() {
        let (root, mut context) = fixture("count");
        context.limits.max_list_entries = 1;
        let output = execute(&context, json!({"pattern": "needle", "mode": "count"})).unwrap();
        assert_eq!(
            output.content,
            "[grep] count 3 matching lines in 2 files for needle\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn files_mode_deduplicates_and_paginates() {
        let (root, context) = fixture("files");
        let output = execute(
            &context,
            json!({
                "pattern": "needle",
                "include": "*.rs",
                "case_insensitive": true,
                "mode": "files_with_matches",
                "head_limit": 1
            }),
        )
        .unwrap();
        assert!(output.content.contains("showing 1-1 of 2"));
        assert!(output.content.contains("use offset 1 to continue"));
        fs::remove_dir_all(root).unwrap();
    }
}
