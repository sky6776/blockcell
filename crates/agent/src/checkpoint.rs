//! # 任务断点恢复 (Checkpoint)
//!
//! 在任务执行的关键节点自动保存 checkpoint（对话历史 + 工具调用结果），
//! 中断后可从最近 checkpoint 恢复。

use blockcell_core::types::ChatMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tracing;

/// 任务断点信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    /// 任务 ID
    pub task_id: String,
    /// 对话历史（完整消息列表）
    pub messages: Vec<ChatMessage>,
    /// 当前轮次
    pub turn: u32,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// 是否已完成（完成后不再需要恢复）
    pub completed: bool,
}

/// Checkpoint 管理器
#[derive(Clone)]
pub struct CheckpointManager {
    /// checkpoint 存储目录
    checkpoint_dir: PathBuf,
}

impl CheckpointManager {
    /// 创建 CheckpointManager
    pub fn new(workspace_dir: &Path) -> Self {
        let checkpoint_dir = workspace_dir.join(".blockcell").join("checkpoints");
        Self { checkpoint_dir }
    }

    /// 验证 task_id 不包含路径遍历字符（防止 ../../etc/passwd 攻击）
    fn validate_task_id(task_id: &str) -> Result<(), String> {
        if task_id.is_empty() {
            return Err("task_id 不能为空".to_string());
        }
        if task_id.contains('/') || task_id.contains('\\') || task_id.contains("..") {
            return Err(format!(
                "task_id 包含非法字符: `{}` (不允许 / \\ ..)",
                task_id
            ));
        }
        Ok(())
    }

