use async_trait::async_trait;
use blockcell_core::{Error, Result};
use serde_json::{json, Value};
use tracing::debug;

use crate::{Tool, ToolContext, ToolSchema};

/// Tool for querying memory using FTS5 full-text search + structured filters.
pub struct MemoryQueryTool;

/// Tool for upserting memory items with type/scope/tags/dedup support.
pub struct MemoryUpsertTool;

/// Tool for deleting/forgetting memory items (soft-delete, batch delete, restore).
pub struct MemoryForgetTool;

fn get_memory_store(ctx: &ToolContext) -> Result<&crate::MemoryStoreHandle> {
    ctx.memory_store
        .as_ref()
        .ok_or_else(|| Error::Tool("Memory store not available".to_string()))
}

fn looks_like_ghost_maintenance_log(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("ghost agent")
        || t.contains("memory garden")
        || t.contains("例行维护")
        || t.contains("维护任务")
        || t.contains("记忆整理")
        || t.contains("文件清理")
        || t.contains("社区互动")
        || t.contains("heart")
        || t.contains("heartbeat")
        || t.contains("feed")
}

#[async_trait]
impl Tool for MemoryQueryTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "memory_query",
            description: "Search and retrieve memory items using full-text search with structured filters. Use this to recall facts, preferences, past decisions, project context, or any previously stored information. Supports filtering by scope (long_term/short_term), type, tags, and time range. Results are ranked by relevance, importance, and recency.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Full-text search query. Leave empty to browse by filters."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["long_term", "short_term"],
                        "description": "Filter by memory scope. Omit to search all scopes."
                    },
                    "type": {
                        "type": "string",
                        "enum": ["fact", "preference", "project", "task", "glossary", "contact", "snippet", "policy", "note", "session_summary"],
                        "description": "Filter by memory type."
                    },
                    "tags": {
                        "type": "string",
                        "description": "Comma-separated tags to filter by (any match)."
                    },
                    "time_range_days": {
                        "type": "integer",
                        "description": "Only return items created within the last N days."
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Maximum number of results to return (default: 20, max: 50)."
                    },
                    "include_deleted": {
                        "type": "boolean",
                        "description": "Include soft-deleted items in results (default: false)."
                    },
                    "stats": {
                        "type": "boolean",
                        "description": "If true, return memory statistics instead of search results."
                    }
                },
                "required": []
            }),
        }
    }

    fn prompt_rule(&self, _ctx: &crate::PromptContext) -> Option<String> {
        Some("- Search `memory_query` before asking the user for information you might already know.".to_string())
    }

    fn validate(&self, _params: &Value) -> Result<()> {
        Ok(())
    }

    async fn execute(&self, ctx: ToolContext, params: Value) -> Result<Value> {
        let store = get_memory_store(&ctx)?;

        // Stats mode
        if params
            .get("stats")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return store.stats_json();
        }

        let query_params = json!({
            "query": params.get("query").and_then(|v| v.as_str()),
            "scope": params.get("scope").and_then(|v| v.as_str()),
            "type": params.get("type").and_then(|v| v.as_str()),
            "tags": params.get("tags").and_then(|v| v.as_str()),
            "time_range_days": params.get("time_range_days").and_then(|v| v.as_i64()),
            "top_k": params.get("top_k").and_then(|v| v.as_i64()).unwrap_or(20).min(50),
            "include_deleted": params.get("include_deleted").and_then(|v| v.as_bool()).unwrap_or(false),
        });

        let results = store.query_json(query_params)?;

        debug!("memory_query executed");
        Ok(results)
    }
}

