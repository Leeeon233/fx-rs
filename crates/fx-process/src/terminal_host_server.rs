//! Unix supervisor lifecycle shared by the detached host binary and tests.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{collections::BTreeMap, sync::mpsc};

use fs4::TryLockError;
use fx_core::{CancellationSignal, ToolError, ToolOutput};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use crate::native_terminal::{
    ClosePolicy, NamedKey, SessionBackend, StartSpec, TerminalSessionHost, TerminalSignal,
    WaitCondition, WritePayload, cursor, failure_output, success_output, valid_dimensions,
};
use crate::terminal_host::RoutingTerminalHost;
use crate::terminal_host_protocol::{
    CAPABILITY_CANCELLATION, CAPABILITY_TERMINAL_MONITORS, CAPABILITY_TERMINAL_SESSIONS,
    HostRequest, HostResponse, ProtocolHello, ReturnCondition, TerminalBackend,
    TerminalClosePolicy, TerminalKey, TerminalOperation, TerminalSignal as WireSignal,
    TerminalWrite, read_frame, write_frame,
};
use crate::terminal_monitor_store::{MonitorContext, MonitorStore};
use crate::terminal_observation::{ObservedLifecycle, TerminalMonitorSource};

pub const INTERNAL_MODE: &str = "--fx-internal-terminal-host";
pub const ENDPOINT_NAME: &str = "host.sock";
const LOCK_NAME: &str = "host.lock";
const ACCEPT_POLL: Duration = Duration::from_millis(10);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
pub struct HostServerConfig {
    pub state_directory: PathBuf,
    pub idle_grace: Duration,
}

struct HostRuntime {
    sessions: RoutingTerminalHost,
    monitor_store: Mutex<MonitorStore>,
    monitors_present: AtomicBool,
    monitor_failure: Mutex<Option<String>>,
    cancellations: Mutex<BTreeMap<String, Arc<HostCancellation>>>,
}

impl HostRuntime {
    fn new(state_directory: &Path) -> Result<Self, HostServerError> {
        let monitor_store = MonitorStore::new(state_directory)
            .map_err(|error| HostServerError::MonitorState(error.to_string()))?;
        let monitors_present = !monitor_store
            .monitored_sessions()
            .map_err(|error| HostServerError::MonitorState(error.to_string()))?
            .is_empty();
        Ok(Self {
            sessions: RoutingTerminalHost::local(),
            monitor_store: Mutex::new(monitor_store),
            monitors_present: AtomicBool::new(monitors_present),
            monitor_failure: Mutex::new(None),
            cancellations: Mutex::new(BTreeMap::new()),
        })
    }

    fn register(&self, request_id: &str) -> Result<Arc<HostCancellation>, HostServerError> {
        if !valid_request_id(request_id) {
            return Err(HostServerError::InvalidRequestId);
        }
        let mut requests = self
            .cancellations
            .lock()
            .map_err(|_| HostServerError::RequestMapPoisoned)?;
        if requests.contains_key(request_id) {
            return Err(HostServerError::DuplicateRequestId);
        }
        let cancellation = Arc::new(HostCancellation::default());
        requests.insert(request_id.to_owned(), cancellation.clone());
        Ok(cancellation)
    }

    fn finish(&self, request_id: &str) {
        if let Ok(mut requests) = self.cancellations.lock() {
            requests.remove(request_id);
        }
    }

    fn cancel(&self, request_id: &str) -> Result<bool, HostServerError> {
        if !valid_request_id(request_id) {
            return Err(HostServerError::InvalidRequestId);
        }
        let requests = self
            .cancellations
            .lock()
            .map_err(|_| HostServerError::RequestMapPoisoned)?;
        let Some(cancellation) = requests.get(request_id) else {
            return Ok(false);
        };
        cancellation.cancelled.store(true, Ordering::Release);
        Ok(true)
    }

    fn evaluate_monitors(&self) -> Result<(), ToolError> {
        if !self.monitors_present.load(Ordering::Acquire) {
            return Ok(());
        }
        let session_ids = self
            .monitor_store
            .lock()
            .map_err(|_| monitor_store_poisoned())?
            .monitored_sessions()?;
        for session_id in session_ids {
            let request = self
                .monitor_store
                .lock()
                .map_err(|_| monitor_store_poisoned())?
                .observation_request(&session_id)?;
            let Some((cursor_offset, include_screen)) = request else {
                continue;
            };
            let observation =
                self.sessions
                    .observe_terminal(&session_id, cursor_offset, include_screen)?;
            let store = self
                .monitor_store
                .lock()
                .map_err(|_| monitor_store_poisoned())?;
            if let Some(observation) = observation {
                store.evaluate_terminal(&session_id, &observation, now_ms()?)?;
            } else {
                store.end_missing_session(&session_id, now_ms()?)?;
            }
        }
        self.refresh_monitors_present()
    }

    fn refresh_monitors_present(&self) -> Result<(), ToolError> {
        let present = !self
            .monitor_store
            .lock()
            .map_err(|_| monitor_store_poisoned())?
            .monitored_sessions()?
            .is_empty();
        self.monitors_present.store(present, Ordering::Release);
        Ok(())
    }

    fn record_monitor_failure(&self, error: ToolError) {
        if let Ok(mut failure) = self.monitor_failure.lock()
            && failure.is_none()
        {
            *failure = Some(error.to_string());
        }
    }

