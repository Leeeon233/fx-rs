//! Thin ACP stdio adapter for [`fx_runtime::FxRuntime`].

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::{ProtocolVersion, v1 as acp};
use agent_client_protocol::{Agent as AcpAgent, ConnectionTo, Stdio};
use fx_core::{
    AgentEvent, AgentEventSink, ApprovalDecision, ApprovalError, ApprovalHandler, ApprovalRequest,
    BoxFuture, ChatMessage, GatewayEvent, Role, ToolReview,
};
use fx_mcp::{HttpServerConfig, McpConfig, StdioServerConfig};
use fx_provider::Model;
use fx_runtime::{
    ASK_MODE_ID, CODE_MODE_ID, FxRuntime, RuntimeError, RuntimeSessionConfiguration,
    RuntimeSessionSetup, RuntimeStopReason, SessionCancellation,
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Options {
    pub model: Option<String>,
    pub log_file: Option<PathBuf>,
}

pub fn parse_options(args: impl IntoIterator<Item = OsString>) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();
    while let Some(argument) = args.next() {
        let argument = argument
            .into_string()
            .map_err(|_| "ACP arguments must be valid UTF-8".to_owned())?;
        match argument.as_str() {
            "--model" => {
                let value = next_value(&mut args, "--model")?;
                if options.model.replace(value).is_some() {
                    return Err("--model may only be specified once".into());
                }
            }
            "--log-file" => {
                let value = next_value(&mut args, "--log-file")?;
                if options.log_file.replace(PathBuf::from(value)).is_some() {
                    return Err("--log-file may only be specified once".into());
                }
            }
            "--help" | "-h" => return Err("help requested".into()),
            flag => return Err(format!("unsupported ACP option: {flag}")),
        }
    }
    Ok(options)
}

