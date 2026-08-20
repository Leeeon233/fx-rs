use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant};

use fs4::TryLockError;
use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{SkillRuntime, invalid_skill_name, read_metadata};

const MAX_SOURCE_BYTES: usize = 16 * 1024;
const MAX_SKILL_FILTER_BYTES: usize = 256;
const MAX_COPY_ENTRIES: usize = 10_000;
const MAX_COPY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_COPY_FILE_BYTES: u64 = 8 * 1024 * 1024;
const CLONE_TIMEOUT: Duration = Duration::from_secs(60);
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct InstallSkillTool {
    runtime: std::sync::Arc<SkillRuntime>,
}

impl InstallSkillTool {
    pub fn new(runtime: std::sync::Arc<SkillRuntime>) -> Self {
        Self { runtime }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    source: String,
    #[serde(default)]
    skill: Option<String>,
}

#[derive(Debug)]
struct InstallRequest {
    source: InstallSource,
    filter: Option<String>,
}

#[derive(Debug)]
enum InstallSource {
    Local(PathBuf),
    Github { slug: String, url: String },
}

impl Tool for InstallSkillTool {
    fn name(&self) -> &str {
        "install_skill"
    }

    fn description(&self) -> &str {
        "Install one or more reusable skills from a local directory or public GitHub repository into managed profile storage."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": { "type": "string" },
                "skill": { "type": "string" }
            },
            "required": ["source"],
            "additionalProperties": false
        })
    }

    fn effect(&self, arguments: &Value) -> Result<ToolEffect, ToolError> {
        let input = decode(arguments)?;
        let request = normalize_request(&self.runtime, input)?;
        Ok(match request.source {
            InstallSource::Local(_) => ToolEffect::Write,
            InstallSource::Github { .. } => ToolEffect::Network,
        })
    }

    fn permission_requests(
        &self,
        _context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let request = normalize_request(&self.runtime, decode(arguments)?)?;
        let destination = self.runtime.managed_root().map_or_else(
            || "managed-skills:unavailable".into(),
            |path| path.display().to_string(),
        );
        let mut permissions = Vec::new();
        if let InstallSource::Local(path) = &request.source {
            permissions.push(PermissionRequest::new(
                self.name(),
                path.display().to_string(),
                ToolEffect::Read,
            ));
        }
        permissions.push(PermissionRequest::new(
            self.name(),
            destination,
            ToolEffect::Write,
        ));
        if let InstallSource::Github { url, .. } = &request.source {
            permissions.insert(
                0,
                PermissionRequest::new(self.name(), url.clone(), ToolEffect::Network),
            );
        }
        Ok(permissions)
    }

    fn execute<'a>(
        &'a self,
        context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let request = normalize_request(&self.runtime, decode(&arguments)?)?;
            let managed_root = self.runtime.managed_root().ok_or_else(|| {
                ToolError::Execution("skill installation is unavailable: HOME not set".into())
            })?;
            ensure_managed_root(&managed_root)?;

            let mut clone_tree = None;
            let (source, fallback) = match &request.source {
                InstallSource::Local(path) => (path.clone(), directory_name(path)?.to_owned()),
                InstallSource::Github { slug, url } => {
                    let tree = TempTree::new(&std::env::temp_dir(), "fx-skill-clone")?;
                    clone_github(url, &tree.path, context).await?;
                    let source = tree.path.clone();
                    let fallback = slug.rsplit('/').next().unwrap_or("skill").to_owned();
                    clone_tree = Some(tree);
                    (source, fallback)
                }
            };

            let candidates = discover_candidates(&source, &fallback, request.filter.as_deref())?;
            let mut installed = Vec::new();
            for candidate in candidates {
                install_candidate(
                    &candidate.directory,
                    &managed_root,
                    &candidate.directory_name,
                )?;
                installed.push(candidate.metadata_name);
            }
            drop(clone_tree);
            self.runtime.refresh()?;

            let content = if installed.is_empty() {
                format!(
                    "No matching skills were installed from {}.",
                    source.display()
                )
            } else {
                let mut content = format!("Installed {} skill(s) into fxrs.\n", installed.len());
                for name in installed {
                    content.push_str("- ");
                    content.push_str(&name);
                    content.push('\n');
                }
                content
            };
            Ok(ToolOutput {
                original_bytes: content.len(),
                content,
                is_error: false,
                structured: None,
                truncated: false,
                durable_content: None,
            })
        })
    }
}

