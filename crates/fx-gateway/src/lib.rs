//! Minimal OpenAI Responses SSE transport used by provider implementations.
//!
//! This crate deliberately keeps a blocking pooled HTTP client outside the
//! executor-neutral core. The ACP composition root runs it on a worker thread.

use std::collections::{BTreeMap, HashMap};
use std::io::Read;

use fx_core::{
    BoxFuture, ChatMessage, FinishReason, Gateway, GatewayError, GatewayEvent, GatewayEventSink,
    GatewayRequest, GatewayResponse, Role, ToolAdvertisementKind, ToolArgumentIntegrity, ToolCall,
    ToolExecutionProvenance, Usage,
};
use serde_json::{Map, Value, json};
use stream_rs::sse::{SseEvent, SseParser};
use zeroize::Zeroizing;

pub const DEFAULT_CODEX_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";
pub const CODEX_WEB_SEARCH_TOOL_ID: &str = "codex.web_search";

const MAX_EVENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// Accepts a loopback base for deterministic tests without allowing bearer
/// credentials to be redirected to arbitrary HTTP servers.
pub fn codex_endpoint_from_base(base: Option<&str>) -> String {
    let Some(base) = base else {
        return DEFAULT_CODEX_ENDPOINT.into();
    };
    let normalized = base.trim_end_matches('/');
    let loopback = normalized.starts_with("http://127.0.0.1:")
        || normalized.starts_with("http://localhost:")
        || normalized == "http://localhost"
        || normalized == "http://127.0.0.1";
    if loopback {
        format!("{normalized}/codex/responses")
    } else {
        DEFAULT_CODEX_ENDPOINT.into()
    }
}

#[derive(Clone)]
pub struct CodexGatewayConfig {
    pub endpoint: String,
    pub model: String,
    access_token: Zeroizing<String>,
    pub account_id: String,
    pub session_id: Option<String>,
    pub originator: String,
}

impl CodexGatewayConfig {
    pub fn new(
        model: impl Into<String>,
        access_token: impl Into<String>,
        account_id: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: DEFAULT_CODEX_ENDPOINT.into(),
            model: model.into(),
            access_token: Zeroizing::new(access_token.into()),
            account_id: account_id.into(),
            session_id: None,
            originator: "fxrs".into(),
        }
    }
}

impl std::fmt::Debug for CodexGatewayConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexGatewayConfig")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("access_token", &"[redacted]")
            .field("account_id", &self.account_id)
            .field("session_id", &self.session_id)
            .field("originator", &self.originator)
            .finish()
    }
}

pub struct CodexGateway {
    config: CodexGatewayConfig,
    http: ureq::Agent,
}

impl CodexGateway {
    pub fn new(config: CodexGatewayConfig) -> Self {
        Self {
            config,
            http: ureq::Agent::new_with_defaults(),
        }
    }
}

impl Gateway for CodexGateway {
    fn complete<'a>(
        &'a self,
        request: GatewayRequest,
        events: &'a mut dyn GatewayEventSink,
    ) -> BoxFuture<'a, Result<GatewayResponse, GatewayError>> {
        Box::pin(async move {
            if self.config.access_token.is_empty() || self.config.account_id.is_empty() {
                return Err(GatewayError::Authentication);
            }
            let body = build_codex_request_body(&self.config.model, &request)?;
            let authorization =
                Zeroizing::new(format!("Bearer {}", self.config.access_token.as_str()));
            let mut builder = self
                .http
                .post(&self.config.endpoint)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .header("authorization", authorization.as_str())
                .header("chatgpt-account-id", &self.config.account_id)
                .header("originator", &self.config.originator)
                .header("openai-beta", "responses=experimental")
                .header("user-agent", concat!("fxrs/", env!("CARGO_PKG_VERSION")));
            if let Some(session_id) = self
                .config
                .session_id
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                builder = builder.header("session_id", session_id);
            }
            let mut response = builder.send(body.as_slice()).map_err(map_send_error)?;
            consume_codex_sse(&mut response.body_mut().as_reader(), events)
        })
    }
}

