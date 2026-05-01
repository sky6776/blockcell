//! Session Memory 模块 - Layer 3 会话记忆
//!
//! 维护一个实时更新的 Markdown 文件，包含当前会话的关键信息。
//! 使用 Forked Agent 后台提取，不中断主对话。
//!
//! ## 核心概念
//!
//! - **10-Section 模板**: 标准化的会话信息结构
//! - **触发条件**: Token 阈值 + Tool Calls 阈值
//! - **异步提取**: 通过 Forked Agent 后台执行
//!
//! ## 与现有 session_summary 的区别
//!
//! | 特性 | Session Cache (本模块) | session_summary (已有) |
//! |------|----------------------|----------------------|
//! | 存储 | Markdown 文件 | SQLite 数据库 |
//! | 生命周期 | 会话级，结束后丢弃 | 持久化，TTL 过期 |
//! | 用途 | Post-Compact 恢复 | 跨会话 FTS5 检索 |

mod extractor;
pub mod recovery;
mod template;

pub use extractor::{
    count_tool_calls_since, extract_session_memory, should_extract_memory, ExtractionError,
    SessionMemoryConfig, SessionMemoryState,
};
pub use recovery::{
    get_session_memory_content_for_compact, get_session_memory_dir, get_session_memory_path,
    wait_for_session_memory_extraction, wait_for_session_memory_extraction_with_timeout,
};
pub use template::{
    validate_session_memory, Section, SectionPriority, ValidationResult,
    DEFAULT_SESSION_MEMORY_TEMPLATE,
};

use std::path::PathBuf;

/// 时间阈值常量
pub const EXTRACTION_WAIT_TIMEOUT_MS: u64 = 15_000;
pub const EXTRACTION_STALE_THRESHOLD_MS: u64 = 60_000;

/// Section 限制常量
pub const MAX_SECTION_LENGTH: usize = 2000;
pub const MAX_TOTAL_SESSION_MEMORY_TOKENS: usize = 12000;

/// 获取 Session Memory 目录
pub fn get_session_memory_base_dir(workspace_dir: &std::path::Path) -> PathBuf {
    workspace_dir.join("sessions")
}

/// 获取 Session Memory 文件路径
pub fn get_session_memory_file_path(workspace_dir: &std::path::Path, session_id: &str) -> PathBuf {
    use blockcell_core::session_file_stem;
    get_session_memory_base_dir(workspace_dir)
        .join(session_file_stem(session_id))
        .join("memory.md")
}
