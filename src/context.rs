use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tracing::{debug, warn};

use crate::plan::PlanStore;
use crate::provider::{Message, Provider, Response, Role, ToolSpec};
use crate::skills::SkillLibrary;

const DEFAULT_SYSTEM_PROMPT: &str = "你是一个个人 AI 编码 Agent。先理解任务，再按需调用工具；工具失败时根据错误调整方案；任务完成后给出简洁、可核验的最终回答。面对需要三个或更多步骤的复杂任务，先调用 plan 的 set 制定计划，开始和完成每一步时用 update 更新状态，必要时用 add 调整；简单单步任务不要使用 plan，避免形式主义。若动态上下文提供了命中的技能正文，应把它作为当前任务的工作方法；未命中的技能只有索引，不要假装已读取其正文。";
const DEFAULT_TOKEN_BUDGET: usize = 32_000;
const DEFAULT_RECENT_MESSAGES: usize = 12;
const MAX_PROJECT_RULE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct ContextConfig {
    pub token_budget: usize,
    pub recent_messages: usize,
    pub mild_compression_percent: usize,
    pub strong_compression_percent: usize,
    pub summary_chunk_tokens: usize,
}

impl ContextConfig {
    pub fn from_env() -> Result<Self> {
        let token_budget = optional_usize("CONTEXT_TOKEN_BUDGET", DEFAULT_TOKEN_BUDGET)?;
        let recent_messages = optional_usize("CONTEXT_RECENT_MESSAGES", DEFAULT_RECENT_MESSAGES)?;
        let mild_compression_percent = optional_usize("CONTEXT_MILD_PERCENT", 60)?;
        let strong_compression_percent = optional_usize("CONTEXT_STRONG_PERCENT", 85)?;
        if token_budget < 256 {
            bail!("CONTEXT_TOKEN_BUDGET 不能小于 256");
        }
        if recent_messages == 0 {
            bail!("CONTEXT_RECENT_MESSAGES 不能为 0");
        }
        if mild_compression_percent == 0
            || mild_compression_percent >= strong_compression_percent
            || strong_compression_percent > 100
        {
            bail!("压缩阈值必须满足 0 < CONTEXT_MILD_PERCENT < CONTEXT_STRONG_PERCENT <= 100");
        }
        Ok(Self {
            token_budget,
            recent_messages,
            mild_compression_percent,
            strong_compression_percent,
            summary_chunk_tokens: (token_budget / 3).max(128),
        })
    }
}

pub struct ContextManager {
    provider: Arc<dyn Provider>,
    workspace: PathBuf,
    system_prompt: String,
    project_rules: Option<String>,
    config: ContextConfig,
    plan: Arc<PlanStore>,
    skills: SkillLibrary,
}

impl ContextManager {
    pub fn new(
        provider: Arc<dyn Provider>,
        workspace: impl AsRef<Path>,
        config: ContextConfig,
        plan: Arc<PlanStore>,
    ) -> Result<Self> {
        Self::with_system_prompt(provider, workspace, config, plan, DEFAULT_SYSTEM_PROMPT)
    }

