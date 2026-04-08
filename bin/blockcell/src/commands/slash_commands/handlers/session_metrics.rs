//! # /session_metrics 命令
//!
//! 显示记忆系统监控指标。
//!
//! ## 用法
//!
//! - `/session_metrics` - 显示所有层的指标
//! - `/session_metrics --json` - JSON 格式输出
//! - `/session_metrics --reset` - 重置计数器
//! - `/session_metrics --layer N` - 只显示第 N 层 (N 为 1-7)

use crate::commands::slash_commands::*;

/// /session_metrics 命令 - 显示记忆系统监控指标
pub struct SessionMetricsCommand;

#[async_trait::async_trait]
impl SlashCommand for SessionMetricsCommand {
    fn name(&self) -> &str {
        "session_metrics"
    }

    fn description(&self) -> &str {
        "Show memory system metrics"
    }

    fn accepts_args(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, _ctx: &CommandContext) -> CommandResult {
        // 去除前后空格
        let args = args.trim();

        // 解析参数
        let parts: Vec<&str> = args.split_whitespace().collect();

        match parts.len() {
            0 => {
                // 无参数：显示所有层
                let summary = blockcell_agent::session_metrics::get_metrics_summary();
                let content = blockcell_agent::session_metrics::format_metrics_table(&summary, None);
                CommandResult::Handled(CommandResponse::markdown(content))
            }
            1 => {
                // 1 个参数：只能是 --json 或 --reset
                match parts[0] {
                    "--json" | "-j" => {
                        let summary = blockcell_agent::session_metrics::get_metrics_summary();
                        let content = match serde_json::to_string_pretty(&summary) {
                            Ok(json) => format!("```json\n{}\n```", json),
                            Err(e) => format!("❌ Failed to serialize metrics: {}", e),
                        };
                        CommandResult::Handled(CommandResponse::markdown(content))
                    }
                    "--reset" | "-r" => {
                        blockcell_agent::session_metrics::reset_metrics();
                        CommandResult::Handled(CommandResponse::markdown(
                            "✅ Metrics counters have been reset.".to_string()
                        ))
                    }
                    _ => {
                        CommandResult::Handled(CommandResponse::markdown(
                            format!("❌ 无效参数: `{}`\n\n用法:\n- `/session_metrics` - 显示所有层\n- `/session_metrics --json` - JSON 格式\n- `/session_metrics --reset` - 重置计数器\n- `/session_metrics --layer N` - 显示第 N 层 (1-7)", parts[0])
                        ))
                    }
                }
            }
            2 => {
                // 2 个参数：只能是 --layer N
                if parts[0] != "--layer" && parts[0] != "-l" {
                    return CommandResult::Handled(CommandResponse::markdown(
                        format!("❌ 无效参数组合: `{}` `{}`\n\n用法:\n- `/session_metrics --layer N` - 显示第 N 层 (N 为 1-7)", parts[0], parts[1])
                    ));
                }

                match parts[1].parse::<u8>() {
                    Ok(n) if (1..=7).contains(&n) => {
                        let summary = blockcell_agent::session_metrics::get_metrics_summary();
                        let content = blockcell_agent::session_metrics::format_metrics_table(&summary, Some(n));
                        CommandResult::Handled(CommandResponse::markdown(content))
                    }
                    Ok(n) => {
                        CommandResult::Handled(CommandResponse::markdown(
                            format!("❌ 层号超出范围: `{}`，必须是 1-7", n)
                        ))
                    }
                    Err(_) => {
                        CommandResult::Handled(CommandResponse::markdown(
                            format!("❌ 无效层号: `{}`，必须是 1-7 之间的数字", parts[1])
                        ))
                    }
                }
            }
            _ => {
                // 超过 2 个参数：不支持
                CommandResult::Handled(CommandResponse::markdown(
                    "❌ 参数过多\n\n用法:\n- `/session_metrics` - 显示所有层\n- `/session_metrics --json` - JSON 格式\n- `/session_metrics --reset` - 重置计数器\n- `/session_metrics --layer N` - 显示第 N 层 (1-7)".to_string()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_no_args() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("BlockCell Memory Metrics Summary"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_json_arg() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--json", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("```json"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_reset_arg() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--reset", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("reset"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_layer_arg_valid() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;

        for layer in 1..=7 {
            let result = cmd.execute(&format!("--layer {}", layer), &ctx).await;
            if let CommandResult::Handled(response) = result {
                assert!(response.content.contains("Layer"));
            } else {
                panic!("Expected Handled result for layer {}", layer);
            }
        }
    }

    #[tokio::test]
    async fn test_layer_arg_invalid_number() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--layer 8", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("超出范围"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_layer_arg_invalid_text() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--layer abc", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("无效层号"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_invalid_single_arg() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--invalid", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("无效参数"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_too_many_args() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--json --layer 5", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("参数过多"));
        } else {
            panic!("Expected Handled result");
        }
    }

    #[tokio::test]
    async fn test_invalid_two_args() {
        let ctx = CommandContext::test_context();
        let cmd = SessionMetricsCommand;
        let result = cmd.execute("--json 5", &ctx).await;

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("无效参数组合"));
        } else {
            panic!("Expected Handled result");
        }
    }
}