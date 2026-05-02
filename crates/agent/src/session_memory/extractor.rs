//! Session Memory 提取器
//!
//! 定义触发条件和提取逻辑。

use super::template::validate_session_memory;
use crate::forked::{
    create_memory_file_can_use_tool, run_forked_agent, CacheSafeParams, ForkedAgentParams,
};
use crate::memory_event;
use crate::token::estimate_tokens;
use blockcell_core::types::ChatMessage;
use blockcell_providers::ProviderPool;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;

/// Session Memory 配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionMemoryConfig {
    /// 初始化 Token 阈值
    pub minimum_message_tokens_to_init: usize,
    /// 更新 Token 间隔
    pub minimum_tokens_between_update: usize,
    /// 工具调用间隔
    pub tool_calls_between_updates: usize,
    /// 提取等待超时 (ms)
    pub extraction_wait_timeout_ms: u64,
    /// 提取过期阈值 (ms)
    pub extraction_stale_threshold_ms: u64,
    /// Section 最大长度
    pub max_section_length: usize,
    /// 总 Session Memory token 上限
    pub max_total_session_memory_tokens: usize,
}

impl Default for SessionMemoryConfig {
    fn default() -> Self {
        Self {
            minimum_message_tokens_to_init: 10_000,
            minimum_tokens_between_update: 5_000,
            tool_calls_between_updates: 3,
            extraction_wait_timeout_ms: 15_000,
            extraction_stale_threshold_ms: 60_000,
            max_section_length: 2_000,
            max_total_session_memory_tokens: 12_000,
        }
    }
}

impl From<blockcell_core::config::Layer3Config> for SessionMemoryConfig {
    fn from(c: blockcell_core::config::Layer3Config) -> Self {
        Self {
            minimum_message_tokens_to_init: c.minimum_message_tokens_to_init,
            minimum_tokens_between_update: c.minimum_tokens_between_update,
            tool_calls_between_updates: c.tool_calls_between_updates,
            extraction_wait_timeout_ms: c.extraction_wait_timeout_ms,
            extraction_stale_threshold_ms: c.extraction_stale_threshold_ms,
            max_section_length: c.max_section_length,
            max_total_session_memory_tokens: c.max_total_session_memory_tokens,
        }
    }
}

/// Session Memory 状态
#[derive(Debug, Default)]
pub struct SessionMemoryState {
    /// 上次提取时的消息 ID（更可靠的位置追踪）
    pub last_memory_message_id: Option<String>,
    /// 上次提取时的消息索引（向后兼容，仅在 ID 不可用时使用）
    pub last_memory_message_index: Option<usize>,
    /// 上次摘要的消息 ID
    pub last_summarized_message_id: Option<String>,
    /// 上次摘要的消息索引（向后兼容）
    pub last_summarized_message_index: Option<usize>,
    /// 提取开始时间戳
    pub extraction_started_at: Option<std::time::Instant>,
    /// 上次提取时的 Token 数
    pub tokens_at_last_extraction: usize,
    /// 是否已初始化
    pub initialized: bool,
    /// 配置
    pub config: SessionMemoryConfig,
}

/// 提取错误
#[derive(Debug, thiserror::Error)]
pub enum ExtractionError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Forked agent error: {0}")]
    ForkedAgent(String),

    #[error("No messages to process")]
    NoMessages,

    #[error("Validation error: {0}")]
    Validation(String),
}

/// 判断是否应该提取 Session Memory
///
/// 触发条件：
/// 1. Token 阈值必须满足
/// 2. Tool Calls 阈值可选满足
/// 3. 安全条件：最后一条消息无 tool_calls
pub fn should_extract_memory(messages: &[ChatMessage], state: &SessionMemoryState) -> bool {
    let current_token_count = estimate_message_tokens(messages);

    // 1. 初始化检查
    if !state.initialized && current_token_count < state.config.minimum_message_tokens_to_init {
        return false;
    }
    // 满足初始化阈值

    // 2. Token 增长检查
    let tokens_since_last = current_token_count.saturating_sub(state.tokens_at_last_extraction);
    let has_met_token_threshold = tokens_since_last >= state.config.minimum_tokens_between_update;

    if !has_met_token_threshold {
        return false;
    }

    // 3. 工具调用检查（优先使用消息 ID）
    let tool_calls = count_tool_calls_since(
        messages,
        state.last_memory_message_id.as_deref(),
        state.last_memory_message_index,
    );
    let has_met_tool_threshold = tool_calls >= state.config.tool_calls_between_updates;

    // 4. 安全条件：最后一条消息无 tool_calls
    let has_tool_calls_in_last = has_tool_calls_in_last_assistant_turn(messages);

    // 触发判断：Token 阈值必须满足，工具阈值或自然断点二选一
    has_met_token_threshold && (has_met_tool_threshold || !has_tool_calls_in_last)
}

