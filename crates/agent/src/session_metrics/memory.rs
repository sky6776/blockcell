//! Memory Metrics - Global metrics for the 7-layer memory system.
//!
//! Uses lock-free atomic counters for high-performance concurrent access.
//!
//! ## Data Persistence
//!
//! **Important:** All metrics are stored in-memory only and reset on application restart.
//! This is by design - metrics are meant for real-time monitoring and debugging during
//! a session. For persistent metrics or historical analysis, consider:
//!
//! - Exporting metrics to external monitoring systems (Prometheus, Grafana, etc.)
//! - Using `/session_metrics --json` to capture snapshots for external storage
//! - Implementing a custom metrics exporter using the `MemoryMetrics::snapshot()` method

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Global memory system metrics instance.
///
/// Note: Metrics are in-memory only and reset on application restart.
/// For persistent metrics, consider exporting to external monitoring systems.
pub static MEMORY_METRICS: OnceLock<MemoryMetrics> = OnceLock::new();

/// Get the global memory metrics instance.
pub fn get_memory_metrics() -> &'static MemoryMetrics {
    MEMORY_METRICS.get_or_init(MemoryMetrics::default)
}

/// Global memory system metrics with lock-free counters.
#[derive(Debug, Default)]
pub struct MemoryMetrics {
    pub layer1: Layer1Metrics,
    pub layer2: Layer2Metrics,
    pub layer3: Layer3Metrics,
    pub layer4: Layer4Metrics,
    pub layer5: Layer5Metrics,
    pub layer6: Layer6Metrics,
    pub layer7: Layer7Metrics,
}

impl MemoryMetrics {
    /// Reset all layer metrics to zero.
    pub fn reset(&self) {
        self.layer1.reset();
        self.layer2.reset();
        self.layer3.reset();
        self.layer4.reset();
        self.layer5.reset();
        self.layer6.reset();
        self.layer7.reset();
    }
}

// ============================================================================
// Layer 1: Tool Result Storage
// ============================================================================

/// Layer 1 metrics - Tool result storage.
#[derive(Debug, Default)]
pub struct Layer1Metrics {
    /// Number of tool results persisted.
    persisted_count: AtomicU64,
    /// Total original size in bytes.
    total_original_size: AtomicU64,
    /// Total preview size in bytes.
    total_preview_size: AtomicU64,
    /// Number of budget exceeded events.
    budget_exceeded_count: AtomicU64,
    // --- 新增字段 ---
    /// Max tool results per message (配置值)
    max_tool_results: AtomicU64,
    /// Preview size limit in bytes (配置值)
    preview_size_limit: AtomicU64,
    /// Current stored results count (实时状态)
    current_stored_results: AtomicU64,
}

impl Layer1Metrics {
    /// Record a tool result persisted.
    pub fn record_persisted(&self, original_size: u64, preview_size: u64) {
        self.persisted_count.fetch_add(1, Ordering::Relaxed);
        self.total_original_size.fetch_add(original_size, Ordering::Relaxed);
        self.total_preview_size.fetch_add(preview_size, Ordering::Relaxed);
    }

    /// Record a budget exceeded event.
    pub fn record_budget_exceeded(&self) {
        self.budget_exceeded_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the number of persisted tool results.
    pub fn persisted_count(&self) -> u64 {
        self.persisted_count.load(Ordering::Relaxed)
    }

    /// Get the total original size.
    pub fn total_original_size(&self) -> u64 {
        self.total_original_size.load(Ordering::Relaxed)
    }

    /// Get the total preview size.
    pub fn total_preview_size(&self) -> u64 {
        self.total_preview_size.load(Ordering::Relaxed)
    }

    /// Get the budget exceeded count.
    pub fn budget_exceeded_count(&self) -> u64 {
        self.budget_exceeded_count.load(Ordering::Relaxed)
    }

    // --- 新增方法 ---
    /// Record config settings for Layer 1.
    pub fn record_config(&self, max_results: u64, preview_limit: u64) {
        self.max_tool_results.store(max_results, Ordering::Relaxed);
        self.preview_size_limit.store(preview_limit, Ordering::Relaxed);
    }

    /// Update current stored results count.
    pub fn update_stored_count(&self, count: u64) {
        self.current_stored_results.store(count, Ordering::Relaxed);
    }

    /// Increment stored results count by 1.
    pub fn increment_stored_count(&self) {
        self.current_stored_results.fetch_add(1, Ordering::Relaxed);
    }

    /// Get max tool results limit.
    pub fn max_tool_results(&self) -> u64 {
        self.max_tool_results.load(Ordering::Relaxed)
    }

    /// Get preview size limit.
    pub fn preview_size_limit(&self) -> u64 {
        self.preview_size_limit.load(Ordering::Relaxed)
    }

    /// Get current stored results count.
    pub fn current_stored_results(&self) -> u64 {
        self.current_stored_results.load(Ordering::Relaxed)
    }

    /// Calculate average compression ratio.
    pub fn average_compression(&self) -> f64 {
        let orig = self.total_original_size.load(Ordering::Relaxed);
        let prev = self.total_preview_size.load(Ordering::Relaxed);
        if orig > 0 {
            1.0 - (prev as f64 / orig as f64)
        } else {
            0.0
        }
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.persisted_count.store(0, Ordering::Relaxed);
        self.total_original_size.store(0, Ordering::Relaxed);
        self.total_preview_size.store(0, Ordering::Relaxed);
        self.budget_exceeded_count.store(0, Ordering::Relaxed);
        // 新增字段不重置（配置值保留）
        self.current_stored_results.store(0, Ordering::Relaxed);
    }
}

// ============================================================================
// Layer 2: Micro Compact
// ============================================================================

/// Layer 2 metrics - Micro compact (time-based cleanup).
#[derive(Debug, Default)]
pub struct Layer2Metrics {
    /// Number of time-based triggers.
    trigger_count: AtomicU64,
    /// Number of items cleared.
    cleared_count: AtomicU64,
    /// Number of items kept.
    kept_count: AtomicU64,
    // --- 新增字段 ---
    /// Gap threshold in minutes (配置值)
    gap_threshold_minutes: AtomicU64,
    /// Keep recent count (配置值)
    keep_recent: AtomicU64,
    /// Last trigger timestamp (Unix ms)
    last_trigger_timestamp: AtomicU64,
}

impl Layer2Metrics {
    /// Record a trigger event.
    pub fn record_trigger(&self) {
        self.trigger_count.fetch_add(1, Ordering::Relaxed);
        self.last_trigger_timestamp.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            Ordering::Relaxed
        );
    }

