use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::{
    ProtocolVersion,
    v1::{
        AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, Implementation,
        InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
        NewSessionRequest, NewSessionResponse, PermissionOption, PermissionOptionKind,
        PromptRequest, PromptResponse, RequestPermissionOutcome, RequestPermissionRequest,
        SessionId, SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallStatus,
        ToolCallUpdate, ToolCallUpdateFields,
    },
};
use agent_client_protocol::{
    Agent, Client as AcpClient, ConnectTo, ConnectionTo, Error as AcpError, Stdio,
};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::client::{DaemonClient, RpcStream};
use crate::daemon::approval::PendingApprovalInfo;
use crate::daemon::protocol::{EventFrame, EventKind, RequestId, ServerFrame};
use crate::entry::recovery;
use crate::provider::{Message, Role};
use crate::slash::{SlashAction, SlashParse, SlashRegistry, SlashResponse};

type ActiveRequests = Arc<Mutex<HashMap<String, RequestId>>>;

pub async fn run_acp_server(client: DaemonClient, workspace: PathBuf) -> Result<()> {
    build_acp_agent(client, workspace)
        .connect_to(Stdio::new())
        .await
        .context("ACP stdio 连接异常结束")
}

fn build_acp_agent(client: DaemonClient, workspace: PathBuf) -> impl ConnectTo<AcpClient> {
    let active_requests = ActiveRequests::default();
    let new_client = client.clone();
    let new_workspace = workspace.clone();
    let load_client = client.clone();
    let load_workspace = workspace.clone();
    let prompt_client = client.clone();
    let prompt_requests = active_requests.clone();
    let cancel_client = client;
    let cancel_requests = active_requests;

    Agent
        .builder()
        .name("my-agent-acp")
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                let response = InitializeResponse::new(ProtocolVersion::V1)
                    .agent_capabilities(AgentCapabilities::new().load_session(true))
                    .agent_info(
                        Implementation::new("my-agent", env!("CARGO_PKG_VERSION"))
                            .title("my-agent Rust 编码 Agent"),
                    );
                if request.protocol_version != ProtocolVersion::V1 {
                    tracing::debug!(
                        requested = ?request.protocol_version,
                        "ACP 客户端请求了不同版本，返回当前稳定版本"
                    );
                }
                responder.respond(response)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: NewSessionRequest, responder, connection| {
                let client = new_client.clone();
                let workspace = new_workspace.clone();
                connection.spawn(async move {
                    if let Err(error) = require_workspace(&request.cwd, &workspace) {
                        return responder.respond_with_error(error);
                    }
                    match crate::entry::cli::request_result(&client, "session.new", json!({})).await
                    {
                        Ok(result) => match daemon_session_id(&result) {
                            Ok(session_id) => {
                                responder.respond(NewSessionResponse::new(session_id))
                            }
                            Err(error) => responder.respond_with_error(error),
                        },
                        Err(error) => responder.respond_with_error(internal_error(error)),
                    }
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: LoadSessionRequest, responder, connection| {
                let client = load_client.clone();
                let workspace = load_workspace.clone();
                let task_connection = connection.clone();
                connection.spawn(async move {
                    if let Err(error) = require_workspace(&request.cwd, &workspace) {
                        return responder.respond_with_error(error);
                    }
                    let snapshot =
                        match recovery::resume_session(&client, &request.session_id.to_string())
                            .await
                        {
                            Ok(snapshot) => snapshot,
                            Err(error) => {
                                return responder.respond_with_error(internal_error(error));
                            }
                        };
                    let session_id = SessionId::new(snapshot.session_id.clone());
                    if let Err(error) =
                        replay_history(&task_connection, &session_id, &snapshot.messages)
                    {
                        return responder.respond_with_error(error);
                    }
                    if let Err(error) = start_active_subscriptions(
                        &client,
                        &task_connection,
                        &session_id,
                        &snapshot.active_requests,
                        snapshot
                            .pending_approvals
                            .iter()
                            .map(|approval| approval.id.clone())
                            .collect(),
                    )
                    .await
                    {
                        return responder.respond_with_error(error);
                    }
                    if let Err(error) = recover_pending_approvals(
                        &client,
                        &task_connection,
                        &session_id,
                        &snapshot.pending_approvals,
                    )
                    .await
                    {
                        return responder.respond_with_error(error);
                    }
                    responder.respond(LoadSessionResponse::new())
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                let client = prompt_client.clone();
                let active_requests = prompt_requests.clone();
                let task_connection = connection.clone();
                connection.spawn(async move {
                    let message = match prompt_to_text(&request.prompt) {
                        Ok(message) => message,
                        Err(error) => return responder.respond_with_error(error),
                    };
                    if message.trim_start().starts_with('/') {
                        return match run_acp_slash(
                            &client,
                            &task_connection,
                            &request.session_id,
                            &message,
                        )
                        .await
                        {
                            Ok(()) => responder.respond(PromptResponse::new(StopReason::EndTurn)),
                            Err(error) => responder.respond_with_error(error),
                        };
                    }
                    let session_key = request.session_id.to_string();
                    let stream = match client
                        .request(
                            "chat.send",
                            json!({
                                "message": message,
                                "session_id": request.session_id.to_string(),
                            }),
                        )
                        .await
                    {
                        Ok(stream) => stream,
                        Err(error) => return responder.respond_with_error(internal_error(error)),
                    };
                    active_requests
                        .lock()
                        .await
                        .insert(session_key.clone(), stream.request_id().clone());
                    let ignored_approvals = HashSet::new();
                    let result = forward_prompt_stream(
                        &client,
                        &task_connection,
                        &request.session_id,
                        stream,
                        &ignored_approvals,
                    )
                    .await;
                    active_requests.lock().await.remove(&session_key);
                    match result {
                        Ok(stop_reason) => responder.respond(PromptResponse::new(stop_reason)),
                        Err(error) => responder.respond_with_error(error),
                    }
                })?;
                Ok(())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |notification: CancelNotification, _connection| {
                let request_id = cancel_requests
                    .lock()
                    .await
                    .get(&notification.session_id.to_string())
                    .cloned();
                if let Some(request_id) = request_id {
                    crate::entry::cli::request_result(
                        &cancel_client,
                        "agent.cancel",
                        json!({
                            "request_id": request_id,
                            "session_id": notification.session_id.to_string(),
                        }),
                    )
                    .await
                    .map_err(internal_error)?;
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

async fn run_acp_slash(
    client: &DaemonClient,
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    line: &str,
) -> Result<(), AcpError> {
    let registry = SlashRegistry::builtin();
    let parsed = registry.parse(line);
    let allowed = matches!(
        &parsed,
        SlashParse::Command(crate::slash::SlashInvocation {
            action: SlashAction::Help
                | SlashAction::Status
                | SlashAction::Sessions
                | SlashAction::Ping,
            ..
        }) | SlashParse::Error(_)
    ) || matches!(
        &parsed,
        SlashParse::Command(crate::slash::SlashInvocation {
            action: SlashAction::Mcp,
            args,
        }) if matches!(args.as_slice(), [command] if command == "list" || command == "status")
    );
    let content = if allowed {
        let value = crate::entry::cli::request_result(
            client,
            "slash.execute",
            json!({
                "line": line,
                "session_id": session_id.to_string(),
            }),
        )
        .await
        .map_err(internal_error)?;
        let response: SlashResponse = serde_json::from_value(value)
            .map_err(|error| internal_error(anyhow::Error::from(error)))?;
        render_acp_slash_response(response)
    } else {
        "ACP 入口仅支持只读 slash 命令：/help、/status、/sessions、/ping、/mcp list、/mcp status。会话切换请使用 ACP session/new 或 session/load。".to_owned()
    };
    connection.send_notification(SessionNotification::new(
        session_id.clone(),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(content.into())),
    ))
}

fn render_acp_slash_response(response: SlashResponse) -> String {
    match response {
        SlashResponse::Text { content } => content,
        SlashResponse::Sessions { sessions, .. } => {
            if sessions.is_empty() {
                return "暂无会话记录。".to_owned();
            }
            sessions
                .iter()
                .enumerate()
                .map(|(index, session)| {
                    format!(
                        "{}. {} · {} 条消息 · {}",
                        index + 1,
                        session.id,
                        session.message_count,
                        session.preview.as_deref().unwrap_or("无摘要")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        SlashResponse::Exit => "ACP 入口不支持 /exit。".to_owned(),
        SlashResponse::SessionChanged { message, .. } => message,
    }
}

fn require_workspace(requested: &Path, workspace: &Path) -> Result<(), AcpError> {
    if requested == workspace {
        return Ok(());
    }
    Err(AcpError::invalid_params().data(json!({
        "message": "ACP cwd 必须与 daemon 工作区一致",
        "requested": requested,
        "workspace": workspace,
    })))
}

fn daemon_session_id(result: &Value) -> Result<SessionId, AcpError> {
    result["session_id"]
        .as_str()
        .map(|value| SessionId::new(value.to_owned()))
        .ok_or_else(|| AcpError::internal_error().data("daemon 响应缺少 session_id"))
}

fn prompt_to_text(blocks: &[ContentBlock]) -> Result<String, AcpError> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => parts.push(text.text.clone()),
            ContentBlock::ResourceLink(link) => {
                parts.push(format!("[资源：{}]({})", link.name, link.uri));
            }
            _ => {
                return Err(AcpError::invalid_params()
                    .data("当前 ACP 入口仅支持 text 与 resource_link 内容块"));
            }
        }
    }
    let message = parts.join("\n");
    if message.trim().is_empty() {
        return Err(AcpError::invalid_params().data("prompt 不能为空"));
    }
    Ok(message)
}

async fn forward_prompt_stream(
    client: &DaemonClient,
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    mut stream: RpcStream,
    ignored_approvals: &HashSet<String>,
) -> Result<StopReason, AcpError> {
    while let Some(frame) = stream.next().await {
        match frame {
            ServerFrame::Event(event) if event.event == EventKind::ApprovalRequired => {
                if event.data["approval"]["id"]
                    .as_str()
                    .is_some_and(|id| ignored_approvals.contains(id))
                {
                    continue;
                }
                request_permission(client, connection, session_id, &event.data).await?;
            }
            ServerFrame::Event(event) => forward_event(connection, session_id, event)?,
            ServerFrame::Response(response) => {
                if let Some(error) = response.error {
                    if error.code == -32800 {
                        return Ok(StopReason::Cancelled);
                    }
                    let code = i32::try_from(error.code).unwrap_or(-32603);
                    return Err(AcpError::new(code, error.message));
                }
                return Ok(StopReason::EndTurn);
            }
        }
    }
    Err(AcpError::internal_error().data("daemon 在终态响应前断开"))
}

fn forward_event(
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    event: EventFrame,
) -> Result<(), AcpError> {
    let update = match event.event {
        EventKind::TextDelta => {
            let delta = event.data["delta"].as_str().unwrap_or_default();
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                delta.to_owned().into(),
            )))
        }
        EventKind::ToolStarted => {
            let call_id = event.data["tool_call_id"].as_str().unwrap_or("unknown");
            let name = event.data["name"].as_str().unwrap_or("unknown");
            Some(SessionUpdate::ToolCall(
                ToolCall::new(call_id.to_owned(), name.to_owned())
                    .status(ToolCallStatus::InProgress),
            ))
        }
        EventKind::ToolFinished => {
            let call_id = event.data["tool_call_id"].as_str().unwrap_or("unknown");
            let output = event.data["output"].as_str().unwrap_or_default();
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                call_id.to_owned(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![output.to_owned().into()])
                    .raw_output(json!({"output": output})),
            )))
        }
        EventKind::TurnStarted | EventKind::TurnCompleted | EventKind::ApprovalRequired => None,
    };
    if let Some(update) = update {
        connection.send_notification(SessionNotification::new(session_id.clone(), update))?;
    }
    Ok(())
}

async fn request_permission(
    client: &DaemonClient,
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    data: &Value,
) -> Result<(), AcpError> {
    let approval = data
        .get("approval")
        .ok_or_else(|| AcpError::internal_error().data("审批事件缺少 approval"))?;
    let approval_id = approval["id"]
        .as_str()
        .ok_or_else(|| AcpError::internal_error().data("审批事件缺少 id"))?;
    let prompt = approval["prompt"]
        .as_str()
        .ok_or_else(|| AcpError::internal_error().data("审批事件缺少 prompt"))?;
    let tool_call = ToolCallUpdate::new(
        approval_id.to_owned(),
        ToolCallUpdateFields::new()
            .title(prompt.to_owned())
            .status(ToolCallStatus::Pending)
            .raw_input(json!({"prompt": prompt})),
    );
    let response = connection
        .send_request(RequestPermissionRequest::new(
            session_id.clone(),
            tool_call,
            vec![
                PermissionOption::new("allow_once", "允许一次", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject_once", "拒绝一次", PermissionOptionKind::RejectOnce),
            ],
        ))
        .block_task()
        .await?;
    let approved = match response.outcome {
        RequestPermissionOutcome::Selected(selected) => {
            selected.option_id.to_string() == "allow_once"
        }
        RequestPermissionOutcome::Cancelled => false,
        _ => false,
    };
    recovery::respond_to_approval(client, approval_id, approved)
        .await
        .map_err(internal_error)
}

fn replay_history(
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    messages: &[Message],
) -> Result<(), AcpError> {
    for message in messages {
        let Some(content) = message
            .content
            .as_deref()
            .filter(|content| !content.is_empty())
        else {
            continue;
        };
        let update = match message.role {
            Role::User => {
                SessionUpdate::UserMessageChunk(ContentChunk::new(content.to_owned().into()))
            }
            Role::Assistant | Role::Tool => {
                SessionUpdate::AgentMessageChunk(ContentChunk::new(content.to_owned().into()))
            }
            Role::System => continue,
        };
        connection.send_notification(SessionNotification::new(session_id.clone(), update))?;
    }
    Ok(())
}

async fn recover_pending_approvals(
    client: &DaemonClient,
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    approvals: &[PendingApprovalInfo],
) -> Result<(), AcpError> {
    for approval in approvals {
        request_permission(
            client,
            connection,
            session_id,
            &json!({"approval": approval}),
        )
        .await?;
    }
    Ok(())
}

async fn start_active_subscriptions(
    client: &DaemonClient,
    connection: &ConnectionTo<AcpClient>,
    session_id: &SessionId,
    active_requests: &[RequestId],
    ignored_approvals: HashSet<String>,
) -> Result<(), AcpError> {
    for request_id in active_requests {
        let stream = recovery::subscribe_for_session(client, request_id, &session_id.to_string())
            .await
            .map_err(internal_error)?;
        let task_client = client.clone();
        let task_connection = connection.clone();
        let task_session_id = session_id.clone();
        let task_ignored_approvals = ignored_approvals.clone();
        connection.spawn(async move {
            if let Err(error) = forward_prompt_stream(
                &task_client,
                &task_connection,
                &task_session_id,
                stream,
                &task_ignored_approvals,
            )
            .await
            {
                tracing::warn!(?error, "ACP 恢复活动请求失败");
            }
            Ok(())
        })?;
    }
    Ok(())
}

fn internal_error(error: impl std::fmt::Display) -> AcpError {
    AcpError::internal_error().data(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use agent_client_protocol::schema::v1::{
        InitializeRequest, NewSessionRequest, PermissionOptionId, PromptRequest,
        RequestPermissionRequest, RequestPermissionResponse, ResourceLink,
        SelectedPermissionOutcome, TextContent,
    };
    use anyhow::Result;
    use async_trait::async_trait;
    use tokio::sync::oneshot;

    use crate::context::{ContextConfig, ContextManager};
    use crate::daemon::DaemonState;
    use crate::daemon::approval::ApprovalBroker;
    use crate::daemon::server::InMemoryServer;
    use crate::loop_engine::LoopEngine;
    use crate::plan::PlanStore;
    use crate::provider::{Provider, Response, ToolCall as ProviderToolCall, ToolSpec};
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
            "测试 ACP 权限往返"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String> {
            if self.approvals.request("执行高风险测试动作：danger").await? {
                Ok("approved".to_owned())
            } else {
                anyhow::bail!("测试动作被拒绝")
            }
        }
    }

    async fn acp_test_daemon() -> (DaemonClient, PathBuf) {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path =
            std::env::temp_dir().join(format!("my-agent-acp-{}-{id}.jsonl", std::process::id()));
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            responses: StdMutex::new(VecDeque::from([
                Response::ToolCalls(vec![ProviderToolCall {
                    id: "danger-call".to_owned(),
                    name: "danger".to_owned(),
                    arguments: json!({}),
                }]),
                Response::Text("审批后完成".to_owned()),
            ])),
        });
        let approvals = ApprovalBroker::new();
        let mut tools = ToolRegistry::new();
        tools.register(ApprovalTool {
            approvals: approvals.clone(),
        });
        let plan = Arc::new(PlanStore::memory_only());
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
            plan,
        )
        .expect("测试上下文应可创建");
        let session = Arc::new(SessionStore::new(&session_path));
        let engine = Arc::new(LoopEngine::new(provider, tools, context, session.clone()));
        let state = Arc::new(DaemonState::new(engine, Vec::new(), session, approvals));
        (InMemoryServer::start(state), session_path)
    }

    #[test]
    fn prompt_accepts_text_and_resource_links() {
        let prompt = vec![
            ContentBlock::Text(TextContent::new("检查代码")),
            ContentBlock::ResourceLink(ResourceLink::new("README", "file:///tmp/README.md")),
        ];

        assert_eq!(
            prompt_to_text(&prompt).unwrap(),
            "检查代码\n[资源：README](file:///tmp/README.md)"
        );
    }

    #[test]
    fn prompt_rejects_unsupported_content() {
        let value = serde_json::from_value::<ContentBlock>(json!({
            "type": "image",
            "data": "AA==",
            "mimeType": "image/png"
        }))
        .unwrap();

        assert!(prompt_to_text(&[value]).is_err());
    }

    #[tokio::test]
    async fn standard_acp_streams_updates_and_round_trips_permission() {
        let (daemon, session_path) = acp_test_daemon().await;
        let workspace = std::env::current_dir().expect("测试工作区应存在");
        let updates = Arc::new(Mutex::new(Vec::<SessionUpdate>::new()));
        let permission_count = Arc::new(AtomicUsize::new(0));
        let handler_updates = updates.clone();
        let handler_permissions = permission_count.clone();
        let request_workspace = workspace.clone();

        AcpClient
            .builder()
            .name("my-agent-acp-test")
            .on_receive_notification(
                async move |notification: SessionNotification, _connection| {
                    handler_updates.lock().await.push(notification.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _connection| {
                    handler_permissions.fetch_add(1, Ordering::SeqCst);
                    let has_allow_once = request
                        .options
                        .iter()
                        .any(|option| option.option_id == PermissionOptionId::new("allow_once"));
                    if !has_allow_once {
                        return responder.respond_with_internal_error("缺少 allow_once 选项");
                    }
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            "allow_once",
                        )),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                build_acp_agent(daemon, workspace),
                async move |connection: ConnectionTo<Agent>| {
                    let initialized = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    assert_eq!(initialized.protocol_version, ProtocolVersion::V1);
                    assert!(initialized.agent_capabilities.load_session);

                    let session = connection
                        .send_request(NewSessionRequest::new(request_workspace))
                        .block_task()
                        .await?;
                    let prompt = connection
                        .send_request(PromptRequest::new(
                            session.session_id.clone(),
                            vec![ContentBlock::Text(TextContent::new("执行测试动作"))],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(prompt.stop_reason, StopReason::EndTurn);
                    let ping = connection
                        .send_request(PromptRequest::new(
                            session.session_id,
                            vec![ContentBlock::Text(TextContent::new("/ping"))],
                        ))
                        .block_task()
                        .await?;
                    assert_eq!(ping.stop_reason, StopReason::EndTurn);
                    Ok(())
                },
            )
            .await
            .expect("标准 ACP 客户端链路应完成");

        assert_eq!(permission_count.load(Ordering::SeqCst), 1);
        let updates = updates.lock().await;
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, SessionUpdate::ToolCall(_)))
        );
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, SessionUpdate::ToolCallUpdate(_)))
        );
        assert!(updates.iter().any(|update| {
            matches!(
                update,
                SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: ContentBlock::Text(text),
                    ..
                }) if text.text == "审批后完成"
            )
        }));
        assert!(updates.iter().any(|update| {
            matches!(
                update,
                SessionUpdate::AgentMessageChunk(ContentChunk {
                    content: ContentBlock::Text(text),
                    ..
                }) if text.text == "pong"
            )
        }));
        drop(updates);
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn acp_load_session_recovers_a_disconnected_pending_turn() {
        let (daemon, session_path) = acp_test_daemon().await;
        let workspace = std::env::current_dir().expect("测试工作区应存在");
        let (permission_seen_tx, permission_seen_rx) = oneshot::channel::<()>();
        let permission_seen = Arc::new(Mutex::new(Some(permission_seen_tx)));
        let (session_id_tx, session_id_rx) = oneshot::channel::<String>();
        let first_workspace = workspace.clone();
        let first_daemon = daemon.clone();
        let first_permission_seen = permission_seen.clone();
        let first = tokio::spawn(async move {
            AcpClient
                .builder()
                .name("my-agent-acp-disconnect-test")
                .on_receive_request(
                    async move |request: RequestPermissionRequest, responder, _connection| {
                        let _ = request;
                        if let Some(sender) = first_permission_seen.lock().await.take() {
                            let _ = sender.send(());
                        }
                        tokio::time::sleep(Duration::from_secs(60)).await;
                        responder.respond_with_internal_error("模拟客户端断开")
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(
                    build_acp_agent(first_daemon, first_workspace.clone()),
                    async move |connection: ConnectionTo<Agent>| {
                        let session = connection
                            .send_request(NewSessionRequest::new(first_workspace.clone()))
                            .block_task()
                            .await?;
                        let _ = session_id_tx.send(session.session_id.to_string());
                        let _ = connection
                            .send_request(PromptRequest::new(
                                session.session_id,
                                vec![ContentBlock::Text(TextContent::new("断线恢复"))],
                            ))
                            .block_task()
                            .await;
                        Ok(())
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), permission_seen_rx)
            .await
            .expect("首个 ACP 客户端应收到审批")
            .expect("审批信号通道不应关闭");
        let session_id = tokio::time::timeout(Duration::from_secs(2), session_id_rx)
            .await
            .expect("应取得 ACP 会话 id")
            .expect("会话 id 通道不应关闭");
        first.abort();
        let _ = first.await;

        let snapshot = recovery::load_snapshot(&daemon)
            .await
            .expect("应读取恢复快照");
        assert_eq!(snapshot.pending_approvals.len(), 1);
        assert_eq!(snapshot.active_requests.len(), 1);

        let (text_seen_tx, text_seen_rx) = oneshot::channel::<()>();
        let text_seen = Arc::new(Mutex::new(Some(text_seen_tx)));
        let second_text_seen = text_seen.clone();
        let second_workspace = workspace.clone();
        AcpClient
            .builder()
            .name("my-agent-acp-reconnect-test")
            .on_receive_notification(
                async move |notification: SessionNotification, _connection| {
                    if matches!(
                        notification.update,
                        SessionUpdate::AgentMessageChunk(ContentChunk {
                            content: ContentBlock::Text(ref text),
                            ..
                        }) if text.text == "审批后完成"
                    ) && let Some(sender) = second_text_seen.lock().await.take()
                    {
                        let _ = sender.send(());
                    }
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _connection| {
                    assert!(request.options.iter().any(|option| {
                        option.option_id == PermissionOptionId::new("allow_once")
                    }));
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            "allow_once",
                        )),
                    ))
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(
                build_acp_agent(daemon.clone(), second_workspace.clone()),
                async move |connection: ConnectionTo<Agent>| {
                    let _ = connection
                        .send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    connection
                        .send_request(LoadSessionRequest::new(
                            SessionId::new(session_id),
                            second_workspace,
                        ))
                        .block_task()
                        .await?;
                    tokio::time::timeout(Duration::from_secs(2), text_seen_rx)
                        .await
                        .expect("恢复 ACP 应收到最终文本")
                        .expect("文本通知通道不应关闭");
                    Ok(())
                },
            )
            .await
            .expect("第二个 ACP 客户端应完成恢复");

        let mut final_snapshot = recovery::load_snapshot(&daemon)
            .await
            .expect("应读取最终快照");
        for _ in 0..100 {
            if final_snapshot.active_requests.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
            final_snapshot = recovery::load_snapshot(&daemon)
                .await
                .expect("应读取最终恢复快照");
        }
        assert!(final_snapshot.pending_approvals.is_empty());
        assert!(final_snapshot.active_requests.is_empty());
        let _ = std::fs::remove_file(session_path);
    }
}
