//! # /log 命令
//!
//! 动态控制日志系统。

use crate::commands::slash_commands::*;
use blockcell_core::logging::{LOG_CONTROLLER, clear_all_logs};
use blockcell_core::{Paths, Config};

/// /log 命令 - 控制日志系统
pub struct LogCommand;

/// 同步日志配置到配置文件
fn sync_config_to_file(level: Option<&str>, console_enabled: Option<bool>, file_enabled: Option<bool>) -> Result<(), String> {
    let paths = Paths::default();
    let config_path = paths.config_file();

    // 加载现有配置
    let mut config = Config::load(&config_path)
        .map_err(|e| format!("Failed to load config: {}", e))?;

    // 更新日志配置
    if let Some(level) = level {
        config.log.level = level.to_string();
    }
    if let Some(console) = console_enabled {
        config.log.console_enabled = console;
    }
    if let Some(file) = file_enabled {
        config.log.file_enabled = file;
    }

    // 保存配置
    config.save(&config_path)
        .map_err(|e| format!("Failed to save config: {}", e))?;

    Ok(())
}

#[async_trait::async_trait]
impl SlashCommand for LogCommand {
    fn name(&self) -> &str {
        "log"
    }

    fn description(&self) -> &str {
        "Control logging system (level, console, file)"
    }

    fn accepts_args(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, _ctx: &CommandContext) -> CommandResult {
        let controller = match LOG_CONTROLLER.get() {
            Some(c) => c,
            None => return CommandResult::Error("Log system not initialized".to_string()),
        };

        match parse_log_command(args.trim()) {
            LogAction::Help => show_help(),
            LogAction::Status => show_status(controller),
            LogAction::SetLevel(level) => {
                match controller.set_level(&level) {
                    Ok(_) => {
                        // 同步配置到文件
                        if let Err(e) = sync_config_to_file(Some(&level), None, None) {
                            tracing::warn!("Failed to sync log config: {}", e);
                        }
                        CommandResult::Handled(CommandResponse::markdown(
                            format!("✓ Log level set to {} (config saved)", level)
                        ))
                    },
                    Err(e) => CommandResult::Error(e),
                }
            }
            LogAction::SetFilter(filter) => {
                match controller.set_filter(&filter) {
                    Ok(_) => CommandResult::Handled(CommandResponse::markdown(
                        format!("✓ Filter set: {}", filter)
                    )),
                    Err(e) => CommandResult::Error(e),
                }
            }
            LogAction::Console(on) => {
                controller.set_console(on);
                // 同步配置到文件
                if let Err(e) = sync_config_to_file(None, Some(on), None) {
                    tracing::warn!("Failed to sync log config: {}", e);
                }
                CommandResult::Handled(CommandResponse::markdown(
                    format!("✓ Console output: {} (config saved)", if on { "ON" } else { "OFF" })
                ))
            }
            LogAction::File(on) => {
                controller.set_file(on);
                // 同步配置到文件
                if let Err(e) = sync_config_to_file(None, None, Some(on)) {
                    tracing::warn!("Failed to sync log config: {}", e);
                }
                CommandResult::Handled(CommandResponse::markdown(
                    format!("✓ File output: {} (config saved)", if on { "ON" } else { "OFF" })
                ))
            }
            LogAction::Clear => {
                let paths = Paths::default();
                let (count, size) = clear_all_logs(&paths.logs_dir());
                let size_mb = size as f64 / 1024.0 / 1024.0;
                CommandResult::Handled(CommandResponse::markdown(
                    format!("✓ Cleared {} log files ({:.2} MB)", count, size_mb)
                ))
            }
            LogAction::Unknown => CommandResult::Handled(CommandResponse::markdown(
                "Unknown /log command. Use /log help for usage.".to_string()
            )),
        }
    }
}

/// 解析命令
fn parse_log_command(input: &str) -> LogAction {
    let input = input.trim();

    if input.is_empty() || input == "help" {
        return LogAction::Help;
    }

    if input == "status" {
        return LogAction::Status;
    }

    if input == "clear" {
        return LogAction::Clear;
    }

    match input {
        "trace" | "debug" | "info" | "warn" | "error" | "off" => {
            return LogAction::SetLevel(input.to_string());
        }
        _ => {}
    }

    if input == "console on" {
        return LogAction::Console(true);
    }
    if input == "console off" {
        return LogAction::Console(false);
    }

    if input == "file on" {
        return LogAction::File(true);
    }
    if input == "file off" {
        return LogAction::File(false);
    }

    if input.contains('=') {
        return LogAction::SetFilter(input.to_string());
    }

    LogAction::Unknown
}

enum LogAction {
    Help,
    Status,
    SetLevel(String),
    SetFilter(String),
    Console(bool),
    File(bool),
    Clear,
    Unknown,
}

