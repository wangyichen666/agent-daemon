use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Tool;
use crate::safety::{PathIntent, SafetyPolicy};

pub struct EditFileTool {
    safety: Arc<SafetyPolicy>,
}

impl EditFileTool {
    pub fn new(safety: Arc<SafetyPolicy>) -> Self {
        Self { safety }
    }
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "用精确字符串替换修改 UTF-8 文本文件；默认要求旧文本只出现一次"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "目标文件路径"},
                "old_text": {"type": "string", "description": "要替换的原文本"},
                "new_text": {"type": "string", "description": "替换后的文本"},
                "replace_all": {"type": "boolean", "description": "是否替换所有匹配，默认 false"}
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let args: EditArgs = serde_json::from_value(args).context("edit_file 参数无效")?;
        if args.old_text.is_empty() {
            bail!("old_text 不能为空");
        }
        let path = self
            .safety
            .authorize_path(&args.path, PathIntent::Edit)
            .await?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .with_context(|| format!("读取待编辑文件失败: {}", path.display()))?;
        let count = content.matches(&args.old_text).count();
        if count == 0 {
            bail!("未找到 old_text，文件未修改");
        }
        if count > 1 && !args.replace_all {
            bail!("old_text 出现 {count} 次；请提供更精确的文本或设置 replace_all=true");
        }

        let updated = if args.replace_all {
            content.replace(&args.old_text, &args.new_text)
        } else {
            content.replacen(&args.old_text, &args.new_text, 1)
        };
        tokio::fs::write(&path, updated)
            .await
            .with_context(|| format!("写回编辑结果失败: {}", path.display()))?;
        Ok(format!("已编辑 {}，替换 {count} 处", path.display()))
    }
}
