//! 记忆类型定义
//!
//! 四种持久化记忆类型及其存储路径。

use std::path::{Path, PathBuf};

/// 记忆类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum MemoryType {
    /// 用户记忆 - 角色、偏好、知识背景
    User,
    /// 项目记忆 - 工作内容、目标、事件
    Project,
    /// 反馈记忆 - 用户纠正、工作指导
    Feedback,
    /// 引用记忆 - 外部系统资源指针
    Reference,
}

impl MemoryType {
    /// 获取所有记忆类型
    pub fn all() -> Vec<Self> {
        vec![Self::User, Self::Project, Self::Feedback, Self::Reference]
    }

    /// 获取记忆类型名称
    pub fn name(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Feedback => "feedback",
            Self::Reference => "reference",
        }
    }

    /// 获取记忆文件名
    pub fn filename(&self) -> &'static str {
        match self {
            Self::User => "user.md",
            Self::Project => "project.md",
            Self::Feedback => "feedback.md",
            Self::Reference => "reference.md",
        }
    }

    /// 获取记忆类型描述
    pub fn description(&self) -> &'static str {
        match self {
            Self::User => "用户角色、偏好、知识背景",
            Self::Project => "项目工作、目标、事件",
            Self::Feedback => "用户纠正、工作指导",
            Self::Reference => "外部系统资源指针",
        }
    }

    /// 获取记忆类型用途说明
    pub fn usage_guide(&self) -> &'static str {
        match self {
            Self::User => {
                "记录用户的永久信息，用于个性化 AI 助手：\n\
                - 用户角色和职责\n\
                - 技术背景和知识领域\n\
                - 代码风格偏好\n\
                - 沟通习惯\n\
                - 常见请求模式\n\n\
                **何时保存**: 当用户提供个人信息或表达偏好时\n\
                **如何使用**: 为用户提供个性化建议和解释"
            }
            Self::Project => {
                "记录项目相关的持久信息：\n\
                - 项目目标和里程碑\n\
                - 当前进度和状态\n\
                - 重要决策和决策理由\n\
                - 技术栈和架构选择\n\
                - 团队成员和职责分配\n\n\
                **何时保存**: 当讨论项目进度、决策或计划时\n\
                **如何使用**: 保持项目上下文连贯性"
            }
            Self::Feedback => {
                "记录用户的纠正和指导，改进 AI 行为：\n\
                - 用户明确纠正的行为\n\
                - 用户要求的工作方式\n\
                - 成功或失败的方法记录\n\
                - 需要避免的做法\n\n\
                **格式**: 每条包含规则 + 原因 + 应用场景\n\n\
                **何时保存**: 当用户说 \"不要做X\" 或 \"保持做Y\" 时\n\
                **如何使用**: 在后续对话中遵循用户指导"
            }
            Self::Reference => {
                "记录外部系统资源的指针：\n\
                - 文档链接和位置\n\
                - API 端点和配置\n\
                - 外部系统访问方式\n\
                - 重要文件路径\n\n\
                **何时保存**: 当用户提到外部资源时\n\
                **如何使用**: 快速定位和访问外部系统"
            }
        }
    }

    /// 获取记忆文件模板
    pub fn template(&self) -> &'static str {
        match self {
            Self::User => {
                "---\n\
                name: user_memory\n\
                description: 用户角色、偏好、知识背景\n\
                type: user\n\
                ---\n\n\
                # User Memory\n\n\
                此文件记录关于用户的永久信息。\n\n\
                ## Role and Responsibilities\n\n\
                _用户的主要角色和工作职责._\n\n\
                ## Technical Background\n\n\
                _用户的技术背景和知识领域._\n\n\
                ## Preferences\n\n\
                _用户的偏好和习惯._\n\n\
                ## Common Requests\n\n\
                _用户常见的请求模式._"
            }
            Self::Project => {
                "---\n\
                name: project_memory\n\
                description: 项目工作、目标、事件\n\
                type: project\n\
                ---\n\n\
                # Project Memory\n\n\
                此文件记录项目相关的持久信息。\n\n\
                ## Project Goals\n\n\
                _项目的主要目标和里程碑._\n\n\
                ## Current Status\n\n\
                _当前进度和状态._\n\n\
                ## Key Decisions\n\n\
                _重要决策及其理由._\n\n\
                ## Team Structure\n\n\
                _团队成员和职责._"
            }
            Self::Feedback => {
                "---\n\
                name: feedback_memory\n\
                description: 用户纠正、工作指导\n\
                type: feedback\n\
                ---\n\n\
                # Feedback Memory\n\n\
                此文件记录用户的纠正和指导。\n\n\
                **格式**: 每条记录包含规则、原因和应用场景。\n\n\
                ## Work Guidance\n\n\
                _用户要求的工作方式._\n\n\
                ## Corrections\n\n\
                _用户纠正的行为._\n\n\
                ## Approaches to Avoid\n\n\
                _需要避免的做法._"
            }
            Self::Reference => {
                "---\n\
                name: reference_memory\n\
                description: 外部系统资源指针\n\
                type: reference\n\
                ---\n\n\
                # Reference Memory\n\n\
                此文件记录外部系统资源的指针。\n\n\
                ## Documentation\n\n\
                _重要文档链接和位置._\n\n\
                ## External Systems\n\n\
                _外部系统访问方式._\n\n\
                ## API References\n\n\
                _API 端点和配置._"
            }
        }
    }
}

/// 记忆文件名映射
pub const MEMORY_FILE_NAMES: &[(&str, &str)] = &[
    ("user", "user.md"),
    ("project", "project.md"),
    ("feedback", "feedback.md"),
    ("reference", "reference.md"),
];

/// 获取记忆文件路径
pub fn get_memory_file_path(config_dir: &Path, memory_type: MemoryType) -> PathBuf {
    config_dir
        .join("memory")
        .join(memory_type.name())
        .with_extension("md")
}

/// 确保记忆目录存在
#[allow(dead_code)]
pub async fn ensure_memory_dir(config_dir: &Path) -> std::io::Result<()> {
    let memory_dir = config_dir.join("memory");
    tokio::fs::create_dir_all(&memory_dir).await?;

    // 为每种类型创建初始文件（如果不存在）
    for memory_type in MemoryType::all() {
        let file_path = get_memory_file_path(config_dir, memory_type);
        if !tokio::fs::try_exists(&file_path).await? {
            tokio::fs::write(&file_path, memory_type.template()).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_type_all() {
        let types = MemoryType::all();
        assert_eq!(types.len(), 4);
    }

    #[test]
    fn test_memory_type_names() {
        assert_eq!(MemoryType::User.name(), "user");
        assert_eq!(MemoryType::Project.name(), "project");
        assert_eq!(MemoryType::Feedback.name(), "feedback");
        assert_eq!(MemoryType::Reference.name(), "reference");
    }

    #[test]
    fn test_get_memory_file_path() {
        let config_dir = Path::new("/config");
        let path = get_memory_file_path(config_dir, MemoryType::User);
        // Check path components instead of string representation (platform-independent)
        assert!(path.ends_with("user.md"));
        assert!(path.to_str().unwrap().contains("memory"));
    }

    #[test]
    fn test_memory_type_templates() {
        for mt in MemoryType::all() {
            let template = mt.template();
            assert!(template.contains("---")); // YAML frontmatter
            assert!(template.contains("# ")); // Markdown header
        }
    }
}