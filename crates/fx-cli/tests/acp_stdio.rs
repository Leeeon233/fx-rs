#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, CloseSessionRequest, ContentBlock, InitializeRequest, LoadSessionRequest,
    LogoutRequest, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionConfigOptionValue, SessionId, SessionNotification, SetSessionConfigOptionRequest,
    SetSessionModeRequest, StopReason, TextContent,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent, Client, ConnectionTo};
use base64::prelude::{BASE64_URL_SAFE_NO_PAD, Engine as _};
use fx_auth::FileCredentialStore;
use fx_core::{Role, SessionStore, SessionTarget};
use fx_provider::{Credential, CredentialStore};
use fx_store::EventLogSessionStore;
use serde_json::json;

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("fx-acp-{label}-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn home(label: &str) -> Self {
        let directory = Self::new(label);
        let codex = directory.path().join(".codex");
        fs::create_dir_all(&codex).unwrap();
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60 * 60;
        let payload = json!({
            "exp": expiry,
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct_test"}
        });
        let token = format!(
            "e30.{}.signature",
            BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        );
        let auth = codex.join("auth.json");
        fs::write(
            &auth,
            json!({
                "auth_mode": "chatgpt",
                "tokens": {"access_token": token, "account_id": "acct_test"}
            })
            .to_string(),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(auth, fs::Permissions::from_mode(0o600)).unwrap();
        directory
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn saved_vercel_login_refreshes_model_options_after_session_start() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let catalog = std::thread::spawn(move || -> Result<String, String> {
        let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
        let request = read_http_headers(&mut stream)?;
        let body = r#"{"data":[{"id":"zai/glm-5.2","name":"GLM 5.2","type":"language","context_window":1000000,"max_tokens":128000,"tags":["reasoning"]},{"id":"private/team-model","name":"Team Model","type":"language","context_window":200000,"max_tokens":16000,"tags":["tool-use"]}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .map_err(|error| error.to_string())?;
        Ok(request)
    });

    let home = TestDirectory::home("vercel-catalog-home");
    let mut attributes = BTreeMap::new();
    attributes.insert("team_id".into(), "team_1".into());
    let expires_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        + 3_600_000;
    FileCredentialStore::new(home.path().join(".fx/credentials"))
        .lock("vercel")
        .unwrap()
        .replace(Credential::OAuth {
            access_token: "vercel-access".into(),
            refresh_token: Some("vercel-refresh".into()),
            expires_at_ms,
            attributes,
        })
        .unwrap();
    let workspace = TestDirectory::new("vercel-catalog-workspace");
    let workspace_path = workspace.path().to_path_buf();
    let received_team_model = Arc::new(Mutex::new(false));
    let notification_flag = received_team_model.clone();
    let assertion_flag = received_team_model.clone();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_GATEWAY_BASE_URL", format!("http://{address}")),
    );

    Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                if format!("{:?}", notification.update).contains("vercel/private/team-model") {
                    *notification_flag.lock().unwrap() = true;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            for _ in 0..100 {
                if *assertion_flag.lock().unwrap() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("ACP client did not receive the refreshed Vercel model catalog");
        })
        .await
        .expect("Vercel catalog ACP exchange failed");

    let request = catalog.join().unwrap().unwrap().to_ascii_lowercase();
    assert!(request.starts_with("get /coding-agent/v1/models?teamid=team_1 "));
    assert!(request.contains("authorization: bearer vercel-access"));
}

