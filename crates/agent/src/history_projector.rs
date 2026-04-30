//! History Projector - Layer 2 时间触发的轻量压缩
//!
//! 本模块实现了 Layer 2: Time-Based MicroCompact，用于在对话间歇期清理旧的工具结果。
//!
//! ## 设计原则
//! - **不截断消息**: 保留完整的对话历史，由 Layer 4 (Full Compact) 负责 LLM 语义压缩
//! - **时间触发**: 仅在对话间歇超过阈值时触发清理
//! - **选择性清理**: 只清理特定类型工具的结果，保留关键信息

use blockcell_core::types::ChatMessage;
use serde_json::Value;
use std::collections::HashSet;

use crate::memory_event;
use crate::token::estimate_tokens;

/// 历史分析结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryAnalysis {
    /// 总轮次数
    pub rounds_total: usize,
    /// 最近技能名称
    pub latest_skill_name: Option<String>,
}

pub(crate) struct HistoryProjector<'a> {
    history: &'a [ChatMessage],
}

impl<'a> HistoryProjector<'a> {
    pub(crate) fn new(history: &'a [ChatMessage]) -> Self {
        Self { history }
    }

    /// 分析历史消息
    ///
    /// 提取关键信息，不进行截断
    pub(crate) fn analyze(&self) -> HistoryAnalysis {
        let rounds_total = self.count_rounds();
        let latest_skill_name = self.find_latest_skill_name();

        HistoryAnalysis {
            rounds_total,
            latest_skill_name,
        }
    }

    /// 计算轮次数
    fn count_rounds(&self) -> usize {
        let mut count = 0;
        let mut has_content = false;

        for msg in self.history {
            if msg.role == "user" {
                if has_content {
                    count += 1;
                }
                has_content = true;
            }
        }

        if has_content {
            count += 1;
        }

        count
    }

    /// 查找最近的技能名称
    fn find_latest_skill_name(&self) -> Option<String> {
        // 从后向前遍历，找到最近的内部技能调用
        for msg in self.history.iter().rev() {
            if let Some(tool_calls) = &msg.tool_calls {
                for call in tool_calls {
                    if is_internal_skill_trace(&call.name) {
                        if let Some(name) =
                            call.arguments.get("skill_name").and_then(|v| v.as_str())
                        {
                            let trimmed = name.trim();
                            if !trimmed.is_empty() {
                                return Some(trimmed.to_string());
                            }
                        }
                    }
                }
            }
        }
        None
    }
}

/// 判断是否为内部技能追踪
fn is_internal_skill_trace(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "skill_enter" | "skill_invoke_python" | "skill_invoke_rhai" | "skill_invoke_script"
    )
}

// ============================================================================
// Layer 2: 时间触发的轻量压缩 (Time-Based MicroCompact)
// ============================================================================

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 可压缩工具列表
///
/// 这些工具的输出可以在时间触发时被清理
pub const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file",
    "shell",
    "grep",
    "glob",
    "web_search",
    "web_fetch",
    "file_edit",
    "file_write",
];

/// 时间触发配置
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TimeBasedMCConfig {
    /// 主开关
    pub enabled: bool,
    /// 触发阈值 (分钟)
    pub gap_threshold_minutes: u32,
    /// 保留最近数量
    pub keep_recent: u32,
}

impl Default for TimeBasedMCConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            gap_threshold_minutes: 60,
            keep_recent: 5,
        }
    }
}

impl From<blockcell_core::config::Layer2Config> for TimeBasedMCConfig {
    fn from(c: blockcell_core::config::Layer2Config) -> Self {
        Self {
            enabled: c.enabled,
            gap_threshold_minutes: c.gap_threshold_minutes,
            keep_recent: c.keep_recent,
        }
    }
}

/// 时间触发结果
#[derive(Debug)]
pub struct TimeTriggerResult {
    pub gap_minutes: u32,
    pub config: TimeBasedMCConfig,
}

/// 判断是否主线程来源
///
/// 只有主线程来源才应该执行时间触发的轻量压缩
fn is_main_thread_source(query_source: Option<&str>) -> bool {
    query_source
        .map(|s| s.starts_with("repl_main_thread"))
        .unwrap_or(true)
}

