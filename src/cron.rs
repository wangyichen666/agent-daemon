use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ::cron::Schedule;
use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, warn};

use crate::context::{ContextConfig, ContextManager};
use crate::loop_engine::{CancellationToken, LoopEngine};
use crate::plan::PlanStore;
use crate::provider::Provider;
use crate::safety::Approval;
use crate::session::SessionStore;
use crate::tools::ToolRegistry;

const MAX_HISTORY: usize = 20;
const CRON_SYSTEM_PROMPT: &str = "你正在执行个人 Agent 的无人值守定时任务。使用全新独立历史，只完成给定 prompt。所有工具仍受安全策略约束；任何需要人工审批的动作都会被拒绝，必须改用安全方案或清晰报告失败。";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScheduleSpec {
    Interval { seconds: u64 },
    Cron { expression: String },
}

impl ScheduleSpec {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Interval { seconds } if *seconds == 0 => bail!("interval 秒数必须大于 0"),
            Self::Interval { .. } => Ok(()),
            Self::Cron { expression } => parse_five_field_cron(expression).map(|_| ()),
        }
    }

    fn next_after(&self, after: u64) -> Result<u64> {
        match self {
            Self::Interval { seconds } => Ok(after.saturating_add(*seconds)),
            Self::Cron { expression } => {
                let schedule = parse_five_field_cron(expression)?;
                let time = UNIX_EPOCH
                    .checked_add(Duration::from_secs(after))
                    .context("cron 基准时间溢出")?;
                let datetime: DateTime<Utc> = time.into();
                schedule
                    .after(&datetime)
                    .next()
                    .map(|next| u64::try_from(next.timestamp()).unwrap_or(u64::MAX))
                    .context("cron 表达式没有下一次触发时间")
            }
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Interval { seconds } => format!("interval={seconds}s"),
            Self::Cron { expression } => format!("cron={expression}"),
        }
    }
}

