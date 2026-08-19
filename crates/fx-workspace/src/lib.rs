//! Shared workspace path authority and symlink-containment rules.
//!
//! A relative path normally carries workspace authority. Absolute, home, and
//! lexically escaping paths are explicit external intent. Consumers still
//! apply their own permission policy after resolution.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use fx_core::ToolError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathIntent {
    Workspace,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedPath {
    pub absolute: PathBuf,
    pub display: PathBuf,
    pub intent: PathIntent,
}

/// Resolves an existing path while preserving the caller's authority intent.
///
/// Absolute, home-relative, and lexically escaping relative inputs explicitly
/// request an external path. A workspace-relative input may not escape through
/// a symlink after canonicalization.
pub fn resolve_existing(workspace_root: &Path, input: &str) -> Result<ResolvedPath, ToolError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ToolError::InvalidArguments("path must not be empty".into()));
    }

    let canonical_workspace = workspace_root
        .canonicalize()
        .map_err(|error| ToolError::Execution(format!("workspace is unavailable: {error}")))?;
    let (candidate, intent) = classify_input(&canonical_workspace, input)?;
    let absolute = candidate
        .canonicalize()
        .map_err(|error| ToolError::Execution(format!("{}: {error}", candidate.display())))?;

    if intent == PathIntent::Workspace && !absolute.starts_with(&canonical_workspace) {
        return Err(ToolError::OutsideWorkspace(absolute.display().to_string()));
    }

    let display = absolute
        .strip_prefix(&canonical_workspace)
        .map(Path::to_owned)
        .unwrap_or_else(|_| absolute.clone());
    Ok(ResolvedPath {
        absolute,
        display,
        intent,
    })
}

/// Resolves a file target that may not exist yet.
///
/// The deepest existing ancestor is canonicalized before missing components
/// are appended. Consequently a workspace-relative target cannot be redirected
/// outside the workspace by an existing symlink, while explicit absolute,
/// home-relative, and lexically escaping paths retain external intent.
pub fn resolve_target(workspace_root: &Path, input: &str) -> Result<ResolvedPath, ToolError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ToolError::InvalidArguments("path must not be empty".into()));
    }

    let canonical_workspace = workspace_root
        .canonicalize()
        .map_err(|error| ToolError::Execution(format!("workspace is unavailable: {error}")))?;
    let (candidate, intent) = classify_input(&canonical_workspace, input)?;

    let mut ancestor = candidate.as_path();
    let mut missing = Vec::new();
    let absolute = loop {
        match ancestor.canonicalize() {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                break canonical;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs_entry_is_symlink(ancestor) {
                    validate_dangling_symlink_scope(
                        ancestor,
                        &missing,
                        intent,
                        &canonical_workspace,
                    )?;
                }
                let name = ancestor.file_name().ok_or_else(|| {
                    ToolError::Execution(format!("{}: {error}", candidate.display()))
                })?;
                missing.push(name.to_owned());
                ancestor = ancestor.parent().ok_or_else(|| {
                    ToolError::Execution(format!("{}: {error}", candidate.display()))
                })?;
            }
            Err(error) => {
                return Err(ToolError::Execution(format!(
                    "{}: {error}",
                    candidate.display()
                )));
            }
        }
    };

    if intent == PathIntent::Workspace && !absolute.starts_with(&canonical_workspace) {
        return Err(ToolError::OutsideWorkspace(absolute.display().to_string()));
    }

    let display = absolute
        .strip_prefix(&canonical_workspace)
        .map(Path::to_owned)
        .unwrap_or_else(|_| absolute.clone());
    Ok(ResolvedPath {
        absolute,
        display,
        intent,
    })
}

fn fs_entry_is_symlink(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn validate_dangling_symlink_scope(
    symlink: &Path,
    missing_tail: &[OsString],
    intent: PathIntent,
    workspace: &Path,
) -> Result<(), ToolError> {
    if intent == PathIntent::External {
        return Ok(());
    }
    let link = std::fs::read_link(symlink)
        .map_err(|error| ToolError::Execution(format!("{}: {error}", symlink.display())))?;
    let mut target = if link.is_absolute() {
        lexical_normalize(&link)
    } else {
        lexical_normalize(&symlink.parent().unwrap_or(workspace).join(link))
    };
    for component in missing_tail.iter().rev() {
        target.push(component);
    }
    let resolved_target = canonicalize_deepest(&target)?;
    if !resolved_target.starts_with(workspace) {
        return Err(ToolError::OutsideWorkspace(
            resolved_target.display().to_string(),
        ));
    }
    Ok(())
}

fn canonicalize_deepest(path: &Path) -> Result<PathBuf, ToolError> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match ancestor.canonicalize() {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| ToolError::Execution(format!("{}: {error}", path.display())))?;
                missing.push(name.to_owned());
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| ToolError::Execution(format!("{}: {error}", path.display())))?;
            }
            Err(error) => {
                return Err(ToolError::Execution(format!("{}: {error}", path.display())));
            }
        }
    }
}

