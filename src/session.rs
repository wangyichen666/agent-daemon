use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, MutexGuard, RwLock};
use tracing::warn;

use crate::provider::{Message, Role};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("会话文件第 {line} 行损坏: {source}")]
    CorruptLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("系统时间早于 UNIX_EPOCH")]
    InvalidSystemTime,
    #[error("找不到会话: {0}")]
    UnknownSession(String),
    #[error("非法会话 ID: {0}")]
    InvalidSessionId(String),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Idle,
    Running,
    Waiting,
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Waiting => "waiting",
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub active: bool,
    pub message_count: usize,
    pub modified_at: Option<u64>,
    pub preview: Option<String>,
    #[serde(default)]
    pub status: SessionStatus,
    #[serde(default)]
    pub active_requests: usize,
    #[serde(default)]
    pub updated_at: Option<u64>,
}

pub struct SessionStore {
    base_path: PathBuf,
    current_path: RwLock<PathBuf>,
    current_id_cache: std::sync::RwLock<String>,
    turn_lock: Mutex<()>,
}

impl SessionStore {
    pub fn from_env(workspace: &Path) -> Self {
        let configured = env::var_os("SESSION_PATH").map(PathBuf::from);
        let path = match configured {
            Some(path) if path.is_absolute() => path,
            Some(path) => workspace.join(path),
            None => workspace.join(".my-agent/session.jsonl"),
        };
        Self::new(path)
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        let base_path = path.into();
        let current_path =
            Self::read_current_pointer(&base_path).unwrap_or_else(|| base_path.clone());
        let current_id = current_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.jsonl")
            .to_owned();
        Self {
            base_path,
            current_path: RwLock::new(current_path),
            current_id_cache: std::sync::RwLock::new(current_id),
            turn_lock: Mutex::new(()),
        }
    }

    #[cfg(test)]
    pub async fn current_id(&self) -> String {
        self.current_id_cache
            .read()
            .expect("session current id lock poisoned")
            .clone()
    }

    pub(crate) fn current_id_sync(&self) -> String {
        self.current_id_cache
            .read()
            .expect("session current id lock poisoned")
            .clone()
    }

