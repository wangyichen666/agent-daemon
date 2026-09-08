use std::collections::HashSet;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::tools::Tool;

static NEXT_MEMORY_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntry {
    pub id: String,
    pub content: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub manual: bool,
}

#[derive(Debug, Error)]
enum MemoryError {
    #[error("memory.jsonl 第 {line} 行损坏: {source}")]
    CorruptLine {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("系统时间早于 UNIX_EPOCH")]
    InvalidSystemTime,
}

pub struct MemoryStore {
    path: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub fn from_env(workspace: &Path) -> Self {
        let configured = env::var_os("MEMORY_PATH").map(PathBuf::from);
        let path = match configured {
            Some(path) if path.is_absolute() => path,
            Some(path) => workspace.join(path),
            None => workspace.join(".my-agent/memory.jsonl"),
        };
        Self::new(path)
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            write_lock: Mutex::new(()),
        }
    }

    pub async fn save(&self, content: String, ttl_days: Option<u64>) -> Result<MemoryEntry> {
        let _guard = self.write_lock.lock().await;
        let now = unix_time()?;
        let expires_at = ttl_days.map(|days: u64| {
            now.saturating_add(
                days.saturating_mul(24)
                    .saturating_mul(60)
                    .saturating_mul(60),
            )
        });
        let sequence = NEXT_MEMORY_ID.fetch_add(1, Ordering::Relaxed);
        let entry = MemoryEntry {
            id: format!("mem-{now}-{sequence}"),
            content,
            created_at: now,
            expires_at,
            manual: true,
        };
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建记忆目录失败: {}", parent.display()))?;
        }
        let mut line = serde_json::to_vec(&entry).context("序列化记忆失败")?;
        line.push(b'\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("打开记忆文件失败: {}", self.path.display()))?;
        file.write_all(&line)
            .await
            .with_context(|| format!("追加记忆失败: {}", self.path.display()))?;
        file.flush().await.context("刷新记忆文件失败")?;
        Ok(entry)
    }

    pub async fn recall(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>> {
        let entries = self.load().await?;
        let now = unix_time()?;
        let query_terms = terms(query);
        let query_lower = query.to_lowercase();
        let mut ranked = entries
            .into_iter()
            .filter(|entry: &MemoryEntry| entry.expires_at.is_none_or(|expiry: u64| expiry > now))
            .filter_map(|entry: MemoryEntry| {
                let content_lower = entry.content.to_lowercase();
                let overlap = terms(&entry.content).intersection(&query_terms).count();
                let substring_bonus = usize::from(
                    !query_lower.is_empty()
                        && (content_lower.contains(&query_lower)
                            || query_lower.contains(&content_lower)),
                );
                let score = overlap.saturating_add(substring_bonus.saturating_mul(100));
                (score > 0).then_some((score, entry))
            })
            .collect::<Vec<(usize, MemoryEntry)>>();
        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.manual.cmp(&left.1.manual))
                .then_with(|| right.1.created_at.cmp(&left.1.created_at))
        });
        Ok(ranked
            .into_iter()
            .take(limit.clamp(1, 20))
            .map(|(_, entry): (usize, MemoryEntry)| entry)
            .collect())
    }

    async fn load(&self) -> Result<Vec<MemoryEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let content = tokio::fs::read_to_string(&self.path)
            .await
            .with_context(|| format!("读取记忆文件失败: {}", self.path.display()))?;
        content
            .lines()
            .enumerate()
            .filter(|(_, line): &(usize, &str)| !line.trim().is_empty())
            .map(|(index, line): (usize, &str)| {
                serde_json::from_str(line).map_err(|source| {
                    MemoryError::CorruptLine {
                        line: index + 1,
                        source,
                    }
                    .into()
                })
            })
            .collect()
    }
}

pub struct RememberTool {
    store: Arc<MemoryStore>,
}

