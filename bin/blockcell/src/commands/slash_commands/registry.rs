//! # 命令注册
//!
//! 创建默认命令处理器并提供全局访问。

use super::*;
use std::sync::Arc;

use crate::commands::slash_commands::handlers::{
    ClearCommand, ClearSkillsCommand, ExitCommand, ForgetSkillCommand, HelpCommand,
    LearnCommand, QuitCommand, SessionMetricsCommand, SkillsCommand, TasksCommand, ToolsCommand,
};

/// 创建默认命令处理器
///
/// 注册所有内置斜杠命令。
pub fn create_default_handler() -> SlashCommandHandler {
    let mut handler = SlashCommandHandler::new();

    // Phase 1 (P0): 核心命令
    handler.register(HelpCommand);
    handler.register(TasksCommand);

    // Phase 2 (P1): 迁移的命令
    handler.register(SkillsCommand);
    handler.register(ToolsCommand);
    handler.register(LearnCommand);
    handler.register(ClearCommand);
    handler.register(ClearSkillsCommand);
    handler.register(ForgetSkillCommand);

    // 监控命令
    handler.register(SessionMetricsCommand);

    // 渠道限制命令
    handler.register(QuitCommand);
    handler.register(ExitCommand);

    handler
}

/// 全局命令处理器实例
///
/// 使用 `once_cell::sync::Lazy` 实现延迟初始化。
/// Rust 1.70+ 可使用 `std::sync::OnceLock` 替代。
pub static SLASH_COMMAND_HANDLER: once_cell::sync::Lazy<Arc<SlashCommandHandler>> =
    once_cell::sync::Lazy::new(|| Arc::new(create_default_handler()));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_default_handler() {
        let handler = create_default_handler();
        let commands = handler.list_commands();
        assert!(
            commands.len() >= 10,
            "Should have at least 10 commands, got {}",
            commands.len()
        );
    }

    #[test]
    fn test_global_handler_available() {
        let handler = SLASH_COMMAND_HANDLER.clone();
        assert!(!handler.list_commands().is_empty());
    }

    #[tokio::test]
    async fn test_global_handler_help() {
        let ctx = CommandContext::test_context();
        let result = SLASH_COMMAND_HANDLER.try_handle("/help", &ctx).await;

        assert!(matches!(result, CommandResult::Handled(_)));
        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("/tasks"));
            assert!(response.content.contains("/quit"));
            assert!(response.content.contains("/skills"));
            assert!(response.content.contains("/tools"));
        }
    }

    #[tokio::test]
    async fn test_global_handler_skills() {
        let ctx = CommandContext::test_context();
        let result = SLASH_COMMAND_HANDLER.try_handle("/skills", &ctx).await;

        assert!(matches!(result, CommandResult::Handled(_)));
    }

    #[tokio::test]
    async fn test_global_handler_tools() {
        let ctx = CommandContext::test_context();
        let result = SLASH_COMMAND_HANDLER.try_handle("/tools", &ctx).await;

        assert!(matches!(result, CommandResult::Handled(_)));
    }
}