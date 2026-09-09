use std::ffi::OsString;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use thiserror::Error;

#[derive(Clone, Copy, Debug)]
pub enum PathIntent {
    Read,
    Write,
    Edit,
}

impl fmt::Display for PathIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Read => "读取",
            Self::Write => "写入",
            Self::Edit => "编辑",
        };
        formatter.write_str(label)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandDecision {
    Allowed,
    NeedsApproval(String),
    Blocked(String),
}

#[derive(Debug, Error)]
pub enum SafetyError {
    #[error("已拦截灾难性命令: {0}")]
    BlockedCommand(String),
    #[error("工作区外读取被拒绝: {0}")]
    OutsideRead(PathBuf),
    #[error("用户拒绝了操作: {0}")]
    UserRejected(String),
    #[error("无法解析路径: {0}")]
    InvalidPath(PathBuf),
}

#[async_trait]
pub trait Approval: Send + Sync {
    async fn request(&self, prompt: &str) -> Result<bool>;
}

pub struct SafetyPolicy {
    workspace: PathBuf,
    approval: Arc<dyn Approval>,
}

impl SafetyPolicy {
    pub fn new(workspace: impl AsRef<Path>, approval: Arc<dyn Approval>) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace.as_ref())
            .with_context(|| format!("无法解析工作区路径: {}", workspace.as_ref().display()))?;
        Ok(Self {
            workspace,
            approval,
        })
    }

    pub async fn authorize_path(
        &self,
        requested: impl AsRef<Path>,
        intent: PathIntent,
    ) -> Result<PathBuf> {
        let resolved = self.resolve_path(requested.as_ref())?;
        if resolved.starts_with(&self.workspace) {
            return Ok(resolved);
        }

        if matches!(intent, PathIntent::Read) {
            return Err(SafetyError::OutsideRead(resolved).into());
        }

        let prompt = format!("{intent}工作区外路径 {}", resolved.display());
        if self.approval.request(&prompt).await? {
            Ok(resolved)
        } else {
            Err(SafetyError::UserRejected(prompt).into())
        }
    }

    pub async fn authorize_command(&self, command: &str) -> Result<()> {
        match classify_command(command) {
            CommandDecision::Allowed => Ok(()),
            CommandDecision::Blocked(reason) => Err(SafetyError::BlockedCommand(reason).into()),
            CommandDecision::NeedsApproval(reason) => {
                let prompt = format!("执行高风险命令（{reason}）：{command}");
                if self.approval.request(&prompt).await? {
                    Ok(())
                } else {
                    Err(SafetyError::UserRejected(prompt).into())
                }
            }
        }
    }

    pub async fn authorize_external_action(
        &self,
        description: &str,
        arguments: &Value,
    ) -> Result<()> {
        let mut notices = Vec::new();
        self.inspect_external_arguments(arguments, None, &mut notices)?;
        let arguments = arguments
            .to_string()
            .chars()
            .take(1_000)
            .collect::<String>();
        let details = if notices.is_empty() {
            String::new()
        } else {
            format!("；安全提示：{}", notices.join("；"))
        };
        let prompt = format!(
            "执行外部 MCP 工具（默认视为有副作用）：{description}；参数：{arguments}{details}"
        );
        if self.approval.request(&prompt).await? {
            Ok(())
        } else {
            Err(SafetyError::UserRejected(prompt).into())
        }
    }

    fn inspect_external_arguments(
        &self,
        value: &Value,
        key: Option<&str>,
        notices: &mut Vec<String>,
    ) -> Result<()> {
        match value {
            Value::Object(object) => {
                for (key, value) in object {
                    self.inspect_external_arguments(value, Some(key), notices)?;
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.inspect_external_arguments(item, key, notices)?;
                }
            }
            Value::String(text) if key.is_some_and(is_command_key) => {
                match classify_command(text) {
                    CommandDecision::Blocked(reason) => {
                        return Err(SafetyError::BlockedCommand(reason).into());
                    }
                    CommandDecision::NeedsApproval(reason) => {
                        notices.push(format!("高风险命令：{reason}"));
                    }
                    CommandDecision::Allowed => {}
                }
            }
            Value::String(text) if key.is_some_and(is_path_key) => {
                let resolved = self.resolve_path(Path::new(text))?;
                if !resolved.starts_with(&self.workspace) {
                    notices.push(format!("工作区外路径：{}", resolved.display()));
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn resolve_path(&self, requested: &Path) -> Result<PathBuf> {
        let candidate = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.workspace.join(requested)
        };
        let normalized = lexical_normalize(&candidate);
        resolve_existing_prefix(&normalized)
    }
}

fn is_command_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "command" | "cmd" | "shell"
    )
}

