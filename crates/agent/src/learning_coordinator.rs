//! Learning Coordinator — unified decision point for all learning operations
//!
//! Replaces scattered `skill_nudge_engine` + `ghost_memory_lifecycle` calls
//! in runtime.rs with a single coordinated decision flow.
//!
//! Key guarantees:
//! - Same turn never triggers two independent reviews
//! - Throttle prevents review storms (max concurrent + cooldown)
//! - Dedup prevents duplicate learning within a time window
//! - Combined review when both memory and skill nudges fire

use std::sync::Mutex;

use crate::ghost_learning::{GhostLearningPolicy, LearningDecision};
use crate::learning_dedup::LearningDedup;
use crate::learning_throttle::LearningThrottle;
use crate::skill_nudge::{NudgeResult, SkillNudgeEngine};

/// What kind of learning review to trigger
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LearningAction {
    /// No learning needed this turn
    Skip,
    /// Review user memory (declarative knowledge)
    MemoryReview { trigger: MemoryTrigger },
    /// Review skill library (procedural knowledge)
    SkillReview { trigger: SkillTrigger },
    /// Review both memory and skills in a single pass
    CombinedReview {
        memory_trigger: MemoryTrigger,
        skill_trigger: SkillTrigger,
    },
}

/// Why a memory review was triggered
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryTrigger {
    /// Nudge threshold reached
    NudgeThreshold { count: u32 },
    /// Ghost learning boundary decision
    GhostBoundary,
    /// Pre-compress flush
    PreCompress,
    /// Session end
    SessionEnd,
    /// Session rotate
    SessionRotate,
    /// Delegation end
    DelegationEnd { success: bool },
}

/// Why a skill review was triggered
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillTrigger {
    /// Nudge threshold reached
    NudgeThreshold { count: u32 },
    /// Evolution trigger (skill error pattern)
    EvolutionTrigger {
        skill_name: String,
        error_count: u32,
    },
}

/// Unified learning coordinator
///
/// Wraps SkillNudgeEngine + GhostLearningPolicy + throttle + dedup
/// into a single decision point.
pub struct LearningCoordinator {
    nudge_engine: Mutex<SkillNudgeEngine>,
    ghost_policy: GhostLearningPolicy,
    throttle: LearningThrottle,
    dedup: LearningDedup,
    ghost_learning_enabled: bool,
    self_improve_review_enabled: bool,
}

impl std::fmt::Debug for LearningCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LearningCoordinator")
            .field("ghost_learning_enabled", &self.ghost_learning_enabled)
            .field(
                "self_improve_review_enabled",
                &self.self_improve_review_enabled,
            )
            .field("ghost_policy", &self.ghost_policy)
            .finish()
    }
}

fn recover_mutex<T>(e: std::sync::PoisonError<T>) -> T {
    tracing::warn!("LearningCoordinator Mutex poisoned, recovering");
    e.into_inner()
}

impl LearningCoordinator {
    pub fn new(
        nudge_engine: SkillNudgeEngine,
        ghost_policy: GhostLearningPolicy,
        throttle: LearningThrottle,
        dedup: LearningDedup,
        ghost_learning_enabled: bool,
        self_improve_review_enabled: bool,
    ) -> Self {
        Self {
            nudge_engine: Mutex::new(nudge_engine),
            ghost_policy,
            throttle,
            dedup,
            ghost_learning_enabled,
            self_improve_review_enabled,
        }
    }

    /// Called at the start of each user turn
    ///
    /// Replaces:
    /// - `ghost_memory_lifecycle.on_turn_start()`
    /// - `skill_nudge_engine.record_user_turn()`
    pub fn on_turn_start(&self, is_real_user: bool) {
        if is_real_user {
            self.nudge_engine
                .lock()
                .unwrap_or_else(recover_mutex)
                .record_user_turn();
        }
    }

    /// Record a tool iteration (each LLM call + tool execution)
    ///
    /// Replaces: `skill_nudge_engine.record_iteration()`
    pub fn record_iteration(&self) {
        self.nudge_engine
            .lock()
            .unwrap_or_else(recover_mutex)
            .record_iteration();
    }