    /// Record cleared items.
    pub fn record_cleared(&self, cleared: u64, kept: u64) {
        self.cleared_count.fetch_add(cleared, Ordering::Relaxed);
        self.kept_count.fetch_add(kept, Ordering::Relaxed);
    }

    /// Record config settings for Layer 2.
    pub fn record_config(&self, gap_minutes: u64, keep_recent: u64) {
        self.gap_threshold_minutes.store(gap_minutes, Ordering::Relaxed);
        self.keep_recent.store(keep_recent, Ordering::Relaxed);
    }

    /// Get the trigger count.
    pub fn trigger_count(&self) -> u64 {
        self.trigger_count.load(Ordering::Relaxed)
    }

    /// Get the cleared count.
    pub fn cleared_count(&self) -> u64 {
        self.cleared_count.load(Ordering::Relaxed)
    }

    /// Get the kept count.
    pub fn kept_count(&self) -> u64 {
        self.kept_count.load(Ordering::Relaxed)
    }

    /// Get gap threshold in minutes.
    pub fn gap_threshold_minutes(&self) -> u64 {
        self.gap_threshold_minutes.load(Ordering::Relaxed)
    }

    /// Get keep recent count.
    pub fn keep_recent(&self) -> u64 {
        self.keep_recent.load(Ordering::Relaxed)
    }

    /// Get last trigger timestamp (Unix ms).
    pub fn last_trigger_timestamp(&self) -> u64 {
        self.last_trigger_timestamp.load(Ordering::Relaxed)
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.trigger_count.store(0, Ordering::Relaxed);
        self.cleared_count.store(0, Ordering::Relaxed);
        self.kept_count.store(0, Ordering::Relaxed);
        // 新增字段不重置（配置值和时间戳保留）
    }
}

// ============================================================================
// Layer 3: Session Memory
// ============================================================================

/// Layer 3 metrics - Session memory.
#[derive(Debug, Default)]
pub struct Layer3Metrics {
    /// Number of extractions.
    extraction_count: AtomicU64,
    /// Number of loads.
    load_count: AtomicU64,
    /// Total token estimate.
    total_token_estimate: AtomicU64,
    /// Current session memory size.
    current_size: AtomicU64,
    // --- 新增字段 ---
    /// Max total tokens limit (配置值, 默认 12000)
    max_total_tokens: AtomicU64,
    /// Max section length (配置值, 默认 2000)
    max_section_length: AtomicU64,
    /// Last extraction timestamp (Unix ms)
    last_extraction_timestamp: AtomicU64,
    /// Current section count
    section_count: AtomicU64,
}