fn is_path_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "path" | "paths" | "cwd" | "directory" | "root"
    ) || key.ends_with("_path")
}

pub fn classify_command(command: &str) -> CommandDecision {
    let normalized = command
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_ascii_lowercase();
    let compact = normalized
        .chars()
        .filter(|character: &char| !character.is_whitespace())
        .collect::<String>();

    if compact.contains(":(){:|:&};:") {
        return CommandDecision::Blocked("fork 炸弹".to_owned());
    }
    if contains_program(&normalized, &["mkfs", "wipefs"]) {
        return CommandDecision::Blocked("文件系统擦除/格式化".to_owned());
    }
    if contains_program(&normalized, &["fdisk", "sfdisk", "parted"]) && normalized.contains("/dev/")
    {
        return CommandDecision::Blocked("直接修改磁盘分区".to_owned());
    }
    if contains_program(&normalized, &["dd"]) && compact.contains("of=/dev/") {
        return CommandDecision::Blocked("向块设备直接写入".to_owned());
    }
    if [">/dev/sd", ">/dev/nvme", ">/dev/mmcblk"]
        .iter()
        .any(|pattern: &&str| compact.contains(pattern))
    {
        return CommandDecision::Blocked("重定向覆盖块设备".to_owned());
    }
    if is_catastrophic_rm(&normalized) {
        return CommandDecision::Blocked("递归强制删除系统或主目录".to_owned());
    }
    if contains_program(&normalized, &["chmod"])
        && normalized.contains("-r")
        && normalized.contains(" 000 ")
        && targets_root(&normalized)
    {
        return CommandDecision::Blocked("递归移除根目录权限".to_owned());
    }
    if contains_program(&normalized, &["chown"])
        && normalized.contains("-r")
        && targets_root(&normalized)
    {
        return CommandDecision::Blocked("递归修改根目录所有权".to_owned());
    }
    if contains_program(&normalized, &["mv"]) && normalized.contains(" /dev/null") {
        return CommandDecision::Blocked("将文件移动到 /dev/null".to_owned());
    }
    if contains_program(&normalized, &["find"])
        && targets_root(&normalized)
        && normalized.contains("-delete")
    {
        return CommandDecision::Blocked("从根目录递归删除".to_owned());
    }

    if contains_program(&normalized, &["kill", "pkill", "killall"])
        || contains_program(&normalized, &["sudo", "su"])
        || contains_program(&normalized, &["shutdown", "reboot", "poweroff", "halt"])
        || normalized.contains("git reset --hard")
        || normalized.contains("git clean -fd")
        || normalized.contains("cargo publish")
        || (normalized.contains("curl ") && normalized.contains("| sh"))
        || (normalized.contains("wget ") && normalized.contains("| sh"))
    {
        return CommandDecision::NeedsApproval("可能影响进程、系统或难以恢复的状态".to_owned());
    }

    CommandDecision::Allowed
}

fn contains_program(command: &str, programs: &[&str]) -> bool {
    command
        .split(|character: char| {
            character.is_whitespace() || matches!(character, ';' | '|' | '&' | '(' | ')')
        })
        .any(|token: &str| {
            let base = token.rsplit('/').next().unwrap_or(token);
            programs
                .iter()
                .any(|program: &&str| base == *program || base.starts_with(&format!("{program}.")))
        })
}