    fn take_monitor_failure(&self) -> Result<Option<String>, HostServerError> {
        self.monitor_failure
            .lock()
            .map(|mut failure| failure.take())
            .map_err(|_| HostServerError::MonitorState("failure lock is poisoned".into()))
    }
}

#[derive(Default)]
struct HostCancellation {
    cancelled: AtomicBool,
}

impl CancellationSignal for HostCancellation {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

impl HostServerConfig {
    pub fn from_environment() -> Result<Self, HostServerError> {
        let state_directory = match std::env::var_os("FX_TERMINAL_HOST_DIR") {
            Some(path) => PathBuf::from(path),
            None => PathBuf::from(std::env::var_os("HOME").ok_or(HostServerError::MissingHome)?)
                .join(".fx/terminal-host-rs"),
        };
        if !state_directory.is_absolute() {
            return Err(HostServerError::UnsafeStateDirectory);
        }
        let idle_ms = std::env::var("FX_TERMINAL_HOST_IDLE_MS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .map_err(|_| HostServerError::InvalidIdleGrace)?
            .unwrap_or(30_000);
        if idle_ms == 0 {
            return Err(HostServerError::InvalidIdleGrace);
        }
        Ok(Self {
            state_directory,
            idle_grace: Duration::from_millis(idle_ms),
        })
    }

    pub fn endpoint(&self) -> PathBuf {
        self.transport_directory().join(ENDPOINT_NAME)
    }

    fn transport_directory(&self) -> PathBuf {
        let preferred = self.state_directory.join(ENDPOINT_NAME);
        if preferred.as_os_str().as_bytes().len() < endpoint_path_limit() {
            return self.state_directory.clone();
        }
        let digest = Sha256::digest(self.state_directory.as_os_str().as_bytes());
        let hash = hex::encode(&digest[..16]);
        let uid = unsafe { nix::libc::getuid() };
        runtime_base().join(format!("fx-terminal-rs-{uid}-{hash}"))
    }
}

pub fn run(config: HostServerConfig) -> Result<(), HostServerError> {
    prepare_private_directory(&config.state_directory)?;
    let lock = open_private_file(&config.state_directory.join(LOCK_NAME))?;
    match fs4::FileExt::try_lock(&lock) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Error(error)) => return Err(HostServerError::Io(error)),
    }

    let transport_directory = config.transport_directory();
    if transport_directory != config.state_directory {
        prepare_private_directory(&transport_directory)?;
    }
    let endpoint = config.endpoint();
    reject_symlink(&endpoint)?;
    match fs::remove_file(&endpoint) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(HostServerError::Io(error)),
    }
    let listener = UnixListener::bind(&endpoint).map_err(HostServerError::Io)?;
    fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))
        .map_err(HostServerError::Io)?;
    let endpoint_guard = EndpointGuard {
        endpoint,
        transport_directory: (transport_directory != config.state_directory)
            .then_some(transport_directory),
    };
    listener
        .set_nonblocking(true)
        .map_err(HostServerError::Io)?;
    let instance_id = Uuid::new_v4().simple().to_string();
    let runtime = Arc::new(HostRuntime::new(&config.state_directory)?);
    let _monitor_worker = MonitorWorker::spawn(runtime.clone())?;
    let mut last_activity = Instant::now();
    let active_connections = Arc::new(AtomicUsize::new(0));

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last_activity = Instant::now();
                active_connections.fetch_add(1, Ordering::AcqRel);
                let active = active_connections.clone();
                let instance_id = instance_id.clone();
                let runtime = runtime.clone();
                thread::Builder::new()
                    .name("fx-terminal-host-client".into())
                    .spawn(move || {
                        let mut stream = stream;
                        if configure_stream(&stream).is_ok() {
                            let _ = handle_connection(&mut stream, &instance_id, runtime.as_ref());
                        }
                        active.fetch_sub(1, Ordering::AcqRel);
                    })
                    .map_err(|error| {
                        active_connections.fetch_sub(1, Ordering::AcqRel);
                        HostServerError::Io(error)
                    })?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if let Some(failure) = runtime.take_monitor_failure()? {
                    return Err(HostServerError::MonitorState(failure));
                }
                if active_connections.load(Ordering::Acquire) > 0
                    || runtime.sessions.has_process_local_sessions()
                    || runtime.monitors_present.load(Ordering::Acquire)
                {
                    last_activity = Instant::now();
                } else if last_activity.elapsed() >= config.idle_grace {
                    break;
                }
                thread::sleep(ACCEPT_POLL.min(config.idle_grace - last_activity.elapsed()));
            }
            Err(error) => return Err(HostServerError::Io(error)),
        }
    }
    drop(endpoint_guard);
    drop(lock);
    Ok(())
}

