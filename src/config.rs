use std::env;
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::provider::{ApiType, ProviderProfile};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
    pub variable: &'static str,
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ConfigFile {
    #[serde(default)]
    pub active_profile: Option<String>,
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
    pub api_type: ApiType,
    pub base_url: String,
    pub model: String,
    pub has_api_key: bool,
}

impl ProfileSummary {
    pub fn from_profile(profile: &ProviderProfile) -> Self {
        Self {
            id: profile.id.clone(),
            name: if profile.name.trim().is_empty() {
                profile.id.clone()
            } else {
                profile.name.clone()
            },
            api_type: profile.api_type,
            base_url: profile.base_url.clone(),
            model: profile.model.clone(),
            has_api_key: profile
                .api_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
}

impl Default for ConfigStore {
    fn default() -> Self {
        Self::new(default_config_path())
    }
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<ConfigFile> {
        if !self.path.exists() {
            return Ok(ConfigFile::default());
        }
        let bytes = fs::read(&self.path).map_err(|error| {
            anyhow::anyhow!("读取配置文件失败 {}：{error}", self.path.display())
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| anyhow::anyhow!("解析配置文件失败 {}：{error}", self.path.display()))
    }

    pub fn save(&self, config: &ConfigFile) -> Result<()> {
        for profile in &config.profiles {
            profile.validate()?;
        }
        if let Some(active) = &config.active_profile
            && !config.profiles.iter().any(|profile| &profile.id == active)
        {
            bail!("活动模型配置不存在：{active}");
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                anyhow::anyhow!("创建配置目录失败 {}：{error}", parent.display())
            })?;
        }
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        let data = serde_json::to_vec_pretty(config)
            .map_err(|error| anyhow::anyhow!("序列化配置失败：{error}"))?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| {
                anyhow::anyhow!("写入临时配置失败 {}：{error}", temporary.display())
            })?;
        use std::io::Write;
        file.write_all(&data)
            .map_err(|error| anyhow::anyhow!("写入配置失败 {}：{error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| anyhow::anyhow!("同步配置失败 {}：{error}", temporary.display()))?;
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(
                |error| anyhow::anyhow!("设置配置权限失败 {}：{error}", temporary.display()),
            )?;
        }
        fs::rename(&temporary, &self.path)
            .map_err(|error| anyhow::anyhow!("提交配置失败 {}：{error}", self.path.display()))
    }

    pub fn active_profile(&self) -> Result<Option<ProviderProfile>> {
        let config = self.load()?;
        Ok(config
            .active_profile
            .as_deref()
            .and_then(|id| config.profiles.iter().find(|profile| profile.id == id))
            .cloned()
            .or_else(|| config.profiles.first().cloned()))
    }

    pub fn upsert(&self, mut profile: ProviderProfile, activate: bool) -> Result<ConfigFile> {
        profile.id = normalize_profile_id(&profile.id, &profile.name, &profile.model);
        profile.name = if profile.name.trim().is_empty() {
            profile.id.clone()
        } else {
            profile.name.trim().to_owned()
        };
        profile.base_url = profile.base_url.trim_end_matches('/').to_owned();
        let profile_id = profile.id.clone();
        let mut config = self.load()?;
        if let Some(existing) = config
            .profiles
            .iter_mut()
            .find(|item| item.id == profile.id)
            && profile.api_key.is_none()
        {
            profile.api_key = existing.api_key.clone();
        }
        profile.validate()?;
        if let Some(existing) = config
            .profiles
            .iter_mut()
            .find(|item| item.id == profile.id)
        {
            *existing = profile;
        } else {
            config.profiles.push(profile);
        }
        if activate || config.active_profile.is_none() {
            config.active_profile = Some(profile_id);
        }
        self.save(&config)?;
        Ok(config)
    }

    pub fn activate(&self, id: &str) -> Result<ProviderProfile> {
        let mut config = self.load()?;
        let profile = config
            .profiles
            .iter()
            .find(|profile| profile.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("不存在模型配置：{id}"))?;
        config.active_profile = Some(id.to_owned());
        self.save(&config)?;
        Ok(profile)
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = env::var_os("MY_AGENT_CONFIG") {
        return PathBuf::from(path);
    }
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("my-agent/config.json");
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| path.join(".config/my-agent/config.json"))
        .unwrap_or_else(|| PathBuf::from(".my-agent/config.json"))
}

pub fn load_persisted_environment() {
    let store = ConfigStore::default();
    let Ok(Some(profile)) = store.active_profile() else {
        return;
    };
    // main 在启动 Tokio runtime 前调用，避免运行中并发修改进程环境。
    set_if_missing("API_TYPE", profile.api_type.as_str());
    set_if_missing("OPENAI_BASE_URL", &profile.base_url);
    set_if_missing("MODEL_NAME", &profile.model);
    if let Some(api_key) = profile.api_key.as_deref() {
        set_if_missing("OPENAI_API_KEY", api_key);
    }
}

fn set_if_missing(name: &str, value: &str) {
    if env::var_os(name).is_none() {
        // SAFETY: called once during process startup before any worker thread is spawned.
        unsafe { env::set_var(name, value) };
    }
}

fn normalize_profile_id(id: &str, name: &str, model: &str) -> String {
    let source = if id.trim().is_empty() {
        if name.trim().is_empty() { model } else { name }
    } else {
        id
    };
    let normalized = source
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let normalized = normalized.trim_matches('-');
    if normalized.is_empty() {
        "model".to_owned()
    } else {
        normalized.to_owned()
    }
}

pub fn check_environment() -> Vec<ConfigIssue> {
    let mut issues = Vec::new();
    let api_type = match ApiType::from_env() {
        Ok(api_type) => Some(api_type),
        Err(error) => {
            issues.push(ConfigIssue {
                variable: "API_TYPE",
                message: error.to_string(),
            });
            None
        }
    };
    if !matches!(api_type, Some(ApiType::Ollama)) {
        require_non_empty("OPENAI_API_KEY", &mut issues);
    }
    validate_base_url(api_type, &mut issues);
    require_non_empty("MODEL_NAME", &mut issues);

    validate_usize("CONTEXT_TOKEN_BUDGET", 256, usize::MAX, &mut issues);
    validate_usize("CONTEXT_RECENT_MESSAGES", 1, usize::MAX, &mut issues);
    validate_usize("CONTEXT_MILD_PERCENT", 1, 99, &mut issues);
    validate_usize("CONTEXT_STRONG_PERCENT", 2, 100, &mut issues);
    validate_usize("SKILLS_MAX_MATCHES", 1, 64, &mut issues);
    validate_usize("CRON_TICK_SECONDS", 1, usize::MAX, &mut issues);
    validate_usize("CRON_STAGGER_SECONDS", 0, usize::MAX, &mut issues);
    validate_usize("CRON_RUN_TIMEOUT_SECS", 1, usize::MAX, &mut issues);
    validate_usize("HEARTBEAT_INTERVAL_SECS", 1, usize::MAX, &mut issues);
    let mild = optional_parsed_usize("CONTEXT_MILD_PERCENT").unwrap_or(60);
    let strong = optional_parsed_usize("CONTEXT_STRONG_PERCENT").unwrap_or(85);
    if mild >= strong {
        issues.push(ConfigIssue {
            variable: "CONTEXT_MILD_PERCENT / CONTEXT_STRONG_PERCENT",
            message: "必须满足温和阈值小于强力阈值".to_owned(),
        });
    }
    for variable in [
        "MULTIMODAL_ENABLED",
        "OLLAMA_TOOLS_ENABLED",
        "HEARTBEAT_ENABLED",
    ] {
        validate_bool(variable, &mut issues);
    }
    issues
}

fn validate_base_url(api_type: Option<ApiType>, issues: &mut Vec<ConfigIssue>) {
    match env::var("OPENAI_BASE_URL") {
        Ok(value) if value.trim().is_empty() => issues.push(ConfigIssue {
            variable: "OPENAI_BASE_URL",
            message: "不能为空".to_owned(),
        }),
        Ok(value) => match reqwest::Url::parse(&value) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => {}
            Ok(_) => issues.push(ConfigIssue {
                variable: "OPENAI_BASE_URL",
                message: "必须使用 http:// 或 https://".to_owned(),
            }),
            Err(error) => issues.push(ConfigIssue {
                variable: "OPENAI_BASE_URL",
                message: format!("不是合法 URL：{error}"),
            }),
        },
        Err(env::VarError::NotPresent) if matches!(api_type, Some(ApiType::Ollama)) => {}
        Err(env::VarError::NotPresent) => issues.push(ConfigIssue {
            variable: "OPENAI_BASE_URL",
            message: "未设置".to_owned(),
        }),
        Err(error) => issues.push(ConfigIssue {
            variable: "OPENAI_BASE_URL",
            message: format!("无法读取：{error}"),
        }),
    }
}

