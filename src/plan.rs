use std::collections::HashSet;
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::tools::Tool;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    #[default]
    Pending,
    InProgress,
    Done,
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PlanStep {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub status: PlanStatus,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct PlanState {
    steps: Vec<PlanStep>,
}

#[derive(Debug, Error)]
enum PlanError {
    #[error("计划步骤 id 不能为空")]
    EmptyId,
    #[error("计划步骤描述不能为空: {0}")]
    EmptyDescription(String),
    #[error("计划步骤 id 重复: {0}")]
    DuplicateId(String),
    #[error("找不到计划步骤: {0}")]
    MissingStep(String),
}

pub struct PlanStore {
    path: Option<PathBuf>,
    state: RwLock<PlanState>,
    mutation_lock: Mutex<()>,
}

impl PlanStore {
    pub async fn from_env(workspace: &Path) -> Result<Self> {
        let path = match env::var_os("PLAN_PATH") {
            Some(value) if value.is_empty() || value == "off" => None,
            Some(value) => {
                let configured = PathBuf::from(value);
                Some(if configured.is_absolute() {
                    configured
                } else {
                    workspace.join(configured)
                })
            }
            None => Some(workspace.join(".my-agent/plan.json")),
        };
        Self::new(path).await
    }

    pub fn memory_only() -> Self {
        Self {
            path: None,
            state: RwLock::new(PlanState::default()),
            mutation_lock: Mutex::new(()),
        }
    }

    pub async fn new(path: Option<PathBuf>) -> Result<Self> {
        let state = if let Some(path) = &path {
            load_state(path).await?
        } else {
            PlanState::default()
        };
        validate_steps(&state.steps)?;
        Ok(Self {
            path,
            state: RwLock::new(state),
            mutation_lock: Mutex::new(()),
        })
    }

    pub async fn set(&self, steps: Vec<PlanStep>) -> Result<String> {
        let _mutation_guard = self.mutation_lock.lock().await;
        validate_steps(&steps)?;
        let next = PlanState { steps };
        self.commit(next).await
    }

    pub async fn update(&self, id: &str, status: PlanStatus) -> Result<String> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut next = self.state.read().await.clone();
        let Some(step) = next
            .steps
            .iter_mut()
            .find(|step: &&mut PlanStep| step.id == id)
        else {
            return Err(PlanError::MissingStep(id.to_owned()).into());
        };
        step.status = status;
        self.commit(next).await
    }

    pub async fn add(&self, step: PlanStep) -> Result<String> {
        let _mutation_guard = self.mutation_lock.lock().await;
        let mut next = self.state.read().await.clone();
        next.steps.push(step);
        validate_steps(&next.steps)?;
        self.commit(next).await
    }

    pub async fn show(&self) -> String {
        render_plan(&self.state.read().await.steps)
    }

    pub async fn context_block(&self) -> Option<String> {
        let state = self.state.read().await;
        (!state.steps.is_empty()).then(|| format!("当前任务计划：\n{}", render_plan(&state.steps)))
    }

    async fn commit(&self, next: PlanState) -> Result<String> {
        persist_state(self.path.as_deref(), &next).await?;
        let rendered = render_plan(&next.steps);
        *self.state.write().await = next;
        Ok(rendered)
    }
}

pub struct PlanTool {
    store: Arc<PlanStore>,
}

