//! 技能追踪器
//!
//! 追踪会话中加载的技能，用于 Post-Compact 恢复。

use crate::token::estimate_tokens;
use std::collections::HashMap;
use std::time::Instant;

/// 技能追踪记录
#[derive(Debug, Clone)]
pub struct SkillRecord {
    /// 技能名称
    pub name: String,
    /// 加载时间
    pub loaded_at: Instant,
    /// 技能内容摘要
    pub summary: String,
    /// 估算 token 数
    pub estimated_tokens: usize,
}

/// 技能追踪器
#[derive(Debug, Default)]
pub struct SkillTracker {
    /// 已加载的技能记录（名称 -> 记录）
    records: HashMap<String, SkillRecord>,
    /// 摘要最大长度
    max_summary_chars: usize,
}

impl SkillTracker {
    /// 创建新的技能追踪器
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            max_summary_chars: 2000, // 约 500 tokens
        }
    }

    /// 记录技能加载
    pub fn record_load(&mut self, name: &str, content: &str) {
        let summary = truncate_to_chars(content, self.max_summary_chars);

        let estimated_tokens = estimate_tokens(content);

        self.records.insert(
            name.to_string(),
            SkillRecord {
                name: name.to_string(),
                loaded_at: Instant::now(),
                summary,
                estimated_tokens,
            },
        );
    }

    /// 获取最近加载的技能（按时间排序）
    pub fn get_recent_skills(&self, _max_tokens_per_skill: usize) -> Vec<&SkillRecord> {
        let mut records: Vec<_> = self.records.values().collect();

        // 按加载时间降序排序（最近的优先）
        records.sort_by(|a, b| b.loaded_at.cmp(&a.loaded_at));

        records
    }

    /// 获取所有记录
    pub fn all_records(&self) -> &HashMap<String, SkillRecord> {
        &self.records
    }

    /// 清空记录
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// 记录数量
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// 安全截断字符串到指定字符数，避免按字节切片破坏 UTF-8 边界。
fn truncate_to_chars(content: &str, max_chars: usize) -> String {
    if content.chars().count() <= max_chars {
        return content.to_string();
    }

    let truncated: String = content.chars().take(max_chars).collect();
    format!("{}...\n[content truncated]", truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skill_tracker() {
        let mut tracker = SkillTracker::new();

        tracker.record_load("skill1", "skill content 1");
        tracker.record_load("skill2", "skill content 2");

        assert_eq!(tracker.len(), 2);

        let recent = tracker.get_recent_skills(5000);
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn test_skill_tracker_update_existing() {
        let mut tracker = SkillTracker::new();

        // 记录同一技能两次
        tracker.record_load("skill1", "content 1");
        tracker.record_load("skill1", "content 2 updated");

        // 应该只有一条记录
        assert_eq!(tracker.len(), 1);

        let records = tracker.all_records();
        let record = records.get("skill1").unwrap();
        assert!(record.summary.contains("updated"));
    }

    #[test]
    fn test_skill_tracker_summary_truncation() {
        let mut tracker = SkillTracker::new();

        // 创建超长内容
        let long_content = "x".repeat(5000);
        tracker.record_load("long_skill", &long_content);

        let records = tracker.all_records();
        let record = records.get("long_skill").unwrap();

        // 摘要应该被截断
        assert!(record.summary.len() <= 2100); // 2000 + "...[content truncated]"
        assert!(record.summary.contains("[content truncated]"));
    }

    #[test]
    fn test_skill_tracker_summary_truncation_utf8() {
        let mut tracker = SkillTracker::new();

        let long_content = "小".repeat(3000);
        tracker.record_load("utf8_skill", &long_content);

        let records = tracker.all_records();
        let record = records.get("utf8_skill").unwrap();

        assert!(record.summary.contains("[content truncated]"));
        assert!(record.summary.starts_with(&"小".repeat(10)));
    }

    #[test]
    fn test_skill_tracker_clear() {
        let mut tracker = SkillTracker::new();

        tracker.record_load("skill1", "content 1");
        tracker.record_load("skill2", "content 2");

        assert_eq!(tracker.len(), 2);

        tracker.clear();

        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_skill_tracker_token_estimation() {
        let mut tracker = SkillTracker::new();

        let content = "a".repeat(1000); // ~125-333 tokens depending on encoding
        tracker.record_load("test_skill", &content);

        let records = tracker.all_records();
        let record = records.get("test_skill").unwrap();

        // Token estimation varies by implementation (tiktoken vs fallback)
        // For repeated 'a' characters, tiktoken may compress more efficiently
        assert!(record.estimated_tokens > 0 && record.estimated_tokens <= 1000);
    }

    #[test]
    fn test_skill_tracker_recent_order() {
        let mut tracker = SkillTracker::new();

        // 按顺序记录技能
        tracker.record_load("first", "first");
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker.record_load("second", "second");
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker.record_load("third", "third");

        let recent = tracker.get_recent_skills(5000);

        // 最近记录的应该在前面
        assert_eq!(recent[0].name, "third");
        assert_eq!(recent[1].name, "second");
        assert_eq!(recent[2].name, "first");
    }

    #[test]
    fn test_skill_tracker_default() {
        let tracker = SkillTracker::default();
        assert!(tracker.is_empty());
    }
}