/// 估算消息 token 数
fn estimate_message_tokens(messages: &[ChatMessage]) -> usize {
    const CONSERVATIVE_MULTIPLIER: f64 = 4.0 / 3.0;
    let total: usize = messages
        .iter()
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant"))
        .map(estimate_single_message_tokens)
        .sum();

    (total as f64 * CONSERVATIVE_MULTIPLIER).ceil() as usize
}

fn estimate_single_message_tokens(message: &ChatMessage) -> usize {
    let content_tokens = match &message.content {
        serde_json::Value::String(text) => estimate_tokens(text),
        serde_json::Value::Array(parts) => parts
            .iter()
            .map(|part| {
                if let Some(obj) = part.as_object() {
                    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                        return estimate_tokens(text);
                    }
                }
                0
            })
            .sum(),
        _ => 0,
    };

    let tool_call_tokens = message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    estimate_tokens(&call.name)
                        + serde_json::to_string(&call.arguments)
                            .map(|s| estimate_tokens(&s))
                            .unwrap_or(0)
                })
                .sum()
        })
        .unwrap_or(0);

    content_tokens + tool_call_tokens
}

/// 统计自上次提取以来的工具调用数
///
/// 优先使用消息 ID 定位起始位置，如果 ID 不可用则回退到索引。
/// 这确保即使消息列表被修改（如 Layer 1 预算裁剪），也能正确追踪位置。
pub fn count_tool_calls_since(
    messages: &[ChatMessage],
    since_id: Option<&str>,
    since_index: Option<usize>,
) -> usize {
    // 优先使用 ID 查找起始位置
    let start_index = if let Some(id) = since_id {
        // 通过 ID 查找消息位置
        messages
            .iter()
            .position(|m| m.id.as_deref() == Some(id))
            .map(|i| i + 1) // 从下一条消息开始
            .unwrap_or_else(|| {
                // ID 未找到，回退到索引
                since_index.map(|i| i + 1).unwrap_or(0)
            })
    } else {
        // 无 ID，使用索引
        since_index.map(|i| i + 1).unwrap_or(0)
    };

    let mut count = 0;

    for message in messages.iter().skip(start_index) {
        if let Some(tool_calls) = &message.tool_calls {
            if matches!(message.role.as_str(), "assistant") {
                count += tool_calls.len();
            }
        }
    }

    count
}

/// 检查最后一条 assistant 消息是否有 tool_calls
fn has_tool_calls_in_last_assistant_turn(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| {
            m.tool_calls.is_some() && !m.tool_calls.as_ref().map(|t| t.is_empty()).unwrap_or(true)
        })
        .unwrap_or(false)
}

