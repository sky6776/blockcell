//! Session Memory 10-Section 模板
//!
//! 定义标准化的会话信息结构和模板。

use crate::token::estimate_tokens;
use std::path::PathBuf;
use tokio::fs;

/// 默认 10-Section 模板
pub const DEFAULT_SESSION_MEMORY_TEMPLATE: &str = r#"# Session Title
_A short and distinctive 5-10 word descriptive title for the session. Super info dense, no filler._

# Current State
_What is actively being worked on right now? Pending tasks not yet completed. Immediate next steps._

# Task specification
_What did the user ask to build? Any design decisions or other explanatory context._

# Files and Functions
_What are the important files? In short, what do they contain and why are they relevant?_

# Workflow
_What bash commands are usually run and in what order? How to interpret their output if not obvious?_

# Errors & Corrections
_Errors encountered and how they were fixed. What did the user correct? What approaches failed and should not be tried again?_

# Codebase and System Documentation
_What are the important system components? How do they work/fit together?_

# Learnings
_What has worked well? What has not? What to avoid? Do not duplicate items from other sections._

# Key results
_If the user asked a specific output such as an answer to a question, a table, or other document, repeat the exact result here._

# Worklog
_Step by step, what was attempted, done? Very terse summary for each step._
"#;

/// Section 定义
#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub description: String,
    pub priority: SectionPriority,
}

/// Section 优先级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SectionPriority {
    /// 最高优先级 (Current State, Errors & Corrections)
    Highest = 4,
    /// 高优先级 (Task specification, Files, Key results)
    High = 3,
    /// 中等优先级 (Workflow, Codebase, Learnings)
    Medium = 2,
    /// 低优先级 (Worklog)
    Low = 1,
}

impl Section {
    /// 获取所有 Section 定义
    pub fn all() -> Vec<Self> {
        vec![
            Self {
                title: "Session Title".to_string(),
                description: "A short and distinctive 5-10 word descriptive title".to_string(),
                priority: SectionPriority::High,
            },
            Self {
                title: "Current State".to_string(),
                description: "What is actively being worked on right now".to_string(),
                priority: SectionPriority::Highest,
            },
            Self {
                title: "Task specification".to_string(),
                description: "What did the user ask to build".to_string(),
                priority: SectionPriority::High,
            },
            Self {
                title: "Files and Functions".to_string(),
                description: "What are the important files".to_string(),
                priority: SectionPriority::High,
            },
            Self {
                title: "Workflow".to_string(),
                description: "What bash commands are usually run".to_string(),
                priority: SectionPriority::Medium,
            },
            Self {
                title: "Errors & Corrections".to_string(),
                description: "Errors encountered and how they were fixed".to_string(),
                priority: SectionPriority::Highest,
            },
            Self {
                title: "Codebase and System Documentation".to_string(),
                description: "What are the important system components".to_string(),
                priority: SectionPriority::Medium,
            },
            Self {
                title: "Learnings".to_string(),
                description: "What has worked well? What has not?".to_string(),
                priority: SectionPriority::Medium,
            },
            Self {
                title: "Key results".to_string(),
                description: "If the user asked a specific output".to_string(),
                priority: SectionPriority::High,
            },
            Self {
                title: "Worklog".to_string(),
                description: "Step by step, what was attempted".to_string(),
                priority: SectionPriority::Low,
            },
        ]
    }

    /// 根据 title 查找 Section
    pub fn find_by_title(title: &str) -> Option<Self> {
        Self::all().into_iter().find(|s| s.title == title)
    }
}

/// 自定义模板路径
#[allow(dead_code)]
pub fn get_custom_template_path(config_dir: &std::path::Path) -> PathBuf {
    config_dir
        .join("session-memory")
        .join("config")
        .join("template.md")
}

/// 加载模板
#[allow(dead_code)]
pub async fn load_session_memory_template(config_dir: &std::path::Path) -> Result<String, std::io::Error> {
    let template_path = get_custom_template_path(config_dir);

    match fs::read_to_string(&template_path).await {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(DEFAULT_SESSION_MEMORY_TEMPLATE.to_string())
        }
        Err(e) => Err(e),
    }
}

/// 截断 Session Memory 用于 Compact
///
/// 当 Session Memory 超过 token 限制时，按优先级截断
pub fn truncate_session_memory_for_compact(content: &str, max_tokens: usize) -> (String, bool) {
    let current_tokens = estimate_tokens(content);
    if current_tokens <= max_tokens {
        return (content.to_string(), false);
    }

    // 按节截断
    let mut sections: Vec<(String, usize)> = Vec::new();
    let mut current_section = String::new();
    let mut in_section = false;

    for line in content.lines() {
        if line.starts_with("# ") {
            if !current_section.is_empty() {
                sections.push((current_section.clone(), estimate_tokens(&current_section)));
            }
            current_section = line.to_string() + "\n";
            in_section = true;
        } else if in_section {
            current_section.push_str(line);
            current_section.push('\n');
        }
    }
    if !current_section.is_empty() {
        sections.push((current_section.clone(), estimate_tokens(&current_section)));
    }

    // 按优先级保留
    let mut result = String::new();
    let mut used_tokens = 0;
    let section_defs = Section::all();

    for section_def in section_defs.iter() {
        if let Some((content, tokens)) = sections
            .iter()
            .find(|(c, _)| c.starts_with(&format!("# {}", section_def.title)))
        {
            let available = max_tokens.saturating_sub(used_tokens);
            if *tokens <= available {
                result.push_str(content);
                used_tokens += *tokens;
            } else {
                // 截断此节（安全处理 UTF-8 边界）
                let char_budget = available * 4;
                let budget = char_budget.min(content.len());
                // 找到安全的 UTF-8 边界
                let mut boundary = budget;
                while boundary > 0 && !content.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                let truncated = format!(
                    "{}\n[... section truncated for length ...]\n",
                    &content[..boundary]
                );
                result.push_str(&truncated);
                used_tokens += available;
            }
        }
    }

    (result, true)
}

