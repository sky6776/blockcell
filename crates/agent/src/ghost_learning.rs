use blockcell_core::config::GhostLearningConfig;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GhostLearningBoundaryKind {
    TurnEnd,
    PreCompress,
    SessionRotate,
    SessionEnd,
    DelegationEnd,
    EvolutionSuccess,
}

impl GhostLearningBoundaryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            GhostLearningBoundaryKind::TurnEnd => "turn_end",
            GhostLearningBoundaryKind::PreCompress => "pre_compress",
            GhostLearningBoundaryKind::SessionRotate => "session_rotate",
            GhostLearningBoundaryKind::SessionEnd => "session_end",
            GhostLearningBoundaryKind::DelegationEnd => "delegation_end",
            GhostLearningBoundaryKind::EvolutionSuccess => "evolution_success",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LearningDecision {
    Ignore,
    ReviewAfterResponse,
    ForceBoundaryReview,
}

impl LearningDecision {
    pub fn episode_status(&self) -> Option<&'static str> {
        match self {
            LearningDecision::Ignore => None,
            LearningDecision::ReviewAfterResponse => Some("pending_review"),
            LearningDecision::ForceBoundaryReview => Some("pending_review"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GhostLearningBoundary {
    pub kind: GhostLearningBoundaryKind,
    pub session_key: Option<String>,
    pub subject_key: Option<String>,
    pub user_intent_summary: String,
    pub assistant_outcome_summary: String,
    pub tool_call_count: u32,
    pub memory_write_count: u32,
    pub correction_count: u32,
    pub preference_correction_count: u32,
    pub success: bool,
    pub complexity_score: u32,
    pub reusable_lesson: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GhostEpisodeSnapshot {
    pub boundary_kind: GhostLearningBoundaryKind,
    pub session_key: Option<String>,
    pub subject_key: Option<String>,
    pub user_intent_summary: String,
    pub assistant_outcome_summary: String,
    pub tool_call_count: u32,
    pub memory_write_count: u32,
    pub correction_count: u32,
    pub preference_correction_count: u32,
    pub complexity_score: u32,
    pub reusable_lesson: Option<String>,
    pub decision: LearningDecision,
}

impl GhostEpisodeSnapshot {
    pub fn summary(&self) -> String {
        if self.assistant_outcome_summary.trim().is_empty() {
            self.user_intent_summary.clone()
        } else {
            format!(
                "{} => {}",
                self.user_intent_summary, self.assistant_outcome_summary
            )
        }
    }
}

impl From<(GhostLearningBoundary, LearningDecision)> for GhostEpisodeSnapshot {
    fn from((boundary, decision): (GhostLearningBoundary, LearningDecision)) -> Self {
        Self {
            boundary_kind: boundary.kind,
            session_key: boundary.session_key,
            subject_key: boundary.subject_key,
            user_intent_summary: boundary.user_intent_summary,
            assistant_outcome_summary: boundary.assistant_outcome_summary,
            tool_call_count: boundary.tool_call_count,
            memory_write_count: boundary.memory_write_count,
            correction_count: boundary.correction_count,
            preference_correction_count: boundary.preference_correction_count,
            complexity_score: boundary.complexity_score,
            reusable_lesson: boundary.reusable_lesson,
            decision,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GhostLearningPolicy {
    method_tool_threshold: u32,
    turn_review_interval: u32,
}

impl GhostLearningPolicy {
    pub fn from_config(config: &GhostLearningConfig) -> Self {
        Self {
            method_tool_threshold: config.method_tool_threshold.max(1),
            turn_review_interval: config.turn_review_interval,
        }
    }

    pub fn decide(&self, boundary: &GhostLearningBoundary) -> LearningDecision {
        self.decide_with_turn_count(boundary, None)
    }

    pub fn decide_with_turn_count(
        &self,
        boundary: &GhostLearningBoundary,
        turn_count: Option<u32>,
    ) -> LearningDecision {
        match boundary.kind {
            GhostLearningBoundaryKind::PreCompress
            | GhostLearningBoundaryKind::SessionRotate
            | GhostLearningBoundaryKind::SessionEnd => {
                return LearningDecision::ForceBoundaryReview;
            }
            GhostLearningBoundaryKind::DelegationEnd
            | GhostLearningBoundaryKind::EvolutionSuccess => {
                if boundary.success {
                    return LearningDecision::ReviewAfterResponse;
                }
            }
            GhostLearningBoundaryKind::TurnEnd => {}
        }

        if !boundary.success {
            return LearningDecision::Ignore;
        }

        if boundary.preference_correction_count > 0 || boundary.correction_count > 0 {
            return LearningDecision::ReviewAfterResponse;
        }

        if boundary.tool_call_count >= self.method_tool_threshold {
            return LearningDecision::ReviewAfterResponse;
        }

        if boundary.complexity_score >= 5 {
            return LearningDecision::ReviewAfterResponse;
        }

        if boundary.memory_write_count > 0 && boundary.reusable_lesson.is_some() {
            return LearningDecision::ReviewAfterResponse;
        }

        if boundary.kind == GhostLearningBoundaryKind::TurnEnd
            && self.turn_review_interval > 0
            && turn_count
                .filter(|count| *count > 0 && count % self.turn_review_interval == 0)
                .is_some()
        {
            return LearningDecision::ReviewAfterResponse;
        }

        LearningDecision::Ignore
    }
}

impl Default for GhostLearningPolicy {
    fn default() -> Self {
        Self {
            method_tool_threshold: 3,
            turn_review_interval: 0,
        }
    }
}

pub fn estimate_turn_complexity_score(user_text: &str) -> u32 {
    let trimmed = user_text.trim();
    if trimmed.is_empty() {
        return 0;
    }

    let lower = trimmed.to_lowercase();
    let mut score = 0;

    let token_count = lower.split_whitespace().count();
    if token_count >= 5 {
        score += 2;
    }

    let cues = [
        "figure out",
        "correct",
        "sequence",
        "analyze",
        "investigate",
        "deploy",
        "rollback",
        "why",
        "how",
        "compare",
        "正确",
        "顺序",
        "分析",
        "排查",
        "回滚",
        "部署",
    ];
    if cues.iter().any(|cue| lower.contains(cue)) {
        score += 4;
    }

    score
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trivial_success_turn() -> GhostLearningBoundary {
        GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::TurnEnd,
            session_key: Some("cli:chat-1".to_string()),
            subject_key: Some("user:test".to_string()),
            user_intent_summary: "say hello".to_string(),
            assistant_outcome_summary: "said hello".to_string(),
            tool_call_count: 0,
            memory_write_count: 0,
            correction_count: 0,
            preference_correction_count: 0,
            success: true,
            complexity_score: 0,
            reusable_lesson: None,
        }
    }

    fn sample_preference_correction_boundary() -> GhostLearningBoundary {
        GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::TurnEnd,
            session_key: Some("cli:chat-1".to_string()),
            subject_key: Some("user:test".to_string()),
            user_intent_summary: "user corrected deploy preference".to_string(),
            assistant_outcome_summary: "captured preferred canary deploy sequence".to_string(),
            tool_call_count: 1,
            memory_write_count: 0,
            correction_count: 1,
            preference_correction_count: 1,
            success: true,
            complexity_score: 4,
            reusable_lesson: Some("Prefer canary-first deploys for this user".to_string()),
        }
    }

    fn sample_high_complexity_tool_boundary() -> GhostLearningBoundary {
        GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::TurnEnd,
            session_key: Some("cli:chat-1".to_string()),
            subject_key: Some("user:test".to_string()),
            user_intent_summary: "analyze the deploy failure and correct sequence".to_string(),
            assistant_outcome_summary: "used tools to determine the correct deploy order"
                .to_string(),
            tool_call_count: 4,
            memory_write_count: 0,
            correction_count: 0,
            preference_correction_count: 0,
            success: true,
            complexity_score: 6,
            reusable_lesson: Some("Validate rollout ordering before deploy".to_string()),
        }
    }

    fn sample_pre_compress_boundary() -> GhostLearningBoundary {
        GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::PreCompress,
            session_key: Some("cli:chat-1".to_string()),
            subject_key: Some("user:test".to_string()),
            user_intent_summary: "conversation budget boundary reached".to_string(),
            assistant_outcome_summary: "about to compact conversation".to_string(),
            tool_call_count: 0,
            memory_write_count: 0,
            correction_count: 0,
            preference_correction_count: 0,
            success: true,
            complexity_score: 0,
            reusable_lesson: None,
        }
    }

    #[test]
    fn ghost_learning_policy_ignores_trivial_success_turn() {
        let policy = GhostLearningPolicy::default();
        let decision = policy.decide(&sample_trivial_success_turn());
        assert_eq!(decision, LearningDecision::Ignore);
    }

    #[test]
    fn ghost_learning_policy_reviews_on_configured_turn_interval() {
        let mut config = GhostLearningConfig::default();
        config.turn_review_interval = 2;
        let policy = GhostLearningPolicy::from_config(&config);

        let boundary = sample_trivial_success_turn();
        assert_eq!(
            policy.decide_with_turn_count(&boundary, Some(1)),
            LearningDecision::Ignore
        );
        assert_eq!(
            policy.decide_with_turn_count(&boundary, Some(2)),
            LearningDecision::ReviewAfterResponse
        );
    }

    #[test]
    fn ghost_learning_policy_preference_correction_requests_review() {
        let policy = GhostLearningPolicy::default();
        let decision = policy.decide(&sample_preference_correction_boundary());
        assert_eq!(decision, LearningDecision::ReviewAfterResponse);
        assert_eq!(decision.episode_status(), Some("pending_review"));
    }

    #[test]
    fn ghost_learning_policy_high_complexity_tool_turn_requests_review() {
        let policy = GhostLearningPolicy::default();
        let decision = policy.decide(&sample_high_complexity_tool_boundary());
        assert_eq!(decision, LearningDecision::ReviewAfterResponse);
        assert_eq!(decision.episode_status(), Some("pending_review"));
    }

    #[test]
    fn ghost_learning_policy_pre_compress_forces_boundary_review() {
        let policy = GhostLearningPolicy::default();
        let decision = policy.decide(&sample_pre_compress_boundary());
        assert_eq!(decision, LearningDecision::ForceBoundaryReview);
        assert_eq!(decision.episode_status(), Some("pending_review"));
    }
}
