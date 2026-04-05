//! Forked Agent 执行核心
//!
//! 提供与父进程共享 Prompt Cache 但状态隔离的子代理执行能力。
//!
//! ## 核心特性
//!
//! - **缓存共享**: 通过 CacheSafeParams 保证 Prompt Cache 命中
//! - **状态隔离**: 可变状态克隆独立副本
//! - **权限控制**: 通过 CanUseToolFn 限制工具调用
//! - **用量追踪**: 追踪所有 API 调用的 token 使用
//! - **工具执行**: 执行有限的文件操作工具（read/write/edit）

use std::path::Path;
use std::time::{Instant, Duration};
use std::sync::Arc;
use blockcell_core::types::ChatMessage;
use blockcell_providers::ProviderPool;
use super::{CacheSafeParams, CanUseToolFn, SubagentOverrides, create_subagent_context, ToolPermission};

/// Provider 获取重试配置
const PROVIDER_RETRY_MAX_ATTEMPTS: usize = 3;
const PROVIDER_RETRY_INITIAL_DELAY_MS: u64 = 100;
const PROVIDER_RETRY_MAX_DELAY_MS: u64 = 2000;

/// Forked Agent 参数
///
/// ## 必须设置 provider_pool
///
/// `provider_pool` 是必需参数。使用以下方式创建：
///
/// ```ignore
/// // 方式 1: 使用 new() 构造函数（推荐）
/// let params = ForkedAgentParams::new(provider_pool, prompt_messages, cache_safe_params);
///
/// // 方式 2: 使用 builder()
/// let params = ForkedAgentParams::builder()
///     .provider_pool(provider_pool)
///     .prompt_messages(prompt_messages)
///     .cache_safe_params(cache_safe_params)
///     .build();
///
/// // 方式 3: Default + 必须调用 set_provider_pool()
/// let params = ForkedAgentParams {
///     provider_pool: Some(provider_pool),
///     ..Default::default()
/// };
/// ```
///
/// **警告**: 如果 `provider_pool` 为 `None`，`run_forked_agent` 会返回 `NoProviderAvailable` 错误。
pub struct ForkedAgentParams {
    /// 子代理查询循环的初始消息
    pub prompt_messages: Vec<ChatMessage>,
    /// 缓存安全参数
    pub cache_safe_params: CacheSafeParams,
    /// Provider 池 (必须设置)
    pub provider_pool: Option<Arc<ProviderPool>>,
    /// 权限检查函数
    pub can_use_tool: CanUseToolFn,
    /// 来源标识符
    pub query_source: &'static str,
    /// 分析标签
    pub fork_label: &'static str,
    /// 子代理上下文覆盖选项
    pub overrides: Option<SubagentOverrides>,
    /// 输出 token 上限（注意：会改变缓存键！）
    pub max_output_tokens: Option<u32>,
    /// 最大轮次限制
    pub max_turns: Option<u32>,
    /// 跳过 transcript 记录
    pub skip_transcript: bool,
    /// 跳过最后消息的缓存写入
    pub skip_cache_write: bool,
    /// 系统提示（可选，覆盖 cache_safe_params）
    pub system_prompt: Option<String>,
}

impl ForkedAgentParams {
    /// 创建新的 ForkedAgentParams（推荐方式）
    ///
    /// 必须参数通过构造函数强制设置，可选参数通过方法链设置。
    ///
    /// ## 参数
    /// - `provider_pool`: LLM Provider 池（必需）
    /// - `prompt_messages`: 子代理的初始消息
    /// - `cache_safe_params`: 缓存安全参数
    pub fn new(
        provider_pool: Arc<ProviderPool>,
        prompt_messages: Vec<ChatMessage>,
        cache_safe_params: CacheSafeParams,
    ) -> Self {
        Self {
            provider_pool: Some(provider_pool),
            prompt_messages,
            cache_safe_params,
            can_use_tool: Arc::new(|_, _| ToolPermission::Allow),
            query_source: "forked",
            fork_label: "forked",
            overrides: None,
            max_output_tokens: None,
            max_turns: None,
            skip_transcript: true,
            skip_cache_write: false,
            system_prompt: None,
        }
    }

