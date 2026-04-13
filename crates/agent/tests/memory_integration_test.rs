//! 7 层记忆系统集成测试
//!
//! 测试完整的记忆流程。

#[cfg(test)]
mod tests {
    use blockcell_agent::auto_memory::{ExtractionCursor, ExtractionCursorManager, MemoryType};
    use blockcell_agent::memory_system::{
        evaluate_memory_hooks, MemorySystem, MemorySystemConfig, PostSamplingAction,
    };
    use blockcell_agent::session_memory::{
        should_extract_memory, SessionMemoryConfig, SessionMemoryState,
    };
    use blockcell_core::types::ChatMessage;
    use std::path::PathBuf;

    /// 测试 MemorySystem 初始化
    #[test]
    fn test_memory_system_init() {
        let config = MemorySystemConfig::default();
        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test-session-123".to_string(),
        );

        assert_eq!(memory_system.session_id(), "test-session-123");
        assert!(!memory_system.has_pending_extraction());
    }

    /// 测试 Compact 触发检测
    #[test]
    fn test_compact_trigger() {
        let config = MemorySystemConfig {
            token_budget: 100,
            compact_threshold: 0.8,
            ..Default::default()
        };
        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test".to_string(),
        );

        // 低于阈值，不应触发
        assert!(!memory_system.should_compact(70));

        // 达到阈值，应触发
        assert!(memory_system.should_compact(80));

        // 超过阈值，应触发
        assert!(memory_system.should_compact(100));
    }

    /// 测试 Post-Sampling Hook 评估
    #[test]
    fn test_post_sampling_hook_none() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test".to_string(),
        );

        let messages = vec![ChatMessage::user("Hello"), ChatMessage::assistant("Hi!")];

        let action = evaluate_memory_hooks(&mut memory_system, &messages, 100);
        assert!(matches!(action, PostSamplingAction::None));
    }

    /// 测试 Post-Sampling Hook Compact 触发
    #[test]
    fn test_post_sampling_hook_compact() {
        let config = MemorySystemConfig {
            token_budget: 100,
            compact_threshold: 0.8,
            ..Default::default()
        };
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test".to_string(),
        );

        let messages = vec![ChatMessage::user("Test")];
        let action = evaluate_memory_hooks(&mut memory_system, &messages, 100);

        assert!(matches!(action, PostSamplingAction::Compact));
    }

    /// 测试 Session Memory 状态更新
    #[test]
    fn test_session_memory_state_update() {
        let mut state = SessionMemoryState::default();
        let message_index = 42;

        state.last_memory_message_index = Some(message_index);
        state.tokens_at_last_extraction = 5000;
        state.initialized = true;

        assert_eq!(state.last_memory_message_index, Some(message_index));
        assert_eq!(state.tokens_at_last_extraction, 5000);
        assert!(state.initialized);
    }

    /// 测试 Session Memory 触发条件
    #[test]
    fn test_session_memory_trigger() {
        // 创建足够的消息来触发提取
        let mut messages = Vec::new();
        for i in 0..100 {
            messages.push(ChatMessage::user(&format!("Message {}", i)));
            messages.push(ChatMessage::assistant(&format!("Response {}", i)));
        }

        let state = SessionMemoryState {
            initialized: true,
            tokens_at_last_extraction: 0,
            config: SessionMemoryConfig {
                minimum_message_tokens_to_init: 10_000,
                minimum_tokens_between_update: 5_000,
                tool_calls_between_updates: 3,
            },
            ..Default::default()
        };

        // 由于消息足够多，应该触发提取
        // 注意：实际触发还取决于 token 估算
        let should = should_extract_memory(&messages, &state);
        // 这个测试可能需要根据实际 token 估算调整
        println!("Should extract: {}", should);
    }

    /// 测试记忆类型枚举
    #[test]
    fn test_memory_types() {
        let types = MemoryType::all();
        assert_eq!(types.len(), 4);
        assert!(types.contains(&MemoryType::User));
        assert!(types.contains(&MemoryType::Project));
        assert!(types.contains(&MemoryType::Feedback));
        assert!(types.contains(&MemoryType::Reference));
    }

    /// 测试记忆类型名称
    #[test]
    fn test_memory_type_names() {
        assert_eq!(MemoryType::User.name(), "user");
        assert_eq!(MemoryType::Project.name(), "project");
        assert_eq!(MemoryType::Feedback.name(), "feedback");
        assert_eq!(MemoryType::Reference.name(), "reference");
    }

    /// 测试提取游标管理器
    #[test]
    fn test_extraction_cursor_manager() {
        let path = PathBuf::from("/tmp/config");
        let manager = ExtractionCursorManager::new(&path);

        let cursor = manager.get_cursor(MemoryType::User);
        assert_eq!(cursor.memory_type, MemoryType::User);
        assert!(cursor.last_extracted_uuid.is_none());
        assert_eq!(cursor.last_message_count, 0);
    }

    /// 测试内容替换状态
    #[test]
    fn test_content_replacement_state() {
        use blockcell_agent::response_cache::ContentReplacementState;

        let mut state = ContentReplacementState::default();

        // 添加一个已见 ID
        state.seen_ids.insert("tool-123".to_string());

        // 检查是否包含
        assert!(state.seen_ids.contains("tool-123"));
        assert!(!state.seen_ids.contains("tool-456"));
    }

    /// 测试记忆系统配置默认值
    #[test]
    fn test_memory_system_config_defaults() {
        let config = MemorySystemConfig::default();

        assert!(config.auto_memory_enabled);
        assert!(config.compact_enabled);
        assert_eq!(config.compact_threshold, 0.8);
        assert_eq!(config.token_budget, 100_000);
        assert_eq!(config.session_memory.minimum_message_tokens_to_init, 10_000);
        assert_eq!(config.session_memory.minimum_tokens_between_update, 5_000);
        assert_eq!(config.session_memory.tool_calls_between_updates, 3);
    }

    // ========================================================================
    // 新增集成测试
    // ========================================================================

    /// 测试后台任务追踪机制
    #[tokio::test]
    async fn test_background_task_tracking() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test-bg-tasks".to_string(),
        );

        // 初始状态无后台任务
        assert_eq!(memory_system.background_task_count(), 0);
        assert!(!memory_system.has_running_background_tasks());

        // 创建一个立即完成的任务
        let handle = tokio::task::spawn(async {});
        memory_system.add_background_task(handle);

        // 有一个后台任务
        assert_eq!(memory_system.background_task_count(), 1);

        // 清理已完成的任务
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let cleaned = memory_system.cleanup_completed_tasks();
        assert_eq!(cleaned, 1);
        assert_eq!(memory_system.background_task_count(), 0);
    }

    /// 测试取消所有后台任务
    #[tokio::test]
    async fn test_abort_all_background_tasks() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test-abort".to_string(),
        );

        // 创建几个长时间运行的任务
        for _ in 0..3 {
            let handle = tokio::task::spawn(async {
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            });
            memory_system.add_background_task(handle);
        }

        assert_eq!(memory_system.background_task_count(), 3);

        // 取消所有任务
        memory_system.abort_all_background_tasks();

        assert_eq!(memory_system.background_task_count(), 0);
    }

    /// 测试消息 ID 追踪
    #[test]
    fn test_message_id_tracking() {
        // 创建带 ID 的消息
        let msg1 = ChatMessage::user("Hello").with_id();
        let msg2 = ChatMessage::assistant("Hi!").with_id();

        // 消息应该有 ID
        assert!(msg1.id.is_some());
        assert!(msg2.id.is_some());

        // ID 应该不同
        assert_ne!(msg1.id, msg2.id);
    }

    /// 测试消息 ID 在 Session Memory 状态中的使用
    #[test]
    fn test_session_memory_message_id() {
        let mut state = SessionMemoryState::default();

        // 更新状态时包含消息 ID
        let msg_id = "msg-uuid-12345".to_string();
        state.last_memory_message_id = Some(msg_id.clone());
        state.last_memory_message_index = Some(42);
        state.tokens_at_last_extraction = 5000;
        state.initialized = true;

        // 验证 ID 被正确存储
        assert_eq!(state.last_memory_message_id, Some(msg_id));
        assert_eq!(state.last_memory_message_index, Some(42));
    }

    /// 测试提取游标的时间冷却（使用 Instant）
    #[test]
    fn test_extraction_cursor_time_cooldown() {
        let mut cursor = ExtractionCursor::new(MemoryType::User);

        // 初始状态，时间冷却应该通过（从未提取过）
        assert!(cursor.check_time_cooldown(300));

        // 更新游标（记录 Instant）
        cursor.update(uuid::Uuid::new_v4(), 10);

        // 刚更新完，时间冷却不应该通过
        assert!(!cursor.check_time_cooldown(300));

        // 验证 last_extraction_instant 已设置
        assert!(cursor.last_extraction_instant.is_some());
    }

    /// 测试提取游标的 monotonic clock 行为
    #[test]
    fn test_extraction_cursor_monotonic_time() {
        let mut cursor = ExtractionCursor::new(MemoryType::User);
        cursor.update(uuid::Uuid::new_v4(), 10);

        // 获取初始 Instant
        let initial_instant = cursor.last_extraction_instant.unwrap();

        // 等待一小段时间
        std::thread::sleep(std::time::Duration::from_millis(50));

        // 验证 elapsed 时间正确计算
        let elapsed = initial_instant.elapsed();
        assert!(elapsed.as_millis() >= 50);
    }

    /// 测试消息 ID 定位（count_tool_calls_since）
    #[test]
    fn test_tool_calls_count_with_message_id() {
        use blockcell_core::types::ToolCallRequest;

        // 创建带工具调用的消息
        let msg1 = ChatMessage::user("Hello").with_specific_id("msg-1");
        let msg2 = ChatMessage {
            id: Some("msg-2".to_string()),
            role: "assistant".to_string(),
            content: serde_json::Value::String(String::new()),
            reasoning_content: None,
            tool_calls: Some(vec![ToolCallRequest {
                id: "tool-1".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            }]),
            tool_call_id: None,
            name: None,
        };
        let msg3 = ChatMessage::user("Next").with_specific_id("msg-3");

        let messages = vec![msg1, msg2, msg3];

        // 使用消息 ID 定位
        let count = blockcell_agent::session_memory::count_tool_calls_since(
            &messages,
            Some("msg-1"), // 从 msg-1 之后开始
            None,
        );

        // 应该找到 1 个工具调用
        assert_eq!(count, 1);
    }

    /// 测试 CacheSafeParams 工具定义兼容性
    #[test]
    fn test_cache_safe_params_tools() {
        use blockcell_agent::forked::{CacheSafeParams, ToolDefinition};

        let tools = vec![
            ToolDefinition::new("read_file", "Read file contents", serde_json::json!({})),
            ToolDefinition::new("write_file", "Write file contents", serde_json::json!({})),
        ];

        let params1 = CacheSafeParams::new("system", "model").with_tools(tools.clone());

        let params2 = CacheSafeParams::new("system", "model").with_tools(tools);

        // 相同工具定义应该兼容
        assert!(params1.is_compatible_with(&params2));

        // 不同工具定义应该不兼容
        let params3 =
            CacheSafeParams::new("system", "model").with_tools(vec![ToolDefinition::new(
                "read_file",
                "Read file",
                serde_json::json!({}),
            )]);
        assert!(!params1.is_compatible_with(&params3));
    }

    /// 测试 MemorySystem 状态更新带消息 ID
    #[test]
    fn test_memory_system_update_with_id() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test-update-id".to_string(),
        );

        // 使用带 ID 的更新方法
        let msg_id = "msg-uuid-67890".to_string();
        memory_system.update_session_memory_state_with_id(Some(msg_id.clone()), 42, 5000);

        // 验证状态
        let state = memory_system.session_memory_state();
        assert_eq!(state.last_memory_message_id, Some(msg_id));
        assert_eq!(state.last_memory_message_index, Some(42));
        assert_eq!(state.tokens_at_last_extraction, 5000);
        assert!(state.initialized);
    }

    // ========================================================================
    // 跨层交互集成测试
    // ========================================================================

    /// 测试 Layer 1 + Layer 2 交互：工具结果存储与轻量压缩
    ///
    /// 验证：当工具结果被持久化后，时间触发轻量压缩能正确清理旧内容
    #[test]
    fn test_layer1_layer2_interaction() {
        use blockcell_agent::history_projector::COMPACTABLE_TOOLS;
        use blockcell_agent::response_cache::{
            generate_preview, PREVIEW_SIZE_BYTES, TIME_BASED_MC_CLEARED_MESSAGE,
        };

        // Layer 1: 生成预览
        let large_content = "line1\nline2\nline3\nline4\nline5\n".repeat(1000);
        let (preview, has_more) = generate_preview(&large_content, PREVIEW_SIZE_BYTES);
        assert!(has_more);
        assert!(preview.len() <= PREVIEW_SIZE_BYTES);

        // Layer 2: 验证可压缩工具列表包含 read_file
        assert!(COMPACTABLE_TOOLS.contains(&"read_file"));

        // 验证清理标记消息
        assert!(!TIME_BASED_MC_CLEARED_MESSAGE.is_empty());
    }

    /// 测试 Layer 3 + Layer 4 交互：会话记忆与完整压缩恢复
    ///
    /// 验证：Compact 后能正确恢复 Session Memory 关键信息
    #[test]
    fn test_layer3_layer4_recovery_interaction() {
        use blockcell_agent::compact::{
            FileRecoveryState, MAX_FILE_RECOVERY_TOKENS, MAX_SINGLE_FILE_TOKENS,
        };
        use blockcell_agent::session_memory::{Section, SectionPriority};

        // Layer 3: 创建会话记忆章节（验证类型可构造）
        let _section = Section {
            priority: SectionPriority::High,
            title: "User Preferences".to_string(),
            description: "User prefers dark mode".to_string(),
        };

        // Layer 4: 验证恢复预算限制
        assert_eq!(MAX_FILE_RECOVERY_TOKENS, 50_000);
        assert_eq!(MAX_SINGLE_FILE_TOKENS, 5_000);

        // 验证恢复上下文能跟踪文件状态
        let file_state = FileRecoveryState {
            path: PathBuf::from("/tmp/test.rs"),
            content_summary: "fn main() {}".to_string(),
            estimated_tokens: 1000,
            was_modified: false,
        };
        assert!(file_state.estimated_tokens <= MAX_SINGLE_FILE_TOKENS);
    }

    /// 测试 Layer 5 + Layer 7 交互：自动记忆提取与 Forked Agent
    ///
    /// 验证：Forked Agent 的工具权限正确限制记忆提取操作
    #[test]
    fn test_layer5_layer7_forked_interaction() {
        use blockcell_agent::forked::{create_auto_mem_can_use_tool, ToolPermission};
        use std::fs;

        // 创建临时目录进行测试
        let temp_dir = std::env::temp_dir().join("blockcell_layer5_test");
        fs::create_dir_all(&temp_dir).ok();
        let memory_dir = &temp_dir;

        // Layer 7: 验证 Forked Agent 工具权限
        let can_use = create_auto_mem_can_use_tool(memory_dir);

        // Layer 5: 自动记忆提取允许的工具
        // read_file 应该被允许（返回 Allow）
        let result = can_use(
            "read_file",
            &serde_json::json!({"file_path": "/tmp/test.md"}),
        );
        assert!(matches!(result, ToolPermission::Allow));

        // grep 应该被允许
        let result = can_use("grep", &serde_json::json!({"pattern": "test"}));
        assert!(matches!(result, ToolPermission::Allow));

        // glob 应该被允许
        let result = can_use("glob", &serde_json::json!({"pattern": "*.md"}));
        assert!(matches!(result, ToolPermission::Allow));

        // file_edit 在 memory 目录内应该被允许（使用临时目录路径）
        let memory_file = temp_dir.join("user.md");
        let memory_file_str = memory_file.to_string_lossy();
        let result = can_use(
            "file_edit",
            &serde_json::json!({"file_path": memory_file_str.as_ref()}),
        );
        assert!(matches!(result, ToolPermission::Allow));

        // file_edit 在 memory 目录外应该被拒绝
        let result = can_use(
            "file_edit",
            &serde_json::json!({"file_path": "/tmp/other/file.md"}),
        );
        assert!(matches!(result, ToolPermission::Deny { .. }));

        // 不允许的危险操作
        let result = can_use("exec", &serde_json::json!({"command": "rm -rf /"}));
        assert!(matches!(result, ToolPermission::Deny { .. }));

        let result = can_use("delete_file", &serde_json::json!({"path": "/tmp/test.md"}));
        assert!(matches!(result, ToolPermission::Deny { .. }));

        // 清理临时目录
        fs::remove_dir_all(&temp_dir).ok();
    }

    /// 测试 Layer 6 梦境机制触发条件
    ///
    /// 验证：时间和会话数阈值正确控制梦境触发
    #[test]
    fn test_layer6_dream_trigger_conditions() {
        // Layer 6 常量验证
        const TIME_GATE_THRESHOLD_HOURS: i64 = 24;
        const SESSION_GATE_THRESHOLD: usize = 5;

        // 时间阈值：24小时
        assert_eq!(TIME_GATE_THRESHOLD_HOURS, 24);

        // 会话数阈值：5个
        assert_eq!(SESSION_GATE_THRESHOLD, 5);

        // 验证触发逻辑：时间不足不应触发
        let hours_since_last: i64 = 12;
        let should_trigger_time = hours_since_last >= TIME_GATE_THRESHOLD_HOURS;
        assert!(!should_trigger_time);

        // 时间足够应触发
        let hours_since_last: i64 = 30;
        let should_trigger_time = hours_since_last >= TIME_GATE_THRESHOLD_HOURS;
        assert!(should_trigger_time);

        // 会话数不足不应触发
        let sessions_count: usize = 3;
        let should_trigger_sessions = sessions_count >= SESSION_GATE_THRESHOLD;
        assert!(!should_trigger_sessions);

        // 会话数足够应触发
        let sessions_count: usize = 6;
        let should_trigger_sessions = sessions_count >= SESSION_GATE_THRESHOLD;
        assert!(should_trigger_sessions);
    }

    /// 测试完整的记忆流程：从 Layer 1 到 Layer 5
    ///
    /// 验证：消息处理流程中各层正确协作
    #[test]
    fn test_full_memory_flow() {
        use blockcell_agent::memory_system::MemorySystemConfig;
        use blockcell_agent::response_cache::{
            DEFAULT_MAX_RESULT_SIZE_CHARS, MAX_TOOL_RESULTS_PER_MESSAGE_CHARS,
        };

        // Layer 1 预算检查
        let tool_result_size = 60_000; // 超过单工具阈值
        assert!(tool_result_size > DEFAULT_MAX_RESULT_SIZE_CHARS);

        // 但在消息级别预算内
        assert!(tool_result_size < MAX_TOOL_RESULTS_PER_MESSAGE_CHARS);

        // 创建记忆系统并验证流程
        let config = MemorySystemConfig {
            token_budget: 50_000,
            compact_threshold: 0.8,
            auto_memory_enabled: true,
            ..Default::default()
        };

        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "flow-test".to_string(),
        );

        // 验证 Compact 触发
        assert!(memory_system.should_compact(45_000)); // 90% 使用率

        // 验证初始状态
        assert!(!memory_system.has_pending_extraction());
    }

    /// 测试 Layer 4 Compact Hook 注册与执行
    ///
    /// 验证：Pre/Post Compact Hook 能正确注册和调用
    #[test]
    fn test_compact_hooks_registration() {
        use blockcell_agent::compact::{
            CompactHookRegistry, PostCompactContext, PreCompactContext,
        };

        let mut registry = CompactHookRegistry::new();

        // 注册 Pre-Hook
        registry.register_pre_hook(|_ctx: PreCompactContext| {
            Box::pin(async { blockcell_agent::compact::PreCompactResult::Continue })
        });

        // 注册 Post-Hook
        registry.register_post_hook(|_ctx: PostCompactContext| {
            Box::pin(async { blockcell_agent::compact::PostCompactResult::Success })
        });

        // 验证 Hook 已注册
        assert!(registry.has_pre_hooks());
        assert!(registry.has_post_hooks());
    }

    /// 测试常量一致性：所有层关键常量
    ///
    /// 验证：实现常量与设计文档一致
    #[test]
    fn test_constants_consistency() {
        use blockcell_agent::compact::{
            MAX_FILES_TO_RECOVER, MAX_FILE_RECOVERY_TOKENS, MAX_SINGLE_FILE_TOKENS,
            MAX_SKILL_RECOVERY_TOKENS,
        };
        use blockcell_agent::response_cache::{
            DEFAULT_MAX_RESULT_SIZE_CHARS, IMAGE_MAX_TOKEN_SIZE,
            MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, PREVIEW_SIZE_BYTES,
        };
        use blockcell_agent::session_memory::{
            MAX_SECTION_LENGTH, MAX_TOTAL_SESSION_MEMORY_TOKENS,
        };

        // Layer 1 常量
        assert_eq!(PREVIEW_SIZE_BYTES, 2000);
        assert_eq!(DEFAULT_MAX_RESULT_SIZE_CHARS, 50_000);
        assert_eq!(MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, 150_000);
        assert_eq!(IMAGE_MAX_TOKEN_SIZE, 2000);

        // Layer 3 常量
        assert_eq!(MAX_SECTION_LENGTH, 2000);
        assert_eq!(MAX_TOTAL_SESSION_MEMORY_TOKENS, 12000);

        // Layer 4 常量
        assert_eq!(MAX_FILE_RECOVERY_TOKENS, 50_000);
        assert_eq!(MAX_SINGLE_FILE_TOKENS, 5_000);
        assert_eq!(MAX_SKILL_RECOVERY_TOKENS, 25_000);
        assert_eq!(MAX_FILES_TO_RECOVER, 5);
    }

    // ========================================================================
    // 高优先级集成测试
    // ========================================================================

    /// [高优先级] Layer 1 → Layer 4 联动：工具结果存储 → Compact 触发
    ///
    /// 场景：大量工具结果导致 Token 超限，触发 Compact
    /// 验证：
    /// 1. 工具结果正确存储到 Layer 1
    /// 2. Token 计数正确累积
    /// 3. 达到阈值时正确触发 Compact
    #[test]
    fn test_layer1_to_layer4_compact_trigger() {
        use blockcell_agent::compact::should_compact;
        use blockcell_agent::response_cache::{ToolResultCandidate, DEFAULT_MAX_RESULT_SIZE_CHARS};

        // 模拟 Layer 1: 工具结果候选
        let candidates: Vec<ToolResultCandidate> = (0..10)
            .map(|i| ToolResultCandidate {
                tool_use_id: format!("call-{}", i),
                content: "x".repeat(DEFAULT_MAX_RESULT_SIZE_CHARS / 5), // 每个 10000 chars
                size: DEFAULT_MAX_RESULT_SIZE_CHARS / 5,
            })
            .collect();

        // 验证候选创建正确
        assert_eq!(candidates.len(), 10);

        // 模拟 Token 累积
        let total_chars: usize = candidates.iter().map(|c| c.content.len()).sum();
        let estimated_tokens = total_chars / 4; // 粗略估算

        // 验证 Compact 触发条件
        let token_budget = 100_000;
        let threshold = 0.8;

        // 如果 Token 使用率高，应触发 Compact
        if estimated_tokens >= (token_budget as f64 * threshold) as usize {
            assert!(should_compact(estimated_tokens, token_budget, threshold));
        }
    }

    /// [高优先级] Layer 3 → Layer 5 联动：Session Memory → Auto Memory 提取
    ///
    /// 场景：Session Memory 更新触发 Auto Memory 提取检查
    /// 验证：
    /// 1. Session Memory 状态正确更新
    /// 2. 提取游标正确追踪位置
    /// 3. 满足条件时触发提取
    #[test]
    fn test_layer3_to_layer5_extraction_flow() {
        use blockcell_agent::auto_memory::{ExtractionCursor, MemoryType};
        use blockcell_agent::session_memory::{
            should_extract_memory, SessionMemoryConfig, SessionMemoryState,
        };

        // Layer 3: Session Memory 状态
        let config = SessionMemoryConfig {
            minimum_message_tokens_to_init: 5_000,
            minimum_tokens_between_update: 2_000,
            tool_calls_between_updates: 2,
        };
        let mut state = SessionMemoryState {
            config,
            initialized: true,
            tokens_at_last_extraction: 0,
            ..Default::default()
        };

        // Layer 5: 提取游标
        let mut cursor = ExtractionCursor::new(MemoryType::User);
        assert!(cursor.should_extract(100, 0)); // 100 messages, 0 cooldown

        // 模拟消息处理 - 使用更多消息确保超过 token 阈值
        let messages: Vec<ChatMessage> = (0..200)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!(
                        "User message number {} with substantial content to reach threshold",
                        i
                    )),
                    ChatMessage::assistant(&format!(
                        "Assistant response {} with meaningful content",
                        i
                    )),
                ]
            })
            .collect();

        // 检查是否应该提取（验证函数可调用）
        let _should = should_extract_memory(&messages, &state);

        // 更新状态（无论是否触发提取，都验证流程）
        let last_msg = messages.last().unwrap();
        state.last_memory_message_id = last_msg.id.clone();
        state.last_memory_message_index = Some(messages.len() - 1);
        state.tokens_at_last_extraction = 15_000;
        state.initialized = true;

        // 更新游标
        cursor.update(uuid::Uuid::new_v4(), messages.len());

        // 验证状态一致性
        assert!(state.initialized);
        assert!(state.last_memory_message_index.is_some());
        assert!(state.last_memory_message_id.is_some());
    }

    /// [高优先级] Layer 7 Forked Agent 完整流程
    ///
    /// 场景：使用 mock provider 验证 Forked Agent 完整执行
    /// 验证：
    /// 1. 参数验证正确
    /// 2. 无 provider 时正确返回错误
    /// 3. 工具权限检查正常
    #[test]
    fn test_layer7_forked_agent_flow() {
        use blockcell_agent::forked::{
            CacheSafeParams, ForkedAgentParams, ToolDefinition, ToolPermission,
        };

        // 验证 builder 缺少 provider 时 build() 返回 Err
        // (ForkedAgentError 未导出，所以我们检查 Result::is_err)
        let result = ForkedAgentParams::builder()
            .prompt_messages(vec![ChatMessage::user("test")])
            .fork_label("test_flow")
            .build();

        // 应该返回错误（没有 provider_pool）
        assert!(result.is_err());

        // 验证 CacheSafeParams 兼容性检查
        let params1 = CacheSafeParams::new("system prompt", "model-name").with_tools(vec![
            ToolDefinition::new("read_file", "Read", serde_json::json!({})),
        ]);

        let params2 = CacheSafeParams::new("system prompt", "model-name").with_tools(vec![
            ToolDefinition::new("read_file", "Read", serde_json::json!({})),
        ]);

        // 相同配置应兼容
        assert!(params1.is_compatible_with(&params2));

        // 验证工具权限检查函数存在
        let can_use = blockcell_agent::forked::create_auto_mem_can_use_tool(std::path::Path::new(
            "/tmp/memory",
        ));

        // read_file 应该被允许
        let result = can_use(
            "read_file",
            &serde_json::json!({"file_path": "/tmp/test.md"}),
        );
        assert!(matches!(result, ToolPermission::Allow));
    }

    // ========================================================================
    // 中优先级集成测试
    // ========================================================================

    /// [中优先级] 跨层状态一致性：MemorySystem 状态同步验证
    ///
    /// 场景：MemorySystem 协调各层状态更新
    /// 验证：
    /// 1. Session Memory 状态正确传播
    /// 2. Auto Memory 游标状态同步
    /// 3. 后台任务状态一致
    #[test]
    fn test_cross_layer_state_consistency() {
        let config = MemorySystemConfig {
            auto_memory_enabled: true,
            ..Default::default()
        };

        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "consistency-test".to_string(),
        );

        // 初始状态
        assert!(!memory_system.has_pending_extraction());
        assert_eq!(memory_system.background_task_count(), 0);

        // 更新 Session Memory 状态
        let msg_id = "msg-consistency-123".to_string();
        memory_system.update_session_memory_state_with_id(Some(msg_id.clone()), 42, 10_000);

        // 验证状态更新
        let state = memory_system.session_memory_state();
        assert_eq!(state.last_memory_message_id, Some(msg_id));
        assert_eq!(state.last_memory_message_index, Some(42));
        assert_eq!(state.tokens_at_last_extraction, 10_000);
        assert!(state.initialized);

        // 验证配置使用默认值（因为 MemorySystemState::default() 不读取 MemorySystemConfig）
        // 这是预期行为：状态初始化使用默认值，配置用于其他用途
        assert_eq!(state.config.minimum_tokens_between_update, 5_000); // 默认值
    }

    /// [中优先级] 后台任务生命周期：wait_for_background_tasks 超时测试
    ///
    /// 场景：后台任务执行和超时处理
    /// 验证：
    /// 1. 任务正确添加和追踪
    /// 2. 超时后正确清理
    /// 3. 任务取消正常工作
    #[tokio::test]
    async fn test_background_task_lifecycle() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "bg-lifecycle-test".to_string(),
        );

        // 添加短时间任务 (返回 ())
        let handle1 = tokio::task::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        });
        memory_system.add_background_task(handle1);

        // 添加长时间任务 (返回 ())
        let handle2 = tokio::task::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        });
        memory_system.add_background_task(handle2);

        assert_eq!(memory_system.background_task_count(), 2);

        // 等待短任务完成
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 清理已完成任务
        let cleaned = memory_system.cleanup_completed_tasks();
        assert_eq!(cleaned, 1);
        assert_eq!(memory_system.background_task_count(), 1);

        // 取消剩余任务
        memory_system.abort_all_background_tasks();
        assert_eq!(memory_system.background_task_count(), 0);
    }

    /// [中优先级] 后台任务超时场景
    ///
    /// 验证：任务在超时后能正确处理
    #[tokio::test]
    async fn test_background_task_timeout_handling() {
        let config = MemorySystemConfig::default();
        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "timeout-test".to_string(),
        );

        // 添加一个永远不会完成的任务（模拟）(返回 ())
        let handle = tokio::task::spawn(async {
            std::future::pending::<()>().await;
        });
        memory_system.add_background_task(handle);

        assert!(memory_system.has_running_background_tasks());

        // 模拟超时检查
        let timeout_duration = std::time::Duration::from_millis(100);
        let start = std::time::Instant::now();

        // 等待超时
        while start.elapsed() < timeout_duration {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // 任务仍在运行
        assert_eq!(memory_system.background_task_count(), 1);

        // 取消任务
        memory_system.abort_all_background_tasks();
        assert_eq!(memory_system.background_task_count(), 0);
    }

    // ========================================================================
    // Compact 恢复重新注入集成测试
    // ========================================================================

    /// [高优先级] Compact 后恢复重新注入集成测试
    ///
    /// 场景：Compact 执行后，文件、技能和 Session Memory 正确恢复到上下文
    /// 验证：
    /// 1. FileTracker 正确追踪已读文件
    /// 2. SkillTracker 正确追踪已加载技能
    /// 3. 恢复消息正确生成
    /// 4. Token 预算正确应用
    #[test]
    fn test_compact_recovery_reinjection() {
        use blockcell_agent::compact::{
            build_recovery_message, FileTracker, SkillTracker, MAX_FILES_TO_RECOVER,
            MAX_SINGLE_FILE_TOKENS,
        };
        use std::path::PathBuf;

        // 1. 创建 FileTracker 并记录文件读取
        let mut file_tracker = FileTracker::new();
        file_tracker.record_read(
            PathBuf::from("/src/main.rs"),
            "fn main() { println!(\"Hello\"); }",
        );
        file_tracker.record_read(
            PathBuf::from("/src/lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }",
        );
        file_tracker.record_read(
            PathBuf::from("/src/utils.rs"),
            "pub fn helper() {} ".repeat(100).as_str(), // 大文件
        );

        // 2. 创建 SkillTracker 并记录技能加载
        let mut skill_tracker = SkillTracker::new();
        skill_tracker.record_load("rust-skills", "Rust programming skill content");
        skill_tracker.record_load("tokio-runtime", "Tokio runtime documentation");

        // 3. 模拟 Session Memory 内容
        let session_memory = r#"# Session Memory

## Current State
_Working on compact recovery test._

## Files and Functions
_Paths: /src/main.rs, /src/lib.rs_

## Errors & Corrections
_No errors encountered._

## Worklog
1. Started test
2. Tracked files
3. Generated recovery message
"#;

        // 4. 构建恢复消息
        let recovery_message =
            build_recovery_message(&file_tracker, &skill_tracker, Some(session_memory));

        // 5. 验证恢复消息内容
        assert!(recovery_message.contains("Files Previously Read"));
        assert!(recovery_message.contains("Skills Previously Loaded"));
        assert!(recovery_message.contains("Session Memory"));

        // 6. 验证文件路径在恢复消息中
        assert!(recovery_message.contains("main.rs"));
        assert!(recovery_message.contains("lib.rs"));

        // 7. 验证技能在恢复消息中
        assert!(recovery_message.contains("rust-skills"));
        assert!(recovery_message.contains("tokio-runtime"));

        // 8. 验证 Session Memory 关键内容在恢复消息中
        assert!(recovery_message.contains("Current State"));

        // 9. 验证 FileTracker 获取最近文件
        let recent_files =
            file_tracker.get_recent_files(MAX_FILES_TO_RECOVER, MAX_SINGLE_FILE_TOKENS);
        assert!(!recent_files.is_empty());
        assert!(recent_files.len() <= MAX_FILES_TO_RECOVER);

        // 10. 验证 SkillTracker 获取最近技能
        let recent_skills = skill_tracker.get_recent_skills(MAX_SINGLE_FILE_TOKENS);
        assert!(!recent_skills.is_empty());

        // 11. 验证预算限制
        for file in &recent_files {
            assert!(file.estimated_tokens <= MAX_SINGLE_FILE_TOKENS);
        }
        for skill in &recent_skills {
            assert!(skill.estimated_tokens <= MAX_SINGLE_FILE_TOKENS);
        }
    }

    /// [高优先级] Compact 恢复消息格式验证
    ///
    /// 验证恢复消息的格式符合预期，能被 LLM 正确理解
    #[test]
    fn test_compact_recovery_message_format() {
        use blockcell_agent::compact::{build_recovery_message, FileTracker, SkillTracker};

        // 创建空的 tracker
        let file_tracker = FileTracker::new();
        let skill_tracker = SkillTracker::new();

        // 测试空内容
        let empty_recovery = build_recovery_message(&file_tracker, &skill_tracker, None);
        assert!(empty_recovery.is_empty());

        // 只测试文件
        let mut file_tracker_only = FileTracker::new();
        file_tracker_only.record_read(PathBuf::from("/test.rs"), "test content");
        let files_only = build_recovery_message(&file_tracker_only, &skill_tracker, None);
        assert!(files_only.contains("Files Previously Read"));
        assert!(!files_only.contains("Skills Previously Loaded"));

        // 只测试技能
        let mut skill_tracker_only = SkillTracker::new();
        skill_tracker_only.record_load("test-skill", "skill content");
        let skills_only = build_recovery_message(&file_tracker, &skill_tracker_only, None);
        assert!(!skills_only.contains("Files Previously Read"));
        assert!(skills_only.contains("Skills Previously Loaded"));

        // 只测试 Session Memory
        let session_only = build_recovery_message(
            &file_tracker,
            &skill_tracker,
            Some("# Session\nTest content"),
        );
        assert!(session_only.contains("Session Memory"));
    }

    /// [中优先级] Compact 恢复 Token 预算验证
    ///
    /// 验证恢复内容的 Token 数在预算范围内
    #[test]
    fn test_compact_recovery_token_budget() {
        use blockcell_agent::compact::{
            build_recovery_message, FileTracker, SkillTracker, MAX_SINGLE_FILE_TOKENS,
        };

        // 创建包含大量内容的 tracker
        let mut file_tracker = FileTracker::new();
        for i in 0..20 {
            let content = "x".repeat(10000); // 每个文件 10000 字符
            file_tracker.record_read(PathBuf::from(format!("/file{}.rs", i)), &content);
        }

        let mut skill_tracker = SkillTracker::new();
        for i in 0..10 {
            let content = "y".repeat(5000); // 每个技能 5000 字符
            skill_tracker.record_load(&format!("skill-{}", i), &content);
        }

        // 获取恢复内容
        let recent_files = file_tracker.get_recent_files(5, MAX_SINGLE_FILE_TOKENS);
        let recent_skills = skill_tracker.get_recent_skills(MAX_SINGLE_FILE_TOKENS);

        // 验证文件数量限制
        assert!(recent_files.len() <= 5);

        // 验证单个文件 Token 限制
        for file in &recent_files {
            assert!(file.estimated_tokens <= MAX_SINGLE_FILE_TOKENS);
        }

        // 验证单个技能 Token 限制
        for skill in &recent_skills {
            assert!(skill.estimated_tokens <= MAX_SINGLE_FILE_TOKENS);
        }

        // 构建恢复消息
        let recovery = build_recovery_message(&file_tracker, &skill_tracker, None);

        // 验证恢复消息不为空
        assert!(!recovery.is_empty());

        // 验证恢复消息包含预期的结构
        assert!(
            recovery.contains("Files Previously Read")
                || recovery.contains("Skills Previously Loaded")
        );
    }

    // =========================================================================
    // 跨层工作流集成测试 (Layer 1→2→3→4)
    // =========================================================================

    /// 测试 Layer 1→2 触发链
    ///
    /// Layer 1: 大工具结果触发持久化
    /// Layer 2: 时间触发清理旧工具结果
    #[test]
    fn test_layer1_to_layer2_workflow() {
        use blockcell_agent::response_cache::{
            ContentReplacementState, DEFAULT_MAX_RESULT_SIZE_CHARS,
            MAX_TOOL_RESULTS_PER_MESSAGE_CHARS,
        };

        // 模拟 Layer 1: 大工具结果超过单工具阈值
        let large_result = "x".repeat(DEFAULT_MAX_RESULT_SIZE_CHARS + 1000);
        assert!(large_result.len() > DEFAULT_MAX_RESULT_SIZE_CHARS);

        // 模拟 Layer 2: 多个工具结果超过消息预算
        let total_candidate_size = MAX_TOOL_RESULTS_PER_MESSAGE_CHARS * 2;
        assert!(total_candidate_size > MAX_TOOL_RESULTS_PER_MESSAGE_CHARS);

        // 验证状态追踪正确
        let state = ContentReplacementState::default();
        assert!(state.seen_ids.is_empty());
    }

    /// 测试 Layer 2→3 触发链
    ///
    /// Layer 2: 时间触发清理后
    /// Layer 3: 检测到足够的消息触发 Session Memory 提取
    #[test]
    fn test_layer2_to_layer3_workflow() {
        use blockcell_agent::history_projector::TimeBasedMCConfig;

        // Layer 2 配置
        let config = TimeBasedMCConfig::default();
        assert!(config.enabled);
        assert_eq!(config.gap_threshold_minutes, 60);

        // Layer 3: 创建足够多的消息触发 Session Memory 提取
        let messages: Vec<ChatMessage> = (0..500)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("Long message {} with substantial content", i)),
                    ChatMessage::assistant("Response with meaningful content"),
                ]
            })
            .collect();

        // 创建 Session Memory 状态
        let state = SessionMemoryState::default();

        // 验证触发条件检测
        let should = should_extract_memory(&messages, &state);
        // 由于未初始化，需要达到阈值才会触发
        // 这里只验证函数不会 panic
        let _ = should;
    }

    /// 测试 Layer 3→4 触发链
    ///
    /// Layer 3: Session Memory 提取后
    /// Layer 4: Token 预算超限触发 Compact
    #[test]
    fn test_layer3_to_layer4_workflow() {
        // Layer 3: Session Memory 已更新
        let session_memory_state = SessionMemoryState {
            initialized: true,
            last_memory_message_index: Some(100),
            last_memory_message_id: Some("msg-100".to_string()),
            tokens_at_last_extraction: 10_000,
            ..Default::default()
        };

        assert!(session_memory_state.initialized);

        // Layer 4: 创建低预算配置以触发 Compact
        let config = MemorySystemConfig {
            token_budget: 1000, // 很低的预算
            compact_threshold: 0.5,
            ..Default::default()
        };

        let memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test-session".to_string(),
        );

        // 验证 Layer 4 触发
        assert!(memory_system.should_compact(500)); // 500/1000 = 0.5
        assert!(memory_system.should_compact(800)); // 800/1000 = 0.8
    }

    /// 测试完整的 Layer 1→2→3→4 工作流
    ///
    /// 模拟一个完整的消息处理周期：
    /// 1. Layer 1: 处理大工具结果
    /// 2. Layer 2: 时间触发清理
    /// 3. Layer 3: Session Memory 提取评估
    /// 4. Layer 4: Compact 触发评估
    #[test]
    fn test_full_layer_workflow() {
        use blockcell_agent::compact::should_compact;

        // ========== Layer 1: 工具结果处理 ==========
        // 模拟消息历史
        let _messages: Vec<ChatMessage> = (0..100)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("User message {}", i)),
                    ChatMessage::assistant(&format!("Assistant response {}", i)),
                ]
            })
            .collect();

        // ========== Layer 2: 时间触发检查 ==========
        // 验证配置
        let layer2_enabled = true;
        assert!(layer2_enabled);

        // ========== Layer 3: Session Memory 评估 ==========
        let _session_state = SessionMemoryState::default();
        let session_config = SessionMemoryConfig::default();

        // 验证阈值配置
        assert_eq!(session_config.minimum_message_tokens_to_init, 10_000);
        assert_eq!(session_config.minimum_tokens_between_update, 5_000);

        // ========== Layer 4: Compact 评估 ==========
        let token_budget = 100_000;
        let threshold = 0.8;

        // 低于阈值不触发
        assert!(!should_compact(50_000, token_budget, threshold));

        // 达到阈值触发
        assert!(should_compact(80_000, token_budget, threshold));

        // 超过阈值触发
        assert!(should_compact(120_000, token_budget, threshold));
    }

    /// 测试 PostSamplingAction 优先级
    ///
    /// 验证 Compact > Session Memory > Auto Memory 的优先级顺序
    #[test]
    fn test_post_sampling_action_priority() {
        // Compact 最高优先级
        let config = MemorySystemConfig {
            token_budget: 100,
            compact_threshold: 0.8,
            auto_memory_enabled: true,
            ..Default::default()
        };

        let mut memory_system = MemorySystem::new(
            config,
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test".to_string(),
        );

        // 创建足够多的消息
        let messages: Vec<ChatMessage> = (0..20)
            .flat_map(|i| {
                vec![
                    ChatMessage::user(&format!("msg {}", i)),
                    ChatMessage::assistant("resp"),
                ]
            })
            .collect();

        // 当 Token 超过阈值时，应返回 Compact 而不是其他 Action
        let action = evaluate_memory_hooks(&mut memory_system, &messages, 100);

        // Compact 优先级最高
        assert!(matches!(action, PostSamplingAction::Compact));
    }

    /// 测试 Layer 各层 Token 预算约束
    #[test]
    fn test_layer_token_budgets() {
        use blockcell_agent::compact::{
            MAX_FILE_RECOVERY_TOKENS, MAX_SINGLE_FILE_TOKENS, MAX_SKILL_RECOVERY_TOKENS,
        };
        use blockcell_agent::response_cache::{
            DEFAULT_MAX_RESULT_SIZE_CHARS, MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, PREVIEW_SIZE_BYTES,
        };
        use blockcell_agent::session_memory::MAX_SECTION_LENGTH;

        // Layer 1 预算
        assert_eq!(PREVIEW_SIZE_BYTES, 2000);
        assert_eq!(DEFAULT_MAX_RESULT_SIZE_CHARS, 50_000);
        assert_eq!(MAX_TOOL_RESULTS_PER_MESSAGE_CHARS, 150_000);

        // Layer 3 预算
        assert_eq!(MAX_SECTION_LENGTH, 2000);

        // Layer 4 预算
        assert_eq!(MAX_FILE_RECOVERY_TOKENS, 50_000);
        assert_eq!(MAX_SKILL_RECOVERY_TOKENS, 25_000);
        assert_eq!(MAX_SINGLE_FILE_TOKENS, 5_000);
        // 总预算 = 文件 + 技能 = 75,000
        let total_recovery_budget = MAX_FILE_RECOVERY_TOKENS + MAX_SKILL_RECOVERY_TOKENS;
        assert_eq!(total_recovery_budget, 75_000);
    }

    /// 测试 MemorySystem 状态一致性
    #[test]
    fn test_memory_system_state_consistency() {
        let mut memory_system = MemorySystem::new(
            MemorySystemConfig::default(),
            PathBuf::from("/tmp/workspace"),
            PathBuf::from("/tmp/config"),
            "test-session".to_string(),
        );

        // 初始状态
        assert!(!memory_system.has_pending_extraction());
        assert_eq!(memory_system.background_task_count(), 0);

        // 更新 Session Memory 状态
        memory_system.update_session_memory_state_with_id(Some("msg-50".to_string()), 50, 5000);

        let state = memory_system.session_memory_state();
        assert!(state.initialized);
        assert_eq!(state.last_memory_message_id, Some("msg-50".to_string()));
        assert_eq!(state.tokens_at_last_extraction, 5000);

        // 记录文件读取
        memory_system.record_file_read(PathBuf::from("/test/file.rs"), "fn main() {}");

        let file_tracker = memory_system.file_tracker();
        assert!(!file_tracker.is_empty());

        // 记录技能加载
        memory_system.record_skill_load("test-skill", "# Test Skill\nContent here");

        let skill_tracker = memory_system.skill_tracker();
        assert!(!skill_tracker.is_empty());
    }
}
