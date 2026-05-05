//! Compact Hooks 注册和执行
//!
//! 允许在 Compact 前后执行自定义逻辑。
//!
//! ## Session Memory 恢复 Hook
//!
//! `SessionMemoryRecoveryHook` 在 Post-Compact 阶段：
//! 1. 等待 Session Memory 提取完成
//! 2. 生成恢复消息
//! 3. 返回 `PostCompactResult::NeedRecovery` 以注入恢复消息

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Pre-Compact Hook 结果
#[derive(Debug, Clone, PartialEq)]
pub enum PreCompactResult {
    /// 继续 Compact
    Continue,
    /// 取消 Compact（例如：预算实际上足够）
    Cancel,
    /// 延迟 Compact（例如：等待后台任务完成）
    Delay(std::time::Duration),
}

/// Post-Compact Hook 结果
#[derive(Debug)]
pub enum PostCompactResult {
    /// 成功完成
    Success,
    /// 需要额外恢复
    NeedRecovery(String),
}

/// Pre-Compact Hook 函数类型
pub type PreCompactHookFn = Arc<
    dyn Fn(PreCompactContext) -> Pin<Box<dyn Future<Output = PreCompactResult> + Send>>
        + Send
        + Sync,
>;

/// Post-Compact Hook 函数类型
pub type PostCompactHookFn = Arc<
    dyn Fn(PostCompactContext) -> Pin<Box<dyn Future<Output = PostCompactResult> + Send>>
        + Send
        + Sync,
>;

/// Pre-Compact Hook 上下文
#[derive(Debug, Clone)]
pub struct PreCompactContext {
    /// 当前 Token 数
    pub current_tokens: usize,
    /// Token 预算
    pub budget_tokens: usize,
    /// 会话 ID
    pub session_id: String,
    /// 是否有待处理的后台任务
    pub has_pending_background_tasks: bool,
}

/// Post-Compact Hook 上下文
#[derive(Debug, Clone)]
pub struct PostCompactContext {
    /// 会话 ID
    pub session_id: String,
    /// 恢复消息
    pub recovery_message: String,
    /// Session Memory 路径
    pub session_memory_path: Option<std::path::PathBuf>,
}

/// Pre-Compact Hook trait
pub trait PreCompactHook: Send + Sync {
    /// 执行 Hook
    fn execute(
        &self,
        ctx: PreCompactContext,
    ) -> Pin<Box<dyn Future<Output = PreCompactResult> + Send>>;
}

/// Post-Compact Hook trait
pub trait PostCompactHook: Send + Sync {
    /// 执行 Hook
    fn execute(
        &self,
        ctx: PostCompactContext,
    ) -> Pin<Box<dyn Future<Output = PostCompactResult> + Send>>;
}

/// Compact Hook 注册表
#[derive(Default)]
pub struct CompactHookRegistry {
    /// Pre-Compact Hooks
    pre_hooks: Vec<PreCompactHookFn>,
    /// Post-Compact Hooks
    post_hooks: Vec<PostCompactHookFn>,
}

impl CompactHookRegistry {
    /// 创建空注册表
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 Pre-Compact Hook
    pub fn register_pre_hook<F>(&mut self, hook: F)
    where
        F: Fn(PreCompactContext) -> Pin<Box<dyn Future<Output = PreCompactResult> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.pre_hooks.push(Arc::new(hook));
    }

    /// 注册 Post-Compact Hook
    pub fn register_post_hook<F>(&mut self, hook: F)
    where
        F: Fn(PostCompactContext) -> Pin<Box<dyn Future<Output = PostCompactResult> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.post_hooks.push(Arc::new(hook));
    }

    /// 执行所有 Pre-Compact Hooks
    pub async fn execute_pre_hooks(&self, ctx: PreCompactContext) -> PreCompactResult {
        for hook in &self.pre_hooks {
            let result = hook(ctx.clone()).await;
            match result {
                PreCompactResult::Cancel => return PreCompactResult::Cancel,
                PreCompactResult::Delay(d) => return PreCompactResult::Delay(d),
                PreCompactResult::Continue => continue,
            }
        }
        PreCompactResult::Continue
    }

    /// 执行所有 Post-Compact Hooks
    pub async fn execute_post_hooks(&self, ctx: PostCompactContext) -> PostCompactResult {
        let mut needs_recovery = String::new();

        for hook in &self.post_hooks {
            let result = hook(ctx.clone()).await;
            match result {
                PostCompactResult::NeedRecovery(msg) => {
                    needs_recovery.push_str(&msg);
                    needs_recovery.push('\n');
                }
                PostCompactResult::Success => continue,
            }
        }

        if needs_recovery.is_empty() {
            PostCompactResult::Success
        } else {
            PostCompactResult::NeedRecovery(needs_recovery)
        }
    }

    /// 检查是否有 Pre-Compact Hooks
    pub fn has_pre_hooks(&self) -> bool {
        !self.pre_hooks.is_empty()
    }

    /// 检查是否有 Post-Compact Hooks
    pub fn has_post_hooks(&self) -> bool {
        !self.post_hooks.is_empty()
    }
}

