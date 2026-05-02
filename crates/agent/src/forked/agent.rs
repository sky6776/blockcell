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

use super::{
    create_subagent_context, CacheSafeParams, CanUseToolFn, SubagentOverrides, ToolPermission,
};
use crate::memory_event;
#[allow(deprecated)]
use crate::skill_mutex::SkillMutex;
use blockcell_core::types::ChatMessage;
use blockcell_providers::ProviderPool;
use blockcell_tools::fuzzy_match::fuzzy_find_and_replace;
use blockcell_tools::security_scan::{scan_skill_content, scan_skill_dir_with_trust};
use blockcell_tools::skill_manage::{atomic_write_text, extract_frontmatter};
use blockcell_tools::MemoryStoreHandle;
use regex::Regex;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

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
#[allow(deprecated)]
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
    /// Memory store handle (shared from parent agent via Arc)
    pub memory_store: Option<MemoryStoreHandle>,
    /// Skills directory (for skill_manage/list_skills in review mode)
    pub skills_dir: Option<PathBuf>,
    /// External skills directories (builtin_skills_dir etc., for skill search)
    pub external_skills_dirs: Vec<PathBuf>,
    /// Skill mutex (shared with parent agent to prevent concurrent skill modifications)
    pub skill_mutex: Option<Arc<SkillMutex>>,
    /// 工具 JSON Schema 列表 (传给 provider.chat() 让 LLM 知道可用工具)
    pub tool_schemas: Vec<serde_json::Value>,
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
            memory_store: None,
            skills_dir: None,
            external_skills_dirs: Vec::new(),
            skill_mutex: None,
            tool_schemas: Vec::new(),
        }
    }

    /// 设置 memory_store（共享父代理的 Memory Store）
    pub fn with_memory_store(mut self, store: MemoryStoreHandle) -> Self {
        self.memory_store = Some(store);
        self
    }

    /// 设置 skills_dir（用于 skill_manage/list_skills 工具）
    pub fn with_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skills_dir = Some(dir);
        self
    }

    /// 设置 external_skills_dirs（用于跨目录搜索 Skill, 如 builtin_skills_dir）
    pub fn with_external_skills_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.external_skills_dirs = dirs;
        self
    }

    /// 设置 skill_mutex（共享父代理的 SkillMutex，防止并发修改）
    #[allow(deprecated)]
    pub fn with_skill_mutex(mut self, mutex: Arc<SkillMutex>) -> Self {
        self.skill_mutex = Some(mutex);
        self
    }

    /// 设置工具 schema 列表（传给 provider.chat() 让 LLM 知道可用工具）
    pub fn with_tool_schemas(mut self, schemas: Vec<serde_json::Value>) -> Self {
        self.tool_schemas = schemas;
        self
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
#[allow(deprecated)]
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
    memory_store: Option<MemoryStoreHandle>,
    skills_dir: Option<PathBuf>,
    external_skills_dirs: Vec<PathBuf>,
    skill_mutex: Option<Arc<SkillMutex>>,
    tool_schemas: Vec<serde_json::Value>,
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

    /// 设置 memory_store（共享父代理的 Memory Store）
    pub fn memory_store(mut self, store: MemoryStoreHandle) -> Self {
        self.memory_store = Some(store);
        self
    }

    /// 设置工具 schema 列表
    pub fn tool_schemas(mut self, schemas: Vec<serde_json::Value>) -> Self {
        self.tool_schemas = schemas;
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
            can_use_tool: self
                .can_use_tool
                .unwrap_or_else(|| Arc::new(|_, _| ToolPermission::Allow)),
            query_source: self.query_source,
            fork_label: self.fork_label,
            overrides: self.overrides,
            max_output_tokens: self.max_output_tokens,
            max_turns: self.max_turns,
            skip_transcript: self.skip_transcript,
            skip_cache_write: self.skip_cache_write,
            system_prompt: self.system_prompt,
            memory_store: self.memory_store,
            skills_dir: self.skills_dir,
            external_skills_dirs: self.external_skills_dirs,
            skill_mutex: self.skill_mutex,
            tool_schemas: self.tool_schemas,
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
        let total =
            self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens;
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
const MAX_SKILL_CONTENT_CHARS: usize = 100_000; // 100K chars for skill content (与主工具一致)
const MAX_OUTPUT_CHARS: usize = 50000;

/// Skill 名称正则 (与主 skill_manage 工具一致): 小写字母、数字、点、下划线、连字符
static VALID_SKILL_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new("^[a-z0-9][a-z0-9._-]*$").expect("VALID_SKILL_NAME_RE"));

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
            "Invalid path: contains null byte".to_string(),
        ));
    }

    // 检查路径遍历
    if path.contains("..") {
        return Err(ForkedAgentError::ToolError(
            "Path traversal detected: '..' not allowed".to_string(),
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
            new_string.len(),
            max_new_size
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

/// 在 skills_dir + external_skills_dirs 中查找 Skill 目录 (与主工具 find_skill_dir 对齐)
///
/// 搜索顺序: skills_dir/{name} → skills_dir/{category}/{name} → 各 external_dir 同理
fn find_skill_dir_forked(
    name: &str,
    category: Option<&str>,
    skills_dir: &Path,
    external_dirs: &[PathBuf],
) -> Option<PathBuf> {
    // 构建搜索目录列表 (主目录优先)
    let mut search_dirs: Vec<PathBuf> = vec![skills_dir.to_path_buf()];
    for dir in external_dirs {
        if dir != skills_dir && dir.exists() {
            search_dirs.push(dir.clone());
        }
    }

    for dir in &search_dirs {
        // 如果指定了 category, 先尝试 {dir}/{category}/{name}
        if let Some(cat) = category {
            let candidate = dir.join(cat).join(name);
            if candidate.is_dir() && candidate.join("SKILL.md").exists() {
                return Some(candidate);
            }
        }

        // 尝试直接匹配 {dir}/{name}
        let direct = dir.join(name);
        if direct.is_dir() && direct.join("SKILL.md").exists() {
            return Some(direct);
        }

        // 遍历 category 子目录查找
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let candidate = path.join(name);
                    if candidate.is_dir() && candidate.join("SKILL.md").exists() {
                        return Some(candidate);
                    }
                }
            }
        }
    }

    None
}

