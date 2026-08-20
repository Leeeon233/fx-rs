//! Secure, provider-neutral skill discovery and invocation.
//!
//! Discovery is intentionally read-only. Installation and repository cloning
//! are separate capabilities so an ACP host can advertise skills without also
//! acquiring ambient write or network authority.

mod install;
mod secure_fs;

pub use install::InstallSkillTool;

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use secure_fs::SecureDir;
use serde::Deserialize;
use serde_json::{Value, json};
use thiserror::Error;

const MAX_FRONTMATTER_BYTES: usize = 64 * 1024;
const MAX_NAME_BYTES: usize = 256;
const MAX_DESCRIPTION_BYTES: usize = 4 * 1024;
const MAX_DESCRIPTION_PROMPT_BYTES: usize = 1024;
const MAX_CATALOG_PROMPT_BYTES: usize = 16 * 1024;
const MAX_RESOURCE_BYTES: usize = 1024 * 1024;
const DEFAULT_CHUNK_BYTES: usize = 20 * 1024;
const SKILL_FILE: &str = "SKILL.md";

const WORKSPACE_ROOTS: &[(&str, SkillSource)] = &[
    ("skills", SkillSource::WorkspaceShared),
    (".opencode/skills", SkillSource::WorkspaceOpenCode),
    (".codex/skills", SkillSource::WorkspaceCodex),
    (".claude/skills", SkillSource::WorkspaceClaude),
    (".agents/skills", SkillSource::WorkspaceAgents),
    (".claw/skills", SkillSource::WorkspaceClaw),
];

const GLOBAL_ROOTS: &[(&str, SkillSource)] = &[
    (".config/opencode/skills", SkillSource::GlobalOpenCode),
    (".codex/skills", SkillSource::GlobalCodex),
    (".claude/skills", SkillSource::GlobalClaude),
    (".agents/skills", SkillSource::GlobalAgents),
    (".claw/skills", SkillSource::GlobalClaw),
];

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SkillSource {
    WorkspaceShared,
    WorkspaceOpenCode,
    WorkspaceCodex,
    WorkspaceClaude,
    WorkspaceAgents,
    WorkspaceClaw,
    GlobalFx,
    GlobalOpenCode,
    GlobalCodex,
    GlobalClaude,
    GlobalAgents,
    GlobalClaw,
}

impl SkillSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::WorkspaceShared => "workspace skills/",
            Self::WorkspaceOpenCode => "workspace .opencode/skills",
            Self::WorkspaceCodex => "workspace .codex/skills",
            Self::WorkspaceClaude => "workspace .claude/skills",
            Self::WorkspaceAgents => "workspace .agents/skills",
            Self::WorkspaceClaw => "workspace .claw/skills",
            Self::GlobalFx => "global ~/.fx/skills",
            Self::GlobalOpenCode => "global ~/.config/opencode/skills",
            Self::GlobalCodex => "global ~/.codex/skills",
            Self::GlobalClaude => "global ~/.claude/skills",
            Self::GlobalAgents => "global ~/.agents/skills",
            Self::GlobalClaw => "global ~/.claw/skills",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub location: String,
    pub source: SkillSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub source: SkillSource,
    pub message: String,
}

#[derive(Clone, Debug)]
enum RootTrust {
    /// Compatibility roots below a workspace ancestor may themselves be
    /// symlinks, but their canonical targets must remain under that ancestor.
    Contained { authority: PathBuf },
    /// Managed and home-global roots reject symlink roots and candidates.
    Strict,
}

#[derive(Clone, Debug)]
struct SkillLocator {
    root: PathBuf,
    candidate: String,
    trust: RootTrust,
}

#[derive(Clone, Debug)]
struct SkillEntry {
    info: SkillInfo,
    locator: SkillLocator,
}

#[derive(Clone, Debug, Default)]
pub struct SkillCatalog {
    entries: Vec<SkillEntry>,
    diagnostics: Vec<SkillDiagnostic>,
}

#[derive(Debug)]
pub struct SkillRuntime {
    workspace: PathBuf,
    home: Option<PathBuf>,
    catalog: RwLock<SkillCatalog>,
}