pub fn build_codex_request_body(
    model: &str,
    request: &GatewayRequest,
) -> Result<Vec<u8>, GatewayError> {
    let instructions = request
        .messages
        .iter()
        .filter(|message| message.role == Role::System)
        .filter_map(|message| message.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut input = Vec::new();
    for (index, message) in request.messages.iter().enumerate() {
        project_message(message, index, &mut input)?;
    }
    let tools = request
        .tools
        .iter()
        .map(project_tool)
        .collect::<Result<Vec<_>, _>>()?;
    let mut root = Map::new();
    root.insert("model".into(), Value::String(model.into()));
    root.insert("store".into(), Value::Bool(false));
    root.insert("stream".into(), Value::Bool(true));
    root.insert("instructions".into(), Value::String(instructions));
    root.insert("input".into(), Value::Array(input));
    root.insert("tools".into(), Value::Array(tools));
    root.insert(
        "tool_choice".into(),
        Value::String(
            match request.tool_choice {
                fx_core::ToolChoice::Auto => "auto",
                fx_core::ToolChoice::None => "none",
                fx_core::ToolChoice::Required => "required",
            }
            .into(),
        ),
    );
    root.insert("parallel_tool_calls".into(), Value::Bool(true));
    root.insert("include".into(), json!(["reasoning.encrypted_content"]));
    root.insert(
        "reasoning".into(),
        json!({"effort": "medium", "summary": "auto"}),
    );
    if let Some(limit) = request.max_output_tokens {
        root.insert("max_output_tokens".into(), Value::from(limit));
    }
    serde_json::to_vec(&root)
        .map_err(|error| GatewayError::InvalidResponse(format!("request serialization: {error}")))
}

fn project_message(
    message: &ChatMessage,
    index: usize,
    input: &mut Vec<Value>,
) -> Result<(), GatewayError> {
    match message.role {
        Role::System => {}
        Role::User => input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": message.content.as_deref().unwrap_or("")
            }]
        })),
        Role::Assistant => {
            if let Some(content) = message.content.as_deref().filter(|value| !value.is_empty()) {
                input.push(json!({
                    "type": "message",
                    "id": format!("msg_fx_{index}"),
                    "role": "assistant",
                    "status": "completed",
                    "content": [{"type": "output_text", "text": content, "annotations": []}]
                }));
            }
            for call in &message.tool_calls {
                if serde_json::from_str::<Value>(&call.arguments_json).is_err() {
                    return Err(GatewayError::InvalidResponse(format!(
                        "assistant tool call `{}` has invalid arguments",
                        call.id
                    )));
                }
                let (call_id, item_id) = split_tool_call_id(&call.id);
                let mut item = Map::new();
                item.insert("type".into(), Value::String("function_call".into()));
                item.insert("call_id".into(), Value::String(call_id.into()));
                item.insert("name".into(), Value::String(call.name.clone()));
                item.insert(
                    "arguments".into(),
                    Value::String(call.arguments_json.clone()),
                );
                if let Some(item_id) = item_id {
                    item.insert("id".into(), Value::String(item_id.into()));
                }
                input.push(Value::Object(item));
            }
        }
        Role::Tool => {
            let (call_id, _) = split_tool_call_id(message.tool_call_id.as_deref().unwrap_or(""));
            if call_id.is_empty() {
                return Err(GatewayError::InvalidResponse(
                    "tool result is missing its call id".into(),
                ));
            }
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": message.content.as_deref().unwrap_or("")
            }));
        }
    }
    Ok(())
}

fn project_tool(tool: &fx_core::ToolAdvertisement) -> Result<Value, GatewayError> {
    match &tool.kind {
        ToolAdvertisementKind::Function => Ok(json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.input_schema,
            "strict": false
        })),
        ToolAdvertisementKind::Provider { id, arguments } if id == CODEX_WEB_SEARCH_TOOL_ID => {
            let mut projected = Map::new();
            projected.insert("type".into(), Value::String("web_search".into()));
            projected.insert("search_context_size".into(), Value::String("medium".into()));
            if let Some(domains) = arguments.get("allowed_domains").and_then(Value::as_array) {
                projected.insert("filters".into(), json!({"allowed_domains": domains}));
            }
            Ok(Value::Object(projected))
        }
        ToolAdvertisementKind::Provider { id, .. } => Err(GatewayError::InvalidResponse(format!(
            "Codex does not support provider tool `{id}`"
        ))),
    }
}

fn split_tool_call_id(value: &str) -> (&str, Option<&str>) {
    value
        .split_once('|')
        .map_or((value, None), |(call_id, item_id)| {
            (call_id, (!item_id.is_empty()).then_some(item_id))
        })
}

