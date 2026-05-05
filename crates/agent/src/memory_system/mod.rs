//! 记忆系统集成模块
//!
//! 封装所有 7 层记忆系统的状态和操作，提供统一接口。

use crate::auto_memory::{ExtractionCursor, ExtractionCursorManager, MemoryType};
use crate::compact::{should_compact, CompactHookRegistry, FileTracker, SkillTracker};
use crate::response_cache::ContentReplacementState;
use crate::session_memory::{
    get_session_memory_path, should_extract_memory, SessionMemoryConfig, SessionMemoryState,
};
use blockcell_core::types::ChatMessage;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::task::JoinHandle;

/// 后台任务句柄类型
pub type BackgroundTaskHandle = JoinHandle<()>;

// Re-export MemorySystemConfig from core crate
pub use blockcell_core::config::MemorySystemConfig;

/// 记忆系统状态
#[derive(Debug, Default)]
pub struct MemorySystemState {
    /// Session Memory 状态
    pub session_memory: SessionMemoryState,
    /// 内容替换状态 (Layer 1)
    pub content_replacement: ContentReplacementState,
    /// 自动记忆提取游标
    pub auto_memory_cursors: Vec<ExtractionCursor>,
    /// 是否有待处理的提取任务
    pub has_pending_extraction: bool,
    /// 文件追踪器 (Layer 4 Compact 恢复)
    pub file_tracker: FileTracker,
    /// 技能追踪器 (Layer 4 Compact 恢复)
    pub skill_tracker: SkillTracker,
    /// 后台任务句柄列表 (用于追踪和取消)
    pub background_tasks: Vec<BackgroundTaskHandle>,
    /// 是否需要重新加载游标状态（后台提取完成后设置）
    pub needs_cursor_reload: bool,
}

/// 记忆系统集成器
///
/// 封装所有记忆系统操作，提供统一接口
pub struct MemorySystem {
    /// 配置
    config: MemorySystemConfig,
    /// 状态
    state: MemorySystemState,
    /// Compact Hooks 注册表
    compact_hooks: CompactHookRegistry,
    /// 工作目录
    workspace_dir: PathBuf,
    /// 配置目录
    config_dir: PathBuf,
    /// 会话 ID
    session_id: String,
    /// 自动记忆提取游标管理器（缓存已加载的状态）
    cursor_manager: ExtractionCursorManager,
    /// 游标重新加载标志（用于后台任务通知主线程）
    cursor_reload_flag: Arc<std::sync::atomic::AtomicBool>,
}