fn decode(arguments: &Value) -> Result<Input, ToolError> {
    let input: Input = serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
    if input.source.trim().is_empty() || input.source.len() > MAX_SOURCE_BYTES {
        return Err(ToolError::InvalidArguments(format!(
            "install_skill source must be 1 to {MAX_SOURCE_BYTES} bytes"
        )));
    }
    if input.skill.as_ref().is_some_and(|skill| {
        skill.is_empty()
            || skill.len() > MAX_SKILL_FILTER_BYTES
            || invalid_skill_name(skill).is_some()
    }) {
        return Err(ToolError::InvalidArguments(
            "install_skill skill filter is invalid".into(),
        ));
    }
    Ok(input)
}

fn normalize_request(runtime: &SkillRuntime, input: Input) -> Result<InstallRequest, ToolError> {
    let (source, command_filter) = parse_install_command(input.source.trim())?;
    let local = resolve_local_source(&runtime.workspace, source);
    if let Some(path) = local {
        return Ok(InstallRequest {
            source: InstallSource::Local(path),
            filter: merge_filter(command_filter, input.skill)?,
        });
    }
    let (source, inline_filter) = parse_inline_filter(source)?;
    let filter = merge_filter(merge_filter(command_filter, inline_filter)?, input.skill)?;
    let slug = github_slug(source)?;
    Ok(InstallRequest {
        source: InstallSource::Github {
            url: format!("https://github.com/{slug}.git"),
            slug,
        },
        filter,
    })
}

fn parse_install_command(source: &str) -> Result<(&str, Option<String>), ToolError> {
    let fields = source.split_ascii_whitespace().collect::<Vec<_>>();
    if !matches!(fields.first(), Some(&"npx" | &"bunx")) {
        return Ok((source, None));
    }
    if fields.get(1) != Some(&"skills") || fields.get(2) != Some(&"add") {
        return Err(ToolError::InvalidArguments(
            "only `npx skills add` or `bunx skills add` install commands are supported".into(),
        ));
    }
    let source = *fields.get(3).ok_or_else(|| {
        ToolError::InvalidArguments("skill install command is missing its source".into())
    })?;
    let mut filter = None;
    let mut index = 4usize;
    while index < fields.len() {
        if fields[index] == "--skill" {
            let value = fields.get(index + 1).ok_or_else(|| {
                ToolError::InvalidArguments("--skill is missing its value".into())
            })?;
            filter = Some((*value).to_owned());
            index += 2;
        } else if let Some(value) = fields[index].strip_prefix("--skill=") {
            filter = Some(value.to_owned());
            index += 1;
        } else {
            return Err(ToolError::InvalidArguments(
                "skill install command contains unsupported arguments".into(),
            ));
        }
    }
    Ok((source, filter))
}

fn resolve_local_source(workspace: &Path, source: &str) -> Option<PathBuf> {
    let path = Path::new(source);
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        workspace.join(path)
    };
    candidate
        .canonicalize()
        .ok()
        .filter(|candidate| candidate.is_dir())
}

fn parse_inline_filter(source: &str) -> Result<(&str, Option<String>), ToolError> {
    if let Some(rest) = source.strip_prefix("https://skills.sh/") {
        let parts = rest.split('/').collect::<Vec<_>>();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(ToolError::InvalidArguments("invalid skills.sh URL".into()));
        }
        let source_len = parts[0].len() + 1 + parts[1].len();
        return Ok((
            &rest[..source_len],
            parts.get(2).map(|value| (*value).to_owned()),
        ));
    }
    if !source.starts_with("https://")
        && let Some((base, filter)) = source.rsplit_once('@')
        && !base.is_empty()
        && invalid_skill_name(filter).is_none()
    {
        return Ok((base, Some(filter.to_owned())));
    }
    Ok((source, None))
}