fn parse_five_field_cron(expression: &str) -> Result<Schedule> {
    if expression.split_whitespace().count() != 5 {
        bail!("cron 表达式必须正好是 5 段：分 时 日 月 周");
    }
    Schedule::from_str(&format!("0 {expression}"))
        .with_context(|| format!("非法 cron 表达式：{expression}"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CronJob {
    pub id: String,
    pub name: String,
    pub schedule: ScheduleSpec,
    pub prompt: String,
    pub enabled: bool,
    pub max_retries: u32,
    pub backoff_seconds: u64,
    pub next_run_at: u64,
    #[serde(default)]
    pub history: Vec<CronRunRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CronRunRecord {
    pub started_at: u64,
    pub finished_at: u64,
    pub attempts: u32,
    pub success: bool,
    pub result: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct CronState {
    jobs: Vec<CronJob>,
}

pub struct CronStore {
    path: PathBuf,
    state: RwLock<CronState>,
    mutation_lock: Mutex<()>,
}

impl CronStore {
    pub async fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join(".my-agent/cron.json");
        let state = if path.exists() {
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("读取 cron store 失败：{}", path.display()))?;
            serde_json::from_slice(&bytes)
                .with_context(|| format!("解析 cron store 失败：{}", path.display()))?
        } else {
            CronState::default()
        };
        for job in &state.jobs {
            job.schedule.validate()?;
        }
        Ok(Self {
            path,
            state: RwLock::new(state),
            mutation_lock: Mutex::new(()),
        })
    }

    pub async fn load_best_effort(workspace: &Path) -> Self {
        match Self::load(workspace).await {
            Ok(store) => store,
            Err(error) => {
                warn!(%error, "cron.json 无效，已禁用现有任务并使用空 store");
                Self {
                    path: workspace.join(".my-agent/cron.json"),
                    state: RwLock::new(CronState::default()),
                    mutation_lock: Mutex::new(()),
                }
            }
        }
    }

    pub async fn list(&self) -> Vec<CronJob> {
        self.state.read().await.jobs.clone()
    }

    pub async fn add(
        &self,
        name: String,
        schedule: ScheduleSpec,
        prompt: String,
        max_retries: u32,
        backoff_seconds: u64,
    ) -> Result<CronJob> {
        let _mutation = self.mutation_lock.lock().await;
        if name.trim().is_empty() || prompt.trim().is_empty() {
            bail!("cron name 与 prompt 不能为空");
        }
        schedule.validate()?;
        let now = unix_now()?;
        let job = CronJob {
            id: format!("cron-{now}-{}", NEXT_JOB_ID.fetch_add(1, Ordering::SeqCst)),
            name,
            next_run_at: schedule.next_after(now)?,
            schedule,
            prompt,
            enabled: true,
            max_retries,
            backoff_seconds,
            history: Vec::new(),
        };
        let mut next = self.state.read().await.clone();
        if next.jobs.iter().any(|item| item.name == job.name) {
            bail!("cron 名称已存在：{}", job.name);
        }
        next.jobs.push(job.clone());
        self.commit(next).await?;
        Ok(job)
    }

    pub async fn set_enabled(&self, id_or_name: &str, enabled: bool) -> Result<CronJob> {
        let _mutation = self.mutation_lock.lock().await;
        let mut next = self.state.read().await.clone();
        let job = find_job_mut(&mut next.jobs, id_or_name)?;
        job.enabled = enabled;
        if enabled {
            job.next_run_at = job.schedule.next_after(unix_now()?)?;
        }
        let output = job.clone();
        self.commit(next).await?;
        Ok(output)
    }

    pub async fn remove(&self, id_or_name: &str) -> Result<CronJob> {
        let _mutation = self.mutation_lock.lock().await;
        let mut next = self.state.read().await.clone();
        let index = next
            .jobs
            .iter()
            .position(|job| job.id == id_or_name || job.name == id_or_name)
            .with_context(|| format!("找不到 cron：{id_or_name}"))?;
        let removed = next.jobs.remove(index);
        self.commit(next).await?;
        Ok(removed)
    }

    pub async fn get(&self, id_or_name: &str) -> Result<CronJob> {
        self.state
            .read()
            .await
            .jobs
            .iter()
            .find(|job| job.id == id_or_name || job.name == id_or_name)
            .cloned()
            .with_context(|| format!("找不到 cron：{id_or_name}"))
    }

    async fn take_due(&self, now: u64) -> Result<Vec<CronJob>> {
        let _mutation = self.mutation_lock.lock().await;
        let mut next = self.state.read().await.clone();
        let mut due = Vec::new();
        for job in &mut next.jobs {
            if job.enabled && job.next_run_at <= now {
                due.push(job.clone());
                job.next_run_at = job.schedule.next_after(now)?;
            }
        }
        if !due.is_empty() {
            self.commit(next).await?;
        }
        Ok(due)
    }

    async fn record(&self, job_id: &str, record: CronRunRecord) -> Result<()> {
        let _mutation = self.mutation_lock.lock().await;
        let mut next = self.state.read().await.clone();
        let job = find_job_mut(&mut next.jobs, job_id)?;
        job.history.push(record);
        if job.history.len() > MAX_HISTORY {
            let remove = job.history.len() - MAX_HISTORY;
            job.history.drain(..remove);
        }
        self.commit(next).await
    }

    pub async fn has_enabled_jobs(&self) -> bool {
        self.state.read().await.jobs.iter().any(|job| job.enabled)
    }

    async fn commit(&self, next: CronState) -> Result<()> {
        persist_state(&self.path, &next).await?;
        *self.state.write().await = next;
        Ok(())
    }
}

fn find_job_mut<'a>(jobs: &'a mut [CronJob], id_or_name: &str) -> Result<&'a mut CronJob> {
    jobs.iter_mut()
        .find(|job| job.id == id_or_name || job.name == id_or_name)
        .with_context(|| format!("找不到 cron：{id_or_name}"))
}

async fn persist_state(path: &Path, state: &CronState) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("创建 cron 目录失败：{}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(state).context("序列化 cron store 失败")?;
    let temporary = path.with_file_name(format!(".cron.json.tmp-{}", std::process::id()));
    tokio::fs::write(&temporary, bytes)
        .await
        .with_context(|| format!("写入临时 cron store 失败：{}", temporary.display()))?;
    tokio::fs::rename(&temporary, path)
        .await
        .with_context(|| format!("提交 cron store 失败：{}", path.display()))
}

#[async_trait]
pub trait ScheduledJobRunner: Send + Sync {
    async fn run(&self, job: &CronJob) -> Result<String>;
}

pub struct AgentCronRunner {
    provider: Arc<dyn Provider>,
    tools: ToolRegistry,
    workspace: PathBuf,
    context_config: ContextConfig,
    timeout: Duration,
}

impl AgentCronRunner {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        workspace: PathBuf,
        context_config: ContextConfig,
    ) -> Self {
        let timeout = std::env::var("CRON_RUN_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse().ok())
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(600));
        Self {
            provider,
            tools,
            workspace,
            context_config,
            timeout,
        }
    }
}

