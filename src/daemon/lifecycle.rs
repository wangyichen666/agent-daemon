use std::env;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;
use tokio::process::Command;

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub directory: PathBuf,
    pub socket: PathBuf,
    pub pid: PathBuf,
    pub ready: PathBuf,
    pub log: PathBuf,
    startup_lock: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DaemonStatus {
    Ready { pid: u32 },
    Starting { pid: Option<u32> },
    Stale { pid: Option<u32> },
    Stopped,
}

#[derive(Deserialize, Serialize)]
struct ReadyMarker {
    pid: u32,
    workspace: PathBuf,
    version: String,
}

impl RuntimePaths {
    pub fn for_workspace(workspace: &Path) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace)
            .with_context(|| format!("无法解析工作区路径: {}", workspace.display()))?;
        let base = env::var_os("MY_AGENT_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| env::temp_dir().join("my-agent"));
        let directory = base.join(format!("{:016x}", stable_workspace_hash(&workspace)));
        Ok(Self {
            socket: directory.join("daemon.sock"),
            pid: directory.join("daemon.pid"),
            ready: directory.join("ready.json"),
            log: directory.join("daemon.log"),
            startup_lock: directory.join("startup.lock"),
            directory,
        })
    }

    #[cfg(test)]
    pub fn for_test(directory: PathBuf) -> Self {
        Self {
            socket: directory.join("daemon.sock"),
            pid: directory.join("daemon.pid"),
            ready: directory.join("ready.json"),
            log: directory.join("daemon.log"),
            startup_lock: directory.join("startup.lock"),
            directory,
        }
    }

    pub async fn prepare(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.directory)
            .await
            .with_context(|| format!("创建运行目录失败: {}", self.directory.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700))
                .await
                .with_context(|| format!("设置运行目录权限失败: {}", self.directory.display()))?;
        }
        Ok(())
    }

    pub async fn mark_ready(&self, workspace: &Path) -> Result<()> {
        tokio::fs::write(&self.pid, std::process::id().to_string())
            .await
            .with_context(|| format!("写入 PID 文件失败: {}", self.pid.display()))?;
        let marker = ReadyMarker {
            pid: std::process::id(),
            workspace: workspace.to_path_buf(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        let bytes = serde_json::to_vec_pretty(&marker).context("序列化 ready 标记失败")?;
        tokio::fs::write(&self.ready, bytes)
            .await
            .with_context(|| format!("写入 ready 标记失败: {}", self.ready.display()))
    }

    pub async fn cleanup(&self) {
        for path in [&self.socket, &self.pid, &self.ready, &self.startup_lock] {
            if let Err(error) = tokio::fs::remove_file(path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(path = %path.display(), %error, "清理 daemon 运行文件失败");
            }
        }
    }

    pub async fn status(&self) -> DaemonStatus {
        if UnixStream::connect(&self.socket).await.is_ok() {
            return DaemonStatus::Ready {
                pid: read_pid(&self.pid).await.unwrap_or(0),
            };
        }
        let pid = read_pid(&self.pid).await;
        if self.ready.exists() || self.socket.exists() {
            return DaemonStatus::Stale { pid };
        }
        if let Some(pid) = pid {
            if process_is_alive(pid).await {
                DaemonStatus::Starting { pid: Some(pid) }
            } else {
                DaemonStatus::Stale { pid: Some(pid) }
            }
        } else {
            DaemonStatus::Stopped
        }
    }

    pub async fn ensure_daemon(&self, workspace: &Path) -> Result<()> {
        self.prepare().await?;
        if matches!(self.status().await, DaemonStatus::Ready { .. }) {
            return Ok(());
        }
        if matches!(self.status().await, DaemonStatus::Stale { .. }) {
            self.cleanup().await;
            self.prepare().await?;
        }
        if startup_lock_is_stale(&self.startup_lock).await {
            let _ = tokio::fs::remove_file(&self.startup_lock).await;
        }

        let startup_guard = match OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&self.startup_lock)
        {
            Ok(file) => Some(StartupGuard {
                path: self.startup_lock.clone(),
                _file: file,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(error) => return Err(error).context("创建 daemon 启动锁失败"),
        };
        if startup_guard.is_some() && !matches!(self.status().await, DaemonStatus::Ready { .. }) {
            self.spawn_daemon(workspace)?;
        }

        for _ in 0..60 {
            if matches!(self.status().await, DaemonStatus::Ready { .. }) {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!(
            "daemon 启动超时；请查看日志 {}，或运行 `my-agent config check`",
            self.log.display()
        )
    }

    fn spawn_daemon(&self, workspace: &Path) -> Result<()> {
        #[cfg(unix)]
        use std::os::unix::process::CommandExt;

        let executable = env::current_exe().context("定位 my-agent 可执行文件失败")?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("打开 daemon 日志失败: {}", self.log.display()))?;
        let stderr = log.try_clone().context("复制 daemon 日志句柄失败")?;
        let mut command = std::process::Command::new(executable);
        command
            .arg("daemon")
            .arg("--workspace")
            .arg(workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(stderr));
        #[cfg(unix)]
        command.process_group(0);
        command.spawn().context("拉起 daemon 失败")?;
        Ok(())
    }
}

struct StartupGuard {
    path: PathBuf,
    _file: std::fs::File,
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn read_pid(path: &Path) -> Option<u32> {
    tokio::fs::read_to_string(path)
        .await
        .ok()
        .and_then(|value| value.trim().parse().ok())
}

async fn process_is_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .await
        .is_ok_and(|status| status.success())
}

async fn startup_lock_is_stale(path: &Path) -> bool {
    let Ok(metadata) = tokio::fs::metadata(path).await else {
        return false;
    };
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= Duration::from_secs(10))
}

fn stable_workspace_hash(workspace: &Path) -> u64 {
    struct FnvHasher(u64);

    impl Hasher for FnvHasher {
        fn finish(&self) -> u64 {
            self.0
        }

        fn write(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.0 ^= u64::from(*byte);
                self.0 = self.0.wrapping_mul(0x100000001b3);
            }
        }
    }

    let mut hasher = FnvHasher(0xcbf29ce484222325);
    workspace.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_are_stable_and_isolated() {
        let current = std::env::current_dir().unwrap();
        let first = RuntimePaths::for_workspace(&current).unwrap();
        let second = RuntimePaths::for_workspace(&current).unwrap();

        assert_eq!(first.socket, second.socket);
        assert!(first.socket.ends_with("daemon.sock"));
        assert!(first.directory.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn workspace_hash_is_deterministic() {
        let path = Path::new("/tmp/example");
        assert_eq!(stable_workspace_hash(path), stable_workspace_hash(path));
        assert_ne!(
            stable_workspace_hash(path),
            stable_workspace_hash(Path::new("/tmp/other"))
        );
    }
}
