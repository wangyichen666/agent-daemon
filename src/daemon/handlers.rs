use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::DaemonState;
use super::protocol::{
    EventFrame, EventKind, JsonRpcRequest, JsonRpcResponse, RequestId, ServerFrame,
};
use crate::loop_engine::{AgentEvent, CancellationToken};

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;
const REQUEST_CANCELLED: i64 = -32800;
const REQUEST_CONFLICT: i64 = -32001;

#[derive(Deserialize)]
struct ChatSendParams {
    message: String,
}

#[derive(Deserialize)]
struct ApprovalRespondParams {
    approval_id: String,
    approved: bool,
}

#[derive(Deserialize)]
struct CancelParams {
    request_id: RequestId,
}

impl DaemonState {
    pub async fn handle_request(
        self: Arc<Self>,
        request: JsonRpcRequest,
        frames: mpsc::UnboundedSender<ServerFrame>,
    ) {
        match request.method.as_str() {
            "chat.send" => self.handle_chat_send(request, frames).await,
            "session.load" => {
                let result = self.session_load().await;
                send_result(&frames, request.id, result);
            }
            "session.list" => {
                let result = self.session_list().await;
                send_result(&frames, request.id, result);
            }
            "session.new" => {
                let result = self.session_new().await;
                send_result(&frames, request.id, result);
            }
            "approval.respond" => {
                let result = match parse_params::<ApprovalRespondParams>(&request.params) {
                    Ok(params) => self
                        .approvals
                        .respond(&params.approval_id, params.approved)
                        .await
                        .map(|()| json!({"accepted": true}))
                        .map_err(|error| (INVALID_PARAMS, format!("{error:#}"))),
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "agent.cancel" => {
                let result = match parse_params::<CancelParams>(&request.params) {
                    Ok(params) => self.cancel(&params.request_id).await,
                    Err(error) => Err((INVALID_PARAMS, error)),
                };
                send_result(&frames, request.id, result);
            }
            "daemon.stop" => {
                send_result(
                    &frames,
                    request.id,
                    Ok(json!({"stopping": true, "active_turns_finish_gracefully": true})),
                );
                let shutdown = self.shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    shutdown.cancel();
                });
            }
            _ => {
                let _ = frames.send(ServerFrame::Response(JsonRpcResponse::failure(
                    request.id,
                    METHOD_NOT_FOUND,
                    format!("未知 RPC 方法: {}", request.method),
                )));
            }
        }
    }

    async fn handle_chat_send(
        self: Arc<Self>,
        request: JsonRpcRequest,
        frames: mpsc::UnboundedSender<ServerFrame>,
    ) {
        let params = match parse_params::<ChatSendParams>(&request.params) {
            Ok(params) if !params.message.trim().is_empty() => params,
            Ok(_) => {
                send_result(
                    &frames,
                    request.id,
                    Err((INVALID_PARAMS, "message 不能为空".to_owned())),
                );
                return;
            }
            Err(error) => {
                send_result(&frames, request.id, Err((INVALID_PARAMS, error)));
                return;
            }
        };

        let cancellation = CancellationToken::new();
        let mut active = self.active.lock().await;
        if active.contains_key(&request.id) {
            drop(active);
            send_result(
                &frames,
                request.id,
                Err((REQUEST_CONFLICT, "请求 id 正在执行".to_owned())),
            );
            return;
        }
        active.insert(request.id.clone(), cancellation.clone());
        drop(active);

        let (agent_events, mut event_receiver) = mpsc::unbounded_channel();
        let run = self
            .approvals
            .with_context(request.id.clone(), frames.clone(), async {
                let mut history = self.history.lock().await;
                self.engine
                    .run_turn_with_events(
                        &mut history,
                        params.message,
                        Some(agent_events),
                        cancellation,
                    )
                    .await
            });
        tokio::pin!(run);

        let result = loop {
            tokio::select! {
                result = &mut run => break result,
                event = event_receiver.recv() => {
                    if let Some(event) = event {
                        send_agent_event(&frames, request.id.clone(), event);
                    }
                }
            }
        };
        while let Ok(event) = event_receiver.try_recv() {
            send_agent_event(&frames, request.id.clone(), event);
        }

        self.active.lock().await.remove(&request.id);
        let response = match result {
            Ok(content) => Ok(json!({"content": content})),
            Err(error) if error.to_string().contains("请求已取消") => {
                Err((REQUEST_CANCELLED, "请求已取消".to_owned()))
            }
            Err(error) => Err((INTERNAL_ERROR, format!("{error:#}"))),
        };
        send_result(&frames, request.id, response);
    }

    async fn session_load(&self) -> Result<Value, (i64, String)> {
        let history = self.history.lock().await.clone();
        let approvals = self.approvals.pending().await;
        let active_requests = self
            .active
            .lock()
            .await
            .keys()
            .cloned()
            .collect::<Vec<RequestId>>();
        Ok(json!({
            "messages": history,
            "pending_approvals": approvals,
            "active_requests": active_requests,
        }))
    }

    async fn session_list(&self) -> Result<Value, (i64, String)> {
        self.session
            .list_sessions()
            .await
            .map(|sessions| json!({"sessions": sessions}))
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))
    }

    async fn session_new(&self) -> Result<Value, (i64, String)> {
        if self.has_active_turns().await {
            return Err((REQUEST_CONFLICT, "有请求正在执行，不能新建会话".to_owned()));
        }
        let mut history = self.history.lock().await;
        let backup = self
            .session
            .rotate_existing()
            .await
            .map_err(|error| (INTERNAL_ERROR, format!("{error:#}")))?;
        history.clear();
        Ok(json!({"created": true, "backup": backup}))
    }

    async fn cancel(&self, request_id: &RequestId) -> Result<Value, (i64, String)> {
        let token = self.active.lock().await.get(request_id).cloned();
        let Some(token) = token else {
            return Ok(json!({"cancelled": false, "reason": "请求未在执行"}));
        };
        token.cancel();
        self.approvals.cancel_request(request_id).await;
        Ok(json!({"cancelled": true}))
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: &Value) -> Result<T, String> {
    serde_json::from_value(params.clone()).map_err(|error| format!("参数无效: {error}"))
}

fn send_result(
    frames: &mpsc::UnboundedSender<ServerFrame>,
    id: RequestId,
    result: Result<Value, (i64, String)>,
) {
    let response = match result {
        Ok(value) => JsonRpcResponse::success(id, value),
        Err((code, message)) => JsonRpcResponse::failure(id, code, message),
    };
    let _ = frames.send(ServerFrame::Response(response));
}

fn send_agent_event(
    frames: &mpsc::UnboundedSender<ServerFrame>,
    request_id: RequestId,
    event: AgentEvent,
) {
    let (kind, data) = match event {
        AgentEvent::TurnStarted => (EventKind::TurnStarted, json!({})),
        AgentEvent::TextDelta(delta) => (EventKind::TextDelta, json!({"delta": delta})),
        AgentEvent::ToolStarted { call_id, name } => (
            EventKind::ToolStarted,
            json!({"tool_call_id": call_id, "name": name}),
        ),
        AgentEvent::ToolFinished {
            call_id,
            name,
            output,
        } => (
            EventKind::ToolFinished,
            json!({"tool_call_id": call_id, "name": name, "output": output}),
        ),
        AgentEvent::TurnCompleted { content } => {
            (EventKind::TurnCompleted, json!({"content": content}))
        }
    };
    let _ = frames.send(ServerFrame::Event(EventFrame::new(request_id, kind, data)));
}
