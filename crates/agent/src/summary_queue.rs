use std::sync::{Arc, Mutex};

use blockcell_core::system_event::{
    SessionSummary, SummaryCategory, SummaryItem, SummaryScope, SystemEvent,
};
use uuid::Uuid;

/// 安全获取锁，处理锁中毒情况
///
/// 如果锁中毒（持有锁的线程 panic），会恢复并返回内部状态。
/// 这是安全的，因为 SummaryQueue 的数据可以重建。
fn get_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("[summary_queue] Lock poisoned, recovering");
            poisoned.into_inner()
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SummaryQueueSnapshot {
    pub pending_count: usize,
    pub items: Vec<SummaryItem>,
}

#[derive(Clone)]
pub struct MainSessionSummaryQueue {
    items: Arc<Mutex<Vec<SummaryItem>>>,
    max_items_before_flush: usize,
    max_age_ms: i64,
}

impl MainSessionSummaryQueue {
    pub fn with_policy(max_items_before_flush: usize, max_age_ms: i64) -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
            max_items_before_flush,
            max_age_ms,
        }
    }

    pub fn enqueue(&self, item: SummaryItem) {
        let mut items = get_lock(&self.items);
        if let Some(merge_key) = item.merge_key.as_deref() {
            if let Some(existing) = items
                .iter_mut()
                .find(|existing| existing.merge_key.as_deref() == Some(merge_key))
            {
                for source_event_id in item.source_event_ids {
                    if !existing.source_event_ids.contains(&source_event_id) {
                        existing.source_event_ids.push(source_event_id);
                    }
                }
                existing.title = item.title;
                existing.body = item.body;
                existing.created_at_ms = item.created_at_ms;
                existing.priority = existing.priority.max(item.priority);
                existing.category = item.category;
                return;
            }
        }
        items.push(item);
    }

    pub fn enqueue_event_as_summary_item(&self, event: &SystemEvent) -> SummaryItem {
        let item = SummaryItem {
            id: format!("sum_{}", Uuid::new_v4()),
            scope: SummaryScope::MainSession,
            category: category_for_event(event),
            title: event.title.clone(),
            body: event.summary.clone(),
            source_event_ids: vec![event.id.clone()],
            created_at_ms: event.created_at_ms,
            priority: event.priority,
            merge_key: event.dedup_key.clone(),
        };
        self.enqueue(item.clone());
        item
    }

    pub fn flush_due_items(&self, now_ms: i64) -> Vec<SummaryItem> {
        let mut items = get_lock(&self.items);
        if items.is_empty() {
            return Vec::new();
        }

        let oldest_created_at = items
            .iter()
            .map(|item| item.created_at_ms)
            .min()
            .unwrap_or(now_ms);
        let age_due = now_ms.saturating_sub(oldest_created_at) >= self.max_age_ms;
        let count_due = items.len() >= self.max_items_before_flush;

        if !age_due && !count_due {
            return Vec::new();
        }

        let mut flushed = items.clone();
        flushed.sort_by_key(|item| item.created_at_ms);
        items.clear();
        flushed
    }

    pub fn snapshot(&self) -> SummaryQueueSnapshot {
        let items = get_lock(&self.items);
        let mut cloned = items.clone();
        cloned.sort_by_key(|item| item.created_at_ms);
        SummaryQueueSnapshot {
            pending_count: cloned.len(),
            items: cloned,
        }
    }

    pub fn build_session_summary(&self, items: Vec<SummaryItem>) -> SessionSummary {
        let compact_text = items
            .iter()
            .map(|item| format!("- {}", item.title))
            .collect::<Vec<_>>()
            .join("\n");
        SessionSummary {
            title: "System updates".to_string(),
            items,
            compact_text,
        }
    }
}

fn category_for_event(event: &SystemEvent) -> SummaryCategory {
    if event.kind.starts_with("task.") {
        SummaryCategory::Task
    } else if event.kind.starts_with("cron.") {
        SummaryCategory::Cron
    } else if event.kind.starts_with("ghost.") {
        SummaryCategory::Ghost
    } else {
        SummaryCategory::System
    }
}