/// Forked Agent 支持的工具：
/// - read_file: 读取文件内容
/// - file_edit / edit_file: 编辑文件（字符串替换）
/// - file_write / write_file: 写入文件
/// - grep: 在文件中搜索模式（简化版）
/// - glob: 匹配文件模式（简化版，支持基本通配符）
/// - skill_manage: 技能管理（create/edit/patch/view/delete/write_file/remove_file）
/// - memory_upsert: 写入/更新记忆项（需要 memory_store）
/// - memory_query: 查询记忆项（需要 memory_store）
/// - memory_forget: 删除记忆项（需要 memory_store）
///
/// 其他工具会返回错误。
#[allow(deprecated, clippy::too_many_arguments)]
async fn execute_forked_tool(
    tool_name: &str,
    input: &serde_json::Value,
    can_use_tool: &CanUseToolFn,
    memory_store: &Option<MemoryStoreHandle>,
    skills_dir: &Option<PathBuf>,
    external_skills_dirs: &[PathBuf],
    skill_mutex: &Option<Arc<SkillMutex>>,
) -> Result<String, ForkedAgentError> {
    // 首先检查权限
    match can_use_tool(tool_name, input) {
        ToolPermission::Allow => {}
        ToolPermission::Deny { message } => {
            // 记录 Layer 7 tool_denied 事件
            crate::memory_event!(layer7, tool_denied, tool_name, &message);
            return Ok(format!("Tool '{}' denied: {}", tool_name, message));
        }
    }

    // SkillMutex 检查: 写入操作前检查是否已有并发修改
    // 注意: _skill_guard 必须在整个 match 块中存活, 才能保护操作期间不被并发修改
    let _skill_guard = if tool_name == "skill_manage" {
        let is_write_action = matches!(
            input.get("action").and_then(|v| v.as_str()).unwrap_or(""),
            "create" | "patch" | "edit" | "delete" | "write_file" | "remove_file"
        );
        if is_write_action {
            if let Some(name) = input.get("name").and_then(|v| v.as_str()) {
                if let Some(ref mutex) = skill_mutex {
                    if !mutex.can_modify(name) {
                        return Ok(json!({
                            "success": false,
                            "message": format!("Skill '{}' is currently being modified by another process. Please try again later.", name)
                        }).to_string());
                    }
                    // 获取锁（guard 存活到函数结束, 操作完成后自动释放）
                    match mutex.acquire(name) {
                        Ok(guard) => Some(guard),
                        Err(e) => {
                            tracing::warn!(skill = %name, error = %e, "SkillMutex acquire failed, rejecting write");
                            return Ok(json!({
                                "success": false,
                                "message": format!("Skill '{}' is currently being modified. Please try again later.", name)
                            }).to_string());
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

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

            // 执行替换（默认只替换第一个匹配，与主 edit_file 工具一致）
            let new_content = if content.contains(old_string) {
                let replace_all = input.get("replace_all")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if replace_all {
                    content.replace(old_string, new_string)
                } else {
                    // 仅替换第一个匹配
                    match content.find(old_string) {
                        Some(pos) => {
                            let mut result = String::with_capacity(content.len() - old_string.len() + new_string.len());
                            result.push_str(&content[..pos]);
                            result.push_str(new_string);
                            result.push_str(&content[pos + old_string.len()..]);
                            result
                        }
                        None => content.clone(),
                    }
                }
            } else {
                return Ok(format!("old_string not found in {}", file_path));
            };

            // 原子写回文件 (temp file + rename, 防止崩溃时损坏)
            atomic_write_text(Path::new(file_path), &new_content)
                .await
                .map_err(|e| ForkedAgentError::ToolError(format!("Failed to write file: {}", e)))?;

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

            // 原子写入文件 (temp file + rename, 防止崩溃时损坏)
            atomic_write_text(Path::new(file_path), content)
                .await
                .map_err(|e| ForkedAgentError::ToolError(format!("Failed to write file: {}", e)))?;

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

        // 记忆工具: memory_upsert
        "memory_upsert" => {
            match memory_store {
                Some(store) => {
                    let result = store.upsert_json(input.clone())
                        .map_err(|e| ForkedAgentError::ToolError(format!("memory_upsert error: {}", e)))?;
                    Ok(serde_json::to_string(&result)
                        .unwrap_or_else(|_| "memory_upsert completed".to_string()))
                }
                None => Ok("Memory store not available".to_string()),
            }
        },

        // 记忆工具: memory_query / memory_search
        "memory_query" | "memory_search" => {
            match memory_store {
                Some(store) => {
                    let result = store.query_json(input.clone())
                        .map_err(|e| ForkedAgentError::ToolError(format!("memory_query error: {}", e)))?;
                    Ok(serde_json::to_string(&result)
                        .unwrap_or_else(|_| "memory_query completed".to_string()))
                }
                None => Ok("Memory store not available".to_string()),
            }
        },

        // 记忆工具: memory_forget
        "memory_forget" => {
            match memory_store {
                Some(store) => {
                    // memory_forget 支持两种模式: 按 id 或按 filter
                    if let Some(id) = input.get("id").and_then(|v| v.as_str()) {
                        let success = store.soft_delete(id)
                            .map_err(|e| ForkedAgentError::ToolError(format!("memory_forget error: {}", e)))?;
                        Ok(if success { format!("Memory item '{}' forgotten", id) } else { format!("Memory item '{}' not found", id) })
                    } else {
                        // 按 filter 批量删除
                        let count = store.batch_soft_delete_json(input.clone())
                            .map_err(|e| ForkedAgentError::ToolError(format!("memory_forget error: {}", e)))?;
                        Ok(format!("{} memory items forgotten", count))
                    }
                }
                None => Ok("Memory store not available".to_string()),
            }
        },

        // Skill 工具: list_skills
        // 支持 category 子目录结构: {skills_dir}/{category}/{name}/
        "list_skills" => {
            match &skills_dir {
                Some(dir) => {
                    let query = input.get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if !dir.exists() {
                        return Ok(json!({"skills": [], "count": 0}).to_string());
                    }

                    let mut entries = Vec::new();
                    if let Ok(read_dir) = std::fs::read_dir(dir) {
                        for entry in read_dir.flatten() {
                            if let Ok(file_type) = entry.file_type() {
                                if file_type.is_dir() {
                                    let entry_name = entry.file_name().to_string_lossy().to_string();
                                    // 检查是否是 category 目录 (包含子目录) 或直接是 skill 目录 (包含 SKILL.md)
                                    let has_skill_md = entry.path().join("SKILL.md").exists();
                                    if has_skill_md {
                                        // 直接是 skill 目录 (无 category)
                                        if query.is_empty() || entry_name.to_lowercase().contains(&query.to_lowercase()) {
                                            entries.push(json!({
                                                "name": entry_name,
                                                "has_skill_md": true,
                                            }));
                                        }
                                    } else {
                                        // 可能是 category 目录，搜索其下的 skill 子目录
                                        if let Ok(sub_entries) = std::fs::read_dir(entry.path()) {
                                            for sub_entry in sub_entries.flatten() {
                                                if sub_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                                    let skill_name = sub_entry.file_name().to_string_lossy().to_string();
                                                    let has_md = sub_entry.path().join("SKILL.md").exists();
                                                    if has_md && (query.is_empty() || skill_name.to_lowercase().contains(&query.to_lowercase())) {
                                                        entries.push(json!({
                                                            "name": skill_name,
                                                            "category": entry_name,
                                                            "has_skill_md": true,
                                                        }));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    if entries.is_empty() {
                        Ok(json!({"skills": [], "count": 0, "message": "No skills found"}).to_string())
                    } else {
                        let count = entries.len();
                        Ok(json!({"skills": entries, "count": count}).to_string())
                    }
                }
                None => Ok(json!({"skills": [], "count": 0, "message": "Skills directory not available"}).to_string()),
            }
        },

        // Skill 工具: skill_manage
        // 与主 skill_manage 工具 (crates/tools/src/skill_manage.rs) 保持一致:
        // - 返回 JSON 格式 {"success": true, "message": "..."} 供 extract_review_summary 解析
        // - patch 使用 fuzzy_match 9-strategy 模糊匹配
        // - create/edit/write_file 执行 security_scan 安全扫描
        // - create 验证 YAML frontmatter (name + description)
        // - 支持 category 参数
        "skill_manage" => {
            match &skills_dir {
                Some(dir) => {
                    let action = input.get("action")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let name = input.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let category = input.get("category")
                        .and_then(|v| v.as_str());

                    if name.is_empty() {
                        return Ok(json!({"success": false, "message": "skill_manage: 'name' parameter is required"}).to_string());
                    }

                    // 验证 skill 名称安全性 (路径遍历 + 正则格式)
                    if name.contains("..") || name.contains('/') || name.contains('\\') || name.contains('\0') {
                        return Ok(json!({"success": false, "message": format!("skill_manage: invalid skill name '{}'", name)}).to_string());
                    }
                    if !VALID_SKILL_NAME_RE.is_match(name) {
                        return Ok(json!({"success": false, "message": format!("skill_manage: invalid skill name '{}' (must match pattern: lowercase letters, digits, dots, underscores, hyphens, starting with letter or digit)", name)}).to_string());
                    }

                    // 支持 category 子目录 (与主工具一致: {skills_dir}/{category}/{name}/)
                    let skill_dir = if let Some(cat) = category {
                        if cat.contains("..") || cat.contains('/') || cat.contains('\\') || cat.contains('\0') {
                            return Ok(json!({"success": false, "message": format!("skill_manage: invalid category '{}'", cat)}).to_string());
                        }
                        dir.join(cat).join(name)
                    } else {
                        dir.join(name)
                    };

                    match action {
                        "view" => {
                            // 使用 find_skill_dir_forked 跨目錄搜索 (與主工具 find_skill_dir 對齊)
                            if let Some(found_dir) = find_skill_dir_forked(name, category, dir, external_skills_dirs) {
                                // 推斷 category: 如果 found_dir 的 parent != skills_dir, 則 parent name 為 category
                                let inferred_cat = if let Some(parent) = found_dir.parent() {
                                    if parent != dir {
                                        parent.file_name().map(|n| n.to_string_lossy().to_string())
                                    } else { None }
                                } else { None };
                                build_view_response_for_skill(&found_dir, name, inferred_cat.as_deref().or(category)).await
                            } else {
                                Ok(json!({"success": false, "message": format!("Skill '{}' not found (no SKILL.md)", name)}).to_string())
                            }
                        }
                        "create" => {
                            let content = input.get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            if content.is_empty() {
                                return Ok(json!({"success": false, "message": "skill_manage create: 'content' parameter is required"}).to_string());
                            }

                            // 安全检查：内容大小限制 (与主工具一致, 使用字节数)
                            if content.len() > MAX_SKILL_CONTENT_CHARS {
                                return Ok(json!({"success": false, "message": format!("skill_manage create: content too large ({} bytes, max {})", content.len(), MAX_SKILL_CONTENT_CHARS)}).to_string());
                            }

                            // Frontmatter 验证: 检查 YAML frontmatter 包含 name 和 description
                            let frontmatter_issues = validate_skill_frontmatter(content);
                            if !frontmatter_issues.is_empty() {
                                return Ok(json!({
                                    "success": false,
                                    "message": format!("Frontmatter validation failed: {}", frontmatter_issues.join("; ")),
                                }).to_string());
                            }

                            // 安全扫描
                            let scan_result = scan_skill_content(content);
                            if !scan_result.passed {
                                return Ok(json!({
                                    "success": false,
                                    "message": format!("Security scan failed: {}",
                                        scan_result.issues.iter()
                                            .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Critical))
                                            .map(|i| i.message.as_str())
                                            .collect::<Vec<_>>()
                                            .join("; ")),
                                }).to_string());
                            }

                            // 创建 skill 目录 — 先检查是否已存在
                            if skill_dir.exists() {
                                return Ok(json!({"success": false, "message": format!("Skill '{}' already exists. Use patch to modify it.", name)}).to_string());
                            }
                            tokio::fs::create_dir_all(&skill_dir).await
                                .map_err(ForkedAgentError::Io)?;

                            // 原子写入 SKILL.md (temp file + rename, 防止崩溃时损坏)
                            let skill_md_path = skill_dir.join("SKILL.md");
                            if let Err(e) = atomic_write_text(&skill_md_path, content).await {
                                // 写入失败: 回滚删除整个目录 (与主工具一致)
                                let _ = tokio::fs::remove_dir_all(&skill_dir).await;
                                return Err(ForkedAgentError::ToolError(format!("Failed to write SKILL.md: {}", e)));
                            }

                            // 生成 meta.json (从 frontmatter 提取元数据)
                            let meta = extract_frontmatter(content);
                            let meta_path = skill_dir.join("meta.json");
                            let meta_json = serde_json::to_string_pretty(&meta)
                                .unwrap_or_else(|_| "{}".to_string());
                            if let Err(e) = atomic_write_text(&meta_path, &meta_json).await {
                                // meta.json 写入失败不影响 Skill 创建, 仅记录警告
                                tracing::warn!(error = %e, "[forked] Failed to write meta.json for skill '{}'", name);
                            }

                            Ok(json!({
                                "success": true,
                                "message": if let Some(cat) = category {
                                    format!("Skill '{}' created in category '{}'", name, cat)
                                } else {
                                    format!("Skill '{}' created", name)
                                },
                                "hint": "Use action='write_file' to add reference files, templates, or scripts to this skill.",
                                "warnings": scan_result.issues.iter()
                                    .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Warning))
                                    .map(|i| i.message.as_str())
                                    .collect::<Vec<_>>()
                            }).to_string())
                        }
                        "patch" => {
                            let old_string = input.get("old_string")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let new_string = input.get("new_string")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let file_path = input.get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("SKILL.md");
                            let replace_all = input.get("replace_all")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);

                            if old_string.is_empty() {
                                return Ok(json!({"success": false, "message": "skill_manage patch: 'old_string' is required"}).to_string());
                            }

                            // 安全检查：file_path 不能包含路径遍历、反斜杠或空组件 (与主工具一致)
                            if file_path.contains("..") || file_path.contains('\0') || file_path.contains('\\') {
                                return Ok(json!({"success": false, "message": format!("skill_manage patch: invalid file_path '{}'", file_path)}).to_string());
                            }
                            // 验证每个路径组件不为空 (防止 // 等异常路径)
                            for component in file_path.split('/') {
                                if component.is_empty() {
                                    return Ok(json!({"success": false, "message": format!("skill_manage patch: invalid file_path '{}' (empty path component)", file_path)}).to_string());
                                }
                            }

                            // 使用 find_skill_dir_forked 跨目录搜索 (与主工具 find_skill_dir 对齐)
                            let patch_skill_dir = match find_skill_dir_forked(name, category, dir, external_skills_dirs) {
                                Some(d) => d,
                                None => {
                                    // 如果 skill_dir 本身存在 (可能是新 skill 还没有 SKILL.md), 也尝试
                                    if skill_dir.is_dir() { skill_dir.clone() }
                                    else { return Ok(json!({"success": false, "message": format!("Skill '{}' not found", name)}).to_string()); }
                                }
                            };

                            let target = patch_skill_dir.join(file_path);
                            if !target.exists() {
                                return Ok(json!({"success": false, "message": format!("skill_manage patch: file '{}' not found in skill '{}'", file_path, name)}).to_string());
                            }

                            let content = tokio::fs::read_to_string(&target).await
                                .map_err(ForkedAgentError::Io)?;

                            // 使用 fuzzy_match 的 9-strategy 模糊匹配 (与主工具一致)
                            match fuzzy_find_and_replace(&content, old_string, new_string, replace_all) {
                                Ok((new_content, match_count, strategy)) => {
                                    // 安全扫描
                                    let scan_result = scan_skill_content(&new_content);
                                    if !scan_result.passed {
                                        return Ok(json!({
                                            "success": false,
                                            "message": format!("Security scan failed. Changes not applied.\nCritical issues: {}",
                                                scan_result.issues.iter()
                                                    .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Critical))
                                                    .map(|i| i.message.as_str())
                                                    .collect::<Vec<_>>()
                                                    .join("; ")),
                                        }).to_string());
                                    }

                                    // 原子写入 (temp file + rename)
                                    atomic_write_text(&target, &new_content).await
                                        .map_err(|e| ForkedAgentError::ToolError(format!("Failed to write patch: {}", e)))?;

                                    // 如果 patch 的是 SKILL.md，更新 meta.json
                                    if file_path == "SKILL.md" {
                                        let meta = extract_frontmatter(&new_content);
                                        let meta_path = patch_skill_dir.join("meta.json");
                                        let meta_json = serde_json::to_string_pretty(&meta)
                                            .unwrap_or_else(|_| "{}".to_string());
                                        let _ = atomic_write_text(&meta_path, &meta_json).await;
                                    }

                                    Ok(json!({
                                        "success": true,
                                        "match_count": match_count,
                                        "strategy": strategy,
                                        "message": format!("Patched {} occurrence(s) in '{}' using {} strategy", match_count, file_path, strategy),
                                        "warnings": scan_result.issues.iter()
                                            .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Warning))
                                            .map(|i| i.message.as_str())
                                            .collect::<Vec<_>>()
                                    }).to_string())
                                }
                                Err(e) => {
                                    Ok(json!({
                                        "success": false,
                                        "message": format!("Fuzzy match failed: {}", e),
                                    }).to_string())
                                }
                            }
                        }
                        "delete" => {
                            // 使用 find_skill_dir_forked 跨目录搜索
                            let del_skill_dir = match find_skill_dir_forked(name, category, dir, external_skills_dirs) {
                                Some(d) => d,
                                None => return Ok(json!({"success": false, "message": format!("Skill '{}' not found", name)}).to_string()),
                            };
                            tokio::fs::remove_dir_all(&del_skill_dir).await
                                .map_err(ForkedAgentError::Io)?;
                            // 清理空的分类目录 (与主工具一致)
                            if let Some(category_dir) = del_skill_dir.parent() {
                                if category_dir != dir {
                                    let _ = tokio::fs::remove_dir(category_dir).await;
                                }
                            }
                            Ok(json!({"success": true, "message": format!("Skill '{}' deleted", name)}).to_string())
                        }
                        "edit" => {
                            let content = input.get("content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            if content.is_empty() {
                                return Ok(json!({"success": false, "message": "skill_manage edit: 'content' parameter is required"}).to_string());
                            }

                            // 安全检查：内容大小限制 (与主工具一致, 使用字节数)
                            if content.len() > MAX_SKILL_CONTENT_CHARS {
                                return Ok(json!({"success": false, "message": format!("skill_manage edit: content too large ({} bytes, max {})", content.len(), MAX_SKILL_CONTENT_CHARS)}).to_string());
                            }

                            // 安全扫描
                            let scan_result = scan_skill_content(content);
                            if !scan_result.passed {
                                return Ok(json!({
                                    "success": false,
                                    "message": format!("Security scan failed: {}",
                                        scan_result.issues.iter()
                                            .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Critical))
                                            .map(|i| i.message.as_str())
                                            .collect::<Vec<_>>()
                                            .join("; ")),
                                }).to_string());
                            }

                            // 使用 find_skill_dir_forked 跨目录搜索
                            let edit_skill_dir = match find_skill_dir_forked(name, category, dir, external_skills_dirs) {
                                Some(d) => d,
                                None => return Ok(json!({"success": false, "message": format!("Skill '{}' not found", name)}).to_string()),
                            };

                            let skill_md = edit_skill_dir.join("SKILL.md");
                            if !skill_md.exists() {
                                return Ok(json!({"success": false, "message": format!("Skill '{}' not found (no SKILL.md)", name)}).to_string());
                            }

                            // 备份原内容 (用于回滚, 与主工具一致)
                            let original_content = tokio::fs::read_to_string(&skill_md).await
                                .map_err(ForkedAgentError::Io)?;

                            if let Err(e) = atomic_write_text(&skill_md, content).await {
                                // 写入失败, 但原文件仍完好 (原子写入不会损坏原文件)
                                return Err(ForkedAgentError::ToolError(format!("Failed to write edit: {}", e)));
                            }

                            // 更新 meta.json
                            let meta = extract_frontmatter(content);
                            let meta_path = edit_skill_dir.join("meta.json");
                            let meta_json = serde_json::to_string_pretty(&meta)
                                .unwrap_or_else(|_| "{}".to_string());
                            if let Err(e) = atomic_write_text(&meta_path, &meta_json).await {
                                // meta.json 写入失败: 回滚 SKILL.md (与主工具一致)
                                let _ = atomic_write_text(&skill_md, &original_content).await;
                                tracing::warn!(error = %e, "[forked] Failed to write meta.json, rolling back SKILL.md for skill '{}'", name);
                                return Ok(json!({"success": false, "message": format!("Failed to write meta.json: {}", e)}).to_string());
                            }

                            Ok(json!({
                                "success": true,
                                "message": format!("Skill '{}' edited (full content replaced)", name),
                                "warnings": scan_result.issues.iter()
                                    .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Warning))
                                    .map(|i| i.message.as_str())
                                    .collect::<Vec<_>>()
                            }).to_string())
                        }
                        "write_file" => {
                            let file_path = input.get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let file_content = input.get("file_content")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            if file_path.is_empty() {
                                return Ok(json!({"success": false, "message": "skill_manage write_file: 'file_path' is required"}).to_string());
                            }

                            // 安全检查：file_path 不能包含路径遍历或反斜杠
                            if file_path.contains("..") || file_path.contains('\0') || file_path.contains('\\') {
                                return Ok(json!({"success": false, "message": format!("skill_manage write_file: invalid file_path '{}'", file_path)}).to_string());
                            }

                            // 安全检查：file_path 必须在允许的子目录下 (与主工具一致)
                            let allowed_prefixes = ["references/", "templates/", "scripts/", "assets/"];
                            if !allowed_prefixes.iter().any(|prefix| file_path.starts_with(prefix)) {
                                return Ok(json!({"success": false, "message": format!("skill_manage write_file: file_path must be under one of: {}", allowed_prefixes.join(", "))}).to_string());
                            }

                            // 安全检查：内容大小限制
                            if file_content.len() > MAX_FILE_SIZE {
                                return Ok(json!({"success": false, "message": format!("skill_manage write_file: content too large ({} bytes, max {})", file_content.len(), MAX_FILE_SIZE)}).to_string());
                            }

                            // 使用 find_skill_dir_forked 跨目录搜索
                            let wf_skill_dir = match find_skill_dir_forked(name, category, dir, external_skills_dirs) {
                                Some(d) => d,
                                None => return Ok(json!({"success": false, "message": format!("Skill '{}' not found", name)}).to_string()),
                            };

                            // 安全扫描
                            let scan_result = scan_skill_content(file_content);
                            if !scan_result.passed {
                                return Ok(json!({
                                    "success": false,
                                    "message": format!("Security scan failed: {}",
                                        scan_result.issues.iter()
                                            .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Critical))
                                            .map(|i| i.message.as_str())
                                            .collect::<Vec<_>>()
                                            .join("; ")),
                                }).to_string());
                            }

                            let target = wf_skill_dir.join(file_path);
                            // 确保父目录存在
                            if let Some(parent) = target.parent() {
                                tokio::fs::create_dir_all(parent).await
                                    .map_err(ForkedAgentError::Io)?;
                            }

                            // 原子写入 (temp file + rename, 防止崩溃时损坏)
                            atomic_write_text(&target, file_content).await
                                .map_err(|e| ForkedAgentError::ToolError(format!("Failed to write file: {}", e)))?;

                            // 目录级安全扫描 (与主工具一致: 写入后检查整个目录)
                            let dir_scan = scan_skill_dir_with_trust(&wf_skill_dir, blockcell_tools::security_scan::TrustLevel::AgentCreated);
                            if !dir_scan.passed {
                                // 写入的文件导致目录级安全问题 → 回滚
                                let _ = tokio::fs::remove_file(&target).await;
                                return Ok(json!({
                                    "success": false,
                                    "message": format!("Directory-level security scan failed after writing file. File removed.\nCritical issues: {}",
                                        dir_scan.issues.iter()
                                            .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Critical))
                                            .map(|i| i.message.as_str())
                                            .collect::<Vec<_>>()
                                            .join("; ")),
                                }).to_string());
                            }

                            Ok(json!({
                                "success": true,
                                "message": format!("File '{}' written to skill '{}'", file_path, name),
                                "warnings": scan_result.issues.iter()
                                    .filter(|i| matches!(i.level, blockcell_tools::security_scan::IssueLevel::Warning))
                                    .map(|i| i.message.as_str())
                                    .collect::<Vec<_>>()
                            }).to_string())
                        }
                        "remove_file" => {
                            let file_path = input.get("file_path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");

                            if file_path.is_empty() {
                                return Ok(json!({"success": false, "message": "skill_manage remove_file: 'file_path' is required"}).to_string());
                            }

                            // 不允许删除 SKILL.md 或 meta.json (与主工具一致)
                            if file_path == "SKILL.md" || file_path == "meta.json" {
                                return Ok(json!({"success": false, "message": "Cannot delete SKILL.md or meta.json. Use delete action to remove the entire skill."}).to_string());
                            }

                            // 安全检查 (与主工具一致: 包含反斜杠检查)
                            if file_path.contains("..") || file_path.contains('\0') || file_path.contains('\\') {
                                return Ok(json!({"success": false, "message": format!("skill_manage remove_file: invalid file_path '{}'", file_path)}).to_string());
                            }

                            // 使用 find_skill_dir_forked 跨目录搜索
                            let rf_skill_dir = match find_skill_dir_forked(name, category, dir, external_skills_dirs) {
                                Some(d) => d,
                                None => return Ok(json!({"success": false, "message": format!("Skill '{}' not found", name)}).to_string()),
                            };

                            let target = rf_skill_dir.join(file_path);
                            if target.exists() {
                                tokio::fs::remove_file(&target).await
                                    .map_err(ForkedAgentError::Io)?;
                                // 清理空父目录 (与主工具一致)
                                if let Some(parent) = target.parent() {
                                    if parent != rf_skill_dir {
                                        let _ = tokio::fs::remove_dir(parent).await;
                                    }
                                }
                                Ok(json!({"success": true, "message": format!("File '{}' removed from skill '{}'", file_path, name)}).to_string())
                            } else {
                                Ok(json!({"success": false, "message": format!("File '{}' not found in skill '{}'", file_path, name)}).to_string())
                            }
                        }
                        _ => Ok(json!({"success": false, "message": format!("skill_manage: unknown action '{}'. Supported: create, patch, view, delete, edit, write_file, remove_file", action)}).to_string())
                    }
                }
                None => Ok(json!({"success": false, "message": "Skills directory not available"}).to_string()),
            }
        },

        // 不支持的工具
        _ => {
            Ok(format!("Tool '{}' is not supported in forked mode. Supported tools: read_file, file_edit, file_write, grep, glob, memory_upsert, memory_query, memory_forget, skill_manage, list_skills", tool_name))
        }
    }
}

