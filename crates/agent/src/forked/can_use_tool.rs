//! 工具权限控制
//!
//! 定义 Forked Agent 的工具权限检查机制。
//! Forked Agent 运行在受限环境中，只能执行特定工具和特定操作。

use std::path::Path;
use std::sync::Arc;

/// 工具权限决策
#[derive(Debug, Clone)]
pub enum ToolPermission {
    /// 允许执行
    Allow,
    /// 拒绝执行，附带原因
    Deny { message: String },
}

/// 工具权限检查函数类型
///
/// 接收工具名称和输入参数，返回权限决策。
pub type CanUseToolFn = Arc<dyn Fn(&str, &serde_json::Value) -> ToolPermission + Send + Sync>;

/// 工具 trait 的简化接口（避免依赖完整的 Tool 类型）
///
/// 在实际集成时，这应该与 blockcell_tools 中的 Tool trait 对应。
#[allow(dead_code)]
pub trait ToolInfo {
    fn name(&self) -> &str;
}

/// 创建 Session Memory 提取的工具权限检查
///
/// 只允许编辑特定的 memory 文件
pub fn create_memory_file_can_use_tool(memory_path: &Path) -> CanUseToolFn {
    let memory_path = memory_path.to_path_buf();
    Arc::new(move |tool_name: &str, input: &serde_json::Value| {
        // 只允许 file_edit 工具，且只能编辑 memory_path 文件
        if tool_name == "file_edit" || tool_name == "edit_file" {
            if let Some(file_path) = input.get("file_path").and_then(|v| v.as_str()) {
                if let Some(memory_path_str) = memory_path.to_str() {
                    if file_path == memory_path_str {
                        return ToolPermission::Allow;
                    }
                }
            }
        }

        ToolPermission::Deny {
            message: format!("only file_edit on {} is allowed", memory_path.display()),
        }
    })
}

/// 创建自动记忆提取的工具权限检查
///
/// 允许：
/// - REPL（内部会重新调用此函数）
/// - Read/Grep/Glob（只读）
/// - Bash 只读命令
/// - Edit/Write 仅限 memory 目录内
pub fn create_auto_mem_can_use_tool(memory_dir: &Path) -> CanUseToolFn {
    let memory_dir = memory_dir.to_path_buf();
    Arc::new(move |tool_name: &str, input: &serde_json::Value| {
        // 允许 REPL
        if tool_name == "repl" {
            return ToolPermission::Allow;
        }

        // 允许 Read/Grep/Glob（只读工具）
        if matches!(tool_name, "read_file" | "grep" | "glob") {
            return ToolPermission::Allow;
        }

        // 允许只读 Bash 命令
        if tool_name == "shell" || tool_name == "bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                if is_read_only_command(cmd) {
                    return ToolPermission::Allow;
                }
            }
            return ToolPermission::Deny {
                message: "Only read-only shell commands permitted".to_string(),
            };
        }

        // Edit/Write 仅限记忆目录内
        if matches!(tool_name, "file_edit" | "edit_file" | "file_write" | "write_file") {
            if let Some(file_path) = input.get("file_path").and_then(|v| v.as_str()) {
                if is_auto_mem_path(file_path, &memory_dir) {
                    return ToolPermission::Allow;
                }
            }
            return ToolPermission::Deny {
                message: "Only Edit/Write within memory dir allowed".to_string(),
            };
        }

        // 其他工具全部拒绝
        ToolPermission::Deny {
            message: "only Read/Grep/Glob and Edit/Write within memory dir".to_string(),
        }
    })
}

/// 创建 Dream 整合的工具权限检查
///
/// 与 auto_mem 类似，但允许更广泛的只读操作
pub fn create_dream_can_use_tool(memory_root: &Path) -> CanUseToolFn {
    let memory_root = memory_root.to_path_buf();
    Arc::new(move |tool_name: &str, input: &serde_json::Value| {
        // 允许 REPL
        if tool_name == "repl" {
            return ToolPermission::Allow;
        }

        // 允许只读工具
        if matches!(tool_name, "read_file" | "grep" | "glob" | "ls") {
            return ToolPermission::Allow;
        }

        // Bash 只允许只读命令
        if tool_name == "shell" || tool_name == "bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                if is_read_only_command(cmd) {
                    return ToolPermission::Allow;
                }
            }
            return ToolPermission::Deny {
                message: "Only read-only shell commands permitted".to_string(),
            };
        }

        // Edit/Write 仅限记忆目录
        if matches!(tool_name, "file_edit" | "edit_file" | "file_write" | "write_file") {
            if let Some(file_path) = input.get("file_path").and_then(|v| v.as_str()) {
                // 安全检查：解析符号链接防止路径遍历
                if is_path_within_directory(file_path, &memory_root) {
                    return ToolPermission::Allow;
                }
            }
            return ToolPermission::Deny {
                message: "Only Edit/Write within memory directory".to_string(),
            };
        }

        ToolPermission::Deny {
            message: "Tool not allowed in dream mode".to_string(),
        }
    })
}