#[tokio::test(flavor = "current_thread")]
async fn acp_reuses_ambient_codex_oauth_at_first_prompt() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let (headers, request) = serve_gateway_response_with_headers(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"authenticated"}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        let headers = headers.to_ascii_lowercase();
        if !headers.contains("authorization: bearer e30.")
            || !headers.contains("chatgpt-account-id: acct_test")
        {
            return Err("Codex request did not use the ambient OAuth account".into());
        }
        for name in [
            "read_tool_result",
            "semantic_search",
            "memory",
            "install_skill",
        ] {
            if !request.contains(&format!(r#""name":"{name}""#)) {
                return Err(format!("ACP session did not advertise {name}"));
            }
        }
        Ok(())
    });

    let home = TestDirectory::home("codex-home");
    let workspace = TestDirectory::new("oidc-workspace");
    let workspace_path = workspace.path().to_path_buf();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("authenticate"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("Codex OAuth ACP exchange failed");
    gateway.join().unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn web_search_runs_provider_worker_through_the_acp_tool_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let first = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"web_1","toolName":"web_search"}"#,
                r#"{"type":"tool-input-delta","id":"web_1","delta":"{\"query\":\"current Rust release\",\"allowed_domains\":[\"rust-lang.org\"]}"}"#,
                r#"{"type":"tool-call","toolCallId":"web_1"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;
        if !first.contains(r#""name":"web_search""#) {
            return Err("outer agent request did not advertise web_search".into());
        }

        let worker = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-call","toolCallId":"provider_1","toolName":"perplexity_search","input":{},"providerExecuted":true}"#,
                r#"{"type":"tool-result","toolCallId":"provider_1","result":{"results":[{"title":"Rust 1.90","url":"https://blog.rust-lang.org/release/1.90.0/"}]}}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"},"usage":{"inputTokens":{"total":7},"outputTokens":{"total":3}}}"#,
            ],
        )?;
        if !worker.contains(r#""type":"web_search""#)
            || !worker.contains(r#""tool_choice":"required""#)
            || !worker.contains("rust-lang.org")
            || !worker.contains(SEARCH_WORKER_PROMPT_FRAGMENT)
        {
            return Err("web search worker request did not preserve provider semantics".into());
        }

        let final_request = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"Rust 1.90 is current."}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        if !final_request.contains("Rust 1.90")
            || !final_request.contains("blog.rust-lang.org")
            || !final_request.contains("untrusted reference material")
        {
            return Err("final agent request omitted normalized web search results".into());
        }
        Ok(())
    });

    let home = TestDirectory::home("web-search-home");
    let workspace = TestDirectory::new("web-search-workspace");
    let workspace_path = workspace.path().to_path_buf();
    let permission_requests = Arc::new(Mutex::new(0usize));
    let review_count = permission_requests.clone();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                *review_count.lock().unwrap() += 1;
                let option = request
                    .options
                    .iter()
                    .find(|option| option.option_id.0.as_ref() == "allow_once")
                    .expect("allow_once option")
                    .option_id
                    .clone();
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("find current Rust"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("web search ACP exchange failed");
    gateway.join().unwrap().unwrap();
    assert_eq!(*permission_requests.lock().unwrap(), 1);
}

const SEARCH_WORKER_PROMPT_FRAGMENT: &str =
    "Research the user's query with the web_search tool and preserve sources for citation.";