impl Layer3Metrics {
    /// Record an extraction event.
    pub fn record_extraction(&self, token_estimate: u64) {
        self.extraction_count.fetch_add(1, Ordering::Relaxed);
        self.total_token_estimate.fetch_add(token_estimate, Ordering::Relaxed);
        self.last_extraction_timestamp.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            Ordering::Relaxed
        );
    }

    /// Record a load event.
    pub fn record_load(&self, size: u64) {
        self.load_count.fetch_add(1, Ordering::Relaxed);
        self.current_size.store(size, Ordering::Relaxed);
    }

    /// Record config settings for Layer 3.
    pub fn record_config(&self, max_total: u64, max_section: u64) {
        self.max_total_tokens.store(max_total, Ordering::Relaxed);
        self.max_section_length.store(max_section, Ordering::Relaxed);
    }

    /// Update section count.
    pub fn update_section_count(&self, count: u64) {
        self.section_count.store(count, Ordering::Relaxed);
    }

    /// Update current size (without incrementing load_count).
    /// Use this for resetting state values, not for recording load events.
    pub fn update_current_size(&self, size: u64) {
        self.current_size.store(size, Ordering::Relaxed);
    }

    /// Get the extraction count.
    pub fn extraction_count(&self) -> u64 {
        self.extraction_count.load(Ordering::Relaxed)
    }

    /// Get the load count.
    pub fn load_count(&self) -> u64 {
        self.load_count.load(Ordering::Relaxed)
    }

    /// Get the current size.
    pub fn current_size(&self) -> u64 {
        self.current_size.load(Ordering::Relaxed)
    }

    /// Get the total token estimate.
    pub fn total_token_estimate(&self) -> u64 {
        self.total_token_estimate.load(Ordering::Relaxed)
    }

    /// Get max total tokens limit.
    pub fn max_total_tokens(&self) -> u64 {
        self.max_total_tokens.load(Ordering::Relaxed)
    }

    /// Get max section length.
    pub fn max_section_length(&self) -> u64 {
        self.max_section_length.load(Ordering::Relaxed)
    }

    /// Get last extraction timestamp.
    pub fn last_extraction_timestamp(&self) -> u64 {
        self.last_extraction_timestamp.load(Ordering::Relaxed)
    }

    /// Get section count.
    pub fn section_count(&self) -> u64 {
        self.section_count.load(Ordering::Relaxed)
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.extraction_count.store(0, Ordering::Relaxed);
        self.load_count.store(0, Ordering::Relaxed);
        self.total_token_estimate.store(0, Ordering::Relaxed);
        self.current_size.store(0, Ordering::Relaxed);
        self.section_count.store(0, Ordering::Relaxed);
        // 配置值和时间戳保留
    }
}

// ============================================================================
// Layer 4: Full Compact
// ============================================================================

/// Layer 4 metrics - Full compact (LLM-based compression).
#[derive(Debug, Default)]
pub struct Layer4Metrics {
    /// Total compact count.
    compact_count: AtomicU64,
    /// Auto compact count.
    auto_compact_count: AtomicU64,
    /// Manual compact count.
    manual_compact_count: AtomicU64,
    /// Failed compact count.
    compact_failed_count: AtomicU64,
    /// Consecutive failures (for circuit breaker).
    consecutive_failures: AtomicU64,
    /// Total pre-compact tokens.
    total_pre_compact_tokens: AtomicU64,
    /// Total post-compact tokens.
    total_post_compact_tokens: AtomicU64,
    /// Total cache read tokens.
    total_cache_read_tokens: AtomicU64,
    /// Total cache creation tokens.
    total_cache_creation_tokens: AtomicU64,
    // --- 新增字段 ---
    /// Token budget (配置值, 默认 100000)
    token_budget: AtomicU64,
    /// Threshold ratio (存储为整数, 0.8 -> 800)
    threshold_ratio: AtomicU64,
    /// Current tokens in use (实时状态)
    current_tokens: AtomicU64,
    /// Last compact timestamp (Unix ms)
    last_compact_timestamp: AtomicU64,
    /// Total recovery budget (文件 50K + 技能 25K + Session 12K = 87K)
    total_recovery_budget: AtomicU64,
}

impl Layer4Metrics {
    /// Record a successful compact.
    pub fn record_compact_success(
        &self,
        pre_tokens: u64,
        post_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
    ) {
        self.record_compact_success_with_type(pre_tokens, post_tokens, cache_read, cache_creation, true);
    }

