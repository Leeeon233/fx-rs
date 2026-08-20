//! Durable asynchronous child-agent manager and its single `subagent` tool.
//!
//! The manager owns lifecycle, relationship, persistence, and explicit
//! inspection semantics. Model execution remains behind [`SubagentExecutor`]
//! so provider/auth selection stays in the application runtime.

mod domain;
mod store;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use domain::{
    ChildEvent, ChildSnapshot, CommandInput, CreateInput, InspectInput, InspectSection,
    LifecycleAction, MAX_MESSAGE_BYTES, MAX_MODEL_BYTES, MAX_NAME_BYTES, MAX_PAGE_LIMIT,
    MAX_PROMPT_BYTES, MAX_WAIT_MS, MessageInput, RelationshipAction, RootInput, ToolActivity,
};
pub use domain::{ChildMode, ChildRunRequest, ChildRunResult, ChildState};
use fx_core::{
    BoxFuture, CancellationSignal, ChatMessage, PermissionMode, PermissionRequest, Role, Tool,
    ToolContext, ToolEffect, ToolError, ToolOutput,
};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Notify;
use uuid::Uuid;

pub use store::{StoreError, SubagentStore};

const MAX_ACTIVITY_ITEMS: usize = 200;
const MAX_EVENT_ITEMS: usize = 200;
const DEFAULT_PAGE_LIMIT: usize = 50;

pub trait SubagentExecutor: Send + Sync {
    fn run(
        &self,
        request: ChildRunRequest,
        cancellation: Arc<SubagentCancellation>,
        events: Arc<dyn SubagentEventSink>,
    ) -> BoxFuture<'static, Result<ChildRunResult, ChildRunError>>;
}

pub trait SubagentEventSink: Send + Sync {
    fn emit(&self, event: SubagentEvent);
}

#[derive(Clone, Debug)]
pub enum SubagentEvent {
    ToolStarted {
        id: String,
        name: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, Error)]
pub enum ChildRunError {
    #[error("child run was cancelled")]
    Cancelled,
    #[error("child run failed: {0}")]
    Failed(String),
}

#[derive(Debug, Default)]
pub struct SubagentCancellation {
    cancelled: AtomicBool,
}

