//! Typed configuration loading with fx-compatible precedence.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use fx_core::{PermissionAction, PermissionMode, PermissionRule, SandboxMode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_MAX_AGENT_STEPS: usize = 0;
pub const DEFAULT_MAX_TOOL_RESULT_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpdateChannel {
    #[default]
    Stable,
    Dev,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct SettingsFile {
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    sandbox: Option<String>,
    max_agent_steps: Option<usize>,
    max_tool_result_bytes: Option<usize>,
    update_channel: Option<UpdateChannel>,
    permission: Option<serde_json::Value>,
    #[serde(default)]
    workspaces: BTreeMap<String, SettingsFile>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ProjectFile {
    max_agent_steps: Option<usize>,
    max_tool_result_bytes: Option<usize>,
    sandbox: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub model: Option<String>,
    pub permission_mode: PermissionMode,
    pub sandbox: SandboxMode,
    pub max_agent_steps: usize,
    pub max_tool_result_bytes: usize,
    pub update_channel: UpdateChannel,
    pub permission_rules: Vec<PermissionRule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: None,
            permission_mode: PermissionMode::Auto,
            sandbox: if cfg!(target_os = "macos") {
                SandboxMode::Os
            } else {
                SandboxMode::None
            },
            max_agent_steps: DEFAULT_MAX_AGENT_STEPS,
            max_tool_result_bytes: DEFAULT_MAX_TOOL_RESULT_BYTES,
            update_channel: UpdateChannel::Stable,
            permission_rules: Vec::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("could not read configuration at {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration at {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("max_tool_result_bytes must be greater than zero")]
    InvalidToolResultLimit,
    #[error("invalid permission configuration: {0}")]
    InvalidPermission(String),
    #[error("invalid sandbox mode `{0}`; expected `os` or `none`")]
    InvalidSandbox(String),
    #[error("operating system sandbox is unavailable on this host")]
    UnsupportedSandbox,
}

/// Loads built-ins, project-safe defaults, profile globals, workspace profile
/// overrides, then environment variables in ascending precedence.
pub fn load(home: Option<&Path>, workspace: &Path) -> Result<Config, ConfigError> {
    load_with_env(home, workspace, |key| std::env::var(key).ok())
}

pub fn load_with_env(
    home: Option<&Path>,
    workspace: &Path,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Config, ConfigError> {
    let mut config = Config::default();

    let project_path = workspace.join(".fx.json");
    if let Some(project) = read_optional::<ProjectFile>(&project_path)? {
        apply_project(&mut config, project)?;
    }

    if let Some(home) = home {
        let settings_path = home.join(".fx/settings.json");
        if let Some(mut profile) = read_optional::<SettingsFile>(&settings_path)? {
            let workspace_key = workspace.to_string_lossy().into_owned();
            let workspace_settings = profile.workspaces.remove(&workspace_key);
            apply_settings(&mut config, profile)?;
            if let Some(settings) = workspace_settings {
                apply_settings(&mut config, settings)?;
            }
        }
    }

    if let Some(model) = env("FX_MODEL").filter(|value| !value.trim().is_empty()) {
        config.model = Some(model);
    }
    if let Some(mode) = env("FX_PERMISSION_MODE").and_then(|value| parse_permission_mode(&value)) {
        config.permission_mode = mode;
    }
    if let Some(mode) = env("FX_SANDBOX") {
        config.sandbox = parse_sandbox_mode(&mode)?;
    }
    if let Some(limit) = env("FX_MAX_AGENT_STEPS").and_then(|value| value.trim().parse().ok()) {
        config.max_agent_steps = limit;
    }

    validate(&config)?;
    Ok(config)
}

fn read_optional<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, ConfigError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })
}

fn apply_project(config: &mut Config, project: ProjectFile) -> Result<(), ConfigError> {
    if let Some(value) = project.max_agent_steps {
        config.max_agent_steps = value;
    }
    if let Some(value) = project.max_tool_result_bytes {
        config.max_tool_result_bytes = value;
    }
    if let Some(value) = project.sandbox {
        config.sandbox = parse_sandbox_mode(&value)?;
    }
    Ok(())
}

fn apply_settings(config: &mut Config, settings: SettingsFile) -> Result<(), ConfigError> {
    if let Some(value) = settings.model {
        config.model = Some(value);
    }
    if let Some(value) = settings.permission_mode {
        config.permission_mode = value;
    }
    if let Some(value) = settings.sandbox {
        config.sandbox = parse_sandbox_mode(&value)?;
    }
    if let Some(value) = settings.max_agent_steps {
        config.max_agent_steps = value;
    }
    if let Some(value) = settings.max_tool_result_bytes {
        config.max_tool_result_bytes = value;
    }
    if let Some(value) = settings.update_channel {
        config.update_channel = value;
    }
    if let Some(permission) = settings.permission {
        config.permission_rules = parse_permission_config(permission)?;
    }
    Ok(())
}

fn parse_permission_mode(value: &str) -> Option<PermissionMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "ask" => Some(PermissionMode::Ask),
        "auto" => Some(PermissionMode::Auto),
        "yolo" => Some(PermissionMode::Yolo),
        _ => None,
    }
}