struct MonitorWorker {
    stopping: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MonitorWorker {
    fn spawn(runtime: Arc<HostRuntime>) -> Result<Self, HostServerError> {
        let stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = stopping.clone();
        let handle = thread::Builder::new()
            .name("fx-terminal-monitor".into())
            .spawn(move || {
                while !worker_stopping.load(Ordering::Acquire) {
                    if let Err(error) = runtime.evaluate_monitors() {
                        runtime.record_monitor_failure(error);
                        break;
                    }
                    let delay = if runtime.monitors_present.load(Ordering::Acquire) {
                        Duration::from_millis(10)
                    } else {
                        Duration::from_millis(50)
                    };
                    thread::sleep(delay);
                }
            })
            .map_err(HostServerError::Io)?;
        Ok(Self {
            stopping,
            handle: Some(handle),
        })
    }
}

impl Drop for MonitorWorker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn request(endpoint: &Path, request: &HostRequest) -> Result<HostResponse, HostServerError> {
    request_with_timeout(endpoint, request, IO_TIMEOUT)
}

fn request_with_timeout(
    endpoint: &Path,
    request: &HostRequest,
    timeout: Duration,
) -> Result<HostResponse, HostServerError> {
    let mut stream = UnixStream::connect(endpoint).map_err(HostServerError::Io)?;
    configure_stream_with_timeout(&stream, timeout)?;
    write_frame(&mut stream, request).map_err(HostServerError::Protocol)?;
    read_frame(&mut stream).map_err(HostServerError::Protocol)
}

#[derive(Clone, Debug)]
pub struct HostClient {
    endpoint: PathBuf,
    instance_id: String,
}

impl HostClient {
    pub fn connect_or_spawn(
        config: &HostServerConfig,
        executable: &Path,
    ) -> Result<Self, HostServerError> {
        if let Ok(client) = Self::connect(config.endpoint()) {
            return Ok(client);
        }
        if !executable.is_absolute() || !executable.is_file() {
            return Err(HostServerError::InvalidExecutable);
        }
        use std::os::unix::process::CommandExt as _;
        let mut child = Command::new(executable)
            .arg(INTERNAL_MODE)
            .env("FX_TERMINAL_HOST_DIR", &config.state_directory)
            .env(
                "FX_TERMINAL_HOST_IDLE_MS",
                u64::try_from(config.idle_grace.as_millis())
                    .unwrap_or(u64::MAX)
                    .to_string(),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .map_err(HostServerError::Io)?;
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if let Ok(client) = Self::connect(config.endpoint()) {
                reap(child);
                return Ok(client);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(HostServerError::StartupTimeout);
            }
            if let Some(status) = child.try_wait().map_err(HostServerError::Io)?
                && !status.success()
            {
                return Err(HostServerError::StartupFailed(status.code()));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn connect(endpoint: PathBuf) -> Result<Self, HostServerError> {
        let response = request(
            &endpoint,
            &HostRequest::Hello {
                hello: ProtocolHello::local(),
            },
        )?;
        let HostResponse::Hello {
            hello,
            negotiated,
            instance_id,
        } = response
        else {
            return Err(HostServerError::HandshakeFailed);
        };
        let expected = ProtocolHello::local()
            .negotiate(hello)
            .map_err(|_| HostServerError::HandshakeFailed)?;
        let required =
            CAPABILITY_TERMINAL_SESSIONS | CAPABILITY_CANCELLATION | CAPABILITY_TERMINAL_MONITORS;
        if negotiated != expected
            || hello.capabilities & required != required
            || instance_id.len() != 32
        {
            return Err(HostServerError::HandshakeFailed);
        }
        Ok(Self {
            endpoint,
            instance_id,
        })
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    pub fn ping(&self) -> Result<(), HostServerError> {
        match request(&self.endpoint, &HostRequest::Ping)? {
            HostResponse::Pong => Ok(()),
            _ => Err(HostServerError::HandshakeFailed),
        }
    }

    pub fn terminal(&self, operation: TerminalOperation) -> Result<ToolOutput, HostServerError> {
        let request_id = Uuid::new_v4().simple().to_string();
        let timeout = operation_timeout(&operation);
        let response = request_with_timeout(
            &self.endpoint,
            &HostRequest::Terminal {
                request_id,
                operation: Box::new(operation),
            },
            timeout,
        )?;
        terminal_response(response)
    }

    pub fn terminal_cancellable(
        &self,
        operation: TerminalOperation,
        cancellation: Arc<dyn CancellationSignal>,
    ) -> Result<ToolOutput, HostServerError> {
        let request_id = Uuid::new_v4().simple().to_string();
        let endpoint = self.endpoint.clone();
        let timeout = operation_timeout(&operation);
        let worker_id = request_id.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("fx-terminal-host-request".into())
            .spawn(move || {
                let result = request_with_timeout(
                    &endpoint,
                    &HostRequest::Terminal {
                        request_id: worker_id,
                        operation: Box::new(operation),
                    },
                    timeout,
                );
                let _ = sender.send(result);
            })
            .map_err(HostServerError::Io)?;
        let mut cancellation_accepted = false;
        loop {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(response) => return terminal_response(response?),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(HostServerError::RequestWorkerStopped);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancellation.is_cancelled() && !cancellation_accepted {
                cancellation_accepted = matches!(
                    request(
                        &self.endpoint,
                        &HostRequest::Cancel {
                            request_id: request_id.clone()
                        }
                    ),
                    Ok(HostResponse::Cancelled { accepted: true })
                );
            }
        }
    }
}

fn terminal_response(response: HostResponse) -> Result<ToolOutput, HostServerError> {
    match response {
        HostResponse::Terminal { output } => Ok(output),
        HostResponse::Failure { code, message } => {
            Err(HostServerError::RemoteFailure { code, message })
        }
        _ => Err(HostServerError::HandshakeFailed),
    }
}

fn operation_timeout(operation: &TerminalOperation) -> Duration {
    let wait_ms = match operation {
        TerminalOperation::Start {
            wait_ceiling_ms, ..
        } => *wait_ceiling_ms,
        TerminalOperation::Wait {
            wait_ceiling_ms, ..
        } => Some(*wait_ceiling_ms),
        _ => None,
    };
    wait_ms.map_or(IO_TIMEOUT, |milliseconds| {
        Duration::from_millis(milliseconds).saturating_add(IO_TIMEOUT)
    })
}

fn reap(mut child: std::process::Child) {
    let _ = thread::Builder::new()
        .name("fx-terminal-host-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

fn handle_connection(
    stream: &mut UnixStream,
    instance_id: &str,
    runtime: &HostRuntime,
) -> Result<(), HostServerError> {
    let request: HostRequest = read_frame(stream).map_err(HostServerError::Protocol)?;
    let response = match request {
        HostRequest::Hello { hello } => match ProtocolHello::local().negotiate(hello) {
            Ok(negotiated) => HostResponse::Hello {
                hello: ProtocolHello::local(),
                negotiated,
                instance_id: instance_id.to_owned(),
            },
            Err(error) => HostResponse::Failure {
                code: "incompatible_protocol".into(),
                message: error.to_string(),
            },
        },
        HostRequest::Ping => HostResponse::Pong,
        HostRequest::Terminal {
            request_id,
            operation,
        } => match runtime.register(&request_id) {
            Ok(cancellation) => {
                let result = execute_terminal(runtime, *operation, cancellation);
                runtime.finish(&request_id);
                match result {
                    Ok(output) => HostResponse::Terminal { output },
                    Err(error) => HostResponse::Failure {
                        code: "terminal_operation_failed".into(),
                        message: error.to_string(),
                    },
                }
            }
            Err(error) => HostResponse::Failure {
                code: "invalid_request".into(),
                message: error.to_string(),
            },
        },
        HostRequest::Cancel { request_id } => match runtime.cancel(&request_id) {
            Ok(accepted) => HostResponse::Cancelled { accepted },
            Err(error) => HostResponse::Failure {
                code: "invalid_request".into(),
                message: error.to_string(),
            },
        },
    };
    write_frame(stream, &response).map_err(HostServerError::Protocol)
}

fn execute_terminal(
    runtime: &HostRuntime,
    operation: TerminalOperation,
    cancellation: Arc<dyn CancellationSignal>,
) -> Result<ToolOutput, ToolError> {
    let sessions = &runtime.sessions;
    match operation {
        TerminalOperation::Start {
            backend,
            workspace_root,
            cwd,
            initial_monitors,
            command,
            shell,
            arguments,
            sandbox_profile,
            rows,
            columns,
            return_when,
            wait_ceiling_ms,
        } => {
            validate_start(StartValidation {
                workspace_root: &workspace_root,
                cwd: &cwd,
                command: command.as_deref(),
                shell: &shell,
                arguments: &arguments,
                sandbox_profile: sandbox_profile.as_deref(),
                rows,
                columns,
                return_when: return_when.as_ref(),
                wait_ceiling_ms,
            })?;
            let installed_at = now_ms()?;
            runtime
                .monitor_store
                .lock()
                .map_err(|_| monitor_store_poisoned())?
                .validate_initial(&initial_monitors, &workspace_root, &cwd, installed_at)?;
            let requested_return = return_when.map(map_return).transpose()?;
            let output = sessions.start(
                StartSpec {
                    backend: map_backend(backend),
                    workspace_root: workspace_root.clone(),
                    cwd: cwd.clone(),
                    initial_monitors: Vec::new(),
                    command,
                    shell,
                    arguments,
                    sandbox_profile,
                    rows,
                    columns,
                    return_when: Some(WaitCondition::Started),
                    wait_ceiling: None,
                },
                cancellation.clone(),
            )?;
            if output.is_error {
                return Ok(output);
            }
            let session_id = output_session_id(&output, "start")?;
            let source = sessions
                .observe_terminal(&session_id, 0, false)?
                .ok_or_else(|| ToolError::Execution("new terminal session was lost".into()))?;
            let installed = runtime
                .monitor_store
                .lock()
                .map_err(|_| monitor_store_poisoned())?
                .install_initial(
                    &session_id,
                    &initial_monitors,
                    MonitorContext {
                        current_cursor: 0,
                        lifecycle: source.lifecycle,
                        workspace_root: &workspace_root,
                        cwd: &cwd,
                        now_ms: installed_at,
                    },
                );
            if let Err(error) = installed {
                let _ = sessions.close(&session_id, ClosePolicy::Force);
                return Err(error);
            }
            runtime.refresh_monitors_present()?;
            let output = match requested_return {
                None | Some(WaitCondition::Started) => output,
                Some(condition) => rename_success_action(
                    sessions.wait(
                        &session_id,
                        condition,
                        Duration::from_millis(wait_ceiling_ms.unwrap_or_default()),
                        cancellation,
                    )?,
                    "wait",
                    "start",
                )?,
            };
            decorate_terminal_output(runtime, "start", Some(&session_id), output)
        }
        TerminalOperation::Read {
            session_id,
            cursor_segment,
            cursor_offset,
        } => {
            let output = sessions.read(&session_id, cursor_segment, cursor_offset)?;
            decorate_terminal_output(runtime, "read", Some(&session_id), output)
        }
        TerminalOperation::Screen { session_id } => {
            let output = sessions.screen(&session_id)?;
            decorate_terminal_output(runtime, "screen", Some(&session_id), output)
        }
        TerminalOperation::Write { session_id, write } => {
            let output = sessions.write(&session_id, map_write(write)?)?;
            decorate_terminal_output(runtime, "write", Some(&session_id), output)
        }
        TerminalOperation::Wait {
            session_id,
            return_when,
            wait_ceiling_ms,
        } => {
            if wait_ceiling_ms == 0 {
                return Err(ToolError::InvalidArguments(
                    "terminal wait ceiling must be positive".into(),
                ));
            }
            let output = sessions.wait(
                &session_id,
                map_return(return_when)?,
                Duration::from_millis(wait_ceiling_ms),
                cancellation,
            )?;
            decorate_terminal_output(runtime, "wait", Some(&session_id), output)
        }
        TerminalOperation::Monitor {
            session_id,
            operation,
        } => {
            let Some(mut facts) = session_facts(sessions, &session_id)? else {
                return Ok(failure_output(
                    "monitor",
                    Some(&session_id),
                    "session_not_found",
                    false,
                ));
            };
            let observation = observation_from_facts(&facts)?;
            let Some(source) =
                sessions.observe_terminal(&session_id, observation.cursor_offset, false)?
            else {
                return Ok(failure_output(
                    "monitor",
                    Some(&session_id),
                    "session_not_found",
                    false,
                ));
            };
            let (set, monitor_id) = {
                runtime
                    .monitor_store
                    .lock()
                    .map_err(|_| monitor_store_poisoned())?
                    .operate(
                        &session_id,
                        operation,
                        MonitorContext {
                            current_cursor: observation.cursor_offset,
                            lifecycle: observation.lifecycle,
                            workspace_root: &source.workspace_root,
                            cwd: &source.cwd,
                            now_ms: now_ms()?,
                        },
                    )?
            };
            runtime.refresh_monitors_present()?;
            project_monitor_facts(&mut facts, set.active_count())?;
            Ok(success_output(
                "monitor",
                json!({"session": facts, "monitor_id": monitor_id}),
            ))
        }
        TerminalOperation::Inspect {
            session_id,
            after_event_id,
            acknowledge_event_id,
            max_events,
        } => inspect_with_monitors(
            runtime,
            &session_id,
            after_event_id,
            acknowledge_event_id,
            usize::from(max_events),
        ),
        TerminalOperation::List { backend } => {
            decorate_terminal_list(runtime, sessions.list(backend.map(map_backend))?)
        }
        TerminalOperation::Resize {
            session_id,
            rows,
            columns,
        } => {
            let output = sessions.resize(&session_id, rows, columns)?;
            decorate_terminal_output(runtime, "resize", Some(&session_id), output)
        }
        TerminalOperation::Signal { session_id, signal } => {
            let output = sessions.signal(&session_id, map_signal(signal))?;
            decorate_terminal_output(runtime, "signal", Some(&session_id), output)
        }
        TerminalOperation::Close {
            session_id,
            close_policy,
        } => {
            let output = sessions.close(
                &session_id,
                match close_policy {
                    TerminalClosePolicy::Graceful => ClosePolicy::Graceful,
                    TerminalClosePolicy::Force => ClosePolicy::Force,
                },
            )?;
            runtime
                .monitor_store
                .lock()
                .map_err(|_| monitor_store_poisoned())?
                .end_session(&session_id, ObservedLifecycle::Closed, now_ms()?)?;
            runtime.refresh_monitors_present()?;
            decorate_terminal_output(runtime, "close", Some(&session_id), output)
        }
    }
}

fn output_session_id(output: &ToolOutput, action: &str) -> Result<String, ToolError> {
    output
        .structured
        .as_ref()
        .and_then(|root| root.pointer(&format!("/success/{action}/session/session_id")))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ToolError::Execution("terminal host omitted session id".into()))
}

fn rename_success_action(
    output: ToolOutput,
    source_action: &str,
    target_action: &str,
) -> Result<ToolOutput, ToolError> {
    if output.is_error {
        return Ok(output);
    }
    let payload = output
        .structured
        .as_ref()
        .and_then(|root| root.pointer(&format!("/success/{source_action}")))
        .cloned()
        .ok_or_else(|| ToolError::Execution("terminal host returned an invalid result".into()))?;
    Ok(success_output(target_action, payload))
}

fn decorate_terminal_output(
    runtime: &HostRuntime,
    action: &str,
    expected_session_id: Option<&str>,
    output: ToolOutput,
) -> Result<ToolOutput, ToolError> {
    if output.is_error {
        return Ok(output);
    }
    let mut payload = output
        .structured
        .as_ref()
        .and_then(|root| root.pointer(&format!("/success/{action}")))
        .cloned()
        .ok_or_else(|| ToolError::Execution("terminal host returned an invalid result".into()))?;
    let facts = payload
        .get_mut("session")
        .ok_or_else(|| ToolError::Execution("terminal host omitted session facts".into()))?;
    let session_id = facts
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ToolError::Execution("terminal host returned an invalid session id".into())
        })?;
    if expected_session_id.is_some_and(|expected| expected != session_id) {
        return Err(ToolError::Execution(
            "terminal host returned mismatched session facts".into(),
        ));
    }
    let active_count = runtime
        .monitor_store
        .lock()
        .map_err(|_| monitor_store_poisoned())?
        .active_count_for(&session_id)?;
    project_monitor_facts(facts, active_count)?;
    Ok(success_output(action, payload))
}

fn decorate_terminal_list(
    runtime: &HostRuntime,
    output: ToolOutput,
) -> Result<ToolOutput, ToolError> {
    if output.is_error {
        return Ok(output);
    }
    let mut payload = output
        .structured
        .as_ref()
        .and_then(|root| root.pointer("/success/list"))
        .cloned()
        .ok_or_else(|| {
            ToolError::Execution("terminal host returned an invalid list result".into())
        })?;
    let sessions = payload
        .get_mut("sessions")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| {
            ToolError::Execution("terminal host returned an invalid session list".into())
        })?;
    let store = runtime
        .monitor_store
        .lock()
        .map_err(|_| monitor_store_poisoned())?;
    for facts in sessions {
        let session_id = facts
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                ToolError::Execution("terminal host returned an invalid session id".into())
            })?;
        let active_count = store.active_count_for(&session_id)?;
        project_monitor_facts(facts, active_count)?;
    }
    Ok(success_output("list", payload))
}

#[derive(Clone, Copy)]
struct SessionObservation {
    lifecycle: ObservedLifecycle,
    cursor_offset: u64,
}

fn session_facts(
    sessions: &RoutingTerminalHost,
    session_id: &str,
) -> Result<Option<Value>, ToolError> {
    let output = sessions.inspect(session_id, None, None, 256)?;
    if output.is_error {
        return Ok(None);
    }
    output
        .structured
        .as_ref()
        .and_then(|root| root.pointer("/success/inspect/session"))
        .cloned()
        .map(Some)
        .ok_or_else(|| ToolError::Execution("terminal host returned invalid session facts".into()))
}

fn observation_from_facts(facts: &Value) -> Result<SessionObservation, ToolError> {
    let lifecycle = match facts.get("lifecycle").and_then(Value::as_str) {
        Some("running") => ObservedLifecycle::Running,
        Some("exited") => ObservedLifecycle::Exited,
        Some("lost") => ObservedLifecycle::Lost,
        Some("closed") => ObservedLifecycle::Closed,
        _ => {
            return Err(ToolError::Execution(
                "terminal host returned an invalid lifecycle".into(),
            ));
        }
    };
    let cursor_offset = facts
        .pointer("/output_cursor/offset")
        .and_then(Value::as_u64)
        .ok_or_else(|| ToolError::Execution("terminal host returned an invalid cursor".into()))?;
    Ok(SessionObservation {
        lifecycle,
        cursor_offset,
    })
}

fn project_monitor_facts(facts: &mut Value, active_count: usize) -> Result<(), ToolError> {
    let monitor_allowed = facts
        .get("lifecycle")
        .and_then(Value::as_str)
        .map(|lifecycle| lifecycle != "closed")
        .ok_or_else(|| {
            ToolError::Execution("terminal host returned an invalid lifecycle".into())
        })?;
    let object = facts.as_object_mut().ok_or_else(|| {
        ToolError::Execution("terminal host returned invalid session facts".into())
    })?;
    object.insert(
        "active_monitor_count".into(),
        Value::from(u64::try_from(active_count).unwrap_or(u64::MAX)),
    );
    object
        .get_mut("next_actions")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| ToolError::Execution("terminal host returned invalid next actions".into()))?
        .insert("monitor".into(), Value::Bool(monitor_allowed));
    Ok(())
}

