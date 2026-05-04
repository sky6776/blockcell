//! 子代理上下文覆盖
//!
//! 定义 Forked Agent 与父代理的状态隔离机制。
//! 可变状态必须克隆以保持隔离，共享状态保证缓存命中。

use blockcell_core::types::ChatMessage;
use blockcell_core::AbortToken;
use std::sync::Arc;
use uuid::Uuid;

// 重新导出 ContentReplacementState，避免重复定义
pub use crate::response_cache::ContentReplacementState;

/// 子代理上下文覆盖选项
///
/// 用于创建与父代理隔离但可共享特定状态的子代理上下文。
/// 推荐使用 `SubagentOverrides::with_abort_token()` 构造，确保取消链完整。
#[derive(Clone, Default)]
pub struct SubagentOverrides {
    /// 覆盖 agent_id
    pub agent_id: Option<String>,
    /// 覆盖 agent_type
    pub agent_type: Option<String>,
    /// 覆盖 messages 数组
    pub messages: Option<Vec<ChatMessage>>,
    /// 覆盖文件状态缓存
    pub file_state: Option<FileStateCache>,
    /// 覆盖 abort controller (legacy)
    pub abort_controller: Option<Arc<AbortController>>,
    /// 新的 AbortToken (推荐)
    /// 必须通过 `with_abort_token()` 或显式设置来提供，否则取消链会断裂。
    pub abort_token: Option<AbortToken>,
    /// 覆盖内容替换状态
    pub content_replacement_state: Option<ContentReplacementState>,
    /// 显式共享父代理的 abort_controller
    pub share_abort_controller: bool,
    /// 关键系统提醒
    pub critical_system_reminder: Option<String>,
    /// 强制调用 can_use_tool
    pub require_can_use_tool: bool,
    /// 最大输出 tokens
    pub max_output_tokens: Option<u32>,
    /// 最大轮次
    pub max_turns: Option<u32>,
    /// 工作目录（用于 worktree 隔离）
    pub working_dir: Option<std::path::PathBuf>,
}

impl SubagentOverrides {
    /// 创建带有 AbortToken 的 SubagentOverrides（推荐构造方式）
    /// 确保取消链完整，子代理可以被父代理取消。
    pub fn with_abort_token(token: AbortToken) -> Self {
        Self {
            abort_token: Some(token),
            ..Default::default()
        }
    }

    /// 检查取消链是否完整（abort_token 已设置）
    pub fn has_abort_chain(&self) -> bool {
        self.abort_token.is_some() || self.abort_controller.is_some()
    }
}

/// 文件状态缓存
///
/// 记录已读取的文件内容，用于 Post-Compact 恢复等场景。
#[derive(Clone, Default)]
pub struct FileStateCache {
    /// 文件路径 -> (内容, 时间戳)
    files: std::collections::HashMap<std::path::PathBuf, (String, i64)>,
}

impl FileStateCache {
    /// 创建新的文件状态缓存
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录文件读取
    pub fn record(&mut self, path: std::path::PathBuf, content: String) {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.files.insert(path, (content, timestamp));
    }

    /// 获取文件内容
    pub fn get(&self, path: &std::path::Path) -> Option<&str> {
        self.files.get(path).map(|(content, _)| content.as_str())
    }

    /// 获取最近访问的文件
    pub fn get_recent_files(&self, max_files: usize) -> Vec<(std::path::PathBuf, String, i64)> {
        let mut files: Vec<_> = self
            .files
            .iter()
            .map(|(p, (c, t))| (p.clone(), c.clone(), *t))
            .collect();

        // 按时间戳倒序排序
        files.sort_by(|a, b| b.2.cmp(&a.2));
        files.truncate(max_files);
        files
    }

    /// 清空缓存
    pub fn clear(&mut self) {
        self.files.clear();
    }

    /// 获取文件数量
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 克隆状态用于 Forked Agent
    pub fn clone_state(&self) -> Self {
        Self {
            files: self.files.clone(),
        }
    }
}

/// Abort Controller
///
/// 用于取消正在进行的操作。
#[derive(Clone)]
pub struct AbortController {
    /// 是否已中止
    aborted: Arc<std::sync::atomic::AtomicBool>,
    /// 中止原因
    reason: Arc<std::sync::RwLock<Option<String>>>,
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortController {
    /// 创建新的 AbortController
    pub fn new() -> Self {
        Self {
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            reason: Arc::new(std::sync::RwLock::new(None)),
        }
    }

    /// 创建子控制器（与父控制器共享状态）
    pub fn child_of(parent: &AbortController) -> Self {
        Self {
            aborted: parent.aborted.clone(),
            reason: parent.reason.clone(),
        }
    }

    /// 中止操作
    pub fn abort(&self, reason: Option<String>) {
        // 先写入 reason，再设置 AtomicBool。
        // 这确保 is_aborted() == true 时 reason() 已可用，
        // 避免调用者看到 aborted=true 但 reason=None 的瞬态。
        if let Some(r) = reason {
            match self.reason.write() {
                Ok(mut guard) => {
                    *guard = Some(r);
                }
                Err(e) => {
                    // 锁中毒，尝试恢复并设置原因
                    // RwLock 的 PoisonError 允许访问内部数据
                    let mut guard = e.into_inner();
                    *guard = Some(r);
                    tracing::warn!(
                        "[abort_controller] RwLock was poisoned, recovered and set reason"
                    );
                }
            }
        }
        self.aborted
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// 检查是否已中止
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::Acquire)
    }

