//! AbortToken Context for Forked Agent Cancellation
//!
//! Provides task-local storage for AbortToken, enabling chain cancellation
//! from parent agent to forked child agents.
//!
//! ## Usage
//!
//! ```ignore
//! // Create parent token
//! let parent_token = AbortToken::new();
//!
//! // Execute in context with cancellation support
//! scope_abort_token(parent_token.clone(), async {
//!     // Forked agent can get child token
//!     let child_token = current_abort_token().unwrap().child();
//!
//!     // Pass to run_forked_agent via overrides
//!     ...
//! }).await;
//!
//! // Cancel all descendants
//! parent_token.cancel();
//! ```

use crate::AbortToken;
use tokio::task_local;

task_local! {
    static ABORT_TOKEN_CONTEXT: AbortToken;
}

/// Get current AbortToken from task-local context (if exists)
pub fn current_abort_token() -> Option<AbortToken> {
    ABORT_TOKEN_CONTEXT.try_get().ok()
}

/// Execute async operation within an AbortToken context
pub async fn scope_abort_token<R>(token: AbortToken, f: impl std::future::Future<Output = R>) -> R {
    ABORT_TOKEN_CONTEXT.scope(token, f).await
}

/// Spawn a task with AbortToken context inherited
pub fn spawn_with_abort_token<F>(token: AbortToken, future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(ABORT_TOKEN_CONTEXT.scope(token, future))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_scope_abort_token() {
        let parent = AbortToken::new();

        let result = scope_abort_token(parent.clone(), async {
            // Should be able to get the token
            let current = current_abort_token();
            assert!(current.is_some());
            assert!(!current.unwrap().is_cancelled());
            "ok"
        })
        .await;

        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn test_abort_token_chain_in_context() {
        let parent = AbortToken::new();

        let result = scope_abort_token(parent.clone(), async {
            // Get child token
            let child = current_abort_token().unwrap().child();

            // Child should not be cancelled initially
            assert!(!child.is_cancelled());

            // Cancel parent
            parent.cancel();

            // Child should now be cancelled
            assert!(child.is_cancelled());

            "verified"
        })
        .await;

        assert_eq!(result, "verified");
    }
}