fn inspect_with_monitors(
    runtime: &HostRuntime,
    session_id: &str,
    after_event_id: Option<u64>,
    acknowledge_event_id: Option<u64>,
    max_events: usize,
) -> Result<ToolOutput, ToolError> {
    let output = runtime
        .sessions
        .inspect(session_id, None, None, max_events)?;
    if output.is_error {
        return Ok(output);
    }
    let mut payload = output
        .structured
        .as_ref()
        .and_then(|root| root.pointer("/success/inspect"))
        .cloned()
        .ok_or_else(|| {
            ToolError::Execution("terminal host returned an invalid inspect result".into())
        })?;
    let page = runtime
        .monitor_store
        .lock()
        .map_err(|_| monitor_store_poisoned())?
        .inspect(session_id, after_event_id, acknowledge_event_id, max_events)?;
    let active_count = page
        .monitors
        .iter()
        .filter(|summary| {
            !matches!(
                summary.state,
                crate::monitor::MonitorState::Paused | crate::monitor::MonitorState::Degraded
            )
        })
        .count();
    let object = payload.as_object_mut().ok_or_else(|| {
        ToolError::Execution("terminal host returned an invalid inspect result".into())
    })?;
    let facts = object.get_mut("session").ok_or_else(|| {
        ToolError::Execution("terminal host omitted inspect session facts".into())
    })?;
    project_monitor_facts(facts, active_count)?;
    object.insert(
        "monitors".into(),
        serde_json::to_value(&page.monitors)
            .map_err(|error| ToolError::Execution(format!("encode terminal monitors: {error}")))?,
    );
    object.insert(
        "events".into(),
        Value::Array(
            page.events
                .iter()
                .map(|event| {
                    json!({
                        "event_id": event.event_id,
                        "monitor_id": event.monitor_id,
                        "reason": event.reason,
                        "lifecycle": event.lifecycle,
                        "cursor": cursor(event.cursor_offset),
                        "created_at_ms": event.created_at_ms
                    })
                })
                .collect(),
        ),
    );
    object.insert("event_gap_through".into(), page.event_gap_through.into());
    object.insert("next_event_id".into(), page.next_event_id.into());
    Ok(success_output("inspect", payload))
}

