use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::agent_prompts::{
    EXPLORE_SYSTEM_PROMPT, GENERAL_SYSTEM_PROMPT, PLAN_SYSTEM_PROMPT, VERIFICATION_SYSTEM_PROMPT,
    VIPER_SYSTEM_PROMPT,
};

/// Permission mode for agent types
///
/// Determines how permissions flow between parent and child agents:
/// - `Inherit`: Child agent inherits parent's permissions (default)
/// - `Bubble`: Permission requests bubble up to parent agent
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum PermissionMode {
    /// Inherit parent's permissions
    #[default]
    Inherit,
    /// Permission requests bubble up to parent
    Bubble,
}

/// Isolation mode for agent execution
///
/// Determines whether the agent runs in an isolated environment:
/// - `None`: No isolation, shares the working directory (default)
/// - `Worktree`: Git worktree isolation for code-writing agents
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub enum IsolationMode {
    /// No isolation, shares working directory
    #[default]
    None,
    /// Git worktree isolation for code-writing agents
    Worktree,
}

/// Agent type definition
///
/// Defines the configuration for an agent type, including:
/// - Type identifier and usage scenario description
/// - Disallowed tools list
/// - Optional max turns limit
/// - Optional system prompt template
/// - ONE_SHOT flag (cannot continue with SendMessage after completion)
/// - Permission mode for permission flow between parent and child agents
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentTypeDefinition {
    /// Agent type identifier
    pub agent_type: String,

    /// Usage scenario description (injected into System Prompt)
    pub when_to_use: String,

    /// List of disallowed tools
    pub disallowed_tools: Vec<String>,

    /// Maximum turns limit (optional)
    pub max_turns: Option<u32>,

    /// System prompt template (optional)
    pub system_prompt_template: Option<String>,

    /// ONE_SHOT flag - if true, cannot continue with SendMessage after completion
    pub one_shot: bool,

    /// Permission mode for permission flow
    pub permission_mode: PermissionMode,

    /// Isolation mode (optional) - determines execution environment isolation
    pub isolation: Option<IsolationMode>,
}

impl Default for AgentTypeDefinition {
    fn default() -> Self {
        Self {
            agent_type: "general".to_string(),
            when_to_use: String::new(),
            disallowed_tools: vec![],
            max_turns: None,
            system_prompt_template: None,
            one_shot: false,
            permission_mode: PermissionMode::default(),
            isolation: None,
        }
    }
}

impl AgentTypeDefinition {
    /// Create an explore agent type definition
    pub fn explore() -> Self {
        Self {
            agent_type: "explore".to_string(),
            when_to_use: "Fast agent specialized for exploring codebases. Use this when you need to quickly find files by patterns (eg. \"src/components/**/*.tsx\"), search code for keywords (eg. \"API endpoints\"), or answer questions about the codebase (eg. \"how do API endpoints work?\"). When calling this agent, specify the desired thoroughness level: \"quick\" for basic searches, \"medium\" for moderate exploration, or \"very thorough\" for comprehensive analysis across multiple locations and naming conventions.".to_string(),
            disallowed_tools: ["agent", "spawn", "write_file", "edit_file", "exec"].iter().map(|s| s.to_string()).collect(),
            max_turns: Some(20),
            system_prompt_template: Some(EXPLORE_SYSTEM_PROMPT.to_string()),
            one_shot: true,
            permission_mode: PermissionMode::Inherit,
            isolation: None,
        }
    }

    /// Create a plan agent type definition
    pub fn plan() -> Self {
        Self {
            agent_type: "plan".to_string(),
            when_to_use: "Software architect agent for designing implementation plans. Use this when you need to plan the implementation strategy for a task. Returns step-by-step plans, identifies critical files, and considers architectural trade-offs.".to_string(),
            disallowed_tools: ["agent", "spawn", "write_file", "edit_file", "exec"].iter().map(|s| s.to_string()).collect(),
            max_turns: Some(30),
            system_prompt_template: Some(PLAN_SYSTEM_PROMPT.to_string()),
            one_shot: true,
            permission_mode: PermissionMode::Inherit,
            isolation: None,
        }
    }

    /// Create a verification agent type definition
    pub fn verification() -> Self {
        Self {
            agent_type: "verification".to_string(),
            when_to_use: "Verification specialist for testing and validating implementations. Tries to break the implementation.".to_string(),
            disallowed_tools: ["agent", "spawn", "write_file", "edit_file", "exec"].iter().map(|s| s.to_string()).collect(),
            max_turns: Some(15),
            system_prompt_template: Some(VERIFICATION_SYSTEM_PROMPT.to_string()),
            one_shot: false,
            permission_mode: PermissionMode::Inherit,
            isolation: None,
        }
    }

