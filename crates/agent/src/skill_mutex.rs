//! Per-skill concurrency guard
//!
//! **Deprecated**: Use [`crate::write_guard::WriteGuard`] instead.
//! This module will be removed in a future version.

use async_trait::async_trait;
use blockcell_tools::{SkillMutexGuard, SkillMutexOps};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::RwLock;

/// Skill 操作互斥锁
#[derive(Debug, Clone)]
#[deprecated(
    since = "0.2.0",
    note = "Use crate::write_guard::WriteGuard instead. This will be removed in a future version."
)]
pub struct SkillMutex {
    /// 正在执行中的 Skill 名称集合
    /// 使用 std::sync::RwLock 而非 tokio::sync::RwLock,
    /// 确保 SkillGuard::Drop 可以同步释放, 无需 tokio runtime
    active_skills: Arc<RwLock<HashSet<String>>>,
}

/// 获取 Skill 守卫失败
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Skill '{skill_name}' is already active")]
pub struct AcquireError {
    pub skill_name: String,
}

#[allow(deprecated)]
impl SkillMutex {
    pub fn new() -> Self {
        Self {
            active_skills: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// 获取 Skill 执行守卫
    /// 如果该 Skill 已在执行中, 返回 Err(AcquireError)
    pub fn acquire(&self, skill_name: &str) -> Result<SkillGuard, AcquireError> {
        {
            let mut active = self.active_skills.write().unwrap_or_else(|e| {
                tracing::warn!(
                    "SkillMutex RwLock poisoned, recovering for skill: {}",
                    skill_name
                );
                e.into_inner()
            });
            if active.contains(skill_name) {
                return Err(AcquireError {
                    skill_name: skill_name.to_string(),
                });
            }
            active.insert(skill_name.to_string());
        }
        Ok(SkillGuard {
            skill_name: skill_name.to_string(),
            active_skills: self.active_skills.clone(),
        })
    }

    /// 检查 Skill 是否正在执行
    pub fn is_active(&self, skill_name: &str) -> bool {
        let active = self.active_skills.read().unwrap_or_else(|e| {
            tracing::warn!("SkillMutex RwLock poisoned during is_active check");
            e.into_inner()
        });
        active.contains(skill_name)
    }

    /// 检查是否可以修改 Skill (不在执行中)
    pub fn can_modify(&self, skill_name: &str) -> bool {
        !self.is_active(skill_name)
    }

    /// 获取所有正在执行的 Skill
    pub fn active_skills(&self) -> Vec<String> {
        let active = self.active_skills.read().unwrap_or_else(|e| {
            tracing::warn!("SkillMutex RwLock poisoned during active_skills check");
            e.into_inner()
        });
        active.iter().cloned().collect()
    }
}

#[allow(deprecated)]
impl Default for SkillMutex {
    fn default() -> Self {
        Self::new()
    }
}

/// 为 SkillMutex 实现 tools crate 的 SkillMutexOps trait
/// 这样 tools crate 可以通过 opaque handle 调用 can_modify / try_acquire
#[async_trait]
#[allow(deprecated)]
impl SkillMutexOps for SkillMutex {
    async fn can_modify(&self, skill_name: &str) -> bool {
        SkillMutex::can_modify(self, skill_name)
    }

    fn try_acquire(&self, skill_name: &str) -> Option<SkillMutexGuard> {
        match self.acquire(skill_name) {
            Ok(guard) => {
                // Wrap the SkillGuard in an Arc<dyn Send+Sync> that releases on drop.
                // SkillGuard's Drop impl removes the skill from the active set.
                Some(Arc::new(guard))
            }
            Err(_) => None,
        }
    }
}

/// Skill 执行守卫 (RAII, Drop 时自动释放)
#[derive(Debug)]
pub struct SkillGuard {
    skill_name: String,
    active_skills: Arc<RwLock<HashSet<String>>>,
}

impl Drop for SkillGuard {
    fn drop(&mut self) {
        // std::sync::RwLock 的 write 是同步操作, Drop 中安全调用
        let mut active = self.active_skills.write().unwrap_or_else(|e| {
            tracing::warn!(
                "SkillMutex RwLock poisoned during SkillGuard drop for '{}'",
                self.skill_name
            );
            e.into_inner()
        });
        active.remove(&self.skill_name);
    }
}

#[cfg(test)]
mod tests {
    #![allow(deprecated)]

    use super::*;

    #[test]
    fn test_acquire_and_release() {
        let mutex = SkillMutex::new();

        {
            let _guard = mutex.acquire("test-skill").unwrap();
            assert!(mutex.is_active("test-skill"));
            assert!(!mutex.can_modify("test-skill"));
        }

        // Guard drop 后立即释放 (同步, 无需等待)
        assert!(!mutex.is_active("test-skill"));
        assert!(mutex.can_modify("test-skill"));
    }

    #[test]
    fn test_acquire_duplicate_error() {
        let mutex = SkillMutex::new();

        let _guard = mutex.acquire("test-skill").unwrap();
        // 再次 acquire 同一 skill 应返回错误
        let err = mutex.acquire("test-skill").unwrap_err();
        assert_eq!(err.skill_name, "test-skill");
    }

    #[test]
    fn test_multiple_skills() {
        let mutex = SkillMutex::new();

        let guard1 = mutex.acquire("skill-1").unwrap();
        let guard2 = mutex.acquire("skill-2").unwrap();

        assert!(mutex.is_active("skill-1"));
        assert!(mutex.is_active("skill-2"));

        let active = mutex.active_skills();
        assert_eq!(active.len(), 2);

        drop(guard1);
        drop(guard2);
    }

    #[test]
    fn test_can_modify() {
        let mutex = SkillMutex::new();

        assert!(mutex.can_modify("test-skill"));

        let _guard = mutex.acquire("test-skill").unwrap();
        assert!(!mutex.can_modify("test-skill"));
    }

    #[test]
    fn test_different_skills_independent() {
        let mutex = SkillMutex::new();

        let _guard = mutex.acquire("skill-1").unwrap();
        assert!(mutex.can_modify("skill-2"));
        assert!(!mutex.can_modify("skill-1"));
    }
}