#[tokio::test(flavor = "current_thread")]
async fn skill_catalog_and_invocation_flow_through_acp_without_permission() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let first = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"skill_1","toolName":"skill"}"#,
                r#"{"type":"tool-input-delta","id":"skill_1","delta":"{\"name\":\"workflow\"}"}"#,
                r#"{"type":"tool-call","toolCallId":"skill_1"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;
        if !first.contains(r#""name":"skill""#)
            || !first.contains("<available_skills>")
            || !first.contains("workflow")
            || !first.contains("Use the skill tool to load a skill")
        {
            return Err("agent request omitted the advertised skill catalog".into());
        }

        let second = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"skill loaded"}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        if !second.contains("<skill_content")
            || !second.contains("Follow the project workflow")
            || !second.contains("next_offset")
        {
            return Err("final agent request omitted the bounded skill content".into());
        }
        Ok(())
    });

    let home = TestDirectory::home("skill-home");
    let workspace = TestDirectory::new("skill-workspace");
    let skill = workspace.path().join("skills/workflow");
    fs::create_dir_all(&skill).unwrap();
    fs::write(
        skill.join("SKILL.md"),
        "---\nname: workflow\ndescription: Project workflow\n---\nFollow the project workflow.\n",
    )
    .unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let permission_requests = Arc::new(Mutex::new(0usize));
    let review_count = permission_requests.clone();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _connection| {
                *review_count.lock().unwrap() += 1;
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new(
                        "use the workflow skill",
                    ))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("skill ACP exchange failed");
    gateway.join().unwrap().unwrap();
    assert_eq!(*permission_requests.lock().unwrap(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn asynchronous_subagent_create_and_wait_flow_through_acp() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let first = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"sub_create","toolName":"subagent"}"#,
                r#"{"type":"tool-input-delta","id":"sub_create","delta":"{\"command\":{\"create\":{\"name\":\"worker\",\"mode\":\"one_off\",\"prompt\":\"delegated child work\"}}}"}"#,
                r#"{"type":"tool-call","toolCallId":"sub_create"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;
        if !first.contains(r#""name":"subagent""#)
            || !first.contains("durable asynchronous child sessions")
        {
            return Err("root request did not advertise the subagent contract".into());
        }

        let mut child_id = None;
        let mut child_answered = false;
        let mut root_inspect_answered = false;
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(|error| error.to_string())?;
            let body = read_http_request(&mut stream)?;
            if !child_answered && body.contains("You are child agent") {
                if !body.contains("delegated child work") || !body.contains(r#""name":"subagent""#)
                {
                    return Err("child request lost prompt or recursive tool authority".into());
                }
                write_gateway_events(
                    &mut stream,
                    &[
                        r#"{"type":"text-delta","delta":"child finding"}"#,
                        r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
                    ],
                )?;
                child_answered = true;
            } else if !root_inspect_answered {
                let id = extract_subagent_child_id(&body).ok_or_else(|| {
                    format!(
                        "created result omitted child_id: {}",
                        body.chars().take(1000).collect::<String>()
                    )
                })?;
                child_id = Some(id.clone());
                let delta = serde_json::to_string(&json!({
                    "command": {
                        "inspect": {
                            "id": id,
                            "sections": ["status", "messages", "tool_activity"],
                            "wait": {"until": "settled", "timeout_ms": 5000}
                        }
                    }
                }))
                .map_err(|error| error.to_string())?;
                let events = [
                    r#"{"type":"tool-input-start","id":"sub_wait","toolName":"subagent"}"#
                        .to_owned(),
                    format!(
                        "{{\"type\":\"tool-input-delta\",\"id\":\"sub_wait\",\"delta\":{}}}",
                        serde_json::to_string(&delta).unwrap()
                    ),
                    r#"{"type":"tool-call","toolCallId":"sub_wait"}"#.to_owned(),
                    r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#.to_owned(),
                ];
                let refs = events.iter().map(String::as_str).collect::<Vec<_>>();
                write_gateway_events(&mut stream, &refs)?;
                root_inspect_answered = true;
            } else {
                if !body.contains("child finding")
                    || !body.contains("completed")
                    || !body.contains(child_id.as_deref().unwrap_or_default())
                {
                    return Err("root request omitted the settled child result".into());
                }
                write_gateway_events(
                    &mut stream,
                    &[
                        r#"{"type":"text-delta","delta":"used child finding"}"#,
                        r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
                    ],
                )?;
            }
        }
        if !child_answered || !root_inspect_answered || child_id.is_none() {
            return Err("subagent exchange did not complete all phases".into());
        }
        Ok(())
    });

    let home = TestDirectory::home("subagent-home");
    let workspace = TestDirectory::new("subagent-workspace");
    let workspace_path = workspace.path().to_path_buf();
    let permission_requests = Arc::new(Mutex::new(0usize));
    let review_count = permission_requests.clone();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    let exchange = Client
        .builder()
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _connection| {
                *review_count.lock().unwrap() += 1;
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("delegate and wait"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await;
    gateway.join().unwrap().unwrap();
    exchange.expect("subagent ACP exchange failed");
    assert_eq!(*permission_requests.lock().unwrap(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn acp_initialization_stays_lazy_when_no_credential_exists() {
    let home = TestDirectory::new("missing-auth-home");
    let workspace = TestDirectory::new("missing-auth-workspace");
    let workspace_path = workspace.path().to_path_buf();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string()),
    );
    Client
        .builder()
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            let initialized = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
            assert_eq!(initialized.auth_methods.len(), 2);
            assert_eq!(initialized.auth_methods[0].id().0.as_ref(), "codex:chatgpt");
            assert_eq!(initialized.auth_methods[1].id().0.as_ref(), "vercel:oauth");
            assert!(initialized.agent_capabilities.auth.logout.is_some());
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let result = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("needs auth"))],
                ))
                .block_task()
                .await;
            assert!(result.is_err());
            Ok(())
        })
        .await
        .expect("missing-auth ACP setup failed");
}

#[tokio::test(flavor = "current_thread")]
async fn acp_logout_deletes_only_fx_owned_provider_credentials() {
    let home = TestDirectory::home("logout-home");
    let store = FileCredentialStore::from_home(home.path());
    store
        .lock("codex")
        .unwrap()
        .replace(Credential::OAuth {
            access_token: "owned-access".into(),
            refresh_token: Some("owned-refresh".into()),
            expires_at_ms: i64::MAX,
            attributes: BTreeMap::from([("account_id".into(), "acct_owned".into())]),
        })
        .unwrap();
    let owned = home.path().join(".fx/credentials/codex.json");
    let ambient = home.path().join(".codex/auth.json");
    assert!(owned.exists());
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string()),
    );
    Client
        .builder()
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            connection
                .send_request(LogoutRequest::new())
                .block_task()
                .await?;
            Ok(())
        })
        .await
        .expect("ACP logout exchange failed");
    assert!(!owned.exists());
    assert!(ambient.exists());
}