/// 默认 Pre-Compact Hook：等待后台任务完成
#[allow(dead_code)]
pub fn default_pre_compact_hook() -> impl PreCompactHook {
    DefaultPreCompactHook
}

#[allow(dead_code)]
struct DefaultPreCompactHook;

impl PreCompactHook for DefaultPreCompactHook {
    fn execute(
        &self,
        ctx: PreCompactContext,
    ) -> Pin<Box<dyn Future<Output = PreCompactResult> + Send>> {
        Box::pin(async move {
            if ctx.has_pending_background_tasks {
                // 等待后台任务完成
                tracing::info!(
                    session_id = %ctx.session_id,
                    "[compact] waiting for background tasks before compact"
                );
                PreCompactResult::Delay(std::time::Duration::from_secs(5))
            } else {
                PreCompactResult::Continue
            }
        })
    }
}

/// 默认 Post-Compact Hook：刷新 Session Memory
#[allow(dead_code)]
pub fn default_post_compact_hook() -> impl PostCompactHook {
    DefaultPostCompactHook
}

#[allow(dead_code)]
struct DefaultPostCompactHook;

impl PostCompactHook for DefaultPostCompactHook {
    fn execute(
        &self,
        ctx: PostCompactContext,
    ) -> Pin<Box<dyn Future<Output = PostCompactResult> + Send>> {
        Box::pin(async move {
            // 验证 Session Memory 文件存在
            if let Some(path) = &ctx.session_memory_path {
                if tokio::fs::try_exists(path).await.ok() == Some(true) {
                    tracing::info!(
                        path = %path.display(),
                        "[compact] session memory file verified"
                    );
                }
            }
            PostCompactResult::Success
        })
    }
}

/// Session Memory 恢复 Hook
///
/// 在 Post-Compact 阶段执行 Session Memory 恢复：
/// 1. 等待后台提取完成
/// 2. 读取 Session Memory 内容
/// 3. 生成恢复消息
///
/// ## 集成到 Compact 流程
///
/// ```ignore
/// let mut registry = CompactHookRegistry::new();
/// registry.register_post_hook(create_session_memory_recovery_hook(
///     workspace_dir,
///     session_id,
///     template,
///     max_tokens,
///     extraction_wait_timeout_ms,
///     extraction_stale_threshold_ms,
/// ));
/// ```
#[allow(dead_code)]
pub struct SessionMemoryRecoveryHook {
    /// 工作目录
    workspace_dir: std::path::PathBuf,
    /// 会话 ID
    session_id: String,
    /// Session Memory 模板
    template: String,
    /// 最大 tokens
    max_tokens: usize,
    /// 提取开始时间
    extraction_started_at: Option<std::time::Instant>,
    /// 提取等待超时 (ms)
    extraction_wait_timeout_ms: u64,
    /// 提取过期阈值 (ms)
    extraction_stale_threshold_ms: u64,
}

impl SessionMemoryRecoveryHook {
    /// 创建 Hook
    pub fn new(
        workspace_dir: std::path::PathBuf,
        session_id: String,
        template: String,
        max_tokens: usize,
        extraction_started_at: Option<std::time::Instant>,
        extraction_wait_timeout_ms: u64,
        extraction_stale_threshold_ms: u64,
    ) -> Self {
        Self {
            workspace_dir,
            session_id,
            template,
            max_tokens,
            extraction_started_at,
            extraction_wait_timeout_ms,
            extraction_stale_threshold_ms,
        }
    }
}