fn validate_bool(variable: &'static str, issues: &mut Vec<ConfigIssue>) {
    if let Ok(value) = env::var(variable)
        && !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "0" | "false" | "no" | "off"
        )
    {
        issues.push(ConfigIssue {
            variable,
            message: "必须是 true/false、1/0、yes/no 或 on/off".to_owned(),
        });
    }
}

fn optional_parsed_usize(variable: &str) -> Option<usize> {
    env::var(variable).ok()?.parse().ok()
}

pub fn validate_environment() -> Result<()> {
    let issues = check_environment();
    if issues.is_empty() {
        return Ok(());
    }
    let details = issues
        .iter()
        .map(|issue| format!("{}：{}", issue.variable, issue.message))
        .collect::<Vec<String>>()
        .join("；");
    bail!(
        "配置校验失败：{details}\n\n配置来源：环境变量或全局配置文件 {}\n推荐方式：先运行 `myagent serve` 打开 Web 设置，保存一次后所有终端复用；也可以继续使用环境变量。\n\nOpenAI 兼容示例：\n  export API_TYPE='openai-chat'\n  export OPENAI_API_KEY='你的密钥'\n  export OPENAI_BASE_URL='https://api.deepseek.com'\n  export MODEL_NAME='deepseek-chat'\nOllama 示例：\n  export API_TYPE='ollama'\n  export MODEL_NAME='qwen3'\n然后运行：myagent config check",
        default_config_path().display()
    )
}