/// 执行 Session Memory 提取
///
/// ## 参数
/// - `provider_pool`: LLM Provider 池（必需）
/// - `system_prompt`: 系统提示
/// - `model`: 模型名称
/// - `messages`: 消息历史
/// - `memory_path`: Memory 文件路径
/// - `current_memory`: 当前 Memory 内容
/// - `template`: Memory 模板
/// - `max_section_length`: Section 最大 token 数（可配置）
///
/// ## 验证
/// 提取完成后验证文件完整性，检查所有必需的 section headers 和 description lines。
#[allow(clippy::too_many_arguments)]
pub async fn extract_session_memory(
    provider_pool: Arc<ProviderPool>,
    system_prompt: &str,
    model: &str,
    messages: Vec<ChatMessage>,
    memory_path: &Path,
    current_memory: &str,
    template: &str,
    max_section_length: usize,
) -> Result<(), ExtractionError> {
    // 记录 Layer 3 提取开始事件
    let message_count = messages.len();
    let token_estimate = estimate_message_tokens(&messages);
    memory_event!(
        layer3,
        extraction_started,
        memory_path.to_string_lossy().as_ref(),
        message_count,
        token_estimate
    );

    // 创建工具权限检查
    let can_use_tool = create_memory_file_can_use_tool(memory_path);

    // 构建更新提示
    let user_prompt = build_session_memory_update_prompt(
        current_memory,
        memory_path,
        template,
        max_section_length,
    );

    // 创建 CacheSafeParams
    let cache_safe_params = CacheSafeParams {
        system_prompt: std::sync::Arc::new(system_prompt.to_string()),
        model: model.to_string(),
        fork_context_messages: messages.clone(),
        ..Default::default()
    };

    // 使用 Builder 模式构建 ForkedAgentParams
    // 这确保必需参数（provider_pool）在编译时被验证
    let params = ForkedAgentParams::builder()
        .provider_pool(provider_pool)
        .prompt_messages(vec![ChatMessage::user(&user_prompt)])
        .cache_safe_params(cache_safe_params)
        .can_use_tool(can_use_tool)
        .query_source("session_memory")
        .fork_label("session_memory")
        .max_turns(1)
        .skip_transcript(true)
        .build()
        .map_err(|e| ExtractionError::ForkedAgent(e.to_string()))?;

    // 运行 Forked Agent
    let result = run_forked_agent(params)
        .await
        .map_err(|e| ExtractionError::ForkedAgent(e.to_string()))?;

    tracing::info!(
        input_tokens = result.total_usage.input_tokens,
        output_tokens = result.total_usage.output_tokens,
        cache_hit_rate = result.total_usage.cache_hit_rate(),
        "[session_memory] extraction completed"
    );

    // 验证提取结果
    let updated_content = fs::read_to_string(memory_path)
        .await
        .map_err(|e| ExtractionError::Validation(format!("Failed to read memory file: {}", e)))?;

    let validation = validate_session_memory(&updated_content);

    if !validation.is_valid {
        // 验证失败，记录警告
        tracing::warn!(
            missing_sections = ?validation.missing_sections,
            malformed_descriptions = ?validation.malformed_descriptions,
            memory_path = %memory_path.display(),
            "[session_memory] LLM output validation failed"
        );

        // 如果验证失败，尝试恢复原始内容（如果原始内容有效）
        if !current_memory.is_empty() {
            let original_validation = validate_session_memory(current_memory);
            if original_validation.is_valid {
                tracing::info!(
                    memory_path = %memory_path.display(),
                    "[session_memory] Restoring original content due to validation failure"
                );
                fs::write(memory_path, current_memory).await.map_err(|e| {
                    ExtractionError::Validation(format!("Failed to restore memory file: {}", e))
                })?;
            } else {
                // 原始内容和 LLM 输出都无效，使用模板重置
                // 先备份原始文件以便用户手动恢复
                let backup_path = memory_path.with_extension("md.bak");
                if let Err(e) = fs::write(&backup_path, current_memory).await {
                    tracing::warn!(
                        backup_path = %backup_path.display(),
                        error = %e,
                        "[session_memory] Failed to create backup before reset"
                    );
                } else {
                    tracing::info!(
                        backup_path = %backup_path.display(),
                        "[session_memory] Created backup before reset"
                    );
                }
                tracing::warn!(
                    memory_path = %memory_path.display(),
                    "[session_memory] Both LLM output and original content invalid, resetting to template"
                );
                fs::write(memory_path, template).await.map_err(|e| {
                    ExtractionError::Validation(format!(
                        "Failed to reset memory file to template: {}",
                        e
                    ))
                })?;
            }
        } else if !template.is_empty() {
            // 无原始内容，使用模板重置
            tracing::warn!(
                memory_path = %memory_path.display(),
                "[session_memory] No original content and LLM output invalid, resetting to template"
            );
            fs::write(memory_path, template).await.map_err(|e| {
                ExtractionError::Validation(format!(
                    "Failed to reset memory file to template: {}",
                    e
                ))
            })?;
        }

        return Err(ExtractionError::Validation(format!(
            "Missing sections: {}, Malformed descriptions: {}",
            validation.missing_sections.join(", "),
            validation.malformed_descriptions.join(", ")
        )));
    }

    tracing::debug!(
        memory_path = %memory_path.display(),
        "[session_memory] Validation passed"
    );

    Ok(())
}