pub fn consume_codex_sse(
    reader: &mut dyn Read,
    events: &mut dyn GatewayEventSink,
) -> Result<GatewayResponse, GatewayError> {
    let mut parser = SseParser::new();
    let mut parsed_events = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total_bytes = 0usize;
    let mut state = StreamState::default();
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| GatewayError::PossiblySent)?;
        if count == 0 {
            parser.finish(&mut parsed_events);
        } else {
            total_bytes = total_bytes.saturating_add(count);
            if total_bytes > MAX_STREAM_BYTES {
                return Err(GatewayError::InvalidResponse(
                    "Codex stream exceeded 64 MiB".into(),
                ));
            }
            parser.feed(&buffer[..count], &mut parsed_events);
        }
        for event in parsed_events.drain(..) {
            if event.data.len() > MAX_EVENT_BYTES {
                return Err(GatewayError::InvalidResponse(
                    "Codex event exceeded 4 MiB".into(),
                ));
            }
            if process_event(&mut state, event, events)? {
                return state.finish();
            }
        }
        if count == 0 {
            return Err(GatewayError::InvalidResponse(
                "Codex stream ended before a terminal response event".into(),
            ));
        }
    }
}

#[derive(Default)]
struct PendingFunction {
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamState {
    content: String,
    text_was_streamed: bool,
    tool_calls: Vec<ToolCall>,
    pending_functions: HashMap<String, PendingFunction>,
    web_search_items: Vec<String>,
    sources: BTreeMap<String, String>,
    generation_id: Option<String>,
    finish_reason: Option<FinishReason>,
    usage: Usage,
    provider_error: Option<String>,
}

impl StreamState {
    fn finish(mut self) -> Result<GatewayResponse, GatewayError> {
        if let Some(error) = self.provider_error {
            return Err(GatewayError::Rejected(error));
        }
        if !self.web_search_items.is_empty() {
            let results = self
                .sources
                .into_iter()
                .map(|(url, title)| json!({"title": title, "url": url}))
                .collect::<Vec<_>>();
            let id = self.web_search_items.remove(0);
            self.tool_calls.push(ToolCall {
                id,
                name: "web_search".into(),
                arguments_json: "{}".into(),
                argument_integrity: ToolArgumentIntegrity::Valid,
                provisional_id: None,
                provider_result: Some(json!({"results": results}).to_string()),
                provenance: ToolExecutionProvenance::Provider,
            });
        }
        Ok(GatewayResponse {
            content: (!self.content.is_empty()).then_some(self.content),
            tool_calls: self.tool_calls,
            generation_id: self.generation_id,
            finish_reason: self.finish_reason,
            usage: self.usage,
            delivery_ambiguous: false,
        })
    }
}

fn process_event(
    state: &mut StreamState,
    event: SseEvent,
    events: &mut dyn GatewayEventSink,
) -> Result<bool, GatewayError> {
    if event.data == "[DONE]" {
        return Ok(false);
    }
    let root: Value = serde_json::from_str(&event.data)
        .map_err(|error| GatewayError::InvalidResponse(format!("invalid SSE JSON: {error}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| GatewayError::InvalidResponse("SSE event must be an object".into()))?;
    let event_type = object.get("type").and_then(Value::as_str).unwrap_or("");
    match event_type {
        "response.created" | "response.in_progress" => {
            capture_response_id(state, object);
        }
        "response.output_text.delta" => {
            let delta = object.get("delta").and_then(Value::as_str).unwrap_or("");
            state.content.push_str(delta);
            state.text_was_streamed = true;
            if !delta.is_empty() {
                events.emit(GatewayEvent::ContentDelta(delta.into()));
            }
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = object.get("delta").and_then(Value::as_str) {
                events.emit(GatewayEvent::ReasoningDelta(delta.into()));
            }
        }
        "response.output_item.added" => capture_output_item_added(state, object, events)?,
        "response.function_call_arguments.delta" => {
            let key = output_item_key(object)?;
            let delta = object.get("delta").and_then(Value::as_str).unwrap_or("");
            if let Some(function) = state.pending_functions.get_mut(key) {
                function.arguments.push_str(delta);
            }
        }
        "response.output_item.done" => capture_output_item_done(state, object)?,
        "response.completed" => {
            capture_terminal_response(state, object, FinishReason::Stop)?;
            return Ok(true);
        }
        "response.incomplete" => {
            capture_terminal_response(state, object, FinishReason::Length)?;
            return Ok(true);
        }
        "response.failed" => {
            capture_response_id(state, object);
            state.provider_error = Some(response_error_message(object));
            return Ok(true);
        }
        "error" => {
            state.provider_error = Some(response_error_message(object));
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

fn capture_output_item_added(
    state: &mut StreamState,
    object: &Map<String, Value>,
    events: &mut dyn GatewayEventSink,
) -> Result<(), GatewayError> {
    let item = object
        .get("item")
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::InvalidResponse("output item is missing".into()))?;
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "function_call" => {
            let item_id = required_string(item, "id")?.to_owned();
            let call_id = required_string(item, "call_id")?.to_owned();
            let name = required_string(item, "name")?.to_owned();
            let stable_id = stable_tool_id(&call_id, &item_id);
            events.emit(GatewayEvent::ToolStarted {
                id: stable_id,
                name: name.clone(),
            });
            state.pending_functions.insert(
                item_id.clone(),
                PendingFunction {
                    call_id,
                    name,
                    arguments: item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                },
            );
        }
        "web_search_call" => {
            let item_id = required_string(item, "id")?.to_owned();
            if !state.web_search_items.contains(&item_id) {
                state.web_search_items.push(item_id);
            }
        }
        _ => capture_message_item(state, item),
    }
    Ok(())
}

fn capture_output_item_done(
    state: &mut StreamState,
    object: &Map<String, Value>,
) -> Result<(), GatewayError> {
    let item = object
        .get("item")
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::InvalidResponse("completed output item is missing".into()))?;
    match item.get("type").and_then(Value::as_str).unwrap_or("") {
        "function_call" => finish_function(state, item),
        "web_search_call" => {
            let item_id = required_string(item, "id")?.to_owned();
            if !state.web_search_items.contains(&item_id) {
                state.web_search_items.push(item_id);
            }
            Ok(())
        }
        "message" => {
            capture_message_item(state, item);
            Ok(())
        }
        _ => Ok(()),
    }
}

fn finish_function(state: &mut StreamState, item: &Map<String, Value>) -> Result<(), GatewayError> {
    let item_id = required_string(item, "id")?;
    let pending = state.pending_functions.remove(item_id);
    let call_id = item
        .get("call_id")
        .and_then(Value::as_str)
        .or_else(|| pending.as_ref().map(|value| value.call_id.as_str()))
        .ok_or_else(|| GatewayError::InvalidResponse("function call id is missing".into()))?
        .to_owned();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| pending.as_ref().map(|value| value.name.as_str()))
        .ok_or_else(|| GatewayError::InvalidResponse("function name is missing".into()))?
        .to_owned();
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .or_else(|| pending.map(|value| value.arguments))
        .unwrap_or_else(|| "{}".into());
    let id = stable_tool_id(&call_id, item_id);
    if state.tool_calls.iter().any(|call| call.id == id) {
        return Err(GatewayError::InvalidResponse(format!(
            "duplicate function call `{id}`"
        )));
    }
    let argument_integrity = if serde_json::from_str::<Value>(&arguments).is_ok() {
        ToolArgumentIntegrity::Valid
    } else {
        ToolArgumentIntegrity::MalformedJson
    };
    state.tool_calls.push(ToolCall {
        id,
        name,
        arguments_json: arguments,
        argument_integrity,
        provisional_id: None,
        provider_result: None,
        provenance: ToolExecutionProvenance::FxLocal,
    });
    Ok(())
}

fn capture_message_item(state: &mut StreamState, item: &Map<String, Value>) {
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return;
    };
    let mut final_text = String::new();
    for part in content {
        let Some(part) = part.as_object() else {
            continue;
        };
        if matches!(
            part.get("type").and_then(Value::as_str),
            Some("output_text")
        ) {
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                final_text.push_str(text);
            }
            if let Some(annotations) = part.get("annotations").and_then(Value::as_array) {
                for annotation in annotations {
                    let Some(annotation) = annotation.as_object() else {
                        continue;
                    };
                    if annotation.get("type").and_then(Value::as_str) == Some("url_citation")
                        && let Some(url) = annotation.get("url").and_then(Value::as_str)
                    {
                        let title = annotation
                            .get("title")
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .unwrap_or(url);
                        state
                            .sources
                            .entry(url.into())
                            .or_insert_with(|| title.into());
                    }
                }
            }
        }
    }
    if !state.text_was_streamed && !final_text.is_empty() {
        state.content = final_text;
    }
}

fn capture_terminal_response(
    state: &mut StreamState,
    object: &Map<String, Value>,
    fallback: FinishReason,
) -> Result<(), GatewayError> {
    capture_response_id(state, object);
    let response = object
        .get("response")
        .and_then(Value::as_object)
        .ok_or_else(|| GatewayError::InvalidResponse("terminal response is missing".into()))?;
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output.iter().filter_map(Value::as_object) {
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "function_call" => {
                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    if !item_id.is_empty()
                        && !state.tool_calls.iter().any(|call| {
                            let (call_id, stored_item_id) = split_tool_call_id(&call.id);
                            stored_item_id == Some(item_id) || call_id == item_id
                        })
                    {
                        finish_function(state, item)?;
                    }
                }
                "web_search_call" => {
                    if let Some(id) = item.get("id").and_then(Value::as_str)
                        && !state.web_search_items.iter().any(|existing| existing == id)
                    {
                        state.web_search_items.push(id.into());
                    }
                }
                "message" => capture_message_item(state, item),
                _ => {}
            }
        }
    }
    state.usage = parse_usage(response);
    state.finish_reason = Some(
        if !state
            .tool_calls
            .iter()
            .filter(|call| call.provenance == ToolExecutionProvenance::FxLocal)
            .collect::<Vec<_>>()
            .is_empty()
        {
            FinishReason::ToolCalls
        } else if fallback == FinishReason::Length {
            parse_incomplete_reason(response)
        } else {
            fallback
        },
    );
    Ok(())
}

