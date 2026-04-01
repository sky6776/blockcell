//! Token estimation utilities for context management.
//!
//! Provides lightweight token counting for chat messages, supporting:
//! - ASCII text with word-based estimation
//! - CJK characters with per-character counting
//! - Reasoning content (thinking) overhead
//! - Tool call overhead

use blockcell_core::types::ChatMessage;

/// Lightweight token estimator.
/// Chinese characters ≈ 1 token each, English words ≈ 1.3 tokens each.
/// This is intentionally conservative (over-estimates) to avoid context overflow.
pub(crate) fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut tokens: usize = 0;
    let mut ascii_word_chars: usize = 0;
    for ch in text.chars() {
        if ch.is_ascii() {
            if ch.is_ascii_whitespace() || ch.is_ascii_punctuation() {
                if ascii_word_chars > 0 {
                    // ~1.3 tokens per English word, round up
                    tokens += 1 + ascii_word_chars / 4;
                    ascii_word_chars = 0;
                }
                // whitespace/punctuation: ~0.25 tokens each, batch them
                tokens += 1;
            } else {
                ascii_word_chars += 1;
            }
        } else {
            // Flush pending ASCII word
            if ascii_word_chars > 0 {
                tokens += 1 + ascii_word_chars / 4;
                ascii_word_chars = 0;
            }
            // CJK and other multi-byte: ~1 token per character
            tokens += 1;
        }
    }
    // Flush trailing ASCII word
    if ascii_word_chars > 0 {
        tokens += 1 + ascii_word_chars / 4;
    }
    // Add per-message overhead (role markers, formatting)
    tokens + 4
}

/// Estimate tokens for reasoning content in a ChatMessage.
pub(crate) fn estimate_thinking_tokens(msg: &ChatMessage) -> usize {
    msg.reasoning_content
        .as_ref()
        .map(|r| estimate_tokens(r))
        .unwrap_or(0)
}

/// Estimate tokens for a single ChatMessage (content + tool_calls + thinking overhead).
pub(crate) fn estimate_message_tokens(msg: &ChatMessage) -> usize {
    let content_tokens = match &msg.content {
        serde_json::Value::String(s) => estimate_tokens(s),
        serde_json::Value::Array(parts) => {
            parts
                .iter()
                .map(|p| {
                    if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                        estimate_tokens(text)
                    } else if p.get("image_url").is_some() {
                        // Base64 images: ~85 tokens for low-detail, ~765 for high-detail
                        // Use conservative estimate
                        200
                    } else {
                        10
                    }
                })
                .sum()
        }
        _ => 0,
    };
    let tool_call_tokens = msg.tool_calls.as_ref().map_or(0, |calls| {
        calls
            .iter()
            .map(|tc| estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments.to_string()) + 10)
            .sum()
    });
    let thinking_tokens = estimate_thinking_tokens(msg);
    content_tokens + tool_call_tokens + thinking_tokens + 4 // role overhead
}

/// Estimate the total token count for a slice of chat messages.
pub(crate) fn estimate_messages_tokens(messages: &[ChatMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens_ascii() {
        // "hello world" = 11 ASCII chars
        // Algorithm: whitespace at positions 5 and 10
        // "hello" = 5 chars -> 1 + 5/4 = 2 tokens
        // "world" = 5 chars -> 1 + 5/4 = 2 tokens
        // 2 whitespace -> 2 tokens
        // overhead +4
        // Total: 2 + 2 + 2 + 4 = 10 tokens
        let tokens = estimate_tokens("hello world");
        assert!(tokens > 0 && tokens < 15);
    }

    #[test]
    fn test_estimate_tokens_cjk() {
        // "你好世界" = 4 CJK chars
        // Algorithm: each CJK char = 1 token
        // 4 tokens + 4 overhead = 8 tokens
        let tokens = estimate_tokens("你好世界");
        assert!((5..=12).contains(&tokens));
    }

    #[test]
    fn test_estimate_tokens_mixed() {
        // Mixed ASCII and CJK
        let tokens = estimate_tokens("hello 你好 world 世界");
        assert!(tokens > 10 && tokens < 25);
    }

    #[test]
    fn test_estimate_message_tokens_simple() {
        let msg = ChatMessage::user("测试消息");
        let tokens = estimate_message_tokens(&msg);
        assert!(tokens > 0);
    }

    #[test]
    fn test_estimate_messages_tokens_multiple() {
        let messages = vec![
            ChatMessage::user("hello"),
            ChatMessage::assistant("world"),
        ];
        let tokens = estimate_messages_tokens(&messages);
        assert!(tokens > 0);
    }
}