    /// 获取中止原因
    pub fn reason(&self) -> Option<String> {
        match self.reason.read() {
            Ok(guard) => guard.clone(),
            Err(e) => {
                // 锁中毒，尝试恢复并获取原因
                let guard = e.into_inner();
                guard.clone()
            }
        }
    }
}

/// 查询追踪信息
#[derive(Clone, Debug)]
pub struct QueryTracking {
    /// 链 ID
    pub chain_id: Uuid,
    /// 深度（Forked Agent 会增加深度）
    pub depth: u32,
}

impl Default for QueryTracking {
    fn default() -> Self {
        Self {
            chain_id: Uuid::new_v4(),
            depth: 0,
        }
    }
}

/// 创建隔离的子代理上下文
///
/// 根据覆盖选项创建与父代理隔离的上下文。
pub fn create_subagent_context(
    parent_file_state: Option<&FileStateCache>,
    parent_replacement_state: Option<&ContentReplacementState>,
    parent_abort_controller: Option<&AbortController>,
    parent_abort_token: Option<&AbortToken>,
    overrides: SubagentOverrides,
) -> SubagentContext {
    // 文件状态克隆
    let file_state = overrides
        .file_state
        .or_else(|| parent_file_state.map(|s| s.clone_state()))
        .unwrap_or_default();

    // 内容替换状态
    let content_replacement_state = overrides
        .content_replacement_state
        .or_else(|| parent_replacement_state.map(|s| s.clone_state()))
        .unwrap_or_default();

    // AbortToken (优先使用新的)
    let has_override = overrides.abort_token.is_some();
    let abort_token = match overrides.abort_token {
        Some(token) => token,
        None => {
            if let Some(parent) = parent_abort_token {
                parent.child()
            } else {
                // 当 overrides.abort_token=None 且 parent_abort_token=None 时，
                // 子代理的取消链断裂，父代理取消无法传播到子代理。
                tracing::warn!(
                    has_override,
                    has_parent = parent_abort_token.is_some(),
                    "Subagent abort chain is broken: no AbortToken provided and no parent to inherit from. \
                     Use SubagentOverrides::with_abort_token() to ensure cancellation propagates."
                );
                AbortToken::default()
            }
        }
    };

    // AbortController (legacy 兼容)
    let abort_controller = overrides
        .abort_controller
        .or_else(|| {
            if overrides.share_abort_controller {
                parent_abort_controller.map(|c| Arc::new(c.clone()))
            } else {
                parent_abort_controller.map(|c| Arc::new(AbortController::child_of(c)))
            }
        })
        .unwrap_or_else(|| Arc::new(AbortController::new()));

    // 查询追踪
    let query_tracking = QueryTracking {
        chain_id: Uuid::new_v4(),
        depth: 0, // 实际深度由调用者设置
    };

    SubagentContext {
        file_state,
        content_replacement_state,
        abort_controller,
        abort_token,
        query_tracking,
        agent_id: overrides.agent_id,
        agent_type: overrides.agent_type,
        messages: overrides.messages,
        critical_system_reminder: overrides.critical_system_reminder,
        require_can_use_tool: overrides.require_can_use_tool,
        max_output_tokens: overrides.max_output_tokens,
        max_turns: overrides.max_turns,
        working_dir: overrides.working_dir,
    }
}

/// 子代理上下文
///
/// 包含隔离的状态和共享的配置。
pub struct SubagentContext {
    /// 文件状态缓存
    pub file_state: FileStateCache,
    /// 内容替换状态
    pub content_replacement_state: ContentReplacementState,
    /// Abort Controller (legacy)
    pub abort_controller: Arc<AbortController>,
    /// Abort Token (推荐，支持链式取消)
    pub abort_token: AbortToken,
    /// 查询追踪
    pub query_tracking: QueryTracking,
    /// Agent ID
    pub agent_id: Option<String>,
    /// Agent Type
    pub agent_type: Option<String>,
    /// 消息列表
    pub messages: Option<Vec<ChatMessage>>,
    /// 关键系统提醒
    pub critical_system_reminder: Option<String>,
    /// 强制工具权限检查
    pub require_can_use_tool: bool,
    /// 最大输出 tokens
    pub max_output_tokens: Option<u32>,
    /// 最大轮次
    pub max_turns: Option<u32>,
    /// 工作目录（用于 worktree 隔离）
    pub working_dir: Option<std::path::PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_state_cache() {
        let mut cache = FileStateCache::new();
        cache.record(
            std::path::PathBuf::from("/path/to/file1.txt"),
            "content1".to_string(),
        );
        cache.record(
            std::path::PathBuf::from("/path/to/file2.txt"),
            "content2".to_string(),
        );

        assert_eq!(cache.len(), 2);
        assert_eq!(
            cache.get(std::path::Path::new("/path/to/file1.txt")),
            Some("content1")
        );

        let recent = cache.get_recent_files(1);
        assert_eq!(recent.len(), 1);
    }

    #[test]
    fn test_content_replacement_state() {
        let mut state = ContentReplacementState::new();

        state.set_replacement("tool-1".to_string(), "replacement content".to_string());

        assert!(state.is_seen("tool-1"));
        assert_eq!(state.get_replacement("tool-1"), Some("replacement content"));

        let cloned = state.clone_state();
        assert!(cloned.is_seen("tool-1"));
    }

    #[test]
    fn test_abort_controller() {
        let controller = AbortController::new();
        assert!(!controller.is_aborted());

        controller.abort(Some("test reason".to_string()));
        assert!(controller.is_aborted());
        assert_eq!(controller.reason(), Some("test reason".to_string()));

        let child = AbortController::child_of(&controller);
        assert!(child.is_aborted()); // 子控制器共享状态
    }
}
