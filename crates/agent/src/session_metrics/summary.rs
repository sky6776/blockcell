//! Metrics Summary - Snapshot and formatting for CLI output.

use super::memory::get_memory_metrics;
use super::circuit_breaker::{get_compact_circuit_breaker, CircuitState};
use serde::Serialize;

/// Snapshot of all memory metrics.
#[derive(Debug, Serialize)]
pub struct MetricsSummary {
    pub layer1: Layer1Summary,
    pub layer2: Layer2Summary,
    pub layer3: Layer3Summary,
    pub layer4: Layer4Summary,
    pub layer5: Layer5Summary,
    pub layer6: Layer6Summary,
    pub layer7: Layer7Summary,
    pub circuit_breaker_state: CircuitState,
    pub circuit_breaker_failures: u64,
}

#[derive(Debug, Serialize)]
pub struct Layer1Summary {
    pub persisted_count: u64,
    pub total_original_size: u64,
    pub total_preview_size: u64,
    pub budget_exceeded_count: u64,
    pub average_compression: f64,
}

#[derive(Debug, Serialize)]
pub struct Layer2Summary {
    pub trigger_count: u64,
    pub cleared_count: u64,
    pub kept_count: u64,
}

#[derive(Debug, Serialize)]
pub struct Layer3Summary {
    pub extraction_count: u64,
    pub load_count: u64,
    pub current_size: u64,
}

#[derive(Debug, Serialize)]
pub struct Layer4Summary {
    pub compact_count: u64,
    pub auto_compact_count: u64,
    pub manual_compact_count: u64,
    pub failed_count: u64,
    pub consecutive_failures: u64,
    pub average_compression_ratio: f64,
    pub cache_hit_rate: f64,
}

#[derive(Debug, Serialize)]
pub struct Layer5Summary {
    pub extraction_count: u64,
    pub user_memories: u64,
    pub project_memories: u64,
    pub feedback_memories: u64,
    pub reference_memories: u64,
    pub total_bytes_written: u64,
}

#[derive(Debug, Serialize)]
pub struct Layer6Summary {
    pub dream_count: u64,
    pub memories_created: u64,
    pub memories_updated: u64,
    pub memories_deleted: u64,
    pub sessions_pruned: u64,
}

#[derive(Debug, Serialize)]
pub struct Layer7Summary {
    pub spawned_count: u64,
    pub completed_count: u64,
    pub failed_count: u64,
    pub tool_denied_count: u64,
    pub total_tokens_used: u64,
    pub total_turns_used: u64,
}

/// Get a snapshot of all memory metrics.
pub fn get_metrics_summary() -> MetricsSummary {
    let m = get_memory_metrics();
    let cb = get_compact_circuit_breaker();

    MetricsSummary {
        layer1: Layer1Summary {
            persisted_count: m.layer1.persisted_count(),
            total_original_size: m.layer1.total_original_size(),
            total_preview_size: m.layer1.total_preview_size(),
            budget_exceeded_count: m.layer1.budget_exceeded_count(),
            average_compression: m.layer1.average_compression(),
        },
        layer2: Layer2Summary {
            trigger_count: m.layer2.trigger_count(),
            cleared_count: m.layer2.cleared_count(),
            kept_count: m.layer2.kept_count(),
        },
        layer3: Layer3Summary {
            extraction_count: m.layer3.extraction_count(),
            load_count: m.layer3.load_count(),
            current_size: m.layer3.current_size(),
        },
        layer4: Layer4Summary {
            compact_count: m.layer4.compact_count(),
            auto_compact_count: m.layer4.auto_compact_count(),
            manual_compact_count: m.layer4.manual_compact_count(),
            failed_count: m.layer4.compact_failed_count(),
            consecutive_failures: m.layer4.consecutive_failures(),
            average_compression_ratio: m.layer4.average_compression_ratio(),
            cache_hit_rate: m.layer4.cache_hit_rate(),
        },
        layer5: Layer5Summary {
            extraction_count: m.layer5.extraction_count(),
            user_memories: m.layer5.user_memories(),
            project_memories: m.layer5.project_memories(),
            feedback_memories: m.layer5.feedback_memories(),
            reference_memories: m.layer5.reference_memories(),
            total_bytes_written: m.layer5.total_bytes_written(),
        },
        layer6: Layer6Summary {
            dream_count: m.layer6.dream_count(),
            memories_created: m.layer6.memories_created(),
            memories_updated: m.layer6.memories_updated(),
            memories_deleted: m.layer6.memories_deleted(),
            sessions_pruned: m.layer6.sessions_pruned(),
        },
        layer7: Layer7Summary {
            spawned_count: m.layer7.spawned_count(),
            completed_count: m.layer7.completed_count(),
            failed_count: m.layer7.failed_count(),
            tool_denied_count: m.layer7.tool_denied_count(),
            total_tokens_used: m.layer7.total_tokens_used(),
            total_turns_used: m.layer7.total_turns_used(),
        },
        circuit_breaker_state: cb.state(),
        circuit_breaker_failures: cb.failure_count(),
    }
}

