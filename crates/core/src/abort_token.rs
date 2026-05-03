use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

/// 最大链深度限制，防止无限递归
const MAX_CHAIN_DEPTH: u32 = 16;

/// Agent级别的取消信号
/// 参考: Claude Code AbortController 链
///
/// 设计说明：parent 使用 Arc<AbortToken> 是必要的，因为：
/// 1. AbortToken 包含 Option<AbortToken> 会导致无限大小
/// 2. Arc 提供间接引用打破递归
/// 3. cancelled 字段使用 Arc<AtomicBool> 共享取消状态
#[derive(Clone)]
pub struct AbortToken {
    /// 是否已取消（所有 clone 的 token 共享此状态）
    cancelled: Arc<AtomicBool>,
    /// 父级取消信号（形成链）
    /// Arc 是必要的，用于打破递归类型
    parent: Option<Arc<AbortToken>>,
}

/// 取消错误
#[derive(Debug, Clone, thiserror::Error)]
#[error("Operation cancelled: {message}")]
pub struct CancelledError {
    pub message: String,
}

impl AbortToken {
    /// 创建新的取消信号
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            parent: None,
        }
    }

    /// 创建子取消信号（链式传递）
    /// 父取消时，子自动取消
    ///
    /// Arc 包装是必要的，用于打破递归类型（AbortToken 包含 parent）
    pub fn child(&self) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            parent: Some(Arc::new(self.clone())),
        }
    }

    /// 检查是否已取消
    pub fn is_cancelled(&self) -> bool {
        self.is_cancelled_with_depth(0)
    }

    /// 带深度限制的取消检查
    fn is_cancelled_with_depth(&self, depth: u32) -> bool {
        if depth > MAX_CHAIN_DEPTH {
            // 链过深，视为未取消而非静默终止操作。
            // 深链可能表示 bug（循环引用或无限嵌套），记录警告并返回 false
            // 让 agent 继续运行，而非错误地终止。
            tracing::warn!(
                depth,
                max = MAX_CHAIN_DEPTH,
                "Abort chain depth exceeded, treating as not cancelled (possible circular reference)"
            );
            return false;
        }
        // 检查自身
        if self.cancelled.load(Ordering::SeqCst) {
            return true;
        }
        // 检查父级（链式传递）
        if let Some(parent) = &self.parent {
            return parent.is_cancelled_with_depth(depth + 1);
        }
        false
    }

    /// 检查取消状态，返回错误
    pub fn check(&self) -> Result<(), CancelledError> {
        if self.is_cancelled() {
            return Err(CancelledError {
                message: "Agent was cancelled".to_string(),
            });
        }
        Ok(())
    }

    /// 触发取消
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// 取消并附带原因
    pub fn cancel_with_reason(&self, reason: String) {
        self.cancelled.store(true, Ordering::SeqCst);
        tracing::info!(reason = %reason, "AbortToken cancelled");
    }
}

impl Default for AbortToken {
    fn default() -> Self {
        Self::new()
    }
}

/// Cleanup 函数类型别名
type CleanupHandler = Box<dyn FnOnce() + Send>;

/// Cleanup 函数注册表
#[allow(clippy::type_complexity)]
pub struct CleanupRegistry {
    handlers: Arc<StdMutex<Vec<CleanupHandler>>>,
}

/// Cleanup 注销句柄
#[allow(clippy::type_complexity)]
pub struct CleanupHandle {
    registry: Arc<StdMutex<Vec<CleanupHandler>>>,
    index: usize,
}

impl CleanupRegistry {
    pub fn new() -> Self {
        Self {
            handlers: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    /// 注册 cleanup 函数，返回注销句柄
    pub fn register<F: FnOnce() + Send + 'static>(&self, handler: F) -> CleanupHandle {
        let mut handlers = match self.handlers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let index = handlers.len();
        handlers.push(Box::new(handler));

        CleanupHandle {
            registry: self.handlers.clone(),
            index,
        }
    }

    /// 执行所有 cleanup 函数
    pub fn run_all(&self) {
        let mut handlers = match self.handlers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for handler in handlers.drain(..) {
            handler();
        }
    }
}

impl Default for CleanupRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CleanupHandle {
    fn drop(&mut self) {
        // Drop 时执行 cleanup handler
        // Recover from poisoned mutex to avoid panic-during-drop (process abort)
        let mut handlers = match self.registry.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if self.index < handlers.len() {
            // 使用 mem::replace 替换为空闭包，保持索引稳定
            // 这样其他 CleanupHandle 的 index 仍然有效
            if let Some(handler) = handlers.get_mut(self.index) {
                let handler = std::mem::replace(handler, Box::new(|| {}));
                // Use catch_unwind to prevent panic-during-drop from aborting the process
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handler();
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_abort_token_new_not_cancelled() {
        let token = AbortToken::new();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn test_abort_token_cancel() {
        let token = AbortToken::new();
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn test_abort_token_chain_parent_cancels_child() {
        let parent = AbortToken::new();
        let child = parent.child();

        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled()); // 子自动取消
    }

    #[test]
    fn test_abort_token_check_returns_error() {
        let token = AbortToken::new();
        assert!(token.check().is_ok());

        token.cancel();
        assert!(token.check().is_err());
    }

    #[test]
    fn test_cleanup_registry_runs_handlers() {
        let registry = CleanupRegistry::new();
        let counter = Arc::new(StdMutex::new(0));

        let counter_clone = counter.clone();
        registry.register(move || {
            *counter_clone.lock().unwrap() += 1;
        });

        registry.run_all();
        assert_eq!(*counter.lock().unwrap(), 1);
    }

    #[test]
    fn test_cleanup_handle_runs_on_drop() {
        let counter = Arc::new(StdMutex::new(0));
        let registry = CleanupRegistry::new();

        {
            let counter_clone = counter.clone();
            let _handle = registry.register(move || {
                *counter_clone.lock().unwrap() += 1;
            });
            // handle dropped here
        }

        assert_eq!(*counter.lock().unwrap(), 1);
    }
}
