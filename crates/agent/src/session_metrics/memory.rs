//! Memory Metrics - Global metrics for the 7-layer memory system.
//!
//! Uses lock-free atomic counters for high-performance concurrent access.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Global memory system metrics instance.
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
}

impl Layer2Metrics {
    /// Record a trigger event.
    pub fn record_trigger(&self) {
        self.trigger_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record cleared items.
    pub fn record_cleared(&self, cleared: u64, kept: u64) {
        self.cleared_count.fetch_add(cleared, Ordering::Relaxed);
        self.kept_count.fetch_add(kept, Ordering::Relaxed);
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

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.trigger_count.store(0, Ordering::Relaxed);
        self.cleared_count.store(0, Ordering::Relaxed);
        self.kept_count.store(0, Ordering::Relaxed);
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
}

impl Layer3Metrics {
    /// Record an extraction event.
    pub fn record_extraction(&self, token_estimate: u64) {
        self.extraction_count.fetch_add(1, Ordering::Relaxed);
        self.total_token_estimate.fetch_add(token_estimate, Ordering::Relaxed);
    }

    /// Record a load event.
    pub fn record_load(&self, size: u64) {
        self.load_count.fetch_add(1, Ordering::Relaxed);
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

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.extraction_count.store(0, Ordering::Relaxed);
        self.load_count.store(0, Ordering::Relaxed);
        self.total_token_estimate.store(0, Ordering::Relaxed);
        self.current_size.store(0, Ordering::Relaxed);
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
    }

    /// Record a failed compact.
    pub fn record_compact_failure(&self) {
        self.compact_failed_count.fetch_add(1, Ordering::Relaxed);
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
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
}

impl Layer5Metrics {
    /// Record a memory written event.
    pub fn record_memory_written(&self, memory_type: &str, content_len: u64) {
        self.extraction_count.fetch_add(1, Ordering::Relaxed);
        self.total_bytes_written.fetch_add(content_len, Ordering::Relaxed);

        match memory_type {
            "user" => self.user_memories.fetch_add(1, Ordering::Relaxed),
            "project" => self.project_memories.fetch_add(1, Ordering::Relaxed),
            "feedback" => self.feedback_memories.fetch_add(1, Ordering::Relaxed),
            "reference" => self.reference_memories.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
    }

    /// Record a memory injection event.
    pub fn record_injection(&self, user: u64, project: u64, feedback: u64, reference: u64) {
        self.user_memories.fetch_add(user, Ordering::Relaxed);
        self.project_memories.fetch_add(project, Ordering::Relaxed);
        self.feedback_memories.fetch_add(feedback, Ordering::Relaxed);
        self.reference_memories.fetch_add(reference, Ordering::Relaxed);
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

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.extraction_count.store(0, Ordering::Relaxed);
        self.user_memories.store(0, Ordering::Relaxed);
        self.project_memories.store(0, Ordering::Relaxed);
        self.feedback_memories.store(0, Ordering::Relaxed);
        self.reference_memories.store(0, Ordering::Relaxed);
        self.total_bytes_written.store(0, Ordering::Relaxed);
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
}

impl Layer6Metrics {
    /// Record a dream started event.
    pub fn record_dream_started(&self) {
        self.dream_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a dream finished event.
    pub fn record_dream_finished(&self, created: u64, updated: u64, deleted: u64, pruned: u64) {
        self.memories_created.fetch_add(created, Ordering::Relaxed);
        self.memories_updated.fetch_add(updated, Ordering::Relaxed);
        self.memories_deleted.fetch_add(deleted, Ordering::Relaxed);
        self.sessions_pruned.fetch_add(pruned, Ordering::Relaxed);
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

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.dream_count.store(0, Ordering::Relaxed);
        self.memories_created.store(0, Ordering::Relaxed);
        self.memories_updated.store(0, Ordering::Relaxed);
        self.memories_deleted.store(0, Ordering::Relaxed);
        self.sessions_pruned.store(0, Ordering::Relaxed);
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
}

impl Layer7Metrics {
    /// Record an agent spawned event.
    pub fn record_spawned(&self) {
        self.spawned_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an agent completed event.
    pub fn record_completed(&self, turns: u64, tokens: u64) {
        self.completed_count.fetch_add(1, Ordering::Relaxed);
        self.total_turns_used.fetch_add(turns, Ordering::Relaxed);
        self.total_tokens_used.fetch_add(tokens, Ordering::Relaxed);
    }

    /// Record an agent failed event.
    pub fn record_failed(&self) {
        self.failed_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a tool denied event.
    pub fn record_tool_denied(&self) {
        self.tool_denied_count.fetch_add(1, Ordering::Relaxed);
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

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.spawned_count.store(0, Ordering::Relaxed);
        self.completed_count.store(0, Ordering::Relaxed);
        self.failed_count.store(0, Ordering::Relaxed);
        self.tool_denied_count.store(0, Ordering::Relaxed);
        self.total_tokens_used.store(0, Ordering::Relaxed);
        self.total_turns_used.store(0, Ordering::Relaxed);
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