fn merge_filter(left: Option<String>, right: Option<String>) -> Result<Option<String>, ToolError> {
    match (left, right) {
        (Some(left), Some(right)) if left != right => Err(ToolError::InvalidArguments(
            "conflicting skill filters were provided".into(),
        )),
        (Some(value), _) | (_, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn github_slug(source: &str) -> Result<String, ToolError> {
    let source = source
        .strip_prefix("https://github.com/")
        .unwrap_or(source)
        .trim_end_matches('/')
        .trim_end_matches(".git");
    if source.contains(['?', '#', '\\']) {
        return Err(ToolError::InvalidArguments(
            "only public GitHub repository sources are supported".into(),
        ));
    }
    let parts = source.split('/').collect::<Vec<_>>();
    if parts.len() != 2
        || parts.iter().any(|part| {
            part.is_empty()
                || *part == "."
                || *part == ".."
                || part.len() > 100
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
    {
        return Err(ToolError::InvalidArguments(
            "only public GitHub repository sources are supported".into(),
        ));
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

async fn clone_github(
    url: &str,
    destination: &Path,
    context: &ToolContext,
) -> Result<(), ToolError> {
    let mut child = tokio::process::Command::new("git")
        .arg("-c")
        .arg("credential.helper=")
        .args(["clone", "--depth", "1", "--no-tags"])
        .arg(url)
        .arg(destination)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ToolError::Execution(format!("could not start git clone: {error}")))?;
    enum Outcome {
        Status(std::io::Result<std::process::ExitStatus>),
        Timeout,
        Cancelled,
    }
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Status(status),
        () = tokio::time::sleep(CLONE_TIMEOUT) => Outcome::Timeout,
        () = wait_for_cancellation(context) => Outcome::Cancelled,
    };
    match outcome {
        Outcome::Status(Ok(status)) if status.success() => Ok(()),
        Outcome::Status(Ok(status)) => Err(ToolError::Execution(format!(
            "git clone failed with status {status}"
        ))),
        Outcome::Status(Err(error)) => Err(ToolError::Execution(format!(
            "could not wait for git clone: {error}"
        ))),
        Outcome::Timeout => {
            let _ = child.kill().await;
            Err(ToolError::Execution("git clone timed out".into()))
        }
        Outcome::Cancelled => {
            let _ = child.kill().await;
            Err(ToolError::Cancelled)
        }
    }
}

async fn wait_for_cancellation(context: &ToolContext) {
    while !context.cancellation.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Debug)]
struct Candidate {
    directory: PathBuf,
    directory_name: String,
    metadata_name: String,
}

fn discover_candidates(
    source: &Path,
    fallback: &str,
    filter: Option<&str>,
) -> Result<Vec<Candidate>, ToolError> {
    let mut directories = Vec::new();
    if regular_file(&source.join("SKILL.md")) {
        directories.push((source.to_owned(), fallback.to_owned()));
    }
    let mut builder = WalkBuilder::new(source);
    builder
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with(".git")
        });
    let mut entries = 0usize;
    for entry in builder.build() {
        let entry = entry.map_err(|error| ToolError::Execution(error.to_string()))?;
        if entry.depth() == 0 {
            continue;
        }
        entries += 1;
        if entries > MAX_COPY_ENTRIES {
            return Err(ToolError::Execution(format!(
                "skill source exceeds {MAX_COPY_ENTRIES} entries"
            )));
        }
        if entry.file_type().is_some_and(|kind| kind.is_file()) && entry.file_name() == "SKILL.md" {
            let directory = entry.path().parent().expect("walked file has parent");
            if directory == source {
                continue;
            }
            let name = directory_name(directory)?.to_owned();
            if invalid_skill_name(&name).is_none() {
                directories.push((directory.to_owned(), name));
            }
        }
    }
    directories.sort_by(|left, right| left.0.cmp(&right.0));
    directories.dedup_by(|left, right| left.0 == right.0);

    let mut result = Vec::new();
    for (directory, directory_name) in directories {
        if invalid_skill_name(&directory_name).is_some() {
            continue;
        }
        let file = open_regular_no_follow(&directory.join("SKILL.md"))?;
        let metadata = read_metadata(file, &directory_name)
            .map_err(|error| ToolError::Execution(error.to_string()))?;
        if filter.is_some_and(|filter| filter != directory_name && filter != metadata.name) {
            continue;
        }
        let metadata_name = metadata.name;
        if result
            .iter()
            .any(|candidate: &Candidate| candidate.directory_name == metadata_name)
        {
            return Err(ToolError::Execution(format!(
                "skill source contains duplicate managed name: {metadata_name}"
            )));
        }
        result.push(Candidate {
            directory,
            directory_name: metadata_name.clone(),
            metadata_name,
        });
    }
    Ok(result)
}

fn install_candidate(source: &Path, managed_root: &Path, name: &str) -> Result<(), ToolError> {
    if invalid_skill_name(name).is_some() {
        return Err(ToolError::Execution("invalid managed skill name".into()));
    }
    let mut transaction = TempTree::new(managed_root, ".skill-install")?;
    let staged = transaction.path.join("staged");
    copy_skill_tree(source, &staged)?;
    let _lock = lock_skill(managed_root, name)?;
    let destination = managed_root.join(name);
    let backup = transaction.path.join("backup");
    let moved_existing = match fs::rename(&destination, &backup) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(io_error(&destination, error)),
    };
    if let Err(commit_error) = fs::rename(&staged, &destination) {
        if moved_existing && let Err(rollback_error) = fs::rename(&backup, &destination) {
            transaction.preserve = true;
            return Err(ToolError::Execution(format!(
                "skill install commit failed: {commit_error}; rollback failed: {rollback_error}; recovery retained at {}",
                transaction.path.display()
            )));
        }
        return Err(io_error(&destination, commit_error));
    }
    Ok(())
}

fn copy_skill_tree(source: &Path, destination: &Path) -> Result<(), ToolError> {
    fs::create_dir(destination).map_err(|error| io_error(destination, error))?;
    let mut builder = WalkBuilder::new(source);
    builder
        .standard_filters(false)
        .hidden(false)
        .follow_links(false)
        .filter_entry(|entry| {
            entry.depth() == 0 || !entry.file_name().to_string_lossy().starts_with(".git")
        });
    let mut entries = 0usize;
    let mut bytes = 0u64;
    for entry in builder.build() {
        let entry = entry.map_err(|error| ToolError::Execution(error.to_string()))?;
        if entry.depth() == 0 {
            continue;
        }
        entries += 1;
        if entries > MAX_COPY_ENTRIES {
            return Err(ToolError::Execution(format!(
                "skill exceeds {MAX_COPY_ENTRIES} entries"
            )));
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|_| ToolError::Execution("skill traversal escaped its source".into()))?;
        let target = destination.join(relative);
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            fs::create_dir(&target).map_err(|error| io_error(&target, error))?;
        } else if kind.is_file() {
            let size = entry
                .metadata()
                .map_err(|error| ToolError::Execution(error.to_string()))?
                .len();
            if size > MAX_COPY_FILE_BYTES {
                return Err(ToolError::Execution(format!(
                    "skill file exceeds {MAX_COPY_FILE_BYTES} bytes: {}",
                    entry.path().display()
                )));
            }
            bytes = bytes.saturating_add(size);
            if bytes > MAX_COPY_BYTES {
                return Err(ToolError::Execution(format!(
                    "skill exceeds {MAX_COPY_BYTES} total bytes"
                )));
            }
            fs::copy(entry.path(), &target).map_err(|error| io_error(&target, error))?;
        }
    }
    Ok(())
}