    /// Create a viper agent type definition
    pub fn viper() -> Self {
        Self {
            agent_type: "viper".to_string(),
            when_to_use: "Implementation agent for writing production code, adding features, and refactoring.".to_string(),
            disallowed_tools: ["agent", "spawn"].iter().map(|s| s.to_string()).collect(),
            max_turns: Some(50),
            system_prompt_template: Some(VIPER_SYSTEM_PROMPT.to_string()),
            one_shot: false,
            permission_mode: PermissionMode::Bubble,
            isolation: Some(IsolationMode::Worktree), // viper 写代码，需要 worktree 隔离
        }
    }

    /// Create a general agent type definition
    pub fn general() -> Self {
        Self {
            agent_type: "general".to_string(),
            when_to_use: "General-purpose agent for complex multi-step tasks that don't fit specialized profiles.".to_string(),
            // 禁止 agent/spawn 防止无限递归 spawn
            disallowed_tools: ["agent", "spawn"].iter().map(|s| s.to_string()).collect(),
            max_turns: None,
            system_prompt_template: Some(GENERAL_SYSTEM_PROMPT.to_string()),
            one_shot: false,
            permission_mode: PermissionMode::Bubble,
            isolation: None,
        }
    }
}

/// Agent type registry
///
/// Manages registration and lookup of agent type definitions.
/// Supports both built-in types and custom types registered at runtime.
#[derive(Clone)]
pub struct AgentTypeRegistry {
    types: HashMap<String, AgentTypeDefinition>,
}

impl AgentTypeRegistry {
    /// Create an empty registry (for testing)
    pub fn new_empty() -> Self {
        Self {
            types: HashMap::new(),
        }
    }

    /// Create a registry with built-in types
    pub fn new() -> Self {
        let mut registry = Self::new_empty();
        for def in built_in_agent_types() {
            registry.register(def);
        }
        registry
    }

    /// Register an agent type. If a type with the same name already exists,
    /// it is overwritten with a warning log.
    ///
    /// Safety: Automatically adds "agent" and "spawn" to disallowed_tools
    /// to prevent recursive spawning (infinite recursion).
    pub fn register(&mut self, mut def: AgentTypeDefinition) {
        // 安全防护：确保所有 agent 类型都禁止递归 spawn
        // 子 agent 不应创建更深层的子 agent，防止无限递归
        let forbidden = ["agent", "spawn"];
        for tool in forbidden {
            if !def.disallowed_tools.iter().any(|t| t == tool) {
                tracing::warn!(
                    agent_type = %def.agent_type,
                    tool = tool,
                    "Auto-adding '{}' to disallowed_tools to prevent recursive agent spawning",
                    tool
                );
                def.disallowed_tools.push(tool.to_string());
            }
        }

        if self.types.contains_key(&def.agent_type) {
            tracing::warn!(
                agent_type = %def.agent_type,
                "Overwriting existing agent type definition"
            );
        }
        self.types.insert(def.agent_type.clone(), def);
    }

    /// Get an agent type definition
    pub fn get(&self, agent_type: &str) -> Option<&AgentTypeDefinition> {
        self.types.get(agent_type)
    }

    /// Iterate over all types
    pub fn iter(&self) -> impl Iterator<Item = (&String, &AgentTypeDefinition)> {
        self.types.iter()
    }

    /// Get type names (for schema enum)
    pub fn type_names(&self) -> Vec<&str> {
        self.types.keys().map(|s| s.as_str()).collect()
    }

    /// 验证 disallowed_tools 中的工具名是否存在于工具注册表
    ///
    /// 返回 (agent_type, 无效工具列表) 对，用于检测配置错误。
    pub fn validate_disallowed_tools(&self, known_tools: &[&str]) -> Vec<(&str, Vec<String>)> {
        let known_set: std::collections::HashSet<&str> = known_tools.iter().copied().collect();
        let mut invalid = Vec::new();

        for (type_name, def) in &self.types {
            let bad: Vec<String> = def
                .disallowed_tools
                .iter()
                .filter(|t| !known_set.contains(t.as_str()))
                .cloned()
                .collect();
            if !bad.is_empty() {
                invalid.push((type_name.as_str(), bad));
            }
        }
        invalid
    }
}

impl Default for AgentTypeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Predefined agent types
///
/// Built-in agent types provide specialized behaviors for common tasks:
/// - `explore`: Fast read-only codebase exploration
/// - `plan`: Architecture and implementation planning
/// - `verification`: Testing and validation specialist
/// - `viper`: Implementation agent for writing production code
/// - `general`: General-purpose agent for complex multi-step tasks
pub fn built_in_agent_types() -> Vec<AgentTypeDefinition> {
    vec![
        AgentTypeDefinition::explore(),
        AgentTypeDefinition::plan(),
        AgentTypeDefinition::verification(),
        AgentTypeDefinition::viper(),
        AgentTypeDefinition::general(),
    ]
}