impl SkillRuntime {
    pub fn discover(workspace: &Path, home: Option<&Path>) -> Self {
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_owned());
        let home = home.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_owned()));
        let catalog = SkillCatalog::discover(&workspace, home.as_deref());
        Self {
            workspace,
            home,
            catalog: RwLock::new(catalog),
        }
    }

    pub fn system_prompt_section(&self) -> String {
        self.catalog
            .read()
            .map(|catalog| catalog.system_prompt_section())
            .unwrap_or_default()
    }

    fn load(
        &self,
        name: &str,
        location: Option<&str>,
        resource: &str,
        offset: usize,
    ) -> Result<ToolOutput, SkillError> {
        self.catalog
            .read()
            .map_err(|_| SkillError::RuntimePoisoned)?
            .load(name, location, resource, offset)
    }

    fn refresh(&self) -> Result<(), ToolError> {
        let refreshed = SkillCatalog::discover(&self.workspace, self.home.as_deref());
        *self
            .catalog
            .write()
            .map_err(|_| ToolError::Execution("skill catalog lock is poisoned".into()))? =
            refreshed;
        Ok(())
    }

    fn managed_root(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|home| home.join(".fx/skills"))
    }
}

impl SkillCatalog {
    /// Discovers the stable default root policy used by fx and compatible
    /// agent ecosystems. A missing root is normal and produces no diagnostic.
    pub fn discover(workspace: &Path, home: Option<&Path>) -> Self {
        let mut catalog = Self::default();
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let home = home.and_then(|path| path.canonicalize().ok());
        let mut seen = HashSet::new();

        let mut ancestor = Some(workspace.as_path());
        while let Some(base) = ancestor {
            if home.as_ref().is_some_and(|home| base == home) {
                break;
            }
            if home.as_ref().is_some_and(|home| !base.starts_with(home)) && base != workspace {
                break;
            }
            for &(relative, source) in WORKSPACE_ROOTS {
                let root = base.join(relative);
                catalog.discover_root(
                    root,
                    source,
                    RootTrust::Contained {
                        authority: base.to_path_buf(),
                    },
                    &mut seen,
                );
            }
            ancestor = base.parent();
        }

        if let Some(home) = &home {
            catalog.discover_root(
                home.join(".fx/skills"),
                SkillSource::GlobalFx,
                RootTrust::Strict,
                &mut seen,
            );
            for &(relative, source) in GLOBAL_ROOTS {
                catalog.discover_root(home.join(relative), source, RootTrust::Strict, &mut seen);
            }
        }
        catalog
    }

    pub fn skills(&self) -> impl ExactSizeIterator<Item = &SkillInfo> {
        self.entries.iter().map(|entry| &entry.info)
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    /// Produces a bounded, XML-safe catalog for the system prompt.
    pub fn system_prompt_section(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }
        const HEADER: &str = "\n\nSkills provide specialized instructions and workflows for specific tasks.\nUse the skill tool to load a skill when a task matches its description.\nDo not assume a skill is loaded just because it is available. Load it first when it seems relevant.\n<available_skills>\n";
        const FOOTER: &str = "</available_skills>\n";

        let rendered = self
            .entries
            .iter()
            .map(|entry| {
                let description = truncate_utf8(
                    &entry.info.description,
                    MAX_DESCRIPTION_PROMPT_BYTES,
                );
                format!(
                    "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>\n",
                    xml_scalar(&entry.info.name),
                    xml_scalar(description),
                    xml_scalar(&entry.info.location),
                )
            })
            .collect::<Vec<_>>();

        let observed =
            HEADER.len() + FOOTER.len() + rendered.iter().map(String::len).sum::<usize>();
        let mut retained = rendered.len();
        loop {
            let omitted = rendered.len() - retained;
            let marker = (omitted > 0).then(|| {
                format!(
                    "  <context_limit name=\"skill_catalog_bytes\" action=\"omitted\" omitted_count=\"{omitted}\" observed_bytes=\"{observed}\" effective_bytes=\"{MAX_CATALOG_PROMPT_BYTES}\" />\n"
                )
            });
            let length = HEADER.len()
                + FOOTER.len()
                + rendered[..retained].iter().map(String::len).sum::<usize>()
                + marker.as_ref().map_or(0, String::len);
            if length <= MAX_CATALOG_PROMPT_BYTES || retained == 0 {
                let mut output = String::with_capacity(length);
                output.push_str(HEADER);
                for entry in &rendered[..retained] {
                    output.push_str(entry);
                }
                if let Some(marker) = marker {
                    output.push_str(&marker);
                }
                output.push_str(FOOTER);
                return output;
            }
            retained -= 1;
        }
    }

