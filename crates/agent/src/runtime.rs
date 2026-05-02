use blockcell_core::path_policy::{PathOp, PathPolicy, PolicyAction};
use blockcell_core::system_event::{EventPriority, EventScope, SessionSummary, SystemEvent};
use blockcell_core::types::{
    ChatMessage, LLMResponse, StreamChunk, ToolCallAccumulator, ToolCallRequest,
};
use blockcell_core::{Config, InboundMessage, OutboundMessage, Paths, Result};
use blockcell_providers::{CallResult, Provider, ProviderPool};
use blockcell_skills::SkillCard;
use blockcell_storage::ghost_ledger::{GhostEpisodeSource, NewGhostEpisode};
use blockcell_storage::{AuditLogger, GhostLedger, SessionStore};
use blockcell_tools::{
    CapabilityRegistryHandle, CoreEvolutionHandle, EventEmitterHandle, MemoryStoreHandle,
    SessionSearchOps, SpawnHandle, SystemEventEmitter, TaskManagerHandle, ToolContext,
    ToolRegistry,
};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::context::{ActiveSkillContext, ContextBuilder, InteractionMode};
use crate::error::{
    classify_tool_failure, dangerous_exec_denied, dangerous_file_ops_denied, disabled_skill_result,
    disabled_tool_result, llm_exhausted_error, scoped_tool_denied_result, ToolFailureKind,
};
use crate::ghost_background_review::spawn_pending_background_reviews;
use crate::ghost_learning::{
    estimate_turn_complexity_score, GhostEpisodeSnapshot, GhostLearningBoundary,
    GhostLearningBoundaryKind, GhostLearningPolicy, LearningDecision,
};
use crate::ghost_recall::should_inject_ghost_recall;
use crate::history_projector::{HistoryProjector, TimeBasedMCConfig};
use crate::intent::{IntentCategory, IntentToolResolver};
use crate::memory_file_store::MemoryFileStore;
use crate::session_metrics::{ProcessingMetrics, ScopedTimer};
use crate::skill_executor::{determine_manual_load_mode, SkillExecutionResult};
use crate::skill_file_store::SkillFileStore;
use crate::skill_kernel::SkillRunMode;
use crate::summary_queue::MainSessionSummaryQueue;
use crate::system_event_orchestrator::{
    HeartbeatDecision, NotificationRequest, SystemEventOrchestrator,
};
use crate::system_event_store::{InMemorySystemEventStore, SystemEventStoreOps};
use crate::task_manager::TaskManager;
use crate::token::estimate_messages_tokens;

const TOOL_ROUND_THROTTLE_MS: u64 = 600;
const TOOL_ROUND_THROTTLE_AFTER_RATE_LIMIT_MS: u64 = 2_500;
const ACTIVATE_SKILL_TOOL_NAME: &str = "activate_skill";

/// Review 模式枚举
///
/// 用于 NudgeEngine 触发后台 Review 时决定审查范围：
/// - Skill: 仅审查 Skill 库
/// - Memory: 仅审查用户记忆
/// - Combined: 同时审查 Skill 库和用户记忆
#[derive(Debug, Clone)]
enum ReviewMode {
    /// 审查 Skill 库，判断是否需要创建/修补 Skill
    Skill,
    /// 审查对话历史，保存用户偏好和重要信息
    Memory,
    /// 同时审查 Skill 库和用户记忆
    Combined,
}

/// Memory Review 提示词
/// Memory Review 提示词 (与 Hermes _MEMORY_REVIEW_PROMPT 一致)
const MEMORY_REVIEW_PROMPT: &str = "\
Review the conversation above and consider saving to memory if appropriate.\n\n\
Focus on:\n\
1. Has the user revealed things about themselves — their persona, desires, \
preferences, or personal details worth remembering?\n\
2. Has the user expressed expectations about how you should behave, their work \
style, or ways they want you to operate?\n\n\
If something stands out, save it using the memory tool. \
If nothing is worth saving, just say 'Nothing to save.' and stop.";

/// Skill Review 提示词 (与 Hermes _SKILL_REVIEW_PROMPT 一致)
const SKILL_REVIEW_PROMPT: &str = "\
Review the conversation above and consider saving or updating a skill if appropriate.\n\n\
Focus on: was a non-trivial approach used to complete a task that required trial \
and error, or changing course due to experiential findings along the way, or did \
the user expect or desire a different method or outcome?\n\n\
If a relevant skill already exists, update it with what you learned. \
Otherwise, create a new skill if the approach is reusable.\n\
If nothing is worth saving, just say 'Nothing to save.' and stop.";

/// Combined Review 提示词 (与 Hermes _COMBINED_REVIEW_PROMPT 一致)
const COMBINED_REVIEW_PROMPT: &str = "\
Review the conversation above and consider two things:\n\n\
**Memory**: Has the user revealed things about themselves — their persona, \
desires, preferences, or personal details? Has the user expressed expectations \
about how you should behave, their work style, or ways they want you to operate? \
If so, save using the memory tool.\n\n\
**Skills**: Was a non-trivial approach used to complete a task that required trial \
and error, or changing course due to experiential findings along the way, or did \
the user expect or desire a different method or outcome? If a relevant skill \
already exists, update it. Otherwise, create a new one if the approach is reusable.\n\n\
Only act if there's something genuinely worth saving. \
If nothing stands out, just say 'Nothing to save.' and stop.";

/// Compact execution context - contains info needed for notifications.
///
/// Used to send user notifications before/after compression operations.
pub struct CompactContext<'a> {
    /// Channel to send notification to.
    pub channel: &'a str,
    /// Chat ID to send notification to.
    pub chat_id: &'a str,
    /// Account ID for multi-tenant scenarios.
    pub account_id: Option<&'a str>,
}

/// Adapter that wraps a Provider to implement the skills::LLMProvider trait.
/// This allows EvolutionService to call the LLM for code generation without
/// depending on the full provider stack.
struct ProviderLLMAdapter {
    provider: Arc<dyn blockcell_providers::Provider>,
}

#[async_trait::async_trait]
impl blockcell_skills::LLMProvider for ProviderLLMAdapter {
    async fn generate(&self, prompt: &str) -> blockcell_core::Result<String> {
        let messages = vec![
            ChatMessage::system(
                "You are a skill evolution assistant. Follow instructions precisely.",
            ),
            ChatMessage::user(prompt),
        ];
        let response = self.provider.chat(&messages, &[]).await?;
        Ok(response.content.unwrap_or_default())
    }
}

/// A SpawnHandle implementation that captures everything needed to spawn
/// subagents, without requiring a reference to AgentRuntime.
#[derive(Clone)]
pub struct RuntimeSpawnHandle {
    config: Config,
    paths: Paths,
    task_manager: TaskManager,
    outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
    provider_pool: Arc<ProviderPool>,
    agent_id: Option<String>,
    event_tx: Option<broadcast::Sender<String>>,
    origin_session_key: String,
    response_cache: crate::response_cache::ResponseCache,
    event_emitter: EventEmitterHandle,
}

impl SpawnHandle for RuntimeSpawnHandle {
    fn spawn(
        &self,
        task: &str,
        label: &str,
        origin_channel: &str,
        origin_chat_id: &str,
    ) -> Result<serde_json::Value> {
        let task_id = uuid::Uuid::new_v4().to_string();

        info!(
            task_id = %task_id,
            label = %label,
            "Spawning subagent via SpawnHandle"
        );

        // Reuse the shared pool for the subagent (pool is Arc, cheap to clone)
        let provider_pool = Arc::clone(&self.provider_pool);

        // Gather everything the background task needs
        let config = self.config.clone();
        let paths = self.paths.clone();
        let task_manager = self.task_manager.clone();
        let outbound_tx = self.outbound_tx.clone();
        let normalized_task = normalize_spawn_task(task);
        let task_id_clone = task_id.clone();
        let label_clone = label.to_string();
        let origin_channel = origin_channel.to_string();
        let origin_chat_id = origin_chat_id.to_string();
        let agent_id = self.agent_id.clone();
        let event_tx = self.event_tx.clone();
        let session_store = SessionStore::new(self.paths.clone());
        let origin_history = session_store
            .load(&self.origin_session_key)
            .unwrap_or_default();
        let origin_history_seed = expand_history_stubs_with_cache(
            &self.response_cache,
            &self.origin_session_key,
            &origin_history,
        );

        // Spawn the background task. Task registration (create_task) happens inside
        // run_subagent_task before set_running(), eliminating the race condition.
        tokio::spawn(run_subagent_task(
            config,
            paths,
            provider_pool,
            task_manager,
            outbound_tx,
            normalized_task,
            task_id_clone,
            label_clone,
            origin_channel,
            origin_chat_id,
            agent_id,
            event_tx,
            origin_history_seed,
            self.event_emitter.clone(),
        ));

        Ok(serde_json::json!({
            "task_id": task_id,
            "label": label,
            "status": "running",
            "note": "Subagent is now processing this task in the background. Use list_tasks to check progress."
        }))
    }
}

/// A request sent from the runtime to the UI layer asking the user to confirm
/// an operation that accesses paths outside the safe workspace directory.
pub struct ConfirmRequest {
    pub tool_name: String,
    pub paths: Vec<String>,
    pub response_tx: tokio::sync::oneshot::Sender<bool>,
    /// The channel the originating message came from (e.g. "ws", "lark", "telegram").
    pub channel: String,
    /// The chat_id of the originating message, used to route the confirmation
    /// prompt back to the correct conversation.
    pub chat_id: String,
}

/// Truncate a string at a safe char boundary.
fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// Summarize a result to 1-2 sentences
#[allow(dead_code)]
fn summarize_result(result: &str) -> String {
    let max_chars = 200;
    if result.chars().count() <= max_chars {
        result.to_string()
    } else {
        format!("{}... (truncated)", truncate_str(result, max_chars))
    }
}

fn tool_round_throttle_delay(saw_rate_limit_this_turn: bool) -> std::time::Duration {
    if saw_rate_limit_this_turn {
        std::time::Duration::from_millis(TOOL_ROUND_THROTTLE_AFTER_RATE_LIMIT_MS)
    } else {
        std::time::Duration::from_millis(TOOL_ROUND_THROTTLE_MS)
    }
}

fn build_activate_skill_tool_schema(skill_cards: &[SkillCard]) -> Option<serde_json::Value> {
    if skill_cards.is_empty() {
        return None;
    }

    let skill_names = skill_cards
        .iter()
        .map(|card| serde_json::Value::String(card.name.clone()))
        .collect::<Vec<_>>();

    Some(serde_json::json!({
        "type": "function",
        "function": {
            "name": ACTIVATE_SKILL_TOOL_NAME,
            "description": "Activate one installed skill when it is a better fit than general tools. Do not combine this with other tool calls in the same assistant turn.",
            "parameters": {
                "type": "object",
                "properties": {
                    "skill_name": {
                        "type": "string",
                        "enum": skill_names,
                        "description": "The installed skill name to activate."
                    },
                    "goal": {
                        "type": "string",
                        "description": "A short execution goal for the selected skill."
                    }
                },
                "required": ["skill_name", "goal"],
                "additionalProperties": false
            }
        }
    }))
}

fn inject_skill_cards_into_system_prompt(
    messages: &mut [ChatMessage],
    skill_cards: &[SkillCard],
    recent_skill_name: Option<&str>,
) {
    if skill_cards.is_empty() {
        return;
    }

    let Some(system_message) = messages.first_mut() else {
        return;
    };
    if system_message.role != "system" {
        return;
    }

    let Some(existing_prompt) = system_message.content.as_str() else {
        return;
    };

    let mut section = String::from(
        "\n\n## Installed Skills\nUse `activate_skill` when one installed skill is a better fit than general tools.\nIf you call `activate_skill`, do not call any other tools in the same assistant turn.\n",
    );
    section.push_str(
        "If a skill is relevant but you need to inspect the learned procedure before using or patching it, inspect it with `skill_view`. If a loaded skill is stale, incomplete, or wrong, patch it with `skill_manage(action=\"patch\")` before finishing.\n",
    );
    section.push_str(
        "If a skill card shows local execution entries, you may use `exec_local` only for those relative paths and only inside the active skill scope. Do not auto-run local scripts unless the skill is active.\n",
    );

    if let Some(skill_name) = recent_skill_name {
        section.push_str(&format!(
            "Recent active skill: `{}`. If the user is continuing that workflow, prefer re-entering the same skill.\n",
            skill_name
        ));
    }

    for card in skill_cards {
        let local_exec_note = if card.supports_local_exec {
            if card.local_exec_entrypoints.is_empty() {
                " | 本地入口: active skill 目录内的相对脚本".to_string()
            } else {
                format!(" | 本地入口: {}", card.local_exec_entrypoints.join(", "))
            }
        } else {
            String::new()
        };

        section.push_str(&format!(
            "- `{}`: {} | 布局: {}{} | 适合: {} | 输出: {}\n",
            card.name,
            card.description,
            card.execution_layout,
            local_exec_note,
            card.when_to_use,
            card.outputs
        ));
    }

    system_message.content = serde_json::Value::String(format!("{}{}", existing_prompt, section));
}

fn normalize_selected_skill_name(
    raw_skill_name: &str,
    skill_cards: &[SkillCard],
) -> Option<String> {
    let candidates = skill_cards
        .iter()
        .map(|card| (card.name.clone(), card.description.clone()))
        .collect::<Vec<_>>();
    crate::skill_decision::SkillDecisionEngine::normalize_selected_skill_name(
        raw_skill_name,
        &candidates,
    )
}

fn append_activated_skill_history(
    history: &mut Vec<ChatMessage>,
    activation_call_id: &str,
    skill_name: &str,
    goal: &str,
    allowed_tools: &[String],
    trace_messages: &[ChatMessage],
    final_response: &str,
) {
    let mut activation_result = ChatMessage::tool_result(
        activation_call_id,
        &serde_json::json!({
            "skill_name": skill_name,
            "goal": goal,
            "status": "completed"
        })
        .to_string(),
    );
    activation_result.name = Some(ACTIVATE_SKILL_TOOL_NAME.to_string());
    history.push(activation_result);

    push_internal_skill_trace(
        history,
        "skill_enter",
        serde_json::json!({
            "skill_name": skill_name,
            "allowed_tools": allowed_tools,
            "goal": goal,
        }),
        &serde_json::json!({
            "skill_name": skill_name,
            "kind": "prompt",
            "allowed_tools": allowed_tools,
            "goal": goal,
        })
        .to_string(),
    );
    history.extend(trace_messages.iter().cloned());
    history.push(ChatMessage::assistant(final_response));
}

/// Compact JSON value for presentation.
fn compact_json_value(value: &serde_json::Value, depth: usize) -> serde_json::Value {
    const MAX_DEPTH: usize = 4;
    const MAX_ARRAY_ITEMS: usize = 8;
    const MAX_STRING_CHARS: usize = 400;

    if depth >= MAX_DEPTH {
        return match value {
            serde_json::Value::String(s) => serde_json::Value::String(truncate_str(s, 160)),
            serde_json::Value::Array(arr) => serde_json::json!({
                "kind": "array",
                "len": arr.len()
            }),
            serde_json::Value::Object(map) => serde_json::json!({
                "kind": "object",
                "keys": map.keys().take(12).cloned().collect::<Vec<_>>()
            }),
            other => other.clone(),
        };
    }

    match value {
        serde_json::Value::Null => serde_json::Value::Null,
        serde_json::Value::Bool(v) => serde_json::Value::Bool(*v),
        serde_json::Value::Number(v) => serde_json::Value::Number(v.clone()),
        serde_json::Value::String(s) => {
            serde_json::Value::String(truncate_str(s, MAX_STRING_CHARS))
        }
        serde_json::Value::Array(arr) => {
            let items = arr
                .iter()
                .take(MAX_ARRAY_ITEMS)
                .map(|item| compact_json_value(item, depth + 1))
                .collect::<Vec<_>>();
            if arr.len() > MAX_ARRAY_ITEMS {
                serde_json::json!({
                    "items": items,
                    "truncated": true,
                    "total": arr.len()
                })
            } else {
                serde_json::Value::Array(items)
            }
        }
        serde_json::Value::Object(map) => {
            let heavy_keys = [
                "content",
                "body",
                "html",
                "markdown",
                "raw",
                "text",
                "full_text",
            ];
            let mut result = serde_json::Map::new();

            for (key, value) in map.iter() {
                if heavy_keys.contains(&key.as_str()) {
                    match value {
                        serde_json::Value::String(s) => {
                            result.insert(
                                key.clone(),
                                serde_json::json!({
                                    "preview": truncate_str(s, 240),
                                    "truncated": s.chars().count() > 240,
                                    "length": s.chars().count()
                                }),
                            );
                        }
                        other => {
                            result.insert(key.clone(), compact_json_value(other, depth + 1));
                        }
                    }
                } else {
                    result.insert(key.clone(), compact_json_value(value, depth + 1));
                }
            }

            serde_json::Value::Object(result)
        }
    }
}

fn build_internal_skill_tool_call(
    tool_name: &str,
    arguments: serde_json::Value,
) -> ToolCallRequest {
    ToolCallRequest {
        id: format!("{}-{}", tool_name, uuid::Uuid::new_v4()),
        name: tool_name.to_string(),
        arguments,
        thought_signature: None,
    }
}

fn push_internal_skill_trace(
    history: &mut Vec<ChatMessage>,
    tool_name: &str,
    arguments: serde_json::Value,
    result: &str,
) {
    let tool_call = build_internal_skill_tool_call(tool_name, arguments);
    history.push(ChatMessage {
        id: None,
        role: "assistant".to_string(),
        content: serde_json::Value::String(String::new()),
        reasoning_content: None,
        tool_calls: Some(vec![tool_call.clone()]),
        tool_call_id: None,
        name: None,
    });

    let mut tool_result = ChatMessage::tool_result(&tool_call.id, result);
    tool_result.name = Some(tool_name.to_string());
    history.push(tool_result);
}

fn persist_prompt_skill_history(
    history: &mut Vec<ChatMessage>,
    user_input: &str,
    skill_name: &str,
    allowed_tools: &[String],
    trace_messages: &[ChatMessage],
    final_response: &str,
) {
    history.push(ChatMessage::user(user_input));
    push_internal_skill_trace(
        history,
        "skill_enter",
        serde_json::json!({
            "skill_name": skill_name,
            "allowed_tools": allowed_tools,
        }),
        &serde_json::json!({
            "skill_name": skill_name,
            "kind": "prompt",
            "allowed_tools": allowed_tools,
        })
        .to_string(),
    );
    history.extend(trace_messages.iter().cloned());
    history.push(ChatMessage::assistant(final_response));
}

#[allow(dead_code)]
fn persist_script_skill_history(
    history: &mut Vec<ChatMessage>,
    user_input: &str,
    skill_name: &str,
    internal_tool_name: &str,
    argv: &[String],
    raw_result: &str,
    final_response: &str,
) {
    history.push(ChatMessage::user(user_input));
    push_internal_skill_trace(
        history,
        internal_tool_name,
        serde_json::json!({
            "skill_name": skill_name,
            "argv": argv,
        }),
        raw_result,
    );
    history.push(ChatMessage::assistant(final_response));
}

fn find_recent_skill_name_from_history(history: &[ChatMessage]) -> Option<String> {
    HistoryProjector::new(history).analyze().latest_skill_name
}

const SESSION_ACTIVE_SKILL_NAME_KEY: &str = "active_skill_name";
const SESSION_ACTIVE_SKILL_CORRECTIONS_KEY: &str = "active_skill_correction_count";
const LEARNED_SKILL_DISABLE_THRESHOLD: u32 = 2;

fn active_skill_name_from_metadata(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get(SESSION_ACTIVE_SKILL_NAME_KEY)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn continued_skill_name(metadata: &serde_json::Value, history: &[ChatMessage]) -> Option<String> {
    active_skill_name_from_metadata(metadata)
        .or_else(|| find_recent_skill_name_from_history(history))
}

fn record_active_skill_name(metadata: &mut serde_json::Value, skill_name: &str) {
    let trimmed = skill_name.trim();
    if trimmed.is_empty() {
        return;
    }

    if !metadata.is_object() {
        *metadata = serde_json::Value::Object(serde_json::Map::new());
    }

    if let Some(map) = metadata.as_object_mut() {
        map.insert(
            SESSION_ACTIVE_SKILL_NAME_KEY.to_string(),
            serde_json::Value::String(trimmed.to_string()),
        );
        map.insert(
            SESSION_ACTIVE_SKILL_CORRECTIONS_KEY.to_string(),
            serde_json::Value::Number(0.into()),
        );
    }
}

fn disable_skill_toggle(paths: &Paths, skill_name: &str) -> Result<()> {
    let path = paths.toggles_file();
    let mut store = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .unwrap_or_else(|| serde_json::json!({"skills": {}, "tools": {}}));
    if !store.is_object() {
        store = serde_json::json!({"skills": {}, "tools": {}});
    }
    if store
        .get("skills")
        .and_then(|value| value.as_object())
        .is_none()
    {
        store["skills"] = serde_json::json!({});
    }
    store["skills"][skill_name] = serde_json::json!(false);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&store)?)?;
    Ok(())
}

fn suppress_prompt_reinjection_for_continued_skill(
    mut active_skill: ActiveSkillContext,
    continued_skill_name: Option<&str>,
) -> ActiveSkillContext {
    if continued_skill_name == Some(active_skill.name.as_str()) {
        active_skill.inject_prompt_md = false;
    }
    active_skill
}

fn apply_skill_fallback_response(final_response: String, fallback_message: Option<&str>) -> String {
    let trimmed_response = final_response.trim();
    if !trimmed_response.is_empty() {
        return trimmed_response.to_string();
    }

    fallback_message
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string)
        .unwrap_or_default()
}

struct PromptSkillLoopOutput {
    final_response: String,
    trace_messages: Vec<ChatMessage>,
}

fn resolve_skill_run_mode(msg: &InboundMessage) -> SkillRunMode {
    match msg
        .metadata
        .get("skill_run_mode")
        .and_then(|value| value.as_str())
    {
        Some("test") => SkillRunMode::Test,
        Some("cron") => SkillRunMode::Cron,
        Some("chat") => SkillRunMode::Chat,
        _ if msg.channel == "cron" => SkillRunMode::Cron,
        _ if msg
            .metadata
            .get("skill_test")
            .and_then(|value| value.as_bool())
            .unwrap_or(false) =>
        {
            SkillRunMode::Test
        }
        _ => SkillRunMode::Chat,
    }
}

fn resolve_cron_deliver_target(msg: &InboundMessage) -> Option<(String, String)> {
    if resolve_skill_run_mode(msg) != SkillRunMode::Cron {
        return None;
    }

    if !msg
        .metadata
        .get("deliver")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return None;
    }

    let channel = msg
        .metadata
        .get("deliver_channel")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let to = msg
        .metadata
        .get("deliver_to")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;

    Some((channel.to_string(), to.to_string()))
}

fn expand_history_stubs_with_cache(
    response_cache: &crate::response_cache::ResponseCache,
    session_key: &str,
    history: &[ChatMessage],
) -> Vec<ChatMessage> {
    history
        .iter()
        .map(|msg| {
            let content_str = msg.content.as_str().unwrap_or("");
            if content_str.contains("ref:") {
                if let Some(ref_pos) = content_str.find("ref:") {
                    let after = &content_str[ref_pos + 4..];
                    let ref_id: String = after
                        .chars()
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect();
                    if !ref_id.is_empty() {
                        if let Some(full) = response_cache.recall(session_key, &ref_id) {
                            let mut expanded = msg.clone();
                            expanded.content = serde_json::Value::String(full);
                            return expanded;
                        }
                    }
                }
            }
            msg.clone()
        })
        .collect()
}

fn parse_spawn_task_forced_skill_request(task: &str) -> Option<(String, String)> {
    let trimmed = task.trim();
    if trimmed.is_empty() {
        return None;
    }

    let regex = Regex::new(
        r"(?i)(?:使用(?:已安装的)?|用|调用|执行|use|using|run|call)\s*([A-Za-z0-9_.@-]+)\s*(?:技能|skill)\s*[：:\-，,]?\s*(.*)",
    )
    .ok()?;

    let captures = regex.captures(trimmed)?;
    let skill_name = captures.get(1)?.as_str().trim().to_string();
    if skill_name.is_empty() {
        return None;
    }
    let remainder = captures
        .get(2)
        .map(|m| m.as_str().trim())
        .filter(|text| !text.is_empty())
        .unwrap_or(trimmed)
        .to_string();

    Some((skill_name, remainder))
}

fn normalize_spawn_task(task: &str) -> String {
    if let Some((skill_name, user_query)) = parse_spawn_task_forced_skill_request(task) {
        format!("__SKILL_EXEC__:{}:{}", skill_name, user_query)
    } else {
        task.to_string()
    }
}

/// Prepare skill result for presentation.
#[allow(dead_code)]
struct SkillResultPresentation {
    direct_text: Option<String>,
    llm_payload: Option<String>,
    fallback_text: String,
}

#[allow(dead_code)]
fn prepare_skill_result_for_presentation(
    skill_name: &str,
    output: &str,
) -> SkillResultPresentation {
    let raw_fallback = format!(
        "[{}] 定时任务执行完成:\n\n{}",
        skill_name,
        truncate_str(output, 4000)
    );

    let parsed: serde_json::Value = match serde_json::from_str(output) {
        Ok(value) => value,
        Err(_) => {
            return SkillResultPresentation {
                direct_text: None,
                llm_payload: Some(truncate_str(output, 4000)),
                fallback_text: raw_fallback,
            };
        }
    };

    let Some(obj) = parsed.as_object() else {
        return SkillResultPresentation {
            direct_text: None,
            llm_payload: Some(truncate_str(output, 4000)),
            fallback_text: raw_fallback,
        };
    };

    let direct_text = obj
        .get("display_text")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let instruction = obj
        .get("instruction")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or("请把结果整理成清晰、简洁、用户可读的回复，不要编造未提供的信息。");

    let llm_source = if let Some(summary) = obj.get("summary_data") {
        serde_json::json!({
            "instruction": instruction,
            "summary_data": compact_json_value(summary, 0)
        })
    } else {
        let mut compact = serde_json::Map::new();
        for (key, value) in obj {
            if key == "raw_data" {
                continue;
            }
            compact.insert(key.clone(), compact_json_value(value, 0));
        }
        serde_json::Value::Object(compact)
    };

    let llm_payload =
        serde_json::to_string_pretty(&llm_source).unwrap_or_else(|_| truncate_str(output, 4000));

    let fallback_text = if let Some(text) = direct_text.as_ref() {
        text.clone()
    } else if let Some(summary) = obj.get("summary_data") {
        let compact = serde_json::to_string_pretty(&compact_json_value(summary, 0))
            .unwrap_or_else(|_| "{}".to_string());
        format!(
            "[{}] 定时任务执行完成（摘要整理失败，以下为结构化摘要）:\n\n{}",
            skill_name,
            truncate_str(&compact, 4000)
        )
    } else {
        raw_fallback
    };

    SkillResultPresentation {
        direct_text,
        llm_payload: Some(truncate_str(&llm_payload, 16000)),
        fallback_text,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MainSessionTarget {
    channel: String,
    account_id: Option<String>,
    chat_id: String,
    session_key: String,
}

#[derive(Clone)]
struct RuntimeSystemEventEmitter {
    store: InMemorySystemEventStore,
}

impl SystemEventEmitter for RuntimeSystemEventEmitter {
    fn emit(&self, event: SystemEvent) {
        self.store.emit(event);
    }
}

fn is_main_session_candidate(msg: &InboundMessage) -> bool {
    if matches!(
        msg.channel.as_str(),
        "system" | "cron" | "subagent" | "ghost"
    ) {
        return false;
    }
    if matches!(msg.sender_id.as_str(), "system" | "cron") {
        return false;
    }
    if msg
        .metadata
        .get("cancel")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    true
}

fn render_system_notification_text(request: &NotificationRequest) -> String {
    match request.priority {
        EventPriority::Critical => format!("🚨 {}\n{}", request.title, request.body),
        EventPriority::High => format!("⚠️ {}\n{}", request.title, request.body),
        _ => format!("ℹ️ {}\n{}", request.title, request.body),
    }
}

fn render_session_summary_text(summary: &SessionSummary) -> String {
    if summary.compact_text.trim().is_empty() {
        summary.title.clone()
    } else {
        format!("🗂️ {}\n{}", summary.title, summary.compact_text)
    }
}

fn is_im_channel(channel: &str) -> bool {
    matches!(
        channel,
        "wecom" | "feishu" | "lark" | "telegram" | "slack" | "discord" | "dingtalk" | "whatsapp"
    )
}

fn resolve_routed_agent_id(metadata: &serde_json::Value) -> Option<String> {
    metadata
        .get("route_agent_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string())
}

fn build_subagent_metadata(agent_id: Option<&str>) -> serde_json::Value {
    match agent_id.map(str::trim).filter(|id| !id.is_empty()) {
        Some(agent_id) => serde_json::json!({
            "route_agent_id": agent_id,
        }),
        None => serde_json::Value::Null,
    }
}

fn parse_structured_skill_task(task: &str) -> Option<(&str, &str)> {
    let rest = task.strip_prefix("__SKILL_EXEC__:")?;
    let (skill_name, user_query) = rest.split_once(':')?;
    let skill_name = skill_name.trim();
    if skill_name.is_empty() {
        return None;
    }
    Some((skill_name, user_query))
}

fn build_subagent_inbound_message(
    task: &str,
    origin_channel: &str,
    origin_chat_id: &str,
    base_metadata: &serde_json::Value,
    session_key: &str,
) -> InboundMessage {
    let mut metadata = if let Some(obj) = base_metadata.as_object() {
        serde_json::Value::Object(obj.clone())
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "subagent_session_key".to_string(),
            serde_json::json!(session_key),
        );

        if let Some((skill_name, _)) = parse_structured_skill_task(task) {
            obj.insert(
                "forced_skill_name".to_string(),
                serde_json::json!(skill_name),
            );
        }
    }

    let content = parse_structured_skill_task(task)
        .map(|(_, user_query)| user_query.to_string())
        .unwrap_or_else(|| task.to_string());

    InboundMessage {
        channel: origin_channel.to_string(),
        account_id: None,
        sender_id: "system".to_string(),
        chat_id: origin_chat_id.to_string(),
        content,
        media: vec![],
        metadata,
        timestamp_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn global_core_tool_names() -> Vec<String> {
    blockcell_tools::registry::global_core_tool_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

fn normalize_ghost_memory_provider_tool_schema(
    schema: serde_json::Value,
) -> Option<serde_json::Value> {
    if schema.get("type").and_then(|value| value.as_str()) == Some("function") {
        let name = schema
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(|value| value.as_str())?;
        if !name.trim().is_empty() {
            return Some(schema);
        }
        return None;
    }

    let name = schema.get("name").and_then(|value| value.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    let description = schema
        .get("description")
        .and_then(|value| value.as_str())
        .unwrap_or("Ghost memory provider tool.");
    let parameters = schema
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}}));

    Some(serde_json::json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    }))
}

fn ghost_memory_provider_tool_schemas(
    manager: Option<&crate::ghost_memory_provider::GhostMemoryProviderManager>,
    disabled_tools: &HashSet<String>,
) -> Vec<serde_json::Value> {
    manager
        .map(|manager| {
            manager
                .get_all_tool_schemas()
                .into_iter()
                .filter_map(normalize_ghost_memory_provider_tool_schema)
                .filter(|schema| {
                    let name = schema
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(|value| value.as_str())
                        .unwrap_or_default();
                    !disabled_tools.contains(name)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn resolve_effective_tool_names(
    config: &Config,
    mode: InteractionMode,
    agent_id: Option<&str>,
    active_skill: Option<&ActiveSkillContext>,
    intents: &[IntentCategory],
    available_tools: &HashSet<String>,
) -> Vec<String> {
    // 1. 先检查 intent_router.enabled
    let router_enabled = config
        .intent_router
        .as_ref()
        .map(|r| r.enabled)
        .unwrap_or(true);

    if !router_enabled {
        // 2. enabled=false 时，检查 load_all_tools
        let load_all = config
            .intent_router
            .as_ref()
            .map(|r| r.load_all_tools)
            .unwrap_or(false);

        if load_all {
            // 全量加载模式：返回所有可用工具（扣除 deny_tools）
            let mut tool_names: Vec<String> = available_tools.iter().cloned().collect();
            // 应用 deny_tools 过滤
            if let Some(router) = config.intent_router.as_ref() {
                let profile_id = config.resolve_intent_profile_id(agent_id);
                if let Some(profile_id) = profile_id {
                    if let Some(profile) = router.profiles.get(&profile_id) {
                        for tool in &profile.deny_tools {
                            tool_names.retain(|name| name != tool);
                        }
                    } else {
                        warn!(
                            profile_id = %profile_id,
                            "Profile not found in load_all_tools mode, deny_tools filtering skipped"
                        );
                    }
                }
            }
            // 应用 napcat 过滤
            if !config.channels.napcat.enabled {
                tool_names.retain(|name| !name.starts_with("napcat_"));
            }
            // 应用 skill 工具（如果有 active skill）
            if let Some(skill) = active_skill {
                tool_names.extend(skill.tools.iter().cloned());
            }
            tool_names.sort();
            tool_names.dedup();
            return tool_names;
        }
        // load_all_tools=false: 走 Unknown profile（原有逻辑会处理）
    }

    // enabled=true 或 load_all_tools=false: 原有意图分类逻辑不变
    let mut tool_names = global_core_tool_names();

    let mut profile_tools = match mode {
        InteractionMode::Chat => {
            resolve_profile_tool_names(config, agent_id, &[IntentCategory::Chat], available_tools)
        }
        InteractionMode::General | InteractionMode::Skill => {
            resolve_profile_tool_names(config, agent_id, intents, available_tools)
        }
    };

    tool_names.append(&mut profile_tools);

    if let Some(skill) = active_skill {
        tool_names.extend(skill.tools.iter().cloned());
    }

    // Filter by available tools (registry)
    tool_names.retain(|name| available_tools.contains(name));

    // Filter napcat tools by config enabled state
    if !config.channels.napcat.enabled {
        tool_names.retain(|name| !name.starts_with("napcat_"));
    }

    tool_names.sort();
    tool_names.dedup();
    tool_names
}

fn resolve_profile_tool_names(
    config: &Config,
    agent_id: Option<&str>,
    intents: &[IntentCategory],
    available_tools: &HashSet<String>,
) -> Vec<String> {
    IntentToolResolver::new(config)
        .resolve_tool_names(agent_id, intents, Some(available_tools))
        .unwrap_or_default()
}

// scoped_tool_denied_result moved to crate::error

fn normalize_path_for_check(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            Component::Normal(seg) => normalized.push(seg),
        }
    }
    normalized
}

fn canonical_or_normalized(path: &Path) -> PathBuf {
    path.canonicalize()
        .unwrap_or_else(|_| normalize_path_for_check(path))
}

fn is_path_within_base(base: &Path, candidate: &Path) -> bool {
    let base_norm = canonical_or_normalized(base);
    let candidate_norm = canonical_or_normalized(candidate);
    candidate_norm.starts_with(&base_norm)
}

fn tool_result_indicates_error(result: &str) -> bool {
    if result.starts_with("Tool error:")
        || result.starts_with("Error:")
        || result.starts_with("Validation error:")
        || result.starts_with("Config error:")
        || result.starts_with("Permission denied:")
    {
        return true;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(result) {
        if value.get("error").is_some() {
            return true;
        }
        if value.get("status").and_then(|v| v.as_str()) == Some("error") {
            return true;
        }
    }

    false
}

fn should_supplement_tool_schema(result: &str) -> bool {
    let lower = result.to_ascii_lowercase();
    lower.contains("unknown tool:")
        || lower.contains("validation error:")
        || lower.contains("config error:")
        || lower.contains("missing required parameter")
        || lower.contains("' is required for")
}

#[derive(Debug, Clone)]
struct InteractionDecision {
    active_skill: Option<ActiveSkillContext>,
    chat_intents: Vec<IntentCategory>,
    mode: InteractionMode,
}

struct FinalResponseContext<'a> {
    msg: &'a InboundMessage,
    persist_session_key: &'a str,
    history: &'a mut [ChatMessage],
    session_metadata: &'a serde_json::Value,
    final_response: &'a str,
    collected_media: Vec<String>,
    cron_deliver_target: Option<(String, String)>,
}

#[cfg(test)]
fn determine_interaction_mode(
    has_active_skill: bool,
    chat_intents: &[IntentCategory],
) -> InteractionMode {
    if has_active_skill {
        return InteractionMode::Skill;
    }

    if chat_intents.len() == 1 && matches!(chat_intents[0], IntentCategory::Chat) {
        return InteractionMode::Chat;
    }

    InteractionMode::General
}

fn user_wants_send_image(text: &str) -> bool {
    let t = text.to_lowercase();
    let has_send =
        t.contains("发") || t.contains("发送") || t.contains("发给") || t.contains("send");
    let has_image = t.contains("图片")
        || t.contains("照片")
        || t.contains("相片")
        || t.contains("截图")
        || t.contains("图像")
        || t.contains("image")
        || t.contains("photo");
    has_send && has_image
}

fn chat_message_text(msg: &ChatMessage) -> String {
    match &msg.content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

#[derive(Clone)]
struct RuntimeSessionSearch {
    paths: Paths,
    current_session_key: Option<String>,
}

impl RuntimeSessionSearch {
    fn new(paths: Paths, current_session_key: Option<String>) -> Self {
        Self {
            paths,
            current_session_key,
        }
    }
}

impl SessionSearchOps for RuntimeSessionSearch {
    fn search_session_json(&self, query: &str, limit: usize) -> Result<serde_json::Value> {
        let tokens = normalize_runtime_session_search_tokens(query);
        if tokens.is_empty() {
            return Ok(serde_json::json!({
                "query": query,
                "count": 0,
                "results": []
            }));
        }

        let mut results = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.paths.sessions_dir()) else {
            return Ok(serde_json::json!({
                "query": query,
                "count": 0,
                "results": []
            }));
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("jsonl") {
                continue;
            }
            let session_key = path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(|stem| stem.replace('_', ":"))
                .unwrap_or_else(|| "unknown".to_string());
            let Ok(file) = std::fs::File::open(&path) else {
                continue;
            };
            for line in BufReader::new(file).lines().map_while(|line| line.ok()) {
                let Ok(message) = serde_json::from_str::<ChatMessage>(&line) else {
                    continue;
                };
                if !matches!(message.role.as_str(), "user" | "assistant") {
                    continue;
                }
                let text = chat_message_text(&message);
                let score = runtime_session_search_score(&text, &tokens);
                if score == 0 {
                    continue;
                }
                let current_boost = self
                    .current_session_key
                    .as_ref()
                    .is_some_and(|current| current == &session_key)
                    as usize;
                results.push((
                    score,
                    current_boost,
                    session_key.clone(),
                    message.role,
                    truncate_runtime_session_search_text(&text, 500),
                ));
            }
        }

        results.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        results.truncate(limit.clamp(1, 20));
        Ok(serde_json::json!({
            "query": query,
            "count": results.len(),
            "results": results
                .into_iter()
                .map(|(score, _current_boost, session_key, role, text)| serde_json::json!({
                    "score": score,
                    "sessionKey": session_key,
                    "role": role,
                    "text": text,
                }))
                .collect::<Vec<_>>()
        }))
    }
}

fn normalize_runtime_session_search_tokens(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|token| token.trim().to_lowercase())
        .filter(|token| token.len() >= 2)
        .collect()
}

fn runtime_session_search_score(text: &str, tokens: &[String]) -> usize {
    let lower = text.to_lowercase();
    tokens
        .iter()
        .map(|token| {
            if lower.contains(token) {
                token.len()
            } else {
                0
            }
        })
        .sum()
}

fn truncate_runtime_session_search_text(text: &str, max_chars: usize) -> String {
    let truncated = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        format!("{truncated}...")
    } else {
        truncated
    }
}

async fn pick_image_path(paths: &Paths, history: &[ChatMessage]) -> Option<String> {
    let re_abs = Regex::new(r#"(/[^\s`"']+\.(?i:jpg|jpeg|png|gif|webp|bmp))"#).ok()?;
    let re_name = Regex::new(r#"([A-Za-z0-9._-]+\.(?i:jpg|jpeg|png|gif|webp|bmp))"#).ok()?;

    let media_dir = paths.media_dir();

    for msg in history.iter().rev() {
        let text = chat_message_text(msg);

        for cap in re_abs.captures_iter(&text) {
            let p = cap.get(1)?.as_str().to_string();
            if tokio::fs::metadata(&p).await.is_ok() {
                // 使用异步 canonicalize 避免阻塞 tokio runtime
                let cp = tokio::fs::canonicalize(&p).await.ok()?;
                let md = tokio::fs::canonicalize(&media_dir).await.ok()?;
                if cp.starts_with(md) {
                    return Some(p);
                }
            }
        }

        for cap in re_name.captures_iter(&text) {
            let file_name = cap.get(1)?.as_str();
            let p = media_dir.join(file_name);
            if tokio::fs::metadata(&p).await.is_ok() {
                return Some(p.display().to_string());
            }
        }
    }

    let mut rd = tokio::fs::read_dir(&media_dir).await.ok()?;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp"
        ) {
            return Some(p.display().to_string());
        }
    }

    None
}

/// Strip fake tool call blocks from LLM responses.
/// Some LLMs output pseudo-tool-call syntax in plain text instead of using the
/// real function calling mechanism. Remove these before sending to user.
fn strip_fake_tool_calls(text: &str) -> String {
    let mut result = text.to_string();

    // Remove [TOOL_CALL]...[/TOOL_CALL] blocks (case-insensitive)
    while let Some(start) = result.to_lowercase().find("[tool_call]") {
        if let Some(end_tag) = result.to_lowercase()[start..].find("[/tool_call]") {
            let end = start + end_tag + "[/tool_call]".len();
            result = format!("{}{}", &result[..start], &result[end..]);
        } else {
            // No closing tag — remove from [TOOL_CALL] to end
            result = result[..start].to_string();
            break;
        }
    }

    // Remove ```tool_call...``` blocks
    while let Some(start) = result.find("```tool_call") {
        if let Some(end_tag) = result[start + 3..].find("```") {
            let end = start + 3 + end_tag + 3;
            result = format!("{}{}", &result[..start], &result[end..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }

    result.trim().to_string()
}

fn is_tool_trace_content(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    t.contains("[Called:")
        || t.contains("<tool_call")
        || t.contains("[TOOL_CALL]")
        || t.contains("[/TOOL_CALL]")
}

/// Detect if a web_search result is "thin" — only contains titles/URLs with no actual content.
/// This happens when the search engine returns page titles but the snippets are empty or near-empty.
/// In this case the LLM should be directed to web_fetch specific URLs instead of giving up.
fn is_thin_search_result(raw: &str) -> bool {
    let val: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let results = match val.get("results").and_then(|v| v.as_array()) {
        Some(r) => r,
        None => return false,
    };
    if results.is_empty() {
        return false;
    }
    // Count results that have meaningful snippet content (>30 chars)
    let rich_count = results
        .iter()
        .filter(|r| {
            let snippet = r
                .get("snippet")
                .and_then(|v| v.as_str())
                .or_else(|| r.get("description").and_then(|v| v.as_str()))
                .unwrap_or("");
            snippet.chars().count() > 30
        })
        .count();
    // Thin if fewer than half the results have meaningful snippets
    rich_count * 2 < results.len()
}

/// Extract URLs from a web_search result JSON (top 3 results).
fn extract_urls_from_search_result(raw: &str) -> Vec<String> {
    let val: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let results = match val.get("results").and_then(|v| v.as_array()) {
        Some(r) => r,
        None => return vec![],
    };
    results
        .iter()
        .filter_map(|r| r.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .filter(|u| !u.is_empty())
        .take(3)
        .collect()
}

fn is_dangerous_exec_command(command: &str) -> bool {
    let c = command.to_lowercase();
    let c = c.trim();
    if c.is_empty() {
        return false;
    }

    let direct_patterns = [
        r"(^|[;&|]\s*|\b(?:sudo|env)\s+)(?:rm|trash|unlink)\b",
        r"(^|[;&|]\s*|\b(?:sudo|env)\s+)rmdir\b",
        r"\bfind\b[\s\S]*\s-delete\b",
        r"\bfind\b[\s\S]*\s-exec\s+rm\b",
        r#"\bsh\s+-c\s+['"][^'"]*\brm\b"#,
        r#"\bbash\s+-c\s+['"][^'"]*\brm\b"#,
        r#"\bzsh\s+-c\s+['"][^'"]*\brm\b"#,
        r"\bpython(?:3)?\b[\s\S]*\b(?:shutil\.rmtree|os\.remove|os\.unlink|os\.rmdir)\b",
        r"\bperl\b[\s\S]*\bunlink\b",
    ];
    for pattern in direct_patterns {
        if let Ok(re) = Regex::new(pattern) {
            if re.is_match(c) {
                return true;
            }
        }
    }

    if let Ok(rm_re) = Regex::new(r"(^|[;&|]\s*|\b(?:sudo|env)\s+)rm\b([^;&|]*)") {
        for caps in rm_re.captures_iter(c) {
            let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            let has_recursive = suffix.contains(" -r")
                || suffix.contains(" -rf")
                || suffix.contains(" -fr")
                || suffix.starts_with("-r")
                || suffix.starts_with("-rf")
                || suffix.starts_with("-fr");
            let has_force = suffix.contains(" -f")
                || suffix.contains(" -rf")
                || suffix.contains(" -fr")
                || suffix.starts_with("-f")
                || suffix.starts_with("-rf")
                || suffix.starts_with("-fr");
            let has_target = suffix
                .split_whitespace()
                .any(|token| !token.starts_with('-') && !token.is_empty());
            if has_target && (has_recursive || has_force) {
                return true;
            }
            if has_target && suffix.contains("../") {
                return true;
            }
        }
    }

    let dangerous = [
        "kill ",
        "pkill",
        "killall",
        "taskkill",
        "systemctl stop",
        "service stop",
        "launchctl bootout",
        "launchctl kill",
    ];

    dangerous.iter().any(|p| c.contains(p))
}

fn is_sensitive_filename(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let name = p.rsplit('/').next().unwrap_or("").to_lowercase();
    matches!(
        name.as_str(),
        "config.json5" | "config.json" | "config.toml" | "config.yaml" | "config.yml"
    )
}

fn user_explicitly_confirms_dangerous_op(user_text: &str) -> bool {
    let t = user_text.trim();
    if t.is_empty() {
        return false;
    }

    // For channels without an interactive confirm prompt (confirm_tx=None),
    // require the user to explicitly confirm in text.
    // Keep this simple and language-friendly.
    t.contains("确认")
        && (t.contains("执行") || t.contains("重启") || t.contains("继续") || t.contains("允许"))
}

fn overwrite_last_assistant_message(history: &mut [ChatMessage], new_text: &str) {
    if let Some(last) = history.last_mut() {
        if last.role == "assistant" {
            last.content = serde_json::Value::String(new_text.to_string());
        }
    }
}

/// Load (or initialise) the path-access policy from the location specified
/// in `config.security.path_access`.
///
/// Side-effect: writes the default template to disk if the file doesn't exist
/// and the configured path matches the standard `~/.blockcell/path_access.json5`
/// location, so first-time users get a ready-to-edit example.
fn load_path_policy(config: &Config, paths: &Paths) -> PathPolicy {
    use blockcell_core::path_policy::{default_policy_template, expand_tilde};

    let pa = &config.security.path_access;
    if !pa.enabled {
        return PathPolicy::safe_default();
    }

    // Resolve the configured policy-file path (supports ~/ expansion)
    let policy_path = if pa.policy_file.trim().is_empty() {
        paths.path_access_file()
    } else {
        expand_tilde(pa.policy_file.trim())
    };

    // Bootstrap: if the file doesn't exist, write the starter template
    if !policy_path.exists() {
        if let Some(parent) = policy_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&policy_path, default_policy_template()) {
            warn!(path = %policy_path.display(), error = %e, "Failed to write default path_access.json5 template");
        } else {
            info!(path = %policy_path.display(), "Wrote default path_access.json5 template");
        }
    }

    PathPolicy::load(&policy_path)
}

/// Read toggles.json and return the set of disabled item names for a category.
/// Returns an empty set if the file doesn't exist or can't be parsed.
fn load_disabled_toggles(paths: &Paths, category: &str) -> HashSet<String> {
    let path = paths.toggles_file();
    let mut disabled = HashSet::new();
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = val.get(category).and_then(|v| v.as_object()) {
                for (name, enabled) in obj {
                    if enabled == false {
                        disabled.insert(name.clone());
                    }
                }
            }
        }
    }
    disabled
}

pub struct AgentRuntime {
    config: Config,
    paths: Paths,
    context_builder: ContextBuilder,
    provider_pool: Arc<ProviderPool>,
    tool_registry: ToolRegistry,
    session_store: SessionStore,
    audit_logger: AuditLogger,
    outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
    inbound_tx: Option<mpsc::Sender<InboundMessage>>,
    confirm_tx: Option<mpsc::Sender<ConfirmRequest>>,
    /// Directories that the user has already authorized access to.
    /// Files within these directories will not require separate confirmation.
    authorized_dirs: HashSet<PathBuf>,
    /// Shared task manager for tracking background subagent tasks.
    task_manager: TaskManager,
    /// Agent id bound to this runtime.
    agent_id: Option<String>,
    /// Shared memory store handle for tools.
    memory_store: Option<MemoryStoreHandle>,
    memory_file_store: Option<blockcell_tools::MemoryFileStoreHandle>,
    ghost_memory_lifecycle: Option<Arc<crate::ghost_memory_provider::GhostMemoryProviderManager>>,
    skill_file_store: Option<blockcell_tools::SkillFileStoreHandle>,
    /// Capability registry handle for tools.
    capability_registry: Option<CapabilityRegistryHandle>,
    /// Core evolution engine handle for tools.
    core_evolution: Option<CoreEvolutionHandle>,
    /// Broadcast sender for streaming events to WebSocket clients (gateway mode).
    event_tx: Option<broadcast::Sender<String>>,
    /// In-memory store for structured system events emitted by runtime producers.
    system_event_store: InMemorySystemEventStore,
    /// Tick orchestrator for system event delivery.
    system_event_orchestrator: SystemEventOrchestrator,
    /// Shared emitter handle used by tools, task manager, and schedulers.
    system_event_emitter: EventEmitterHandle,
    /// Last interactive main-session target for summary / notification delivery.
    main_session_target: Option<MainSessionTarget>,
    /// Cooldown tracker: capability_id → last auto-request timestamp (epoch secs).
    /// Prevents repeated auto-triggering of the same capability within 24h.
    cap_request_cooldown: HashMap<String, i64>,
    /// Persistent registry of known channel contacts for cross-channel messaging.
    channel_contacts: blockcell_storage::ChannelContacts,
    /// Loaded path-access policy engine (from `~/.blockcell/path_access.json5`).
    path_policy: PathPolicy,
    /// Per-session cache for large list/table responses (prevents history token explosion).
    response_cache: crate::response_cache::ResponseCache,
    /// 7-Layer Memory System integration.
    memory_system: Option<crate::memory_system::MemorySystem>,
    /// Flag to signal that memory injector cache needs refresh after Layer 5 extraction.
    /// Uses Arc<AtomicBool> because background tasks need to set this flag.
    memory_injector_needs_reload: Arc<std::sync::atomic::AtomicBool>,
    /// Unified learning coordinator — replaces scattered skill_nudge_engine + ghost_policy calls
    learning_coordinator: Arc<crate::learning_coordinator::LearningCoordinator>,
    /// Skill 操作互斥锁 — 防止 Skill 并发修改冲突
    #[allow(deprecated)]
    skill_mutex: crate::skill_mutex::SkillMutex,
    /// Unified write guard for coordinated write protection across memory + skill files
    write_guard: Arc<crate::write_guard::WriteGuard>,
}

impl AgentRuntime {
    pub fn new(
        config: Config,
        paths: Paths,
        provider_pool: Arc<ProviderPool>,
        tool_registry: ToolRegistry,
    ) -> Result<Self> {
        let mut context_builder = ContextBuilder::new(paths.clone(), config.clone());

        // 默认使用 pool 中第一个可用 provider 作为 evolution provider
        // 可以通过 set_evolution_provider() 方法覆盖
        if let Some((_, p)) = provider_pool.acquire() {
            let llm_adapter = Arc::new(ProviderLLMAdapter { provider: p });
            context_builder.set_evolution_llm_provider(llm_adapter);
            info!("🧠 [自进化] Evolution LLM provider wired from provider pool");
        } else {
            warn!("🧠 [自进化] Failed to acquire provider from pool for evolution — evolution pipeline will not auto-drive");
        }

        let session_store = SessionStore::new(paths.clone());
        let audit_logger = AuditLogger::new(paths.clone());
        let channel_contacts = blockcell_storage::ChannelContacts::new(paths.clone());
        let path_policy = load_path_policy(&config, &paths);
        let system_event_store = InMemorySystemEventStore::default();
        let summary_queue = MainSessionSummaryQueue::with_policy(
            5,
            config.tools.tick_interval_secs.clamp(10, 300) as i64 * 1000,
        );
        let system_event_orchestrator =
            SystemEventOrchestrator::new(system_event_store.clone(), summary_queue.clone());
        let system_event_emitter: EventEmitterHandle = Arc::new(RuntimeSystemEventEmitter {
            store: system_event_store.clone(),
        });
        let ghost_memory_lifecycle = Arc::new(
            crate::ghost_memory_provider::GhostMemoryProviderManager::with_local_file(
                paths.clone(),
            ),
        );
        ghost_memory_lifecycle.initialize_all("runtime", "primary");

        // 构建 Skill 索引摘要并注入到系统提示词
        let skills_dir = paths.skills_dir();
        if skills_dir.exists() {
            let index = crate::skill_index::SkillIndex::build_from_dir(&skills_dir);
            if !index.entries().is_empty() {
                context_builder.set_skill_index_summary(index.to_prompt_summary());
            }
        }

        // 从 config 中提取 nudge 配置 (在 config 被 move 之前)
        let nudge_config = crate::skill_nudge::NudgeConfig::from_config(&config.self_improve.nudge);

        let response_cache_config =
            crate::response_cache::ResponseCacheConfig::from(&config.memory.memory_system.layer1);

        // Extract config values before config is moved into Self
        let ghost_learning_enabled = config.agents.ghost.learning.enabled;
        let self_improve_review_enabled = config.self_improve.review.enabled;
        let ghost_learning_config = config.agents.ghost.learning.clone();

        // Create unified write guard for coordinated write protection
        let write_guard = Arc::new(crate::write_guard::WriteGuard::new(paths.base.clone()));

        Ok(Self {
            config,
            paths,
            context_builder,
            provider_pool,
            tool_registry,
            session_store,
            audit_logger,
            outbound_tx: None,
            inbound_tx: None,
            confirm_tx: None,
            authorized_dirs: HashSet::new(),
            task_manager: TaskManager::new(),
            agent_id: None,
            memory_store: None,
            memory_file_store: None,
            ghost_memory_lifecycle: Some(ghost_memory_lifecycle),
            skill_file_store: None,
            capability_registry: None,
            core_evolution: None,
            event_tx: None,
            system_event_store,
            system_event_orchestrator,
            system_event_emitter,
            main_session_target: None,
            cap_request_cooldown: HashMap::new(),
            channel_contacts,
            path_policy,
            response_cache: crate::response_cache::ResponseCache::with_config(
                response_cache_config,
            ),
            memory_system: None,
            memory_injector_needs_reload: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            learning_coordinator: Arc::new({
                let nudge_engine = crate::skill_nudge::SkillNudgeEngine::new(nudge_config);
                let ghost_policy =
                    crate::ghost_learning::GhostLearningPolicy::from_config(&ghost_learning_config);
                let throttle = crate::learning_throttle::LearningThrottle::new(2, 300);
                let dedup = crate::learning_dedup::LearningDedup::new(600);
                crate::learning_coordinator::LearningCoordinator::new(
                    nudge_engine,
                    ghost_policy,
                    throttle,
                    dedup,
                    ghost_learning_enabled,
                    self_improve_review_enabled,
                )
            }),
            #[allow(deprecated)]
            skill_mutex: crate::skill_mutex::SkillMutex::new(),
            write_guard,
        })
    }

    /// Build permissions for tool execution based on channel, sender, and chat context.
    ///
    /// This method grants appropriate permissions based on:
    /// - Channel type (napcat, telegram, discord, etc.)
    /// - User whitelist membership
    /// - Admin status
    fn build_tool_permissions(
        &self,
        channel: &str,
        sender_id: Option<&str>,
        chat_id: &str,
    ) -> blockcell_core::types::PermissionSet {
        use blockcell_core::types::PermissionSet;

        let mut perms = PermissionSet::new();

        // Grant channel-specific permissions
        match channel {
            "napcat" => {
                // Use NapCat-specific permission builder
                #[cfg(feature = "napcat")]
                {
                    perms = blockcell_tools::napcat::build_napcat_user_permissions(
                        &self.config.channels.napcat,
                        sender_id,
                        chat_id,
                    );
                }
                #[cfg(not(feature = "napcat"))]
                {
                    _ = (sender_id, chat_id); // Suppress unused variable warning
                    perms = perms.with_permission("channel:napcat");
                }
            }
            "telegram" => {
                perms = perms.with_permission("channel:telegram");
                // Grant basic tool access for telegram users
                perms = perms.with_permission("telegram:tools");
            }
            "discord" => {
                perms = perms.with_permission("channel:discord");
                perms = perms.with_permission("discord:tools");
            }
            "slack" => {
                perms = perms.with_permission("channel:slack");
                perms = perms.with_permission("slack:tools");
            }
            "feishu" | "lark" => {
                perms = perms.with_permission(&format!("channel:{}", channel));
                perms = perms.with_permission("feishu:tools");
            }
            "wecom" => {
                perms = perms.with_permission("channel:wecom");
                perms = perms.with_permission("wecom:tools");
            }
            "dingtalk" => {
                perms = perms.with_permission("channel:dingtalk");
                perms = perms.with_permission("dingtalk:tools");
            }
            "whatsapp" => {
                perms = perms.with_permission("channel:whatsapp");
                perms = perms.with_permission("whatsapp:tools");
            }
            "cli" => {
                // CLI mode gets full permissions
                perms = perms.with_permission("channel:cli");
                perms = perms.with_permission("cli:tools");
            }
            _ => {
                // Unknown channel - grant basic access
                perms = perms.with_permission(&format!("channel:{}", channel));
            }
        }

        perms
    }

    pub fn context_builder(&self) -> &ContextBuilder {
        &self.context_builder
    }

    pub fn set_outbound(&mut self, tx: mpsc::Sender<OutboundMessage>) {
        self.outbound_tx = Some(tx);
    }

    pub fn set_inbound(&mut self, tx: mpsc::Sender<InboundMessage>) {
        self.inbound_tx = Some(tx);
    }

    pub fn set_confirm(&mut self, tx: mpsc::Sender<ConfirmRequest>) {
        self.confirm_tx = Some(tx);
    }

    /// Get a reference to the task manager.
    pub fn task_manager(&self) -> &TaskManager {
        &self.task_manager
    }

    /// Set a shared task manager (e.g. from the command layer).
    pub fn set_task_manager(&mut self, tm: TaskManager) {
        self.task_manager = tm;
        self.sync_task_manager_event_emitter();
    }

    pub fn set_agent_id(&mut self, agent_id: Option<String>) {
        self.agent_id = agent_id;
        self.sync_task_manager_event_emitter();
    }

    /// Set the broadcast sender for streaming events to WebSocket clients.
    pub fn set_event_tx(&mut self, tx: broadcast::Sender<String>) {
        self.event_tx = Some(tx);
    }

    pub fn set_event_emitter(&mut self, emitter: EventEmitterHandle) {
        self.system_event_emitter = emitter;
        self.sync_task_manager_event_emitter();
    }

    pub fn event_emitter_handle(&self) -> EventEmitterHandle {
        self.system_event_emitter.clone()
    }

    /// Set a shared ResponseCache instance.
    ///
    /// This allows external code (like the CLI stdin loop) to share the same
    /// cache instance with the runtime, enabling cache clearing via `/clear` command.
    pub fn set_response_cache(&mut self, cache: crate::response_cache::ResponseCache) {
        self.response_cache = cache;
    }

    /// Get a reference to the ResponseCache.
    ///
    /// This is useful for external code to clear session caches.
    pub fn response_cache(&self) -> &crate::response_cache::ResponseCache {
        &self.response_cache
    }

    /// Initialize the 7-layer memory system for this session.
    ///
    /// This method creates the memory system and performs async initialization:
    /// - Loads cursor state from disk
    /// - Marks session as active (creates `.active` file)
    pub async fn init_memory_system(&mut self, session_id: String) -> std::io::Result<()> {
        use crate::memory_system::MemorySystem;

        let config = self.config.memory.memory_system.clone();
        // Use paths.base as both workspace and config directory
        let base_dir = self.paths.base.clone();

        let mut memory_system = MemorySystem::new(config, base_dir.clone(), base_dir, session_id);

        // Perform async initialization: load cursor state + mark session active
        memory_system.initialize().await?;

        // ========== Record config for all layers to metrics ==========

        // Layer 1: Tool Result Storage
        crate::memory_event!(
            layer1,
            config,
            memory_system.config().layer1.cache_max_per_session,
            memory_system.config().layer1.preview_size_bytes
        );

        // Layer 2: Micro Compact
        let layer2_config = crate::history_projector::TimeBasedMCConfig::from(
            memory_system.config().layer2.clone(),
        );
        crate::memory_event!(
            layer2,
            config,
            layer2_config.gap_threshold_minutes,
            layer2_config.keep_recent
        );

        // Layer 3: Session Memory
        crate::memory_event!(
            layer3,
            config,
            memory_system
                .config()
                .layer3
                .max_total_session_memory_tokens,
            memory_system.config().layer3.max_section_length
        );

        // Layer 4: Full Compact
        let recovery_budget = memory_system.config().layer4.max_file_recovery_tokens
            + memory_system.config().layer4.max_skill_recovery_tokens
            + memory_system
                .config()
                .layer4
                .max_session_memory_recovery_tokens;
        crate::memory_event!(
            layer4,
            config,
            memory_system.config().token_budget,
            memory_system.config().layer4.compact_threshold_ratio,
            recovery_budget
        );

        // Layer 5: Memory Extraction
        crate::memory_event!(
            layer5,
            config,
            memory_system.config().layer5.min_messages_for_extraction,
            memory_system.config().layer5.extraction_cooldown_messages,
            memory_system.config().layer5.max_memory_file_tokens
        );

        // Layer 6: Auto Dream (interval is typically 24 hours)
        crate::memory_event!(layer6, config, 24);

        // Layer 7: Forked Agent (max_turns default is typically 10)
        crate::memory_event!(layer7, config, 10);

        self.memory_system = Some(memory_system);

        info!("[memory_system] initialized for session");
        Ok(())
    }

    /// Get the memory system (if initialized).
    pub fn memory_system(&self) -> Option<&crate::memory_system::MemorySystem> {
        self.memory_system.as_ref()
    }

    /// Get mutable access to the memory system.
    pub fn memory_system_mut(&mut self) -> Option<&mut crate::memory_system::MemorySystem> {
        self.memory_system.as_mut()
    }

    fn sync_task_manager_event_emitter(&self) {
        self.task_manager
            .register_event_emitter(self.agent_id.as_deref(), self.system_event_emitter.clone());
    }

    async fn update_main_session_target(&mut self, msg: &InboundMessage) {
        if !is_main_session_candidate(msg) {
            return;
        }

        let next_session_key = msg.session_key();
        if self.ghost_learning_enabled() {
            if let Some(previous) = self.main_session_target.as_ref() {
                if previous.session_key != next_session_key {
                    if let Err(err) = self
                        .capture_session_rotate_learning_boundary(previous, msg)
                        .await
                    {
                        warn!(
                            error = %err,
                            from_session = %previous.session_key,
                            to_session = %next_session_key,
                            "Ghost learning session-rotate capture failed"
                        );
                    }
                }
            }
        }

        self.main_session_target = Some(MainSessionTarget {
            channel: msg.channel.clone(),
            account_id: msg.account_id.clone(),
            chat_id: msg.chat_id.clone(),
            session_key: next_session_key,
        });
    }

    fn resolve_event_delivery_target(&self, scope: &EventScope) -> Option<MainSessionTarget> {
        match scope {
            EventScope::Channel { channel, chat_id } => Some(MainSessionTarget {
                channel: channel.clone(),
                account_id: None,
                chat_id: chat_id.clone(),
                session_key: format!("{}:{}", channel, chat_id),
            }),
            EventScope::Session {
                channel,
                chat_id,
                session_key,
            } => Some(MainSessionTarget {
                channel: channel.clone(),
                account_id: None,
                chat_id: chat_id.clone(),
                session_key: session_key.clone(),
            }),
            EventScope::MainSession | EventScope::Global => self.main_session_target.clone(),
        }
    }

    async fn dispatch_system_event_notification(&self, request: &NotificationRequest) {
        let target = self.resolve_event_delivery_target(&request.scope);
        let target_channel = target.as_ref().map(|value| value.channel.clone());
        let target_chat_id = target.as_ref().map(|value| value.chat_id.clone());

        if let Some(ref event_tx) = self.event_tx {
            let event = serde_json::json!({
                "type": "system_event_notification",
                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                "event_id": request.event_id.clone(),
                "priority": request.priority,
                "title": request.title.clone(),
                "body": request.body.clone(),
                "channel": target_channel,
                "chat_id": target_chat_id,
            });
            let _ = event_tx.send(event.to_string());
        }

        if let Some(target) = target {
            if target.channel == "ws" {
                return;
            }
            if let Some(tx) = &self.outbound_tx {
                let mut outbound = OutboundMessage::new(
                    &target.channel,
                    &target.chat_id,
                    &render_system_notification_text(request),
                );
                outbound.account_id = target.account_id.clone();
                let _ = tx.send(outbound).await;
            }
        }
    }

    async fn dispatch_system_event_summary(&self, summary: &SessionSummary) {
        let target = self.main_session_target.clone();
        let target_channel = target.as_ref().map(|value| value.channel.clone());
        let target_chat_id = target.as_ref().map(|value| value.chat_id.clone());

        if let Some(ref event_tx) = self.event_tx {
            let event = serde_json::json!({
                "type": "system_event_summary",
                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                "channel": target_channel,
                "chat_id": target_chat_id,
                "title": summary.title.clone(),
                "compact_text": summary.compact_text.clone(),
                "items": summary.items.clone(),
            });
            let _ = event_tx.send(event.to_string());
        }

        if let Some(target) = target {
            if target.channel == "ws" {
                return;
            }
            if let Some(tx) = &self.outbound_tx {
                let mut outbound = OutboundMessage::new(
                    &target.channel,
                    &target.chat_id,
                    &render_session_summary_text(summary),
                );
                outbound.account_id = target.account_id.clone();
                let _ = tx.send(outbound).await;
            }
        }
    }

    async fn process_system_event_tick(&self, now_ms: i64) -> HeartbeatDecision {
        let decision = self.system_event_orchestrator.process_tick(now_ms);

        for request in &decision.immediate_notifications {
            self.dispatch_system_event_notification(request).await;
        }

        for summary in &decision.flushed_summaries {
            self.dispatch_system_event_summary(summary).await;
        }

        let _ = self.system_event_store.cleanup_expired(7 * 24 * 60 * 60);

        self.spawn_pending_ghost_background_reviews();

        decision
    }

    pub fn validate_intent_router(&self) -> Result<()> {
        let resolver = crate::intent::IntentToolResolver::new(&self.config);
        let mcp = blockcell_core::mcp_config::McpResolvedConfig::load_merged(&self.paths)?;
        resolver.validate_with_mcp(&self.tool_registry, Some(&mcp))
    }

    /// 设置独立的自进化 LLM provider（可选覆盖，不影响主 pool）
    pub fn set_evolution_provider(&mut self, provider: Box<dyn Provider>) {
        let provider_arc: Arc<dyn Provider> = Arc::from(provider);
        let llm_adapter = Arc::new(ProviderLLMAdapter {
            provider: provider_arc,
        });
        self.context_builder.set_evolution_llm_provider(llm_adapter);
    }

    /// Set the memory store handle for tools and context builder.
    pub fn set_memory_store(&mut self, store: MemoryStoreHandle) {
        self.memory_store = Some(store.clone());
        self.context_builder.set_memory_store(store);
    }

    pub fn init_memory_file_store(&mut self) -> Result<()> {
        let mut store = MemoryFileStore::open(&self.paths)?;
        store.set_write_guard(Arc::clone(&self.write_guard));
        self.memory_file_store = Some(Arc::new(store));
        Ok(())
    }

    pub fn init_skill_file_store(&mut self) -> Result<()> {
        let mut store = SkillFileStore::open(&self.paths)?;
        store.set_write_guard(Arc::clone(&self.write_guard));
        self.skill_file_store = Some(Arc::new(store));
        Ok(())
    }

    /// 后台触发 Review (参考 Hermes nudge_engine)
    ///
    /// 根据审查模式 (Skill / Memory / Combined) 在后台启动 ForkedAgent，
    /// 审查 Skill 库或用户记忆，并根据对话上下文创建/修补 Skill 或保存记忆。
    ///
    /// 如果提供了 `notify_channel`，Review 完成后会通过 outbound_tx 发送摘要通知。
    fn spawn_review(
        &self,
        mode: ReviewMode,
        messages: Vec<ChatMessage>,
        notify_channel: Option<(String, String)>,
    ) {
        let label = match mode {
            ReviewMode::Skill => "skill_nudge_review",
            ReviewMode::Memory => "memory_nudge_review",
            ReviewMode::Combined => "combined_nudge_review",
        };
        tracing::info!("[Nudge] 阈值到达, 启动后台 {:?} Review", mode);

        let skills_dir = self.paths.skills_dir();
        // 克隆一份供 ForkedAgent 使用（spawn_blocking 会 move 原始值）
        let skills_dir_clone = skills_dir.clone();
        let builtin_skills_dir = self.paths.builtin_skills_dir();
        let external_skills_dirs = vec![builtin_skills_dir];
        let provider_pool = self.provider_pool.clone();
        let model = self.config.agents.defaults.model.clone();
        let max_review_rounds = self.config.self_improve.review.max_rounds;
        let memory_store = self.memory_store.clone();
        let skill_mutex = Arc::new(self.skill_mutex.clone());
        let mode_clone = mode.clone();
        // 与 Hermes 一致: review_agent 继承主 agent 的 system prompt
        let system_prompt = self.context_builder.build_system_prompt();
        let outbound_tx = self.outbound_tx.clone();
        // 共享 skill_index_summary Arc, 供后台 Review 完成后刷新
        let skill_index_cache = self.context_builder.skill_index_summary_arc();
        let learning_coordinator = Arc::clone(&self.learning_coordinator);

        tokio::spawn(async move {
            // 构建 Skill 索引（仅在 Skill/Combined 模式下需要）
            let skill_summary = match mode_clone {
                ReviewMode::Memory => String::new(),
                ReviewMode::Skill | ReviewMode::Combined => {
                    if !skills_dir.exists() {
                        tracing::info!("[Nudge] Skills 目录不存在, 跳过 Skill 部分");
                        String::new()
                    } else {
                        let index = match tokio::task::spawn_blocking(move || {
                            crate::skill_index::SkillIndex::build_from_dir(&skills_dir)
                        })
                        .await
                        {
                            Ok(idx) => idx,
                            Err(e) => {
                                tracing::warn!(error = %e, "[Nudge] 构建索引任务失败");
                                return;
                            }
                        };

                        if index.entries().is_empty() {
                            tracing::info!("[Nudge] 无可用 Skill, 跳过 Skill 部分");
                            String::new()
                        } else {
                            index.to_prompt_summary()
                        }
                    }
                }
            };

            // 构建 Review 提示词 (与 Hermes 一致: 选择对应模式的 prompt)
            let review_prompt = match mode_clone {
                ReviewMode::Skill => SKILL_REVIEW_PROMPT.to_string(),
                ReviewMode::Memory => MEMORY_REVIEW_PROMPT.to_string(),
                ReviewMode::Combined => COMBINED_REVIEW_PROMPT.to_string(),
            };

            // 构建工具权限
            // 与 Hermes 一致: review_agent 继承主 agent 的 system prompt，不设自定义系统提示词
            // Hermes: review_agent = AIAgent(model=self.model, ...) → 使用默认 system prompt
            let can_use_tool = match mode_clone {
                ReviewMode::Skill => crate::forked::create_skill_review_can_use_tool(),
                ReviewMode::Memory => crate::forked::create_memory_review_can_use_tool(),
                ReviewMode::Combined => crate::forked::create_combined_review_can_use_tool(),
            };

            // 构建工具 Schema (传给 provider.chat() 让 LLM 知道可用工具)
            let tool_schemas = match mode_clone {
                ReviewMode::Skill => crate::forked::build_skill_review_tool_schemas(),
                ReviewMode::Memory => crate::forked::build_memory_review_tool_schemas(),
                ReviewMode::Combined => crate::forked::build_combined_review_tool_schemas(),
            };

            // 构建 ForkedAgent 参数 (与 Hermes 一致: 传入对话历史 + review prompt 作为用户消息)
            // Hermes: review_agent.run_conversation(user_message=prompt, conversation_history=messages_snapshot)
            let mut review_messages = messages.clone();
            // 如果有 Skill 索引，在 prompt 前附加
            let full_prompt = if skill_summary.is_empty() {
                review_prompt
            } else {
                format!("{}\n\n## Existing Skills\n{}", review_prompt, skill_summary)
            };
            review_messages.push(ChatMessage::user(&full_prompt));

            let cache_safe = crate::forked::CacheSafeParams::new(system_prompt, &model);
            let mut params =
                crate::forked::ForkedAgentParams::new(provider_pool, review_messages, cache_safe)
                    .with_can_use_tool(can_use_tool)
                    .with_tool_schemas(tool_schemas)
                    .with_query_source("review")
                    .with_fork_label(label)
                    .with_max_turns(max_review_rounds);

            // 传入 memory_store（Memory/Combined 模式需要）
            if let Some(store) = memory_store {
                params = params.with_memory_store(store);
            }

            // 传入 skill_mutex（防止 review agent 与主 agent 并发修改同一 Skill）
            params = params.with_skill_mutex(skill_mutex);

            // 传入 skills_dir（Skill/Combined 模式需要，否则 skill_manage/list_skills 无法工作）
            match mode_clone {
                ReviewMode::Skill | ReviewMode::Combined => {
                    // skills_dir 已在上方被 move 到 spawn_blocking 中用于构建索引，
                    // 但 ForkedAgent 也需要它来执行 skill_manage 工具。
                    // 由于 PathBuf 实现了 Clone，我们在 spawn_blocking 之前克隆一份。
                    // 注意: 此处 skills_dir_clone 是从外层闭包捕获的。
                    params = params.with_skills_dir(skills_dir_clone.clone());
                    // 传入 external_skills_dirs (builtin_skills_dir) 以支持跨目录搜索
                    params = params.with_external_skills_dirs(external_skills_dirs.clone());
                }
                ReviewMode::Memory => {}
            }

            match crate::forked::run_forked_agent(params).await {
                Ok(result) => {
                    tracing::info!(
                        mode = ?mode_clone,
                        tokens_out = result.total_usage.output_tokens,
                        "[Nudge] Review 完成"
                    );
                    if let Some(content) = &result.final_content {
                        let preview: String = content.chars().take(200).collect();
                        tracing::info!("[Nudge] Review 结果: {}", preview);
                    }
                    // 提取 Review 摘要并通知用户 (与 Hermes 一致)
                    if let Some((channel, chat_id)) = &notify_channel {
                        if let Some(tx) = &outbound_tx {
                            if let Some(summary) = Self::extract_review_summary(&result.messages) {
                                let outbound = OutboundMessage::new(channel, chat_id, &summary);
                                let _ = tx.send(outbound).await;
                                tracing::info!("[Nudge] Review 通知已发送: {}", summary);
                            }
                        }
                    }

                    // 刷新父 Agent 的 Skill 索引缓存 (后台 Review 可能创建/修改了 Skill)
                    // 与 Hermes 一致: 系统提示词在下次 LLM 调用时反映最新的 Skill 列表
                    if matches!(mode_clone, ReviewMode::Skill | ReviewMode::Combined)
                        && skills_dir_clone.exists()
                    {
                        if let Ok(index) = tokio::task::spawn_blocking(move || {
                            crate::skill_index::SkillIndex::build_from_dir(&skills_dir_clone)
                        })
                        .await
                        {
                            let mut cache =
                                skill_index_cache.write().unwrap_or_else(|e| e.into_inner());
                            *cache = if index.entries().is_empty() {
                                None
                            } else {
                                Some(index.to_prompt_summary())
                            };
                            tracing::info!("[Nudge] Skill 索引缓存已刷新");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(mode = ?mode_clone, error = %e, "[Nudge] Review 失败");
                }
            }
            // Mark review as completed for throttle tracking
            learning_coordinator.review_completed();
        });
    }

    /// 从 Review Agent 的 tool 结果中提取操作摘要 (参考 Hermes 行为)
    ///
    /// Hermes 扫描 review_agent._session_messages 中的 tool 结果,
    /// 查找 created/updated/deleted 等操作，汇总为用户可见的摘要。
    fn extract_review_summary(messages: &[ChatMessage]) -> Option<String> {
        let mut actions: Vec<String> = Vec::new();

        for msg in messages {
            if msg.role != "tool" {
                continue;
            }
            let content = match msg.content.as_str() {
                Some(c) => c,
                None => continue,
            };
            // 解析 JSON (skill_manage 和 memory 工具返回 JSON，但格式不同)
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(content) {
                // ── skill_manage 结果: {"success": true, "message": "Skill 'xxx' created", ...} ──
                let is_skill_success = data
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);

                if is_skill_success {
                    if let Some(msg_text) = data.get("message").and_then(|v| v.as_str()) {
                        let lower = msg_text.to_lowercase();
                        if lower.contains("created")
                            || lower.contains("deleted")
                            || lower.contains("updated")
                            || lower.contains("patched")
                            || lower.contains("edited")
                            || lower.contains("added")
                            || lower.contains("removed")
                            || lower.contains("replaced")
                        {
                            actions.push(msg_text.to_string());
                        }
                    }

                    // memory 工具 (Hermes 格式): {"target": "memory", "success": true, ...}
                    if let Some(target) = data.get("target").and_then(|v| v.as_str()) {
                        if !target.is_empty() && data.get("message").is_none() {
                            let label = match target {
                                "memory" => "Memory updated",
                                "user" => "User profile updated",
                                other => other,
                            };
                            actions.push(label.to_string());
                        }
                    }
                }

                // ── memory_upsert 结果: {"status": "saved", "item": {...}} ──
                if data.get("status").and_then(|v| v.as_str()) == Some("saved") {
                    actions.push("Memory updated".to_string());
                }

                // ── memory_forget 结果: {"action": "delete", "deleted": true, ...} ──
                match data.get("action").and_then(|v| v.as_str()) {
                    Some("delete") => {
                        if data
                            .get("deleted")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            actions.push("Memory updated".to_string());
                        }
                    }
                    Some("batch_delete") => {
                        let count = data
                            .get("deleted_count")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        if count > 0 {
                            actions.push(format!("Memory updated ({} items forgotten)", count));
                        }
                    }
                    Some("restore") => {
                        if data
                            .get("restored")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            actions.push("Memory item restored".to_string());
                        }
                    }
                    _ => {}
                }
            }
        }

        if actions.is_empty() {
            None
        } else {
            Some(format!("\u{1F4BE} {}", actions.join(" \u{00B7} ")))
        }
    }

    /// 在上下文压缩前，让 LLM 保存重要信息到 Memory Store
    ///
    /// 参考 Hermes `flush_memories()` — 使用 ForkedAgent 执行，
    /// 只允许 memory_upsert 和 memory_query 工具。
    /// 与 Hermes 一致: 传入完整对话历史 + flush 提示作为用户消息
    async fn flush_memory_store_before_compact(&self, messages: &[ChatMessage]) {
        if self.memory_store.is_none() {
            tracing::debug!("[flush] 无 Memory Store, 跳过 flush");
            return;
        }

        tracing::info!("[flush] 上下文压缩前保存重要信息...");

        // 与 Hermes 一致: 传入完整对话历史，追加 flush 提示作为用户消息
        // Hermes: messages + user_message="[System: The session is being compressed...]"
        let mut flush_messages = messages.to_vec();
        flush_messages.push(ChatMessage::user(
            "[System: The session is being compressed. \
             Save anything worth remembering — prioritize user preferences, \
             corrections, and recurring patterns over task-specific details.]",
        ));

        let model = self.config.agents.defaults.model.clone();
        // 与 Hermes 一致: flush_agent 继承主 agent 的 system prompt
        let system_prompt = self.context_builder.build_system_prompt();
        let cache_safe = crate::forked::CacheSafeParams::new(&system_prompt, &model);

        let can_use_tool = crate::forked::create_flush_can_use_tool();

        let mut params = crate::forked::ForkedAgentParams::new(
            self.provider_pool.clone(),
            flush_messages,
            cache_safe,
        )
        .with_can_use_tool(can_use_tool)
        .with_query_source("memory_flush")
        .with_fork_label("memory_flush")
        .with_max_turns(1); // 与 Hermes 一致: flush 仅单次 API 调用, 无需多轮

        if let Some(store) = &self.memory_store {
            params = params.with_memory_store(store.clone());
        }

        match crate::forked::run_forked_agent(params).await {
            Ok(result) => {
                tracing::info!(
                    tokens_out = result.total_usage.output_tokens,
                    "[flush] Memory flush 完成"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "[flush] Memory flush 失败, 继续压缩");
            }
        }
    }

    /// Initialize and load Layer 5 memory injector (7-layer memory system).
    /// This loads the four memory files (user.md, project.md, feedback.md, reference.md)
    /// from the memory directory and makes them available for system prompt injection.
    pub async fn init_memory_injector(&mut self) -> std::io::Result<()> {
        use crate::auto_memory::{get_memory_dir, InjectionConfig, MemoryInjector};

        // Use the config base directory (e.g., ~/.blockcell/memory/)
        let memory_dir = get_memory_dir(&self.paths.base);
        let mut injector = MemoryInjector::new(InjectionConfig::from(
            self.config.memory.memory_system.layer5.clone(),
        ));

        // Try to load memory files; log warning if directory doesn't exist
        match injector.load_memories(&memory_dir).await {
            Ok(()) => {
                let count = injector.cache_size();
                if count > 0 {
                    info!(
                        memory_dir = %memory_dir.display(),
                        files_loaded = count,
                        "[Layer 5] Memory injector initialized with {} memory files",
                        count
                    );
                } else {
                    debug!(
                        memory_dir = %memory_dir.display(),
                        "[Layer 5] Memory injector initialized (no memory files found)"
                    );
                }
                self.context_builder.set_memory_injector(injector);
            }
            Err(e) => {
                // Non-fatal: memory injection is optional enhancement
                warn!(
                    memory_dir = %memory_dir.display(),
                    error = %e,
                    "[Layer 5] Failed to load memory files, continuing without persistent memory injection"
                );
            }
        }

        Ok(())
    }

    /// Check if memory injector cache needs refresh.
    pub fn memory_injector_needs_reload(&self) -> bool {
        self.memory_injector_needs_reload
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Signal that memory injector cache needs refresh (called by background tasks).
    pub fn signal_memory_injector_reload(&self) {
        self.memory_injector_needs_reload
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reload memory injector cache if needed.
    /// This should be called at the start of each conversation turn.
    pub async fn reload_memory_injector_if_needed(&mut self) -> std::io::Result<()> {
        if !self.memory_injector_needs_reload() {
            return Ok(());
        }

        use crate::auto_memory::{get_memory_dir, InjectionConfig, MemoryInjector};

        let memory_dir = get_memory_dir(&self.paths.base);
        let mut injector = MemoryInjector::new(InjectionConfig::from(
            self.config.memory.memory_system.layer5.clone(),
        ));
        injector.load_memories(&memory_dir).await?;

        let count = injector.cache_size();
        info!(
            memory_dir = %memory_dir.display(),
            files_loaded = count,
            "[Layer 5] Memory injector cache reloaded after extraction"
        );

        self.context_builder.set_memory_injector(injector);
        self.memory_injector_needs_reload
            .store(false, std::sync::atomic::Ordering::Relaxed);

        Ok(())
    }

    /// Get a clone of the reload flag for use in background tasks.
    pub fn memory_injector_reload_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.memory_injector_needs_reload)
    }

    /// Set the capability registry handle for tools.
    pub fn set_capability_registry(&mut self, registry: CapabilityRegistryHandle) {
        self.capability_registry = Some(registry);
    }

    /// Set the core evolution engine handle for tools.
    pub fn set_core_evolution(&mut self, core_evo: CoreEvolutionHandle) {
        self.core_evolution = Some(core_evo);
    }

    /// Deprecated: MCP tools are now injected before runtime construction via the shared MCP manager.
    pub async fn mount_mcp_servers(&mut self) {}

    fn ghost_learning_enabled(&self) -> bool {
        self.config.agents.ghost.learning.enabled
    }

    fn spawn_pending_ghost_background_reviews(&self) {
        if self.config.agents.ghost.learning.enabled {
            spawn_pending_background_reviews(
                self.paths.clone(),
                Arc::clone(&self.provider_pool),
                8,
            );
        }
    }

    fn persist_ghost_learning_boundary(
        &self,
        boundary: GhostLearningBoundary,
        sources: Vec<GhostEpisodeSource>,
    ) -> Result<Option<String>> {
        if !self.ghost_learning_enabled() {
            return Ok(None);
        }
        let decision = self.learning_coordinator.ghost_decide(&boundary);
        persist_ghost_learning_boundary_with_decision(
            &self.config,
            &self.paths,
            boundary,
            sources,
            decision,
        )
    }

    fn detect_correction_signal_count(user_text: &str) -> u32 {
        let lower = user_text.to_lowercase();
        let cues = [
            "correct", "fix", "instead", "prefer", "wrong", "更正", "改成", "修正", "不要", "优先",
            "正确",
        ];
        if cues.iter().any(|cue| lower.contains(cue)) {
            1
        } else {
            0
        }
    }

    fn detect_preference_correction_count(user_text: &str) -> u32 {
        let lower = user_text.to_lowercase();
        let cues = ["prefer", "use ", "instead", "优先", "改成", "不要", "以后"];
        if cues.iter().any(|cue| lower.contains(cue)) {
            1
        } else {
            0
        }
    }

    fn apply_learned_skill_negative_feedback(
        &self,
        session_metadata: &mut serde_json::Value,
        msg: &InboundMessage,
    ) -> Result<()> {
        let correction_count = u32::from(
            Self::detect_correction_signal_count(&msg.content)
                + Self::detect_preference_correction_count(&msg.content)
                > 0,
        );
        if correction_count == 0 {
            return Ok(());
        }
        let Some(skill_name) = active_skill_name_from_metadata(session_metadata) else {
            return Ok(());
        };
        let current = session_metadata
            .get(SESSION_ACTIVE_SKILL_CORRECTIONS_KEY)
            .and_then(|value| value.as_u64())
            .unwrap_or(0) as u32;
        let next = current.saturating_add(correction_count);
        if !session_metadata.is_object() {
            *session_metadata = serde_json::json!({});
        }
        if let Some(map) = session_metadata.as_object_mut() {
            map.insert(
                SESSION_ACTIVE_SKILL_CORRECTIONS_KEY.to_string(),
                serde_json::Value::Number(next.into()),
            );
        }
        if next >= LEARNED_SKILL_DISABLE_THRESHOLD {
            disable_skill_toggle(&self.paths, &skill_name)?;
            if let Some(map) = session_metadata.as_object_mut() {
                map.remove(SESSION_ACTIVE_SKILL_NAME_KEY);
                map.insert(
                    "auto_disabled_skill".to_string(),
                    serde_json::Value::String(skill_name.clone()),
                );
            }
            warn!(
                skill = %skill_name,
                corrections = next,
                "Auto-disabled learned skill after repeated correction"
            );
        }
        Ok(())
    }

    fn latest_role_text(messages: &[ChatMessage], role: &str) -> Option<String> {
        messages
            .iter()
            .rev()
            .find(|msg| msg.role == role)
            .map(chat_message_text)
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
    }

    fn capture_turn_end_learning_boundary(
        &self,
        msg: &InboundMessage,
        history: &[ChatMessage],
        final_response: &str,
        tool_call_counts: &HashMap<String, u32>,
    ) -> Result<Option<String>> {
        if !self.ghost_learning_enabled()
            || matches!(
                msg.channel.as_str(),
                "ghost" | "cron" | "system" | "subagent"
            )
        {
            return Ok(None);
        }

        let final_text = final_response.trim();
        if final_text.is_empty() {
            return Ok(None);
        }

        let boundary = GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::TurnEnd,
            session_key: Some(msg.session_key()),
            subject_key: Some(format!("chat:{}:sender:{}", msg.chat_id, msg.sender_id)),
            user_intent_summary: msg.content.clone(),
            assistant_outcome_summary: final_text.to_string(),
            tool_call_count: tool_call_counts.values().copied().sum(),
            memory_write_count: 0,
            correction_count: Self::detect_correction_signal_count(&msg.content),
            preference_correction_count: Self::detect_preference_correction_count(&msg.content),
            success: true,
            complexity_score: estimate_turn_complexity_score(&msg.content),
            reusable_lesson: None,
        };

        let turn_count = history
            .iter()
            .filter(|message| message.role == "user")
            .count() as u32;
        let decision = GhostLearningPolicy::from_config(&self.config.agents.ghost.learning)
            .decide_with_turn_count(&boundary, Some(turn_count));

        persist_ghost_learning_boundary_with_decision(
            &self.config,
            &self.paths,
            boundary,
            vec![
                GhostEpisodeSource {
                    source_type: "session".to_string(),
                    source_key: msg.session_key(),
                    role: "primary".to_string(),
                },
                GhostEpisodeSource {
                    source_type: "chat".to_string(),
                    source_key: msg.chat_id.clone(),
                    role: "context".to_string(),
                },
                GhostEpisodeSource {
                    source_type: "history".to_string(),
                    source_key: history.len().to_string(),
                    role: "summary".to_string(),
                },
            ],
            decision,
        )
    }

    async fn capture_pre_compress_learning_boundary(
        &self,
        session_key: &str,
        messages: &[ChatMessage],
    ) -> Result<Option<String>> {
        if !self.ghost_learning_enabled() {
            return Ok(None);
        }
        let memory_write_count = self
            .flush_memories(session_key, messages, "pre_compress")
            .await?;
        let provider_pre_compress_context = if let Some(manager) =
            self.ghost_memory_lifecycle.as_ref()
        {
            let message_texts = messages.iter().map(chat_message_text).collect::<Vec<_>>();
            let provider_block = manager.on_pre_compress(&message_texts, session_key);
            if !provider_block.trim().is_empty() {
                debug!(session_key = %session_key, "Ghost memory provider contributed pre-compress context");
                Some(truncate_str(&provider_block, 1200))
            } else {
                None
            }
        } else {
            None
        };

        let boundary = GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::PreCompress,
            session_key: Some(session_key.to_string()),
            subject_key: Some(format!("session:{}", session_key)),
            user_intent_summary: Self::latest_role_text(messages, "user")
                .unwrap_or_else(|| "pre-compress boundary".to_string()),
            assistant_outcome_summary: Self::latest_role_text(messages, "assistant")
                .unwrap_or_else(|| "conversation is about to compact".to_string()),
            tool_call_count: messages
                .iter()
                .filter_map(|msg| msg.tool_calls.as_ref().map(|calls| calls.len() as u32))
                .sum(),
            memory_write_count,
            correction_count: 0,
            preference_correction_count: 0,
            success: true,
            complexity_score: 0,
            reusable_lesson: provider_pre_compress_context,
        };

        self.persist_ghost_learning_boundary(
            boundary,
            vec![GhostEpisodeSource {
                source_type: "session".to_string(),
                source_key: session_key.to_string(),
                role: "primary".to_string(),
            }],
        )
    }

    async fn capture_main_session_end_learning_boundary(&self) -> Result<Option<String>> {
        if !self.ghost_learning_enabled() {
            return Ok(None);
        }

        let Some(target) = self.main_session_target.as_ref() else {
            return Ok(None);
        };
        let history = self.session_store.load(&target.session_key)?;
        if history.is_empty() {
            return Ok(None);
        }
        let memory_write_count = self
            .flush_memories(&target.session_key, &history, "session_end")
            .await?;
        let provider_session_end_context =
            if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
                let message_texts = history.iter().map(chat_message_text).collect::<Vec<_>>();
                manager.on_session_end(&message_texts, &target.session_key);
                let provider_block =
                    manager.on_session_boundary_context(&message_texts, &target.session_key);
                if !provider_block.trim().is_empty() {
                    Some(truncate_str(&provider_block, 1200))
                } else {
                    None
                }
            } else {
                None
            };

        let boundary = GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::SessionEnd,
            session_key: Some(target.session_key.clone()),
            subject_key: Some(format!("chat:{}", target.chat_id)),
            user_intent_summary: Self::latest_role_text(&history, "user")
                .unwrap_or_else(|| "session end".to_string()),
            assistant_outcome_summary: Self::latest_role_text(&history, "assistant")
                .unwrap_or_else(|| "session end boundary".to_string()),
            tool_call_count: history
                .iter()
                .filter_map(|msg| msg.tool_calls.as_ref().map(|calls| calls.len() as u32))
                .sum(),
            memory_write_count,
            correction_count: 0,
            preference_correction_count: 0,
            success: true,
            complexity_score: 0,
            reusable_lesson: provider_session_end_context,
        };

        self.persist_ghost_learning_boundary(
            boundary,
            vec![GhostEpisodeSource {
                source_type: "session".to_string(),
                source_key: target.session_key.clone(),
                role: "primary".to_string(),
            }],
        )
    }

    async fn capture_session_rotate_learning_boundary(
        &self,
        previous: &MainSessionTarget,
        next_msg: &InboundMessage,
    ) -> Result<Option<String>> {
        if !self.ghost_learning_enabled() {
            return Ok(None);
        }

        let history = self.session_store.load(&previous.session_key)?;
        if history.is_empty() {
            return Ok(None);
        }
        let memory_write_count = self
            .flush_memories(&previous.session_key, &history, "session_rotate")
            .await?;
        let provider_session_end_context =
            if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
                let message_texts = history.iter().map(chat_message_text).collect::<Vec<_>>();
                manager.on_session_end(&message_texts, &previous.session_key);
                let provider_block =
                    manager.on_session_boundary_context(&message_texts, &previous.session_key);
                if !provider_block.trim().is_empty() {
                    Some(truncate_str(&provider_block, 1200))
                } else {
                    None
                }
            } else {
                None
            };

        let boundary = GhostLearningBoundary {
            kind: GhostLearningBoundaryKind::SessionRotate,
            session_key: Some(previous.session_key.clone()),
            subject_key: Some(format!("chat:{}", previous.chat_id)),
            user_intent_summary: Self::latest_role_text(&history, "user")
                .unwrap_or_else(|| "session rotate".to_string()),
            assistant_outcome_summary: Self::latest_role_text(&history, "assistant")
                .unwrap_or_else(|| "session rotated to a new active chat".to_string()),
            tool_call_count: history
                .iter()
                .filter_map(|msg| msg.tool_calls.as_ref().map(|calls| calls.len() as u32))
                .sum(),
            memory_write_count,
            correction_count: 0,
            preference_correction_count: 0,
            success: true,
            complexity_score: 0,
            reusable_lesson: Some(match provider_session_end_context {
                Some(context) => format!(
                    "Switched active session from {} to {}\n\n{}",
                    previous.chat_id, next_msg.chat_id, context
                ),
                None => format!(
                    "Switched active session from {} to {}",
                    previous.chat_id, next_msg.chat_id
                ),
            }),
        };

        self.persist_ghost_learning_boundary(
            boundary,
            vec![
                GhostEpisodeSource {
                    source_type: "session".to_string(),
                    source_key: previous.session_key.clone(),
                    role: "primary".to_string(),
                },
                GhostEpisodeSource {
                    source_type: "chat".to_string(),
                    source_key: previous.chat_id.clone(),
                    role: "context".to_string(),
                },
                GhostEpisodeSource {
                    source_type: "session".to_string(),
                    source_key: next_msg.session_key(),
                    role: "next".to_string(),
                },
            ],
        )
    }

    async fn flush_memories(
        &self,
        session_key: &str,
        messages: &[ChatMessage],
        boundary: &str,
    ) -> Result<u32> {
        if messages.is_empty() {
            return Ok(0);
        }
        let Some((_provider_idx, provider)) = self.provider_pool.acquire() else {
            warn!(session_key = %session_key, boundary = %boundary, "Ghost memory flush skipped: no provider available");
            return Ok(0);
        };

        let mut loop_messages = Self::build_memory_flush_messages(session_key, messages, boundary);
        let registry = Self::restricted_memory_flush_tool_registry();
        let tools = registry.get_filtered_schemas(&["memory_manage"]);
        let mut writes = 0u32;

        for _round in 0..2 {
            let response = match provider.chat(&loop_messages, &tools).await {
                Ok(response) => response,
                Err(err) => {
                    warn!(error = %err, session_key = %session_key, boundary = %boundary, "Ghost memory flush provider call failed");
                    return Ok(writes);
                }
            };
            if response.tool_calls.is_empty() {
                return Ok(writes);
            }

            let mut assistant = ChatMessage::assistant(response.content.as_deref().unwrap_or(""));
            assistant.tool_calls = Some(response.tool_calls.clone());
            loop_messages.push(assistant);

            for call in response.tool_calls {
                if call.name != "memory_manage" {
                    let result = serde_json::json!({
                        "error": format!("tool '{}' is not allowed during memory flush", call.name),
                    });
                    loop_messages.push(Self::memory_flush_tool_result_message(&call, &result));
                    continue;
                }
                let result = registry
                    .execute(
                        &call.name,
                        self.memory_flush_tool_context(session_key)?,
                        call.arguments.clone(),
                    )
                    .await;
                match result {
                    Ok(value) => {
                        if value
                            .get("success")
                            .and_then(|success| success.as_bool())
                            .unwrap_or(false)
                        {
                            writes += 1;
                        }
                        loop_messages.push(Self::memory_flush_tool_result_message(&call, &value));
                    }
                    Err(err) => {
                        let result = serde_json::json!({"error": err.to_string()});
                        loop_messages.push(Self::memory_flush_tool_result_message(&call, &result));
                    }
                }
            }
        }

        Ok(writes)
    }

    fn memory_flush_tool_context(&self, session_key: &str) -> Result<ToolContext> {
        Ok(ToolContext {
            workspace: self.paths.workspace(),
            builtin_skills_dir: Some(self.paths.builtin_skills_dir()),
            active_skill_dir: None,
            session_key: session_key.to_string(),
            channel: "ghost".to_string(),
            account_id: None,
            sender_id: None,
            chat_id: session_key.to_string(),
            config: self.config.clone(),
            permissions: blockcell_core::types::PermissionSet::new(),
            task_manager: None,
            memory_store: None,
            memory_file_store: Some({
                let mut mfs = MemoryFileStore::open(&self.paths)?;
                mfs.set_write_guard(Arc::clone(&self.write_guard));
                Arc::new(mfs)
            }),
            ghost_memory_lifecycle: self.ghost_memory_lifecycle.clone().map(|manager| {
                manager as Arc<dyn blockcell_tools::GhostMemoryLifecycleOps + Send + Sync>
            }),
            skill_file_store: None,
            session_search: None,
            outbound_tx: None,
            spawn_handle: None,
            capability_registry: None,
            core_evolution: None,
            event_emitter: None,
            channel_contacts_file: Some(self.paths.channel_contacts_file()),
            response_cache: None,
            skill_mutex: None,
        })
    }

    fn restricted_memory_flush_tool_registry() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(blockcell_tools::memory::MemoryManageTool));
        registry
    }

    fn build_memory_flush_messages(
        session_key: &str,
        messages: &[ChatMessage],
        boundary: &str,
    ) -> Vec<ChatMessage> {
        let mut flush_messages = messages.iter().rev().take(24).cloned().collect::<Vec<_>>();
        flush_messages.reverse();

        let sentinel = format!(
            "__ghost_memory_flush_sentinel:{}:{}",
            session_key,
            chrono::Utc::now().timestamp_millis()
        );
        flush_messages.push(ChatMessage::user(
            &serde_json::json!({
                "_flush_sentinel": sentinel,
                "task": "The session is reaching a compression/session boundary. Save anything worth remembering before context is lost.",
                "boundary": boundary,
                "sessionKey": session_key,
                "allowedTools": ["memory_manage"],
                "rules": [
                    "Use only memory_manage.",
                    "Save durable user preferences, recurring corrections, stable project facts, reusable non-procedural lessons, and environment constraints.",
                    "Do not save task progress, temporary TODOs, completed-work logs, one-off outcomes, or short-lived status.",
                    "If nothing durable should be saved, make no tool calls."
                ]
            })
            .to_string(),
        ));

        flush_messages
    }

    fn memory_flush_tool_result_message(
        call: &ToolCallRequest,
        result: &serde_json::Value,
    ) -> ChatMessage {
        let mut message = ChatMessage::tool_result(&call.id, &result.to_string());
        message.name = Some(call.name.clone());
        message
    }

    /// Create a restricted tool registry for subagents (no spawn, no message, no cron).
    pub(crate) fn subagent_tool_registry() -> ToolRegistry {
        use blockcell_tools::alert_rule::AlertRuleTool;
        use blockcell_tools::app_control::AppControlTool;
        use blockcell_tools::audio_transcribe::AudioTranscribeTool;
        use blockcell_tools::browser::BrowseTool;
        use blockcell_tools::camera::CameraCaptureTool;
        use blockcell_tools::chart_generate::ChartGenerateTool;
        use blockcell_tools::community_hub::CommunityHubTool;
        use blockcell_tools::data_process::DataProcessTool;
        use blockcell_tools::email::EmailTool;
        use blockcell_tools::encrypt::EncryptTool;
        use blockcell_tools::exec::ExecTool;
        use blockcell_tools::file_ops::FileOpsTool;
        use blockcell_tools::fs::*;
        use blockcell_tools::http_request::HttpRequestTool;
        use blockcell_tools::image_understand::ImageUnderstandTool;
        use blockcell_tools::knowledge_graph::KnowledgeGraphTool;
        use blockcell_tools::memory::{MemoryForgetTool, MemoryQueryTool, MemoryUpsertTool};
        use blockcell_tools::memory_maintenance::MemoryMaintenanceTool;
        use blockcell_tools::network_monitor::NetworkMonitorTool;
        use blockcell_tools::ocr::OcrTool;
        use blockcell_tools::office_write::OfficeWriteTool;
        use blockcell_tools::skills::{ListSkillsTool, SkillManageTool, SkillViewTool};
        use blockcell_tools::stream_subscribe::StreamSubscribeTool;
        use blockcell_tools::system_info::{CapabilityEvolveTool, SystemInfoTool};
        use blockcell_tools::tasks::ListTasksTool;
        use blockcell_tools::termux_api::TermuxApiTool;
        use blockcell_tools::toggle_manage::ToggleManageTool;
        use blockcell_tools::tts::TtsTool;
        use blockcell_tools::video_process::VideoProcessTool;
        use blockcell_tools::web::*;

        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ReadFileTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(EditFileTool));
        registry.register(Arc::new(ListDirTool));
        registry.register(Arc::new(ExecTool));
        registry.register(Arc::new(WebSearchTool));
        registry.register(Arc::new(WebFetchTool));
        registry.register(Arc::new(ListTasksTool));
        registry.register(Arc::new(BrowseTool));
        registry.register(Arc::new(MemoryQueryTool));
        registry.register(Arc::new(MemoryUpsertTool));
        registry.register(Arc::new(MemoryForgetTool));
        registry.register(Arc::new(ListSkillsTool));
        registry.register(Arc::new(SkillViewTool));
        registry.register(Arc::new(SkillManageTool));
        registry.register(Arc::new(SystemInfoTool));
        registry.register(Arc::new(CapabilityEvolveTool));
        registry.register(Arc::new(CameraCaptureTool));
        registry.register(Arc::new(AppControlTool));
        registry.register(Arc::new(FileOpsTool));
        registry.register(Arc::new(DataProcessTool));
        registry.register(Arc::new(HttpRequestTool));
        registry.register(Arc::new(EmailTool));
        registry.register(Arc::new(AudioTranscribeTool));
        registry.register(Arc::new(ChartGenerateTool));
        registry.register(Arc::new(OfficeWriteTool));
        registry.register(Arc::new(TtsTool));
        registry.register(Arc::new(OcrTool));
        registry.register(Arc::new(ImageUnderstandTool));
        registry.register(Arc::new(VideoProcessTool));
        registry.register(Arc::new(EncryptTool));
        registry.register(Arc::new(NetworkMonitorTool));
        registry.register(Arc::new(KnowledgeGraphTool));
        registry.register(Arc::new(StreamSubscribeTool));
        registry.register(Arc::new(AlertRuleTool));
        registry.register(Arc::new(CommunityHubTool));
        registry.register(Arc::new(MemoryMaintenanceTool));
        registry.register(Arc::new(ToggleManageTool));
        registry.register(Arc::new(TermuxApiTool));
        // No SpawnTool, MessageTool, CronTool — subagents can't spawn or send messages
        registry
    }

    /// 返回当前 provider pool（供外部检查状态）
    pub fn provider_pool(&self) -> &Arc<ProviderPool> {
        &self.provider_pool
    }

    /// Build an extractive summary from session history (no LLM call).
    /// Extracts user questions and final assistant answers, truncated to fit.
    fn build_extractive_summary(history: &[ChatMessage]) -> String {
        let mut summary_parts: Vec<String> = Vec::new();
        let mut i = 0;
        while i < history.len() {
            let msg = &history[i];
            if msg.role == "user" {
                let user_text = match &msg.content {
                    serde_json::Value::String(s) => {
                        let chars: String = s.chars().take(100).collect();
                        if s.chars().count() > 100 {
                            format!("{}...", chars)
                        } else {
                            chars
                        }
                    }
                    _ => "(media)".to_string(),
                };
                // Find the last assistant text reply in this round
                let mut assistant_text = String::new();
                let mut j = i + 1;
                while j < history.len() && history[j].role != "user" {
                    if history[j].role == "assistant" && history[j].tool_calls.is_none() {
                        assistant_text = match &history[j].content {
                            serde_json::Value::String(s) => {
                                let chars: String = s.chars().take(150).collect();
                                if s.chars().count() > 150 {
                                    format!("{}...", chars)
                                } else {
                                    chars
                                }
                            }
                            _ => String::new(),
                        };
                    }
                    j += 1;
                }
                if !assistant_text.is_empty() {
                    summary_parts.push(format!("Q: {} → A: {}", user_text, assistant_text));
                } else {
                    summary_parts.push(format!("Q: {} → (tool interaction)", user_text));
                }
                i = j;
            } else {
                i += 1;
            }
        }

        // Cap total summary length
        let mut summary = summary_parts.join("\n");
        if summary.chars().count() > 800 {
            // Keep only the most recent entries
            while summary.chars().count() > 800 && summary_parts.len() > 1 {
                summary_parts.remove(0);
                summary = summary_parts.join("\n");
            }
        }
        summary
    }

    /// Execute Layer 4 Full Compact - LLM 语义压缩
    ///
    /// 当 token 超过预算阈值时，使用 LLM 生成 9-part structured summary，
    /// 并收集恢复信息（文件、技能、Session Memory）。
    ///
    /// ## 参数
    /// - `messages` - 要压缩的消息列表
    /// - `_session_key` - 会话标识符
    /// - `compact_ctx` - 可选的通知上下文，用于发送用户通知
    ///
    /// ## 返回
    /// - `CompactResult` - 压缩结果（通过 `success` 字段判断是否成功）
    ///   - 成功：`success: true`，包含摘要和恢复消息
    ///   - 失败：`success: false`，`error` 字段包含错误信息
    async fn execute_layer4_compact(
        &self,
        messages: &[ChatMessage],
        _session_key: &str,
        compact_ctx: Option<CompactContext<'_>>,
        is_auto: bool,
    ) -> crate::compact::CompactResult {
        use crate::compact::{generate_compact_summary, CompactResult};
        use crate::session_memory::get_session_memory_path;
        use crate::session_metrics::get_compact_circuit_breaker;

        let pre_compact_tokens = estimate_messages_tokens(messages);
        let keep_recent_messages = self
            .memory_system
            .as_ref()
            .map(|m| m.config().layer4.keep_recent_messages)
            .unwrap_or(2);
        let recent_messages: Vec<ChatMessage> = messages
            .iter()
            .rev()
            .take(keep_recent_messages)
            .cloned()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();

        // ========== 0. Memory Flush — 压缩前保存重要信息 ==========
        self.flush_memory_store_before_compact(messages).await;

        // ========== 1. 熔断器检查 ==========
        let circuit_breaker = get_compact_circuit_breaker();
        if !circuit_breaker.allow() {
            warn!(
                target: "blockcell.session_metrics.layer4",
                "[layer4] Compact skipped - circuit breaker OPEN"
            );
            return CompactResult::failed("Circuit breaker open - too many recent failures");
        }

        // ========== 2. 发送压缩开始通知 ==========
        if let (Some(ref tx), Some(ref ctx)) = (&self.outbound_tx, &compact_ctx) {
            let mut notification = OutboundMessage::new(
                ctx.channel,
                ctx.chat_id,
                "🔄 对话历史较长，正在压缩以保持性能...",
            );
            if let Some(aid) = ctx.account_id {
                notification.account_id = Some(aid.to_string());
            }
            let _ = tx.send(notification).await;
        }

        // ========== 3. 记录压缩开始事件 ==========
        let threshold = self
            .memory_system
            .as_ref()
            .map(|m| m.config().layer4.compact_threshold_ratio)
            .unwrap_or(0.8);
        crate::memory_event!(
            layer4,
            compact_started,
            pre_compact_tokens,
            threshold,
            is_auto
        );

        info!(pre_compact_tokens, "[layer4] Starting full compact");

        // ========== 4. 生成系统提示 ==========
        let system_prompt = Arc::new(
            "你是一个对话摘要助手。请根据对话历史生成结构化摘要，保留关键信息用于后续继续工作。"
                .to_string(),
        );

        // ========== 5. 获取模型配置 ==========
        let model = self.config.agents.defaults.model.clone();

        // ========== 6. 执行 LLM 语义压缩 ==========
        let max_output_tokens = self
            .memory_system
            .as_ref()
            .map(|m| m.config().layer4.max_output_tokens as u32)
            .unwrap_or(12_000);
        let summary_result = generate_compact_summary(
            Arc::clone(&self.provider_pool),
            system_prompt,
            &model,
            messages.to_vec(),
            max_output_tokens,
        )
        .await;

        let (summary_message, cache_read_tokens, cache_creation_tokens) = match summary_result {
            Ok(result) => (
                result.summary.to_markdown(),
                result.cache_read_tokens,
                result.cache_creation_tokens,
            ),
            Err(e) => {
                let error_msg = format!("LLM compact summary generation failed: {}", e);
                warn!(error = %e, "[layer4] Failed to generate compact summary");

                // 记录失败事件和熔断器状态
                crate::memory_event!(layer4, compact_failed, &error_msg, pre_compact_tokens, 1);
                circuit_breaker.record_failure();

                // 发送失败通知
                if let (Some(ref tx), Some(ref ctx)) = (&self.outbound_tx, &compact_ctx) {
                    let mut notification = OutboundMessage::new(
                        ctx.channel,
                        ctx.chat_id,
                        "⚠️ 压缩失败，继续使用当前历史。",
                    );
                    if let Some(aid) = ctx.account_id {
                        notification.account_id = Some(aid.to_string());
                    }
                    let _ = tx.send(notification).await;
                }

                return CompactResult::failed(&error_msg);
            }
        };

        // ========== 7. 收集恢复信息 ==========
        let recovery_message = if let Some(memory_system) = self.memory_system.as_ref() {
            let session_memory_path =
                get_session_memory_path(memory_system.workspace_dir(), memory_system.session_id());
            let session_memory_content =
                if tokio::fs::try_exists(&session_memory_path).await.ok() == Some(true) {
                    tokio::fs::read_to_string(&session_memory_path).await.ok()
                } else {
                    None
                };

            memory_system.generate_compact_recovery(session_memory_content.as_deref())
        } else {
            String::new()
        };

        // ========== 8. 构建 CompactResult ==========
        let post_compact_tokens = estimate_messages_tokens(&[
            ChatMessage::system(&summary_message),
            ChatMessage::user(&recovery_message),
        ]);

        // ========== 9. 记录成功事件 ==========
        // 使用来自 LLM API 响应的真实 cache usage 数据
        crate::memory_event!(
            layer4,
            compact_completed,
            pre_compact_tokens,
            post_compact_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            is_auto
        );
        circuit_breaker.record_success();

        info!(
            pre_compact_tokens,
            post_compact_tokens,
            compression_ratio = if pre_compact_tokens > 0 {
                (pre_compact_tokens - post_compact_tokens) as f64 / pre_compact_tokens as f64
            } else {
                0.0
            },
            "[layer4] Compact completed successfully"
        );

        // ========== 10. 发送压缩成功通知 ==========
        if let (Some(ref tx), Some(ref ctx)) = (&self.outbound_tx, &compact_ctx) {
            let notification_content = if pre_compact_tokens > 0 {
                let compression_ratio = (pre_compact_tokens - post_compact_tokens) as f64
                    / pre_compact_tokens as f64
                    * 100.0;
                format!(
                    "✅ 已压缩对话历史，保留关键信息。\n📊 Token: {} → {} (压缩 {:.0}%)",
                    pre_compact_tokens, post_compact_tokens, compression_ratio
                )
            } else {
                "✅ 压缩完成（无历史内容需要压缩）".to_string()
            };
            let mut notification =
                OutboundMessage::new(ctx.channel, ctx.chat_id, &notification_content);
            if let Some(aid) = ctx.account_id {
                notification.account_id = Some(aid.to_string());
            }
            let _ = tx.send(notification).await;
        }

        CompactResult {
            summary_message,
            recovery_message,
            pre_compact_tokens,
            post_compact_tokens,
            cache_read_tokens,
            cache_creation_tokens,
            success: true,
            error: None,
            recent_messages,
        }
    }

    async fn chat_with_provider(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> Result<LLMResponse> {
        if let Some((pidx, provider)) = self.provider_pool.acquire() {
            let result = provider.chat(messages, tools).await;
            match &result {
                Ok(_) => self.provider_pool.report(pidx, CallResult::Success),
                Err(e) => self
                    .provider_pool
                    .report(pidx, ProviderPool::classify_error(&format!("{}", e))),
            }
            result
        } else {
            Err(blockcell_core::Error::Config(
                "ProviderPool: no healthy providers".to_string(),
            ))
        }
    }

    async fn run_prompt_skill_loop(
        &mut self,
        msg: &InboundMessage,
        messages: Vec<ChatMessage>,
        tools: Vec<serde_json::Value>,
        tool_names: &[String],
        active_skill_dir: Option<PathBuf>,
    ) -> Result<PromptSkillLoopOutput> {
        let allowed_tool_names = tool_names.iter().cloned().collect::<HashSet<_>>();
        let max_iterations = self.config.agents.defaults.max_tool_iterations.clamp(1, 30);
        let tools_max_iterations = self
            .config
            .agents
            .defaults
            .max_tool_iterations_by_tool
            .clone();
        let mut tool_call_counts: HashMap<String, u32> = HashMap::new();
        let mut over_iteration: bool = false;
        let mut current_messages = messages;
        let mut trace_messages = Vec::new();

        let final_response = loop {
            let response = self.chat_with_provider(&current_messages, &tools).await?;

            if response.tool_calls.is_empty() {
                break response.content.unwrap_or_default();
            }

            let assistant_tool_call = ChatMessage {
                id: None,
                role: "assistant".to_string(),
                content: serde_json::Value::String(response.content.unwrap_or_default()),
                reasoning_content: response.reasoning_content.clone(),
                tool_calls: Some(response.tool_calls.clone()),
                tool_call_id: None,
                name: None,
            };
            current_messages.push(assistant_tool_call.clone());
            trace_messages.push(assistant_tool_call);

            for tool_call in response.tool_calls {
                let tool_result =
                    if crate::prompt_skill_executor::PromptSkillExecutor::is_tool_allowed(
                        &tool_call.name,
                        &allowed_tool_names,
                    ) {
                        let max_iterations = tools_max_iterations
                            .get(&tool_call.name)
                            .copied()
                            .unwrap_or(max_iterations);
                        let count = tool_call_counts.entry(tool_call.name.clone()).or_insert(0);

                        *count += 1;
                        if *count > max_iterations {
                            over_iteration = true;
                            serde_json::json!({
                                "error": format!(
                                    "Tool '{}' execeeded max call limit ({}).",
                                    tool_call.name, max_iterations
                                ),
                                "tool": tool_call.name,
                                "hint": "Reduce repeated tool calls or adjust maxToolIterationsByTool."
                            })
                            .to_string()
                        } else {
                            self.execute_tool_call(&tool_call, msg, active_skill_dir.clone())
                                .await
                        }
                    } else {
                        serde_json::json!({
                            "error": format!(
                                "Tool '{}' is not available inside prompt skill scope.",
                                tool_call.name
                            ),
                            "tool": tool_call.name,
                            "hint": "Use only the tools declared by the active skill."
                        })
                        .to_string()
                    };
                let mut tool_message = ChatMessage::tool_result(&tool_call.id, &tool_result);
                tool_message.name = Some(tool_call.name.clone());
                current_messages.push(tool_message.clone());
                trace_messages.push(tool_message);
            }

            if over_iteration {
                let mut final_messages = current_messages.clone();
                final_messages.push(ChatMessage::user(
                    "请基于以上技能上下文和工具结果，直接给出最终答案。不要再调用任何工具。",
                ));
                let final_response = self
                    .chat_with_provider(&final_messages, &[])
                    .await?
                    .content
                    .unwrap_or_default();
                break final_response;
            }
        };

        let final_response = strip_fake_tool_calls(final_response.trim());
        Ok(PromptSkillLoopOutput {
            final_response: final_response.trim().to_string(),
            trace_messages,
        })
    }

    fn last_local_exec_tool_name(trace_messages: &[ChatMessage]) -> Option<String> {
        for message in trace_messages.iter().rev() {
            if let Some(name) = message.name.as_deref() {
                if matches!(name, "exec_skill_script" | "exec_local") {
                    return Some(name.to_string());
                }
            }

            if let Some(tool_calls) = message.tool_calls.as_ref() {
                for call in tool_calls.iter().rev() {
                    if matches!(call.name.as_str(), "exec_skill_script" | "exec_local") {
                        return Some(call.name.clone());
                    }
                }
            }
        }

        None
    }

    async fn decide_interaction(
        &mut self,
        msg: &InboundMessage,
        disabled_skills: &HashSet<String>,
        classifier: &crate::intent::IntentClassifier,
        history: &[ChatMessage],
        session_metadata: &serde_json::Value,
    ) -> Result<InteractionDecision> {
        let forced_skill_name = msg
            .metadata
            .get("forced_skill_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let chat_intents = classifier.classify(&msg.content);
        let session_skill_name = continued_skill_name(session_metadata, history);

        if !forced_skill_name.is_empty() {
            let active_skill = self
                .context_builder
                .resolve_active_skill_by_name(forced_skill_name, disabled_skills)
                .map(|skill| {
                    suppress_prompt_reinjection_for_continued_skill(
                        skill,
                        session_skill_name.as_deref(),
                    )
                })
                .ok_or_else(|| {
                    blockcell_core::Error::Skill(format!(
                        "Forced skill '{}' is not available",
                        forced_skill_name
                    ))
                })?;

            info!(
                mode = ?InteractionMode::Skill,
                active_skill = %active_skill.name,
                "Interaction mode resolved from forced skill"
            );

            return Ok(InteractionDecision {
                active_skill: Some(active_skill),
                chat_intents,
                mode: InteractionMode::Skill,
            });
        }

        info!(
            mode = ?InteractionMode::General,
            intents = ?chat_intents,
            recent_skill = session_skill_name.as_deref(),
            "Interaction mode resolved from unified entry"
        );
        Ok(InteractionDecision {
            active_skill: None,
            chat_intents,
            mode: InteractionMode::General,
        })
    }

    async fn execute_decided_skill_route(
        &mut self,
        decision: &InteractionDecision,
        msg: &InboundMessage,
        persist_session_key: &str,
    ) -> Option<Result<String>> {
        if !matches!(decision.mode, InteractionMode::Skill) {
            return None;
        }

        let skill_ctx = decision.active_skill.as_ref()?.clone();
        info!(
            skill = %skill_ctx.name,
            "Skill matched — entering unified skill executor"
        );
        Some(
            self.execute_skill_for_user(&skill_ctx, msg, persist_session_key)
                .await
                .map(|result| result.final_response),
        )
    }

    async fn execute_skill_for_user(
        &mut self,
        active_skill: &ActiveSkillContext,
        msg: &InboundMessage,
        persist_session_key: &str,
    ) -> Result<SkillExecutionResult> {
        // Layer 4: Track skill activation for Post-Compact recovery
        // 在技能执行入口处追踪，覆盖手动激活和意图路由自动加载
        if let Some(memory_system) = self.memory_system.as_mut() {
            memory_system.record_skill_load(&active_skill.name, &active_skill.prompt_md);
            debug!(skill_name = %active_skill.name, "[layer4] Tracked skill activation for recovery (auto-routed or manual)");
        }

        let history = self.session_store.load(persist_session_key)?;
        let (result, mut session_metadata, allowed_tools) = self
            .run_skill_for_turn(active_skill, msg, &history, persist_session_key)
            .await?;
        record_active_skill_name(&mut session_metadata, &active_skill.name);
        let mut updated_history = history;
        persist_prompt_skill_history(
            &mut updated_history,
            &msg.content,
            &active_skill.name,
            &allowed_tools,
            &result.trace_messages,
            &result.final_response,
        );
        self.session_store.save_with_metadata(
            persist_session_key,
            &updated_history,
            &session_metadata,
        )?;
        self.deliver_skill_response(msg, &result.final_response, Some("skill"))
            .await;

        Ok(result)
    }

    fn resolved_skill_tool_names(&self, active_skill: &ActiveSkillContext) -> Vec<String> {
        let available_tools = self
            .tool_registry
            .tool_names()
            .into_iter()
            .collect::<HashSet<_>>();
        let mut declared_tools = active_skill.tools.clone();
        if self
            .context_builder
            .skill_manager()
            .and_then(|manager| manager.get(&active_skill.name))
            .map(blockcell_skills::SkillManager::build_skill_card)
            .is_some_and(|card| card.supports_local_exec)
        {
            declared_tools.push("exec_skill_script".to_string());
            declared_tools.push("exec_local".to_string());
        }
        crate::prompt_skill_executor::PromptSkillExecutor::resolve_allowed_tool_names(
            &declared_tools,
            &available_tools,
        )
    }

    async fn run_skill_for_turn(
        &mut self,
        active_skill: &ActiveSkillContext,
        msg: &InboundMessage,
        history: &[ChatMessage],
        session_key: &str,
    ) -> Result<(SkillExecutionResult, serde_json::Value, Vec<String>)> {
        let manual_mode = determine_manual_load_mode(&active_skill.name, history);
        info!(
            skill = %active_skill.name,
            manual_mode = ?manual_mode,
            "Unified skill executor starting"
        );

        let mut prompt_skill = active_skill.clone();
        prompt_skill.inject_prompt_md =
            prompt_skill.inject_prompt_md && manual_mode.should_load_manual();

        let allowed_tools = self.resolved_skill_tool_names(&prompt_skill);
        let (final_response, trace_messages, session_metadata) = self
            .run_prompt_skill_for_session(&prompt_skill, msg, history, session_key, &allowed_tools)
            .await?;

        Ok((
            SkillExecutionResult {
                final_response,
                trace_messages,
            },
            session_metadata,
            allowed_tools,
        ))
    }

    async fn persist_and_deliver_final_response(
        &mut self,
        ctx: FinalResponseContext<'_>,
    ) -> Result<String> {
        let FinalResponseContext {
            msg,
            persist_session_key,
            history,
            session_metadata,
            final_response,
            collected_media,
            cron_deliver_target,
        } = ctx;
        let final_response = strip_fake_tool_calls(final_response.trim());
        info!(target: "chat::output", content = %final_response, "Final response");

        // Only cache if this turn had substantive tool results — prevents caching
        // LLM-hallucinated lists from empty/error tool results.
        // A tool message with empty/null content (e.g. memory_query returning [])
        // should not qualify as "real" data backing the assistant's list.
        let has_tool_results = history.iter().any(|m| {
            m.role == "tool"
                && match &m.content {
                    serde_json::Value::String(s) => {
                        !s.is_empty() && s != "[]" && !s.starts_with("{\"error\"")
                    }
                    serde_json::Value::Null => false,
                    _ => true,
                }
        });
        if let Some(stub) = self.response_cache.maybe_cache_and_stub(
            persist_session_key,
            &final_response,
            has_tool_results,
        ) {
            overwrite_last_assistant_message(history, &stub);
        }

        self.session_store
            .save_with_metadata(persist_session_key, history, session_metadata)?;

        if history.len() >= 6 {
            if let Some(ref store) = self.memory_store {
                let summary = Self::build_extractive_summary(history);
                if !summary.is_empty() {
                    if let Err(e) = store.upsert_session_summary(persist_session_key, &summary) {
                        debug!(error = %e, "Failed to upsert session summary");
                    }
                }
            }
        }

        if msg.channel == "cron"
            && msg
                .metadata
                .get("cron_agent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        {
            if let Some(tx) = &self.outbound_tx {
                let mut outbound =
                    OutboundMessage::new(&msg.channel, &msg.chat_id, &final_response);
                outbound.account_id = msg.account_id.clone();
                outbound.media = collected_media.clone();
                outbound.metadata = extract_reply_metadata(msg);
                let _ = tx.send(outbound).await;
            }

            if let Some((channel, to)) = cron_deliver_target {
                if channel == "ws" {
                    if let Some(ref event_tx) = self.event_tx {
                        let event = serde_json::json!({
                            "type": "message_done",
                            "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                            "chat_id": to,
                            "task_id": "",
                            "content": final_response,
                            "tool_calls": 0,
                            "duration_ms": 0,
                            "media": collected_media,
                            "background_delivery": true,
                            "delivery_kind": "cron",
                            "cron_kind": "agent",
                        });
                        let _ = event_tx.send(event.to_string());
                    }
                    if let Some(tx) = &self.outbound_tx {
                        let mut outbound = OutboundMessage::new(&channel, &to, &final_response);
                        outbound.account_id = msg.account_id.clone();
                        outbound.media = collected_media.clone();
                        let _ = tx.send(outbound).await;
                    }
                } else if let Some(tx) = &self.outbound_tx {
                    let mut outbound = OutboundMessage::new(&channel, &to, &final_response);
                    outbound.account_id = msg.account_id.clone();
                    outbound.media = collected_media.clone();
                    let _ = tx.send(outbound).await;
                }
            }

            return Ok(final_response.to_string());
        }

        if msg.channel == "ws" || msg.channel == "cli" {
            if let Some(ref event_tx) = self.event_tx {
                let event = serde_json::json!({
                    "type": "message_done",
                    "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                    "chat_id": msg.chat_id,
                    "task_id": "",
                    "content": final_response,
                    "tool_calls": 0,
                    "duration_ms": 0,
                    "media": collected_media,
                });
                let _ = event_tx.send(event.to_string());
            }
        }

        if msg.channel != "ghost" {
            if let Some(tx) = &self.outbound_tx {
                let mut outbound =
                    OutboundMessage::new(&msg.channel, &msg.chat_id, &final_response);
                outbound.account_id = msg.account_id.clone();
                outbound.media = collected_media.clone();
                outbound.metadata = extract_reply_metadata(msg);
                let _ = tx.send(outbound).await;
            }
        }

        if msg.channel == "cron" {
            if let Some(deliver) = msg.metadata.get("deliver").and_then(|v| v.as_bool()) {
                if deliver {
                    if let (Some(channel), Some(to)) = (
                        msg.metadata.get("deliver_channel").and_then(|v| v.as_str()),
                        msg.metadata.get("deliver_to").and_then(|v| v.as_str()),
                    ) {
                        if let Some(tx) = &self.outbound_tx {
                            let outbound = OutboundMessage::new(channel, to, &final_response);
                            let _ = tx.send(outbound).await;
                        }
                    }
                }
            }
        }

        Ok(final_response.to_string())
    }

    /// Extracted sub-function (#15): Call LLM with streaming and retry on transient errors.
    /// Returns the LLM response on success, or the last error on exhaustion.
    async fn call_llm_with_retry(
        &mut self,
        current_messages: &[ChatMessage],
        tools: &[serde_json::Value],
        msg: &InboundMessage,
        ghost_recall_context_block: Option<&str>,
        iteration: &HashMap<String, u32>,
        saw_rate_limit_this_turn: &mut bool,
    ) -> std::result::Result<LLMResponse, blockcell_core::Error> {
        let max_retries = self.config.agents.defaults.llm_max_retries;
        let base_delay_ms = self.config.agents.defaults.llm_retry_delay_ms;
        let mut last_error = None;
        let api_messages = append_ephemeral_context_to_latest_user_message(
            current_messages,
            ghost_recall_context_block,
        );

        for attempt in 0..=max_retries {
            if attempt > 0 {
                let delay_ms = base_delay_ms * (1u64 << (attempt - 1).min(4));
                warn!(
                    attempt,
                    max_retries,
                    delay_ms,
                    ?iteration,
                    "Retrying LLM call after transient error"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            let (pool_idx, provider) = match self.provider_pool.acquire() {
                Some(p) => p,
                None => {
                    last_error = Some(blockcell_core::Error::Config(
                        "ProviderPool: no healthy providers available".to_string(),
                    ));
                    break;
                }
            };

            match provider.chat_stream(&api_messages, tools).await {
                Ok(mut stream_rx) => {
                    if attempt > 0 {
                        info!(
                            attempt,
                            ?iteration,
                            pool_idx,
                            "LLM stream call succeeded after retry"
                        );
                    }
                    let mut accumulated_content = String::new();
                    let mut accumulated_reasoning = String::new();
                    let mut tool_call_accumulators: HashMap<String, ToolCallAccumulator> =
                        HashMap::new();
                    let mut emitted_text_delta = false;
                    let mut stream_error: Option<blockcell_core::Error> = None;

                    const STREAM_TIMEOUT_SECS: u64 = 300;

                    loop {
                        let recv_result = tokio::time::timeout(
                            std::time::Duration::from_secs(STREAM_TIMEOUT_SECS),
                            stream_rx.recv(),
                        )
                        .await;

                        match recv_result {
                            Ok(Some(chunk)) => match chunk {
                                StreamChunk::TextDelta { delta } => {
                                    accumulated_content.push_str(&delta);
                                    emitted_text_delta = true;
                                    if let Some(ref event_tx) = self.event_tx {
                                        let event = serde_json::json!({
                                            "type": "token",
                                            "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                                            "chat_id": msg.chat_id.clone(),
                                            "delta": delta,
                                        });
                                        let _ = event_tx.send(event.to_string());
                                    }
                                }
                                StreamChunk::ReasoningDelta { delta } => {
                                    accumulated_reasoning.push_str(&delta);
                                    if let Some(ref event_tx) = self.event_tx {
                                        let event = serde_json::json!({
                                            "type": "thinking",
                                            "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                                            "chat_id": msg.chat_id.clone(),
                                            "content": delta,
                                        });
                                        let _ = event_tx.send(event.to_string());
                                    }
                                }
                                StreamChunk::ToolCallStart { index: _, id, name } => {
                                    let acc = tool_call_accumulators.entry(id.clone()).or_default();
                                    acc.id = id.clone();
                                    acc.name = name.clone();
                                }
                                StreamChunk::ToolCallDelta {
                                    index: _,
                                    id,
                                    delta,
                                } => {
                                    if let Some(acc) = tool_call_accumulators.get_mut(&id) {
                                        acc.arguments.push_str(&delta);
                                    }
                                }
                                StreamChunk::Done { response } => {
                                    let final_tool_calls = if !tool_call_accumulators.is_empty() {
                                        tool_call_accumulators
                                            .drain()
                                            .map(|(_, acc)| acc.to_tool_call_request())
                                            .collect()
                                    } else {
                                        response.tool_calls.clone()
                                    };

                                    let final_content = if !accumulated_content.is_empty() {
                                        Some(accumulated_content.clone())
                                    } else {
                                        response.content.clone()
                                    };

                                    let final_reasoning = if !accumulated_reasoning.is_empty() {
                                        Some(accumulated_reasoning.clone())
                                    } else {
                                        response.reasoning_content.clone()
                                    };

                                    return Ok(LLMResponse {
                                        content: final_content,
                                        reasoning_content: final_reasoning,
                                        tool_calls: final_tool_calls,
                                        finish_reason: response.finish_reason.clone(),
                                        usage: response.usage.clone(),
                                    });
                                }
                                StreamChunk::Error { message } => {
                                    warn!(error = %message, "Stream error");
                                    stream_error = Some(blockcell_core::Error::Provider(message));
                                    break;
                                }
                            },
                            Ok(None) => {
                                break;
                            }
                            Err(_) => {
                                warn!(
                                    "Stream receive timeout after {} seconds",
                                    STREAM_TIMEOUT_SECS
                                );
                                stream_error = Some(blockcell_core::Error::Provider(format!(
                                    "Stream timeout after {} seconds",
                                    STREAM_TIMEOUT_SECS
                                )));
                                break;
                            }
                        }
                    }

                    // Fallback: tolerate providers that close the stream cleanly without an
                    // explicit Done event. If the stream ended with an error, retry instead of
                    // committing a partial answer.
                    if stream_error.is_none()
                        && (!tool_call_accumulators.is_empty() || !accumulated_content.is_empty())
                    {
                        self.provider_pool.report(pool_idx, CallResult::Success);
                        let final_tool_calls: Vec<ToolCallRequest> = tool_call_accumulators
                            .into_values()
                            .map(|acc| acc.to_tool_call_request())
                            .collect();

                        return Ok(LLMResponse {
                            content: if accumulated_content.is_empty() {
                                None
                            } else {
                                Some(accumulated_content)
                            },
                            reasoning_content: if accumulated_reasoning.is_empty() {
                                None
                            } else {
                                Some(accumulated_reasoning)
                            },
                            tool_calls: final_tool_calls,
                            finish_reason: "stop".to_string(),
                            usage: serde_json::Value::Null,
                        });
                    }

                    if emitted_text_delta {
                        if let Some(ref event_tx) = self.event_tx {
                            let event = serde_json::json!({
                                "type": "stream_reset",
                                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                                "chat_id": msg.chat_id.clone(),
                            });
                            let _ = event_tx.send(event.to_string());
                        }
                    }

                    let err = stream_error.unwrap_or_else(|| {
                        blockcell_core::Error::Provider(
                            "Stream ended unexpectedly before completion".to_string(),
                        )
                    });
                    let err_str = format!("{}", err);
                    let call_result = ProviderPool::classify_error(&err_str);
                    if matches!(&call_result, CallResult::RateLimit) {
                        *saw_rate_limit_this_turn = true;
                    }
                    self.provider_pool.report(pool_idx, call_result);
                    last_error = Some(err);
                }
                Err(e) => {
                    let err_str = format!("{}", e);
                    warn!(error = %err_str, attempt, max_retries, ?iteration, pool_idx, "LLM stream call failed");
                    let call_result = ProviderPool::classify_error(&err_str);
                    if matches!(&call_result, CallResult::RateLimit) {
                        *saw_rate_limit_this_turn = true;
                    }
                    self.provider_pool.report(pool_idx, call_result);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            blockcell_core::Error::Provider("LLM call failed with no error details".to_string())
        }))
    }

    pub async fn process_message(&mut self, msg: InboundMessage) -> Result<String> {
        let mut metrics = ProcessingMetrics::new();
        let session_key = msg.session_key();
        let cron_deliver_target = resolve_cron_deliver_target(&msg);
        let persist_session_key = if let Some((channel, to)) = &cron_deliver_target {
            blockcell_core::build_session_key(channel, to)
        } else {
            session_key.clone()
        };
        info!(session_key = %session_key, "Processing message");
        info!(target: "chat::user", content = %msg.content, "User input");
        self.update_main_session_target(&msg).await;
        if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
            let turn_number = self
                .session_store
                .load(&session_key)
                .map(|history| {
                    history
                        .iter()
                        .filter(|message| message.role == "user")
                        .count() as u32
                        + 1
                })
                .unwrap_or(1);
            manager.on_turn_start(turn_number, &msg.content, &session_key);
        }

        // Learning Coordinator: record user turn (replaces skill_nudge_engine.record_user_turn)
        // Only real user messages increment counters (not cron/system/heartbeat)
        if msg.channel != "system" && msg.channel != "cron" {
            self.learning_coordinator.on_turn_start(true);
        }

        // ── Refresh memory injector cache if Layer 5 extraction completed ──
        if let Err(e) = self.reload_memory_injector_if_needed().await {
            warn!(error = %e, "[Layer 5] Failed to reload memory injector cache");
        }

        // ── Record sender as a known channel contact (for cross-channel lookup) ──
        if msg.channel != "ws" && msg.channel != "cli" && msg.channel != "system" {
            let sender_name = msg
                .metadata
                .get("sender_nick")
                .and_then(|v| v.as_str())
                .or_else(|| msg.metadata.get("username").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            let chat_type = match msg
                .metadata
                .get("conversation_type")
                .and_then(|v| v.as_str())
            {
                Some("1") => "private",
                Some("2") => "group",
                _ => {
                    if msg
                        .metadata
                        .get("is_group")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        "group"
                    } else if msg.sender_id == msg.chat_id {
                        "private"
                    } else {
                        "group"
                    }
                }
            };
            self.channel_contacts
                .upsert(blockcell_storage::ChannelContact {
                    channel: msg.channel.clone(),
                    chat_id: msg.chat_id.clone(),
                    sender_id: msg.sender_id.clone(),
                    name: sender_name,
                    chat_type: chat_type.to_string(),
                    last_active: chrono::Utc::now().to_rfc3339(),
                });
        }

        // ── Cron reminder fast path: deliver directly without LLM ──
        if msg
            .metadata
            .get("reminder")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            let reminder_msg = msg
                .metadata
                .get("reminder_message")
                .and_then(|v| v.as_str())
                .unwrap_or(&msg.content);
            let job_name = msg
                .metadata
                .get("job_name")
                .and_then(|v| v.as_str())
                .unwrap_or("提醒");
            let final_response = format!("⏰ [{}] {}", job_name, reminder_msg);
            info!(job_name = %job_name, "Cron reminder delivered directly (bypassing LLM)");

            // Don't store reminder message in history to prevent LLM from learning the format
            // Users can view their scheduled tasks via `cron list` tool

            // Send to outbound (CLI printer + gateway's outbound_to_ws_bridge)
            if let Some(tx) = &self.outbound_tx {
                let mut outbound =
                    OutboundMessage::new(&msg.channel, &msg.chat_id, &final_response);
                outbound.account_id = msg.account_id.clone();
                let _ = tx.send(outbound).await;
            }

            // Deliver to external channel if configured
            if let Some(true) = msg.metadata.get("deliver").and_then(|v| v.as_bool()) {
                if let (Some(channel), Some(to)) = (
                    msg.metadata.get("deliver_channel").and_then(|v| v.as_str()),
                    msg.metadata.get("deliver_to").and_then(|v| v.as_str()),
                ) {
                    if channel == "ws" {
                        if let Some(ref event_tx) = self.event_tx {
                            let event = serde_json::json!({
                                "type": "message_done",
                                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                                "chat_id": to,
                                "task_id": "",
                                "content": final_response,
                                "tool_calls": 0,
                                "duration_ms": 0,
                                "media": [],
                                "background_delivery": true,
                                "delivery_kind": "cron",
                                "cron_kind": "reminder",
                            });
                            let _ = event_tx.send(event.to_string());
                        }
                    }
                    if let Some(tx) = &self.outbound_tx {
                        let outbound = OutboundMessage::new(channel, to, &final_response);
                        let _ = tx.send(outbound).await;
                    }
                }
            }

            return Ok(final_response);
        }

        // ── Handle manual compact request from /compact command ──
        if msg.content == "__COMPACT_REQUEST__" {
            info!(
                session_key = %session_key,
                channel = %msg.channel,
                "[compact] Manual compact request received"
            );

            let compact_ctx = CompactContext {
                channel: &msg.channel,
                chat_id: &msg.chat_id,
                account_id: msg.account_id.as_deref(),
            };

            // Load session history for compact
            let history = self.session_store.load(&session_key)?;
            if let Err(e) = self
                .capture_pre_compress_learning_boundary(&session_key, &history)
                .await
            {
                warn!(error = %e, session_key = %session_key, "Ghost learning pre-compress capture failed");
            }

            // Execute compact directly (is_auto=false for manual trigger)
            let result = self
                .execute_layer4_compact(&history, &session_key, Some(compact_ctx), false)
                .await;

            if result.success {
                // Store compacted history
                let mut compacted_messages = vec![
                    ChatMessage::system(&result.to_compact_message()),
                    ChatMessage::user("请继续当前任务。"),
                ];
                compacted_messages.extend(result.recent_messages);
                self.session_store.save(&session_key, &compacted_messages)?;

                // Clear trackers
                if let Some(ms) = self.memory_system.as_mut() {
                    ms.file_tracker_mut().clear();
                    ms.skill_tracker_mut().clear();
                }

                // Record compression metrics
                metrics.record_compression();

                // Send WebSocket notification for ws channel
                if msg.channel == "ws" {
                    if let Some(ref event_tx) = self.event_tx {
                        let notification_content = if result.pre_compact_tokens > 0 {
                            let compression_ratio =
                                (result.pre_compact_tokens - result.post_compact_tokens) as f64
                                    / result.pre_compact_tokens as f64
                                    * 100.0;
                            format!(
                                "✅ 已压缩对话历史，保留关键信息。\n📊 Token: {} → {} (压缩 {:.0}%)",
                                result.pre_compact_tokens,
                                result.post_compact_tokens,
                                compression_ratio
                            )
                        } else {
                            "✅ 压缩完成（无历史内容需要压缩）".to_string()
                        };
                        let event = serde_json::json!({
                            "type": "message_done",
                            "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                            "chat_id": msg.chat_id,
                            "task_id": "",
                            "content": notification_content,
                            "tool_calls": 0,
                            "duration_ms": 0,
                            "media": [],
                            "is_markdown": true,
                        });
                        let _ = event_tx.send(event.to_string());
                    }
                }
            } else {
                // Log failure for debugging
                warn!(
                    session_key = %session_key,
                    reason = result.error.as_deref().unwrap_or("unknown"),
                    "[compact] Manual compact request failed"
                );

                // Send failure notification
                if msg.channel == "ws" {
                    if let Some(ref event_tx) = self.event_tx {
                        let error_msg = result.error.as_deref().unwrap_or("压缩失败，请稍后重试。");
                        let notification_content = format!("⚠️ 压缩失败: {}", error_msg);
                        let event = serde_json::json!({
                            "type": "message_done",
                            "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                            "chat_id": msg.chat_id,
                            "task_id": "",
                            "content": notification_content,
                            "tool_calls": 0,
                            "duration_ms": 0,
                            "media": [],
                            "is_markdown": true,
                        });
                        let _ = event_tx.send(event.to_string());
                    }
                } else if let Some(ref tx) = &self.outbound_tx {
                    let error_msg = result.error.as_deref().unwrap_or("压缩失败，请稍后重试。");
                    let notification_content = format!("⚠️ 压缩失败: {}", error_msg);
                    let mut notification =
                        OutboundMessage::new(&msg.channel, &msg.chat_id, &notification_content);
                    if let Some(aid) = msg.account_id.as_deref() {
                        notification.account_id = Some(aid.to_string());
                    }
                    let _ = tx.send(notification).await;
                }
            }

            return Ok(String::new());
        }

        // Load session history
        let mut history = self.session_store.load(&session_key)?;
        let mut session_metadata = self.session_store.load_metadata(&persist_session_key)?;
        if let Err(err) = self.apply_learned_skill_negative_feedback(&mut session_metadata, &msg) {
            warn!(
                error = %err,
                session_key = %persist_session_key,
                "Learned skill negative feedback handling failed"
            );
        }

        // Layer 2: 时间触发的轻量压缩
        // 检查会话最后更新时间，如果超过阈值则清理旧工具结果
        let time_config = TimeBasedMCConfig::from(self.config.memory.memory_system.layer2.clone());
        if let Some(updated_at_str) = session_metadata.get("updated_at").and_then(|v| v.as_str()) {
            if let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(updated_at_str) {
                let last_assistant_timestamp = Some(updated_at.with_timezone(&chrono::Utc));
                let projector = HistoryProjector::new(&history);

                // 应用时间触发的轻量压缩
                if let Some(compacted) = projector.time_based_microcompact(
                    last_assistant_timestamp,
                    None, // 主线程来源
                    &time_config,
                ) {
                    tracing::info!(
                        original_count = history.len(),
                        compacted_count = compacted.len(),
                        gap_threshold_minutes = time_config.gap_threshold_minutes,
                        "[layer2] time-based microcompact applied"
                    );
                    history = compacted;
                }
            }
        }

        // Auto-set session display name from first user message
        if history.is_empty() {
            if let Some(new_name) = self
                .session_store
                .set_session_name_if_new(&session_key, &msg.content)
            {
                if msg.channel == "ws" {
                    if let Some(ref event_tx) = self.event_tx {
                        let event = serde_json::json!({
                            "type": "session_renamed",
                            "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                            "chat_id": msg.chat_id,
                            "name": new_name,
                        });
                        let _ = event_tx.send(event.to_string());
                    }
                }
            }
        }

        // 配置文件中有自定义意图规则时，叠加到内置规则上；否则使用全局单例（避免重复编译正则）
        let config_intent_rules = self
            .config
            .intent_router
            .as_ref()
            .map(|r| r.intent_rules.as_slice())
            .unwrap_or(&[]);
        let _classifier_owned;
        let classifier: &crate::intent::IntentClassifier = if config_intent_rules.is_empty() {
            crate::intent::IntentClassifier::global()
        } else {
            _classifier_owned =
                crate::intent::IntentClassifier::with_extra_rules(config_intent_rules);
            &_classifier_owned
        };

        // Load disabled toggles for filtering
        let disabled_tools = load_disabled_toggles(&self.paths, "tools");
        let disabled_skills = load_disabled_toggles(&self.paths, "skills");
        let recent_skill_name = continued_skill_name(&session_metadata, &history);
        let _ = self.context_builder.reload_skills();
        let skill_cards = self
            .context_builder
            .skill_manager()
            .map(|manager| manager.list_enabled_skill_cards(&disabled_skills))
            .unwrap_or_default();

        let decision_timer = ScopedTimer::new();
        let decision = self
            .decide_interaction(
                &msg,
                &disabled_skills,
                classifier,
                &history,
                &session_metadata,
            )
            .await?;
        metrics.record_decision(decision_timer.elapsed_ms());
        if let Some(result) = self
            .execute_decided_skill_route(&decision, &msg, &persist_session_key)
            .await
        {
            return result;
        }

        let available_tools: HashSet<String> =
            self.tool_registry.tool_names().into_iter().collect();

        let routed_agent_id = self.agent_id.as_deref();
        let mut tool_names = resolve_effective_tool_names(
            &self.config,
            decision.mode,
            routed_agent_id,
            decision.active_skill.as_ref(),
            &decision.chat_intents,
            &available_tools,
        );

        if tool_names.is_empty() && !matches!(decision.mode, InteractionMode::Chat) {
            tool_names = global_core_tool_names();
            tool_names.retain(|name| available_tools.contains(name));
        }

        // Ghost routine: ensure required tools are always available.
        // Rationale: intent classification may treat the routine prompt as Chat, producing zero tools,
        // which would cause the LLM to think tools are unavailable.
        if msg.metadata.get("ghost").and_then(|v| v.as_bool()) == Some(true) {
            let required = [
                "community_hub",
                "memory_maintenance",
                "memory_query",
                "memory_upsert",
                "list_dir",
                "read_file",
                "file_ops",
                "notification",
            ];
            for name in required {
                if !tool_names.iter().any(|tool_name| tool_name == name) {
                    tool_names.push(name.to_string());
                }
            }
        }

        if !skill_cards.is_empty()
            && !tool_names
                .iter()
                .any(|name| name == ACTIVATE_SKILL_TOOL_NAME)
        {
            tool_names.push(ACTIVATE_SKILL_TOOL_NAME.to_string());
        }

        let provider_tool_schemas = ghost_memory_provider_tool_schemas(
            self.ghost_memory_lifecycle.as_deref(),
            &disabled_tools,
        );
        let provider_tool_names = provider_tool_schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        tool_names.extend(provider_tool_names);

        tool_names.sort();
        tool_names.dedup();

        // Collect tool-specific prompt rules from the registry for actually loaded tools.
        let mode_names: Vec<String> = match decision.mode {
            InteractionMode::Skill => decision
                .active_skill
                .as_ref()
                .map(|skill| vec![format!("Skill:{}", skill.name)])
                .unwrap_or_else(|| vec!["Skill".to_string()]),
            InteractionMode::Chat => vec!["Chat".to_string()],
            InteractionMode::General => vec!["General".to_string()],
        };
        let prompt_ctx = blockcell_tools::PromptContext {
            channel: &msg.channel,
            intents: &mode_names,
            default_timezone: self.config.default_timezone.as_deref(),
        };
        let tool_name_refs: Vec<&str> = tool_names.iter().map(|s| s.as_str()).collect();
        let mut tool_prompt_rules = self
            .tool_registry
            .get_prompt_rules(&tool_name_refs, &prompt_ctx);
        // MCP meta-rule: inject if any loaded tool is an MCP tool (name contains "__")
        if tool_names.iter().any(|t| t.contains("__")) {
            tool_prompt_rules.push("- **MCP (Model Context Protocol)**: blockcell **已内置 MCP 客户端支持**，可连接任意 MCP 服务器（SQLite、GitHub、文件系统、数据库等）。MCP 工具会以 `<serverName>__<toolName>` 格式出现在工具列表中。若用户询问 MCP 功能或当前工具列表中无 MCP 工具，说明尚未配置 MCP 服务器，请引导用户使用 `blockcell mcp add <template>` 快捷添加，或直接编辑 `~/.blockcell/mcp.json` / `~/.blockcell/mcp.d/*.json`。例如：`blockcell mcp add sqlite --db-path /tmp/test.db`，重启后即可使用。".to_string());
        }

        // Build messages for LLM with skill-first mode prompt.
        // Note: build_messages_for_mode_with_channel appends the current user message from user_content,
        // so we pass history WITHOUT the current user message to avoid duplication.
        let pending_intent = msg
            .metadata
            .get("media_pending_intent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut messages = self
            .context_builder
            .build_messages_for_session_mode_with_channel(
                &session_key,
                &history,
                &msg.content,
                &msg.media,
                decision.mode,
                decision.active_skill.as_ref(),
                &disabled_skills,
                &disabled_tools,
                &msg.channel,
                pending_intent,
                &tool_names,
                &tool_prompt_rules,
            );
        if decision.active_skill.is_none() {
            inject_skill_cards_into_system_prompt(
                &mut messages,
                &skill_cards,
                recent_skill_name.as_deref(),
            );
        }

        // Now add user message to history for session persistence
        history.push(ChatMessage::user(&msg.content));

        // Layer 4: Initialize memory system if needed
        let needs_memory_system_init = self
            .memory_system
            .as_ref()
            .map(|memory_system| memory_system.session_id() != session_key)
            .unwrap_or(true);
        if needs_memory_system_init {
            if let Err(e) = self.init_memory_system(session_key.clone()).await {
                warn!(error = %e, "[layer4] Failed to initialize memory system");
            }
        }

        // Layer 5: Initialize memory injector if needed (load persistent memory files)
        if self.context_builder.memory_injector().is_none() {
            if let Err(e) = self.init_memory_injector().await {
                warn!(error = %e, "[layer5] Failed to initialize memory injector");
            }
        }

        // Get tool schemas from resolved tool names
        let mut tools = if tool_names.is_empty() {
            // Chat mode: no tools
            vec![]
        } else {
            let tool_name_refs: Vec<&str> = tool_names.iter().map(String::as_str).collect();
            let mut schemas = self.tool_registry.get_tiered_schemas(
                &tool_name_refs,
                blockcell_tools::registry::global_core_tool_names(),
            );

            if !disabled_tools.is_empty() {
                schemas.retain(|schema| {
                    let name = schema
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    !disabled_tools.contains(name)
                });
            }
            schemas
        };
        if let Some(schema) = build_activate_skill_tool_schema(&skill_cards) {
            tools.push(schema);
        }
        tools.extend(provider_tool_schemas);
        info!(
            mode = ?decision.mode,
            active_skill = decision.active_skill.as_ref().map(|s| s.name.as_str()),
            tool_count = tools.len(),
            disabled_tools = disabled_tools.len(),
            disabled_skills = disabled_skills.len(),
            "Tools loaded for interaction mode"
        );

        // Main loop with max iterations
        let max_iterations = self.config.agents.defaults.max_tool_iterations;
        let tools_max_iterations = self
            .config
            .agents
            .defaults
            .max_tool_iterations_by_tool
            .clone();
        let mut tool_call_counts: HashMap<String, u32> = HashMap::new();
        let mut over_iteration: bool = false;
        let mut current_messages = messages;

        // Layer 1: 消息级别预算检查
        // 如果工具结果总和超过预算，持久化最大的结果
        if let Some(memory_system) = self.memory_system.as_ref() {
            let candidates =
                crate::response_cache::collect_tool_result_candidates(&current_messages);
            if !candidates.is_empty() {
                let total_size: usize = candidates.iter().map(|c| c.size).sum();
                let budget = self
                    .memory_system
                    .as_ref()
                    .map(|ms| ms.config().layer1.max_tool_results_per_message_chars)
                    .unwrap_or(crate::response_cache::MAX_TOOL_RESULTS_PER_MESSAGE_CHARS);
                let preview_size_bytes = self
                    .memory_system
                    .as_ref()
                    .map(|ms| ms.config().layer1.preview_size_bytes)
                    .unwrap_or(crate::response_cache::PREVIEW_SIZE_BYTES);

                if total_size > budget {
                    debug!(
                        total_size = total_size,
                        budget = budget,
                        candidates_count = candidates.len(),
                        "[layer1] Message budget exceeded, applying budget"
                    );

                    let state = memory_system.content_replacement_state().clone();
                    let mut state_mut = state.clone();

                    current_messages = crate::response_cache::apply_budget_async(
                        &current_messages,
                        &candidates,
                        &mut state_mut,
                        budget,
                        &self.paths.base,
                        &session_key,
                        preview_size_bytes,
                    )
                    .await;

                    // 更新状态
                    if let Some(ms) = self.memory_system.as_mut() {
                        *ms.content_replacement_state_mut() = state_mut;
                    }
                }
            }
        }

        // Layer 4: 第一次 LLM 调用前的 Compact 检查
        // 如果从磁盘恢复的历史已经超过阈值，先压缩再进入主循环
        {
            let estimated_tokens = estimate_messages_tokens(&current_messages);
            // Update Layer 4 token usage metrics
            if let Some(memory_system) = self.memory_system.as_ref() {
                crate::memory_event!(
                    layer4,
                    token_usage,
                    estimated_tokens,
                    memory_system.config().token_budget,
                    memory_system.config().layer4.compact_threshold_ratio
                );
                if memory_system.should_compact(estimated_tokens) {
                    info!(
                        estimated_tokens,
                        token_budget = memory_system.config().token_budget,
                        threshold = memory_system.config().layer4.compact_threshold_ratio,
                        "[layer4] Pre-loop compact check triggered"
                    );

                    let compact_ctx = CompactContext {
                        channel: &msg.channel,
                        chat_id: &msg.chat_id,
                        account_id: msg.account_id.as_deref(),
                    };
                    if let Err(e) = self
                        .capture_pre_compress_learning_boundary(&session_key, &current_messages)
                        .await
                    {
                        warn!(error = %e, session_key = %session_key, "Ghost learning pre-compress capture failed");
                    }
                    let compact_result = self
                        .execute_layer4_compact(
                            &current_messages,
                            &session_key,
                            Some(compact_ctx),
                            true, // is_auto for automatic compact
                        )
                        .await;
                    if compact_result.success {
                        current_messages.clear();
                        current_messages
                            .push(ChatMessage::system(&compact_result.to_compact_message()));
                        current_messages.push(ChatMessage::user("请继续当前任务。"));

                        current_messages.extend(compact_result.recent_messages);

                        info!(
                            post_compact_tokens = estimate_messages_tokens(&current_messages),
                            "[layer4] Pre-loop compact completed"
                        );
                        metrics.record_compression();

                        // Compact 成功后清空追踪器，防止下次 Compact 重复恢复
                        if let Some(ms) = self.memory_system.as_mut() {
                            ms.file_tracker_mut().clear();
                            ms.skill_tracker_mut().clear();
                        }
                    } else {
                        warn!(
                            error = ?compact_result.error,
                            "[layer4] Pre-loop compact failed"
                        );
                    }
                }
            }
        }

        let ghost_recall_context_block = if should_inject_ghost_recall(&self.config, &msg) {
            if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
                let learning = &self.config.agents.ghost.learning;
                manager.prefetch_all_as_context_block(
                    &msg.content,
                    &session_key,
                    learning.recall_max_items as usize,
                    learning.recall_token_budget as usize,
                )
            } else {
                None
            }
        } else {
            None
        };

        let mut final_response = String::new();
        let mut message_tool_sent_media = false;
        let mut tool_fail_counts: HashMap<String, u32> = HashMap::new();
        let mut resource_missing_hints_sent: HashSet<String> = HashSet::new();
        let mut should_throttle_next_tool_round = false;
        let mut saw_rate_limit_this_turn = false;
        // Collect media paths produced by tools (screenshots, generated images, etc.)
        let mut collected_media: Vec<String> = Vec::new();

        // Schema cache flag: tools are loaded once before the loop.
        // Only dynamic supplement (below) mutates the `tools` vec — no redundant reload.
        let mut _schema_cache_dirty = false;

        // 延迟 Review 状态 (与 Hermes 一致: 在响应发送后触发后台 Review)
        let mut deferred_review_mode: Option<ReviewMode> = None;
        let mut deferred_review_snapshot: Vec<ChatMessage> = Vec::new();

        // Memory Nudge: check before LLM loop (replaces skill_nudge_engine.check_memory_nudge)
        // Memory nudge is based on user turns, not tool iterations
        {
            let has_memory_store = self.memory_store.is_some();
            if let Some(_memory_trigger) = self
                .learning_coordinator
                .check_memory_nudge(has_memory_store)
            {
                deferred_review_mode = Some(ReviewMode::Memory);
                deferred_review_snapshot = current_messages.clone();
            }
        }

        loop {
            debug!(iteration = ?tool_call_counts, "LLM call iteration");
            // Learning Coordinator: record iteration (replaces skill_nudge_engine.record_iteration)
            self.learning_coordinator.record_iteration();

            debug!(
                iteration = ?tool_call_counts,
                current_messages_len = current_messages.len(),
                tool_schema_count = tools.len(),
                "LLM loop state"
            );

            if should_throttle_next_tool_round {
                let delay = tool_round_throttle_delay(saw_rate_limit_this_turn);
                info!(
                    iteration = ?tool_call_counts,
                    delay_ms = delay.as_millis() as u64,
                    saw_rate_limit_this_turn,
                    "Throttling next LLM call after tool round"
                );
                tokio::time::sleep(delay).await;
                should_throttle_next_tool_round = false;
            }

            // Call LLM with extracted sub-function (#15)
            let llm_timer = ScopedTimer::new();
            let llm_result = self
                .call_llm_with_retry(
                    &current_messages,
                    &tools,
                    &msg,
                    ghost_recall_context_block.as_deref(),
                    &tool_call_counts,
                    &mut saw_rate_limit_this_turn,
                )
                .await;
            metrics.record_llm_call(llm_timer.elapsed_ms());

            let response = match llm_result {
                Ok(r) => r,
                Err(e) => {
                    let max_retries = self.config.agents.defaults.llm_max_retries;
                    warn!(error = %e, iteration = ?tool_call_counts, retries = max_retries, "LLM call failed after all retries");
                    final_response = llm_exhausted_error(max_retries, &e);
                    if let Some(evo_service) = self.context_builder.evolution_service() {
                        let _ = evo_service
                            .report_error("__llm_provider__", &format!("{}", e), None, vec![])
                            .await;
                    }
                    history.push(ChatMessage::assistant(&final_response));
                    break;
                }
            };

            info!(
                content_len = response.content.as_ref().map(|c| c.len()).unwrap_or(0),
                tool_calls_count = response.tool_calls.len(),
                finish_reason = %response.finish_reason,
                "LLM response received"
            );
            debug!(target: "chat::response", response = serde_json::to_string(&response).unwrap_or_default(), "Response detail");

            // Handle tool calls
            if !response.tool_calls.is_empty() {
                let short_circuit_after_tools = is_im_channel(&msg.channel)
                    && response.tool_calls.iter().all(|c| c.name == "message")
                    && response.tool_calls.iter().all(|c| {
                        let ch = c.arguments.get("channel").and_then(|v| v.as_str());
                        let to = c.arguments.get("chat_id").and_then(|v| v.as_str());
                        ch.map(|s| s == msg.channel).unwrap_or(true)
                            && to.map(|s| s == msg.chat_id).unwrap_or(true)
                    });
                let activate_skill_call = response
                    .tool_calls
                    .iter()
                    .find(|call| call.name == ACTIVATE_SKILL_TOOL_NAME)
                    .cloned();

                // Add assistant message with tool calls — use direct struct literal
                // to atomically preserve reasoning_content and tool_calls, avoiding
                // the fragile create-then-mutate pattern that silently loses data
                // if any field assignment is accidentally removed.
                let assistant_content = response.content.as_deref().unwrap_or("");
                let assistant_content = if is_tool_trace_content(assistant_content) {
                    ""
                } else {
                    assistant_content
                };
                let assistant_msg = ChatMessage {
                    id: Some(uuid::Uuid::new_v4().to_string()),
                    role: "assistant".to_string(),
                    content: serde_json::Value::String(assistant_content.to_string()),
                    reasoning_content: response.reasoning_content.clone(),
                    tool_calls: Some(response.tool_calls.clone()),
                    tool_call_id: None,
                    name: None,
                };
                current_messages.push(assistant_msg.clone());
                history.push(assistant_msg);

                if let Some(skill_call) = activate_skill_call {
                    if response.tool_calls.len() > 1 {
                        warn!(
                            tool_calls = response.tool_calls.len(),
                            "activate_skill was returned with additional tool calls; only the skill activation will be executed"
                        );
                    }

                    let raw_skill_name = skill_call
                        .arguments
                        .get("skill_name")
                        .and_then(|value| value.as_str())
                        .unwrap_or("");
                    let skill_name = normalize_selected_skill_name(raw_skill_name, &skill_cards)
                        .ok_or_else(|| {
                            blockcell_core::Error::Skill(format!(
                                "Model selected unavailable skill '{}'",
                                raw_skill_name
                            ))
                        })?;
                    let goal = skill_call
                        .arguments
                        .get("goal")
                        .and_then(|value| value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .unwrap_or(msg.content.as_str())
                        .to_string();
                    let skill_ctx = self
                        .context_builder
                        .resolve_active_skill_by_name(&skill_name, &disabled_skills)
                        .map(|skill| {
                            suppress_prompt_reinjection_for_continued_skill(
                                skill,
                                recent_skill_name.as_deref(),
                            )
                        })
                        .ok_or_else(|| {
                            blockcell_core::Error::Skill(format!(
                                "Skill '{}' is not available",
                                skill_name
                            ))
                        })?;

                    // Layer 4: Track skill activation for Post-Compact recovery
                    if let Some(memory_system) = self.memory_system.as_mut() {
                        memory_system.record_skill_load(&skill_ctx.name, &skill_ctx.prompt_md);
                        debug!(skill_name = %skill_ctx.name, "[layer4] Tracked skill activation for recovery");
                    }

                    let skill_history_seed = history[..history.len().saturating_sub(1)].to_vec();
                    let (skill_result, updated_metadata, allowed_tools) = self
                        .run_skill_for_turn(
                            &skill_ctx,
                            &msg,
                            &skill_history_seed,
                            &persist_session_key,
                        )
                        .await?;
                    session_metadata = updated_metadata;
                    record_active_skill_name(&mut session_metadata, &skill_ctx.name);
                    append_activated_skill_history(
                        &mut history,
                        &skill_call.id,
                        &skill_ctx.name,
                        &goal,
                        &allowed_tools,
                        &skill_result.trace_messages,
                        &skill_result.final_response,
                    );
                    final_response = skill_result.final_response;
                    break;
                }

                // Execute each tool call, with dynamic tool supplement for intent misclassification
                let mut supplemented_tools = false;
                let mut tool_results: Vec<ChatMessage> = Vec::new();
                let mut wants_forced_answer = false;
                let mut web_search_thin_results: Vec<String> = Vec::new(); // URLs from thin search results
                for tool_call in &response.tool_calls {
                    if tool_call.name == "web_search" || tool_call.name == "web_fetch" {
                        wants_forced_answer = true;
                    }
                    // Check message tool has media BEFORE execution (for message_tool_sent_media flag only)
                    if tool_call.name == "message" {
                        let has_media = tool_call
                            .arguments
                            .get("media")
                            .and_then(|v| v.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false);
                        if has_media {
                            message_tool_sent_media = true;
                        }
                    }
                    let tool_timer = ScopedTimer::new();
                    let result = if tool_names.iter().any(|allowed| allowed == &tool_call.name) {
                        let max_iterations = tools_max_iterations
                            .get(&tool_call.name)
                            .copied()
                            .unwrap_or(max_iterations);
                        let count = tool_call_counts.entry(tool_call.name.clone()).or_insert(0);

                        *count += 1;
                        if *count > max_iterations {
                            over_iteration = true;
                            serde_json::json!({
                                "error": format!(
                                    "Tool '{}' execeeded max call limit ({}).",
                                    tool_call.name, max_iterations
                                ),
                                "tool": tool_call.name,
                                "hint": "Reduce repeated tool calls or adjust maxToolIterationsByTool."
                            })
                            .to_string()
                        } else {
                            self.execute_tool_call(tool_call, &msg, None).await
                        }
                    } else {
                        scoped_tool_denied_result(&tool_call.name)
                    };

                    metrics.record_tool_execution(&tool_call.name, tool_timer.elapsed_ms());

                    // Collect media paths from tool results for WebUI display.
                    // Skip the "message" tool — it already dispatches its own OutboundMessage
                    // with media; collecting here would cause a duplicate send.
                    if tool_call.name != "message" {
                        if let Ok(ref rv) = serde_json::from_str::<serde_json::Value>(&result) {
                            let media_exts = [
                                "png", "jpg", "jpeg", "gif", "webp", "bmp", "svg", "mp3", "wav",
                                "m4a", "mp4", "webm", "mov",
                            ];
                            // Scalar fields: output_path, path, file_path, etc.
                            for key in &[
                                "output_path",
                                "path",
                                "file_path",
                                "screenshot_path",
                                "image_path",
                            ] {
                                if let Some(p) = rv.get(key).and_then(|v| v.as_str()) {
                                    let ext = p.rsplit('.').next().unwrap_or("").to_lowercase();
                                    if media_exts.contains(&ext.as_str()) {
                                        collected_media.push(p.to_string());
                                    }
                                }
                            }
                            // Array field: "media"
                            if let Some(arr) = rv.get("media").and_then(|v| v.as_array()) {
                                for mv in arr {
                                    if let Some(p) = mv.as_str() {
                                        let ext = p.rsplit('.').next().unwrap_or("").to_lowercase();
                                        if media_exts.contains(&ext.as_str()) {
                                            collected_media.push(p.to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Detect thin web_search results (only titles/URLs, no actual content).
                    // When this happens, extract the top URLs so the next hint can suggest web_fetch.
                    if tool_call.name == "web_search"
                        && !result.starts_with("Tool error:")
                        && is_thin_search_result(&result)
                    {
                        let urls = extract_urls_from_search_result(&result);
                        if !urls.is_empty() {
                            web_search_thin_results.extend(urls);
                        }
                    }

                    // Dynamic tool supplement: if tool was not found or validation failed
                    // (e.g. lightweight schema had no params), inject full schema and retry.
                    let needs_supplement = should_supplement_tool_schema(&result);
                    if needs_supplement {
                        if let Some(schema) = self.tool_registry.get(&tool_call.name) {
                            // Check if we need to upgrade from lightweight to full schema
                            let already_full = tools.iter().any(|t| {
                                t.get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|n| n.as_str())
                                    == Some(&tool_call.name)
                                    && t.get("function")
                                        .and_then(|f| f.get("parameters"))
                                        .and_then(|p| p.get("properties"))
                                        .map(|props| {
                                            props.as_object().is_some_and(|o| !o.is_empty())
                                        })
                                        .unwrap_or(false)
                            });
                            if !already_full {
                                let schema_val = serde_json::json!({
                                    "type": "function",
                                    "function": {
                                        "name": schema.schema().name,
                                        "description": schema.schema().description,
                                        "parameters": schema.schema().parameters
                                    }
                                });
                                // Replace lightweight schema with full schema
                                tools.retain(|t| {
                                    t.get("function")
                                        .and_then(|f| f.get("name"))
                                        .and_then(|n| n.as_str())
                                        != Some(&tool_call.name)
                                });
                                tools.push(schema_val);
                                supplemented_tools = true;
                                _schema_cache_dirty = true;
                                info!(tool = %tool_call.name, "Dynamically supplemented tool with full schema");
                                break;
                            }
                        }
                    }

                    // Track tool failures with transient/permanent classification (#6)
                    let is_error = tool_result_indicates_error(&result);
                    if is_error {
                        let failure_kind = classify_tool_failure(&result);
                        match failure_kind {
                            ToolFailureKind::SkillContextMissing => {
                                // Skill context missing — give friendly hint to activate skill first
                                let hint = format!(
                                    "💡 工具 `{}` 需要先激活技能才能使用。\n\
                                     请先调用 `activate_skill` 工具激活技能，例如：\n\
                                     ```\n\
                                     activate_skill({{skill_name: \"<技能名>\", goal: \"<目标>\"}})\n\
                                     ```\n\
                                     激活后再调用 `{}` 执行技能脚本。",
                                    tool_call.name, tool_call.name
                                );
                                info!(tool = %tool_call.name, "Skill context missing — suggesting activate_skill");
                                current_messages.push(ChatMessage::user(&hint));
                            }
                            ToolFailureKind::Permanent | ToolFailureKind::Transient => {
                                let count =
                                    tool_fail_counts.entry(tool_call.name.clone()).or_insert(0);
                                *count += 1;

                                if failure_kind == ToolFailureKind::Permanent && *count == 1 {
                                    let hint = format!(
                                        "⚠️ 工具 `{}` 遇到永久性错误（如 API key 缺失、权限不足），请不要重试，改用其他可用工具或告知用户配置问题。",
                                        tool_call.name
                                    );
                                    warn!(tool = %tool_call.name, kind = ?failure_kind, "Permanent tool failure — injecting immediate hint");
                                    current_messages.push(ChatMessage::user(&hint));
                                }
                            }
                            ToolFailureKind::ResourceMissing => {
                                tool_fail_counts.remove(&tool_call.name);
                                if resource_missing_hints_sent.insert(tool_call.name.clone()) {
                                    let hint = format!(
                                        "⚠️ 工具 `{}` 报告目标资源不存在。不要重复调用同一工具重试同一个标识；直接向用户说明未找到，或请用户提供新的标识/范围。",
                                        tool_call.name
                                    );
                                    current_messages.push(ChatMessage::user(&hint));
                                }
                            }
                        }
                    } else {
                        // Reset on success
                        tool_fail_counts.remove(&tool_call.name);
                        resource_missing_hints_sent.remove(&tool_call.name);
                    }

                    let mut tool_msg = ChatMessage::tool_result(&tool_call.id, &result);
                    tool_msg.name = Some(tool_call.name.clone());
                    tool_results.push(tool_msg);
                }

                // If we supplemented tools, roll back the assistant message and tool results
                // so the LLM retries with the full tool schema available.
                if supplemented_tools {
                    // Remove the assistant message we just pushed (last element)
                    current_messages.pop();
                    history.pop();
                    // Do NOT push tool results — the LLM will retry from scratch
                    continue;
                }

                // Normal path: commit tool results to messages and history,
                // trimming each tool result to prevent unbounded growth.
                for mut tool_msg in tool_results {
                    // Trim tool result content (tool results can be very large,
                    // e.g. web_fetch markdown, finance_api JSON arrays)
                    if let serde_json::Value::String(ref s) = tool_msg.content {
                        let char_count = s.chars().count();
                        if char_count > 2400 {
                            let head: String = s.chars().take(1600).collect();
                            let tail: String = s
                                .chars()
                                .rev()
                                .take(800)
                                .collect::<String>()
                                .chars()
                                .rev()
                                .collect();
                            tool_msg.content = serde_json::Value::String(format!(
                                "{}\n...<trimmed {} chars>...\n{}",
                                head,
                                char_count - 2400,
                                tail
                            ));
                        }
                    }
                    current_messages.push(tool_msg.clone());
                    history.push(tool_msg);
                }

                if wants_forced_answer && !over_iteration {
                    if !web_search_thin_results.is_empty() {
                        // Thin results: guide LLM to fetch actual page content instead of giving up
                        let urls_hint = web_search_thin_results
                            .iter()
                            .take(3)
                            .cloned()
                            .collect::<Vec<_>>()
                            .join("\n- ");
                        let hint = format!(
                            "搜索结果只包含链接标题，没有具体内容。**不要直接返回\"未找到\"，请立即改用 `web_fetch` 直接抓取以下页面获取真实数据**：\n- {}\n\n抓取后给出最终答案。",
                            urls_hint
                        );
                        current_messages.push(ChatMessage::user(&hint));
                    } else {
                        current_messages.push(ChatMessage::user(
                            "请基于刚才工具返回的结果直接给出最终答案（例如：整理成要点/列表/摘要）。除非结果明显不足，否则不要继续调用 web_search/web_fetch。",
                        ));
                    }
                }

                // Fallback hint: when a tool has failed 2+ times, tell the LLM to switch
                // to alternative tools. This prevents infinite retry loops (e.g. qveris without API key).
                let repeated_failures: Vec<String> = tool_fail_counts
                    .iter()
                    .filter(|(_, count)| **count >= 2)
                    .map(|(name, count)| format!("{} ({}x)", name, count))
                    .collect();
                if !repeated_failures.is_empty() {
                    let hint = format!(
                        "⚠️ 以下工具连续失败: {}。请不要继续重试，改用其他可用工具完成任务。对于金融数据查询失败，可降级使用 `web_search` 搜索相关新闻。",
                        repeated_failures.join(", ")
                    );
                    warn!(failures = ?repeated_failures, "Injecting fallback hint due to repeated tool failures");
                    current_messages.push(ChatMessage::user(&hint));
                }

                // Layer 4: Full Compact - 当 token 超过预算阈值时触发 LLM 语义压缩
                // 预算阈值: token_budget * compact_threshold (默认 100_000 * 0.8 = 80_000)
                let estimated_tokens = estimate_messages_tokens(&current_messages);
                // Update Layer 4 token usage metrics
                if let Some(memory_system) = self.memory_system.as_ref() {
                    crate::memory_event!(
                        layer4,
                        token_usage,
                        estimated_tokens,
                        memory_system.config().token_budget,
                        memory_system.config().layer4.compact_threshold_ratio
                    );
                    if memory_system.should_compact(estimated_tokens) {
                        info!(
                            estimated_tokens,
                            token_budget = memory_system.config().token_budget,
                            threshold = memory_system.config().layer4.compact_threshold_ratio,
                            "[layer4] Full compact threshold reached"
                        );

                        // 执行 Layer 4 Compact
                        let compact_ctx = CompactContext {
                            channel: &msg.channel,
                            chat_id: &msg.chat_id,
                            account_id: msg.account_id.as_deref(),
                        };
                        if let Err(e) = self
                            .capture_pre_compress_learning_boundary(&session_key, &current_messages)
                            .await
                        {
                            warn!(error = %e, session_key = %session_key, "Ghost learning pre-compress capture failed");
                        }
                        let compact_result = self
                            .execute_layer4_compact(
                                &current_messages,
                                &session_key,
                                Some(compact_ctx),
                                true, // is_auto for automatic compact
                            )
                            .await;
                        if compact_result.success {
                            // 替换消息历史为压缩后的内容
                            current_messages.clear();
                            current_messages
                                .push(ChatMessage::system(&compact_result.to_compact_message()));
                            // 添加当前用户消息作为继续点
                            current_messages.push(ChatMessage::user("请继续当前任务。"));

                            current_messages.extend(compact_result.recent_messages);

                            info!(
                                post_compact_tokens = estimate_messages_tokens(&current_messages),
                                "[layer4] Compact completed, messages replaced with summary"
                            );
                            metrics.record_compression();

                            // Compact 成功后清空追踪器，防止下次 Compact 重复恢复
                            if let Some(ms) = self.memory_system.as_mut() {
                                ms.file_tracker_mut().clear();
                                ms.skill_tracker_mut().clear();
                            }

                            // 跳过后续处理
                            continue;
                        } else {
                            warn!(
                                error = ?compact_result.error,
                                "[layer4] Compact failed, continuing without compression"
                            );
                        }
                    }
                }

                // Skill Nudge: check after each iteration (replaces skill_nudge_engine.check_skill_nudge)
                // If memory nudge already triggered, upgrade to Combined
                let has_skill_tool = self.tool_registry.get("skill_manage").is_some();
                let existing_memory = matches!(deferred_review_mode, Some(ReviewMode::Memory));
                if let Some(_skill_trigger) = self
                    .learning_coordinator
                    .check_skill_nudge(has_skill_tool, existing_memory)
                {
                    if matches!(deferred_review_mode, Some(ReviewMode::Memory)) {
                        deferred_review_mode = Some(ReviewMode::Combined);
                        // Use latest messages snapshot (updated during iteration)
                        deferred_review_snapshot = current_messages.clone();
                    } else if deferred_review_mode.is_none() {
                        deferred_review_mode = Some(ReviewMode::Skill);
                        deferred_review_snapshot = current_messages.clone();
                    }
                }

                if !over_iteration && !short_circuit_after_tools {
                    should_throttle_next_tool_round = true;
                }

                if short_circuit_after_tools {
                    final_response.clear();
                    break;
                }

                if over_iteration {
                    warn!(
                        iteration = ?tool_call_counts,
                        max_iterations,
                        ?tools_max_iterations,
                        "Reached max iterations; forcing a final no-tools answer"
                    );
                    let mut final_messages = current_messages.clone();
                    final_messages.push(ChatMessage::user(
                        "请基于以上工具调用的结果，直接给出最终答案。不要再调用任何工具，也不要输出类似[Called: ...]的过程信息。",
                    ));
                    let final_messages = append_ephemeral_context_to_latest_user_message(
                        &final_messages,
                        ghost_recall_context_block.as_deref(),
                    );

                    let chat_result = if let Some((pidx, p)) = self.provider_pool.acquire() {
                        let r = p.chat(&final_messages, &[]).await;
                        match &r {
                            Ok(_) => self.provider_pool.report(pidx, CallResult::Success),
                            Err(e) => self
                                .provider_pool
                                .report(pidx, ProviderPool::classify_error(&format!("{}", e))),
                        }
                        r
                    } else {
                        Err(blockcell_core::Error::Config(
                            "ProviderPool: no healthy providers".to_string(),
                        ))
                    };
                    match chat_result {
                        Ok(r) => {
                            final_response = r.content.unwrap_or_default();
                            // 保留 reasoning_content，避免 DeepSeek thinking mode 400 错误
                            history.push(ChatMessage::assistant_with_reasoning(
                                &final_response,
                                r.reasoning_content.clone(),
                            ));
                        }
                        Err(e) => {
                            warn!(error = %e, "Final no-tools LLM call failed");
                            final_response =
                                "I've reached the maximum number of tool iterations.".to_string();
                            history.push(ChatMessage::assistant(&final_response));
                        }
                    }
                    break;
                }
            } else {
                // No tool calls, we have the final response
                final_response = response.content.unwrap_or_default();

                // 保留 reasoning_content，避免 DeepSeek thinking mode 400 错误
                history.push(ChatMessage::assistant_with_reasoning(
                    &final_response,
                    response.reasoning_content.clone(),
                ));
                break;
            }
        }

        // Deferred background Review (after response sent)
        // Only trigger when there's a complete response
        if !final_response.is_empty() {
            if let Some(mode) = deferred_review_mode.take() {
                if self.learning_coordinator.is_self_improve_enabled() {
                    self.learning_coordinator.review_started();
                    let notify = Some((msg.channel.clone(), msg.chat_id.clone()));
                    self.spawn_review(mode, deferred_review_snapshot, notify);
                }
            }
        }

        if is_im_channel(&msg.channel)
            && user_wants_send_image(&msg.content)
            && !message_tool_sent_media
        {
            if let Some(image_path) = pick_image_path(&self.paths, &history).await {
                info!(
                    image_path = %image_path,
                    channel = %msg.channel,
                    "Auto-sending image fallback (LLM did not call message tool)"
                );
                if let Some(tx) = &self.outbound_tx {
                    let mut outbound = OutboundMessage::new(&msg.channel, &msg.chat_id, "");
                    outbound.account_id = msg.account_id.clone();
                    outbound.media = vec![image_path.clone()];
                    let _ = tx.send(outbound).await;
                }

                final_response.clear();
                overwrite_last_assistant_message(&mut history, "");
            }
        }

        let _ghost_learning_episode_id = match self.capture_turn_end_learning_boundary(
            &msg,
            &history,
            &final_response,
            &tool_call_counts,
        ) {
            Ok(episode_id) => episode_id,
            Err(e) => {
                warn!(error = %e, session_key = %session_key, "Ghost learning turn-end capture failed");
                None
            }
        };

        // Post-Sampling Hooks: Layer 3 & Layer 5
        // 在主循环结束后执行 Session Memory 和 Auto Memory 提取
        // 使用 tokio::spawn 非阻塞执行，不延迟用户响应
        // 预先获取共享引用（避免借用冲突）
        let reload_flag = self.memory_injector_reload_flag();
        let cursor_reload_flag = self
            .memory_system
            .as_ref()
            .map(|ms| ms.cursor_reload_flag());

        if let Some(memory_system) = self.memory_system.as_mut() {
            let current_tokens = estimate_messages_tokens(&history);
            let action = crate::memory_system::evaluate_memory_hooks(
                memory_system,
                &history,
                current_tokens,
            );

            match action {
                crate::memory_system::PostSamplingAction::ExtractSessionMemory => {
                    info!("[post-sampling] Spawning Session Memory extraction task");

                    // 克隆必要的数据用于异步任务
                    let provider_pool = Arc::clone(&self.provider_pool);
                    let history_clone = history.clone();
                    let memory_path = crate::session_memory::get_session_memory_path(
                        memory_system.workspace_dir(),
                        memory_system.session_id(),
                    );
                    let model = self.config.agents.defaults.model.clone();
                    let max_section_length = memory_system.config().layer3.max_section_length;

                    // 非阻塞执行
                    let handle = tokio::spawn(async move {
                        let system_prompt = Arc::new(
                            "你是一个会话记忆提取助手。请从对话中提取关键信息并更新 Session Memory 文件。"
                                .to_string(),
                        );

                        let current_memory = tokio::fs::read_to_string(&memory_path)
                            .await
                            .unwrap_or_else(|_| {
                                crate::session_memory::DEFAULT_SESSION_MEMORY_TEMPLATE.to_string()
                            });

                        let result = crate::session_memory::extract_session_memory(
                            provider_pool,
                            &system_prompt,
                            &model,
                            history_clone,
                            &memory_path,
                            &current_memory,
                            crate::session_memory::DEFAULT_SESSION_MEMORY_TEMPLATE,
                            max_section_length,
                        )
                        .await;

                        match result {
                            Ok(_) => info!("[layer3] Session Memory extraction completed"),
                            Err(e) => {
                                warn!(error = %e, "[layer3] Session Memory extraction failed")
                            }
                        }
                    });

                    // 保存任务句柄
                    if let Some(ms) = self.memory_system.as_mut() {
                        ms.add_background_task(handle);
                    }
                }
                crate::memory_system::PostSamplingAction::ExtractAutoMemory(types) => {
                    info!(
                        memory_types = ?types,
                        "[post-sampling] Spawning Auto Memory extraction tasks"
                    );

                    // 克隆必要的数据
                    let provider_pool = Arc::clone(&self.provider_pool);
                    let history_clone = history.clone();
                    let config_dir = memory_system.config_dir().to_path_buf();
                    let model = self.config.agents.defaults.model.clone();
                    let layer5_config = memory_system.config().layer5.clone();
                    // 使用预先获取的 cursor_reload_flag
                    let cursor_reload_flag = cursor_reload_flag
                        .clone()
                        .unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicBool::new(false)));

                    // 为每种记忆类型创建独立的异步任务
                    for memory_type in types {
                        let provider_pool_for_type = Arc::clone(&provider_pool);
                        let history_for_type = history_clone.clone();
                        let config_dir_for_type = config_dir.clone();
                        let model_for_type = model.clone();
                        let layer5_config_for_type = layer5_config.clone();
                        let reload_flag_for_type = Arc::clone(&reload_flag);
                        let cursor_reload_flag_for_type = Arc::clone(&cursor_reload_flag);

                        // 获取最后一条用户消息的 UUID（用于游标更新）
                        let last_user_uuid = history_for_type
                            .iter()
                            .rev()
                            .find(|m| m.role == "user")
                            .and_then(|m| m.id.clone())
                            .and_then(|s| uuid::Uuid::parse_str(&s).ok())
                            .unwrap_or_else(uuid::Uuid::new_v4);

                        let message_count = history_for_type.len();

                        let handle = tokio::spawn(async move {
                            // 创建提取器（会加载持久化的游标状态）
                            let extractor_config =
                                crate::auto_memory::AutoMemoryConfig::from(layer5_config_for_type);
                            let mut extractor =
                                match crate::auto_memory::AutoMemoryExtractor::with_config(
                                    &config_dir_for_type,
                                    extractor_config,
                                )
                                .await
                                {
                                    Ok(e) => e,
                                    Err(e) => {
                                        warn!(error = %e, "[layer5] Failed to create AutoMemoryExtractor");
                                        return;
                                    }
                                };

                            let system_prompt = Arc::new(
                                "你是一个记忆提取助手。请从对话中提取用户偏好、项目信息、反馈和外部资源引用。"
                                    .to_string(),
                            );

                            // 使用 ExtractionParams 和 extract() 方法
                            // 这样游标状态会被正确更新和保存
                            let params = crate::auto_memory::ExtractionParams {
                                provider_pool: provider_pool_for_type,
                                memory_type,
                                system_prompt,
                                model: model_for_type,
                                messages: history_for_type,
                                last_message_uuid: last_user_uuid,
                                message_count,
                            };

                            let result = extractor.extract(params).await;

                            if result.success {
                                info!(
                                    memory_type = memory_type.name(),
                                    input_tokens = result.input_tokens,
                                    output_tokens = result.output_tokens,
                                    cursor_save_failed = result.cursor_save_failed,
                                    "[layer5] Auto Memory extraction completed"
                                );
                                // 标记需要刷新缓存
                                reload_flag_for_type
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                                // 标记需要重新加载游标状态（通知主线程）
                                cursor_reload_flag_for_type
                                    .store(true, std::sync::atomic::Ordering::Relaxed);
                            } else {
                                warn!(
                                    memory_type = memory_type.name(),
                                    error = ?result.error,
                                    "[layer5] Auto Memory extraction failed"
                                );
                            }
                        });

                        // 保存任务句柄
                        if let Some(ms) = self.memory_system.as_mut() {
                            ms.add_background_task(handle);
                        }
                    }
                }
                crate::memory_system::PostSamplingAction::Compact => {
                    // Post-Sampling 中的 Compact - 同步执行压缩
                    // Compact 应在当前交互结束前同步执行
                    // 这样下次交互时历史已经是压缩后的状态，用户无感知
                    info!(
                        current_tokens,
                        token_budget = memory_system.config().token_budget,
                        "[post-sampling] Executing synchronous compact before response delivery"
                    );

                    let compact_ctx = CompactContext {
                        channel: &msg.channel,
                        chat_id: &msg.chat_id,
                        account_id: msg.account_id.as_deref(),
                    };
                    if let Err(e) = self
                        .capture_pre_compress_learning_boundary(&session_key, &history)
                        .await
                    {
                        warn!(error = %e, session_key = %session_key, "Ghost learning pre-compress capture failed");
                    }
                    let compact_result = self
                        .execute_layer4_compact(
                            &history,
                            &session_key,
                            Some(compact_ctx),
                            true, // is_auto for automatic compact
                        )
                        .await;
                    if compact_result.success {
                        // 压缩成功，替换历史
                        history.clear();
                        history.push(ChatMessage::system(&compact_result.to_compact_message()));
                        history.push(ChatMessage::user("请继续当前任务。"));

                        history.extend(compact_result.recent_messages);

                        info!(
                            post_compact_tokens = estimate_messages_tokens(&history),
                            "[post-sampling] Compact completed, history replaced"
                        );
                        metrics.record_compression();

                        // 清空追踪器
                        if let Some(ms) = self.memory_system.as_mut() {
                            ms.file_tracker_mut().clear();
                            ms.skill_tracker_mut().clear();
                        }
                    } else {
                        warn!(
                            error = ?compact_result.error,
                            "[post-sampling] Compact failed, continuing without compression"
                        );
                    }
                }
                crate::memory_system::PostSamplingAction::None => {}
            }

            // 清理已完成的后台任务
            if let Some(ms) = self.memory_system.as_mut() {
                let cleaned = ms.cleanup_completed_tasks();
                if cleaned > 0 {
                    debug!(
                        cleaned_count = cleaned,
                        "Cleaned up completed background tasks"
                    );
                }
            }
        }

        let delivered_response = self
            .persist_and_deliver_final_response(FinalResponseContext {
                msg: &msg,
                persist_session_key: &persist_session_key,
                history: &mut history,
                session_metadata: &session_metadata,
                final_response: &final_response,
                collected_media,
                cron_deliver_target,
            })
            .await?;

        if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
            manager.sync_all(&msg.content, &delivered_response, &session_key);
            manager.queue_prefetch_all(&msg.content, &session_key);
        }

        self.spawn_pending_ghost_background_reviews();

        Ok(delivered_response)
    }

    /// Extract filesystem paths from tool call parameters.
    fn extract_paths(&self, tool_name: &str, args: &serde_json::Value) -> Vec<String> {
        let mut paths = Vec::new();
        match tool_name {
            "read_file" | "write_file" | "edit_file" | "list_dir" => {
                if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                    paths.push(p.to_string());
                }
            }
            "file_ops" | "data_process" | "audio_transcribe" | "chart_generate"
            | "office_write" | "video_process" | "health_api" | "encrypt" => {
                if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                    paths.push(p.to_string());
                }
                if let Some(d) = args.get("destination").and_then(|v| v.as_str()) {
                    paths.push(d.to_string());
                }
                if let Some(o) = args.get("output_path").and_then(|v| v.as_str()) {
                    paths.push(o.to_string());
                }
                if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
                    for p in arr {
                        if let Some(s) = p.as_str() {
                            paths.push(s.to_string());
                        }
                    }
                }
            }
            "message" => {
                if let Some(arr) = args.get("media").and_then(|v| v.as_array()) {
                    for p in arr {
                        if let Some(s) = p.as_str() {
                            paths.push(s.to_string());
                        }
                    }
                }
            }
            "browse" => {
                if let Some(o) = args.get("output_path").and_then(|v| v.as_str()) {
                    paths.push(o.to_string());
                }
            }
            "exec" => {
                if let Some(wd) = args.get("working_dir").and_then(|v| v.as_str()) {
                    paths.push(wd.to_string());
                }
            }
            _ => {}
        }
        paths
    }

    /// Resolve a path string the same way tools do (expand ~ and relative paths).
    fn resolve_path(&self, path_str: &str) -> PathBuf {
        if path_str.starts_with("~/") {
            dirs::home_dir()
                .map(|h| h.join(&path_str[2..]))
                .unwrap_or_else(|| PathBuf::from(path_str))
        } else if path_str.starts_with('/') {
            PathBuf::from(path_str)
        } else {
            self.paths.workspace().join(path_str)
        }
    }

    /// Check if a resolved path is inside the safe workspace directory.
    fn is_path_safe(&self, resolved: &std::path::Path) -> bool {
        is_path_within_base(&self.paths.workspace(), resolved)
    }

    /// Check whether a resolved path falls within an already-authorized directory.
    /// Optimized (#12): walk ancestors with O(1) HashSet lookups instead of O(n) iteration.
    /// `authorized_dirs` stores already-canonicalized paths, so no re-canonicalization needed.
    fn is_path_authorized(&self, resolved: &std::path::Path) -> bool {
        if self.authorized_dirs.is_empty() {
            return false;
        }
        let rp = canonical_or_normalized(resolved);
        let mut current = rp.as_path();
        loop {
            if self.authorized_dirs.contains(current) {
                return true;
            }
            match current.parent() {
                Some(parent) if parent != current => current = parent,
                _ => return false,
            }
        }
    }

    /// Record a directory as authorized so future accesses within it are auto-approved.
    fn authorize_directory(&mut self, resolved: &std::path::Path) {
        // If the path is a directory, authorize it directly.
        // If it's a file, authorize its parent directory.
        let dir = if resolved.is_dir() {
            resolved.to_path_buf()
        } else {
            resolved
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| resolved.to_path_buf())
        };
        let dir = canonical_or_normalized(&dir);
        if self.authorized_dirs.insert(dir.clone()) {
            info!(dir = %dir.display(), "Directory authorized for future access");
        }
    }

    /// For tools that access the filesystem, check if any paths are outside the
    /// workspace. Applies the path-access policy first; only paths whose policy
    /// outcome is `Confirm` are forwarded to the user for interactive approval.
    ///
    /// Priority (highest → lowest):
    /// 1. Workspace-safe paths  → always allowed
    /// 2. Session-authorized dirs → allowed (cached from prior confirmation)
    /// 3. Policy `Deny`         → rejected immediately, no confirmation sent
    /// 4. Policy `Allow`        → allowed immediately, cached for this session
    /// 5. Policy `Confirm`      → user confirmation required
    async fn check_path_permission(
        &mut self,
        tool_name: &str,
        args: &serde_json::Value,
        msg: &InboundMessage,
    ) -> bool {
        if matches!(tool_name, "exec_local" | "exec_skill_script") {
            return true;
        }
        let raw_paths = self.extract_paths(tool_name, args);
        if raw_paths.is_empty() {
            return true;
        }

        let op = PathOp::from_tool_name(tool_name);

        // Classify each path by policy outcome
        let mut deny_paths: Vec<String> = Vec::new();
        let mut confirm_paths: Vec<String> = Vec::new();

        for p in &raw_paths {
            let resolved = self.resolve_path(p);

            // 1. Workspace-safe → always OK
            if self.is_path_safe(&resolved) {
                continue;
            }

            // 2. Already authorized by user this session → OK
            if self.is_path_authorized(&resolved) {
                continue;
            }

            // 3. Evaluate policy
            let action = self.path_policy.evaluate(&resolved, op);
            match action {
                PolicyAction::Deny => {
                    warn!(
                        tool = tool_name,
                        path = %resolved.display(),
                        "Path access denied by policy"
                    );
                    deny_paths.push(p.clone());
                }
                PolicyAction::Allow => {
                    // Policy explicitly allows — cache for this session
                    info!(
                        tool = tool_name,
                        path = %resolved.display(),
                        "Path access allowed by policy"
                    );
                    if self.path_policy.cache_confirmed_dirs() {
                        self.authorize_directory(&resolved);
                    }
                }
                PolicyAction::Confirm => {
                    confirm_paths.push(p.clone());
                }
            }
        }

        // Any hard-deny → reject the whole operation
        if !deny_paths.is_empty() {
            return false;
        }

        // All paths were allowed (workspace / session-cache / policy-allow)
        if confirm_paths.is_empty() {
            return true;
        }

        // Need user confirmation for the remaining paths
        if let Some(confirm_tx) = &self.confirm_tx {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let request = ConfirmRequest {
                tool_name: tool_name.to_string(),
                paths: confirm_paths.clone(),
                response_tx,
                channel: msg.channel.clone(),
                chat_id: msg.chat_id.clone(),
            };

            if confirm_tx.send(request).await.is_err() {
                warn!("Failed to send confirmation request, denying access");
                return false;
            }

            match response_rx.await {
                Ok(allowed) => {
                    if allowed && self.path_policy.cache_confirmed_dirs() {
                        for p in &confirm_paths {
                            let resolved = self.resolve_path(p);
                            self.authorize_directory(&resolved);
                        }
                    }
                    allowed
                }
                Err(_) => {
                    warn!("Confirmation channel closed, denying access");
                    false
                }
            }
        } else {
            warn!(
                tool = tool_name,
                "No confirmation channel, denying access to paths outside workspace"
            );
            false
        }
    }

    async fn confirm_dangerous_operation(
        &mut self,
        tool_name: &str,
        items: Vec<String>,
        msg: &InboundMessage,
    ) -> bool {
        if items.is_empty() {
            return true;
        }
        if let Some(confirm_tx) = &self.confirm_tx {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            let request = ConfirmRequest {
                tool_name: tool_name.to_string(),
                paths: items,
                response_tx,
                channel: msg.channel.clone(),
                chat_id: msg.chat_id.clone(),
            };
            if confirm_tx.send(request).await.is_err() {
                warn!(
                    tool = tool_name,
                    "Failed to send dangerous-operation confirmation request, denying"
                );
                return false;
            }
            match response_rx.await {
                Ok(allowed) => allowed,
                Err(_) => {
                    warn!(
                        tool = tool_name,
                        "Dangerous-operation confirmation channel closed, denying"
                    );
                    false
                }
            }
        } else {
            warn!(
                tool = tool_name,
                "No confirmation channel, denying dangerous operation"
            );
            false
        }
    }

    async fn execute_runtime_tool_call(
        &self,
        tool_name: &str,
        ctx: blockcell_tools::ToolContext,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
            if manager.has_tool(tool_name) {
                return manager.handle_tool_call(tool_name, arguments);
            }
        }
        self.tool_registry.execute(tool_name, ctx, arguments).await
    }

    async fn execute_tool_call(
        &mut self,
        tool_call: &ToolCallRequest,
        msg: &InboundMessage,
        active_skill_dir: Option<PathBuf>,
    ) -> String {
        // Hard block: reject disabled tools at execution level (not just prompt filtering)
        let disabled_tools = load_disabled_toggles(&self.paths, "tools");
        if disabled_tools.contains(&tool_call.name) {
            return disabled_tool_result(&tool_call.name);
        }
        // Also block disabled skills invoked as tools (skill scripts registered as tools)
        let disabled_skills = load_disabled_toggles(&self.paths, "skills");
        if disabled_skills.contains(&tool_call.name) {
            return disabled_skill_result(&tool_call.name);
        }

        // Dangerous-operation gate: require explicit user confirmation before executing
        // self-destructive commands or destructive file operations.
        if tool_call.name == "exec" {
            if let Some(cmd) = tool_call.arguments.get("command").and_then(|v| v.as_str()) {
                if is_dangerous_exec_command(cmd) {
                    let items = vec![format!("command: {}", cmd)];
                    if self.confirm_tx.is_none() {
                        if !user_explicitly_confirms_dangerous_op(&msg.content) {
                            return dangerous_exec_denied(false);
                        }
                    } else if !self.confirm_dangerous_operation("exec", items, msg).await {
                        return dangerous_exec_denied(true);
                    }
                }
            }
        }

        if tool_call.name == "file_ops" {
            let action = tool_call
                .arguments
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let path = tool_call
                .arguments
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let destination = tool_call
                .arguments
                .get("destination")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let recursive = tool_call
                .arguments
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut items = Vec::new();
            if action == "delete" && recursive {
                items.push(format!("file_ops delete recursive=true path={}", path));
            }
            if (action == "delete" || action == "rename" || action == "move")
                && (is_sensitive_filename(path) || is_sensitive_filename(destination))
            {
                items.push(format!(
                    "file_ops {} sensitive file (config*) path={} destination={}",
                    action, path, destination
                ));
            }

            if !items.is_empty() {
                if self.confirm_tx.is_none() {
                    if !user_explicitly_confirms_dangerous_op(&msg.content) {
                        return dangerous_file_ops_denied();
                    }
                } else if !self
                    .confirm_dangerous_operation("file_ops", items, msg)
                    .await
                {
                    return dangerous_file_ops_denied();
                }
            }
        }

        // Check path safety before executing filesystem/exec tools
        if !self
            .check_path_permission(&tool_call.name, &tool_call.arguments, msg)
            .await
        {
            return crate::error::path_access_denied(&tool_call.name, "outside workspace");
        }

        // Build TaskManager handle for tools
        let tm_handle: TaskManagerHandle = Arc::new(self.task_manager.clone());

        // Build spawn handle for tools
        let spawn_handle = Arc::new(RuntimeSpawnHandle {
            config: self.config.clone(),
            paths: self.paths.clone(),
            task_manager: self.task_manager.clone(),
            outbound_tx: self.outbound_tx.clone(),
            provider_pool: Arc::clone(&self.provider_pool),
            agent_id: resolve_routed_agent_id(&msg.metadata).or_else(|| self.agent_id.clone()),
            event_tx: self.event_tx.clone(),
            origin_session_key: msg.session_key(),
            response_cache: self.response_cache.clone(),
            event_emitter: self.system_event_emitter.clone(),
        });

        let ctx = blockcell_tools::ToolContext {
            workspace: self.paths.workspace(),
            builtin_skills_dir: Some(self.paths.builtin_skills_dir()),
            active_skill_dir,
            session_key: msg.session_key(),
            channel: msg.channel.clone(),
            account_id: msg.account_id.clone(),
            sender_id: Some(msg.sender_id.clone()),
            chat_id: msg.chat_id.clone(),
            config: self.config.clone(),
            permissions: self.build_tool_permissions(
                &msg.channel,
                Some(&msg.sender_id),
                &msg.chat_id,
            ),
            task_manager: Some(tm_handle),
            memory_store: self.memory_store.clone(),
            memory_file_store: self.memory_file_store.clone(),
            ghost_memory_lifecycle: self.ghost_memory_lifecycle.clone().map(|manager| {
                manager as Arc<dyn blockcell_tools::GhostMemoryLifecycleOps + Send + Sync>
            }),
            skill_file_store: self.skill_file_store.clone(),
            session_search: Some(Arc::new(RuntimeSessionSearch::new(
                self.paths.clone(),
                Some(msg.session_key()),
            ))),
            outbound_tx: self.outbound_tx.clone(),
            spawn_handle: Some(spawn_handle),
            capability_registry: self.capability_registry.clone(),
            core_evolution: self.core_evolution.clone(),
            event_emitter: Some(self.system_event_emitter.clone()),
            channel_contacts_file: Some(self.paths.channel_contacts_file()),
            response_cache: Some(
                Arc::new(self.response_cache.clone()) as blockcell_tools::ResponseCacheHandle
            ),
            skill_mutex: Some(
                Arc::new(self.skill_mutex.clone()) as blockcell_tools::SkillMutexHandle
            ),
        };

        // Emit tool_call_start event to WebSocket clients
        if let Some(ref event_tx) = self.event_tx {
            let event = serde_json::json!({
                "type": "tool_call_start",
                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                "chat_id": msg.chat_id,
                "task_id": "",
                "tool": tool_call.name,
                "call_id": tool_call.id,
                "params": tool_call.arguments,
            });
            let _ = event_tx.send(event.to_string());
        }

        let start = std::time::Instant::now();
        let result = self
            .execute_runtime_tool_call(&tool_call.name, ctx, tool_call.arguments.clone())
            .await;
        let duration_ms = start.elapsed().as_millis() as u64;

        let is_error = result.is_err();
        let (result_str, result_json) = match &result {
            Ok(val) => (val.to_string(), val.clone()),
            Err(e) => {
                let err_str = format!("Error: {}", e);
                (err_str.clone(), serde_json::json!({"error": err_str}))
            }
        };

        // Detect writes to the skills directory and trigger hot-reload + Dashboard refresh
        if !is_error
            && matches!(
                tool_call.name.as_str(),
                "write_file" | "edit_file" | "skill_manage"
            )
        {
            if let Some(path_str) = tool_call.arguments.get("path").and_then(|v| v.as_str()) {
                let resolved = self.resolve_path(path_str);
                let skills_dir = self.paths.skills_dir();
                let in_skills = resolved.starts_with(&skills_dir)
                    || resolved.canonicalize().ok().is_some_and(|c| {
                        skills_dir
                            .canonicalize()
                            .ok()
                            .is_some_and(|sd| c.starts_with(&sd))
                    });
                if in_skills {
                    info!(path = %path_str, "🔄 Detected write to skills directory, reloading...");
                    let new_skills = self.context_builder.reload_skills();
                    if !new_skills.is_empty() {
                        info!(skills = ?new_skills, "🔄 Hot-reloaded new skills");
                    }
                    // 刷新 Skill 索引摘要 (使下次 LLM 调用获取最新 Skill 列表)
                    self.context_builder.refresh_skill_index_summary();
                    // Always broadcast so Dashboard refreshes (even for updates to existing skills)
                    if let Some(ref event_tx) = self.event_tx {
                        let event = serde_json::json!({
                            "type": "skills_updated",
                            "new_skills": new_skills,
                        });
                        let _ = event_tx.send(event.to_string());
                    }
                }
            }
        }

        // Detect skill_manage changes and refresh Skill index summary
        if !is_error && tool_call.name == "skill_manage" {
            let action = tool_call
                .arguments
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if matches!(
                action,
                "create" | "patch" | "delete" | "edit" | "write_file" | "remove_file"
            ) {
                debug!(
                    action = action,
                    "🔄 skill_manage modified skills, refreshing index summary"
                );
                self.context_builder.refresh_skill_index_summary();
            }
        }

        let mut learning_hint: Option<String> = None;
        if is_error {
            let is_unknown_tool = result_str.contains("Unknown tool:");

            if is_unknown_tool {
                learning_hint = Some(format!(
                    "[系统] 工具 `{}` 未注册/不可用（Unknown tool）。这不是可通过技能自进化修复的问题。\
                    请改用已存在的工具完成任务，或提示用户安装/启用对应工具。",
                    tool_call.name
                ));
            } else if let Some(evo_service) = self.context_builder.evolution_service() {
                // OpenClaw skill 不触发自进化
                let is_openclaw = self
                    .context_builder
                    .skill_manager()
                    .is_some_and(|sm| sm.is_tool_from_openclaw(&tool_call.name));
                if is_openclaw {
                    debug!(
                        tool = %tool_call.name,
                        "Skipping evolution for OpenClaw skill"
                    );
                } else {
                    // Preserve any legacy top-level Rhai asset as supplemental evolution context.
                    let source_snippet = self
                        .context_builder
                        .skill_manager()
                        .and_then(|sm| sm.get(&tool_call.name))
                        .and_then(|skill| skill.load_rhai());
                    match evo_service
                        .report_error(&tool_call.name, &result_str, source_snippet, vec![])
                        .await
                    {
                        Ok(report) => {
                            if report.evolution_triggered.is_some() {
                                learning_hint = Some(format!(
                                    "[系统] 技能 `{}` 执行失败，已自动触发进化学习。\
                                请向用户坦诚说明：你暂时还不具备这个技能，但已经开始学习，\
                                学会后会自动生效。同时尝试用其他方式帮助用户解决当前问题。",
                                    tool_call.name
                                ));
                            } else if report.evolution_in_progress {
                                learning_hint = Some(format!(
                                    "[系统] 技能 `{}` 执行失败，该技能正在学习改进中。\
                                请告诉用户：这个技能正在学习中，请稍后再试。",
                                    tool_call.name
                                ));
                            }
                        }
                        Err(e) => {
                            debug!(error = %e, "Evolution report_error failed");
                        }
                    }
                }
            }
        }
        // 报告调用结果给灰度统计（OpenClaw skill 跳过）
        if let Some(evo_service) = self.context_builder.evolution_service() {
            let is_openclaw = self
                .context_builder
                .skill_manager()
                .is_some_and(|sm| sm.is_tool_from_openclaw(&tool_call.name));
            if !is_openclaw {
                let reported_name = tool_call.name.clone();
                evo_service
                    .report_skill_call(&reported_name, is_error)
                    .await;
            }
        }

        // Emit tool_call_result event to WebSocket clients
        if let Some(ref event_tx) = self.event_tx {
            let event = serde_json::json!({
                "type": "tool_call_result",
                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                "chat_id": msg.chat_id,
                "task_id": "",
                "tool": tool_call.name,
                "call_id": tool_call.id,
                "result": result_json,
                "duration_ms": duration_ms,
            });
            let _ = event_tx.send(event.to_string());
        }

        // Log to audit
        let _ = self.audit_logger.log_tool_call(
            &tool_call.name,
            tool_call.arguments.clone(),
            result_json,
            &msg.session_key(),
            None, // trace_id can be added later
            Some(duration_ms),
        );

        // Skill Nudge: 两个独立计数器 (Skill + Memory)
        // 与 Hermes 一致: 只有 skill_manage 写操作重置 Skill 计数器 (view/list_skills 等只读操作不重置)
        // 与 Hermes 一致: 只有 memory 写操作重置 Memory 计数器 (memory_query 等只读操作不重置)
        let tool_name_str = tool_call.name.as_str();
        let is_skill_write_tool = tool_name_str == "skill_manage"
            && matches!(
                tool_call
                    .arguments
                    .get("action")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
                "create" | "patch" | "edit" | "delete" | "write_file" | "remove_file"
            );
        let is_memory_write_tool = matches!(
            tool_name_str,
            "memory_upsert" | "memory_forget" | "auto_memory"
        );

        // Skill/Memory write tools reset corresponding counters via learning coordinator
        if is_skill_write_tool {
            self.learning_coordinator.reset_skill();
        }
        if is_memory_write_tool {
            self.learning_coordinator.reset_memory();
        }

        // Layer 4: Track file reads for Post-Compact recovery
        // 追踪多种文件访问工具的结果，用于 Compact 后恢复
        if !is_error {
            let file_content_to_track: Option<(std::path::PathBuf, &str)> =
                match tool_call.name.as_str() {
                    "read_file" => {
                        // read_file: 直接追踪文件内容
                        if let Some(path_str) =
                            tool_call.arguments.get("path").and_then(|v| v.as_str())
                        {
                            Some((self.resolve_path(path_str), &result_str))
                        } else {
                            None
                        }
                    }
                    "grep" | "rg" => {
                        // grep/rg: 追踪搜索路径和匹配结果
                        let path = tool_call
                            .arguments
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or(".");
                        Some((self.resolve_path(path), &result_str))
                    }
                    "glob" => {
                        // glob: 追踪匹配的文件列表
                        let path = tool_call
                            .arguments
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or(".");
                        Some((self.resolve_path(path), &result_str))
                    }
                    _ => None,
                };

            if let Some((path, content)) = file_content_to_track {
                if let Some(memory_system) = self.memory_system.as_mut() {
                    memory_system.record_file_read(path.clone(), content);
                    debug!(path = %path.display(), tool = %tool_call.name, "[layer4] Tracked file access for recovery");
                }
            }
        }

        // 在工具结果中追加学习提示，让 LLM 自然地回复用户
        match learning_hint {
            Some(hint) => format!("{}\n\n{}", result_str, hint),
            None => result_str,
        }
    }

    async fn run_prompt_skill_for_session(
        &mut self,
        active_skill: &ActiveSkillContext,
        msg: &InboundMessage,
        history: &[ChatMessage],
        session_key: &str,
        tool_names: &[String],
    ) -> Result<(String, Vec<ChatMessage>, serde_json::Value)> {
        let disabled_skills = load_disabled_toggles(&self.paths, "skills");
        let disabled_tools = load_disabled_toggles(&self.paths, "tools");

        let mode_names = vec![format!("Skill:{}", active_skill.name)];
        let prompt_ctx = blockcell_tools::PromptContext {
            channel: &msg.channel,
            intents: &mode_names,
            default_timezone: self.config.default_timezone.as_deref(),
        };
        let tool_name_refs = tool_names
            .iter()
            .map(|name| name.as_str())
            .collect::<Vec<_>>();
        let tool_prompt_rules = self
            .tool_registry
            .get_prompt_rules(&tool_name_refs, &prompt_ctx);
        let pending_intent = msg
            .metadata
            .get("media_pending_intent")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let session_metadata = self.session_store.load_metadata(session_key)?;
        let messages = self.context_builder.build_messages_for_mode_with_channel(
            history,
            &msg.content,
            &msg.media,
            InteractionMode::Skill,
            Some(active_skill),
            &disabled_skills,
            &disabled_tools,
            &msg.channel,
            pending_intent,
            tool_names,
            &tool_prompt_rules,
        );

        let mut tools = if tool_names.is_empty() {
            Vec::new()
        } else {
            self.tool_registry.get_tiered_schemas(
                &tool_name_refs,
                blockcell_tools::registry::global_core_tool_names(),
            )
        };
        if !disabled_tools.is_empty() {
            tools.retain(|schema| {
                let name = schema
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                !disabled_tools.contains(name)
            });
        }

        let prompt_result = self
            .run_prompt_skill_loop(
                msg,
                messages,
                tools,
                tool_names,
                self.context_builder
                    .skill_manager()
                    .and_then(|manager| manager.get(&active_skill.name))
                    .map(|skill| skill.path.clone()),
            )
            .await?;

        let mut final_response = prompt_result.final_response;
        if let Some(last_local_exec_tool_name) =
            Self::last_local_exec_tool_name(&prompt_result.trace_messages)
        {
            if let Some(summary_bundle) = self
                .context_builder
                .skill_manager()
                .and_then(|manager| manager.get(&active_skill.name))
                .and_then(|skill| skill.load_summary_bundle())
            {
                let summary_system_prompt = concat!(
                    "You are blockcell, an AI assistant with access to tools.\n\n",
                    "You are in a final summary-only step for a script-backed skill. ",
                    "Follow the skill summary instructions, preserve factual meaning, and output only the user-facing answer. ",
                    "Do not call tools.\n"
                );
                let summary_prompt = build_script_skill_summary_prompt(
                    &msg.content,
                    &active_skill.name,
                    &last_local_exec_tool_name,
                    &summary_bundle,
                    &final_response,
                );
                let summary_messages = vec![
                    ChatMessage::system(summary_system_prompt),
                    ChatMessage::user(&summary_prompt),
                ];
                let summary_response = self
                    .chat_with_provider(&summary_messages, &[])
                    .await?
                    .content
                    .unwrap_or_default();
                if !summary_response.trim().is_empty() {
                    final_response = summary_response;
                }
            }
        }

        final_response =
            apply_skill_fallback_response(final_response, active_skill.fallback_message.as_deref());

        Ok((
            final_response,
            prompt_result.trace_messages,
            session_metadata,
        ))
    }

    async fn deliver_skill_response(
        &self,
        msg: &InboundMessage,
        final_response: &str,
        cron_kind: Option<&str>,
    ) {
        if let Some((channel, to)) = resolve_cron_deliver_target(msg) {
            if channel == "ws" {
                if let Some(ref event_tx) = self.event_tx {
                    let mut event = serde_json::json!({
                        "type": "message_done",
                        "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                        "chat_id": to,
                        "task_id": "",
                        "content": final_response,
                        "tool_calls": 0,
                        "duration_ms": 0,
                        "media": [],
                        "background_delivery": true,
                        "delivery_kind": "cron",
                    });
                    if let Some(cron_kind) = cron_kind {
                        event["cron_kind"] = serde_json::json!(cron_kind);
                    }
                    let _ = event_tx.send(event.to_string());
                }
                return;
            }

            if let Some(tx) = &self.outbound_tx {
                let mut outbound = OutboundMessage::new(&channel, &to, final_response);
                outbound.account_id = msg.account_id.clone();
                let _ = tx.send(outbound).await;
            }
            return;
        }

        if msg.channel == "ws" {
            if let Some(ref event_tx) = self.event_tx {
                let event = serde_json::json!({
                    "type": "message_done",
                    "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                    "chat_id": msg.chat_id,
                    "task_id": "",
                    "content": final_response,
                    "tool_calls": 0,
                    "duration_ms": 0,
                    "media": [],
                });
                let _ = event_tx.send(event.to_string());
            }
        }

        if let Some(tx) = &self.outbound_tx {
            let mut outbound = OutboundMessage::new(&msg.channel, &msg.chat_id, final_response);
            outbound.account_id = msg.account_id.clone();
            outbound.metadata = extract_reply_metadata(msg);
            let _ = tx.send(outbound).await;
        }
    }

    #[allow(dead_code)]
    #[deprecated(
        note = "Legacy compatibility helper for direct SKILL.rhai execution. Prefer SKILL.md-driven exec_skill_script flows."
    )]
    async fn run_rhai_script_with_context(
        &self,
        rhai_path: &std::path::Path,
        skill_name: &str,
        msg: &InboundMessage,
        extra_ctx: Option<serde_json::Value>,
    ) -> Result<String> {
        use blockcell_skills::dispatcher::SkillDispatcher;
        use std::collections::HashMap;

        let script = std::fs::read_to_string(rhai_path).map_err(|e| {
            blockcell_core::Error::Skill(format!("Failed to read {}: {}", rhai_path.display(), e))
        })?;

        // Build a synchronous tool executor that uses the tool registry
        let registry = self.tool_registry.clone();
        let config = self.config.clone();
        let paths = self.paths.clone();
        let session_key = msg.session_key();
        let channel = msg.channel.clone();
        let chat_id = msg.chat_id.clone();
        let task_manager = self.task_manager.clone();
        let memory_store = self.memory_store.clone();
        let outbound_tx = self.outbound_tx.clone();
        let capability_registry = self.capability_registry.clone();
        let core_evolution = self.core_evolution.clone();
        let event_emitter = self.system_event_emitter.clone();

        let tool_executor =
            move |tool_name: &str, params: serde_json::Value| -> Result<serde_json::Value> {
                // Security gate: block disabled tools/skills in skill scripts
                let disabled_tools = load_disabled_toggles(&paths, "tools");
                if disabled_tools.contains(tool_name) {
                    return Err(blockcell_core::Error::Tool(format!(
                        "Tool '{}' is disabled via toggles",
                        tool_name
                    )));
                }
                let disabled_skills = load_disabled_toggles(&paths, "skills");
                if disabled_skills.contains(tool_name) {
                    return Err(blockcell_core::Error::Tool(format!(
                        "Skill '{}' is disabled via toggles",
                        tool_name
                    )));
                }

                // Security gate: block dangerous exec commands from skill scripts
                if tool_name == "exec" {
                    if let Some(cmd) = params.get("command").and_then(|v| v.as_str()) {
                        if is_dangerous_exec_command(cmd) {
                            return Err(blockcell_core::Error::Tool(format!(
                                "Dangerous command blocked in skill script: {}",
                                cmd
                            )));
                        }
                    }
                }

                // Security gate: validate filesystem paths are within workspace
                let fs_tools = [
                    "read_file",
                    "write_file",
                    "edit_file",
                    "list_dir",
                    "file_ops",
                ];
                if fs_tools.contains(&tool_name) {
                    let workspace = paths.workspace();
                    for key in &["path", "destination", "output_path"] {
                        if let Some(p) = params.get(*key).and_then(|v| v.as_str()) {
                            let resolved = if std::path::Path::new(p).is_absolute() {
                                std::path::PathBuf::from(p)
                            } else {
                                workspace.join(p)
                            };
                            if !is_path_within_base(&workspace, &resolved) {
                                return Err(blockcell_core::Error::Tool(format!(
                                    "Path '{}' is outside workspace — blocked in skill script",
                                    p
                                )));
                            }
                        }
                    }
                }

                let ctx = blockcell_tools::ToolContext {
                    workspace: paths.workspace(),
                    builtin_skills_dir: Some(paths.builtin_skills_dir()),
                    active_skill_dir: None,
                    session_key: session_key.clone(),
                    channel: channel.clone(),
                    account_id: None,
                    sender_id: None, // Cron jobs have no sender
                    chat_id: chat_id.clone(),
                    config: config.clone(),
                    permissions: blockcell_core::types::PermissionSet::new(),
                    task_manager: Some(Arc::new(task_manager.clone())),
                    memory_store: memory_store.clone(),
                    memory_file_store: None,
                    ghost_memory_lifecycle: None,
                    skill_file_store: None,
                    session_search: None,
                    outbound_tx: outbound_tx.clone(),
                    spawn_handle: None, // No spawning from cron skill scripts
                    capability_registry: capability_registry.clone(),
                    core_evolution: core_evolution.clone(),
                    event_emitter: Some(event_emitter.clone()),
                    channel_contacts_file: Some(paths.channel_contacts_file()),
                    response_cache: None,
                    skill_mutex: None,
                };

                // Execute tool synchronously via a new tokio runtime handle
                let rt = tokio::runtime::Handle::current();
                let tool_name_owned = tool_name.to_string();
                std::thread::scope(|s| {
                    s.spawn(|| {
                        rt.block_on(async { registry.execute(&tool_name_owned, ctx, params).await })
                    })
                    .join()
                    .unwrap_or_else(|_| {
                        Err(blockcell_core::Error::Tool(
                            "Tool execution panicked".into(),
                        ))
                    })
                })
            };

        // Context variables for the legacy compatibility script.
        let mut context_vars = HashMap::new();
        context_vars.insert("skill_name".to_string(), serde_json::json!(skill_name));
        context_vars.insert("trigger".to_string(), serde_json::json!("cron"));

        let invocation = extra_ctx
            .as_ref()
            .and_then(|ctx| ctx.get("invocation"))
            .cloned();

        // Build a `ctx` map so legacy Rhai assets can use `ctx.user_input`, `ctx.channel`, etc.
        let mut ctx_json = serde_json::json!({
            "user_input": msg.content,
            "skill_name": skill_name,
            "trigger": "cron",
            "channel": msg.channel,
            "chat_id": msg.chat_id,
            "message": msg.content,
            "metadata": msg.metadata,
        });
        if let Some(invocation_value) = invocation.clone() {
            context_vars.insert("invocation".to_string(), invocation_value.clone());
            if let Some(ctx_obj) = ctx_json.as_object_mut() {
                ctx_obj.insert("invocation".to_string(), invocation_value);
            }
        }
        context_vars.insert("ctx".to_string(), ctx_json);

        // Execute the compatibility Rhai asset in a blocking task.
        let dispatcher = SkillDispatcher::new();
        let user_input = msg.content.clone();

        let result = tokio::task::spawn_blocking(move || {
            dispatcher.execute_sync(&script, &user_input, context_vars, tool_executor)
        })
        .await
        .map_err(|e| {
            blockcell_core::Error::Skill(format!("Skill execution join error: {}", e))
        })??;

        if result.success {
            // Format output as string
            let output_str = match &result.output {
                serde_json::Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            info!(
                skill = %skill_name,
                tool_calls = result.tool_calls.len(),
                "Legacy Rhai compatibility execution succeeded"
            );
            Ok(output_str)
        } else {
            let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
            warn!(
                skill = %skill_name,
                error = %err,
                "Legacy Rhai compatibility execution failed"
            );
            Err(blockcell_core::Error::Skill(err))
        }
    }

    pub async fn run_loop(
        &mut self,
        mut inbound_rx: mpsc::Receiver<InboundMessage>,
        mut shutdown_rx: Option<broadcast::Receiver<()>>,
    ) {
        info!("AgentRuntime started");

        // 启动灰度发布调度器（每 60 秒 tick 一次）
        let has_evolution = self.context_builder.evolution_service().is_some();
        if has_evolution {
            info!("Evolution rollout scheduler enabled");
        }

        let tick_secs = self.config.tools.tick_interval_secs.clamp(10, 300) as u64;
        info!(tick_secs = tick_secs, "Tick interval configured");
        let mut tick_interval = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
        tick_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut active_chat_tasks: HashMap<String, String> = HashMap::new();
        let mut active_message_tasks: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
        let (task_done_tx, mut task_done_rx) = mpsc::unbounded_channel::<(String, String)>();

        async fn abort_active_message_tasks(
            task_manager: &TaskManager,
            active_chat_tasks: &mut HashMap<String, String>,
            active_message_tasks: &mut HashMap<String, tokio::task::JoinHandle<()>>,
        ) {
            let active_task_ids: Vec<String> = active_message_tasks.keys().cloned().collect();
            for task_id in active_task_ids {
                if let Some(handle) = active_message_tasks.remove(&task_id) {
                    handle.abort();
                }
                task_manager.remove_task(&task_id).await;
            }
            active_chat_tasks.clear();
        }

        loop {
            tokio::select! {
                _ = async {
                    if let Some(ref mut rx) = shutdown_rx {
                        let _ = rx.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {
                    if let Err(e) = self.capture_main_session_end_learning_boundary().await {
                        warn!(error = %e, "Ghost learning session-end capture failed during shutdown");
                    }
                    abort_active_message_tasks(
                        &self.task_manager,
                        &mut active_chat_tasks,
                        &mut active_message_tasks,
                    ).await;
                    break;
                }
                done = task_done_rx.recv() => {
                    if let Some((task_id, chat_id)) = done {
                        active_message_tasks.remove(&task_id);
                        if active_chat_tasks.get(&chat_id).is_some_and(|id| id == &task_id) {
                            active_chat_tasks.remove(&chat_id);
                        }
                    }
                }
                msg = inbound_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            if msg.metadata.get("cancel").and_then(|v| v.as_bool()).unwrap_or(false) {
                                let chat_id = msg.chat_id.clone();
                                let mut cancelled = false;
                                if let Some(task_id) = active_chat_tasks.remove(&chat_id) {
                                    if let Some(handle) = active_message_tasks.remove(&task_id) {
                                        handle.abort();
                                        cancelled = true;
                                        self.task_manager.remove_task(&task_id).await;
                                        info!(chat_id = %chat_id, task_id = %task_id, "Cancelled running chat task");
                                    }
                                }
                                if cancelled {
                                    if let Some(ref event_tx) = self.event_tx {
                                        let _ = event_tx.send(
                                            serde_json::json!({
                                                "type": "message_done",
                                                "agent_id": self.agent_id.clone().unwrap_or_else(|| "default".to_string()),
                                                "chat_id": chat_id,
                                                "task_id": "",
                                                "content": "⏹️ 当前对话已终止",
                                                "tool_calls": 0,
                                                "duration_ms": 0
                                            }).to_string()
                                        );
                                    }
                                }
                                continue;
                            }

                            self.update_main_session_target(&msg).await;

                            // Spawn each message as a background task so the loop
                            // stays responsive for new user input.
                            let task_id = format!("msg_{}", uuid::Uuid::new_v4());
                            let label = if msg.content.chars().count() > 40 {
                                format!("{}...", truncate_str(&msg.content, 40))
                            } else {
                                msg.content.clone()
                            };

                            let task_manager = self.task_manager.clone();
                            let config = self.config.clone();
                            let paths = self.paths.clone();
                            let outbound_tx = self.outbound_tx.clone();
                            let confirm_tx = self.confirm_tx.clone();
                            let memory_store = self.memory_store.clone();
                            let capability_registry = self.capability_registry.clone();
                            let core_evolution = self.core_evolution.clone();
                            let event_tx = self.event_tx.clone();
                            let agent_id = self.agent_id.clone();
                            let event_emitter = self.system_event_emitter.clone();
                            let tool_registry = self.tool_registry.clone();
                            let task_id_clone = task_id.clone();
                            let provider_pool = Arc::clone(&self.provider_pool);
                            let chat_id_for_task = msg.chat_id.clone();
                            let task_done_tx = task_done_tx.clone();
                            let done_task_id = task_id.clone();
                            let done_chat_id = chat_id_for_task.clone();

                            // Register task
                            task_manager.create_task(
                                &task_id,
                                &label,
                                &msg.content,
                                &msg.channel,
                                &msg.chat_id,
                                self.agent_id.as_deref(),
                                false,
                            ).await;

                            if let Some(prev_task_id) = active_chat_tasks.remove(&chat_id_for_task) {
                                if let Some(prev_handle) = active_message_tasks.remove(&prev_task_id) {
                                    prev_handle.abort();
                                    self.task_manager.remove_task(&prev_task_id).await;
                                    info!(
                                        chat_id = %chat_id_for_task,
                                        task_id = %prev_task_id,
                                        "Cancelled previous running chat task"
                                    );
                                }
                            }

                            active_chat_tasks.insert(chat_id_for_task, task_id.clone());
                            let handle = tokio::spawn(async move {
                                run_message_task(
                                    config,
                                    paths,
                                    provider_pool,
                                    tool_registry,
                                    task_manager,
                                    outbound_tx,
                                    confirm_tx,
                                    memory_store,
                                    capability_registry,
                                    core_evolution,
                                    event_tx,
                                    agent_id,
                                    event_emitter,
                                    msg,
                                    task_id_clone,
                                ).await;
                                let _ = task_done_tx.send((done_task_id, done_chat_id));
                            });
                            active_message_tasks.insert(task_id, handle);
                        }
                        None => {
                            if let Err(e) = self.capture_main_session_end_learning_boundary().await {
                                warn!(error = %e, "Ghost learning session-end capture failed on inbound close");
                            }
                            break
                        }, // channel closed
                    }
                }
                _ = tick_interval.tick() => {
                    // Auto-cleanup completed/failed tasks older than 5 minutes
                    self.task_manager.cleanup_old_tasks(
                        std::time::Duration::from_secs(300)
                    ).await;

                    // Memory maintenance (TTL cleanup, recycle bin purge)
                    if let Some(ref store) = self.memory_store {
                        if let Err(e) = store.maintenance(30) {
                            warn!(error = %e, "Memory maintenance error");
                        }
                    }

                    let _ = self
                        .process_system_event_tick(chrono::Utc::now().timestamp_millis())
                        .await;

                    // Evolution rollout tick
                    if has_evolution {
                        if let Some(evo_service) = self.context_builder.evolution_service() {
                            if let Err(e) = evo_service.tick().await {
                                warn!(error = %e, "Evolution rollout tick error");
                            }
                        }
                    }

                    // Process pending core evolutions
                    if let Some(ref core_evo_handle) = self.core_evolution {
                        let core_evo = core_evo_handle.lock().await;
                        match core_evo.run_pending_evolutions().await {
                            Ok(n) if n > 0 => {
                                info!(count = n, "🧬 [核心进化] 处理了 {} 个待处理进化", n);
                            }
                            Err(e) => {
                                warn!(error = %e, "🧬 [核心进化] 处理待处理进化出错");
                            }
                            _ => {}
                        }
                    }

                    // Periodic skill hot-reload (picks up skills created by chat)
                    let new_skills = self.context_builder.reload_skills();
                    if !new_skills.is_empty() {
                        info!(skills = ?new_skills, "🔄 Tick: hot-reloaded new skills");
                        if let Some(ref event_tx) = self.event_tx {
                            let event = serde_json::json!({
                                "type": "skills_updated",
                                "new_skills": new_skills,
                            });
                            let _ = event_tx.send(event.to_string());
                        }
                    }

                    // Refresh capability brief for prompt injection + sync capability IDs to SkillManager
                    if let Some(ref registry_handle) = self.capability_registry {
                        let registry = registry_handle.lock().await;
                        let brief = registry.generate_brief().await;
                        self.context_builder.set_capability_brief(brief);
                        // Sync available capability IDs so SkillManager can validate skill dependencies
                        let cap_ids = registry.list_available_ids().await;
                        self.context_builder.sync_capabilities(cap_ids);
                    }

                    // Auto-trigger Capability evolution for missing skill dependencies
                    // With 24h cooldown per capability to prevent repeated requests
                    if let Some(ref core_evo_handle) = self.core_evolution {
                        let missing = self.context_builder.get_missing_capabilities();
                        let now = chrono::Utc::now().timestamp();
                        const COOLDOWN_SECS: i64 = 86400; // 24 hours

                        for (skill_name, cap_id) in missing {
                            // Cooldown check: skip if requested within 24h
                            if let Some(&last_request) = self.cap_request_cooldown.get(&cap_id) {
                                if now - last_request < COOLDOWN_SECS {
                                    continue;
                                }
                            }

                            let description = format!(
                                "Auto-requested: required by skill '{}'",
                                skill_name
                            );
                            let core_evo = core_evo_handle.lock().await;
                            match core_evo.request_capability(&cap_id, &description, "script").await {
                                Ok(_) => {
                                    self.cap_request_cooldown.insert(cap_id.clone(), now);
                                    info!(
                                        capability_id = %cap_id,
                                        skill = %skill_name,
                                        "🧬 Auto-requested missing capability '{}' for skill '{}'",
                                        cap_id, skill_name
                                    );
                                }
                                Err(e) => {
                                    // Also record cooldown on error (blocked/failed) to avoid retrying immediately
                                    self.cap_request_cooldown.insert(cap_id.clone(), now);
                                    debug!(
                                        capability_id = %cap_id,
                                        error = %e,
                                        "Failed to auto-request capability (cooldown set)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        abort_active_message_tasks(
            &self.task_manager,
            &mut active_chat_tasks,
            &mut active_message_tasks,
        )
        .await;
        if let Some(manager) = self.ghost_memory_lifecycle.as_ref() {
            manager.shutdown_all();
        }
        info!("AgentRuntime stopped");
    }
}

fn persist_ghost_learning_boundary_with_config(
    config: &Config,
    paths: &Paths,
    boundary: GhostLearningBoundary,
    sources: Vec<GhostEpisodeSource>,
) -> Result<Option<String>> {
    if !config.agents.ghost.learning.enabled {
        return Ok(None);
    }

    let decision =
        GhostLearningPolicy::from_config(&config.agents.ghost.learning).decide(&boundary);
    persist_ghost_learning_boundary_with_decision(config, paths, boundary, sources, decision)
}

fn persist_ghost_learning_boundary_with_decision(
    config: &Config,
    paths: &Paths,
    boundary: GhostLearningBoundary,
    sources: Vec<GhostEpisodeSource>,
    decision: LearningDecision,
) -> Result<Option<String>> {
    if !config.agents.ghost.learning.enabled {
        return Ok(None);
    }

    let Some(status) = decision.episode_status() else {
        return Ok(None);
    };

    let snapshot = GhostEpisodeSnapshot::from((boundary.clone(), decision));
    let episode = NewGhostEpisode {
        boundary_kind: boundary.kind.as_str().to_string(),
        subject_key: snapshot.subject_key.clone(),
        status: status.to_string(),
        summary: snapshot.summary(),
        metadata: serde_json::to_value(&snapshot)?,
        sources,
    };

    let ledger = GhostLedger::open(&paths.ghost_ledger_db())?;
    let episode_id = ledger.insert_episode(episode)?;
    crate::ghost_metrics::get_ghost_metrics(paths).record_episode_captured();
    Ok(Some(episode_id))
}

fn capture_delegation_end_learning_boundary_with_config(
    config: &Config,
    paths: &Paths,
    origin_channel: &str,
    origin_chat_id: &str,
    task_id: Option<&str>,
    task_goal: &str,
    child_summary: &str,
) -> Result<Option<String>> {
    let task_goal = task_goal.trim();
    let child_summary = child_summary.trim();
    if task_goal.is_empty() || child_summary.is_empty() {
        return Ok(None);
    }

    let session_key = blockcell_core::build_session_key(origin_channel, origin_chat_id);
    let mut sources = vec![
        GhostEpisodeSource {
            source_type: "session".to_string(),
            source_key: session_key.clone(),
            role: "primary".to_string(),
        },
        GhostEpisodeSource {
            source_type: "chat".to_string(),
            source_key: origin_chat_id.to_string(),
            role: "context".to_string(),
        },
    ];
    if let Some(task_id) = task_id {
        sources.push(GhostEpisodeSource {
            source_type: "task".to_string(),
            source_key: task_id.to_string(),
            role: "delegation".to_string(),
        });
    }

    let boundary = GhostLearningBoundary {
        kind: GhostLearningBoundaryKind::DelegationEnd,
        session_key: Some(session_key),
        subject_key: Some(format!("chat:{}", origin_chat_id)),
        user_intent_summary: task_goal.to_string(),
        assistant_outcome_summary: child_summary.to_string(),
        tool_call_count: 0,
        memory_write_count: 0,
        correction_count: 0,
        preference_correction_count: 0,
        success: true,
        complexity_score: estimate_turn_complexity_score(task_goal),
        reusable_lesson: Some(truncate_str(child_summary, 240)),
    };

    persist_ghost_learning_boundary_with_config(config, paths, boundary, sources)
}

#[cfg(test)]
impl AgentRuntime {
    fn test_ghost_ledger(&self) -> GhostLedger {
        GhostLedger::open(&self.paths.ghost_ledger_db()).expect("open ghost ledger")
    }

    fn test_ghost_metrics(&self) -> crate::GhostMetricsSnapshot {
        crate::ghost_metrics_summary(&self.paths)
    }

    async fn test_trigger_pre_compress(&mut self) -> Result<()> {
        let session_key = blockcell_core::build_session_key("cli", "ghost-pre-compress");
        let history = vec![
            ChatMessage::user("figure out the correct deploy sequence"),
            ChatMessage::assistant("captured deploy analysis before compact"),
        ];
        self.capture_pre_compress_learning_boundary(&session_key, &history)
            .await
            .map(|_| ())
    }

    async fn test_trigger_session_end(&mut self) -> Result<()> {
        self.capture_main_session_end_learning_boundary()
            .await
            .map(|_| ())
    }

    async fn test_complete_delegated_task(
        &self,
        task_goal: &str,
        child_summary: &str,
    ) -> Result<Option<String>> {
        capture_delegation_end_learning_boundary_with_config(
            &self.config,
            &self.paths,
            "cli",
            "ghost-delegation",
            None,
            task_goal,
            child_summary,
        )
    }
}

/// Extract the first JSON object from potentially markdown-wrapped LLM output.
/// Handles ```json...```, ```...```, `<tool_call>` XML with `<parameter=argv>`,
/// bare `{...}` objects, and bare `[...]` arrays (wrapped as `{"argv":[...]}`).
#[allow(dead_code)]
fn extract_json_from_text(text: &str) -> String {
    // Try ```json ... ``` blocks first
    if let Some(start) = text.find("```json") {
        let after = &text[start + 7..];
        if let Some(end) = after.find("```") {
            return after[..end].trim().to_string();
        }
    }
    // Try ``` ... ``` blocks containing an object or array
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        if let Some(end) = after.find("```") {
            let candidate = after[..end].trim();
            if candidate.starts_with('{') || candidate.starts_with('[') {
                if candidate.starts_with('[') {
                    return format!("{{\"argv\": {}}}", candidate);
                }
                return candidate.to_string();
            }
        }
    }
    // Handle <tool_call> XML: extract argv from <parameter=argv>...</parameter>
    if text.contains("<parameter=argv>") {
        if let Some(start) = text.find("<parameter=argv>") {
            let after = &text[start + 16..];
            let end_tag = after.find("</parameter>").unwrap_or(after.len());
            let content = after[..end_tag].trim();
            if content.starts_with('[') {
                return format!("{{\"argv\": {}}}", content);
            }
            if content.starts_with('{') {
                return content.to_string();
            }
        }
    }
    // Fall back to first { ... } span
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if end >= start {
                return text[start..=end].to_string();
            }
        }
    }
    // Handle bare JSON arrays (wrap as {"argv": [...]})
    if let Some(start) = text.find('[') {
        if let Some(end) = text.rfind(']') {
            if end >= start {
                return format!("{{\"argv\": {}}}", &text[start..=end]);
            }
        }
    }
    text.trim().to_string()
}

#[allow(dead_code)]
fn build_script_skill_summary_prompt(
    user_question: &str,
    skill_name: &str,
    method_name: &str,
    skill_md: &str,
    script_output: &str,
) -> String {
    crate::skill_summary::SkillSummaryFormatter::build_prompt(
        user_question,
        skill_name,
        Some(method_name),
        skill_md,
        script_output,
    )
}

/// Free async function that runs a user message in the background.
/// Each message gets its own AgentRuntime so the main loop stays responsive.
#[allow(clippy::too_many_arguments)]
async fn run_message_task(
    config: Config,
    paths: Paths,
    provider_pool: Arc<ProviderPool>,
    tool_registry: ToolRegistry,
    task_manager: TaskManager,
    outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
    confirm_tx: Option<mpsc::Sender<ConfirmRequest>>,
    memory_store: Option<MemoryStoreHandle>,
    capability_registry: Option<CapabilityRegistryHandle>,
    core_evolution: Option<CoreEvolutionHandle>,
    event_tx: Option<broadcast::Sender<String>>,
    agent_id: Option<String>,
    event_emitter: EventEmitterHandle,
    msg: InboundMessage,
    task_id: String,
) {
    task_manager.set_running(&task_id).await;

    let mut runtime = match AgentRuntime::new(config, paths, provider_pool, tool_registry) {
        Ok(r) => r,
        Err(e) => {
            task_manager.set_failed(&task_id, &format!("{}", e)).await;
            if let Some(tx) = &outbound_tx {
                let mut outbound =
                    OutboundMessage::new(&msg.channel, &msg.chat_id, &format!("❌ {}", e));
                outbound.account_id = msg.account_id.clone();
                let _ = tx.send(outbound).await;
            }
            return;
        }
    };

    // Wire up channels
    if let Some(tx) = outbound_tx.clone() {
        runtime.set_outbound(tx);
    }
    if let Some(tx) = confirm_tx {
        runtime.set_confirm(tx);
    }
    runtime.set_task_manager(task_manager.clone());
    runtime.set_agent_id(agent_id.clone());
    runtime.set_event_emitter(event_emitter);
    if let Some(store) = memory_store {
        runtime.set_memory_store(store);
    }
    if let Err(e) = runtime.init_memory_file_store() {
        tracing::warn!(error = %e, "Failed to initialize file memory store");
    }
    if let Err(e) = runtime.init_skill_file_store() {
        tracing::warn!(error = %e, "Failed to initialize skill file store");
    }
    if let Some(registry) = capability_registry {
        runtime.set_capability_registry(registry);
    }
    if let Some(core_evo) = core_evolution {
        runtime.set_core_evolution(core_evo);
    }
    if let Some(tx) = event_tx.clone() {
        runtime.set_event_tx(tx);
    }

    let error_chat_id = msg.chat_id.clone();

    match runtime.process_message(msg).await {
        Ok(response) => {
            debug!(task_id = %task_id, response_len = response.len(), "Message task completed");
            // Remove completed message tasks immediately — the response was already
            // sent via outbound_tx. Only subagent tasks persist in the task list.
            task_manager.remove_task(&task_id).await;
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            error!(task_id = %task_id, error = %e, "Message task failed");
            if let Some(ref event_tx) = event_tx {
                let _ = event_tx.send(
                    serde_json::json!({
                        "type": "error",
                        "agent_id": agent_id.clone().unwrap_or_else(|| "default".to_string()),
                        "chat_id": error_chat_id,
                        "task_id": task_id.clone(),
                        "message": err_msg,
                    })
                    .to_string(),
                );
            }
            // Keep failed tasks briefly for visibility, then let tick cleanup handle them
            task_manager.set_failed(&task_id, &err_msg).await;
        }
    }
}

/// Free async function that runs a subagent task in the background.
/// This is separate from `AgentRuntime` methods to break the recursive async type
/// chain that would otherwise prevent the future from being `Send`.
#[allow(clippy::too_many_arguments)]
async fn run_subagent_task(
    config: Config,
    paths: Paths,
    provider_pool: Arc<ProviderPool>,
    task_manager: TaskManager,
    outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
    task_str: String,
    task_id: String,
    label: String,
    origin_channel: String,
    origin_chat_id: String,
    agent_id: Option<String>,
    event_tx: Option<broadcast::Sender<String>>,
    origin_history_seed: Vec<ChatMessage>,
    event_emitter: EventEmitterHandle,
) {
    // Create the task entry first, then immediately mark it running.
    // This ensures set_running() never operates on a non-existent task ID.
    task_manager
        .create_task(
            &task_id,
            &label,
            &task_str,
            &origin_channel,
            &origin_chat_id,
            agent_id.as_deref(),
            true,
        )
        .await;
    task_manager.set_running(&task_id).await;
    task_manager.set_progress(&task_id, "Processing...").await;

    // Create isolated runtime with restricted tools
    let tool_registry = AgentRuntime::subagent_tool_registry();
    let learning_config = config.clone();
    let learning_paths = paths.clone();
    let mut sub_runtime = match AgentRuntime::new(config, paths, provider_pool, tool_registry) {
        Ok(r) => r,
        Err(e) => {
            task_manager.set_failed(&task_id, &format!("{}", e)).await;
            return;
        }
    };
    sub_runtime.set_task_manager(task_manager.clone());
    sub_runtime.set_agent_id(agent_id.clone());
    sub_runtime.set_event_emitter(event_emitter);
    if let Err(e) = sub_runtime.init_memory_file_store() {
        tracing::warn!(error = %e, "Failed to initialize subagent file memory store");
    }
    if let Err(e) = sub_runtime.init_skill_file_store() {
        tracing::warn!(error = %e, "Failed to initialize subagent skill file store");
    }

    // Create a unique session key for this subagent
    let session_key = format!("subagent:{}", task_id);
    if !origin_history_seed.is_empty() {
        let _ = sub_runtime
            .session_store
            .save(&session_key, &origin_history_seed);
    }

    let mut subagent_metadata = build_subagent_metadata(agent_id.as_deref());
    if !subagent_metadata.is_object() {
        subagent_metadata = serde_json::json!({});
    }
    if let Some(obj) = subagent_metadata.as_object_mut() {
        obj.insert(
            "origin_channel".to_string(),
            serde_json::json!(origin_channel.clone()),
        );
        obj.insert(
            "origin_chat_id".to_string(),
            serde_json::json!(origin_chat_id.clone()),
        );
    }

    let inbound = build_subagent_inbound_message(
        &task_str,
        &origin_channel,
        &origin_chat_id,
        &subagent_metadata,
        &session_key,
    );
    let result = sub_runtime.process_message(inbound).await;

    match result {
        Ok(result) => {
            task_manager.set_completed(&task_id, &result).await;
            info!(task_id = %task_id, label = %label, "Subagent completed");

            if let Err(err) = capture_delegation_end_learning_boundary_with_config(
                &learning_config,
                &learning_paths,
                &origin_channel,
                &origin_chat_id,
                Some(&task_id),
                &task_str,
                &result,
            ) {
                warn!(
                    task_id = %task_id,
                    error = %err,
                    "Failed to persist delegation-end ghost learning episode"
                );
            }
            if let Some(manager) = sub_runtime.ghost_memory_lifecycle.as_ref() {
                manager.on_delegation(&task_str, &result, &session_key);
            }

            deliver_subagent_result_to_origin(
                &origin_channel,
                &origin_chat_id,
                &result,
                agent_id.as_deref().unwrap_or("default"),
                outbound_tx.clone(),
                event_tx.clone(),
            )
            .await;
        }
        Err(e) => {
            let err_msg = format!("{}", e);
            task_manager.set_failed(&task_id, &err_msg).await;
            error!(task_id = %task_id, error = %e, "Subagent failed");

            let short_id = truncate_str(&task_id, 8);
            let failure_message = format!(
                "\n❌ 后台任务失败: **{}** (ID: {})\n错误: {}",
                label, short_id, err_msg
            );
            deliver_subagent_result_to_origin(
                &origin_channel,
                &origin_chat_id,
                &failure_message,
                agent_id.as_deref().unwrap_or("default"),
                outbound_tx.clone(),
                event_tx.clone(),
            )
            .await;
        }
    }
}

async fn deliver_subagent_result_to_origin(
    origin_channel: &str,
    origin_chat_id: &str,
    content: &str,
    agent_id: &str,
    outbound_tx: Option<mpsc::Sender<OutboundMessage>>,
    event_tx: Option<broadcast::Sender<String>>,
) {
    if origin_channel == "ws" {
        if let Some(event_tx) = event_tx {
            let event = serde_json::json!({
                "type": "message_done",
                "agent_id": agent_id,
                "chat_id": origin_chat_id,
                "task_id": "",
                "content": content,
                "tool_calls": 0,
                "duration_ms": 0,
                "media": [],
                "background_delivery": true,
                "delivery_kind": "subagent",
            });
            let _ = event_tx.send(event.to_string());
        }
        return;
    }

    if let Some(tx) = outbound_tx {
        let notification = OutboundMessage::new(origin_channel, origin_chat_id, content);
        let _ = tx.send(notification).await;
    }
}

fn append_ephemeral_context_to_latest_user_message(
    messages: &[ChatMessage],
    context_block: Option<&str>,
) -> Vec<ChatMessage> {
    let Some(context_block) = context_block
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return messages.to_vec();
    };
    let mut api_messages = messages.to_vec();
    if let Some(message) = api_messages
        .iter_mut()
        .rev()
        .find(|message| message.role == "user")
    {
        let base = chat_message_text(message);
        *message = ChatMessage::user(&format!("{}\n\n{}", base, context_block));
    }
    api_messages
}

/// Build outbound metadata containing reply-to information from an inbound message.
/// Only applies to group chats — single/DM chats return Null so no quoting is added.
fn extract_reply_metadata(msg: &InboundMessage) -> serde_json::Value {
    match msg.channel.as_str() {
        "telegram" => {
            // Telegram group/supergroup chat_ids are negative integers
            let is_group = msg.chat_id.parse::<i64>().unwrap_or(0) < 0;
            if is_group {
                if let Some(mid) = msg.metadata.get("message_id") {
                    return serde_json::json!({ "reply_to_message_id": mid });
                }
            }
            serde_json::Value::Null
        }
        "feishu" | "lark" => {
            // Use chat_type from metadata: "group" = group chat, "p2p" = direct message
            let is_group = msg.metadata.get("chat_type").and_then(|v| v.as_str()) == Some("group");
            if is_group {
                if let Some(mid) = msg.metadata.get("message_id").and_then(|v| v.as_str()) {
                    return serde_json::json!({ "reply_to_message_id": mid });
                }
            }
            serde_json::Value::Null
        }
        "discord" => {
            // Discord server messages carry a non-empty guild_id; DMs do not
            let in_guild = msg
                .metadata
                .get("guild_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .is_some();
            if in_guild {
                if let Some(mid) = msg.metadata.get("message_id").and_then(|v| v.as_str()) {
                    return serde_json::json!({ "reply_to_message_id": mid });
                }
            }
            serde_json::Value::Null
        }
        "slack" => {
            // Slack DM channel IDs start with 'D'; public/private channels start with 'C'/'G'
            let is_dm = msg.chat_id.starts_with('D');
            if !is_dm {
                if let Some(ts) = msg.metadata.get("ts").and_then(|v| v.as_str()) {
                    return serde_json::json!({ "thread_ts": ts });
                }
            }
            serde_json::Value::Null
        }
        "dingtalk" => {
            // DingTalk group chats have conversation_type "2"
            let is_group = msg
                .metadata
                .get("conversation_type")
                .and_then(|v| v.as_str())
                == Some("2");
            if is_group {
                if let Some(mid) = msg.metadata.get("msg_id").and_then(|v| v.as_str()) {
                    return serde_json::json!({ "reply_to_message_id": mid });
                }
            }
            serde_json::Value::Null
        }
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::types::LLMResponse;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    struct TestProvider;
    struct StreamingRetryProvider {
        attempts: AtomicUsize,
    }
    struct StreamingCloseProvider;
    struct UnifiedEntryProvider {
        calls: AtomicUsize,
    }
    struct RecallCaptureProvider {
        calls: Mutex<Vec<Vec<ChatMessage>>>,
    }
    struct SequencedGhostProvider;
    struct ReviewAndCaptureProvider {
        calls: Mutex<Vec<Vec<ChatMessage>>>,
        review_calls: AtomicUsize,
    }
    struct BoundaryFlushProvider {
        calls: Mutex<Vec<Vec<ChatMessage>>>,
        flush_calls: AtomicUsize,
    }
    struct BoundaryMemoryProvider;
    struct ProviderToolCaptureProvider {
        seen_tools: Mutex<Vec<Vec<serde_json::Value>>>,
    }
    struct RuntimeProviderTool {
        calls: Mutex<Vec<serde_json::Value>>,
    }

    fn extract_active_skill_name(system_text: &str) -> Option<String> {
        let marker = "## Active Skill: ";
        let start = system_text.find(marker)?;
        let rest = &system_text[start + marker.len()..];
        let skill_name = rest.lines().next()?.trim();
        if skill_name.is_empty() {
            None
        } else {
            Some(skill_name.to_string())
        }
    }

    fn drain_ws_events(event_rx: &mut broadcast::Receiver<String>) -> Vec<serde_json::Value> {
        let mut events = Vec::new();
        loop {
            match event_rx.try_recv() {
                Ok(payload) => {
                    events.push(
                        serde_json::from_str::<serde_json::Value>(&payload)
                            .expect("parse ws event"),
                    );
                }
                Err(broadcast::error::TryRecvError::Empty)
                | Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        events
    }

    fn collect_event_types(events: &[serde_json::Value]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event.get("type").and_then(|value| value.as_str()))
            .map(str::to_string)
            .collect()
    }

    fn contains_event_subsequence(events: &[String], expected: &[&str]) -> bool {
        let mut cursor = 0usize;
        for event in events {
            if cursor < expected.len() && event == expected[cursor] {
                cursor += 1;
            }
        }
        cursor == expected.len()
    }

    #[test]
    fn apply_skill_fallback_response_uses_fallback_for_empty_output() {
        let fallback = "当前无法获取腾讯新闻数据，请先检查 CLI 安装、API Key 配置或网络环境。";

        assert_eq!(
            apply_skill_fallback_response(String::new(), Some(fallback)),
            fallback
        );
        assert_eq!(
            apply_skill_fallback_response("   \n\t".to_string(), Some(fallback)),
            fallback
        );
    }

    #[test]
    fn apply_skill_fallback_response_keeps_non_empty_output() {
        assert_eq!(
            apply_skill_fallback_response("  ok  ".to_string(), Some("fallback")),
            "ok"
        );
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for TestProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            let system_text = messages.first().map(chat_message_text).unwrap_or_default();
            let user_text = messages
                .iter()
                .rev()
                .find(|msg| msg.role == "user")
                .map(chat_message_text)
                .unwrap_or_default();
            let latest_tool_text = messages
                .iter()
                .rev()
                .find(|msg| msg.role == "tool")
                .map(chat_message_text);
            let active_skill_name = extract_active_skill_name(&system_text);

            let response = if matches!(active_skill_name.as_deref(), Some("compat_local_demo"))
                && latest_tool_text.is_none()
            {
                LLMResponse {
                    content: Some("准备调用兼容本地脚本".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "test-exec-local-compat".to_string(),
                        name: "exec_local".to_string(),
                        arguments: serde_json::json!({
                            "path": "scripts/hello.sh",
                            "runner": "sh",
                            "args": ["skill"],
                            "cwd_mode": "skill"
                        }),
                        thought_signature: None,
                    }],
                    finish_reason: "tool_calls".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if matches!(active_skill_name.as_deref(), Some("compat_local_demo")) {
                let stdout = latest_tool_text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .and_then(|value| {
                        value
                            .get("stdout")
                            .and_then(|value| value.as_str())
                            .map(str::trim)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                LLMResponse {
                    content: Some(format!("local exec result: {}", stdout)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if matches!(
                active_skill_name.as_deref(),
                Some("local_demo" | "legacy_script_demo" | "cli_demo")
            ) && latest_tool_text.is_none()
            {
                let (path, args) = match active_skill_name.as_deref() {
                    Some("cli_demo") => ("bin/cli.sh", vec!["demo"]),
                    _ => ("scripts/hello.sh", vec!["skill"]),
                };
                LLMResponse {
                    content: Some("准备调用本地脚本".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "test-exec-skill-script".to_string(),
                        name: "exec_skill_script".to_string(),
                        arguments: serde_json::json!({
                            "path": path,
                            "runner": "sh",
                            "args": args,
                            "cwd_mode": "skill"
                        }),
                        thought_signature: None,
                    }],
                    finish_reason: "tool_calls".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if matches!(
                active_skill_name.as_deref(),
                Some("local_demo" | "legacy_script_demo" | "cli_demo")
            ) {
                let stdout = latest_tool_text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .and_then(|value| {
                        value
                            .get("stdout")
                            .and_then(|value| value.as_str())
                            .map(str::trim)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                LLMResponse {
                    content: Some(format!("local exec result: {}", stdout)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if user_text.contains("技能说明摘要") && user_text.contains("执行结果")
            {
                let execution_result = user_text
                    .split("执行结果：")
                    .nth(1)
                    .or_else(|| user_text.split("执行结果:").nth(1))
                    .unwrap_or_default()
                    .trim();
                LLMResponse {
                    content: Some(format!("summary: {}", execution_result)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else {
                LLMResponse {
                    content: Some(format!("mock answer: {}", user_text)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            };

            Ok(response)
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for ProviderToolCaptureProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            self.seen_tools.lock().unwrap().push(tools.to_vec());
            let latest_tool_text = messages
                .iter()
                .rev()
                .find(|msg| msg.role == "tool")
                .map(chat_message_text);

            if let Some(tool_text) = latest_tool_text {
                return Ok(LLMResponse {
                    content: Some(format!("provider result: {}", tool_text)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                });
            }

            Ok(LLMResponse {
                content: Some("checking external memory".to_string()),
                reasoning_content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "provider-tool-call".to_string(),
                    name: "external_memory_lookup".to_string(),
                    arguments: serde_json::json!({"query": "canary rollout"}),
                    thought_signature: None,
                }],
                finish_reason: "tool_calls".to_string(),
                usage: serde_json::Value::Null,
            })
        }
    }

    impl crate::ghost_memory_provider::GhostMemoryProvider for RuntimeProviderTool {
        fn name(&self) -> &'static str {
            "runtime_provider_tool"
        }

        fn get_tool_schemas(&self) -> Vec<serde_json::Value> {
            vec![serde_json::json!({
                "name": "external_memory_lookup",
                "description": "Lookup provider-backed external memory.",
                "parameters": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"]
                }
            })]
        }

        fn handle_tool_call(
            &self,
            tool_name: &str,
            args: serde_json::Value,
        ) -> Result<serde_json::Value> {
            assert_eq!(tool_name, "external_memory_lookup");
            self.calls.lock().unwrap().push(args.clone());
            Ok(serde_json::json!({
                "success": true,
                "provider": self.name(),
                "query": args.get("query").cloned().unwrap_or(serde_json::Value::Null),
                "memory": "Prefer canary rollout before broad release."
            }))
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for StreamingRetryProvider {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            Ok(LLMResponse {
                content: Some("unexpected non-stream call".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: serde_json::Value::Null,
            })
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> blockcell_core::Result<mpsc::Receiver<StreamChunk>> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = mpsc::channel(8);

            tokio::spawn(async move {
                if attempt == 0 {
                    let _ = tx
                        .send(StreamChunk::TextDelta {
                            delta: "partial".to_string(),
                        })
                        .await;
                    let _ = tx
                        .send(StreamChunk::Error {
                            message: "temporary stream failure".to_string(),
                        })
                        .await;
                    return;
                }

                let response = LLMResponse {
                    content: Some("final answer".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                };
                let _ = tx
                    .send(StreamChunk::TextDelta {
                        delta: "final answer".to_string(),
                    })
                    .await;
                let _ = tx.send(StreamChunk::Done { response }).await;
            });

            Ok(rx)
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for StreamingCloseProvider {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            Ok(LLMResponse {
                content: Some("unexpected non-stream call".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: serde_json::Value::Null,
            })
        }

        async fn chat_stream(
            &self,
            _messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> blockcell_core::Result<mpsc::Receiver<StreamChunk>> {
            let (tx, rx) = mpsc::channel(8);
            tokio::spawn(async move {
                let _ = tx
                    .send(StreamChunk::TextDelta {
                        delta: "closed answer".to_string(),
                    })
                    .await;
            });
            Ok(rx)
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for UnifiedEntryProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            _tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);

            let system_text = messages.first().map(chat_message_text).unwrap_or_default();
            let user_text = messages
                .iter()
                .rev()
                .find(|msg| msg.role == "user")
                .map(chat_message_text)
                .unwrap_or_default();
            let latest_tool_msg = messages.iter().rev().find(|msg| msg.role == "tool");
            let latest_tool_name = latest_tool_msg
                .and_then(|msg| msg.name.as_deref())
                .unwrap_or_default()
                .to_string();
            let latest_tool_text = latest_tool_msg.map(chat_message_text);
            let active_skill_name = extract_active_skill_name(&system_text);

            let response = if matches!(active_skill_name.as_deref(), Some("compat_local_demo"))
                && latest_tool_name != "exec_local"
            {
                LLMResponse {
                    content: Some("进入 compat_local_demo".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "skill-exec-local-compat".to_string(),
                        name: "exec_local".to_string(),
                        arguments: serde_json::json!({
                            "path": "scripts/hello.sh",
                            "runner": "sh",
                            "args": ["skill"],
                            "cwd_mode": "skill"
                        }),
                        thought_signature: None,
                    }],
                    finish_reason: "tool_calls".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if matches!(active_skill_name.as_deref(), Some("compat_local_demo")) {
                let stdout = latest_tool_text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .and_then(|value| {
                        value
                            .get("stdout")
                            .and_then(|value| value.as_str())
                            .map(str::trim)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                LLMResponse {
                    content: Some(format!("local exec result: {}", stdout)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if matches!(active_skill_name.as_deref(), Some("local_demo"))
                && latest_tool_name != "exec_skill_script"
            {
                LLMResponse {
                    content: Some("进入 local_demo".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "skill-exec-skill-script".to_string(),
                        name: "exec_skill_script".to_string(),
                        arguments: serde_json::json!({
                            "path": "scripts/hello.sh",
                            "runner": "sh",
                            "args": ["skill"],
                            "cwd_mode": "skill"
                        }),
                        thought_signature: None,
                    }],
                    finish_reason: "tool_calls".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if matches!(active_skill_name.as_deref(), Some("local_demo")) {
                let stdout = latest_tool_text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .and_then(|value| {
                        value
                            .get("stdout")
                            .and_then(|value| value.as_str())
                            .map(str::trim)
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                LLMResponse {
                    content: Some(format!("local exec result: {}", stdout)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if user_text.contains("技能说明摘要") && user_text.contains("执行结果")
            {
                let execution_result = user_text
                    .split("执行结果：")
                    .nth(1)
                    .or_else(|| user_text.split("执行结果:").nth(1))
                    .unwrap_or_default()
                    .trim();
                LLMResponse {
                    content: Some(format!("summary: {}", execution_result)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if latest_tool_name == "list_dir" {
                let path = latest_tool_text
                    .as_deref()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
                    .and_then(|value| {
                        value
                            .get("path")
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| ".".to_string());
                LLMResponse {
                    content: Some(format!("目录内容：{}", path)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if user_text.contains("查看当前目录下文件") {
                LLMResponse {
                    content: Some("先列目录".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "general-list-dir".to_string(),
                        name: "list_dir".to_string(),
                        arguments: serde_json::json!({ "path": "." }),
                        thought_signature: None,
                    }],
                    finish_reason: "tool_calls".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else if user_text.contains("运行本地脚本") {
                LLMResponse {
                    content: Some("改用 skill".to_string()),
                    reasoning_content: None,
                    tool_calls: vec![ToolCallRequest {
                        id: "activate-skill-local-demo".to_string(),
                        name: ACTIVATE_SKILL_TOOL_NAME.to_string(),
                        arguments: serde_json::json!({
                            "skill_name": "local_demo",
                            "goal": "运行本地脚本"
                        }),
                        thought_signature: None,
                    }],
                    finish_reason: "tool_calls".to_string(),
                    usage: serde_json::Value::Null,
                }
            } else {
                LLMResponse {
                    content: Some(format!("mock answer: {}", user_text)),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                }
            };

            Ok(response)
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for RecallCaptureProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            self.calls.lock().unwrap().push(messages.to_vec());

            let system_text = messages.first().map(chat_message_text).unwrap_or_default();
            if system_text.contains("quiet Ghost learning reviewer") && !tools.is_empty() {
                return Ok(LLMResponse {
                    content: Some("no durable learning".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                });
            }

            Ok(LLMResponse {
                content: Some("mock answer: recall applied".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: serde_json::Value::Null,
            })
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for SequencedGhostProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            let system_text = messages.first().map(chat_message_text).unwrap_or_default();
            let user_text = messages
                .iter()
                .rev()
                .find(|msg| msg.role == "user")
                .map(chat_message_text)
                .unwrap_or_default();

            if system_text.contains("quiet Ghost learning reviewer") && !tools.is_empty() {
                return Ok(LLMResponse {
                    content: Some("no durable learning".to_string()),
                    reasoning_content: None,
                    tool_calls: Vec::new(),
                    finish_reason: "stop".to_string(),
                    usage: serde_json::Value::Null,
                });
            }

            Ok(LLMResponse {
                content: Some(format!("mock answer: {}", user_text)),
                reasoning_content: None,
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: serde_json::Value::Null,
            })
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for ReviewAndCaptureProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            let system_text = messages.first().map(chat_message_text).unwrap_or_default();
            if system_text.contains("quiet Ghost learning reviewer") && !tools.is_empty() {
                let review_index = self.review_calls.fetch_add(1, Ordering::SeqCst);
                let tool_calls = if review_index == 0 {
                    vec![
                        ToolCallRequest {
                            id: "review-user-memory".to_string(),
                            name: "memory_manage".to_string(),
                            arguments: serde_json::json!({
                                "action": "add",
                                "target": "user",
                                "content": "User prefers canary-first rollout."
                            }),
                            thought_signature: None,
                        },
                        ToolCallRequest {
                            id: "review-project-memory".to_string(),
                            name: "memory_manage".to_string(),
                            arguments: serde_json::json!({
                                "action": "add",
                                "target": "memory",
                                "content": "Confirm rollback plan before release verification."
                            }),
                            thought_signature: None,
                        },
                    ]
                } else {
                    Vec::new()
                };
                return Ok(LLMResponse {
                    content: None,
                    reasoning_content: None,
                    finish_reason: if tool_calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    }
                    .to_string(),
                    tool_calls,
                    usage: serde_json::Value::Null,
                });
            }

            self.calls.lock().unwrap().push(messages.to_vec());
            let user_text = messages
                .iter()
                .rev()
                .find(|msg| msg.role == "user")
                .map(chat_message_text)
                .unwrap_or_default();
            Ok(LLMResponse {
                content: Some(format!("mock answer: {}", user_text)),
                reasoning_content: None,
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: serde_json::Value::Null,
            })
        }
    }

    #[async_trait::async_trait]
    impl blockcell_providers::Provider for BoundaryFlushProvider {
        async fn chat(
            &self,
            messages: &[ChatMessage],
            tools: &[serde_json::Value],
        ) -> blockcell_core::Result<LLMResponse> {
            self.calls.lock().unwrap().push(messages.to_vec());
            let latest_user_text = messages
                .iter()
                .rev()
                .find(|message| message.role == "user")
                .map(chat_message_text)
                .unwrap_or_default();
            if latest_user_text.contains("__ghost_memory_flush_sentinel") && !tools.is_empty() {
                let call_idx = self.flush_calls.fetch_add(1, Ordering::SeqCst);
                let tool_calls = if call_idx == 0 {
                    vec![ToolCallRequest {
                        id: "flush-memory".to_string(),
                        name: "memory_manage".to_string(),
                        arguments: serde_json::json!({
                            "action": "add",
                            "target": "user",
                            "content": "User prefers checking rollback order before deploy compression."
                        }),
                        thought_signature: None,
                    }]
                } else {
                    Vec::new()
                };
                return Ok(LLMResponse {
                    content: None,
                    reasoning_content: None,
                    finish_reason: if tool_calls.is_empty() {
                        "stop"
                    } else {
                        "tool_calls"
                    }
                    .to_string(),
                    tool_calls,
                    usage: serde_json::Value::Null,
                });
            }

            Ok(LLMResponse {
                content: Some("mock answer".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
                finish_reason: "stop".to_string(),
                usage: serde_json::Value::Null,
            })
        }
    }

    impl crate::ghost_memory_provider::GhostMemoryProvider for BoundaryMemoryProvider {
        fn name(&self) -> &'static str {
            "boundary_test"
        }

        fn on_pre_compress(&self, _messages: &[String], _session_id: &str) -> Result<String> {
            Ok("preserve provider-derived rollback preference before compression".to_string())
        }

        fn on_session_end(&self, _messages: &[String], _session_id: &str) -> Result<()> {
            Ok(())
        }

        fn on_session_boundary_context(
            &self,
            _messages: &[String],
            _session_id: &str,
        ) -> Result<String> {
            Ok("preserve provider-derived session-end deploy preference".to_string())
        }
    }

    #[test]
    fn test_core_tools_contains_toggle_manage() {
        assert!(global_core_tool_names()
            .iter()
            .any(|name| name == "toggle_manage"));
    }

    #[test]
    fn test_path_within_base_allows_normal_child_path() {
        let base = PathBuf::from("/tmp/workspace");
        let candidate = base.join("skills/new/SKILL.py");
        assert!(is_path_within_base(&base, &candidate));
    }

    #[test]
    fn test_path_within_base_blocks_nonexistent_traversal() {
        let base = PathBuf::from("/tmp/workspace");
        let candidate = base.join("../../etc/passwd");
        assert!(!is_path_within_base(&base, &candidate));
    }

    #[test]
    fn test_tool_result_indicates_error_for_json_error_field() {
        let result = r#"{"error":"Permission denied: blocked"}"#;
        assert!(tool_result_indicates_error(result));
    }

    #[test]
    fn test_tool_result_indicates_error_does_not_use_failed_substring() {
        let result = "Task succeeded, previous attempt failed but recovered.";
        assert!(!tool_result_indicates_error(result));
    }

    #[test]
    fn test_should_supplement_tool_schema_for_validation_error() {
        let result = "Error: Validation error: Missing required parameter: path";
        assert!(should_supplement_tool_schema(result));
    }

    #[test]
    fn test_should_supplement_tool_schema_for_config_error() {
        let result = "Error: Config error: 'enabled' (boolean) is required for 'set' action";
        assert!(should_supplement_tool_schema(result));
    }

    #[test]
    fn test_should_supplement_tool_schema_ignores_permission_denied() {
        let result = "Error: Tool error: Permission denied: path blocked";
        assert!(!should_supplement_tool_schema(result));
    }

    #[test]
    fn test_resolve_routed_agent_id_from_metadata() {
        let metadata = serde_json::json!({
            "route_agent_id": "ops"
        });

        assert_eq!(resolve_routed_agent_id(&metadata).as_deref(), Some("ops"));
        assert_eq!(resolve_routed_agent_id(&serde_json::Value::Null), None);
    }

    #[test]
    fn test_build_subagent_inbound_for_structured_skill_task_uses_forced_skill_name() {
        let inbound = build_subagent_inbound_message(
            "__SKILL_EXEC__:weather:北京天气",
            "cli",
            "chat-1",
            &serde_json::json!({
                "route_agent_id": "ops"
            }),
            "subagent:test",
        );

        assert_eq!(inbound.content, "北京天气");
        assert_eq!(
            inbound
                .metadata
                .get("forced_skill_name")
                .and_then(|value| value.as_str()),
            Some("weather")
        );
        assert_eq!(
            inbound
                .metadata
                .get("subagent_session_key")
                .and_then(|value| value.as_str()),
            Some("subagent:test")
        );
        assert!(inbound.metadata.get("skill_script").is_none());
        assert!(inbound.metadata.get("skill_script_kind").is_none());
        assert!(inbound.metadata.get("skill_python").is_none());
        assert!(inbound.metadata.get("skill_rhai").is_none());
        assert!(inbound.metadata.get("skill_markdown").is_none());
    }

    #[test]
    fn test_parse_spawn_task_forces_explicit_skill_request() {
        let parsed = parse_spawn_task_forced_skill_request(
            "使用已安装的 xiaohongshu 技能：先获取推荐流 feeds，然后定位第15条笔记",
        );

        assert_eq!(
            parsed,
            Some((
                "xiaohongshu".to_string(),
                "先获取推荐流 feeds，然后定位第15条笔记".to_string()
            ))
        );
    }

    #[test]
    fn test_subagent_metadata_preserves_route_agent_id() {
        let metadata = build_subagent_metadata(Some("ops"));

        assert_eq!(
            metadata.get("route_agent_id").and_then(|v| v.as_str()),
            Some("ops")
        );
    }

    #[test]
    fn test_global_core_tool_names_excludes_email() {
        let names = global_core_tool_names();

        assert!(names.iter().any(|name| name == "toggle_manage"));
        assert!(names.iter().any(|name| name == "memory_query"));
        assert!(names.iter().any(|name| name == "list_skills"));
        assert!(!names.iter().any(|name| name == "email"));
        assert!(!names.iter().any(|name| name == "finance_api"));
        assert!(!names.iter().any(|name| name == "read_file"));
    }

    #[test]
    fn test_active_tool_names_for_skill_include_kernel_and_declared_tools() {
        use crate::context::ActiveSkillContext;

        let available: HashSet<String> = [
            "memory_query",
            "memory_upsert",
            "memory_forget",
            "spawn",
            "list_tasks",
            "list_skills",
            "toggle_manage",
            "finance_api",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let skill = ActiveSkillContext {
            name: "stock_analysis".to_string(),
            prompt_md: String::new(),
            inject_prompt_md: true,
            tools: vec!["finance_api".to_string()],
            fallback_message: None,
            source: blockcell_skills::manager::SkillSource::BlockCell,
        };

        let tool_names = resolve_effective_tool_names(
            &Config::default(),
            InteractionMode::Skill,
            None,
            Some(&skill),
            &[IntentCategory::Unknown],
            &available,
        );

        assert!(tool_names.contains(&"finance_api".to_string()));
        assert!(tool_names.contains(&"memory_query".to_string()));
        assert!(tool_names.contains(&"toggle_manage".to_string()));
        assert_eq!(
            tool_names
                .iter()
                .filter(|name| name.as_str() == "finance_api")
                .count(),
            1
        );
    }

    #[test]
    fn test_tool_context_supports_optional_event_emitter() {
        use blockcell_core::system_event::{EventPriority, SystemEvent};
        use blockcell_tools::{SystemEventEmitter, ToolContext};
        use std::path::PathBuf;
        use std::sync::Arc;

        struct NoopEmitter;

        impl SystemEventEmitter for NoopEmitter {
            fn emit(&self, _event: SystemEvent) {}

            fn emit_simple(
                &self,
                kind: &str,
                source: &str,
                priority: EventPriority,
                title: &str,
                summary: &str,
            ) {
                let _ = SystemEvent::new_main_session(kind, source, priority, title, summary);
            }
        }

        let ctx = ToolContext {
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
            memory_store: None,
            memory_file_store: None,
            ghost_memory_lifecycle: None,
            skill_file_store: None,
            session_search: None,
            outbound_tx: None,
            spawn_handle: None,
            capability_registry: None,
            core_evolution: None,
            event_emitter: Some(Arc::new(NoopEmitter)),
            channel_contacts_file: None,
            response_cache: None,
            #[allow(deprecated)]
            skill_mutex: Some(Arc::new(crate::skill_mutex::SkillMutex::new())
                as blockcell_tools::SkillMutexHandle),
        };

        assert!(ctx.event_emitter.is_some());
    }

    #[test]
    fn test_skill_decision_engine_normalizes_selected_skill_name() {
        use crate::skill_decision::SkillDecisionEngine;

        let candidates = vec![
            ("xiaohongshu".to_string(), "小红书相关能力".to_string()),
            ("weather".to_string(), "天气查询".to_string()),
        ];

        let exact = SkillDecisionEngine::normalize_selected_skill_name("xiaohongshu", &candidates);
        let partial = SkillDecisionEngine::normalize_selected_skill_name(
            "最合适的是 xiaohongshu。",
            &candidates,
        );
        let missing = SkillDecisionEngine::normalize_selected_skill_name("finance", &candidates);

        assert_eq!(exact.as_deref(), Some("xiaohongshu"));
        assert_eq!(partial.as_deref(), Some("xiaohongshu"));
        assert_eq!(missing, None);
    }

    #[test]
    fn test_expand_history_stubs_with_cache_restores_cached_content() {
        let cache = crate::response_cache::ResponseCache::new();
        let session_key = "ws:chat-1";
        let cached_list = (1..=18)
            .map(|i| {
                format!(
                    "{}. 第{}条推荐，包含足够长的标题、作者信息、摘要说明以及若干补充字段，用来模拟小红书推荐流里带隐藏定位字段的大列表返回结果。",
                    i, i
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let stub = cache
            .maybe_cache_and_stub(session_key, &cached_list, true)
            .expect("content should be cached");
        let history = vec![ChatMessage::assistant(&stub)];

        let expanded = expand_history_stubs_with_cache(&cache, session_key, &history);

        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].content.as_str(), Some(cached_list.as_str()));
    }

    #[test]
    fn test_resolve_skill_run_mode_prefers_explicit_metadata() {
        let msg = InboundMessage {
            channel: "cron".to_string(),
            account_id: None,
            sender_id: "system".to_string(),
            chat_id: "chat-1".to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "skill_run_mode": "test",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        assert_eq!(resolve_skill_run_mode(&msg), SkillRunMode::Test);
    }

    #[test]
    fn test_resolve_cron_deliver_target_requires_cron_mode_and_delivery_fields() {
        let msg = InboundMessage {
            channel: "cron".to_string(),
            account_id: None,
            sender_id: "system".to_string(),
            chat_id: "chat-1".to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "skill_run_mode": "cron",
                "deliver": true,
                "deliver_channel": "ws",
                "deliver_to": "chat-2",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        assert_eq!(
            resolve_cron_deliver_target(&msg),
            Some(("ws".to_string(), "chat-2".to_string()))
        );
    }

    #[test]
    fn test_build_script_skill_summary_prompt_includes_skill_md_brief() {
        let prompt = build_script_skill_summary_prompt(
            "帮我搜一下小红书露营装备",
            "xiaohongshu",
            "search",
            "请优先提炼结果，不要冗长输出。",
            "找到 3 条高互动笔记",
        );

        assert!(prompt.contains("帮我搜一下小红书露营装备"));
        assert!(prompt.contains("xiaohongshu"));
        assert!(prompt.contains("search"));
        assert!(prompt.contains("请优先提炼结果"));
        assert!(prompt.contains("找到 3 条高互动笔记"));
    }

    #[test]
    fn test_skill_prompt_injection_keeps_activate_skill_mainline() {
        let mut messages = vec![ChatMessage::system("You are BlockCell.")];
        let skill_cards = vec![SkillCard {
            name: "local_demo".to_string(),
            description: "Local demo skill".to_string(),
            execution_layout: "PromptTool + LocalScript".to_string(),
            when_to_use: "Run local demo scripts".to_string(),
            outputs: "Local exec output".to_string(),
            allowed_tools: vec!["exec_local".to_string()],
            local_exec_entrypoints: vec!["scripts/hello.sh".to_string()],
            supports_local_exec: true,
        }];

        inject_skill_cards_into_system_prompt(&mut messages, &skill_cards, Some("local_demo"));

        let prompt = messages[0].content.as_str().unwrap_or_default();
        assert!(prompt.contains("## Installed Skills"));
        assert!(prompt.contains(
            "Use `activate_skill` when one installed skill is a better fit than general tools."
        ));
        assert!(prompt.contains("inspect it with `skill_view`"));
        assert!(prompt.contains("patch it with `skill_manage(action=\"patch\")`"));
        assert!(prompt.contains("If a skill card shows local execution entries, you may use `exec_local` only for those relative paths and only inside the active skill scope."));
        assert!(prompt.contains("Recent active skill: `local_demo`"));
        assert!(prompt.contains("布局: PromptTool + LocalScript"));
        assert!(prompt.contains("本地入口: scripts/hello.sh"));
    }

    #[test]
    fn test_markdown_skill_executor_limits_tools_to_skill_scope() {
        let available: HashSet<String> = ["web_search", "read_file", "spawn", "memory_query"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let tool_names =
            crate::prompt_skill_executor::PromptSkillExecutor::resolve_allowed_tool_names(
                &[
                    "web_search".to_string(),
                    "spawn".to_string(),
                    "unknown_tool".to_string(),
                ],
                &available,
            );

        assert_eq!(tool_names, vec!["web_search".to_string()]);
    }

    #[test]
    fn test_markdown_skill_executor_does_not_fallback_to_global_tools() {
        let available: HashSet<String> = ["web_search", "read_file", "memory_query"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let tool_names =
            crate::prompt_skill_executor::PromptSkillExecutor::resolve_allowed_tool_names(
                &[],
                &available,
            );

        assert!(tool_names.is_empty());
    }

    #[tokio::test]
    async fn test_prompt_skill_executes_through_unified_skill_executor() {
        let mut runtime = test_runtime();
        let skill_dir = runtime.paths.skills_dir().join("prompt_demo");
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: prompt_demo
description: prompt demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Prompt Demo

## Shared {#shared}
你是一个简洁的整理助手。

## Prompt {#prompt}
直接整理用户输入，不需要调用工具。
"#,
        )
        .expect("write skill md");
        runtime.context_builder.reload_skills();

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-1".to_string(),
            content: "请帮我整理这句话".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "forced_skill_name": "prompt_demo",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        assert!(result.contains("请帮我整理这句话"));

        let session_key = blockcell_core::build_session_key("cli", "chat-1");
        let history = runtime
            .session_store
            .load(&session_key)
            .expect("load session history");
        assert!(history.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls.iter().any(|call| {
                        call.name == "skill_enter"
                            && call.arguments["skill_name"].as_str() == Some("prompt_demo")
                    })
                })
                .unwrap_or(false)
        }));
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_prompt_skill_can_use_exec_skill_script_inside_skill_scope() {
        let mut runtime = test_runtime();
        runtime.tool_registry.register(Arc::new(
            blockcell_tools::exec_skill_script::ExecSkillScriptTool,
        ));
        let skill_dir = runtime.paths.skills_dir().join("local_demo");
        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: local_demo
description: local demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Local Demo

## Shared {#shared}
适合执行 skill 目录内的本地脚本。

## Prompt {#prompt}
如果要运行本地脚本，使用 exec_skill_script 调用 `scripts/hello.sh`。
"#,
        )
        .expect("write skill md");
        std::fs::write(
            scripts_dir.join("hello.sh"),
            "#!/bin/sh\necho local-skill-$1\n",
        )
        .expect("write script");
        runtime.context_builder.reload_skills();

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-local".to_string(),
            content: "运行本地脚本".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "forced_skill_name": "local_demo",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        let session_key = blockcell_core::build_session_key("cli", "chat-local");
        let history = runtime
            .session_store
            .load(&session_key)
            .expect("load session history");
        assert!(
            result.starts_with("summary:"),
            "unexpected skill result: {}",
            result
        );
        assert!(
            result.contains("local-skill-skill"),
            "unexpected skill result: {}; history: {:?}",
            result,
            history
        );
        assert!(history.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().any(|call| call.name == "exec_skill_script"))
                .unwrap_or(false)
        }));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_resolved_skill_tool_names_include_exec_skill_script_for_script_capable_skill() {
        let mut runtime = test_runtime();
        runtime.tool_registry.register(Arc::new(
            blockcell_tools::exec_skill_script::ExecSkillScriptTool,
        ));
        runtime
            .tool_registry
            .register(Arc::new(blockcell_tools::exec_local::ExecLocalTool));
        let skill_dir = runtime.paths.skills_dir().join("script_demo");
        std::fs::create_dir_all(skill_dir.join("scripts")).expect("create scripts dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: script_demo
description: script demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Script Demo

## Shared {#shared}
适合执行 skill 目录内的脚本资产。

## Prompt {#prompt}
如果需要运行本地脚本，使用 exec_skill_script 调用 `scripts/hello.sh`。
"#,
        )
        .expect("write skill md");
        std::fs::write(skill_dir.join("scripts/hello.sh"), "#!/bin/sh\necho ok\n")
            .expect("write script");
        runtime.context_builder.reload_skills();

        let active_skill = crate::context::ActiveSkillContext {
            name: "script_demo".to_string(),
            prompt_md: String::new(),
            inject_prompt_md: true,
            tools: vec![],
            fallback_message: None,
            source: blockcell_skills::manager::SkillSource::BlockCell,
        };

        let tool_names = runtime.resolved_skill_tool_names(&active_skill);
        assert!(tool_names.contains(&"exec_skill_script".to_string()));
        assert!(tool_names.contains(&"exec_local".to_string()));
    }

    #[tokio::test]
    async fn test_check_path_permission_allows_exec_skill_script_skill_paths() {
        let mut runtime = test_runtime();
        let msg = test_main_session_inbound("cli", "chat-script-path");

        assert!(
            runtime
                .check_path_permission(
                    "exec_skill_script",
                    &serde_json::json!({"path": "scripts/hello.sh"}),
                    &msg,
                )
                .await
        );
    }

    #[tokio::test]
    async fn test_skill_executor_uses_manual_not_file_type_to_choose_skill_script() {
        let mut runtime = test_runtime();
        runtime.tool_registry.register(Arc::new(
            blockcell_tools::exec_skill_script::ExecSkillScriptTool,
        ));
        let skill_dir = runtime.paths.skills_dir().join("legacy_script_demo");
        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: legacy_script_demo
description: legacy script demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Legacy Script Demo

## Shared {#shared}
适合执行 skill 目录内的本地脚本。

## Prompt {#prompt}
如果需要运行本地脚本，使用 exec_skill_script 调用 `scripts/hello.sh`。
"#,
        )
        .expect("write skill md");
        std::fs::write(
            skill_dir.join("SKILL.py"),
            "print('legacy path should not run')\n",
        )
        .expect("write legacy py");
        std::fs::write(
            scripts_dir.join("hello.sh"),
            "#!/bin/sh\necho local-skill-$1\n",
        )
        .expect("write script");
        runtime.context_builder.reload_skills();

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-legacy".to_string(),
            content: "运行这个技能".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "forced_skill_name": "legacy_script_demo",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        let session_key = blockcell_core::build_session_key("cli", "chat-legacy");
        let history = runtime
            .session_store
            .load(&session_key)
            .expect("load session history");
        assert!(
            result.contains("local-skill-skill"),
            "unexpected skill result: {}; history: {:?}",
            result,
            history
        );
        assert!(history.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().any(|call| call.name == "exec_skill_script"))
                .unwrap_or(false)
        }));
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_cli_style_skill_runs_via_exec_skill_script() {
        let mut runtime = test_runtime();
        runtime.tool_registry.register(Arc::new(
            blockcell_tools::exec_skill_script::ExecSkillScriptTool,
        ));
        let skill_dir = runtime.paths.skills_dir().join("cli_demo");
        let bin_dir = skill_dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: cli_demo
description: cli demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# CLI Demo

## Shared {#shared}
适合执行 skill 目录中的 CLI 脚本。

## Prompt {#prompt}
当用户要求执行 CLI 时，使用 exec_skill_script 调用 `bin/cli.sh`。
"#,
        )
        .expect("write skill md");
        std::fs::write(bin_dir.join("cli.sh"), "#!/bin/sh\necho local-cli-$1\n")
            .expect("write cli script");
        runtime.context_builder.reload_skills();

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-cli".to_string(),
            content: "执行 CLI".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "forced_skill_name": "cli_demo",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        assert!(
            result.contains("local-cli-demo"),
            "unexpected cli result: {}",
            result
        );
    }

    #[tokio::test]
    async fn test_prompt_skill_can_still_use_exec_local_inside_skill_scope_for_compat() {
        let mut runtime = test_runtime();
        runtime
            .tool_registry
            .register(Arc::new(blockcell_tools::exec_local::ExecLocalTool));
        let skill_dir = runtime.paths.skills_dir().join("compat_local_demo");
        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: compat_local_demo
description: compat local demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Compat Local Demo

## Shared {#shared}
适合执行 skill 目录内的本地脚本。

## Prompt {#prompt}
如果要运行本地脚本，使用 exec_local。
"#,
        )
        .expect("write skill md");
        std::fs::write(
            scripts_dir.join("hello.sh"),
            "#!/bin/sh\necho local-skill-$1\n",
        )
        .expect("write script");
        runtime.context_builder.reload_skills();

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-local-compat".to_string(),
            content: "运行兼容本地脚本".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "forced_skill_name": "compat_local_demo",
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        let session_key = blockcell_core::build_session_key("cli", "chat-local-compat");
        let history = runtime
            .session_store
            .load(&session_key)
            .expect("load session history");
        assert!(
            result.starts_with("summary:"),
            "unexpected skill result: {}",
            result
        );
        assert!(
            result.contains("local-skill-skill"),
            "unexpected skill result: {}; history: {:?}",
            result,
            history
        );
        assert!(history.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().any(|call| call.name == "exec_local"))
                .unwrap_or(false)
        }));
    }

    #[tokio::test]
    async fn test_unified_entry_calls_general_tool_without_extra_planning_roundtrip() {
        let provider = Arc::new(UnifiedEntryProvider {
            calls: AtomicUsize::new(0),
        });
        let mut runtime = test_runtime_with_provider(provider.clone());

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-general-tool".to_string(),
            content: "查看当前目录下文件".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        assert!(result.contains("目录内容"));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn test_unified_entry_can_activate_skill_without_forced_skill_metadata() {
        let provider = Arc::new(UnifiedEntryProvider {
            calls: AtomicUsize::new(0),
        });
        let mut runtime = test_runtime_with_provider(provider.clone());
        runtime.tool_registry.register(Arc::new(
            blockcell_tools::exec_skill_script::ExecSkillScriptTool,
        ));
        let skill_dir = runtime.paths.skills_dir().join("local_demo");
        let scripts_dir = skill_dir.join("scripts");
        std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        std::fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: local_demo
description: local demo
"#,
        )
        .expect("write meta");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Local Demo

## Shared {#shared}
适合执行 skill 目录内的本地脚本。

## Prompt {#prompt}
如果需要运行本地脚本，使用 exec_skill_script 调用 `scripts/hello.sh`。
"#,
        )
        .expect("write skill md");
        std::fs::write(
            scripts_dir.join("hello.sh"),
            "#!/bin/sh\necho local-skill-$1\n",
        )
        .expect("write script");
        runtime.context_builder.reload_skills();

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "chat-activate-skill".to_string(),
            content: "运行本地脚本".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        let session_key = blockcell_core::build_session_key("cli", "chat-activate-skill");
        let history = runtime
            .session_store
            .load(&session_key)
            .expect("load session history");

        assert!(
            result.starts_with("summary:"),
            "unexpected result: {}",
            result
        );
        assert!(history.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .any(|call| call.name == ACTIVATE_SKILL_TOOL_NAME)
                })
                .unwrap_or(false)
        }));
        assert!(history.iter().any(|message| {
            message
                .tool_calls
                .as_ref()
                .map(|calls| calls.iter().any(|call| call.name == "skill_enter"))
                .unwrap_or(false)
        }));
    }

    #[test]
    fn test_determine_interaction_mode_prefers_skill() {
        let mode = determine_interaction_mode(true, &[IntentCategory::Chat]);
        assert_eq!(mode, InteractionMode::Skill);
    }

    #[test]
    fn test_determine_interaction_mode_uses_chat_for_single_chat_intent() {
        let mode = determine_interaction_mode(false, &[IntentCategory::Chat]);
        assert_eq!(mode, InteractionMode::Chat);
    }

    #[test]
    fn test_determine_interaction_mode_falls_back_to_general_without_skill() {
        let mode = determine_interaction_mode(false, &[IntentCategory::Unknown]);
        assert_eq!(mode, InteractionMode::General);
    }

    #[test]
    fn test_skill_summary_formatter_uses_brief_md_and_result() {
        let prompt = crate::skill_summary::SkillSummaryFormatter::build_prompt(
            "帮我搜一下 AI 新闻",
            "ai_news",
            Some("search"),
            "请优先提炼要点，不要重复脚本原文。",
            "找到 5 条相关新闻",
        );

        assert!(prompt.contains("帮我搜一下 AI 新闻"));
        assert!(prompt.contains("ai_news"));
        assert!(prompt.contains("search"));
        assert!(prompt.contains("请优先提炼要点"));
        assert!(prompt.contains("找到 5 条相关新闻"));
    }

    #[test]
    fn test_prompt_and_script_skills_share_summary_formatter() {
        let prompt_skill_prompt = crate::skill_summary::SkillSummaryFormatter::build_prompt(
            "帮我深度分析 BTC",
            "deep_analysis",
            None,
            "请按结构化方式输出。",
            "这是最终分析结果。",
        );
        let script_skill_prompt = crate::skill_summary::SkillSummaryFormatter::build_prompt(
            "北京天气",
            "weather",
            Some("forecast"),
            "优先给出天气摘要。",
            "今天晴，最高 18 度。",
        );

        assert!(prompt_skill_prompt.contains("技能说明摘要"));
        assert!(script_skill_prompt.contains("技能说明摘要"));
        assert!(prompt_skill_prompt.contains("执行结果"));
        assert!(script_skill_prompt.contains("执行结果"));
    }

    #[test]
    fn test_prompt_skill_persists_internal_skill_enter_and_real_tool_chain() {
        let mut history = Vec::new();
        let real_tool_call = ToolCallRequest {
            id: "call-web-search".to_string(),
            name: "web_search".to_string(),
            arguments: serde_json::json!({ "query": "BTC" }),
            thought_signature: None,
        };
        let mut real_tool_result =
            ChatMessage::tool_result("call-web-search", r#"{"items":[{"title":"BTC news"}]}"#);
        real_tool_result.name = Some("web_search".to_string());

        persist_prompt_skill_history(
            &mut history,
            "帮我深度分析 BTC",
            "deep_analysis",
            &["web_search".to_string()],
            &[
                ChatMessage {
                    id: None,
                    role: "assistant".to_string(),
                    content: serde_json::Value::String("搜索 BTC 新闻".to_string()),
                    reasoning_content: None,
                    tool_calls: Some(vec![real_tool_call]),
                    tool_call_id: None,
                    name: None,
                },
                real_tool_result,
            ],
            "整理后的最终回答",
        );

        assert_eq!(history.len(), 6);
        assert_eq!(history[0].role, "user");
        assert_eq!(
            history[1].tool_calls.as_ref().unwrap()[0].name,
            "skill_enter"
        );
        assert_eq!(history[2].role, "tool");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(history[2].content.as_str().unwrap())
                .unwrap()["skill_name"],
            "deep_analysis"
        );
        assert_eq!(
            history[3].tool_calls.as_ref().unwrap()[0].name,
            "web_search"
        );
        assert_eq!(history[4].role, "tool");
        assert_eq!(history[4].name.as_deref(), Some("web_search"));
        assert_eq!(history[5].content.as_str(), Some("整理后的最终回答"));
    }

    #[test]
    fn test_script_skill_persists_internal_skill_invoke_and_raw_result() {
        let mut history = Vec::new();

        persist_script_skill_history(
            &mut history,
            "北京天气",
            "weather",
            "skill_invoke_python",
            &[
                "forecast".to_string(),
                "--city".to_string(),
                "beijing".to_string(),
            ],
            r#"{"temp":18,"condition":"sunny"}"#,
            "今天晴，最高 18 度。",
        );

        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, "user");
        assert_eq!(
            history[1].tool_calls.as_ref().unwrap()[0].name,
            "skill_invoke_python"
        );
        assert_eq!(
            history[1].tool_calls.as_ref().unwrap()[0].arguments["argv"],
            serde_json::json!(["forecast", "--city", "beijing"])
        );
        assert_eq!(history[2].role, "tool");
        assert_eq!(
            history[2].content.as_str(),
            Some(r#"{"temp":18,"condition":"sunny"}"#)
        );
        assert_eq!(history[3].content.as_str(), Some("今天晴，最高 18 度。"));
    }

    #[test]
    fn test_find_recent_skill_name_from_history_reads_internal_skill_trace() {
        let mut history = Vec::new();
        persist_prompt_skill_history(
            &mut history,
            "帮我深度分析 BTC",
            "deep_analysis",
            &["web_search".to_string()],
            &[],
            "整理后的最终回答",
        );

        assert_eq!(
            find_recent_skill_name_from_history(&history).as_deref(),
            Some("deep_analysis")
        );
    }

    #[test]
    fn test_active_skill_name_metadata_roundtrip() {
        let mut metadata = serde_json::Value::Null;
        record_active_skill_name(&mut metadata, "ppt-generator");

        assert_eq!(
            active_skill_name_from_metadata(&metadata).as_deref(),
            Some("ppt-generator")
        );
        assert_eq!(
            metadata
                .get(SESSION_ACTIVE_SKILL_CORRECTIONS_KEY)
                .and_then(|value| value.as_u64()),
            Some(0)
        );
    }

    #[test]
    fn repeated_learned_skill_corrections_disable_skill_toggle() {
        let paths = Paths::with_base(
            std::env::temp_dir().join(format!("blockcell-disable-skill-{}", uuid::Uuid::new_v4())),
        );
        let provider_pool = blockcell_providers::ProviderPool::from_single_provider(
            "test/mock",
            "test",
            Arc::new(TestProvider),
        );
        let runtime = AgentRuntime::new(
            Config::default(),
            paths.clone(),
            provider_pool,
            blockcell_tools::ToolRegistry::new(),
        )
        .expect("create runtime");
        let mut metadata = serde_json::Value::Null;
        record_active_skill_name(&mut metadata, "release_checklist");
        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "disable-skill".to_string(),
            content: "不要这样做，以后先检查 rollback plan".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        runtime
            .apply_learned_skill_negative_feedback(&mut metadata, &msg)
            .unwrap();
        assert!(load_disabled_toggles(&paths, "skills").is_empty());
        runtime
            .apply_learned_skill_negative_feedback(&mut metadata, &msg)
            .unwrap();

        assert!(load_disabled_toggles(&paths, "skills").contains("release_checklist"));
        assert!(active_skill_name_from_metadata(&metadata).is_none());
    }

    #[test]
    fn test_continued_skill_name_prefers_metadata_and_falls_back_to_history() {
        let mut history = Vec::new();
        persist_prompt_skill_history(
            &mut history,
            "帮我深度分析 BTC",
            "deep_analysis",
            &["web_search".to_string()],
            &[],
            "整理后的最终回答",
        );

        assert_eq!(
            continued_skill_name(
                &serde_json::json!({"active_skill_name":"ppt-generator"}),
                &history
            )
            .as_deref(),
            Some("ppt-generator")
        );
        assert_eq!(
            continued_skill_name(&serde_json::Value::Null, &history).as_deref(),
            Some("deep_analysis")
        );
    }

    #[test]
    fn test_continued_skill_suppresses_prompt_reinjection_for_same_skill() {
        let active_skill = crate::context::ActiveSkillContext {
            name: "ppt-generator".to_string(),
            prompt_md: "manual".to_string(),
            inject_prompt_md: true,
            tools: vec!["write_file".to_string()],
            fallback_message: None,
            source: blockcell_skills::manager::SkillSource::BlockCell,
        };

        let continued = suppress_prompt_reinjection_for_continued_skill(
            active_skill.clone(),
            Some("ppt-generator"),
        );
        assert!(!continued.inject_prompt_md);

        let other = suppress_prompt_reinjection_for_continued_skill(active_skill, Some("weather"));
        assert!(other.inject_prompt_md);
    }

    #[test]
    fn test_tool_round_throttle_delay_uses_base_delay_without_rate_limit() {
        assert_eq!(
            tool_round_throttle_delay(false),
            std::time::Duration::from_millis(600)
        );
    }

    #[test]
    fn test_tool_round_throttle_delay_uses_longer_delay_after_rate_limit() {
        assert_eq!(
            tool_round_throttle_delay(true),
            std::time::Duration::from_millis(2500)
        );
    }

    #[test]
    fn test_extract_json_from_text_handles_markdown_fence() {
        let text = "```json\n{\"argv\":[\"search\",\"btc\"]}\n```";
        assert_eq!(
            extract_json_from_text(text),
            "{\"argv\":[\"search\",\"btc\"]}"
        );
    }

    #[tokio::test]
    async fn init_memory_system_uses_runtime_memory_config() {
        let mut config = Config::default();
        config.memory.memory_system.token_budget = 1_000;
        config.memory.memory_system.layer1.preview_size_bytes = 123;
        config.memory.memory_system.layer2.gap_threshold_minutes = 7;
        config
            .memory
            .memory_system
            .layer3
            .minimum_message_tokens_to_init = 111;
        config.memory.memory_system.layer3.max_section_length = 222;
        config.memory.memory_system.layer4.compact_threshold_ratio = 0.5;
        config.memory.memory_system.layer4.keep_recent_messages = 3;
        config
            .memory
            .memory_system
            .layer5
            .min_messages_for_extraction = 2;

        let mut runtime = test_runtime_with_provider_and_paths(
            Paths::with_base(std::env::temp_dir().join(format!(
                "blockcell-memory-config-runtime-{}",
                uuid::Uuid::new_v4()
            ))),
            Arc::new(TestProvider),
            config,
        );

        runtime
            .init_memory_system("cli:configured-session".to_string())
            .await
            .unwrap();

        let memory_system = runtime.memory_system().expect("memory system initialized");
        assert_eq!(memory_system.session_id(), "cli:configured-session");
        assert_eq!(memory_system.config().token_budget, 1_000);
        assert_eq!(memory_system.config().layer1.preview_size_bytes, 123);
        assert_eq!(memory_system.config().layer2.gap_threshold_minutes, 7);
        assert_eq!(
            memory_system
                .session_memory_state()
                .config
                .minimum_message_tokens_to_init,
            111
        );
        assert_eq!(
            memory_system
                .session_memory_state()
                .config
                .max_section_length,
            222
        );
        assert!(memory_system.should_compact(500));
        assert!(!memory_system.should_compact(499));
        assert_eq!(memory_system.config().layer4.keep_recent_messages, 3);
        assert_eq!(memory_system.config().layer5.min_messages_for_extraction, 2);
    }

    #[tokio::test]
    async fn process_message_reinitializes_memory_system_for_new_session() {
        let mut runtime = test_runtime_with_provider(Arc::new(TestProvider));

        let first = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "memory-session-a".to_string(),
            content: "hello a".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        let first_session = first.session_key();
        runtime.process_message(first).await.unwrap();
        assert_eq!(
            runtime.memory_system().map(|system| system.session_id()),
            Some(first_session.as_str())
        );

        let second = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "memory-session-b".to_string(),
            content: "hello b".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        let second_session = second.session_key();
        runtime.process_message(second).await.unwrap();
        assert_eq!(
            runtime.memory_system().map(|system| system.session_id()),
            Some(second_session.as_str())
        );
    }

    #[tokio::test]
    async fn test_stream_retry_emits_reset_before_retrying_ws_response() {
        let mut runtime = test_runtime_with_provider(Arc::new(StreamingRetryProvider {
            attempts: AtomicUsize::new(0),
        }));
        let (event_tx, mut event_rx) = broadcast::channel(32);
        runtime.set_event_tx(event_tx);

        let msg = InboundMessage {
            channel: "ws".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "stream-retry".to_string(),
            content: "hello retry".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        assert_eq!(result, "final answer");

        let events = drain_ws_events(&mut event_rx);
        let event_types = collect_event_types(&events);
        assert!(
            contains_event_subsequence(
                &event_types,
                &["token", "stream_reset", "token", "message_done"]
            ),
            "unexpected event order: {:?}",
            event_types
        );
        let final_event = events
            .iter()
            .rev()
            .find(|event| event["type"] == "message_done")
            .expect("message_done event missing");
        assert_eq!(final_event["content"], "final answer");
    }

    #[tokio::test]
    async fn test_stream_close_without_done_returns_accumulated_response() {
        let mut runtime = test_runtime_with_provider(Arc::new(StreamingCloseProvider));

        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "stream-close".to_string(),
            content: "hello close".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime.process_message(msg).await.expect("process message");
        assert_eq!(result, "closed answer");
    }

    fn test_runtime() -> AgentRuntime {
        let mut config = Config::default();
        config.agents.defaults.model = "test/mock".to_string();
        config.agents.defaults.provider = Some("test".to_string());

        let base = std::env::temp_dir().join(format!(
            "blockcell-system-event-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).expect("create temp runtime dir");
        let paths = Paths::with_base(base);
        test_runtime_with_provider_and_paths(paths, Arc::new(TestProvider), config)
    }

    fn test_runtime_with_provider(provider: Arc<dyn Provider>) -> AgentRuntime {
        let mut config = Config::default();
        config.agents.defaults.model = "test/mock".to_string();
        config.agents.defaults.provider = Some("test".to_string());

        let base = std::env::temp_dir().join(format!(
            "blockcell-system-event-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).expect("create temp runtime dir");
        let paths = Paths::with_base(base);
        test_runtime_with_provider_and_paths(paths, provider, config)
    }

    fn test_runtime_with_embedded_ghost_learning() -> AgentRuntime {
        let mut config = Config::default();
        config.agents.defaults.model = "test/mock".to_string();
        config.agents.defaults.provider = Some("test".to_string());
        config.agents.ghost.learning.enabled = true;
        config.agents.ghost.learning.shadow_mode = true;

        let base = std::env::temp_dir().join(format!(
            "blockcell-ghost-learning-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).expect("create temp ghost runtime dir");
        let paths = Paths::with_base(base);
        test_runtime_with_provider_and_paths(paths, Arc::new(TestProvider), config)
    }

    fn test_runtime_with_boundary_flush_provider(
        provider: Arc<BoundaryFlushProvider>,
    ) -> AgentRuntime {
        let mut config = Config::default();
        config.agents.defaults.model = "test/mock".to_string();
        config.agents.defaults.provider = Some("test".to_string());
        config.agents.ghost.learning.enabled = true;
        config.agents.ghost.learning.shadow_mode = true;

        let base = std::env::temp_dir().join(format!(
            "blockcell-boundary-flush-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).expect("create temp boundary flush runtime dir");
        let paths = Paths::with_base(base);
        test_runtime_with_provider_and_paths(paths, provider, config)
    }

    fn test_runtime_with_ghost_review_provider(
        provider: Arc<dyn Provider>,
        shadow_mode: bool,
    ) -> (AgentRuntime, Paths) {
        let mut config = Config::default();
        config.agents.defaults.model = "test/mock".to_string();
        config.agents.defaults.provider = Some("test".to_string());
        config.agents.ghost.learning.enabled = true;
        config.agents.ghost.learning.shadow_mode = shadow_mode;

        let base = std::env::temp_dir().join(format!(
            "blockcell-ghost-review-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).expect("create temp ghost review runtime dir");
        let paths = Paths::with_base(base);

        (
            test_runtime_with_provider_and_paths(paths.clone(), provider, config),
            paths,
        )
    }

    fn test_runtime_with_file_memory_recall(provider: Arc<dyn Provider>) -> (AgentRuntime, Paths) {
        let mut config = Config::default();
        config.agents.defaults.model = "test/mock".to_string();
        config.agents.defaults.provider = Some("test".to_string());
        config.agents.ghost.learning.enabled = true;
        config.agents.ghost.learning.shadow_mode = false;
        config.agents.ghost.learning.recall_max_items = 4;
        config.agents.ghost.learning.recall_token_budget = 240;

        let base = std::env::temp_dir().join(format!(
            "blockcell-file-memory-recall-runtime-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&base).expect("create temp file memory recall runtime dir");
        let paths = Paths::with_base(base);
        paths.ensure_dirs().expect("ensure dirs");
        std::fs::write(
            paths.memory_md(),
            "Project fact: write deploy docs as concise step-by-step instructions with a rollback checklist.",
        )
        .expect("write memory md");

        (
            test_runtime_with_provider_and_paths(paths.clone(), provider, config),
            paths,
        )
    }

    fn test_runtime_with_provider_and_paths(
        paths: Paths,
        provider: Arc<dyn Provider>,
        config: Config,
    ) -> AgentRuntime {
        let provider_pool =
            blockcell_providers::ProviderPool::from_single_provider("test/mock", "test", provider);

        let mut runtime = AgentRuntime::new(
            config,
            paths,
            provider_pool,
            blockcell_tools::ToolRegistry::new(),
        )
        .expect("create runtime");
        runtime.set_agent_id(Some("default".to_string()));
        runtime
    }

    fn test_main_session_inbound(channel: &str, chat_id: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: chat_id.to_string(),
            content: "hello".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        }
    }

    async fn wait_for_runtime_review_runs(paths: &Paths, expected: usize) {
        for _ in 0..50 {
            let ledger = GhostLedger::open(&paths.ghost_ledger_db()).expect("open ghost ledger");
            if ledger.review_run_count().expect("count review runs") >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for ghost review runs");
    }

    #[tokio::test]
    async fn non_trivial_turn_creates_learning_episode() {
        let mut runtime = test_runtime_with_embedded_ghost_learning();
        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "ghost-turn".to_string(),
            content: "figure out the correct deploy sequence".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        runtime.process_message(msg).await.unwrap();

        assert_eq!(runtime.test_ghost_ledger().episode_count().unwrap(), 1);
        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_boundary_kind()
                .unwrap()
                .as_deref(),
            Some("turn_end")
        );
        let episode = runtime
            .test_ghost_ledger()
            .latest_episode_by_boundary_kind("turn_end")
            .unwrap()
            .unwrap();
        assert_eq!(
            episode.subject_key.as_deref(),
            Some("chat:ghost-turn:sender:user")
        );
    }

    #[tokio::test]
    async fn runtime_exposes_and_dispatches_ghost_memory_provider_tools() {
        let llm_provider = Arc::new(ProviderToolCaptureProvider {
            seen_tools: Mutex::new(Vec::new()),
        });
        let provider_tool = Arc::new(RuntimeProviderTool {
            calls: Mutex::new(Vec::new()),
        });
        let mut runtime = test_runtime_with_provider(llm_provider.clone());
        runtime.ghost_memory_lifecycle = Some(Arc::new(
            crate::ghost_memory_provider::GhostMemoryProviderManager::new()
                .with_provider(provider_tool.clone()),
        ));

        let response = runtime
            .process_message(InboundMessage {
                channel: "cli".to_string(),
                account_id: None,
                sender_id: "user".to_string(),
                chat_id: "provider-tool-chat".to_string(),
                content: "look up my release preference".to_string(),
                media: vec![],
                metadata: serde_json::Value::Null,
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
            })
            .await
            .expect("process provider tool message");

        assert!(response.contains("runtime_provider_tool"));
        let seen_tools = llm_provider.seen_tools.lock().unwrap().clone();
        assert!(seen_tools.iter().any(|tools| {
            tools.iter().any(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(|value| value.as_str())
                    == Some("external_memory_lookup")
            })
        }));

        let calls = provider_tool.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["query"], serde_json::json!("canary rollout"));
    }

    #[tokio::test]
    async fn pre_compress_boundary_creates_force_review_episode() {
        let mut runtime = test_runtime_with_embedded_ghost_learning();

        runtime.test_trigger_pre_compress().await.unwrap();

        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_episode_status()
                .unwrap()
                .as_deref(),
            Some("pending_review")
        );
    }

    #[tokio::test]
    async fn pre_compress_boundary_flushes_user_preference_to_file_memory() {
        let provider = Arc::new(BoundaryFlushProvider {
            calls: Mutex::new(Vec::new()),
            flush_calls: AtomicUsize::new(0),
        });
        let mut runtime = test_runtime_with_boundary_flush_provider(provider.clone());

        runtime.test_trigger_pre_compress().await.unwrap();

        let user_memory = std::fs::read_to_string(runtime.paths.user_md()).expect("read USER.md");
        assert!(user_memory.contains("rollback order before deploy compression"));
        let calls = provider.calls.lock().unwrap().clone();
        let flush_call = calls
            .iter()
            .find(|messages| {
                messages
                    .last()
                    .map(chat_message_text)
                    .unwrap_or_default()
                    .contains("__ghost_memory_flush_sentinel")
            })
            .expect("boundary flush model call");
        assert!(flush_call
            .iter()
            .any(|message| chat_message_text(message).contains("allowedTools")));
        assert!(flush_call
            .iter()
            .any(|message| chat_message_text(message)
                .contains("figure out the correct deploy sequence")));
        assert!(flush_call.iter().all(|message| {
            message.role != "tool"
                || !chat_message_text(message).contains("__ghost_memory_flush_sentinel")
        }));
    }

    #[test]
    fn runtime_session_search_finds_persisted_history() {
        let base = std::env::temp_dir().join(format!(
            "blockcell-session-search-test-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = Paths::with_base(base);
        paths.ensure_dirs().expect("create runtime dirs");
        let store = SessionStore::new(paths.clone());
        store
            .save(
                "cli:chat-1",
                &[
                    ChatMessage::user("How should we deploy this service?"),
                    ChatMessage::assistant("Use canary-first deploys and verify rollback order."),
                ],
            )
            .expect("save session");

        let search = RuntimeSessionSearch::new(paths, Some("cli:chat-1".to_string()));
        let result = search
            .search_session_json("canary rollback", 5)
            .expect("search session history");
        assert_eq!(
            result.get("count").and_then(|value| value.as_u64()),
            Some(1)
        );
        assert!(result.to_string().contains("canary-first deploys"));
        assert!(result.to_string().contains("cli:chat-1"));
    }

    #[tokio::test]
    async fn pre_compress_boundary_includes_provider_context_in_episode_snapshot() {
        let mut runtime = test_runtime_with_embedded_ghost_learning();
        runtime.ghost_memory_lifecycle = Some(Arc::new(
            crate::ghost_memory_provider::GhostMemoryProviderManager::new()
                .with_provider(Arc::new(BoundaryMemoryProvider)),
        ));

        runtime.test_trigger_pre_compress().await.unwrap();

        let mut claimed = runtime
            .test_ghost_ledger()
            .claim_reviewable_episodes(1)
            .expect("claim pre-compress episode");
        let episode = claimed.pop().expect("pre-compress episode");
        assert_eq!(episode.boundary_kind, "pre_compress");
        assert!(episode
            .metadata
            .get("reusableLesson")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .contains("preserve provider-derived rollback preference"));
    }

    #[tokio::test]
    async fn session_end_boundary_includes_provider_context_in_episode_snapshot() {
        let mut runtime = test_runtime_with_embedded_ghost_learning();
        runtime.ghost_memory_lifecycle = Some(Arc::new(
            crate::ghost_memory_provider::GhostMemoryProviderManager::new()
                .with_provider(Arc::new(BoundaryMemoryProvider)),
        ));
        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "ghost-session-end-provider".to_string(),
            content: "figure out the correct deploy order".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        runtime.process_message(msg).await.unwrap();

        runtime.test_trigger_session_end().await.unwrap();

        let mut claimed = runtime
            .test_ghost_ledger()
            .claim_reviewable_episodes(4)
            .expect("claim session-end episodes");
        let episode = claimed
            .drain(..)
            .find(|episode| episode.boundary_kind == "session_end")
            .expect("session-end episode");
        assert!(episode
            .metadata
            .get("reusableLesson")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .contains("preserve provider-derived session-end deploy preference"));
    }

    #[tokio::test]
    async fn session_end_boundary_creates_force_review_episode() {
        let mut runtime = test_runtime_with_embedded_ghost_learning();
        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "ghost-session-end".to_string(),
            content: "figure out the correct deploy order".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };
        runtime.process_message(msg).await.unwrap();

        runtime.test_trigger_session_end().await.unwrap();

        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_episode_status()
                .unwrap()
                .as_deref(),
            Some("pending_review")
        );
        assert_eq!(runtime.test_ghost_ledger().episode_count().unwrap(), 2);
    }

    #[tokio::test]
    async fn session_rotate_boundary_creates_force_review_episode() {
        let provider = Arc::new(SequencedGhostProvider);
        let (mut runtime, paths) = test_runtime_with_ghost_review_provider(provider, true);

        runtime
            .process_message(InboundMessage {
                channel: "cli".to_string(),
                account_id: None,
                sender_id: "user".to_string(),
                chat_id: "ghost-rotate-a".to_string(),
                content: "figure out the correct deploy order".to_string(),
                media: vec![],
                metadata: serde_json::Value::Null,
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
            })
            .await
            .unwrap();

        runtime
            .process_message(InboundMessage {
                channel: "cli".to_string(),
                account_id: None,
                sender_id: "user".to_string(),
                chat_id: "ghost-rotate-b".to_string(),
                content: "analyze the safer rollback sequence".to_string(),
                media: vec![],
                metadata: serde_json::Value::Null,
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
            })
            .await
            .unwrap();

        wait_for_runtime_review_runs(&paths, 3).await;

        assert_eq!(
            runtime
                .test_ghost_ledger()
                .episode_count_by_boundary_kind("session_rotate")
                .unwrap(),
            1
        );
        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_episode_status_by_boundary_kind("session_rotate")
                .unwrap()
                .as_deref(),
            Some("reviewed")
        );
    }

    #[tokio::test]
    async fn session_rotate_boundary_includes_provider_context_in_episode_snapshot() {
        let provider = Arc::new(SequencedGhostProvider);
        let (mut runtime, _paths) = test_runtime_with_ghost_review_provider(provider, true);
        runtime.ghost_memory_lifecycle = Some(Arc::new(
            crate::ghost_memory_provider::GhostMemoryProviderManager::new()
                .with_provider(Arc::new(BoundaryMemoryProvider)),
        ));

        for chat_id in ["ghost-rotate-provider-a", "ghost-rotate-provider-b"] {
            runtime
                .process_message(InboundMessage {
                    channel: "cli".to_string(),
                    account_id: None,
                    sender_id: "user".to_string(),
                    chat_id: chat_id.to_string(),
                    content: "figure out the correct deploy order".to_string(),
                    media: vec![],
                    metadata: serde_json::Value::Null,
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                })
                .await
                .unwrap();
        }

        let episode = runtime
            .test_ghost_ledger()
            .latest_episode_by_boundary_kind("session_rotate")
            .expect("load session-rotate episode")
            .expect("session-rotate episode");
        let lesson = episode
            .metadata
            .get("reusableLesson")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        assert!(lesson.contains("Switched active session"));
        assert!(lesson.contains("preserve provider-derived session-end deploy preference"));
    }

    #[tokio::test]
    async fn delegation_completion_creates_parent_learning_episode() {
        let runtime = test_runtime_with_embedded_ghost_learning();

        runtime
            .test_complete_delegated_task(
                "research the release failure",
                "identified the root cause and the safer rollback order",
            )
            .await
            .unwrap();

        assert_eq!(runtime.test_ghost_ledger().episode_count().unwrap(), 1);
        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_boundary_kind()
                .unwrap()
                .as_deref(),
            Some("delegation_end")
        );
    }

    #[tokio::test]
    async fn file_memory_recall_is_fenced_and_not_persisted() {
        let provider = Arc::new(RecallCaptureProvider {
            calls: Mutex::new(Vec::new()),
        });
        let (mut runtime, paths) = test_runtime_with_file_memory_recall(provider.clone());
        let msg = InboundMessage {
            channel: "cli".to_string(),
            account_id: None,
            sender_id: "user".to_string(),
            chat_id: "ghost-recall".to_string(),
            content: "how do I usually like deploy docs written?".to_string(),
            media: vec![],
            metadata: serde_json::Value::Null,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        runtime.process_message(msg.clone()).await.unwrap();

        let calls = provider.calls.lock().unwrap().clone();
        let first_call = calls.first().expect("first llm call");
        assert!(
            first_call.iter().any(|message| {
                message.role == "user"
                    && chat_message_text(message).contains("<memory-context>")
                    && chat_message_text(message).contains("rollback checklist")
            }),
            "expected fenced file memory recall in provider payload"
        );

        let session = SessionStore::new(paths).load(&msg.session_key()).unwrap();
        assert!(session
            .iter()
            .all(|message| { !chat_message_text(message).contains("<memory-context>") }));
    }

    #[tokio::test]
    async fn ghost_learning_closes_loop_from_experience_to_file_memory_only() {
        let provider = Arc::new(ReviewAndCaptureProvider {
            calls: Mutex::new(Vec::new()),
            review_calls: AtomicUsize::new(0),
        });
        let (mut runtime, paths) = test_runtime_with_ghost_review_provider(provider.clone(), false);

        runtime
            .process_message(InboundMessage {
                channel: "cli".to_string(),
                account_id: None,
                sender_id: "user".to_string(),
                chat_id: "ghost-closure".to_string(),
                content: "figure out the correct release verification sequence with rollback plan"
                    .to_string(),
                media: vec![],
                metadata: serde_json::Value::Null,
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
            })
            .await
            .unwrap();

        wait_for_runtime_review_runs(&paths, 1).await;

        let user_memory = std::fs::read_to_string(paths.user_md()).expect("read USER.md");
        let durable_memory = std::fs::read_to_string(paths.memory_md()).expect("read MEMORY.md");
        assert!(user_memory.contains("canary-first rollout"));
        assert!(durable_memory.contains("Confirm rollback plan before release verification"));
        assert!(!paths
            .skills_dir()
            .join("release_verification")
            .join("SKILL.md")
            .exists());

        let ledger = GhostLedger::open(&paths.ghost_ledger_db()).expect("open ghost ledger");
        assert_eq!(ledger.review_run_count().unwrap(), 1);

        assert_eq!(provider.review_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shadow_mode_captures_and_reviews_without_runtime_recall() {
        let provider = Arc::new(SequencedGhostProvider);
        let (mut runtime, paths) = test_runtime_with_ghost_review_provider(provider, true);
        crate::reset_ghost_metrics_for_paths(&paths);

        runtime
            .process_message(InboundMessage {
                channel: "cli".to_string(),
                account_id: None,
                sender_id: "user".to_string(),
                chat_id: "ghost-shadow-review".to_string(),
                content: "learn my preferred deploy style".to_string(),
                media: vec![],
                metadata: serde_json::Value::Null,
                timestamp_ms: chrono::Utc::now().timestamp_millis(),
            })
            .await
            .unwrap();

        wait_for_runtime_review_runs(&paths, 1).await;

        let metrics = runtime.test_ghost_metrics();
        assert_eq!(metrics.episodes_captured, 1);
        assert_eq!(metrics.reviews_started, 1);
        assert_eq!(metrics.reviews_failed, 0);
        assert_eq!(runtime.test_ghost_ledger().review_run_count().unwrap(), 1);
    }

    #[tokio::test]
    async fn turn_review_interval_captures_periodic_trivial_turn() {
        let provider = Arc::new(SequencedGhostProvider);
        let (mut runtime, _paths) = test_runtime_with_ghost_review_provider(provider, true);
        runtime.config.agents.ghost.learning.turn_review_interval = 2;

        for content in ["hello", "thanks"] {
            runtime
                .process_message(InboundMessage {
                    channel: "cli".to_string(),
                    account_id: None,
                    sender_id: "user".to_string(),
                    chat_id: "ghost-interval".to_string(),
                    content: content.to_string(),
                    media: vec![],
                    metadata: serde_json::Value::Null,
                    timestamp_ms: chrono::Utc::now().timestamp_millis(),
                })
                .await
                .unwrap();
        }

        assert_eq!(runtime.test_ghost_ledger().episode_count().unwrap(), 1);
        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_boundary_kind()
                .unwrap()
                .as_deref(),
            Some("turn_end")
        );
    }

    #[tokio::test]
    async fn system_tick_processes_pending_ghost_reviews() {
        let provider = Arc::new(SequencedGhostProvider);
        let (mut runtime, paths) = test_runtime_with_ghost_review_provider(provider, true);

        runtime.test_trigger_pre_compress().await.unwrap();
        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_episode_status()
                .unwrap()
                .as_deref(),
            Some("pending_review")
        );

        runtime
            .process_system_event_tick(chrono::Utc::now().timestamp_millis())
            .await;

        wait_for_runtime_review_runs(&paths, 1).await;
        assert_eq!(
            runtime
                .test_ghost_ledger()
                .latest_episode_status()
                .unwrap()
                .as_deref(),
            Some("reviewed")
        );
    }

    #[tokio::test]
    async fn test_orchestrator_tick_emits_event_tx_for_immediate_notifications() {
        let mut runtime = test_runtime();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        runtime.set_event_tx(event_tx);
        runtime
            .update_main_session_target(&test_main_session_inbound("cli", "chat-1"))
            .await;

        let mut event = SystemEvent::new_main_session(
            "task.failed",
            "task_manager",
            EventPriority::Critical,
            "Task failed",
            "Background report failed",
        );
        event.delivery.immediate = true;
        runtime.event_emitter_handle().emit(event);

        let decision = runtime
            .process_system_event_tick(chrono::Utc::now().timestamp_millis())
            .await;

        assert_eq!(decision.immediate_notifications.len(), 1);
        let payload = event_rx.recv().await.expect("receive ws event");
        let json: serde_json::Value = serde_json::from_str(&payload).expect("parse ws event");
        assert_eq!(json["type"], "system_event_notification");
        assert_eq!(json["chat_id"], "chat-1");
        assert_eq!(json["title"], "Task failed");
    }

    #[tokio::test]
    async fn test_orchestrator_tick_flushes_summary_to_main_session_outbound() {
        let mut runtime = test_runtime();
        let (outbound_tx, mut outbound_rx) = mpsc::channel(8);
        runtime.set_outbound(outbound_tx);
        runtime
            .update_main_session_target(&test_main_session_inbound("cli", "chat-1"))
            .await;

        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut event = SystemEvent::new_main_session(
            "task.completed",
            "task_manager",
            EventPriority::Normal,
            "Report ready",
            "Background report finished",
        );
        event.created_at_ms = now_ms - 60_000;
        runtime.event_emitter_handle().emit(event);

        let decision = runtime.process_system_event_tick(now_ms).await;

        assert_eq!(decision.flushed_summaries.len(), 1);
        let outbound = outbound_rx.recv().await.expect("receive outbound summary");
        assert_eq!(outbound.channel, "cli");
        assert_eq!(outbound.chat_id, "chat-1");
        assert!(outbound.content.contains("Report ready"));
        assert!(outbound.content.contains("System updates") || outbound.content.contains("🗂️"));
    }

    #[tokio::test]
    async fn test_cron_agent_delivery_emits_ws_event_for_deliver_target() {
        let mut runtime = test_runtime();
        let (event_tx, mut event_rx) = broadcast::channel(8);
        runtime.set_event_tx(event_tx);

        let msg = InboundMessage {
            channel: "cron".to_string(),
            account_id: None,
            sender_id: "cron".to_string(),
            chat_id: "job-123".to_string(),
            content: "任务完成摘要".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "deliver": true,
                "deliver_channel": "ws",
                "deliver_to": "webui-chat-1",
                "cron_agent": true,
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime
            .process_message(msg)
            .await
            .expect("process cron message");
        assert!(!result.is_empty());

        let json = loop {
            let payload = event_rx.recv().await.expect("receive ws event");
            let event: serde_json::Value = serde_json::from_str(&payload).expect("parse ws event");
            if event["type"] == "message_done" {
                break event;
            }
        };
        assert_eq!(json["type"], "message_done");
        assert_eq!(json["chat_id"], "webui-chat-1");
        assert_eq!(json["content"], result);
        assert_eq!(json["background_delivery"], true);
        assert_eq!(json["delivery_kind"], "cron");
        assert_eq!(json["cron_kind"], "agent");
    }

    #[tokio::test]
    async fn test_cron_agent_persists_to_deliver_session_not_cron_job_session() {
        let mut runtime = test_runtime();

        let msg = InboundMessage {
            channel: "cron".to_string(),
            account_id: None,
            sender_id: "cron".to_string(),
            chat_id: "job-456".to_string(),
            content: "搜索美伊战争最新消息，并将结果发给用户。".to_string(),
            media: vec![],
            metadata: serde_json::json!({
                "deliver": true,
                "deliver_channel": "ws",
                "deliver_to": "webui-chat-2",
                "cron_agent": true,
            }),
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        let result = runtime
            .process_message(msg)
            .await
            .expect("process cron message");
        assert!(!result.is_empty());

        let ws_session_key = blockcell_core::build_session_key("ws", "webui-chat-2");
        let cron_session_key = blockcell_core::build_session_key("cron", "job-456");

        let ws_history = runtime
            .session_store
            .load(&ws_session_key)
            .expect("load ws session history");
        assert!(!ws_history.is_empty());
        assert!(ws_history.iter().any(|m| match &m.content {
            serde_json::Value::String(s) => s.contains("搜索美伊战争最新消息"),
            _ => false,
        }));

        let cron_path = runtime.paths.session_file(&cron_session_key);
        assert!(
            !cron_path.exists(),
            "cron job session file should not be created"
        );
    }

    #[tokio::test]
    async fn test_orchestrator_tick_gracefully_handles_missing_dispatchers() {
        let runtime = test_runtime();

        let event = SystemEvent::new_main_session(
            "task.failed",
            "task_manager",
            EventPriority::Critical,
            "Task failed",
            "No dispatcher configured",
        );
        runtime.event_emitter_handle().emit(event);

        let decision = runtime
            .process_system_event_tick(chrono::Utc::now().timestamp_millis())
            .await;

        assert_eq!(decision.immediate_notifications.len(), 1);
    }

    #[test]
    fn test_resolve_profile_tool_names_uses_agent_profile_for_unknown_intent() {
        let raw = r#"{
  "agents": {
    "list": [
      { "id": "ops", "enabled": true, "intentProfile": "ops" }
    ]
  },
  "intentRouter": {
    "enabled": true,
    "defaultProfile": "default",
    "profiles": {
      "default": {
        "coreTools": ["read_file"],
        "intentTools": {
          "Chat": { "inheritBase": false, "tools": [] },
          "Unknown": ["browse"]
        }
      },
      "ops": {
        "coreTools": ["read_file", "exec", "file_ops"],
        "intentTools": {
          "Chat": { "inheritBase": false, "tools": [] },
          "Unknown": ["browse", "http_request"],
          "DevOps": ["git_api", "network_monitor"]
        }
      }
    }
  }
}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let available: HashSet<String> = [
            "read_file",
            "exec",
            "file_ops",
            "browse",
            "http_request",
            "git_api",
            "network_monitor",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let tool_names = resolve_profile_tool_names(
            &config,
            Some("ops"),
            &[IntentCategory::Unknown],
            &available,
        );

        assert!(tool_names.contains(&"read_file".to_string()));
        assert!(tool_names.contains(&"exec".to_string()));
        assert!(tool_names.contains(&"file_ops".to_string()));
        assert!(tool_names.contains(&"browse".to_string()));
        assert!(tool_names.contains(&"http_request".to_string()));
        assert!(!tool_names.contains(&"git_api".to_string()));
    }

    #[test]
    fn test_resolve_profile_tool_names_returns_empty_for_chat_when_profile_configures_none() {
        let config: Config = serde_json::from_str("{}").unwrap();
        let available: HashSet<String> = ["read_file", "browse"]
            .into_iter()
            .map(str::to_string)
            .collect();

        let tool_names =
            resolve_profile_tool_names(&config, None, &[IntentCategory::Chat], &available);

        assert!(tool_names.is_empty());
    }

    #[test]
    fn test_napcat_tools_hidden_when_disabled() {
        // Config with napcat disabled (default)
        let config: Config = serde_json::from_str(
            r#"{
                "channels": {
                    "napcat": {
                        "enabled": false
                    }
                }
            }"#,
        )
        .unwrap();

        let available: HashSet<String> = [
            "read_file",
            "napcat_get_group_list",
            "napcat_get_login_info",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let tool_names = resolve_effective_tool_names(
            &config,
            InteractionMode::General,
            None,
            None,
            &[IntentCategory::Communication],
            &available,
        );

        // napcat tools should be filtered out
        assert!(tool_names.contains(&"read_file".to_string()));
        assert!(!tool_names.contains(&"napcat_get_group_list".to_string()));
        assert!(!tool_names.contains(&"napcat_get_login_info".to_string()));
    }

    #[test]
    fn test_napcat_tools_visible_when_enabled() {
        // Config with napcat enabled
        let config: Config = serde_json::from_str(
            r#"{
                "channels": {
                    "napcat": {
                        "enabled": true
                    }
                }
            }"#,
        )
        .unwrap();

        let available: HashSet<String> = [
            "read_file",
            "napcat_get_group_list",
            "napcat_get_login_info",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();

        let tool_names = resolve_effective_tool_names(
            &config,
            InteractionMode::General,
            None,
            None,
            &[IntentCategory::Communication],
            &available,
        );

        // napcat tools should be visible
        assert!(tool_names.contains(&"read_file".to_string()));
        assert!(tool_names.contains(&"napcat_get_group_list".to_string()));
        assert!(tool_names.contains(&"napcat_get_login_info".to_string()));
    }

    #[test]
    fn test_prepare_skill_result_for_presentation_keeps_full_result_payload() {
        let output = serde_json::json!({
            "success": true,
            "action": "search",
            "display_text": "找到 1 条相关笔记。",
            "data": {
                "items": [
                    {
                        "index": 1,
                        "title": "上海咖啡推荐"
                    }
                ]
            },
            "raw_result_context": {
                "search_results": [
                    {
                        "index": 1,
                        "title": "上海咖啡推荐",
                        "feed_id": "feed-1",
                        "xsec_token": "token-1"
                    }
                ]
            }
        })
        .to_string();

        let presentation = prepare_skill_result_for_presentation("xiaohongshu", &output);

        assert_eq!(
            presentation.direct_text.as_deref(),
            Some("找到 1 条相关笔记。")
        );
        let llm_payload = presentation
            .llm_payload
            .as_ref()
            .expect("structured payload should still provide LLM summary input");
        assert!(llm_payload.contains("上海咖啡推荐"));
        assert!(llm_payload.contains("feed-1"));
        assert!(llm_payload.contains("xsec_token"));
    }

    #[test]
    fn test_is_sensitive_filename_matches_json5_config() {
        assert!(is_sensitive_filename("config.json5"));
        assert!(is_sensitive_filename("/tmp/.blockcell/config.json5"));
    }

    #[tokio::test]
    async fn test_deliver_subagent_result_to_ws_origin_emits_message_done_event() {
        let (event_tx, mut event_rx) = broadcast::channel::<String>(8);

        deliver_subagent_result_to_origin(
            "ws",
            "webui-chat-9",
            "第15条内容已经整理完成",
            "default",
            None,
            Some(event_tx),
        )
        .await;

        let payload = event_rx.recv().await.expect("receive ws event");
        let json: serde_json::Value = serde_json::from_str(&payload).expect("parse ws event");
        assert_eq!(json["type"], "message_done");
        assert_eq!(json["chat_id"], "webui-chat-9");
        assert_eq!(json["content"], "第15条内容已经整理完成");
        assert_eq!(json["background_delivery"], true);
    }

    // resolve_effective_tool_names 测试
    #[test]
    fn test_resolve_effective_tool_names_load_all_applies_deny_tools() {
        // 当 enabled=false 且 load_all_tools=true 时，应应用 deny_tools 过滤
        let raw = r#"{
            "intentRouter": {
                "enabled": false,
                "loadAllTools": true,
                "defaultProfile": "default",
                "profiles": {
                    "default": {
                        "coreTools": [],
                        "intentTools": {
                            "Chat": { "inheritBase": false, "tools": [] },
                            "Unknown": []
                        },
                        "denyTools": ["exec"]
                    }
                }
            },
            "channels": {
                "napcat": { "enabled": false }
            }
        }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let available: HashSet<String> = [
            "read_file",
            "write_file",
            "exec",
            "web_search",
            "napcat_send",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let tools = resolve_effective_tool_names(
            &config,
            InteractionMode::General,
            None,
            None,
            &[IntentCategory::Unknown],
            &available,
        );

        // exec 被 deny_tools 过滤，napcat_send 被 napcat.enabled=false 过滤
        assert_eq!(tools.len(), 3);
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"write_file".to_string()));
        assert!(tools.contains(&"web_search".to_string()));
        assert!(!tools.contains(&"exec".to_string()));
        assert!(!tools.contains(&"napcat_send".to_string()));
    }

    #[test]
    fn test_resolve_effective_tool_names_load_all_applies_napcat_filter() {
        // 当 napcat.enabled=true 时，napcat 工具应可用
        let raw = r#"{
            "intentRouter": {
                "enabled": false,
                "loadAllTools": true,
                "defaultProfile": "default",
                "profiles": {
                    "default": {
                        "coreTools": [],
                        "intentTools": {
                            "Chat": { "inheritBase": false, "tools": [] },
                            "Unknown": []
                        },
                        "denyTools": []
                    }
                }
            },
            "channels": {
                "napcat": { "enabled": true }
            }
        }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let available: HashSet<String> = ["read_file", "napcat_send", "napcat_receive"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let tools = resolve_effective_tool_names(
            &config,
            InteractionMode::General,
            None,
            None,
            &[IntentCategory::Unknown],
            &available,
        );

        // napcat 工具应可用（enabled=true）
        assert_eq!(tools.len(), 3);
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"napcat_send".to_string()));
        assert!(tools.contains(&"napcat_receive".to_string()));
    }

    #[test]
    fn test_resolve_effective_tool_names_load_all_extends_skill_tools() {
        // 当有 active_skill 时，应扩展 skill.tools
        let raw = r#"{
            "intentRouter": {
                "enabled": false,
                "loadAllTools": true,
                "defaultProfile": "default",
                "profiles": {
                    "default": {
                        "coreTools": [],
                        "intentTools": {
                            "Chat": { "inheritBase": false, "tools": [] },
                            "Unknown": []
                        },
                        "denyTools": []
                    }
                }
            },
            "channels": {
                "napcat": { "enabled": false }
            }
        }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let available: HashSet<String> = ["read_file", "write_file"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let skill = ActiveSkillContext {
            name: "test_skill".to_string(),
            prompt_md: String::new(),
            inject_prompt_md: false,
            tools: vec!["skill_tool_1".to_string(), "skill_tool_2".to_string()],
            fallback_message: None,
            source: blockcell_skills::manager::SkillSource::BlockCell,
        };
        let tools = resolve_effective_tool_names(
            &config,
            InteractionMode::Skill,
            None,
            Some(&skill),
            &[IntentCategory::Unknown],
            &available,
        );

        // 应包含 available tools + skill.tools
        assert_eq!(tools.len(), 4);
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"write_file".to_string()));
        assert!(tools.contains(&"skill_tool_1".to_string()));
        assert!(tools.contains(&"skill_tool_2".to_string()));
    }

    #[test]
    fn test_resolve_effective_tool_names_enabled_true_uses_intent_classification() {
        // 当 enabled=true 时，应走意图分类流程，忽略 load_all_tools
        let raw = r#"{
            "intentRouter": {
                "enabled": true,
                "loadAllTools": true,
                "defaultProfile": "default",
                "profiles": {
                    "default": {
                        "coreTools": ["read_file"],
                        "intentTools": {
                            "Chat": { "inheritBase": false, "tools": [] },
                            "FileOps": ["edit_file"]
                        },
                        "denyTools": []
                    }
                }
            },
            "channels": {
                "napcat": { "enabled": false }
            }
        }"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let available: HashSet<String> = ["read_file", "edit_file", "exec", "web_search"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        // FileOps 意图应返回 read_file (core) + edit_file (intent)
        let tools = resolve_effective_tool_names(
            &config,
            InteractionMode::General,
            None,
            None,
            &[IntentCategory::FileOps],
            &available,
        );
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"edit_file".to_string()));
    }
}