#[tokio::test(flavor = "current_thread")]
async fn permission_round_trip_executes_tool_without_blocking_dispatch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let first = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"write_1","toolName":"write_file"}"#,
                r#"{"type":"tool-input-delta","id":"write_1","delta":"{\"path\":\"result.txt\",\"content\":\"written through ACP\"}"}"#,
                r#"{"type":"tool-call","toolCallId":"write_1"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;
        if !first.contains(r#""name":"write_file""#) {
            return Err("first gateway request did not advertise write_file".into());
        }

        let second = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"tool complete"}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        if !second.contains("wrote result.txt (19 bytes)") {
            return Err("second gateway request did not contain tool output".into());
        }
        Ok(())
    });

    let home = TestDirectory::home("home");
    let workspace = TestDirectory::new("workspace");
    let updates = Arc::new(Mutex::new(Vec::new()));
    let permission_requests = Arc::new(Mutex::new(0usize));
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    let notification_updates = updates.clone();
    let review_count = permission_requests.clone();
    let workspace_path = workspace.path().to_path_buf();

    let exchange = Client
        .builder()
        .on_receive_notification(
            async move |notification: SessionNotification, _connection| {
                notification_updates
                    .lock()
                    .unwrap()
                    .push(format!("{:?}", notification.update));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                *review_count.lock().unwrap() += 1;
                let option = request
                    .options
                    .iter()
                    .find(|option| option.option_id.0.as_ref() == "allow_once")
                    .expect("allow_once option")
                    .option_id
                    .clone();
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |connection: ConnectionTo<Agent>| async move {
            let initialized = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
            assert!(initialized.agent_capabilities.mcp_capabilities.http);
            assert!(!initialized.agent_capabilities.mcp_capabilities.sse);
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("write result"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        });

    tokio::time::timeout(Duration::from_secs(10), exchange)
        .await
        .expect("ACP exchange timed out")
        .expect("ACP exchange failed");
    gateway.join().unwrap().unwrap();
    assert_eq!(*permission_requests.lock().unwrap(), 1);
    assert_eq!(
        fs::read_to_string(workspace.path().join("result.txt")).unwrap(),
        "written through ACP"
    );
    let updates = updates.lock().unwrap();
    assert!(updates.iter().any(|update| update.contains("InProgress")));
    assert!(updates.iter().any(|update| update.contains("Completed")));
}

#[tokio::test(flavor = "current_thread")]
async fn nested_project_rules_defer_acp_write_until_the_model_reissues_it() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"nested_1","toolName":"write_file"}"#,
                r#"{"type":"tool-input-delta","id":"nested_1","delta":"{\"path\":\"nested/result.txt\",\"content\":\"scoped write\"}"}"#,
                r#"{"type":"tool-call","toolCallId":"nested_1"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;

        let retry = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"nested_2","toolName":"write_file"}"#,
                r#"{"type":"tool-input-delta","id":"nested_2","delta":"{\"path\":\"nested/result.txt\",\"content\":\"scoped write\"}"}"#,
                r#"{"type":"tool-call","toolCallId":"nested_2"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;
        if !retry.contains("NESTED RULE SENTINEL") || !retry.contains("execution deferred") {
            return Err("retry request omitted nested rules or the deferral result".into());
        }

        let final_request = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"scoped write complete"}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        if !final_request.contains("wrote nested/result.txt (12 bytes)") {
            return Err("final request omitted the reissued write result".into());
        }
        Ok(())
    });

    let home = TestDirectory::home("scoped-home");
    let workspace = TestDirectory::new("scoped-workspace");
    fs::create_dir(workspace.path().join("nested")).unwrap();
    fs::write(
        workspace.path().join("nested/AGENTS.md"),
        "NESTED RULE SENTINEL",
    )
    .unwrap();
    let permission_requests = Arc::new(Mutex::new(0usize));
    let review_count = permission_requests.clone();
    let workspace_path = workspace.path().to_path_buf();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .on_receive_request(
            async move |request: RequestPermissionRequest, responder, _connection| {
                *review_count.lock().unwrap() += 1;
                let option = request
                    .options
                    .iter()
                    .find(|option| option.option_id.0.as_ref() == "allow_once")
                    .expect("allow_once option")
                    .option_id
                    .clone();
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(option)),
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("write nested file"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("scoped project-context ACP exchange failed");
    gateway.join().unwrap().unwrap();
    assert_eq!(*permission_requests.lock().unwrap(), 1);
    assert_eq!(
        fs::read_to_string(workspace.path().join("nested/result.txt")).unwrap(),
        "scoped write"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_mode_routes_mutation_through_the_automatic_reviewer() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let first = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"tool-input-start","id":"write_auto","toolName":"write_file"}"#,
                r#"{"type":"tool-input-delta","id":"write_auto","delta":"{\"path\":\"auto.txt\",\"content\":\"reviewed automatically\"}"}"#,
                r#"{"type":"tool-call","toolCallId":"write_auto"}"#,
                r#"{"type":"finish","finishReason":{"unified":"tool-calls"}}"#,
            ],
        )?;
        if !first.contains(r#""name":"write_file""#) {
            return Err("agent request did not advertise write_file".into());
        }

        let review = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"{\"decision\":\"allow\",\"rationale\":\"bounded workspace write\"}"}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        if !review.contains("last-chance safety reviewer")
            || !review.contains(r#""tool_choice":"none""#)
        {
            return Err("automatic reviewer request was not isolated".into());
        }

        let third = serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"reviewed write complete"}"#,
                r#"{"type":"finish","finishReason":{"unified":"stop"}}"#,
            ],
        )?;
        if !third.contains("wrote auto.txt (22 bytes)") {
            return Err("final agent request omitted reviewed tool output".into());
        }
        Ok(())
    });

    let home = TestDirectory::home("auto-home");
    let workspace = TestDirectory::new("auto-workspace");
    let permission_requests = Arc::new(Mutex::new(0usize));
    let review_count = permission_requests.clone();
    let workspace_path = workspace.path().to_path_buf();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .on_receive_request(
            async move |_request: RequestPermissionRequest, responder, _connection| {
                *review_count.lock().unwrap() += 1;
                responder.respond(RequestPermissionResponse::new(
                    RequestPermissionOutcome::Cancelled,
                ))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            connection
                .send_request(SetSessionModeRequest::new(
                    session.session_id.clone(),
                    "code",
                ))
                .block_task()
                .await?;
            let response = connection
                .send_request(PromptRequest::new(
                    session.session_id,
                    vec![ContentBlock::Text(TextContent::new("write with review"))],
                ))
                .block_task()
                .await?;
            assert_eq!(response.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("automatic review ACP exchange failed");
    gateway.join().unwrap().unwrap();
    assert_eq!(*permission_requests.lock().unwrap(), 0);
    assert_eq!(
        fs::read_to_string(workspace.path().join("auto.txt")).unwrap(),
        "reviewed automatically"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn session_modes_and_model_preference_round_trip_through_stdio() {
    let home = TestDirectory::home("controls-home");
    let workspace = TestDirectory::new("controls-workspace");
    let saved_id = Arc::new(Mutex::new(None::<SessionId>));
    let captured_id = saved_id.clone();
    let workspace_path = workspace.path().to_path_buf();
    let first_agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string()),
    );

    Client
        .builder()
        .connect_with(
            first_agent,
            move |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let created = connection
                    .send_request(NewSessionRequest::new(workspace_path.clone()))
                    .block_task()
                    .await?;
                assert_eq!(
                    created.modes.as_ref().unwrap().current_mode_id.0.as_ref(),
                    "ask"
                );
                assert_config_value(
                    created.config_options.as_ref().unwrap(),
                    "model",
                    "codex/gpt-5.6-sol",
                );

                connection
                    .send_request(SetSessionModeRequest::new(
                        created.session_id.clone(),
                        "code",
                    ))
                    .block_task()
                    .await?;
                let changed = connection
                    .send_request(SetSessionConfigOptionRequest::new(
                        created.session_id.clone(),
                        "model",
                        SessionConfigOptionValue::value_id("codex/gpt-5.4"),
                    ))
                    .block_task()
                    .await?;
                assert_config_value(&changed.config_options, "model", "codex/gpt-5.4");
                assert_config_value(&changed.config_options, "mode", "code");
                let reloaded = connection
                    .send_request(LoadSessionRequest::new(
                        created.session_id.clone(),
                        workspace_path,
                    ))
                    .block_task()
                    .await?;
                assert_eq!(
                    reloaded.modes.as_ref().unwrap().current_mode_id.0.as_ref(),
                    "ask"
                );
                assert_config_value(reloaded.config_options.as_ref().unwrap(), "mode", "ask");
                connection
                    .send_request(CloseSessionRequest::new(created.session_id.clone()))
                    .block_task()
                    .await?;
                *captured_id.lock().unwrap() = Some(created.session_id);
                Ok(())
            },
        )
        .await
        .expect("first ACP controls exchange failed");

    let session_id = saved_id.lock().unwrap().clone().unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let second_agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string()),
    );
    Client
        .builder()
        .connect_with(
            second_agent,
            move |connection: ConnectionTo<Agent>| async move {
                connection
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let loaded = connection
                    .send_request(LoadSessionRequest::new(session_id, workspace_path))
                    .block_task()
                    .await?;
                assert_config_value(
                    loaded.config_options.as_ref().unwrap(),
                    "model",
                    "codex/gpt-5.4",
                );
                assert_eq!(
                    loaded.modes.as_ref().unwrap().current_mode_id.0.as_ref(),
                    "ask"
                );
                Ok(())
            },
        )
        .await
        .expect("second ACP controls exchange failed");
}