    fn discover_root(
        &mut self,
        root: PathBuf,
        source: SkillSource,
        trust: RootTrust,
        seen: &mut HashSet<(PathBuf, SkillSource)>,
    ) {
        if !root.exists() || !seen.insert((root.clone(), source)) {
            return;
        }
        let readable_root = match trusted_root_path(&root, &trust) {
            Ok(path) => path,
            Err(error) => {
                self.diagnostic(root, source, error.to_string());
                return;
            }
        };
        let mut candidates = match std::fs::read_dir(&readable_root) {
            Ok(entries) => entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect::<Vec<_>>(),
            Err(error) => {
                self.diagnostic(root, source, format!("could not read skill root: {error}"));
                return;
            }
        };
        candidates.sort();
        for candidate in candidates {
            if invalid_skill_name(&candidate).is_some() {
                continue;
            }
            let locator = SkillLocator {
                root: root.clone(),
                candidate,
                trust: trust.clone(),
            };
            match inspect_candidate(&locator) {
                Ok(metadata) => {
                    let location = locator.root.join(&locator.candidate);
                    self.entries.push(SkillEntry {
                        info: SkillInfo {
                            name: metadata.name,
                            description: metadata.description,
                            location: location.display().to_string(),
                            source,
                        },
                        locator,
                    });
                }
                Err(error) => self.diagnostic(
                    locator.root.join(&locator.candidate),
                    source,
                    error.to_string(),
                ),
            }
        }
    }

    fn diagnostic(&mut self, path: PathBuf, source: SkillSource, message: String) {
        self.diagnostics.push(SkillDiagnostic {
            path,
            source,
            message,
        });
    }

    fn resolve(&self, name: &str, location: Option<&str>) -> Result<&SkillEntry, SkillError> {
        if let Some(location) = location {
            let entry = self
                .entries
                .iter()
                .find(|entry| entry.info.location == location)
                .ok_or_else(|| SkillError::NotFound(name.to_owned()))?;
            if entry.info.name != name {
                return Err(SkillError::NameLocationMismatch {
                    name: name.to_owned(),
                    location: location.to_owned(),
                });
            }
            return Ok(entry);
        }

        let mut matches = self.entries.iter().filter(|entry| entry.info.name == name);
        let first = matches
            .next()
            .ok_or_else(|| SkillError::NotFound(name.to_owned()))?;
        if matches.next().is_some() {
            return Err(SkillError::Ambiguous(name.to_owned()));
        }
        Ok(first)
    }

    fn load(
        &self,
        name: &str,
        location: Option<&str>,
        resource: &str,
        offset: usize,
    ) -> Result<ToolOutput, SkillError> {
        let entry = self.resolve(name, location)?;
        validate_resource_path(resource)?;
        let candidate = open_candidate(&entry.locator)?;
        let current = read_metadata(
            candidate.open_file(Path::new(SKILL_FILE))?,
            &entry.locator.candidate,
        )?;
        if current.name != entry.info.name {
            return Err(SkillError::Changed(entry.info.location.clone()));
        }

        let mut file = candidate.open_file(Path::new(resource))?;
        let observed = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
        let readable = observed.min(MAX_RESOURCE_BYTES);
        let mut bytes = Vec::with_capacity(readable);
        file.seek(SeekFrom::Start(0))?;
        file.by_ref()
            .take(u64::try_from(readable).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| SkillError::BinaryResource)?;
        if offset > text.len() || !text.is_char_boundary(offset) {
            return Err(SkillError::InvalidOffset);
        }
        if offset == text.len() && observed > text.len() {
            return Err(SkillError::ResourceTooLarge(observed));
        }

        let remaining = &text[offset..];
        let chunk_len = line_safe_prefix_length(remaining, DEFAULT_CHUNK_BYTES);
        let next_offset = offset + chunk_len;
        let truncated = next_offset < text.len();
        let blocked_remainder = next_offset == text.len() && observed > text.len();
        let mut output = format!(
            "<skill_content name=\"{}\" resource=\"{}\" offset=\"{offset}\" next_offset=\"{next_offset}\">\n{}",
            xml_scalar(name),
            xml_scalar(resource),
            &remaining[..chunk_len],
        );
        if truncated {
            output.push_str(&format!(
                "\n<context_limit name=\"skill_chunk_bytes\" action=\"truncated\" observed_bytes=\"{}\" effective_bytes=\"{DEFAULT_CHUNK_BYTES}\" next_offset=\"{next_offset}\" />",
                remaining.len()
            ));
        } else if blocked_remainder {
            output.push_str(&format!(
                "\n<context_limit name=\"skill_file_bytes\" action=\"blocked_remainder\" observed_bytes=\"{observed}\" effective_bytes=\"{MAX_RESOURCE_BYTES}\" />"
            ));
        }
        output.push_str("\n</skill_content>");
        Ok(ToolOutput {
            original_bytes: output.len(),
            truncated: truncated || blocked_remainder,
            content: output,
            is_error: false,
            structured: None,
            durable_content: None,
        })
    }
}