impl<'a> HistoryProjector<'a> {
    /// 评估时间触发
    ///
    /// 检查是否应该执行时间触发的轻量压缩
    ///
    /// ## 参数
    /// - `last_assistant_timestamp`: 最后一个 assistant 消息的时间戳
    /// - `query_source`: 查询来源标识符，用于判断是否为主线程
    /// - `config`: 时间触发配置
    ///
    /// ## 时间测量的注意事项
    ///
    /// 使用 `DateTime<Utc>` (wall clock) 计算时间差，而非 monotonic clock (`Instant`)。
    /// 这意味着系统时钟调整（NTP 同步、手动修改、时区变化）可能影响触发准确性。
    ///
    /// **实际影响分析**:
    /// - 默认阈值 60 分钟，系统时钟调整通常只是几分钟
    /// - 影响仅限于压缩触发时机，不影响功能正确性
    /// - 如果时钟向前调整，可能导致提前触发（无害）
    /// - 如果时钟向后调整，可能导致延迟触发（增加内存占用）
    ///
    /// **为什么不使用 Instant**:
    /// - 消息时间戳来自 API (如 Telegram/Slack)，已序列化为 `DateTime<Utc>`
    /// - `Instant` 无法序列化或跨会话保存
    /// - 需要跨会话计算时间差的场景必须使用 wall clock
    pub fn evaluate_time_based_trigger(
        &self,
        last_assistant_timestamp: Option<DateTime<Utc>>,
        query_source: Option<&str>,
        config: &TimeBasedMCConfig,
    ) -> Option<TimeTriggerResult> {
        if !config.enabled {
            return None;
        }

        // 只在主线程执行
        if !is_main_thread_source(query_source) {
            return None;
        }

        // 找到最后一个 assistant 消息的时间戳
        let last_timestamp = last_assistant_timestamp?;

        // 计算时间差（使用 wall clock，见上方文档说明）
        let gap_minutes = (Utc::now() - last_timestamp).num_minutes() as u32;

        // 必须超过阈值
        if gap_minutes < config.gap_threshold_minutes {
            return None;
        }

        Some(TimeTriggerResult {
            gap_minutes,
            config: config.clone(),
        })
    }

    /// 时间触发的轻量压缩
    ///
    /// 清理旧的工具结果，保留最近的 N 个
    ///
    /// ## 参数
    /// - `last_assistant_timestamp`: 最后一个 assistant 消息的时间戳
    /// - `query_source`: 查询来源标识符，用于判断是否为主线程
    /// - `config`: 时间触发配置
    pub fn time_based_microcompact(
        &self,
        last_assistant_timestamp: Option<DateTime<Utc>>,
        query_source: Option<&str>,
        config: &TimeBasedMCConfig,
    ) -> Option<Vec<ChatMessage>> {
        let trigger =
            self.evaluate_time_based_trigger(last_assistant_timestamp, query_source, config)?;

        // 记录 Layer 2 触发事件
        memory_event!(
            layer2,
            triggered,
            trigger.gap_minutes,
            config.gap_threshold_minutes
        );

        // 收集可压缩的工具 ID
        let compactable_ids = collect_compactable_tool_ids(self.history);

        if compactable_ids.is_empty() {
            return None;
        }

        // 保留最近 N 个 (最少 1 个)
        let keep_recent = std::cmp::max(1, config.keep_recent) as usize;
        let keep_set: HashSet<_> = compactable_ids
            .iter()
            .rev()
            .take(keep_recent)
            .cloned()
            .collect();

        // 清理集合
        let clear_set: HashSet<_> = compactable_ids
            .into_iter()
            .filter(|id| !keep_set.contains(id))
            .collect();

        if clear_set.is_empty() {
            return None;
        }

        let cleared_count = clear_set.len() as u64;
        let kept_count = keep_set.len() as u64;

        // 修改消息内容
        let result = self
            .history
            .iter()
            .map(|message| maybe_clear_tool_result(message, &clear_set))
            .collect();

        // 记录 Layer 2 清理完成事件
        memory_event!(layer2, cleared, cleared_count, kept_count);

        Some(result)
    }
}

/// 收集可压缩的工具 ID
fn collect_compactable_tool_ids(messages: &[ChatMessage]) -> Vec<String> {
    let mut ids = Vec::new();

    for message in messages {
        if message.role != "assistant" {
            continue;
        }
        if let Some(tool_calls) = &message.tool_calls {
            for call in tool_calls {
                if COMPACTABLE_TOOLS.contains(&call.name.as_str()) {
                    ids.push(call.id.clone());
                }
            }
        }
    }

    ids
}

/// 清理工具结果内容
fn maybe_clear_tool_result(message: &ChatMessage, clear_set: &HashSet<String>) -> ChatMessage {
    if message.role != "tool" {
        return message.clone();
    }

    // 检查 tool_call_id 是否在清理集合中
    if let Some(tool_call_id) = &message.tool_call_id {
        if clear_set.contains(tool_call_id) {
            let mut cleared = message.clone();
            cleared.content =
                Value::String(crate::response_cache::TIME_BASED_MC_CLEARED_MESSAGE.to_string());
            return cleared;
        }
    }

    message.clone()
}

/// Token 估算函数
///
/// 使用 tiktoken 精确估算文本的 token 数量
pub fn rough_token_count_estimation(text: &str) -> usize {
    estimate_tokens(text)
}

