use std::path::Path;
use std::sync::Arc;

use anyhow::Result;

use super::DaemonState;
use super::approval::ApprovalBroker;
use crate::context::{ContextConfig, ContextManager};
use crate::loop_engine::LoopEngine;
use crate::memory::{MemoryStore, RecallMemoryTool, RememberTool};
use crate::plan::{PlanStore, PlanTool};
use crate::provider::{OpenAiProvider, Provider};
use crate::safety::SafetyPolicy;
use crate::session::SessionStore;
use crate::sub_agent::SubAgentTool;
use crate::tools::{EditFileTool, ExecTool, ReadFileTool, ToolRegistry, WriteFileTool};

pub async fn build_daemon_state(workspace: &Path) -> Result<Arc<DaemonState>> {
    let provider: Arc<dyn Provider> = Arc::new(OpenAiProvider::from_env()?);
    let approvals = ApprovalBroker::new();
    let safety = Arc::new(SafetyPolicy::new(workspace, Arc::new(approvals.clone()))?);
    let mut tools = ToolRegistry::new();
    tools.register(ReadFileTool::from_env(safety.clone())?);
    tools.register(ExecTool::new(safety.clone()));
    tools.register(WriteFileTool::new(safety.clone()));
    tools.register(EditFileTool::new(safety));
    let memory = Arc::new(MemoryStore::from_env(workspace));
    tools.register(RememberTool::new(memory.clone()));
    tools.register(RecallMemoryTool::new(memory));
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
    let context = ContextManager::new(provider.clone(), workspace, context_config, plan)?;
    let session = Arc::new(SessionStore::from_env(workspace));
    let history = session.load().await?;
    let engine = Arc::new(LoopEngine::new(provider, tools, context, session.clone()));
    Ok(Arc::new(DaemonState::new(
        engine, history, session, approvals,
    )))
}