    /// Evaluate what learning action to take at turn end
    ///
    /// Called before the LLM loop to check memory nudge,
    /// and after each iteration to check skill nudge.
    /// Returns the combined action.
    ///
    /// Replaces:
    /// - `skill_nudge_engine.check_memory_nudge()` + `check_skill_nudge()`
    /// - `deferred_review_mode` logic
    pub fn evaluate_nudge(
        &self,
        has_memory_store: bool,
        has_skill_tool: bool,
        existing_action: Option<&LearningAction>,
    ) -> LearningAction {
        if !self.self_improve_review_enabled {
            return LearningAction::Skip;
        }

        if !self.throttle.can_start_review() {
            return LearningAction::Skip;
        }

        // Check memory nudge (based on user turns)
        let mut engine = self.nudge_engine.lock().unwrap_or_else(recover_mutex);
        let memory_nudge = engine.check_memory_nudge();
        let memory_due = memory_nudge != NudgeResult::NoNudge && has_memory_store;

        // Check skill nudge (based on tool iterations)
        let skill_nudge = engine.check_skill_nudge();
        let skill_due = skill_nudge != NudgeResult::NoNudge && has_skill_tool;

        // Dedup check
        let dedup_key = if memory_due && skill_due {
            "combined_nudge".to_string()
        } else if memory_due {
            "memory_nudge".to_string()
        } else if skill_due {
            "skill_nudge".to_string()
        } else {
            return existing_action.cloned().unwrap_or(LearningAction::Skip);
        };

        if self.dedup.is_duplicate(&dedup_key) {
            return existing_action.cloned().unwrap_or(LearningAction::Skip);
        }

        // Determine action
        let new_action = if memory_due && skill_due {
            let memory_count = match memory_nudge {
                NudgeResult::SoftNudge { count } | NudgeResult::HardNudge { count } => count,
                _ => 0,
            };
            let skill_count = match skill_nudge {
                NudgeResult::SoftNudge { count } | NudgeResult::HardNudge { count } => count,
                _ => 0,
            };
            LearningAction::CombinedReview {
                memory_trigger: MemoryTrigger::NudgeThreshold {
                    count: memory_count,
                },
                skill_trigger: SkillTrigger::NudgeThreshold { count: skill_count },
            }
        } else if memory_due {
            let count = match memory_nudge {
                NudgeResult::SoftNudge { count } | NudgeResult::HardNudge { count } => count,
                _ => 0,
            };
            LearningAction::MemoryReview {
                trigger: MemoryTrigger::NudgeThreshold { count },
            }
        } else if skill_due {
            let count = match skill_nudge {
                NudgeResult::SoftNudge { count } | NudgeResult::HardNudge { count } => count,
                _ => 0,
            };
            LearningAction::SkillReview {
                trigger: SkillTrigger::NudgeThreshold { count },
            }
        } else {
            return existing_action.cloned().unwrap_or(LearningAction::Skip);
        };

        // Reset counters for triggered nudge types
        if memory_due {
            engine.reset_memory();
        }
        if skill_due {
            engine.reset_skill();
        }

        // If there's an existing action, merge/upgrade
        match (&new_action, existing_action) {
            (
                LearningAction::SkillReview { .. },
                Some(LearningAction::MemoryReview { trigger }),
            ) => LearningAction::CombinedReview {
                memory_trigger: trigger.clone(),
                skill_trigger: SkillTrigger::NudgeThreshold { count: 0 },
            },
            (
                LearningAction::MemoryReview { .. },
                Some(LearningAction::SkillReview { trigger }),
            ) => LearningAction::CombinedReview {
                memory_trigger: MemoryTrigger::NudgeThreshold { count: 0 },
                skill_trigger: trigger.clone(),
            },
            _ => new_action,
        }
    }

    /// Check memory nudge only (called before LLM loop)
    ///
    /// Returns the memory action if a memory nudge is due.
    /// This is separate from evaluate_nudge because memory nudge
    /// is checked once before the loop, while skill nudge is
    /// checked each iteration.
    pub fn check_memory_nudge(&self, has_memory_store: bool) -> Option<MemoryTrigger> {
        if !self.self_improve_review_enabled || !self.throttle.can_start_review() {
            return None;
        }

        let mut engine = self.nudge_engine.lock().unwrap_or_else(recover_mutex);
        let memory_nudge = engine.check_memory_nudge();
        let memory_due = memory_nudge != NudgeResult::NoNudge && has_memory_store;

        if !memory_due {
            return None;
        }

        if self.dedup.is_duplicate("memory_nudge") {
            return None;
        }

        let count = match memory_nudge {
            NudgeResult::SoftNudge { count } | NudgeResult::HardNudge { count } => count,
            _ => 0,
        };

        engine.reset_memory();
        Some(MemoryTrigger::NudgeThreshold { count })
    }

