use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::extract::ws::{Message as WebSocketMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::protocol::{
    EventKind, JsonRpcRequest, JsonRpcResponse, MAX_FRAME_BYTES, RequestId, ServerFrame,
    decode_request,
};
use crate::entry::recovery;

#[derive(Clone)]
struct ApiState {
    client: DaemonClient,
    model: String,
    bearer_token: Option<String>,
}

#[derive(Deserialize)]
struct ChatCompletionRequest {
    model: Option<String>,
    messages: Vec<OpenAiMessage>,
    #[serde(default)]
    stream: bool,
}

#[derive(Deserialize)]
struct OpenAiMessage {
    role: String,
    content: Value,
}

#[derive(Deserialize)]
struct WebSocketConnect {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    token: Option<String>,
}

pub async fn run_http_server(
    client: DaemonClient,
    address: SocketAddr,
    model: String,
    bearer_token: Option<String>,
) -> Result<()> {
    let state = ApiState {
        client,
        model,
        bearer_token,
    };
    let app = api_router(state);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("监听本地 API 失败: {address}"))?;
    println!("my-agent 本地 API 正在监听 http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("本地 API 服务异常退出")
}

fn api_router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/ws", get(websocket_upgrade))
        .with_state(state)
}

async fn websocket_upgrade(State(state): State<ApiState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .max_message_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_websocket(socket, state))
}

async fn handle_websocket(mut socket: WebSocket, state: ApiState) {
    let connect = match socket.recv().await {
        Some(Ok(WebSocketMessage::Text(text))) if text.len() <= MAX_FRAME_BYTES => {
            serde_json::from_str::<WebSocketConnect>(&text)
        }
        Some(Ok(_)) => {
            send_websocket_control_error(&mut socket, "第一帧必须是文本 connect 帧").await;
            return;
        }
        Some(Err(error)) => {
            tracing::debug!(%error, "读取 WebSocket connect 帧失败");
            return;
        }
        None => return,
    };
    let connect = match connect {
        Ok(connect) if connect.kind == "connect" => connect,
        Ok(_) | Err(_) => {
            send_websocket_control_error(&mut socket, "第一帧必须是合法 connect 帧").await;
            return;
        }
    };
    if !token_matches(connect.token.as_deref(), state.bearer_token.as_deref()) {
        send_websocket_control_error(&mut socket, "Token 无效或缺失").await;
        return;
    }
    if socket
        .send(WebSocketMessage::Text(
            json!({
                "type": "connected",
                "protocol": "my-agent-jsonrpc",
                "version": 1,
            })
            .to_string()
            .into(),
        ))
        .await
        .is_err()
    {
        return;
    }

    let (mut websocket_writer, mut websocket_reader) = socket.split();
    let (outgoing, mut outgoing_receiver) = mpsc::unbounded_channel::<WebSocketMessage>();
    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_receiver.recv().await {
            if websocket_writer.send(message).await.is_err() {
                break;
            }
        }
    });
    let active_ids = std::sync::Arc::new(Mutex::new(std::collections::HashMap::<
        RequestId,
        RequestId,
    >::new()));
    let mut jobs = JoinSet::new();
    if let Err(error) =
        start_websocket_recovery(&state.client, &active_ids, &outgoing, &mut jobs).await
    {
        let _ = outgoing.send(WebSocketMessage::Text(
            json!({"type": "recovery_error", "error": format!("{error:#}")})
                .to_string()
                .into(),
        ));
    }

    while let Some(message) = websocket_reader.next().await {
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                tracing::debug!(%error, "WebSocket 读取结束");
                break;
            }
        };
        match message {
            WebSocketMessage::Text(text) => {
                let request = match decode_request(text.as_bytes()) {
                    Ok(request) => request,
                    Err(error) => {
                        send_ws_server_frame(
                            &outgoing,
                            ServerFrame::Response(JsonRpcResponse::failure(
                                RequestId::String("protocol".to_owned()),
                                if text.len() > MAX_FRAME_BYTES {
                                    -32002
                                } else {
                                    -32700
                                },
                                error.to_string(),
                            )),
                        );
                        continue;
                    }
                };
                start_websocket_request(&state.client, request, &active_ids, &outgoing, &mut jobs)
                    .await;
            }
            WebSocketMessage::Ping(payload) => {
                let _ = outgoing.send(WebSocketMessage::Pong(payload));
            }
            WebSocketMessage::Close(_) => break,
            WebSocketMessage::Pong(_) | WebSocketMessage::Binary(_) => {}
        }
    }

    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
    drop(outgoing);
    let _ = writer.await;
}

