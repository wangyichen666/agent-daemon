use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use async_trait::async_trait;
use futures_util::{StreamExt, stream};
use tokio::sync::{Notify, mpsc};
use tracing::{debug, warn};

use crate::context::ContextManager;
use crate::provider::{Message, Provider, Response, Role, ToolCall};
use crate::session::SessionStore;
use crate::tool_calls::ToolCallAssembler;
use crate::tools::{ToolCancellation, ToolOutput, ToolRegistry};

pub const DEFAULT_MAX_ROUNDS: usize = 50;
const REPETITION_THRESHOLD: usize = 3;
const REPETITION_REMINDER: &str = "检测到连续重复的工具调用、参数与结果。可以继续使用任何工具，但请先判断该重复是否必要；若没有新信息，考虑换个思路或直接给出结论。";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEvent {
    TurnStarted,
    TextDelta(String),
    ToolStarted {
        call_id: String,
        name: String,
    },
    ToolFinished {
        call_id: String,
        name: String,
        output: String,
    },
    TurnCompleted {
        content: String,
    },
}

#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.inner.notify.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

#[async_trait]
impl ToolCancellation for CancellationToken {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }

    async fn cancelled(&self) {
        self.cancelled().await;
    }
}

#[derive(Clone)]
pub struct LoopEngine {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    context: ContextManager,
    session: Option<Arc<SessionStore>>,
    max_rounds: usize,
}

