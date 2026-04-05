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

mod memory_type;
mod extractor;
mod cursor;
mod injector;

pub use memory_type::{MemoryType, MEMORY_FILE_NAMES, get_memory_file_path};
pub use extractor::{
    AutoMemoryExtractor, ExtractionResult, ExtractionParams,
    should_extract_auto_memory, extract_auto_memory,
};
pub use cursor::{ExtractionCursor, ExtractionCursorManager};
pub use injector::{
    MemoryInjector, InjectionConfig, InjectedMemory,
    format_memory_for_context,
};

/// 记忆提取配置
pub const MIN_MESSAGES_FOR_EXTRACTION: usize = 10;
pub const EXTRACTION_COOLDOWN_MESSAGES: usize = 5;
pub const MAX_MEMORY_FILE_TOKENS: usize = 4000;

/// 记忆目录名
pub const MEMORY_DIR_NAME: &str = "memory";

/// 获取记忆目录路径
pub fn get_memory_dir(config_dir: &std::path::Path) -> std::path::PathBuf {
    config_dir.join(MEMORY_DIR_NAME)
}