//! 鑷姩璁板繂鎻愬彇鍣?//!
//! 鍚庡彴鎻愬彇鍥涚绫诲瀷璁板繂鐨勬牳蹇冮€昏緫銆?
use super::cursor::ExtractionCursorManager;
use super::{get_memory_file_path, MemoryType};
use crate::forked::{
    create_auto_mem_can_use_tool, run_forked_agent, CacheSafeParams, ForkedAgentParams,
};
use crate::memory_event;
use crate::unified_security_scanner::scan_learned_memory_content;
use blockcell_core::types::ChatMessage;
use blockcell_providers::ProviderPool;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// Auto Memory 配置（从 Layer5Config 派生）
#[derive(Debug, Clone)]
pub struct AutoMemoryConfig {
    /// 触发提取的最小消息数
    pub min_messages_for_extraction: usize,
    /// 提取冷却消息数
    pub extraction_cooldown_messages: usize,
    /// 记忆文件最大 token 数
    pub max_memory_file_tokens: usize,
    /// 鎻愬彇鏃堕棿鍐峰嵈闃堝€硷紙绉掞級
    pub extraction_time_cooldown_secs: u64,
    /// 鍐呭鍙樺寲闃堝€硷紙瀛楃鏁帮級
    pub content_change_threshold: usize,
}

impl Default for AutoMemoryConfig {
    fn default() -> Self {
        use super::cursor::{CONTENT_CHANGE_THRESHOLD, TIME_COOLDOWN_SECS};
        Self {
            min_messages_for_extraction: super::MIN_MESSAGES_FOR_EXTRACTION,
            extraction_cooldown_messages: super::EXTRACTION_COOLDOWN_MESSAGES,
            max_memory_file_tokens: super::MAX_MEMORY_FILE_TOKENS,
            extraction_time_cooldown_secs: TIME_COOLDOWN_SECS,
            content_change_threshold: CONTENT_CHANGE_THRESHOLD,
        }
    }
}

impl From<blockcell_core::config::Layer5Config> for AutoMemoryConfig {
    fn from(c: blockcell_core::config::Layer5Config) -> Self {
        Self {
            min_messages_for_extraction: c.min_messages_for_extraction,
            extraction_cooldown_messages: c.extraction_cooldown_messages,
            max_memory_file_tokens: c.max_memory_file_tokens,
            extraction_time_cooldown_secs: c.extraction_time_cooldown_secs,
            content_change_threshold: c.content_change_threshold,
        }
    }
}

/// 鎻愬彇缁撴灉
#[derive(Debug)]
pub struct ExtractionResult {
    /// 璁板繂绫诲瀷
    pub memory_type: MemoryType,
    /// 鎻愬彇鎴愬姛
    pub success: bool,
    /// 杈撳叆 tokens
    pub input_tokens: usize,
    /// 杈撳嚭 tokens
    pub output_tokens: usize,
    /// 閿欒淇℃伅
    pub error: Option<String>,
    /// 娓告爣淇濆瓨鏄惁澶辫触
    pub cursor_save_failed: bool,
}

/// 鎻愬彇鍙傛暟
///
/// 封装 `extract` 方法所需的所有参数，避免函数签名过长。
pub struct ExtractionParams {
    /// LLM Provider 池（必需）
    pub provider_pool: Arc<ProviderPool>,
    /// 璁板繂绫诲瀷
    pub memory_type: MemoryType,
    /// 绯荤粺鎻愮ず
    pub system_prompt: Arc<String>,
    /// 妯″瀷鍚嶇О
    pub model: String,
    /// 娑堟伅鍘嗗彶
    pub messages: Vec<ChatMessage>,
    /// 鏈€鍚庝竴鏉℃秷鎭殑 UUID
    pub last_message_uuid: Uuid,
    /// 褰撳墠娑堟伅鎬绘暟
    pub message_count: usize,
}

/// 自动记忆提取器
pub struct AutoMemoryExtractor {
    /// 閰嶇疆鐩綍
    config_dir: std::path::PathBuf,
    /// 游标管理器
    cursor_manager: ExtractionCursorManager,
    /// 閰嶇疆
    config: AutoMemoryConfig,
}

impl AutoMemoryExtractor {
    /// 创建提取器
    pub async fn new(config_dir: &Path) -> std::io::Result<Self> {
        Self::with_config(config_dir, AutoMemoryConfig::default()).await
    }

    /// 创建提取器（带可配置参数）
    pub async fn with_config(config_dir: &Path, config: AutoMemoryConfig) -> std::io::Result<Self> {
        let mut cursor_manager = ExtractionCursorManager::new(config_dir);
        cursor_manager.load().await?;

        Ok(Self {
            config_dir: config_dir.to_path_buf(),
            cursor_manager,
            config,
        })
    }

