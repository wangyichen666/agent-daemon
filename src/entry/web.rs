use std::fs::OpenOptions;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_WEB_BIND: &str = "127.0.0.1:8787";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebLaunch {
    pub url: String,
    pub started: bool,
}

#[derive(Deserialize)]
struct HealthResponse {
    workspace: PathBuf,
}

enum ProbeResult {
    Missing,
    Healthy,
    Unhealthy,
}

pub fn configured_address() -> Result<SocketAddr> {
    std::env::var("MY_AGENT_WEB_ADDR")
        .unwrap_or_else(|_| DEFAULT_WEB_BIND.to_owned())
        .parse()
        .context("MY_AGENT_WEB_ADDR 必须是 host:port，例如 127.0.0.1:8787")
}

pub fn configured_url() -> String {
    configured_address()
        .map(|address| format!("http://{address}"))
        .unwrap_or_else(|_| format!("http://{DEFAULT_WEB_BIND}"))
}

pub async fn ensure_and_open(workspace: &Path) -> Result<WebLaunch> {
    let launch = ensure_running(workspace).await?;
    open_browser(&launch.url)?;
    Ok(launch)
}

pub async fn ensure_running(workspace: &Path) -> Result<WebLaunch> {
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("无法解析 Web 工作区: {}", workspace.display()))?;
    let address = configured_address()?;
    if !address.ip().is_loopback() {
        bail!("TUI /web 只允许自动启动回环地址，当前为 {address}");
    }
    let base_url = format!("http://{address}");
    let url = workspace_url(&base_url, &workspace)?;
    match probe(&base_url).await? {
        ProbeResult::Healthy => {
            return Ok(WebLaunch {
                url,
                started: false,
            });
        }
        ProbeResult::Unhealthy => {
            for _ in 0..20 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if matches!(probe(&base_url).await?, ProbeResult::Missing) {
                    break;
                }
            }
            if !matches!(probe(&base_url).await?, ProbeResult::Missing) {
                bail!("旧 Web 服务仍在退出，请稍后重试 /web");
            }
        }
        ProbeResult::Missing => {}
    }

    spawn_web_process(&workspace, address)?;
    let mut last_error = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match probe(&base_url).await {
            Ok(ProbeResult::Healthy) => {
                return Ok(WebLaunch { url, started: true });
            }
            Ok(ProbeResult::Missing | ProbeResult::Unhealthy) => {}
            Err(error) => last_error = Some(error),
        }
    }
    if let Some(error) = last_error {
        return Err(error).context("Web 服务启动后健康检查失败");
    }
    bail!(
        "Web 服务启动超时；请查看 {}",
        web_log_path(&workspace).display()
    )
}

async fn probe(url: &str) -> Result<ProbeResult> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(600))
        .build()
        .context("创建 Web 健康检查客户端失败")?;
    let mut request = client.get(format!("{url}/health"));
    if let Some(token) = api_token() {
        request = request.bearer_auth(token);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) if error.is_connect() || error.is_timeout() => return Ok(ProbeResult::Missing),
        Err(error) => return Err(error).context("Web 健康检查请求失败"),
    };
    let status = response.status();
    let health: HealthResponse = response.json().await.context("Web 健康检查响应格式无效")?;
    let _ = health.workspace;
    if status.is_success() {
        Ok(ProbeResult::Healthy)
    } else if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        Ok(ProbeResult::Unhealthy)
    } else {
        bail!("{url} 已有服务响应，但健康检查返回 {status}")
    }
}

fn workspace_url(base_url: &str, workspace: &Path) -> Result<String> {
    let mut url = reqwest::Url::parse(base_url).context("Web 地址格式无效")?;
    url.query_pairs_mut()
        .append_pair("workspace", &workspace.to_string_lossy());
    Ok(url.into())
}

fn spawn_web_process(workspace: &Path, address: SocketAddr) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe().context("定位 my-agent 可执行文件失败")?;
    let log_path = web_log_path(workspace);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 Web 日志目录失败: {}", parent.display()))?;
    }
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("打开 Web 日志失败: {}", log_path.display()))?;
    let stderr = stdout.try_clone().context("复制 Web 日志句柄失败")?;
    let mut command = std::process::Command::new(executable);
    command
        .arg("--workspace")
        .arg(workspace)
        .arg("serve")
        .arg("--bind")
        .arg(address.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    #[cfg(unix)]
    command.process_group(0);
    command.spawn().context("启动 Web 服务失败")?;
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut command = std::process::Command::new("xdg-open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    bail!("当前平台不支持自动打开浏览器，请手动访问 {url}");

    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("无法打开默认浏览器，请手动访问 {url}"))?;
    Ok(())
}

fn api_token() -> Option<String> {
    std::env::var("MY_AGENT_API_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn web_log_path(workspace: &Path) -> PathBuf {
    workspace.join(".my-agent/web.log")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_url_has_http_scheme() {
        assert!(configured_url().starts_with("http://"));
    }

    #[test]
    fn workspace_url_carries_the_requested_directory() {
        let url = workspace_url("http://127.0.0.1:8787", Path::new("/workspace/one two"))
            .expect("工作区 URL 应可生成");
        assert_eq!(
            url,
            "http://127.0.0.1:8787/?workspace=%2Fworkspace%2Fone+two"
        );
    }
}