impl PostCompactHook for SessionMemoryRecoveryHook {
    fn execute(
        &self,
        _ctx: PostCompactContext,
    ) -> Pin<Box<dyn Future<Output = PostCompactResult> + Send>> {
        let workspace_dir = self.workspace_dir.clone();
        let session_id = self.session_id.clone();
        let template = self.template.clone();
        let max_tokens = self.max_tokens;
        let extraction_started_at = self.extraction_started_at;
        let extraction_wait_timeout_ms = self.extraction_wait_timeout_ms;
        let extraction_stale_threshold_ms = self.extraction_stale_threshold_ms;

        Box::pin(async move {
            use crate::session_memory::recovery::{
                get_session_memory_content_for_compact, get_session_memory_path,
                wait_for_session_memory_extraction_with_timeout,
            };

            let memory_path = get_session_memory_path(&workspace_dir, &session_id);

            // 1. 等待提取完成（使用可配置超时）
            if let Err(e) = wait_for_session_memory_extraction_with_timeout(
                &memory_path,
                extraction_started_at,
                extraction_wait_timeout_ms,
                extraction_stale_threshold_ms,
            )
            .await
            {
                tracing::warn!(
                    path = %memory_path.display(),
                    error = %e,
                    "[compact] session memory extraction wait failed"
                );
                return PostCompactResult::Success; // 继续，不阻塞 Compact
            }

            // 2. 获取 Session Memory 内容
            match get_session_memory_content_for_compact(&memory_path, &template, max_tokens).await
            {
                Ok(content) => {
                    // 3. 检查是否有实际内容
                    if content == template {
                        tracing::debug!(
                            path = %memory_path.display(),
                            "[compact] session memory is empty, skipping recovery"
                        );
                        return PostCompactResult::Success;
                    }

                    // 4. 生成恢复消息
                    let recovery_message = format!(
                        "## Session Memory Recovery\n\n\
                         Session Memory file: {}\n\n\
                         ```markdown\n{}\n```",
                        memory_path.display(),
                        content
                    );

                    tracing::info!(
                        path = %memory_path.display(),
                        "[compact] session memory recovery message generated"
                    );

                    PostCompactResult::NeedRecovery(recovery_message)
                }
                Err(e) => {
                    tracing::warn!(
                        path = %memory_path.display(),
                        error = %e,
                        "[compact] failed to read session memory for recovery"
                    );
                    PostCompactResult::Success
                }
            }
        })
    }
}

/// 创建 Session Memory 恢复 Hook 函数
///
/// 用于直接注册到 `CompactHookRegistry`。
#[allow(dead_code)]
pub fn create_session_memory_recovery_hook(
    workspace_dir: std::path::PathBuf,
    session_id: String,
    template: String,
    max_tokens: usize,
    extraction_wait_timeout_ms: u64,
    extraction_stale_threshold_ms: u64,
) -> impl Fn(PostCompactContext) -> Pin<Box<dyn Future<Output = PostCompactResult> + Send>>
       + Send
       + Sync
       + 'static {
    let hook = SessionMemoryRecoveryHook::new(
        workspace_dir,
        session_id,
        template,
        max_tokens,
        None,
        extraction_wait_timeout_ms,
        extraction_stale_threshold_ms,
    );

    move |ctx: PostCompactContext| {
        let hook = hook.clone();
        Box::pin(async move { hook.execute(ctx).await })
    }
}

// 实现 Clone for SessionMemoryRecoveryHook（用于闭包捕获）
impl Clone for SessionMemoryRecoveryHook {
    fn clone(&self) -> Self {
        Self {
            workspace_dir: self.workspace_dir.clone(),
            session_id: self.session_id.clone(),
            template: self.template.clone(),
            max_tokens: self.max_tokens,
            // Reset extraction_started_at to None in the clone to avoid
            // carrying stale timing from the original instance.
            extraction_started_at: None,
            extraction_wait_timeout_ms: self.extraction_wait_timeout_ms,
            extraction_stale_threshold_ms: self.extraction_stale_threshold_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_registry_new() {
        let registry = CompactHookRegistry::new();
        assert!(registry.pre_hooks.is_empty());
        assert!(registry.post_hooks.is_empty());
    }

    #[tokio::test]
    async fn test_execute_pre_hooks_continue() {
        let registry = CompactHookRegistry::new();
        let ctx = PreCompactContext {
            current_tokens: 100_000,
            budget_tokens: 80_000,
            session_id: "test".to_string(),
            has_pending_background_tasks: false,
        };

        let result = registry.execute_pre_hooks(ctx).await;
        assert_eq!(result, PreCompactResult::Continue);
    }

    #[tokio::test]
    async fn test_default_pre_compact_hook() {
        let hook = default_pre_compact_hook();

        // 有后台任务时延迟
        let ctx_with_tasks = PreCompactContext {
            current_tokens: 100_000,
            budget_tokens: 80_000,
            session_id: "test".to_string(),
            has_pending_background_tasks: true,
        };
        let result = hook.execute(ctx_with_tasks).await;
        assert!(matches!(result, PreCompactResult::Delay(_)));

        // 无后台任务时继续
        let ctx_no_tasks = PreCompactContext {
            current_tokens: 100_000,
            budget_tokens: 80_000,
            session_id: "test".to_string(),
            has_pending_background_tasks: false,
        };
        let result = hook.execute(ctx_no_tasks).await;
        assert_eq!(result, PreCompactResult::Continue);
    }

    #[tokio::test]
    async fn test_register_and_execute_post_hook() {
        let mut registry = CompactHookRegistry::new();

        registry.register_post_hook(|ctx| {
            Box::pin(async move {
                PostCompactResult::NeedRecovery(format!("Recovery needed for {}", ctx.session_id))
            })
        });

        let ctx = PostCompactContext {
            session_id: "test".to_string(),
            recovery_message: "".to_string(),
            session_memory_path: None,
        };

        let result = registry.execute_post_hooks(ctx).await;
        assert!(matches!(result, PostCompactResult::NeedRecovery(_)));
    }
}
