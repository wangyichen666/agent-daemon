use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use super::DaemonState;
use super::lifecycle::RuntimePaths;
#[cfg(test)]
use super::protocol::JsonRpcRequest;
use super::protocol::{
    JsonRpcResponse, MAX_FRAME_BYTES, RequestId, ServerFrame, decode_request, encode_frame,
    server_frame_request_id,
};
#[cfg(test)]
use crate::client::DaemonClient;

#[cfg(test)]
pub(crate) struct InMemoryEnvelope {
    pub request: JsonRpcRequest,
    pub frames: mpsc::UnboundedSender<ServerFrame>,
}

#[cfg(test)]
pub struct InMemoryServer;

#[cfg(test)]
impl InMemoryServer {
    pub fn start(state: Arc<DaemonState>) -> DaemonClient {
        let (requests, mut receiver) = mpsc::channel::<InMemoryEnvelope>(64);
        tokio::spawn(async move {
            while let Some(envelope) = receiver.recv().await {
                let state = state.clone();
                tokio::spawn(async move {
                    state
                        .handle_request(envelope.request, envelope.frames)
                        .await;
                });
            }
        });
        DaemonClient::in_memory(requests)
    }
}

pub async fn run_unix_server(
    state: Arc<DaemonState>,
    paths: &RuntimePaths,
    workspace: &std::path::Path,
) -> Result<()> {
    paths.prepare().await?;
    if paths.socket.exists() {
        tokio::fs::remove_file(&paths.socket)
            .await
            .with_context(|| format!("清理旧 socket 失败: {}", paths.socket.display()))?;
    }
    let listener = UnixListener::bind(&paths.socket)
        .with_context(|| format!("监听 Unix socket 失败: {}", paths.socket.display()))?;
    paths.mark_ready(workspace).await?;
    let (connection_done, mut done_receiver) = mpsc::unbounded_channel::<()>();
    let mut clients = 0usize;
    let mut accepted_any = false;
    let mut idle_since = None;
    let mut lifecycle_tick = tokio::time::interval(Duration::from_millis(250));
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("接受 daemon 客户端连接失败")?;
                clients = clients.saturating_add(1);
                accepted_any = true;
                idle_since = None;
                let state = state.clone();
                let connection_done = connection_done.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_unix_connection(stream, state).await {
                        tracing::warn!(%error, "daemon 客户端连接异常结束");
                    }
                    let _ = connection_done.send(());
                });
            }
            Some(()) = done_receiver.recv() => {
                clients = clients.saturating_sub(1);
                if clients == 0 {
                    idle_since = Some(tokio::time::Instant::now());
                }
            }
            _ = lifecycle_tick.tick() => {
                if accepted_any
                    && clients == 0
                    && !state.has_active_turns().await
                    && !state.has_persistent_background_work().await
                    && idle_since.is_some_and(|since| since.elapsed() >= Duration::from_secs(2))
                {
                    break;
                }
            }
            _ = state.shutdown.cancelled() => break,
            result = &mut interrupt => {
                result.context("监听 daemon Ctrl-C 失败")?;
                break;
            }
        }
    }

    state.shutdown.cancel();
    while state.has_active_turns().await {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    state.join_background().await;
    paths.cleanup().await;
    Ok(())
}