#[derive(Debug, Error)]
enum SkillError {
    #[error("skill `{0}` was not advertised")]
    NotFound(String),
    #[error("skill `{0}` is ambiguous; retry with its exact advertised location")]
    Ambiguous(String),
    #[error("skill `{name}` does not match advertised location `{location}`")]
    NameLocationMismatch { name: String, location: String },
    #[error("skill at `{0}` changed after discovery; reload the session catalog")]
    Changed(String),
    #[error("invalid skill metadata: {0}")]
    InvalidMetadata(String),
    #[error("skill resource path must be a non-empty relative path without `.` or `..`")]
    InvalidResource,
    #[error("skill resource is not valid UTF-8")]
    BinaryResource,
    #[error("skill offset must be at a valid UTF-8 boundary within the selected resource")]
    InvalidOffset,
    #[error("skill resource exceeds the {MAX_RESOURCE_BYTES}-byte limit (observed {0} bytes)")]
    ResourceTooLarge(usize),
    #[error("skill catalog lock is poisoned")]
    RuntimePoisoned,
    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Clone)]
enum SkillCatalogSource {
    Static(Arc<SkillCatalog>),
    Runtime(Arc<SkillRuntime>),
}

#[derive(Clone)]
pub struct SkillTool {
    catalog: SkillCatalogSource,
}

impl SkillTool {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self {
            catalog: SkillCatalogSource::Static(catalog),
        }
    }

    pub fn from_runtime(runtime: Arc<SkillRuntime>) -> Self {
        Self {
            catalog: SkillCatalogSource::Runtime(runtime),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillInput {
    name: String,
    location: Option<String>,
    resource: Option<String>,
    #[serde(default)]
    offset: usize,
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Load the instructions or another UTF-8 resource of an advertised skill. Use location when a skill name is ambiguous, and next_offset to continue a truncated resource."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Advertised skill name" },
                "location": { "type": "string", "description": "Exact advertised location, required for ambiguous names" },
                "resource": { "type": "string", "description": "Relative resource path; defaults to SKILL.md" },
                "offset": { "type": "integer", "minimum": 0, "description": "UTF-8 byte offset for continuation" }
            },
            "required": ["name"],
            "additionalProperties": false
        })
    }

    fn effect(&self, _arguments: &Value) -> Result<ToolEffect, ToolError> {
        Ok(ToolEffect::Read)
    }

    fn permission_requests(
        &self,
        _context: &ToolContext,
        _arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        // The system prompt advertises only roots already granted by the host;
        // invocation revalidates that exact identity and cannot escape it.
        Ok(Vec::new())
    }

    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input: SkillInput = serde_json::from_value(arguments)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            if invalid_skill_name(&input.name).is_some() {
                return Err(ToolError::InvalidArguments("invalid skill name".into()));
            }
            let resource = input.resource.as_deref().unwrap_or(SKILL_FILE);
            let result = match &self.catalog {
                SkillCatalogSource::Static(catalog) => catalog.load(
                    &input.name,
                    input.location.as_deref(),
                    resource,
                    input.offset,
                ),
                SkillCatalogSource::Runtime(runtime) => runtime.load(
                    &input.name,
                    input.location.as_deref(),
                    resource,
                    input.offset,
                ),
            };
            result.map_err(|error| match error {
                SkillError::InvalidResource | SkillError::InvalidOffset => {
                    ToolError::InvalidArguments(error.to_string())
                }
                _ => ToolError::Execution(error.to_string()),
            })
        })
    }
}

#[derive(Debug)]
struct Metadata {
    name: String,
    description: String,
}