    pub fn with_system_prompt(
        provider: Arc<dyn Provider>,
        workspace: impl AsRef<Path>,
        config: ContextConfig,
        plan: Arc<PlanStore>,
        system_prompt: impl Into<String>,
    ) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace.as_ref())
            .with_context(|| format!("无法解析上下文工作区: {}", workspace.as_ref().display()))?;
        let project_rules = load_project_rules(&workspace)?;
        let skills = SkillLibrary::from_env(&workspace);
        Ok(Self {
            provider,
            workspace,
            system_prompt: system_prompt.into(),
            project_rules,
            config,
            plan,
            skills,
        })
    }

    pub async fn prepare(
        &self,
        history: &mut Vec<Message>,
        tools: &[ToolSpec],
    ) -> Result<Vec<Message>> {
        let before = self.compose(history).await;
        let total = estimate_messages(&before) + estimate_json_tokens(tools)?;
        let mild_threshold = self
            .config
            .token_budget
            .saturating_mul(self.config.mild_compression_percent)
            / 100;
        let strong_threshold = self
            .config
            .token_budget
            .saturating_mul(self.config.strong_compression_percent)
            / 100;
        if total > strong_threshold {
            debug!(
                estimated_tokens = total,
                threshold = strong_threshold,
                "上下文超过强力压缩闸门"
            );
            self.compress_history(history, CompressionMode::Strong)
                .await?;
        } else if total > mild_threshold {
            debug!(
                estimated_tokens = total,
                threshold = mild_threshold,
                "上下文超过温和压缩闸门"
            );
            self.compress_history(history, CompressionMode::Mild)
                .await?;
        }
        Ok(self.compose(history).await)
    }

    async fn compose(&self, history: &[Message]) -> Vec<Message> {
        let mut messages = Vec::with_capacity(history.len() + 3);
        messages.push(Message::text(Role::System, &self.system_prompt));
        if let Some(rules) = &self.project_rules {
            messages.push(Message::text(
                Role::System,
                format!("项目规则（AGENTS.md）：\n{rules}"),
            ));
        }
        messages.extend_from_slice(history);
        messages.push(Message::text(
            Role::System,
            self.dynamic_environment(history).await,
        ));
        messages
    }

    async fn dynamic_environment(&self, history: &[Message]) -> String {
        let branch = tokio::process::Command::new("git")
            .arg("branch")
            .arg("--show-current")
            .current_dir(&self.workspace)
            .output()
            .await
            .ok()
            .filter(|output: &std::process::Output| output.status.success())
            .map(|output: std::process::Output| {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            })
            .filter(|branch: &String| !branch.is_empty())
            .unwrap_or_else(|| "（非 Git 仓库或 detached HEAD）".to_owned());
        let mut environment = format!(
            "动态环境：cwd={}；git_branch={branch}",
            self.workspace.display()
        );
        if let Some(plan) = self.plan.context_block().await {
            environment.push('\n');
            environment.push_str(&plan);
        }
        let query = history
            .iter()
            .rev()
            .find(|message: &&Message| message.role == Role::User)
            .and_then(|message: &Message| message.content.as_deref())
            .unwrap_or_default();
        if let Some(skills) = self.skills.context_for(query).await {
            environment.push('\n');
            environment.push_str(&skills);
        }
        environment
    }

    async fn compress_history(
        &self,
        history: &mut Vec<Message>,
        mode: CompressionMode,
    ) -> Result<()> {
        if history.len() <= self.config.recent_messages {
            warn!(
                messages = history.len(),
                "上下文过大，但没有可压缩的旧消息；保留当前输入"
            );
            return Ok(());
        }

        let compressible = history.len() - self.config.recent_messages;
        let mut split = match mode {
            CompressionMode::Mild => compressible.div_ceil(3).max(1),
            CompressionMode::Strong => compressible,
        };
        while split > 0
            && history
                .get(split)
                .is_some_and(|item: &Message| item.role == Role::Tool)
        {
            split -= 1;
        }
        if split == 0 {
            return Ok(());
        }

        let old = history[..split].to_vec();
        let serialized = serde_json::to_string(&old).context("序列化待压缩历史失败")?;
        let summary = self.summarize_text(serialized, 0).await?;
        let recent = history.split_off(split);
        history.clear();
        history.push(Message::text(
            Role::System,
            format!("此前对话摘要：\n{summary}"),
        ));
        history.extend(recent);
        Ok(())
    }

    fn summarize_text<'a>(
        &'a self,
        text: String,
        depth: usize,
    ) -> futures_util::future::BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            if depth >= 8 {
                return Ok(truncate_chars(
                    &text,
                    self.config.summary_chunk_tokens.saturating_mul(3),
                ));
            }
            if estimate_text_tokens(&text) <= self.config.summary_chunk_tokens {
                return self.request_summary(&text).await;
            }

            let chunks = split_for_token_budget(&text, self.config.summary_chunk_tokens);
            let mut summaries = Vec::with_capacity(chunks.len());
            for chunk in chunks {
                summaries.push(self.request_summary(&chunk).await?);
            }
            let merged = summaries.join("\n\n");
            self.summarize_text(merged, depth + 1).await
        })
    }

    async fn request_summary(&self, source: &str) -> Result<String> {
        let messages = [
            Message::text(
                Role::System,
                "把给定的早期对话压缩成简短事实摘要。保留用户目标、关键决策、文件改动、工具结果、未完成事项；不要虚构。只输出摘要。",
            ),
            Message::text(Role::User, source),
        ];
        match self.provider.chat(&messages, &[]).await? {
            Response::Text(summary) => Ok(summary),
            Response::ToolCalls(_) => bail!("模型在上下文压缩时返回了工具调用"),
        }
    }
}

#[derive(Clone, Copy)]
enum CompressionMode {
    Mild,
    Strong,
}