/// 显示帮助
fn show_help() -> CommandResult {
    CommandResult::Handled(CommandResponse::markdown(r#"## /log 命令帮助

### 基本用法
- `/log status` - 显示当前日志配置（包括文件统计）
- `/log help` - 显示此帮助信息
- `/log clear` - 清理所有日志文件

### 日志等级
| 等级 | 说明 | 适用场景 |
|------|------|---------|
| `trace` | 最详细追踪信息 | 深度调试、跟踪执行流程 |
| `debug` | 详细调试信息 | 开发调试、问题排查 |
| `info`  | 一般信息 | 正常运行监控 |
| `warn`  | 警告信息 | 潜在问题提醒 |
| `error` | 错误信息 | 错误和异常 |
| `off`   | 关闭日志 | 完全静默模式 |

等级从低到高：trace < debug < info < warn < error

### 模块过滤
设置特定模块的日志等级：
- `/log blockcell_agent=trace` - Agent 模块详细日志
- `/log blockcell_tools=debug` - Tools 模块调试日志
- `/log blockcell_channels::telegram=trace` - Telegram 频道详细日志

常用模块名：
- `blockcell_agent` - Agent 运行时、任务管理、意图解析
- `blockcell_tools` - 工具执行、50+ 内置工具
- `blockcell_channels` - 消息渠道（Telegram/Slack/Discord/飞书等）
- `blockcell_channels::telegram` - Telegram 频道
- `blockcell_channels::napcat` - NapCat QQ 频道
- `blockcell_providers` - LLM Provider 调用（OpenAI/DeepSeek/Anthropic）
- `blockcell_skills` - 技能引擎、Rhai 脚本、自我进化
- `blockcell_scheduler` - Cron 任务、心跳、后台作业
- `blockcell_storage` - SQLite 存储、会话、记忆
- `blockcell_updater` - 自动更新机制
- `blockcell_core` - 核心类型、消息、配置

### 控制台输出（独立控制，不影响文件）
- `/log console on` - 开启控制台输出（默认）
- `/log console off` - 关闭控制台输出

### 文件输出（独立控制，不影响控制台）
- `/log file on` - 开启文件输出（默认）
- `/log file off` - 关闭文件输出

### 示例
```text
# 设置全局 DEBUG 等级
/log debug

# 只看 Agent 模块详细日志
/log blockcell_agent=trace

# 关闭控制台输出（仅文件）
/log console off

# 关闭文件输出（仅控制台）
/log file off

# 清理所有日志文件
/log clear
```
"#.to_string()))
}

/// 显示状态（包括日志文件统计）
fn show_status(controller: &blockcell_core::logging::LogController) -> CommandResult {
    let status = controller.status();

    // 统计日志文件数量和大小
    let paths = Paths::default();
    let logs_dir = paths.logs_dir();

    let mut file_count = 0;
    let mut total_size = 0u64;

    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // 匹配 agent.log 或 agent.log.YYYY-MM-DD 格式
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_log_file = file_name == "agent.log"
                || file_name.starts_with("agent.log.");

            if path.is_file() && is_log_file {
                if let Ok(metadata) = entry.metadata() {
                    total_size += metadata.len();
                    file_count += 1;
                }
            }
        }
    }

    let size_mb = total_size as f64 / 1024.0 / 1024.0;

    let content = format!(
        "📊 **Log Status:**\n\n\
        - Level: `{}`\n\
        - Console: `{}`\n\
        - File: `{}`\n\
        - Log file: `{}`\n\
        - Module filters: {}\n\
        - Files: {} ({:.2} MB)\n\
        - Auto-delete: 3 days",
        status.level,
        if status.console_enabled { "ON" } else { "OFF" },
        if status.file_enabled { "ON" } else { "OFF" },
        status.log_file,
        if status.module_filters.is_empty() {
            "(none)".to_string()
        } else {
            status.module_filters.iter()
                .map(|f| format!("`{}`", f))
                .collect::<Vec<_>>()
                .join(", ")
        },
        file_count,
        size_mb
    );

    CommandResult::Handled(CommandResponse::markdown(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_log_command_help() {
        assert!(matches!(parse_log_command("help"), LogAction::Help));
        assert!(matches!(parse_log_command(""), LogAction::Help));
    }

    #[test]
    fn test_parse_log_command_levels() {
        assert!(matches!(parse_log_command("debug"), LogAction::SetLevel(l) if l == "debug"));
        assert!(matches!(parse_log_command("trace"), LogAction::SetLevel(l) if l == "trace"));
        assert!(matches!(parse_log_command("off"), LogAction::SetLevel(l) if l == "off"));
    }

    #[test]
    fn test_parse_log_command_console() {
        assert!(matches!(parse_log_command("console on"), LogAction::Console(true)));
        assert!(matches!(parse_log_command("console off"), LogAction::Console(false)));
    }

    #[test]
    fn test_parse_log_command_file() {
        assert!(matches!(parse_log_command("file on"), LogAction::File(true)));
        assert!(matches!(parse_log_command("file off"), LogAction::File(false)));
    }

    #[test]
    fn test_parse_log_command_filter() {
        assert!(matches!(parse_log_command("blockcell_agent=trace"), LogAction::SetFilter(_)));
    }
}