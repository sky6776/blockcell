//! Auto Memory 模块 - Layer 5 自动记忆提取
//!
//! 后台自动提取四种类型的记忆：
//! - User: 用户角色、偏好、知识背景
//! - Project: 项目工作、目标、事件
//! - Feedback: 用户纠正、工作指导
//! - Reference: 外部系统资源指针
//!
//! ## 工具权限矩阵
//! | 工具 | 权限 |
//! |------|------|
//! | Read/Grep/Glob | ✅ 允许 |
//! | Bash | ✅ 只读命令 |
//! | Edit/Write | ✅ 仅 memory 目录 |

mod cursor;
mod extractor;
mod injector;
mod memory_type;
pub mod scanner;

pub use cursor::{ExtractionCursor, ExtractionCursorManager};
pub(crate) use extractor::build_message_content_signature;
pub use extractor::{
    should_extract_auto_memory, should_extract_auto_memory_with_config, AutoMemoryConfig,
    AutoMemoryExtractor, ExtractionParams, ExtractionResult,
};
pub use injector::{format_memory_for_context, InjectedMemory, InjectionConfig, MemoryInjector};
pub use memory_type::{get_memory_file_path, MemoryType, MEMORY_FILE_NAMES};

/// 记忆提取配置 — 仅用作 AutoMemoryConfig::default() 和
/// should_extract_auto_memory() 的回退值，
/// 运行时使用 Layer5Config 中的对应字段
#[deprecated(note = "use crate::unified_security_scanner for learned content scanning")]
pub use scanner as deprecated_scanner;
pub const MIN_MESSAGES_FOR_EXTRACTION: usize = 15;
pub const EXTRACTION_COOLDOWN_MESSAGES: usize = 5;
pub const MAX_MEMORY_FILE_TOKENS: usize = 4000;

/// 注入预算默认值 — 仅用作 InjectionConfig::default() 的回退值，
/// 运行时使用 Layer5Config.injection_max_tokens
pub const DEFAULT_INJECTION_MAX_TOKENS: usize = 4000;

/// 记忆目录名
pub const MEMORY_DIR_NAME: &str = "memory";

/// 获取记忆目录路径
pub fn get_memory_dir(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join(MEMORY_DIR_NAME)
}