/// Lazy static reference to built-in agent types
///
/// This provides a convenient way to access built-in types without
/// calling the function each time. Uses std::sync::OnceLock for safe one-time initialization.
#[macro_export]
macro_rules! built_in_agent_types_lazy {
    () => {{
        static TYPES: std::sync::OnceLock<Vec<AgentTypeDefinition>> = std::sync::OnceLock::new();
        TYPES.get_or_init(built_in_agent_types)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_permission_mode_default_is_inherit() {
        let mode = PermissionMode::default();
        assert!(matches!(mode, PermissionMode::Inherit));
    }

    #[test]
    fn test_permission_mode_serde_serialize() {
        let mode = PermissionMode::Bubble;
        let json = serde_json::to_string(&mode).unwrap();
        assert_eq!(json, "\"Bubble\"");
    }

    #[test]
    fn test_permission_mode_serde_deserialize() {
        let json = "\"Inherit\"";
        let mode: PermissionMode = serde_json::from_str(json).unwrap();
        assert_eq!(mode, PermissionMode::Inherit);
    }

    #[test]
    fn test_permission_mode_serde_deserialize_bubble() {
        let json = "\"Bubble\"";
        let mode: PermissionMode = serde_json::from_str(json).unwrap();
        assert_eq!(mode, PermissionMode::Bubble);
    }

    #[test]
    fn test_permission_mode_debug_output() {
        let mode = PermissionMode::Inherit;
        let debug_str = format!("{:?}", mode);
        assert_eq!(debug_str, "Inherit");
    }

    #[test]
    fn test_permission_mode_clone() {
        let mode = PermissionMode::Bubble;
        let cloned = mode.clone();
        assert_eq!(mode, cloned);
    }

    #[test]
    fn test_built_in_agent_types_count() {
        let types = built_in_agent_types();
        assert_eq!(types.len(), 5);
    }

    #[test]
    fn test_agent_type_definition_explore() {
        let def = AgentTypeDefinition::explore();
        assert_eq!(def.agent_type, "explore");
        assert!(def.one_shot);
        assert_eq!(def.permission_mode, PermissionMode::Inherit);
    }

    #[test]
    fn test_agent_type_registry_new() {
        let registry = AgentTypeRegistry::new();
        assert_eq!(registry.type_names().len(), 5);
        assert!(registry.get("explore").is_some());
        assert!(registry.get("plan").is_some());
        assert!(registry.get("verification").is_some());
        assert!(registry.get("viper").is_some());
        assert!(registry.get("general").is_some());
    }

    #[test]
    fn test_agent_type_registry_register() {
        let mut registry = AgentTypeRegistry::new_empty();
        let custom_def = AgentTypeDefinition {
            agent_type: "custom".to_string(),
            when_to_use: "Custom agent type".to_string(),
            disallowed_tools: vec!["exec".to_string()],
            max_turns: Some(10),
            system_prompt_template: None,
            one_shot: false,
            permission_mode: PermissionMode::Inherit,
            isolation: None,
        };
        registry.register(custom_def);
        assert!(registry.get("custom").is_some());
    }

    #[test]
    fn test_validate_disallowed_tools_all_valid() {
        let registry = AgentTypeRegistry::new();
        let known_tools = vec![
            "agent",
            "spawn",
            "write_file",
            "edit_file",
            "exec",
            "read_file",
            "grep",
            "web_search",
        ];
        let invalid = registry.validate_disallowed_tools(&known_tools);
        assert!(
            invalid.is_empty(),
            "All built-in disallowed_tools should be valid"
        );
    }

    #[test]
    fn test_validate_disallowed_tools_catches_invalid() {
        let mut registry = AgentTypeRegistry::new_empty();
        let def = AgentTypeDefinition {
            agent_type: "test".to_string(),
            when_to_use: "Test".to_string(),
            disallowed_tools: vec!["real_tool".to_string(), "phantom_tool".to_string()],
            max_turns: None,
            system_prompt_template: None,
            one_shot: false,
            permission_mode: PermissionMode::Inherit,
            isolation: None,
        };
        registry.register(def);
        let known_tools = vec!["real_tool"];
        let invalid = registry.validate_disallowed_tools(&known_tools);
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].0, "test");
        assert!(invalid[0].1.contains(&"phantom_tool".to_string()));
    }
}