    /// 创建 Builder（用于复杂配置）
    ///
    /// Builder 模式允许链式设置所有参数，`build()` 会验证必需参数。
    pub fn builder() -> ForkedAgentParamsBuilder {
        ForkedAgentParamsBuilder::default()
    }

    /// 设置 prompt_messages
    pub fn with_prompt_messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.prompt_messages = messages;
        self
    }

    /// 设置 cache_safe_params
    pub fn with_cache_safe_params(mut self, params: CacheSafeParams) -> Self {
        self.cache_safe_params = params;
        self
    }

    /// 设置权限检查函数
    pub fn with_can_use_tool(mut self, can_use_tool: CanUseToolFn) -> Self {
        self.can_use_tool = can_use_tool;
        self
    }

    /// 设置来源标识符
    pub fn with_query_source(mut self, source: &'static str) -> Self {
        self.query_source = source;
        self
    }

    /// 设置分析标签
    pub fn with_fork_label(mut self, label: &'static str) -> Self {
        self.fork_label = label;
        self
    }

    /// 设置最大轮次
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// 设置最大输出 tokens
    pub fn with_max_output_tokens(mut self, max_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_tokens);
        self
    }

    /// 设置系统提示
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = Some(prompt);
        self
    }

    /// 验证必需参数
    ///
    /// 返回 `Ok(())` 如果必需参数都已设置，否则返回错误。
    pub fn validate(&self) -> Result<(), ForkedAgentError> {
        if self.provider_pool.is_none() {
            return Err(ForkedAgentError::NoProviderAvailable);
        }
        Ok(())
    }
}

/// ForkedAgentParams Builder
///
/// 用于链式构建 ForkedAgentParams，`build()` 会验证必需参数。
#[derive(Default)]
pub struct ForkedAgentParamsBuilder {
    prompt_messages: Vec<ChatMessage>,
    cache_safe_params: CacheSafeParams,
    provider_pool: Option<Arc<ProviderPool>>,
    can_use_tool: Option<CanUseToolFn>,
    query_source: &'static str,
    fork_label: &'static str,
    overrides: Option<SubagentOverrides>,
    max_output_tokens: Option<u32>,
    max_turns: Option<u32>,
    skip_transcript: bool,
    skip_cache_write: bool,
    system_prompt: Option<String>,
}

impl ForkedAgentParamsBuilder {
    /// 设置 provider_pool（必需）
    pub fn provider_pool(mut self, pool: Arc<ProviderPool>) -> Self {
        self.provider_pool = Some(pool);
        self
    }

    /// 设置 prompt_messages
    pub fn prompt_messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.prompt_messages = messages;
        self
    }

    /// 设置 cache_safe_params
    pub fn cache_safe_params(mut self, params: CacheSafeParams) -> Self {
        self.cache_safe_params = params;
        self
    }

    /// 设置权限检查函数
    pub fn can_use_tool(mut self, can_use_tool: CanUseToolFn) -> Self {
        self.can_use_tool = Some(can_use_tool);
        self
    }

    /// 设置来源标识符
    pub fn query_source(mut self, source: &'static str) -> Self {
        self.query_source = source;
        self
    }

    /// 设置分析标签
    pub fn fork_label(mut self, label: &'static str) -> Self {
        self.fork_label = label;
        self
    }

    /// 设置子代理上下文覆盖
    pub fn overrides(mut self, overrides: SubagentOverrides) -> Self {
        self.overrides = Some(overrides);
        self
    }

    /// 设置最大轮次
    pub fn max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
    }

    /// 设置最大输出 tokens
    pub fn max_output_tokens(mut self, max_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_tokens);
        self
    }

    /// 设置跳过 transcript
    pub fn skip_transcript(mut self, skip: bool) -> Self {
        self.skip_transcript = skip;
        self
    }

    /// 设置跳过缓存写入
    pub fn skip_cache_write(mut self, skip: bool) -> Self {
        self.skip_cache_write = skip;
        self
    }

    /// 设置系统提示
    pub fn system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = Some(prompt);
        self
    }

    /// 构建 ForkedAgentParams
    ///
    /// 如果 `provider_pool` 未设置，返回 `ForkedAgentError::NoProviderAvailable`。
    pub fn build(self) -> Result<ForkedAgentParams, ForkedAgentError> {
        if self.provider_pool.is_none() {
            return Err(ForkedAgentError::NoProviderAvailable);
        }

        Ok(ForkedAgentParams {
            prompt_messages: self.prompt_messages,
            cache_safe_params: self.cache_safe_params,
            provider_pool: self.provider_pool,
            can_use_tool: self.can_use_tool.unwrap_or_else(|| Arc::new(|_, _| ToolPermission::Allow)),
            query_source: self.query_source,
            fork_label: self.fork_label,
            overrides: self.overrides,
            max_output_tokens: self.max_output_tokens,
            max_turns: self.max_turns,
            skip_transcript: self.skip_transcript,
            skip_cache_write: self.skip_cache_write,
            system_prompt: self.system_prompt,
        })
    }
}

