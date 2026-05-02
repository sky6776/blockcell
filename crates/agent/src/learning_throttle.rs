//! Learning throttle — prevents review storms and concurrent review overload

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Throttle for learning review operations
///
/// Prevents:
/// - Too many concurrent reviews (max_concurrent_reviews)
/// - Reviews firing too frequently (min_review_interval_secs)
pub struct LearningThrottle {
    last_review_completed: Mutex<Option<Instant>>,
    active_reviews: AtomicU32,
    max_concurrent_reviews: u32,
    min_review_interval_secs: u64,
}

impl LearningThrottle {
    pub fn new(max_concurrent_reviews: u32, min_review_interval_secs: u64) -> Self {
        Self {
            last_review_completed: Mutex::new(None),
            active_reviews: AtomicU32::new(0),
            max_concurrent_reviews,
            min_review_interval_secs,
        }
    }

    /// Check if a new review can be started
    ///
    /// Returns false if:
    /// - Too many concurrent reviews are active
    /// - Last review completed too recently
    pub fn can_start_review(&self) -> bool {
        // Check concurrent limit
        if self.active_reviews.load(Ordering::Relaxed) >= self.max_concurrent_reviews {
            return false;
        }

        // Check cooldown
        if let Ok(guard) = self.last_review_completed.lock() {
            if let Some(last) = *guard {
                if last.elapsed().as_secs() < self.min_review_interval_secs {
                    return false;
                }
            }
        }

        true
    }

    /// Record that a review has started
    pub fn review_started(&self) {
        self.active_reviews.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a review has completed
    pub fn review_completed(&self) {
        self.active_reviews.fetch_sub(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.last_review_completed.lock() {
            *guard = Some(Instant::now());
        }
    }

    /// Get the number of currently active reviews
    pub fn active_count(&self) -> u32 {
        self.active_reviews.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for LearningThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LearningThrottle")
            .field(
                "active_reviews",
                &self.active_reviews.load(Ordering::Relaxed),
            )
            .field("max_concurrent_reviews", &self.max_concurrent_reviews)
            .field("min_review_interval_secs", &self.min_review_interval_secs)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_can_start_when_idle() {
        let throttle = LearningThrottle::new(2, 0);
        assert!(throttle.can_start_review());
    }

    #[test]
    fn test_blocks_when_concurrent_limit_reached() {
        let throttle = LearningThrottle::new(2, 0);
        throttle.review_started();
        throttle.review_started();
        assert!(!throttle.can_start_review());
        assert_eq!(throttle.active_count(), 2);
    }

    #[test]
    fn test_allows_after_completion() {
        let throttle = LearningThrottle::new(1, 0);
        throttle.review_started();
        assert!(!throttle.can_start_review());
        throttle.review_completed();
        assert!(throttle.can_start_review());
    }

    #[test]
    fn test_cooldown_prevents_rapid_fire() {
        let throttle = LearningThrottle::new(2, 300);
        throttle.review_started();
        throttle.review_completed();
        // Immediately after completion, cooldown is active
        assert!(!throttle.can_start_review());
    }
}