fn capture_response_id(state: &mut StreamState, object: &Map<String, Value>) {
    if let Some(id) = object
        .get("response")
        .and_then(Value::as_object)
        .and_then(|response| response.get("id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        state.generation_id.get_or_insert_with(|| id.into());
    }
}

fn parse_usage(response: &Map<String, Value>) -> Usage {
    let usage = response.get("usage").and_then(Value::as_object);
    Usage {
        input_tokens: usage
            .and_then(|usage| usage.get("input_tokens"))
            .and_then(Value::as_u64),
        output_tokens: usage
            .and_then(|usage| usage.get("output_tokens"))
            .and_then(Value::as_u64),
    }
}

fn parse_incomplete_reason(response: &Map<String, Value>) -> FinishReason {
    match response
        .get("incomplete_details")
        .and_then(Value::as_object)
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
    {
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Length,
    }
}

fn output_item_key(object: &Map<String, Value>) -> Result<&str, GatewayError> {
    object
        .get("item_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::InvalidResponse("function delta item id is missing".into()))
}

fn stable_tool_id(call_id: &str, item_id: &str) -> String {
    if item_id.is_empty() || call_id == item_id {
        call_id.into()
    } else {
        format!("{call_id}|{item_id}")
    }
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, GatewayError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| GatewayError::InvalidResponse(format!("SSE field `{field}` is missing")))
}

fn response_error_message(object: &Map<String, Value>) -> String {
    object
        .get("response")
        .and_then(Value::as_object)
        .and_then(|response| response.get("error"))
        .or_else(|| object.get("error"))
        .and_then(|error| {
            error
                .as_object()
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| error.as_str())
        })
        .or_else(|| object.get("message").and_then(Value::as_str))
        .unwrap_or("Codex reported an error")
        .into()
}

