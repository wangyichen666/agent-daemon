use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;
use crate::safety::{PathIntent, SafetyPolicy};

pub struct WriteFileTool {
    safety: Arc<SafetyPolicy>,
}

impl WriteFileTool {
    pub fn new(safety: Arc<SafetyPolicy>) -> Self {
        Self { safety }
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "创建或完整覆盖一个 UTF-8 文本文件"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "目标文件路径"},
                "content": {"type": "string", "description": "完整文件内容"}
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let args: WriteArgs = serde_json::from_value(args).context("write_file 参数无效")?;
        let path = self
            .safety
            .authorize_path(&args.path, PathIntent::Write)
            .await?;
        tokio::fs::write(&path, args.content)
            .await
            .with_context(|| format!("写入文件失败: {}", path.display()))?;
        Ok(format!("已写入 {}", path.display()))
    }
}
