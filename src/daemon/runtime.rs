use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use super::DaemonState;
use super::approval::ApprovalBroker;
use crate::context::{ContextConfig, ContextManager};
use crate::cron::{AgentCronRunner, CronManager, CronStore, UnattendedApproval};
use crate::loop_engine::LoopEngine;
use crate::mcp::McpManager;
use crate::memory::{MemoryStore, RecallMemoryTool, RememberTool};
use crate::plan::{PlanStore, PlanTool};
use crate::provider::{Provider, build_provider_from_env};
use crate::safety::SafetyPolicy;
use crate::session::SessionStore;
use crate::skills::SkillLibrary;
use crate::sub_agent::SubAgentTool;
use crate::tools::{EditFileTool, ExecTool, ReadFileTool, ToolRegistry, WriteFileTool};

pub async fn build_daemon_state(workspace: &Path) -> Result<Arc<DaemonState>> {
    let provider: Arc<dyn Provider> = Arc::from(build_provider_from_env()?);
    let approvals = ApprovalBroker::new();
    let safety = Arc::new(SafetyPolicy::new(workspace, Arc::new(approvals.clone()))?);
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::from_env(safety.clone())?);
    tools.register(ExecTool::new(safety.clone()));
    tools.register(WriteFileTool::new(safety.clone()));
    tools.register(EditFileTool::new(safety.clone()));
    let memory = Arc::new(MemoryStore::from_env(workspace));
    tools.register(RememberTool::new(memory.clone()));
    tools.register(RecallMemoryTool::new(memory.clone()));
    let sub_agent_tools = tools.clone();
    let context_config = ContextConfig::from_env()?;
    let plan = Arc::new(PlanStore::from_env(workspace).await?);
    tools.register(PlanTool::new(plan.clone()));
    tools.register(SubAgentTool::new(
        provider.clone(),
        sub_agent_tools,
        workspace.to_path_buf(),
        context_config.clone(),
    ));
    let skills = SkillLibrary::from_env(workspace);
    skills.refresh().await?;
    let mcp = Arc::new(McpManager::new(workspace, safety));
    mcp.reload().await;
    tools.register_dynamic_source(mcp.clone());

    let cron_safety = Arc::new(SafetyPolicy::new(workspace, Arc::new(UnattendedApproval))?);
    let mut cron_tools = ToolRegistry::new();
    cron_tools.register(ReadFileTool::from_env(cron_safety.clone())?);
    cron_tools.register(ExecTool::new(cron_safety.clone()));
    cron_tools.register(WriteFileTool::new(cron_safety.clone()));
    cron_tools.register(EditFileTool::new(cron_safety));
    cron_tools.register(RememberTool::new(memory.clone()));
    cron_tools.register(RecallMemoryTool::new(memory));
    let cron_store = Arc::new(CronStore::load_best_effort(workspace).await);
    let heartbeat = env_bool("HEARTBEAT_ENABLED", false)
        .then(|| Duration::from_secs(env_u64("HEARTBEAT_INTERVAL_SECS", 300)));
    let cron_runner = Arc::new(AgentCronRunner::new(
        provider.clone(),
        cron_tools,
        workspace.to_path_buf(),
        context_config.clone(),
    ));
    let cron = Arc::new(CronManager::new(
        cron_store,
        cron_runner,
        Duration::from_secs(env_u64("CRON_TICK_SECONDS", 1).max(1)),
        Duration::from_secs(env_u64("CRON_STAGGER_SECONDS", 2)),
        heartbeat,
    ));
    let context = ContextManager::new_with_skills(
        provider.clone(),
        workspace,
        context_config,
        plan,
        skills.clone(),
    )?;
    let session = Arc::new(SessionStore::from_env(workspace));
    let history = session.load().await?;
    let engine = Arc::new(LoopEngine::new(provider, tools, context, session.clone()));
    let state = Arc::new(DaemonState::new_with_services(
        engine,
        history,
        session,
        approvals,
        Some(skills),
        Some(cron.clone()),
        Some(mcp),
    ));
    cron.start(state.shutdown.clone()).await;
    Ok(state)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name).ok().map_or(default, |value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}
