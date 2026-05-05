//! Skill Nudge Engine — 提醒 Agent 在复杂任务后保存 Skill / Memory
//!
//! 两个独立计数器 (语义不同):
//! - `iterations_since_skill`: 基于 **工具迭代次数** (每次 LLM 调用+工具执行递增)
//! - `turns_since_memory`: 基于 **用户轮次** (每次收到用户消息递增)
//!
//! 参考 Hermes `agent_core/nudge_engine.py`:
//! - Hermes `_iters_since_skill` → BlockCell `iterations_since_skill`
//! - Hermes `_turns_since_memory` → BlockCell `turns_since_memory`

use serde::{Deserialize, Serialize};
use std::time::Instant;

use blockcell_core::config::SelfImproveNudgeConfig;

/// Skill Nudge 配置 — Skill 和 Memory 使用独立阈值
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NudgeConfig {
    /// Skill nudge 软阈值 (默认: 5 次工具迭代)
    #[serde(default = "default_skill_soft")]
    pub skill_soft_threshold: u32,
    /// Skill nudge 硬阈值 (默认: 10 次工具迭代)
    #[serde(default = "default_skill_hard")]
    pub skill_hard_threshold: u32,
    /// Memory nudge 软阈值 (默认: 3 次用户轮次)
    #[serde(default = "default_memory_soft")]
    pub memory_soft_threshold: u32,
    /// Memory nudge 硬阈值 (默认: 6 次用户轮次)
    #[serde(default = "default_memory_hard")]
    pub memory_hard_threshold: u32,
    /// 是否启用 nudge (默认: true)
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 最小 nudge 间隔秒数 (默认: 300)
    #[serde(default = "default_min_nudge_interval")]
    pub min_nudge_interval_secs: u64,
}

fn default_skill_soft() -> u32 {
    5
}
fn default_skill_hard() -> u32 {
    10
}
fn default_memory_soft() -> u32 {
    3
}
fn default_memory_hard() -> u32 {
    6
}
fn default_true() -> bool {
    true
}

fn default_min_nudge_interval() -> u64 {
    300
}

impl Default for NudgeConfig {
    fn default() -> Self {
        Self {
            skill_soft_threshold: 5,
            skill_hard_threshold: 10,
            memory_soft_threshold: 3,
            memory_hard_threshold: 6,
            enabled: true,
            min_nudge_interval_secs: 300,
        }
    }
}

impl NudgeConfig {
    /// 从 core config 的 SelfImproveNudgeConfig 构建
    /// 验证阈值不变量: soft < hard, 且都大于 0
    pub fn from_config(cfg: &SelfImproveNudgeConfig) -> Self {
        // 验证阈值不变量: soft < hard, 且都大于 0
        // 注意: skill_hard 的验证必须使用已修正的 skill_soft, 而非原始 cfg 值
        let skill_soft = if cfg.skill_soft_threshold == 0
            || cfg.skill_soft_threshold >= cfg.skill_hard_threshold
        {
            tracing::warn!(
                "Invalid skill nudge thresholds: soft={}, hard={}. Using defaults: soft=5, hard=10",
                cfg.skill_soft_threshold,
                cfg.skill_hard_threshold
            );
            5
        } else {
            cfg.skill_soft_threshold
        };
        let skill_hard = if cfg.skill_hard_threshold == 0 || cfg.skill_hard_threshold <= skill_soft
        {
            // 使用已修正的 skill_soft 而非 cfg.skill_soft_threshold
            skill_soft + 5
        } else {
            cfg.skill_hard_threshold
        };
        let memory_soft = if cfg.memory_soft_threshold == 0
            || cfg.memory_soft_threshold >= cfg.memory_hard_threshold
        {
            tracing::warn!(
                "Invalid memory nudge thresholds: soft={}, hard={}. Using defaults: soft=3, hard=6",
                cfg.memory_soft_threshold,
                cfg.memory_hard_threshold
            );
            3
        } else {
            cfg.memory_soft_threshold
        };
        let memory_hard =
            if cfg.memory_hard_threshold == 0 || cfg.memory_hard_threshold <= memory_soft {
                // 使用已修正的 memory_soft 而非 cfg.memory_soft_threshold
                memory_soft + 3
            } else {
                cfg.memory_hard_threshold
            };
        let min_interval = if cfg.min_nudge_interval_secs == 0 {
            tracing::warn!("Invalid min_nudge_interval_secs: 0. Using default: 300");
            300
        } else {
            cfg.min_nudge_interval_secs
        };

        Self {
            skill_soft_threshold: skill_soft,
            skill_hard_threshold: skill_hard,
            memory_soft_threshold: memory_soft,
            memory_hard_threshold: memory_hard,
            enabled: cfg.enabled,
            min_nudge_interval_secs: min_interval,
        }
    }
}