async fn start_websocket_request(
    client: &DaemonClient,
    mut request: JsonRpcRequest,
    active_ids: &std::sync::Arc<Mutex<std::collections::HashMap<RequestId, RequestId>>>,
    outgoing: &mpsc::UnboundedSender<WebSocketMessage>,
    jobs: &mut JoinSet<()>,
) {
    if request.method == "agent.cancel"
        && let Some(target) = request
            .params
            .get("request_id")
            .and_then(|value| serde_json::from_value::<RequestId>(value.clone()).ok())
        && let Some(internal) = active_ids.lock().await.get(&target).cloned()
    {
        request.params["request_id"] = json!(internal);
    }
    let external_id = request.id;
    let is_chat = request.method == "chat.send";
    let stream = match client.request(&request.method, request.params).await {
        Ok(stream) => stream,
        Err(error) => {
            send_ws_server_frame(
                outgoing,
                ServerFrame::Response(JsonRpcResponse::failure(
                    external_id,
                    -32000,
                    format!("{error:#}"),
                )),
            );
            return;
        }
    };
    if is_chat {
        active_ids
            .lock()
            .await
            .insert(external_id.clone(), stream.request_id().clone());
    }
    spawn_websocket_stream(stream, external_id, is_chat, active_ids, outgoing, jobs);
}

fn spawn_websocket_stream(
    mut stream: RpcStream,
    external_id: RequestId,
    is_chat: bool,
    active_ids: &std::sync::Arc<Mutex<std::collections::HashMap<RequestId, RequestId>>>,
    outgoing: &mpsc::UnboundedSender<WebSocketMessage>,
    jobs: &mut JoinSet<()>,
) {
    let task_ids = active_ids.clone();
    let task_outgoing = outgoing.clone();
    jobs.spawn(async move {
        while let Some(mut frame) = stream.next().await {
            let terminal = matches!(frame, ServerFrame::Response(_));
            remap_frame_id(&mut frame, &external_id);
            send_ws_server_frame(&task_outgoing, frame);
            if terminal {
                break;
            }
        }
        if is_chat {
            task_ids.lock().await.remove(&external_id);
        }
    });
}

async fn start_websocket_recovery(
    client: &DaemonClient,
    active_ids: &std::sync::Arc<Mutex<std::collections::HashMap<RequestId, RequestId>>>,
    outgoing: &mpsc::UnboundedSender<WebSocketMessage>,
    jobs: &mut JoinSet<()>,
) -> Result<()> {
    let snapshot = recovery::load_snapshot(client).await?;
    let active_set = snapshot
        .active_requests
        .iter()
        .cloned()
        .collect::<std::collections::HashSet<RequestId>>();
    let _ = outgoing.send(WebSocketMessage::Text(
        json!({"type": "recovery", "snapshot": &snapshot})
            .to_string()
            .into(),
    ));
    for approval in &snapshot.pending_approvals {
        if active_set.contains(&approval.request_id) {
            continue;
        }
        send_ws_server_frame(
            outgoing,
            ServerFrame::Event(crate::daemon::protocol::EventFrame::new(
                RequestId::String("recovery".to_owned()),
                EventKind::ApprovalRequired,
                json!({"approval": approval}),
            )),
        );
    }
    for request_id in snapshot.active_requests {
        let stream = recovery::subscribe(client, &request_id).await?;
        active_ids
            .lock()
            .await
            .insert(request_id.clone(), request_id.clone());
        spawn_websocket_stream(stream, request_id, true, active_ids, outgoing, jobs);
    }
    Ok(())
}