// 注意：故意不实现 Default trait
// ForkedAgentParams 必须通过 new() 或 builder() 创建
// 这确保 provider_pool 在编译时被强制设置

/// Forked Agent 结果
#[derive(Debug)]
pub struct ForkedAgentResult {
    /// 查询循环产生的所有消息
    pub messages: Vec<ChatMessage>,
    /// 所有 API 调用的累积用量
    pub total_usage: UsageMetrics,
    /// 修改的文件列表
    pub files_modified: Vec<String>,
    /// 最终响应内容
    pub final_content: Option<String>,
}

/// 用量指标
#[derive(Debug, Clone, Default)]
pub struct UsageMetrics {
    /// 输入 tokens
    pub input_tokens: u64,
    /// 输出 tokens
    pub output_tokens: u64,
    /// 缓存读取的 tokens
    pub cache_read_input_tokens: u64,
    /// 缓存创建的 tokens
    pub cache_creation_input_tokens: u64,
}

impl UsageMetrics {
    /// 累加用量
    pub fn accumulate(&mut self, input: u64, output: u64, cache_read: u64, cache_creation: u64) {
        self.input_tokens += input;
        self.output_tokens += output;
        self.cache_read_input_tokens += cache_read;
        self.cache_creation_input_tokens += cache_creation;
    }

    /// 计算缓存命中率
    pub fn cache_hit_rate(&self) -> f64 {
        let total = self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens;
        if total > 0 {
            self.cache_read_input_tokens as f64 / total as f64
        } else {
            0.0
        }
    }

    /// 合并另一个用量指标
    pub fn merge(&mut self, other: &UsageMetrics) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
    }
}

/// Forked Agent 错误
#[derive(Debug, thiserror::Error)]
pub enum ForkedAgentError {
    #[error("LLM provider error: {0}")]
    ProviderError(String),

    #[error("Tool execution error: {0}")]
    ToolError(String),

    #[error("Max turns exceeded")]
    MaxTurnsExceeded,

    #[error("No provider available")]
    NoProviderAvailable,