fn assert_config_value(
    options: &[agent_client_protocol::schema::v1::SessionConfigOption],
    id: &str,
    expected: &str,
) {
    let option = options
        .iter()
        .find(|option| option.id.0.as_ref() == id)
        .expect("config option");
    let json = serde_json::to_value(option).unwrap();
    assert_eq!(json["currentValue"], expected);
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_interrupts_blocking_gateway_and_close_waits_for_prompt_cleanup() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let _request = read_http_request(&mut stream)?;
        std::thread::sleep(Duration::from_millis(1200));
        let payload = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"too late\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_late\",\"output\":[],\"usage\":{}}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(response.as_bytes());
        Ok(())
    });

    let home = TestDirectory::home("cancel-home");
    let workspace = TestDirectory::new("cancel-workspace");
    let workspace_path = workspace.path().to_path_buf();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let prompt = connection.send_request(PromptRequest::new(
                session.session_id.clone(),
                vec![ContentBlock::Text(TextContent::new("wait forever"))],
            ));
            tokio::time::sleep(Duration::from_millis(100)).await;
            let started = Instant::now();
            connection.send_notification(CancelNotification::new(session.session_id.clone()))?;
            let response = prompt.block_task().await?;
            assert_eq!(response.stop_reason, StopReason::Cancelled);
            assert!(started.elapsed() < Duration::from_millis(800));
            connection
                .send_request(CloseSessionRequest::new(session.session_id))
                .block_task()
                .await?;
            Ok(())
        })
        .await
        .expect("cancel ACP exchange failed");
    gateway.join().unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn gateway_failure_keeps_the_staged_user_prompt_durable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        serve_gateway_response(
            &listener,
            &[
                r#"{"type":"text-delta","delta":"partial answer"}"#,
                r#"{"type":"error","error":{"message":"provider unavailable"}}"#,
                r#"{"type":"finish","finishReason":{"unified":"error"}}"#,
            ],
        )?;
        Ok(())
    });

    let home = TestDirectory::home("failure-home");
    let workspace = TestDirectory::new("failure-workspace");
    let saved_id = Arc::new(Mutex::new(None::<SessionId>));
    let captured_id = saved_id.clone();
    let workspace_path = workspace.path().to_path_buf();
    let agent = AcpAgent::new(
        AcpAgentConfig::new(env!("CARGO_BIN_EXE_fx-acp"))
            .env("HOME", home.path().display().to_string())
            .env("FX_CODEX_BASE_URL", format!("http://{address}")),
    );
    Client
        .builder()
        .connect_with(agent, move |connection: ConnectionTo<Agent>| async move {
            connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = connection
                .send_request(NewSessionRequest::new(workspace_path))
                .block_task()
                .await?;
            let result = connection
                .send_request(PromptRequest::new(
                    session.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("keep this prompt"))],
                ))
                .block_task()
                .await;
            assert!(result.is_err());
            *captured_id.lock().unwrap() = Some(session.session_id);
            Ok(())
        })
        .await
        .expect("gateway failure ACP exchange failed");
    gateway.join().unwrap().unwrap();

    let session_id = saved_id.lock().unwrap().clone().unwrap().0.to_string();
    let store = EventLogSessionStore::new(home.path().join(".fx/sessions"));
    let loaded = store
        .load(
            SessionTarget::Id(session_id),
            &workspace.path().display().to_string(),
        )
        .await
        .unwrap();
    let user = &loaded.history[loaded.history.len() - 2];
    assert_eq!(user.role, Role::User);
    assert_eq!(user.content.as_deref(), Some("keep this prompt"));
    let partial = loaded.history.last().unwrap();
    assert_eq!(partial.role, Role::Assistant);
    assert_eq!(partial.content.as_deref(), Some("partial answer"));
}

