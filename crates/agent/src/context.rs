use crate::auto_memory::MemoryInjector;
use blockcell_core::types::ChatMessage;
use blockcell_core::{Config, Paths};
use blockcell_skills::manager::SkillSource;
use blockcell_skills::{EvolutionService, EvolutionServiceConfig, LLMProvider, SkillManager};
use blockcell_tools::MemoryStoreHandle;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionMode {
    Skill,
    Chat,
    General,
}

#[derive(Debug, Clone)]
pub struct ActiveSkillContext {
    pub name: String,
    pub prompt_md: String,
    pub inject_prompt_md: bool,
    pub tools: Vec<String>,
    pub fallback_message: Option<String>,
    /// 技能来源，用于运行时区分（如自进化屏蔽）
    pub source: SkillSource,
}

/// Skill 系统引导 (参考 Hermes MEMORY_GUIDANCE)
///
/// 注入到系统提示词中, 引导 Agent 正确使用 Skill 系统。
const SKILL_GUIDANCE: &str = r#"
## Skill System Guidance

You have a skill system for reusable procedural knowledge.

### Creating skills
After completing complex tasks (5+ tool calls, errors overcome, user-corrected approach),
offer to save the workflow as a skill. Use `skill_manage` with action="create".

### Patching skills
When using a skill and discovering issues not covered by it, patch it immediately
with `skill_manage` action="patch" — don't wait to be asked.

### Skill maintenance
Skills that aren't maintained become liabilities. Periodically review skills you use
and patch them when you find stale instructions or missing steps.

### Memory vs Skill boundary
- Memory: declarative facts (preferences, environment, conventions)
- Skill: procedural knowledge (steps, workflows, pitfalls)
- "User prefers concise responses" → memory
- "Deploy to K8s requires pushing image first" → skill
"#;

/// Memory 使用指导 — 参考 Hermes MEMORY_GUIDANCE
///
/// 注入到系统提示词中, 引导 Agent 正确使用 Memory 系统。
const MEMORY_GUIDANCE: &str = r#"
## Memory Guidance

You have a memory system for storing durable facts about the user and environment.

### What to save
- User preferences and habits (communication style, language, formatting)
- Environment facts (OS, shell, project structure, conventions)
- Important decisions and their rationale
- Recurring patterns the user has confirmed

### What NOT to save
- Task progress or temporary state (it changes and becomes stale)
- Full conversation history (already available in context)
- Information the user can easily re-derive
- Speculative or unverified assumptions

### Memory vs Skill boundary
- Memory: declarative facts (preferences, environment, conventions)
- Skill: procedural knowledge (steps, workflows, pitfalls)
- "User prefers concise responses" → memory
- "Deploy to K8s requires pushing image first" → skill
"#;

pub struct ContextBuilder {
    paths: Paths,
    skill_manager: Option<SkillManager>,
    ghost_learning_enabled: bool,
    file_memory_snapshots: Mutex<HashMap<String, FrozenFileMemorySnapshot>>,
    memory_store: Option<MemoryStoreHandle>,
    /// Layer 5 记忆注入器 (7 层记忆系统)
    memory_injector: Option<MemoryInjector>,
    /// Cached capability brief for prompt injection (updated from tick).
    capability_brief: Option<String>,
    /// Skill 索引摘要 (可用 Skill 列表, 注入到系统提示词)
    /// 使用 Arc<RwLock> 允许后台 Review Agent 在创建/修改 Skill 后刷新
    skill_index_summary: Arc<RwLock<Option<String>>>,
}

#[derive(Debug, Clone, Default)]
struct FrozenFileMemorySnapshot {
    user: Option<String>,
    memory: Option<String>,
}

impl FrozenFileMemorySnapshot {
    fn load(paths: &Paths) -> Self {
        Self {
            user: std::fs::read_to_string(paths.user_md()).ok(),
            memory: std::fs::read_to_string(paths.memory_md()).ok(),
        }
    }
}

fn replace_prompt_section(mut prompt: String, header: &str, replacement: Option<&str>) -> String {
    let Some(start) = prompt.find(header) else {
        return prompt;
    };
    let body_start = start + header.len();
    let next_section = prompt[body_start..]
        .find("\n## ")
        .map(|offset| body_start + offset + 1)
        .unwrap_or(prompt.len());
    let section = replacement
        .filter(|content| !content.trim().is_empty())
        .map(|content| format!("{}{}\n\n", header, content))
        .unwrap_or_default();
    prompt.replace_range(start..next_section, &section);
    prompt
}