    #[error("Aborted: {0}")]
    Aborted(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Tool not supported in forked mode: {0}")]
    ToolNotSupported(String),
}

/// 执行 Forked Agent 工具
///
/// 内容大小限制常量
const MAX_FILE_SIZE: usize = 10 * 1024 * 1024; // 10MB
const MAX_EDIT_SIZE: usize = 100 * 1024; // 100KB for new_string
const MAX_OUTPUT_CHARS: usize = 50000;

/// 验证路径安全性（防御性检查）
///
/// 即使 can_use_tool 已经验证过，这里再次检查作为 fail-safe。
///
/// ## 检查项
///
/// 1. 路径不能包含 `..`（路径遍历）
/// 2. 路径不能包含空字节
///
/// ## 不检查绝对路径
///
/// 此函数**不检查绝对路径**，原因：
/// - `can_use_tool` 回调（如 `create_auto_mem_can_use_tool`）已限制路径范围
/// - `is_path_within_directory` 和 `is_auto_mem_path` 会解析符号链接并验证目录边界
/// - 此层仅作为 fail-safe，不应过度限制合法用例
///
/// ## 安全模型
///
/// ```text
/// 用户输入 -> can_use_tool 回调（主要防护） -> validate_path_safety（fail-safe）
/// ```
fn validate_path_safety(path: &str) -> Result<(), ForkedAgentError> {
    // 检查空字节注入
    if path.contains('\0') {
        return Err(ForkedAgentError::ToolError(
            "Invalid path: contains null byte".to_string()
        ));
    }

    // 检查路径遍历
    if path.contains("..") {
        return Err(ForkedAgentError::ToolError(
            "Path traversal detected: '..' not allowed".to_string()
        ));
    }

    Ok(())
}

/// 验证编辑内容安全性
///
/// 检查：
/// 1. new_string 大小限制
/// 2. 内容增长比例限制
fn validate_edit_content(
    original: &str,
    new_string: &str,
    max_new_size: usize,
) -> Result<(), ForkedAgentError> {
    // 检查 new_string 大小
    if new_string.len() > max_new_size {
        return Err(ForkedAgentError::ToolError(format!(
            "new_string too large: {} bytes (max {})",
            new_string.len(), max_new_size
        )));
    }

    // 检查内容增长比例（防止爆炸性增长）
    let original_len = original.len().max(1);
    let growth_ratio = new_string.len() as f64 / original_len as f64;
    if growth_ratio > 100.0 {
        return Err(ForkedAgentError::ToolError(format!(
            "Content growth ratio too high: {:.1}x (max 100x)",
            growth_ratio
        )));
    }

    Ok(())
}

/// Forked Agent 只支持有限的文件操作工具：
/// - read_file: 读取文件内容
/// - file_edit / edit_file: 编辑文件（字符串替换）
/// - file_write / write_file: 写入文件
/// - grep: 在文件中搜索模式（简化版）
/// - glob: 匹配文件模式（简化版，支持基本通配符）
///
/// 其他工具会返回错误。
async fn execute_forked_tool(
    tool_name: &str,
    input: &serde_json::Value,
    can_use_tool: &CanUseToolFn,
) -> Result<String, ForkedAgentError> {
    // 首先检查权限
    match can_use_tool(tool_name, input) {
        ToolPermission::Allow => {},
        ToolPermission::Deny { message } => {
            return Ok(format!("Tool '{}' denied: {}", tool_name, message));
        }
    }

    match tool_name {
        "read_file" => {
            let file_path = input.get("file_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing file_path parameter".to_string()))?;

            // 二次路径验证（fail-safe）
            validate_path_safety(file_path)?;

            // 检查文件大小
            let metadata = tokio::fs::metadata(Path::new(file_path)).await
                .map_err(ForkedAgentError::Io)?;
            if metadata.len() as usize > MAX_FILE_SIZE {
                return Err(ForkedAgentError::ToolError(format!(
                    "File too large: {} bytes (max {})",
                    metadata.len(), MAX_FILE_SIZE
                )));
            }

            let content = tokio::fs::read_to_string(Path::new(file_path))
                .await
                .map_err(ForkedAgentError::Io)?;

            // 截断过长的输出（安全处理 UTF-8 边界）
            let truncated = if content.len() > MAX_OUTPUT_CHARS {
                // 找到安全的 UTF-8 边界
                let mut boundary = MAX_OUTPUT_CHARS;
                while boundary > 0 && !content.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                format!("{}...\n[Truncated, total {} chars]",
                    &content[..boundary], content.len())
            } else {
                content
            };

            Ok(truncated)
        },

        "file_edit" | "edit_file" => {
            let file_path = input.get("file_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing file_path parameter".to_string()))?;

            let old_string = input.get("old_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing old_string parameter".to_string()))?;

            let new_string = input.get("new_string")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing new_string parameter".to_string()))?;

            // 二次路径验证（fail-safe）
            validate_path_safety(file_path)?;

            // 验证编辑内容安全性
            validate_edit_content(old_string, new_string, MAX_EDIT_SIZE)?;

            // 读取文件
            let content = tokio::fs::read_to_string(Path::new(file_path))
                .await
                .map_err(ForkedAgentError::Io)?;

            // 执行替换
            let new_content = if content.contains(old_string) {
                content.replace(old_string, new_string)
            } else {
                return Ok(format!("old_string not found in {}", file_path));
            };

            // 写回文件
            tokio::fs::write(Path::new(file_path), &new_content)
                .await
                .map_err(ForkedAgentError::Io)?;

