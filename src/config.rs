use std::env;

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigIssue {
    pub variable: &'static str,
    pub message: String,
}

pub fn check_environment() -> Vec<ConfigIssue> {
    let mut issues = Vec::new();
    require_non_empty("OPENAI_API_KEY", &mut issues);
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
        Err(env::VarError::NotPresent) => issues.push(ConfigIssue {
            variable: "OPENAI_BASE_URL",
            message: "未设置".to_owned(),
        }),
        Err(error) => issues.push(ConfigIssue {
            variable: "OPENAI_BASE_URL",
            message: format!("无法读取：{error}"),
        }),
    }
    require_non_empty("MODEL_NAME", &mut issues);

    validate_usize("CONTEXT_TOKEN_BUDGET", 256, usize::MAX, &mut issues);
    validate_usize("CONTEXT_RECENT_MESSAGES", 1, usize::MAX, &mut issues);
    validate_usize("CONTEXT_MILD_PERCENT", 1, 99, &mut issues);
    validate_usize("CONTEXT_STRONG_PERCENT", 2, 100, &mut issues);
    let mild = optional_parsed_usize("CONTEXT_MILD_PERCENT").unwrap_or(60);
    let strong = optional_parsed_usize("CONTEXT_STRONG_PERCENT").unwrap_or(85);
    if mild >= strong {
        issues.push(ConfigIssue {
            variable: "CONTEXT_MILD_PERCENT / CONTEXT_STRONG_PERCENT",
            message: "必须满足温和阈值小于强力阈值".to_owned(),
        });
    }
    if let Ok(value) = env::var("MULTIMODAL_ENABLED")
        && !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on" | "0" | "false" | "no" | "off"
        )
    {
        issues.push(ConfigIssue {
            variable: "MULTIMODAL_ENABLED",
            message: "必须是 true/false、1/0、yes/no 或 on/off".to_owned(),
        });
    }
    issues
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
        "配置校验失败：{details}\n首次使用示例：\n  export OPENAI_API_KEY='你的密钥'\n  export OPENAI_BASE_URL='https://api.deepseek.com'\n  export MODEL_NAME='deepseek-chat'\n然后运行：my-agent config check"
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