fn map_send_error(error: ureq::Error) -> GatewayError {
    match error {
        ureq::Error::StatusCode(401 | 403) => GatewayError::Authentication,
        ureq::Error::StatusCode(status) => GatewayError::Rejected(format!("HTTP status {status}")),
        ureq::Error::Http(_)
        | ureq::Error::BadUri(_)
        | ureq::Error::HostNotFound
        | ureq::Error::InvalidProxyUrl
        | ureq::Error::ConnectionFailed
        | ureq::Error::BodyExceedsLimit(_)
        | ureq::Error::Tls(_)
        | ureq::Error::ConnectProxyFailed(_)
        | ureq::Error::TlsRequired
        | ureq::Error::RequireHttpsOnly(_) => GatewayError::DefinitelyUnsent,
        ureq::Error::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::AddrNotAvailable
            ) =>
        {
            GatewayError::DefinitelyUnsent
        }
        _ => GatewayError::PossiblySent,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use fx_core::{CachePolicy, GatewayEvent, ToolAdvertisement, ToolChoice};

    use super::*;

    #[derive(Default)]
    struct Events(Vec<GatewayEvent>);

    impl GatewayEventSink for Events {
        fn emit(&mut self, event: GatewayEvent) {
            self.0.push(event);
        }
    }

    #[test]
    fn projects_history_functions_and_native_search() {
        let request = GatewayRequest {
            model: "codex/ignored".into(),
            messages: vec![
                ChatMessage::text(Role::System, "system"),
                ChatMessage::text(Role::User, "question"),
                ChatMessage {
                    role: Role::Assistant,
                    content: None,
                    tool_call_id: None,
                    tool_name: None,
                    tool_calls: vec![ToolCall {
                        id: "call_1|fc_1".into(),
                        name: "read_file".into(),
                        arguments_json: "{\"path\":\"README.md\"}".into(),
                        argument_integrity: ToolArgumentIntegrity::Valid,
                        provisional_id: None,
                        provider_result: None,
                        provenance: ToolExecutionProvenance::FxLocal,
                    }],
                    permission_feedback: false,
                    cache_policy: CachePolicy::Default,
                },
                ChatMessage {
                    role: Role::Tool,
                    content: Some("contents".into()),
                    tool_call_id: Some("call_1|fc_1".into()),
                    tool_name: Some("read_file".into()),
                    tool_calls: Vec::new(),
                    permission_feedback: false,
                    cache_policy: CachePolicy::Default,
                },
            ],
            tools: vec![
                ToolAdvertisement::function("read_file", "Read", json!({"type": "object"})),
                ToolAdvertisement::provider(
                    CODEX_WEB_SEARCH_TOOL_ID,
                    "web_search",
                    json!({"allowed_domains": ["example.com"]}),
                ),
            ],
            tool_choice: ToolChoice::Auto,
            max_output_tokens: Some(4096),
        };
        let value: Value =
            serde_json::from_slice(&build_codex_request_body("gpt-test", &request).unwrap())
                .unwrap();
        assert_eq!(value["model"], "gpt-test");
        assert_eq!(value["instructions"], "system");
        assert_eq!(value["input"][1]["id"], "fc_1");
        assert_eq!(value["input"][2]["call_id"], "call_1");
        assert_eq!(value["tools"][0]["parameters"]["type"], "object");
        assert_eq!(
            value["tools"][1]["filters"]["allowed_domains"][0],
            "example.com"
        );
        assert_eq!(value["max_output_tokens"], 4096);
    }

    #[test]
    fn parses_streamed_text_function_usage_and_identity() {
        let stream = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"path\\\":\\\"README.md\\\"}\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
        );
        let mut events = Events::default();
        let response = consume_codex_sse(&mut Cursor::new(stream), &mut events).unwrap();
        assert_eq!(response.content.as_deref(), Some("hello"));
        assert_eq!(response.generation_id.as_deref(), Some("resp_1"));
        assert_eq!(response.finish_reason, Some(FinishReason::ToolCalls));
        assert_eq!(response.usage.input_tokens, Some(10));
        assert_eq!(response.tool_calls[0].id, "call_1|fc_1");
        assert_eq!(
            response.tool_calls[0].arguments_json,
            "{\"path\":\"README.md\"}"
        );
        assert!(
            events
                .0
                .contains(&GatewayEvent::ReasoningDelta("think".into()))
        );
    }

    #[test]
    fn synthesizes_provider_search_result_from_url_citations() {
        let stream = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"web_search_call\",\"id\":\"ws_1\"}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"id\":\"msg_1\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\",\"annotations\":[{\"type\":\"url_citation\",\"url\":\"https://example.com\",\"title\":\"Example\"}]}]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[],\"usage\":{}}}\n\n",
        );
        let response = consume_codex_sse(&mut Cursor::new(stream), &mut Events::default()).unwrap();
        assert_eq!(response.content.as_deref(), Some("answer"));
        let call = &response.tool_calls[0];
        assert_eq!(call.name, "web_search");
        assert_eq!(call.provenance, ToolExecutionProvenance::Provider);
        assert!(
            call.provider_result
                .as_deref()
                .unwrap()
                .contains("example.com")
        );
    }

    #[test]
    fn pre_delivery_errors_remain_the_only_retryable_class() {
        assert!(matches!(
            map_send_error(ureq::Error::HostNotFound),
            GatewayError::DefinitelyUnsent
        ));
        assert!(matches!(
            map_send_error(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "reset"
            ))),
            GatewayError::PossiblySent
        ));
    }
}