#[async_trait]
impl Tool for MemoryUpsertTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "memory_upsert",
            description: "Save or update a memory item. Supports structured metadata (type, scope, tags, importance) and dedup_key for automatic merge/update of existing items. Use scope='long_term' for persistent facts/preferences, scope='short_term' for session notes and temporary context.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The content to remember (markdown text)."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["long_term", "short_term"],
                        "description": "Memory scope. 'long_term' for persistent facts/preferences, 'short_term' for session notes. Default: 'short_term'."
                    },
                    "type": {
                        "type": "string",
                        "enum": ["fact", "preference", "project", "task", "glossary", "contact", "snippet", "policy", "note"],
                        "description": "Type classification. Default: 'note'."
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional short title/label for the memory item."
                    },
                    "summary": {
                        "type": "string",
                        "description": "Optional 1-2 line summary (used in brief injection to save prompt tokens)."
                    },
                    "tags": {
                        "type": "string",
                        "description": "Comma-separated tags for categorization and filtering."
                    },
                    "importance": {
                        "type": "number",
                        "description": "Importance score 0.0-1.0. Higher = more likely to appear in brief. Default: 0.5."
                    },
                    "dedup_key": {
                        "type": "string",
                        "description": "Deduplication key. If an item with the same dedup_key exists, it will be updated instead of creating a duplicate. Use for preferences (e.g. 'pref.language'), facts (e.g. 'user.name'), etc."
                    },
                    "expires_in_days": {
                        "type": "integer",
                        "description": "Auto-expire after N days. Useful for short-term items. Omit for no expiry."
                    }
                },
                "required": ["content"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<()> {
        if params.get("content").and_then(|v| v.as_str()).is_none() {
            return Err(Error::Validation(
                "Missing required parameter: content".to_string(),
            ));
        }
        Ok(())
    }

    async fn execute(&self, ctx: ToolContext, params: Value) -> Result<Value> {
        let store = get_memory_store(&ctx)?;

        let content = params["content"].as_str().unwrap();
        let scope = params
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("short_term");
        let item_type = params
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("note");
        let title = params.get("title").and_then(|v| v.as_str());
        let summary = params.get("summary").and_then(|v| v.as_str());
        let tags_str = params.get("tags").and_then(|v| v.as_str()).unwrap_or("");
        let importance = params
            .get("importance")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5);
        let dedup_key = params.get("dedup_key").and_then(|v| v.as_str());
        let expires_in_days = params.get("expires_in_days").and_then(|v| v.as_i64());

        // Guardrail: Ghost maintenance routine must not write its own logs into memory.
        // We allow Ghost to upsert only meaningful long-term items (facts/preferences/projects/tasks).
        if ctx.channel == "ghost" {
            if scope != "long_term" {
                return Err(Error::Validation(
                    "Ghost channel is not allowed to save short_term memory. Extract only genuine long_term facts/preferences/projects/tasks.".to_string(),
                ));
            }

            match item_type {
                "fact" | "preference" | "project" | "task" => {}
                _ => {
                    return Err(Error::Validation(
                        "Ghost channel may only save long_term memory of type: fact/preference/project/task.".to_string(),
                    ));
                }
            }

            let title_text = title.unwrap_or("");
            if looks_like_ghost_maintenance_log(title_text)
                || looks_like_ghost_maintenance_log(content)
            {
                return Err(Error::Validation(
                    "Refusing to save Ghost maintenance logs into memory.".to_string(),
                ));
            }
        }

        let expires_at = expires_in_days
            .map(|days| (chrono::Utc::now() + chrono::Duration::days(days)).to_rfc3339());

        let upsert_params = json!({
            "scope": scope,
            "type": item_type,
            "title": title,
            "content": content,
            "summary": summary,
            "tags": tags_str,
            "source": "tool",
            "channel": ctx.channel,
            "session_key": ctx.session_key,
            "importance": importance,
            "dedup_key": dedup_key,
            "expires_at": expires_at,
        });

        let result = store.upsert_json(upsert_params)?;

        debug!(
            scope = scope,
            item_type = item_type,
            "memory_upsert executed"
        );
        Ok(json!({
            "status": "saved",
            "item": result,
        }))
    }
}