impl MemorySystem {
    /// 创建记忆系统
    pub fn new(
        config: MemorySystemConfig,
        workspace_dir: PathBuf,
        config_dir: PathBuf,
        session_id: String,
    ) -> Self {
        let cursor_manager = ExtractionCursorManager::new(&config_dir);

        let tracker_summary_chars = config.layer4.tracker_summary_chars;
        let session_memory_config: SessionMemoryConfig = config.layer3.clone().into();
        let max_replacement_entries = config.layer1.max_replacement_entries;

        Self {
            config,
            state: MemorySystemState {
                content_replacement: ContentReplacementState::with_max_entries(
                    max_replacement_entries,
                ),
                file_tracker: FileTracker::with_config(tracker_summary_chars),
                skill_tracker: SkillTracker::with_config(tracker_summary_chars),
                session_memory: SessionMemoryState {
                    config: session_memory_config,
                    ..Default::default()
                },
                ..Default::default()
            },
            compact_hooks: CompactHookRegistry::new(),
            workspace_dir,
            config_dir,
            session_id,
            cursor_manager,
            cursor_reload_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// 异步初始化（加载游标状态 + 标记会话活跃）
    pub async fn initialize(&mut self) -> std::io::Result<()> {
        self.cursor_manager.load().await?;
        // 标记会话为活跃状态
        self.mark_session_active().await
    }

    /// 重新加载游标状态
    ///
    /// 在后台提取任务完成后调用，确保下次检查使用最新的游标状态。
    pub async fn reload_cursors(&mut self) -> std::io::Result<()> {
        self.cursor_manager.load().await?;
        tracing::trace!("[memory_system] Cursor state reloaded");
        Ok(())
    }

    /// 获取会话目录路径
    ///
    /// 注意：session_id 中的冒号、斜杠等字符会被替换为下划线，以确保跨平台兼容性。
    /// 例如：`cli:default` -> `cli_default`
    pub fn session_dir(&self) -> PathBuf {
        use blockcell_core::session_file_stem;
        let safe_session_id = session_file_stem(&self.session_id);
        self.workspace_dir.join("sessions").join(safe_session_id)
    }

    /// 标记会话为活跃状态
    ///
    /// 创建/更新 `.active` 文件，用于防止 prune 删除活跃会话。
    async fn mark_session_active(&self) -> std::io::Result<()> {
        let session_dir = self.session_dir();
        tokio::fs::create_dir_all(&session_dir).await?;

        let active_file = session_dir.join(".active");
        tokio::fs::write(&active_file, chrono::Utc::now().to_rfc3339()).await?;

        tracing::trace!(
            session_id = %self.session_id,
            active_file = %active_file.display(),
            "[memory_system] Session marked as active"
        );
        Ok(())
    }

    /// 清除会话活跃标记
    ///
    /// 在会话结束时调用，允许 prune 清理该会话。
    pub async fn clear_session_active(&self) -> std::io::Result<()> {
        let active_file = self.session_dir().join(".active");

        if tokio::fs::try_exists(&active_file).await? {
            tokio::fs::remove_file(&active_file).await?;
            tracing::trace!(
                session_id = %self.session_id,
                "[memory_system] Session active marker cleared"
            );
        }
        Ok(())
    }

    /// 创建并初始化记忆系统（便捷方法）
    pub async fn new_initialized(
        config: MemorySystemConfig,
        workspace_dir: PathBuf,
        config_dir: PathBuf,
        session_id: String,
    ) -> std::io::Result<Self> {
        let mut system = Self::new(config, workspace_dir, config_dir, session_id);
        system.initialize().await?;
        Ok(system)
    }

    /// 获取 Session Memory 文件路径
    pub fn session_memory_path(&self) -> PathBuf {
        get_session_memory_path(&self.workspace_dir, &self.session_id)
    }

    /// 检查是否应该提取 Session Memory
    pub fn should_extract_session_memory(&self, messages: &[ChatMessage]) -> bool {
        should_extract_memory(messages, &self.state.session_memory)
    }

    /// 检查是否应该执行 Compact
    pub fn should_compact(&self, current_tokens: usize) -> bool {
        if !self.config.compact_enabled {
            return false;
        }
        should_compact(
            current_tokens,
            self.config.token_budget,
            self.config.layer4.compact_threshold_ratio,
        )
    }

    /// 更新 Session Memory 状态
    pub fn update_session_memory_state(&mut self, message_index: usize, token_count: usize) {
        self.state.session_memory.last_memory_message_index = Some(message_index);
        self.state.session_memory.tokens_at_last_extraction = token_count;
        self.state.session_memory.initialized = true;
    }

    /// 更新 Session Memory 状态（包含消息 ID）
    ///
    /// 推荐使用此方法，因为消息 ID 在消息列表被修改时仍然有效
    pub fn update_session_memory_state_with_id(
        &mut self,
        message_id: Option<String>,
        message_index: usize,
        token_count: usize,
    ) {
        self.state.session_memory.last_memory_message_id = message_id;
        self.state.session_memory.last_memory_message_index = Some(message_index);
        self.state.session_memory.tokens_at_last_extraction = token_count;
        self.state.session_memory.initialized = true;
    }

    /// 获取内容替换状态
    pub fn content_replacement_state(&self) -> &ContentReplacementState {
        &self.state.content_replacement
    }

    /// 获取可变内容替换状态
    pub fn content_replacement_state_mut(&mut self) -> &mut ContentReplacementState {
        &mut self.state.content_replacement
    }

    /// 获取 Session Memory 状态
    pub fn session_memory_state(&self) -> &SessionMemoryState {
        &self.state.session_memory
    }

    /// 获取可变 Session Memory 状态
    pub fn session_memory_state_mut(&mut self) -> &mut SessionMemoryState {
        &mut self.state.session_memory
    }

    /// 获取 Compact Hooks 注册表
    pub fn compact_hooks(&self) -> &CompactHookRegistry {
        &self.compact_hooks
    }

    /// 获取可变 Compact Hooks 注册表
    pub fn compact_hooks_mut(&mut self) -> &mut CompactHookRegistry {
        &mut self.compact_hooks
    }

    /// 标记有待处理的提取任务
    pub fn set_pending_extraction(&mut self, pending: bool) {
        self.state.has_pending_extraction = pending;
    }

    /// 检查是否有待处理的提取任务
    pub fn has_pending_extraction(&self) -> bool {
        self.state.has_pending_extraction
    }

    /// 标记需要重新加载游标状态
    pub fn set_needs_cursor_reload(&mut self, needs_reload: bool) {
        self.state.needs_cursor_reload = needs_reload;
    }

    /// 检查是否需要重新加载游标状态
    pub fn needs_cursor_reload(&self) -> bool {
        self.state.needs_cursor_reload
    }

    /// 获取游标重新加载标志（用于后台任务通知）
    pub fn cursor_reload_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.cursor_reload_flag)
    }

