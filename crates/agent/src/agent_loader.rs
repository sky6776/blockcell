//! Agent 定义加载器
//!
//! 从多种来源发现、解析、验证 Agent 定义文件：
//! - 内置 Agent (Rust 代码硬编码)
//! - 用户级 Agent (~/.blockcell/agents/*.md)
//! - 项目级 Agent (<project>/.blockcell/agents/*.md)
//!
//! 加载顺序: Built-in → User-level → Project-level
//! 后加载的同名 Agent 覆盖先加载的

use std::path::{Path, PathBuf};

use crate::agent_types::{
    built_in_agent_types, AgentSource, AgentTypeDefinition, AgentTypeRegistry, IsolationMode,
    PermissionMode,
};

/// Agent 加载错误类型
#[derive(Debug, thiserror::Error)]
pub enum AgentLoadError {
    /// IO 错误
    #[error("读取 {0} 时 IO 错误: {1}")]
    Io(String, #[source] std::io::Error),

    /// YAML 解析错误
    #[error("解析 {0} 时 YAML 错误: {1}")]
    YamlParse(String, #[source] serde_yaml::Error),

    /// 缺少必填字段
    #[error("Agent 文件 {0} 缺少必填字段: {1}")]
    MissingField(String, String),

    /// Agent 类型名无效
    #[error("Agent 文件 {0} 中的类型名 '{1}' 无效: 必须为 3-50 个字母数字/连字符")]
    InvalidTypeName(String, String),

    /// Frontmatter 格式错误
    #[error("Agent 文件 {0} 的 frontmatter 格式错误: {1}")]
    InvalidFrontmatter(String, String),
}

/// YAML frontmatter 中间结构
///
/// 用于从 YAML 解析原始值，再转换为 AgentTypeDefinition
#[derive(Debug, Clone, serde::Deserialize)]
struct AgentFrontmatter {
    /// Agent 类型标识符 (必填，缺失时返回 MissingField 错误)
    #[serde(default)]
    name: String,
    /// 使用场景描述 (必填，缺失时返回 MissingField 错误)
    #[serde(default)]
    description: String,
    /// 允许的工具列表 (逗号分隔)
    #[serde(default)]
    tools: Option<String>,
    /// 禁止的工具列表 (逗号分隔)
    #[serde(default)]
    disallowed_tools: Option<String>,
    /// 最大轮次限制
    #[serde(default)]
    max_turns: Option<u32>,
    /// 权限流模式
    #[serde(default)]
    permission_mode: Option<PermissionMode>,
    /// 隔离模式
    #[serde(default)]
    isolation: Option<IsolationMode>,
    /// 是否一次性
    #[serde(default)]
    one_shot: Option<bool>,
    /// 模型覆盖
    #[serde(default)]
    model: Option<String>,
    /// 预加载技能列表 (逗号分隔)
    #[serde(default)]
    skills: Option<String>,
    /// MCP 服务器引用列表
    #[serde(default)]
    mcp_servers: Option<Vec<String>>,
    /// 首轮提示注入
    #[serde(default)]
    initial_prompt: Option<String>,
    /// 是否后台运行
    #[serde(default)]
    background: Option<bool>,
    /// UI 显示颜色
    #[serde(default)]
    color: Option<String>,
}

/// Agent 定义加载器
///
/// 负责从多种来源发现、解析、验证 Agent 定义：
/// - 内置 Agent (Rust 代码硬编码)
/// - 用户级 Agent (~/.blockcell/agents/*.md)
/// - 项目级 Agent (<project>/.blockcell/agents/*.md)
pub struct AgentDefinitionLoader {
    /// 用户级 agents 目录 (~/.blockcell/agents/)
    user_agents_dir: PathBuf,
    /// 项目级 agents 目录 (<project>/.blockcell/agents/)
    project_agents_dir: Option<PathBuf>,
}

impl AgentDefinitionLoader {
    /// 创建加载器
    ///
    /// # 参数
    /// - `workspace_dir`: 工作空间目录 (通常是 ~/.blockcell)
    /// - `project_dir`: 项目目录 (可选，用于加载项目级 Agent)
    pub fn new(workspace_dir: &Path, project_dir: Option<&Path>) -> Self {
        let user_agents_dir = workspace_dir.join("agents");
        let project_agents_dir = project_dir.map(|d| d.join(".blockcell").join("agents"));
        Self {
            user_agents_dir,
            project_agents_dir,
        }
    }

    /// 从所有来源加载 Agent 定义
    ///
    /// 加载顺序: Built-in → User-level → Project-level
    /// 后加载的同名 Agent 覆盖先加载的
    pub fn load_all(&self) -> AgentTypeRegistry {
        let mut registry = AgentTypeRegistry::new_empty();

        // 1. 加载内置 Agent
        for def in built_in_agent_types() {
            registry.register(def);
        }

        // 2. 加载用户级 Agent
        if self.user_agents_dir.exists() {
            match self.load_from_directory(&self.user_agents_dir, AgentSource::UserLevel) {
                Ok(defs) => {
                    for def in defs {
                        registry.register(def);
                    }
                }
                Err(errors) => {
                    for e in errors {
                        tracing::warn!(error = %e, "加载用户级 Agent 失败");
                    }
                }
            }
        }

        // 3. 加载项目级 Agent
        if let Some(ref project_dir) = self.project_agents_dir {
            if project_dir.exists() {
                match self.load_from_directory(project_dir, AgentSource::ProjectLevel) {
                    Ok(defs) => {
                        for def in defs {
                            registry.register(def);
                        }
                    }
                    Err(errors) => {
                        for e in errors {
                            tracing::warn!(error = %e, "加载项目级 Agent 失败");
                        }
                    }
                }
            }
        }

        registry
    }

    /// 从目录扫描 .md 文件并加载 Agent 定义
    ///
    /// 返回成功解析的定义列表，或错误列表
    fn load_from_directory(
        &self,
        dir: &Path,
        source: AgentSource,
    ) -> Result<Vec<AgentTypeDefinition>, Vec<AgentLoadError>> {
        let mut defs = Vec::new();
        let mut errors = Vec::new();

        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                errors.push(AgentLoadError::Io(dir.display().to_string(), e));
                return Err(errors);
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    errors.push(AgentLoadError::Io(dir.display().to_string(), e));
                    continue;
                }
            };

            let path = entry.path();

            // 只处理 .md 文件
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }

