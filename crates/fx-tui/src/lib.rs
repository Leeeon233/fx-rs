//! Responsive terminal frontend that communicates with coding agents through ACP.

mod app;
mod render;
mod terminal;
mod theme;

use std::ffi::OsString;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, CancelNotification, CloseSessionRequest, ContentBlock, Implementation,
    InitializeRequest, LoadSessionRequest, NewSessionRequest, PermissionOptionId, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionConfigOption, SessionConfigOptionValue, SessionNotification,
    SetSessionConfigOptionRequest, SetSessionModeRequest, TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use app::{Action, App, PendingPermission};
use crossterm::event::{Event, EventStream};
use futures_util::StreamExt;
use terminal::TerminalSession;
use theme::Theme;
use tokio::sync::{mpsc, oneshot};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "fx-tui — ACP-native terminal interface for fxrs

Usage:
  fx-tui [OPTIONS]

Options:
  --cwd <PATH>       Workspace directory (defaults to the current directory)
  --session <ID>     Resume an existing ACP session
  --acp-exe <PATH>   ACP agent executable (defaults to sibling fx-acp)
  -h, --help         Show this help
  -V, --version      Show version

Environment:
  FX_ACP_EXE         Override the fx-acp executable path
  NO_COLOR           Disable the true-color theme
";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("ACP error: {0}")]
    Acp(String),
}

#[derive(Debug)]
struct Options {
    cwd: PathBuf,
    session: Option<String>,
    acp_exe: PathBuf,
}

enum RuntimeEvent {
    Session(SessionNotification),
    Permission {
        request: RequestPermissionRequest,
        responder: oneshot::Sender<Option<PermissionOptionId>>,
    },
    PromptFinished(Result<String, String>),
    OperationFinished {
        label: &'static str,
        result: Result<(), String>,
    },
    ConfigApplied(Result<Vec<SessionConfigOption>, String>),
    ModeApplied {
        mode: String,
        result: Result<(), String>,
    },
    SessionReady {
        id: String,
        modes: Option<agent_client_protocol::schema::v1::SessionModeState>,
        configs: Option<Vec<SessionConfigOption>>,
    },
    TransportClosed,
}

/// Parse CLI arguments and run the TUI on a compact current-thread Tokio runtime.
pub fn run_cli<I>(args: I) -> Result<(), Error>
where
    I: IntoIterator<Item = OsString>,
{
    let Some(options) = parse_options(args)? else {
        return Ok(());
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(options))
}

fn parse_options<I>(args: I) -> Result<Option<Options>, Error>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let mut cwd = None;
    let mut session = None;
    let mut acp_exe = None;
    while let Some(argument) = args.next() {
        let argument = argument
            .into_string()
            .map_err(|_| Error::Usage("arguments must be valid UTF-8".into()))?;
        match argument.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("fx-tui {VERSION}");
                return Ok(None);
            }
            "--cwd" => cwd = Some(next_path(&mut args, "--cwd")?),
            "--session" => session = Some(next_string(&mut args, "--session")?),
            "--acp-exe" => acp_exe = Some(next_path(&mut args, "--acp-exe")?),
            _ => {
                return Err(Error::Usage(format!(
                    "unknown argument `{argument}`\n\n{HELP}"
                )));
            }
        }
    }
    let cwd = match cwd {
        Some(cwd) if cwd.is_absolute() => cwd,
        Some(cwd) => std::env::current_dir()?.join(cwd),
        None => std::env::current_dir()?,
    };
    let acp_exe = acp_exe
        .or_else(|| std::env::var_os("FX_ACP_EXE").map(PathBuf::from))
        .unwrap_or_else(resolve_sibling_acp);
    Ok(Some(Options {
        cwd,
        session,
        acp_exe,
    }))
}

fn next_path<I>(args: &mut I, option: &str) -> Result<PathBuf, Error>
where
    I: Iterator<Item = OsString>,
{
    Ok(PathBuf::from(next_string(args, option)?))
}

fn next_string<I>(args: &mut I, option: &str) -> Result<String, Error>
where
    I: Iterator<Item = OsString>,
{
    args.next()
        .ok_or_else(|| Error::Usage(format!("missing value for `{option}`")))?
        .into_string()
        .map_err(|_| Error::Usage(format!("value for `{option}` must be valid UTF-8")))
}