/// 创建 Compact 压缩的工具权限检查
///
/// Compact 不需要工具，只生成文本摘要
pub fn create_compact_can_use_tool() -> CanUseToolFn {
    Arc::new(|tool_name: &str, _input: &serde_json::Value| {
        ToolPermission::Deny {
            message: format!(
                "Compact mode does not allow any tools, attempted: {}",
                tool_name
            ),
        }
    })
}

/// 检查是否只读命令
///
/// 安全检查：
/// 1. 命令必须以只读前缀开头
/// 2. 不能包含输出重定向符号 (>, >>)
/// 3. 不能包含管道符号 (|)
/// 4. 不能包含命令替换 ($(), ``)
/// 5. 不能包含换行符（防止命令注入）
/// 6. 不能包含 null 字节
///
/// 注意：`env` 和 `printenv` 已从允许列表中移除，因为可能泄露敏感环境变量。
fn is_read_only_command(cmd: &str) -> bool {
    // 快速拒绝：检查控制字符和危险字符
    if cmd.contains('\0') || cmd.contains('\n') || cmd.contains('\r') {
        return false;
    }

    let read_only_prefixes = [
        "ls", "find", "grep", "cat", "stat", "wc", "head", "tail",
        "git status", "git log", "git diff", "git show", "git branch",
        "echo", "pwd", "which", "whoami",
        "type", "file", "du", "tree",
    ];

    let cmd_trimmed = cmd.trim();
    let cmd_lower = cmd_trimmed.to_lowercase();

    // 安全检查：检测危险符号
    // 1. 输出重定向 (>, >>) - 可能覆盖或追加文件
    // 2. 管道 (|) - 可能将数据传递给写入命令
    // 3. 命令替换 ($(), ``) - 可能执行任意命令
    // 4. 分号 (;) 和逻辑运算符 (&&, ||) - 可能链接多个命令
    // 5. 后台执行 (&) - 可能在后台执行危险操作
    // 6. 换行转义 (\n, \r) - 可能注入新命令
    let dangerous_patterns = [
        ">>", ">", "|", "$(", "`", ";", "&&", "||", "&",
        "\\n", "\\r", "\n", "\r",
    ];

    for pattern in &dangerous_patterns {
        if cmd_lower.contains(pattern) {
            return false;
        }
    }

    // 检查是否以只读前缀开头
    // 使用更严格的匹配：命令必须紧随空格或结束
    read_only_prefixes.iter().any(|&prefix| {
        if cmd_lower == prefix {
            return true;
        }
        if cmd_lower.starts_with(prefix) {
            // 检查前缀后是否是空格或参数结束
            let after_prefix = &cmd_trimmed[prefix.len()..];
            after_prefix.starts_with(' ') || after_prefix.is_empty()
        } else {
            false
        }
    })
}

/// 检查是否在记忆目录内
///
/// 安全检查：解析符号链接，防止路径遍历攻击
fn is_auto_mem_path(path: &str, memory_dir: &Path) -> bool {
    let path = Path::new(path);

    // 首先检查直接路径前缀（快速路径）
    if path.starts_with(memory_dir) {
        // 如果路径存在，验证它不是符号链接或符号链接目标仍在目录内
        if path.exists() {
            if let Ok(canonical) = path.canonicalize() {
                // 确保解析后的路径仍在记忆目录内
                if let Ok(canonical_dir) = memory_dir.canonicalize() {
                    return canonical.starts_with(&canonical_dir);
                }
            }
        }
        // 路径不存在或无法解析，检查父目录
        if let Some(parent) = path.parent() {
            if parent.exists() {
                if let Ok(canonical_parent) = parent.canonicalize() {
                    if let Ok(canonical_dir) = memory_dir.canonicalize() {
                        return canonical_parent.starts_with(&canonical_dir);
                    }
                }
            }
        }
        // 无法验证，保守拒绝（安全优先）
        return false;
    }

    // 路径不以 memory_dir 开头，保守拒绝（安全优先）
    // 注意：移除了基于文件名的 fallback 检查，因为它允许访问任意位置的文件
    // 这是安全漏洞：攻击者可构造 /etc/user.md 等路径绕过目录限制
    false
}