/// 为 skill_manage "view" 构建完整响应 (包含 meta, references, templates)
async fn build_view_response_for_skill(
    skill_dir: &Path,
    skill_name: &str,
    category: Option<&str>,
) -> Result<String, ForkedAgentError> {
    let skill_md = skill_dir.join("SKILL.md");
    let content = tokio::fs::read_to_string(&skill_md)
        .await
        .map_err(ForkedAgentError::Io)?;
    let truncated = if content.len() > MAX_OUTPUT_CHARS {
        let mut boundary = MAX_OUTPUT_CHARS;
        while boundary > 0 && !content.is_char_boundary(boundary) {
            boundary -= 1;
        }
        format!(
            "{}...\n[Truncated, total {} chars]",
            &content[..boundary],
            content.len()
        )
    } else {
        content
    };
    let meta = read_meta_json(skill_dir);
    let references = list_dir_files(&skill_dir.join("references"));
    let templates = list_dir_files(&skill_dir.join("templates"));
    let mut resp = json!({
        "success": true,
        "name": skill_name,
        "content": truncated,
        "meta": meta,
        "references": references,
        "templates": templates,
    });
    if let Some(cat) = category {
        resp["category"] = json!(cat);
    }
    Ok(resp.to_string())
}

/// 列出目录中的文件 (仅文件名, 最多 50)
fn list_dir_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if dir.exists() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    files.push(entry.file_name().to_string_lossy().to_string());
                    if files.len() >= 50 {
                        break;
                    }
                }
            }
        }
    }
    files
}