async fn serve_unix_connection(stream: UnixStream, state: Arc<DaemonState>) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let (frames, mut frame_receiver) = mpsc::unbounded_channel::<ServerFrame>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = frame_receiver.recv().await {
            let encoded = match encode_frame(&frame) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let fallback = ServerFrame::Response(JsonRpcResponse::failure(
                        server_frame_request_id(&frame).clone(),
                        -32002,
                        error.to_string(),
                    ));
                    encode_frame(&fallback).context("编码超限错误响应失败")?
                }
            };
            writer
                .write_all(&encoded)
                .await
                .context("写入 daemon 响应失败")?;
            writer.flush().await.context("刷新 daemon 响应失败")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let mut reader = BufReader::new(reader);
    loop {
        let mut line = Vec::new();
        let read = reader
            .read_until(b'\n', &mut line)
            .await
            .context("读取 daemon 请求失败")?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_FRAME_BYTES + 1 {
            let _ = frames.send(ServerFrame::Response(JsonRpcResponse::failure(
                RequestId::String("protocol".to_owned()),
                -32002,
                format!("协议帧超过 {MAX_FRAME_BYTES} 字节限制"),
            )));
            continue;
        }
        let request = match decode_request(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = frames.send(ServerFrame::Response(JsonRpcResponse::failure(
                    RequestId::String("protocol".to_owned()),
                    -32700,
                    error.to_string(),
                )));
                continue;
            }
        };
        let request_frames = frames.clone();
        let state = state.clone();
        tokio::spawn(async move {
            state.handle_request(request, request_frames).await;
        });
    }
    drop(frames);
    writer_task.await.context("daemon 响应写入任务异常终止")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::context::{ContextConfig, ContextManager};
    use crate::cron::{CronJob, CronManager, CronStore, ScheduledJobRunner};
    use crate::daemon::approval::ApprovalBroker;
    use crate::daemon::protocol::{EventKind, JsonRpcResponse, RequestId};
    use crate::loop_engine::LoopEngine;
    use crate::mcp::McpManager;
    use crate::plan::PlanStore;
    use crate::provider::{Message, Provider, Response, ToolSpec};
    use crate::safety::{Approval, SafetyPolicy};
    use crate::session::SessionStore;
    use crate::skills::SkillLibrary;
    use crate::slash::SlashResponse;
    use crate::tools::{Tool, ToolRegistry};

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    struct MockProvider {
        responses: Mutex<VecDeque<Response>>,
    }

    struct PendingProvider;

    struct ApprovalTool {
        approvals: ApprovalBroker,
    }

    struct SuccessfulCronRunner;

    #[async_trait]
    impl ScheduledJobRunner for SuccessfulCronRunner {
        async fn run(&self, job: &CronJob) -> Result<String> {
            Ok(format!("完成：{}", job.prompt))
        }
    }

    #[async_trait]
    impl Tool for ApprovalTool {
        fn name(&self) -> &str {
            "danger"
        }

        fn description(&self) -> &str {
            "测试断线审批恢复"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String> {
            if self.approvals.request("执行断线恢复测试动作").await? {
                Ok("approved".to_owned())
            } else {
                anyhow::bail!("测试动作被拒绝")
            }
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock 响应不足"))
        }
    }

    #[async_trait]
    impl Provider for PendingProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            std::future::pending().await
        }
    }

    async fn state_with_provider(
        provider: Arc<dyn Provider>,
    ) -> (Arc<DaemonState>, std::path::PathBuf) {
        state_with_provider_and_skills(provider, None).await
    }

    async fn state_with_provider_and_skills(
        provider: Arc<dyn Provider>,
        skills: Option<SkillLibrary>,
    ) -> (Arc<DaemonState>, std::path::PathBuf) {
        state_with_provider_and_services(provider, skills, None, None).await
    }

    async fn state_with_provider_and_services(
        provider: Arc<dyn Provider>,
        skills: Option<SkillLibrary>,
        cron: Option<Arc<CronManager>>,
        mcp: Option<Arc<McpManager>>,
    ) -> (Arc<DaemonState>, std::path::PathBuf) {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path =
            std::env::temp_dir().join(format!("my-agent-daemon-{}-{id}.jsonl", std::process::id()));
        let session = Arc::new(SessionStore::new(&session_path));
        let context = ContextManager::new(
            provider.clone(),
            std::env::current_dir().unwrap(),
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let engine = Arc::new(LoopEngine::new(
            provider,
            ToolRegistry::new(),
            context,
            session.clone(),
        ));
        (
            Arc::new(DaemonState::new_with_services(
                engine,
                Vec::new(),
                session,
                ApprovalBroker::new(),
                skills,
                cron,
                mcp,
            )),
            session_path,
        )
    }

    #[tokio::test]
    async fn slash_cron_commands_share_persistent_store_and_runner() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let workspace =
            std::env::temp_dir().join(format!("my-agent-slash-cron-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let store = Arc::new(CronStore::load(&workspace).await.unwrap());
        let cron = Arc::new(CronManager::new(
            store,
            Arc::new(SuccessfulCronRunner),
            Duration::from_secs(60),
            Duration::ZERO,
            None,
        ));
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::new()),
        });
        let (state, session_path) =
            state_with_provider_and_services(provider, None, Some(cron), None).await;
        let client = InMemoryServer::start(state);

        async fn slash(client: &DaemonClient, line: &str) -> SlashResponse {
            let value =
                crate::entry::cli::request_result(client, "slash.execute", json!({"line": line}))
                    .await
                    .unwrap();
            serde_json::from_value(value).unwrap()
        }

        let added = slash(
            &client,
            "/cron add smoke interval=60 --retries=1 --backoff=0 执行巡检",
        )
        .await;
        assert!(
            matches!(added, SlashResponse::Text { content } if content.contains("已添加 smoke"))
        );
        let listed = slash(&client, "/cron list").await;
        assert!(
            matches!(listed, SlashResponse::Text { content } if content.contains("smoke") && content.contains("interval=60s"))
        );
        let disabled = slash(&client, "/cron disable smoke").await;
        assert!(matches!(disabled, SlashResponse::Text { content } if content.contains("已停用")));
        let ran = slash(&client, "/cron run-now smoke").await;
        assert!(
            matches!(ran, SlashResponse::Text { content } if content.contains("立即运行成功") && content.contains("执行巡检"))
        );
        let refused = slash(&client, "/cron remove smoke").await;
        assert!(
            matches!(refused, SlashResponse::Text { content } if content.contains("--confirm"))
        );
        let removed = slash(&client, "/cron remove smoke --confirm").await;
        assert!(
            matches!(removed, SlashResponse::Text { content } if content.contains("已删除 smoke"))
        );

        let _ = std::fs::remove_file(session_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn slash_mcp_list_status_and_reload_share_daemon_manager() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let workspace =
            std::env::temp_dir().join(format!("my-agent-slash-mcp-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = std::fs::canonicalize(workspace).unwrap();
        let approvals = ApprovalBroker::new();
        let safety = Arc::new(SafetyPolicy::new(&workspace, Arc::new(approvals)).unwrap());
        let mcp = Arc::new(McpManager::new(&workspace, safety));
        mcp.reload().await;
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::new()),
        });
        let (state, session_path) =
            state_with_provider_and_services(provider, None, None, Some(mcp)).await;
        let client = InMemoryServer::start(state);

        for line in ["/mcp list", "/mcp status", "/mcp reload"] {
            let value =
                crate::entry::cli::request_result(&client, "slash.execute", json!({"line": line}))
                    .await
                    .unwrap();
            let response: SlashResponse = serde_json::from_value(value).unwrap();
            assert!(
                matches!(response, SlashResponse::Text { content } if content.contains("未配置 MCP server"))
            );
        }

        let _ = std::fs::remove_file(session_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    async fn test_state() -> (Arc<DaemonState>, std::path::PathBuf) {
        state_with_provider(Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([Response::Text("回环回答".to_owned())])),
        }))
        .await
    }

    #[tokio::test]
    async fn slash_skill_commands_share_the_daemon_owned_library() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let workspace =
            std::env::temp_dir().join(format!("my-agent-slash-skill-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        let source = workspace.join("demo source.md");
        std::fs::write(
            &source,
            "---\nname: demo\nversion: 1.0.0\ndescription: 演示 skill\nkeywords: [demo]\nscope: [rust]\n---\n正文\n",
        )
        .unwrap();
        let skills = SkillLibrary::from_env(&workspace);
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::new()),
        });
        let (state, session_path) = state_with_provider_and_skills(provider, Some(skills)).await;
        let client = InMemoryServer::start(state);

        async fn slash(client: &DaemonClient, line: &str) -> SlashResponse {
            let value =
                crate::entry::cli::request_result(client, "slash.execute", json!({"line": line}))
                    .await
                    .unwrap();
            serde_json::from_value(value).unwrap()
        }

        let installed = slash(&client, &format!("/skill install {}", source.display())).await;
        assert!(
            matches!(installed, SlashResponse::Text { content } if content.contains("已安装 demo@1.0.0"))
        );
        let listed = slash(&client, "/skill list").await;
        assert!(
            matches!(listed, SlashResponse::Text { content } if content.contains("demo@1.0.0"))
        );
        let refused = slash(&client, "/skill remove demo").await;
        assert!(
            matches!(refused, SlashResponse::Text { content } if content.contains("--confirm"))
        );
        let removed = slash(&client, "/skill remove demo --confirm").await;
        assert!(matches!(removed, SlashResponse::Text { content } if content.contains("已删除")));

        let _ = std::fs::remove_file(session_path);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn streams_chat_and_exposes_same_session_through_rpc() {
        let (state, session_path) = test_state().await;
        let client = InMemoryServer::start(state);
        let mut chat = client
            .request("chat.send", json!({"message": "你好"}))
            .await
            .unwrap();
        let mut events = Vec::new();
        let response = loop {
            match chat.next().await.unwrap() {
                ServerFrame::Event(event) => events.push(event.event),
                ServerFrame::Response(response) => break response,
            }
        };

        assert_eq!(response.result.unwrap()["content"], "回环回答");
        assert_eq!(
            events,
            [
                EventKind::TurnStarted,
                EventKind::TextDelta,
                EventKind::TurnCompleted,
            ]
        );

        let mut load = client.request("session.load", json!({})).await.unwrap();
        let ServerFrame::Response(JsonRpcResponse { result, .. }) = load.next().await.unwrap()
        else {
            panic!("预期 session.load 响应");
        };
        assert_eq!(result.unwrap()["messages"].as_array().unwrap().len(), 2);

        let mut list = client.request("session.list", json!({})).await.unwrap();
        let ServerFrame::Response(JsonRpcResponse { result, .. }) = list.next().await.unwrap()
        else {
            panic!("预期 session.list 响应");
        };
        let sessions = result.unwrap();
        assert_eq!(sessions["sessions"][0]["active"], true);
        assert_eq!(sessions["sessions"][0]["message_count"], 2);
        assert_eq!(sessions["sessions"][0]["preview"], "你好");
        let original_session_id = sessions["sessions"][0]["id"].as_str().unwrap().to_owned();

        let mut new_session = client.request("session.new", json!({})).await.unwrap();
        let ServerFrame::Response(new_session) = new_session.next().await.unwrap() else {
            panic!("预期 session.new 响应");
        };
        let new_session = new_session.result.unwrap();
        assert_ne!(new_session["session_id"], original_session_id);
        assert!(new_session["messages"].as_array().unwrap().is_empty());

        let mut resume = client
            .request("session.resume", json!({"session_id": original_session_id}))
            .await
            .unwrap();
        let ServerFrame::Response(resume) = resume.next().await.unwrap() else {
            panic!("预期 session.resume 响应");
        };
        let resumed = resume.result.unwrap();
        assert_eq!(resumed["messages"].as_array().unwrap().len(), 2);
        assert_eq!(resumed["session_id"], original_session_id);
        let _ = std::fs::remove_file(SessionStore::pointer_path(&session_path));
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn independent_sessions_run_and_snapshot_without_blocking_each_other() {
        let (state, session_path) = state_with_provider(Arc::new(PendingProvider)).await;
        let client = InMemoryServer::start(state);

        async fn new_session(client: &DaemonClient) -> String {
            let value = crate::entry::cli::request_result(client, "session.new", json!({}))
                .await
                .unwrap();
            value["session_id"].as_str().unwrap().to_owned()
        }

        let first_session = new_session(&client).await;
        let second_session = new_session(&client).await;
        assert_ne!(first_session, second_session);

        let mut first = client
            .request(
                "chat.send",
                json!({"message": "第一个窗口", "session_id": first_session}),
            )
            .await
            .unwrap();
        let first_request = first.request_id().clone();
        let mut second = client
            .request(
                "chat.send",
                json!({"message": "第二个窗口", "session_id": second_session}),
            )
            .await
            .unwrap();
        let second_request = second.request_id().clone();

        for stream in [&mut first, &mut second] {
            let frame = tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                frame,
                ServerFrame::Event(event) if event.event == EventKind::TurnStarted
            ));
        }

        for session_id in [&first_session, &second_session] {
            let mut snapshot = Value::Null;
            for _ in 0..100 {
                snapshot = crate::entry::cli::request_result(
                    &client,
                    "session.load",
                    json!({"session_id": session_id}),
                )
                .await
                .unwrap();
                if snapshot["messages"]
                    .as_array()
                    .is_some_and(|messages| messages.len() == 1)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(snapshot["active_requests"].as_array().unwrap().len(), 1);
            assert_eq!(snapshot["messages"].as_array().unwrap().len(), 1);

            let listed = crate::entry::cli::request_result(&client, "session.list", json!({}))
                .await
                .unwrap();
            let info = listed["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|session| session["id"] == *session_id)
                .expect("活动 session 应出现在 session.list");
            assert_eq!(info["status"], "running");
            assert_eq!(info["active_requests"], 1);
        }

        let new_session = tokio::time::timeout(
            Duration::from_secs(1),
            crate::entry::cli::request_result(&client, "session.new", json!({})),
        )
        .await
        .expect("新窗口不应等待其它窗口的活动 turn")
        .unwrap();
        assert!(new_session["created"].as_bool().unwrap());

        for (request_id, session_id) in [
            (&first_request, &first_session),
            (&second_request, &second_session),
        ] {
            let cancelled = crate::entry::cli::request_result(
                &client,
                "agent.cancel",
                json!({"request_id": request_id, "session_id": session_id}),
            )
            .await
            .unwrap();
            assert!(cancelled["cancelled"].as_bool().unwrap());
        }
        for stream in [&mut first, &mut second] {
            let terminal = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let Some(ServerFrame::Response(response)) = stream.next().await {
                        break response;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(terminal.error.unwrap().code, -32800);
        }

        for session_id in [first_session, second_session] {
            let _ = std::fs::remove_file(session_path.with_file_name(session_id));
        }
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn cancels_an_active_turn_by_request_id() {
        let (state, session_path) = state_with_provider(Arc::new(PendingProvider)).await;
        let client = InMemoryServer::start(state);
        let turn_id = RequestId::String("slow-turn".to_owned());
        let mut chat = client
            .request_with_id(turn_id.clone(), "chat.send", json!({"message": "等待"}))
            .await
            .unwrap();
        let started = tokio::time::timeout(std::time::Duration::from_secs(1), chat.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            started,
            ServerFrame::Event(event) if event.event == EventKind::TurnStarted
        ));

        let mut cancel = client
            .request("agent.cancel", json!({"request_id": turn_id}))
            .await
            .unwrap();
        let ServerFrame::Response(cancelled) = cancel.next().await.unwrap() else {
            panic!("预期取消响应");
        };
        assert_eq!(cancelled.result.unwrap()["cancelled"], true);

        let terminal = tokio::time::timeout(std::time::Duration::from_secs(1), chat.next())
            .await
            .unwrap()
            .unwrap();
        let ServerFrame::Response(response) = terminal else {
            panic!("预期取消终态响应");
        };
        assert_eq!(response.error.unwrap().code, -32800);
        let _ = std::fs::remove_file(session_path);
    }

    #[tokio::test]
    async fn unix_socket_uses_the_same_handlers_and_stops_cleanly() {
        let (state, session_path) = test_state().await;
        let runtime_directory = std::env::temp_dir().join(format!(
            "my-agent-unix-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::SeqCst)
        ));
        let paths = RuntimePaths::for_test(runtime_directory.clone());
        let server_paths = paths.clone();
        let workspace = std::env::current_dir().unwrap();
        let server =
            tokio::spawn(async move { run_unix_server(state, &server_paths, &workspace).await });
        for _ in 0..100 {
            if paths.socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let client = DaemonClient::connect_unix(&paths.socket).await.unwrap();
        let mut chat = client
            .request("chat.send", json!({"message": "你好"}))
            .await
            .unwrap();
        loop {
            if matches!(chat.next().await.unwrap(), ServerFrame::Response(_)) {
                break;
            }
        }
        let mut stop = client.request("daemon.stop", json!({})).await.unwrap();
        assert!(matches!(
            stop.next().await.unwrap(),
            ServerFrame::Response(JsonRpcResponse { error: None, .. })
        ));
        drop(client);
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();

        let _ = std::fs::remove_file(session_path);
        let _ = std::fs::remove_dir_all(runtime_directory);
    }

    #[tokio::test]
    async fn dropped_stream_can_resubscribe_and_resolve_pending_approval() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let session_path = std::env::temp_dir().join(format!(
            "my-agent-reconnect-{}-{id}.jsonl",
            std::process::id()
        ));
        let provider: Arc<dyn Provider> = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([
                Response::ToolCalls(vec![crate::provider::ToolCall {
                    id: "reconnect-danger".to_owned(),
                    name: "danger".to_owned(),
                    arguments: json!({}),
                }]),
                Response::Text("恢复后完成".to_owned()),
            ])),
        });
        let approvals = ApprovalBroker::new();
        let mut tools = ToolRegistry::new();
        tools.register(ApprovalTool {
            approvals: approvals.clone(),
        });
        let session = Arc::new(SessionStore::new(&session_path));
        let context = ContextManager::new(
            provider.clone(),
            std::env::current_dir().unwrap(),
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let engine = Arc::new(LoopEngine::new(provider, tools, context, session.clone()));
        let state = Arc::new(DaemonState::new(engine, Vec::new(), session, approvals));
        let client = InMemoryServer::start(state);
        let original_id = RequestId::String("lost-client-turn".to_owned());
        let mut original = client
            .request_with_id(
                original_id.clone(),
                "chat.send",
                json!({"message": "执行恢复测试"}),
            )
            .await
            .unwrap();
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(2), original.next())
                .await
                .expect("等待原连接审批事件超时")
                .expect("原连接在审批事件前关闭");
            if matches!(
                frame,
                ServerFrame::Event(ref event) if event.event == EventKind::ApprovalRequired
            ) {
                break;
            }
        }
        drop(original);

        let mut load = client.request("session.load", json!({})).await.unwrap();
        let ServerFrame::Response(load) = tokio::time::timeout(Duration::from_secs(2), load.next())
            .await
            .expect("等待恢复快照超时")
            .expect("恢复快照流提前关闭")
        else {
            panic!("预期恢复快照响应");
        };
        let snapshot = load.result.unwrap();
        assert_eq!(snapshot["pending_approvals"].as_array().unwrap().len(), 1);
        assert_eq!(snapshot["active_requests"][0], json!(original_id));
        let approval_id = snapshot["pending_approvals"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();

        let mut subscription = client
            .request("agent.subscribe", json!({"request_id": original_id}))
            .await
            .unwrap();
        let mut approval_response = client
            .request(
                "approval.respond",
                json!({"approval_id": approval_id, "approved": true}),
            )
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), approval_response.next())
                .await
                .expect("等待审批响应超时")
                .expect("审批响应流提前关闭"),
            ServerFrame::Response(JsonRpcResponse { error: None, .. })
        ));

        let mut saw_text = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), subscription.next())
                .await
                .expect("等待恢复订阅事件超时")
                .expect("恢复订阅流提前关闭")
            {
                ServerFrame::Event(event) if event.event == EventKind::TextDelta => {
                    saw_text |= event.data["delta"] == "恢复后完成";
                }
                ServerFrame::Response(response) => {
                    assert!(response.error.is_none());
                    break;
                }
                ServerFrame::Event(_) => {}
            }
        }
        assert!(saw_text);
        let _ = std::fs::remove_file(session_path);
    }
}