#[test]
fn stdio_disconnect_cancels_active_prompt_and_releases_process() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (request_started, request_observed) = std::sync::mpsc::sync_channel(1);
    let gateway = std::thread::spawn(move || -> Result<(), String> {
        let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|error| error.to_string())?;
        let _request = read_http_request(&mut stream)?;
        request_started
            .send(())
            .map_err(|error| error.to_string())?;
        std::thread::sleep(Duration::from_millis(1200));
        let payload = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"too late\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_late\",\"output\":[],\"usage\":{}}}\n\n"
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
        let _ = stream.write_all(response.as_bytes());
        Ok(())
    });

    let home = TestDirectory::home("disconnect-home");
    let workspace = TestDirectory::new("disconnect-workspace");
    let mut child = Command::new(env!("CARGO_BIN_EXE_fx-acp"))
        .env("HOME", home.path())
        .env("FX_CODEX_BASE_URL", format!("http://{address}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send_json_line(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {"protocolVersion": 1, "clientCapabilities": {}}
        }),
    );
    assert!(read_json_line(&mut stdout)["result"].is_object());
    send_json_line(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": {"cwd": workspace.path(), "mcpServers": []}
        }),
    );
    let created = read_json_line(&mut stdout);
    let session_id = created["result"]["sessionId"].as_str().unwrap();
    send_json_line(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "wait forever"}]
            }
        }),
    );
    request_observed
        .recv_timeout(Duration::from_secs(3))
        .expect("gateway request did not start");

    let started = Instant::now();
    drop(stdin);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        if started.elapsed() >= Duration::from_millis(800) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("ACP process did not release an active prompt after stdio disconnect");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    gateway.join().unwrap().unwrap();
}