fn require_non_empty(variable: &'static str, issues: &mut Vec<ConfigIssue>) {
    match env::var(variable) {
        Ok(value) if !value.trim().is_empty() => {}
        Ok(_) => issues.push(ConfigIssue {
            variable,
            message: "不能为空".to_owned(),
        }),
        Err(env::VarError::NotPresent) => issues.push(ConfigIssue {
            variable,
            message: "未设置".to_owned(),
        }),
        Err(error) => issues.push(ConfigIssue {
            variable,
            message: format!("无法读取：{error}"),
        }),
    }
}

fn validate_usize(
    variable: &'static str,
    minimum: usize,
    maximum: usize,
    issues: &mut Vec<ConfigIssue>,
) {
    let Ok(value) = env::var(variable) else {
        return;
    };
    match value.parse::<usize>() {
        Ok(parsed) if (minimum..=maximum).contains(&parsed) => {}
        _ => issues.push(ConfigIssue {
            variable,
            message: format!("必须是 {minimum}..={maximum} 的整数"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, model: &str, key: Option<&str>) -> ProviderProfile {
        ProviderProfile {
            id: id.to_owned(),
            name: id.to_owned(),
            api_type: ApiType::OpenaiChat,
            api_key: key.map(str::to_owned),
            base_url: "https://api.example.com".to_owned(),
            model: model.to_owned(),
        }
    }

    #[test]
    fn config_store_round_trips_and_preserves_omitted_api_key() {
        let path = std::env::temp_dir().join(format!(
            "my-agent-config-test-{}-{}.json",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let store = ConfigStore::new(&path);
        let saved = store
            .upsert(profile("deepseek", "deepseek-chat", Some("secret")), true)
            .unwrap();
        assert_eq!(saved.active_profile.as_deref(), Some("deepseek"));
        let summary = ProfileSummary::from_profile(&saved.profiles[0]);
        assert!(summary.has_api_key);

        let saved = store
            .upsert(profile("deepseek", "deepseek-reasoner", None), true)
            .unwrap();
        assert_eq!(saved.profiles[0].api_key.as_deref(), Some("secret"));
        assert_eq!(
            store.active_profile().unwrap().unwrap().model,
            "deepseek-reasoner"
        );
        let _ = store
            .upsert(profile("other", "other-model", Some("other-secret")), false)
            .unwrap();
        let saved = store
            .upsert(profile("deepseek", "deepseek-v4", None), true)
            .unwrap();
        assert_eq!(saved.active_profile.as_deref(), Some("deepseek"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn profile_validation_requires_key_for_remote_provider() {
        let error = profile("remote", "model", None).validate().unwrap_err();
        assert!(error.to_string().contains("API key"));
        let mut ollama = profile("local", "qwen3", None);
        ollama.api_type = ApiType::Ollama;
        ollama.base_url = "http://127.0.0.1:11434".to_owned();
        assert!(ollama.validate().is_ok());
    }
}
