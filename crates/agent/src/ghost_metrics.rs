use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use blockcell_core::Paths;
use serde::Serialize;

#[derive(Debug, Default)]
pub struct GhostMetrics {
    episodes_captured: AtomicU64,
    reviews_started: AtomicU64,
    reviews_failed: AtomicU64,
    dead_letters: AtomicU64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GhostMetricsSnapshot {
    pub episodes_captured: u64,
    pub reviews_started: u64,
    pub reviews_failed: u64,
    pub dead_letters: u64,
}

impl GhostMetrics {
    pub fn record_episode_captured(&self) {
        self.episodes_captured.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_review_started(&self) {
        self.reviews_started.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_review_failed(&self) {
        self.reviews_failed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_dead_letter(&self) {
        self.dead_letters.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> GhostMetricsSnapshot {
        GhostMetricsSnapshot {
            episodes_captured: self.episodes_captured.load(Ordering::Relaxed),
            reviews_started: self.reviews_started.load(Ordering::Relaxed),
            reviews_failed: self.reviews_failed.load(Ordering::Relaxed),
            dead_letters: self.dead_letters.load(Ordering::Relaxed),
        }
    }

    pub fn reset(&self) {
        self.episodes_captured.store(0, Ordering::Relaxed);
        self.reviews_started.store(0, Ordering::Relaxed);
        self.reviews_failed.store(0, Ordering::Relaxed);
        self.dead_letters.store(0, Ordering::Relaxed);
    }
}

fn metrics_registry() -> &'static Mutex<HashMap<String, Arc<GhostMetrics>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<GhostMetrics>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn registry_key(paths: &Paths) -> String {
    paths.base.display().to_string()
}

pub fn get_ghost_metrics(paths: &Paths) -> Arc<GhostMetrics> {
    let key = registry_key(paths);
    let mut registry = metrics_registry()
        .lock()
        .expect("ghost metrics registry lock poisoned");
    registry
        .entry(key)
        .or_insert_with(|| Arc::new(GhostMetrics::default()))
        .clone()
}

pub fn ghost_metrics_summary(paths: &Paths) -> GhostMetricsSnapshot {
    get_ghost_metrics(paths).snapshot()
}

pub fn reset_ghost_metrics_for_paths(paths: &Paths) {
    get_ghost_metrics(paths).reset();
}