impl LoopEngine {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        context: ContextManager,
        session: Arc<SessionStore>,
    ) -> Self {
        Self {
            provider,
            tools,
            context,
            session: Some(session),
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }

    pub fn ephemeral(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        context: ContextManager,
        max_rounds: usize,
    ) -> Self {
        Self {
            provider,
            tools,
            context,
            session: None,
            max_rounds,
        }
    }

    pub fn for_session(&self, session: Arc<SessionStore>) -> Self {
        Self {
            provider: self.provider.clone(),
            tools: self.tools.clone(),
            context: self.context.clone(),
            session: Some(session),
            max_rounds: self.max_rounds,
        }
    }

    pub async fn run_turn(&self, history: &mut Vec<Message>, input: String) -> Result<String> {
        self.run_turn_with_events(history, input, None, CancellationToken::new())
            .await
    }

    pub async fn run_turn_with_events(
        &self,
        history: &mut Vec<Message>,
        input: String,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
        cancellation: CancellationToken,
    ) -> Result<String> {
        if cancellation.is_cancelled() {
            bail!("请求已取消");
        }
        let _turn_guard = match &self.session {
            Some(session) => Some(session.lock_turn().await),
            None => None,
        };
        emit(&events, AgentEvent::TurnStarted);
        self.record(history, Message::text(Role::User, input))
            .await?;
        let capabilities = self.provider.capabilities();
        let specs = if capabilities.tools {
            self.tools.specs()
        } else {
            Vec::new()
        };
        let mut transient_messages = Vec::new();
        let mut repeat_detector = RepeatDetector::default();
        let mut repetition_reminder = false;

        for round in 1..=self.max_rounds {
            debug!(round, "开始 ReAct 轮次");
            let mut request_messages = self.context.prepare(history, &specs).await?;
            request_messages.extend(transient_messages.clone());
            if repetition_reminder {
                request_messages.push(Message::text(Role::System, REPETITION_REMINDER));
                repetition_reminder = false;
            }
            let response = self
                .request_model(&request_messages, &specs, &events, &cancellation)
                .await?;
            match response {
                Response::Text(text) => {
                    self.record(history, Message::text(Role::Assistant, text.clone()))
                        .await?;
                    emit(
                        &events,
                        AgentEvent::TurnCompleted {
                            content: text.clone(),
                        },
                    );
                    return Ok(text);
                }
                Response::ToolCalls(calls) => {
                    if let Err(error) = self.tools.admit_all(&calls) {
                        self.record(
                            history,
                            Message::text(
                                Role::System,
                                format!(
                                    "[tool_call_admission_error:{}] {}。本轮没有执行任何工具；请修正全部调用后重试。",
                                    error.code(),
                                    error
                                ),
                            ),
                        )
                        .await?;
                        continue;
                    }
                    self.record(history, Message::assistant_tool_calls(calls.clone()))
                        .await?;
                    let execute = self.execute_in_waves(&calls, events.clone(), &cancellation);
                    tokio::pin!(execute);
                    let results = tokio::select! {
                        results = &mut execute => results,
                        _ = cancellation.cancelled() => bail!("请求已取消"),
                    };
                    for result in results {
                        self.record(history, result.message).await?;
                        transient_messages.extend(result.transient_messages);
                        if repeat_detector.observe(result.fingerprint) {
                            repetition_reminder = true;
                        }
                    }
                }
                Response::ToolAssemblyFailed(error) => {
                    warn!(code = %error.code, message = %error.message, "工具调用装配失败，整轮拒绝");
                    self.record(
                        history,
                        Message::text(
                            Role::System,
                            format!(
                                "[tool_call_assembly_error:{}] {}。本轮没有执行任何工具；请重新生成完整且合法的工具调用。",
                                error.code, error.message
                            ),
                        ),
                    )
                    .await?;
                }
            }
        }

        bail!("ReAct 循环达到最大轮次 {}，已停止", self.max_rounds)
    }

    async fn request_model(
        &self,
        messages: &[Message],
        specs: &[crate::provider::ToolSpec],
        events: &Option<mpsc::UnboundedSender<AgentEvent>>,
        cancellation: &CancellationToken,
    ) -> Result<Response> {
        let request_messages;
        let messages = if self.provider.capabilities().images {
            messages
        } else {
            request_messages = without_images(messages);
            &request_messages
        };
        let (provider_events, mut provider_rx) = mpsc::unbounded_channel();
        let request = self.provider.chat_stream(messages, specs, provider_events);
        tokio::pin!(request);
        let mut assembler = ToolCallAssembler::default();
        loop {
            tokio::select! {
                response = &mut request => {
                    response?;
                    break;
                },
                event = provider_rx.recv() => {
                    if let Some(event) = event
                        && let Some(delta) = assembler.accept(event)
                    {
                        emit(events, AgentEvent::TextDelta(delta));
                    }
                }
                _ = cancellation.cancelled() => bail!("请求已取消"),
            }
        }
        while let Ok(event) = provider_rx.try_recv() {
            if let Some(delta) = assembler.accept(event) {
                emit(events, AgentEvent::TextDelta(delta));
            }
        }
        Ok(assembler.finish())
    }

    async fn record(&self, history: &mut Vec<Message>, message: Message) -> Result<()> {
        if let Some(session) = &self.session {
            session.append(&message).await?;
        }
        history.push(message);
        Ok(())
    }

    async fn execute_in_waves(
        &self,
        calls: &[ToolCall],
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
        cancellation: &CancellationToken,
    ) -> Vec<ToolExecution> {
        let mut results = Vec::with_capacity(calls.len());
        let mut cursor = 0;
        while cursor < calls.len() {
            if self.tools.is_read_only(&calls[cursor].name) {
                let mut end = cursor + 1;
                while end < calls.len() && self.tools.is_read_only(&calls[end].name) {
                    end += 1;
                }
                let batch = stream::iter(calls[cursor..end].to_vec())
                    .map(|call: ToolCall| {
                        let events = events.clone();
                        async move { self.execute_one(&call, events, cancellation).await }
                    })
                    .buffered(8)
                    .collect::<Vec<ToolExecution>>()
                    .await;
                results.extend(batch);
                cursor = end;
            } else {
                results.push(
                    self.execute_one(&calls[cursor], events.clone(), cancellation)
                        .await,
                );
                cursor += 1;
            }
        }
        results
    }

    async fn execute_one(
        &self,
        call: &ToolCall,
        events: Option<mpsc::UnboundedSender<AgentEvent>>,
        cancellation: &CancellationToken,
    ) -> ToolExecution {
        emit(
            &events,
            AgentEvent::ToolStarted {
                call_id: call.id.clone(),
                name: call.name.clone(),
            },
        );
        let result = match self
            .tools
            .execute_with_cancellation(&call.name, call.arguments.clone(), cancellation)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                warn!(tool = %call.name, %error, "工具执行失败，将错误回填给模型");
                ToolOutput::text(format!("工具执行错误: {error:#}"))
            }
        };
        emit(
            &events,
            AgentEvent::ToolFinished {
                call_id: call.id.clone(),
                name: call.name.clone(),
                output: result.content.clone(),
            },
        );
        let fingerprint = ToolFingerprint {
            tool_name: call.name.clone(),
            arguments: fingerprint_json(&call.arguments),
            result: fingerprint_text(&result.content),
        };
        ToolExecution {
            message: Message::tool_result(call, result.content),
            transient_messages: result.transient_messages,
            fingerprint,
        }
    }
}

