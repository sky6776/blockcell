//! 自动记忆提取器
//!
//! 后台提取四种类型记忆的核心逻辑。

use super::{
    MemoryType, get_memory_file_path, MIN_MESSAGES_FOR_EXTRACTION,
    EXTRACTION_COOLDOWN_MESSAGES, MAX_MEMORY_FILE_TOKENS,
};
use super::cursor::{ExtractionCursor, ExtractionCursorManager};
use crate::forked::{
    run_forked_agent, ForkedAgentParams, CacheSafeParams,
    create_auto_mem_can_use_tool,
};
use blockcell_core::types::ChatMessage;
use blockcell_providers::ProviderPool;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// 提取结果
#[derive(Debug)]
pub struct ExtractionResult {
    /// 记忆类型
    pub memory_type: MemoryType,
    /// 提取成功
    pub success: bool,
    /// 输入 tokens
    pub input_tokens: usize,
    /// 输出 tokens
    pub output_tokens: usize,
    /// 错误信息
    pub error: Option<String>,
    /// 游标保存是否失败
    pub cursor_save_failed: bool,
}

/// 提取参数
///
/// 封装 `extract` 方法所需的所有参数，避免函数签名过长。
pub struct ExtractionParams {
    /// LLM Provider 池（必需）
    pub provider_pool: Arc<ProviderPool>,
    /// 记忆类型
    pub memory_type: MemoryType,
    /// 系统提示
    pub system_prompt: Arc<String>,
    /// 模型名称
    pub model: String,
    /// 消息历史
    pub messages: Vec<ChatMessage>,
    /// 最后一条消息的 UUID
    pub last_message_uuid: Uuid,
    /// 当前消息总数
    pub message_count: usize,
}

/// 自动记忆提取器
pub struct AutoMemoryExtractor {
    /// 配置目录
    config_dir: std::path::PathBuf,
    /// 游标管理器
    cursor_manager: ExtractionCursorManager,
}

impl AutoMemoryExtractor {
    /// 创建提取器
    pub async fn new(config_dir: &Path) -> std::io::Result<Self> {
        let mut cursor_manager = ExtractionCursorManager::new(config_dir);
        cursor_manager.load().await?;

        Ok(Self {
            config_dir: config_dir.to_path_buf(),
            cursor_manager,
        })
    }

    /// 检查是否需要提取
    pub fn should_extract(&self, current_message_count: usize) -> Vec<MemoryType> {
        if current_message_count < MIN_MESSAGES_FOR_EXTRACTION {
            return Vec::new();
        }

        self.cursor_manager
            .all_cursors()
            .iter()
            .filter(|c| c.should_extract(current_message_count, EXTRACTION_COOLDOWN_MESSAGES))
            .map(|c| c.memory_type)
            .collect()
    }