fn ensure_managed_root(root: &Path) -> Result<(), ToolError> {
    let profile = root
        .parent()
        .ok_or_else(|| ToolError::Execution("managed skill root has no profile parent".into()))?;
    let home = profile
        .parent()
        .ok_or_else(|| ToolError::Execution("managed skill root has no home parent".into()))?;
    ensure_real_directory(home, false)?;
    ensure_real_directory(profile, true)?;
    ensure_real_directory(root, true)
}

fn ensure_real_directory(path: &Path, create: bool) -> Result<(), ToolError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ToolError::Execution(format!(
                "skill path is not a real directory: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| io_error(path, error))?;
        }
        Err(error) => return Err(io_error(path, error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_error(path, error))?;
    }
    Ok(())
}

fn lock_skill(managed_root: &Path, name: &str) -> Result<File, ToolError> {
    let lock_root = managed_root
        .parent()
        .expect("managed root has profile parent")
        .join(".skill-install-locks");
    ensure_real_directory(&lock_root, true)?;
    let path = lock_root.join(name);
    let file = open_private_lock(&path)?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(ToolError::Execution(format!(
                    "timed out waiting for skill install lock at {}",
                    path.display()
                )));
            }
            Err(TryLockError::Error(error)) => return Err(io_error(&path, error)),
        }
    }
}