/// Skill Nudge 引擎 — 两个独立计数器 (Skill + Memory)
///
/// - Skill 计数器基于工具迭代次数
/// - Memory 计数器基于用户轮次
#[derive(Debug)]
pub struct SkillNudgeEngine {
    config: NudgeConfig,
    /// 自上次 Skill 相关操作以来的工具迭代次数
    iterations_since_skill: u32,
    /// 自上次 Memory 相关操作以来的用户轮次 (不是工具迭代)
    turns_since_memory: u32,
    /// 总迭代次数
    total_iterations: u32,
    /// 上次 Skill nudge 时间 (避免频繁提醒)
    last_skill_nudge_time: Option<Instant>,
    /// 上次 Memory nudge 时间 (避免频繁提醒)
    last_memory_nudge_time: Option<Instant>,
    /// 最小 nudge 间隔 (秒)
    min_nudge_interval_secs: u64,
}

/// Nudge 结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NudgeResult {
    /// 不需要提醒
    NoNudge,
    /// 软提醒: 建议保存 Skill / Memory
    /// `count` 含义取决于 nudge 类型:
    /// - Skill nudge: 工具迭代次数
    /// - Memory nudge: 用户轮次数
    SoftNudge { count: u32 },
    /// 硬提醒: 强烈建议保存 Skill / Memory
    HardNudge { count: u32 },
}

impl SkillNudgeEngine {
    pub fn new(config: NudgeConfig) -> Self {
        let min_nudge_interval_secs = config.min_nudge_interval_secs;
        Self {
            config,
            iterations_since_skill: 0,
            turns_since_memory: 0,
            total_iterations: 0,
            last_skill_nudge_time: None,
            last_memory_nudge_time: None,
            min_nudge_interval_secs,
        }
    }

    /// 记录一次工具迭代 (每次 LLM 调用 + 工具执行时调用)
    /// 只递增 Skill 计数器，不递增 Memory 计数器
    /// 使用 saturating_add 防止长时间运行后溢出
    pub fn record_iteration(&mut self) {
        self.total_iterations = self.total_iterations.saturating_add(1);
        self.iterations_since_skill = self.iterations_since_skill.saturating_add(1);
    }

    /// 记录一次用户轮次 (仅在收到用户消息时调用)
    /// 只递增 Memory 计数器，不递增 Skill 计数器
    /// 使用 saturating_add 防止长时间运行后溢出
    pub fn record_user_turn(&mut self) {
        self.turns_since_memory = self.turns_since_memory.saturating_add(1);
    }

    /// 重置 Skill 计数器 (在 Skill 相关工具使用后调用)
    pub fn reset_skill(&mut self) {
        self.iterations_since_skill = 0;
    }

    /// 重置 Memory 计数器 (在 Memory 相关工具使用后调用)
    pub fn reset_memory(&mut self) {
        self.turns_since_memory = 0;
    }