fn now_ms() -> Result<i64, ToolError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            ToolError::Execution(format!("terminal host clock is invalid: {error}"))
        })?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| ToolError::Execution("terminal host clock overflowed".into()))
}

fn monitor_store_poisoned() -> ToolError {
    ToolError::Execution("terminal monitor store lock is poisoned".into())
}

struct StartValidation<'a> {
    workspace_root: &'a Path,
    cwd: &'a Path,
    command: Option<&'a str>,
    shell: &'a Path,
    arguments: &'a [String],
    sandbox_profile: Option<&'a str>,
    rows: u16,
    columns: u16,
    return_when: Option<&'a ReturnCondition>,
    wait_ceiling_ms: Option<u64>,
}

fn validate_start(spec: StartValidation<'_>) -> Result<(), ToolError> {
    let argument_bytes = spec
        .arguments
        .iter()
        .map(String::len)
        .try_fold(0_usize, usize::checked_add);
    if !spec.workspace_root.is_absolute()
        || !spec.workspace_root.is_dir()
        || !spec.cwd.is_absolute()
        || !spec.cwd.is_dir()
        || !spec.cwd.starts_with(spec.workspace_root)
        || !spec.shell.is_absolute()
        || !spec.shell.is_file()
        || !matches!(
            spec.shell.file_name().and_then(|name| name.to_str()),
            Some("bash" | "zsh")
        )
        || !valid_dimensions(spec.rows, spec.columns)
        || spec
            .command
            .is_some_and(|value| value.len() > 64 * 1024 || value.contains('\0'))
        || spec.arguments.len() > 64
        || spec.arguments.iter().any(|value| value.contains('\0'))
        || argument_bytes.is_none_or(|bytes| bytes > 64 * 1024)
        || spec
            .sandbox_profile
            .is_some_and(|profile| profile.len() > 64 * 1024 || profile.contains('\0'))
        || spec.wait_ceiling_ms == Some(0)
        || spec.return_when.is_some_and(|condition| {
            !matches!(condition, ReturnCondition::Started) && spec.wait_ceiling_ms.is_none()
        })
        || match spec.command {
            Some(command) => {
                spec.arguments.len() < 2
                    || spec.arguments[spec.arguments.len() - 2] != "-c"
                    || spec.arguments.last().is_none_or(|value| value != command)
            }
            None => spec.arguments.iter().any(|argument| argument == "-c"),
        }
    {
        return Err(ToolError::InvalidArguments(
            "terminal host start request is invalid".into(),
        ));
    }
    Ok(())
}