fn classify_input(workspace: &Path, input: &str) -> Result<(PathBuf, PathIntent), ToolError> {
    let path = Path::new(input);
    if path.is_absolute() {
        return Ok((lexical_normalize(path), PathIntent::External));
    }

    if input == "~" || input.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                ToolError::InvalidArguments("HOME is not set to an absolute path".into())
            })?;
        let relative = input.strip_prefix('~').unwrap_or_default();
        return Ok((
            lexical_normalize(&home.join(relative.trim_start_matches('/'))),
            PathIntent::External,
        ));
    }
    if input.starts_with('~') {
        return Err(ToolError::InvalidArguments(
            "only `~` and `~/...` home paths are supported".into(),
        ));
    }

    let candidate = lexical_normalize(&workspace.join(path));
    let intent = if candidate.starts_with(workspace) {
        PathIntent::Workspace
    } else {
        PathIntent::External
    };
    Ok((candidate, intent))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut prefix = None;
    let mut rooted = false;
    let mut parts: Vec<OsString> = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = Some(value.as_os_str().to_owned()),
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if !parts.is_empty() {
                    parts.pop();
                }
            }
            Component::Normal(value) => parts.push(value.to_owned()),
        }
    }

    let mut result = PathBuf::new();
    if let Some(value) = prefix {
        result.push(value);
    }
    if rooted {
        result.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }
    result.extend(parts);
    result
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("fx-path-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("workspace/inside")).unwrap();
        fs::create_dir_all(root.join("external")).unwrap();
        fs::write(root.join("workspace/inside/file.txt"), "inside").unwrap();
        fs::write(root.join("external/file.txt"), "outside").unwrap();
        root
    }

    #[test]
    fn relative_escape_is_explicit_external_intent() {
        let root = fixture("relative-external");
        let workspace = root.join("workspace").canonicalize().unwrap();
        let resolved = resolve_existing(&workspace, "../external/file.txt").unwrap();
        assert_eq!(resolved.intent, PathIntent::External);
        assert!(resolved.display.is_absolute());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workspace_relative_symlink_cannot_escape() {
        use std::os::unix::fs::symlink;

        let root = fixture("symlink-escape");
        let workspace = root.join("workspace").canonicalize().unwrap();
        symlink(root.join("external"), workspace.join("escape")).unwrap();
        let error = resolve_existing(&workspace, "escape/file.txt").unwrap_err();
        assert!(matches!(error, ToolError::OutsideWorkspace(_)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_workspace_target_is_resolved_from_existing_ancestor() {
        let root = fixture("missing-target");
        let workspace = root.join("workspace").canonicalize().unwrap();
        let resolved = resolve_target(&workspace, "new/nested/file.txt").unwrap();
        assert_eq!(resolved.absolute, workspace.join("new/nested/file.txt"));
        assert_eq!(resolved.display, PathBuf::from("new/nested/file.txt"));
        assert_eq!(resolved.intent, PathIntent::Workspace);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn missing_target_below_external_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = fixture("missing-symlink-escape");
        let workspace = root.join("workspace").canonicalize().unwrap();
        symlink(root.join("external"), workspace.join("escape")).unwrap();
        let error = resolve_target(&workspace, "escape/new.txt").unwrap_err();
        assert!(matches!(error, ToolError::OutsideWorkspace(_)));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn dangling_target_symlink_outside_workspace_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = fixture("dangling-symlink-escape");
        let workspace = root.join("workspace").canonicalize().unwrap();
        symlink(
            root.join("external/missing.txt"),
            workspace.join("dangling"),
        )
        .unwrap();
        let error = resolve_target(&workspace, "dangling").unwrap_err();
        assert!(matches!(error, ToolError::OutsideWorkspace(_)));
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn safe_dangling_target_symlink_keeps_entry_path() {
        use std::os::unix::fs::symlink;

        let root = fixture("safe-dangling-symlink");
        let workspace = root.join("workspace").canonicalize().unwrap();
        symlink("missing.txt", workspace.join("dangling")).unwrap();
        let resolved = resolve_target(&workspace, "dangling").unwrap();
        assert_eq!(resolved.absolute, workspace.join("dangling"));
        fs::remove_dir_all(root).unwrap();
    }
}
