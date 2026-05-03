use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::VecDeque;

/// 最大最近活动数量
const MAX_RECENT_ACTIVITIES: usize = 5;

/// 工具活动记录
#[derive(Debug, Clone)]
pub struct ToolActivity {
    /// 工具名称
    pub tool_name: String,
    /// 工具输入参数
    pub input: Value,
    /// 预计算的可读描述
    pub activity_description: Option<String>,
    /// 是否为搜索类工具
    pub is_search: bool,
    /// 是否为读取类工具
    pub is_read: bool,
    /// 活动时间戳
    pub timestamp: DateTime<Utc>,
}

impl ToolActivity {
    /// 创建搜索类活动
    pub fn search(tool_name: String, input: Value, description: Option<String>) -> Self {
        Self {
            tool_name,
            input,
            activity_description: description,
            is_search: true,
            is_read: false,
            timestamp: Utc::now(),
        }
    }

    /// 创建读取类活动
    pub fn read(tool_name: String, input: Value, description: Option<String>) -> Self {
        Self {
            tool_name,
            input,
            activity_description: description,
            is_search: false,
            is_read: true,
            timestamp: Utc::now(),
        }
    }

    /// 创建其他类活动
    pub fn other(tool_name: String, input: Value, description: Option<String>) -> Self {
        Self {
            tool_name,
            input,
            activity_description: description,
            is_search: false,
            is_read: false,
            timestamp: Utc::now(),
        }
    }
}

/// 进度追踪器
#[derive(Debug, Clone)]
pub struct ProgressTracker {
    /// 当前累积的 token 数
    pub total_tokens: u64,

    /// 当前累积的工具调用数
    pub total_tool_calls: u64,

    /// 上次报告时的 token 数
    pub last_reported_tokens: u64,

    /// 上次报告时的工具调用数
    pub last_reported_tool_calls: u64,

    /// 上次报告时间
    pub last_reported_time: Option<DateTime<Utc>>,

    /// 报告间隔阈值（毫秒）
    pub report_interval_ms: u64,

    // ===== 新增字段 =====
    /// 最近5次工具活动历史
    pub recent_activities: VecDeque<ToolActivity>,
    /// 上次活动描述（用于UI快速显示）
    pub last_activity_description: Option<String>,
}

/// 进度增量
#[derive(Debug, Clone, Default)]
pub struct ProgressDelta {
    pub tokens_added: u64,
    pub tools_added: u64,
    pub total_tokens: u64,
    pub total_tools: u64,
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            total_tokens: 0,
            total_tool_calls: 0,
            last_reported_tokens: 0,
            last_reported_tool_calls: 0,
            last_reported_time: None,
            report_interval_ms: 1000,
            recent_activities: VecDeque::with_capacity(MAX_RECENT_ACTIVITIES),
            last_activity_description: None,
        }
    }

    /// 累积 token 数
    pub fn add_tokens(&mut self, count: u64) {
        self.total_tokens += count;
    }

    /// 累积工具调用数
    pub fn add_tool_call(&mut self) {
        self.total_tool_calls += 1;
    }

    /// 添加工具活动
    pub fn add_tool_activity(&mut self, activity: ToolActivity) {
        self.total_tool_calls += 1;
        self.last_activity_description = activity.activity_description.clone();

        if self.recent_activities.len() >= MAX_RECENT_ACTIVITIES {
            self.recent_activities.pop_front();
        }
        self.recent_activities.push_back(activity);
    }

    /// 获取最近活动列表
    pub fn get_recent_activities(&self) -> Vec<&ToolActivity> {
        self.recent_activities.iter().collect()
    }

    /// 获取最近活动描述摘要
    pub fn get_activity_summary(&self) -> String {
        self.recent_activities
            .iter()
            .filter_map(|a| a.activity_description.as_ref())
            .take(3)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 计算增量（带间隔检查）
    pub fn compute_delta(&mut self) -> ProgressDelta {
        let now = Utc::now();

        // 检查报告间隔
        if let Some(last_time) = self.last_reported_time {
            let elapsed = (now - last_time).num_milliseconds() as u64;
            if elapsed < self.report_interval_ms {
                return ProgressDelta::empty();
            }
        }

        // 计算增量
        let token_delta = self.total_tokens - self.last_reported_tokens;
        let tool_delta = self.total_tool_calls - self.last_reported_tool_calls;

        // 更新 last_reported
        self.last_reported_tokens = self.total_tokens;
        self.last_reported_tool_calls = self.total_tool_calls;
        self.last_reported_time = Some(now);

        ProgressDelta {
            tokens_added: token_delta,
            tools_added: tool_delta,
            total_tokens: self.total_tokens,
            total_tools: self.total_tool_calls,
        }
    }

    /// 强制报告（用于终端状态）
    pub fn force_report(&mut self) -> ProgressDelta {
        let token_delta = self.total_tokens - self.last_reported_tokens;
        let tool_delta = self.total_tool_calls - self.last_reported_tool_calls;

        self.last_reported_tokens = self.total_tokens;
        self.last_reported_tool_calls = self.total_tool_calls;
        self.last_reported_time = Some(Utc::now());

        ProgressDelta {
            tokens_added: token_delta,
            tools_added: tool_delta,
            total_tokens: self.total_tokens,
            total_tools: self.total_tool_calls,
        }
    }
}

impl ProgressDelta {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn should_report(&self) -> bool {
        self.tokens_added > 0 || self.tools_added > 0
    }
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}
