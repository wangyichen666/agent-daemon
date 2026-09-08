use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, MutexGuard};
use tracing::warn;

use crate::provider::Message;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session.jsonl 第 {line} 行损坏: {source}")]
    CorruptLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("系统时间早于 UNIX_EPOCH")]
    InvalidSystemTime,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub active: bool,
    pub message_count: usize,
    pub modified_at: Option<u64>,
}

pub struct SessionStore {
    path: PathBuf,
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
        Self {
            path: path.into(),
            turn_lock: Mutex::new(()),
        }
    }

    pub fn current_id(&self) -> String {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.jsonl")
            .to_owned()
    }

    pub async fn lock_turn(&self) -> MutexGuard<'_, ()> {
        self.turn_lock.lock().await
    }

    pub async fn load(&self) -> Result<Vec<Message>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let bytes = tokio::fs::read(&self.path)
            .await
            .with_context(|| format!("读取会话失败: {}", self.path.display()))?;
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
                        path = %self.path.display(),
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
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建会话目录失败: {}", parent.display()))?;
        }
        let mut line = serde_json::to_vec(message).context("序列化会话消息失败")?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("打开会话文件失败: {}", self.path.display()))?;
        file.write_all(&line)
            .await
            .with_context(|| format!("追加会话消息失败: {}", self.path.display()))?;
        file.flush()
            .await
            .with_context(|| format!("刷新会话文件失败: {}", self.path.display()))?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionInfo>> {
        let Some(directory) = self.path.parent() else {
            return Ok(Vec::new());
        };
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let active_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.jsonl")
            .to_owned();
        let backup_prefix = format!("{active_name}.bak-");
        let mut entries = tokio::fs::read_dir(directory)
            .await
            .with_context(|| format!("读取会话目录失败: {}", directory.display()))?;
        let mut sessions = Vec::new();
        while let Some(entry) = entries.next_entry().await.context("遍历会话目录失败")? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            let active = name == active_name;
            if !active && !name.starts_with(&backup_prefix) {
                continue;
            }
            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("读取会话元数据失败: {}", path.display()))?;
            let message_count = String::from_utf8_lossy(&bytes)
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
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

    pub async fn rotate_existing(&self) -> Result<Option<PathBuf>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| SessionError::InvalidSystemTime)?
            .as_secs();
        let file_name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("session.jsonl");
        let backup = self
            .path
            .with_file_name(format!("{file_name}.bak-{timestamp}"));
        tokio::fs::rename(&self.path, &backup)
            .await
            .with_context(|| format!("轮换旧会话失败: {}", self.path.display()))?;
        Ok(Some(backup))
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
}