fn send_json_line(writer: &mut impl Write, value: &serde_json::Value) {
    serde_json::to_writer(&mut *writer, value).unwrap();
    writer.write_all(b"\n").unwrap();
    writer.flush().unwrap();
}

fn read_json_line(reader: &mut impl BufRead) -> serde_json::Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn serve_gateway_response(listener: &TcpListener, events: &[&str]) -> Result<String, String> {
    serve_gateway_response_with_headers(listener, events).map(|(_, body)| body)
}

fn serve_gateway_response_with_headers(
    listener: &TcpListener,
    events: &[&str],
) -> Result<(String, String), String> {
    let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let request = read_http_request_with_headers(&mut stream)?;
    let events = codex_events(events)?;
    let payload = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| error.to_string())?;
    Ok(request)
}

fn write_gateway_events(stream: &mut TcpStream, events: &[&str]) -> Result<(), String> {
    let events = codex_events(events)?;
    let payload = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| error.to_string())
}

/// Keeps scenario fixtures compact while the server still emits the exact
/// Codex Responses event protocol consumed by the production adapter.
fn codex_events(events: &[&str]) -> Result<Vec<String>, String> {
    let mut translated = Vec::new();
    let mut functions = BTreeMap::<String, (String, String)>::new();
    for event in events {
        let value: serde_json::Value =
            serde_json::from_str(event).map_err(|error| error.to_string())?;
        let kind = value["type"].as_str().unwrap_or_default();
        if kind.starts_with("response.") {
            translated.push(value.to_string());
            continue;
        }
        match kind {
            "text-delta" => translated.push(
                json!({"type": "response.output_text.delta", "delta": value["delta"]}).to_string(),
            ),
            "reasoning-delta" => translated.push(
                json!({"type": "response.reasoning_summary_text.delta", "delta": value["delta"]})
                    .to_string(),
            ),
            "tool-input-start" => {
                let id = value["id"].as_str().ok_or("tool start omitted id")?;
                let name = value["toolName"]
                    .as_str()
                    .ok_or("tool start omitted name")?;
                functions.insert(id.into(), (name.into(), String::new()));
                translated.push(
                    json!({
                        "type": "response.output_item.added",
                        "item": {
                            "type": "function_call",
                            "id": format!("fc_{id}"),
                            "call_id": id,
                            "name": name,
                            "arguments": ""
                        }
                    })
                    .to_string(),
                );
            }
            "tool-input-delta" => {
                let id = value["id"].as_str().ok_or("tool delta omitted id")?;
                let delta = value["delta"].as_str().unwrap_or_default();
                let function = functions.get_mut(id).ok_or("tool delta was unmatched")?;
                function.1.push_str(delta);
                translated.push(
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": format!("fc_{id}"),
                        "delta": delta
                    })
                    .to_string(),
                );
            }
            "tool-call" if value["providerExecuted"].as_bool().unwrap_or(false) => {
                let id = value["toolCallId"]
                    .as_str()
                    .ok_or("provider tool omitted id")?;
                translated.push(
                    json!({
                        "type": "response.output_item.added",
                        "item": {"type": "web_search_call", "id": id}
                    })
                    .to_string(),
                );
            }
            "tool-call" => {
                let id = value["toolCallId"].as_str().ok_or("tool call omitted id")?;
                let (name, streamed) = functions
                    .remove(id)
                    .ok_or_else(|| format!("tool call `{id}` was unmatched"))?;
                let arguments = value
                    .get("input")
                    .filter(|input| !input.is_null())
                    .map(serde_json::Value::to_string)
                    .unwrap_or(streamed);
                translated.push(
                    json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "function_call",
                            "id": format!("fc_{id}"),
                            "call_id": id,
                            "name": name,
                            "arguments": arguments
                        }
                    })
                    .to_string(),
                );
            }
            "tool-result" => {
                let annotations = value["result"]["results"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|source| {
                        Some(json!({
                            "type": "url_citation",
                            "url": source.get("url")?.as_str()?,
                            "title": source.get("title").and_then(serde_json::Value::as_str).unwrap_or("Source")
                        }))
                    })
                    .collect::<Vec<_>>();
                translated.push(
                    json!({
                        "type": "response.output_item.done",
                        "item": {
                            "type": "message",
                            "id": "msg_search",
                            "content": [{
                                "type": "output_text",
                                "text": value["result"].get("answer").and_then(serde_json::Value::as_str).unwrap_or(""),
                                "annotations": annotations
                            }]
                        }
                    })
                    .to_string(),
                );
            }
            "finish" => {
                let reason = value
                    .pointer("/finishReason/unified")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("stop");
                let usage = json!({
                    "input_tokens": value.pointer("/usage/inputTokens/total").and_then(serde_json::Value::as_u64),
                    "output_tokens": value.pointer("/usage/outputTokens/total").and_then(serde_json::Value::as_u64),
                });
                if reason == "error" {
                    translated.push(
                        json!({
                            "type": "response.failed",
                            "response": {"id": "resp_test", "error": {"message": "provider failed"}}
                        })
                        .to_string(),
                    );
                } else {
                    translated.push(
                        json!({
                            "type": if reason == "length" { "response.incomplete" } else { "response.completed" },
                            "response": {
                                "id": "resp_test",
                                "output": [],
                                "usage": usage,
                                "incomplete_details": if reason == "length" { json!({"reason": "max_output_tokens"}) } else { serde_json::Value::Null }
                            }
                        })
                        .to_string(),
                    );
                }
            }
            "error" => translated.push(value.to_string()),
            other => return Err(format!("unsupported fixture event `{other}`")),
        }
    }
    Ok(translated)
}