    /// Record a successful compact with type distinction.
    pub fn record_compact_success_with_type(
        &self,
        pre_tokens: u64,
        post_tokens: u64,
        cache_read: u64,
        cache_creation: u64,
        is_auto: bool,
    ) {
        self.compact_count.fetch_add(1, Ordering::Relaxed);
        if is_auto {
            self.auto_compact_count.fetch_add(1, Ordering::Relaxed);
        } else {
            self.manual_compact_count.fetch_add(1, Ordering::Relaxed);
        }
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.total_pre_compact_tokens.fetch_add(pre_tokens, Ordering::Relaxed);
        self.total_post_compact_tokens.fetch_add(post_tokens, Ordering::Relaxed);
        self.total_cache_read_tokens.fetch_add(cache_read, Ordering::Relaxed);
        self.total_cache_creation_tokens.fetch_add(cache_creation, Ordering::Relaxed);
        // 更新时间戳
        self.last_compact_timestamp.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            Ordering::Relaxed
        );
    }

    /// Record a failed compact.
    pub fn record_compact_failure(&self) {
        self.compact_failed_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Record config settings for Layer 4.
    pub fn record_config(&self, budget: u64, threshold: f64, recovery_budget: u64) {
        self.token_budget.store(budget, Ordering::Relaxed);
        self.threshold_ratio.store((threshold * 1000.0) as u64, Ordering::Relaxed);
        self.total_recovery_budget.store(recovery_budget, Ordering::Relaxed);
    }

    /// Update current token usage (实时状态).
    pub fn update_token_usage(&self, current: u64) {
        self.current_tokens.store(current, Ordering::Relaxed);
    }

    /// Get the compact count.
    pub fn compact_count(&self) -> u64 {
        self.compact_count.load(Ordering::Relaxed)
    }

    /// Get the failed count.
    pub fn compact_failed_count(&self) -> u64 {
        self.compact_failed_count.load(Ordering::Relaxed)
    }

    /// Get the consecutive failures.
    pub fn consecutive_failures(&self) -> u64 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Calculate average compression ratio.
    pub fn average_compression_ratio(&self) -> f64 {
        let pre = self.total_pre_compact_tokens.load(Ordering::Relaxed);
        let post = self.total_post_compact_tokens.load(Ordering::Relaxed);
        if pre > 0 {
            1.0 - (post as f64 / pre as f64)
        } else {
            0.0
        }
    }

    /// Calculate cache hit rate.
    pub fn cache_hit_rate(&self) -> f64 {
        let read = self.total_cache_read_tokens.load(Ordering::Relaxed);
        let creation = self.total_cache_creation_tokens.load(Ordering::Relaxed);
        let total = read + creation;
        if total > 0 {
            read as f64 / total as f64
        } else {
            0.0
        }
    }

    /// Get auto compact count.
    pub fn auto_compact_count(&self) -> u64 {
        self.auto_compact_count.load(Ordering::Relaxed)
    }

    /// Get manual compact count.
    pub fn manual_compact_count(&self) -> u64 {
        self.manual_compact_count.load(Ordering::Relaxed)
    }

    /// Get total pre-compact tokens.
    pub fn total_pre_compact_tokens(&self) -> u64 {
        self.total_pre_compact_tokens.load(Ordering::Relaxed)
    }

    /// Get total post-compact tokens.
    pub fn total_post_compact_tokens(&self) -> u64 {
        self.total_post_compact_tokens.load(Ordering::Relaxed)
    }

    /// Get token budget.
    pub fn token_budget(&self) -> u64 {
        self.token_budget.load(Ordering::Relaxed)
    }

    /// Get threshold ratio (as float, 800 -> 0.8).
    pub fn threshold_ratio(&self) -> f64 {
        self.threshold_ratio.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Get threshold tokens (budget * threshold_ratio).
    pub fn threshold_tokens(&self) -> u64 {
        let budget = self.token_budget.load(Ordering::Relaxed);
        let ratio = self.threshold_ratio.load(Ordering::Relaxed);
        (budget * ratio) / 1000
    }

    /// Get current tokens.
    pub fn current_tokens(&self) -> u64 {
        self.current_tokens.load(Ordering::Relaxed)
    }

    /// Get remaining tokens (budget - current).
    pub fn remaining_tokens(&self) -> u64 {
        let budget = self.token_budget.load(Ordering::Relaxed);
        let current = self.current_tokens.load(Ordering::Relaxed);
        budget.saturating_sub(current)
    }

    /// Get usage percentage.
    pub fn usage_percentage(&self) -> f64 {
        let budget = self.token_budget.load(Ordering::Relaxed);
        let current = self.current_tokens.load(Ordering::Relaxed);
        if budget > 0 {
            (current as f64 / budget as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Get last compact timestamp.
    pub fn last_compact_timestamp(&self) -> u64 {
        self.last_compact_timestamp.load(Ordering::Relaxed)
    }

    /// Get total recovery budget.
    pub fn total_recovery_budget(&self) -> u64 {
        self.total_recovery_budget.load(Ordering::Relaxed)
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.compact_count.store(0, Ordering::Relaxed);
        self.auto_compact_count.store(0, Ordering::Relaxed);
        self.manual_compact_count.store(0, Ordering::Relaxed);
        self.compact_failed_count.store(0, Ordering::Relaxed);
        self.consecutive_failures.store(0, Ordering::Relaxed);
        self.total_pre_compact_tokens.store(0, Ordering::Relaxed);
        self.total_post_compact_tokens.store(0, Ordering::Relaxed);
        self.total_cache_read_tokens.store(0, Ordering::Relaxed);
        self.total_cache_creation_tokens.store(0, Ordering::Relaxed);
        // 新增字段：配置值保留，状态值重置
        self.current_tokens.store(0, Ordering::Relaxed);
    }
}

// ============================================================================
// Layer 5: Memory Extraction
// ============================================================================

/// Layer 5 metrics - Memory extraction (auto-memory).
#[derive(Debug, Default)]
pub struct Layer5Metrics {
    /// Number of extractions.
    extraction_count: AtomicU64,
    /// User memories count.
    user_memories: AtomicU64,
    /// Project memories count.
    project_memories: AtomicU64,
    /// Feedback memories count.
    feedback_memories: AtomicU64,
    /// Reference memories count.
    reference_memories: AtomicU64,
    /// Total bytes written.
    total_bytes_written: AtomicU64,
    // --- 新增字段 ---
    /// Min messages for extraction (配置值, 默认 10)
    min_messages: AtomicU64,
    /// Cooldown messages (配置值, 默认 5)
    cooldown_messages: AtomicU64,
    /// Max file tokens (配置值, 默认 4000)
    max_file_tokens: AtomicU64,
    /// Last extraction timestamp (Unix ms)
    last_extraction_timestamp: AtomicU64,
    /// User memory bytes
    user_bytes: AtomicU64,
    /// Project memory bytes
    project_bytes: AtomicU64,
    /// Feedback memory bytes
    feedback_bytes: AtomicU64,
    /// Reference memory bytes
    reference_bytes: AtomicU64,
}

impl Layer5Metrics {
    /// Record a memory written event.
    pub fn record_memory_written(&self, memory_type: &str, content_len: u64) {
        self.extraction_count.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_written.fetch_add(content_len, Ordering::Relaxed);
        self.last_extraction_timestamp.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            Ordering::Relaxed
        );

        match memory_type {
            "user" => {
                self.user_memories.fetch_add(1, Ordering::Relaxed);
                self.user_bytes.fetch_add(content_len, Ordering::Relaxed);
            }
            "project" => {
                self.project_memories.fetch_add(1, Ordering::Relaxed);
                self.project_bytes.fetch_add(content_len, Ordering::Relaxed);
            }
            "feedback" => {
                self.feedback_memories.fetch_add(1, Ordering::Relaxed);
                self.feedback_bytes.fetch_add(content_len, Ordering::Relaxed);
            }
            "reference" => {
                self.reference_memories.fetch_add(1, Ordering::Relaxed);
                self.reference_bytes.fetch_add(content_len, Ordering::Relaxed);
            }
            _ => {}
        };
    }

    /// Record a memory injection event.
    pub fn record_injection(&self, user: u64, project: u64, feedback: u64, reference: u64) {
        self.user_memories.fetch_add(user, Ordering::Relaxed);
        self.project_memories.fetch_add(project, Ordering::Relaxed);
        self.feedback_memories.fetch_add(feedback, Ordering::Relaxed);
        self.reference_memories.fetch_add(reference, Ordering::Relaxed);
    }

    /// Record config settings for Layer 5.
    pub fn record_config(&self, min_msg: u64, cooldown: u64, max_file: u64) {
        self.min_messages.store(min_msg, Ordering::Relaxed);
        self.cooldown_messages.store(cooldown, Ordering::Relaxed);
        self.max_file_tokens.store(max_file, Ordering::Relaxed);
    }

    /// Get the extraction count.
    pub fn extraction_count(&self) -> u64 {
        self.extraction_count.load(Ordering::Relaxed)
    }

    /// Get the total bytes written.
    pub fn total_bytes_written(&self) -> u64 {
        self.total_bytes_written.load(Ordering::Relaxed)
    }

    /// Get user memories count.
    pub fn user_memories(&self) -> u64 {
        self.user_memories.load(Ordering::Relaxed)
    }

    /// Get project memories count.
    pub fn project_memories(&self) -> u64 {
        self.project_memories.load(Ordering::Relaxed)
    }

    /// Get feedback memories count.
    pub fn feedback_memories(&self) -> u64 {
        self.feedback_memories.load(Ordering::Relaxed)
    }

    /// Get reference memories count.
    pub fn reference_memories(&self) -> u64 {
        self.reference_memories.load(Ordering::Relaxed)
    }

    /// Get min messages for extraction.
    pub fn min_messages(&self) -> u64 {
        self.min_messages.load(Ordering::Relaxed)
    }

    /// Get cooldown messages.
    pub fn cooldown_messages(&self) -> u64 {
        self.cooldown_messages.load(Ordering::Relaxed)
    }

    /// Get max file tokens.
    pub fn max_file_tokens(&self) -> u64 {
        self.max_file_tokens.load(Ordering::Relaxed)
    }

    /// Get last extraction timestamp.
    pub fn last_extraction_timestamp(&self) -> u64 {
        self.last_extraction_timestamp.load(Ordering::Relaxed)
    }

    /// Get user memory bytes.
    pub fn user_bytes(&self) -> u64 {
        self.user_bytes.load(Ordering::Relaxed)
    }

    /// Get project memory bytes.
    pub fn project_bytes(&self) -> u64 {
        self.project_bytes.load(Ordering::Relaxed)
    }

    /// Get feedback memory bytes.
    pub fn feedback_bytes(&self) -> u64 {
        self.feedback_bytes.load(Ordering::Relaxed)
    }

    /// Get reference memory bytes.
    pub fn reference_bytes(&self) -> u64 {
        self.reference_bytes.load(Ordering::Relaxed)
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.extraction_count.store(0, Ordering::Relaxed);
        self.user_memories.store(0, Ordering::Relaxed);
        self.project_memories.store(0, Ordering::Relaxed);
        self.feedback_memories.store(0, Ordering::Relaxed);
        self.reference_memories.store(0, Ordering::Relaxed);
        self.total_bytes_written.store(0, Ordering::Relaxed);
        // 新增字段
        self.user_bytes.store(0, Ordering::Relaxed);
        self.project_bytes.store(0, Ordering::Relaxed);
        self.feedback_bytes.store(0, Ordering::Relaxed);
        self.reference_bytes.store(0, Ordering::Relaxed);
        // 配置值保留
    }
}

// ============================================================================
// Layer 6: Auto Dream
// ============================================================================

/// Layer 6 metrics - Auto dream (consolidation).
#[derive(Debug, Default)]
pub struct Layer6Metrics {
    /// Number of dream runs.
    dream_count: AtomicU64,
    /// Memories created.
    memories_created: AtomicU64,
    /// Memories updated.
    memories_updated: AtomicU64,
    /// Memories deleted.
    memories_deleted: AtomicU64,
    /// Sessions pruned.
    sessions_pruned: AtomicU64,
    // --- 新增字段 ---
    /// Dream interval hours (配置值)
    dream_interval_hours: AtomicU64,
    /// Last dream timestamp (Unix ms)
    last_dream_timestamp: AtomicU64,
    /// Sessions processed
    sessions_processed: AtomicU64,
}

impl Layer6Metrics {
    /// Record a dream started event.
    pub fn record_dream_started(&self) {
        self.dream_count.fetch_add(1, Ordering::Relaxed);
        self.last_dream_timestamp.store(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            Ordering::Relaxed
        );
    }

    /// Record a dream finished event.
    pub fn record_dream_finished(&self, created: u64, updated: u64, deleted: u64, pruned: u64, sessions: u64) {
        self.memories_created.fetch_add(created, Ordering::Relaxed);
        self.memories_updated.fetch_add(updated, Ordering::Relaxed);
        self.memories_deleted.fetch_add(deleted, Ordering::Relaxed);
        self.sessions_pruned.fetch_add(pruned, Ordering::Relaxed);
        self.sessions_processed.fetch_add(sessions, Ordering::Relaxed);
    }

    /// Record config settings for Layer 6.
    pub fn record_config(&self, interval_hours: u64) {
        self.dream_interval_hours.store(interval_hours, Ordering::Relaxed);
    }

    /// Get the dream count.
    pub fn dream_count(&self) -> u64 {
        self.dream_count.load(Ordering::Relaxed)
    }

    /// Get memories created count.
    pub fn memories_created(&self) -> u64 {
        self.memories_created.load(Ordering::Relaxed)
    }

    /// Get memories updated count.
    pub fn memories_updated(&self) -> u64 {
        self.memories_updated.load(Ordering::Relaxed)
    }

    /// Get memories deleted count.
    pub fn memories_deleted(&self) -> u64 {
        self.memories_deleted.load(Ordering::Relaxed)
    }

    /// Get sessions pruned count.
    pub fn sessions_pruned(&self) -> u64 {
        self.sessions_pruned.load(Ordering::Relaxed)
    }

    /// Get dream interval hours.
    pub fn dream_interval_hours(&self) -> u64 {
        self.dream_interval_hours.load(Ordering::Relaxed)
    }

    /// Get last dream timestamp.
    pub fn last_dream_timestamp(&self) -> u64 {
        self.last_dream_timestamp.load(Ordering::Relaxed)
    }

    /// Get sessions processed.
    pub fn sessions_processed(&self) -> u64 {
        self.sessions_processed.load(Ordering::Relaxed)
    }

    /// Calculate consolidation rate.
    pub fn consolidation_rate(&self) -> f64 {
        let processed = self.sessions_processed.load(Ordering::Relaxed);
        let created = self.memories_created.load(Ordering::Relaxed);
        let updated = self.memories_updated.load(Ordering::Relaxed);
        if processed > 0 {
            (created + updated) as f64 / processed as f64
        } else {
            0.0
        }
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.dream_count.store(0, Ordering::Relaxed);
        self.memories_created.store(0, Ordering::Relaxed);
        self.memories_updated.store(0, Ordering::Relaxed);
        self.memories_deleted.store(0, Ordering::Relaxed);
        self.sessions_pruned.store(0, Ordering::Relaxed);
        self.sessions_processed.store(0, Ordering::Relaxed);
        // 配置值和时间戳保留
    }
}

// ============================================================================
// Layer 7: Forked Agent
// ============================================================================

/// Layer 7 metrics - Forked agent.
#[derive(Debug, Default)]
pub struct Layer7Metrics {
    /// Number of agents spawned.
    spawned_count: AtomicU64,
    /// Number of agents completed.
    completed_count: AtomicU64,
    /// Number of agents failed.
    failed_count: AtomicU64,
    /// Number of tool denied events.
    tool_denied_count: AtomicU64,
    /// Total tokens used.
    total_tokens_used: AtomicU64,
    /// Total turns used.
    total_turns_used: AtomicU64,
    // --- 新增字段 ---
    /// Max turns per agent (配置值)
    max_turns: AtomicU64,
    /// Current active agents
    active_count: AtomicU64,
    /// Total completion time in ms
    total_completion_time_ms: AtomicU64,
}

impl Layer7Metrics {
    /// Record an agent spawned event.
    pub fn record_spawned(&self) {
        self.spawned_count.fetch_add(1, Ordering::Relaxed);
        self.active_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an agent completed event.
    pub fn record_completed(&self, turns: u64, tokens: u64) {
        self.completed_count.fetch_add(1, Ordering::Relaxed);
        self.total_turns_used.fetch_add(turns, Ordering::Relaxed);
        self.total_tokens_used.fetch_add(tokens, Ordering::Relaxed);
        self.active_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record an agent completed event with duration.
    pub fn record_completed_with_duration(&self, turns: u64, tokens: u64, duration_ms: u64) {
        self.completed_count.fetch_add(1, Ordering::Relaxed);
        self.total_turns_used.fetch_add(turns, Ordering::Relaxed);
        self.total_tokens_used.fetch_add(tokens, Ordering::Relaxed);
        self.total_completion_time_ms.fetch_add(duration_ms, Ordering::Relaxed);
        self.active_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record an agent failed event.
    pub fn record_failed(&self) {
        self.failed_count.fetch_add(1, Ordering::Relaxed);
        self.active_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record a tool denied event.
    pub fn record_tool_denied(&self) {
        self.tool_denied_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record config settings for Layer 7.
    pub fn record_config(&self, max_turns: u64) {
        self.max_turns.store(max_turns, Ordering::Relaxed);
    }

    /// Get the spawned count.
    pub fn spawned_count(&self) -> u64 {
        self.spawned_count.load(Ordering::Relaxed)
    }

    /// Get the completed count.
    pub fn completed_count(&self) -> u64 {
        self.completed_count.load(Ordering::Relaxed)
    }

    /// Get the failed count.
    pub fn failed_count(&self) -> u64 {
        self.failed_count.load(Ordering::Relaxed)
    }

    /// Get the tool denied count.
    pub fn tool_denied_count(&self) -> u64 {
        self.tool_denied_count.load(Ordering::Relaxed)
    }

    /// Get total tokens used.
    pub fn total_tokens_used(&self) -> u64 {
        self.total_tokens_used.load(Ordering::Relaxed)
    }

    /// Get total turns used.
    pub fn total_turns_used(&self) -> u64 {
        self.total_turns_used.load(Ordering::Relaxed)
    }

    /// Get max turns per agent.
    pub fn max_turns(&self) -> u64 {
        self.max_turns.load(Ordering::Relaxed)
    }

    /// Get current active count.
    pub fn active_count(&self) -> u64 {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Get total completion time in ms.
    pub fn total_completion_time_ms(&self) -> u64 {
        self.total_completion_time_ms.load(Ordering::Relaxed)
    }

    /// Calculate average completion time in ms.
    pub fn avg_completion_time_ms(&self) -> f64 {
        let completed = self.completed_count.load(Ordering::Relaxed);
        let total_time = self.total_completion_time_ms.load(Ordering::Relaxed);
        if completed > 0 {
            total_time as f64 / completed as f64
        } else {
            0.0
        }
    }

    /// Calculate average tokens per agent.
    pub fn avg_tokens_per_agent(&self) -> f64 {
        let completed = self.completed_count.load(Ordering::Relaxed);
        let total_tokens = self.total_tokens_used.load(Ordering::Relaxed);
        if completed > 0 {
            total_tokens as f64 / completed as f64
        } else {
            0.0
        }
    }

    /// Calculate average turns per agent.
    pub fn avg_turns(&self) -> f64 {
        let completed = self.completed_count.load(Ordering::Relaxed);
        let total_turns = self.total_turns_used.load(Ordering::Relaxed);
        if completed > 0 {
            total_turns as f64 / completed as f64
        } else {
            0.0
        }
    }

    /// Calculate success rate.
    pub fn success_rate(&self) -> f64 {
        let completed = self.completed_count.load(Ordering::Relaxed);
        let failed = self.failed_count.load(Ordering::Relaxed);
        let total = completed + failed;
        if total > 0 {
            completed as f64 / total as f64
        } else {
            1.0 // No agents run means 100% success
        }
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.spawned_count.store(0, Ordering::Relaxed);
        self.completed_count.store(0, Ordering::Relaxed);
        self.failed_count.store(0, Ordering::Relaxed);
        self.tool_denied_count.store(0, Ordering::Relaxed);
        self.total_tokens_used.store(0, Ordering::Relaxed);
        self.total_turns_used.store(0, Ordering::Relaxed);
        self.active_count.store(0, Ordering::Relaxed);
        self.total_completion_time_ms.store(0, Ordering::Relaxed);
        // 配置值保留
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_layer1_metrics() {
        let metrics = Layer1Metrics::default();
        metrics.record_persisted(1000, 100);
        metrics.record_persisted(2000, 200);
        metrics.record_budget_exceeded();

        assert_eq!(metrics.persisted_count(), 2);
        assert_eq!(metrics.total_original_size(), 3000);
        assert_eq!(metrics.total_preview_size(), 300);
        assert_eq!(metrics.budget_exceeded_count(), 1);
        assert!((metrics.average_compression() - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_layer1_config() {
        let metrics = Layer1Metrics::default();
        metrics.record_config(50, 500);

        assert_eq!(metrics.max_tool_results(), 50);
        assert_eq!(metrics.preview_size_limit(), 500);

        // 测试 update_stored_count 和 increment_stored_count
        metrics.update_stored_count(10);
        assert_eq!(metrics.current_stored_results(), 10);

        metrics.increment_stored_count();
        assert_eq!(metrics.current_stored_results(), 11);
    }

    #[test]
    fn test_layer4_metrics() {
        let metrics = Layer4Metrics::default();
        metrics.record_compact_success(10000, 3000, 8000, 2000);
        metrics.record_compact_success(20000, 5000, 15000, 5000);
        metrics.record_compact_failure();

        assert_eq!(metrics.compact_count(), 2);
        assert_eq!(metrics.compact_failed_count(), 1);
        assert_eq!(metrics.consecutive_failures(), 1);

        // Compression ratio: 1 - (3000 + 5000) / (10000 + 20000) = 1 - 0.267 = 0.733
        assert!((metrics.average_compression_ratio() - 0.733).abs() < 0.01);

        // Cache hit rate: (8000 + 15000) / (8000 + 2000 + 15000 + 5000) = 23000 / 30000 = 0.767
        assert!((metrics.cache_hit_rate() - 0.767).abs() < 0.01);
    }

    #[test]
    fn test_layer4_config() {
        let metrics = Layer4Metrics::default();
        metrics.record_config(100000, 0.8, 50000);

        assert_eq!(metrics.token_budget(), 100000);
        assert!((metrics.threshold_ratio() - 0.8).abs() < 0.001);
        assert_eq!(metrics.threshold_tokens(), 80000); // 100000 * 0.8

        // 测试 update_token_usage
        metrics.update_token_usage(60000);
        assert_eq!(metrics.current_tokens(), 60000);
        assert_eq!(metrics.remaining_tokens(), 40000); // 100000 - 60000
        assert!((metrics.usage_percentage() - 60.0).abs() < 0.01);
    }

    #[test]
    fn test_layer2_config() {
        let metrics = Layer2Metrics::default();
        metrics.record_config(30, 10);

        assert_eq!(metrics.gap_threshold_minutes(), 30);
        assert_eq!(metrics.keep_recent(), 10);
    }

    #[test]
    fn test_layer3_config() {
        let metrics = Layer3Metrics::default();
        metrics.record_config(50000, 2000);

        assert_eq!(metrics.max_total_tokens(), 50000);
        assert_eq!(metrics.max_section_length(), 2000);

        // 测试 update_section_count
        metrics.update_section_count(5);
        assert_eq!(metrics.section_count(), 5);
    }

    #[test]
    fn test_layer5_config() {
        let metrics = Layer5Metrics::default();
        metrics.record_config(50, 10, 100000);

        assert_eq!(metrics.min_messages(), 50);
        assert_eq!(metrics.cooldown_messages(), 10);
        assert_eq!(metrics.max_file_tokens(), 100000);
    }

    #[test]
    fn test_layer6_config() {
        let metrics = Layer6Metrics::default();
        metrics.record_config(4);

        assert_eq!(metrics.dream_interval_hours(), 4);
    }

    #[test]
    fn test_layer7_config() {
        let metrics = Layer7Metrics::default();
        metrics.record_config(50);

        assert_eq!(metrics.max_turns(), 50);
    }

    #[test]
    fn test_layer7_statistics() {
        let metrics = Layer7Metrics::default();

        // 测试无 Agent 时的默认值
        assert!((metrics.success_rate() - 1.0).abs() < 0.001);
        assert!((metrics.avg_completion_time_ms() - 0.0).abs() < 0.001);
        assert!((metrics.avg_tokens_per_agent() - 0.0).abs() < 0.001);
        assert!((metrics.avg_turns() - 0.0).abs() < 0.001);

        // 添加一些数据
        // record_completed_with_duration 参数顺序: (turns, tokens, duration_ms)
        metrics.record_spawned();
        metrics.record_completed_with_duration(10, 1000, 500); // 10 turns, 1000 tokens, 500ms
        metrics.record_spawned();
        metrics.record_completed_with_duration(15, 2000, 800); // 15 turns, 2000 tokens, 800ms
        metrics.record_spawned();
        metrics.record_failed();

        // 计算统计数据
        assert_eq!(metrics.spawned_count(), 3);
        assert_eq!(metrics.completed_count(), 2);
        assert_eq!(metrics.failed_count(), 1);

        // success_rate: 2 / (2 + 1) = 0.667
        assert!((metrics.success_rate() - 0.667).abs() < 0.01);

        // avg_completion_time_ms: (500 + 800) / 2 = 650
        assert!((metrics.avg_completion_time_ms() - 650.0).abs() < 0.01);

        // avg_tokens_per_agent: (1000 + 2000) / 2 = 1500
        assert!((metrics.avg_tokens_per_agent() - 1500.0).abs() < 0.01);

        // avg_turns: (10 + 15) / 2 = 12.5
        assert!((metrics.avg_turns() - 12.5).abs() < 0.01);
    }

    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let metrics = Arc::new(MemoryMetrics::default());

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let m = Arc::clone(&metrics);
                thread::spawn(move || {
                    m.layer1.record_persisted(i, i / 10);
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let total: u64 = (0..10).sum();
        assert_eq!(metrics.layer1.persisted_count(), 10);
        assert_eq!(metrics.layer1.total_original_size(), total);
    }
}