fn map_backend(backend: TerminalBackend) -> SessionBackend {
    match backend {
        TerminalBackend::Native => SessionBackend::Native,
        TerminalBackend::Tmux => SessionBackend::Tmux,
    }
}

fn map_return(condition: ReturnCondition) -> Result<WaitCondition, ToolError> {
    match condition {
        ReturnCondition::Started => Ok(WaitCondition::Started),
        ReturnCondition::Exit => Ok(WaitCondition::Exit),
        ReturnCondition::Quiet { duration_ms } if duration_ms > 0 => {
            Ok(WaitCondition::Quiet(Duration::from_millis(duration_ms)))
        }
        ReturnCondition::Match { pattern }
            if !pattern.is_empty() && pattern.len() <= 4_096 && !pattern.contains('\0') =>
        {
            Ok(WaitCondition::Match(pattern))
        }
        _ => Err(ToolError::InvalidArguments(
            "terminal host return condition is invalid".into(),
        )),
    }
}

fn map_write(write: TerminalWrite) -> Result<WritePayload, ToolError> {
    match write {
        TerminalWrite::Text { text } | TerminalWrite::Paste { text }
            if text.is_empty() || text.len() > 64 * 1024 =>
        {
            Err(ToolError::InvalidArguments(
                "terminal host text write is invalid".into(),
            ))
        }
        TerminalWrite::Text { text } => Ok(WritePayload::Text(text)),
        TerminalWrite::Paste { text } => Ok(WritePayload::Paste(text)),
        TerminalWrite::Keys { keys } if keys.is_empty() || keys.len() > 4_096 => Err(
            ToolError::InvalidArguments("terminal host key write is invalid".into()),
        ),
        TerminalWrite::Keys { keys } => {
            Ok(WritePayload::Keys(keys.into_iter().map(map_key).collect()))
        }
        TerminalWrite::Controls { controls }
            if controls.is_empty()
                || controls.len() > 4_096
                || controls
                    .iter()
                    .any(|value| !matches!(value, b'?' | b'@'..=b'_' | b'a'..=b'z')) =>
        {
            Err(ToolError::InvalidArguments(
                "terminal host control write is invalid".into(),
            ))
        }
        TerminalWrite::Controls { controls } => Ok(WritePayload::Controls(controls)),
    }
}

