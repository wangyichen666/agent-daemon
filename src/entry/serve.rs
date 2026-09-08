use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::protocol::{EventKind, RequestId, ServerFrame};

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
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);
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
    provided == Some(required_token)
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
    use super::*;

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
}