fn parse_sandbox_mode(value: &str) -> Result<SandboxMode, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "none" => Ok(SandboxMode::None),
        "os" | "macos" if cfg!(target_os = "macos") => Ok(SandboxMode::Os),
        "os" | "macos" => Err(ConfigError::UnsupportedSandbox),
        _ => Err(ConfigError::InvalidSandbox(value.to_owned())),
    }
}

fn parse_permission_config(value: serde_json::Value) -> Result<Vec<PermissionRule>, ConfigError> {
    let mut rules = Vec::new();
    match value {
        serde_json::Value::String(action) => {
            rules.push(PermissionRule {
                permission: "*".into(),
                pattern: "*".into(),
                action: parse_permission_action(&action)?,
            });
        }
        serde_json::Value::Object(permissions) => {
            for (permission, value) in permissions {
                let permission = permission.trim();
                if permission.is_empty() {
                    return Err(ConfigError::InvalidPermission(
                        "permission name must not be empty".into(),
                    ));
                }
                match value {
                    serde_json::Value::String(action) => rules.push(PermissionRule {
                        permission: permission.into(),
                        pattern: "*".into(),
                        action: parse_permission_action(&action)?,
                    }),
                    serde_json::Value::Object(patterns) => {
                        for (pattern, action) in patterns {
                            let serde_json::Value::String(action) = action else {
                                return Err(ConfigError::InvalidPermission(format!(
                                    "action for `{permission}` and `{pattern}` must be a string"
                                )));
                            };
                            rules.push(PermissionRule {
                                permission: permission.into(),
                                pattern: pattern.trim().into(),
                                action: parse_permission_action(&action)?,
                            });
                        }
                    }
                    _ => {
                        return Err(ConfigError::InvalidPermission(format!(
                            "rules for `{permission}` must be an action or pattern object"
                        )));
                    }
                }
            }
        }
        _ => {
            return Err(ConfigError::InvalidPermission(
                "permission must be an action or object".into(),
            ));
        }
    }
    Ok(rules)
}