fn remap_frame_id(frame: &mut ServerFrame, external_id: &RequestId) {
    match frame {
        ServerFrame::Event(event) => event.request_id = external_id.clone(),
        ServerFrame::Response(response) => response.id = external_id.clone(),
    }
}

fn send_ws_server_frame(outgoing: &mpsc::UnboundedSender<WebSocketMessage>, frame: ServerFrame) {
    let message = match serde_json::to_string(&frame) {
        Ok(message) if message.len() <= MAX_FRAME_BYTES => message,
        Ok(_) => serde_json::to_string(&ServerFrame::Response(JsonRpcResponse::failure(
            crate::daemon::protocol::server_frame_request_id(&frame).clone(),
            -32002,
            "WebSocket 响应超过帧大小限制",
        )))
        .unwrap_or_else(|_| "{\"type\":\"error\"}".to_owned()),
        Err(error) => {
            tracing::warn!(%error, "序列化 WebSocket daemon 帧失败");
            return;
        }
    };
    let _ = outgoing.send(WebSocketMessage::Text(message.into()));
}

async fn send_websocket_control_error(socket: &mut WebSocket, message: &str) {
    let _ = socket
        .send(WebSocketMessage::Text(
            json!({"type": "error", "error": message})
                .to_string()
                .into(),
        ))
        .await;
    let _ = socket.send(WebSocketMessage::Close(None)).await;
}

async fn health(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers, state.bearer_token.as_deref()) {
        return api_error(StatusCode::UNAUTHORIZED, "Bearer Token 无效或缺失");
    }
    match crate::entry::cli::request_result(&state.client, "session.load", json!({})).await {
        Ok(_) => Json(json!({"status": "ok", "daemon": "ready"})).into_response(),
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "error", "error": error.to_string()})),
        )
            .into_response(),
    }
}

async fn chat_completions(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<ChatCompletionRequest>,
) -> Response {
    if !is_authorized(&headers, state.bearer_token.as_deref()) {
        return api_error(StatusCode::UNAUTHORIZED, "Bearer Token 无效或缺失");
    }
    if let Some(requested) = &request.model
        && requested != &state.model
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            format!(
                "当前 daemon 模型为 {}，不支持请求模型 {requested}",
                state.model
            ),
        );
    }
    let Some(prompt) = extract_latest_user_prompt(&request.messages) else {
        return api_error(StatusCode::BAD_REQUEST, "messages 中缺少非空 user 消息");
    };
    let rpc = match state
        .client
        .request("chat.send", json!({"message": prompt}))
        .await
    {
        Ok(rpc) => rpc,
        Err(error) => return api_error(StatusCode::BAD_GATEWAY, format!("{error:#}")),
    };
    let completion_id = format!("chatcmpl-{}", request_id_text(rpc.request_id()));
    let created = unix_time();
    if request.stream {
        let stream = completion_stream(rpc, state, completion_id, created);
        return Sse::new(stream)
            .keep_alive(KeepAlive::default())
            .into_response();
    }

    match collect_answer(rpc, &state.client).await {
        Ok(content) => Json(json!({
            "id": completion_id,
            "object": "chat.completion",
            "created": created,
            "model": state.model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }]
        }))
        .into_response(),
        Err(error) => api_error(StatusCode::BAD_GATEWAY, error),
    }
}