    /// 检查是否需要 Skill nudge
    pub fn check_skill_nudge(&mut self) -> NudgeResult {
        if !self.config.enabled {
            return NudgeResult::NoNudge;
        }

        // 检查冷却时间
        if let Some(last) = self.last_skill_nudge_time {
            if last.elapsed().as_secs() < self.min_nudge_interval_secs {
                return NudgeResult::NoNudge;
            }
        }

        let iterations = self.iterations_since_skill;

        if iterations >= self.config.skill_hard_threshold {
            self.last_skill_nudge_time = Some(Instant::now());
            self.iterations_since_skill = 0; // Reset counter on nudge (matches Hermes behavior)
            NudgeResult::HardNudge { count: iterations }
        } else if iterations >= self.config.skill_soft_threshold {
            self.last_skill_nudge_time = Some(Instant::now());
            self.iterations_since_skill = 0; // Reset counter on nudge (matches Hermes behavior)
            NudgeResult::SoftNudge { count: iterations }
        } else {
            NudgeResult::NoNudge
        }
    }

    /// 检查是否需要 Memory nudge
    pub fn check_memory_nudge(&mut self) -> NudgeResult {
        if !self.config.enabled {
            return NudgeResult::NoNudge;
        }

        // 检查冷却时间
        if let Some(last) = self.last_memory_nudge_time {
            if last.elapsed().as_secs() < self.min_nudge_interval_secs {
                return NudgeResult::NoNudge;
            }
        }

        let turns = self.turns_since_memory;

        if turns >= self.config.memory_hard_threshold {
            self.last_memory_nudge_time = Some(Instant::now());
            self.turns_since_memory = 0; // Reset counter on nudge (matches Hermes behavior)
            NudgeResult::HardNudge { count: turns }
        } else if turns >= self.config.memory_soft_threshold {
            self.last_memory_nudge_time = Some(Instant::now());
            self.turns_since_memory = 0; // Reset counter on nudge (matches Hermes behavior)
            NudgeResult::SoftNudge { count: turns }
        } else {
            NudgeResult::NoNudge
        }
    }

    /// 获取当前 iterations_since_skill 计数 (用于 coordinator 在 check 前捕获值)
    pub fn iterations_since_skill(&self) -> u32 {
        self.iterations_since_skill
    }

    /// 获取当前 turns_since_memory 计数 (用于 coordinator 在 check 前捕获值)
    pub fn turns_since_memory(&self) -> u32 {
        self.turns_since_memory
    }

    /// Read-only check: would a memory nudge trigger WITHOUT resetting counters?
    /// Used by LearningCoordinator to check dedup before committing to a nudge.
    pub fn would_memory_nudge(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        if let Some(last) = self.last_memory_nudge_time {
            if last.elapsed().as_secs() < self.min_nudge_interval_secs {
                return false;
            }
        }
        self.turns_since_memory >= self.config.memory_soft_threshold
    }

    /// Read-only check: would a skill nudge trigger WITHOUT resetting counters?
    /// Used by LearningCoordinator to check dedup before committing to a nudge.
    pub fn would_skill_nudge(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        if let Some(last) = self.last_skill_nudge_time {
            if last.elapsed().as_secs() < self.min_nudge_interval_secs {
                return false;
            }
        }
        self.iterations_since_skill >= self.config.skill_soft_threshold
    }

    /// 重置所有计数器
    pub fn reset(&mut self) {
        self.iterations_since_skill = 0;
        self.turns_since_memory = 0;
    }

    /// 获取当前状态 (用于调试)
    pub fn status(&self) -> String {
        format!(
            "SkillNudge: iterations_since_skill={}, turns_since_memory={}, total_iterations={}",
            self.iterations_since_skill, self.turns_since_memory, self.total_iterations
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_nudge_below_threshold() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig::default());
        for _ in 0..4 {
            engine.record_iteration();
        }
        assert_eq!(engine.check_skill_nudge(), NudgeResult::NoNudge);
        // Memory nudge uses user turns, not iterations
        assert_eq!(engine.check_memory_nudge(), NudgeResult::NoNudge);
    }

