use async_trait::async_trait;
use blockcell_core::{Error, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{Tool, ToolContext, ToolSchema};

/// Agent tool parameters
#[derive(Debug, Deserialize)]
pub struct AgentToolParams {
    /// Agent type (omit for Fork mode - inherits parent context)
    pub subagent_type: Option<String>,

    /// Task description - full prompt to send to subagent
    pub prompt: String,

    /// Short description (3-5 words) for display and notification
    pub description: Option<String>,

    /// Force spawn even if a same-type task is already running
    #[serde(default)]
    pub force: bool,
}

/// Agent tool - spawn specialized subagents for complex tasks
///
/// Available agent types:
/// - explore: Fast read-only codebase exploration (one-shot)
/// - plan: Architecture and implementation planning (one-shot)
/// - verification: Testing and validation specialist (multi-turn, Bubble permission)
/// - viper: Production code implementation agent (multi-turn, Bubble permission)
/// - general: General-purpose agent for complex tasks (multi-turn, Bubble permission)
///
/// Fork mode (omit subagent_type):
/// - Inherits full parent conversation context
/// - Shares prompt cache for efficiency
/// - Executes synchronously, returns result directly (not task_id)
/// - Cannot spawn further subagents (prevents recursion)
///
/// Typed agents (specify subagent_type) run in background,
/// returning task_id for progress tracking via /tasks.
pub struct AgentTool;

#[async_trait]
impl Tool for AgentTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "agent",
            description: "Launch a new agent to handle complex, multi-step tasks. Each agent type has specific capabilities and tools available.

When to use:
- If the target is already known, use the direct tool: Read for a known path, Grep for a specific symbol
- Reserve agent tool for open-ended questions spanning the codebase, or tasks matching available agent types

Available types:
- explore: Fast read-only agent for codebase exploration (one-shot, max 20 turns)
- plan: Architect agent for implementation planning (one-shot, max 30 turns)
- verification: Testing/validation specialist (multi-turn, Bubble permission)
- viper: Implementation agent for production code (multi-turn, Bubble permission)
- general: General-purpose complex task handler (multi-turn, Bubble permission)

Fork mode (omit subagent_type):
Executes synchronously, inherits conversation context, shares prompt cache.
Returns result directly (not task_id). Cannot spawn further subagents to prevent recursion.

Typed agents (specify subagent_type):
Run in background, return task_id for progress tracking via /tasks command.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "subagent_type": {
                        "type": "string",
                        "enum": ["explore", "plan", "verification", "viper", "general"],
                        "description": "Agent type to use. Omit for fork mode (inherits parent conversation context, shares prompt cache)."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the agent to perform. Be specific and include relevant details. For open-ended research, describe what information is needed."
                    },
                    "description": {
                        "type": "string",
                        "description": "A short (3-5 word) description of the task. Shown in notifications and task lists."
                    },
                    "force": {
                        "type": "boolean",
                        "default": false,
                        "description": "Force spawn even if a same-type agent task is already running. Use when the previous task result is stale or the user explicitly wants a new task."
                    }
                },
                "required": ["prompt"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<()> {
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        if prompt.is_none() {
            return Err(Error::Validation(
                "'prompt' is required and must be non-empty".to_string(),
            ));
        }

        if let Some(type_str) = params.get("subagent_type").and_then(|v| v.as_str()) {
            let valid_types = ["explore", "plan", "verification", "viper", "general"];
            if !valid_types.contains(&type_str) {
                return Err(Error::Validation(format!(
                    "Invalid subagent_type: {}",
                    type_str
                )));
            }
        }

        Ok(())
    }

    async fn execute(&self, ctx: ToolContext, params: Value) -> Result<Value> {
        let parsed: AgentToolParams = serde_json::from_value(params)?;

        // ===== 新增：检查spawn权限 =====
        if !ctx.can_spawn_subagent() {
            return Ok(json!({
                "error": "Cannot spawn subagent",
                "reason": "ForkChild and ONE_SHOT agents cannot spawn further agents",
                "hint": "Use direct tools instead (Read, Grep, Write, Edit)"
            }));
        }

        // Check if runtime handle is available
        let runtime_handle = ctx
            .runtime_handle
            .as_ref()
            .ok_or_else(|| Error::Tool("Runtime handle not available".to_string()))?;

        // If subagent_type is omitted -> Fork mode (synchronous execution)
        // Fork mode executes directly and returns the result content (not a task_id)
        if parsed.subagent_type.is_none() {
            let result = runtime_handle.execute_fork_mode(parsed.prompt).await?;
            return Ok(json!({
                "mode": "fork",
                "result": result,
                "completed": true
            }));
        }

        // Typed agent execution (always runs in background)
        let agent_type = parsed.subagent_type.unwrap();

        // ===== 检查是否有同类型的 Running 任务 =====
        // 防止 LLM 基于过时的对话历史误判任务仍在运行，
        // 实际查询 TaskManager 获取真实状态
        if !parsed.force {
            if let Some(ref tm) = ctx.task_manager {
                let running_tasks = tm.list_tasks_json(Some("running".to_string())).await;
                if let Some(tasks_array) = running_tasks.as_array() {
                    // 查找同 agent_type 的 Running 任务
                    let same_type_running: Vec<_> = tasks_array
                        .iter()
                        .filter(|t| {
                            t.get("agent_type")
                                .and_then(|v| v.as_str())
                                .map(|at| at == agent_type)
                                .unwrap_or(false)
                        })
                        .collect();

                    if !same_type_running.is_empty() {
                        // 有同类型任务在运行，返回信息让 LLM 做决策
                        let running_ids: Vec<String> = same_type_running
                            .iter()
                            .filter_map(|t| {
                                t.get("id").and_then(|v| v.as_str()).map(|s| {
                                    let short: String = s.chars().take(12).collect();
                                    format!("{} ({})", short, agent_type)
                                })
                            })
                            .collect();

                        return Ok(json!({
                            "mode": "typed",
                            "status": "duplicate_check",
                            "message": format!(
                                "There is already a {} agent task running (task_id: {}). \
                                 If you want to start a new task anyway, call the agent tool again with force=true. \
                                 If you want to wait for the existing task, use /tasks to monitor progress.",
                                agent_type,
                                running_ids.join(", ")
                            ),
                            "running_task_ids": running_ids,
                            "agent_type": agent_type,
                            "hint": "Set force=true to spawn anyway, or wait for the existing task to complete"
                        }));
                    }
                }
            }
        }

        let result = runtime_handle
            .spawn_typed_agent(&agent_type, parsed.prompt, parsed.description)
            .await?;

        Ok(json!({
            "mode": "typed",
            "task_id": result
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_schema() {
        let tool = AgentTool;
        let schema = tool.schema();
        assert_eq!(schema.name, "agent");
        // Check description is comprehensive
        let desc = schema.description;
        assert!(desc.contains("explore"));
        assert!(desc.contains("plan"));
        assert!(desc.contains("verification"));
        assert!(desc.contains("viper"));
        assert!(desc.contains("general"));
        assert!(desc.contains("Fork mode"));
        assert!(desc.contains("background"));
    }

    #[test]
    fn test_agent_schema_parameters() {
        let tool = AgentTool;
        let schema = tool.schema();
        let params = schema.parameters.as_object().unwrap();
        let props = params.get("properties").unwrap().as_object().unwrap();

        // Check required fields
        assert!(params
            .get("required")
            .unwrap()
            .as_array()
            .unwrap()
            .contains(&json!("prompt")));

        // Check all properties exist
        assert!(props.contains_key("subagent_type"));
        assert!(props.contains_key("prompt"));
        assert!(props.contains_key("description"));
        assert!(props.contains_key("force"));

        // Check enum values for subagent_type
        let subagent_enum = props
            .get("subagent_type")
            .unwrap()
            .get("enum")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(subagent_enum.len(), 5);
        assert!(subagent_enum.contains(&json!("explore")));
        assert!(subagent_enum.contains(&json!("plan")));
        assert!(subagent_enum.contains(&json!("verification")));
        assert!(subagent_enum.contains(&json!("viper")));
        assert!(subagent_enum.contains(&json!("general")));
    }

    #[test]
    fn test_agent_validate_prompt_required() {
        let tool = AgentTool;
        // Valid: has prompt
        assert!(tool
            .validate(&json!({"prompt": "analyze the codebase"}))
            .is_ok());
        // Invalid: no prompt
        assert!(tool.validate(&json!({})).is_err());
        // Invalid: empty prompt
        assert!(tool.validate(&json!({"prompt": ""})).is_err());
    }

    #[test]
    fn test_agent_validate_subagent_type() {
        let tool = AgentTool;
        // Valid types
        for agent_type in ["explore", "plan", "verification", "viper", "general"] {
            assert!(tool
                .validate(&json!({"prompt": "test", "subagent_type": agent_type}))
                .is_ok());
        }
        // Invalid type
        assert!(tool
            .validate(&json!({"prompt": "test", "subagent_type": "invalid_type"}))
            .is_err());
    }

    #[test]
    fn test_agent_validate_optional_fields() {
        let tool = AgentTool;
        // description is optional
        assert!(tool
            .validate(&json!({"prompt": "test", "description": "short desc"}))
            .is_ok());
        // all optional fields
        assert!(tool
            .validate(&json!({
                "prompt": "test",
                "subagent_type": "explore",
                "description": "code analysis"
            }))
            .is_ok());
    }

    #[test]
    fn test_agent_params_deserialization() {
        // Minimal params
        let params: AgentToolParams =
            serde_json::from_value(json!({"prompt": "test prompt"})).unwrap();
        assert_eq!(params.prompt, "test prompt");
        assert!(params.subagent_type.is_none());
        assert!(params.description.is_none());

        // Full params
        let params: AgentToolParams = serde_json::from_value(json!({
            "prompt": "analyze code",
            "subagent_type": "explore",
            "description": "code analysis"
        }))
        .unwrap();
        assert_eq!(params.prompt, "analyze code");
        assert_eq!(params.subagent_type, Some("explore".to_string()));
        assert_eq!(params.description, Some("code analysis".to_string()));
    }

    #[test]
    fn test_agent_fork_mode_detection() {
        // Fork mode: no subagent_type
        let params: AgentToolParams =
            serde_json::from_value(json!({"prompt": "fork task"})).unwrap();
        assert!(params.subagent_type.is_none());

        // Typed mode: has subagent_type
        let params: AgentToolParams = serde_json::from_value(json!({
            "prompt": "typed task",
            "subagent_type": "explore"
        }))
        .unwrap();
        assert!(params.subagent_type.is_some());
    }
}
