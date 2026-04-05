//! Forked Agent 模块 - 后台任务基础设施
//!
//! 提供与父进程共享 Prompt Cache 但状态隔离的子代理执行能力。
//! 用于 Session Memory 提取、自动记忆提取、梦境机制等后台任务。
//!
//! ## 核心概念
//!
//! - **CacheSafeParams**: 保证 Prompt Cache 命中的参数结构
//! - **SubagentOverrides**: 子代理状态隔离配置
//! - **CanUseToolFn**: 工具权限检查函数
//!
//! ## 使用示例
//!
//! ```ignore
//! use blockcell_agent::forked::{run_forked_agent, ForkedAgentParams, create_auto_mem_can_use_tool};
//!
//! let result = run_forked_agent(ForkedAgentParams {
//!     prompt_messages: vec![ChatMessage::user("任务提示")],
//!     cache_safe_params,
//!     can_use_tool: create_auto_mem_can_use_tool(&memory_dir),
//!     query_source: "auto_memory",
//!     fork_label: "auto_memory",
//!     max_turns: Some(5),
//!     skip_transcript: true,
//!     ..Default::default()
//! }).await?;
//! ```

mod agent;
mod cache_safe;
mod can_use_tool;
mod context;

pub use agent::{run_forked_agent, ForkedAgentParams, ForkedAgentResult, UsageMetrics};
pub use cache_safe::{
    CacheSafeParams, ToolDefinition,
    save_cache_safe_params, get_last_cache_safe_params,
    create_cache_safe_params, create_cache_safe_params_with_tools,
};
pub use can_use_tool::{
    ToolPermission, CanUseToolFn,
    create_memory_file_can_use_tool, create_auto_mem_can_use_tool,
    create_dream_can_use_tool, create_compact_can_use_tool,
};
pub use context::{
    SubagentOverrides, SubagentContext, create_subagent_context,
    FileStateCache, ContentReplacementState, AbortController, QueryTracking,
};