pub fn estimate_messages(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|message: &Message| {
            let content = message.content.as_deref().map_or(0, estimate_text_tokens);
            let tool_calls = serde_json::to_string(&message.tool_calls)
                .map(|value: String| estimate_text_tokens(&value))
                .unwrap_or(0);
            let images = message.image_urls.len().saturating_mul(384);
            content + tool_calls + images + 6
        })
        .sum()
}

pub fn estimate_text_tokens(text: &str) -> usize {
    let mut cjk = 0_usize;
    let mut other = 0_usize;
    for character in text.chars() {
        if is_cjk(character) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk.saturating_mul(2).div_ceil(3) + other.div_ceil(4)
}

fn estimate_json_tokens<T: serde::Serialize + ?Sized>(value: &T) -> Result<usize> {
    let serialized = serde_json::to_string(value).context("估算 JSON token 时序列化失败")?;
    Ok(estimate_text_tokens(&serialized))
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
    )
}

fn split_for_token_budget(text: &str, budget: usize) -> Vec<String> {
    let char_limit = budget.saturating_mul(3).max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if current.chars().count() >= char_limit || estimate_text_tokens(&current) >= budget {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut output = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        output.push_str("…[摘要已截断]");
    }
    output
}

fn load_project_rules(workspace: &Path) -> Result<Option<String>> {
    let path = workspace.join("AGENTS.md");
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        std::fs::read(&path).with_context(|| format!("读取项目规则失败: {}", path.display()))?;
    let limited = if bytes.len() > MAX_PROJECT_RULE_BYTES {
        &bytes[..MAX_PROJECT_RULE_BYTES]
    } else {
        &bytes
    };
    Ok(Some(String::from_utf8_lossy(limited).into_owned()))
}

