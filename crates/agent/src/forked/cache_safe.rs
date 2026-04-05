//! CacheSafe 参数管理
//!
//! 保证 Forked Agent 与父进程共享 Prompt Cache 的参数结构。
//! Anthropic API 的缓存键由以下组成：
//! - system_prompt + tools + model + messages_prefix + thinking_config
//!
//! 任何一项不匹配都会导致缓存失效，因此必须严格控制这些参数的一致性。

use std::sync::Arc;
use std::collections::HashMap;
use blockcell_core::types::ChatMessage;
use tokio::sync::RwLock;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

/// 工具定义 - 用于缓存键计算
///
/// Anthropic API 缓存键包含完整的工具定义，任何字段变化都会导致缓存失效。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolDefinition {
    /// 工具名称
    pub name: String,
    /// 工具描述
    pub description: String,
    /// 参数 Schema (JSON Schema)
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    /// 创建新的工具定义
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }

    /// 计算工具定义的哈希值（用于快速比较）
    pub fn compute_hash(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.name.hash(&mut hasher);
        self.description.hash(&mut hasher);
        // 对 parameters 进行规范化后再哈希
        if let Ok(canonical) = serde_json::to_string(&self.parameters) {
            canonical.hash(&mut hasher);
        }
        hasher.finish()
    }
}

/// 缓存安全参数 - 必须与父进程保持一致才能共享 Prompt Cache
///
/// 这些参数决定了 Anthropic API 的缓存键，任何一项不匹配都会导致缓存失效。
/// Forked Agent 必须使用与父进程相同的 CacheSafeParams 才能共享缓存。
#[derive(Clone)]
pub struct CacheSafeParams {
    /// System prompt - 必须匹配父代理
    pub system_prompt: Arc<String>,
    /// User context - 预置于消息前
    pub user_context: HashMap<String, String>,
    /// System context - 追加到 system prompt
    pub system_context: HashMap<String, String>,
    /// Fork context messages - 父进程消息前缀（用于缓存共享）
    pub fork_context_messages: Vec<ChatMessage>,
    /// 模型名称
    pub model: String,
    /// 完整的工具定义列表（用于缓存键计算）
    pub tools: Vec<ToolDefinition>,
    /// 工具定义的哈希（用于快速比较）
    pub tools_hash: Option<u64>,
}

impl Default for CacheSafeParams {
    fn default() -> Self {
        Self {
            system_prompt: Arc::new(String::new()),
            user_context: HashMap::new(),
            system_context: HashMap::new(),
            fork_context_messages: Vec::new(),
            model: String::new(),
            tools: Vec::new(),
            tools_hash: None,
        }
    }
}

