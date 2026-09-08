use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::context::{ContextConfig, ContextManager};
use crate::loop_engine::LoopEngine;
use crate::plan::PlanStore;
use crate::provider::Provider;
use crate::tools::{Tool, ToolRegistry};

const DEFAULT_MAX_ROUNDS: usize = 15;
const DEFAULT_TOOLS: [&str; 3] = ["read_file", "exec", "recall_memory"];
const SUB_AGENT_SYSTEM_PROMPT: &str = "你是主 Agent 派出的只负责一个明确子任务的子 Agent。你拥有全新且独立的消息历史，不知道主对话内容；只依据用户给出的子任务和可用工具开展工作。优先调查、核验和提炼结论，不扩展任务范围。工具失败时可调整方案。完成后只返回给主 Agent 一份自洽、简洁、包含关键证据的结论。你不能再派生子 Agent。";

pub struct SubAgentTool {
    provider: Arc<dyn Provider>,
    available_tools: ToolRegistry,
    workspace: PathBuf,
    context_config: ContextConfig,
    max_rounds: usize,
}

impl SubAgentTool {
    pub fn new(
        provider: Arc<dyn Provider>,
        available_tools: ToolRegistry,
        workspace: PathBuf,
        context_config: ContextConfig,
    ) -> Self {
        Self {
            provider,
            available_tools,
            workspace,
            context_config,
            max_rounds: DEFAULT_MAX_ROUNDS,
        }
    }

    #[cfg(test)]
    fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds;
        self
    }
}

#[derive(Deserialize)]
struct SubAgentArgs {
    task: String,
    tools: Option<Vec<String>>,
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &str {
        "sub_agent"
    }

    fn description(&self) -> &str {
        "把边界清晰、可独立完成的调研或分析子任务交给一个全新上下文的子 Agent，只取回最终结论；默认仅开放 read_file、exec、recall_memory，可显式指定其他可用工具"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "完整、自洽的子任务描述；子 Agent 不会看到主对话"
                },
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "可选工具名列表；省略时使用 read_file、exec、recall_memory。write_file/edit_file 等写工具必须显式列出"
                }
            },
            "required": ["task"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let args: SubAgentArgs = serde_json::from_value(args).context("sub_agent 参数无效")?;
        let task = args.task.trim();
        if task.is_empty() {
            bail!("sub_agent.task 不能为空");
        }

        let requested = args.tools.unwrap_or_else(|| {
            DEFAULT_TOOLS
                .iter()
                .map(|name: &&str| (*name).to_owned())
                .collect()
        });
        if requested.is_empty() {
            bail!("sub_agent.tools 不能为空数组；若不需要工具请省略该字段");
        }
        let tools = self
            .available_tools
            .subset(requested.iter().map(String::as_str))?;
        let context = ContextManager::with_system_prompt(
            self.provider.clone(),
            &self.workspace,
            self.context_config.clone(),
            Arc::new(PlanStore::memory_only()),
            SUB_AGENT_SYSTEM_PROMPT,
        )?;
        let runner = LoopEngine::ephemeral(self.provider.clone(), tools, context, self.max_rounds);
        runner.run_turn(&mut Vec::new(), task.to_owned()).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::provider::{Message, Response, Role, ToolSpec};

    struct MockProvider {
        responses: Mutex<VecDeque<Response>>,
        snapshots: Mutex<Vec<(Vec<Message>, Vec<ToolSpec>)>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat(&self, messages: &[Message], tools: &[ToolSpec]) -> Result<Response> {
            self.snapshots
                .lock()
                .unwrap()
                .push((messages.to_vec(), tools.to_vec()));
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("mock 响应不足"))
        }
    }

    struct NamedTool(&'static str);

    #[async_trait]
    impl Tool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }

        fn description(&self) -> &str {
            "测试工具"
        }

        fn parameters(&self) -> Value {
            json!({"type": "object"})
        }

        async fn execute(&self, _args: Value) -> Result<String> {
            Ok("ok".to_owned())
        }
    }

    fn config() -> ContextConfig {
        ContextConfig {
            token_budget: 1_000_000,
            recent_messages: 100,
            mild_compression_percent: 60,
            strong_compression_percent: 85,
            summary_chunk_tokens: 100_000,
        }
    }

    fn available_tools() -> ToolRegistry {
        let mut tools = ToolRegistry::new();
        for name in [
            "read_file",
            "exec",
            "recall_memory",
            "write_file",
            "edit_file",
        ] {
            tools.register(NamedTool(name));
        }
        tools
    }

    #[tokio::test]
    async fn uses_fresh_history_and_default_limited_tools() {
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([Response::Text("调研结论".to_owned())])),
            snapshots: Mutex::new(Vec::new()),
        });
        let tool = SubAgentTool::new(
            provider.clone(),
            available_tools(),
            std::env::current_dir().unwrap(),
            config(),
        );

        let result = tool
            .execute(json!({"task": "只调查模块关系"}))
            .await
            .unwrap();

        assert_eq!(result, "调研结论");
        let snapshots = provider.snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 1);
        let (messages, specs) = &snapshots[0];
        assert_eq!(messages[0].role, Role::System);
        assert!(
            messages[0]
                .content
                .as_deref()
                .unwrap()
                .contains("全新且独立")
        );
        assert!(messages.iter().any(|message: &Message| {
            message.role == Role::User && message.content.as_deref() == Some("只调查模块关系")
        }));
        let names = specs
            .iter()
            .map(|spec: &ToolSpec| spec.name.as_str())
            .collect::<Vec<&str>>();
        assert_eq!(names, ["exec", "read_file", "recall_memory"]);
        assert!(!names.contains(&"write_file"));
        assert!(!names.contains(&"sub_agent"));
    }

    #[tokio::test]
    async fn enforces_round_limit_without_persisting_a_session() {
        let provider = Arc::new(MockProvider {
            responses: Mutex::new(VecDeque::from([Response::ToolCalls(Vec::new())])),
            snapshots: Mutex::new(Vec::new()),
        });
        let tool = SubAgentTool::new(
            provider,
            available_tools(),
            std::env::current_dir().unwrap(),
            config(),
        )
        .with_max_rounds(1);

        let error = tool.execute(json!({"task": "不要结束"})).await.unwrap_err();

        assert!(error.to_string().contains("最大轮次 1"));
    }
}
