use blockcell_core::types::ChatMessage;

const RECENT_SKILL_TRACE_WINDOW: usize = 12;

#[derive(Debug, Clone)]
pub struct SkillExecutionResult {
    pub final_response: String,
    pub trace_messages: Vec<ChatMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkillManualLoadMode {
    Initial,
    ReuseRecent,
    ReloadInsufficient,
}

impl SkillManualLoadMode {
    pub(crate) fn should_load_manual(self) -> bool {
        !matches!(self, Self::ReuseRecent)
    }
}

pub(crate) fn determine_manual_load_mode(
    skill_name: &str,
    history: &[ChatMessage],
) -> SkillManualLoadMode {
    let has_any_trace = history
        .iter()
        .any(|message| message_has_skill_trace(message, skill_name));
    if !has_any_trace {
        return SkillManualLoadMode::Initial;
    }

    let has_recent_trace = history
        .iter()
        .rev()
        .take(RECENT_SKILL_TRACE_WINDOW)
        .any(|message| message_has_skill_trace(message, skill_name));
    if has_recent_trace {
        SkillManualLoadMode::ReuseRecent
    } else {
        SkillManualLoadMode::ReloadInsufficient
    }
}

fn message_has_skill_trace(message: &ChatMessage, skill_name: &str) -> bool {
    message
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls.iter().any(|call| {
                call.arguments
                    .get("skill_name")
                    .and_then(|value| value.as_str())
                    == Some(skill_name)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::types::ToolCallRequest;

    fn skill_trace_message(skill_name: &str) -> ChatMessage {
        ChatMessage {
            id: None,
            role: "assistant".to_string(),
            content: serde_json::Value::String(String::new()),
            reasoning_content: None,
            tool_calls: Some(vec![ToolCallRequest {
                id: format!("skill-trace-{}", skill_name),
                name: "skill_enter".to_string(),
                arguments: serde_json::json!({
                    "skill_name": skill_name,
                }),
                thought_signature: None,
            }]),
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn test_skill_executor_loads_full_skill_manual_once() {
        let history = vec![ChatMessage::user("第一次执行")];

        assert_eq!(
            determine_manual_load_mode("weather", &history),
            SkillManualLoadMode::Initial
        );
        assert!(SkillManualLoadMode::Initial.should_load_manual());
    }

    #[test]
    fn test_skill_executor_followup_reuses_history_without_reinjecting_manual() {
        let history = vec![
            ChatMessage::user("查天气"),
            skill_trace_message("weather"),
            ChatMessage::tool_result("skill-trace-weather", r#"{"skill_name":"weather"}"#),
            ChatMessage::assistant("深圳今天晴"),
            ChatMessage::user("那明天呢"),
        ];

        assert_eq!(
            determine_manual_load_mode("weather", &history),
            SkillManualLoadMode::ReuseRecent
        );
        assert!(!SkillManualLoadMode::ReuseRecent.should_load_manual());
    }

    #[test]
    fn test_skill_executor_can_reload_manual_when_context_is_insufficient() {
        let mut history = vec![
            ChatMessage::user("第一次查"),
            skill_trace_message("xiaohongshu"),
            ChatMessage::tool_result(
                "skill-trace-xiaohongshu",
                r#"{"skill_name":"xiaohongshu"}"#,
            ),
            ChatMessage::assistant("返回了推荐流"),
        ];
        history.extend((0..RECENT_SKILL_TRACE_WINDOW).map(|index| {
            if index % 2 == 0 {
                ChatMessage::user(&format!("other message {}", index))
            } else {
                ChatMessage::assistant(&format!("other reply {}", index))
            }
        }));

        assert_eq!(
            determine_manual_load_mode("xiaohongshu", &history),
            SkillManualLoadMode::ReloadInsufficient
        );
        assert!(SkillManualLoadMode::ReloadInsufficient.should_load_manual());
    }
}