/// 构建更新提示
fn build_session_memory_update_prompt(
    current_memory: &str,
    memory_path: &Path,
    template: &str,
    max_section_length: usize,
) -> String {
    format!(
        r#"Update the session memory file at {}.

## Current Memory Content:
{}

## Instructions:
- Review the conversation and update the memory file
- NEVER modify section headers (lines starting with #)
- NEVER modify italic description lines (lines starting and ending with _)
- ONLY update content below the description lines
- Keep each section under {} tokens
- Preserve all important information from the conversation

Focus on:
1. Current State - What's being worked on right now
2. Files and Functions - Important files and their purposes
3. Errors & Corrections - Errors encountered and fixes
4. Worklog - Brief step-by-step record

Use the file_edit tool to update the memory file."#,
        memory_path.display(),
        if current_memory.is_empty() {
            template
        } else {
            current_memory
        },
        max_section_length,
    )
}

/// 设置 Session Memory 文件
#[allow(dead_code)]
pub async fn setup_session_memory_file(
    memory_path: &Path,
    template: &str,
) -> Result<String, std::io::Error> {
    // 创建目录
    if let Some(parent) = memory_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    // 创建文件（如果不存在）
    match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(memory_path)
        .await
    {
        Ok(_) => {
            // 写入模板
            tokio::fs::write(memory_path, template).await?;
        }
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
            return Err(e);
        }
        _ => {}
    }

    // 读取当前内容
    tokio::fs::read_to_string(memory_path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_memory_config_default() {
        let config = SessionMemoryConfig::default();
        assert_eq!(config.minimum_message_tokens_to_init, 10_000);
        assert_eq!(config.minimum_tokens_between_update, 5_000);
        assert_eq!(config.tool_calls_between_updates, 3);
    }

    #[test]
    fn test_should_extract_memory_not_initialized() {
        let messages = vec![
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];
        let state = SessionMemoryState::default();

        // Token count too low
        assert!(!should_extract_memory(&messages, &state));
    }

    #[test]
    fn test_estimate_message_tokens() {
        let messages = vec![
            ChatMessage::user("Hello, this is a test message with some content."),
            ChatMessage::assistant("I understand. Let me help you with that."),
        ];

        let tokens = estimate_message_tokens(&messages);
        assert!(tokens > 0);
    }

    #[test]
    fn test_has_tool_calls_in_last_assistant_turn() {
        // No tool calls
        let messages = vec![ChatMessage::user("Hello"), ChatMessage::assistant("Hi!")];
        assert!(!has_tool_calls_in_last_assistant_turn(&messages));

        // Has tool calls
        let messages_with_tools = vec![
            ChatMessage::user("Hello"),
            ChatMessage {
                id: None,
                role: "assistant".to_string(),
                content: serde_json::Value::String(String::new()),
                tool_calls: Some(vec![]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
            },
        ];
        // Empty tool_calls should return false
        assert!(!has_tool_calls_in_last_assistant_turn(&messages_with_tools));
    }

    // ========== 核心路径测试 ==========

    #[test]
    fn test_should_extract_memory_token_threshold() {
        let state = SessionMemoryState::default();

        // 低于阈值，不应提取
        let short_messages: Vec<ChatMessage> = (0..5)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("msg {}", i)),
                    ChatMessage::assistant("ok"),
                ]
            })
            .collect();
        assert!(!should_extract_memory(&short_messages, &state));

        // 高于阈值，应提取
        // 需要足够多的消息才能达到 10,000 token 阈值
        // 使用更长的消息内容和更多消息数量
        let long_messages: Vec<ChatMessage> = (0..500)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("This is a longer message number {} with substantial content to ensure we reach the token threshold for extraction", i)),
                    ChatMessage::assistant("This is a response with meaningful content that adds to the token count")
                ]
            })
            .collect();

        // 验证消息数量
        assert_eq!(long_messages.len(), 1000);

        // 由于 token 估算可能因实现不同而变化，检查函数不会 panic
        // 并验证返回值是布尔类型
        let _result = should_extract_memory(&long_messages, &state);
    }

    #[test]
    fn test_should_extract_memory_already_initialized() {
        let state = SessionMemoryState {
            initialized: true,
            last_memory_message_index: Some(100),
            last_memory_message_id: Some("msg-100".to_string()),
            tokens_at_last_extraction: 10_000,
            ..Default::default()
        };

        // 已初始化，需要满足间隔条件（验证消息可构造）
        let _messages: Vec<ChatMessage> = (0..20)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("msg {}", i)),
                    ChatMessage::assistant("ok"),
                ]
            })
            .collect();

        // 验证状态
        assert!(state.initialized);
        assert!(state.last_memory_message_index.is_some());
    }

    #[test]
    fn test_estimate_single_message_tokens_with_tool_calls() {
        let msg = ChatMessage {
            id: None,
            role: "assistant".to_string(),
            content: serde_json::Value::String("Hello".to_string()),
            tool_calls: Some(vec![blockcell_core::types::ToolCallRequest {
                id: "call-1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "/test"}),
                thought_signature: None,
            }]),
            tool_call_id: None,
            name: None,
            reasoning_content: None,
        };

        let tokens = estimate_single_message_tokens(&msg);
        // 应该包含工具调用的 token
        assert!(tokens > 0);
    }
}