impl PlanTool {
    pub fn new(store: Arc<PlanStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum PlanAction {
    Set {
        steps: Vec<PlanStep>,
    },
    Update {
        id: String,
        status: PlanStatus,
    },
    Add {
        id: String,
        description: String,
        #[serde(default)]
        status: PlanStatus,
    },
    Show,
}

#[async_trait]
impl Tool for PlanTool {
    fn name(&self) -> &str {
        "plan"
    }

    fn description(&self) -> &str {
        "管理当前多步骤任务计划。复杂任务先 set；开始或完成步骤时 update；新增步骤用 add；查看用 show。简单任务无需计划。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["set", "update", "add", "show"],
                    "description": "set 整体重写；update 更新状态；add 追加；show 查看"
                },
                "steps": {
                    "type": "array",
                    "description": "set 必填。步骤为 {id, description, status?}，status 默认 pending"
                },
                "id": {"type": "string", "description": "update/add 必填的步骤 id"},
                "description": {"type": "string", "description": "add 必填的步骤描述"},
                "status": {
                    "type": "string",
                    "enum": ["pending", "in_progress", "done"],
                    "description": "update 必填；add 可选"
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let action: PlanAction = serde_json::from_value(args).context("plan 参数无效")?;
        match action {
            PlanAction::Set { steps } => self.store.set(steps).await,
            PlanAction::Update { id, status } => self.store.update(&id, status).await,
            PlanAction::Add {
                id,
                description,
                status,
            } => {
                self.store
                    .add(PlanStep {
                        id,
                        description,
                        status,
                    })
                    .await
            }
            PlanAction::Show => Ok(self.store.show().await),
        }
    }
}

fn validate_steps(steps: &[PlanStep]) -> Result<()> {
    let mut ids = HashSet::new();
    for step in steps {
        if step.id.trim().is_empty() {
            return Err(PlanError::EmptyId.into());
        }
        if step.description.trim().is_empty() {
            return Err(PlanError::EmptyDescription(step.id.clone()).into());
        }
        if !ids.insert(step.id.as_str()) {
            return Err(PlanError::DuplicateId(step.id.clone()).into());
        }
    }
    Ok(())
}

fn render_plan(steps: &[PlanStep]) -> String {
    if steps.is_empty() {
        return "当前计划为空".to_owned();
    }
    let done = steps
        .iter()
        .filter(|step: &&PlanStep| step.status == PlanStatus::Done)
        .count();
    let mut output = format!("计划进度：{done}/{}\n", steps.len());
    for step in steps {
        let marker = match step.status {
            PlanStatus::Pending => "[ ]",
            PlanStatus::InProgress => "[>]",
            PlanStatus::Done => "[x]",
        };
        output.push_str(&format!(
            "{marker} {} · {} ({})\n",
            step.id, step.description, step.status
        ));
    }
    output.trim_end().to_owned()
}

async fn load_state(path: &Path) -> Result<PlanState> {
    if !path.exists() {
        return Ok(PlanState::default());
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("读取计划失败: {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("解析计划失败: {}", path.display()))
}

async fn persist_state(path: Option<&Path>, state: &PlanState) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建计划目录失败: {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(state).context("序列化计划失败")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("plan.json");
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    tokio::fs::write(&temporary, bytes)
        .await
        .with_context(|| format!("写入临时计划失败: {}", temporary.display()))?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("提交计划失败: {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    fn step(id: &str, description: &str) -> PlanStep {
        PlanStep {
            id: id.to_owned(),
            description: description.to_owned(),
            status: PlanStatus::Pending,
        }
    }

    #[tokio::test]
    async fn set_update_add_and_restore_plan() {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("my-agent-plan-{}-{id}.json", std::process::id()));
        let store = PlanStore::new(Some(path.clone())).await.unwrap();

        store
            .set(vec![step("1", "分析"), step("2", "实现")])
            .await
            .unwrap();
        store.update("1", PlanStatus::InProgress).await.unwrap();
        store.add(step("3", "验证")).await.unwrap();
        store.update("1", PlanStatus::Done).await.unwrap();

        let restored = PlanStore::new(Some(path.clone())).await.unwrap();
        let shown = restored.show().await;
        assert!(shown.contains("计划进度：1/3"));
        assert!(shown.contains("[x] 1 · 分析"));
        assert!(shown.contains("[ ] 3 · 验证"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn rejects_duplicate_ids_without_changing_current_plan() {
        let store = PlanStore::memory_only();
        store.set(vec![step("1", "已有")]).await.unwrap();
        let error = store
            .set(vec![step("x", "甲"), step("x", "乙")])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("重复"));
        assert!(store.show().await.contains("已有"));
    }

    #[tokio::test]
    async fn serializes_concurrent_mutations_without_losing_updates() {
        let store = PlanStore::memory_only();
        store
            .set(vec![step("1", "第一步"), step("2", "第二步")])
            .await
            .unwrap();

        let (first, second) = tokio::join!(
            store.update("1", PlanStatus::InProgress),
            store.update("2", PlanStatus::Done),
        );
        first.unwrap();
        second.unwrap();

        let shown = store.show().await;
        assert!(shown.contains("[>] 1 · 第一步"));
        assert!(shown.contains("[x] 2 · 第二步"));
    }
}
