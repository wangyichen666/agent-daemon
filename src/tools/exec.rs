use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::Tool;
use crate::safety::SafetyPolicy;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub struct ExecTool {
    safety: Arc<SafetyPolicy>,
}

impl ExecTool {
    pub fn new(safety: Arc<SafetyPolicy>) -> Self {
        Self { safety }
    }
}

#[derive(Deserialize)]
struct ExecArgs {
    command: String,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "exec"
    }

    fn description(&self) -> &str {
        "在当前工作目录执行一条 shell 命令，返回退出码、stdout 与 stderr"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "要执行的 shell 命令" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let args: ExecArgs = serde_json::from_value(args).context("exec 参数无效")?;
        self.safety.authorize_command(&args.command).await?;
        let output = Command::new("sh")
            .arg("-lc")
            .arg(&args.command)
            .output()
            .await
            .with_context(|| format!("执行命令失败: {}", args.command))?;

        let stdout = limited_lossy(&output.stdout);
        let stderr = limited_lossy(&output.stderr);
        Ok(format!(
            "exit_code: {}\nstdout:\n{}\nstderr:\n{}",
            output.status.code().unwrap_or(-1),
            stdout,
            stderr
        ))
    }
}

fn limited_lossy(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_OUTPUT_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut output = String::from_utf8_lossy(&bytes[..MAX_OUTPUT_BYTES]).into_owned();
    output.push_str(&format!("\n...[输出已截断，原始大小 {} 字节]", bytes.len()));
    output
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
    async fn executes_shell_command() {
        let safety = Arc::new(
            SafetyPolicy::new(std::env::current_dir().unwrap(), Arc::new(AllowApproval)).unwrap(),
        );
        let output = ExecTool::new(safety)
            .execute(json!({"command": "printf hello"}))
            .await
            .unwrap();
        assert!(output.contains("exit_code: 0"));
        assert!(output.contains("hello"));
    }

    #[tokio::test]
    async fn refuses_catastrophic_command() {
        let safety = Arc::new(
            SafetyPolicy::new(std::env::current_dir().unwrap(), Arc::new(AllowApproval)).unwrap(),
        );
        let error = ExecTool::new(safety)
            .execute(json!({"command": "rm -rf /"}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("已拦截"));
    }
}
