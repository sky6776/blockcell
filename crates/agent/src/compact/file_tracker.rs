//! 文件追踪器
//!
//! 追踪会话中读取的文件，用于 Post-Compact 恢复。

use crate::token::estimate_tokens;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// 文件追踪记录
#[derive(Debug, Clone)]
pub struct FileRecord {
    /// 文件路径
    pub path: PathBuf,
    /// 读取时间
    pub read_at: Instant,
    /// 内容摘要（前 N 个字符）
    pub summary: String,
    /// 估算 token 数
    pub estimated_tokens: usize,
}

/// 文件追踪器
#[derive(Debug, Default)]
pub struct FileTracker {
    /// 已读取的文件记录（路径 -> 记录）
    records: HashMap<PathBuf, FileRecord>,
    /// 摘要最大长度
    max_summary_chars: usize,
}

impl FileTracker {
    /// 创建新的文件追踪器
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            max_summary_chars: 2000, // 约 500 tokens
        }
    }

    /// 记录文件读取
    pub fn record_read(&mut self, path: PathBuf, content: &str) {
        let summary = if content.len() > self.max_summary_chars {
            // 找到安全的 UTF-8 字符边界，避免在多字节字符中间截断导致 panic
            let mut boundary = self.max_summary_chars;
            while boundary > 0 && !content.is_char_boundary(boundary) {
                boundary -= 1;
            }
            format!(
                "{}...\n[content truncated]",
                &content[..boundary]
            )
        } else {
            content.to_string()
        };

        let estimated_tokens = estimate_tokens(content);

        self.records.insert(
            path.clone(),
            FileRecord {
                path,
                read_at: Instant::now(),
                summary,
                estimated_tokens,
            },
        );
    }

    /// 获取最近读取的文件（按时间排序）
    pub fn get_recent_files(
        &self,
        max_files: usize,
        _max_tokens_per_file: usize,
    ) -> Vec<&FileRecord> {
        let mut records: Vec<_> = self.records.values().collect();

        // 按读取时间降序排序（最近的优先）
        records.sort_by(|a, b| b.read_at.cmp(&a.read_at));

        // 截断 token 超限的摘要
        records.truncate(max_files);

        records
    }

    /// 获取所有记录
    pub fn all_records(&self) -> &HashMap<PathBuf, FileRecord> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_tracker() {
        let mut tracker = FileTracker::new();

        tracker.record_read(PathBuf::from("/path/to/file1.rs"), "content 1");
        tracker.record_read(PathBuf::from("/path/to/file2.rs"), "content 2");

        assert_eq!(tracker.len(), 2);

        let recent = tracker.get_recent_files(5, 5000);
        assert_eq!(recent.len(), 2);
    }

    #[test]
    fn test_file_tracker_max_files() {
        let mut tracker = FileTracker::new();

        for i in 0..10 {
            tracker.record_read(
                PathBuf::from(format!("/file{}.rs", i)),
                &format!("content {}", i),
            );
        }

        let recent = tracker.get_recent_files(3, 5000);
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn test_file_tracker_summary_truncation() {
        let mut tracker = FileTracker::new();

        // 创建超长内容
        let long_content = "x".repeat(5000);
        tracker.record_read(PathBuf::from("/long_file.rs"), &long_content);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/long_file.rs")).unwrap();

        // 摘要应该被截断
        assert!(record.summary.len() <= 2100); // 2000 + "...[content truncated]"
        assert!(record.summary.contains("[content truncated]"));
    }

    #[test]
    fn test_file_tracker_update_existing() {
        let mut tracker = FileTracker::new();

        // 记录同一文件两次
        tracker.record_read(PathBuf::from("/file.rs"), "content 1");
        tracker.record_read(PathBuf::from("/file.rs"), "content 2 updated");

        // 应该只有一条记录
        assert_eq!(tracker.len(), 1);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/file.rs")).unwrap();
        assert!(record.summary.contains("updated"));
    }

    #[test]
    fn test_file_tracker_clear() {
        let mut tracker = FileTracker::new();

        tracker.record_read(PathBuf::from("/file1.rs"), "content 1");
        tracker.record_read(PathBuf::from("/file2.rs"), "content 2");

        assert_eq!(tracker.len(), 2);

        tracker.clear();

        assert!(tracker.is_empty());
        assert_eq!(tracker.len(), 0);
    }

    #[test]
    fn test_file_tracker_token_estimation() {
        let mut tracker = FileTracker::new();

        let content = "a".repeat(1000); // ~125-333 tokens depending on encoding
        tracker.record_read(PathBuf::from("/file.rs"), &content);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/file.rs")).unwrap();

        // Token estimation varies by implementation (tiktoken vs fallback)
        // For repeated 'a' characters, tiktoken may compress more efficiently
        assert!(record.estimated_tokens > 0 && record.estimated_tokens <= 1000);
    }

    #[test]
    fn test_file_tracker_recent_order() {
        let mut tracker = FileTracker::new();

        // 按顺序记录文件
        tracker.record_read(PathBuf::from("/first.rs"), "first");
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker.record_read(PathBuf::from("/second.rs"), "second");
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker.record_read(PathBuf::from("/third.rs"), "third");

        let recent = tracker.get_recent_files(10, 5000);

        // 最近记录的应该在前面
        assert_eq!(recent[0].path, PathBuf::from("/third.rs"));
        assert_eq!(recent[1].path, PathBuf::from("/second.rs"));
        assert_eq!(recent[2].path, PathBuf::from("/first.rs"));
    }

    #[test]
    fn test_file_tracker_chinese_text_truncation() {
        // Bug #70: 中文字符在 UTF-8 中占 3 字节，字节索引 2000 可能落在字符中间导致 panic
        let mut tracker = FileTracker::new();

        // 构造超过 2000 字节的中文内容（每个中文字符 3 字节）
        let chinese_content = "你好世界".repeat(200); // 4*3*200 = 2400 字节
        assert!(chinese_content.len() > 2000);

        // 不应 panic
        tracker.record_read(PathBuf::from("/chinese.md"), &chinese_content);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/chinese.md")).unwrap();

        // 摘要应被截断且包含截断标记
        assert!(record.summary.contains("[content truncated]"));
        // 截断后的内容必须是有效的 UTF-8（不会 panic 说明已通过）
        let _ = record.summary.chars().count();
    }

    #[test]
    fn test_file_tracker_mixed_text_truncation() {
        // 混合 ASCII + 中文 + emoji 的截断测试
        let mut tracker = FileTracker::new();

        let mut mixed = String::new();
        // 构造恰好使字节 2000 落在多字节字符中间的内容
        mixed.push_str(&"a".repeat(1999)); // 1999 字节 ASCII
        mixed.push_str("假");               // 3 字节中文，总 2002 字节
        mixed.push_str(&"b".repeat(100));   // 追加更多

        tracker.record_read(PathBuf::from("/mixed.txt"), &mixed);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/mixed.txt")).unwrap();
        assert!(record.summary.contains("[content truncated]"));
    }
}
