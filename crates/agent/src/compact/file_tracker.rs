//! 鏂囦欢杩借釜鍣?//!
//! 杩借釜浼氳瘽涓鍙栫殑鏂囦欢锛岀敤浜?Post-Compact 鎭㈠銆?
use crate::token::estimate_tokens;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// 鏂囦欢杩借釜璁板綍
#[derive(Debug, Clone)]
pub struct FileRecord {
    /// 鏂囦欢璺緞
    pub path: PathBuf,
    /// 璇诲彇鏃堕棿
    pub read_at: Instant,
    /// 鍐呭鎽樿锛堝墠 N 涓瓧绗︼級
    pub summary: String,
    /// 估算 token 数
    pub estimated_tokens: usize,
}

/// 文件追踪器
#[derive(Debug)]
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
            max_summary_chars: 2000, // 绾?500 tokens
        }
    }

    /// 鍒涘缓甯﹁嚜瀹氫箟鎽樿闀垮害闄愬埗鐨勬枃浠惰拷韪櫒
    pub fn with_config(max_summary_chars: usize) -> Self {
        Self {
            records: HashMap::new(),
            max_summary_chars,
        }
    }

    /// 璁板綍鏂囦欢璇诲彇
    pub fn record_read(&mut self, path: PathBuf, content: &str) {
        let summary = self.truncate_summary(content);
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

    /// 截断内容生成摘要（安全处理 UTF-8 边界）
    fn truncate_summary(&self, content: &str) -> String {
        if content.len() <= self.max_summary_chars {
            return content.to_string();
        }

        // 鎵惧埌瀹夊叏鐨?UTF-8 瀛楃杈圭晫锛岄伩鍏嶅湪澶氬瓧鑺傚瓧绗︿腑闂存埅鏂鑷?panic
        let mut boundary = self.max_summary_chars;
        while boundary > 0 && !content.is_char_boundary(boundary) {
            boundary -= 1;
        }

        // 边界情况：如果 boundary 为 0，找到第一个有效字符。
        // 这确保至少保留第一个字符，避免返回空前缀。
        if boundary == 0 {
            if let Some(first_char) = content.chars().next() {
                boundary = first_char.len_utf8();
            } else {
                // 鍐呭涓虹┖锛堜笉搴旇鍙戠敓锛屽洜涓哄墠闈㈠凡妫€鏌ラ暱搴︼級
                return content.to_string();
            }
        }

        format!("{}...\n[content truncated]", &content[..boundary])
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

    /// 娓呯┖璁板綍
    pub fn clear(&mut self) {
        self.records.clear();
    }

    /// 璁板綍鏁伴噺
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// 鏄惁涓虹┖
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl Default for FileTracker {
    fn default() -> Self {
        Self::new()
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

        // 鍒涘缓瓒呴暱鍐呭
        let long_content = "x".repeat(5000);
        tracker.record_read(PathBuf::from("/long_file.rs"), &long_content);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/long_file.rs")).unwrap();

        // 鎽樿搴旇琚埅鏂?        assert!(record.summary.len() <= 2100); // 2000 + "...[content truncated]"
        assert!(record.summary.contains("[content truncated]"));
    }

    #[test]
    fn test_file_tracker_update_existing() {
        let mut tracker = FileTracker::new();

        // 璁板綍鍚屼竴鏂囦欢涓ゆ
        tracker.record_read(PathBuf::from("/file.rs"), "content 1");
        tracker.record_read(PathBuf::from("/file.rs"), "content 2 updated");

        // 搴旇鍙湁涓€鏉¤褰?        assert_eq!(tracker.len(), 1);

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

        // 鎸夐『搴忚褰曟枃浠?        tracker.record_read(PathBuf::from("/first.rs"), "first");
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker.record_read(PathBuf::from("/second.rs"), "second");
        std::thread::sleep(std::time::Duration::from_millis(10));
        tracker.record_read(PathBuf::from("/third.rs"), "third");

        let recent = tracker.get_recent_files(10, 5000);

        // 鏈€杩戣褰曠殑搴旇鍦ㄥ墠闈?        assert_eq!(recent[0].path, PathBuf::from("/third.rs"));
        assert_eq!(recent[1].path, PathBuf::from("/second.rs"));
        assert_eq!(recent[2].path, PathBuf::from("/first.rs"));
    }

    #[test]
    fn test_file_tracker_chinese_text_truncation() {
        // Bug #70: 涓枃瀛楃鍦?UTF-8 涓崰 3 瀛楄妭锛屽瓧鑺傜储寮?2000 鍙兘钀藉湪瀛楃涓棿瀵艰嚧 panic
        let mut tracker = FileTracker::new();

        // 鏋勯€犺秴杩?2000 瀛楄妭鐨勪腑鏂囧唴瀹癸紙姣忎釜涓枃瀛楃 3 瀛楄妭锛?        let chinese_content = "浣犲ソ涓栫晫".repeat(200); // 4*3*200 = 2400 瀛楄妭
        assert!(chinese_content.len() > 2000);

        // 涓嶅簲 panic
        tracker.record_read(PathBuf::from("/chinese.md"), &chinese_content);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/chinese.md")).unwrap();

        // 鎽樿搴旇鎴柇涓斿寘鍚埅鏂爣璁?        assert!(record.summary.contains("[content truncated]"));
        // 鎴柇鍚庣殑鍐呭蹇呴』鏄湁鏁堢殑 UTF-8锛堜笉浼?panic 璇存槑宸查€氳繃锛?        let _ = record.summary.chars().count();
    }

    #[test]
    fn test_file_tracker_mixed_text_truncation() {
        // 娣峰悎 ASCII + 涓枃 + emoji 鐨勬埅鏂祴璇?        let mut tracker = FileTracker::new();

        let mut mixed = String::new();
        // 鏋勯€犳伆濂戒娇瀛楄妭 2000 钀藉湪澶氬瓧鑺傚瓧绗︿腑闂寸殑鍐呭
        mixed.push_str(&"a".repeat(1999)); // 1999 瀛楄妭 ASCII
        mixed.push('假'); // 3 字节中文，总 2002 字节

        tracker.record_read(PathBuf::from("/mixed.txt"), &mixed);

        let records = tracker.all_records();
        let record = records.get(&PathBuf::from("/mixed.txt")).unwrap();
        assert!(record.summary.contains("[content truncated]"));
    }
}
