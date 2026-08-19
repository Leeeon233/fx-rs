use std::fs::{File, Metadata};
use std::io::Read;
use std::time::UNIX_EPOCH;

use fx_core::{
    BoxFuture, PermissionRequest, ReadEvidence, Tool, ToolContext, ToolEffect, ToolError,
    ToolOutput,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::resolve_existing;

const MAX_SNAPSHOT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_MODEL_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_LINE_COUNT: usize = 2_000;
const LINE_TRUNCATED_SUFFIX: &str = "... (line truncated)";

#[derive(Clone, Copy, Debug, Default)]
pub struct ReadFile;

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    #[serde(default = "first_line")]
    start_line: usize,
    #[serde(default = "default_line_count")]
    line_count: usize,
}

fn first_line() -> usize {
    1
}

fn default_line_count() -> usize {
    400
}

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a text file with stable line numbers and bounded output."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer", "minimum": 1 },
                "line_count": { "type": "integer", "minimum": 1 }
            },
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
    let mut input: Input = serde_json::from_value(arguments)
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    input.path = input.path.trim().to_owned();
    if input.path.is_empty() {
        return Err(ToolError::InvalidArguments(
            "read_file field `path` must not be empty".into(),
        ));
    }
    if input.start_line == 0 || input.line_count == 0 {
        return Err(ToolError::InvalidArguments(
            "start_line and line_count must be positive integers".into(),
        ));
    }
    input.line_count = input
        .line_count
        .min(MAX_LINE_COUNT)
        .min(context.limits.max_read_file_lines);

    let resolved = resolve_existing(&context.workspace_root, &input.path)?;
    let mut file = File::open(&resolved.absolute).map_err(|error| {
        ToolError::Execution(format!("{}: {error}", resolved.absolute.display()))
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let file_bytes = metadata.len();
    let mut bytes = Vec::with_capacity(file_bytes.min(MAX_SNAPSHOT_BYTES) as usize);
    file.by_ref()
        .take(MAX_SNAPSHOT_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| ToolError::Execution(error.to_string()))?;
    let snapshot_complete = file_bytes <= MAX_SNAPSHOT_BYTES;
    let display = resolved.display.to_string_lossy();

    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(ToolOutput {
            content: format!(
                "<path>{display}</path>\n<content>binary or non-utf8 file omitted ({file_bytes} bytes)</content>"
            ),
            is_error: false,
            structured: None,
            original_bytes: usize::try_from(file_bytes).unwrap_or(usize::MAX),
            truncated: true,
            durable_content: None,
        });
    };
    if text.contains('\0') {
        return Ok(ToolOutput {
            content: format!(
                "<path>{display}</path>\n<content>binary or non-utf8 file omitted ({file_bytes} bytes)</content>"
            ),
            is_error: false,
            structured: None,
            original_bytes: usize::try_from(file_bytes).unwrap_or(usize::MAX),
            truncated: true,
            durable_content: None,
        });
    }

    let content_hash = Sha256::digest(&bytes).into();

    let lines: Vec<&str> = text.split_terminator('\n').collect();
    let total_lines = lines.len();
    let start_index = input.start_line.saturating_sub(1);
    let mut records = Vec::new();
    let mut rendered_bytes = 0usize;
    let mut display_truncated = false;
    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(start_index)
        .take(input.line_count)
    {
        let number = index + 1;
        let (line, line_truncated) = truncate_utf8(line, context.limits.max_read_file_line_bytes);
        let suffix = if line_truncated {
            LINE_TRUNCATED_SUFFIX
        } else {
            ""
        };
        let record_bytes = digits(number) + 1 + line.len() + suffix.len() + 1;
        if rendered_bytes + record_bytes > MAX_MODEL_OUTPUT_BYTES {
            display_truncated = true;
            break;
        }
        rendered_bytes += record_bytes;
        display_truncated |= line_truncated;
        records.push((number, line, suffix));
    }
    if start_index.saturating_add(records.len()) < total_lines {
        display_truncated = true;
    }

    let mut content = format!("<path>{display}</path>\n<content>\n");
    if records.is_empty() && total_lines > 0 && input.start_line > total_lines {
        content.push_str(&format!(
            "... [start_line {} is beyond end of file; total lines {}]\n",
            input.start_line, total_lines
        ));
    } else if let Some((last, _, _)) = records.last() {
        let width = digits(*last);
        for (number, line, suffix) in &records {
            content.push_str(&format!("{number:<width$}\t{line}{suffix}\n"));
        }
    }
    let model_complete = snapshot_complete
        && !display_truncated
        && input.start_line == 1
        && records.len() == total_lines;
    if let Some(store) = &context.read_evidence {
        store.record(
            resolved.absolute.clone(),
            ReadEvidence {
                modified_ns: modified_ns(&metadata),
                content_hash,
                model_view_covers_full_file: model_complete,
                snapshot_covers_full_file: snapshot_complete,
            },
        );
    }
    if !model_complete && (!records.is_empty() || display_truncated) {
        if snapshot_complete {
            content.push_str(&format!(
                "... [showing {} of {} lines; use start_line/line_count to read more.]\n",
                records.len(),
                total_lines
            ));
        } else {
            content.push_str(&format!(
                "... [showing {} of at least {} lines; file snapshot was capped before EOF.]\n",
                records.len(),
                total_lines
            ));
        }
    }
    content.push_str("</content>");

    Ok(ToolOutput {
        content,
        is_error: false,
        structured: None,
        original_bytes: usize::try_from(file_bytes).unwrap_or(usize::MAX),
        truncated: !model_complete,
        durable_content: None,
    })
}