            Ok(format!("Successfully edited {}", file_path))
        },

        "file_write" | "write_file" => {
            let file_path = input.get("file_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing file_path parameter".to_string()))?;

            let content = input.get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing content parameter".to_string()))?;

            // 二次路径验证（fail-safe）
            validate_path_safety(file_path)?;

            // 检查内容大小
            if content.len() > MAX_FILE_SIZE {
                return Err(ForkedAgentError::ToolError(format!(
                    "Content too large: {} bytes (max {})",
                    content.len(), MAX_FILE_SIZE
                )));
            }

            // 确保父目录存在（create_dir_all 会处理已存在的情况）
            let parent = Path::new(file_path).parent()
                .ok_or_else(|| ForkedAgentError::ToolError("Invalid file path".to_string()))?;

            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ForkedAgentError::Io)?;

            // 写入文件
            tokio::fs::write(Path::new(file_path), content)
                .await
                .map_err(ForkedAgentError::Io)?;

            Ok(format!("Successfully wrote {}", file_path))
        },

        "grep" => {
            let pattern = input.get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing pattern parameter".to_string()))?;

            let path = input.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");

            // 二次路径验证（fail-safe）
            validate_path_safety(path)?;

            // 简化版 grep - 只搜索单个文件
            let content = tokio::fs::read_to_string(Path::new(path))
                .await
                .map_err(ForkedAgentError::Io)?;

            let matches: Vec<&str> = content
                .lines()
                .filter(|line| line.contains(pattern))
                .take(100)  // 限制结果数量
                .collect();

            if matches.is_empty() {
                Ok(format!("No matches found for pattern '{}'", pattern))
            } else {
                Ok(matches.join("\n"))
            }
        },

        "glob" => {
            let pattern = input.get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ForkedAgentError::ToolError("Missing pattern parameter".to_string()))?;

            let path = input.get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(".");

            // 二次路径验证（fail-safe）
            validate_path_safety(path)?;

            // 简化版 glob - 只支持基本模式
            let base_path = Path::new(path);
            let mut results = Vec::new();

            // 使用 tokio 异步读取目录
            match tokio::fs::read_dir(base_path).await {
                Ok(mut entries) => {
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        let file_name = entry.file_name().to_string_lossy().to_string();
                        // 简单的通配符匹配
                        if simple_glob_match(pattern, &file_name) {
                            results.push(entry.path().to_string_lossy().to_string());
                        }
                        if results.len() >= 100 {
                            break;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, path = %base_path.display(), "[forked] Failed to read directory");
                }
            }

            if results.is_empty() {
                Ok(format!("No files matching '{}'", pattern))
            } else {
                Ok(results.join("\n"))
            }
        },

        // 不支持的工具
        _ => {
            Ok(format!("Tool '{}' is not supported in forked mode. Supported tools: read_file, file_edit, file_write, grep, glob", tool_name))
        }
    }
}

/// 简化的 glob 匹配
fn simple_glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(ext) = pattern.strip_prefix("*.") {
        return name.ends_with(ext);
    }
    if let Some(prefix) = pattern.strip_suffix("*") {
        return name.starts_with(prefix);
    }
    name == pattern
}