fn resolve_sibling_acp() -> PathBuf {
    let executable = format!("fx-acp{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|current| current.parent().map(|parent| parent.join(&executable)))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| PathBuf::from(executable))
}

async fn run(options: Options) -> Result<(), Error> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::Usage(
            "an interactive terminal is required; use `fxrs acp` for stdio protocol access".into(),
        ));
    }
    if !options.cwd.is_dir() {
        return Err(Error::Usage(format!(
            "workspace `{}` is not a directory",
            options.cwd.display()
        )));
    }
    let agent = AcpAgent::new(AcpAgentConfig::new(options.acp_exe));
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let notification_tx = event_tx.clone();
    let permission_tx = event_tx.clone();
    let close_tx = event_tx.clone();
    let cwd = options.cwd;
    let resume = options.session;

    Client
        .builder()
        .name("fxrs-tui")
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                let _ = notification_tx.send(RuntimeEvent::Session(notification));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                let (choice_tx, choice_rx) = oneshot::channel();
                if permission_tx
                    .send(RuntimeEvent::Permission {
                        request,
                        responder: choice_tx,
                    })
                    .is_err()
                {
                    return responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    ));
                }
                let outcome = match choice_rx.await.ok().flatten() {
                    Some(option_id) => RequestPermissionOutcome::Selected(
                        SelectedPermissionOutcome::new(option_id),
                    ),
                    None => RequestPermissionOutcome::Cancelled,
                };
                responder.respond(RequestPermissionResponse::new(outcome))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_close(async move |_connection| {
            let _ = close_tx.send(RuntimeEvent::TransportClosed);
            Ok(())
        })
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            run_connected(connection, cwd, resume, event_tx, event_rx)
                .await
                .map_err(|error| {
                    agent_client_protocol::Error::internal_error().data(error.to_string())
                })
        })
        .await
        .map_err(|error| Error::Acp(error.to_string()))
}

async fn run_connected(
    connection: ConnectionTo<Agent>,
    cwd: PathBuf,
    resume: Option<String>,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    mut event_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
) -> Result<(), Error> {
    let theme = Theme::detect();
    terminal::install_panic_hook();
    let mut terminal = TerminalSession::enter()?;
    let mut app = App::new(cwd.clone());
    terminal
        .terminal_mut()
        .draw(|frame| render::draw(frame, &mut app, theme))?;

    let initialize = InitializeRequest::new(ProtocolVersion::V1)
        .client_info(Implementation::new("fxrs-tui", VERSION).title("fxrs TUI"));
    let initialized = connection
        .send_request(initialize)
        .block_task()
        .await
        .map_err(|error| Error::Acp(error.to_string()))?;
    let agent_name = initialized
        .agent_info
        .as_ref()
        .map(|info| info.title.as_deref().unwrap_or(&info.name).to_owned())
        .unwrap_or_else(|| "fxrs".into());
    app.set_initialized(agent_name, &initialized.auth_methods);

    if let Some(session_id) = resume {
        let response = connection
            .send_request(LoadSessionRequest::new(session_id.clone(), cwd.clone()))
            .block_task()
            .await
            .map_err(|error| Error::Acp(error.to_string()))?;
        app.set_session(session_id, response.modes, response.config_options);
    } else {
        let response = connection
            .send_request(NewSessionRequest::new(cwd.clone()))
            .block_task()
            .await
            .map_err(|error| Error::Acp(error.to_string()))?;
        app.set_session(
            response.session_id.0.to_string(),
            response.modes,
            response.config_options,
        );
    }

    let mut input = EventStream::new();
    let mut redraw = tokio::time::interval(Duration::from_millis(16));
    redraw.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut animation = tokio::time::interval(Duration::from_millis(90));
    animation.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    while !app.quit {
        tokio::select! {
            terminal_event = input.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        if let Some(action) = app.handle_key(key) {
                            dispatch(action, &mut app, &connection, &event_tx, &cwd);
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        if let Some(action) = app.handle_mouse(mouse) {
                            dispatch(action, &mut app, &connection, &event_tx, &cwd);
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => app.dirty = true,
                    Some(Ok(Event::Paste(text))) => {
                        app.composer.insert_str(text);
                        app.dirty = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(Error::Io(error)),
                    None => break,
                }
            }
            runtime_event = event_rx.recv() => {
                let Some(runtime_event) = runtime_event else {
                    break;
                };
                if let Some(action) = handle_runtime_event(&mut app, runtime_event) {
                    if let Action::Submit(prompt) = &action {
                        app.mark_prompt_started(prompt);
                    }
                    dispatch(action, &mut app, &connection, &event_tx, &cwd);
                }
            }
            _ = redraw.tick(), if app.dirty => {
                terminal
                    .terminal_mut()
                    .draw(|frame| render::draw(frame, &mut app, theme))?;
                app.dirty = false;
            }
            _ = animation.tick(), if app.is_animating() => app.advance_spinner(),
        }
    }

    Ok(())
}

fn dispatch(
    action: Action,
    app: &mut App,
    connection: &ConnectionTo<Agent>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    cwd: &Path,
) {
    match action {
        Action::Submit(prompt) => {
            let connection = connection.clone();
            let session_id = app.session_id.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = connection
                    .send_request(PromptRequest::new(
                        session_id,
                        vec![ContentBlock::Text(TextContent::new(prompt))],
                    ))
                    .block_task()
                    .await
                    .map(|response| format!("{:?}", response.stop_reason))
                    .map_err(|error| error.to_string());
                let _ = tx.send(RuntimeEvent::PromptFinished(result));
            });
        }
        Action::Cancel => {
            if let Err(error) =
                connection.send_notification(CancelNotification::new(app.session_id.clone()))
            {
                app.on_operation_finished("Cancel", Err(error.to_string()));
            }
        }
        Action::Authenticate(method) => {
            app.mark_working("Opening authentication…");
            let connection = connection.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = connection
                    .send_request(AuthenticateRequest::new(method))
                    .block_task()
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = tx.send(RuntimeEvent::OperationFinished {
                    label: "Authentication",
                    result,
                });
            });
        }
        Action::NewSession => {
            app.begin_session_switch("Creating session…");
            let connection = connection.clone();
            let previous_session = app.session_id.clone();
            let cwd = cwd.to_path_buf();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = connection
                    .send_request(NewSessionRequest::new(cwd))
                    .block_task()
                    .await;
                match result {
                    Ok(response) => {
                        if !previous_session.is_empty() {
                            let _ = connection
                                .send_request(CloseSessionRequest::new(previous_session))
                                .block_task()
                                .await;
                        }
                        let _ = tx.send(RuntimeEvent::SessionReady {
                            id: response.session_id.0.to_string(),
                            modes: response.modes,
                            configs: response.config_options,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(RuntimeEvent::OperationFinished {
                            label: "New session",
                            result: Err(error.to_string()),
                        });
                    }
                }
            });
        }
        Action::ResumeSession(session_id) => {
            app.begin_session_switch("Loading session…");
            let connection = connection.clone();
            let previous_session = app.session_id.clone();
            let cwd = cwd.to_path_buf();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = connection
                    .send_request(LoadSessionRequest::new(session_id.clone(), cwd))
                    .block_task()
                    .await;
                match result {
                    Ok(response) => {
                        if !previous_session.is_empty() && previous_session != session_id {
                            let _ = connection
                                .send_request(CloseSessionRequest::new(previous_session))
                                .block_task()
                                .await;
                        }
                        let _ = tx.send(RuntimeEvent::SessionReady {
                            id: session_id,
                            modes: response.modes,
                            configs: response.config_options,
                        });
                    }
                    Err(error) => {
                        let _ = tx.send(RuntimeEvent::OperationFinished {
                            label: "Resume session",
                            result: Err(error.to_string()),
                        });
                    }
                }
            });
        }
        Action::SetMode(mode) => {
            app.mark_working("Switching mode…");
            let connection = connection.clone();
            let session_id = app.session_id.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = connection
                    .send_request(SetSessionModeRequest::new(session_id, mode.clone()))
                    .block_task()
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string());
                let _ = tx.send(RuntimeEvent::ModeApplied { mode, result });
            });
        }
        Action::SetConfig { config_id, value } => {
            app.mark_working("Applying configuration…");
            let connection = connection.clone();
            let session_id = app.session_id.clone();
            let tx = event_tx.clone();
            tokio::spawn(async move {
                let result = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        session_id,
                        config_id,
                        SessionConfigOptionValue::value_id(value),
                    ))
                    .block_task()
                    .await
                    .map(|response| response.config_options)
                    .map_err(|error| error.to_string());
                let _ = tx.send(RuntimeEvent::ConfigApplied(result));
            });
        }
        Action::Quit => app.quit = true,
    }
}