/// 检查 Session Memory 是否为空
pub fn is_session_memory_empty(content: &str) -> bool {
    // 检查是否只有模板结构，没有实际内容
    for line in content.lines() {
        // 跳过标题行和描述行
        if line.starts_with("# ") || (line.starts_with("_") && line.ends_with("_")) {
            continue;
        }
        // 如果有非空内容
        if !line.trim().is_empty() {
            return false;
        }
    }
    true
}

/// 精确 token 估算 (使用 tiktoken)
#[allow(dead_code)]
pub fn rough_token_count(text: &str) -> usize {
    estimate_tokens(text)
}

/// 验证 Session Memory 内容完整性
///
/// 检查所有必需的 section headers 和 description lines 是否存在。
/// 返回验证结果和缺失的 sections 列表。
#[derive(Debug, Clone)]
pub struct ValidationResult {
    /// 是否验证通过
    pub is_valid: bool,
    /// 缺失的 section titles
    pub missing_sections: Vec<String>,
    /// 格式错误的 section descriptions
    pub malformed_descriptions: Vec<String>,
}

impl ValidationResult {
    /// 创建一个成功的验证结果
    pub fn success() -> Self {
        Self {
            is_valid: true,
            missing_sections: Vec::new(),
            malformed_descriptions: Vec::new(),
        }
    }

    /// 创建一个失败的验证结果
    pub fn failure(missing_sections: Vec<String>, malformed_descriptions: Vec<String>) -> Self {
        Self {
            is_valid: false,
            missing_sections,
            malformed_descriptions,
        }
    }
}

/// 验证 Session Memory 内容完整性
///
/// ## 检查项
/// 1. 所有 10 个 section headers (`# Section Title`) 必须存在
///
/// 注意：description lines (`_..._`) 是模板占位符，LLM 更新后会被替换为实际内容，
/// 因此不验证 description lines 格式。
pub fn validate_session_memory(content: &str) -> ValidationResult {
    let required_sections = Section::all();
    let mut missing_sections: Vec<String> = Vec::new();

    let lines: Vec<&str> = content.lines().collect();

    for section in &required_sections {
        let header_pattern = format!("# {}", section.title);
        let header_found = lines.iter().any(|line| {
            let trimmed = line.trim();
            trimmed == header_pattern || trimmed.starts_with(&format!("{} ", header_pattern))
        });

        if !header_found {
            missing_sections.push(section.title.clone());
        }
    }

    if missing_sections.is_empty() {
        ValidationResult::success()
    } else {
        ValidationResult::failure(missing_sections, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_section_all() {
        let sections = Section::all();
        assert_eq!(sections.len(), 10);

        // 检查优先级
        let current_state = Section::find_by_title("Current State").unwrap();
        assert_eq!(current_state.priority, SectionPriority::Highest);

        let worklog = Section::find_by_title("Worklog").unwrap();
        assert_eq!(worklog.priority, SectionPriority::Low);
    }

    #[test]
    fn test_is_session_memory_empty() {
        // 空模板
        assert!(is_session_memory_empty(DEFAULT_SESSION_MEMORY_TEMPLATE));

        // 有内容
        let with_content = r#"# Session Title
_A title._

# Current State
Working on something.
"#;
        assert!(!is_session_memory_empty(with_content));
    }

    #[test]
    fn test_truncate_session_memory() {
        let content = r#"# Session Title
Test session title

# Current State
Working on test

# Worklog
Step 1
Step 2
Step 3
"#;
        let (_truncated, was_truncated) = truncate_session_memory_for_compact(content, 1000);
        assert!(!was_truncated); // 内容足够短，不需要截断

        let (_truncated2, was_truncated2) = truncate_session_memory_for_compact(content, 10);
        assert!(was_truncated2); // 内容太长，需要截断
    }

    #[test]
    fn test_validate_session_memory_valid() {
        // 使用默认模板，应该验证通过
        let result = validate_session_memory(DEFAULT_SESSION_MEMORY_TEMPLATE);
        assert!(result.is_valid);
        assert!(result.missing_sections.is_empty());
        assert!(result.malformed_descriptions.is_empty());
    }

    #[test]
    fn test_validate_session_memory_missing_section() {
        // 缺少一个 section
        let content = r#"# Session Title
_A title._

# Current State
_Working._

# Task specification
_Task._

# Files and Functions
_Files._

# Workflow
_Workflow._

# Errors & Corrections
_Errors._

# Codebase and System Documentation
_Docs._

# Learnings
_Learnings._

# Key results
_Results._
"#;
        // 缺少 Worklog section
        let result = validate_session_memory(content);
        assert!(!result.is_valid);
        assert!(result.missing_sections.contains(&"Worklog".to_string()));
    }

    #[test]
    fn test_validate_session_memory_with_content() {
        // 有实际内容的完整模板（LLM 更新后的格式）
        let content = r#"# Session Title
My test session

# Current State
Working on validation

# Task specification
User asked to validate session memory

# Files and Functions
src/template.rs - validation logic

# Workflow
cargo test

# Errors & Corrections
None yet

# Codebase and System Documentation
Session memory uses 10 sections

# Learnings
Validation is important

# Key results
Tests pass

# Worklog
1. Added validation
2. Added tests
"#;
        let result = validate_session_memory(content);
        assert!(result.is_valid);
    }
}