fn map_key(key: TerminalKey) -> NamedKey {
    match key {
        TerminalKey::Enter => NamedKey::Enter,
        TerminalKey::Tab => NamedKey::Tab,
        TerminalKey::Escape => NamedKey::Escape,
        TerminalKey::Backspace => NamedKey::Backspace,
        TerminalKey::Delete => NamedKey::Delete,
        TerminalKey::Insert => NamedKey::Insert,
        TerminalKey::ArrowUp => NamedKey::ArrowUp,
        TerminalKey::ArrowDown => NamedKey::ArrowDown,
        TerminalKey::ArrowLeft => NamedKey::ArrowLeft,
        TerminalKey::ArrowRight => NamedKey::ArrowRight,
        TerminalKey::Home => NamedKey::Home,
        TerminalKey::End => NamedKey::End,
        TerminalKey::PageUp => NamedKey::PageUp,
        TerminalKey::PageDown => NamedKey::PageDown,
    }
}

fn map_signal(signal: WireSignal) -> TerminalSignal {
    match signal {
        WireSignal::Hangup => TerminalSignal::Hangup,
        WireSignal::Interrupt => TerminalSignal::Interrupt,
        WireSignal::Quit => TerminalSignal::Quit,
        WireSignal::Terminate => TerminalSignal::Terminate,
        WireSignal::Kill => TerminalSignal::Kill,
    }
}

