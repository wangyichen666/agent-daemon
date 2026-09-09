use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use super::{Tool, ToolCancellation, ToolOutput};
use crate::safety::SafetyPolicy;

const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 300;

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
        let output = self.execute_command(args, &NeverCancelled).await?;
        Ok(output)
    }

    async fn execute_rich_with_cancellation(
        &self,
        args: Value,
        cancellation: &dyn ToolCancellation,
    ) -> Result<ToolOutput> {
        self.execute_command(args, cancellation)
            .await
            .map(ToolOutput::text)
    }
}

impl ExecTool {
    async fn execute_command(
        &self,
        args: Value,
        cancellation: &dyn ToolCancellation,
    ) -> Result<String> {
        let args: ExecArgs = serde_json::from_value(args).context("exec 参数无效")?;
        self.safety.authorize_command(&args.command).await?;
        if cancellation.is_cancelled() {
            bail!("命令执行已取消");
        }

        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(&args.command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);

        let child = command
            .spawn()
            .with_context(|| format!("执行命令失败: {}", args.command))?;
        let process_id = child.id();
        let timeout = execution_timeout();
        let wait = child.wait_with_output();
        tokio::pin!(wait);
        let output = tokio::select! {
            result = &mut wait => result.with_context(|| format!("等待命令结束失败: {}", args.command))?,
            _ = cancellation.cancelled() => {
                terminate_process_group(process_id);
                let _ = (&mut wait).await;
                bail!("命令执行已取消: {}", args.command);
            }
            _ = tokio::time::sleep(timeout) => {
                terminate_process_group(process_id);
                let _ = (&mut wait).await;
                bail!("命令执行超时（{} 秒）: {}", timeout.as_secs(), args.command);
            }
        };

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

struct NeverCancelled;

#[async_trait]
impl ToolCancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }

    async fn cancelled(&self) {
        std::future::pending::<()>().await;
    }
}

fn execution_timeout() -> Duration {
    std::env::var("MY_AGENT_EXEC_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_EXEC_TIMEOUT_SECS))
}

#[cfg(unix)]
fn terminate_process_group(process_id: Option<u32>) {
    if let Some(process_id) = process_id
        && let Ok(process_group) = i32::try_from(process_id)
    {
        // `process_group(0)` puts the shell and its descendants in their own
        // group, so cancelling an exec cannot leave a Maven/Java child behind.
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_process_group(_process_id: Option<u32>) {}

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
    use std::time::Duration;

    use super::*;
    use crate::loop_engine::CancellationToken;
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

    #[tokio::test]
    async fn cancellation_terminates_a_long_running_command() {
        let safety = Arc::new(
            SafetyPolicy::new(std::env::current_dir().unwrap(), Arc::new(AllowApproval)).unwrap(),
        );
        let tool = ExecTool::new(safety);
        let cancellation = CancellationToken::new();
        let future =
            tool.execute_rich_with_cancellation(json!({"command": "sleep 30"}), &cancellation);
        tokio::pin!(future);
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancellation.cancel();
        let result = match tokio::time::timeout(Duration::from_secs(2), &mut future)
            .await
            .expect("取消长命令不应超过 2 秒")
        {
            Ok(_) => panic!("命令取消后不应成功"),
            Err(error) => error,
        };
        assert!(result.to_string().contains("已取消"));
    }
}