#[async_trait]
impl ScheduledJobRunner for AgentCronRunner {
    async fn run(&self, job: &CronJob) -> Result<String> {
        let context = ContextManager::with_system_prompt(
            self.provider.clone(),
            &self.workspace,
            self.context_config.clone(),
            Arc::new(PlanStore::memory_only()),
            CRON_SYSTEM_PROMPT,
        )?;
        let session_path = self.workspace.join(".my-agent/cron-sessions").join(format!(
            "{}-{}.jsonl",
            job.id,
            unix_now()?
        ));
        let session = Arc::new(SessionStore::new(session_path));
        let runner = LoopEngine::new(self.provider.clone(), self.tools.clone(), context, session);
        tokio::time::timeout(
            self.timeout,
            runner.run_turn(&mut Vec::new(), job.prompt.clone()),
        )
        .await
        .context("cron 任务执行超时")?
    }
}

pub struct CronManager {
    store: Arc<CronStore>,
    runner: Arc<dyn ScheduledJobRunner>,
    tick: Duration,
    stagger: Duration,
    heartbeat: Option<Duration>,
    running: Mutex<HashSet<String>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CronManager {
    pub fn new(
        store: Arc<CronStore>,
        runner: Arc<dyn ScheduledJobRunner>,
        tick: Duration,
        stagger: Duration,
        heartbeat: Option<Duration>,
    ) -> Self {
        Self {
            store,
            runner,
            tick,
            stagger,
            heartbeat,
            running: Mutex::new(HashSet::new()),
            task: Mutex::new(None),
        }
    }

    pub async fn start(self: &Arc<Self>, shutdown: CancellationToken) {
        let mut task = self.task.lock().await;
        if task.is_some() {
            return;
        }
        let manager = self.clone();
        *task = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(manager.tick);
            let mut last_heartbeat = tokio::time::Instant::now();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = tick.tick() => {
                        if let Err(error) = manager.run_due(unix_now().unwrap_or(0)).await {
                            warn!(%error, "cron 调度 tick 失败");
                        }
                        if manager.heartbeat.is_some_and(|period| last_heartbeat.elapsed() >= period) {
                            manager.heartbeat_once().await;
                            last_heartbeat = tokio::time::Instant::now();
                        }
                    }
                }
            }
        }));
    }

    pub async fn join(&self) {
        if let Some(task) = self.task.lock().await.take()
            && let Err(error) = task.await
        {
            warn!(%error, "cron 调度任务异常终止");
        }
    }

    pub fn store(&self) -> &Arc<CronStore> {
        &self.store
    }

    pub async fn run_now(&self, id_or_name: &str) -> Result<CronRunRecord> {
        let job = self.store.get(id_or_name).await?;
        self.execute_job(job).await
    }

    async fn run_due(&self, now: u64) -> Result<()> {
        let due = self.store.take_due(now).await?;
        for (index, job) in due.into_iter().enumerate() {
            if index > 0 && !self.stagger.is_zero() {
                tokio::time::sleep(self.stagger).await;
            }
            if let Err(error) = self.execute_job(job).await {
                warn!(%error, "记录 cron 执行结果失败");
            }
        }
        Ok(())
    }

    async fn execute_job(&self, job: CronJob) -> Result<CronRunRecord> {
        {
            let mut running = self.running.lock().await;
            if !running.insert(job.id.clone()) {
                bail!("cron 已在运行：{}", job.name);
            }
        }
        let started_at = unix_now()?;
        let mut attempts = 0_u32;
        let final_result = loop {
            attempts = attempts.saturating_add(1);
            match self.runner.run(&job).await {
                Ok(result) => break (true, result),
                Err(error) if attempts <= job.max_retries => {
                    let exponent = attempts.saturating_sub(1).min(20);
                    let delay = job.backoff_seconds.saturating_mul(1_u64 << exponent);
                    if delay > 0 {
                        tokio::time::sleep(Duration::from_secs(delay)).await;
                    }
                    debug!(job = %job.name, attempts, %error, "cron 任务准备重试");
                }
                Err(error) => break (false, format!("{error:#}")),
            }
        };
        self.running.lock().await.remove(&job.id);
        let record = CronRunRecord {
            started_at,
            finished_at: unix_now()?,
            attempts,
            success: final_result.0,
            result: final_result.1,
        };
        self.store.record(&job.id, record.clone()).await?;
        Ok(record)
    }

    async fn heartbeat_once(&self) {
        let jobs = self.store.list().await;
        debug!(jobs = jobs.len(), "heartbeat：cron store 自检完成");
    }

    pub async fn keeps_daemon_alive(&self) -> bool {
        self.heartbeat.is_some() || self.store.has_enabled_jobs().await
    }
}

pub struct UnattendedApproval;

#[async_trait]
impl Approval for UnattendedApproval {
    async fn request(&self, prompt: &str) -> Result<bool> {
        warn!(prompt, "无人值守 cron 动作需要审批，已安全拒绝");
        Ok(false)
    }
}