#[async_trait]
impl Tool for MemoryForgetTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "memory_forget",
            description: "Delete or restore memory items. Supports single item deletion by ID, batch deletion by filters (scope, type, tags, time), and restoration of soft-deleted items.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["delete", "batch_delete", "restore"],
                        "description": "Action to perform. 'delete' soft-deletes one item by ID, 'batch_delete' soft-deletes by filters, 'restore' recovers a soft-deleted item."
                    },
                    "id": {
                        "type": "string",
                        "description": "Memory item ID (required for 'delete' and 'restore' actions)."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["long_term", "short_term"],
                        "description": "Filter for batch_delete: only delete items in this scope."
                    },
                    "type": {
                        "type": "string",
                        "description": "Filter for batch_delete: only delete items of this type."
                    },
                    "tags": {
                        "type": "string",
                        "description": "Filter for batch_delete: comma-separated tags (items matching any tag)."
                    },
                    "before_days": {
                        "type": "integer",
                        "description": "Filter for batch_delete: only delete items created more than N days ago."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<()> {
        let action = params.get("action").and_then(|v| v.as_str());
        match action {
            Some("delete") | Some("restore") => {
                if params.get("id").and_then(|v| v.as_str()).is_none() {
                    return Err(Error::Validation(
                        "'id' is required for delete/restore actions".to_string(),
                    ));
                }
            }
            Some("batch_delete") => {}
            _ => {
                return Err(Error::Validation(
                    "'action' must be 'delete', 'batch_delete', or 'restore'".to_string(),
                ));
            }
        }
        Ok(())
    }

    async fn execute(&self, ctx: ToolContext, params: Value) -> Result<Value> {
        let store = get_memory_store(&ctx)?;
        let action = params["action"].as_str().unwrap();

        match action {
            "delete" => {
                let id = params["id"].as_str().unwrap();
                let deleted = store.soft_delete(id)?;
                Ok(json!({
                    "action": "delete",
                    "id": id,
                    "deleted": deleted,
                    "note": if deleted { "Item moved to recycle bin. Use action='restore' to recover." } else { "Item not found or already deleted." },
                }))
            }
            "batch_delete" => {
                let batch_params = json!({
                    "scope": params.get("scope").and_then(|v| v.as_str()),
                    "type": params.get("type").and_then(|v| v.as_str()),
                    "tags": params.get("tags").and_then(|v| v.as_str()),
                    "before_days": params.get("before_days").and_then(|v| v.as_i64()),
                });
                let count = store.batch_soft_delete_json(batch_params)?;
                Ok(json!({
                    "action": "batch_delete",
                    "deleted_count": count,
                    "note": format!("{} items moved to recycle bin.", count),
                }))
            }
            "restore" => {
                let id = params["id"].as_str().unwrap();
                let restored = store.restore(id)?;
                Ok(json!({
                    "action": "restore",
                    "id": id,
                    "restored": restored,
                    "note": if restored { "Item restored from recycle bin." } else { "Item not found in recycle bin." },
                }))
            }
            _ => Err(Error::Validation("Invalid action".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::Config;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use serde_json::json;

    struct CaptureMemoryStore {
        last_upsert: Mutex<Option<Value>>,
    }

    impl CaptureMemoryStore {
        fn new() -> Self {
            Self {
                last_upsert: Mutex::new(None),
            }
        }

        fn last_upsert(&self) -> Value {
            self.last_upsert
                .lock()
                .expect("last_upsert lock")
                .clone()
                .expect("captured upsert params")
        }
    }

    impl crate::MemoryStoreOps for CaptureMemoryStore {
        fn upsert_json(&self, params_json: Value) -> Result<Value> {
            *self.last_upsert.lock().expect("last_upsert lock") = Some(params_json);
            Ok(json!({
                "id": "mem-1",
                "scope": "short_term",
                "type": "note",
                "content": "remember this",
                "summary": null,
                "tags": [],
                "source": "tool",
                "channel": "cli",
                "session_key": "cli:test",
                "importance": 0.5,
                "created_at": "2026-03-25T00:00:00Z",
                "updated_at": "2026-03-25T00:00:00Z",
                "last_accessed_at": null,
                "access_count": 0,
                "expires_at": null,
                "deleted_at": null,
                "dedup_key": null,
            }))
        }

        fn query_json(&self, _params_json: Value) -> Result<Value> {
            Ok(json!([]))
        }

        fn soft_delete(&self, _id: &str) -> Result<bool> {
            Ok(false)
        }

        fn batch_soft_delete_json(&self, _params_json: Value) -> Result<usize> {
            Ok(0)
        }

        fn restore(&self, _id: &str) -> Result<bool> {
            Ok(false)
        }

        fn stats_json(&self) -> Result<Value> {
            Ok(json!({}))
        }

        fn generate_brief(&self, _long_term_max: usize, _short_term_max: usize) -> Result<String> {
            Ok(String::new())
        }

        fn generate_brief_for_query(&self, _query: &str, _max_items: usize) -> Result<String> {
            Ok(String::new())
        }

        fn upsert_session_summary(&self, _session_key: &str, _summary: &str) -> Result<()> {
            Ok(())
        }

        fn get_session_summary(&self, _session_key: &str) -> Result<Option<String>> {
            Ok(None)
        }

        fn maintenance(&self, _recycle_days: i64) -> Result<(usize, usize)> {
            Ok((0, 0))
        }
    }

    fn test_context(memory_store: Arc<dyn crate::MemoryStoreOps + Send + Sync>) -> ToolContext {
        ToolContext {
            workspace: PathBuf::from("/tmp/workspace"),
            builtin_skills_dir: None,
            active_skill_dir: None,
            session_key: "cli:test".to_string(),
            channel: "cli".to_string(),
            account_id: None,
            sender_id: None,
            chat_id: "chat-1".to_string(),
            config: Config::default(),
            permissions: blockcell_core::types::PermissionSet::new(),
            task_manager: None,
            memory_store: Some(memory_store),
            outbound_tx: None,
            spawn_handle: None,
            capability_registry: None,
            core_evolution: None,
            event_emitter: None,
            channel_contacts_file: None,
            response_cache: None,
        }
    }

    fn schema_enum_values(parameters: &Value, field: &str) -> Vec<String> {
        parameters["properties"][field]["enum"]
            .as_array()
            .expect("enum array")
            .iter()
            .filter_map(|value| value.as_str().map(ToString::to_string))
            .collect()
    }

    #[test]
    fn test_memory_query_schema() {
        let tool = MemoryQueryTool;
        let schema = tool.schema();
        assert_eq!(schema.name, "memory_query");
    }

    #[test]
    fn test_memory_query_validate() {
        let tool = MemoryQueryTool;
        assert!(tool.validate(&json!({})).is_ok());
        assert!(tool.validate(&json!({"query": "test"})).is_ok());
    }

    #[test]
    fn test_memory_query_schema_uses_canonical_types() {
        let tool = MemoryQueryTool;
        let schema = tool.schema();
        let types = schema_enum_values(&schema.parameters, "type");

        assert!(!types.contains(&"summary".to_string()));
        assert!(types.contains(&"session_summary".to_string()));
    }

    #[test]
    fn test_memory_upsert_schema() {
        let tool = MemoryUpsertTool;
        let schema = tool.schema();
        assert_eq!(schema.name, "memory_upsert");
    }

    #[test]
    fn test_memory_upsert_schema_uses_allowed_types_only() {
        let tool = MemoryUpsertTool;
        let schema = tool.schema();
        let types = schema_enum_values(&schema.parameters, "type");

        assert!(!types.contains(&"summary".to_string()));
        assert!(!types.contains(&"session_summary".to_string()));
    }

    #[test]
    fn test_memory_upsert_validate() {
        let tool = MemoryUpsertTool;
        assert!(tool.validate(&json!({"content": "remember this"})).is_ok());
        assert!(tool.validate(&json!({})).is_err());
    }

    #[tokio::test]
    async fn test_memory_upsert_execute_leaves_default_ttl_to_storage_service() {
        let store = Arc::new(CaptureMemoryStore::new());
        let tool = MemoryUpsertTool;

        tool.execute(
            test_context(store.clone()),
            json!({
                "content": "remember this",
                "scope": "short_term",
                "type": "note"
            }),
        )
        .await
        .expect("memory_upsert should succeed");

        let captured = store.last_upsert();
        assert!(captured["expires_at"].is_null());
    }

    #[test]
    fn test_memory_forget_schema() {
        let tool = MemoryForgetTool;
        let schema = tool.schema();
        assert_eq!(schema.name, "memory_forget");
    }

    #[test]
    fn test_memory_forget_validate() {
        let tool = MemoryForgetTool;
        assert!(tool
            .validate(&json!({"action": "delete", "id": "abc"}))
            .is_ok());
        assert!(tool
            .validate(&json!({"action": "restore", "id": "abc"}))
            .is_ok());
        assert!(tool.validate(&json!({"action": "batch_delete"})).is_ok());
        assert!(tool.validate(&json!({"action": "delete"})).is_err());
        assert!(tool.validate(&json!({"action": "invalid"})).is_err());
    }
}