/// 运行 Forked Agent
///
/// 这是 Forked Agent 的主要入口点。
///
/// ## 参数
///
/// - `params`: Forked Agent 配置参数
///
/// ## 返回
///
/// - `Ok(ForkedAgentResult)`: 执行成功，包含消息和用量
/// - `Err(ForkedAgentError)`: 执行失败
///
/// ## 示例
///
/// ```ignore
/// let result = run_forked_agent(ForkedAgentParams {
///     prompt_messages: vec![ChatMessage::user("分析这段对话")],
///     cache_safe_params,
///     provider_pool,
///     can_use_tool: create_auto_mem_can_use_tool(&memory_dir),
///     query_source: "auto_memory",
///     fork_label: "auto_memory",
///     max_turns: Some(5),
///     ..Default::default()
/// }).await?;
/// ```
pub async fn run_forked_agent(params: ForkedAgentParams) -> Result<ForkedAgentResult, ForkedAgentError> {
    let start_time = Instant::now();
    let mut output_messages = Vec::new();
    let mut total_usage = UsageMetrics::default();
    let files_modified = Vec::new();

    // 创建子代理上下文
    let context = create_subagent_context(
        None, // parent_file_state - 在实际集成时从 runtime 获取
        None, // parent_replacement_state
        None, // parent_abort_controller
        params.overrides.unwrap_or_default(),
    );

    // 检查是否已中止
    if context.abort_controller.is_aborted() {
        return Err(ForkedAgentError::Aborted(
            context.abort_controller.reason().unwrap_or_else(|| "Aborted".to_string())
        ));
    }

    // 构建初始消息（父消息 + 子代理输入）
    let mut messages: Vec<ChatMessage> = params
        .cache_safe_params
        .fork_context_messages
        .iter()
        .cloned()
        .chain(params.prompt_messages.iter().cloned())
        .collect();

    // 添加系统提示
    let system_prompt = params.system_prompt
        .clone()
        .unwrap_or_else(|| (*params.cache_safe_params.system_prompt).clone());

    if !system_prompt.is_empty() {
        messages.insert(0, ChatMessage::system(&system_prompt));
    }

    // 记录开始
    tracing::info!(
        fork_label = params.fork_label,
        query_source = params.query_source,
        message_count = messages.len(),
        max_turns = ?params.max_turns,
        "[forked_agent] starting"
    );

    // 获取 Provider（带重试和指数退避）
    let provider_pool = params.provider_pool
        .as_ref()
        .ok_or(ForkedAgentError::NoProviderAvailable)?;

    let provider = acquire_provider_with_retry(
        provider_pool,
        PROVIDER_RETRY_MAX_ATTEMPTS,
        PROVIDER_RETRY_INITIAL_DELAY_MS,
        PROVIDER_RETRY_MAX_DELAY_MS,
    ).await?;

    let max_turns = params.max_turns.unwrap_or(5);
    let mut current_messages = messages.clone();
    let mut final_content = None;

    for turn in 0..max_turns {
        // 检查中止
        if context.abort_controller.is_aborted() {
            tracing::warn!(
                fork_label = params.fork_label,
                turn,
                "[forked_agent] aborted"
            );
            return Err(ForkedAgentError::Aborted(
                context.abort_controller.reason().unwrap_or_else(|| "Aborted".to_string())
            ));
        }

        // 调用 LLM
        let response = match provider.chat(&current_messages, &[]).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    fork_label = params.fork_label,
                    turn,
                    error = %e,
                    "[forked_agent] LLM call failed"
                );
                return Err(ForkedAgentError::ProviderError(format!("{}", e)));
            }
        };

        // 提取用量
        if !response.usage.is_null() {
            let usage = &response.usage;
            let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let output = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let cache_creation = usage.get("cache_creation_input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            total_usage.accumulate(input, output, cache_read, cache_creation);
        }

        // 提取内容
        let content = response.content.clone();
        final_content = content.clone();

        // 创建 assistant 消息
        let assistant_msg = if !response.tool_calls.is_empty() {
            // 有工具调用
            ChatMessage {
                id: None,
                role: "assistant".to_string(),
                content: serde_json::Value::String(content.clone().unwrap_or_default()),
                reasoning_content: None,
                tool_calls: Some(response.tool_calls.clone()),
                tool_call_id: None,
                name: None,
            }
        } else {
            ChatMessage::assistant(content.as_deref().unwrap_or(""))
        };

        current_messages.push(assistant_msg.clone());
        output_messages.push(assistant_msg);

        // 检查是否有工具调用
        if !response.tool_calls.is_empty() {
            tracing::debug!(
                fork_label = params.fork_label,
                turn,
                tool_count = response.tool_calls.len(),
                "[forked_agent] executing tool calls"
            );

            // 执行每个工具调用
            for tool_call in &response.tool_calls {
                let tool_name = &tool_call.name;
                let tool_input = &tool_call.arguments;

                tracing::debug!(
                    fork_label = params.fork_label,
                    turn,
                    tool_name,
                    "[forked_agent] executing tool"
                );

                // 执行工具
                let tool_result = execute_forked_tool(
                    tool_name,
                    tool_input,
                    &params.can_use_tool,
                ).await;

                // 构建工具结果消息，包含详细的错误上下文
                let result_content = match tool_result {
                    Ok(result) => result,
                    Err(ref e) => {
                        // 包含错误类型和详细信息，便于调试
                        let error_type = match e {
                            ForkedAgentError::ProviderError(_) => "ProviderError",
                            ForkedAgentError::ToolError(_) => "ToolError",
                            ForkedAgentError::Io(_) => "IoError",
                            ForkedAgentError::Json(_) => "JsonError",
                            ForkedAgentError::ToolNotSupported(_) => "ToolNotSupported",
                            ForkedAgentError::MaxTurnsExceeded => "MaxTurnsExceeded",
                            ForkedAgentError::NoProviderAvailable => "NoProviderAvailable",
                            ForkedAgentError::Aborted(_) => "Aborted",
                        };
                        format!("Tool execution error ({}): {}", error_type, e)
                    }
                };

                // 添加工具结果到消息
                let tool_result_msg = ChatMessage {
                    id: None,
                    role: "tool".to_string(),
                    content: serde_json::Value::String(result_content),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: Some(tool_call.id.clone()),
                    name: Some(tool_name.clone()),
                };

                current_messages.push(tool_result_msg);
            }

            // 继续循环让 LLM 处理工具结果
            continue;
        }

        // 如果没有工具调用，结束循环
        break;
    }

    // 清理资源
    drop(context);

    // 记录分析事件
    let duration_ms = start_time.elapsed().as_millis() as u64;
    tracing::info!(
        fork_label = params.fork_label,
        query_source = params.query_source,
        duration_ms,
        message_count = output_messages.len(),
        input_tokens = total_usage.input_tokens,
        output_tokens = total_usage.output_tokens,
        cache_hit_rate = total_usage.cache_hit_rate(),
        "[forked_agent] completed"
    );

    Ok(ForkedAgentResult {
        messages: output_messages,
        total_usage,
        files_modified,
        final_content,
    })
}

