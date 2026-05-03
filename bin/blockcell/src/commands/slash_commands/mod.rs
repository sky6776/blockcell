//! # Slash Commands Unified Module
//!
//! 在 Gateway 模式及所有 Channel 中支持斜杠命令的统一处理模块。
//!
//! ## 概述
//!
//! 该模块提供统一的斜杠命令处理机制，使所有渠道的用户都能：
//! - 快速查询系统状态
//! - 执行常用操作
//! - 大部分命令零 Token 消耗（不经过 LLM）
//!
//! ## 架构
//!
//! ```text
//! Channel 消息到达 (Telegram/Slack/...)
//!     │
//!     ▼
//! ┌─────────────────────┐
//! │  allowFrom 检查     │  ← Channel 层：白名单验证（现有机制）
//! └─────────┬───────────┘
//!           │
//!     ┌─────┴─────┐
//!     │           │
//!   拒绝        通过
//!     │           │
//!     ▼           ▼
//!   忽略    发送 InboundMessage
//!               │
//!               ▼
//!         ┌─────────────────────┐
//!         │ Gateway Interceptor │  ← Gateway 层：统一拦截
//!         │ (slash_commands)    │
//!         └─────────┬───────────┘
//!                   │
//!             ┌─────┴─────┐
//!             │           │
//!         是斜杠命令   非斜杠命令
//!             │           │
//!             ▼           ▼
//!         本地执行    AgentRuntime
//!             │
//!             ▼
//!         返回结果到原渠道
//! ```

mod context;
pub mod handlers;
mod registry;

pub use context::*;
pub use registry::*;

use async_trait::async_trait;

/// 斜杠命令处理器 trait
///
/// 所有斜杠命令都需要实现此 trait。
///
/// # Example
///
/// ```rust
/// pub struct HelpCommand;
///
/// #[async_trait]
/// impl SlashCommand for HelpCommand {
///     fn name(&self) -> &str { "help" }
///     fn description(&self) -> &str { "Show available commands" }
///
///     async fn execute(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
///         CommandResult::Handled(CommandResponse {
///             content: "Available commands: ...".to_string(),
///             is_markdown: false,
///         })
///     }
/// }
/// ```
#[async_trait]
pub trait SlashCommand: Send + Sync {
    /// 命令名称 (不含斜杠)
    fn name(&self) -> &str;

    /// 命令描述 (用于 /help 显示)
    fn description(&self) -> &str;

    /// 是否需要权限验证
    fn requires_permission(&self) -> bool {
        false
    }

    /// 支持的渠道列表 (None 表示所有渠道)
    ///
    /// 例如：`Some(vec!["cli"])` 表示仅在 CLI 模式可用
    fn available_channels(&self) -> Option<Vec<&'static str>> {
        None
    }

    /// 是否接受参数
    ///
    /// - `true`: 命令接受参数，如 `/learn 技能描述`
    /// - `false`: 命令不接受参数，如 `/help`、`/tasks`。如果用户输入了额外内容，命令不会触发
    ///
    /// 默认为 `false`（不接受参数）
    fn accepts_args(&self) -> bool {
        false
    }

    /// 命令执行超时时间（秒），默认 10 秒
    ///
    /// 注意：`/learn` 命令会调用 LLM，需要更长超时
    fn timeout_secs(&self) -> u64 {
        10
    }

    /// 执行命令
    ///
    /// # Arguments
    ///
    /// * `args` - 命令参数（命令名称后面的部分）
    /// * `ctx` - 命令执行上下文
    ///
    /// # Returns
    ///
    /// 返回命令处理结果
    async fn execute(&self, args: &str, ctx: &CommandContext) -> CommandResult;
}

/// 统一命令处理器
///
/// 管理所有注册的斜杠命令，提供统一的处理入口。
pub struct SlashCommandHandler {
    commands: Vec<Box<dyn SlashCommand>>,
}

impl SlashCommandHandler {
    /// 创建新的处理器
    pub fn new() -> Self {
        Self {
            commands: Vec::new(),
        }
    }

