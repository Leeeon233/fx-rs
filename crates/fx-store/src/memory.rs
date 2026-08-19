use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use atomic_write_file::AtomicWriteFile;
use fs4::TryLockError;
use fx_core::{BoxFuture, PermissionRequest, Tool, ToolContext, ToolEffect, ToolError, ToolOutput};
use serde::Deserialize;
use serde_json::{Value, json};

const MEMORY_FILE: &str = "memories.json";
const MEMORY_LOCK_FILE: &str = "memories.lock";
const MAX_MEMORY_BYTES: usize = 1024 * 1024;
const MAX_FACT_BYTES: usize = 64 * 1024;
const MAX_FACTS: usize = 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct MemoryTool {
    profile_directory: Option<PathBuf>,
}

impl MemoryTool {
    pub fn new(home: Option<&Path>) -> Self {
        Self {
            profile_directory: home.map(|home| home.join(".fx")),
        }
    }

    fn memory_path(&self) -> Option<PathBuf> {
        self.profile_directory
            .as_ref()
            .map(|directory| directory.join(MEMORY_FILE))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Save,
    List,
    Clear,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    action: Action,
    #[serde(default)]
    fact: Option<String>,
}

impl Tool for MemoryTool {
    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        "Save, list, or clear a small profile-scoped set of durable user facts. Never store credentials or unverified inferences."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["save", "list", "clear"] },
                "fact": { "type": "string" }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn effect(&self, arguments: &Value) -> Result<ToolEffect, ToolError> {
        Ok(match decode(arguments)?.action {
            Action::List => ToolEffect::Read,
            Action::Save | Action::Clear => ToolEffect::Write,
        })
    }

    fn irreversible(&self, arguments: &Value) -> Result<bool, ToolError> {
        Ok(decode(arguments)?.action == Action::Clear)
    }

    fn permission_requests(
        &self,
        _context: &ToolContext,
        arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        let effect = self.effect(arguments)?;
        let target = self.memory_path().map_or_else(
            || "profile-memory:unavailable".into(),
            |path| path.display().to_string(),
        );
        Ok(vec![PermissionRequest::new(self.name(), target, effect)])
    }

    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let input = decode(&arguments)?;
            validate_input(&input)?;
            let Some(directory) = &self.profile_directory else {
                return tool_output("memory unavailable: HOME not set".into());
            };
            ensure_profile_directory(directory)?;
            let _lock = lock_memory(directory)?;
            let path = directory.join(MEMORY_FILE);
            let content = match input.action {
                Action::List => {
                    let facts = load_memories(&path)?;
                    if facts.is_empty() {
                        "No saved memories".into()
                    } else {
                        let mut output = String::new();
                        for fact in facts {
                            output.push_str("- ");
                            output.push_str(&fact);
                            output.push('\n');
                        }
                        output
                    }
                }
                Action::Save => {
                    let fact = input.fact.expect("save fact validated");
                    let mut facts = load_memories(&path)?;
                    if !facts.iter().any(|existing| existing == &fact) {
                        if facts.len() >= MAX_FACTS {
                            return Err(ToolError::Execution(format!(
                                "memory contains the maximum of {MAX_FACTS} facts"
                            )));
                        }
                        facts.push(fact);
                        save_memories(&path, &facts)?;
                    }
                    "remembered".into()
                }
                Action::Clear => {
                    reject_unsafe_file(&path)?;
                    match fs::remove_file(&path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(memory_io(&path, error)),
                    }
                    "memories cleared".into()
                }
            };
            tool_output(content)
        })
    }
}

fn decode(arguments: &Value) -> Result<Input, ToolError> {
    serde_json::from_value(arguments.clone())
        .map_err(|error| ToolError::InvalidArguments(error.to_string()))
}

fn validate_input(input: &Input) -> Result<(), ToolError> {
    match (input.action, input.fact.as_deref()) {
        (Action::Save, Some(fact)) if !fact.trim().is_empty() && fact.len() <= MAX_FACT_BYTES => {
            Ok(())
        }
        (Action::Save, _) => Err(ToolError::InvalidArguments(format!(
            "memory save requires a non-empty fact of at most {MAX_FACT_BYTES} bytes"
        ))),
        (Action::List | Action::Clear, None) => Ok(()),
        (Action::List | Action::Clear, Some(_)) => Err(ToolError::InvalidArguments(
            "memory fact is only valid for action=save".into(),
        )),
    }
}

fn ensure_profile_directory(path: &Path) -> Result<(), ToolError> {
    let parent = path
        .parent()
        .ok_or_else(|| ToolError::Execution("profile directory has no parent".into()))?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|error| memory_io(parent, error))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(ToolError::Execution(format!(
            "profile home is not a real directory: {}",
            parent.display()
        )));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ToolError::Execution(format!(
                "profile path is not a real directory: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| memory_io(path, error))?;
        }
        Err(error) => return Err(memory_io(path, error)),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| memory_io(path, error))?;
    }
    Ok(())
}