            // 路径安全检查：防止路径遍历
            if let Some(filename) = path.file_name().and_then(|f| f.to_str()) {
                if filename.contains("..") {
                    tracing::warn!(
                        path = %path.display(),
                        "跳过包含路径遍历的 Agent 文件"
                    );
                    continue;
                }
            }

            match parse_agent_markdown(&path, source) {
                Ok(def) => {
                    tracing::info!(
                        agent_type = %def.agent_type,
                        source = ?source,
                        path = %path.display(),
                        "加载自定义 Agent 定义"
                    );
                    defs.push(def);
                }
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "解析 Agent 文件失败"
                    );
                    errors.push(e);
                }
            }
        }

        if errors.is_empty() {
            Ok(defs)
        } else if defs.is_empty() {
            Err(errors)
        } else {
            // 部分成功：返回成功解析的定义，但记录错误
            for e in &errors {
                tracing::warn!(error = %e, "部分 Agent 文件解析失败");
            }
            Ok(defs)
        }
    }
}

/// 解析 Markdown Agent 定义文件
///
/// 文件格式:
/// ```markdown
/// ---
/// name: my-agent
/// description: "使用场景描述"
/// tools: read_file, grep
/// ---
///
/// 系统提示内容
/// ```
fn parse_agent_markdown(
    path: &Path,
    source: AgentSource,
) -> Result<AgentTypeDefinition, AgentLoadError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| AgentLoadError::Io(path.display().to_string(), e))?;

    // 提取文件元数据
    let filename = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string());
    let base_dir = path.parent().map(|p| p.to_path_buf());
    let file_id = path.display().to_string();

    parse_agent_markdown_from_str(&content, source, file_id, filename, base_dir)
}