fn completion_stream(
    rpc: RpcStream,
    state: ApiState,
    completion_id: String,
    created: u64,
) -> impl Stream<Item = Result<Event, Infallible>> {
    struct StreamState {
        rpc: RpcStream,
        api: ApiState,
        completion_id: String,
        created: u64,
        done_pending: bool,
        finished: bool,
    }

    stream::unfold(
        StreamState {
            rpc,
            api: state,
            completion_id,
            created,
            done_pending: false,
            finished: false,
        },
        |mut state| async move {
            if state.finished {
                return None;
            }
            if state.done_pending {
                state.finished = true;
                return Some((Ok(Event::default().data("[DONE]")), state));
            }
            loop {
                let Some(frame) = state.rpc.next().await else {
                    let event = Event::default()
                        .data(json!({"error": {"message": "daemon 在终态响应前断开"}}).to_string());
                    state.done_pending = true;
                    return Some((Ok(event), state));
                };
                match frame {
                    ServerFrame::Event(event) if event.event == EventKind::TextDelta => {
                        let delta = event.data["delta"].as_str().unwrap_or_default();
                        let chunk = completion_chunk(
                            &state.completion_id,
                            state.created,
                            &state.api.model,
                            json!({"content": delta}),
                            Value::Null,
                        );
                        return Some((Ok(Event::default().data(chunk.to_string())), state));
                    }
                    ServerFrame::Event(event) if event.event == EventKind::ApprovalRequired => {
                        if let Err(error) = deny_approval(&state.api.client, &event.data).await {
                            let event = Event::default()
                                .data(json!({"error": {"message": error}}).to_string());
                            state.done_pending = true;
                            return Some((Ok(event), state));
                        }
                    }
                    ServerFrame::Event(_) => {}
                    ServerFrame::Response(response) => {
                        let chunk = if let Some(error) = response.error {
                            json!({"error": {"code": error.code, "message": error.message}})
                        } else {
                            completion_chunk(
                                &state.completion_id,
                                state.created,
                                &state.api.model,
                                json!({}),
                                json!("stop"),
                            )
                        };
                        state.done_pending = true;
                        return Some((Ok(Event::default().data(chunk.to_string())), state));
                    }
                }
            }
        },
    )
}

async fn collect_answer(mut rpc: RpcStream, client: &DaemonClient) -> Result<String, String> {
    while let Some(frame) = rpc.next().await {
        match frame {
            ServerFrame::Event(event) if event.event == EventKind::ApprovalRequired => {
                deny_approval(client, &event.data).await?;
            }
            ServerFrame::Event(_) => {}
            ServerFrame::Response(response) => {
                if let Some(error) = response.error {
                    return Err(format!("daemon RPC {}: {}", error.code, error.message));
                }
                return response
                    .result
                    .and_then(|result| result["content"].as_str().map(str::to_owned))
                    .ok_or_else(|| "daemon 响应缺少 content".to_owned());
            }
        }
    }
    Err("daemon 在终态响应前断开".to_owned())
}

async fn deny_approval(client: &DaemonClient, data: &Value) -> Result<(), String> {
    let approval_id = data["approval"]["id"]
        .as_str()
        .ok_or_else(|| "审批事件缺少 id".to_owned())?;
    crate::entry::cli::request_result(
        client,
        "approval.respond",
        json!({"approval_id": approval_id, "approved": false}),
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("自动拒绝 HTTP 审批失败: {error:#}"))
}

fn is_authorized(headers: &HeaderMap, required_token: Option<&str>) -> bool {
    let Some(required_token) = required_token else {
        return true;
    };
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    token_matches(provided, Some(required_token))
}

fn token_matches(provided: Option<&str>, required: Option<&str>) -> bool {
    match required {
        Some(required) => provided == Some(required),
        None => true,
    }
}

fn extract_latest_user_prompt(messages: &[OpenAiMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        if message.role != "user" {
            return None;
        }
        let content = match &message.content {
            Value::String(content) => content.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter(|part| part["type"] == "text")
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<&str>>()
                .join("\n"),
            _ => String::new(),
        };
        (!content.trim().is_empty()).then_some(content)
    })
}

fn completion_chunk(
    id: &str,
    created: u64,
    model: &str,
    delta: Value,
    finish_reason: Value,
) -> Value {
    json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]
    })
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({"error": {"message": message.into(), "type": "my_agent_error"}})),
    )
        .into_response()
}