fn inspect_candidate(locator: &SkillLocator) -> Result<Metadata, SkillError> {
    let candidate = open_candidate(locator)?;
    let file = candidate.open_file(Path::new(SKILL_FILE))?;
    read_metadata(file, &locator.candidate)
}

fn trusted_root_path(root: &Path, trust: &RootTrust) -> Result<PathBuf, SkillError> {
    match trust {
        RootTrust::Strict => {
            SecureDir::open(root)?;
            Ok(root.to_path_buf())
        }
        RootTrust::Contained { authority } => {
            let canonical = root.canonicalize()?;
            let authority = authority.canonicalize()?;
            if !canonical.starts_with(&authority) {
                return Err(SkillError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "skill root symlink escapes its workspace ancestor",
                )));
            }
            SecureDir::open(&canonical)?;
            Ok(canonical)
        }
    }
}

fn open_candidate(locator: &SkillLocator) -> Result<SecureDir, SkillError> {
    match &locator.trust {
        RootTrust::Strict => {
            let root = SecureDir::open(&locator.root)?;
            Ok(root.open_dir(&locator.candidate)?)
        }
        RootTrust::Contained { authority } => {
            let location = locator.root.join(&locator.candidate);
            let canonical = location.canonicalize()?;
            let authority = authority.canonicalize()?;
            if !canonical.starts_with(&authority) {
                return Err(SkillError::Io(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "skill candidate symlink escapes its workspace ancestor",
                )));
            }
            Ok(SecureDir::open(&canonical)?)
        }
    }
}

fn read_metadata(mut file: File, fallback_name: &str) -> Result<Metadata, SkillError> {
    let size = usize::try_from(file.metadata()?.len()).unwrap_or(usize::MAX);
    let readable = size.min(MAX_FRONTMATTER_BYTES + 1);
    let mut bytes = Vec::with_capacity(readable);
    file.by_ref()
        .take(u64::try_from(readable).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| SkillError::InvalidMetadata("metadata is not valid UTF-8".into()))?;
    parse_metadata(content, fallback_name, size > bytes.len())
}

fn parse_metadata(
    content: &str,
    fallback_name: &str,
    prefix_truncated: bool,
) -> Result<Metadata, SkillError> {
    let header_start = if content.starts_with("---\r\n") {
        Some(5)
    } else if content.starts_with("---\n") {
        Some(4)
    } else if content == "---" {
        Some(3)
    } else {
        None
    };
    let Some(header_start) = header_start else {
        validate_name(fallback_name)?;
        return Ok(Metadata {
            name: fallback_name.to_owned(),
            description: String::new(),
        });
    };

    let rest = &content[header_start..];
    let mut closing = None;
    let mut cursor = header_start;
    for line in rest.split_inclusive('\n') {
        let value = line
            .strip_suffix('\n')
            .unwrap_or(line)
            .trim_end_matches('\r');
        if value == "---" {
            closing = Some(cursor);
            break;
        }
        cursor += line.len();
    }
    if closing.is_none() && rest.lines().last() == Some("---") {
        closing = Some(content.len() - 3);
    }
    let closing = closing.ok_or_else(|| {
        let reason = if prefix_truncated {
            "frontmatter exceeds 64 KiB"
        } else {
            "frontmatter has no closing delimiter"
        };
        SkillError::InvalidMetadata(reason.into())
    })?;
    if closing > MAX_FRONTMATTER_BYTES {
        return Err(SkillError::InvalidMetadata(
            "frontmatter exceeds 64 KiB".into(),
        ));
    }
    parse_header(&content[header_start..closing])
}

fn parse_header(header: &str) -> Result<Metadata, SkillError> {
    let lines = header.lines().collect::<Vec<_>>();
    let mut name = None;
    let mut description = None;
    let mut index = 0;
    while index < lines.len() {
        let raw = lines[index].trim_end_matches('\r');
        let trimmed = raw.trim();
        index += 1;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((key, raw_value)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = raw_value.trim();
        match key {
            "name" => {
                if name.is_some() {
                    return Err(SkillError::InvalidMetadata("duplicate name".into()));
                }
                name = Some(parse_scalar(value)?);
            }
            "description" => {
                if description.is_some() {
                    return Err(SkillError::InvalidMetadata("duplicate description".into()));
                }
                if matches!(value, ">" | ">-" | "|") {
                    let start = index;
                    while index < lines.len() {
                        let line = lines[index].trim_end_matches('\r');
                        if !line.is_empty() && !line.starts_with(' ') {
                            break;
                        }
                        index += 1;
                    }
                    description = Some(parse_block(&lines[start..index], value)?);
                } else {
                    if value.starts_with('>') || value.starts_with('|') {
                        return Err(SkillError::InvalidMetadata(
                            "unsupported multiline description".into(),
                        ));
                    }
                    description = Some(parse_scalar(value)?);
                }
            }
            _ => {}
        }
    }
    let name = name.ok_or_else(|| SkillError::InvalidMetadata("missing name".into()))?;
    validate_name(&name)?;
    let description = description.unwrap_or_default();
    if description.len() > MAX_DESCRIPTION_BYTES {
        return Err(SkillError::InvalidMetadata(
            "description exceeds 4 KiB".into(),
        ));
    }
    if description
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(SkillError::InvalidMetadata(
            "description contains control characters".into(),
        ));
    }
    Ok(Metadata { name, description })
}

