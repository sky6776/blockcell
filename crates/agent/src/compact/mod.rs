//! Compact 模块 - Layer 4 完整压缩
//!
//! 当 Token 预算超限时，执行 LLM 语义压缩。
//!
//! ## 核心流程
//! 1. Pre-Compact Hooks 执行
//! 2. LLM 生成摘要 (9-part structured summary)
//! 3. Post-Compact 恢复 (文件 + 技能 + Session Memory)
//!
//! ## 恢复预算
//! - 文件: 50,000 tokens (最多 5 个文件，每个文件最多 5,000 tokens)
//! - 技能: 25,000 tokens
//! - Session Memory: 12,000 tokens

mod summary;
mod recovery;
mod hooks;
mod file_tracker;
mod skill_tracker;

pub use summary::{CompactSummary, CompactSummarySection, CompactSummaryResult, generate_compact_summary};
pub use recovery::{
    CompactRecoveryContext, FileRecoveryState, SkillRecoveryState,
    create_recovery_context, generate_recovery_message,
};
pub use hooks::{
    PreCompactHook, PostCompactHook, CompactHookRegistry,
    PreCompactContext, PostCompactContext,
    PreCompactResult, PostCompactResult,
};
pub use file_tracker::{FileTracker, FileRecord};
pub use skill_tracker::{SkillTracker, SkillRecord};

use crate::token::estimate_tokens;

/// Compact 配置
/// 总文件恢复预算
pub const MAX_FILE_RECOVERY_TOKENS: usize = 50_000;
/// 单个文件恢复上限 (设计文档: "单文件上限 | 5,000 tokens")
pub const MAX_SINGLE_FILE_TOKENS: usize = 5_000;
/// 技能恢复预算
pub const MAX_SKILL_RECOVERY_TOKENS: usize = 25_000;
/// Session Memory 恢复预算
pub const MAX_SESSION_MEMORY_RECOVERY_TOKENS: usize = 12_000;
/// 最大恢复文件数
pub const MAX_FILES_TO_RECOVER: usize = 5;

/// 禁止工具使用的 preamble
pub const NO_TOOLS_PREAMBLE: &str = r#"IMPORTANT: You are in compact mode.
You cannot use any tools. You must generate a summary based solely on the conversation history.
Do not attempt to call any tools, read files, or execute commands."#;

/// Compact 触发检查
pub fn should_compact(
    current_tokens: usize,
    budget_tokens: usize,
    threshold: f64,  // 默认 0.8
) -> bool {
    current_tokens >= (budget_tokens as f64 * threshold) as usize
}

/// Compact 配置
#[derive(Debug, Clone)]
pub struct CompactConfig {
    /// Token 阈值（超过此值触发压缩）
    pub token_threshold: usize,
    /// 阈值比例（默认 0.8）
    pub threshold_ratio: f64,
    /// 保留最近消息数
    pub keep_recent_messages: usize,
    /// 最大输出 tokens
    pub max_output_tokens: usize,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            token_threshold: 100_000,
            threshold_ratio: 0.8,
            keep_recent_messages: 2,
            max_output_tokens: 12_000,
        }
    }
}

/// 压缩结果
#[derive(Debug)]
pub struct CompactResult {
    /// 压缩后的摘要消息
    pub summary_message: String,
    /// 恢复消息（文件 + 技能 + Session Memory）
    pub recovery_message: String,
    /// 压缩前 token 数
    pub pre_compact_tokens: usize,
    /// 压缩后 token 数（估算）
    pub post_compact_tokens: usize,
    /// 缓存读取的 tokens（来自 LLM API 响应）
    pub cache_read_tokens: u64,
    /// 缓存创建的 tokens（来自 LLM API 响应）
    pub cache_creation_tokens: u64,
    /// 是否成功
    pub success: bool,
    /// 错误信息（如果失败）
    pub error: Option<String>,
}

impl CompactResult {
    /// 创建失败的压缩结果
    pub fn failed(error: &str) -> Self {
        Self {
            summary_message: String::new(),
            recovery_message: String::new(),
            pre_compact_tokens: 0,
            post_compact_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            success: false,
            error: Some(error.to_string()),
        }
    }

    /// 创建成功的压缩结果
    pub fn success(
        summary_message: String,
        recovery_message: String,
        pre_compact_tokens: usize,
        post_compact_tokens: usize,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) -> Self {
        Self {
            summary_message,
            recovery_message,
            pre_compact_tokens,
            post_compact_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            success: true,
            error: None,
        }
    }

    /// 生成最终的压缩后消息
    pub fn to_compact_message(&self) -> String {
        let mut message = String::new();

        // 添加摘要
        if !self.summary_message.is_empty() {
            message.push_str("# Conversation Compacted\n\n");
            message.push_str(&self.summary_message);
        }

        // 添加恢复信息
        if !self.recovery_message.is_empty() {
            message.push_str("\n\n---\n\n");
            message.push_str(&self.recovery_message);
        }

        message
    }
}

