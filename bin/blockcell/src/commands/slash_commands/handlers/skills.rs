//! # /skills 命令
//!
//! 列出技能和演化状态。

use crate::commands::slash_commands::*;
use blockcell_skills::evolution::EvolutionRecord;
use std::collections::{BTreeMap, HashSet};

/// 技能分类
const SKILL_CATEGORIES: &[(&str, &[&str])] = &[
    (
        "💰 Finance",
        &[
            "stock_monitor",
            "stock_screener",
            "bond_monitor",
            "futures_monitor",
            "futures_strategy",
            "portfolio_advisor",
            "macro_monitor",
            "daily_finance_report",
        ],
    ),
    (
        "⛓️ Blockchain/DeFi",
        &[
            "crypto_research",
            "crypto_onchain",
            "crypto_sentiment",
            "crypto_tax",
            "quant_crypto",
            "defi_analysis",
            "nft_analysis",
            "dao_analysis",
            "token_security",
            "contract_audit",
            "wallet_security",
            "whale_tracker",
            "address_monitor",
            "treasury_management",
        ],
    ),
    (
        "📧 Email",
        &[
            "email_digest",
            "email_auto_reply",
            "email_cleanup",
            "email_backup",
            "email_report",
            "email_to_tasks",
        ],
    ),
    ("🖥️ GUI Automation", &["app_control", "camera"]),
    (
        "📅 Productivity",
        &[
            "daily_digest",
            "weekly_review",
            "calendar_manager",
            "calendar_reminders",
            "personal_life",
            "smart_home",
            "learning_assistant",
        ],
    ),
    (
        "🔧 DevOps",
        &[
            "dev_workflow",
            "dev_security",
            "devops_monitor",
            "log_monitor",
            "site_monitor",
            "security_privacy",
        ],
    ),
    ("📰 Content", &["news_monitor", "content_creator"]),
    ("🏢 Business", &["business_ops"]),
];

/// /skills 命令 - 列出技能和演化状态
pub struct SkillsCommand;

#[async_trait::async_trait]
impl SlashCommand for SkillsCommand {
    fn name(&self) -> &str {
        "skills"
    }

    fn description(&self) -> &str {
        "List skills and evolution status"
    }

    async fn execute(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
        let content = print_skills_status(&ctx.paths);
        CommandResult::Handled(CommandResponse::markdown(content))
    }
}

/// 扫描技能目录
fn scan_skill_dirs(dir: &std::path::Path) -> Vec<(String, String)> {
    let mut skills = Vec::new();
    if !dir.exists() {
        return skills;
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // 尝试读取描述
                    let desc_path = path.join("README.md");
                    let desc = if desc_path.exists() {
                        if let Ok(content) = std::fs::read_to_string(&desc_path) {
                            // 提取第一行作为描述
                            content
                                .lines()
                                .next()
                                .map(|s| s.trim_start_matches('#').trim().to_string())
                                .unwrap_or_default()
                        } else {
                            String::new()
                        }
                    } else {
                        String::new()
                    };
                    skills.push((name.to_string(), desc));
                }
            }
        }
    }
    skills
}

/// 判断是否为内置工具
fn is_builtin_tool(name: &str) -> bool {
    // 简单判断，可以根据需要扩展
    name.starts_with("capability_") || name.starts_with("tool_")
}

/// 格式化时间戳
fn format_timestamp(ts: i64) -> String {
    use chrono::{TimeZone, Utc};
    if let Some(dt) = Utc.timestamp_opt(ts, 0).single() {
        dt.format("%Y-%m-%d %H:%M").to_string()
    } else {
        ts.to_string()
    }
}

