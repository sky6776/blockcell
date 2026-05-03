//! 提取游标管理
//!
//! 管理每种记忆类型的提取进度游标。
//!
//! ## 三重冷却机制
//!
//! 1. **消息计数冷却**: 距离上次提取需要经过一定数量的消息
//! 2. **时间冷却**: 距离上次提取需要经过一定时间
//! 3. **内容变化检测**: 消息内容需要有实质性变化
//!
//! ## 时间测量的安全性
//!
//! 使用 `Instant` (monotonic clock) 替代 `SystemTime` 进行时间差计算，
//! 避免系统时钟调整（NTP 同步、手动修改、时区变化等）导致的问题。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::fs;
use uuid::Uuid;

use super::memory_type::MemoryType;

/// 时间冷却阈值（秒）
///
/// 默认 5 分钟 = 300 秒
///
/// 仅用作 AutoMemoryConfig::default() 的回退值，
/// 运行时使用 Layer5Config.extraction_time_cooldown_secs
pub const TIME_COOLDOWN_SECS: u64 = 300;

/// 内容变化阈值（字符数）
///
/// 默认值 500，可通过 Layer5Config.content_change_threshold 配置。
/// 仅用作 AutoMemoryConfig::default() 的回退值，
/// 运行时使用 Layer5Config.content_change_threshold
pub const CONTENT_CHANGE_THRESHOLD: usize = 500;

/// 单个记忆类型的游标
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionCursor {
    /// 记忆类型
    pub memory_type: MemoryType,
    /// 上次提取的消息 UUID
    pub last_extracted_uuid: Option<Uuid>,
    /// 上次提取时的消息数
    pub last_message_count: usize,
    /// 上次提取时间戳（秒，用于持久化）
    pub last_extraction_time: Option<u64>,
    /// 提取次数
    pub extraction_count: usize,
    /// 上次提取时的内容签名（用于检测内容变化）
    pub last_content_signature: Option<u64>,
    /// 上次提取时的内容长度（用于精确计算内容变化量）
    pub last_content_length: Option<usize>,
    /// 上次提取的 monotonic 时间点（不序列化，运行时使用）
    #[serde(skip)]
    pub last_extraction_instant: Option<Instant>,
}

impl ExtractionCursor {
    /// 创建新游标
    pub fn new(memory_type: MemoryType) -> Self {
        Self {
            memory_type,
            last_extracted_uuid: None,
            last_message_count: 0,
            last_extraction_time: None,
            extraction_count: 0,
            last_content_signature: None,
            last_content_length: None,
            last_extraction_instant: None,
        }
    }

    /// 检查是否需要提取（消息计数冷却）
    pub fn should_extract(&self, current_message_count: usize, cooldown: usize) -> bool {
        let messages_since_last = current_message_count.saturating_sub(self.last_message_count);
        messages_since_last >= cooldown
    }

    /// 检查是否满足时间冷却
    ///
    /// 使用 monotonic clock (`Instant`) 进行时间差计算，
    /// 不受系统时钟调整影响。
    ///
    /// 返回 true 表示时间冷却已满足（可以提取）
    pub fn check_time_cooldown(&self, cooldown_secs: u64) -> bool {
        // 优先使用 Instant (monotonic clock)
        if let Some(instant) = self.last_extraction_instant {
            let elapsed = instant.elapsed().as_secs();
            return elapsed >= cooldown_secs;
        }

        // 回退到 SystemTime（用于从持久化状态恢复的情况）
        let last_time = match self.last_extraction_time {
            Some(t) => t,
            None => return true, // 从未提取过，时间冷却通过
        };

        // 使用 match 替代 unwrap_or_default，以便记录警告
        let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(e) => {
                // 系统时钟异常（罕见情况：嵌入式系统、NTP 同步错误等）
                // 使用 0 作为 fallback，可能导致时间冷却立即通过
                tracing::warn!(
                    error = %e,
                    "[cursor] System clock before Unix epoch, time cooldown check may be inaccurate"
                );
                0
            }
        };

