use blockcell_core::types::ChatMessage;
use serde_json::Value;
use std::collections::HashSet;

use crate::token::estimate_messages_tokens;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HistoryProjectionProfile {
    Conversation,
    ScriptPlanning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryAnalysis {
    pub rounds_total: usize,
    pub latest_skill_name: Option<String>,
    pub reference_round: Option<usize>,
    pub has_reference_intent: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct HistoryProjection {
    pub messages: Vec<ChatMessage>,
    pub rounds_total: usize,
    pub rounds_embedded: usize,
    pub messages_embedded: usize,
    pub reference_round: Option<usize>,
}

pub(crate) struct HistoryProjector<'a> {
    history: &'a [ChatMessage],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReferenceIntent {
    None,
    Ordinal,
    Demonstrative,
    Continue,
    Detail,
}

#[derive(Debug)]
struct HistoryRound<'a> {
    index: usize,
    messages: Vec<&'a ChatMessage>,
    latest_skill_name: Option<String>,
    has_tool_result: bool,
    has_structured_payload: bool,
    list_items_hint: usize,
    explicit_index_max: usize,
    user_reference_intent: ReferenceIntent,
}

impl<'a> HistoryProjector<'a> {
    pub(crate) fn new(history: &'a [ChatMessage]) -> Self {
        Self { history }
    }

    pub(crate) fn analyze(&self, user_input: &str) -> HistoryAnalysis {
        let rounds = self.rounds();
        let reference_intent = detect_reference_intent(user_input);
        let reference_round = select_reference_round(&rounds, reference_intent, user_input);
        HistoryAnalysis {
            rounds_total: rounds.len(),
            latest_skill_name: rounds
                .iter()
                .rev()
                .find_map(|round| round.latest_skill_name.clone()),
            reference_round,
            has_reference_intent: !matches!(reference_intent, ReferenceIntent::None),
        }
    }

    pub(crate) fn project(
        &self,
        user_input: &str,
        profile: HistoryProjectionProfile,
        token_budget: usize,
    ) -> HistoryProjection {
        let rounds = self.rounds();
        let analysis = self.analyze(user_input);
        let reference_round = analysis.reference_round;
        let mandatory_indexes = mandatory_round_indexes(&rounds, profile, reference_round);

        let mut remaining_budget = token_budget;
        let mut selected_indexes = Vec::new();
        let mut seen = HashSet::new();

        for &idx in &mandatory_indexes {
            let rendered =
                render_round(&rounds[idx], rounds.len(), profile, reference_round, true);
            let round_tokens = estimate_history_tokens(&rendered);
            if round_tokens > remaining_budget && !selected_indexes.is_empty() {
                continue;
            }
            remaining_budget = remaining_budget.saturating_sub(round_tokens);
            selected_indexes.push(idx);
            seen.insert(idx);
        }

        let mut older_rounds_reversed = Vec::new();
        for idx in (0..rounds.len()).rev() {
            if seen.contains(&idx) {
                continue;
            }
            let rendered =
                render_round(&rounds[idx], rounds.len(), profile, reference_round, false);
            let round_tokens = estimate_history_tokens(&rendered);
            if round_tokens > remaining_budget {
                continue;
            }
            remaining_budget = remaining_budget.saturating_sub(round_tokens);
            older_rounds_reversed.push(idx);
        }
        older_rounds_reversed.reverse();
        selected_indexes.splice(0..0, older_rounds_reversed);

        let mut messages = Vec::new();
        for idx in &selected_indexes {
            messages.extend(render_round(
                &rounds[*idx],
                rounds.len(),
                profile,
                reference_round,
                mandatory_indexes.contains(idx),
            ));
        }

        HistoryProjection {
            rounds_total: rounds.len(),
            rounds_embedded: selected_indexes.len(),
            messages_embedded: messages.len(),
            reference_round,
            messages,
        }
    }