/// 从字符串内容解析 Agent 定义 (用于测试和内部委托)
///
/// # 参数
/// - `content`: Markdown 内容
/// - `source`: Agent 来源
/// - `file_id`: 文件标识 (用于错误消息)
/// - `filename`: 文件名 (不含扩展名)
/// - `base_dir`: 文件所在目录
pub fn parse_agent_markdown_from_str(
    content: &str,
    source: AgentSource,
    file_id: String,
    filename: Option<String>,
    base_dir: Option<PathBuf>,
) -> Result<AgentTypeDefinition, AgentLoadError> {
    let (frontmatter, body) = extract_frontmatter(content).ok_or_else(|| {
        AgentLoadError::InvalidFrontmatter(
            file_id.clone(),
            "未找到有效的 YAML frontmatter (需要 --- 分隔符)".to_string(),
        )
    })?;

    let raw: AgentFrontmatter = serde_yaml::from_str(frontmatter)
        .map_err(|e| AgentLoadError::YamlParse(file_id.clone(), e))?;

    if raw.name.is_empty() {
        return Err(AgentLoadError::MissingField(file_id, "name".to_string()));
    }
    if raw.description.is_empty() {
        return Err(AgentLoadError::MissingField(
            file_id,
            "description".to_string(),
        ));
    }

    validate_agent_type_name(&raw.name).map_err(|reason| {
        AgentLoadError::InvalidTypeName(file_id, format!("{}: {}", raw.name, reason))
    })?;

    let tools = raw.tools.map(|s| parse_comma_list(&s));
    let disallowed_tools = raw
        .disallowed_tools
        .map(|s| parse_comma_list(&s))
        .unwrap_or_default();

    Ok(AgentTypeDefinition {
        agent_type: raw.name,
        when_to_use: raw.description,
        tools,
        disallowed_tools,
        model: raw.model,
        max_turns: raw.max_turns,
        permission_mode: raw.permission_mode.unwrap_or_default(),
        isolation: raw.isolation,
        one_shot: raw.one_shot.unwrap_or(false),
        skills: raw.skills.map(|s| parse_comma_list(&s)).unwrap_or_default(),
        mcp_servers: raw.mcp_servers.unwrap_or_default(),
        initial_prompt: raw.initial_prompt,
        background: raw.background.unwrap_or(false),
        color: raw.color,
        system_prompt_template: if body.trim().is_empty() {
            None
        } else {
            Some(body)
        },
        source,
        filename,
        base_dir,
    })
}

/// 提取 YAML frontmatter 和正文
///
/// 返回 (frontmatter_str, body_str)，如果格式无效返回 None
fn extract_frontmatter(content: &str) -> Option<(&str, String)> {
    let trimmed = content.trim_start();

    // 必须以 --- 开头
    if !trimmed.starts_with("---") {
        return None;
    }

    // 找到第二个 ---
    let after_first = &trimmed[3..];
    let rest = after_first.trim_start_matches(['\r', '\n']);

    // 查找结束的 ---
    let end_pos = rest.find("\n---").or_else(|| rest.find("\r\n---"))?;

    let frontmatter = &rest[..end_pos];

    // 跳过结束的 "\n---" 分隔符和后续空白
    // after_end 以 "\n---" 开头，需要跳过这 4 个字符，然后 trim 前导换行
    // 注意：不能使用 trim_start_matches('-')，因为正文开头可能有合法的连字符（如 Markdown 水平线）
    let after_end = &rest[end_pos..];
    let after_separator = after_end
        .strip_prefix("\n---")
        .or_else(|| after_end.strip_prefix("\r\n---"))
        .unwrap_or(after_end)
        .trim_start_matches(['\r', '\n']);

    Some((frontmatter, after_separator.to_string()))
}

