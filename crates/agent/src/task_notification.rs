use crate::task_manager::TaskStatus;
use blockcell_core::UsageMetrics;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// TaskNotification Payload - 用于 Agent 任务完成/失败的通知
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskNotificationPayload {
    /// 任务 ID
    pub task_id: String,
    /// Agent 类型标识
    pub agent_type: String,
    /// 任务状态
    pub status: TaskStatus,
    /// 执行结果
    pub result: String,
    /// 完成时间
    pub completed_at: DateTime<Utc>,
    /// 用量指标
    pub usage: UsageMetrics,
}