/// 估算消息 token 数 (保守估算)
///
/// 上浮 4/3 保守估算
pub fn estimate_message_tokens_conservative(messages: &[ChatMessage]) -> usize {
    const CONSERVATIVE_MULTIPLIER: f64 = 4.0 / 3.0;

    let total: usize = messages
        .iter()
        .filter(|m| matches!(m.role.as_str(), "user" | "assistant"))
        .map(estimate_single_message_tokens)
        .sum();

    (total as f64 * CONSERVATIVE_MULTIPLIER).ceil() as usize
}

fn estimate_single_message_tokens(message: &ChatMessage) -> usize {
    let content_tokens = match &message.content {
        Value::String(text) => rough_token_count_estimation(text),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                if let Some(obj) = part.as_object() {
                    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
                        return rough_token_count_estimation(text);
                    }
                }
                0
            })
            .sum(),
        _ => 0,
    };

    let tool_call_tokens = message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| {
                    rough_token_count_estimation(&format!(
                        "{}{}",
                        call.name,
                        serde_json::to_string(&call.arguments).unwrap_or_default()
                    ))
                })
                .sum()
        })
        .unwrap_or(0);

    content_tokens + tool_call_tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::types::ToolCallRequest;

    #[test]
    fn test_time_based_config_default() {
        let config = TimeBasedMCConfig::default();
        assert!(config.enabled);
        assert_eq!(config.gap_threshold_minutes, 60);
        assert_eq!(config.keep_recent, 5);
    }

    #[test]
    fn test_is_main_thread_source() {
        // 主线程来源
        assert!(is_main_thread_source(None));
        assert!(is_main_thread_source(Some("repl_main_thread")));
        assert!(is_main_thread_source(Some("repl_main_thread_query")));

        // 非主线程来源
        assert!(!is_main_thread_source(Some("forked")));
        assert!(!is_main_thread_source(Some("auto_memory")));
        assert!(!is_main_thread_source(Some("session_memory")));
        assert!(!is_main_thread_source(Some("dream")));
    }

    #[test]
    fn test_collect_compactable_tool_ids() {
        let messages = vec![
            ChatMessage {
                id: None,
                role: "assistant".to_string(),
                content: Value::String(String::new()),
                tool_calls: Some(vec![
                    ToolCallRequest {
                        id: "tool-1".to_string(),
                        name: "read_file".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    },
                    ToolCallRequest {
                        id: "tool-2".to_string(),
                        name: "shell".to_string(),
                        arguments: serde_json::json!({}),
                        thought_signature: None,
                    },
                ]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
            },
            ChatMessage {
                id: None,
                role: "assistant".to_string(),
                content: Value::String(String::new()),
                tool_calls: Some(vec![ToolCallRequest {
                    id: "tool-3".to_string(),
                    name: "not_compactable".to_string(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }]),
                tool_call_id: None,
                name: None,
                reasoning_content: None,
            },
        ];

        let ids = collect_compactable_tool_ids(&messages);
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&"tool-1".to_string()));
        assert!(ids.contains(&"tool-2".to_string()));
    }

    #[test]
    fn test_rough_token_estimation() {
        let text = "Hello World! This is a test.";
        let tokens = rough_token_count_estimation(text);
        assert!(tokens > 0);
        // Approximately 29 chars / 4 ≈ 7 tokens
        assert!(tokens >= 6 && tokens <= 8);
    }

    #[test]
    fn test_estimate_message_tokens_conservative() {
        let messages = vec![
            ChatMessage::user("Hello, this is a test message with some content."),
            ChatMessage::assistant("I understand. Let me help you with that."),
        ];

        let tokens = estimate_message_tokens_conservative(&messages);
        // Should include 4/3 multiplier
        assert!(tokens > 0);
    }

    // ========== 核心路径测试 ==========

    #[test]
    fn test_history_projector_analyze_empty() {
        let history: Vec<ChatMessage> = Vec::new();
        let projector = HistoryProjector::new(&history);
        let analysis = projector.analyze();

        assert_eq!(analysis.rounds_total, 0);
        assert!(analysis.latest_skill_name.is_none());
    }

    #[test]
    fn test_history_projector_analyze_single_round() {
        let history = vec![
            ChatMessage::user("What is Rust?"),
            ChatMessage::assistant("Rust is a systems programming language."),
        ];
        let projector = HistoryProjector::new(&history);
        let analysis = projector.analyze();

        assert_eq!(analysis.rounds_total, 1);
    }

    #[test]
    fn test_history_projector_analyze_multiple_rounds() {
        let history = vec![
            ChatMessage::user("What is Rust?"),
            ChatMessage::assistant("Rust is a systems programming language."),
            ChatMessage::user("Tell me more"),
            ChatMessage::assistant("Rust focuses on safety and performance."),
        ];
        let projector = HistoryProjector::new(&history);
        let analysis = projector.analyze();

        assert_eq!(analysis.rounds_total, 2);
    }

    #[test]
    fn test_time_based_mc_config() {
        let config = TimeBasedMCConfig::default();

        assert!(config.enabled);
        assert_eq!(config.gap_threshold_minutes, 60);
        assert_eq!(config.keep_recent, 5);
    }
}