/// 解析逗号分隔的列表
///
/// "read_file, grep, glob" → ["read_file", "grep", "glob"]
fn parse_comma_list(s: &str) -> Vec<String> {
    s.split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// 验证 Agent 类型名格式
///
/// 规则:
/// - 长度 3-50 个字符
/// - 只允许字母、数字、连字符
/// - 必须以字母或数字开头和结尾
fn validate_agent_type_name(name: &str) -> Result<(), String> {
    if name.len() < 3 {
        return Err(format!("类型名 '{}' 太短 (最少 3 个字符)", name));
    }
    if name.len() > 50 {
        return Err(format!("类型名 '{}' 太长 (最多 50 个字符)", name));
    }

    let chars: Vec<char> = name.chars().collect();

    // 必须以字母或数字开头
    if !chars[0].is_ascii_alphanumeric() {
        return Err(format!("类型名 '{}' 必须以字母或数字开头", name));
    }

    // 必须以字母或数字结尾
    if !chars[chars.len() - 1].is_ascii_alphanumeric() {
        return Err(format!("类型名 '{}' 必须以字母或数字结尾", name));
    }

    // 只允许字母、数字、连字符
    for &ch in &chars {
        if !ch.is_ascii_alphanumeric() && ch != '-' {
            return Err(format!("类型名 '{}' 包含非法字符 '{}'", name, ch));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_frontmatter_basic() {
        let content = r#"---
name: test-agent
description: "测试 Agent"
---

你是测试 Agent。"#;
        let (fm, body) = extract_frontmatter(content).unwrap();
        assert!(fm.contains("name: test-agent"));
        assert!(body.contains("你是测试 Agent"));
    }

    #[test]
    fn test_extract_frontmatter_no_frontmatter() {
        let content = "没有 frontmatter 的内容";
        assert!(extract_frontmatter(content).is_none());
    }

    #[test]
    fn test_parse_agent_markdown_basic() {
        let content = r#"---
name: test-agent
description: "测试 Agent"
tools: read_file, grep
max_turns: 10
---

你是测试 Agent。"#;
        let def = parse_agent_markdown_from_str(
            content,
            AgentSource::UserLevel,
            "<string>".to_string(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(def.agent_type, "test-agent");
        assert_eq!(def.tools, Some(vec!["read_file".into(), "grep".into()]));
        assert_eq!(def.max_turns, Some(10));
        assert_eq!(
            def.system_prompt_template,
            Some("你是测试 Agent。".to_string())
        );
        assert_eq!(def.source, AgentSource::UserLevel);
    }

    #[test]
    fn test_parse_agent_markdown_missing_name() {
        let content = r#"---
description: "没有名称"
---
提示"#;
        let result = parse_agent_markdown_from_str(
            content,
            AgentSource::UserLevel,
            "<string>".to_string(),
            None,
            None,
        );
        assert!(matches!(result, Err(AgentLoadError::MissingField(_, f)) if f == "name"));
    }

    #[test]
    fn test_parse_agent_markdown_missing_description() {
        let content = r#"---
name: my-agent
---
提示"#;
        let result = parse_agent_markdown_from_str(
            content,
            AgentSource::UserLevel,
            "<string>".to_string(),
            None,
            None,
        );
        assert!(matches!(result, Err(AgentLoadError::MissingField(_, f)) if f == "description"));
    }

    #[test]
    fn test_parse_agent_markdown_invalid_name() {
        let content = r#"---
name: "ab"
description: "名称太短"
---
提示"#;
        let result = parse_agent_markdown_from_str(
            content,
            AgentSource::UserLevel,
            "<string>".to_string(),
            None,
            None,
        );
        assert!(matches!(result, Err(AgentLoadError::InvalidTypeName(_, _))));
    }

    #[test]
    fn test_parse_agent_markdown_full_fields() {
        let content = r#"---
name: code-reviewer
description: "代码审查专家"
tools: read_file, grep, glob
disallowed_tools: exec, write_file
max_turns: 20
permission_mode: Inherit
one_shot: true
model: deepseek-chat
skills: review, simplify
mcp_servers:
  - filesystem
initial_prompt: "关注安全漏洞"
background: false
color: blue
---

你是代码审查专家。"#;
        let def = parse_agent_markdown_from_str(
            content,
            AgentSource::ProjectLevel,
            "<string>".to_string(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(def.agent_type, "code-reviewer");
        assert_eq!(
            def.tools,
            Some(vec!["read_file".into(), "grep".into(), "glob".into()])
        );
        assert!(def.disallowed_tools.contains(&"exec".to_string()));
        assert!(def.disallowed_tools.contains(&"write_file".to_string()));
        assert_eq!(def.max_turns, Some(20));
        assert_eq!(def.permission_mode, PermissionMode::Inherit);
        assert!(def.one_shot);
        assert_eq!(def.model, Some("deepseek-chat".to_string()));
        assert_eq!(
            def.skills,
            vec!["review".to_string(), "simplify".to_string()]
        );
        assert_eq!(def.mcp_servers, vec!["filesystem".to_string()]);
        assert_eq!(def.initial_prompt, Some("关注安全漏洞".to_string()));
        assert_eq!(def.color, Some("blue".to_string()));
        assert_eq!(def.source, AgentSource::ProjectLevel);
    }

    #[test]
    fn test_validate_agent_type_name_valid() {
        assert!(validate_agent_type_name("explore").is_ok());
        assert!(validate_agent_type_name("code-reviewer").is_ok());
        assert!(validate_agent_type_name("a123").is_ok());
        assert!(validate_agent_type_name("my-agent-v2").is_ok());
    }

    #[test]
    fn test_validate_agent_type_name_invalid() {
        // 太短
        assert!(validate_agent_type_name("ab").is_err());
        // 以连字符开头
        assert!(validate_agent_type_name("-agent").is_err());
        // 以连字符结尾
        assert!(validate_agent_type_name("agent-").is_err());
        // 包含空格
        assert!(validate_agent_type_name("my agent").is_err());
        // 包含下划线
        assert!(validate_agent_type_name("my_agent").is_err());
    }

    #[test]
    fn test_parse_comma_list() {
        assert_eq!(
            parse_comma_list("read_file, grep, glob"),
            vec!["read_file", "grep", "glob"]
        );
        assert_eq!(parse_comma_list("single"), vec!["single"]);
        assert_eq!(parse_comma_list(""), Vec::<String>::new());
        assert_eq!(parse_comma_list("a, b, , c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_registry_priority_override() {
        let mut registry = AgentTypeRegistry::new();
        // 内置 explore 存在
        assert!(registry.get("explore").is_some());
        // 加载自定义 explore (项目级，应覆盖)
        let custom = AgentTypeDefinition {
            agent_type: "explore".to_string(),
            when_to_use: "自定义 explore".to_string(),
            source: AgentSource::ProjectLevel,
            ..Default::default()
        };
        registry.register(custom);
        assert_eq!(
            registry.get("explore").unwrap().when_to_use,
            "自定义 explore"
        );
    }

    #[test]
    fn test_recursive_spawn_guard() {
        let mut registry = AgentTypeRegistry::new_empty();
        let def = AgentTypeDefinition {
            agent_type: "custom".to_string(),
            when_to_use: "自定义".to_string(),
            disallowed_tools: vec![], // 未包含 agent/spawn
            ..Default::default()
        };
        registry.register(def);
        let registered = registry.get("custom").unwrap();
        assert!(registered.disallowed_tools.contains(&"agent".to_string()));
        assert!(registered.disallowed_tools.contains(&"spawn".to_string()));
    }

    #[test]
    fn test_loader_new() {
        let workspace = PathBuf::from("/home/user/.blockcell");
        let project = PathBuf::from("/home/user/project");
        let loader = AgentDefinitionLoader::new(&workspace, Some(&project));
        assert_eq!(
            loader.user_agents_dir,
            PathBuf::from("/home/user/.blockcell/agents")
        );
        assert_eq!(
            loader.project_agents_dir,
            Some(PathBuf::from("/home/user/project/.blockcell/agents"))
        );
    }
}