    /// Check skill nudge only (called each iteration)
    ///
    /// Returns the skill action if a skill nudge is due,
    /// potentially upgrading an existing memory action to combined.
    pub fn check_skill_nudge(
        &self,
        has_skill_tool: bool,
        existing_memory: bool,
    ) -> Option<SkillTrigger> {
        if !self.self_improve_review_enabled || !self.throttle.can_start_review() {
            return None;
        }

        let mut engine = self.nudge_engine.lock().unwrap_or_else(recover_mutex);
        let skill_nudge = engine.check_skill_nudge();
        let skill_due = skill_nudge != NudgeResult::NoNudge && has_skill_tool;

        if !skill_due {
            return None;
        }

        let dedup_key = if existing_memory {
            "combined_nudge"
        } else {
            "skill_nudge"
        };

        if self.dedup.is_duplicate(dedup_key) {
            return None;
        }

        let count = match skill_nudge {
            NudgeResult::SoftNudge { count } | NudgeResult::HardNudge { count } => count,
            _ => 0,
        };

        engine.reset_skill();
        Some(SkillTrigger::NudgeThreshold { count })
    }

    /// Reset skill counter after a skill write tool is used
    ///
    /// Replaces: `skill_nudge_engine.reset_skill()`
    pub fn reset_skill(&self) {
        self.nudge_engine
            .lock()
            .unwrap_or_else(recover_mutex)
            .reset_skill();
    }

    /// Reset memory counter after a memory write tool is used
    ///
    /// Replaces: `skill_nudge_engine.reset_memory()`
    pub fn reset_memory(&self) {
        self.nudge_engine
            .lock()
            .unwrap_or_else(recover_mutex)
            .reset_memory();
    }

    /// Record that a review has started (for throttle tracking)
    pub fn review_started(&self) {
        self.throttle.review_started();
    }

    /// Record that a review has completed (for throttle tracking)
    pub fn review_completed(&self) {
        self.throttle.review_completed();
    }

    /// Get the ghost learning policy decision for a boundary
    pub fn ghost_decide(
        &self,
        boundary: &crate::ghost_learning::GhostLearningBoundary,
    ) -> LearningDecision {
        if !self.ghost_learning_enabled {
            return LearningDecision::Ignore;
        }
        self.ghost_policy.decide(boundary)
    }

    /// Get the ghost learning policy decision with turn count
    pub fn ghost_decide_with_turn_count(
        &self,
        boundary: &crate::ghost_learning::GhostLearningBoundary,
        turn_count: Option<u32>,
    ) -> LearningDecision {
        if !self.ghost_learning_enabled {
            return LearningDecision::Ignore;
        }
        self.ghost_policy
            .decide_with_turn_count(boundary, turn_count)
    }

    /// Check if self-improve review is enabled
    pub fn is_self_improve_enabled(&self) -> bool {
        self.self_improve_review_enabled
    }

    /// Get debug status
    pub fn status(&self) -> String {
        format!(
            "LearningCoordinator: nudge={}, throttle_active={}, dedup_entries={}",
            self.nudge_engine
                .lock()
                .unwrap_or_else(recover_mutex)
                .status(),
            self.throttle.active_count(),
            self.dedup.len(),
        )
    }
}

/// Convert LearningAction to the ReviewMode used by spawn_review
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMode {
    Skill,
    Memory,
    Combined,
}

impl From<&LearningAction> for Option<ReviewMode> {
    fn from(action: &LearningAction) -> Self {
        match action {
            LearningAction::Skip => None,
            LearningAction::MemoryReview { .. } => Some(ReviewMode::Memory),
            LearningAction::SkillReview { .. } => Some(ReviewMode::Skill),
            LearningAction::CombinedReview { .. } => Some(ReviewMode::Combined),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_nudge::NudgeConfig;

    fn test_coordinator() -> LearningCoordinator {
        let nudge_engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });
        let ghost_policy = GhostLearningPolicy::default();
        let throttle = LearningThrottle::new(2, 0);
        let dedup = LearningDedup::new(0); // 0 window = no dedup for tests
        LearningCoordinator::new(nudge_engine, ghost_policy, throttle, dedup, true, true)
    }

