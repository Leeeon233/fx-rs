use std::fs;
use std::path::{Path, PathBuf};

use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::resolve_existing;

const WALK_ENTRY_CAP: usize = 2_000;
const KEYWORD_CAP: usize = 16;
const FILE_BYTE_CAP: u64 = 400 * 1024;
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
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "in", "on", "at", "to", "for", "of", "and", "or",
    "not", "it", "this", "that", "with", "from", "by", "as", "do", "does", "how", "what", "where",
    "when", "why", "which",
];

#[derive(Clone, Copy, Debug, Default)]
pub struct SemanticSearch;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    query: String,
    #[serde(default = "default_path")]
    path: String,
}

#[derive(Debug)]
struct SearchMatch {
    path: PathBuf,
    score: u32,
    line_number: usize,
    sample: String,
}

fn default_path() -> String {
    ".".into()
}

impl Tool for SemanticSearch {
    fn name(&self) -> &str {
        "semantic_search"
    }

    fn description(&self) -> &str {
        "Find files by scoring bounded case-insensitive keyword matches in file names and text content."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["query"],
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
    ) -> Result<Vec<PathBuf>, ToolError> {
        let input = decode(arguments)?;
        Ok(vec![
            resolve_existing(&context.workspace_root, normalized_path(&input.path))?.absolute,
        ])
    }

    fn permission_requests(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let input = decode(arguments)?;
        let resolved = resolve_existing(&context.workspace_root, normalized_path(&input.path))?;
        Ok(vec![PermissionRequest::new(
            self.name(),
            resolved.absolute.display().to_string(),
            ToolEffect::Read,
        )])
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move { execute(context, arguments) })
    }
}

fn decode(arguments: &Value) -> Result<Input, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

fn normalized_path(path: &str) -> &str {
    if path.trim().is_empty() {
        "."
    } else {
        path.trim()
    }
}

fn execute(context: &ToolContext, arguments: Value) -> Result<ToolOutput, ToolError> {
    let input = decode(&arguments)?;
    let keywords = split_keywords(&input.query);
    if keywords.is_empty() {
        return output("[search] empty query\n".into(), false);
    }
    let root = resolve_existing(&context.workspace_root, normalized_path(&input.path))?;
    let retained_cap = context.limits.max_list_entries.max(1).saturating_mul(2);
    let mut matches = Vec::new();
    let mut walk_capped = false;
    let mut result_capped = false;

    if root.absolute.is_file() {
        if let Some(found) = score_file(&root.absolute, &root.display, &keywords) {
            matches.push(found);
        }
    } else if root.absolute.is_dir() {
        let mut walked = 0usize;
        let mut builder = WalkBuilder::new(&root.absolute);
        builder
            .standard_filters(true)
            .hidden(true)
            .follow_links(false)
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !IGNORED_NAMES.contains(&entry.file_name().to_string_lossy().as_ref())
            });
        for entry in builder.build() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if entry.depth() == 0 {
                continue;
            }
            if walked >= WALK_ENTRY_CAP {
                walk_capped = true;
                break;
            }
            walked += 1;
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root.absolute)
                .unwrap_or(entry.path());
            let display = if root.display.as_os_str().is_empty() {
                relative.to_owned()
            } else {
                root.display.join(relative)
            };
            let Some(found) = score_file(entry.path(), &display, &keywords) else {
                continue;
            };
            if matches.len() >= retained_cap {
                result_capped = true;
                break;
            }
            matches.push(found);
        }
    } else {
        return Err(ToolError::Execution(format!(
            "not a regular file or directory: {}",
            root.absolute.display()
        )));
    }

    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.path.cmp(&right.path))
    });
    let shown = matches.len().min(context.limits.max_list_entries);
    let mut content = if shown == 0 {
        format!("[search] no results for: {}\n", input.query)
    } else {
        format!("[search] {shown} results for: {}\n", input.query)
    };
    for found in &matches[..shown] {
        content.push_str(&format!(
            "{}:{}: {}\n",
            found.path.display(),
            found.line_number,
            clip_utf8(&found.sample, context.limits.max_read_file_line_bytes)
        ));
    }
    if matches.len() > shown {
        content.push_str(&format!("... and {} more\n", matches.len() - shown));
    }
    if walk_capped {
        content.push_str(
            "... results may be incomplete; traversal cap reached before all files were searched\n",
        );
    }
    if result_capped {
        content.push_str(
            "... results may be incomplete; result cap reached before all matching files were scored\n",
        );
    }
    output(content, walk_capped || result_capped)
}

