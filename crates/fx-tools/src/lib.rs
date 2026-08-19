//! Built-in tool implementations for native hosts.
//!
//! Tool implementations depend only on `fx-core` contracts. The CLI and SDK
//! choose which tools to register, keeping unused capabilities out of a host's
//! dependency graph.

mod direct_fs;
mod edit_file;
mod file_info;
mod file_mutation;
mod glob_files;
mod grep_files;
mod list_files;
mod read_file;
mod semantic_search;
mod write_file;

pub use direct_fs::{CopyFile, CreateFolder, DeleteFile, RenameFile};
pub use edit_file::EditFile;
pub use file_info::FileInfo;
pub use fx_workspace::{PathIntent, ResolvedPath, resolve_existing, resolve_target};
pub use glob_files::GlobFiles;
pub use grep_files::GrepFiles;
pub use list_files::ListFiles;
pub use read_file::ReadFile;
pub use semantic_search::SemanticSearch;
pub use write_file::WriteFile;

use fx_core::{PermissionRequest, RegistryError, ToolContext, ToolEffect, ToolError, ToolRegistry};

fn read_path_permission(
    tool: &str,
    context: &ToolContext,
    path: &str,
) -> Result<Vec<PermissionRequest>, ToolError> {
    let resolved = resolve_existing(&context.workspace_root, path)?;
    Ok(vec![PermissionRequest::new(
        tool,
        resolved.absolute.display().to_string(),
        ToolEffect::Read,
    )])
}

/// Registers the filesystem observation tools in their stable advertisement
/// order.
pub fn register_read_tools(registry: &mut ToolRegistry) -> Result<(), RegistryError> {
    registry.register(ReadFile)?;
    registry.register(ListFiles)?;
    registry.register(GlobFiles)?;
    registry.register(GrepFiles)?;
    registry.register(SemanticSearch)?;
    registry.register(FileInfo)?;
    Ok(())
}

/// Registers filesystem mutations in the original stable advertisement order.
pub fn register_mutation_tools(registry: &mut ToolRegistry) -> Result<(), RegistryError> {
    registry.register(WriteFile)?;
    registry.register(EditFile)?;
    registry.register(CreateFolder)?;
    registry.register(CopyFile)?;
    registry.register(RenameFile)?;
    registry.register(DeleteFile)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    #[test]
    fn registration_preserves_gateway_advertisement_order() {
        let mut registry = ToolRegistry::default();
        register_read_tools(&mut registry).unwrap();
        let names: Vec<_> = registry
            .advertisements()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            names,
            [
                "read_file",
                "list_files",
                "glob_files",
                "grep_files",
                "semantic_search",
                "file_info"
            ]
        );
    }

    #[test]
    fn mutation_registration_preserves_gateway_advertisement_order() {
        let mut registry = ToolRegistry::default();
        register_mutation_tools(&mut registry).unwrap();
        let names: Vec<_> = registry
            .advertisements()
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(
            names,
            [
                "write_file",
                "edit_file",
                "create_folder",
                "copy_file",
                "rename_file",
                "delete_file"
            ]
        );
    }

    #[test]
    fn read_permissions_bind_the_resolved_target_instead_of_the_workspace() {
        let root = std::env::temp_dir().join(format!("fx-read-permission-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/file.txt"), "text").unwrap();
        let context = ToolContext::new(root.clone());
        let mut registry = ToolRegistry::default();
        register_read_tools(&mut registry).unwrap();

        let request = registry
            .get("read_file")
            .unwrap()
            .permission_requests(&context, &json!({"path": "nested/file.txt"}))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            request.target,
            root.join("nested/file.txt")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