fn without_images(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .cloned()
        .map(|mut message| {
            if !message.image_urls.is_empty() {
                let note = format!(
                    "[当前 provider 不支持图片，已省略 {} 个图片内容块]",
                    message.image_urls.len()
                );
                message.content = Some(match message.content.take() {
                    Some(content) if !content.is_empty() => format!("{content}\n{note}"),
                    _ => note,
                });
                message.image_urls.clear();
            }
            message
        })
        .collect()
}

fn emit(events: &Option<mpsc::UnboundedSender<AgentEvent>>, event: AgentEvent) {
    if let Some(events) = events {
        let _ = events.send(event);
    }
}

struct ToolExecution {
    message: Message,
    transient_messages: Vec<Message>,
    fingerprint: ToolFingerprint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ToolFingerprint {
    tool_name: String,
    arguments: u64,
    result: u64,
}

#[derive(Default)]
struct RepeatDetector {
    last: Option<ToolFingerprint>,
    count: usize,
}

impl RepeatDetector {
    fn observe(&mut self, fingerprint: ToolFingerprint) -> bool {
        if self.last.as_ref() == Some(&fingerprint) {
            self.count = self.count.saturating_add(1);
        } else {
            self.last = Some(fingerprint);
            self.count = 1;
        }
        self.count == REPETITION_THRESHOLD
    }
}

fn fingerprint_json(value: &serde_json::Value) -> u64 {
    serde_json::to_string(value).map_or_else(
        |_| fingerprint_text("<invalid-json>"),
        |text| fingerprint_text(&text),
    )
}

fn fingerprint_text(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use anyhow::Result;
    use async_trait::async_trait;
    use serde_json::json;

    use super::*;
    use crate::context::ContextConfig;
    use crate::plan::PlanStore;
    use crate::provider::{ToolCall, ToolSpec};
    use crate::safety::{Approval, SafetyPolicy};
    use crate::session::SessionStore;
    use crate::tools::{ExecTool, ReadFileTool, ToolOutput};

    struct AllowApproval;

    #[async_trait]
    impl Approval for AllowApproval {
        async fn request(&self, _prompt: &str) -> Result<bool> {
            Ok(true)
        }
    }

    struct MockProvider {
        responses: Mutex<VecDeque<Response>>,
        snapshots: Mutex<Vec<Vec<Message>>>,
    }

    fn test_context(provider: Arc<dyn Provider>) -> ContextManager {
        ContextManager::new(
            provider,
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
        .unwrap()
    }

    fn test_session(name: &str) -> Arc<SessionStore> {
        let path =
            std::env::temp_dir().join(format!("my-agent-loop-{}-{name}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Arc::new(SessionStore::new(path))
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            self.snapshots.lock().unwrap().push(messages.to_vec());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock 响应不足"))
        }
    }

    #[tokio::test]
    async fn executes_tool_and_pairs_result_by_call_id() {
        let workspace = std::env::current_dir().unwrap();
        let temp_path = workspace.join(format!(
            "my-agent-phase1-{}-{}.txt",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        std::fs::write(&temp_path, "这是 README 内容").unwrap();
        let call = ToolCall {
            id: "call-read-1".to_owned(),
            name: "read_file".to_owned(),
            arguments: json!({"path": temp_path}),
        };
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([
                Response::ToolCalls(vec![call]),
                Response::Text("总结完成".to_owned()),
            ])),
            snapshots: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::new();
        let safety = Arc::new(SafetyPolicy::new(&workspace, Arc::new(AllowApproval)).unwrap());
        registry.register(ReadFileTool::new(safety));
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider.clone()),
            test_session("pairing"),
        );
        let mut history = Vec::new();

        let answer = engine
            .run_turn(&mut history, "读取并总结".to_owned())
            .await
            .unwrap();

        assert_eq!(answer, "总结完成");
        let snapshots = provider.snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2);
        let tool_result = snapshots[1]
            .iter()
            .find(|message: &&Message| {
                message
                    .tool_call_id
                    .as_deref()
                    .and_then(crate::provider::outbound_wire_id)
                    .as_deref()
                    == Some("call-read-1")
            })
            .unwrap();
        assert_eq!(tool_result.content.as_deref(), Some("这是 README 内容"));
        let _ = std::fs::remove_file(temp_path);
    }

    #[tokio::test]
    async fn stops_at_round_limit() {
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([Response::ToolCalls(Vec::new())])),
            snapshots: Mutex::new(Vec::new()),
        });
        let engine = LoopEngine::ephemeral(
            provider.clone(),
            ToolRegistry::new(),
            test_context(provider),
            1,
        );
        let error = engine
            .run_turn(&mut Vec::new(), "继续".to_owned())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("最大轮次"));
    }

    #[tokio::test]
    async fn executes_exec_and_returns_output_to_provider() {
        let call = ToolCall {
            id: "call-exec-1".to_owned(),
            name: "exec".to_owned(),
            arguments: json!({"command": "find . -maxdepth 1 -type f | wc -l"}),
        };
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([
                Response::ToolCalls(vec![call]),
                Response::Text("已完成文件计数".to_owned()),
            ])),
            snapshots: Mutex::new(Vec::new()),
        });
        let safety = Arc::new(
            SafetyPolicy::new(std::env::current_dir().unwrap(), Arc::new(AllowApproval)).unwrap(),
        );
        let mut registry = ToolRegistry::new();
        registry.register(ExecTool::new(safety));
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider.clone()),
            test_session("exec"),
        );

        let answer = engine
            .run_turn(&mut Vec::new(), "统计当前目录文件数".to_owned())
            .await
            .unwrap();

        assert_eq!(answer, "已完成文件计数");
        let snapshots = provider.snapshots.lock().unwrap();
        let result = snapshots[1]
            .iter()
            .find(|message: &&Message| {
                message
                    .tool_call_id
                    .as_deref()
                    .and_then(crate::provider::outbound_wire_id)
                    .as_deref()
                    == Some("call-exec-1")
            })
            .unwrap();
        assert!(result.content.as_deref().unwrap().contains("exit_code: 0"));
    }

    struct ParallelProbe {
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
    }

    struct DelayedReadTool {
        name: &'static str,
        probe: Arc<ParallelProbe>,
    }

    #[async_trait]
    impl crate::tools::Tool for DelayedReadTool {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "并行测试工具"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<String> {
            use std::sync::atomic::Ordering;

            let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.probe.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            self.probe.active.fetch_sub(1, Ordering::SeqCst);
            Ok(self.name.to_owned())
        }
    }

    #[tokio::test]
    async fn runs_consecutive_read_only_calls_in_parallel() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = ["read_a", "read_b", "read_c"]
            .into_iter()
            .enumerate()
            .map(|(index, name): (usize, &str)| ToolCall {
                id: format!("call-{index}"),
                name: name.to_owned(),
                arguments: json!({}),
            })
            .collect::<Vec<ToolCall>>();
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([
                Response::ToolCalls(calls),
                Response::Text("完成".to_owned()),
            ])),
            snapshots: Mutex::new(Vec::new()),
        });
        let probe = Arc::new(ParallelProbe {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        });
        let mut registry = ToolRegistry::new();
        for name in ["read_a", "read_b", "read_c"] {
            registry.register(DelayedReadTool {
                name,
                probe: probe.clone(),
            });
        }
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider),
            test_session("parallel"),
        );

        engine
            .run_turn(&mut Vec::new(), "并行读取".to_owned())
            .await
            .unwrap();

        assert!(probe.peak.load(Ordering::SeqCst) >= 2);
    }

    struct ImageProbeTool;

    #[async_trait]
    impl crate::tools::Tool for ImageProbeTool {
        fn name(&self) -> &str {
            "image_probe"
        }

        fn description(&self) -> &str {
            "返回测试图片"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<String> {
            Ok("已读取图片".to_owned())
        }

        async fn execute_rich(&self, _args: serde_json::Value) -> Result<ToolOutput> {
            Ok(ToolOutput {
                content: "已读取图片".to_owned(),
                transient_messages: vec![Message::user_with_images(
                    "测试图片",
                    vec!["data:image/png;base64,AAAA".to_owned()],
                )],
            })
        }
    }

    #[tokio::test]
    async fn sends_image_transiently_without_persisting_base64() {
        let call = ToolCall {
            id: "call-image-1".to_owned(),
            name: "image_probe".to_owned(),
            arguments: json!({}),
        };
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([
                Response::ToolCalls(vec![call]),
                Response::Text("看到了图片".to_owned()),
            ])),
            snapshots: Mutex::new(Vec::new()),
        });
        let mut registry = ToolRegistry::new();
        registry.register(ImageProbeTool);
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider.clone()),
            test_session("image"),
        );
        let mut history = Vec::new();

        engine
            .run_turn(&mut history, "分析图片".to_owned())
            .await
            .unwrap();

        let snapshots = provider.snapshots.lock().unwrap();
        assert!(
            snapshots[1]
                .iter()
                .any(|message: &Message| !message.image_urls.is_empty())
        );
        assert!(
            history
                .iter()
                .all(|message: &Message| message.image_urls.is_empty())
        );
    }

    struct RepeatingTool {
        executions: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::tools::Tool for RepeatingTool {
        fn name(&self) -> &str {
            "repeat_probe"
        }

        fn description(&self) -> &str {
            "重复检测测试工具"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({"type": "object"})
        }

        fn is_read_only(&self) -> bool {
            true
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<String> {
            self.executions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("相同结果".to_owned())
        }
    }

    #[tokio::test]
    async fn gently_reminds_after_three_identical_tool_results_without_disabling_tool() {
        let calls = (1..=3)
            .map(|index: usize| {
                Response::ToolCalls(vec![ToolCall {
                    id: format!("repeat-{index}"),
                    name: "repeat_probe".to_owned(),
                    arguments: json!({"same": true}),
                }])
            })
            .chain(std::iter::once(Response::Text("换个思路后完成".to_owned())))
            .collect::<VecDeque<Response>>();
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(calls),
            snapshots: Mutex::new(Vec::new()),
        });
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(RepeatingTool {
            executions: executions.clone(),
        });
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider.clone()),
            test_session("repeat"),
        );

        let answer = engine
            .run_turn(&mut Vec::new(), "触发重复".to_owned())
            .await
            .unwrap();

        assert_eq!(answer, "换个思路后完成");
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 3);
        let snapshots = provider.snapshots.lock().unwrap();
        assert!(snapshots[3].iter().any(|message: &Message| {
            message.role == Role::System
                && message
                    .content
                    .as_deref()
                    .is_some_and(|content: &str| content.contains("检测到连续重复"))
        }));
        assert!(
            engine
                .tools
                .specs()
                .iter()
                .any(|spec: &ToolSpec| spec.name == "repeat_probe")
        );
    }

    struct InvalidArgumentsStream {
        requests: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl Provider for InvalidArgumentsStream {
        async fn chat_stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            events: tokio::sync::mpsc::UnboundedSender<crate::provider::ProviderEvent>,
        ) -> Result<()> {
            use crate::provider::{
                ApiType, ExecutionIdentity, ProviderEvent, ToolArgumentsFragment,
            };
            if self
                .requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                for (wire_id, arguments) in [("first", "{}"), ("broken", "{")] {
                    let exec_id = ExecutionIdentity::wire(ApiType::OpenaiChat, wire_id);
                    events.send(ProviderEvent::ToolCallStarted {
                        exec_id: exec_id.clone(),
                        name: "side_effect".to_owned(),
                    })?;
                    events.send(ProviderEvent::ToolCallDelta {
                        exec_id: exec_id.clone(),
                        fragment: ToolArgumentsFragment::Append(arguments.to_owned()),
                    })?;
                    events.send(ProviderEvent::ToolCallCompleted { exec_id })?;
                }
            } else {
                events.send(ProviderEvent::TextDelta("已修正".to_owned()))?;
            }
            Ok(())
        }
    }

    struct SideEffectProbe {
        name: &'static str,
        executions: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait]
    impl crate::tools::Tool for SideEffectProbe {
        fn name(&self) -> &str {
            self.name
        }

        fn description(&self) -> &str {
            "原子拒绝测试工具"
        }

        fn parameters(&self) -> serde_json::Value {
            json!({
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"],
                "additionalProperties": false
            })
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<String> {
            self.executions
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("executed".to_owned())
        }
    }

    #[tokio::test]
    async fn assembly_failure_rejects_the_whole_wave_before_side_effects() {
        let provider = Arc::new(InvalidArgumentsStream {
            requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register(SideEffectProbe {
            name: "side_effect",
            executions: executions.clone(),
        });
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider),
            test_session("assembly-fail-closed"),
        );
        let mut history = Vec::new();

        let answer = engine
            .run_turn(&mut history, "测试装配失败".to_owned())
            .await
            .unwrap();

        assert_eq!(answer, "已修正");
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(history.iter().any(|message| {
            message
                .content
                .as_deref()
                .is_some_and(|content| content.contains("tool_call_assembly_error:invalid_json"))
        }));
    }

    #[tokio::test]
    async fn admission_failure_rejects_all_calls_before_side_effects() {
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([
                Response::ToolCalls(vec![
                    ToolCall {
                        id: "valid".to_owned(),
                        name: "first_effect".to_owned(),
                        arguments: json!({"value": "ok"}),
                    },
                    ToolCall {
                        id: "invalid".to_owned(),
                        name: "second_effect".to_owned(),
                        arguments: json!({}),
                    },
                ]),
                Response::Text("已修正".to_owned()),
            ])),
            snapshots: Mutex::new(Vec::new()),
        });
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        for name in ["first_effect", "second_effect"] {
            registry.register(SideEffectProbe {
                name,
                executions: executions.clone(),
            });
        }
        let engine = LoopEngine::new(
            provider.clone(),
            registry,
            test_context(provider),
            test_session("admission-fail-closed"),
        );

        let answer = engine
            .run_turn(&mut Vec::new(), "测试准入失败".to_owned())
            .await
            .unwrap();

        assert_eq!(answer, "已修正");
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