/// 检查路径是否在指定目录内（安全版本，解析符号链接）
fn is_path_within_directory(path: &str, directory: &Path) -> bool {
    let path = Path::new(path);

    // 快速路径：直接前缀检查
    if !path.starts_with(directory) {
        return false;
    }

    // 安全检查：解析符号链接
    // 如果路径存在，解析并验证
    if path.exists() {
        if let Ok(canonical_path) = path.canonicalize() {
            if let Ok(canonical_dir) = directory.canonicalize() {
                return canonical_path.starts_with(&canonical_dir);
            }
        }
    } else {
        // 路径不存在，检查父目录
        if let Some(parent) = path.parent() {
            if parent.exists() {
                if let Ok(canonical_parent) = parent.canonicalize() {
                    if let Ok(canonical_dir) = directory.canonicalize() {
                        return canonical_parent.starts_with(&canonical_dir);
                    }
                }
            }
        }
    }

    // 无法验证，保守拒绝
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_read_only_commands() {
        assert!(is_read_only_command("ls -la"));
        assert!(is_read_only_command("grep pattern file.txt"));
        assert!(is_read_only_command("git status"));
        assert!(is_read_only_command("cat file.txt"));
        assert!(!is_read_only_command("rm file.txt"));
        assert!(!is_read_only_command("npm install"));
    }

    #[test]
    fn test_read_only_command_redirect_detection() {
        // 输出重定向 - 应该被拒绝
        assert!(!is_read_only_command("echo hello > file.txt"));
        assert!(!is_read_only_command("echo hello >> file.txt"));
        assert!(!is_read_only_command("cat file > output.txt"));
        assert!(!is_read_only_command("ls -la > listing.txt"));

        // 管道 - 应该被拒绝
        assert!(!is_read_only_command("ls | grep foo"));
        assert!(!is_read_only_command("cat file | grep pattern"));

        // 命令替换 - 应该被拒绝
        assert!(!is_read_only_command("echo $(cat secret)"));
        assert!(!is_read_only_command("echo `whoami`"));

        // 分号和逻辑运算符 - 应该被拒绝
        assert!(!is_read_only_command("ls; rm file"));
        assert!(!is_read_only_command("ls && rm file"));
        assert!(!is_read_only_command("ls || rm file"));

        // 后台执行 - 应该被拒绝
        assert!(!is_read_only_command("ls &"));

        // 纯只读命令 - 应该通过
        assert!(is_read_only_command("ls -la"));
        assert!(is_read_only_command("echo hello"));
        assert!(is_read_only_command("cat file.txt"));
        assert!(is_read_only_command("grep -r pattern"));
        assert!(is_read_only_command("git status"));
    }

    #[test]
    fn test_memory_file_permission() {
        let memory_path = Path::new("/path/to/memory.md");
        let can_use = create_memory_file_can_use_tool(memory_path);

        // 允许编辑正确的文件
        let result = can_use("file_edit", &json!({"file_path": "/path/to/memory.md"}));
        assert!(matches!(result, ToolPermission::Allow));

        // 拒绝编辑其他文件
        let result = can_use("file_edit", &json!({"file_path": "/other/file.md"}));
        assert!(matches!(result, ToolPermission::Deny { .. }));

        // 拒绝其他工具
        let result = can_use("read_file", &json!({"file_path": "/path/to/memory.md"}));
        assert!(matches!(result, ToolPermission::Deny { .. }));
    }

    #[test]
    fn test_auto_mem_permission() {
        use std::fs;

        // 创建临时目录进行测试
        let temp_dir = std::env::temp_dir().join("blockcell_test_memory");
        fs::create_dir_all(&temp_dir).ok();
        let memory_dir = &temp_dir;

        let can_use = create_auto_mem_can_use_tool(memory_dir);

        // 允许只读工具
        assert!(matches!(
            can_use("read_file", &json!({"file_path": "/any/file"})),
            ToolPermission::Allow
        ));

        // 允许只读 shell 命令
        assert!(matches!(
            can_use("shell", &json!({"command": "ls -la"})),
            ToolPermission::Allow
        ));

        // 拒绝写入 shell 命令
        assert!(matches!(
            can_use("shell", &json!({"command": "rm file"})),
            ToolPermission::Deny { .. }
        ));

        // 允许在 memory 目录内写入（使用临时目录路径）
        let memory_file = temp_dir.join("user.md");
        let memory_file_str = memory_file.to_string_lossy();
        assert!(matches!(
            can_use("file_edit", &json!({"file_path": memory_file_str.as_ref()})),
            ToolPermission::Allow
        ));

        // 拒绝在 memory 目录外写入
        assert!(matches!(
            can_use("file_edit", &json!({"file_path": "/other/file.md"})),
            ToolPermission::Deny { .. }
        ));

        // 清理临时目录
        fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_path_traversal_security() {
        // 测试路径遍历安全：文件名匹配不应绕过目录边界检查
        let memory_dir = Path::new("/safe/memory/dir");

        // 路径不在 memory_dir 内，即使文件名匹配也应返回 false
        assert!(!is_auto_mem_path("/etc/user.md", memory_dir));
        assert!(!is_auto_mem_path("/root/.ssh/project.md", memory_dir));
        assert!(!is_auto_mem_path("/var/log/feedback.md", memory_dir));
        assert!(!is_auto_mem_path("/tmp/reference.md", memory_dir));
        assert!(!is_auto_mem_path("/etc/cron.d/user.md", memory_dir));

        // 路径在 memory_dir 内才应返回 true（如果路径存在且验证通过）
        // 注意：由于路径不存在，此测试中会返回 false，这是正确的安全行为
        let result = is_auto_mem_path("/safe/memory/dir/user.md", memory_dir);
        // 路径不存在时保守拒绝，符合安全优先原则
        assert!(!result);
    }
}