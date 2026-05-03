use crate::agent_identity::AgentIdentity;
use tokio::task_local;

task_local! {
    static AGENT_CONTEXT: AgentIdentity;
}

/// 获取当前Agent上下文（如果存在）
pub fn current_agent_context() -> Option<AgentIdentity> {
    AGENT_CONTEXT.try_get().ok()
}

/// 检查当前Agent是否可以spawn子Agent（简化版）
/// 只检查ForkChild，不查询registry
/// 无上下文时默认拒绝（保守策略，防止 ForkChild 逃逸）
pub fn can_spawn_subagent() -> bool {
    current_agent_context()
        .map(|ctx| ctx.can_spawn_subagent_basic())
        .unwrap_or(false)
}

/// 在指定Agent上下文中执行异步操作
pub async fn scope_agent_context<R>(
    context: AgentIdentity,
    f: impl std::future::Future<Output = R>,
) -> R {
    AGENT_CONTEXT.scope(context, f).await
}

/// 在Agent上下文中spawn任务
pub fn spawn_with_context<F>(
    context: AgentIdentity,
    future: F,
) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(AGENT_CONTEXT.scope(context, future))
}
