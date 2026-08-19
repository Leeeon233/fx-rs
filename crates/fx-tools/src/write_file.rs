use fx_core::{BoxFuture, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolPreparation};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_mutation::{Mutation, prepare_file_mutation};
use crate::resolve_target;

#[derive(Clone, Copy, Debug, Default)]
pub struct WriteFile;

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    content: String,
}

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Create or atomically replace a file after a reviewable content preview."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
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
        let input: Input = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        Ok(vec![
            resolve_target(&context.workspace_root, &input.path)?.absolute,
        ])
    }

    fn prepare(
        &self,
        context: &ToolContext,
        arguments: &Value,
    ) -> Result<ToolPreparation, ToolError> {
        let input: Input = serde_json::from_value(arguments.clone())
            .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
        prepare_file_mutation(
            context,
            self.name(),
            input.path,
            Mutation::Write(input.content.into_bytes()),
        )
        .map(ToolPreparation::Prepared)
    }

    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        _arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async {
            Err(ToolError::PermissionDenied(
                "write_file execution requires canonical tool runtime authorization".into(),
            ))
        })
    }
}