/// 读取 meta.json 内容 (如果存在)
fn read_meta_json(skill_dir: &Path) -> Option<serde_json::Value> {
    let meta_path = skill_dir.join("meta.json");
    if meta_path.exists() {
        std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
    } else {
        None
    }
}

/// 验证 Skill frontmatter: 检查 YAML frontmatter 包含必需的 name 和 description 字段
///
/// 返回问题列表，空列表表示通过验证
fn validate_skill_frontmatter(content: &str) -> Vec<String> {
    let mut issues = Vec::new();

    // 检查是否以 frontmatter 分隔符开头
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        issues.push("Missing YAML frontmatter: content must start with '---'".to_string());
        return issues;
    }

    // 提取 frontmatter 内容
    let after_first = &trimmed[3..]; // skip leading ---
    let fm_end = after_first.find("\n---");
    if fm_end.is_none() {
        // 尝试只到文件末尾
        issues.push("Unclosed YAML frontmatter: missing closing '---'".to_string());
        return issues;
    }

    let fm_content = &after_first[..fm_end.unwrap()];

    // 检查必需的 name 字段 (包括空值检查)
    let has_valid_name = fm_content.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed == "name:" || trimmed.starts_with("name:") || trimmed.starts_with("name :") {
            // 检查值是否非空
            if let Some(val) = trimmed.split_once(':') {
                let value = val.1.trim();
                return !value.is_empty();
            }
        }
        false
    });
    if !has_valid_name {
        issues.push("Missing or empty required field 'name' in frontmatter".to_string());
    }

    // 检查必需的 description 字段 (包括空值和长度检查)
    let max_desc_len = 1024;
    let has_valid_desc = fm_content.lines().any(|line| {
        let trimmed = line.trim();
        if trimmed == "description:"
            || trimmed.starts_with("description:")
            || trimmed.starts_with("description :")
        {
            // 检查值是否非空且不超过长度限制
            if let Some(val) = trimmed.split_once(':') {
                let value = val.1.trim();
                return !value.is_empty() && value.len() <= max_desc_len;
            }
        }
        false
    });
    if !has_valid_desc {
        issues.push(
            "Missing or empty required field 'description' in frontmatter (max 1024 chars)"
                .to_string(),
        );
    }

    issues
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
pub async fn run_forked_agent(
    params: ForkedAgentParams,
) -> Result<ForkedAgentResult, ForkedAgentError> {
    let start_time = Instant::now();
    let mut output_messages = Vec::new();
    let mut total_usage = UsageMetrics::default();
    let mut files_modified = Vec::new();

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
            context
                .abort_controller
                .reason()
                .unwrap_or_else(|| "Aborted".to_string()),
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
    let system_prompt = params
        .system_prompt
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

    // 记录 Layer 7 agent_spawned 事件
    memory_event!(
        layer7,
        agent_spawned,
        params.fork_label,
        params.max_turns.unwrap_or(5),
        "main"
    );

    // 获取 Provider（带重试和指数退避）
    let provider_pool = match params.provider_pool.as_ref() {
        Some(pool) => pool,
        None => {
            // 记录 Layer 7 agent_failed 事件（Provider 未配置）
            memory_event!(layer7, agent_failed, params.fork_label, "no_provider", 0);
            return Err(ForkedAgentError::NoProviderAvailable);
        }
    };

    let provider = match acquire_provider_with_retry(
        provider_pool,
        PROVIDER_RETRY_MAX_ATTEMPTS,
        PROVIDER_RETRY_INITIAL_DELAY_MS,
        PROVIDER_RETRY_MAX_DELAY_MS,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            // 记录 Layer 7 agent_failed 事件（Provider 获取失败）
            memory_event!(
                layer7,
                agent_failed,
                params.fork_label,
                "provider_acquire_failed",
                0
            );
            return Err(e);
        }
    };

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
            // 记录 Layer 7 agent_failed 事件
            memory_event!(layer7, agent_failed, params.fork_label, "aborted", turn);
            return Err(ForkedAgentError::Aborted(
                context
                    .abort_controller
                    .reason()
                    .unwrap_or_else(|| "Aborted".to_string()),
            ));
        }

        // 调用 LLM (传入工具 schema 让 LLM 知道可用工具)
        let response = match provider.chat(&current_messages, &params.tool_schemas).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(
                    fork_label = params.fork_label,
                    turn,
                    error = %e,
                    "[forked_agent] LLM call failed"
                );
                // 记录 Layer 7 agent_failed 事件
                memory_event!(layer7, agent_failed, params.fork_label, "llm_error", turn);
                return Err(ForkedAgentError::ProviderError(format!("{}", e)));
            }
        };

        // 提取用量
        if !response.usage.is_null() {
            let usage = &response.usage;
            let input = usage
                .get("input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let output = usage
                .get("output_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_read = usage
                .get("cache_read_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let cache_creation = usage
                .get("cache_creation_input_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
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
                    &params.memory_store,
                    &params.skills_dir,
                    &params.external_skills_dirs,
                    &params.skill_mutex,
                )
                .await;

                // 跟踪修改的文件
                if tool_result.is_ok() {
                    match tool_name.as_str() {
                        "file_edit" | "edit_file" | "file_write" | "write_file" => {
                            if let Some(file_path) =
                                tool_input.get("file_path").and_then(|v| v.as_str())
                            {
                                if !files_modified.contains(&file_path.to_string()) {
                                    files_modified.push(file_path.to_string());
                                }
                            }
                        }
                        "skill_manage" => {
                            if let Some(name) = tool_input.get("name").and_then(|v| v.as_str()) {
                                let action = tool_input
                                    .get("action")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if matches!(
                                    action,
                                    "create"
                                        | "edit"
                                        | "patch"
                                        | "delete"
                                        | "write_file"
                                        | "remove_file"
                                ) {
                                    let skill_path = format!("skills/{}/", name);
                                    if !files_modified.contains(&skill_path) {
                                        files_modified.push(skill_path);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }

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

    // 记录 Layer 7 agent_completed 事件（带 duration）
    memory_event!(
        layer7,
        agent_completed_with_duration,
        params.fork_label,
        max_turns,
        total_usage.input_tokens + total_usage.output_tokens,
        duration_ms
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