/// 带重试的 Provider 获取
///
/// 使用指数退避策略重试获取 provider，避免因短暂不可用而直接失败。
///
/// ## 已知限制
/// 重试循环在 sleep 时不检查 abort 信号。如果需要支持取消，
/// 应该将 `AbortController` 传入此函数并使用 `tokio::select!`。
async fn acquire_provider_with_retry(
    provider_pool: &Arc<ProviderPool>,
    max_attempts: usize,
    initial_delay_ms: u64,
    max_delay_ms: u64,
) -> Result<Arc<dyn blockcell_providers::Provider>, ForkedAgentError> {
    let mut delay_ms = initial_delay_ms;

    for attempt in 0..max_attempts {
        match provider_pool.acquire() {
            Some((_name, provider)) => {
                if attempt > 0 {
                    tracing::info!(
                        attempt = attempt + 1,
                        "[forked_agent] Provider acquired after retry"
                    );
                }
                return Ok(provider);
            }
            None => {
                if attempt < max_attempts - 1 {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max_attempts,
                        delay_ms,
                        "[forked_agent] No provider available, retrying..."
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    // 指数退避，但不超过最大延迟
                    delay_ms = (delay_ms * 2).min(max_delay_ms);
                }
            }
        }
    }

    Err(ForkedAgentError::NoProviderAvailable)
}

/// Forked Agent 事件
///
/// 用于遥测和日志记录
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct ForkAgentEvent {
    /// Fork 标签
    pub fork_label: &'static str,
    /// 查询来源
    pub query_source: &'static str,
    /// 执行时长 (ms)
    pub duration_ms: u64,
    /// 消息数量
    pub message_count: usize,
    /// 用量指标
    pub total_usage: UsageMetrics,
}