        let elapsed = now.saturating_sub(last_time);
        elapsed >= cooldown_secs
    }

    /// 检查内容是否有实质性变化
    ///
    /// 通过计算内容签名来检测变化
    /// `content_change_threshold` 来自 Layer5Config.content_change_threshold
    pub fn check_content_change(
        &self,
        current_content: &str,
        content_change_threshold: usize,
    ) -> bool {
        let current_signature = compute_content_signature(current_content);

        match self.last_content_signature {
            Some(last_sig) => {
                if current_signature == last_sig {
                    return false; // 签名相同，内容未变
                }
                // 签名不同，检查内容长度变化量
                let last_len = self.last_content_length.unwrap_or(0);
                let content_delta = current_content.len().abs_diff(last_len);
                content_delta >= content_change_threshold
            }
            None => true, // 从未提取过，内容变化通过
        }
    }

    /// 检查内容是否有实质性变化（使用默认阈值）
    ///
    /// 便捷方法，使用 CONTENT_CHANGE_THRESHOLD 常量作为默认值
    pub fn check_content_change_default(&self, current_content: &str) -> bool {
        self.check_content_change(current_content, CONTENT_CHANGE_THRESHOLD)
    }

    /// 综合检查是否应该提取
    ///
    /// 三个条件：
    /// 1. 消息计数冷却
    /// 2. 时间冷却
    /// 3. 内容变化（可选，根据 need_content_change 参数）
    ///
    /// `content_change_threshold` 来自 Layer5Config.content_change_threshold
    pub fn should_extract_full(
        &self,
        current_message_count: usize,
        current_content: &str,
        message_cooldown: usize,
        time_cooldown_secs: u64,
        require_content_change: bool,
        content_change_threshold: usize,
    ) -> ExtractionDecision {
        // 1. 消息计数冷却
        let messages_since_last = current_message_count.saturating_sub(self.last_message_count);
        let message_cooldown_met = messages_since_last >= message_cooldown;

        if !message_cooldown_met {
            return ExtractionDecision::Wait {
                reason: ExtractionWaitReason::MessageCooldown {
                    current: messages_since_last,
                    required: message_cooldown,
                },
            };
        }

        // 2. 时间冷却
        let time_cooldown_met = self.check_time_cooldown(time_cooldown_secs);

        if !time_cooldown_met {
            let elapsed = if let Some(instant) = self.last_extraction_instant {
                instant.elapsed().as_secs()
            } else if let Some(last_time) = self.last_extraction_time {
                // 使用 match 替代 unwrap_or_default，以便记录警告
                let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                    Ok(d) => d.as_secs(),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "[cursor] System clock error in time cooldown calculation"
                        );
                        0
                    }
                };
                now.saturating_sub(last_time)
            } else {
                0
            };

            return ExtractionDecision::Wait {
                reason: ExtractionWaitReason::TimeCooldown {
                    elapsed_secs: elapsed,
                    required_secs: time_cooldown_secs,
                },
            };
        }

        // 3. 内容变化（可选）
        if require_content_change {
            let content_changed =
                self.check_content_change(current_content, content_change_threshold);
            if !content_changed {
                return ExtractionDecision::Wait {
                    reason: ExtractionWaitReason::NoContentChange,
                };
            }
        }

        ExtractionDecision::Proceed
    }

    /// 更新游标
    pub fn update(&mut self, message_uuid: Uuid, message_count: usize) {
        self.last_extracted_uuid = Some(message_uuid);
        self.last_message_count = message_count;
        // 使用 monotonic clock 记录时间
        self.last_extraction_instant = Some(Instant::now());
        // 同时记录 Unix 时间戳用于持久化
        // 使用 match 替代 unwrap_or_default，持久化时 0 值是可接受的 fallback
        self.last_extraction_time = Some(
            match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                Ok(d) => d.as_secs(),
                Err(e) => {
                    // 系统时钟异常，使用 0 作为 fallback（持久化时可接受）
                    tracing::warn!(
                        error = %e,
                        "[cursor] System clock error when updating cursor, using 0 as timestamp"
                    );
                    0
                }
            },
        );
        self.extraction_count += 1;
    }

    /// 更新游标（包含内容签名和长度）
    pub fn update_with_content(&mut self, message_uuid: Uuid, message_count: usize, content: &str) {
        self.update(message_uuid, message_count);
        self.last_content_signature = Some(compute_content_signature(content));
        self.last_content_length = Some(content.len());
    }
}

/// 提取决策
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionDecision {
    /// 可以进行提取
    Proceed,
    /// 需要等待
    Wait { reason: ExtractionWaitReason },
}

/// 等待原因
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractionWaitReason {
    /// 消息计数冷却未满足
    MessageCooldown { current: usize, required: usize },
    /// 时间冷却未满足
    TimeCooldown {
        elapsed_secs: u64,
        required_secs: u64,
    },
    /// 内容无变化
    NoContentChange,
}

