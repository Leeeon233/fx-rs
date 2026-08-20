use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ToolEffect;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    Ask,
    #[default]
    Auto,
    Yolo,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionRule {
    pub permission: String,
    pub pattern: String,
    pub action: PermissionAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequest {
    pub permission: String,
    /// Human-facing and configured-rule matching target.
    pub target: String,
    /// Exact session-grant identity. Defaults to `target`, but adapters may
    /// bind hidden execution context such as cwd or shell profile.
    pub grant_target: String,
    pub effect: ToolEffect,
}

impl PermissionRequest {
    pub fn new(
        permission: impl Into<String>,
        target: impl Into<String>,
        effect: ToolEffect,
    ) -> Self {
        let target = target.into();
        Self {
            permission: permission.into(),
            grant_target: target.clone(),
            target,
            effect,
        }
    }

    pub fn with_grant_target(mut self, target: impl Into<String>) -> Self {
        self.grant_target = target.into();
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionDecision {
    Allow,
    Ask,
    AutoReview,
    Deny,
}

/// Pure permission policy shared by interactive and noninteractive adapters.
#[derive(Clone, Debug, Default)]
pub struct PermissionEngine {
    mode: PermissionMode,
    configured_rules: Vec<PermissionRule>,
    session_grants: Vec<(String, String)>,
}

impl PermissionEngine {
    pub fn new(mode: PermissionMode, configured_rules: Vec<PermissionRule>) -> Self {
        Self {
            mode,
            configured_rules,
            session_grants: Vec::new(),
        }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    /// Changes the session baseline without discarding configured rules or
    /// approvals already granted for this session.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.configured_rules
    }

    pub fn grants(&self) -> &[(String, String)] {
        &self.session_grants
    }

    pub fn grant_for_session(&mut self, permission: &str, target: &str) {
        let entry = (permission.to_owned(), target.to_owned());
        if !self.session_grants.contains(&entry) {
            self.session_grants.push(entry);
        }
    }

    pub fn grant_request_for_session(&mut self, request: &PermissionRequest) {
        self.grant_for_session(&request.permission, &request.grant_target);
    }

    /// Resolves a request using the original precedence: yolo, configured
    /// denies, exact session grants, configured allow/ask, then mode baseline.
    pub fn decide(&self, request: &PermissionRequest) -> PermissionDecision {
        if self.mode == PermissionMode::Yolo {
            return PermissionDecision::Allow;
        }

        let mut configured = None;
        for rule in self
            .configured_rules
            .iter()
            .filter(|rule| rule.permission == request.permission || rule.permission == "*")
            .filter(|rule| pattern_matches(&rule.pattern, &request.target))
        {
            configured = Some(match rule.action {
                PermissionAction::Allow => PermissionDecision::Allow,
                PermissionAction::Ask => PermissionDecision::Ask,
                PermissionAction::Deny => PermissionDecision::Deny,
            });
        }

        if configured == Some(PermissionDecision::Deny) {
            return PermissionDecision::Deny;
        }

        if self.session_grants.iter().any(|(permission, target)| {
            permission == &request.permission && target == &request.grant_target
        }) {
            return PermissionDecision::Allow;
        }

        configured.unwrap_or(match self.mode {
            PermissionMode::Ask => PermissionDecision::Ask,
            PermissionMode::Auto => automatic_baseline(request.effect),
            PermissionMode::Yolo => PermissionDecision::Allow,
        })
    }
}

fn automatic_baseline(effect: ToolEffect) -> PermissionDecision {
    match effect {
        ToolEffect::Read => PermissionDecision::Allow,
        ToolEffect::Write
        | ToolEffect::Process
        | ToolEffect::Network
        | ToolEffect::UserInteraction
        | ToolEffect::Delegation => PermissionDecision::AutoReview,
    }
}

fn pattern_matches(pattern: &str, target: &str) -> bool {
    if pattern == "*" || pattern == target {
        return true;
    }
    let Some(prefix) = pattern.strip_suffix("/**") else {
        return false;
    };
    let prefix = Path::new(prefix);
    PathBuf::from(target).starts_with(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(effect: ToolEffect) -> PermissionRequest {
        PermissionRequest::new("write_file", "/repo/src/lib.rs", effect)
    }

    #[test]
    fn configured_deny_wins_over_session_grant() {
        let mut engine = PermissionEngine::new(
            PermissionMode::Auto,
            vec![PermissionRule {
                permission: "write_file".into(),
                pattern: "/repo/**".into(),
                action: PermissionAction::Deny,
            }],
        );
        engine.grant_for_session("write_file", "/repo/src/lib.rs");

        assert_eq!(
            engine.decide(&request(ToolEffect::Write)),
            PermissionDecision::Deny
        );
    }

    #[test]
    fn auto_allows_reads_and_reviews_mutations() {
        let engine = PermissionEngine::new(PermissionMode::Auto, Vec::new());
        assert_eq!(
            engine.decide(&request(ToolEffect::Read)),
            PermissionDecision::Allow
        );
        assert_eq!(
            engine.decide(&request(ToolEffect::Write)),
            PermissionDecision::AutoReview
        );
    }

    #[test]
    fn later_matching_configured_rule_wins_within_one_layer() {
        let engine = PermissionEngine::new(
            PermissionMode::Auto,
            vec![
                PermissionRule {
                    permission: "write_file".into(),
                    pattern: "/repo/**".into(),
                    action: PermissionAction::Deny,
                },
                PermissionRule {
                    permission: "write_file".into(),
                    pattern: "/repo/src/lib.rs".into(),
                    action: PermissionAction::Allow,
                },
            ],
        );

        assert_eq!(
            engine.decide(&request(ToolEffect::Write)),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn configured_ask_stays_interactive_in_auto_mode() {
        let engine = PermissionEngine::new(
            PermissionMode::Auto,
            vec![PermissionRule {
                permission: "write_file".into(),
                pattern: "/repo/**".into(),
                action: PermissionAction::Ask,
            }],
        );
        assert_eq!(
            engine.decide(&request(ToolEffect::Write)),
            PermissionDecision::Ask
        );
    }
}