fn parse_scalar(value: &str) -> Result<String, SkillError> {
    let first = value.chars().next();
    if matches!(first, Some('\'' | '"')) {
        let quote = first.unwrap_or_default();
        if value.len() < 2 || !value.ends_with(quote) {
            return Err(SkillError::InvalidMetadata("malformed quote".into()));
        }
        Ok(value[quote.len_utf8()..value.len() - quote.len_utf8()].to_owned())
    } else {
        Ok(value.to_owned())
    }
}

fn parse_block(lines: &[&str], style: &str) -> Result<String, SkillError> {
    let base_indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start_matches(' ').len())
        .next()
        .unwrap_or(0);
    if lines.iter().any(|line| {
        line.starts_with('\t')
            || (!line.trim().is_empty()
                && line.len() - line.trim_start_matches(' ').len() < base_indent)
    }) {
        return Err(SkillError::InvalidMetadata(
            "unsupported multiline indentation".into(),
        ));
    }
    let stripped = lines
        .iter()
        .map(|line| {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                ""
            } else {
                &line[base_indent..]
            }
        })
        .collect::<Vec<_>>();
    let mut output = if style == "|" {
        stripped.join("\n")
    } else {
        let mut value = String::new();
        let mut previous_blank = false;
        for line in stripped {
            if line.is_empty() {
                value.push('\n');
                previous_blank = true;
            } else {
                if !value.is_empty() && !previous_blank {
                    value.push(' ');
                }
                value.push_str(line);
                previous_blank = false;
            }
        }
        value
    };
    if style != ">-" {
        output.push('\n');
    }
    Ok(output)
}

fn validate_name(name: &str) -> Result<(), SkillError> {
    if let Some(reason) = invalid_skill_name(name) {
        Err(SkillError::InvalidMetadata(reason.into()))
    } else {
        Ok(())
    }
}

fn invalid_skill_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("skill name is empty")
    } else if name.len() > MAX_NAME_BYTES {
        Some("skill name exceeds 256 bytes")
    } else if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        Some("skill name must be one path component")
    } else if name.chars().any(char::is_control) {
        Some("skill name contains control characters")
    } else {
        None
    }
}

fn validate_resource_path(resource: &str) -> Result<(), SkillError> {
    let path = Path::new(resource);
    if resource.is_empty() || path.is_absolute() {
        return Err(SkillError::InvalidResource);
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(SkillError::InvalidResource);
    }
    Ok(())
}