fn request_id_text(id: &RequestId) -> String {
    match id {
        RequestId::Number(value) => value.to_string(),
        RequestId::String(value) => value.clone(),
    }
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use anyhow::Result;
    use async_trait::async_trait;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;

    use crate::context::{ContextConfig, ContextManager};
    use crate::daemon::DaemonState;
    use crate::daemon::approval::ApprovalBroker;
    use crate::daemon::server::InMemoryServer;
    use crate::loop_engine::LoopEngine;
    use crate::plan::PlanStore;
    use crate::provider::{Message, Provider, Response, ToolCall, ToolSpec};
    use crate::safety::Approval;
    use crate::session::SessionStore;
    use crate::tools::{Tool, ToolRegistry};

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    struct MockProvider {
        responses: StdMutex<VecDeque<Response>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            self.responses
                .lock()
                .map_err(|_| anyhow::anyhow!("mock provider 锁已损坏"))?
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock 响应不足"))
        }
    }

    struct ApprovalTool {
        approvals: ApprovalBroker,
    }

    #[async_trait]
    impl Tool for ApprovalTool {
        fn name(&self) -> &str {
            "danger"
        }

        fn description(&self) -> &str {
            "测试 WebSocket 审批往返"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String> {
            if self
                .approvals
                .request("执行高风险 WebSocket 测试动作")
                .await?
            {
                Ok("approved".to_owned())
            } else {
                anyhow::bail!("测试动作被拒绝")
            }
        }
    }

    async fn websocket_test_state() -> (ApiState, std::path::PathBuf) {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path = std::env::temp_dir().join(format!(
            "my-agent-websocket-{}-{id}.jsonl",
            std::process::id()
        ));
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            responses: StdMutex::new(VecDeque::from([
                Response::ToolCalls(vec![ToolCall {
                    id: "ws-danger".to_owned(),
                    name: "danger".to_owned(),
                    arguments: json!({}),
                }]),
                Response::Text("WebSocket 审批后完成".to_owned()),
            ])),
        });
        let approvals = ApprovalBroker::new();
        let mut tools = ToolRegistry::new();
        tools.register(ApprovalTool {
            approvals: approvals.clone(),
        });
        let context = ContextManager::new(
            provider.clone(),
            std::env::current_dir().expect("测试工作区应存在"),
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .expect("测试上下文应可创建");
        let session = Arc::new(SessionStore::new(&session_path));
        let engine = Arc::new(LoopEngine::new(provider, tools, context, session.clone()));
        let daemon = Arc::new(DaemonState::new(engine, Vec::new(), session, approvals));
        (
            ApiState {
                client: InMemoryServer::start(daemon),
                model: "test-model".to_owned(),
                bearer_token: Some("test-token".to_owned()),
            },
            session_path,
        )
    }

    #[test]
    fn extracts_latest_string_or_content_parts() {
        let messages = vec![
            OpenAiMessage {
                role: "user".to_owned(),
                content: Value::String("旧问题".to_owned()),
            },
            OpenAiMessage {
                role: "assistant".to_owned(),
                content: Value::String("旧回答".to_owned()),
            },
            OpenAiMessage {
                role: "user".to_owned(),
                content: json!([
                    {"type": "text", "text": "第一段"},
                    {"type": "image_url", "image_url": {"url": "ignored"}},
                    {"type": "text", "text": "第二段"}
                ]),
            },
        ];

        assert_eq!(
            extract_latest_user_prompt(&messages).as_deref(),
            Some("第一段\n第二段")
        );
    }

    #[test]
    fn bearer_auth_is_only_enforced_when_configured() {
        let mut headers = HeaderMap::new();
        assert!(is_authorized(&headers, None));
        assert!(!is_authorized(&headers, Some("secret")));
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer secret".parse().unwrap(),
        );
        assert!(is_authorized(&headers, Some("secret")));
    }

    #[tokio::test]
    async fn websocket_requires_token_and_supports_interactive_approval() {
        let (state, session_path) = websocket_test_state().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("测试端口应可监听");
        let address = listener.local_addr().expect("测试端口应有地址");
        let server = tokio::spawn(async move {
            axum::serve(listener, api_router(state))
                .await
                .expect("测试 Web 服务应正常运行");
        });
        let url = format!("ws://{address}/ws");

        let (mut rejected, _) = connect_async(&url).await.expect("WebSocket 应可升级");
        rejected
            .send(TungsteniteMessage::Text(
                json!({"type": "connect", "token": "wrong"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("错误 token 帧应可发送");
        let rejected_message = rejected
            .next()
            .await
            .expect("应返回鉴权错误")
            .expect("鉴权错误帧应可读取")
            .into_text()
            .expect("鉴权错误应为文本");
        assert_eq!(
            serde_json::from_str::<Value>(&rejected_message).expect("错误帧应为 JSON")["type"],
            "error"
        );

        let (mut socket, _) = connect_async(&url).await.expect("WebSocket 应可再次升级");
        socket
            .send(TungsteniteMessage::Text(
                json!({"type": "connect", "token": "test-token"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("connect 帧应可发送");
        let connected = socket
            .next()
            .await
            .expect("应返回 connected")
            .expect("connected 帧应可读取")
            .into_text()
            .expect("connected 应为文本");
        assert_eq!(
            serde_json::from_str::<Value>(&connected).expect("connected 应为 JSON")["type"],
            "connected"
        );
        let recovery = socket
            .next()
            .await
            .expect("应返回恢复快照")
            .expect("恢复快照应可读取")
            .into_text()
            .expect("恢复快照应为文本");
        assert_eq!(
            serde_json::from_str::<Value>(&recovery).expect("恢复快照应为 JSON")["type"],
            "recovery"
        );
        socket
            .send(TungsteniteMessage::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": "web-chat",
                    "method": "chat.send",
                    "params": {"message": "执行测试动作"}
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("chat 请求应可发送");

        let mut saw_approval = false;
        let mut saw_text = false;
        let mut completed = false;
        while !completed {
            let message = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
                .await
                .expect("WebSocket 响应不应超时")
                .expect("WebSocket 不应提前关闭")
                .expect("WebSocket 帧应可读取")
                .into_text()
                .expect("daemon 帧应为文本");
            let frame: ServerFrame =
                serde_json::from_str(&message).expect("daemon 帧应符合 ServerFrame");
            match frame {
                ServerFrame::Event(event) if event.event == EventKind::ApprovalRequired => {
                    saw_approval = true;
                    let approval_id = event.data["approval"]["id"]
                        .as_str()
                        .expect("审批事件应有 id");
                    socket
                        .send(TungsteniteMessage::Text(
                            json!({
                                "jsonrpc": "2.0",
                                "id": "web-approval",
                                "method": "approval.respond",
                                "params": {"approval_id": approval_id, "approved": true}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .expect("审批响应应可发送");
                }
                ServerFrame::Event(event) if event.event == EventKind::TextDelta => {
                    saw_text |= event.data["delta"] == "WebSocket 审批后完成";
                }
                ServerFrame::Response(response)
                    if response.id == RequestId::String("web-chat".to_owned()) =>
                {
                    assert!(response.error.is_none());
                    completed = true;
                }
                ServerFrame::Event(_) | ServerFrame::Response(_) => {}
            }
        }

        assert!(saw_approval);
        assert!(saw_text);
        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn websocket_reconnect_recovers_pending_approval_and_active_output() {
        let (state, session_path) = websocket_test_state().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("测试端口应可监听");
        let address = listener.local_addr().expect("测试端口应有地址");
        let server = tokio::spawn(async move {
            axum::serve(listener, api_router(state))
                .await
                .expect("测试 Web 服务应正常运行");
        });
        let url = format!("ws://{address}/ws");

        let (mut first, _) = connect_async(&url).await.expect("首次连接应成功");
        first
            .send(TungsteniteMessage::Text(
                json!({"type": "connect", "token": "test-token"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("首次 connect 应可发送");
        let _ = first.next().await.expect("应收到 connected");
        let _ = first.next().await.expect("应收到首次 recovery");
        first
            .send(TungsteniteMessage::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": "lost-ws-chat",
                    "method": "chat.send",
                    "params": {"message": "断线 WebSocket 恢复"}
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("chat 请求应可发送");
        loop {
            let message = tokio::time::timeout(Duration::from_secs(2), first.next())
                .await
                .expect("首次连接应收到审批")
                .expect("首次连接不应提前结束")
                .expect("首次 WebSocket 帧应可读取")
                .into_text()
                .expect("首次 daemon 帧应为文本");
            let frame: ServerFrame = serde_json::from_str(&message).expect("daemon 帧应为 JSON");
            if matches!(
                frame,
                ServerFrame::Event(event) if event.event == EventKind::ApprovalRequired
            ) {
                break;
            }
        }
        drop(first);

        let (mut second, _) = connect_async(&url).await.expect("重连应成功");
        second
            .send(TungsteniteMessage::Text(
                json!({"type": "connect", "token": "test-token"})
                    .to_string()
                    .into(),
            ))
            .await
            .expect("重连 connect 应可发送");
        assert_eq!(
            serde_json::from_str::<Value>(
                &second
                    .next()
                    .await
                    .expect("应收到重连 connected")
                    .expect("connected 帧应可读取")
                    .into_text()
                    .expect("connected 应为文本"),
            )
            .expect("connected 应为 JSON")["type"],
            "connected"
        );
        let recovery = second
            .next()
            .await
            .expect("应收到重连 recovery")
            .expect("recovery 帧应可读取")
            .into_text()
            .expect("recovery 应为文本");
        let recovery_value: Value = serde_json::from_str(&recovery).expect("recovery 应为 JSON");
        assert_eq!(recovery_value["type"], "recovery");
        assert_eq!(
            recovery_value["snapshot"]["pending_approvals"]
                .as_array()
                .expect("恢复快照应包含审批")
                .len(),
            1
        );

        let mut saw_text = false;
        let mut completed = false;
        while !completed {
            let message = tokio::time::timeout(Duration::from_secs(2), second.next())
                .await
                .expect("重连订阅不应超时")
                .expect("重连 WebSocket 不应提前关闭")
                .expect("重连帧应可读取")
                .into_text()
                .expect("重连 daemon 帧应为文本");
            let frame: ServerFrame = serde_json::from_str(&message).expect("重连帧应为 JSON");
            match frame {
                ServerFrame::Event(event) if event.event == EventKind::ApprovalRequired => {
                    let approval_id = event.data["approval"]["id"]
                        .as_str()
                        .expect("恢复审批应包含 id");
                    second
                        .send(TungsteniteMessage::Text(
                            json!({
                                "jsonrpc": "2.0",
                                "id": "reconnect-approval",
                                "method": "approval.respond",
                                "params": {"approval_id": approval_id, "approved": true}
                            })
                            .to_string()
                            .into(),
                        ))
                        .await
                        .expect("恢复审批响应应可发送");
                }
                ServerFrame::Event(event) if event.event == EventKind::TextDelta => {
                    saw_text |= event.data["delta"] == "WebSocket 审批后完成";
                }
                ServerFrame::Response(response)
                    if response
                        .result
                        .as_ref()
                        .is_some_and(|result| result.get("content").is_some()) =>
                {
                    assert!(response.error.is_none());
                    completed = true;
                }
                ServerFrame::Event(_) | ServerFrame::Response(_) => {}
            }
        }
        assert!(saw_text);
        server.abort();
        let _ = server.await;
        let _ = std::fs::remove_file(session_path);
    }
}
