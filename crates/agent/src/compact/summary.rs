//! Compact 摘要生成
//!
//! 定义 9-part structured summary 结构和生成逻辑。

use crate::token::estimate_tokens;
use blockcell_core::types::ChatMessage;
use blockcell_providers::ProviderPool;
use std::sync::Arc;

/// Compact 摘要章节
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CompactSummarySection {
    /// 用户请求
    UserRequest = 1,
    /// 成功的尝试
    SuccessfulAttempts = 2,
    /// 失败的尝试
    FailedAttempts = 3,
    /// 当前状态
    CurrentState = 4,
    /// 关键文件
    KeyFiles = 5,
    /// 用户偏好
    UserPreferences = 6,
    /// 重要上下文
    ImportantContext = 7,
    /// 待处理任务
    PendingTasks = 8,
    /// 工作日志
    WorkLog = 9,
}

impl CompactSummarySection {
    /// 获取章节标题
    pub fn title(&self) -> &'static str {
        match self {
            Self::UserRequest => "User Request",
            Self::SuccessfulAttempts => "Successful Attempts",
            Self::FailedAttempts => "Failed Attempts",
            Self::CurrentState => "Current State",
            Self::KeyFiles => "Key Files",
            Self::UserPreferences => "User Preferences",
            Self::ImportantContext => "Important Context",
            Self::PendingTasks => "Pending Tasks",
            Self::WorkLog => "Work Log",
        }
    }

    /// 获取章节描述
    pub fn description(&self) -> &'static str {
        match self {
            Self::UserRequest => "What the user asked for, in their own words",
            Self::SuccessfulAttempts => "What worked well - approaches to reuse",
            Self::FailedAttempts => "What failed - approaches to avoid",
            Self::CurrentState => "What is actively being worked on right now",
            Self::KeyFiles => "Files that were read or modified and why",
            Self::UserPreferences => "User's stated preferences for approach",
            Self::ImportantContext => "Critical context for continuing work",
            Self::PendingTasks => "Tasks mentioned but not yet started",
            Self::WorkLog => "Step-by-step summary of actions taken",
        }
    }

    /// 获取所有章节（按优先级排序）
    pub fn all() -> Vec<Self> {
        vec![
            Self::UserRequest,
            Self::SuccessfulAttempts,
            Self::FailedAttempts,
            Self::CurrentState,
            Self::KeyFiles,
            Self::UserPreferences,
            Self::ImportantContext,
            Self::PendingTasks,
            Self::WorkLog,
        ]
    }
}

/// Compact 摘要结构
#[derive(Debug, Clone)]
pub struct CompactSummary {
    /// 章节内容
    sections: Vec<(CompactSummarySection, String)>,
    /// 总 token 数
    total_tokens: usize,
}

/// Compact 摘要生成结果（包含 usage 数据）
#[derive(Debug, Clone)]
pub struct CompactSummaryResult {
    /// 摘要内容
    pub summary: CompactSummary,
    /// 输入 tokens
    pub input_tokens: u64,
    /// 输出 tokens
    pub output_tokens: u64,
    /// 缓存读取 tokens
    pub cache_read_tokens: u64,
    /// 缓存创建 tokens
    pub cache_creation_tokens: u64,
}

impl CompactSummary {
    /// 创建空摘要
    pub fn empty() -> Self {
        Self {
            sections: Vec::new(),
            total_tokens: 0,
        }
    }

    /// 添加章节
    pub fn add_section(&mut self, section: CompactSummarySection, content: String) {
        let tokens = estimate_tokens(&content);
        self.sections.push((section, content));
        self.total_tokens += tokens;
    }

    /// 获取总 token 数
    pub fn total_tokens(&self) -> usize {
        self.total_tokens
    }

    /// 生成 Markdown 格式
    pub fn to_markdown(&self) -> String {
        let mut output = String::new();
        output.push_str("# Conversation Summary\n\n");

        for (section, content) in &self.sections {
            output.push_str(&format!("## {}\n", section.title()));
            output.push_str(&format!("*{}*\n\n", section.description()));
            output.push_str(content);
            output.push_str("\n\n");
        }

        output
    }

    /// 生成 Compact Prompt
    pub fn generate_compact_prompt() -> String {
        let mut prompt = String::new();
        prompt.push_str("Generate a structured summary of the conversation.\n\n");
        prompt.push_str("Include the following sections:\n\n");

        for section in CompactSummarySection::all() {
            prompt.push_str(&format!("{}. {} - {}\n",
                section as u8, section.title(), section.description()));
        }

        prompt.push_str("\nKeep each section concise and focused on information essential for continuing the work.\n");
        prompt.push_str("Omit details that are not relevant to the current task.\n");

        prompt
    }
}

