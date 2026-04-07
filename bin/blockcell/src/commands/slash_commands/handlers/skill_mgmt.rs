//! # 技能管理命令
//!
//! /clear-skills 和 /forget-skill 命令。

use crate::commands::slash_commands::*;
use blockcell_skills::evolution::EvolutionRecord;

/// /clear-skills 命令 - 清除所有技能演化记录
pub struct ClearSkillsCommand;

#[async_trait::async_trait]
impl SlashCommand for ClearSkillsCommand {
    fn name(&self) -> &str {
        "clear-skills"
    }

    fn description(&self) -> &str {
        "Clear all skill evolution records"
    }

    async fn execute(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
        let records_dir = ctx.paths.workspace().join("evolution_records");
        let mut count = 0;

        if records_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&records_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json")
                        && std::fs::remove_file(&path).is_ok()
                    {
                        count += 1;
                    }
                }
            }
        }

        let content = if count > 0 {
            format!("✅ **Cleared all skill evolution records** ({} total)\n", count)
        } else {
            "*(No records to clear)*\n".to_string()
        };

        CommandResult::Handled(CommandResponse::markdown(content))
    }
}

/// /forget-skill 命令 - 删除指定技能记录
pub struct ForgetSkillCommand;

#[async_trait::async_trait]
impl SlashCommand for ForgetSkillCommand {
    fn name(&self) -> &str {
        "forget-skill"
    }

    fn description(&self) -> &str {
        "Delete records for a specific skill"
    }

    /// 此命令接受参数
    fn accepts_args(&self) -> bool {
        true
    }

    async fn execute(&self, args: &str, ctx: &CommandContext) -> CommandResult {
        let skill_name = args.trim();

        if skill_name.is_empty() {
            return CommandResult::Handled(CommandResponse::markdown(
                "Usage: `/forget-skill <skill_name>`\n".to_string(),
            ));
        }

        let records_dir = ctx.paths.workspace().join("evolution_records");
        let mut count = 0;

        if records_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&records_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "json") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Ok(record) = serde_json::from_str::<EvolutionRecord>(&content) {
                                if record.skill_name == skill_name
                                    && std::fs::remove_file(&path).is_ok()
                                {
                                    count += 1;
                                }
                            }
                        }
                    }
                }
            }
        }

        let content = if count > 0 {
            format!(
                "✅ **Deleted all records for skill `{}`** ({} total)\n",
                skill_name, count
            )
        } else {
            format!("⚠️ **No records found for skill `{}`**\n", skill_name)
        };

        CommandResult::Handled(CommandResponse::markdown(content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_clear_skills_command() {
        let cmd = ClearSkillsCommand;
        let ctx = CommandContext::test_context();

        let result = cmd.execute("", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));
    }

    #[tokio::test]
    async fn test_forget_skill_command_empty() {
        let cmd = ForgetSkillCommand;
        let ctx = CommandContext::test_context();

        let result = cmd.execute("", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("Usage"));
        }
    }

    #[tokio::test]
    async fn test_forget_skill_command_with_name() {
        let cmd = ForgetSkillCommand;
        let ctx = CommandContext::test_context();

        let result = cmd.execute("test_skill", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));
    }
}