    #[test]
    fn test_soft_nudge_at_threshold() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });
        for _ in 0..5 {
            engine.record_iteration();
        }
        assert_eq!(
            engine.check_skill_nudge(),
            NudgeResult::SoftNudge { count: 5 }
        );
    }

    #[test]
    fn test_hard_nudge_at_threshold() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });
        for _ in 0..10 {
            engine.record_iteration();
        }
        assert_eq!(
            engine.check_skill_nudge(),
            NudgeResult::HardNudge { count: 10 }
        );
    }

    #[test]
    fn test_skill_reset() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });
        for _ in 0..4 {
            engine.record_iteration();
        }
        engine.reset_skill(); // 重置 Skill 计数器
        engine.record_iteration();
        assert_eq!(engine.check_skill_nudge(), NudgeResult::NoNudge);
    }

    #[test]
    fn test_memory_uses_user_turns() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });

        // 一次用户轮次 + 5 次工具迭代
        engine.record_user_turn();
        for _ in 0..5 {
            engine.record_iteration();
        }

        // Memory nudge 基于 turns_since_memory = 1, 未达阈值 3
        assert_eq!(engine.check_memory_nudge(), NudgeResult::NoNudge);

        // 再来 2 次用户轮次
        engine.record_user_turn();
        engine.record_user_turn();

        // Memory nudge 基于 turns_since_memory = 3, 达到软阈值
        assert_eq!(
            engine.check_memory_nudge(),
            NudgeResult::SoftNudge { count: 3 }
        );
    }

    #[test]
    fn test_memory_independent_counter() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });
        // 6 次工具迭代 + 1 次用户轮次
        engine.record_user_turn();
        for _ in 0..6 {
            engine.record_iteration();
        }
        // 重置 Skill 计数器 (模拟 skill_manage 使用)
        engine.reset_skill();
        // Memory 计数器不受影响 (基于用户轮次 = 1, 未达阈值 3)
        assert_eq!(engine.check_memory_nudge(), NudgeResult::NoNudge);
        // Skill 计数器已重置
        assert_eq!(engine.check_skill_nudge(), NudgeResult::NoNudge);
    }

    #[test]
    fn test_disabled_no_nudge() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            enabled: false,
            ..Default::default()
        });
        for _ in 0..20 {
            engine.record_iteration();
        }
        assert_eq!(engine.check_skill_nudge(), NudgeResult::NoNudge);
        assert_eq!(engine.check_memory_nudge(), NudgeResult::NoNudge);
    }

    #[test]
    fn test_reset() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig::default());
        for _ in 0..8 {
            engine.record_iteration();
        }
        engine.record_user_turn();
        engine.reset();
        assert_eq!(engine.iterations_since_skill, 0);
        assert_eq!(engine.turns_since_memory, 0);
    }

    #[test]
    fn test_status() {
        let engine = SkillNudgeEngine::new(NudgeConfig::default());
        let status = engine.status();
        assert!(status.contains("iterations_since_skill=0"));
        assert!(status.contains("turns_since_memory=0"));
    }

    #[test]
    fn test_separate_thresholds() {
        let mut engine = SkillNudgeEngine::new(NudgeConfig {
            skill_soft_threshold: 3,
            skill_hard_threshold: 6,
            memory_soft_threshold: 2,
            memory_hard_threshold: 4,
            min_nudge_interval_secs: 0,
            ..Default::default()
        });

        // 3 tool iterations → skill soft threshold
        for _ in 0..3 {
            engine.record_iteration();
        }
        assert_eq!(
            engine.check_skill_nudge(),
            NudgeResult::SoftNudge { count: 3 }
        );

        // 2 user turns → memory soft threshold
        engine.record_user_turn();
        engine.record_user_turn();
        assert_eq!(
            engine.check_memory_nudge(),
            NudgeResult::SoftNudge { count: 2 }
        );
    }
}