impl ContextBuilder {
    pub fn new(paths: Paths, config: Config) -> Self {
        let skills_dir = paths.skills_dir();
        let mut skill_manager = SkillManager::new()
            .with_versioning(skills_dir.clone())
            .with_evolution(skills_dir, EvolutionServiceConfig::default());
        skill_manager.set_openclaw_skill_enabled(config.openclaw_skill_enabled);
        let _ = skill_manager.load_from_paths(&paths);

        Self {
            paths,
            skill_manager: Some(skill_manager),
            ghost_learning_enabled: config.agents.ghost.learning.enabled,
            file_memory_snapshots: Mutex::new(HashMap::new()),
            memory_store: None,
            memory_injector: None,
            capability_brief: None,
            skill_index_summary: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_skill_manager(&mut self, manager: SkillManager) {
        self.skill_manager = Some(manager);
    }

    pub fn set_memory_store(&mut self, store: MemoryStoreHandle) {
        self.memory_store = Some(store);
    }

    /// Set the Layer 5 memory injector (7-layer memory system).
    pub fn set_memory_injector(&mut self, injector: MemoryInjector) {
        self.memory_injector = Some(injector);
    }

    /// Get the memory injector (for checking if initialized).
    pub fn memory_injector(&self) -> Option<&MemoryInjector> {
        self.memory_injector.as_ref()
    }

    /// Get the memory injector (for async loading).
    pub fn memory_injector_mut(&mut self) -> Option<&mut MemoryInjector> {
        self.memory_injector.as_mut()
    }

    /// Set the cached capability brief (called from tick or initialization).
    pub fn set_capability_brief(&mut self, brief: String) {
        if brief.is_empty() {
            self.capability_brief = None;
        } else {
            self.capability_brief = Some(brief);
        }
    }

    /// 设置 Skill 索引摘要 (可用 Skill 列表, 注入到系统提示词)
    pub fn set_skill_index_summary(&self, summary: String) {
        let mut s = self
            .skill_index_summary
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if summary.is_empty() {
            *s = None;
        } else {
            *s = Some(summary);
        }
    }

    /// 返回 skill_index_summary Arc 的克隆 (供后台任务共享)
    pub fn skill_index_summary_arc(&self) -> Arc<RwLock<Option<String>>> {
        self.skill_index_summary.clone()
    }

    /// 刷新 Skill 索引摘要 (Skill 变更后调用, 使下次 LLM 调用获取最新 Skill 列表)
    /// 使用 `&self` (内部 Arc<RwLock>) 以便后台 Review Agent 在完成后刷新
    pub fn refresh_skill_index_summary(&self) {
        let skills_dir = self.paths.skills_dir();
        let mut summary = self
            .skill_index_summary
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if skills_dir.exists() {
            let index = crate::skill_index::SkillIndex::build_from_dir(&skills_dir);
            *summary = if index.entries().is_empty() {
                None
            } else {
                Some(index.to_prompt_summary())
            };
        } else {
            *summary = None;
        }
    }

    /// Sync available capability IDs from the registry to the SkillManager.
    /// This allows skills to validate their capability dependencies.
    pub fn sync_capabilities(&mut self, capability_ids: Vec<String>) {
        if let Some(ref mut manager) = self.skill_manager {
            manager.sync_capabilities(capability_ids);
        }
    }

    /// Get missing capabilities across all skills (for auto-triggering evolution).
    pub fn get_missing_capabilities(&self) -> Vec<(String, String)> {
        if let Some(ref manager) = self.skill_manager {
            manager.get_missing_capabilities()
        } else {
            vec![]
        }
    }

    pub fn evolution_service(&self) -> Option<&EvolutionService> {
        self.skill_manager
            .as_ref()
            .and_then(|m| m.evolution_service())
    }

    /// Wire an LLM provider into the EvolutionService so that tick() can automatically
    /// drive the full generate→audit→dry run→shadow test→rollout pipeline.
    /// Call this after the provider is created in agent startup.
    pub fn set_evolution_llm_provider(&mut self, provider: Arc<dyn LLMProvider>) {
        if let Some(ref mut manager) = self.skill_manager {
            if let Some(evo) = manager.evolution_service_mut() {
                evo.set_llm_provider(provider);
            }
        }
    }

    /// Re-scan skill directories and pick up newly created skills.
    /// Returns the names of newly discovered skills.
    pub fn reload_skills(&mut self) -> Vec<String> {
        if let Some(ref mut manager) = self.skill_manager {
            match manager.reload_skills(&self.paths) {
                Ok(new_skills) => new_skills,
                Err(e) => {
                    tracing::warn!(error = ?e, "Failed to reload skills");
                    vec![]
                }
            }
        } else {
            vec![]
        }
    }

    /// Build system prompt with all content (legacy, no intent filtering).
    pub fn build_system_prompt(&self) -> String {
        self.build_system_prompt_for_mode_with_channel(
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "",
            "",
            &[],
            &[],
        )
    }

    fn frozen_file_memory_snapshot(&self, session_key: &str) -> FrozenFileMemorySnapshot {
        let mut snapshots = self
            .file_memory_snapshots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        snapshots
            .entry(session_key.to_string())
            .or_insert_with(|| FrozenFileMemorySnapshot::load(&self.paths))
            .clone()
    }

    pub fn resolve_active_skill(
        &self,
        user_input: &str,
        disabled_skills: &HashSet<String>,
    ) -> Option<ActiveSkillContext> {
        let skill_name = user_input.trim();
        if skill_name.is_empty() {
            return None;
        }
        self.resolve_active_skill_by_name(skill_name, disabled_skills)
    }

    pub fn resolve_active_skill_by_name(
        &self,
        skill_name: &str,
        disabled_skills: &HashSet<String>,
    ) -> Option<ActiveSkillContext> {
        if skill_name.is_empty() {
            return None;
        }
        if disabled_skills.contains(skill_name) {
            return None;
        }
        let manager = self.skill_manager.as_ref()?;
        let skill = manager.get(skill_name)?;
        if !skill.available {
            return None;
        }
        let prompt_md = skill.load_prompt_bundle()?;
        Some(ActiveSkillContext {
            name: skill.name.clone(),
            prompt_md,
            inject_prompt_md: true,
            tools: skill.meta.effective_tools(),
            fallback_message: skill
                .meta
                .fallback
                .as_ref()
                .and_then(|fallback| fallback.message.clone()),
            source: skill.meta.source.clone(),
        })
    }

    pub fn skill_manager(&self) -> Option<&SkillManager> {
        self.skill_manager.as_ref()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_system_prompt_for_mode_with_channel(
        &self,
        mode: InteractionMode,
        active_skill: Option<&ActiveSkillContext>,
        disabled_skills: &HashSet<String>,
        disabled_tools: &HashSet<String>,
        _channel: &str,
        user_query: &str,
        available_tool_names: &[String],
        tool_prompt_rules: &[String],
    ) -> String {
        let mut prompt = String::new();
        let is_chat = matches!(mode, InteractionMode::Chat);
        let is_skill_mode = matches!(mode, InteractionMode::Skill);
        let is_general = matches!(mode, InteractionMode::General);

        prompt.push_str("You are blockcell, an AI assistant with access to tools.\n\n");

        if let Some(content) = self.load_file_if_exists(self.paths.agents_md()) {
            prompt.push_str("## Agent Guidelines\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
        }

        if let Some(content) = self.load_file_if_exists(self.paths.soul_md()) {
            prompt.push_str("## Personality\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
        }

        if let Some(content) = self.load_file_if_exists(self.paths.user_md()) {
            prompt.push_str("## User Preferences\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
        }

        if let Some(content) = self.load_file_if_exists(self.paths.memory_md()) {
            prompt.push_str("## Durable File Memory\n");
            prompt.push_str(&content);
            prompt.push_str("\n\n");
        }

        if self.ghost_learning_enabled && !is_chat {
            prompt.push_str("## Ghost Learning\n");
            prompt.push_str(
                "BlockCell may review successful interactions after the response to learn durable user preferences, stable project facts, reusable workflows, and prompt-only learned skills.\n",
            );
            prompt.push_str(
                "- Save only durable facts that will still matter later: user preferences, recurring corrections, stable project facts, environment details, tool quirks, and conventions.\n",
            );
            prompt.push_str(
                "- Do not save task progress, temporary TODOs, completed-work logs, one-off outcomes, or short-lived status as durable memory.\n",
            );
            prompt.push_str(
                "- Write memories as declarative facts, not commands to yourself. Example: 'User prefers concise responses' is good; 'Always respond concisely' is not.\n",
            );
            prompt.push_str(
                "- Procedures and workflows belong in skills, not memory. When a complex method succeeds, a tricky error is fixed, or the user corrects your approach, state the reusable workflow naturally and concisely so Ghost can review it later.\n",
            );
            prompt.push_str(
                "- If the user references prior conversations or you suspect relevant history exists, use `session_search` before asking the user to repeat context.\n",
            );
            prompt.push_str(
                "- If an available learned skill is relevant, load it with `skill_view` before proceeding, even if you think you already know the task.\n",
            );
            prompt.push_str(
                "- If a loaded skill is stale, incomplete, or wrong, patch it with `skill_manage(action=\"patch\")` after validating the fix.\n",
            );
            prompt.push_str(
                "- Current user instructions always override learned memory and generated skills.\n\n",
            );
        }

        if !is_chat {
            prompt.push_str("\n## Tools\n");
            prompt.push_str("- Use tools when needed; otherwise answer directly.\n");
            prompt.push_str("- Prefer fewer tool calls; batch related work.\n");
            prompt.push_str("- Validate tool parameters against schema.\n");
            prompt.push_str("- For filesystem tools such as `list_dir`, `read_file`, `write_file`, and `edit_file`, always pass the required `path` explicitly. Do not call them with `{}` and do not assume an implicit current directory.\n");
            prompt.push_str("- When the user asks about agent nodes, node status, configured agents, or which agent owns which channel/account, use `agent_status` instead of guessing.\n");
            prompt.push_str(
                "- Never hardcode credentials — ask the user or read from config/memory.\n",
            );
            if available_tool_names.is_empty() {
                prompt.push_str("- There are no callable tools available in the current agent scope for this interaction. Do not claim tools outside the current scope.\n");
            } else {
                prompt.push_str(&format!(
                    "- Current callable tools in this interaction: {}\n",
                    available_tool_names.join(", ")
                ));
                prompt.push_str("- When the user asks which tools/capabilities you have, answer only from the current callable tool list above. Do not mention globally registered tools that are not in the current agent scope.\n");
            }
            for rule in tool_prompt_rules {
                prompt.push_str(rule);
                if !rule.ends_with('\n') {
                    prompt.push('\n');
                }
            }
            if tool_prompt_rules.is_empty() {
                prompt.push_str("- **MCP (Model Context Protocol)**: blockcell **已内置 MCP 客户端支持**，可连接任意 MCP 服务器（SQLite、GitHub、文件系统、数据库等）。MCP 工具会以 `<serverName>__<toolName>` 格式出现在工具列表中。若用户询问 MCP 功能或当前工具列表中无 MCP 工具，说明尚未配置 MCP 服务器，请引导用户使用 `blockcell mcp add <template>` 快捷添加，或直接编辑 `~/.blockcell/mcp.json` / `~/.blockcell/mcp.d/*.json`。例如：`blockcell mcp add sqlite --db-path /tmp/test.db`，重启后即可使用。\n");
            }
            prompt.push('\n');
        }

        let now = chrono::Utc::now();
        let local_time = chrono::Local::now();
        prompt.push_str(&format!(
            "Current time: {} ({} UTC)\n",
            local_time.format("%Y-%m-%d %H:%M:%S"),
            now.format("%Y-%m-%d %H:%M:%S")
        ));
        prompt.push_str(&format!(
            "Workspace: {}\n\n",
            self.paths.workspace().display()
        ));

        if is_skill_mode || is_general {
            if let Some(ref store) = self.memory_store {
                let brief_result = if !user_query.is_empty() {
                    store.generate_brief_for_query(user_query, 8)
                } else {
                    store.generate_brief(5, 3)
                };
                match brief_result {
                    Ok(brief) if !brief.is_empty() => {
                        prompt.push_str("## Memory Brief (SQLite FTS5 Search)\n");
                        prompt.push_str("> 以下是通过语义搜索检索的相关记忆：\n\n");
                        prompt.push_str(&brief);
                        prompt.push_str("\n\n");
                    }
                    _ => {}
                }
            } else {
                let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
                if let Some(content) = self.load_file_if_exists(self.paths.daily_memory(&today)) {
                    prompt.push_str("## Today's Notes (Legacy File)\n");
                    prompt.push_str(&content);
                    prompt.push_str("\n\n");
                }
            }
        }

        // Layer 5: 注入持久化记忆 (7 层记忆系统)
        // 在 SQLite 记忆之后注入，提供更深层的上下文
        if let Some(ref injector) = self.memory_injector {
            let injection = injector.build_injection_content();
            if !injection.is_empty() {
                prompt.push_str(&injection);

                // 记录 Layer 5 injection_completed 事件
                let (user, project, feedback, reference) = injector.memory_counts();
                crate::memory_event!(
                    layer5,
                    injection_completed,
                    user,
                    project,
                    feedback,
                    reference
                );
            }
        }

        if !disabled_skills.is_empty() || !disabled_tools.is_empty() {
            prompt.push_str("## ⚠️ Disabled Items\n");
            prompt.push_str("The following items have been disabled by the user via toggle.\n");
            prompt.push_str("IMPORTANT: When user asks to 打开/开启/启用/enable any of these, you MUST call `toggle_manage` tool with action='set', category, name, enabled=true. Do NOT use list_skills.\n");
            if !disabled_skills.is_empty() {
                let mut names: Vec<&String> = disabled_skills.iter().collect();
                names.sort();
                prompt.push_str(&format!(
                    "Disabled skills: {}\n",
                    names
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !disabled_tools.is_empty() {
                let mut names: Vec<&String> = disabled_tools.iter().collect();
                names.sort();
                prompt.push_str(&format!(
                    "Disabled tools: {}\n",
                    names
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            prompt.push('\n');
        }

        if is_skill_mode {
            if let Some(ref brief) = self.capability_brief {
                prompt.push_str("## Dynamic Evolved Tools\n");
                prompt.push_str("The following tools have been dynamically evolved and are available. Use `capability_evolve` tool with action='execute' to invoke them.\n");
                prompt.push_str(brief);
                prompt.push_str("\n\n");
            }
        }

        if let Some(skill) = active_skill {
            prompt.push_str(&format!("## Active Skill: {}\n", skill.name));
            if skill.inject_prompt_md {
                prompt.push_str("The user's input matches this installed skill. Follow the skill's instructions below. Prefer the skill's scoped tools and avoid unrelated tools.\n\n");
                prompt.push_str(&skill.prompt_md);
                prompt.push_str("\n\n");
            } else {
                prompt.push_str("The user's input matches this installed skill. Use the skill's scoped tools and avoid unrelated tools.\n\n");
            }
            if let Some(fallback_message) = &skill.fallback_message {
                prompt.push_str("## Skill Fallback\n");
                prompt.push_str(fallback_message);
                prompt.push_str("\n\n");
            }
        }

        if is_general {
            prompt.push_str("## Core Tool Scope\n");
            prompt.push_str("You currently have access to the minimal built-in tool kernel only. Specialized domain tools are activated by matching installed skills. Prefer the available core tools unless a skill is explicitly active. If the user's request would be better served by specialized domain capabilities that are not currently active, briefly remind the user that they can install the corresponding skills to extend blockcell.\n\n");
        }

        // 注入 Skill 系统引导 (参考 Hermes MEMORY_GUIDANCE)
        if !is_chat {
            prompt.push_str(SKILL_GUIDANCE);
            prompt.push('\n');
        }

        // 注入 Memory 使用指导 (与 Hermes MEMORY_GUIDANCE 对齐)
        if self.memory_store.is_some() {
            prompt.push_str(MEMORY_GUIDANCE);
            prompt.push('\n');
        }

        // 注入 Skill 索引摘要 (可用 Skill 列表)
        if let Some(ref summary) = *self
            .skill_index_summary
            .read()
            .unwrap_or_else(|e| e.into_inner())
        {
            if !summary.is_empty() {
                prompt.push_str("\n## Available Skills\n");
                prompt.push_str(summary);
                prompt.push('\n');
            }
        }

        prompt
    }

    #[allow(clippy::too_many_arguments)]
    fn build_system_prompt_for_mode_with_channel_and_memory_snapshot(
        &self,
        mode: InteractionMode,
        active_skill: Option<&ActiveSkillContext>,
        disabled_skills: &HashSet<String>,
        disabled_tools: &HashSet<String>,
        channel: &str,
        user_query: &str,
        available_tool_names: &[String],
        tool_prompt_rules: &[String],
        memory_snapshot: Option<&FrozenFileMemorySnapshot>,
    ) -> String {
        if memory_snapshot.is_none() {
            return self.build_system_prompt_for_mode_with_channel(
                mode,
                active_skill,
                disabled_skills,
                disabled_tools,
                channel,
                user_query,
                available_tool_names,
                tool_prompt_rules,
            );
        }

        let snapshot = memory_snapshot.expect("checked above");
        let mut prompt = self.build_system_prompt_for_mode_with_channel(
            mode,
            active_skill,
            disabled_skills,
            disabled_tools,
            channel,
            user_query,
            available_tool_names,
            tool_prompt_rules,
        );

        prompt = replace_prompt_section(prompt, "## User Preferences\n", snapshot.user.as_deref());
        replace_prompt_section(
            prompt,
            "## Durable File Memory\n",
            snapshot.memory.as_deref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_messages_for_mode_with_channel(
        &self,
        history: &[ChatMessage],
        user_content: &str,
        media: &[String],
        mode: InteractionMode,
        active_skill: Option<&ActiveSkillContext>,
        disabled_skills: &HashSet<String>,
        disabled_tools: &HashSet<String>,
        channel: &str,
        pending_intent: bool,
        available_tool_names: &[String],
        tool_prompt_rules: &[String],
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        let system_prompt = self.build_system_prompt_for_mode_with_channel(
            mode,
            active_skill,
            disabled_skills,
            disabled_tools,
            channel,
            user_content,
            available_tool_names,
            tool_prompt_rules,
        );
        messages.push(ChatMessage::system(&system_prompt));
        self.append_history_and_user_message(
            &mut messages,
            history,
            user_content,
            media,
            pending_intent,
        );
        messages
    }

    #[allow(clippy::too_many_arguments)]
    pub fn build_messages_for_session_mode_with_channel(
        &self,
        session_key: &str,
        history: &[ChatMessage],
        user_content: &str,
        media: &[String],
        mode: InteractionMode,
        active_skill: Option<&ActiveSkillContext>,
        disabled_skills: &HashSet<String>,
        disabled_tools: &HashSet<String>,
        channel: &str,
        pending_intent: bool,
        available_tool_names: &[String],
        tool_prompt_rules: &[String],
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        let memory_snapshot = self.frozen_file_memory_snapshot(session_key);
        let system_prompt = self.build_system_prompt_for_mode_with_channel_and_memory_snapshot(
            mode,
            active_skill,
            disabled_skills,
            disabled_tools,
            channel,
            user_content,
            available_tool_names,
            tool_prompt_rules,
            Some(&memory_snapshot),
        );
        messages.push(ChatMessage::system(&system_prompt));
        self.append_history_and_user_message(
            &mut messages,
            history,
            user_content,
            media,
            pending_intent,
        );
        messages
    }

    fn append_history_and_user_message(
        &self,
        messages: &mut Vec<ChatMessage>,
        history: &[ChatMessage],
        user_content: &str,
        media: &[String],
        pending_intent: bool,
    ) {
        let user_msg = if media.is_empty() {
            let trimmed = Self::trim_text_head_tail(user_content, 4000);
            ChatMessage::user(&trimmed)
        } else {
            let trimmed = Self::trim_text_head_tail(user_content, 4000);
            let all_paths: Vec<&str> = media
                .iter()
                .filter(|p| !p.is_empty())
                .map(|p| p.as_str())
                .collect();
            let text_with_paths = if all_paths.is_empty() {
                trimmed
            } else {
                let paths_str = all_paths
                    .iter()
                    .map(|p| format!("- `{}`", p))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!(
                    "{}\n\n[附件本地路径（发回给用户时请用此路径）]\n{}",
                    trimmed, paths_str
                )
            };
            if pending_intent {
                ChatMessage::user(&text_with_paths)
            } else {
                self.build_multimodal_message(&text_with_paths, media)
            }
        };

        let safe_start = Self::find_safe_history_start(history);
        for msg in &history[safe_start..] {
            messages.push(msg.clone());
        }
        messages.push(user_msg);
    }

    fn build_multimodal_message(&self, text: &str, media: &[String]) -> ChatMessage {
        let mut content_parts = Vec::new();

        // Add media (images as base64)
        for media_path in media {
            if let Some(image_content) = self.encode_image_to_base64(media_path) {
                content_parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": image_content
                    }
                }));
            }
        }

        // Add text
        if !text.is_empty() {
            content_parts.push(serde_json::json!({
                "type": "text",
                "text": text
            }));
        }

        ChatMessage {
            id: None,
            role: "user".to_string(),
            content: serde_json::Value::Array(content_parts),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    fn _is_image_path(path: &str) -> bool {
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
        matches!(
            ext.as_str(),
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "tiff" | "ico"
        )
    }

    fn encode_image_to_base64(&self, path: &str) -> Option<String> {
        use base64::Engine;
        use std::path::Path;

        let path = Path::new(path);
        if !path.exists() {
            return None;
        }

        // Check if it's an image file
        let ext = path.extension()?.to_str()?.to_lowercase();
        let mime_type = match ext.as_str() {
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None, // Not an image
        };

        // Read and encode
        let bytes = std::fs::read(path).ok()?;
        let base64_str = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Some(format!("data:{};base64,{}", mime_type, base64_str))
    }

    /// Find a safe starting index in truncated history to avoid orphaned tool messages.
    ///
    /// After truncation, the history might start with:
    /// - A "tool" message whose tool_call_id references an assistant message that was cut off
    /// - An "assistant" message with tool_calls but missing subsequent tool responses
    ///
    /// Both cases cause LLM API 400 errors ("tool_call_id not found").
    /// This function skips forward until we find a clean starting point.
    fn find_safe_history_start(history: &[ChatMessage]) -> usize {
        if history.is_empty() {
            return 0;
        }

        let mut i = 0;

        // Skip leading "tool" role messages — they reference tool_calls from a missing assistant message
        while i < history.len() && history[i].role == "tool" {
            i += 1;
        }

        // If we land on an "assistant" message with tool_calls, check that ALL its
        // tool responses are present in the subsequent messages
        while i < history.len() {
            if history[i].role == "assistant" {
                if let Some(ref tool_calls) = history[i].tool_calls {
                    if !tool_calls.is_empty() {
                        // Collect expected tool_call_ids
                        let expected_ids: Vec<&str> =
                            tool_calls.iter().map(|tc| tc.id.as_str()).collect();

                        // Check that all expected tool responses follow
                        let mut found_ids = std::collections::HashSet::new();
                        for msg in history.iter().skip(i + 1) {
                            if msg.role == "tool" {
                                if let Some(ref id) = msg.tool_call_id {
                                    found_ids.insert(id.as_str());
                                }
                            } else {
                                break; // Stop at first non-tool message
                            }
                        }

                        let all_present = expected_ids.iter().all(|id| found_ids.contains(id));
                        if !all_present {
                            // Skip this assistant + its partial tool responses
                            i += 1;
                            while i < history.len() && history[i].role == "tool" {
                                i += 1;
                            }
                            continue;
                        }
                    }
                }
            }
            break;
        }

        i
    }

    fn trim_text_head_tail(s: &str, max_chars: usize) -> String {
        if max_chars == 0 {
            return String::new();
        }

        let char_count = s.chars().count();
        if char_count <= max_chars {
            return s.to_string();
        }

        let head_chars = (max_chars * 2) / 3;
        let tail_chars = max_chars.saturating_sub(head_chars);

        let head = s.chars().take(head_chars).collect::<String>();
        let tail = s.chars().rev().take(tail_chars).collect::<String>();
        let tail = tail.chars().rev().collect::<String>();

        format!(
            "{}\n...<trimmed {} chars>...\n{}",
            head,
            char_count.saturating_sub(max_chars),
            tail
        )
    }

    fn load_file_if_exists<P: AsRef<Path>>(&self, path: P) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::Result;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn test_chat_message_text(msg: &ChatMessage) -> String {
        match &msg.content {
            serde_json::Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }

    struct EmptyMemoryStore;

    impl blockcell_tools::MemoryStoreOps for EmptyMemoryStore {
        fn upsert_json(&self, _params_json: Value) -> Result<Value> {
            Ok(json!({}))
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
    use std::fs;

    #[test]
    fn test_resolve_active_skill_by_name_keeps_manual_injection_for_script_skill() {
        let base =
            std::env::temp_dir().join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_base(base);
        let skill_dir = paths.skills_dir().join("structured_demo");
        fs::create_dir_all(&skill_dir).expect("create skill dir");
        fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: structured_demo
description: structured demo
"#,
        )
        .expect("write meta");
        fs::write(skill_dir.join("SKILL.md"), "structured skill manual").expect("write skill md");
        fs::write(skill_dir.join("SKILL.py"), "print('ok')").expect("write skill py");

        let builder = ContextBuilder::new(paths, Config::default());

        let ctx = builder
            .resolve_active_skill_by_name("structured_demo", &HashSet::new())
            .expect("active skill should resolve");

        assert!(ctx.inject_prompt_md);
    }

    #[test]
    fn test_resolve_active_skill_by_name_uses_prompt_bundle_not_root_skill_md() {
        let base =
            std::env::temp_dir().join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_base(base);
        let skill_dir = paths.skills_dir().join("prompt_demo");
        fs::create_dir_all(skill_dir.join("manual")).expect("create manual dir");
        fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: prompt_demo
description: prompt demo
"#,
        )
        .expect("write meta");
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Prompt Demo

## Shared {#shared}
Shared rule.

## Prompt {#prompt}
- [Prompt details](manual/prompt.md#details)

## Planning {#planning}
Planning-only rule.
"#,
        )
        .expect("write skill md");
        fs::write(
            skill_dir.join("manual/prompt.md"),
            r#"## Prompt details {#details}
Prompt-only rule.
"#,
        )
        .expect("write prompt child md");

        let builder = ContextBuilder::new(paths, Config::default());

        let ctx = builder
            .resolve_active_skill_by_name("prompt_demo", &HashSet::new())
            .expect("active skill should resolve");

        assert!(ctx.inject_prompt_md);
        assert!(ctx.prompt_md.contains("Shared rule."));
        assert!(ctx.prompt_md.contains("Prompt-only rule."));
        assert!(!ctx.prompt_md.contains("Planning-only rule."));
    }

    #[test]
    fn test_resolve_active_skill_does_not_match_free_text_without_explicit_name() {
        let base =
            std::env::temp_dir().join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_base(base);
        let skill_dir = paths.skills_dir().join("deploy_demo");
        fs::create_dir_all(&skill_dir).expect("create skill dir");
        fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: deploy_demo
description: deploy demo
"#,
        )
        .expect("write meta");
        fs::write(skill_dir.join("SKILL.md"), "deploy manual").expect("write skill md");

        let builder = ContextBuilder::new(paths, Config::default());

        assert!(builder
            .resolve_active_skill("please deploy the release", &HashSet::new())
            .is_none());
        assert_eq!(
            builder
                .resolve_active_skill("deploy_demo", &HashSet::new())
                .map(|ctx| ctx.name),
            Some("deploy_demo".to_string())
        );
    }

    #[test]
    fn test_build_system_prompt_skips_skill_md_when_prompt_injection_disabled() {
        let builder = ContextBuilder::new(
            Paths::with_base(
                std::env::temp_dir()
                    .join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4())),
            ),
            Config::default(),
        );
        let active_skill = ActiveSkillContext {
            name: "structured_demo".to_string(),
            prompt_md: "DO NOT INCLUDE".to_string(),
            inject_prompt_md: false,
            tools: vec!["finance_api".to_string()],
            fallback_message: Some("fallback".to_string()),
            source: blockcell_skills::manager::SkillSource::BlockCell,
        };

        let prompt = builder.build_system_prompt_for_mode_with_channel(
            InteractionMode::Skill,
            Some(&active_skill),
            &HashSet::new(),
            &HashSet::new(),
            "cli",
            "",
            &[],
            &[],
        );

        assert!(prompt.contains("## Active Skill: structured_demo"));
        assert!(!prompt.contains("DO NOT INCLUDE"));
        assert!(prompt.contains("fallback"));
    }

    #[test]
    fn test_build_messages_does_not_inject_followup_resolution_hint() {
        let builder = ContextBuilder::new(
            Paths::with_base(
                std::env::temp_dir()
                    .join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4())),
            ),
            Config::default(),
        );
        let messages = builder.build_messages_for_mode_with_channel(
            &[],
            "查看 .env 的内容",
            &[],
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "ws",
            false,
            &["read_file".to_string()],
            &[],
        );

        let last = messages.last().expect("user message");
        let content = last.content.as_str().expect("string user content");
        assert!(content.contains("查看 .env 的内容"));
        assert!(!content.contains("[Follow-up Reference]"));
        assert!(!content.contains("/Users/apple/.blockcell/.env"));
    }
    #[test]
    fn test_build_system_prompt_always_injects_file_memory() {
        let base =
            std::env::temp_dir().join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_base(base);
        paths.ensure_dirs().expect("ensure dirs");
        std::fs::write(
            paths.memory_md(),
            "Project fact: release verification starts with rollback planning.",
        )
        .expect("write memory md");
        let mut builder = ContextBuilder::new(paths, Config::default());
        builder.set_memory_store(Arc::new(EmptyMemoryStore));

        let prompt = builder.build_system_prompt_for_mode_with_channel(
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "cli",
            "release verification",
            &[],
            &[],
        );

        assert!(prompt.contains("## Durable File Memory"));
        assert!(prompt.contains("release verification starts with rollback planning"));
    }

    #[test]
    fn test_file_memory_prompt_snapshot_is_frozen_per_session() {
        let base =
            std::env::temp_dir().join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_base(base);
        paths.ensure_dirs().expect("ensure dirs");
        std::fs::write(paths.memory_md(), "Initial durable memory.").expect("write memory md");
        let builder = ContextBuilder::new(paths.clone(), Config::default());

        let first = builder.build_messages_for_session_mode_with_channel(
            "session-a",
            &[],
            "hello",
            &[],
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "cli",
            false,
            &[],
            &[],
        );
        std::fs::write(paths.memory_md(), "Updated durable memory.").expect("rewrite memory md");
        let same_session = builder.build_messages_for_session_mode_with_channel(
            "session-a",
            &[],
            "hello again",
            &[],
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "cli",
            false,
            &[],
            &[],
        );
        let next_session = builder.build_messages_for_session_mode_with_channel(
            "session-b",
            &[],
            "new session",
            &[],
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "cli",
            false,
            &[],
            &[],
        );

        let first_prompt = first
            .first()
            .map(test_chat_message_text)
            .unwrap_or_default();
        let same_prompt = same_session
            .first()
            .map(test_chat_message_text)
            .unwrap_or_default();
        let next_prompt = next_session
            .first()
            .map(test_chat_message_text)
            .unwrap_or_default();
        assert!(first_prompt.contains("Initial durable memory."));
        assert!(same_prompt.contains("Initial durable memory."));
        assert!(!same_prompt.contains("Updated durable memory."));
        assert!(next_prompt.contains("Updated durable memory."));
    }

    #[test]
    fn test_build_system_prompt_injects_ghost_learning_guidance_when_enabled() {
        let mut config = Config::default();
        config.agents.ghost.learning.enabled = true;
        let builder = ContextBuilder::new(
            Paths::with_base(
                std::env::temp_dir()
                    .join(format!("blockcell-context-test-{}", uuid::Uuid::new_v4())),
            ),
            config,
        );

        let prompt = builder.build_system_prompt_for_mode_with_channel(
            InteractionMode::General,
            None,
            &HashSet::new(),
            &HashSet::new(),
            "cli",
            "用户以后 prefers canary deploys",
            &[],
            &[],
        );

        assert!(prompt.contains("## Ghost Learning"));
        assert!(prompt.contains("durable user preferences"));
        assert!(prompt.contains("reusable workflows"));
        assert!(prompt.contains("prompt-only learned skills"));
        assert!(prompt.contains("Write memories as declarative facts"));
        assert!(prompt.contains("Procedures and workflows belong in skills"));
        assert!(prompt.contains("use `session_search`"));
        assert!(prompt.contains("load it with `skill_view`"));
        assert!(prompt.contains("patch it with `skill_manage(action=\"patch\")`"));
        assert!(prompt.contains("Do not save task progress"));
        assert!(!prompt.contains("skill candidates"));
    }
}