/// 打印技能状态
fn print_skills_status(paths: &blockcell_core::Paths) -> String {
    let records_dir = paths.workspace().join("evolution_records");

    // 收集所有技能
    let builtin_skills = scan_skill_dirs(&paths.builtin_skills_dir());
    let workspace_skills = scan_skill_dirs(&paths.skills_dir());

    // 合并：workspace 覆盖 built-in
    let mut skill_map: BTreeMap<String, String> = BTreeMap::new();
    for (name, desc) in &builtin_skills {
        skill_map.insert(name.clone(), desc.clone());
    }
    for (name, desc) in &workspace_skills {
        skill_map.insert(name.clone(), desc.clone());
    }

    let mut content = String::new();
    content.push_str(&format!("🧠 **Skills** ({} total)\n\n", skill_map.len()));

    // 按分类分组
    let mut categorized = HashSet::new();

    for (category, skill_names) in SKILL_CATEGORIES {
        let mut items: Vec<(&str, &str)> = Vec::new();
        for &sn in *skill_names {
            if let Some(desc) = skill_map.get(sn) {
                items.push((sn, desc.as_str()));
                categorized.insert(sn.to_string());
            }
        }
        if !items.is_empty() {
            content.push_str(&format!("**{}** ({})\n", category, items.len()));
            for (name, desc) in &items {
                if desc.is_empty() {
                    content.push_str(&format!("- {}\n", name));
                } else {
                    let char_count = desc.chars().count();
                    if char_count > 40 {
                        let short: String = desc.chars().take(40).collect();
                        content.push_str(&format!("- {} — {}…\n", name, short));
                    } else {
                        content.push_str(&format!("- {} — {}\n", name, desc));
                    }
                }
            }
            content.push('\n');
        }
    }

    // 未分类的技能
    let uncategorized: Vec<_> = skill_map
        .iter()
        .filter(|(name, _)| !categorized.contains(name.as_str()))
        .collect();
    if !uncategorized.is_empty() {
        content.push_str(&format!("📦 **Other** ({})\n", uncategorized.len()));
        for (name, desc) in &uncategorized {
            if desc.is_empty() {
                content.push_str(&format!("- {}\n", name));
            } else {
                let char_count = desc.chars().count();
                if char_count > 40 {
                    let short: String = desc.chars().take(40).collect();
                    content.push_str(&format!("- {} — {}…\n", name, short));
                } else {
                    content.push_str(&format!("- {} — {}\n", name, desc));
                }
            }
        }
        content.push('\n');
    }

    // 演化记录
    let mut records: Vec<EvolutionRecord> = Vec::new();
    if records_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&records_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(record) = serde_json::from_str::<EvolutionRecord>(&content) {
                            records.push(record);
                        }
                    }
                }
            }
        }
    }
    records.sort_by(|a, b| b.created_at.cmp(&a.created_at));

    let mut seen = HashSet::new();
    let mut learning = Vec::new();
    let mut learned = Vec::new();
    let mut failed = Vec::new();

    for r in &records {
        if is_builtin_tool(&r.skill_name) {
            continue;
        }
        if !seen.insert(r.skill_name.clone()) {
            continue;
        }
        let status_str = format!("{:?}", r.status);
        match status_str.as_str() {
            "Completed" => learned.push(r),
            "Failed" | "RolledBack" | "AuditFailed" | "DryRunFailed" | "TestFailed" => {
                failed.push(r)
            }
            _ => learning.push(r),
        }
    }

    if !learned.is_empty() || !learning.is_empty() || !failed.is_empty() {
        content.push_str("── **Evolution Status** ──\n\n");
    }

    if !learned.is_empty() {
        content.push_str(&format!("✅ **Learned** ({}):\n", learned.len()));
        for r in &learned {
            content.push_str(&format!(
                "- {} ({})\n",
                r.skill_name,
                format_timestamp(r.created_at)
            ));
        }
        content.push('\n');
    }

    if !learning.is_empty() {
        content.push_str(&format!("🔄 **Learning** ({}):\n", learning.len()));
        for r in &learning {
            let status_desc = match format!("{:?}", r.status).as_str() {
                "Triggered" => "pending",
                "Generating" => "generating",
                "Generated" => "generated",
                "Auditing" => "auditing",
                "AuditPassed" => "audit passed",
                "CompilePassed" | "DryRunPassed" | "TestPassed" => "compile passed",
                "CompileFailed" | "DryRunFailed" | "TestFailed" | "Testing" => "compile failed",
                "Observing" | "RollingOut" => "observing",
                _ => "in progress",
            };
            content.push_str(&format!(
                "- {} [{}] ({})\n",
                r.skill_name,
                status_desc,
                format_timestamp(r.created_at)
            ));
        }
        content.push('\n');
    }

    if !failed.is_empty() {
        content.push_str(&format!("❌ **Failed** ({}):\n", failed.len()));
        for r in &failed {
            content.push_str(&format!("- {}\n", r.skill_name));
        }
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_skills_command() {
        let cmd = SkillsCommand;
        let ctx = CommandContext::test_context();

        let result = cmd.execute("", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));
    }
}