fn open_private_lock(path: &Path) -> Result<File, ToolError> {
    reject_unsafe_file(path)?;
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| io_error(path, error))
}

fn open_regular_no_follow(path: &Path) -> Result<File, ToolError> {
    reject_unsafe_file(path)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|error| io_error(path, error))
}

fn reject_unsafe_file(path: &Path) -> Result<(), ToolError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ToolError::Execution(format!(
                "skill path is not a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error(path, error)),
    }
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn directory_name(path: &Path) -> Result<&str, ToolError> {
    path.file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ToolError::Execution(format!(
                "skill directory has no UTF-8 name: {}",
                path.display()
            ))
        })
}

fn io_error(path: &Path, error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("{}: {error}", path.display()))
}

struct TempTree {
    path: PathBuf,
    preserve: bool,
}

impl TempTree {
    fn new(parent: &Path, prefix: &str) -> Result<Self, ToolError> {
        for _ in 0..8 {
            let path = parent.join(format!("{prefix}-{}", Uuid::new_v4().simple()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                            .map_err(|error| io_error(&path, error))?;
                    }
                    return Ok(Self {
                        path,
                        preserve: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error(&path, error)),
            }
        }
        Err(ToolError::Execution(
            "could not allocate a unique skill transaction directory".into(),
        ))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        if !self.preserve {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fx_core::NeverCancelled;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "fx-skill-install-{name}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_install_is_atomic_and_hot_refreshes_the_reader() {
        let fixture = Fixture::new("local");
        let home = fixture.0.join("home");
        let workspace = fixture.0.join("workspace");
        let source = workspace.join("source/review");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&source).unwrap();
        fs::write(
            source.join("SKILL.md"),
            "---\nname: review\ndescription: Review code\n---\n\nRead carefully.\n",
        )
        .unwrap();
        fs::write(source.join("asset.txt"), "asset").unwrap();
        let runtime = std::sync::Arc::new(SkillRuntime::discover(&workspace, Some(&home)));
        let installer = InstallSkillTool::new(runtime.clone());
        let reader = super::super::SkillTool::from_runtime(runtime);
        let mut context = ToolContext::new(workspace);
        context.cancellation = std::sync::Arc::new(NeverCancelled);

        let output = installer
            .execute(
                &context,
                json!({"source": "source/review", "skill": "review"}),
            )
            .await
            .unwrap();
        assert!(output.content.contains("Installed 1 skill"));
        let loaded = reader
            .execute(&context, json!({"name": "review"}))
            .await
            .unwrap();
        assert!(loaded.content.contains("Read carefully"));
        assert_eq!(
            fs::read_to_string(home.join(".fx/skills/review/asset.txt")).unwrap(),
            "asset"
        );
    }

    #[test]
    fn source_normalization_rejects_non_github_network_and_filter_conflicts() {
        let fixture = Fixture::new("normalize");
        let workspace = fixture.0.join("workspace");
        let home = fixture.0.join("home");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&home).unwrap();
        let runtime = SkillRuntime::discover(&workspace, Some(&home));
        assert!(
            normalize_request(
                &runtime,
                Input {
                    source: "https://example.com/repo".into(),
                    skill: None,
                }
            )
            .is_err()
        );
        assert!(
            normalize_request(
                &runtime,
                Input {
                    source: "owner/repo@one".into(),
                    skill: Some("two".into()),
                }
            )
            .is_err()
        );
        let request = normalize_request(
            &runtime,
            Input {
                source: "npx skills add owner/repo --skill review".into(),
                skill: Some("review".into()),
            },
        )
        .unwrap();
        assert!(matches!(request.source, InstallSource::Github { .. }));
        assert_eq!(request.filter.as_deref(), Some("review"));
    }
}
