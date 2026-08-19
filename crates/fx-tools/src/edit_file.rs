use fx_core::{BoxFuture, Tool, ToolContext, ToolEffect, ToolError, ToolOutput, ToolPreparation};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::file_mutation::{Mutation, prepare_file_mutation};
use crate::resolve_target;

#[derive(Clone, Copy, Debug, Default)]
pub struct EditFile;

#[derive(Debug, Deserialize)]
struct Input {
    path: String,
    old_string: String,
    new_string: String,
}

impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Atomically replace one exact, unique string in an existing file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" }
            },
            "required": ["path", "old_string", "new_string"],
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
            Mutation::Edit {
                old_string: input.old_string.into_bytes(),
                new_string: input.new_string.into_bytes(),
            },
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
                "edit_file execution requires canonical tool runtime authorization".into(),
            ))
        })
    }
}