fn lock_memory(directory: &Path) -> Result<File, ToolError> {
    let path = directory.join(MEMORY_LOCK_FILE);
    reject_unsafe_file(&path)?;
    let file = open_private(&path, true, true)?;
    let deadline = Instant::now() + LOCK_TIMEOUT;
    loop {
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(ToolError::Execution(format!(
                    "timed out waiting for memory lock at {}",
                    path.display()
                )));
            }
            Err(TryLockError::Error(error)) => return Err(memory_io(&path, error)),
        }
    }
}

fn load_memories(path: &Path) -> Result<Vec<String>, ToolError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Ok(_) => reject_unsafe_file(path)?,
        Err(error) => return Err(memory_io(path, error)),
    }
    let mut file = open_private(path, false, false)?;
    let size = file
        .metadata()
        .map_err(|error| memory_io(path, error))?
        .len();
    if size > MAX_MEMORY_BYTES as u64 {
        return Err(ToolError::Execution(format!(
            "memory file exceeds {MAX_MEMORY_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| memory_io(path, error))?;
    let facts: Vec<String> = serde_json::from_slice(&bytes)
        .map_err(|error| ToolError::Execution(format!("invalid memory file: {error}")))?;
    if facts.len() > MAX_FACTS
        || facts
            .iter()
            .any(|fact| fact.is_empty() || fact.len() > MAX_FACT_BYTES)
    {
        return Err(ToolError::Execution(
            "memory file exceeds its limits".into(),
        ));
    }
    Ok(facts)
}

fn save_memories(path: &Path, facts: &[String]) -> Result<(), ToolError> {
    reject_unsafe_file(path)?;
    let bytes = serde_json::to_vec_pretty(facts)
        .map_err(|error| ToolError::Execution(format!("could not encode memories: {error}")))?;
    if bytes.len() > MAX_MEMORY_BYTES {
        return Err(ToolError::Execution(format!(
            "memory file exceeds {MAX_MEMORY_BYTES} bytes"
        )));
    }
    let mut stage = AtomicWriteFile::open(path).map_err(|error| memory_io(path, error))?;
    set_private(stage.as_file(), path)?;
    stage
        .write_all(&bytes)
        .map_err(|error| memory_io(path, error))?;
    stage.commit().map_err(|error| memory_io(path, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| memory_io(path, error))?;
    }
    Ok(())
}

fn reject_unsafe_file(path: &Path) -> Result<(), ToolError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(ToolError::Execution(format!(
                "memory path is not a regular file: {}",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(memory_io(path, error)),
    }
}

fn open_private(path: &Path, create: bool, write: bool) -> Result<File, ToolError> {
    let mut options = OpenOptions::new();
    options.create(create).read(true).write(write);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path).map_err(|error| memory_io(path, error))?;
    set_private(&file, path)?;
    Ok(file)
}

fn set_private(file: &File, path: &Path) -> Result<(), ToolError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| memory_io(path, error))?;
    }
    #[cfg(not(unix))]
    let _ = (file, path);
    Ok(())
}

fn memory_io(path: &Path, error: std::io::Error) -> ToolError {
    ToolError::Execution(format!("{}: {error}", path.display()))
}

fn tool_output(content: String) -> Result<ToolOutput, ToolError> {
    Ok(ToolOutput {
        original_bytes: content.len(),
        content,
        is_error: false,
        structured: None,
        truncated: false,
        durable_content: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new(name: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("fx-memory-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn saves_deduplicates_lists_and_clears_profile_facts() {
        let fixture = Fixture::new("round-trip");
        let tool = MemoryTool::new(Some(&fixture.0));
        let context = ToolContext::new(fixture.0.clone());

        for _ in 0..2 {
            let saved = pollster::block_on(tool.execute(
                &context,
                json!({"action": "save", "fact": "prefers concise output"}),
            ))
            .unwrap();
            assert_eq!(saved.content, "remembered");
        }
        let listed = pollster::block_on(tool.execute(&context, json!({"action": "list"}))).unwrap();
        assert_eq!(listed.content, "- prefers concise output\n");
        let cleared =
            pollster::block_on(tool.execute(&context, json!({"action": "clear"}))).unwrap();
        assert_eq!(cleared.content, "memories cleared");
        assert_eq!(
            pollster::block_on(tool.execute(&context, json!({"action": "list"})))
                .unwrap()
                .content,
            "No saved memories"
        );
    }

    #[test]
    fn rejects_cross_action_fields_and_symbolic_memory_files() {
        let fixture = Fixture::new("validation");
        let tool = MemoryTool::new(Some(&fixture.0));
        let context = ToolContext::new(fixture.0.clone());
        let error = pollster::block_on(
            tool.execute(&context, json!({"action": "list", "fact": "not-owned"})),
        )
        .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArguments(_)));

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::create_dir(fixture.0.join(".fx")).unwrap();
            let outside = fixture.0.join("outside.json");
            fs::write(&outside, "[]").unwrap();
            symlink(&outside, fixture.0.join(".fx/memories.json")).unwrap();
            let error =
                pollster::block_on(tool.execute(&context, json!({"action": "list"}))).unwrap_err();
            assert!(error.to_string().contains("not a regular file"));
        }
    }
}
