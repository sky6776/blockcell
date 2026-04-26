use blockcell_core::{Config, InboundMessage, Paths, Result};
use std::collections::HashSet;

use crate::token::estimate_tokens;

const GHOST_RECALL_CHANNELS_DENYLIST: [&str; 4] = ["ghost", "cron", "system", "subagent"];

pub(crate) fn should_inject_ghost_recall(config: &Config, msg: &InboundMessage) -> bool {
    config.agents.ghost.learning.enabled
        && !config.agents.ghost.learning.shadow_mode
        && config.agents.ghost.learning.recall_max_items > 0
        && !GHOST_RECALL_CHANNELS_DENYLIST.contains(&msg.channel.as_str())
}

pub fn build_ghost_recall_context_block(
    paths: &Paths,
    config: &Config,
    msg: &InboundMessage,
) -> Result<Option<String>> {
    if !should_inject_ghost_recall(config, msg) {
        return Ok(None);
    }

    let items = query_file_memory_recall_items(
        paths,
        &msg.content,
        config.agents.ghost.learning.recall_max_items as usize,
    )?;
    let Some(block) = build_memory_context_block(
        &items,
        config.agents.ghost.learning.recall_token_budget as usize,
    ) else {
        return Ok(None);
    };

    Ok(Some(block))
}

pub(crate) fn build_memory_context_block(
    items: &[FileMemoryRecallItem],
    token_budget: usize,
) -> Option<String> {
    if items.is_empty() || token_budget == 0 {
        return None;
    }

    let header = concat!(
        "<memory-context>\n",
        "Relevant durable file memory from USER.md and MEMORY.md.\n",
        "Use only when directly relevant. Current user instructions override this context.\n",
    );
    let footer = "</memory-context>";
    let mut body = String::new();
    let base_tokens = estimate_tokens(header) + estimate_tokens(footer);
    let mut used_tokens = base_tokens;
    let mut included = 0usize;

    for item in items {
        let entry = format!(
            "- [{}] {}\n",
            item.source,
            truncate_chars(item.content.trim(), 260)
        );
        let entry_tokens = estimate_tokens(&entry);
        if included > 0 && used_tokens + entry_tokens > token_budget {
            break;
        }
        if included == 0 && used_tokens + entry_tokens > token_budget {
            return None;
        }
        body.push_str(&entry);
        used_tokens += entry_tokens;
        included += 1;
    }

    if included == 0 {
        return None;
    }

    Some(format!("{header}{body}{footer}"))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let truncated = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        format!("{truncated}...")
    } else {
        truncated
    }
}

pub(crate) fn query_file_memory_recall_items(
    paths: &Paths,
    raw_query: &str,
    limit: usize,
) -> Result<Vec<FileMemoryRecallItem>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let query_tokens = normalize_recall_tokens(raw_query);
    if query_tokens.is_empty() {
        return Ok(Vec::new());
    }

    let mut collected = collect_file_memory_items(paths, &query_tokens)?;
    collected.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.content.cmp(&right.content))
    });
    collected.truncate(limit);
    Ok(collected)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileMemoryRecallItem {
    pub(crate) source: &'static str,
    pub(crate) content: String,
    pub(crate) score: usize,
}

fn collect_file_memory_items(
    paths: &Paths,
    query_tokens: &[String],
) -> Result<Vec<FileMemoryRecallItem>> {
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for (source, path) in [
        ("USER.md", paths.user_md()),
        ("MEMORY.md", paths.memory_md()),
    ] {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        for chunk in memory_chunks(&content) {
            let score = recall_score(&chunk, query_tokens);
            if score == 0 {
                continue;
            }
            let key = format!("{source}:{chunk}");
            if seen.insert(key) {
                items.push(FileMemoryRecallItem {
                    source,
                    content: chunk,
                    score,
                });
            }
        }
    }
    Ok(items)
}

fn memory_chunks(content: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut paragraph = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !paragraph.trim().is_empty() {
                chunks.push(paragraph.trim().to_string());
                paragraph.clear();
            }
            continue;
        }
        if trimmed.starts_with('-') || trimmed.starts_with('*') || trimmed.starts_with('#') {
            if !paragraph.trim().is_empty() {
                chunks.push(paragraph.trim().to_string());
                paragraph.clear();
            }
            chunks.push(trimmed.to_string());
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(trimmed);
    }
    if !paragraph.trim().is_empty() {
        chunks.push(paragraph.trim().to_string());
    }
    chunks
}

fn recall_score(chunk: &str, query_tokens: &[String]) -> usize {
    let lower = chunk.to_lowercase();
    query_tokens
        .iter()
        .map(|token| {
            if lower.contains(token) {
                token.len().max(1)
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::{Config, InboundMessage, Paths};

    fn test_msg(content: &str) -> InboundMessage {
        InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-1".to_string(),
            content: content.to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    #[test]
    fn file_memory_recall_builds_memory_context_fence() {
        let base = std::env::temp_dir().join(format!(
            "blockcell-ghost-recall-test-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = Paths::with_base(base);
        paths.ensure_dirs().expect("ensure dirs");
        std::fs::write(
            paths.memory_md(),
            "Deploy docs should include a rollback checklist.\n\nUnrelated note.",
        )
        .expect("write memory md");
        let mut config = Config::default();
        config.agents.ghost.learning.enabled = true;
        config.agents.ghost.learning.shadow_mode = false;
        config.agents.ghost.learning.recall_max_items = 2;
        config.agents.ghost.learning.recall_token_budget = 160;

        let text = build_ghost_recall_context_block(&paths, &config, &test_msg("deploy docs"))
            .expect("recall")
            .expect("context block");
        assert!(text.contains("<memory-context>"));
        assert!(text.contains("rollback checklist"));
        assert!(!text.contains("<ghost-recall>"));
    }

    #[test]
    fn file_memory_recall_skips_irrelevant_memory() {
        let base = std::env::temp_dir().join(format!(
            "blockcell-ghost-recall-test-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = Paths::with_base(base);
        paths.ensure_dirs().expect("ensure dirs");
        std::fs::write(paths.memory_md(), "Only remember invoice formatting.")
            .expect("write memory md");
        let mut config = Config::default();
        config.agents.ghost.learning.enabled = true;
        config.agents.ghost.learning.shadow_mode = false;
        config.agents.ghost.learning.recall_max_items = 2;
        config.agents.ghost.learning.recall_token_budget = 160;

        let message = build_ghost_recall_context_block(&paths, &config, &test_msg("deploy docs"))
            .expect("recall");
        assert!(message.is_none());
    }
}

fn normalize_recall_tokens(raw_query: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "and", "are", "do", "does", "how", "i", "if", "in", "is", "it", "like", "my",
        "of", "on", "or", "the", "to", "usually", "we", "what", "written", "would", "you",
    ];

    raw_query
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|token| token.trim().to_lowercase())
        .filter(|token| !token.is_empty())
        .filter(|token| !STOP_WORDS.contains(&token.as_str()))
        .collect()
}