pub fn run_cli(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) if error == "help requested" => {
            print!(
                "fxrs acp\n\nStart an ACP server over stdio\n\nUsage:\n  fxrs acp [--model <id>] [--log-file <path>]\n"
            );
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("fxrs acp: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("fxrs acp: could not initialize runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fxrs acp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn next_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, String> {
    let value = args
        .next()
        .ok_or_else(|| format!("{flag} requires a value"))?
        .into_string()
        .map_err(|_| format!("{flag} requires valid UTF-8"))?;
    if value.trim().is_empty() {
        return Err(format!("{flag} requires a nonempty value"));
    }
    Ok(value)
}

pub async fn run(options: Options) -> agent_client_protocol::Result<()> {
    let state = Arc::new(AcpState::new(options.model)?);
    let transport = match options.log_file {
        Some(path) => {
            let file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&path)
                .map_err(|error| internal(format!("could not open {}: {error}", path.display())))?;
            let file = Arc::new(Mutex::new(file));
            Stdio::new().with_debug(move |line, direction| {
                if let Ok(mut file) = file.lock() {
                    let _ = writeln!(file, "{direction:?} {line}");
                }
            })
        }
        None => Stdio::new(),
    };

    let initialize_state = state.clone();
    let authenticate_state = state.clone();
    let logout_state = state.clone();
    let new_state = state.clone();
    let load_state = state.clone();
    let resume_state = state.clone();
    let list_state = state.clone();
    let close_state = state.clone();
    let set_mode_state = state.clone();
    let set_config_state = state.clone();
    let prompt_state = state.clone();
    let cancel_state = state.clone();

    let result = AcpAgent
        .builder()
        .name("fxrs")
        .on_receive_request(
            async move |request: acp::InitializeRequest, responder, _connection| {
                initialize_state.claim_initialization()?;
                let version = if request.protocol_version == ProtocolVersion::V1 {
                    request.protocol_version
                } else {
                    ProtocolVersion::V1
                };
                let session_capabilities = acp::SessionCapabilities::new()
                    .list(acp::SessionListCapabilities::new())
                    .resume(acp::SessionResumeCapabilities::new())
                    .close(acp::SessionCloseCapabilities::new());
                let capabilities = acp::AgentCapabilities::new()
                    .auth(acp::AgentAuthCapabilities::new().logout(acp::LogoutCapabilities::new()))
                    .load_session(true)
                    .prompt_capabilities(acp::PromptCapabilities::new().embedded_context(true))
                    .mcp_capabilities(acp::McpCapabilities::new().http(true))
                    .session_capabilities(session_capabilities);
                responder.respond(
                    acp::InitializeResponse::new(version)
                        .agent_capabilities(capabilities)
                        .auth_methods(initialize_state.auth_methods())
                        .agent_info(
                            acp::Implementation::new("fxrs", env!("CARGO_PKG_VERSION"))
                                .title("fxrs"),
                        ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::AuthenticateRequest, responder, connection| {
                let refreshed = authenticate_state
                    .runtime
                    .authenticate(request.method_id.0.as_ref())
                    .await
                    .map_err(map_runtime_error)?;
                responder.respond(acp::AuthenticateResponse::new())?;
                if refreshed {
                    authenticate_state
                        .publish_session_config_updates(&connection)
                        .await?;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: acp::LogoutRequest, responder, _connection| {
                logout_state
                    .runtime
                    .logout()
                    .await
                    .map_err(map_runtime_error)?;
                responder.respond(acp::LogoutResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::NewSessionRequest, responder, connection| {
                let setup = new_state
                    .runtime
                    .create_session(
                        request.cwd,
                        request.additional_directories,
                        project_mcp_servers(request.mcp_servers)?,
                    )
                    .await
                    .map_err(map_runtime_error)?;
                let setup = project_session_setup(setup);
                responder.respond(
                    acp::NewSessionResponse::new(setup.session_id)
                        .modes(setup.modes)
                        .config_options(setup.config_options),
                )?;
                new_state.start_background_catalog_refresh(connection);
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::LoadSessionRequest, responder, connection| {
                let id = request.session_id.clone();
                let loaded = load_state
                    .runtime
                    .load_session(
                        request.session_id.0.to_string(),
                        request.cwd,
                        request.additional_directories,
                        project_mcp_servers(request.mcp_servers)?,
                        true,
                    )
                    .await
                    .map_err(map_runtime_error)?;
                replay_history(&connection, &id, &loaded.history)?;
                let setup = project_session_setup(loaded.setup);
                responder.respond(
                    acp::LoadSessionResponse::new()
                        .modes(setup.modes)
                        .config_options(setup.config_options),
                )?;
                load_state.start_background_catalog_refresh(connection);
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::ResumeSessionRequest, responder, connection| {
                let loaded = resume_state
                    .runtime
                    .load_session(
                        request.session_id.0.to_string(),
                        request.cwd,
                        request.additional_directories,
                        project_mcp_servers(request.mcp_servers)?,
                        false,
                    )
                    .await
                    .map_err(map_runtime_error)?;
                let setup = project_session_setup(loaded.setup);
                responder.respond(
                    acp::ResumeSessionResponse::new()
                        .modes(setup.modes)
                        .config_options(setup.config_options),
                )?;
                resume_state.start_background_catalog_refresh(connection);
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::ListSessionsRequest, responder, _connection| {
                let listed = list_state
                    .runtime
                    .list_sessions(request.cwd, request.cursor)
                    .await
                    .map_err(map_runtime_error)?;
                let sessions = listed
                    .sessions
                    .into_iter()
                    .map(|session| {
                        acp::SessionInfo::new(session.session_id, session.cwd)
                            .title(session.title)
                            .updated_at(session.updated_at)
                    })
                    .collect();
                responder.respond(
                    acp::ListSessionsResponse::new(sessions).next_cursor(listed.next_cursor),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::CloseSessionRequest, responder, _connection| {
                close_state
                    .runtime
                    .close_session(request.session_id.0.as_ref())
                    .await
                    .map_err(map_runtime_error)?;
                responder.respond(acp::CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::SetSessionModeRequest, responder, _connection| {
                set_mode_state
                    .runtime
                    .set_session_mode(request.session_id.0.as_ref(), request.mode_id.0.as_ref())
                    .map_err(map_runtime_error)?;
                responder.respond(acp::SetSessionModeResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::SetSessionConfigOptionRequest, responder, _connection| {
                let value = request
                    .value
                    .as_value_id()
                    .ok_or_else(|| invalid_params("configuration must be a select value"))?;
                let configuration = set_config_state
                    .runtime
                    .set_session_config_option(
                        request.session_id.0.as_ref(),
                        request.config_id.0.as_ref(),
                        value.0.as_ref(),
                    )
                    .await
                    .map_err(map_runtime_error)?;
                responder.respond(acp::SetSessionConfigOptionResponse::new(
                    project_config_options(&configuration),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: acp::PromptRequest, responder, connection| {
                let prompt_state = prompt_state.clone();
                let task_connection = connection.clone();
                connection.spawn(async move {
                    let response = prompt_state.prompt(request, task_connection).await;
                    responder.respond_with_result(response)
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: acp::CancelNotification, _connection| {
                cancel_state
                    .runtime
                    .cancel_session(notification.session_id.0.as_ref());
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    state.runtime.shutdown().await;
    result
}

struct AcpState {
    runtime: Arc<FxRuntime>,
    initialized: AtomicBool,
}

impl AcpState {
    fn new(model_override: Option<String>) -> agent_client_protocol::Result<Self> {
        Ok(Self {
            runtime: Arc::new(FxRuntime::from_process(model_override).map_err(map_runtime_error)?),
            initialized: AtomicBool::new(false),
        })
    }

    fn claim_initialization(&self) -> agent_client_protocol::Result<()> {
        self.initialized
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| invalid_request("ACP connection is already initialized"))
    }

    fn auth_methods(&self) -> Vec<acp::AuthMethod> {
        self.runtime
            .auth_methods()
            .into_iter()
            .map(|method| {
                acp::AuthMethod::Agent(
                    acp::AuthMethodAgent::new(method.id, method.name)
                        .description(method.description),
                )
            })
            .collect()
    }

    fn start_background_catalog_refresh(
        self: &Arc<Self>,
        connection: ConnectionTo<agent_client_protocol::Client>,
    ) {
        let state = self.clone();
        tokio::spawn(async move {
            match state.runtime.refresh_models_once().await {
                Ok(true) => {
                    if let Err(error) = state.publish_session_config_updates(&connection).await {
                        eprintln!("fxrs model catalog update: {error}");
                    }
                }
                Ok(false) => {}
                Err(error) => eprintln!("fxrs model catalog worker failed: {error}"),
            }
        });
    }

    async fn publish_session_config_updates(
        &self,
        connection: &ConnectionTo<agent_client_protocol::Client>,
    ) -> agent_client_protocol::Result<()> {
        let configurations = self
            .runtime
            .session_configurations()
            .await
            .map_err(map_runtime_error)?;
        for configuration in configurations {
            let update = acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                project_config_options(&configuration),
            ));
            connection.send_notification(acp::SessionNotification::new(
                acp::SessionId::new(configuration.session_id),
                update,
            ))?;
        }
        Ok(())
    }

    async fn prompt(
        &self,
        request: acp::PromptRequest,
        connection: ConnectionTo<agent_client_protocol::Client>,
    ) -> agent_client_protocol::Result<acp::PromptResponse> {
        let id = request.session_id.0.to_string();
        let prompt = project_prompt(request.prompt)?;
        let cancellation = self.runtime.cancellation(&id).map_err(map_runtime_error)?;
        let mut events = AcpEvents::new(connection.clone(), request.session_id.clone());
        let mut approvals = AcpApproval::new(connection, request.session_id, cancellation);
        let result = self
            .runtime
            .prompt(&id, prompt, &mut approvals, &mut events)
            .await;
        let notification_result = events.finish();
        let reason = result.map_err(map_runtime_error)?;
        notification_result?;
        Ok(acp::PromptResponse::new(match reason {
            RuntimeStopReason::Complete => acp::StopReason::EndTurn,
            RuntimeStopReason::StepLimit => acp::StopReason::MaxTurnRequests,
            RuntimeStopReason::Cancelled => acp::StopReason::Cancelled,
        }))
    }
}

struct SessionSetup {
    session_id: acp::SessionId,
    modes: acp::SessionModeState,
    config_options: Vec<acp::SessionConfigOption>,
}

fn project_session_setup(setup: RuntimeSessionSetup) -> SessionSetup {
    SessionSetup {
        session_id: acp::SessionId::new(setup.session_id),
        modes: session_modes(&setup.mode),
        config_options: session_config_options(&setup.model, &setup.mode, &setup.models),
    }
}

fn project_config_options(
    configuration: &RuntimeSessionConfiguration,
) -> Vec<acp::SessionConfigOption> {
    session_config_options(
        &configuration.model,
        &configuration.mode,
        &configuration.models,
    )
}

fn session_modes(current: &str) -> acp::SessionModeState {
    acp::SessionModeState::new(
        current.to_owned(),
        vec![
            acp::SessionMode::new(CODE_MODE_ID, "Code")
                .description("Write and modify code with full tool access"),
            acp::SessionMode::new(ASK_MODE_ID, "Ask")
                .description("Request permission before making any changes"),
        ],
    )
}

fn session_config_options(
    model: &str,
    mode: &str,
    models: &[Model],
) -> Vec<acp::SessionConfigOption> {
    let model_option = acp::SessionConfigOption::select(
        "model",
        "Model",
        model.to_owned(),
        models
            .iter()
            .map(|candidate| {
                acp::SessionConfigSelectOption::new(candidate.route(), candidate.name.clone())
                    .description(format!(
                        "{} · {} token context{}",
                        candidate.provider_id,
                        candidate.context_window,
                        if candidate.reasoning {
                            " · reasoning"
                        } else {
                            ""
                        }
                    ))
            })
            .collect::<Vec<_>>(),
    )
    .category(acp::SessionConfigOptionCategory::Model);
    let mode_option = acp::SessionConfigOption::select(
        "mode",
        "Session Mode",
        mode.to_owned(),
        vec![
            acp::SessionConfigSelectOption::new(CODE_MODE_ID, "Code")
                .description("Write and modify code with full tool access"),
            acp::SessionConfigSelectOption::new(ASK_MODE_ID, "Ask")
                .description("Request permission before making any changes"),
        ],
    )
    .description("Controls how the agent requests permission")
    .category(acp::SessionConfigOptionCategory::Mode);
    vec![model_option, mode_option]
}

struct AcpEvents {
    connection: ConnectionTo<agent_client_protocol::Client>,
    session_id: acp::SessionId,
    announced_tools: HashSet<String>,
    failure: Option<String>,
}

impl AcpEvents {
    fn new(
        connection: ConnectionTo<agent_client_protocol::Client>,
        session_id: acp::SessionId,
    ) -> Self {
        Self {
            connection,
            session_id,
            announced_tools: HashSet::new(),
            failure: None,
        }
    }

    fn notify(&mut self, update: acp::SessionUpdate) {
        if self.failure.is_some() {
            return;
        }
        if let Err(error) = self
            .connection
            .send_notification(acp::SessionNotification::new(
                self.session_id.clone(),
                update,
            ))
        {
            self.failure = Some(error.to_string());
        }
    }

    fn finish(self) -> agent_client_protocol::Result<()> {
        match self.failure {
            Some(error) => Err(internal(format!("could not publish ACP update: {error}"))),
            None => Ok(()),
        }
    }
}

impl AgentEventSink for AcpEvents {
    fn emit(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::Gateway(GatewayEvent::ContentDelta(text)) => {
                self.notify(acp::SessionUpdate::AgentMessageChunk(
                    acp::ContentChunk::new(text.into()),
                ));
            }
            AgentEvent::Gateway(GatewayEvent::ReasoningDelta(text)) => self.notify(
                acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(text.into())),
            ),
            AgentEvent::Gateway(GatewayEvent::ToolStarted { id, name }) => {
                if self.announced_tools.insert(id.clone()) {
                    self.notify(acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(id, name.clone())
                            .kind(tool_kind(&name))
                            .status(acp::ToolCallStatus::Pending),
                    ));
                }
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments_json,
            } => {
                if self.announced_tools.insert(id.clone()) {
                    self.notify(acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(id.clone(), name.clone())
                            .kind(tool_kind(&name))
                            .status(acp::ToolCallStatus::Pending),
                    ));
                }
                let input = serde_json::from_str(&arguments_json).ok();
                self.notify(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        id,
                        acp::ToolCallUpdateFields::new()
                            .status(acp::ToolCallStatus::InProgress)
                            .raw_input(input),
                    ),
                ));
            }
            AgentEvent::ToolFinished {
                id,
                is_error,
                output,
                ..
            } => {
                let status = if is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                };
                let raw_output = output
                    .structured
                    .clone()
                    .or_else(|| Some(serde_json::Value::String(output.content.clone())));
                self.notify(acp::SessionUpdate::ToolCallUpdate(
                    acp::ToolCallUpdate::new(
                        id,
                        acp::ToolCallUpdateFields::new()
                            .status(status)
                            .content(vec![output.content.into()])
                            .raw_output(raw_output),
                    ),
                ));
            }
        }
    }
}

struct AcpApproval {
    connection: ConnectionTo<agent_client_protocol::Client>,
    session_id: acp::SessionId,
    cancellation: Arc<SessionCancellation>,
}

impl AcpApproval {
    fn new(
        connection: ConnectionTo<agent_client_protocol::Client>,
        session_id: acp::SessionId,
        cancellation: Arc<SessionCancellation>,
    ) -> Self {
        Self {
            connection,
            session_id,
            cancellation,
        }
    }
}

impl ApprovalHandler for AcpApproval {
    fn review<'a>(
        &'a mut self,
        request: ApprovalRequest,
    ) -> BoxFuture<'a, Result<ApprovalDecision, ApprovalError>> {
        Box::pin(async move {
            let title = approval_title(&request);
            let tool_call = acp::ToolCallUpdate::new(
                request.tool_call_id,
                acp::ToolCallUpdateFields::new()
                    .title(title)
                    .kind(tool_kind(&request.tool_name))
                    .status(acp::ToolCallStatus::Pending),
            );
            let options = vec![
                acp::PermissionOption::new(
                    "allow_once",
                    "Allow once",
                    acp::PermissionOptionKind::AllowOnce,
                ),
                acp::PermissionOption::new(
                    "allow_always",
                    "Allow for this session",
                    acp::PermissionOptionKind::AllowAlways,
                ),
                acp::PermissionOption::new(
                    "reject_once",
                    "Reject",
                    acp::PermissionOptionKind::RejectOnce,
                ),
            ];
            let request = self
                .connection
                .send_request(acp::RequestPermissionRequest::new(
                    self.session_id.clone(),
                    tool_call,
                    options,
                ));
            let response = tokio::select! {
                response = request.block_task() => response
                    .map_err(|error| ApprovalError::Unavailable(error.to_string()))?,
                () = self.cancellation.cancelled() => return Ok(ApprovalDecision::Deny),
            };
            match response.outcome {
                acp::RequestPermissionOutcome::Selected(selected) => {
                    match selected.option_id.0.as_ref() {
                        "allow_once" => Ok(ApprovalDecision::AllowOnce),
                        "allow_always" => Ok(ApprovalDecision::AllowForSession),
                        _ => Ok(ApprovalDecision::Deny),
                    }
                }
                acp::RequestPermissionOutcome::Cancelled => Ok(ApprovalDecision::Deny),
                _ => Ok(ApprovalDecision::Deny),
            }
        })
    }
}

fn approval_title(request: &ApprovalRequest) -> String {
    let targets = request
        .permission_requests
        .iter()
        .map(|permission| permission.target.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    match &request.review {
        Some(ToolReview::FileChange(change)) => {
            format!("{} {}", request.tool_name, change.path.display())
        }
        Some(ToolReview::Command(command)) => {
            format!("{} {}", request.tool_name, command.command)
        }
        None if targets.is_empty() => request.tool_name.clone(),
        None => format!("{} {targets}", request.tool_name),
    }
}

pub fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read_file" | "list_files" | "glob_files" | "file_info" => acp::ToolKind::Read,
        "write_file" | "edit_file" | "create_folder" => acp::ToolKind::Edit,
        "delete_file" => acp::ToolKind::Delete,
        "copy_file" | "rename_file" => acp::ToolKind::Move,
        "grep_files" | "semantic_search" | "web_search" => acp::ToolKind::Search,
        "run_command" | "terminal" => acp::ToolKind::Execute,
        "web_fetch" => acp::ToolKind::Fetch,
        _ => acp::ToolKind::Other,
    }
}

pub fn project_prompt(blocks: Vec<acp::ContentBlock>) -> agent_client_protocol::Result<String> {
    let mut projected = String::new();
    for block in blocks {
        if !projected.is_empty() {
            projected.push_str("\n\n");
        }
        match block {
            acp::ContentBlock::Text(text) => projected.push_str(&text.text),
            acp::ContentBlock::ResourceLink(link) => {
                projected.push_str("Referenced resource: ");
                projected.push_str(&link.name);
                projected.push_str(" (");
                projected.push_str(&link.uri);
                projected.push(')');
                if let Some(description) = link.description {
                    projected.push('\n');
                    projected.push_str(&description);
                }
            }
            acp::ContentBlock::Resource(resource) => match resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(text) => {
                    projected.push_str("Embedded resource: ");
                    projected.push_str(&text.uri);
                    projected.push_str("\n\n");
                    projected.push_str(&text.text);
                }
                acp::EmbeddedResourceResource::BlobResourceContents(_) => {
                    return Err(invalid_params(
                        "binary embedded resources are not supported",
                    ));
                }
                _ => return Err(invalid_params("unsupported embedded resource")),
            },
            acp::ContentBlock::Image(_) => {
                return Err(invalid_params("image prompts are not supported"));
            }
            acp::ContentBlock::Audio(_) => {
                return Err(invalid_params("audio prompts are not supported"));
            }
            _ => return Err(invalid_params("unsupported prompt content block")),
        }
    }
    if projected.trim().is_empty() {
        return Err(invalid_params("prompt must not be empty"));
    }
    Ok(projected)
}

pub fn project_mcp_servers(
    servers: Vec<acp::McpServer>,
) -> agent_client_protocol::Result<McpConfig> {
    let mut names = HashSet::new();
    let mut stdio_servers = Vec::with_capacity(servers.len());
    let mut http_servers = Vec::new();
    for server in servers {
        let name = match &server {
            acp::McpServer::Stdio(server) => &server.name,
            acp::McpServer::Http(server) => &server.name,
            acp::McpServer::Sse(server) => &server.name,
            _ => return Err(invalid_params("unsupported ACP MCP transport")),
        };
        if name.trim().is_empty() || !names.insert(name.clone()) {
            return Err(invalid_params(
                "ACP MCP server names must be unique and nonempty",
            ));
        }
        match server {
            acp::McpServer::Stdio(server) => {
                if !server.command.is_absolute() {
                    return Err(invalid_params(format!(
                        "ACP MCP command for `{}` must be absolute",
                        server.name
                    )));
                }
                let command = server
                    .command
                    .into_os_string()
                    .into_string()
                    .map_err(|_| invalid_params("ACP MCP commands must be valid UTF-8"))?;
                let mut environment = BTreeMap::new();
                for variable in server.env {
                    if variable.name.is_empty()
                        || environment.insert(variable.name, variable.value).is_some()
                    {
                        return Err(invalid_params(
                            "ACP MCP environment names must be unique and nonempty",
                        ));
                    }
                }
                stdio_servers.push(StdioServerConfig {
                    name: server.name,
                    command,
                    args: server.args,
                    environment,
                    enabled: true,
                    required: true,
                    startup_timeout: Duration::from_secs(10),
                    operation_timeout: Duration::from_secs(60),
                });
            }
            acp::McpServer::Http(server) => {
                let mut headers = BTreeMap::new();
                for header in server.headers {
                    if header.name.is_empty() || headers.insert(header.name, header.value).is_some()
                    {
                        return Err(invalid_params(
                            "ACP MCP HTTP header names must be unique and nonempty",
                        ));
                    }
                }
                let projected = HttpServerConfig {
                    name: server.name,
                    url: server.url,
                    headers,
                    enabled: true,
                    required: true,
                    startup_timeout: Duration::from_secs(10),
                    operation_timeout: Duration::from_secs(60),
                };
                projected
                    .validate()
                    .map_err(|error| invalid_params(error.to_string()))?;
                http_servers.push(projected);
            }
            acp::McpServer::Sse(_) => {
                return Err(invalid_params(
                    "legacy SSE MCP transport is not enabled in this build",
                ));
            }
            _ => return Err(invalid_params("unsupported ACP MCP transport")),
        }
    }
    Ok(McpConfig {
        servers: stdio_servers,
        http_servers,
        unsupported_servers: Vec::new(),
    })
}

fn replay_history(
    connection: &ConnectionTo<agent_client_protocol::Client>,
    session_id: &acp::SessionId,
    history: &[ChatMessage],
) -> agent_client_protocol::Result<()> {
    for message in history {
        let Some(content) = message
            .content
            .as_deref()
            .filter(|content| !content.is_empty())
        else {
            continue;
        };
        let update = match message.role {
            Role::User => {
                acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(content.into()))
            }
            Role::Assistant => {
                acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(content.into()))
            }
            Role::System | Role::Tool => continue,
        };
        connection.send_notification(acp::SessionNotification::new(session_id.clone(), update))?;
    }
    Ok(())
}

fn map_runtime_error(error: RuntimeError) -> agent_client_protocol::Error {
    match error {
        RuntimeError::InvalidArgument(message) | RuntimeError::NotFound(message) => {
            invalid_params(message)
        }
        RuntimeError::Conflict(message) => invalid_request(message),
        RuntimeError::Internal(message) => internal(message),
    }
}

fn invalid_params(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(message.into())
}

fn invalid_request(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_request().data(message.into())
}

fn internal(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(message.into())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn parses_options_without_positional_input() {
        let options = parse_options([
            OsString::from("--model"),
            OsString::from("provider/model"),
            OsString::from("--log-file"),
            OsString::from("trace.log"),
        ])
        .unwrap();
        assert_eq!(options.model.as_deref(), Some("provider/model"));
        assert_eq!(options.log_file.as_deref(), Some(Path::new("trace.log")));
        assert!(parse_options([OsString::from("prompt")]).is_err());
    }

    #[test]
    fn initialization_is_claimed_once() {
        let state = AcpState::new(None).unwrap();
        assert!(state.claim_initialization().is_ok());
        assert!(state.claim_initialization().is_err());
    }

    #[test]
    fn projects_text_links_and_embedded_text() {
        let prompt = project_prompt(vec![
            acp::ContentBlock::from("Explain"),
            acp::ContentBlock::ResourceLink(acp::ResourceLink::new(
                "guide",
                "file:///repo/guide.md",
            )),
            acp::ContentBlock::Resource(acp::EmbeddedResource::new(
                acp::EmbeddedResourceResource::TextResourceContents(
                    acp::TextResourceContents::new("body", "file:///repo/body.md"),
                ),
            )),
        ])
        .unwrap();
        assert!(prompt.contains("guide (file:///repo/guide.md)"));
        assert!(prompt.contains("Embedded resource: file:///repo/body.md\n\nbody"));
    }

    #[test]
    fn projects_only_absolute_stdio_mcp_servers() {
        let config = project_mcp_servers(vec![acp::McpServer::Stdio(
            acp::McpServerStdio::new("demo", "/usr/bin/demo")
                .env(vec![acp::EnvVariable::new("TOKEN", "secret")]),
        )])
        .unwrap();
        assert_eq!(config.servers[0].command, "/usr/bin/demo");
        assert_eq!(config.servers[0].environment["TOKEN"], "secret");
        assert!(
            project_mcp_servers(vec![acp::McpServer::Stdio(acp::McpServerStdio::new(
                "demo", "relative"
            ))])
            .is_err()
        );
    }

    #[test]
    fn maps_tool_categories() {
        assert_eq!(tool_kind("read_file"), acp::ToolKind::Read);
        assert_eq!(tool_kind("write_file"), acp::ToolKind::Edit);
        assert_eq!(tool_kind("rename_file"), acp::ToolKind::Move);
        assert_eq!(tool_kind("unknown"), acp::ToolKind::Other);
    }
}