    /// 确保 checkpoint 目录存在
    fn ensure_dir(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.checkpoint_dir)
    }

    /// 保存 checkpoint（原子写入：先写 .tmp 再 rename，防止中断导致文件损坏）
    pub fn save(&self, checkpoint: &TaskCheckpoint) -> Result<(), String> {
        Self::validate_task_id(&checkpoint.task_id)?;

        self.ensure_dir()
            .map_err(|e| format!("创建 checkpoint 目录失败: {}", e))?;

        let file_path = self
            .checkpoint_dir
            .join(format!("{}.json", checkpoint.task_id));
        let tmp_path = self
            .checkpoint_dir
            .join(format!("{}.json.tmp", checkpoint.task_id));
        let json = serde_json::to_string_pretty(checkpoint)
            .map_err(|e| format!("序列化 checkpoint 失败: {}", e))?;

        // 原子写入：先写临时文件，再 rename
        fs::write(&tmp_path, &json).map_err(|e| format!("写入临时文件失败: {}", e))?;
        fs::rename(&tmp_path, &file_path).map_err(|e| format!("重命名临时文件失败: {}", e))?;

        tracing::debug!(
            task_id = %checkpoint.task_id,
            turn = checkpoint.turn,
            messages = checkpoint.messages.len(),
            "Checkpoint 已保存"
        );

        Ok(())
    }

    /// 加载指定任务的 checkpoint
    pub fn load(&self, task_id: &str) -> Result<Option<TaskCheckpoint>, String> {
        Self::validate_task_id(task_id)?;
        let file_path = self.checkpoint_dir.join(format!("{}.json", task_id));

        if !file_path.exists() {
            return Ok(None);
        }

        let json = fs::read_to_string(&file_path)
            .map_err(|e| format!("读取 checkpoint 文件失败: {}", e))?;

        let checkpoint: TaskCheckpoint =
            serde_json::from_str(&json).map_err(|e| format!("解析 checkpoint 文件失败: {}", e))?;

        Ok(Some(checkpoint))
    }

    /// 查找所有未完成的 checkpoint（可恢复的任务）
    /// 最多返回 MAX_FIND_UNFINISHED 条，防止磁盘上有大量文件时 OOM
    pub fn find_unfinished(&self) -> Vec<TaskCheckpoint> {
        const MAX_FIND_UNFINISHED: usize = 100;
        if !self.checkpoint_dir.exists() {
            return Vec::new();
        }

        let mut result = Vec::new();

        let entries = match fs::read_dir(&self.checkpoint_dir) {
            Ok(e) => e,
            Err(_) => return result,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            match fs::read_to_string(&path) {
                Ok(json) => match serde_json::from_str::<TaskCheckpoint>(&json) {
                    Ok(cp) if !cp.completed => {
                        result.push(cp);
                        if result.len() >= MAX_FIND_UNFINISHED {
                            break;
                        }
                    }
                    Ok(_) => {} // 已完成的 checkpoint，跳过
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "解析 checkpoint 文件失败，跳过"
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "读取 checkpoint 文件失败，跳过"
                    );
                }
            }
        }

        // 按创建时间降序排列（最新的在前）
        result.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        result
    }

    /// 标记 checkpoint 为已完成
    pub fn mark_completed(&self, task_id: &str) -> Result<(), String> {
        Self::validate_task_id(task_id)?;
        if let Some(mut checkpoint) = self.load(task_id)? {
            checkpoint.completed = true;
            self.save(&checkpoint)?;
        }
        Ok(())
    }

    /// 删除指定任务的 checkpoint
    pub fn remove(&self, task_id: &str) -> Result<(), String> {
        Self::validate_task_id(task_id)?;
        let file_path = self.checkpoint_dir.join(format!("{}.json", task_id));
        if file_path.exists() {
            fs::remove_file(&file_path).map_err(|e| format!("删除 checkpoint 文件失败: {}", e))?;
        }
        Ok(())
    }

    /// 清理所有已完成的 checkpoint
    pub fn cleanup_completed(&self) -> usize {
        if !self.checkpoint_dir.exists() {
            return 0;
        }

        let mut cleaned = 0;
        let entries = match fs::read_dir(&self.checkpoint_dir) {
            Ok(e) => e,
            Err(_) => return 0,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }

            if let Ok(json) = fs::read_to_string(&path) {
                if let Ok(cp) = serde_json::from_str::<TaskCheckpoint>(&json) {
                    if cp.completed && fs::remove_file(&path).is_ok() {
                        cleaned += 1;
                    }
                }
            }
        }

        if cleaned > 0 {
            tracing::info!(cleaned = cleaned, "已清理完成的 checkpoint 文件");
        }

        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            id: None,
            role: role.to_string(),
            content: serde_json::Value::String(content.to_string()),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    #[test]
    fn test_save_and_load_checkpoint() {
        let dir = TempDir::new().unwrap();
        let manager = CheckpointManager::new(dir.path());

        let checkpoint = TaskCheckpoint {
            task_id: "test-task-1".to_string(),
            messages: vec![
                make_message("user", "hello"),
                make_message("assistant", "hi there"),
            ],
            turn: 2,
            created_at: Utc::now(),
            completed: false,
        };

        manager.save(&checkpoint).unwrap();

        let loaded = manager.load("test-task-1").unwrap().unwrap();
        assert_eq!(loaded.task_id, "test-task-1");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.turn, 2);
        assert!(!loaded.completed);
    }

    #[test]
    fn test_find_unfinished_checkpoints() {
        let dir = TempDir::new().unwrap();
        let manager = CheckpointManager::new(dir.path());

        // 保存两个 checkpoint：一个未完成，一个已完成
        let cp1 = TaskCheckpoint {
            task_id: "task-1".to_string(),
            messages: vec![make_message("user", "hello")],
            turn: 1,
            created_at: Utc::now(),
            completed: false,
        };
        let cp2 = TaskCheckpoint {
            task_id: "task-2".to_string(),
            messages: vec![make_message("user", "world")],
            turn: 1,
            created_at: Utc::now(),
            completed: true,
        };

        manager.save(&cp1).unwrap();
        manager.save(&cp2).unwrap();

        let unfinished = manager.find_unfinished();
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].task_id, "task-1");
    }

    #[test]
    fn test_mark_completed() {
        let dir = TempDir::new().unwrap();
        let manager = CheckpointManager::new(dir.path());

        let checkpoint = TaskCheckpoint {
            task_id: "task-3".to_string(),
            messages: vec![],
            turn: 0,
            created_at: Utc::now(),
            completed: false,
        };

        manager.save(&checkpoint).unwrap();
        manager.mark_completed("task-3").unwrap();

        let loaded = manager.load("task-3").unwrap().unwrap();
        assert!(loaded.completed);

        // 未完成的列表不应包含它
        let unfinished = manager.find_unfinished();
        assert!(unfinished.is_empty());
    }

    #[test]
    fn test_cleanup_completed() {
        let dir = TempDir::new().unwrap();
        let manager = CheckpointManager::new(dir.path());

        let cp1 = TaskCheckpoint {
            task_id: "done-1".to_string(),
            messages: vec![],
            turn: 0,
            created_at: Utc::now(),
            completed: true,
        };
        let cp2 = TaskCheckpoint {
            task_id: "active-1".to_string(),
            messages: vec![],
            turn: 0,
            created_at: Utc::now(),
            completed: false,
        };

        manager.save(&cp1).unwrap();
        manager.save(&cp2).unwrap();

        let cleaned = manager.cleanup_completed();
        assert_eq!(cleaned, 1);

        // 已完成的文件应被删除
        assert!(manager.load("done-1").unwrap().is_none());
        // 未完成的应保留
        assert!(manager.load("active-1").unwrap().is_some());
    }

    #[test]
    fn test_path_traversal_rejected() {
        let dir = TempDir::new().unwrap();
        let manager = CheckpointManager::new(dir.path());

        // 包含路径遍历的 task_id 应被拒绝
        assert!(manager.load("../../etc/passwd").is_err());
        assert!(manager.load("foo/bar").is_err());
        assert!(manager.load("foo\\bar").is_err());
        assert!(manager.load("..secret").is_err());

        // 正常的 task_id 应成功
        assert!(manager.load("valid-task-id").is_ok());
    }

    #[test]
    fn test_atomic_write() {
        let dir = TempDir::new().unwrap();
        let manager = CheckpointManager::new(dir.path());

        let checkpoint = TaskCheckpoint {
            task_id: "atomic-test".to_string(),
            messages: vec![make_message("user", "hello")],
            turn: 1,
            created_at: Utc::now(),
            completed: false,
        };

        manager.save(&checkpoint).unwrap();

        // .tmp 文件不应存在（已 rename）
        let tmp_path = dir
            .path()
            .join(".blockcell/checkpoints/atomic-test.json.tmp");
        assert!(!tmp_path.exists());

        // 正式文件应存在
        let loaded = manager.load("atomic-test").unwrap().unwrap();
        assert_eq!(loaded.task_id, "atomic-test");
    }
}