    pub async fn lock_turn(&self) -> MutexGuard<'_, ()> {
        self.turn_lock.lock().await
    }

    pub async fn load(&self) -> Result<Vec<Message>> {
        let path = self.current_path.read().await.clone();
        Self::load_path(&path).await
    }

    async fn load_path(path: &Path) -> Result<Vec<Message>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("读取会话失败: {}", path.display()))?;
        let content = String::from_utf8_lossy(&bytes);
        let lines = content.lines().collect::<Vec<&str>>();
        let has_complete_last_line = bytes.last().is_none_or(|byte: &u8| *byte == b'\n');
        let mut messages = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let message = match serde_json::from_str::<Message>(line) {
                Ok(message) => message,
                Err(_) if index + 1 == lines.len() && !has_complete_last_line => {
                    warn!(
                        line = index + 1,
                        path = %path.display(),
                        "忽略崩溃留下的不完整会话末行"
                    );
                    break;
                }
                Err(source) => {
                    return Err(SessionError::CorruptLine {
                        line: index + 1,
                        source,
                    }
                    .into());
                }
            };
            messages.push(message);
        }
        Ok(messages)
    }

    pub async fn append(&self, message: &Message) -> Result<()> {
        let path = self.current_path.read().await.clone();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建会话目录失败: {}", parent.display()))?;
        }
        let mut line = serde_json::to_vec(message).context("序列化会话消息失败")?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .with_context(|| format!("打开会话文件失败: {}", path.display()))?;
        file.write_all(&line)
            .await
            .with_context(|| format!("追加会话消息失败: {}", path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("刷新会话文件失败: {}", path.display()))?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let Some(directory) = self.base_path.parent() else {
            return Ok(Vec::new());
        };
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let current_path = self.current_path.read().await.clone();
        let active_name = current_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.jsonl")
            .to_owned();
        let mut entries = tokio::fs::read_dir(directory)
            .await
            .with_context(|| format!("读取会话目录失败: {}", directory.display()))?;
        let mut sessions = Vec::new();
        while let Some(entry) = entries.next_entry().await.context("遍历会话目录失败")? {
            if !entry
                .file_type()
                .await
                .context("读取会话文件类型失败")?
                .is_file()
            {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            let active = name == active_name;
            if !active && !Self::is_session_name(&self.base_path, name) {
                continue;
            }
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("读取会话元数据失败: {}", path.display()))?;
            let message_count = String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            let preview = Self::preview(&bytes);
            let metadata = entry.metadata().await.context("读取会话文件属性失败")?;
            let modified_at = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs());
            sessions.push(SessionInfo {
                id: name.to_owned(),
                path,
                active,
                message_count,
                modified_at,
                preview,
                status: SessionStatus::Idle,
                active_requests: 0,
                updated_at: modified_at,
            });
        }
        if !sessions.iter().any(|session| session.active) {
            sessions.push(SessionInfo {
                id: active_name,
                path: current_path,
                active: true,
                message_count: 0,
                modified_at: None,
                preview: None,
                status: SessionStatus::Idle,
                active_requests: 0,
                updated_at: None,
            });
        }
        sessions.sort_by(|left, right| {
            right
                .active
                .cmp(&left.active)
                .then_with(|| right.modified_at.cmp(&left.modified_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(sessions)
    }

    #[cfg(test)]
    pub async fn start_new(&self) -> Result<String> {
        let _guard = self.turn_lock.lock().await;
        let id = self.new_session_id()?;
        let path = self
            .base_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(&id);
        self.persist_current_pointer(&id).await?;
        *self.current_path.write().await = path;
        *self
            .current_id_cache
            .write()
            .expect("session current id lock poisoned") = id.clone();
        Ok(id)
    }

    #[cfg(test)]
    pub async fn resume(&self, session_id: &str) -> Result<Vec<Message>> {
        if Path::new(session_id).file_name() != Some(OsStr::new(session_id))
            || !Self::is_session_name(&self.base_path, session_id)
        {
            return Err(SessionError::InvalidSessionId(session_id.to_owned()).into());
        }
        let target = self
            .base_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(session_id);
        if !tokio::fs::symlink_metadata(&target)
            .await
            .is_ok_and(|metadata| metadata.file_type().is_file())
        {
            return Err(SessionError::UnknownSession(session_id.to_owned()).into());
        }
        let _guard = self.turn_lock.lock().await;
        let history = Self::load_path(&target).await?;
        self.persist_current_pointer(session_id).await?;
        *self.current_path.write().await = target;
        *self
            .current_id_cache
            .write()
            .expect("session current id lock poisoned") = session_id.to_owned();
        Ok(history)
    }

    /// Open an existing session without changing the workspace-wide current pointer.
    /// This is the primitive used by concurrent TUI/editor windows.
    pub fn open_session(&self, session_id: &str) -> Result<Self> {
        self.validate_session_id(session_id)?;
        let target = self.session_path(session_id);
        let exists =
            std::fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_file());
        if !exists && self.current_id_sync() != session_id {
            return Err(SessionError::UnknownSession(session_id.to_owned()).into());
        }
        Ok(Self::with_current_path(self.base_path.clone(), target))
    }

    /// Allocate a fresh session without touching the workspace-wide current
    /// pointer. This path is safe while another window is actively writing the
    /// default session.
    pub fn create_isolated_session(&self) -> Result<(String, Self)> {
        let id = self.new_session_id()?;
        let target = self.session_path(&id);
        Ok((id, Self::with_current_path(self.base_path.clone(), target)))
    }

    fn with_current_path(base_path: PathBuf, current_path: PathBuf) -> Self {
        let current_id = current_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.jsonl")
            .to_owned();
        Self {
            base_path,
            current_path: RwLock::new(current_path),
            current_id_cache: std::sync::RwLock::new(current_id),
            turn_lock: Mutex::new(()),
        }
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.base_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(session_id)
    }

    pub(crate) fn path_for_session(&self, session_id: &str) -> PathBuf {
        self.session_path(session_id)
    }

    fn validate_session_id(&self, session_id: &str) -> Result<()> {
        if Path::new(session_id).file_name() != Some(OsStr::new(session_id))
            || !Self::is_session_name(&self.base_path, session_id)
        {
            return Err(SessionError::InvalidSessionId(session_id.to_owned()).into());
        }
        Ok(())
    }

    fn new_session_id(&self) -> Result<String> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SessionError::InvalidSystemTime)?
            .as_millis();
        let sequence = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let stem = self
            .base_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("session");
        let extension = self
            .base_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("jsonl");
        Ok(format!(
            "{stem}-{timestamp}-{}-{sequence}.{extension}",
            std::process::id()
        ))
    }

    fn is_session_name(base_path: &Path, name: &str) -> bool {
        let base_name = base_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("session.jsonl");
        let stem = base_path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("session");
        let extension = base_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("jsonl");
        name == base_name
            || name.starts_with(&format!("{base_name}.bak-"))
            || (name.starts_with(&format!("{stem}-")) && name.ends_with(&format!(".{extension}")))
    }

    pub(crate) fn pointer_path(base_path: &Path) -> PathBuf {
        let name = base_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("session.jsonl");
        base_path.with_file_name(format!("{name}.current"))
    }

    fn read_current_pointer(base_path: &Path) -> Option<PathBuf> {
        let id = std::fs::read_to_string(Self::pointer_path(base_path)).ok()?;
        let id = id.trim();
        if Path::new(id).file_name() != Some(OsStr::new(id))
            || !Self::is_session_name(base_path, id)
        {
            return None;
        }
        let candidate = base_path.parent()?.join(id);
        candidate
            .symlink_metadata()
            .ok()
            .is_some_and(|metadata| metadata.file_type().is_file())
            .then_some(candidate)
    }

    #[cfg(test)]
    async fn persist_current_pointer(&self, session_id: &str) -> Result<()> {
        let pointer = Self::pointer_path(&self.base_path);
        if let Some(parent) = pointer.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建会话目录失败: {}", parent.display()))?;
        }
        let pointer_name = pointer
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("session.jsonl.current");
        let temporary =
            pointer.with_file_name(format!("{pointer_name}.tmp-{}", std::process::id()));
        tokio::fs::write(&temporary, session_id)
            .await
            .with_context(|| format!("写入当前会话指针失败: {}", temporary.display()))?;
        tokio::fs::rename(&temporary, &pointer)
            .await
            .with_context(|| format!("提交当前会话指针失败: {}", pointer.display()))
    }

    fn preview(bytes: &[u8]) -> Option<String> {
        String::from_utf8_lossy(bytes).lines().find_map(|line| {
            let message = serde_json::from_str::<Message>(line).ok()?;
            if message.role != Role::User {
                return None;
            }
            let content = message
                .content?
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            (!content.is_empty()).then(|| content.chars().take(60).collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::provider::Role;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    fn temp_session() -> (SessionStore, PathBuf) {
        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "my-agent-session-{}-{id}.jsonl",
            std::process::id()
        ));
        (SessionStore::new(&path), path)
    }

    #[tokio::test]
    async fn appends_and_restores_messages() {
        let (store, path) = temp_session();
        store
            .append(&Message::text(Role::User, "问题"))
            .await
            .unwrap();
        store
            .append(&Message::text(Role::Assistant, "回答"))
            .await
            .unwrap();

        let restored = store.load().await.unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].content.as_deref(), Some("问题"));
        assert_eq!(restored[1].content.as_deref(), Some("回答"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn turn_lock_is_released_by_raii() {
        let (store, path) = temp_session();
        let store = Arc::new(store);
        let first = store.lock_turn().await;
        let second_store = store.clone();
        let task = tokio::spawn(async move {
            let _guard = second_store.lock_turn().await;
            true
        });
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        drop(first);
        assert!(task.await.unwrap());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn ignores_incomplete_final_line_after_crash() {
        let (store, path) = temp_session();
        let complete = serde_json::to_string(&Message::text(Role::User, "已保存")).unwrap();
        std::fs::write(&path, format!("{complete}\n{{\"role\":\"assistant\"")).unwrap();

        let restored = store.load().await.unwrap();

        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].content.as_deref(), Some("已保存"));
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn creates_stable_sessions_and_resumes_by_id() {
        let (store, base_path) = temp_session();
        store
            .append(&Message::text(Role::User, "旧会话问题"))
            .await
            .unwrap();
        let old_id = base_path.file_name().unwrap().to_string_lossy().to_string();

        let new_id = store.start_new().await.unwrap();
        store
            .append(&Message::text(Role::User, "新会话问题"))
            .await
            .unwrap();
        let sessions = store.list_sessions().await.unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].id, new_id);
        assert!(sessions[0].active);
        assert_eq!(sessions[0].preview.as_deref(), Some("新会话问题"));

        let restored = store.resume(&old_id).await.unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].content.as_deref(), Some("旧会话问题"));
        assert_eq!(store.current_id().await, old_id);

        let reopened = SessionStore::new(&base_path);
        assert_eq!(reopened.current_id().await, old_id);
        assert_eq!(reopened.load().await.unwrap().len(), 1);

        let new_path = base_path.with_file_name(new_id);
        let pointer_path = SessionStore::pointer_path(&base_path);
        let _ = std::fs::remove_file(base_path);
        let _ = std::fs::remove_file(new_path);
        let _ = std::fs::remove_file(pointer_path);
    }

    #[tokio::test]
    async fn refuses_session_path_traversal() {
        let (store, path) = temp_session();
        let error = store.resume("../session.jsonl").await.unwrap_err();
        assert!(error.to_string().contains("非法会话 ID"));
        let _ = std::fs::remove_file(path);
    }
}
