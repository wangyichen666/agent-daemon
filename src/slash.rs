use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::SessionInfo;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlashAction {
    Help,
    Status,
    Sessions,
    Resume,
    New,
    Cancel,
    Exit,
    Ping,
    Skill,
    Cron,
    Mcp,
}

#[derive(Clone, Copy)]
enum ArgSpec {
    None,
    OptionalOne(&'static str),
    AtLeastOne(&'static str),
}

#[derive(Clone, Copy)]
pub struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub help: &'static str,
    usage: &'static str,
    args: ArgSpec,
    action: SlashAction,
}

const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "help",
        aliases: &[],
        help: "查看命令",
        usage: "/help",
        args: ArgSpec::None,
        action: SlashAction::Help,
    },
    SlashCommand {
        name: "status",
        aliases: &[],
        help: "查看当前会话状态",
        usage: "/status",
        args: ArgSpec::None,
        action: SlashAction::Status,
    },
    SlashCommand {
        name: "sessions",
        aliases: &[],
        help: "列出会话文件",
        usage: "/sessions",
        args: ArgSpec::None,
        action: SlashAction::Sessions,
    },
    SlashCommand {
        name: "resume",
        aliases: &[],
        help: "列出或恢复历史会话",
        usage: "/resume [编号|ID]",
        args: ArgSpec::OptionalOne("编号或 session ID"),
        action: SlashAction::Resume,
    },
    SlashCommand {
        name: "new",
        aliases: &[],
        help: "新建空白会话",
        usage: "/new",
        args: ArgSpec::None,
        action: SlashAction::New,
    },
    SlashCommand {
        name: "cancel",
        aliases: &[],
        help: "提示如何取消前台请求",
        usage: "/cancel",
        args: ArgSpec::None,
        action: SlashAction::Cancel,
    },
    SlashCommand {
        name: "skill",
        aliases: &[],
        help: "管理本地版本化 skill（list/install/update/remove）",
        usage: "/skill <子命令> [...]",
        args: ArgSpec::AtLeastOne("子命令"),
        action: SlashAction::Skill,
    },
    SlashCommand {
        name: "cron",
        aliases: &[],
        help: "管理定时任务（list/add/enable/disable/run-now/remove）",
        usage: "/cron <子命令> [...]",
        args: ArgSpec::AtLeastOne("子命令"),
        action: SlashAction::Cron,
    },
    SlashCommand {
        name: "mcp",
        aliases: &[],
        help: "查看或重载 MCP server（list/status/reload）",
        usage: "/mcp <list|status|reload>",
        args: ArgSpec::AtLeastOne("子命令"),
        action: SlashAction::Mcp,
    },
    SlashCommand {
        name: "ping",
        aliases: &[],
        help: "检查命令注册表与 daemon 连通性",
        usage: "/ping",
        args: ArgSpec::None,
        action: SlashAction::Ping,
    },
    SlashCommand {
        name: "exit",
        aliases: &["quit"],
        help: "断开并退出当前交互入口",
        usage: "/exit",
        args: ArgSpec::None,
        action: SlashAction::Exit,
    },
];

pub struct SlashRegistry;

impl SlashRegistry {
    pub const fn builtin() -> Self {
        Self
    }

    #[cfg(test)]
    pub fn commands(&self) -> &'static [SlashCommand] {
        COMMANDS
    }

    pub fn parse(&self, input: &str) -> SlashParse {
        let input = input.trim();
        let Some(rest) = input.strip_prefix('/') else {
            return SlashParse::NotCommand;
        };
        let mut parts = rest.split_whitespace();
        let name = parts.next().unwrap_or_default().to_ascii_lowercase();
        let args = parts.map(str::to_owned).collect::<Vec<_>>();
        let Some(command) = COMMANDS.iter().find(|command| {
            command.name == name || command.aliases.iter().any(|alias| *alias == name)
        }) else {
            return SlashParse::Error(format!("未知命令：/{name}。输入 /help 查看命令。"));
        };
        let valid = match command.args {
            ArgSpec::None => args.is_empty(),
            ArgSpec::OptionalOne(_) => args.len() <= 1,
            ArgSpec::AtLeastOne(_) => !args.is_empty(),
        };
        if !valid {
            let detail = match command.args {
                ArgSpec::None => "不接受参数".to_owned(),
                ArgSpec::OptionalOne(label) => format!("最多接受一个参数：{label}"),
                ArgSpec::AtLeastOne(label) => format!("至少需要一个参数：{label}"),
            };
            return SlashParse::Error(format!(
                "命令 /{} 参数错误：{detail}。用法：{}",
                command.name, command.usage
            ));
        }
        SlashParse::Command(SlashInvocation {
            action: command.action,
            args,
        })
    }

    pub fn help(&self) -> String {
        let width = COMMANDS
            .iter()
            .map(|command| command.usage.chars().count())
            .max()
            .unwrap_or(0)
            + 2;
        COMMANDS
            .iter()
            .map(|command| {
                let aliases = if command.aliases.is_empty() {
                    String::new()
                } else {
                    format!("（别名：/{}）", command.aliases.join("、/"))
                };
                format!(
                    "{:<width$}{}{}",
                    command.usage,
                    command.help,
                    aliases,
                    width = width
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlashInvocation {
    pub action: SlashAction,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashParse {
    NotCommand,
    Command(SlashInvocation),
    Error(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SlashResponse {
    Text {
        content: String,
    },
    Exit,
    Sessions {
        sessions: Vec<SessionInfo>,
        select: bool,
    },
    SessionChanged {
        message: String,
        snapshot: Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_alias_and_validates_arguments() {
        let registry = SlashRegistry::builtin();
        assert_eq!(
            registry.parse("/quit"),
            SlashParse::Command(SlashInvocation {
                action: SlashAction::Exit,
                args: Vec::new(),
            })
        );
        assert!(matches!(
            registry.parse("/status extra"),
            SlashParse::Error(_)
        ));
        assert!(matches!(registry.parse("hello"), SlashParse::NotCommand));
    }

    #[test]
    fn help_is_generated_from_the_same_registry() {
        let registry = SlashRegistry::builtin();
        let help = registry.help();
        for command in registry.commands() {
            assert!(help.contains(command.usage));
        }
        assert!(help.contains("/ping"));
    }
}
