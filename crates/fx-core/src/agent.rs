use std::collections::BTreeSet;
use std::future::poll_fn;
use std::sync::Arc;
use std::task::Poll;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    BoxFuture, CachePolicy, CancellationSignal, ChatMessage, ContextProjector,
    DEFAULT_HISTORY_CONTEXT_TOKENS, DeterministicContextProjector, Gateway, GatewayError,
    GatewayEvent, GatewayEventSink, GatewayRequest, LARGE_TOOL_RESULT_BYTES, NeverCancelled,
    PermissionDecision, PermissionEngine, PermissionRequest, Role, TOOL_RESULT_PREVIEW_BYTES,
    ToolArgumentIntegrity, ToolError, ToolExecutionProvenance, ToolOutput, ToolPreparation,
    ToolRegistry, ToolReview, Usage,
};

#[derive(Clone, Debug)]
pub struct AgentOptions {
    pub model: String,
    pub max_steps: usize,
    pub max_output_tokens: Option<u32>,
    pub history_context_tokens: usize,
}

impl AgentOptions {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_steps: 50,
            max_output_tokens: None,
            history_context_tokens: DEFAULT_HISTORY_CONTEXT_TOKENS,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AgentRequest {
    pub history: Vec<ChatMessage>,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStopReason {
    Complete,
    StepLimit,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct AgentResult {
    pub messages: Vec<ChatMessage>,
    pub output: String,
    pub usage: Usage,
    pub steps: usize,
    pub stop_reason: AgentStopReason,
    pub delivery_ambiguous: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowForSession,
    Deny,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalKind {
    User,
    Automatic,
}

#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub kind: ApprovalKind,
    pub tool_call_id: String,
    pub tool_name: String,
    pub arguments_json: String,
    pub permission_requests: Vec<PermissionRequest>,
    pub irreversible: bool,
    pub review: Option<ToolReview>,
}

#[derive(Debug, Error)]
pub enum ApprovalError {
    #[error("approval is unavailable: {0}")]
    Unavailable(String),
}

pub trait ApprovalHandler: Send {
    fn review<'a>(
        &'a mut self,
        request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>>;
}

#[derive(Clone, Copy, Debug)]
pub struct StaticApprovalHandler {
    decision: ApprovalDecision,
}

impl StaticApprovalHandler {
    pub fn allow_once() -> Self {
        Self {
            decision: ApprovalDecision::AllowOnce,
        }
    }

    pub fn deny() -> Self {
        Self {
            decision: ApprovalDecision::Deny,
        }
    }
}

impl ApprovalHandler for StaticApprovalHandler {
    fn review<'a>(
        &'a mut self,
        _request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(async move { Ok(self.decision) })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    Gateway(GatewayEvent),
    ToolStarted {
        id: String,
        name: String,
        arguments_json: String,
    },
    ToolFinished {
        id: String,
        name: String,
        is_error: bool,
        output: ToolOutput,
    },
}

pub trait AgentEventSink: Send {
    fn emit(&mut self, event: AgentEvent);
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Gateway(#[from] GatewayError),
    #[error(transparent)]
    Approval(#[from] ApprovalError),
    #[error(transparent)]
    ProjectContext(#[from] crate::ScopedProjectContextError),
}

/// Provider-neutral agent orchestration.
///
/// The loop owns message ordering, permission admission, provider/local tool
/// provenance, and the prepare-review-commit boundary. Network, UI, and disk
/// remain behind injected traits.
pub struct Agent {
    gateway: Arc<dyn Gateway>,
    tools: Arc<ToolRegistry>,
    options: AgentOptions,
    context_projector: Arc<dyn ContextProjector>,
}

impl Agent {
    pub fn new(gateway: Arc<dyn Gateway>, tools: Arc<ToolRegistry>, options: AgentOptions) -> Self {
        Self {
            gateway,
            tools,
            options,
            context_projector: Arc::new(DeterministicContextProjector),
        }
    }

    pub fn with_context_projector(mut self, projector: Arc<dyn ContextProjector>) -> Self {
        self.context_projector = projector;
        self
    }

    pub fn run<'a>(
        &'a self,
        request: AgentRequest,
        tool_context: &'a crate::ToolContext,
        permissions: &'a mut PermissionEngine,
        approvals: &'a mut dyn ApprovalHandler,
        events: &'a mut dyn AgentEventSink,
    ) -> BoxFuture<'a, Result<AgentResult, AgentError>> {
        self.run_controlled(
            request,
            tool_context,
            permissions,
            approvals,
            events,
            Arc::new(NeverCancelled),
        )
    }

    pub fn run_controlled<'a>(
        &'a self,
        request: AgentRequest,
        tool_context: &'a crate::ToolContext,
        permissions: &'a mut PermissionEngine,
        approvals: &'a mut dyn ApprovalHandler,
        events: &'a mut dyn AgentEventSink,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> BoxFuture<'a, Result<AgentResult, AgentError>> {
        Box::pin(async move {
            let mut messages = request.history;
            messages.push(ChatMessage::text(Role::User, request.prompt));
            let mut usage = Usage::default();
            let mut output = String::new();
            let mut delivery_ambiguous = false;
            let mut invocation_context = tool_context.clone();
            invocation_context.cancellation = cancellation.clone();

            let mut step = 0usize;
            while self.options.max_steps == 0 || step < self.options.max_steps {
                if cancellation.is_cancelled() {
                    return Ok(cancelled_result(
                        messages,
                        output,
                        usage,
                        step,
                        delivery_ambiguous,
                    ));
                }
                step += 1;
                let projection = self
                    .context_projector
                    .project(&messages, self.options.history_context_tokens);
                let gateway_request = GatewayRequest {
                    model: self.options.model.clone(),
                    messages: projection.messages,
                    tools: self.tools.advertisements(),
                    tool_choice: crate::ToolChoice::Auto,
                    max_output_tokens: self.options.max_output_tokens,
                };
                let response = {
                    let mut bridge = GatewayEventBridge(events);
                    match self.gateway.complete(gateway_request, &mut bridge).await {
                        Err(GatewayError::Cancelled) if cancellation.is_cancelled() => {
                            return Ok(cancelled_result(
                                messages,
                                output,
                                usage,
                                step,
                                delivery_ambiguous,
                            ));
                        }
                        result => result?,
                    }
                };
                accumulate_usage(&mut usage, response.usage);
                delivery_ambiguous |= response.delivery_ambiguous;
                if let Some(content) = &response.content {
                    output.push_str(content);
                }

                messages.push(ChatMessage {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: response.tool_calls.clone(),
                    permission_feedback: false,
                    cache_policy: CachePolicy::Default,
                });

                if cancellation.is_cancelled() {
                    return Ok(cancelled_result(
                        messages,
                        output,
                        usage,
                        step,
                        delivery_ambiguous,
                    ));
                }

                if response.tool_calls.is_empty() {
                    return Ok(AgentResult {
                        messages,
                        output,
                        usage,
                        steps: step,
                        stop_reason: AgentStopReason::Complete,
                        delivery_ambiguous,
                    });
                }

                let calls = response.tool_calls;
                let (context_delta, context_deferred) =
                    select_scoped_project_context(&self.tools, &invocation_context, &calls)?;
                if let Some(delta) = context_delta {
                    insert_stable_system_context(&mut messages, delta);
                }
                let outcomes = execute_tool_batch(
                    &self.tools,
                    &invocation_context,
                    permissions,
                    approvals,
                    events,
                    &calls,
                    &context_deferred,
                )
                .await?;
                for (call, (tool_output, permission_feedback)) in calls.into_iter().zip(outcomes) {
                    messages.push(ChatMessage {
                        role: Role::Tool,
                        content: Some(tool_output.content),
                        tool_call_id: Some(call.id),
                        tool_name: Some(call.name),
                        tool_calls: Vec::new(),
                        permission_feedback,
                        cache_policy: CachePolicy::Default,
                    });
                }
            }

            Ok(AgentResult {
                messages,
                output,
                usage,
                steps: step,
                stop_reason: AgentStopReason::StepLimit,
                delivery_ambiguous,
            })
        })
    }
}

fn cancelled_result(
    messages: Vec<ChatMessage>,
    output: String,
    usage: Usage,
    steps: usize,
    delivery_ambiguous: bool,
) -> AgentResult {
    AgentResult {
        messages,
        output,
        usage,
        steps,
        stop_reason: AgentStopReason::Cancelled,
        delivery_ambiguous,
    }
}

async fn execute_tool_batch(
    tools: &ToolRegistry,
    context: &crate::ToolContext,
    permissions: &mut PermissionEngine,
    approvals: &mut dyn ApprovalHandler,
    events: &mut dyn AgentEventSink,
    calls: &[crate::ToolCall],
    context_deferred: &[bool],
) -> Result<Vec<(ToolOutput, bool)>, AgentError> {
    let mut outcomes = Vec::with_capacity(calls.len());
    let mut index = 0usize;
    while index < calls.len() {
        if context.cancellation.is_cancelled() {
            outcomes.extend(
                calls[index..]
                    .iter()
                    .map(|_| (error_output(ToolError::Cancelled.to_string()), false)),
            );
            break;
        }
        if context_deferred.get(index).copied().unwrap_or(false) {
            let call = &calls[index];
            emit_tool_started(events, call);
            let output = error_output(
                "tool execution deferred because new scoped project instructions were loaded; review the new rules and retry the action",
            );
            emit_tool_finished(events, call, &output);
            outcomes.push((output, false));
            index += 1;
            continue;
        }
        let parallel_len = parallel_read_prefix_len(tools, &calls[index..]);
        if parallel_len >= 2 {
            let batch = &calls[index..index + parallel_len];
            for call in batch {
                emit_tool_started(events, call);
            }
            let mut executions = Vec::with_capacity(batch.len());
            for call in batch {
                let admission =
                    admit_local_tool(tools, context, permissions, approvals, call).await?;
                executions.push(admission.into_future(context.clone()));
            }
            let mut completed = join_ordered(executions).await;
            for (call, outcome) in batch.iter().zip(&mut completed) {
                finalize_tool_output(context, call, &mut outcome.0);
                emit_tool_finished(events, call, &outcome.0);
            }
            outcomes.extend(completed);
            index += parallel_len;
            continue;
        }

        let call = &calls[index];
        emit_tool_started(events, call);
        let mut outcome = execute_one_tool(tools, context, permissions, approvals, call).await?;
        finalize_tool_output(context, call, &mut outcome.0);
        emit_tool_finished(events, call, &outcome.0);
        outcomes.push(outcome);
        index += 1;
    }
    Ok(outcomes)
}

fn select_scoped_project_context(
    tools: &ToolRegistry,
    context: &crate::ToolContext,
    calls: &[crate::ToolCall],
) -> Result<(Option<String>, Vec<bool>), AgentError> {
    let mut deferred = vec![false; calls.len()];
    let Some(provider) = &context.project_context else {
        return Ok((None, deferred));
    };
    let mut targets = BTreeSet::new();
    let mut sensitive_with_targets = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        if call.provenance != ToolExecutionProvenance::FxLocal
            || call.argument_integrity == ToolArgumentIntegrity::MalformedJson
        {
            continue;
        }
        let Ok((tool, arguments)) = tools.validate_call(call) else {
            continue;
        };
        let Ok(call_targets) = tool.project_context_targets(context, &arguments) else {
            continue;
        };
        if call_targets.is_empty() {
            continue;
        }
        let sensitive = !matches!(tool.effect(&arguments), Ok(crate::ToolEffect::Read));
        targets.extend(call_targets);
        if sensitive {
            sensitive_with_targets.push(index);
        }
    }
    if targets.is_empty() {
        return Ok((None, deferred));
    }
    let targets = targets.into_iter().collect::<Vec<_>>();
    let delta = provider.select(&targets)?;
    if delta.is_some() {
        for index in sensitive_with_targets {
            deferred[index] = true;
        }
    }
    Ok((delta, deferred))
}

fn insert_stable_system_context(messages: &mut Vec<ChatMessage>, content: String) {
    let prefix_end = messages
        .iter()
        .take_while(|message| message.role == Role::System)
        .count();
    messages.insert(prefix_end, ChatMessage::text(Role::System, content));
}

fn parallel_read_prefix_len(tools: &ToolRegistry, calls: &[crate::ToolCall]) -> usize {
    calls
        .iter()
        .take_while(|call| {
            if call.provenance != ToolExecutionProvenance::FxLocal
                || call.argument_integrity == ToolArgumentIntegrity::MalformedJson
            {
                return false;
            }
            let Ok((tool, arguments)) = tools.validate_call(call) else {
                return false;
            };
            matches!(tool.effect(&arguments), Ok(crate::ToolEffect::Read))
        })
        .count()
}

fn emit_tool_started(events: &mut dyn AgentEventSink, call: &crate::ToolCall) {
    events.emit(AgentEvent::ToolStarted {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments_json: call.arguments_json.clone(),
    });
}

fn emit_tool_finished(
    events: &mut dyn AgentEventSink,
    call: &crate::ToolCall,
    output: &ToolOutput,
) {
    events.emit(AgentEvent::ToolFinished {
        id: call.id.clone(),
        name: call.name.clone(),
        is_error: output.is_error,
        output: output.clone(),
    });
}

async fn execute_one_tool(
    tools: &ToolRegistry,
    context: &crate::ToolContext,
    permissions: &mut PermissionEngine,
    approvals: &mut dyn ApprovalHandler,
    call: &crate::ToolCall,
) -> Result<(ToolOutput, bool), AgentError> {
    if call.provenance == ToolExecutionProvenance::Provider {
        let content = call
            .provider_result
            .clone()
            .unwrap_or_else(|| "provider-executed tool returned no result".into());
        return Ok((
            ToolOutput {
                original_bytes: content.len(),
                content,
                is_error: false,
                structured: None,
                truncated: false,
                durable_content: None,
            },
            false,
        ));
    }
    if call.argument_integrity == ToolArgumentIntegrity::MalformedJson {
        return Ok((error_output("tool arguments were malformed JSON"), false));
    }
    let admission = admit_local_tool(tools, context, permissions, approvals, call).await?;
    Ok(admission.into_future(context.clone()).await)
}

enum LocalToolAdmission {
    Immediate(ToolOutput, bool),
    Direct {
        tool: Arc<dyn crate::Tool>,
        arguments: serde_json::Value,
    },
    Prepared(crate::PreparedToolCall),
}

impl LocalToolAdmission {
    fn into_future(self, context: crate::ToolContext) -> BoxFuture<'static, (ToolOutput, bool)> {
        Box::pin(async move {
            match self {
                Self::Immediate(output, permission_feedback) => (output, permission_feedback),
                Self::Direct { tool, arguments } => {
                    if context.cancellation.is_cancelled() {
                        return (error_output(ToolError::Cancelled.to_string()), false);
                    }
                    match tool.execute(&context, arguments).await {
                        Ok(output) => (output, false),
                        Err(error) => (error_output(error.to_string()), false),
                    }
                }
                Self::Prepared(prepared) => {
                    if context.cancellation.is_cancelled() {
                        return (error_output(ToolError::Cancelled.to_string()), false);
                    }
                    match prepared.commit(&context).await {
                        Ok(output) => (output, false),
                        Err(error) => (error_output(error.to_string()), false),
                    }
                }
            }
        })
    }
}

async fn admit_local_tool(
    tools: &ToolRegistry,
    context: &crate::ToolContext,
    permissions: &mut PermissionEngine,
    approvals: &mut dyn ApprovalHandler,
    call: &crate::ToolCall,
) -> Result<LocalToolAdmission, AgentError> {
    let (tool, arguments) = match tools.validate_call(call) {
        Ok(validated) => validated,
        Err(error) => {
            return Ok(LocalToolAdmission::Immediate(
                error_output(error.to_string()),
                false,
            ));
        }
    };
    let preparation = match tool.prepare(context, &arguments) {
        Ok(preparation) => preparation,
        Err(error) => {
            return Ok(LocalToolAdmission::Immediate(
                error_output(error.to_string()),
                false,
            ));
        }
    };
    let (requests, irreversible, review) = match &preparation {
        ToolPreparation::Direct {
            permission_requests,
            irreversible,
        } => (permission_requests, *irreversible, None),
        ToolPreparation::Prepared(prepared) => (
            &prepared.permission_requests,
            prepared.irreversible,
            prepared.review.clone(),
        ),
    };

    let decisions: Vec<_> = requests
        .iter()
        .map(|request| permissions.decide(request))
        .collect();
    if decisions.contains(&PermissionDecision::Deny) {
        return Ok(LocalToolAdmission::Immediate(
            error_output("tool permission denied"),
            true,
        ));
    }
    if decisions.contains(&PermissionDecision::Ask)
        || decisions.contains(&PermissionDecision::AutoReview)
    {
        let kind = if decisions.contains(&PermissionDecision::Ask) {
            ApprovalKind::User
        } else {
            ApprovalKind::Automatic
        };
        let review_request = ApprovalRequest {
            kind,
            tool_call_id: call.id.clone(),
            tool_name: tool.name().to_owned(),
            arguments_json: call.arguments_json.clone(),
            permission_requests: requests.clone(),
            irreversible,
            review,
        };
        match approvals.review(review_request).await? {
            ApprovalDecision::AllowOnce => {}
            ApprovalDecision::AllowForSession => {
                for (request, decision) in requests.iter().zip(decisions) {
                    if matches!(
                        decision,
                        PermissionDecision::Ask | PermissionDecision::AutoReview
                    ) {
                        permissions.grant_request_for_session(request);
                    }
                }
            }
            ApprovalDecision::Deny => {
                let reason = match kind {
                    ApprovalKind::User => "tool permission denied by user",
                    ApprovalKind::Automatic => "tool permission denied by automatic safety review",
                };
                return Ok(LocalToolAdmission::Immediate(error_output(reason), true));
            }
        }
    }

    if context.cancellation.is_cancelled() {
        return Ok(LocalToolAdmission::Immediate(
            error_output(ToolError::Cancelled.to_string()),
            false,
        ));
    }

    Ok(match preparation {
        ToolPreparation::Direct { .. } => LocalToolAdmission::Direct { tool, arguments },
        ToolPreparation::Prepared(prepared) => LocalToolAdmission::Prepared(prepared),
    })
}

async fn join_ordered<T>(futures: Vec<BoxFuture<'static, T>>) -> Vec<T> {
    let mut futures = futures.into_iter().map(Some).collect::<Vec<_>>();
    let mut outputs = (0..futures.len()).map(|_| None).collect::<Vec<_>>();
    poll_fn(move |task_context| {
        let mut remaining = 0usize;
        for (index, future) in futures.iter_mut().enumerate() {
            let Some(pending) = future.as_mut() else {
                continue;
            };
            match pending.as_mut().poll(task_context) {
                Poll::Ready(output) => {
                    outputs[index] = Some(output);
                    *future = None;
                }
                Poll::Pending => remaining += 1,
            }
        }
        if remaining == 0 {
            Poll::Ready(
                outputs
                    .iter_mut()
                    .map(|output| output.take().expect("completed future has output"))
                    .collect(),
            )
        } else {
            Poll::Pending
        }
    })
    .await
}

fn error_output(content: impl Into<String>) -> ToolOutput {
    let content = content.into();
    let original_bytes = content.len();
    ToolOutput {
        content,
        is_error: true,
        structured: None,
        original_bytes,
        truncated: false,
        durable_content: None,
    }
}

fn finalize_tool_output(
    context: &crate::ToolContext,
    call: &crate::ToolCall,
    output: &mut ToolOutput,
) {
    let Some(durable) = output.durable_content.take() else {
        if let std::borrow::Cow::Owned(redacted) = crate::redact_secrets(&output.content) {
            output.content = redacted;
            output.structured = None;
        }
        return;
    };
    output.original_bytes = output.original_bytes.max(durable.len());
    let durable = crate::redact_secrets(&durable).into_owned();
    if durable.len() > LARGE_TOOL_RESULT_BYTES
        && let Some(store) = &context.tool_results
    {
        match store.store(&call.id, &call.name, &durable) {
            Ok(stored) => {
                let preview = utf8_prefix(&durable, TOOL_RESULT_PREVIEW_BYTES);
                output.content = format!(
                    "<tool_result_preview handle=\"{}\" stored_bytes=\"{}\">\n{}\n</tool_result_preview>\n<tool_result_handle>{}</tool_result_handle>\nFull result is stored outside session JSON. Use read_tool_result with this handle to inspect a byte range or literal query.",
                    stored.handle, stored.stored_bytes, preview, stored.handle
                );
                output.structured = None;
                output.truncated = true;
                return;
            }
            Err(error) => {
                *output = error_output(format!("tool result storage failed: {error}"));
                return;
            }
        }
    }

    let (content, truncated) = bounded_inline_output(
        &durable,
        context.limits.max_result_bytes,
        call.name.as_str(),
    );
    output.content = content;
    output.truncated |= truncated;
    if truncated {
        output.structured = None;
    }
}

fn bounded_inline_output(content: &str, max_bytes: usize, tool_name: &str) -> (String, bool) {
    if content.len() <= max_bytes {
        return (content.to_owned(), false);
    }
    let marker = format!(
        "\n... [tool result truncated for {tool_name}: original {} bytes; cap is {max_bytes} bytes]\n",
        content.len()
    );
    if marker.len() >= max_bytes {
        return (utf8_prefix(&marker, max_bytes).to_owned(), true);
    }
    let prefix = utf8_prefix(content, max_bytes - marker.len());
    (format!("{prefix}{marker}"), true)
}

fn utf8_prefix(content: &str, max_bytes: usize) -> &str {
    let mut end = content.len().min(max_bytes);
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

fn accumulate_usage(total: &mut Usage, next: Usage) {
    total.input_tokens = sum_optional(total.input_tokens, next.input_tokens);
    total.output_tokens = sum_optional(total.output_tokens, next.output_tokens);
}

fn sum_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

struct GatewayEventBridge<'a>(&'a mut dyn AgentEventSink);

impl GatewayEventSink for GatewayEventBridge<'_> {
    fn emit(&mut self, event: GatewayEvent) {
        self.0.emit(AgentEvent::Gateway(event));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use serde_json::{Value, json};

    use super::*;
    use crate::{
        FinishReason, GatewayResponse, PermissionMode, ScopedProjectContextError,
        ScopedProjectContextProvider, StoredToolResult, Tool, ToolCall, ToolContext, ToolEffect,
        ToolError, ToolResultMatch, ToolResultPage, ToolResultStore, ToolResultStoreError,
    };

    struct ScriptedGateway {
        responses: Mutex<VecDeque<GatewayResponse>>,
        requests: Mutex<Vec<GatewayRequest>>,
    }

    struct CancellingGateway {
        cancellation: Arc<AtomicBool>,
        executions: Arc<AtomicUsize>,
    }

    struct CancelledGateway {
        cancellation: Arc<AtomicBool>,
    }

    #[derive(Default)]
    struct MemoryToolResultStore {
        stored: Mutex<Vec<String>>,
    }

    struct OneScopedDelta(AtomicUsize);

    impl ScopedProjectContextProvider for OneScopedDelta {
        fn select(
            &self,
            targets: &[std::path::PathBuf],
        ) -> Result<Option<String>, ScopedProjectContextError> {
            assert_eq!(
                targets,
                [std::path::PathBuf::from("/workspace/nested/file.rs")]
            );
            Ok((self.0.fetch_add(1, Ordering::SeqCst) == 0).then(|| "NESTED PROJECT RULE".into()))
        }

        fn fork_session(&self) -> Arc<dyn ScopedProjectContextProvider> {
            Arc::new(Self(AtomicUsize::new(0)))
        }
    }

    struct ScopedWriteTool(Arc<AtomicUsize>);

    impl Tool for ScopedWriteTool {
        fn name(&self) -> &str {
            "scoped_write"
        }

        fn description(&self) -> &str {
            "Test scoped write."
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }

        fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
            Ok(ToolEffect::Write)
        }

        fn project_context_targets(
            &self,
            _context: &ToolContext,
            _arguments: &Value,
        ) -> Result<Vec<std::path::PathBuf>, ToolError> {
            Ok(vec!["/workspace/nested/file.rs".into()])
        }

        fn execute<'a>(
            &'a self,
            _context: &'a ToolContext,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput {
                    content: "written".into(),
                    is_error: false,
                    structured: None,
                    original_bytes: 7,
                    truncated: false,
                    durable_content: None,
                })
            })
        }
    }

    impl ToolResultStore for MemoryToolResultStore {
        fn store(
            &self,
            _tool_call_id: &str,
            _tool_name: &str,
            content: &str,
        ) -> Result<StoredToolResult, ToolResultStoreError> {
            self.stored.lock().unwrap().push(content.to_owned());
            Ok(StoredToolResult {
                handle: "result-test.txt".into(),
                stored_bytes: content.len(),
            })
        }

        fn read_range(
            &self,
            _handle: &str,
            _start_byte: usize,
            _byte_count: usize,
        ) -> Result<ToolResultPage, ToolResultStoreError> {
            Err(ToolResultStoreError::NotFound)
        }

        fn search(
            &self,
            _handle: &str,
            _query: &str,
            _max_matches: usize,
        ) -> Result<Vec<ToolResultMatch>, ToolResultStoreError> {
            Err(ToolResultStoreError::NotFound)
        }
    }

    impl Gateway for CancelledGateway {
        fn complete<'a>(
            &'a self,
            _request: GatewayRequest,
            _events: &'a mut dyn GatewayEventSink,
        ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
            Box::pin(async move {
                self.cancellation.store(true, Ordering::Release);
                Err(GatewayError::Cancelled)
            })
        }
    }

    impl Gateway for CancellingGateway {
        fn complete<'a>(
            &'a self,
            _request: GatewayRequest,
            _events: &'a mut dyn GatewayEventSink,
        ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
            Box::pin(async move {
                self.executions.fetch_add(1, Ordering::SeqCst);
                self.cancellation.store(true, Ordering::Release);
                Ok(response(
                    Some("partial"),
                    vec![tool_call(ToolExecutionProvenance::FxLocal)],
                ))
            })
        }
    }

    struct AtomicCancellation(Arc<AtomicBool>);

    impl CancellationSignal for AtomicCancellation {
        fn is_cancelled(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    impl Gateway for ScriptedGateway {
        fn complete<'a>(
            &'a self,
            request: GatewayRequest,
            events: &'a mut dyn GatewayEventSink,
        ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
            Box::pin(async move {
                events.emit(GatewayEvent::ContentDelta("stream".into()));
                self.requests.lock().unwrap().push(request);
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .ok_or_else(|| GatewayError::InvalidResponse("script exhausted".into()))
            })
        }
    }

    struct CountingTool(Arc<AtomicUsize>);

    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "count"
        }

        fn description(&self) -> &str {
            "Count local executions."
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }

        fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
            Ok(ToolEffect::Write)
        }

        fn execute<'a>(
            &'a self,
            _context: &'a ToolContext,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(ToolOutput {
                    content: "counted".into(),
                    is_error: false,
                    structured: None,
                    original_bytes: 7,
                    truncated: false,
                    durable_content: None,
                })
            })
        }
    }

    struct CooperativeReadTool {
        name: &'static str,
        started: Arc<AtomicUsize>,
    }

    impl Tool for CooperativeReadTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "Read concurrently."
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }

        fn effect(&self, _: &Value) -> Result<ToolEffect, ToolError> {
            Ok(ToolEffect::Read)
        }

        fn execute<'a>(
            &'a self,
            _context: &'a ToolContext,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<ToolOutput, ToolError>> {
            let mut announced = false;
            Box::pin(std::future::poll_fn(move |task_context| {
                if !announced {
                    self.started.fetch_add(1, Ordering::SeqCst);
                    announced = true;
                }
                if self.started.load(Ordering::SeqCst) < 2 {
                    task_context.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(Ok(ToolOutput {
                    content: self.name.into(),
                    is_error: false,
                    structured: None,
                    original_bytes: self.name.len(),
                    truncated: false,
                    durable_content: None,
                }))
            }))
        }
    }

    #[derive(Default)]
    struct Events(Vec<AgentEvent>);

    impl AgentEventSink for Events {
        fn emit(&mut self, event: AgentEvent) {
            self.0.push(event);
        }
    }

    fn tool_call(provenance: ToolExecutionProvenance) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            name: "count".into(),
            arguments_json: "{}".into(),
            argument_integrity: ToolArgumentIntegrity::Valid,
            provisional_id: None,
            provider_result: (provenance == ToolExecutionProvenance::Provider)
                .then(|| "provider result".into()),
            provenance,
        }
    }

    fn named_tool_call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments_json: "{}".into(),
            argument_integrity: ToolArgumentIntegrity::Valid,
            provisional_id: None,
            provider_result: None,
            provenance: ToolExecutionProvenance::FxLocal,
        }
    }

    fn response(content: Option<&str>, tool_calls: Vec<ToolCall>) -> GatewayResponse {
        GatewayResponse {
            content: content.map(str::to_owned),
            tool_calls,
            generation_id: Some("generation".into()),
            finish_reason: Some(FinishReason::Stop),
            usage: Usage {
                input_tokens: Some(2),
                output_tokens: Some(3),
            },
            delivery_ambiguous: false,
        }
    }

    fn run_ready(
        agent: &Agent,
        context: &ToolContext,
        permissions: &mut PermissionEngine,
        approvals: &mut dyn ApprovalHandler,
        events: &mut dyn AgentEventSink,
    ) -> Result<AgentResult, AgentError> {
        let mut future = agent.run(
            AgentRequest {
                history: Vec::new(),
                prompt: "go".into(),
            },
            context,
            permissions,
            approvals,
            events,
        );
        let mut task_context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut task_context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("scripted agent unexpectedly yielded"),
        }
    }

    fn run_controlled_ready(
        agent: &Agent,
        context: &ToolContext,
        permissions: &mut PermissionEngine,
        approvals: &mut dyn ApprovalHandler,
        events: &mut dyn AgentEventSink,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<AgentResult, AgentError> {
        let mut future = agent.run_controlled(
            AgentRequest {
                history: Vec::new(),
                prompt: "go".into(),
            },
            context,
            permissions,
            approvals,
            events,
            cancellation,
        );
        let mut task_context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut task_context) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("scripted agent unexpectedly yielded"),
        }
    }

    fn run_eventually_ready(
        agent: &Agent,
        context: &ToolContext,
        permissions: &mut PermissionEngine,
        approvals: &mut dyn ApprovalHandler,
        events: &mut dyn AgentEventSink,
    ) -> Result<AgentResult, AgentError> {
        let mut future = agent.run(
            AgentRequest {
                history: Vec::new(),
                prompt: "go".into(),
            },
            context,
            permissions,
            approvals,
            events,
        );
        let mut task_context = Context::from_waker(Waker::noop());
        for _ in 0..16 {
            if let Poll::Ready(result) = future.as_mut().poll(&mut task_context) {
                return result;
            }
        }
        panic!("cooperative agent did not complete")
    }

    #[test]
    fn complete_large_tool_output_is_stored_before_model_projection() {
        let store = Arc::new(MemoryToolResultStore::default());
        let mut context = ToolContext::new(std::env::current_dir().unwrap());
        context.tool_results = Some(store.clone());
        let call = named_tool_call("call-sensitive", "mcp_remote_search");
        let complete = format!(
            "evidence\nAPI_KEY=server-private-value\n{}\nneedle",
            "你".repeat(6_000)
        );
        let mut output = ToolOutput {
            content: "old truncated projection".into(),
            is_error: false,
            structured: Some(json!({"large": true})),
            original_bytes: complete.len(),
            truncated: true,
            durable_content: Some(complete.clone()),
        };

        finalize_tool_output(&context, &call, &mut output);

        let stored = store.stored.lock().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].contains("API_KEY=[redacted]"));
        assert!(!stored[0].contains("server-private-value"));
        assert!(output.content.contains("result-test.txt"));
        assert!(output.content.contains("Use read_tool_result"));
        assert!(output.content.is_char_boundary(output.content.len()));
        assert!(output.structured.is_none());
        assert!(output.truncated);
        assert!(output.durable_content.is_none());
    }

    #[test]
    fn newly_selected_scoped_context_defers_write_until_next_generation() {
        let executions = Arc::new(AtomicUsize::new(0));
        let gateway = Arc::new(ScriptedGateway {
            responses: Mutex::new(VecDeque::from([
                response(None, vec![named_tool_call("call_1", "scoped_write")]),
                response(None, vec![named_tool_call("call_2", "scoped_write")]),
                response(Some("done"), Vec::new()),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::default();
        registry
            .register(ScopedWriteTool(executions.clone()))
            .unwrap();
        let agent = Agent::new(
            gateway.clone(),
            Arc::new(registry),
            AgentOptions::new("model"),
        );
        let mut context = ToolContext::new(std::env::current_dir().unwrap());
        context.project_context = Some(Arc::new(OneScopedDelta(AtomicUsize::new(0))));
        let mut permissions = PermissionEngine::new(PermissionMode::Yolo, Vec::new());
        let mut approvals = StaticApprovalHandler::allow_once();
        let mut events = Events::default();

        let result = run_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
        )
        .unwrap();

        assert_eq!(result.output, "done");
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        let requests = gateway.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].messages[0].role, Role::System);
        assert_eq!(
            requests[1].messages[0].content.as_deref(),
            Some("NESTED PROJECT RULE")
        );
        assert!(requests[1].messages.iter().any(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("execution deferred"))
        }));
        assert!(
            requests[2]
                .messages
                .iter()
                .any(|message| { message.content.as_deref() == Some("written") })
        );
    }

    #[test]
    fn local_tool_round_trip_preserves_message_order_and_usage() {
        let executions = Arc::new(AtomicUsize::new(0));
        let gateway = Arc::new(ScriptedGateway {
            responses: Mutex::new(VecDeque::from([
                response(None, vec![tool_call(ToolExecutionProvenance::FxLocal)]),
                response(Some("done"), Vec::new()),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::default();
        registry.register(CountingTool(executions.clone())).unwrap();
        let agent = Agent::new(
            gateway.clone(),
            Arc::new(registry),
            AgentOptions::new("model"),
        );
        let context = ToolContext::new(std::env::current_dir().unwrap());
        let mut permissions = PermissionEngine::new(PermissionMode::Auto, Vec::new());
        let mut approvals = StaticApprovalHandler::allow_once();
        let mut events = Events::default();

        let result = run_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
        )
        .unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert_eq!(result.output, "done");
        assert_eq!(result.steps, 2);
        assert_eq!(result.usage.input_tokens, Some(4));
        assert_eq!(result.messages[2].role, Role::Tool);
        assert_eq!(result.messages[2].content.as_deref(), Some("counted"));
        assert_eq!(gateway.requests.lock().unwrap().len(), 2);
        assert!(events.0.iter().any(|event| matches!(
            event,
            AgentEvent::ToolFinished {
                is_error: false,
                ..
            }
        )));
    }

    #[test]
    fn consecutive_read_tools_run_concurrently_and_commit_results_in_call_order() {
        let started = Arc::new(AtomicUsize::new(0));
        let gateway = Arc::new(ScriptedGateway {
            responses: Mutex::new(VecDeque::from([
                response(
                    None,
                    vec![
                        named_tool_call("call-a", "read_a"),
                        named_tool_call("call-b", "read_b"),
                    ],
                ),
                response(Some("done"), Vec::new()),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::default();
        registry
            .register(CooperativeReadTool {
                name: "read_a",
                started: started.clone(),
            })
            .unwrap();
        registry
            .register(CooperativeReadTool {
                name: "read_b",
                started: started.clone(),
            })
            .unwrap();
        let agent = Agent::new(gateway, Arc::new(registry), AgentOptions::new("model"));
        let context = ToolContext::new(std::env::current_dir().unwrap());
        let mut permissions = PermissionEngine::new(PermissionMode::Auto, Vec::new());
        let mut approvals = StaticApprovalHandler::deny();
        let mut events = Events::default();

        let result = run_eventually_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
        )
        .unwrap();
        assert_eq!(started.load(Ordering::SeqCst), 2);
        assert_eq!(result.messages[2].tool_call_id.as_deref(), Some("call-a"));
        assert_eq!(result.messages[2].content.as_deref(), Some("read_a"));
        assert_eq!(result.messages[3].tool_call_id.as_deref(), Some("call-b"));
        assert_eq!(result.messages[3].content.as_deref(), Some("read_b"));
        let phases = events
            .0
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolStarted { id, .. } => Some(format!("start:{id}")),
                AgentEvent::ToolFinished { id, .. } => Some(format!("finish:{id}")),
                AgentEvent::Gateway(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            phases,
            [
                "start:call-a",
                "start:call-b",
                "finish:call-a",
                "finish:call-b"
            ]
        );
    }

    #[test]
    fn provider_tool_result_is_never_dispatched_locally() {
        let executions = Arc::new(AtomicUsize::new(0));
        let gateway = Arc::new(ScriptedGateway {
            responses: Mutex::new(VecDeque::from([
                response(None, vec![tool_call(ToolExecutionProvenance::Provider)]),
                response(Some("done"), Vec::new()),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::default();
        registry.register(CountingTool(executions.clone())).unwrap();
        let agent = Agent::new(
            gateway.clone(),
            Arc::new(registry),
            AgentOptions::new("model"),
        );
        let context = ToolContext::new(std::env::current_dir().unwrap());
        let mut permissions = PermissionEngine::new(PermissionMode::Auto, Vec::new());
        let mut approvals = StaticApprovalHandler::deny();
        let mut events = Events::default();

        let result = run_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
        )
        .unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert_eq!(
            result.messages[2].content.as_deref(),
            Some("provider result")
        );
    }

    #[test]
    fn denied_review_becomes_permission_feedback_for_next_generation() {
        let executions = Arc::new(AtomicUsize::new(0));
        let gateway = Arc::new(ScriptedGateway {
            responses: Mutex::new(VecDeque::from([
                response(None, vec![tool_call(ToolExecutionProvenance::FxLocal)]),
                response(Some("understood"), Vec::new()),
            ])),
            requests: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::default();
        registry.register(CountingTool(executions.clone())).unwrap();
        let agent = Agent::new(
            gateway.clone(),
            Arc::new(registry),
            AgentOptions::new("model"),
        );
        let context = ToolContext::new(std::env::current_dir().unwrap());
        let mut permissions = PermissionEngine::new(PermissionMode::Auto, Vec::new());
        let mut approvals = StaticApprovalHandler::deny();
        let mut events = Events::default();

        let result = run_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
        )
        .unwrap();
        assert_eq!(executions.load(Ordering::SeqCst), 0);
        assert!(result.messages[2].permission_feedback);
        assert!(
            result.messages[2]
                .content
                .as_deref()
                .unwrap()
                .contains("denied")
        );
    }

    #[test]
    fn cancellation_after_gateway_prevents_local_tool_execution() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let gateway_executions = Arc::new(AtomicUsize::new(0));
        let tool_executions = Arc::new(AtomicUsize::new(0));
        let gateway = Arc::new(CancellingGateway {
            cancellation: cancellation.clone(),
            executions: gateway_executions.clone(),
        });
        let mut registry = ToolRegistry::default();
        registry
            .register(CountingTool(tool_executions.clone()))
            .unwrap();
        let agent = Agent::new(gateway, Arc::new(registry), AgentOptions::new("model"));
        let context = ToolContext::new(std::env::current_dir().unwrap());
        let mut permissions = PermissionEngine::new(PermissionMode::Auto, Vec::new());
        let mut approvals = StaticApprovalHandler::allow_once();
        let mut events = Events::default();

        let result = run_controlled_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
            Arc::new(AtomicCancellation(cancellation)),
        )
        .unwrap();

        assert_eq!(result.stop_reason, AgentStopReason::Cancelled);
        assert_eq!(result.steps, 1);
        assert_eq!(result.output, "partial");
        assert_eq!(gateway_executions.load(Ordering::SeqCst), 1);
        assert_eq!(tool_executions.load(Ordering::SeqCst), 0);
        assert!(!events.0.iter().any(|event| matches!(
            event,
            AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. }
        )));
    }

    #[test]
    fn cooperative_gateway_cancellation_is_a_normal_agent_stop() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let agent = Agent::new(
            Arc::new(CancelledGateway {
                cancellation: cancellation.clone(),
            }),
            Arc::new(ToolRegistry::default()),
            AgentOptions::new("model"),
        );
        let context = ToolContext::new(std::env::current_dir().unwrap());
        let mut permissions = PermissionEngine::new(PermissionMode::Ask, Vec::new());
        let mut approvals = StaticApprovalHandler::deny();
        let mut events = Events::default();
        let result = run_controlled_ready(
            &agent,
            &context,
            &mut permissions,
            &mut approvals,
            &mut events,
            Arc::new(AtomicCancellation(cancellation)),
        )
        .unwrap();
        assert_eq!(result.stop_reason, AgentStopReason::Cancelled);
        assert_eq!(result.steps, 1);
    }
}