    /// 执行提取
    ///
    /// ## 参数
    /// - `params`: 提取参数（参见 `ExtractionParams`）
    pub async fn extract(&mut self, params: ExtractionParams) -> ExtractionResult {
        let ExtractionParams {
            provider_pool,
            memory_type,
            system_prompt,
            model,
            messages,
            last_message_uuid,
            message_count,
        } = params;

        let memory_path = get_memory_file_path(&self.config_dir, memory_type);

        // 读取当前记忆内容
        let current_content = tokio::fs::read_to_string(&memory_path)
            .await
            .unwrap_or_else(|_| memory_type.template().to_string());

        // 构建提取 prompt
        let extraction_prompt = build_extraction_prompt(memory_type, &current_content);

        // 创建 CacheSafeParams
        let cache_safe_params = CacheSafeParams {
            system_prompt: system_prompt.clone(),
            model: model.clone(),
            fork_context_messages: messages.clone(),
            ..Default::default()
        };

        // 使用 Builder 模式构建 ForkedAgentParams（强制设置 provider_pool）
        let params = ForkedAgentParams::builder()
            .provider_pool(provider_pool)
            .prompt_messages(vec![ChatMessage::user(&extraction_prompt)])
            .cache_safe_params(cache_safe_params)
            .can_use_tool(create_auto_mem_can_use_tool(&self.config_dir))
            .query_source("auto_memory")
            .fork_label(memory_type.name())
            .max_turns(1)
            .skip_transcript(true)
            .build();

        // 如果 builder 失败（理论上不会，因为我们设置了 provider_pool）
        let params = match params {
            Ok(p) => p,
            Err(e) => {
                return ExtractionResult {
                    memory_type,
                    success: false,
                    input_tokens: 0,
                    output_tokens: 0,
                    error: Some(format!("Failed to build ForkedAgentParams: {}", e)),
                    cursor_save_failed: false,
                };
            }
        };

        // 运行 Forked Agent
        let result = run_forked_agent(params).await;

        match result {
            Ok(forked_result) => {
                // 更新游标
                let mut cursor = self.cursor_manager.get_cursor(memory_type);
                cursor.update(last_message_uuid, message_count);
                self.cursor_manager.update_cursor(cursor);

                // 保存游标状态
                let cursor_save_failed = match self.cursor_manager.save().await {
                    Err(e) => {
                        tracing::error!(error = %e, "[auto_memory] failed to save cursor");
                        true
                    }
                    Ok(()) => false,
                };

                tracing::info!(
                    memory_type = memory_type.name(),
                    input_tokens = forked_result.total_usage.input_tokens,
                    output_tokens = forked_result.total_usage.output_tokens,
                    cursor_save_failed,
                    "[auto_memory] extraction completed"
                );

                ExtractionResult {
                    memory_type,
                    success: true,
                    input_tokens: forked_result.total_usage.input_tokens as usize,
                    output_tokens: forked_result.total_usage.output_tokens as usize,
                    error: None,
                    cursor_save_failed,
                }
            }
            Err(e) => {
                tracing::error!(
                    memory_type = memory_type.name(),
                    error = %e,
                    "[auto_memory] extraction failed"
                );

                ExtractionResult {
                    memory_type,
                    success: false,
                    input_tokens: 0,
                    output_tokens: 0,
                    error: Some(e.to_string()),
                    cursor_save_failed: false,
                }
            }
        }
    }

    /// 保存游标状态
    pub async fn save_cursors(&self) -> std::io::Result<()> {
        self.cursor_manager.save().await
    }
}

/// 构建提取 prompt
fn build_extraction_prompt(memory_type: MemoryType, current_content: &str) -> String {
    format!(
        r#"Update the {} memory file.

## Current Memory Content:
{}

## Memory Type Purpose:
{}

## Instructions:
- Review the recent conversation and extract relevant information for this memory type
- DO NOT modify the YAML frontmatter
- DO NOT modify section headers
- ONLY update content below the section headers
- Keep the total content under {} tokens
- Preserve all existing important information
- Add new information that matches this memory type's purpose

Use the file_edit tool to update the memory file at the configured path.
"#,
        memory_type.name(),
        current_content,
        memory_type.usage_guide(),
        MAX_MEMORY_FILE_TOKENS,
    )
}

/// 检查是否应该提取自动记忆
pub fn should_extract_auto_memory(
    cursor_manager: &ExtractionCursorManager,
    current_message_count: usize,
) -> Vec<MemoryType> {
    if current_message_count < MIN_MESSAGES_FOR_EXTRACTION {
        return Vec::new();
    }

    cursor_manager
        .all_cursors()
        .iter()
        .filter(|c| c.should_extract(current_message_count, EXTRACTION_COOLDOWN_MESSAGES))
        .map(|c| c.memory_type)
        .collect()
}