/// Reset all metrics to zero.
pub fn reset_metrics() {
    let m = get_memory_metrics();
    let cb = get_compact_circuit_breaker();

    // Reset all layer metrics
    m.reset();

    // Reset circuit breaker
    cb.reset();

    tracing::info!(
        target: "blockcell.session_metrics",
        "All metrics counters have been reset"
    );
}

/// Format bytes as human-readable string.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

/// Format metrics as a markdown table for CLI output.
pub fn format_metrics_table(summary: &MetricsSummary, layer_filter: Option<u8>) -> String {
    let mut output = String::new();

    output.push_str("```\n");
    output.push_str("╔═══════════════════════════════════════════════════════════════╗\n");
    output.push_str("║              BlockCell Memory Metrics Summary                 ║\n");
    output.push_str("╠═══════════════════════════════════════════════════════════════╣\n");

    // Layer 1
    if layer_filter.is_none() || layer_filter == Some(1) {
        output.push_str("║                                                               ║\n");
        output.push_str("║  📁 Layer 1: Tool Result Storage\n");
        output.push_str(&format!(
            "║  ├─ Persisted: {} files\n",
            summary.layer1.persisted_count
        ));
        output.push_str(&format!(
            "║  ├─ Original: {} → Preview: {}\n",
            format_bytes(summary.layer1.total_original_size),
            format_bytes(summary.layer1.total_preview_size)
        ));
        output.push_str(&format!(
            "║  ├─ Budget exceeded: {} times\n",
            summary.layer1.budget_exceeded_count
        ));
        output.push_str(&format!(
            "║  └─ Compression: {:.1}%\n",
            summary.layer1.average_compression * 100.0
        ));
    }

    // Layer 2
    if layer_filter.is_none() || layer_filter == Some(2) {
        output.push_str("║                                                               ║\n");
        output.push_str("║  ⚡ Layer 2: Micro Compact\n");
        output.push_str(&format!(
            "║  ├─ Triggered: {} times\n",
            summary.layer2.trigger_count
        ));
        output.push_str(&format!(
            "║  ├─ Cleared: {} items\n",
            summary.layer2.cleared_count
        ));
        output.push_str(&format!("║  └─ Kept: {} items\n", summary.layer2.kept_count));
    }

    // Layer 3
    if layer_filter.is_none() || layer_filter == Some(3) {
        output.push_str("║                                                               ║\n");
        output.push_str("║  📝 Layer 3: Session Memory\n");
        output.push_str(&format!(
            "║  ├─ Extractions: {}\n",
            summary.layer3.extraction_count
        ));
        output.push_str(&format!("║  ├─ Loads: {}\n", summary.layer3.load_count));
        output.push_str(&format!(
            "║  └─ Current size: {}\n",
            format_bytes(summary.layer3.current_size)
        ));
    }

    // Layer 4
    if layer_filter.is_none() || layer_filter == Some(4) {
        let success_rate = if summary.layer4.compact_count > 0 {
            1.0 - (summary.layer4.failed_count as f64 / summary.layer4.compact_count as f64)
        } else {
            1.0
        };

        output.push_str("║                                                               ║\n");
        output.push_str("║  🗜️  Layer 4: Full Compact\n");
        output.push_str(&format!(
            "║  ├─ Total: {} (auto: {}, manual: {})\n",
            summary.layer4.compact_count,
            summary.layer4.auto_compact_count,
            summary.layer4.manual_compact_count
        ));
        output.push_str(&format!(
            "║  ├─ Failed: {} ({:.1}%)\n",
            summary.layer4.failed_count,
            (1.0 - success_rate) * 100.0
        ));
        output.push_str(&format!(
            "║  ├─ Avg compression: {:.1}%\n",
            summary.layer4.average_compression_ratio * 100.0
        ));
        output.push_str(&format!(
            "║  └─ Cache hit rate: {:.1}%\n",
            summary.layer4.cache_hit_rate * 100.0
        ));
    }

    // Layer 5
    if layer_filter.is_none() || layer_filter == Some(5) {
        output.push_str("║                                                               ║\n");
        output.push_str("║  🧠 Layer 5: Memory Extraction\n");
        output.push_str(&format!(
            "║  ├─ Extractions: {}\n",
            summary.layer5.extraction_count
        ));
        output.push_str(&format!(
            "║  ├─ Memories: user({}), project({}), feedback({}), ref({})\n",
            summary.layer5.user_memories,
            summary.layer5.project_memories,
            summary.layer5.feedback_memories,
            summary.layer5.reference_memories
        ));
        output.push_str(&format!(
            "║  └─ Storage: {}\n",
            format_bytes(summary.layer5.total_bytes_written)
        ));
    }

    // Layer 6
    if layer_filter.is_none() || layer_filter == Some(6) {
        output.push_str("║                                                               ║\n");
        output.push_str("║  💤 Layer 6: Auto Dream\n");
        output.push_str(&format!(
            "║  ├─ Dream runs: {}\n",
            summary.layer6.dream_count
        ));
        output.push_str(&format!(
            "║  ├─ Memories: +{}/~{}/-{}\n",
            summary.layer6.memories_created,
            summary.layer6.memories_updated,
            summary.layer6.memories_deleted
        ));
        output.push_str(&format!(
            "║  └─ Sessions pruned: {}\n",
            summary.layer6.sessions_pruned
        ));
    }

    // Layer 7
    if layer_filter.is_none() || layer_filter == Some(7) {
        output.push_str("║                                                               ║\n");
        output.push_str("║  🤖 Layer 7: Forked Agent\n");
        output.push_str(&format!(
            "║  ├─ Spawned: {}\n",
            summary.layer7.spawned_count
        ));
        output.push_str(&format!(
            "║  ├─ Completed: {} / Failed: {}\n",
            summary.layer7.completed_count, summary.layer7.failed_count
        ));
        output.push_str(&format!(
            "║  ├─ Tool denied: {}\n",
            summary.layer7.tool_denied_count
        ));
        output.push_str(&format!(
            "║  └─ Tokens used: {}\n",
            format_bytes(summary.layer7.total_tokens_used)
        ));
    }

    // Circuit Breaker
    output.push_str("║                                                               ║\n");
    let cb_display = match summary.circuit_breaker_state {
        CircuitState::Open => ("○", "OPEN", "熔断中"),
        CircuitState::HalfOpen => ("◐", "HALF_OPEN", "半开"),
        CircuitState::Closed => ("●", "CLOSED", "正常"),
    };
    output.push_str(&format!(
        "║  Circuit Breaker: {} {} ({})\n",
        cb_display.0, cb_display.1, cb_display.2
    ));

    output.push_str("║                                                               ║\n");
    output.push_str("╚═══════════════════════════════════════════════════════════════╝\n");
    output.push_str("```\n");

    output
}