fn output(content: String, truncated: bool) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput {
        original_bytes: content.len(),
        content,
        is_error: false,
        structured: None,
        truncated,
        durable_content: None,
    })
}

fn split_keywords(query: &str) -> Vec<&str> {
    query
        .split([' ', '\t', ',', '.', ';', ':', '?', '!'])
        .filter(|word| word.len() >= 2)
        .filter(|word| {
            !STOP_WORDS
                .iter()
                .any(|stop| word.eq_ignore_ascii_case(stop))
        })
        .take(KEYWORD_CAP)
        .collect()
}

fn score_file(path: &Path, display: &Path, keywords: &[&str]) -> Option<SearchMatch> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > FILE_BYTE_CAP {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    let mut score = 0u32;
    let mut best_line_score = 0u32;
    let mut best_line_number = 0usize;
    let mut sample = "";
    for (index, line) in text.split('\n').enumerate() {
        let line_score = keywords
            .iter()
            .filter(|keyword| contains_ascii_case_insensitive(line, keyword))
            .count() as u32;
        score = score.saturating_add(line_score);
        if line_score > best_line_score {
            best_line_score = line_score;
            best_line_number = index + 1;
            sample = line;
        }
    }
    let basename = display.file_name()?.to_string_lossy();
    for keyword in keywords {
        if contains_ascii_case_insensitive(&basename, keyword) {
            score = score.saturating_add(3);
        }
    }
    (score > 0).then(|| SearchMatch {
        path: display.to_owned(),
        score,
        line_number: best_line_number,
        sample: sample.to_owned(),
    })
}

fn contains_ascii_case_insensitive(text: &str, needle: &str) -> bool {
    let text = text.as_bytes();
    let needle = needle.as_bytes();
    !needle.is_empty()
        && text
            .windows(needle.len())
            .any(|candidate| candidate.eq_ignore_ascii_case(needle))
}

fn clip_utf8(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> (PathBuf, ToolContext) {
        let root =
            std::env::temp_dir().join(format!("fx-semantic-search-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("docs")).unwrap();
        (root.clone(), ToolContext::new(root))
    }

    #[test]
    fn scores_content_and_basename_in_deterministic_order() {
        let (root, context) = fixture("ranking");
        fs::write(root.join("docs/alpha-name.txt"), "alpha topic line\n").unwrap();
        fs::write(root.join("docs/beta.txt"), "alpha topic line\n").unwrap();

        let result = execute(&context, json!({"query": "alpha"})).unwrap();
        assert!(result.content.starts_with(
            "[search] 2 results for: alpha\ndocs/alpha-name.txt:1: alpha topic line\n"
        ));
        assert!(result.content.contains("docs/beta.txt:1: alpha topic line"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stopword_queries_and_direct_file_roots_match_zig_contract() {
        let (root, context) = fixture("direct");
        fs::write(root.join("direct.txt"), "needle is here\n").unwrap();
        assert_eq!(
            execute(&context, json!({"query": "the and it"}))
                .unwrap()
                .content,
            "[search] empty query\n"
        );
        assert_eq!(
            execute(&context, json!({"query": "needle", "path": "direct.txt"}))
                .unwrap()
                .content,
            "[search] 1 results for: needle\ndirect.txt:1: needle is here\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ignores_build_directories_and_non_text_files() {
        let (root, context) = fixture("ignored");
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("target/debug/generated.txt"), "needle").unwrap();
        fs::write(root.join("binary.dat"), b"needle\0binary").unwrap();

        let result = execute(&context, json!({"query": "needle"})).unwrap();
        assert_eq!(result.content, "[search] no results for: needle\n");
        fs::remove_dir_all(root).unwrap();
    }
}