    #[test]
    fn test_skip_when_no_nudge() {
        let coord = test_coordinator();
        let action = coord.evaluate_nudge(true, true, None);
        assert_eq!(action, LearningAction::Skip);
    }

    #[test]
    fn test_memory_nudge_after_turns() {
        let coord = test_coordinator();
        // 3 user turns = soft threshold for memory
        for _ in 0..3 {
            coord.on_turn_start(true);
        }
        let trigger = coord.check_memory_nudge(true);
        assert!(trigger.is_some());
    }

    #[test]
    fn test_skill_nudge_after_iterations() {
        let coord = test_coordinator();
        // 5 iterations = soft threshold for skill
        for _ in 0..5 {
            coord.record_iteration();
        }
        let trigger = coord.check_skill_nudge(true, false);
        assert!(trigger.is_some());
    }

    #[test]
    fn test_combined_when_both_due() {
        let coord = test_coordinator();
        // 3 user turns for memory
        for _ in 0..3 {
            coord.on_turn_start(true);
        }
        // 5 iterations for skill
        for _ in 0..5 {
            coord.record_iteration();
        }
        let action = coord.evaluate_nudge(true, true, None);
        assert!(matches!(action, LearningAction::CombinedReview { .. }));
    }

    #[test]
    fn test_skill_upgrades_memory_to_combined() {
        let coord = test_coordinator();
        // Memory nudge fires first
        for _ in 0..3 {
            coord.on_turn_start(true);
        }
        let memory_trigger = coord.check_memory_nudge(true);
        assert!(memory_trigger.is_some());

        // Then skill nudge fires
        for _ in 0..5 {
            coord.record_iteration();
        }
        let skill_trigger = coord.check_skill_nudge(true, true);
        assert!(skill_trigger.is_some());
    }

    #[test]
    fn test_reset_skill_after_write() {
        let coord = test_coordinator();
        for _ in 0..5 {
            coord.record_iteration();
        }
        coord.reset_skill();
        let trigger = coord.check_skill_nudge(true, false);
        assert!(trigger.is_none());
    }

    #[test]
    fn test_reset_memory_after_write() {
        let coord = test_coordinator();
        for _ in 0..3 {
            coord.on_turn_start(true);
        }
        coord.reset_memory();
        let trigger = coord.check_memory_nudge(true);
        assert!(trigger.is_none());
    }

    #[test]
    fn test_disabled_self_improve() {
        let nudge_engine = SkillNudgeEngine::new(NudgeConfig {
            min_nudge_interval_secs: 0,
            ..Default::default()
        });
        let ghost_policy = GhostLearningPolicy::default();
        let throttle = LearningThrottle::new(2, 0);
        let dedup = LearningDedup::new(0);
        let coord =
            LearningCoordinator::new(nudge_engine, ghost_policy, throttle, dedup, true, false);
        for _ in 0..3 {
            coord.on_turn_start(true);
        }
        let trigger = coord.check_memory_nudge(true);
        assert!(trigger.is_none());
    }

    #[test]
    fn test_review_mode_conversion() {
        assert_eq!(Option::<ReviewMode>::from(&LearningAction::Skip), None);
        assert_eq!(
            Option::<ReviewMode>::from(&LearningAction::MemoryReview {
                trigger: MemoryTrigger::NudgeThreshold { count: 3 }
            }),
            Some(ReviewMode::Memory)
        );
        assert_eq!(
            Option::<ReviewMode>::from(&LearningAction::SkillReview {
                trigger: SkillTrigger::NudgeThreshold { count: 5 }
            }),
            Some(ReviewMode::Skill)
        );
        assert_eq!(
            Option::<ReviewMode>::from(&LearningAction::CombinedReview {
                memory_trigger: MemoryTrigger::NudgeThreshold { count: 3 },
                skill_trigger: SkillTrigger::NudgeThreshold { count: 5 },
            }),
            Some(ReviewMode::Combined)
        );
    }
}