    fn rounds(&self) -> Vec<HistoryRound<'a>> {
        let mut round_messages: Vec<Vec<&ChatMessage>> = Vec::new();
        let mut current = Vec::new();

        for msg in self.history {
            if msg.role == "user" && !current.is_empty() {
                round_messages.push(current);
                current = Vec::new();
            }
            current.push(msg);
        }
        if !current.is_empty() {
            round_messages.push(current);
        }

        round_messages
            .into_iter()
            .enumerate()
            .map(|(index, messages)| {
                let latest_skill_name = messages.iter().find_map(|msg| {
                    msg.tool_calls.as_ref().and_then(|calls| {
                        calls.iter().find_map(|call| {
                            if !is_internal_skill_trace(call.name.as_str()) {
                                return None;
                            }
                            call.arguments
                                .get("skill_name")
                                .and_then(|value| value.as_str())
                                .map(str::trim)
                                .filter(|value| !value.is_empty())
                                .map(str::to_string)
                        })
                    })
                });
                let list_items_hint = detect_round_list_items_hint(&messages);
                let explicit_index_max = detect_round_explicit_index_max(&messages);
                let user_reference_intent = messages
                    .iter()
                    .find(|msg| msg.role == "user")
                    .map(|msg| detect_reference_intent(&stringify_chat_message_content(msg)))
                    .unwrap_or(ReferenceIntent::None);
                let has_structured_payload = messages.iter().any(|msg| {
                    parse_structured_content(msg)
                        .map(|value| has_structured_payload(&value))
                        .unwrap_or(false)
                });
                let has_tool_result = messages.iter().any(|msg| msg.role == "tool");

                HistoryRound {
                    index,
                    messages,
                    latest_skill_name,
                    has_tool_result,
                    has_structured_payload,
                    list_items_hint,
                    explicit_index_max,
                    user_reference_intent,
                }
            })
            .collect()
    }
}

