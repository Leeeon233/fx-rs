#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fx_core::CancellationSignal;
use fx_process::monitor::{
    MonitorCondition, MonitorDefinition, MonitorLifetime, MonitorOperation, NotifySchedule,
};
use fx_process::terminal_host_protocol::{HostRequest, HostResponse, ProtocolHello};
use fx_process::terminal_host_protocol::{
    ReturnCondition, TerminalBackend, TerminalClosePolicy, TerminalOperation, TerminalWrite,
};
use fx_process::terminal_host_server::{HostClient, HostServerConfig, INTERNAL_MODE, request};

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct HostedSessionGuard {
    client: HostClient,
    session_id: Option<String>,
}

#[derive(Default)]
struct TestCancellation(AtomicBool);

impl CancellationSignal for TestCancellation {
    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

impl Drop for HostedSessionGuard {
    fn drop(&mut self) {
        if let Some(session_id) = self.session_id.take() {
            let _ = self.client.terminal(TerminalOperation::Close {
                session_id,
                close_policy: TerminalClosePolicy::Force,
            });
        }
    }
}

#[test]
fn private_companion_negotiates_and_exits_after_idle() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        PathBuf::from("/tmp").join(format!("fxth-process-test-{}-{nonce}", std::process::id()));
    let config = HostServerConfig {
        state_directory: root.clone(),
        idle_grace: Duration::from_millis(200),
    };
    let endpoint = config.endpoint();
    let child = Command::new(env!("CARGO_BIN_EXE_fx-terminal-host"))
        .arg(INTERNAL_MODE)
        .env("FX_TERMINAL_HOST_DIR", &root)
        .env("FX_TERMINAL_HOST_IDLE_MS", "200")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = ChildGuard(child);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !endpoint.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(endpoint.exists());
    let response = request(
        &endpoint,
        &HostRequest::Hello {
            hello: ProtocolHello::local(),
        },
    )
    .unwrap();
    assert!(matches!(
        response,
        HostResponse::Hello {
            negotiated: fx_process::terminal_host_protocol::PROTOCOL_CURRENT,
            ..
        }
    ));
    assert_eq!(
        request(&endpoint, &HostRequest::Ping).unwrap(),
        HostResponse::Pong
    );