fn line_safe_prefix_length(text: &str, limit: usize) -> usize {
    if text.len() <= limit {
        return text.len();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = text[..end].rfind('\n') {
        newline + 1
    } else {
        end
    }
}

fn truncate_utf8(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn xml_scalar(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            character if character.is_control() && character != '\n' => {
                output.push_str(&format!("&#x{:x};", u32::from(character)));
            }
            character => output.push(character),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use uuid::Uuid;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("fx-skills-{}", Uuid::new_v4().simple()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_skill(root: &Path, directory: &str, content: &str) -> PathBuf {
        let path = root.join("skills").join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join(SKILL_FILE), content).unwrap();
        path
    }

    #[test]
    fn discovers_ancestor_skills_in_stable_order_and_builds_catalog() {
        let temp = TempDir::new();
        let workspace = temp.0.join("repo/child");
        fs::create_dir_all(&workspace).unwrap();
        write_skill(
            &temp.0.join("repo"),
            "zeta",
            "---\nname: zeta\ndescription: Last & useful\n---\nbody",
        );
        write_skill(
            &workspace,
            "alpha",
            "---\nname: alpha\ndescription: First\n---\nbody",
        );

        let catalog = SkillCatalog::discover(&workspace, Some(&temp.0));
        let names = catalog
            .skills()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["alpha", "zeta"]);
        let prompt = catalog.system_prompt_section();
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("Last &amp; useful"));
    }

    #[test]
    fn loads_resources_in_utf8_line_safe_chunks() {
        let temp = TempDir::new();
        let workspace = temp.0.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        let location = write_skill(
            &workspace,
            "workflow",
            "---\nname: workflow\ndescription: Test\n---\nInstructions\n",
        );
        let long = format!("{}\nend", "é".repeat(DEFAULT_CHUNK_BYTES));
        fs::write(location.join("assets.txt"), long).unwrap();
        let catalog = Arc::new(SkillCatalog::discover(&workspace, Some(&temp.0)));
        let tool = SkillTool::new(catalog);
        let context = ToolContext::new(workspace);
        let output = pollster::block_on(tool.execute(
            &context,
            json!({"name":"workflow", "resource":"assets.txt"}),
        ))
        .unwrap();
        assert!(output.content.contains("next_offset="));
        assert!(output.content.contains("skill_chunk_bytes"));
        assert!(std::str::from_utf8(output.content.as_bytes()).is_ok());
    }

    #[test]
    fn ambiguous_name_requires_exact_location() {
        let temp = TempDir::new();
        let workspace = temp.0.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        write_skill(
            &workspace,
            "one",
            "---\nname: duplicate\ndescription: One\n---\n",
        );
        let other = temp.0.join(".fx");
        write_skill(
            &other,
            "two",
            "---\nname: duplicate\ndescription: Two\n---\n",
        );
        // `write_skill(.fx, ...)` creates `.fx/skills/two`, the managed root.
        let catalog = SkillCatalog::discover(&workspace, Some(&temp.0));
        assert!(matches!(
            catalog.resolve("duplicate", None),
            Err(SkillError::Ambiguous(_))
        ));
        let location = catalog
            .skills()
            .find(|skill| skill.description == "One")
            .unwrap()
            .location
            .clone();
        assert_eq!(
            catalog
                .resolve("duplicate", Some(&location))
                .unwrap()
                .info
                .description,
            "One"
        );
    }

    #[test]
    fn invocation_revalidates_name_and_rejects_parent_resources() {
        let temp = TempDir::new();
        let workspace = temp.0.join("repo");
        fs::create_dir_all(&workspace).unwrap();
        let location = write_skill(
            &workspace,
            "workflow",
            "---\nname: workflow\ndescription: Test\n---\n",
        );
        let catalog = SkillCatalog::discover(&workspace, Some(&temp.0));
        assert!(matches!(
            catalog.load("workflow", None, "../secret", 0),
            Err(SkillError::InvalidResource)
        ));
        fs::write(
            location.join(SKILL_FILE),
            "---\nname: changed\ndescription: Test\n---\n",
        )
        .unwrap();
        assert!(matches!(
            catalog.load("workflow", None, SKILL_FILE, 0),
            Err(SkillError::Changed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn strict_global_root_rejects_symlink_candidates() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let workspace = temp.0.join("repo");
        let outside = temp.0.join("outside/escaped");
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            outside.join(SKILL_FILE),
            "---\nname: escaped\ndescription: no\n---\n",
        )
        .unwrap();
        fs::create_dir_all(temp.0.join(".fx/skills")).unwrap();
        symlink(&outside, temp.0.join(".fx/skills/escaped")).unwrap();
        let catalog = SkillCatalog::discover(&workspace, Some(&temp.0));
        assert!(catalog.skills().all(|skill| skill.name != "escaped"));
        assert!(!catalog.diagnostics().is_empty());
    }

    #[test]
    fn metadata_supports_legacy_quotes_and_block_descriptions() {
        assert_eq!(
            parse_metadata("legacy body", "legacy", false).unwrap().name,
            "legacy"
        );
        let parsed = parse_metadata(
            "---\r\nname: 'workflow'\r\ndescription: |\r\n  first\r\n  second\r\n---\r\nbody",
            "ignored",
            false,
        )
        .unwrap();
        assert_eq!(parsed.name, "workflow");
        assert_eq!(parsed.description, "first\nsecond\n");
    }
}