impl RememberTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct RememberArgs {
    content: String,
    ttl_days: Option<u64>,
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "长期保存用户明确要求记住的偏好、约定或项目事实"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {"type": "string", "description": "要记住的自足事实"},
                "ttl_days": {"type": "integer", "description": "可选有效天数；省略表示长期有效"}
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let args: RememberArgs = serde_json::from_value(args).context("remember 参数无效")?;
        let entry = self.store.save(args.content, args.ttl_days).await?;
        Ok(format!("已保存长期记忆，id={}", entry.id))
    }
}

pub struct RecallMemoryTool {
    store: Arc<MemoryStore>,
}

impl RecallMemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self { store }
    }
}

#[derive(Deserialize)]
struct RecallArgs {
    query: String,
    #[serde(default = "default_recall_limit")]
    limit: usize,
}

#[async_trait]
impl Tool for RecallMemoryTool {
    fn name(&self) -> &str {
        "recall_memory"
    }

    fn description(&self) -> &str {
        "按关键词与中文相邻双字召回长期记忆；处理偏好或既有约定前可调用"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "召回查询"},
                "limit": {"type": "integer", "description": "最多返回条数，默认 5，最大 20"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let args: RecallArgs = serde_json::from_value(args).context("recall_memory 参数无效")?;
        let entries = self.store.recall(&args.query, args.limit).await?;
        if entries.is_empty() {
            return Ok("未找到相关长期记忆".to_owned());
        }
        serde_json::to_string_pretty(&entries).context("序列化召回结果失败")
    }
}

fn default_recall_limit() -> usize {
    5
}

fn terms(text: &str) -> HashSet<String> {
    let mut output = HashSet::new();
    let mut ascii = String::new();
    let mut cjk_run = Vec::<char>::new();

    let flush_ascii = |buffer: &mut String, destination: &mut HashSet<String>| {
        if !buffer.is_empty() {
            destination.insert(std::mem::take(buffer));
        }
    };
    let flush_cjk = |buffer: &mut Vec<char>, destination: &mut HashSet<String>| {
        if buffer.len() == 1 {
            destination.insert(buffer[0].to_string());
        } else {
            for pair in buffer.windows(2) {
                destination.insert(pair.iter().collect());
            }
        }
        buffer.clear();
    };

    for character in text.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
            flush_cjk(&mut cjk_run, &mut output);
            ascii.push(character);
        } else if is_cjk(character) {
            flush_ascii(&mut ascii, &mut output);
            cjk_run.push(character);
        } else {
            flush_ascii(&mut ascii, &mut output);
            flush_cjk(&mut cjk_run, &mut output);
        }
    }
    flush_ascii(&mut ascii, &mut output);
    flush_cjk(&mut cjk_run, &mut output);
    output
}

fn is_cjk(character: char) -> bool {
    matches!(
        character as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF
    )
}

fn unix_time() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| MemoryError::InvalidSystemTime.into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

    fn temp_store() -> (MemoryStore, PathBuf) {
        let id = NEXT_TEST.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("my-agent-memory-{}-{id}.jsonl", std::process::id()));
        (MemoryStore::new(&path), path)
    }

    #[test]
    fn extracts_ascii_keywords_and_chinese_bigrams() {
        let found = terms("Rust 项目偏好 async-trait");
        assert!(found.contains("rust"));
        assert!(found.contains("项目"));
        assert!(found.contains("偏好"));
        assert!(found.contains("async-trait"));
    }

    #[tokio::test]
    async fn saves_and_recalls_relevant_memory() {
        let (store, path) = temp_store();
        store
            .save("用户偏好使用 Rust 和中文注释".to_owned(), None)
            .await
            .unwrap();
        store
            .save("数据库使用 SQLite".to_owned(), None)
            .await
            .unwrap();

        let recalled = store.recall("Rust 编程偏好", 5).await.unwrap();

        assert_eq!(recalled.len(), 1);
        assert!(recalled[0].content.contains("Rust"));
        let _ = std::fs::remove_file(path);
    }
}