    /// 注册命令
    pub fn register<C: SlashCommand + 'static>(&mut self, command: C) {
        self.commands.push(Box::new(command));
    }

    /// 尝试处理输入
    ///
    /// # Arguments
    ///
    /// * `input` - 用户输入的文本
    /// * `ctx` - 命令执行上下文
    ///
    /// # Returns
    ///
    /// - `CommandResult::Handled` - 命令已处理，返回响应
    /// - `CommandResult::NotACommand` - 非斜杠命令，交给下游处理
    /// - `CommandResult::PermissionDenied` - 命令需要权限，拒绝执行
    /// - `CommandResult::Error` - 命令执行错误
    /// - `CommandResult::ExitRequested` - 请求退出交互模式 (仅 /quit 和 /exit)
    pub async fn try_handle(&self, input: &str, ctx: &CommandContext) -> CommandResult {
        let input = input.trim();

        // 检查是否为斜杠命令：必须以 '/' 开头
        if !input.starts_with('/') {
            return CommandResult::NotACommand;
        }

        // 解析命令和参数
        let (cmd_name, args) = if let Some(space_pos) = input.find(' ') {
            (&input[1..space_pos], &input[space_pos + 1..])
        } else {
            (&input[1..], "")
        };

        // 验证命令名称格式：
        // 1. 不能为空（处理 "/ help" 这种情况）
        // 2. 只能包含字母、数字、连字符和下划线
        // 3. 必须以字母开头
        if cmd_name.is_empty()
            || !cmd_name
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            || !cmd_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return CommandResult::NotACommand;
        }

        // 查找命令处理器
        for command in &self.commands {
            if command.name() == cmd_name {
                // 渠道限制检查
                if let Some(channels) = command.available_channels() {
                    if !channels.iter().any(|c| *c == ctx.source.channel) {
                        return CommandResult::Handled(CommandResponse {
                            content: format!(
                                "命令 /{} 仅在 {} 模式可用",
                                cmd_name,
                                channels.join(", ")
                            ),
                            is_markdown: false,
                        });
                    }
                }

                // 参数检查：如果命令不接受参数但用户提供了参数，不触发命令
                if !command.accepts_args() && !args.is_empty() {
                    return CommandResult::NotACommand;
                }

                // 权限检查
                if command.requires_permission() {
                    // 预留的权限验证扩展点
                    // 当前所有命令 requires_permission() 返回 false，此分支不会触发
                    // 未来实现时需：
                    // 1. 在 CommandSource 中填充 sender_id（用户身份）
                    // 2. 在 Config 中定义权限规则（管理员列表/角色映射）
                    // 3. 实现权限检查逻辑（检查用户是否有权执行此命令）
                    // 示例：
                    // if !ctx.source.sender_id.map(|id| is_admin(&id)).unwrap_or(false) {
                    //     return CommandResult::PermissionDenied;
                    // }
                }

                // 带超时执行命令
                let timeout_duration = std::time::Duration::from_secs(command.timeout_secs());
                return match tokio::time::timeout(timeout_duration, command.execute(args, ctx))
                    .await
                {
                    Ok(result) => result,
                    Err(_) => CommandResult::Error(format!(
                        "命令 /{} 执行超时 ({}秒)",
                        cmd_name,
                        command.timeout_secs()
                    )),
                };
            }
        }

        // 未知命令
        CommandResult::Handled(CommandResponse {
            content: format!("未知命令: /{}。输入 /help 查看可用命令。", cmd_name),
            is_markdown: false,
        })
    }

    /// 获取所有注册的命令（用于 /help 显示）
    pub fn list_commands(&self) -> Vec<&dyn SlashCommand> {
        self.commands.iter().map(|c| c.as_ref()).collect()
    }
}

impl Default for SlashCommandHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_non_command_passthrough() {
        let handler = SlashCommandHandler::new();
        let ctx = CommandContext::test_context();

        let result = handler.try_handle("hello world", &ctx).await;
        assert!(matches!(result, CommandResult::NotACommand));
    }

    #[tokio::test]
    async fn test_unknown_command() {
        let handler = SlashCommandHandler::new();
        let ctx = CommandContext::test_context();

        let result = handler.try_handle("/unknowncommand", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("未知命令"));
        }
    }

    #[tokio::test]
    async fn test_command_with_args() {
        let handler = SlashCommandHandler::new();
        let ctx = CommandContext::test_context();

        let result = handler.try_handle("/unknown test args", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));
    }
}