fn extract_subagent_child_id(body: &str) -> Option<String> {
    let request: serde_json::Value = serde_json::from_str(body).ok()?;
    fn find(value: &serde_json::Value) -> Option<String> {
        if let Some(id) = value.get("child_id").and_then(serde_json::Value::as_str) {
            return Some(id.to_owned());
        }
        match value {
            serde_json::Value::Array(values) => values.iter().find_map(find),
            serde_json::Value::Object(values) => values.values().find_map(find),
            serde_json::Value::String(value) => {
                serde_json::from_str(value).ok().as_ref().and_then(find)
            }
            _ => None,
        }
    }
    find(&request)
}

fn read_http_request(stream: &mut TcpStream) -> Result<String, String> {
    read_http_request_with_headers(stream).map(|(_, body)| body)
}

fn read_http_headers(stream: &mut TcpStream) -> Result<String, String> {
    let mut bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("request ended before headers".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            return String::from_utf8(bytes[..position + 4].to_vec())
                .map_err(|error| error.to_string());
        }
    }
}

fn read_http_request_with_headers(stream: &mut TcpStream) -> Result<(String, String), String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("gateway request ended before headers".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .ok_or_else(|| "gateway request omitted content-length".to_owned())?;
    while bytes.len() < header_end + content_length {
        let mut chunk = [0u8; 4096];
        let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("gateway request body ended early".into());
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let body = String::from_utf8(bytes[header_end..header_end + content_length].to_vec())
        .map_err(|error| error.to_string())?;
    Ok((headers, body))
}