fn mandatory_round_indexes(
    rounds: &[HistoryRound<'_>],
    profile: HistoryProjectionProfile,
    reference_round: Option<usize>,
) -> Vec<usize> {
    let mut indexes = Vec::new();
    let len = rounds.len();
    let (full_recent, compact_recent) = match profile {
        HistoryProjectionProfile::Conversation => (6usize, 0usize),
        HistoryProjectionProfile::ScriptPlanning => (3usize, 3usize),
    };
    let recent_keep = full_recent + compact_recent;
    let recent_start = len.saturating_sub(recent_keep);

    for idx in recent_start..len {
        indexes.push(idx);
    }

    if let Some(reference_idx) = reference_round {
        if !indexes.contains(&reference_idx) {
            indexes.insert(0, reference_idx);
        }
    }

    indexes
}

fn render_round(
    round: &HistoryRound<'_>,
    total_rounds: usize,
    profile: HistoryProjectionProfile,
    reference_round: Option<usize>,
    mandatory: bool,
) -> Vec<ChatMessage> {
    let render_mode = match profile {
        HistoryProjectionProfile::Conversation => {
            if Some(round.index) == reference_round || mandatory {
                RoundRenderKind::ConversationIntact
            } else {
                RoundRenderKind::Compressed
            }
        }
        HistoryProjectionProfile::ScriptPlanning => {
            let full_recent_start = total_rounds.saturating_sub(3);
            let compact_recent_start = total_rounds.saturating_sub(6);
            if Some(round.index) == reference_round || round.index >= full_recent_start {
                RoundRenderKind::PlanningFull
            } else if mandatory || round.index >= compact_recent_start {
                RoundRenderKind::PlanningCompact
            } else {
                RoundRenderKind::Compressed
            }
        }
    };

    match render_mode {
        RoundRenderKind::ConversationIntact => round
            .messages
            .iter()
            .map(|msg| trim_message_for_conversation(msg))
            .collect(),
        RoundRenderKind::PlanningFull => round
            .messages
            .iter()
            .map(|msg| trim_message_for_script_planning(msg, false))
            .collect(),
        RoundRenderKind::PlanningCompact => round
            .messages
            .iter()
            .map(|msg| trim_message_for_script_planning(msg, true))
            .collect(),
        RoundRenderKind::Compressed => build_compressed_round(&round.messages),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RoundRenderKind {
    ConversationIntact,
    PlanningFull,
    PlanningCompact,
    Compressed,
}

fn build_compressed_round(round: &[&ChatMessage]) -> Vec<ChatMessage> {
    let user_msg = round.iter().find(|msg| msg.role == "user");
    let assistant_msg = round
        .iter()
        .rev()
        .find(|msg| msg.role == "assistant" && msg.tool_calls.is_none())
        .or_else(|| round.iter().rev().find(|msg| msg.role == "assistant"));

    let mut compressed = Vec::new();
    if let Some(user_msg) = user_msg {
        compressed.push(ChatMessage::user(&trim_text_head_tail(
            &stringify_chat_message_content(user_msg),
            200,
        )));
    }
    if let Some(assistant_msg) = assistant_msg {
        compressed.push(ChatMessage::assistant(&trim_text_head_tail(
            &stringify_chat_message_content(assistant_msg),
            400,
        )));
    }
    compressed
}

fn trim_message_for_conversation(msg: &ChatMessage) -> ChatMessage {
    let mut out = msg.clone();
    let max_chars = match msg.role.as_str() {
        "tool" => 2400,
        "system" => 8000,
        _ => 1400,
    };
    trim_message_content(&mut out, max_chars);

    // Preserve reasoning by truncating instead of discarding
    // Conversation mode uses smaller budget for tighter context limits
    let max_thinking_chars = 2000;
    out.reasoning_content = truncate_reasoning_content(&msg.reasoning_content, max_thinking_chars);

    out
}

fn trim_message_for_script_planning(msg: &ChatMessage, compact: bool) -> ChatMessage {
    let mut out = msg.clone();
    let max_chars = match (msg.role.as_str(), compact) {
        ("tool", true) => 6000,
        ("tool", false) => 20_000,
        ("assistant", _) if msg.tool_calls.is_some() => 1500,
        (_, true) => 1200,
        _ => 3000,
    };
    trim_message_content(&mut out, max_chars);

    // Preserve reasoning by truncating instead of discarding
    // Max thinking chars per message (approximately 1000 tokens = ~4000 chars)
    let max_thinking_chars = 4000;
    out.reasoning_content = truncate_reasoning_content(&msg.reasoning_content, max_thinking_chars);

    out
}

/// Truncate reasoning_content to a maximum character budget.
fn truncate_reasoning_content(content: &Option<String>, max_chars: usize) -> Option<String> {
    content.as_ref().map(|text| trim_text_head_tail(text, max_chars))
}

fn trim_message_content(msg: &mut ChatMessage, max_chars: usize) {
    match &msg.content {
        Value::String(text) => {
            msg.content = Value::String(trim_text_head_tail(text, max_chars));
        }
        Value::Array(parts) => {
            let mut new_parts = Vec::with_capacity(parts.len());
            for part in parts {
                if let Some(obj) = part.as_object() {
                    if let Some(text) = obj.get("text").and_then(|value| value.as_str()) {
                        let mut new_obj = obj.clone();
                        new_obj.insert(
                            "text".to_string(),
                            Value::String(trim_text_head_tail(text, max_chars)),
                        );
                        new_parts.push(Value::Object(new_obj));
                        continue;
                    }
                }
                new_parts.push(part.clone());
            }
            msg.content = Value::Array(new_parts);
        }
        _ => {}
    }
}

fn estimate_history_tokens(messages: &[ChatMessage]) -> usize {
    estimate_messages_tokens(messages)
}

fn trim_text_head_tail(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }

    let char_count = text.chars().count();
    if char_count <= max_chars {
        return text.to_string();
    }

    let head_chars = (max_chars * 2) / 3;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let head = text.chars().take(head_chars).collect::<String>();
    let tail = text
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();

    format!(
        "{}\n...<trimmed {} chars>...\n{}",
        head,
        char_count.saturating_sub(max_chars),
        tail
    )
}

fn detect_reference_intent(user_input: &str) -> ReferenceIntent {
    let trimmed = user_input.trim();
    if trimmed.is_empty() {
        return ReferenceIntent::None;
    }

    if trimmed.contains('第') && trimmed.chars().any(|ch| ch.is_ascii_digit()) {
        return ReferenceIntent::Ordinal;
    }

    if ["这个", "那个", "它", "上一条", "上一项", "上一个", "刚才"]
        .iter()
        .any(|keyword| trimmed.contains(keyword))
    {
        return ReferenceIntent::Demonstrative;
    }

    if ["继续", "接着", "接下来", "然后"]
        .iter()
        .any(|keyword| trimmed.contains(keyword))
    {
        return ReferenceIntent::Continue;
    }

    if ["详情", "详细", "展开", "打开", "点开", "查看", "内容", "标题"]
        .iter()
        .any(|keyword| trimmed.contains(keyword))
    {
        return ReferenceIntent::Detail;
    }

    ReferenceIntent::None
}

fn select_reference_round(
    rounds: &[HistoryRound<'_>],
    reference_intent: ReferenceIntent,
    user_input: &str,
) -> Option<usize> {
    if matches!(reference_intent, ReferenceIntent::None) {
        return None;
    }

    let mut best: Option<(usize, i32)> = None;
    let total = rounds.len();
    let ordinal_target = parse_reference_ordinal(user_input);

    for round in rounds {
        let mut score = 0i32;
        if round.has_tool_result {
            score += 5;
        }
        if round.has_structured_payload {
            score += 3;
        }
        if round.list_items_hint > 0 {
            score += 4 + round.list_items_hint.min(8) as i32;
        }
        if round.explicit_index_max > 0 {
            score += 6 + round.explicit_index_max.min(8) as i32;
        }
        if round.latest_skill_name.is_some() {
            score += 3;
        }

        match reference_intent {
            ReferenceIntent::Ordinal => {
                if let Some(target) = ordinal_target {
                    if round.explicit_index_max >= target {
                        score += 18;
                    } else if round.explicit_index_max > 0 {
                        score -= 6;
                    } else if round.list_items_hint >= target {
                        score += 2;
                    } else if round.list_items_hint == 0 {
                        score -= 8;
                    } else {
                        score -= 10;
                    }
                } else if round.explicit_index_max > 0 {
                    score += 12;
                } else if round.list_items_hint > 0 {
                    score += 2;
                } else {
                    score -= 6;
                }

                if !matches!(round.user_reference_intent, ReferenceIntent::None) {
                    score -= 8;
                }
            }
            ReferenceIntent::Demonstrative => score += 4,
            ReferenceIntent::Continue => score += 3,
            ReferenceIntent::Detail => {
                if round.list_items_hint > 0 || round.has_structured_payload {
                    score += 6;
                }
            }
            ReferenceIntent::None => {}
        }

        let distance_from_latest = total
            .saturating_sub(1)
            .saturating_sub(round.index)
            .min(10);
        let recency_bonus = (10usize.saturating_sub(distance_from_latest)) as i32;
        score += recency_bonus;

        if score < 8 {
            continue;
        }

        match best {
            Some((_, best_score)) if score > best_score => {
                best = Some((round.index, score));
            }
            Some(_) => {}
            None => {
                best = Some((round.index, score));
            }
        }
    }

    let (best_index, _) = best?;
    Some(best_index)
}

fn detect_round_list_items_hint(round: &[&ChatMessage]) -> usize {
    round
        .iter()
        .filter_map(|msg| parse_structured_content(msg))
        .map(|value| structured_list_size(&value))
        .max()
        .unwrap_or_else(|| {
            round.iter()
                .map(|msg| count_numbered_lines(&stringify_chat_message_content(msg)))
                .max()
                .unwrap_or(0)
        })
}

fn detect_round_explicit_index_max(round: &[&ChatMessage]) -> usize {
    let structured_max = round
        .iter()
        .filter_map(|msg| parse_structured_content(msg))
        .map(|value| explicit_index_max_in_value(&value))
        .max()
        .unwrap_or(0);
    let numbered_max = round
        .iter()
        .map(|msg| max_numbered_line_index(&stringify_chat_message_content(msg)))
        .max()
        .unwrap_or(0);

    structured_max.max(numbered_max)
}

fn parse_structured_content(msg: &ChatMessage) -> Option<Value> {
    let text = stringify_chat_message_content(msg);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    serde_json::from_str::<Value>(trimmed).ok()
}

fn structured_list_size(value: &Value) -> usize {
    match value {
        Value::Array(items) => items.len(),
        Value::Object(map) => {
            map.values().map(structured_list_size).max().unwrap_or(0)
        }
        _ => 0,
    }
}

fn has_structured_payload(value: &Value) -> bool {
    matches!(value, Value::Object(_) | Value::Array(_))
}

fn explicit_index_max_in_value(value: &Value) -> usize {
    match value {
        Value::Array(items) => {
            let indexed = items
                .iter()
                .filter_map(extract_explicit_position)
                .collect::<Vec<_>>();
            let indexed_max = if indexed.len() >= 2 {
                indexed.into_iter().max().unwrap_or(0)
            } else {
                0
            };
            let nested_max = items
                .iter()
                .map(explicit_index_max_in_value)
                .max()
                .unwrap_or(0);
            indexed_max.max(nested_max)
        }
        Value::Object(map) => map
            .values()
            .map(explicit_index_max_in_value)
            .max()
            .unwrap_or(0),
        _ => 0,
    }
}

fn extract_explicit_position(value: &Value) -> Option<usize> {
    let map = value.as_object()?;
    [
        "index", "position", "rank", "order", "no", "seq", "number", "序号",
    ]
    .iter()
    .find_map(|key| map.get(*key).and_then(json_value_to_usize))
}

fn json_value_to_usize(value: &Value) -> Option<usize> {
    value
        .as_u64()
        .map(|n| n as usize)
        .or_else(|| value.as_str()?.trim().parse::<usize>().ok())
}

fn count_numbered_lines(text: &str) -> usize {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with('[') && trimmed.chars().skip(1).take_while(|ch| ch.is_ascii_digit()).count() > 0
                || trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count() > 0
                    && trimmed
                        .chars()
                        .nth(trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count())
                        .is_some_and(|ch| matches!(ch, '.' | ')' | '、'))
        })
        .count()
}

fn max_numbered_line_index(text: &str) -> usize {
    text.lines()
        .filter_map(|line| parse_numbered_line_prefix(line.trim_start()))
        .max()
        .unwrap_or(0)
}

fn parse_numbered_line_prefix(line: &str) -> Option<usize> {
    let trimmed = line
        .strip_prefix('[')
        .unwrap_or(line)
        .trim_start();
    let digits = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>();
    if digits.is_empty() {
        return None;
    }
    let next = trimmed[digits.len()..].chars().next()?;
    if matches!(next, '.' | ')' | '、' | ']') {
        digits.parse::<usize>().ok()
    } else {
        None
    }
}

fn parse_reference_ordinal(user_input: &str) -> Option<usize> {
    let chars = user_input.chars().collect::<Vec<_>>();
    for (idx, ch) in chars.iter().enumerate() {
        if *ch != '第' {
            continue;
        }
        let digits = chars[idx + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        if digits.is_empty() {
            continue;
        }
        if let Ok(value) = digits.parse::<usize>() {
            return Some(value);
        }
    }
    None
}

fn stringify_chat_message_content(msg: &ChatMessage) -> String {
    match &msg.content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(|value| value.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    }
}

fn is_internal_skill_trace(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "skill_enter" | "skill_invoke_python" | "skill_invoke_rhai" | "skill_invoke_script"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::types::ToolCallRequest;

    fn build_internal_call(
        name: &str,
        skill_name: &str,
        arguments: serde_json::Value,
    ) -> ToolCallRequest {
        ToolCallRequest {
            id: format!("{}-call", name),
            name: name.to_string(),
            arguments: serde_json::json!({
                "skill_name": skill_name,
                "argv": ["search"],
                "payload": arguments,
            }),
            thought_signature: None,
        }
    }

    fn build_script_round(
        user: &str,
        skill_name: &str,
        tool_name: &str,
        tool_result: &str,
        final_answer: &str,
    ) -> Vec<ChatMessage> {
        let tool_call = build_internal_call(tool_name, skill_name, serde_json::json!({}));
        let mut tool_message = ChatMessage::tool_result(&tool_call.id, tool_result);
        tool_message.name = Some(tool_name.to_string());
        vec![
            ChatMessage::user(user),
            ChatMessage {
                role: "assistant".to_string(),
                content: serde_json::Value::String(String::new()),
                reasoning_content: None,
                tool_calls: Some(vec![tool_call]),
                tool_call_id: None,
                name: None,
            },
            tool_message,
            ChatMessage::assistant(final_answer),
        ]
    }

    #[test]
    fn test_analysis_detects_latest_skill_name_and_reference_round_for_ordinal_followup() {
        let mut history = Vec::new();
        history.extend(build_script_round(
            "查看小红书推荐",
            "xiaohongshu",
            "skill_invoke_python",
            r#"{"items":[{"index":1,"feed_id":"feed-1","xsec_token":"token-1","title":"第一条"},{"index":2,"feed_id":"feed-2","xsec_token":"token-2","title":"第二条"},{"index":3,"feed_id":"feed-3","xsec_token":"token-3","title":"第三条"},{"index":4,"feed_id":"feed-4","xsec_token":"token-4","title":"第四条"},{"index":5,"feed_id":"feed-5","xsec_token":"token-5","title":"第五条"},{"index":6,"feed_id":"feed-6","xsec_token":"token-6","title":"第六条"}]}"#,
            "找到 6 条结果",
        ));
        history.extend(build_script_round(
            "第1条标题",
            "xiaohongshu",
            "skill_invoke_python",
            r#"{"title":"第一条"}"#,
            "第一条",
        ));

        let analysis = HistoryProjector::new(&history).analyze("第6条标题");

        assert_eq!(analysis.rounds_total, 2);
        assert_eq!(analysis.latest_skill_name.as_deref(), Some("xiaohongshu"));
        assert_eq!(analysis.reference_round, Some(0));
        assert!(analysis.has_reference_intent);
    }

    #[test]
    fn test_script_planning_projection_keeps_older_reference_round() {
        let mut history = Vec::new();
        history.extend(build_script_round(
            "查看小红书推荐",
            "xiaohongshu",
            "skill_invoke_python",
            r#"{"items":[{"index":1,"feed_id":"feed-1","xsec_token":"token-1","title":"第一条"},{"index":2,"feed_id":"feed-2","xsec_token":"token-2","title":"第二条"},{"index":3,"feed_id":"feed-3","xsec_token":"token-3","title":"第三条"},{"index":4,"feed_id":"feed-4","xsec_token":"token-4","title":"第四条"},{"index":5,"feed_id":"feed-5","xsec_token":"token-5","title":"第五条"},{"index":6,"feed_id":"feed-6","xsec_token":"token-6","title":"第六条"}]}"#,
            "找到 6 条结果",
        ));
        for n in 1..=6 {
            history.extend(build_script_round(
                &format!("第{}条标题", n),
                "xiaohongshu",
                "skill_invoke_python",
                &format!(r#"{{"title":"第{}条"}}"#, n),
                &format!("第{}条", n),
            ));
        }

        let projection = HistoryProjector::new(&history).project(
            "第6条内容",
            HistoryProjectionProfile::ScriptPlanning,
            100_000,
        );

        let projection_text = projection
            .messages
            .iter()
            .map(|msg| match &msg.content {
                serde_json::Value::String(text) => text.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(projection.rounds_total, 7);
        assert_eq!(projection.reference_round, Some(0));
        assert!(projection_text.contains("feed-6"));
        assert!(projection_text.contains("token-6"));
    }

    #[test]
    fn test_ordinal_followup_prefers_original_indexed_list_over_recent_detail_round() {
        let mut history = Vec::new();
        let list_items = (1..=20)
            .map(|i| {
                serde_json::json!({
                    "index": i,
                    "title": format!("第{}条", i),
                    "feed_id": format!("feed-{}", i),
                    "xsec_token": format!("token-{}", i),
                })
            })
            .collect::<Vec<_>>();
        history.extend(build_script_round(
            "查看小红书推荐",
            "xiaohongshu",
            "skill_invoke_python",
            &serde_json::json!({
                "items": list_items,
            })
            .to_string(),
            "找到 20 条结果",
        ));
        history.extend(build_script_round(
            "查看第20条笔记内容",
            "xiaohongshu",
            "skill_invoke_python",
            r#"{
                "title":"第20条",
                "content":"这里是第20条正文",
                "comments":[
                    {"author":"甲","text":"评论1"},
                    {"author":"乙","text":"评论2"},
                    {"author":"丙","text":"评论3"},
                    {"author":"丁","text":"评论4"},
                    {"author":"戊","text":"评论5"},
                    {"author":"己","text":"评论6"},
                    {"author":"庚","text":"评论7"}
                ]
            }"#,
            "第20条详情",
        ));
        for n in 1..=4 {
            history.push(ChatMessage::user(&format!("第{}条标题", n)));
            history.push(ChatMessage::assistant(&format!("第{}条标题", n)));
        }
        history.extend(build_script_round(
            "查看第5条内容",
            "xiaohongshu",
            "skill_invoke_python",
            r#"{
                "title":"第5条",
                "content":"这里是第5条正文",
                "comments":[
                    {"author":"甲","text":"评论1"},
                    {"author":"乙","text":"评论2"},
                    {"author":"丙","text":"评论3"},
                    {"author":"丁","text":"评论4"},
                    {"author":"戊","text":"评论5"},
                    {"author":"己","text":"评论6"},
                    {"author":"庚","text":"评论7"}
                ]
            }"#,
            "第5条详情",
        ));

        let analysis = HistoryProjector::new(&history).analyze("查看第6条标题");
        assert_eq!(analysis.reference_round, Some(0));

        let projection = HistoryProjector::new(&history).project(
            "查看第6条标题",
            HistoryProjectionProfile::ScriptPlanning,
            100_000,
        );
        let projection_text = projection
            .messages
            .iter()
            .map(stringify_chat_message_content)
            .collect::<Vec<_>>()
            .join("\n");

        assert!(projection_text.contains("feed-6"));
        assert!(projection_text.contains("token-6"));
    }

    #[test]
    fn test_conversation_projection_does_not_duplicate_reference_round_already_in_recent_window() {
        let mut history = Vec::new();
        for round in 1..=4 {
            history.extend(build_script_round(
                &format!("第{}轮", round),
                "weather",
                "skill_invoke_rhai",
                &format!(r#"{{"items":[{{"index":{},"title":"第{}条"}}]}}"#, round, round),
                &format!("第{}轮结果", round),
            ));
        }

        let projection = HistoryProjector::new(&history).project(
            "查看第4条",
            HistoryProjectionProfile::Conversation,
            100_000,
        );

        let occurrences = projection
            .messages
            .iter()
            .filter(|msg| msg.content.as_str().is_some_and(|text| text.contains("第4轮结果")))
            .count();

        assert_eq!(projection.reference_round, Some(3));
        assert_eq!(occurrences, 1);
    }
}