/// 执行单次自动记忆提取
///
/// ## 参数
/// - `provider_pool`: LLM Provider 池（必需）
/// - `config_dir`: 配置目录
/// - `memory_type`: 记忆类型
/// - `system_prompt`: 系统提示
/// - `model`: 模型名称
/// - `messages`: 消息历史
/// - `cursor`: 用于验证提取条件的游标
pub async fn extract_auto_memory(
    provider_pool: Arc<ProviderPool>,
    config_dir: &Path,
    memory_type: MemoryType,
    system_prompt: Arc<String>,
    model: &str,
    messages: Vec<ChatMessage>,
    cursor: ExtractionCursor,  // 用于验证提取条件
) -> ExtractionResult {
    // 使用 cursor 验证是否满足提取条件
    let current_message_count = messages.len();
    if !cursor.should_extract(current_message_count, EXTRACTION_COOLDOWN_MESSAGES) {
        tracing::debug!(
            memory_type = memory_type.name(),
            last_count = cursor.last_message_count,
            current_count = current_message_count,
            "[auto_memory] extraction skipped - cooldown not met"
        );
        return ExtractionResult {
            memory_type,
            success: false,
            input_tokens: 0,
            output_tokens: 0,
            error: Some("Extraction cooldown not met".to_string()),
            cursor_save_failed: false,
        };
    }

    let memory_path = get_memory_file_path(config_dir, memory_type);

    // 读取当前内容
    let current_content = tokio::fs::read_to_string(&memory_path)
        .await
        .unwrap_or_else(|_| memory_type.template().to_string());

    // 构建 prompt
    let extraction_prompt = build_extraction_prompt(memory_type, &current_content);

    // 创建 CacheSafeParams
    let cache_safe_params = CacheSafeParams {
        system_prompt,
        model: model.to_string(),
        fork_context_messages: messages,
        ..Default::default()
    };

    // 使用 Builder 模式构建 ForkedAgentParams
    // 这确保必需参数（provider_pool）在编译时被验证
    let params = match ForkedAgentParams::builder()
        .provider_pool(provider_pool)
        .prompt_messages(vec![ChatMessage::user(&extraction_prompt)])
        .cache_safe_params(cache_safe_params)
        .can_use_tool(create_auto_mem_can_use_tool(config_dir))
        .query_source("auto_memory")
        .fork_label(memory_type.name())
        .max_turns(1)
        .skip_transcript(true)
        .build()
    {
        Ok(p) => p,
        Err(e) => {
            return ExtractionResult {
                memory_type,
                success: false,
                input_tokens: 0,
                output_tokens: 0,
                error: Some(e.to_string()),
                cursor_save_failed: false,
            };
        }
    };

    // 运行 Forked Agent
    let result = run_forked_agent(params).await;

    match result {
        Ok(forked_result) => {
            tracing::info!(
                memory_type = memory_type.name(),
                input_tokens = forked_result.total_usage.input_tokens,
                output_tokens = forked_result.total_usage.output_tokens,
                "[auto_memory] extraction completed"
            );

            ExtractionResult {
                memory_type,
                success: true,
                input_tokens: forked_result.total_usage.input_tokens as usize,
                output_tokens: forked_result.total_usage.output_tokens as usize,
                error: None,
                cursor_save_failed: false,
            }
        }
        Err(e) => {
            ExtractionResult {
                memory_type,
                success: false,
                input_tokens: 0,
                output_tokens: 0,
                error: Some(e.to_string()),
                cursor_save_failed: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_extraction_prompt() {
        let prompt = build_extraction_prompt(MemoryType::User, "current content");
        assert!(prompt.contains("user memory file"));
        assert!(prompt.contains("current content"));
        assert!(prompt.contains("4000 tokens"));
    }

    #[test]
    fn test_should_extract_auto_memory() {
        let manager = ExtractionCursorManager::new(Path::new("/config"));

        // 消息数不足
        let types = should_extract_auto_memory(&manager, 5);
        assert!(types.is_empty());

        // 消息数足够
        let types = should_extract_auto_memory(&manager, 15);
        assert!(!types.is_empty());
        assert!(types.contains(&MemoryType::User));
    }
}