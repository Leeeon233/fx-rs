use std::path::Path;

use fx_core::{SandboxMode, ToolContext, ToolError};

pub(crate) const MACOS_SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

pub(crate) fn profile(context: &ToolContext) -> Result<Option<String>, ToolError> {
    match context.sandbox {
        SandboxMode::None => Ok(None),
        SandboxMode::Os => platform_profile(context).map(Some),
    }
}

#[cfg(target_os = "macos")]
fn platform_profile(context: &ToolContext) -> Result<String, ToolError> {
    if !Path::new(MACOS_SANDBOX_EXEC).is_file() {
        return Err(ToolError::Execution(
            "operating system sandbox is unavailable on this host".into(),
        ));
    }
    let workspace = context.workspace_root.canonicalize().map_err(|error| {
        ToolError::Execution(format!("workspace is unavailable for sandboxing: {error}"))
    })?;
    let mut profile = String::from("(version 1)\n(deny default)\n(allow file-read*)\n");
    write_subpath(&mut profile, "file-write*", &workspace);
    for root in &context.additional_roots {
        let root = root.canonicalize().map_err(|error| {
            ToolError::Execution(format!(
                "additional sandbox root {} is unavailable: {error}",
                root.display()
            ))
        })?;
        write_subpath(&mut profile, "file-write*", &root);
    }
    for path in ["/tmp", "/private/tmp", "/dev"] {
        write_subpath(&mut profile, "file-write*", Path::new(path));
    }
    profile.push_str(
        "(allow process-exec)\n\
         (allow process-fork)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow network-outbound)\n\
         (allow signal)\n\
         (allow iokit-open)\n",
    );
    Ok(profile)
}

#[cfg(not(target_os = "macos"))]
fn platform_profile(_context: &ToolContext) -> Result<String, ToolError> {
    Err(ToolError::Execution(
        "operating system sandbox is unavailable on this host".into(),
    ))
}

#[cfg(target_os = "macos")]
fn write_subpath(output: &mut String, operation: &str, path: &Path) {
    output.push_str("(allow ");
    output.push_str(operation);
    output.push_str(" (subpath \"");
    for character in path.to_string_lossy().chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '"' => output.push_str("\\\""),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character => output.push(character),
        }
    }
    output.push_str("\"))\n");
}

pub(crate) fn denied(stderr: &str) -> bool {
    stderr.contains("Sandbox: deny")
        || stderr.contains("sandbox-exec: sandbox_apply")
        || stderr.contains("Operation not permitted")
        || stderr.contains("operation not permitted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_sandbox_style_stderr() {
        assert!(denied("Sandbox: deny(1) file-write-create /outside"));
        assert!(denied("open: Operation not permitted"));
        assert!(denied("zsh: operation not permitted: /outside"));
        assert!(!denied("ordinary command failure"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn profile_allows_workspace_writes_but_not_inbound_network() {
        let workspace = std::env::current_dir().unwrap();
        let mut context = ToolContext::new(workspace.clone());
        context.sandbox = SandboxMode::Os;
        let profile = profile(&context).unwrap().unwrap();
        assert!(profile.contains(&workspace.display().to_string()));
        assert!(profile.contains("network-outbound"));
        assert!(!profile.contains("network-inbound"));
    }
}