fn is_catastrophic_rm(command: &str) -> bool {
    if !contains_program(command, &["rm"]) {
        return false;
    }
    let tokens = command.split_whitespace().collect::<Vec<&str>>();
    let recursive = tokens
        .iter()
        .any(|token: &&str| token.starts_with('-') && token.contains('r'));
    let force = tokens
        .iter()
        .any(|token: &&str| token.starts_with('-') && token.contains('f'));
    let catastrophic_target = tokens.iter().any(|token: &&str| {
        matches!(
            token.trim_matches(['\'', '"']),
            "/" | "/*" | "~" | "~/" | "$home" | "${home}"
        )
    });
    recursive && force && catastrophic_target
}

fn targets_root(command: &str) -> bool {
    command
        .split_whitespace()
        .any(|token: &str| matches!(token.trim_matches(['\'', '"']), "/" | "/*"))
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

fn resolve_existing_prefix(path: &Path) -> Result<PathBuf> {
    let mut existing = path;
    let mut suffix = Vec::<OsString>::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(SafetyError::InvalidPath(path.to_path_buf()).into());
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return Err(SafetyError::InvalidPath(path.to_path_buf()).into());
        };
        existing = parent;
    }

    let mut resolved = std::fs::canonicalize(existing)
        .with_context(|| format!("无法解析路径前缀: {}", existing.display()))?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FixedApproval {
        allowed: bool,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl Approval for FixedApproval {
        async fn request(&self, _prompt: &str) -> Result<bool> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.allowed)
        }
    }

    #[test]
    fn blocks_catastrophic_commands() {
        let commands = [
            "rm -rf /",
            "rm -fr ~",
            "mkfs.ext4 /dev/sda",
            "wipefs -a /dev/sda",
            "dd if=/dev/zero of=/dev/sda",
            "echo bad > /dev/nvme0n1",
            ":(){ :|:& };:",
            "chmod -R 000 / ",
            "chown -R nobody /",
            "find / -delete",
            "mv important /dev/null",
            "parted /dev/sda mklabel gpt",
        ];
        for command in commands {
            assert!(
                matches!(classify_command(command), CommandDecision::Blocked(_)),
                "未拦截: {command}"
            );
        }
    }

    #[test]
    fn kill_requires_approval_instead_of_hard_block() {
        assert!(matches!(
            classify_command("pkill my-server"),
            CommandDecision::NeedsApproval(_)
        ));
    }

    #[tokio::test]
    async fn outside_write_uses_approval_and_outside_read_is_denied() {
        let approval = Arc::new(FixedApproval {
            allowed: true,
            calls: AtomicUsize::new(0),
        });
        let workspace = std::env::current_dir().unwrap();
        let policy = SafetyPolicy::new(&workspace, approval.clone()).unwrap();
        let outside = workspace.parent().unwrap().join("outside-file.txt");

        assert!(
            policy
                .authorize_path(&outside, PathIntent::Read)
                .await
                .is_err()
        );
        assert_eq!(approval.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            policy
                .authorize_path(&outside, PathIntent::Write)
                .await
                .unwrap(),
            outside
        );
        assert_eq!(approval.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn external_tool_arguments_keep_command_and_path_safety_checks() {
        let approval = Arc::new(FixedApproval {
            allowed: true,
            calls: AtomicUsize::new(0),
        });
        let workspace = std::env::current_dir().unwrap();
        let policy = SafetyPolicy::new(&workspace, approval.clone()).unwrap();
        assert!(
            policy
                .authorize_external_action(
                    "server / tool",
                    &serde_json::json!({"command": "rm -rf /"}),
                )
                .await
                .is_err()
        );
        assert_eq!(approval.calls.load(Ordering::SeqCst), 0);

        let outside = workspace.parent().unwrap().join("external-output.txt");
        policy
            .authorize_external_action(
                "server / tool",
                &serde_json::json!({"output_path": outside}),
            )
            .await
            .unwrap();
        assert_eq!(approval.calls.load(Ordering::SeqCst), 1);
    }
}