    /// 检查并清除游标重新加载标志
    ///
    /// 如果后台任务设置了标志，返回 true 并清除标志。
    fn check_and_clear_cursor_reload(&self) -> bool {
        self.cursor_reload_flag
            .swap(false, std::sync::atomic::Ordering::Relaxed)
    }

    /// 获取配置
    pub fn config(&self) -> &MemorySystemConfig {
        &self.config
    }

    /// 获取状态
    pub fn state(&self) -> &MemorySystemState {
        &self.state
    }

    /// 获取配置目录
    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// 获取工作目录
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    /// 获取会话 ID
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 获取文件追踪器
    pub fn file_tracker(&self) -> &FileTracker {
        &self.state.file_tracker
    }

    /// 获取可变文件追踪器
    pub fn file_tracker_mut(&mut self) -> &mut FileTracker {
        &mut self.state.file_tracker
    }

    /// 获取技能追踪器
    pub fn skill_tracker(&self) -> &SkillTracker {
        &self.state.skill_tracker
    }

    /// 获取可变技能追踪器
    pub fn skill_tracker_mut(&mut self) -> &mut SkillTracker {
        &mut self.state.skill_tracker
    }

    /// 记录文件读取
    pub fn record_file_read(&mut self, path: std::path::PathBuf, content: &str) {
        self.state.file_tracker.record_read(path, content);
    }

    /// 记录技能加载
    pub fn record_skill_load(&mut self, name: &str, content: &str) {
        self.state.skill_tracker.record_load(name, content);
    }

    /// 生成 Compact 恢复消息
    pub fn generate_compact_recovery(&self, session_memory_content: Option<&str>) -> String {
        let budget = crate::compact::RecoveryBudget::from(&self.config.layer4);
        crate::compact::build_recovery_message(
            &self.state.file_tracker,
            &self.state.skill_tracker,
            session_memory_content,
            &budget,
        )
    }

    /// 获取游标管理器
    pub fn cursor_manager(&self) -> &ExtractionCursorManager {
        &self.cursor_manager
    }

    /// 获取可变游标管理器
    pub fn cursor_manager_mut(&mut self) -> &mut ExtractionCursorManager {
        &mut self.cursor_manager
    }

    /// 检查是否应该触发自动记忆提取
    pub fn should_extract_auto_memory(&self, messages: &[ChatMessage]) -> Vec<MemoryType> {
        let config = crate::auto_memory::AutoMemoryConfig::from(self.config.layer5.clone());
        let current_content = crate::auto_memory::build_message_content_signature(messages);
        crate::auto_memory::should_extract_auto_memory_with_config(
            &self.cursor_manager,
            messages.len(),
            &current_content,
            &config,
        )
    }

    /// 保存游标状态
    pub async fn save_cursors(&self) -> std::io::Result<()> {
        self.cursor_manager.save().await
    }