fn handle_runtime_event(app: &mut App, event: RuntimeEvent) -> Option<Action> {
    match event {
        RuntimeEvent::Session(notification) => {
            if notification.session_id.0.as_ref() == app.session_id
                || app.phase == app::Phase::Working
            {
                app.on_session_update(notification.update);
            }
        }
        RuntimeEvent::Permission { request, responder } => {
            app.set_permission(PendingPermission::from_request(request, responder));
        }
        RuntimeEvent::PromptFinished(result) => return app.on_prompt_finished(result),
        RuntimeEvent::OperationFinished { label, result } => {
            app.on_operation_finished(label, result)
        }
        RuntimeEvent::ConfigApplied(result) => match result {
            Ok(configs) => {
                app.set_configs(configs);
                app.on_operation_finished("Configuration", Ok(()));
            }
            Err(error) => app.on_operation_finished("Configuration", Err(error)),
        },
        RuntimeEvent::ModeApplied { mode, result } => {
            if result.is_ok() {
                app.current_mode = Some(mode);
            }
            app.on_operation_finished("Mode switch", result);
        }
        RuntimeEvent::SessionReady { id, modes, configs } => app.set_session(id, modes, configs),
        RuntimeEvent::TransportClosed => {
            app.add_notice("ACP connection closed", "The agent process exited.");
            app.status = "Disconnected".into();
            app.phase = app::Phase::Idle;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_workspace_and_session() {
        let options = parse_options([
            OsString::from("--cwd"),
            OsString::from("."),
            OsString::from("--session"),
            OsString::from("abc"),
            OsString::from("--acp-exe"),
            OsString::from("/bin/fx-acp"),
        ])
        .unwrap()
        .unwrap();
        assert!(options.cwd.is_absolute());
        assert_eq!(options.session.as_deref(), Some("abc"));
        assert_eq!(options.acp_exe, PathBuf::from("/bin/fx-acp"));
    }

    #[test]
    fn rejects_unknown_arguments() {
        let error = parse_options([OsString::from("--wat")]).unwrap_err();
        assert!(error.to_string().contains("unknown argument"));
    }
}