fn modified_ns(metadata: &Metadata) -> i128 {
    let Ok(modified) = metadata.modified() else {
        return 0;
    };
    match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => i128::try_from(duration.as_nanos()).unwrap_or(i128::MAX),
        Err(error) => -i128::try_from(error.duration().as_nanos()).unwrap_or(i128::MAX),
    }
}

fn truncate_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn digits(mut value: usize) -> usize {
    let mut count = 1;
    while value >= 10 {
        value /= 10;
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use fx_core::{MemoryReadEvidenceStore, ReadEvidenceStore};
    use sha2::{Digest, Sha256};

    use super::*;

    fn fixture(name: &str) -> (PathBuf, ToolContext) {
        let root = std::env::temp_dir().join(format!("fx-read-tool-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let context = ToolContext::new(root.clone());
        (root, context)
    }

    #[test]
    fn reads_ranges_with_zig_compatible_output() {
        let (root, context) = fixture("ranges");
        fs::write(root.join("file.txt"), "one\ntwo\nthree\n").unwrap();
        let output = execute(
            &context,
            json!({"path": "file.txt", "start_line": 2, "line_count": 1}),
        )
        .unwrap();
        assert_eq!(
            output.content,
            "<path>file.txt</path>\n<content>\n2\ttwo\n... [showing 1 of 3 lines; use start_line/line_count to read more.]\n</content>"
        );
        assert!(output.truncated);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn omits_non_utf8_content() {
        let (root, context) = fixture("binary");
        fs::write(root.join("binary"), [0xff, 0x00]).unwrap();
        let output = execute(&context, json!({"path": "binary"})).unwrap();
        assert!(
            output
                .content
                .contains("binary or non-utf8 file omitted (2 bytes)")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_text_read_records_snapshot_and_model_coverage_separately() {
        let (root, mut context) = fixture("evidence");
        let path = root.join("file.txt");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let store = Arc::new(MemoryReadEvidenceStore::default());
        context.read_evidence = Some(store.clone());

        execute(
            &context,
            json!({"path": "file.txt", "start_line": 2, "line_count": 1}),
        )
        .unwrap();
        let evidence = store.lookup(&path.canonicalize().unwrap()).unwrap();
        assert!(!evidence.model_view_covers_full_file);
        assert!(evidence.snapshot_covers_full_file);
        assert_eq!(
            evidence.content_hash,
            <[u8; 32]>::from(Sha256::digest(b"one\ntwo\nthree\n"))
        );
        fs::remove_dir_all(root).unwrap();
    }
}