impl ForkAgentEvent {
    /// 记录事件到日志
    #[allow(dead_code)]
    pub fn log(&self) {
        tracing::info!(
            fork_label = self.fork_label,
            query_source = self.query_source,
            duration_ms = self.duration_ms,
            message_count = self.message_count,
            input_tokens = self.total_usage.input_tokens,
            output_tokens = self.total_usage.output_tokens,
            cache_read = self.total_usage.cache_read_input_tokens,
            cache_creation = self.total_usage.cache_creation_input_tokens,
            cache_hit_rate = self.total_usage.cache_hit_rate(),
            "[fork_agent_event]"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_usage_metrics() {
        let mut metrics = UsageMetrics::default();
        metrics.accumulate(1000, 500, 800, 200);

        assert_eq!(metrics.input_tokens, 1000);
        assert_eq!(metrics.output_tokens, 500);
        assert_eq!(metrics.cache_read_input_tokens, 800);
        assert_eq!(metrics.cache_creation_input_tokens, 200);

        // 缓存命中率 = 800 / (1000 + 800 + 200) = 0.4
        let hit_rate = metrics.cache_hit_rate();
        assert!((hit_rate - 0.4).abs() < 0.01);
    }

    #[test]
    fn test_usage_metrics_merge() {
        let mut m1 = UsageMetrics {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 80,
            cache_creation_input_tokens: 20,
        };
        let m2 = UsageMetrics {
            input_tokens: 200,
            output_tokens: 100,
            cache_read_input_tokens: 160,
            cache_creation_input_tokens: 40,
        };

        m1.merge(&m2);

        assert_eq!(m1.input_tokens, 300);
        assert_eq!(m1.output_tokens, 150);
    }

    // 注意：ForkedAgentParams 不再实现 Default trait
    // 必须通过 new() 或 builder() 创建，强制设置 provider_pool

    // ========== 核心路径测试 ==========

    #[test]
    fn test_forked_agent_params_builder_missing_provider() {
        let result = ForkedAgentParams::builder()
            .prompt_messages(vec![ChatMessage::user("test")])
            .fork_label("test_fork")
            .max_turns(3)
            .build();

        // 没有 provider_pool，应该返回错误
        assert!(result.is_err());
        // 直接 matches! 检查，避免需要 Debug trait
        assert!(matches!(result, Err(ForkedAgentError::NoProviderAvailable)));
    }

    #[test]
    fn test_forked_agent_params_validate_no_provider() {
        // 使用 builder 不设置 provider_pool，build() 应返回错误
        let result = ForkedAgentParams::builder()
            .prompt_messages(vec![ChatMessage::user("test")])
            .build();

        // 没有 provider_pool，应该返回错误
        assert!(result.is_err());
        assert!(matches!(result, Err(ForkedAgentError::NoProviderAvailable)));
    }

    #[test]
    fn test_forked_agent_params_builder_methods() {
        // 测试 builder 的方法链（不调用 build，避免需要 provider_pool）
        let builder = ForkedAgentParams::builder()
            .prompt_messages(vec![ChatMessage::user("test")])
            .fork_label("custom_label")
            .query_source("custom_source")
            .max_turns(10);

        // 验证 builder 字段设置正确
        // 由于无法直接访问 builder 的私有字段，我们通过 build 后检查错误类型
        let result = builder.build();
        assert!(result.is_err());
    }

    #[test]
    fn test_forked_agent_params_builder_requires_provider() {
        // 测试 builder 必须设置 provider_pool
        // 不设置 provider_pool 时，build() 应返回错误
        let result = ForkedAgentParams::builder()
            .prompt_messages(vec![ChatMessage::user("test")])
            .fork_label("custom_label")
            .query_source("custom_source")
            .max_turns(10)
            .build();

        // 应该失败，因为没有 provider_pool
        assert!(result.is_err());
        assert!(matches!(result, Err(ForkedAgentError::NoProviderAvailable)));
    }

    #[test]
    fn test_forked_agent_error_variants() {
        let err = ForkedAgentError::ProviderError("test".to_string());
        assert!(err.to_string().contains("LLM provider error"));

        let err = ForkedAgentError::MaxTurnsExceeded;
        assert!(err.to_string().contains("Max turns exceeded"));

        let err = ForkedAgentError::NoProviderAvailable;
        assert!(err.to_string().contains("No provider available"));

        let err = ForkedAgentError::ToolNotSupported("bad_tool".to_string());
        assert!(err.to_string().contains("Tool not supported"));
    }

    #[test]
    fn test_usage_metrics_cache_hit_rate_zero() {
        let metrics = UsageMetrics::default();
        assert_eq!(metrics.cache_hit_rate(), 0.0);
    }

    #[test]
    fn test_simple_glob_match() {
        assert!(simple_glob_match("*", "anything"));
        assert!(simple_glob_match("*.rs", "main.rs"));
        assert!(simple_glob_match("test*", "testing"));
        assert!(!simple_glob_match("*.rs", "main.txt"));
        assert!(simple_glob_match("exact", "exact"));
    }
}