impl SubagentCancellation {
    fn reset(&self) {
        self.cancelled.store(false, Ordering::Release);
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

impl CancellationSignal for SubagentCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

struct Child {
    snapshot: Mutex<ChildSnapshot>,
    cancellation: Arc<SubagentCancellation>,
    notify: Notify,
}

impl Child {
    fn new(snapshot: ChildSnapshot) -> Self {
        Self {
            snapshot: Mutex::new(snapshot),
            cancellation: Arc::new(SubagentCancellation::default()),
            notify: Notify::new(),
        }
    }
}

pub struct SubagentManager {
    root_id: String,
    store: Option<SubagentStore>,
    children: Mutex<BTreeMap<String, Arc<Child>>>,
}

#[derive(Debug, Error)]
enum ManagerError {
    #[error("subagent command must select exactly one branch")]
    InvalidBranch,
    #[error("invalid subagent input: {0}")]
    InvalidInput(String),
    #[error("subagent `{0}` was not found or is outside caller authority")]
    NotFound(String),
    #[error("subagent `{0}` is busy")]
    Busy(String),
    #[error("subagent `{0}` does not support this operation")]
    Unsupported(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl SubagentManager {
    pub fn restore(
        root_id: impl Into<String>,
        store: Option<SubagentStore>,
    ) -> Result<Arc<Self>, StoreError> {
        let root_id = root_id.into();
        let mut restored = BTreeMap::new();
        let snapshots = match &store {
            Some(store) => store.load(&root_id)?,
            None => Vec::new(),
        };
        let now = now_ms();
        let mut reconciled = false;
        for mut snapshot in snapshots {
            if matches!(snapshot.state, ChildState::Queued | ChildState::Running) {
                snapshot.state = ChildState::Interrupted;
                snapshot.generation = snapshot.generation.saturating_add(1);
                snapshot.updated_at_ms = now;
                push_event(
                    &mut snapshot,
                    "interrupted",
                    "host restarted before the child settled",
                );
                reconciled = true;
            }
            restored.insert(snapshot.id.clone(), Arc::new(Child::new(snapshot)));
        }
        let manager = Arc::new(Self {
            root_id,
            store,
            children: Mutex::new(restored),
        });
        if reconciled {
            manager.persist()?;
        }
        Ok(manager)
    }

    pub fn child_count(&self) -> usize {
        self.children.lock().map_or(0, |children| children.len())
    }

    fn execute(
        self: &Arc<Self>,
        caller_id: &str,
        authority_mode: PermissionMode,
        executor: Arc<dyn SubagentExecutor>,
        command: CommandInput,
    ) -> BoxFuture<'static, Result<Value, ManagerError>> {
        let manager = self.clone();
        let caller_id = caller_id.to_owned();
        Box::pin(async move {
            if command.branch_count() != 1 {
                return Err(ManagerError::InvalidBranch);
            }
            if let Some(create) = command.create {
                return manager
                    .create(&caller_id, authority_mode, create, executor)
                    .map(|snapshot| outcome("created", Some(&snapshot), json!({})));
            }
            if let Some(inspect) = command.inspect {
                return manager.inspect(&caller_id, inspect).await;
            }
            if let Some(message) = command.message {
                return manager.message(&caller_id, message, executor);
            }
            if let Some(relationship) = command.relationship {
                return manager.relationship(&caller_id, relationship);
            }
            if let Some(configure) = command.configure {
                return manager.configure(&caller_id, authority_mode, configure);
            }
            if let Some(lifecycle) = command.lifecycle {
                return manager.lifecycle(&caller_id, lifecycle, executor);
            }
            Err(ManagerError::InvalidBranch)
        })
    }

    fn create(
        self: &Arc<Self>,
        caller_id: &str,
        authority_mode: PermissionMode,
        input: CreateInput,
        executor: Arc<dyn SubagentExecutor>,
    ) -> Result<ChildSnapshot, ManagerError> {
        validate_text("name", &input.name, MAX_NAME_BYTES)?;
        if let Some(model) = &input.model {
            validate_text("model", model, MAX_MODEL_BYTES)?;
        }
        if let Some(prompt) = &input.prompt {
            validate_text("prompt", prompt, MAX_PROMPT_BYTES)?;
        }
        if input.mode == ChildMode::OneOff && input.prompt.is_none() {
            return Err(ManagerError::InvalidInput(
                "one_off creation requires a prompt".into(),
            ));
        }
        let permission_mode = input.permission_mode.unwrap_or(authority_mode);
        if permission_rank(permission_mode) > permission_rank(authority_mode) {
            return Err(ManagerError::InvalidInput(
                "child permission mode cannot exceed caller authority".into(),
            ));
        }
        let now = now_ms();
        let id = format!("child-{}", Uuid::new_v4().simple());
        let model = input.model.unwrap_or_default();
        let prompt = input.prompt;
        let mut snapshot = ChildSnapshot {
            id: id.clone(),
            parent_id: Some(caller_id.to_owned()),
            name: input.name,
            mode: input.mode,
            model,
            permission_mode,
            state: if prompt.is_some() {
                ChildState::Queued
            } else {
                ChildState::Idle
            },
            generation: 1,
            created_at_ms: now,
            updated_at_ms: now,
            history: Vec::new(),
            queued_messages: Vec::new(),
            tool_activity: Vec::new(),
            events: Vec::new(),
            last_output: None,
            failure: None,
        };
        push_event(&mut snapshot, "created", "child admitted");
        let child = Arc::new(Child::new(snapshot.clone()));
        self.children
            .lock()
            .map_err(|_| ManagerError::InvalidInput("manager lock poisoned".into()))?
            .insert(id, child.clone());
        self.persist()?;
        if let Some(prompt) = prompt {
            self.spawn_run(child, prompt, executor);
        }
        Ok(snapshot)
    }

    async fn inspect(
        self: &Arc<Self>,
        caller_id: &str,
        input: InspectInput,
    ) -> Result<Value, ManagerError> {
        validate_id(&input.id)?;
        if input.sections.is_empty() {
            return Err(ManagerError::InvalidInput(
                "inspect requires at least one section".into(),
            ));
        }
        let child = self.authorized_child(caller_id, &input.id)?;
        let mut wait_timed_out = false;
        if let Some(wait) = &input.wait {
            if !input.sections.contains(&InspectSection::Status) || input.cursor.is_some() {
                return Err(ManagerError::InvalidInput(
                    "inspect wait requires status and cannot use cursor".into(),
                ));
            }
            if wait.timeout_ms == 0 || wait.timeout_ms > MAX_WAIT_MS {
                return Err(ManagerError::InvalidInput("invalid wait timeout".into()));
            }
            let deadline = tokio::time::Instant::now() + Duration::from_millis(wait.timeout_ms);
            loop {
                let notified = child.notify.notified();
                let ready = {
                    let snapshot = child
                        .snapshot
                        .lock()
                        .map_err(|_| ManagerError::InvalidInput("child lock poisoned".into()))?;
                    snapshot.state.is_settled()
                        && wait
                            .after_generation
                            .is_none_or(|generation| snapshot.generation > generation)
                };
                if ready {
                    break;
                }
                if tokio::time::timeout_at(deadline, notified).await.is_err() {
                    wait_timed_out = true;
                    break;
                }
            }
        }
        let snapshot = child
            .snapshot
            .lock()
            .map_err(|_| ManagerError::InvalidInput("child lock poisoned".into()))?
            .clone();
        project_inspection(snapshot, input, wait_timed_out)
    }

    fn message(
        self: &Arc<Self>,
        caller_id: &str,
        input: MessageInput,
        executor: Arc<dyn SubagentExecutor>,
    ) -> Result<Value, ManagerError> {
        let branches = usize::from(input.send.is_some()) + usize::from(input.milestone.is_some());
        if branches != 1 {
            return Err(ManagerError::InvalidBranch);
        }
        if let Some(milestone) = input.milestone {
            validate_text("milestone", &milestone.name, MAX_NAME_BYTES)?;
            if caller_id == self.root_id {
                return Err(ManagerError::Unsupported(caller_id.to_owned()));
            }
            let child = self.authorized_child(caller_id, caller_id)?;
            let snapshot = mutate_child(&child, |snapshot| {
                snapshot.generation = snapshot.generation.saturating_add(1);
                snapshot.updated_at_ms = now_ms();
                push_event(snapshot, "milestone", &milestone.name);
                snapshot.clone()
            })?;
            self.persist()?;
            child.notify.notify_waiters();
            return Ok(outcome(
                "milestone_emitted",
                Some(&snapshot),
                json!({"milestone": milestone.name}),
            ));
        }

        let send = input.send.expect("validated message branch");
        validate_id(&send.id)?;
        validate_text("message", &send.content, MAX_MESSAGE_BYTES)?;
        let child = self.authorized_child(caller_id, &send.id)?;
        let mut start = false;
        let snapshot = mutate_child(&child, |snapshot| {
            if snapshot.mode != ChildMode::Persistent || snapshot.state == ChildState::Archived {
                return Err(ManagerError::Unsupported(snapshot.id.clone()));
            }
            snapshot.generation = snapshot.generation.saturating_add(1);
            snapshot.updated_at_ms = now_ms();
            snapshot.failure = None;
            match snapshot.state {
                ChildState::Queued | ChildState::Running => {
                    snapshot.queued_messages.push(send.content.clone());
                }
                _ => {
                    snapshot.state = ChildState::Queued;
                    start = true;
                }
            }
            push_event(snapshot, "message_queued", "parent sent work");
            Ok(snapshot.clone())
        })??;
        self.persist()?;
        child.notify.notify_waiters();
        if start {
            self.spawn_run(child, send.content, executor);
        }
        Ok(outcome("queued", Some(&snapshot), json!({})))
    }

    fn configure(
        &self,
        caller_id: &str,
        authority_mode: PermissionMode,
        input: domain::ConfigureInput,
    ) -> Result<Value, ManagerError> {
        if input.name.is_none() && input.model.is_none() && input.permission_mode.is_none() {
            return Err(ManagerError::InvalidInput(
                "configure requires at least one change".into(),
            ));
        }
        let child = self.authorized_child(caller_id, &input.id)?;
        let snapshot = mutate_child(&child, |snapshot| {
            if matches!(snapshot.state, ChildState::Queued | ChildState::Running) {
                return Err(ManagerError::Busy(snapshot.id.clone()));
            }
            if let Some(name) = &input.name {
                validate_text("name", name, MAX_NAME_BYTES)?;
                snapshot.name.clone_from(name);
            }
            if let Some(model) = &input.model {
                validate_text("model", model, MAX_MODEL_BYTES)?;
                snapshot.model.clone_from(model);
            }
            if let Some(mode) = input.permission_mode {
                if permission_rank(mode) > permission_rank(authority_mode) {
                    return Err(ManagerError::InvalidInput(
                        "child permission mode cannot exceed caller authority".into(),
                    ));
                }
                snapshot.permission_mode = mode;
            }
            snapshot.generation = snapshot.generation.saturating_add(1);
            snapshot.updated_at_ms = now_ms();
            push_event(snapshot, "configured", "child configuration changed");
            Ok(snapshot.clone())
        })??;
        self.persist()?;
        child.notify.notify_waiters();
        Ok(outcome("configured", Some(&snapshot), json!({})))
    }

    fn relationship(
        &self,
        caller_id: &str,
        input: domain::RelationshipInput,
    ) -> Result<Value, ManagerError> {
        let child = self.authorized_child(caller_id, &input.id)?;
        if caller_id != self.root_id {
            return Err(ManagerError::Unsupported(input.id));
        }
        let next_parent = match input.action {
            RelationshipAction::Detach => None,
            RelationshipAction::Attach => Some(self.root_id.clone()),
            RelationshipAction::Reparent => {
                let parent = input.parent_id.ok_or_else(|| {
                    ManagerError::InvalidInput("reparent requires parent_id".into())
                })?;
                if !self.is_authorized(caller_id, &parent)
                    || self.relationship_would_cycle(&input.id, &parent)
                {
                    return Err(ManagerError::InvalidInput(
                        "invalid or cyclic parent relationship".into(),
                    ));
                }
                Some(parent)
            }
        };
        let snapshot = mutate_child(&child, |snapshot| {
            snapshot.parent_id = next_parent;
            snapshot.generation = snapshot.generation.saturating_add(1);
            snapshot.updated_at_ms = now_ms();
            push_event(
                snapshot,
                "relationship_changed",
                "parent relationship changed",
            );
            snapshot.clone()
        })?;
        self.persist()?;
        child.notify.notify_waiters();
        Ok(outcome("relationship_changed", Some(&snapshot), json!({})))
    }

    fn lifecycle(
        self: &Arc<Self>,
        caller_id: &str,
        input: domain::LifecycleInput,
        executor: Arc<dyn SubagentExecutor>,
    ) -> Result<Value, ManagerError> {
        let child = self.authorized_child(caller_id, &input.id)?;
        let mut resume_prompt = None;
        let snapshot = mutate_child(&child, |snapshot| {
            snapshot.generation = snapshot.generation.saturating_add(1);
            snapshot.updated_at_ms = now_ms();
            match input.action {
                LifecycleAction::Cancel => {
                    if snapshot.state.is_settled() {
                        return Err(ManagerError::Unsupported(snapshot.id.clone()));
                    }
                    child.cancellation.cancel();
                    snapshot.state = ChildState::Cancelled;
                    push_event(snapshot, "cancelled", "cancellation requested");
                }
                LifecycleAction::Close => {
                    child.cancellation.cancel();
                    snapshot.state = ChildState::Archived;
                    snapshot.queued_messages.clear();
                    push_event(snapshot, "archived", "child closed");
                }
                LifecycleAction::Reopen => {
                    if snapshot.state != ChildState::Archived {
                        return Err(ManagerError::Unsupported(snapshot.id.clone()));
                    }
                    snapshot.state = if snapshot.mode == ChildMode::Persistent {
                        ChildState::Idle
                    } else {
                        ChildState::Interrupted
                    };
                    push_event(snapshot, "reopened", "child reopened");
                }
                LifecycleAction::Resume => {
                    if !matches!(
                        snapshot.state,
                        ChildState::Interrupted | ChildState::Failed | ChildState::Cancelled
                    ) {
                        return Err(ManagerError::Unsupported(snapshot.id.clone()));
                    }
                    resume_prompt = Some(
                        snapshot
                            .queued_messages
                            .first()
                            .cloned()
                            .unwrap_or_else(|| {
                                "Resume the interrupted task. Reassess current workspace state before continuing."
                                    .into()
                            }),
                    );
                    if !snapshot.queued_messages.is_empty() {
                        snapshot.queued_messages.remove(0);
                    }
                    snapshot.state = ChildState::Queued;
                    snapshot.failure = None;
                    push_event(snapshot, "resumed", "child queued for resume");
                }
            }
            Ok(snapshot.clone())
        })??;
        self.persist()?;
        child.notify.notify_waiters();
        if let Some(prompt) = resume_prompt {
            self.spawn_run(child, prompt, executor);
        }
        Ok(outcome("lifecycle_changed", Some(&snapshot), json!({})))
    }

    fn spawn_run(
        self: &Arc<Self>,
        child: Arc<Child>,
        prompt: String,
        executor: Arc<dyn SubagentExecutor>,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run_child(child, prompt, executor).await;
        });
    }

    async fn run_child(
        self: Arc<Self>,
        child: Arc<Child>,
        mut prompt: String,
        executor: Arc<dyn SubagentExecutor>,
    ) {
        loop {
            child.cancellation.reset();
            let request = match mutate_child(&child, |snapshot| {
                if snapshot.state == ChildState::Archived {
                    return None;
                }
                snapshot.state = ChildState::Running;
                snapshot.generation = snapshot.generation.saturating_add(1);
                snapshot.updated_at_ms = now_ms();
                snapshot.failure = None;
                push_event(snapshot, "started", "child generation started");
                Some(ChildRunRequest {
                    id: snapshot.id.clone(),
                    name: snapshot.name.clone(),
                    model: snapshot.model.clone(),
                    permission_mode: snapshot.permission_mode,
                    history: snapshot.history.clone(),
                    prompt: prompt.clone(),
                })
            }) {
                Ok(Some(request)) => request,
                _ => return,
            };
            let _ = self.persist();
            child.notify.notify_waiters();
            let events: Arc<dyn SubagentEventSink> = Arc::new(LiveEvents {
                manager: Arc::downgrade(&self),
                child: child.clone(),
            });
            let result = executor
                .run(request, child.cancellation.clone(), events)
                .await;

            let next = mutate_child(&child, |snapshot| {
                snapshot.generation = snapshot.generation.saturating_add(1);
                snapshot.updated_at_ms = now_ms();
                if snapshot.state == ChildState::Archived {
                    return None;
                }
                match result {
                    Ok(result) if !child.cancellation.is_cancelled() => {
                        snapshot.history = bound_history(result.history);
                        snapshot.last_output = Some(result.output);
                        snapshot.failure = None;
                        snapshot.state = if snapshot.mode == ChildMode::Persistent {
                            ChildState::Idle
                        } else {
                            ChildState::Completed
                        };
                        push_event(snapshot, "settled", "child generation completed");
                    }
                    Ok(_) | Err(ChildRunError::Cancelled) => {
                        snapshot.state = ChildState::Cancelled;
                        snapshot.failure = None;
                        push_event(snapshot, "cancelled", "child generation cancelled");
                    }
                    Err(ChildRunError::Failed(ref error)) => {
                        snapshot.state = ChildState::Failed;
                        snapshot.failure = Some(truncate_owned(error, MAX_MESSAGE_BYTES));
                        push_event(snapshot, "failed", "child generation failed");
                    }
                }
                if snapshot.mode == ChildMode::Persistent
                    && snapshot.state != ChildState::Archived
                    && !snapshot.queued_messages.is_empty()
                {
                    let next = snapshot.queued_messages.remove(0);
                    snapshot.state = ChildState::Queued;
                    Some(next)
                } else {
                    None
                }
            })
            .ok()
            .flatten();
            let _ = self.persist();
            child.notify.notify_waiters();
            match next {
                Some(next) => prompt = next,
                None => return,
            }
        }
    }

    fn authorized_child(&self, caller_id: &str, id: &str) -> Result<Arc<Child>, ManagerError> {
        if !self.is_authorized(caller_id, id) {
            return Err(ManagerError::NotFound(id.to_owned()));
        }
        self.children
            .lock()
            .map_err(|_| ManagerError::InvalidInput("manager lock poisoned".into()))?
            .get(id)
            .cloned()
            .ok_or_else(|| ManagerError::NotFound(id.to_owned()))
    }

    fn is_authorized(&self, caller_id: &str, id: &str) -> bool {
        let Ok(children) = self.children.lock() else {
            return false;
        };
        if caller_id == self.root_id {
            return children.contains_key(id);
        }
        let mut current = id.to_owned();
        for _ in 0..children.len() {
            if current == caller_id {
                return true;
            }
            let Some(child) = children.get(&current) else {
                return false;
            };
            let Ok(snapshot) = child.snapshot.lock() else {
                return false;
            };
            let Some(parent) = snapshot.parent_id.clone() else {
                return false;
            };
            if parent == caller_id {
                return true;
            }
            current = parent;
        }
        false
    }

    fn relationship_would_cycle(&self, child_id: &str, parent_id: &str) -> bool {
        let Ok(children) = self.children.lock() else {
            return true;
        };
        let mut current = parent_id.to_owned();
        for _ in 0..=children.len() {
            if current == child_id {
                return true;
            }
            let Some(child) = children.get(&current) else {
                return false;
            };
            let Ok(snapshot) = child.snapshot.lock() else {
                return true;
            };
            let Some(parent) = snapshot.parent_id.clone() else {
                return false;
            };
            current = parent;
        }
        true
    }

    fn persist(&self) -> Result<(), StoreError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let children = self
            .children
            .lock()
            .map_err(|_| StoreError::Unavailable("manager lock poisoned".into()))?
            .values()
            .map(|child| {
                child
                    .snapshot
                    .lock()
                    .map(|snapshot| snapshot.clone())
                    .map_err(|_| StoreError::Unavailable("child lock poisoned".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        store.save(&self.root_id, children)
    }
}

struct LiveEvents {
    manager: Weak<SubagentManager>,
    child: Arc<Child>,
}

impl SubagentEventSink for LiveEvents {
    fn emit(&self, event: SubagentEvent) {
        let _ = mutate_child(&self.child, |snapshot| {
            snapshot.generation = snapshot.generation.saturating_add(1);
            snapshot.updated_at_ms = now_ms();
            let (phase, tool, call_id, is_error) = match event {
                SubagentEvent::ToolStarted { id, name } => ("started".to_owned(), name, id, None),
                SubagentEvent::ToolFinished { id, name, is_error } => {
                    ("finished".to_owned(), name, id, Some(is_error))
                }
            };
            let generation = snapshot.generation;
            snapshot.tool_activity.push(ToolActivity {
                generation,
                phase,
                tool,
                call_id,
                is_error,
            });
            retain_tail(&mut snapshot.tool_activity, MAX_ACTIVITY_ITEMS);
        });
        if let Some(manager) = self.manager.upgrade() {
            let _ = manager.persist();
        }
        self.child.notify.notify_waiters();
    }
}

#[derive(Clone)]
pub struct SubagentTool {
    manager: Arc<SubagentManager>,
    executor: Arc<dyn SubagentExecutor>,
    caller_id: String,
    authority_mode: PermissionMode,
}

impl SubagentTool {
    pub fn new(
        manager: Arc<SubagentManager>,
        executor: Arc<dyn SubagentExecutor>,
        caller_id: impl Into<String>,
        authority_mode: PermissionMode,
    ) -> Self {
        Self {
            manager,
            executor,
            caller_id: caller_id.into(),
            authority_mode,
        }
    }
}

impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }

    fn description(&self) -> &str {
        "Create, inspect, message, relate, configure, or control durable asynchronous child sessions. Select exactly one command branch. Creation returns immediately; use inspect.wait when the current turn needs a settled result."
    }

    fn input_schema(&self) -> Value {
        subagent_schema()
    }

    fn effect(&self, arguments: &Value) -> Result<ToolEffect, ToolError> {
        let read_only = arguments
            .pointer("/command/inspect")
            .is_some_and(|value| value.is_object());
        Ok(if read_only {
            ToolEffect::Read
        } else {
            ToolEffect::Delegation
        })
    }

    fn permission_requests(
        &self,
        _context: &ToolContext,
        _arguments: &Value,
    ) -> Result<Vec<PermissionRequest>, ToolError> {
        // The manager enforces explicit caller/descendant authority, and child
        // tool effects are independently admitted by the child's own engine.
        Ok(Vec::new())
    }

    fn execute<'a>(
        &'a self,
        _context: &'a ToolContext,
        arguments: Value,
    ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
        Box::pin(async move {
            let root: RootInput = serde_json::from_value(arguments)
                .map_err(|error| ToolError::InvalidArguments(error.to_string()))?;
            let value = self
                .manager
                .execute(
                    &self.caller_id,
                    self.authority_mode,
                    self.executor.clone(),
                    root.command,
                )
                .await
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            let content = serde_json::to_string(&value)
                .map_err(|error| ToolError::Execution(error.to_string()))?;
            Ok(ToolOutput {
                original_bytes: content.len(),
                content,
                is_error: false,
                structured: Some(value),
                truncated: false,
                durable_content: None,
            })
        })
    }
}

fn project_inspection(
    snapshot: ChildSnapshot,
    input: InspectInput,
    wait_timed_out: bool,
) -> Result<Value, ManagerError> {
    let offset = input
        .cursor
        .as_deref()
        .map(str::parse::<usize>)
        .transpose()
        .map_err(|_| ManagerError::InvalidInput("invalid inspect cursor".into()))?
        .unwrap_or(0);
    let limit = input.limit.unwrap_or(DEFAULT_PAGE_LIMIT);
    if limit == 0 || limit > MAX_PAGE_LIMIT {
        return Err(ManagerError::InvalidInput("invalid inspect limit".into()));
    }
    let mut requested = serde_json::Map::new();
    for section in input.sections {
        match section {
            InspectSection::Status => {
                requested.insert(
                    "status".into(),
                    json!({
                        "state": snapshot.state,
                        "generation": snapshot.generation,
                        "wait_timed_out": wait_timed_out,
                        "last_output": snapshot.last_output,
                        "failure": snapshot.failure,
                    }),
                );
            }
            InspectSection::Messages => {
                if offset > snapshot.history.len() {
                    return Err(ManagerError::InvalidInput(
                        "inspect cursor is out of range".into(),
                    ));
                }
                let end = (offset + limit).min(snapshot.history.len());
                requested.insert(
                    "messages".into(),
                    json!({
                        "items": &snapshot.history[offset..end],
                        "next_cursor": (end < snapshot.history.len()).then(|| end.to_string()),
                        "queued": snapshot.queued_messages,
                    }),
                );
            }
            InspectSection::ToolActivity => {
                requested.insert("tool_activity".into(), json!(snapshot.tool_activity));
            }
            InspectSection::Events => {
                requested.insert("events".into(), json!(snapshot.events));
            }
            InspectSection::Configuration => {
                requested.insert(
                    "configuration".into(),
                    json!({
                        "name": snapshot.name,
                        "mode": snapshot.mode,
                        "model": snapshot.model,
                        "permission_mode": snapshot.permission_mode,
                    }),
                );
            }
            InspectSection::Relationship => {
                requested.insert(
                    "relationship".into(),
                    json!({"parent_id": snapshot.parent_id}),
                );
            }
        }
    }
    Ok(outcome(
        if wait_timed_out {
            "wait_timed_out"
        } else {
            "inspected"
        },
        Some(&snapshot),
        Value::Object(requested),
    ))
}

fn outcome(status: &str, child: Option<&ChildSnapshot>, requested: Value) -> Value {
    json!({
        "ok": true,
        "child_id": child.map(|child| child.id.as_str()),
        "status": status,
        "error_code": null,
        "retryable": false,
        "generation": child.map(|child| child.generation),
        "requested": requested,
        "cursor": null,
    })
}

fn subagent_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "object",
                "properties": {
                    "create": {
                        "type": "object",
                        "properties": {
                            "name": {"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},
                            "mode": {"type":"string","enum":["one_off","persistent"]},
                            "prompt": {"type":"string","minLength":1,"maxLength":MAX_PROMPT_BYTES},
                            "model": {"type":"string","minLength":1,"maxLength":MAX_MODEL_BYTES},
                            "permission_mode": {"type":"string","enum":["ask","auto","yolo"]}
                        },
                        "required": ["name","mode"], "additionalProperties": false
                    },
                    "inspect": {
                        "type": "object",
                        "properties": {
                            "id": {"type":"string","minLength":1},
                            "sections": {"type":"array","minItems":1,"maxItems":6,"items":{"type":"string","enum":["status","messages","tool_activity","events","configuration","relationship"]}},
                            "cursor": {"type":"string","minLength":1},
                            "limit": {"type":"integer","minimum":1,"maximum":MAX_PAGE_LIMIT},
                            "wait": {"type":"object","properties":{
                                "until":{"type":"string","enum":["settled"]},
                                "after_generation":{"type":"integer","minimum":0},
                                "timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS}
                            },"required":["until","timeout_ms"],"additionalProperties":false}
                        },
                        "required": ["id","sections"], "additionalProperties": false
                    },
                    "message": {
                        "type":"object","properties":{
                            "send":{"type":"object","properties":{"id":{"type":"string","minLength":1},"content":{"type":"string","minLength":1,"maxLength":MAX_MESSAGE_BYTES}},"required":["id","content"],"additionalProperties":false},
                            "milestone":{"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES}},"required":["name"],"additionalProperties":false}
                        },"minProperties":1,"maxProperties":1,"additionalProperties":false
                    },
                    "relationship": {
                        "type":"object","properties":{"action":{"type":"string","enum":["attach","detach","reparent"]},"id":{"type":"string","minLength":1},"parent_id":{"type":"string","minLength":1}},"required":["action","id"],"additionalProperties":false
                    },
                    "configure": {
                        "type":"object","properties":{"id":{"type":"string","minLength":1},"name":{"type":"string","minLength":1,"maxLength":MAX_NAME_BYTES},"model":{"type":"string","minLength":1,"maxLength":MAX_MODEL_BYTES},"permission_mode":{"type":"string","enum":["ask","auto","yolo"]}},"required":["id"],"additionalProperties":false
                    },
                    "lifecycle": {
                        "type":"object","properties":{"id":{"type":"string","minLength":1},"action":{"type":"string","enum":["cancel","resume","close","reopen"]}},"required":["id","action"],"additionalProperties":false
                    }
                },
                "minProperties": 1,
                "maxProperties": 1,
                "additionalProperties": false
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

fn mutate_child<T>(
    child: &Child,
    mutate: impl FnOnce(&mut ChildSnapshot) -> T,
) -> Result<T, ManagerError> {
    let mut snapshot = child
        .snapshot
        .lock()
        .map_err(|_| ManagerError::InvalidInput("child lock poisoned".into()))?;
    Ok(mutate(&mut snapshot))
}

fn validate_text(field: &str, value: &str, max: usize) -> Result<(), ManagerError> {
    if value.trim().is_empty()
        || value.len() > max
        || value
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        Err(ManagerError::InvalidInput(format!("invalid {field}")))
    } else {
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), ManagerError> {
    if id.is_empty()
        || id.len() > 255
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        Err(ManagerError::InvalidInput("invalid child id".into()))
    } else {
        Ok(())
    }
}

fn permission_rank(mode: PermissionMode) -> u8 {
    match mode {
        PermissionMode::Ask => 0,
        PermissionMode::Auto => 1,
        PermissionMode::Yolo => 2,
    }
}

fn push_event(snapshot: &mut ChildSnapshot, kind: &str, detail: &str) {
    snapshot.events.push(ChildEvent {
        generation: snapshot.generation,
        kind: kind.to_owned(),
        detail: detail.to_owned(),
        at_ms: now_ms(),
    });
    retain_tail(&mut snapshot.events, MAX_EVENT_ITEMS);
}

fn retain_tail<T>(items: &mut Vec<T>, max: usize) {
    if items.len() > max {
        items.drain(..items.len() - max);
    }
}

fn bound_history(mut history: Vec<ChatMessage>) -> Vec<ChatMessage> {
    const MAX_MESSAGES: usize = 256;
    if history.len() <= MAX_MESSAGES {
        return history;
    }
    let system = history
        .first()
        .filter(|message| message.role == Role::System)
        .cloned();
    let tail = history.split_off(history.len() - (MAX_MESSAGES - usize::from(system.is_some())));
    system.into_iter().chain(tail).collect()
}

fn truncate_owned(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn now_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestExecutor {
        calls: AtomicUsize,
    }

    impl SubagentExecutor for TestExecutor {
        fn run(
            &self,
            request: ChildRunRequest,
            cancellation: Arc<SubagentCancellation>,
            events: Arc<dyn SubagentEventSink>,
        ) -> BoxFuture<'static, Result<ChildRunResult, ChildRunError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                events.emit(SubagentEvent::ToolStarted {
                    id: "read-1".into(),
                    name: "read_file".into(),
                });
                tokio::task::yield_now().await;
                if cancellation.is_cancelled() {
                    return Err(ChildRunError::Cancelled);
                }
                events.emit(SubagentEvent::ToolFinished {
                    id: "read-1".into(),
                    name: "read_file".into(),
                    is_error: false,
                });
                let mut history = request.history;
                history.push(ChatMessage::text(Role::User, request.prompt));
                history.push(ChatMessage::text(Role::Assistant, "done"));
                Ok(ChildRunResult {
                    history,
                    output: "done".into(),
                })
            })
        }
    }

    fn manager() -> (Arc<SubagentManager>, Arc<TestExecutor>) {
        let manager = SubagentManager::restore("root", None).unwrap();
        let executor = Arc::new(TestExecutor {
            calls: AtomicUsize::new(0),
        });
        (manager, executor)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_off_create_returns_before_inspect_wait_observes_completion() {
        let (manager, executor) = manager();
        let created = manager
            .execute(
                "root",
                PermissionMode::Auto,
                executor,
                serde_json::from_value::<RootInput>(json!({"command":{"create":{
                    "name":"worker","mode":"one_off","prompt":"inspect files"
                }}}))
                .unwrap()
                .command,
            )
            .await
            .unwrap();
        assert_eq!(created["status"], "created");
        let id = created["child_id"].as_str().unwrap();
        let inspected = manager
            .execute(
                "root",
                PermissionMode::Auto,
                Arc::new(TestExecutor {
                    calls: AtomicUsize::new(0),
                }),
                serde_json::from_value::<RootInput>(json!({"command":{"inspect":{
                    "id":id,"sections":["status","messages","tool_activity"],
                    "wait":{"until":"settled","timeout_ms":1000}
                }}}))
                .unwrap()
                .command,
            )
            .await
            .unwrap();
        assert_eq!(inspected["requested"]["status"]["state"], "completed");
        assert_eq!(
            inspected["requested"]["tool_activity"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn persistent_children_accept_follow_up_messages() {
        let (manager, executor) = manager();
        let created = manager
            .execute(
                "root",
                PermissionMode::Auto,
                executor.clone(),
                serde_json::from_value::<RootInput>(json!({"command":{"create":{
                    "name":"worker","mode":"persistent"
                }}}))
                .unwrap()
                .command,
            )
            .await
            .unwrap();
        let id = created["child_id"].as_str().unwrap();
        manager
            .execute(
                "root",
                PermissionMode::Auto,
                executor.clone(),
                serde_json::from_value::<RootInput>(json!({"command":{"message":{"send":{
                    "id":id,"content":"first"
                }}}}))
                .unwrap()
                .command,
            )
            .await
            .unwrap();
        manager
            .inspect(
                "root",
                serde_json::from_value(json!({
                    "id":id,"sections":["status"],
                    "wait":{"until":"settled","timeout_ms":1000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn store_restores_running_children_as_interrupted() {
        let unique = Uuid::new_v4().simple().to_string();
        let root = std::env::temp_dir().join(format!("fx-subagent-{unique}"));
        let store = SubagentStore::new(&root);
        let manager = SubagentManager::restore("root", Some(store.clone())).unwrap();
        let now = now_ms();
        manager.children.lock().unwrap().insert(
            "child-test".into(),
            Arc::new(Child::new(ChildSnapshot {
                id: "child-test".into(),
                parent_id: Some("root".into()),
                name: "test".into(),
                mode: ChildMode::OneOff,
                model: "model".into(),
                permission_mode: PermissionMode::Auto,
                state: ChildState::Running,
                generation: 1,
                created_at_ms: now,
                updated_at_ms: now,
                history: Vec::new(),
                queued_messages: Vec::new(),
                tool_activity: Vec::new(),
                events: Vec::new(),
                last_output: None,
                failure: None,
            })),
        );
        manager.persist().unwrap();
        drop(manager);
        let restored = SubagentManager::restore("root", Some(store)).unwrap();
        let state = restored
            .children
            .lock()
            .unwrap()
            .get("child-test")
            .unwrap()
            .snapshot
            .lock()
            .unwrap()
            .state;
        assert_eq!(state, ChildState::Interrupted);
        let _ = std::fs::remove_dir_all(root);
    }
}