fn optional_usize(name: &str, default: usize) -> Result<usize> {
    match env::var(name) {
        Ok(value) => value
            .parse::<usize>()
            .with_context(|| format!("环境变量 {name} 必须是正整数")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error).with_context(|| format!("读取环境变量 {name} 失败")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::plan::{PlanStatus, PlanStep};

    struct SummaryProvider {
        responses: Mutex<VecDeque<Response>>,
        calls: Mutex<usize>,
    }

    #[async_trait]
    impl Provider for SummaryProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            *self.calls.lock().unwrap() += 1;
            Ok(self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Response::Text("更短摘要".to_owned())))
        }
    }

    #[test]
    fn token_estimate_treats_chinese_more_densely_than_ascii() {
        assert_eq!(estimate_text_tokens("中文中文中文"), 4);
        assert_eq!(estimate_text_tokens("abcdefghijkl"), 3);
    }

    #[tokio::test]
    async fn compresses_old_messages_and_keeps_recent_messages() {
        let provider = Arc::new(SummaryProvider {
            responses: Mutex::new(VecDeque::from([Response::Text(
                "目标和决策摘要".to_owned(),
            )])),
            calls: Mutex::new(0),
        });
        let config = ContextConfig {
            token_budget: 256,
            recent_messages: 2,
            mild_compression_percent: 1,
            strong_compression_percent: 2,
            summary_chunk_tokens: 2_000,
        };
        let manager = ContextManager::new(
            provider.clone(),
            std::env::current_dir().unwrap(),
            config,
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let mut history = vec![
            Message::text(Role::User, "旧问题"),
            Message::text(Role::Assistant, "旧回答"),
            Message::text(Role::User, "新问题"),
            Message::text(Role::Assistant, "新回答"),
        ];

        let prepared = manager.prepare(&mut history, &[]).await.unwrap();

        assert_eq!(history.len(), 3);
        assert!(
            history[0]
                .content
                .as_deref()
                .unwrap()
                .contains("目标和决策摘要")
        );
        assert_eq!(history[1].content.as_deref(), Some("新问题"));
        assert_eq!(history[2].content.as_deref(), Some("新回答"));
        assert_eq!(*provider.calls.lock().unwrap(), 1);
        assert_eq!(prepared.first().unwrap().role, Role::System);
        assert!(
            prepared
                .last()
                .unwrap()
                .content
                .as_deref()
                .unwrap()
                .contains("cwd=")
        );
    }

    #[tokio::test]
    async fn mild_gate_only_summarizes_the_oldest_slice() {
        let provider = Arc::new(SummaryProvider {
            responses: Mutex::new(VecDeque::from([Response::Text("温和摘要".to_owned())])),
            calls: Mutex::new(0),
        });
        let manager = ContextManager::new(
            provider.clone(),
            std::env::current_dir().unwrap(),
            ContextConfig {
                token_budget: 100_000,
                recent_messages: 2,
                mild_compression_percent: 1,
                strong_compression_percent: 90,
                summary_chunk_tokens: 100_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let mut history = (0..8)
            .map(|index: usize| {
                Message::text(Role::User, format!("消息-{index}-{}", "x".repeat(1_000)))
            })
            .collect::<Vec<Message>>();

        manager.prepare(&mut history, &[]).await.unwrap();

        assert_eq!(history.len(), 7);
        assert!(history[0].content.as_deref().unwrap().contains("温和摘要"));
        assert!(
            history[1]
                .content
                .as_deref()
                .unwrap()
                .starts_with("消息-2-")
        );
        assert!(
            history[6]
                .content
                .as_deref()
                .unwrap()
                .starts_with("消息-7-")
        );
        assert_eq!(*provider.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn injects_current_plan_into_last_dynamic_block() {
        let provider = Arc::new(SummaryProvider {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(0),
        });
        let plan = Arc::new(PlanStore::memory_only());
        plan.set(vec![PlanStep {
            id: "1".to_owned(),
            description: "实现功能".to_owned(),
            status: PlanStatus::InProgress,
        }])
        .await
        .unwrap();
        let manager = ContextManager::new(
            provider,
            std::env::current_dir().unwrap(),
            ContextConfig {
                token_budget: 10_000,
                recent_messages: 12,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 2_000,
            },
            plan,
        )
        .unwrap();

        let prepared = manager.prepare(&mut Vec::new(), &[]).await.unwrap();
        let last = prepared.last().unwrap().content.as_deref().unwrap();
        assert!(last.contains("当前任务计划"));
        assert!(last.contains("[>] 1 · 实现功能"));
    }

    #[tokio::test]
    async fn orders_context_from_stable_prefix_to_dynamic_suffix() {
        let workspace =
            std::env::temp_dir().join(format!("my-agent-context-order-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("AGENTS.md"), "# 固定项目规则\n始终先测试").unwrap();
        let provider = Arc::new(SummaryProvider {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(0),
        });
        let manager = ContextManager::new(
            provider,
            &workspace,
            ContextConfig {
                token_budget: 10_000,
                recent_messages: 12,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 2_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();
        let mut history = vec![Message::text(Role::User, "本轮问题")];

        let prepared = manager.prepare(&mut history, &[]).await.unwrap();

        assert_eq!(prepared.len(), 4);
        assert!(
            prepared[0]
                .content
                .as_deref()
                .unwrap()
                .contains("编码 Agent")
        );
        assert!(
            prepared[1]
                .content
                .as_deref()
                .unwrap()
                .contains("固定项目规则")
        );
        assert_eq!(prepared[2].content.as_deref(), Some("本轮问题"));
        assert!(
            prepared[3]
                .content
                .as_deref()
                .unwrap()
                .starts_with("动态环境")
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn injects_only_bodies_of_skills_matching_the_user_request() {
        let workspace =
            std::env::temp_dir().join(format!("my-agent-skill-context-{}", std::process::id()));
        let skills_dir = workspace.join(".my-agent/skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("rust-testing.md"),
            "# Rust 测试\nsummary: 用 cargo test 验证 Rust 项目\n\n## 流程\nONLY_MATCHED_BODY\n",
        )
        .unwrap();
        let provider = Arc::new(SummaryProvider {
            responses: Mutex::new(VecDeque::new()),
            calls: Mutex::new(0),
        });
        let manager = ContextManager::new(
            provider,
            &workspace,
            ContextConfig {
                token_budget: 10_000,
                recent_messages: 12,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 2_000,
            },
            Arc::new(PlanStore::memory_only()),
        )
        .unwrap();

        let matched = manager
            .prepare(
                &mut vec![Message::text(Role::User, "请为 Rust 模块补单元测试")],
                &[],
            )
            .await
            .unwrap();
        let unmatched = manager
            .prepare(&mut vec![Message::text(Role::User, "查询今天的天气")], &[])
            .await
            .unwrap();

        let matched_dynamic = matched.last().unwrap().content.as_deref().unwrap();
        assert!(matched_dynamic.contains("可用技能索引"));
        assert!(matched_dynamic.contains("ONLY_MATCHED_BODY"));
        let unmatched_dynamic = unmatched.last().unwrap().content.as_deref().unwrap();
        assert!(unmatched_dynamic.contains("rust-testing · Rust 测试"));
        assert!(!unmatched_dynamic.contains("ONLY_MATCHED_BODY"));
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