/// 计算内容签名
///
/// 使用简单的哈希算法来检测内容变化
fn compute_content_signature(content: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

/// 游标管理器
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionCursorManager {
    /// 各记忆类型的游标
    cursors: HashMap<String, ExtractionCursor>,
    /// 游标文件路径
    cursor_file_path: PathBuf,
}

impl ExtractionCursorManager {
    /// 创建新的游标管理器
    pub fn new(config_dir: &Path) -> Self {
        let cursor_file_path = config_dir.join("memory").join(".cursors.json");

        Self {
            cursors: HashMap::new(),
            cursor_file_path,
        }
    }

    /// 加载游标状态
    pub async fn load(&mut self) -> std::io::Result<()> {
        if let Ok(content) = fs::read_to_string(&self.cursor_file_path).await {
            if let Ok(manager) = serde_json::from_str::<ExtractionCursorManager>(&content) {
                self.cursors = manager.cursors;
            } else {
                // JSON 解析失败，备份损坏文件后使用默认值
                tracing::warn!(
                    path = %self.cursor_file_path.display(),
                    "[cursor] Failed to parse cursor file, backing up and using defaults"
                );
                // 备份损坏的文件，避免数据永久丢失
                let backup_path = self.cursor_file_path.with_extension("cursors.json.bak");
                if let Err(e) = fs::rename(&self.cursor_file_path, &backup_path).await {
                    tracing::warn!(
                        error = %e,
                        "[cursor] Failed to backup corrupted cursor file"
                    );
                }
            }
        }
        Ok(())
    }

    /// 保存游标状态
    ///
    /// 使用原子写入模式：先写入临时文件，然后重命名，避免崩溃导致数据丢失。
    ///
    /// ## 并发安全
    /// 使用包含进程 ID 的唯一临时文件名，避免多进程并发写入时临时文件冲突。
    pub async fn save(&self) -> std::io::Result<()> {
        let content = serde_json::to_string_pretty(&self)?;

        // 确保父目录存在
        if let Some(parent) = self.cursor_file_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // 创建唯一的临时文件路径（包含进程 ID 和时间戳）
        // 这避免了多进程并发写入时的临时文件冲突
        let pid = std::process::id();
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let temp_path = self
            .cursor_file_path
            .with_extension(format!("tmp.{}.{}", pid, timestamp));

        // 写入临时文件
        fs::write(&temp_path, &content).await?;

        // 原子重命名（在大多数平台上是原子的）
        // 即使多个进程同时保存，rename 也是原子的，最终只有一个会成功
        fs::rename(&temp_path, &self.cursor_file_path).await?;

        Ok(())
    }

    /// 获取特定记忆类型的游标
    pub fn get_cursor(&self, memory_type: MemoryType) -> ExtractionCursor {
        self.cursors
            .get(memory_type.name())
            .cloned()
            .unwrap_or_else(|| ExtractionCursor::new(memory_type))
    }

    /// 更新游标
    pub fn update_cursor(&mut self, cursor: ExtractionCursor) {
        self.cursors
            .insert(cursor.memory_type.name().to_string(), cursor);
    }

    /// 获取所有游标
    pub fn all_cursors(&self) -> Vec<ExtractionCursor> {
        MemoryType::all()
            .iter()
            .map(|mt| self.get_cursor(*mt))
            .collect()
    }

    /// 重置所有游标
    pub fn reset_all(&mut self) {
        self.cursors.clear();
        for mt in MemoryType::all() {
            self.cursors
                .insert(mt.name().to_string(), ExtractionCursor::new(mt));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extraction_cursor_new() {
        let cursor = ExtractionCursor::new(MemoryType::User);
        assert_eq!(cursor.memory_type, MemoryType::User);
        assert!(cursor.last_extracted_uuid.is_none());
        assert_eq!(cursor.last_message_count, 0);
    }

    #[test]
    fn test_extraction_cursor_should_extract() {
        let cursor = ExtractionCursor::new(MemoryType::User);

        // 初始状态，消息数不足
        assert!(!cursor.should_extract(3, 5));

        // 消息数足够
        assert!(cursor.should_extract(10, 5));
    }

    #[test]
    fn test_extraction_cursor_update() {
        let mut cursor = ExtractionCursor::new(MemoryType::User);
        let uuid = Uuid::new_v4();

        cursor.update(uuid, 15);

        assert_eq!(cursor.last_extracted_uuid, Some(uuid));
        assert_eq!(cursor.last_message_count, 15);
        assert!(cursor.last_extraction_time.is_some());
        assert_eq!(cursor.extraction_count, 1);
    }

    #[test]
    fn test_cursor_manager_new() {
        let manager = ExtractionCursorManager::new(Path::new("/config"));
        assert!(manager.cursors.is_empty());
    }

    #[test]
    fn test_cursor_manager_get_cursor() {
        let manager = ExtractionCursorManager::new(Path::new("/config"));

        // 未存储的游标会创建新的
        let cursor = manager.get_cursor(MemoryType::User);
        assert_eq!(cursor.memory_type, MemoryType::User);
    }

    #[test]
    fn test_cursor_manager_update_cursor() {
        let mut manager = ExtractionCursorManager::new(Path::new("/config"));

        let mut cursor = ExtractionCursor::new(MemoryType::User);
        cursor.update(Uuid::new_v4(), 10);

        manager.update_cursor(cursor.clone());

        let retrieved = manager.get_cursor(MemoryType::User);
        assert_eq!(retrieved.last_message_count, 10);
    }

    #[test]
    fn test_cursor_manager_all_cursors() {
        let manager = ExtractionCursorManager::new(Path::new("/config"));
        let cursors = manager.all_cursors();

        assert_eq!(cursors.len(), 4);
    }
}