    /// 添加后台任务句柄
    pub fn add_background_task(&mut self, handle: BackgroundTaskHandle) {
        self.state.background_tasks.push(handle);
    }

    /// 获取后台任务数量
    pub fn background_task_count(&self) -> usize {
        self.state.background_tasks.len()
    }

    /// 清理已完成的后台任务
    ///
    /// 返回清理的任务数量
    pub fn cleanup_completed_tasks(&mut self) -> usize {
        let before = self.state.background_tasks.len();
        self.state
            .background_tasks
            .retain(|handle| !handle.is_finished());
        before - self.state.background_tasks.len()
    }

    /// 取消所有后台任务
    ///
    /// 在会话结束或需要紧急停止时调用
    pub fn abort_all_background_tasks(&mut self) {
        for handle in self.state.background_tasks.drain(..) {
            handle.abort();
        }
    }

    /// 检查是否有正在运行的后台任务
    pub fn has_running_background_tasks(&self) -> bool {
        self.state.background_tasks.iter().any(|h| !h.is_finished())
    }

    /// 等待所有后台任务完成（带超时）
    ///
    /// 在会话结束前调用，确保后台任务有时间完成。
    /// 如果超时，会取消剩余的任务。
    ///
    /// ## 参数
    /// - `timeout_secs`: 最大等待时间（秒）
    ///
    /// ## 返回
    /// - `Ok(())`: 所有任务成功完成
    /// - `Err(timeout_secs)`: 超时，剩余任务已取消
    pub async fn wait_for_background_tasks(&mut self, timeout_secs: u64) -> Result<(), u64> {
        if self.state.background_tasks.is_empty() {
            return Ok(());
        }

        let start = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(timeout_secs);

        // 等待所有任务完成或超时
        while self.has_running_background_tasks() {
            if start.elapsed() >= timeout {
                // 超时，取消剩余任务
                let running_count = self
                    .state
                    .background_tasks
                    .iter()
                    .filter(|h| !h.is_finished())
                    .count();
                tracing::warn!(
                    running_count,
                    timeout_secs,
                    session_id = %self.session_id,
                    "[memory_system] Timeout waiting for background tasks, aborting remaining"
                );
                self.abort_all_background_tasks();
                // 确保句柄向量已清空（abort_all_background_tasks 使用 drain，已清空）
                // 但为了安全，再次调用清理
                self.state.background_tasks.clear();
                return Err(timeout_secs);
            }

            // 短暂休眠避免忙等待
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // 清理已完成的任务
        self.cleanup_completed_tasks();

        tracing::debug!(
            session_id = %self.session_id,
            duration_ms = start.elapsed().as_millis() as u64,
            "[memory_system] All background tasks completed"
        );

        Ok(())
    }
}

impl Drop for MemorySystem {
    fn drop(&mut self) {
        // 清除会话活跃标记
        //
        // ## 清理策略说明
        //
        // `.active` 文件是会话活跃标记，用于防止 Dream Service 的 prune 机制删除正在运行的会话。
        // 清理失败不会影响功能正确性，因为：
        // 1. Dream Service 会检查进程是否存活（通过 PID）
        // 2. 过期的 `.active` 文件（>24小时）会被自动清理
        //
        // 因此这里使用尽力而为（best-effort）的清理策略：
        // - 尝试在后台线程执行清理，避免阻塞当前线程
        // - 如果线程创建失败或进程快速退出，清理可能未完成，但不影响功能
        // - 依赖 Dream Service 的 prune 机制作为兜底清理
        let active_file = self.session_dir().join(".active");
        // 克隆两次：一个用于线程内，一个用于线程创建失败时的日志
        let session_id_for_thread = self.session_id.clone();
        let session_id_for_error = self.session_id.clone();

        // 使用 std::thread::Builder 以便在失败时记录警告
        let thread_result = std::thread::Builder::new()
            .name("blockcell-session-cleanup".to_string())
            .spawn(move || {
                if active_file.exists() {
                    if let Err(e) = std::fs::remove_file(&active_file) {
                        // 仅在 Debug 模式记录警告，减少生产环境日志噪音
                        tracing::debug!(
                            error = %e,
                            session_id = %session_id_for_thread,
                            "[memory_system] Best-effort cleanup failed for session active marker (will be pruned by Dream Service)"
                        );
                    } else {
                        tracing::trace!(
                            session_id = %session_id_for_thread,
                            "[memory_system] Session active marker cleared on drop"
                        );
                    }
                }
            });

        // 如果线程创建失败，记录警告但不阻塞
        if let Err(e) = thread_result {
            tracing::debug!(
                error = %e,
                session_id = %session_id_for_error,
                "[memory_system] Failed to spawn cleanup thread, relying on Dream Service prune"
            );
        }

        // 自动清理所有后台任务，防止 zombie tasks
        let running_count = self
            .state
            .background_tasks
            .iter()
            .filter(|h| !h.is_finished())
            .count();
        if running_count > 0 {
            tracing::debug!(
                running_count,
                session_id = %self.session_id,
                "[memory_system] Dropping with running background tasks, aborting them"
            );
            self.abort_all_background_tasks();
        }
    }
}

/// Post-Sampling Hook 结果
#[derive(Debug)]
pub enum PostSamplingAction {
    /// 无操作
    None,
    /// 触发 Session Memory 提取
    ExtractSessionMemory,
    /// 触发自动记忆提取
    ExtractAutoMemory(Vec<MemoryType>),
    /// 触发 Compact
    Compact,
}

/// 检查是否应该触发记忆操作
///
/// ## 游标状态同步
/// 如果后台任务设置了 `cursor_reload_flag`，会先重新加载游标状态。
/// 这确保了后台提取任务完成后，主线程使用最新的游标状态。
///
/// ## 为什么是 async
/// 此函数从 async 运行时循环中调用。之前使用 `block_in_place + block_on`
/// 在单线程 runtime 或 `multi_thread` 且只有 1 个 worker 时会死锁，
/// 因为 `block_on` 需要另一个 worker 来驱动被阻塞的 future。
/// 改为直接 await 可安全适用于所有 runtime 配置。
pub async fn evaluate_memory_hooks(
    memory_system: &mut MemorySystem,
    messages: &[ChatMessage],
    current_tokens: usize,
) -> PostSamplingAction {
    // 检查是否需要重新加载游标状态（后台提取完成后）
    if memory_system.check_and_clear_cursor_reload() {
        if let Err(e) = memory_system.reload_cursors().await {
            tracing::warn!(error = %e, "[evaluate_memory_hooks] Failed to reload cursor state");
        }
    }

    // 1. 检查 Compact (最高优先级)
    if memory_system.should_compact(current_tokens) {
        return PostSamplingAction::Compact;
    }

    // 2. 检查 Session Memory 提取
    if memory_system.should_extract_session_memory(messages) {
        return PostSamplingAction::ExtractSessionMemory;
    }

    // 3. 检查自动记忆提取
    if memory_system.config().auto_memory_enabled {
        // 使用已加载的 cursor_manager，确保冷却机制正确工作
        let types_to_extract = memory_system.should_extract_auto_memory(messages);

        if !types_to_extract.is_empty() {
            return PostSamplingAction::ExtractAutoMemory(types_to_extract);
        }
    }

    PostSamplingAction::None
}

/// 默认记忆目录路径
pub fn default_memory_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".blockcell")
        .join("memory")
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::config::Layer4Config;

    #[test]
    fn test_memory_system_config_default() {
        let config = MemorySystemConfig::default();
        assert!(config.auto_memory_enabled);
        assert!(config.compact_enabled);
        assert_eq!(config.layer4.compact_threshold_ratio, 0.8);
    }

    #[test]
    fn test_memory_system_new() {
        let config = MemorySystemConfig::default();
        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test-session".to_string(),
        );

        assert_eq!(memory_system.session_id(), "test-session");
        assert!(!memory_system.has_pending_extraction());
    }

    #[test]
    fn test_should_compact() {
        let config = MemorySystemConfig {
            token_budget: 100_000,
            layer4: Layer4Config {
                compact_threshold_ratio: 0.8,
                ..Default::default()
            },
            ..Default::default()
        };
        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 低于阈值
        assert!(!memory_system.should_compact(70_000));

        // 达到阈值
        assert!(memory_system.should_compact(80_000));

        // 超过阈值
        assert!(memory_system.should_compact(100_000));
    }

    #[test]
    fn test_should_compact_disabled() {
        let config = MemorySystemConfig {
            compact_enabled: false,
            ..Default::default()
        };
        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 即使超过阈值也不触发
        assert!(!memory_system.should_compact(1_000_000));
    }

    #[tokio::test]
    async fn test_evaluate_memory_hooks_none() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        let messages = vec![ChatMessage::user("Hello"), ChatMessage::assistant("Hi!")];

        let action = evaluate_memory_hooks(&mut memory_system, &messages, 100).await;
        assert!(matches!(action, PostSamplingAction::None));
    }

    #[tokio::test]
    async fn test_evaluate_memory_hooks_compact() {
        let config = MemorySystemConfig {
            token_budget: 100,
            layer4: Layer4Config {
                compact_threshold_ratio: 0.8,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        let messages = vec![ChatMessage::user("Test")];
        let action = evaluate_memory_hooks(&mut memory_system, &messages, 100).await;

        assert!(matches!(action, PostSamplingAction::Compact));
    }

    #[test]
    fn test_memory_system_file_tracker() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 记录文件读取
        memory_system.record_file_read(PathBuf::from("/test.rs"), "test content");

        let tracker = memory_system.file_tracker();
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_memory_system_skill_tracker() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 记录技能加载
        memory_system.record_skill_load("test_skill", "skill content");

        let tracker = memory_system.skill_tracker();
        assert_eq!(tracker.len(), 1);
    }

    #[test]
    fn test_memory_system_update_session_memory_state() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        memory_system.update_session_memory_state(42, 5000);

        let state = memory_system.session_memory_state();
        assert_eq!(state.last_memory_message_index, Some(42));
        assert_eq!(state.tokens_at_last_extraction, 5000);
        assert!(state.initialized);
    }

    #[test]
    fn test_memory_system_pending_extraction() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        assert!(!memory_system.has_pending_extraction());

        memory_system.set_pending_extraction(true);
        assert!(memory_system.has_pending_extraction());

        memory_system.set_pending_extraction(false);
        assert!(!memory_system.has_pending_extraction());
    }

    #[test]
    fn test_memory_system_generate_compact_recovery() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 记录文件和技能
        memory_system.record_file_read(PathBuf::from("/test.rs"), "file content");
        memory_system.record_skill_load("test_skill", "skill content");

        // 生成恢复消息
        let recovery = memory_system.generate_compact_recovery(Some("session memory content"));

        assert!(recovery.contains("Files Previously Read"));
        assert!(recovery.contains("Skills Previously Loaded"));
        assert!(recovery.contains("Session Memory"));
    }

    #[test]
    fn test_memory_system_generate_compact_recovery_empty() {
        let config = MemorySystemConfig::default();
        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 不记录任何内容，生成空恢复消息
        let recovery = memory_system.generate_compact_recovery(None);

        // 应该是空字符串
        assert!(recovery.is_empty());
    }

    #[tokio::test]
    async fn test_post_sampling_action_order() {
        // 测试 Compact 优先级高于其他操作
        let config = MemorySystemConfig {
            token_budget: 100,
            layer4: Layer4Config {
                compact_threshold_ratio: 0.8,
                ..Default::default()
            },
            auto_memory_enabled: true,
            ..Default::default()
        };
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/config"),
            "test".to_string(),
        );

        // 即使有足够的消息触发 auto memory，也应该优先返回 Compact
        let messages: Vec<ChatMessage> = (0..20)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("msg {}", i)),
                    ChatMessage::assistant("resp"),
                ]
            })
            .collect();

        let action = evaluate_memory_hooks(&mut memory_system, &messages, 100).await;

        // Compact 优先级最高
        assert!(matches!(action, PostSamplingAction::Compact));
    }
}
