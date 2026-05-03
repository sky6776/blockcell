use serde::{Deserialize, Serialize};

/// Agent角色类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentRole {
    /// 主Agent（用户直接交互）
    Lead,
    /// Fork子Agent（继承父对话上下文）
    ForkChild,
    /// Typed Agent（特定类型的子Agent）
    Typed { agent_type: String },
}

impl AgentRole {
    /// 检查是否为ForkChild
    pub fn is_fork_child(&self) -> bool {
        matches!(self, AgentRole::ForkChild)
    }

    /// 检查是否为Typed Agent
    pub fn is_typed(&self) -> bool {
        matches!(self, AgentRole::Typed { .. })
    }

    /// 检查是否为ONE_SHOT类型（简化版）
    pub fn is_one_shot_basic(&self) -> Option<bool> {
        match self {
            AgentRole::ForkChild => Some(true),
            AgentRole::Lead => Some(false),
            AgentRole::Typed { .. } => None,
        }
    }

    /// 获取agent_type（如果是Typed）
    pub fn agent_type(&self) -> Option<&str> {
        match self {
            AgentRole::Typed { agent_type } => Some(agent_type),
            _ => None,
        }
    }
}

/// Agent身份标识
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentIdentity {
    /// Agent唯一标识
    pub agent_id: String,
    /// Agent显示名称
    pub agent_name: String,
    /// Agent角色
    pub role: AgentRole,
    /// 父会话ID（用于链路追踪）
    pub parent_session_id: Option<String>,
    /// 触发请求ID（spawn/resume边界追踪）
    pub invoking_request_id: Option<String>,
}

impl AgentIdentity {
    /// 创建主Agent身份
    pub fn lead(agent_id: String, agent_name: String) -> Self {
        Self {
            agent_id,
            agent_name,
            role: AgentRole::Lead,
            parent_session_id: None,
            invoking_request_id: None,
        }
    }

    /// 创建Fork子Agent身份
    pub fn fork_child(agent_id: String, parent_session_id: String) -> Self {
        Self {
            agent_id,
            agent_name: "fork".to_string(),
            role: AgentRole::ForkChild,
            parent_session_id: Some(parent_session_id),
            invoking_request_id: None,
        }
    }

    /// 创建Typed Agent身份
    pub fn typed(agent_id: String, agent_type: String, parent_session_id: String) -> Self {
        Self {
            agent_id,
            agent_name: agent_type.clone(),
            role: AgentRole::Typed { agent_type },
            parent_session_id: Some(parent_session_id),
            invoking_request_id: None,
        }
    }

    /// 检查是否可以spawn子Agent（简化版）
    pub fn can_spawn_subagent_basic(&self) -> bool {
        !self.role.is_fork_child()
    }

    /// 获取日志标识名称
    pub fn log_name(&self) -> &str {
        match &self.role {
            AgentRole::Lead => &self.agent_name,
            AgentRole::ForkChild => "fork",
            AgentRole::Typed { agent_type } => agent_type,
        }
    }
}

impl Default for AgentIdentity {
    fn default() -> Self {
        Self::lead(uuid::Uuid::new_v4().to_string(), "lead".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_role_is_fork_child() {
        assert!(AgentRole::ForkChild.is_fork_child());
        assert!(!AgentRole::Lead.is_fork_child());
    }

    #[test]
    fn test_agent_identity_can_spawn_basic() {
        let lead = AgentIdentity::lead("id1".to_string(), "lead".to_string());
        assert!(lead.can_spawn_subagent_basic());

        let fork = AgentIdentity::fork_child("id2".to_string(), "parent".to_string());
        assert!(!fork.can_spawn_subagent_basic());
    }
}
