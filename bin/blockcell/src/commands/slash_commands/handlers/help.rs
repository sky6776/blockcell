//! # /help 命令
//!
//! 显示所有可用斜杠命令。

use crate::commands::slash_commands::*;

/// /help 命令 - 显示所有可用命令
pub struct HelpCommand;

#[async_trait::async_trait]
impl SlashCommand for HelpCommand {
    fn name(&self) -> &str {
        "help"
    }

    fn description(&self) -> &str {
        "Show all available commands"
    }

    async fn execute(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
        let handler = crate::commands::slash_commands::registry::SLASH_COMMAND_HANDLER.clone();
        let commands = handler.list_commands();

        let mut content = String::new();
        content.push_str("📋 **Available commands:**\n\n");

        for cmd in commands {
            let name = cmd.name();
            let desc = cmd.description();

            // 检查渠道限制
            let channel_note = if let Some(channels) = cmd.available_channels() {
                if !channels.iter().any(|c| *c == ctx.source.channel) {
                    format!(" (仅 {} 可用)", channels.join(", "))
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            content.push_str(&format!("- `/{}{}` — {}\n", name, channel_note, desc));
        }

        content.push_str("\n💡 **提示:**\n");
        content.push_str("- 大部分命令零 Token 消耗，本地直接执行\n");
        content.push_str("- `/learn` 命令会调用 LLM，消耗 Token\n");

        CommandResult::Handled(CommandResponse::markdown(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_help_command() {
        let cmd = HelpCommand;
        let ctx = CommandContext::test_context();

        let result = cmd.execute("", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("/help"));
            assert!(response.content.contains("/tasks"));
        }
    }
}