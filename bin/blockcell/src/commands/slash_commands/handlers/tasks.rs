//! # /tasks 命令
//!
//! 列出后台任务状态。

use crate::commands::slash_commands::*;

/// /tasks 命令 - 列出后台任务状态
pub struct TasksCommand;

#[async_trait::async_trait]
impl SlashCommand for TasksCommand {
    fn name(&self) -> &str {
        "tasks"
    }

    fn description(&self) -> &str {
        "List background tasks status"
    }

    async fn execute(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
        let task_manager = match &ctx.task_manager {
            Some(tm) => tm,
            None => {
                return CommandResult::Handled(CommandResponse::markdown(
                    "⚠️ **Task manager not available**\n".to_string(),
                ));
            }
        };

        // 获取任务摘要
        let (queued, running, completed, failed) = task_manager.summary().await;
        let task_list = task_manager.list_tasks(None).await;

        let mut content = String::new();
        content.push_str("📋 **Task overview:**\n\n");
        content.push_str(&format!(
            "- {} queued | {} running | {} completed | {} failed\n",
            queued, running, completed, failed
        ));

        if task_list.is_empty() {
            content.push_str("\n*(No tasks)*\n");
        } else {
            content.push_str("\n**Tasks:**\n");
            for t in &task_list {
                let status_icon = match t.status.to_string().as_str() {
                    "queued" => "⏳",
                    "running" => "🔄",
                    "completed" => "✅",
                    "failed" => "❌",
                    _ => "•",
                };
                let short_id: String = t.id.chars().take(12).collect();
                content.push_str(&format!(
                    "- {} `[{}]` {} - {}\n",
                    status_icon, short_id, t.status, t.label
                ));

                if let Some(ref progress) = t.progress {
                    content.push_str(&format!("  - Progress: {}\n", progress));
                }
                if let Some(ref result) = t.result {
                    let preview = if result.chars().count() > 100 {
                        let truncated: String = result.chars().take(100).collect();
                        format!("{}...", truncated)
                    } else {
                        result.clone()
                    };
                    content.push_str(&format!("  - Result: {}\n", preview));
                }
                if let Some(ref err) = t.error {
                    content.push_str(&format!("  - Error: {}\n", err));
                }
            }
        }

        CommandResult::Handled(CommandResponse::markdown(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tasks_command_no_manager() {
        let cmd = TasksCommand;
        let ctx = CommandContext::test_context(); // No task manager

        let result = cmd.execute("", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("not available"));
        }
    }
}