impl CacheSafeParams {
    /// 创建新的 CacheSafeParams
    pub fn new(
        system_prompt: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            system_prompt: Arc::new(system_prompt.into()),
            model: model.into(),
            ..Default::default()
        }
    }

    /// 设置 user context
    pub fn with_user_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.user_context.insert(key.into(), value.into());
        self
    }

    /// 设置 system context
    pub fn with_system_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.system_context.insert(key.into(), value.into());
        self
    }

    /// 设置 fork context messages
    pub fn with_fork_messages(mut self, messages: Vec<ChatMessage>) -> Self {
        self.fork_context_messages = messages;
        self
    }

    /// 设置工具定义列表
    pub fn with_tools(mut self, tools: Vec<ToolDefinition>) -> Self {
        // 计算工具哈希
        self.tools_hash = Some(self.compute_tools_hash(&tools));
        self.tools = tools;
        self
    }

    /// 设置工具哈希（用于向后兼容）
    pub fn with_tools_hash(mut self, hash: u64) -> Self {
        self.tools_hash = Some(hash);
        self
    }

    /// 计算工具列表的哈希值
    fn compute_tools_hash(&self, tools: &[ToolDefinition]) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        for tool in tools {
            tool.name.hash(&mut hasher);
            tool.description.hash(&mut hasher);
            if let Ok(canonical) = serde_json::to_string(&tool.parameters) {
                canonical.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// 检查是否与另一个 CacheSafeParams 兼容（可以共享缓存）
    pub fn is_compatible_with(&self, other: &CacheSafeParams) -> bool {
        // 系统提示必须完全匹配
        if self.system_prompt != other.system_prompt {
            return false;
        }
        // 模型必须匹配
        if self.model != other.model {
            return false;
        }
        // 工具定义必须匹配
        // 优先比较完整工具定义，其次比较哈希
        if !self.tools.is_empty() && !other.tools.is_empty() {
            // 两边都有完整定义，直接比较
            return self.tools == other.tools;
        }
        // 至少一边没有完整定义，比较哈希
        match (self.tools_hash, other.tools_hash) {
            (Some(h1), Some(h2)) if h1 != h2 => return false,
            _ => {}
        }
        true
    }
}

/// 全局存储最后一次的 CacheSafeParams
/// 由 handle_stop_hooks 调用 save_cache_safe_params 保存
///
/// 注意: 使用 `tokio::sync::RwLock` 而非 `std::sync::RwLock`。
/// `tokio::sync::RwLock` 不会因 panic 而中毒，这是异步安全的实现。
static LAST_CACHE_SAFE_PARAMS: Lazy<RwLock<Option<Arc<CacheSafeParams>>>> =
    Lazy::new(|| RwLock::new(None));

/// 保存 CacheSafeParams（由 handle_stop_hooks 调用）
pub async fn save_cache_safe_params(params: Option<Arc<CacheSafeParams>>) {
    let mut guard = LAST_CACHE_SAFE_PARAMS.write().await;
    *guard = params;
}

/// 获取最后的 CacheSafeParams
pub async fn get_last_cache_safe_params() -> Option<Arc<CacheSafeParams>> {
    let guard = LAST_CACHE_SAFE_PARAMS.read().await;
    guard.clone()
}

/// 从 REPLHookContext 创建 CacheSafeParams
///
/// 这个函数用于在主代理停止时保存缓存参数，供后续 Forked Agent 使用。
/// 注意：调用者需要提供必要的上下文信息。
pub fn create_cache_safe_params(
    system_prompt: impl Into<String>,
    model: impl Into<String>,
    messages: Vec<ChatMessage>,
) -> CacheSafeParams {
    CacheSafeParams {
        system_prompt: Arc::new(system_prompt.into()),
        model: model.into(),
        fork_context_messages: messages,
        ..Default::default()
    }
}

/// 从工具定义列表创建 CacheSafeParams
///
/// 这是推荐的创建方式，确保工具定义完整
pub fn create_cache_safe_params_with_tools(
    system_prompt: impl Into<String>,
    model: impl Into<String>,
    messages: Vec<ChatMessage>,
    tools: Vec<ToolDefinition>,
) -> CacheSafeParams {
    let mut params = CacheSafeParams {
        system_prompt: Arc::new(system_prompt.into()),
        model: model.into(),
        fork_context_messages: messages,
        ..Default::default()
    };
    params.tools_hash = Some(params.compute_tools_hash(&tools));
    params.tools = tools;
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_safe_params_compatibility() {
        let p1 = CacheSafeParams::new("system prompt", "model-a");
        let p2 = CacheSafeParams::new("system prompt", "model-a");
        let p3 = CacheSafeParams::new("different prompt", "model-a");
        let p4 = CacheSafeParams::new("system prompt", "model-b");

        assert!(p1.is_compatible_with(&p2));
        assert!(!p1.is_compatible_with(&p3));
        assert!(!p1.is_compatible_with(&p4));
    }

    #[test]
    fn test_cache_safe_params_builder() {
        let params = CacheSafeParams::new("system", "model")
            .with_user_context("key1", "value1")
            .with_system_context("key2", "value2")
            .with_tools_hash(12345);

        assert_eq!(params.user_context.get("key1"), Some(&"value1".to_string()));
        assert_eq!(params.system_context.get("key2"), Some(&"value2".to_string()));
        assert_eq!(params.tools_hash, Some(12345));
    }

    #[test]
    fn test_tool_definition_hash() {
        let tool1 = ToolDefinition::new(
            "read_file",
            "Read file contents",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );
        let tool2 = ToolDefinition::new(
            "read_file",
            "Read file contents",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );
        let tool3 = ToolDefinition::new(
            "read_file",
            "Read file",  // Different description
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );

        // Same tools should have same hash
        assert_eq!(tool1.compute_hash(), tool2.compute_hash());
        // Different tools should have different hash
        assert_ne!(tool1.compute_hash(), tool3.compute_hash());
    }

    #[test]
    fn test_cache_safe_params_with_tools() {
        let tools = vec![
            ToolDefinition::new(
                "read_file",
                "Read file contents",
                serde_json::json!({"type": "object"}),
            ),
            ToolDefinition::new(
                "write_file",
                "Write file contents",
                serde_json::json!({"type": "object"}),
            ),
        ];

        let params = CacheSafeParams::new("system", "model")
            .with_tools(tools.clone());

        assert_eq!(params.tools.len(), 2);
        assert!(params.tools_hash.is_some());
    }

    #[test]
    fn test_cache_safe_params_tools_compatibility() {
        let tools1 = vec![
            ToolDefinition::new("read_file", "Read file", serde_json::json!({})),
        ];
        let tools2 = vec![
            ToolDefinition::new("read_file", "Read file", serde_json::json!({})),
        ];
        let tools3 = vec![
            ToolDefinition::new("write_file", "Write file", serde_json::json!({})),
        ];

        let p1 = CacheSafeParams::new("system", "model").with_tools(tools1);
        let p2 = CacheSafeParams::new("system", "model").with_tools(tools2);
        let p3 = CacheSafeParams::new("system", "model").with_tools(tools3);

        // Same tools should be compatible
        assert!(p1.is_compatible_with(&p2));
        // Different tools should not be compatible
        assert!(!p1.is_compatible_with(&p3));
    }

    #[test]
    fn test_create_cache_safe_params_with_tools() {
        let tools = vec![
            ToolDefinition::new("read_file", "Read file", serde_json::json!({})),
        ];

        let params = create_cache_safe_params_with_tools(
            "system prompt",
            "model-a",
            vec![ChatMessage::user("hello")],
            tools.clone(),
        );

        assert_eq!(params.system_prompt.as_str(), "system prompt");
        assert_eq!(params.model, "model-a");
        assert_eq!(params.tools.len(), 1);
        assert!(params.tools_hash.is_some());
    }
}