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
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("创建父目录失败: {}", parent.display()))?;
        }
        tokio::fs::write(&path, args.content)
            .await
            .with_context(|| format!("写入文件失败: {}", path.display()))?;
        Ok(format!("已写入 {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::safety::Approval;

    struct AllowApproval;

    #[async_trait]
    impl Approval for AllowApproval {
        async fn request(&self, _prompt: &str) -> Result<bool> {
            Ok(true)
        }
    }

    #[tokio::test]
    async fn creates_missing_parent_directories_before_writing() {
        let workspace = std::env::current_dir().expect("测试工作区应存在");
        let root = workspace.join(format!(".my-agent-write-test-{}", std::process::id()));
        let path = root.join("src/static/css/style.css");
        let _ = std::fs::remove_dir_all(&root);
        let safety = Arc::new(
            SafetyPolicy::new(&workspace, Arc::new(AllowApproval)).expect("测试安全策略应可创建"),
        );
        let tool = WriteFileTool::new(safety);

        let result = tool
            .execute(json!({"path": path, "content": "body {}"}))
            .await
            .expect("嵌套文件应可写入");

        assert!(result.contains("style.css"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "body {}");
        std::fs::remove_dir_all(root).expect("应清理测试目录");
    }
}