fn unix_now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("系统时间早于 UNIX_EPOCH")
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use crate::provider::{Message, Response, ToolSpec};

    use super::*;

    struct FakeRunner {
        failures: usize,
        calls: AtomicUsize,
    }

    struct TextProvider;

    #[async_trait]
    impl Provider for TextProvider {
        async fn chat(&self, _messages: &[Message], _tools: &[ToolSpec]) -> Result<Response> {
            Ok(Response::Text("scheduled-result".to_owned()))
        }
    }

    struct RejectedRunner;

    #[async_trait]
    impl ScheduledJobRunner for RejectedRunner {
        async fn run(&self, _job: &CronJob) -> Result<String> {
            if !UnattendedApproval.request("danger").await? {
                bail!("无人值守审批已拒绝");
            }
            Ok("unexpected".to_owned())
        }
    }

    #[async_trait]
    impl ScheduledJobRunner for FakeRunner {
        async fn run(&self, _job: &CronJob) -> Result<String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                bail!("planned failure")
            }
            Ok("done".to_owned())
        }
    }

    fn workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("my-agent-cron-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn persists_due_job_and_records_success() {
        let workspace = workspace("persist");
        let store = Arc::new(CronStore::load(&workspace).await.unwrap());
        let job = store
            .add(
                "fast".to_owned(),
                ScheduleSpec::Interval { seconds: 5 },
                "run".to_owned(),
                0,
                0,
            )
            .await
            .unwrap();
        let runner = Arc::new(FakeRunner {
            failures: 0,
            calls: AtomicUsize::new(0),
        });
        let manager = CronManager::new(
            store.clone(),
            runner,
            Duration::from_secs(1),
            Duration::ZERO,
            None,
        );
        manager.run_due(job.next_run_at).await.unwrap();

        let reloaded = CronStore::load(&workspace).await.unwrap();
        let jobs = reloaded.list().await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].history.len(), 1);
        assert!(jobs[0].history[0].success);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn stops_after_finite_retries() {
        let workspace = workspace("retry");
        let store = Arc::new(CronStore::load(&workspace).await.unwrap());
        let job = store
            .add(
                "failure".to_owned(),
                ScheduleSpec::Interval { seconds: 5 },
                "fail".to_owned(),
                2,
                0,
            )
            .await
            .unwrap();
        let runner = Arc::new(FakeRunner {
            failures: usize::MAX,
            calls: AtomicUsize::new(0),
        });
        let manager = CronManager::new(
            store,
            runner.clone(),
            Duration::from_secs(1),
            Duration::ZERO,
            None,
        );
        let record = manager.run_now(&job.id).await.unwrap();
        assert!(!record.success);
        assert_eq!(record.attempts, 3);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 3);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn unattended_approval_always_refuses() {
        assert!(!UnattendedApproval.request("danger").await.unwrap());
    }

    #[tokio::test]
    async fn agent_runner_uses_an_independent_persisted_session() {
        let workspace = workspace("independent-session");
        let store = CronStore::load(&workspace).await.unwrap();
        let job = store
            .add(
                "session".to_owned(),
                ScheduleSpec::Interval { seconds: 5 },
                "run".to_owned(),
                0,
                0,
            )
            .await
            .unwrap();
        let runner = AgentCronRunner::new(
            Arc::new(TextProvider),
            ToolRegistry::new(),
            workspace.clone(),
            ContextConfig {
                token_budget: 1_000_000,
                recent_messages: 100,
                mild_compression_percent: 60,
                strong_compression_percent: 85,
                summary_chunk_tokens: 100_000,
            },
        );
        assert_eq!(runner.run(&job).await.unwrap(), "scheduled-result");
        let sessions = std::fs::read_dir(workspace.join(".my-agent/cron-sessions"))
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert!(!workspace.join(".my-agent/session.jsonl").exists());
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn unattended_rejection_is_recorded_as_failure() {
        let workspace = workspace("approval-record");
        let store = Arc::new(CronStore::load(&workspace).await.unwrap());
        let job = store
            .add(
                "danger".to_owned(),
                ScheduleSpec::Interval { seconds: 5 },
                "danger".to_owned(),
                0,
                0,
            )
            .await
            .unwrap();
        let manager = CronManager::new(
            store.clone(),
            Arc::new(RejectedRunner),
            Duration::from_secs(1),
            Duration::ZERO,
            None,
        );
        let record = manager.run_now(&job.id).await.unwrap();
        assert!(!record.success);
        assert!(record.result.contains("无人值守审批已拒绝"));
        assert_eq!(store.get(&job.id).await.unwrap().history.len(), 1);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn accepts_standard_five_field_cron() {
        assert!(parse_five_field_cron("0 9 * * 1-5").is_ok());
        assert!(parse_five_field_cron("0 9 * *").is_err());
    }
}