/// 使用 LLM 生成 Compact 摘要
///
/// 通过 Forked Agent 执行，不使用工具
///
/// ## 参数
/// - `provider_pool`: LLM Provider 池（必需）
/// - `system_prompt`: 系统提示
/// - `model`: 模型名称
/// - `messages`: 消息历史
pub async fn generate_compact_summary(
    provider_pool: Arc<ProviderPool>,
    system_prompt: Arc<String>,
    model: &str,
    messages: Vec<ChatMessage>,
) -> Result<CompactSummaryResult, CompactError> {
    use crate::forked::{run_forked_agent, ForkedAgentParams, CacheSafeParams, create_compact_can_use_tool};
    use super::NO_TOOLS_PREAMBLE;

    // 构建 Compact Prompt
    let compact_prompt = CompactSummary::generate_compact_prompt();

    // 创建 CacheSafeParams
    let cache_safe_params = CacheSafeParams {
        system_prompt: system_prompt.clone(),
        model: model.to_string(),
        fork_context_messages: messages.clone(),
        ..Default::default()
    };

    // 使用 Builder 模式构建 ForkedAgentParams
    // 这确保必需参数（provider_pool）在编译时被验证
    let params = ForkedAgentParams::builder()
        .provider_pool(provider_pool)
        .prompt_messages(vec![
            ChatMessage::system(NO_TOOLS_PREAMBLE),
            ChatMessage::user(&compact_prompt),
        ])
        .cache_safe_params(cache_safe_params)
        .can_use_tool(create_compact_can_use_tool())
        .query_source("compact")
        .fork_label("compact_summary")
        .max_turns(1)
        .skip_transcript(true)
        .build()
        .map_err(|e| CompactError::ForkedAgent(e.to_string()))?;

    // 运行 Forked Agent
    let result = run_forked_agent(params)
        .await
        .map_err(|e| CompactError::ForkedAgent(e.to_string()))?;

    // 解析结果
    let summary = parse_summary_from_messages(&result.messages)?;

    tracing::info!(
        input_tokens = result.total_usage.input_tokens,
        output_tokens = result.total_usage.output_tokens,
        cache_read_tokens = result.total_usage.cache_read_input_tokens,
        cache_creation_tokens = result.total_usage.cache_creation_input_tokens,
        summary_tokens = summary.total_tokens(),
        "[compact] summary generated"
    );

    Ok(CompactSummaryResult {
        summary,
        input_tokens: result.total_usage.input_tokens,
        output_tokens: result.total_usage.output_tokens,
        cache_read_tokens: result.total_usage.cache_read_input_tokens,
        cache_creation_tokens: result.total_usage.cache_creation_input_tokens,
    })
}

/// 从消息中解析摘要
fn parse_summary_from_messages(messages: &[ChatMessage]) -> Result<CompactSummary, CompactError> {
    // 找到最后一条 assistant 消息
    let last_assistant = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant");

    match last_assistant {
        Some(msg) => {
            let content = match &msg.content {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(parts) => parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => return Err(CompactError::InvalidFormat),
            };

            // 解析 Markdown 章节
            let mut summary = CompactSummary::empty();
            parse_markdown_sections(&content, &mut summary);

            Ok(summary)
        }
        None => Err(CompactError::NoSummary),
    }
}

/// 解析 Markdown 章节
fn parse_markdown_sections(content: &str, summary: &mut CompactSummary) {
    let mut current_section: Option<CompactSummarySection> = None;
    let mut current_content = String::new();

    for line in content.lines() {
        if line.starts_with("## ") {
            // 保存上一个章节
            if let Some(section) = current_section {
                if !current_content.trim().is_empty() {
                    summary.add_section(section, current_content.trim().to_string());
                }
            }

            // 解析新章节
            let title = line.trim_start_matches("## ").trim();
            current_section = CompactSummarySection::all()
                .iter()
                .find(|s| s.title() == title)
                .cloned();
            current_content = String::new();
        } else if !line.starts_with("*") || !line.ends_with("*") {
            // 非描述行，添加内容
            current_content.push_str(line);
            current_content.push('\n');
        }
    }

    // 保存最后一个章节
    if let Some(section) = current_section {
        if !current_content.trim().is_empty() {
            summary.add_section(section, current_content.trim().to_string());
        }
    }
}

/// Compact 错误类型
#[derive(Debug, thiserror::Error)]
pub enum CompactError {
    #[error("Forked agent error: {0}")]
    ForkedAgent(String),

    #[error("No summary generated")]
    NoSummary,

    #[error("Invalid summary format")]
    InvalidFormat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compact_summary_section_order() {
        let sections = CompactSummarySection::all();
        assert_eq!(sections.len(), 9);

        // 验证优先级顺序
        assert!(sections[0] < sections[8]);
    }

    #[test]
    fn test_compact_summary_to_markdown() {
        let mut summary = CompactSummary::empty();
        summary.add_section(CompactSummarySection::UserRequest, "Build a REST API".to_string());
        summary.add_section(CompactSummarySection::CurrentState, "Working on auth".to_string());

        let md = summary.to_markdown();
        assert!(md.contains("# Conversation Summary"));
        assert!(md.contains("## User Request"));
        assert!(md.contains("Build a REST API"));
    }

    #[test]
    fn test_parse_markdown_sections() {
        let content = r#"## User Request
*What the user asked for*

Build a REST API with authentication.

## Current State
*What is actively being worked on*

Working on JWT implementation.
"#;

        let mut summary = CompactSummary::empty();
        parse_markdown_sections(content, &mut summary);

        assert_eq!(summary.sections.len(), 2);
        assert!(summary.total_tokens > 0);
    }
}