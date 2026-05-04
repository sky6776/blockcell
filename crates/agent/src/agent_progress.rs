use crate::task_notification::TaskNotificationPayload;
use serde::{Deserialize, Serialize};

/// Agent 进度事件
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AgentProgress {
    /// 增量进度（LLM 交互产生的 token/tool 统计）
    #[serde(rename = "delta")]
    Delta {
        task_id: String,
        tokens_added: u64,
        tools_added: u64,
        total_tokens: u64,
        total_tools: u64,
    },

    /// 阶段进度更新（任务执行阶段变化，如"正在分析代码"、"正在执行工具"等）
    #[serde(rename = "stage")]
    Stage {
        task_id: String,
        /// 阶段描述
        stage: String,
        /// 进度百分比 (0-100)
        percent: u8,
    },

    /// 任务通知（完成/失败）
    #[serde(rename = "notification")]
    Notification(TaskNotificationPayload),

    /// 工具调用开始（通过 progress_tx 转发到外部渠道）
    #[serde(rename = "tool_call_start")]
    ToolCallStart {
        task_id: String,
        tool: String,
        call_id: String,
        agent_type: String,
        summary: String,
    },

    /// 工具调用结束（通过 progress_tx 转发到外部渠道）
    #[serde(rename = "tool_call_end")]
    ToolCallEnd {
        task_id: String,
        tool: String,
        call_id: String,
        agent_type: String,
        success: bool,
    },
}