fn configure_stream(stream: &UnixStream) -> Result<(), HostServerError> {
    configure_stream_with_timeout(stream, IO_TIMEOUT)
}

fn configure_stream_with_timeout(
    stream: &UnixStream,
    timeout: Duration,
) -> Result<(), HostServerError> {
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(timeout)))
        .map_err(HostServerError::Io)
}

fn prepare_private_directory(path: &Path) -> Result<(), HostServerError> {
    if !path.is_absolute() {
        return Err(HostServerError::UnsafeStateDirectory);
    }
    fs::create_dir_all(path).map_err(HostServerError::Io)?;
    let metadata = fs::symlink_metadata(path).map_err(HostServerError::Io)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(HostServerError::UnsafeStateDirectory);
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(HostServerError::Io)
}

fn open_private_file(path: &Path) -> Result<File, HostServerError> {
    reject_symlink(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(HostServerError::Io)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(HostServerError::Io)?;
    Ok(file)
}

fn reject_symlink(path: &Path) -> Result<(), HostServerError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(HostServerError::UnsafeStateDirectory)
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(HostServerError::Io(error)),
    }
}

fn valid_request_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct EndpointGuard {
    endpoint: PathBuf,
    transport_directory: Option<PathBuf>,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.endpoint);
        if let Some(directory) = &self.transport_directory {
            let _ = fs::remove_dir(directory);
        }
    }
}

const fn endpoint_path_limit() -> usize {
    if cfg!(target_os = "macos") { 104 } else { 108 }
}

fn runtime_base() -> PathBuf {
    if cfg!(target_os = "macos") {
        PathBuf::from("/private/tmp")
    } else {
        PathBuf::from("/tmp")
    }
}

#[derive(Debug)]
pub enum HostServerError {
    Io(io::Error),
    Protocol(crate::terminal_host_protocol::ProtocolError),
    MissingHome,
    InvalidIdleGrace,
    UnsafeStateDirectory,
    InvalidExecutable,
    HandshakeFailed,
    StartupTimeout,
    StartupFailed(Option<i32>),
    RemoteFailure { code: String, message: String },
    InvalidRequestId,
    DuplicateRequestId,
    RequestMapPoisoned,
    RequestWorkerStopped,
    MonitorState(String),
}

impl std::fmt::Display for HostServerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "terminal host I/O: {error}"),
            Self::Protocol(error) => write!(formatter, "terminal host protocol: {error}"),
            Self::MissingHome => formatter.write_str("HOME is required for the terminal host"),
            Self::InvalidIdleGrace => formatter.write_str("terminal host idle grace is invalid"),
            Self::UnsafeStateDirectory => {
                formatter.write_str("terminal host state directory is unsafe")
            }
            Self::InvalidExecutable => formatter.write_str("terminal host executable is invalid"),
            Self::HandshakeFailed => formatter.write_str("terminal host handshake failed"),
            Self::StartupTimeout => formatter.write_str("terminal host startup timed out"),
            Self::StartupFailed(code) => write!(
                formatter,
                "terminal host startup failed{}",
                code.map_or_else(String::new, |code| format!(" with exit code {code}"))
            ),
            Self::RemoteFailure { code, message } => {
                write!(formatter, "terminal host {code}: {message}")
            }
            Self::InvalidRequestId => formatter.write_str("terminal host request id is invalid"),
            Self::DuplicateRequestId => formatter.write_str("terminal host request id is active"),
            Self::RequestMapPoisoned => {
                formatter.write_str("terminal host request map is poisoned")
            }
            Self::RequestWorkerStopped => {
                formatter.write_str("terminal host request worker stopped")
            }
            Self::MonitorState(message) => {
                write!(formatter, "terminal host monitor state: {message}")
            }
        }
    }
}

impl std::error::Error for HostServerError {}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn server_negotiates_pings_enforces_single_instance_and_retires_idle() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = PathBuf::from("/tmp").join(format!(
            "fx-terminal-host-test-{}-{nonce}",
            std::process::id()
        ));
        let config = HostServerConfig {
            state_directory: root.clone(),
            idle_grace: Duration::from_millis(150),
        };
        let endpoint = config.endpoint();
        let server_config = config.clone();
        let server = thread::spawn(move || run(server_config));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !endpoint.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(endpoint.exists());

        let hello = request(
            &endpoint,
            &HostRequest::Hello {
                hello: ProtocolHello::local(),
            },
        )
        .unwrap();
        let HostResponse::Hello {
            negotiated,
            instance_id,
            ..
        } = hello
        else {
            panic!("host did not negotiate")
        };
        assert_eq!(negotiated, crate::terminal_host_protocol::PROTOCOL_CURRENT);
        assert_eq!(instance_id.len(), 32);
        assert_eq!(
            request(&endpoint, &HostRequest::Ping).unwrap(),
            HostResponse::Pong
        );

        // A second server sees the held advisory lock and exits without
        // unlinking or replacing the live endpoint.
        run(config).unwrap();
        assert!(endpoint.exists());
        server.join().unwrap().unwrap();
        assert!(!endpoint.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