/// 构建 Post-Compact 恢复消息
///
/// 收集文件、技能和 Session Memory 的恢复信息
pub fn build_recovery_message(
    file_tracker: &FileTracker,
    skill_tracker: &SkillTracker,
    session_memory_content: Option<&str>,
) -> String {
    let mut recovery = String::new();
    let mut total_tokens = 0;

    // 1. 文件恢复
    let files = file_tracker.get_recent_files(MAX_FILES_TO_RECOVER, MAX_SINGLE_FILE_TOKENS);
    let files_count = files.len();
    if !files.is_empty() {
        recovery.push_str("## Files Previously Read\n\n");

        for file in &files {
            let truncated_summary = truncate_to_tokens(&file.summary, MAX_SINGLE_FILE_TOKENS);
            recovery.push_str(&format!("### {}\n```\n{}\n```\n\n", file.path.display(), truncated_summary));
            total_tokens += file.estimated_tokens.min(MAX_SINGLE_FILE_TOKENS);
        }
    }

    // 2. 技能恢复
    let skills = skill_tracker.get_recent_skills(MAX_SINGLE_FILE_TOKENS);
    let skills_count = skills.len();
    if !skills.is_empty() {
        recovery.push_str("## Skills Previously Loaded\n\n");

        for skill in &skills {
            let truncated_summary = truncate_to_tokens(&skill.summary, MAX_SINGLE_FILE_TOKENS);
            recovery.push_str(&format!("### {}\n```\n{}\n```\n\n", skill.name, truncated_summary));
            total_tokens += skill.estimated_tokens.min(MAX_SINGLE_FILE_TOKENS);
        }
    }

    // 3. Session Memory 恢复
    if let Some(session_memory) = session_memory_content {
        if !session_memory.is_empty() {
            recovery.push_str("## Session Memory\n\n");
            let truncated = truncate_to_tokens(session_memory, MAX_SESSION_MEMORY_RECOVERY_TOKENS);
            recovery.push_str(&truncated);
            total_tokens += estimate_tokens(&truncated);
        }
    }

    tracing::info!(
        files_count = files_count,
        skills_count = skills_count,
        has_session_memory = session_memory_content.is_some(),
        total_recovery_tokens = total_tokens,
        "[compact] built recovery message"
    );

    recovery
}

/// 截断字符串到指定 token 数（安全处理 UTF-8 边界）
///
/// 如果内容超过最大 token 数，安全截断到最近的 UTF-8 字符边界。
/// 确保至少保留第一个有效字符（即使它很大），避免返回空字符串。
fn truncate_to_tokens(content: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens * 4;
    if content.len() <= max_chars {
        content.to_string()
    } else {
        // 找到安全的 UTF-8 边界
        let mut boundary = max_chars;
        while boundary > 0 && !content.is_char_boundary(boundary) {
            boundary -= 1;
        }

        // 边界情况：如果 boundary 为 0，找到第一个有效字符
        if boundary == 0 {
            // 找到第一个字符的结束位置
            if let Some(first_char) = content.chars().next() {
                boundary = first_char.len_utf8();
            } else {
                // 内容为空（不应该发生，因为前面已检查长度）
                return content.to_string();
            }
        }

        format!("{}...\n[content truncated]", &content[..boundary])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_compact_below_threshold() {
        // 低于阈值，不应压缩
        assert!(!should_compact(50_000, 100_000, 0.8));
    }

    #[test]
    fn test_should_compact_at_threshold() {
        // 达到阈值，应压缩
        assert!(should_compact(80_000, 100_000, 0.8));
    }

    #[test]
    fn test_should_compact_above_threshold() {
        // 超过阈值，应压缩
        assert!(should_compact(100_000, 100_000, 0.8));
    }

    #[test]
    fn test_compact_config_default() {
        let config = CompactConfig::default();
        assert_eq!(config.token_threshold, 100_000);
        assert_eq!(config.threshold_ratio, 0.8);
        assert_eq!(config.keep_recent_messages, 2);
    }

    #[test]
    fn test_compact_result_failed() {
        let result = CompactResult::failed("Test error message");

        assert!(!result.success);
        assert_eq!(result.error, Some("Test error message".to_string()));
        assert!(result.summary_message.is_empty());
        assert!(result.recovery_message.is_empty());
    }

    #[test]
    fn test_compact_result_success() {
        let result = CompactResult::success(
            "Summary content".to_string(),
            "Recovery content".to_string(),
            100_000,
            20_000,
            80_000,  // cache_read_tokens
            10_000,  // cache_creation_tokens
        );

        assert!(result.success);
        assert!(result.error.is_none());
        assert_eq!(result.summary_message, "Summary content");
        assert_eq!(result.recovery_message, "Recovery content");
        assert_eq!(result.pre_compact_tokens, 100_000);
        assert_eq!(result.post_compact_tokens, 20_000);
        assert_eq!(result.cache_read_tokens, 80_000);
        assert_eq!(result.cache_creation_tokens, 10_000);
    }

    #[test]
    fn test_compact_result_to_compact_message() {
        let result = CompactResult::success(
            "Summary".to_string(),
            "Recovery".to_string(),
            100,
            50,
            80,
            10,
        );

        let message = result.to_compact_message();

        assert!(message.contains("# Conversation Compacted"));
        assert!(message.contains("Summary"));
        assert!(message.contains("Recovery"));
    }

    #[test]
    fn test_truncate_to_tokens_short() {
        let content = "Short content";
        let result = truncate_to_tokens(content, 100);

        assert_eq!(result, content);
    }

    #[test]
    fn test_truncate_to_tokens_long() {
        let content = "This is a very long content that should be truncated";
        let result = truncate_to_tokens(content, 5);

        assert!(result.contains("[content truncated]"));
        assert!(result.len() < content.len() + 30);
    }

    #[test]
    fn test_truncate_to_tokens_utf8() {
        let content = "你好世界，这是一个测试内容，用于验证 UTF-8 边界处理";
        let result = truncate_to_tokens(content, 5);

        // 应该在安全边界截断，不应该 panic
        assert!(result.contains("[content truncated]") || result == content);
    }
}