    let exit_deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "host did not retire after idle grace"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success());
    assert!(!endpoint.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn public_invocation_is_rejected() {
    let output = Command::new(env!("CARGO_BIN_EXE_fx-terminal-host"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("private companion"));
}

#[test]
fn concurrent_clients_race_to_one_companion_instance() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        PathBuf::from("/tmp").join(format!("fxth-client-test-{}-{nonce}", std::process::id()));
    let config = HostServerConfig {
        state_directory: root.clone(),
        idle_grace: Duration::from_millis(300),
    };
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_fx-terminal-host"));
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let config = config.clone();
            let executable = executable.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                HostClient::connect_or_spawn(&config, &executable).unwrap()
            })
        })
        .collect();
    let clients: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(clients[0].instance_id(), clients[1].instance_id());
    clients[0].ping().unwrap();

    let endpoint = config.endpoint();
    let deadline = Instant::now() + Duration::from_secs(2);
    while endpoint.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!endpoint.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn companion_owns_a_native_pty_across_client_reconstruction() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        PathBuf::from("/tmp").join(format!("fxth-session-test-{}-{nonce}", std::process::id()));
    let config = HostServerConfig {
        state_directory: root.clone(),
        idle_grace: Duration::from_millis(250),
    };
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_fx-terminal-host"));
    let client = HostClient::connect_or_spawn(&config, &executable).unwrap();
    let shell = test_shell();
    let command = "printf hosted_ready; IFS= read -r line; printf '\\nhosted:%s\\n' \"$line\"";
    let mut arguments = if shell.ends_with("bash") {
        vec!["--noprofile".into(), "--norc".into(), "-i".into()]
    } else {
        vec!["-f".into(), "-i".into()]
    };
    arguments.extend(["-c".into(), command.into()]);
    let started = client
        .terminal(TerminalOperation::Start {
            backend: TerminalBackend::Native,
            workspace_root: std::env::current_dir().unwrap(),
            cwd: std::env::current_dir().unwrap(),
            initial_monitors: Vec::new(),
            command: Some(command.into()),
            shell,
            arguments,
            sandbox_profile: None,
            rows: 8,
            columns: 40,
            return_when: Some(ReturnCondition::Match {
                pattern: "hosted_ready".into(),
            }),
            wait_ceiling_ms: Some(2_000),
        })
        .unwrap();
    assert!(!started.is_error, "{}", started.content);
    let session_id =
        started.structured.as_ref().unwrap()["success"]["start"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
    let instance_id = client.instance_id().to_owned();
    drop(client);

    let recovered = HostClient::connect(config.endpoint()).unwrap();
    assert_eq!(recovered.instance_id(), instance_id);
    let mut guard = HostedSessionGuard {
        client: recovered.clone(),
        session_id: Some(session_id.clone()),
    };
    let read = recovered
        .terminal(TerminalOperation::Read {
            session_id: session_id.clone(),
            cursor_segment: 1,
            cursor_offset: 0,
        })
        .unwrap();
    assert!(read.content.contains("hosted_ready"), "{}", read.content);
    let written = recovered
        .terminal(TerminalOperation::Write {
            session_id: session_id.clone(),
            write: TerminalWrite::Text {
                text: "hello\n".into(),
            },
        })
        .unwrap();
    assert!(!written.is_error, "{}", written.content);
    let waited = recovered
        .terminal(TerminalOperation::Wait {
            session_id: session_id.clone(),
            return_when: ReturnCondition::Exit,
            wait_ceiling_ms: 2_000,
        })
        .unwrap();
    assert!(
        waited.structured.as_ref().unwrap()["success"]["wait"]["outcome"]
            .get("exited")
            .is_some(),
        "{}",
        waited.content
    );
    let closed = recovered
        .terminal(TerminalOperation::Close {
            session_id,
            close_policy: TerminalClosePolicy::Force,
        })
        .unwrap();
    assert!(!closed.is_error, "{}", closed.content);
    guard.session_id = None;

    let endpoint = config.endpoint();
    let deadline = Instant::now() + Duration::from_secs(2);
    while endpoint.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!endpoint.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn companion_cancels_a_remote_start_wait_without_losing_the_session() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        PathBuf::from("/tmp").join(format!("fxth-cancel-test-{}-{nonce}", std::process::id()));
    let config = HostServerConfig {
        state_directory: root.clone(),
        idle_grace: Duration::from_millis(250),
    };
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_fx-terminal-host"));
    let client = HostClient::connect_or_spawn(&config, &executable).unwrap();
    let shell = test_shell();
    let command = "sleep 10";
    let mut arguments = if shell.ends_with("bash") {
        vec!["--noprofile".into(), "--norc".into(), "-i".into()]
    } else {
        vec!["-f".into(), "-i".into()]
    };
    arguments.extend(["-c".into(), command.into()]);
    let cancellation = Arc::new(TestCancellation::default());
    let trigger = cancellation.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        trigger.0.store(true, Ordering::Release);
    });
    let started_at = Instant::now();
    let started = client
        .terminal_cancellable(
            TerminalOperation::Start {
                backend: TerminalBackend::Native,
                workspace_root: std::env::current_dir().unwrap(),
                cwd: std::env::current_dir().unwrap(),
                initial_monitors: Vec::new(),
                command: Some(command.into()),
                shell,
                arguments,
                sandbox_profile: None,
                rows: 8,
                columns: 40,
                return_when: Some(ReturnCondition::Match {
                    pattern: "never-produced".into(),
                }),
                wait_ceiling_ms: Some(10_000),
            },
            cancellation,
        )
        .unwrap();
    canceller.join().unwrap();
    assert!(started_at.elapsed() < Duration::from_secs(2));
    assert_eq!(
        started.structured.as_ref().unwrap()["success"]["start"]["outcome"],
        serde_json::json!({"cancelled": {}})
    );
    let session_id =
        started.structured.as_ref().unwrap()["success"]["start"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
    let closed = client
        .terminal(TerminalOperation::Close {
            session_id,
            close_policy: TerminalClosePolicy::Force,
        })
        .unwrap();
    assert!(!closed.is_error, "{}", closed.content);

    let endpoint = config.endpoint();
    let deadline = Instant::now() + Duration::from_secs(2);
    while endpoint.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!endpoint.exists());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn companion_persists_monitor_operations_and_projects_inspect_events() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root =
        PathBuf::from("/tmp").join(format!("fxth-monitor-test-{}-{nonce}", std::process::id()));
    let config = HostServerConfig {
        state_directory: root.clone(),
        idle_grace: Duration::from_millis(250),
    };
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_fx-terminal-host"));
    let client = HostClient::connect_or_spawn(&config, &executable).unwrap();
    let shell = test_shell();
    let command = "printf ready; sleep 10";
    let mut arguments = if shell.ends_with("bash") {
        vec!["--noprofile".into(), "--norc".into(), "-i".into()]
    } else {
        vec!["-f".into(), "-i".into()]
    };
    arguments.extend(["-c".into(), command.into()]);
    let started = client
        .terminal(TerminalOperation::Start {
            backend: TerminalBackend::Native,
            workspace_root: std::env::current_dir().unwrap(),
            cwd: std::env::current_dir().unwrap(),
            initial_monitors: vec![MonitorDefinition {
                condition: MonitorCondition::OutputContains {
                    pattern: "ready".into(),
                },
                check_interval_ms: None,
                notify: NotifySchedule::OnMatch,
                lifetime: MonitorLifetime::UntilSessionEnd,
            }],
            command: Some(command.into()),
            shell,
            arguments,
            sandbox_profile: None,
            rows: 8,
            columns: 40,
            return_when: Some(ReturnCondition::Started),
            wait_ceiling_ms: None,
        })
        .unwrap();
    let session_id =
        started.structured.as_ref().unwrap()["success"]["start"]["session"]["session_id"]
            .as_str()
            .unwrap()
            .to_owned();
    let mut guard = HostedSessionGuard {
        client: client.clone(),
        session_id: Some(session_id.clone()),
    };
    assert_eq!(
        started.structured.as_ref().unwrap()["success"]["start"]["session"]["active_monitor_count"],
        1
    );
    let match_deadline = Instant::now() + Duration::from_secs(2);
    let matched = loop {
        let inspected = client
            .terminal(TerminalOperation::Inspect {
                session_id: session_id.clone(),
                after_event_id: None,
                acknowledge_event_id: None,
                max_events: 256,
            })
            .unwrap();
        if inspected.structured.as_ref().unwrap()["success"]["inspect"]["events"]
            .as_array()
            .is_some_and(|events| !events.is_empty())
        {
            break inspected;
        }
        assert!(
            Instant::now() < match_deadline,
            "monitor did not observe terminal output"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(
        matched.structured.as_ref().unwrap()["success"]["inspect"]["events"][0]["reason"],
        "matched"
    );
    let non_consuming_read = client
        .terminal(TerminalOperation::Read {
            session_id: session_id.clone(),
            cursor_segment: 1,
            cursor_offset: 0,
        })
        .unwrap();
    assert!(
        non_consuming_read.content.contains("ready"),
        "{}",
        non_consuming_read.content
    );
    let paused = client
        .terminal(TerminalOperation::Monitor {
            session_id: session_id.clone(),
            operation: MonitorOperation::Pause {
                monitor_id: "monitor-1".into(),
            },
        })
        .unwrap();
    assert_eq!(
        paused.structured.as_ref().unwrap()["success"]["monitor"]["session"]["active_monitor_count"],
        0
    );
    let instance = client.instance_id().to_owned();
    drop(client);

    let recovered = HostClient::connect(config.endpoint()).unwrap();
    assert_eq!(recovered.instance_id(), instance);
    let inspected = recovered
        .terminal(TerminalOperation::Inspect {
            session_id: session_id.clone(),
            after_event_id: None,
            acknowledge_event_id: None,
            max_events: 256,
        })
        .unwrap();
    let inspect = &inspected.structured.as_ref().unwrap()["success"]["inspect"];
    assert_eq!(inspect["monitors"][0]["monitor_id"], "monitor-1");
    assert_eq!(inspect["monitors"][0]["state"], "paused");
    assert_eq!(inspect["events"][0]["event_id"], 1);
    assert_eq!(inspect["events"][0]["reason"], "matched");
    assert_eq!(inspect["events"][0]["cursor"]["segment"], 1);
    assert_eq!(inspect["events"][1]["event_id"], 2);
    assert_eq!(inspect["events"][1]["reason"], "paused");
    assert_eq!(inspect["next_event_id"], 3);
    let acknowledged = recovered
        .terminal(TerminalOperation::Inspect {
            session_id: session_id.clone(),
            after_event_id: Some(2),
            acknowledge_event_id: Some(2),
            max_events: 256,
        })
        .unwrap();
    assert!(
        acknowledged.structured.as_ref().unwrap()["success"]["inspect"]["events"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let closed = recovered
        .terminal(TerminalOperation::Close {
            session_id,
            close_policy: TerminalClosePolicy::Force,
        })
        .unwrap();
    assert!(!closed.is_error, "{}", closed.content);
    guard.session_id = None;

    let endpoint = config.endpoint();
    let deadline = Instant::now() + Duration::from_secs(2);
    while endpoint.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!endpoint.exists());
    fs::remove_dir_all(root).unwrap();
}

fn test_shell() -> PathBuf {
    ["/bin/zsh", "/bin/bash"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("test host has Bash or zsh")
}