fn parse_permission_action(value: &str) -> Result<PermissionAction, ConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "allow" => Ok(PermissionAction::Allow),
        "ask" => Ok(PermissionAction::Ask),
        "deny" => Ok(PermissionAction::Deny),
        _ => Err(ConfigError::InvalidPermission(format!(
            "unknown action `{value}`"
        ))),
    }
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    if config.max_tool_result_bytes == 0 {
        return Err(ConfigError::InvalidToolResultLimit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn precedence_matches_environment_workspace_profile_project_defaults() {
        let root = std::env::temp_dir().join(format!(
            "fx-config-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("precedence")
        ));
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(home.join(".fx")).unwrap();
        fs::create_dir_all(&workspace).unwrap();

        fs::write(
            workspace.join(".fx.json"),
            r#"{"max_agent_steps":1,"max_tool_result_bytes":100}"#,
        )
        .unwrap();
        let workspace_key = workspace.to_string_lossy();
        fs::write(
            home.join(".fx/settings.json"),
            format!(
                r#"{{"model":"global","max_agent_steps":2,"workspaces":{{"{workspace_key}":{{"model":"workspace","max_agent_steps":3}}}}}}"#
            ),
        )
        .unwrap();

        let env = HashMap::from([
            ("FX_MODEL", "environment"),
            ("FX_MAX_AGENT_STEPS", "4"),
            ("FX_PERMISSION_MODE", "ask"),
        ]);
        let config = load_with_env(Some(&home), &workspace, |key| {
            env.get(key).map(ToString::to_string)
        })
        .unwrap();

        assert_eq!(config.model.as_deref(), Some("environment"));
        assert_eq!(config.max_agent_steps, 4);
        assert_eq!(config.max_tool_result_bytes, 100);
        assert_eq!(config.permission_mode, PermissionMode::Ask);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_file_cannot_set_profile_owned_keys() {
        let root =
            std::env::temp_dir().join(format!("fx-project-safe-test-{}", std::process::id()));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        fs::write(
            workspace.join(".fx.json"),
            r#"{"model":"unsafe","permission_mode":"yolo","max_agent_steps":7}"#,
        )
        .unwrap();

        let config = load_with_env(None, &workspace, |_| None).unwrap();
        assert_eq!(config.model, None);
        assert_eq!(config.permission_mode, PermissionMode::Auto);
        assert_eq!(config.max_agent_steps, 7);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_follows_project_profile_workspace_and_environment_precedence() {
        let root =
            std::env::temp_dir().join(format!("fx-sandbox-config-test-{}", std::process::id()));
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(home.join(".fx")).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::write(workspace.join(".fx.json"), r#"{"sandbox":"none"}"#).unwrap();
        let workspace_key = workspace.to_string_lossy();
        fs::write(
            home.join(".fx/settings.json"),
            format!(
                r#"{{"sandbox":"os","workspaces":{{"{workspace_key}":{{"sandbox":"none"}}}}}}"#
            ),
        )
        .unwrap();

        let config = load_with_env(Some(&home), &workspace, |key| {
            (key == "FX_SANDBOX").then(|| "os".to_owned())
        })
        .unwrap();
        assert_eq!(config.sandbox, SandboxMode::Os);

        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn rejects_os_sandbox_on_unsupported_platforms() {
        let workspace = std::env::temp_dir();
        let error = load_with_env(None, &workspace, |key| {
            (key == "FX_SANDBOX").then(|| "os".to_owned())
        })
        .unwrap_err();
        assert!(matches!(error, ConfigError::UnsupportedSandbox));
    }

    #[test]
    fn permission_object_preserves_order_and_workspace_layer_replaces_global() {
        let root = std::env::temp_dir().join(format!("fx-permission-test-{}", std::process::id()));
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(home.join(".fx")).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        let workspace_key = workspace.to_string_lossy();
        fs::write(
            home.join(".fx/settings.json"),
            format!(
                r#"{{"permission":{{"bash":"deny"}},"workspaces":{{"{workspace_key}":{{"permission":{{"*":"ask","bash":{{"git *":"allow","git push *":"deny"}},"edit":"deny"}}}}}}}}"#
            ),
        )
        .unwrap();

        let config = load_with_env(Some(&home), &workspace, |_| None).unwrap();
        let observed: Vec<_> = config
            .permission_rules
            .iter()
            .map(|rule| (rule.permission.as_str(), rule.pattern.as_str(), rule.action))
            .collect();
        assert_eq!(
            observed,
            [
                ("*", "*", PermissionAction::Ask),
                ("bash", "git *", PermissionAction::Allow),
                ("bash", "git push *", PermissionAction::Deny),
                ("edit", "*", PermissionAction::Deny),
            ]
        );

        fs::remove_dir_all(root).unwrap();
    }
}