    /// 检查是否需要提取
    pub fn should_extract(&self, current_message_count: usize) -> Vec<MemoryType> {
        if current_message_count < self.config.min_messages_for_extraction {
            return Vec::new();
        }

        self.cursor_manager
            .all_cursors()
            .iter()
            .filter(|c| {
                c.should_extract(
                    current_message_count,
                    self.config.extraction_cooldown_messages,
                )
            })
            .map(|c| c.memory_type)
            .collect()
    }

    /// 鎵ц鎻愬彇
    ///
    /// ## 鍙傛暟
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

        // 璇诲彇褰撳墠璁板繂鍐呭
        let current_content = tokio::fs::read_to_string(&memory_path)
            .await
            .unwrap_or_else(|_| memory_type.template().to_string());

        // 鏋勫缓鎻愬彇 prompt
        let extraction_prompt = build_extraction_prompt(
            memory_type,
            &current_content,
            self.config.max_memory_file_tokens,
        );

        // 鍒涘缓 CacheSafeParams
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

        // 如果 builder 失败（理论上不会，因为已经设置 provider_pool）
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

        // 杩愯 Forked Agent
        let result = run_forked_agent(params).await;

        match result {
            Ok(forked_result) => {
                // 安全扫描: 检查写入后的记忆文件内容
                if let Ok(updated_content) = tokio::fs::read_to_string(&memory_path).await {
                    if let Err(err) = scan_learned_memory_content(&updated_content) {
                        tracing::warn!(
                            memory_type = memory_type.name(),
                            error = %err,
                            "[auto_memory] 瀹夊叏鎵弿鍙戠幇濞佽儊, 鍥炴粴璁板繂鏂囦欢"
                        );
                        // 回滚到提取前的内容
                        let _ = tokio::fs::write(&memory_path, &current_content).await;
                        return ExtractionResult {
                            memory_type,
                            success: false,
                            input_tokens: forked_result.total_usage.input_tokens as usize,
                            output_tokens: forked_result.total_usage.output_tokens as usize,
                            error: Some(format!(
                                "瀹夊叏鎵弿澶辫触, 璁板繂鏂囦欢宸插洖婊? {}",
                                err
                            )),
                            cursor_save_failed: false,
                        };
                    }
                }

                // 鏇存柊娓告爣
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

                // 璁板綍 Layer 5 memory_written 浜嬩欢
                // 读取写入后的文件内容来获取长度
                if let Ok(updated_content) = tokio::fs::read_to_string(&memory_path).await {
                    memory_event!(
                        layer5,
                        memory_written,
                        memory_type.name(),
                        memory_path.to_string_lossy().as_ref(),
                        updated_content.len()
                    );
                }

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

/// 鏀舵暃瑙勫垯 + Memory/Skill 杈圭晫瑙勫垯 (鍙傝€?Hermes MEMORY_GUIDANCE)
const EXTRACTION_ENHANCEMENT: &str = r#"
## Convergence Rules
- If nothing has changed since the last extraction, do NOT modify the file 鈥?just stop.
- Do not duplicate information already present in the file.
- Do not add trivial or obvious information.

## Memory vs Skill Boundary
- Memory stores declarative facts: preferences, environment, conventions, user background.
- Skill stores procedural knowledge: steps, workflows, pitfalls, how-to guides.
- "User prefers concise responses" 鈫?memory (save here)
- "Deploy to K8s requires pushing image first" 鈫?skill-domain procedural knowledge (do not save here)
- This extractor is memory-only: do not create, patch, or request skills, and do not write procedural workflows into memory.
"#;

/// 鏋勫缓鎻愬彇 prompt
fn build_extraction_prompt(
    memory_type: MemoryType,
    current_content: &str,
    max_memory_file_tokens: usize,
) -> String {
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
{}
Use the file_edit tool to update the memory file at the configured path.
"#,
        memory_type.name(),
        current_content,
        memory_type.usage_guide(),
        max_memory_file_tokens,
        EXTRACTION_ENHANCEMENT,
    )
}

/// 检查是否应该提取自动记忆（使用配置参数）
pub fn should_extract_auto_memory_with_config(
    cursor_manager: &ExtractionCursorManager,
    current_message_count: usize,
    config: &AutoMemoryConfig,
) -> Vec<MemoryType> {
    if current_message_count < config.min_messages_for_extraction {
        return Vec::new();
    }

    cursor_manager
        .all_cursors()
        .iter()
        .filter(|c| c.should_extract(current_message_count, config.extraction_cooldown_messages))
        .map(|c| c.memory_type)
        .collect()
}

/// 检查是否应该提取自动记忆（使用默认常量）
pub fn should_extract_auto_memory(
    cursor_manager: &ExtractionCursorManager,
    current_message_count: usize,
) -> Vec<MemoryType> {
    should_extract_auto_memory_with_config(
        cursor_manager,
        current_message_count,
        &AutoMemoryConfig::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_extraction_prompt() {
        let prompt = build_extraction_prompt(MemoryType::User, "current content", 4000);
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
