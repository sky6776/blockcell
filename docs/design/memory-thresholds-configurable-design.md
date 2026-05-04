# 记忆系统阈值可配置化设计文档

> 版本: v1.1
> 日期: 2026-04-30
> 状态: Draft
> 关联 Issue: 记忆系统各项压缩阈值从硬编码改为可配置

---

## 1. 背景与动机

BlockCell 的 7 层记忆系统当前所有压缩/触发阈值均为硬编码常量或 `Default` 实现，用户无法根据自身场景（模型上下文窗口大小、对话风格、硬件资源）进行调整。

**问题**:
- 小模型（如 8K 上下文）使用默认 100K token 预算毫无意义
- 大模型（如 200K 上下文）可能希望延迟 compact 以保留更多历史
- 不同使用场景对记忆提取频率、恢复预算的需求差异巨大
- 调优需要修改源码重新编译，门槛极高

**目标**:
- 所有阈值均可通过 `config.json5` 配置
- 配置缺失时使用当前默认值（零破坏性变更）
- 配置值有合理范围校验，防止误配导致系统崩溃
- 各层阈值独立配置，互不干扰

---

## 2. 7 层记忆系统阈值清单

### 2.1 全景图

```
┌─────────────────────────────────────────────────────────────────┐
│                    7-Layer Memory System                        │
├──────────┬──────────────────────────────┬──────────────────────┤
│  Layer   │  职责                         │  关键阈值            │
├──────────┼──────────────────────────────┼──────────────────────┤
│  L1      │  工具结果持久化 + 预算控制    │  6 个阈值            │
│  L2      │  时间触发 MicroCompact       │  3 个阈值            │
│  L3      │  Session Memory 提取         │  7 个阈值            │
│  L4      │  Full Compact + 恢复         │  7 个阈值            │
│  L5      │  Auto Memory 提取 + 注入     │  4 个阈值            │
│  L6      │  SQLite 持久化存储           │  (由 storage 层管理) │
│  L7      │  Dream Service 后台整理      │  (由 scheduler 管理) │
├──────────┼──────────────────────────────┼──────────────────────┤
│  全局    │  MemorySystemConfig          │  3 个阈值            │
└──────────┴──────────────────────────────┴──────────────────────┘
```

### 2.2 各层阈值详细分析

---

#### Layer 1: 工具结果持久化 + 预算控制

**源文件**: `crates/agent/src/response_cache.rs`

| 阈值名 | 当前值 | 类型 | 说明 | 合理范围 |
|--------|--------|------|------|----------|
| `max_result_size_chars` | 50,000 | usize | 单个工具结果持久化阈值（字符数），超过则写入磁盘 | 10,000 ~ 500,000 |
| `max_tool_results_per_message_chars` | 150,000 | usize | 单条消息中所有工具结果总大小上限 | 50,000 ~ 1,000,000 |
| `preview_size_bytes` | 2,000 | usize | 持久化后预览大小（字节） | 500 ~ 10,000 |
| `max_replacement_entries` | 1,000 | usize | 内容替换状态最大条目数（LRU 淘汰） | 100 ~ 10,000 |
| `cache_max_per_session` | 10 | usize | ResponseCache 每会话最大缓存条目 | 5 ~ 100 |
| `cache_min_items` | 5 | usize | ResponseCache 最小列表项数才触发缓存 | 3 ~ 20 |

**阈值分配逻辑**:
- `max_result_size_chars`: 50K 字符 ≈ 12.5K tokens，对于典型代码文件输出合理；小模型可降至 20K，大模型可升至 100K
- `max_tool_results_per_message_chars`: 150K ≈ 37.5K tokens，约为典型 128K 上下文窗口的 30%；与 `max_result_size_chars` 保持 3:1 比例
- `preview_size_bytes`: 2KB 预览足够展示文件头/错误信息开头
- `max_replacement_entries`: 1000 条足以覆盖长会话，过多浪费内存
- `cache_max_per_session`: 10 条缓存覆盖常见场景
- `cache_min_items`: 5 项以下不值得缓存替换

---

#### Layer 2: 时间触发 MicroCompact

**源文件**: `crates/agent/src/history_projector.rs`

| 阈值名 | 当前值 | 类型 | 说明 | 合理范围 |
|--------|--------|------|------|----------|
| `enabled` | true | bool | 是否启用时间触发 | — |
| `gap_threshold_minutes` | 60 | u32 | 对话间歇阈值（分钟），超过则触发清理 | 10 ~ 480 |
| `keep_recent` | 5 | u32 | 保留最近 N 个工具结果 | 1 ~ 20 |

**阈值分配逻辑**:
- `gap_threshold_minutes`: 60 分钟适合交互式对话；自动化/批处理场景可降至 10-15 分钟
- `keep_recent`: 5 个保留最近操作上下文；代码重构等密集操作场景可升至 10

---

#### Layer 3: Session Memory 提取

**源文件**: `crates/agent/src/session_memory/extractor.rs`, `crates/agent/src/session_memory/mod.rs`

| 阈值名 | 当前值 | 类型 | 说明 | 合理范围 |
|--------|--------|------|------|----------|
| `minimum_message_tokens_to_init` | 10,000 | usize | 首次提取的 Token 门槛 | 2,000 ~ 50,000 |
| `minimum_tokens_between_update` | 5,000 | usize | 两次提取间的 Token 增量门槛 | 1,000 ~ 20,000 |
| `tool_calls_between_updates` | 3 | usize | 两次提取间的工具调用次数门槛 | 1 ~ 10 |
| `extraction_wait_timeout_ms` | 15,000 | u64 | 提取等待超时（毫秒） | 5,000 ~ 60,000 |
| `extraction_stale_threshold_ms` | 60,000 | u64 | 提取结果过期阈值（毫秒） | 30,000 ~ 300,000 |
| `max_section_length` | 2,000 | usize | 单个 Section 最大长度（字符） | 500 ~ 5,000 |
| `max_total_session_memory_tokens` | 12,000 | usize | Session Memory 总 Token 上限 | 4,000 ~ 30,000 |

**阈值分配逻辑**:
- `minimum_message_tokens_to_init`: 10K tokens 约为 3-5 轮对话，确保有足够内容提取；短对话场景可降至 5K
- `minimum_tokens_between_update`: 5K 约为 1-2 轮增量，避免频繁提取浪费 LLM 调用
- `tool_calls_between_updates`: 3 次工具调用作为辅助触发条件
- `extraction_wait_timeout_ms`: 15 秒足够 Forked Agent 完成提取
- `extraction_stale_threshold_ms`: 60 秒内结果有效，超过需重新提取
- `max_section_length`: 2K 字符 ≈ 500 tokens，10 个 section 总计 5K tokens
- `max_total_session_memory_tokens`: 12K tokens 约占 128K 上下文的 9%，合理开销

---

#### Layer 4: Full Compact + 恢复

**源文件**: `crates/agent/src/compact/mod.rs`, `crates/agent/src/compact/file_tracker.rs`, `crates/agent/src/compact/skill_tracker.rs`

| 阈值名 | 当前值 | 类型 | 说明 | 合理范围 |
|--------|--------|------|------|----------|
| `token_threshold` | 100,000 | usize | Compact 触发 Token 阈值 | 20,000 ~ 500,000 |
| `threshold_ratio` | 0.8 | f64 | Compact 触发比例（threshold_ratio × token_budget） | 0.5 ~ 0.95 |
| `keep_recent_messages` | 2 | usize | Compact 后保留最近消息数 | 1 ~ 10 |
| `max_output_tokens` | 12,000 | usize | Compact 摘要最大输出 tokens | 4,000 ~ 32,000 |
| `max_file_recovery_tokens` | 50,000 | usize | 文件恢复总 Token 预算 | 10,000 ~ 200,000 |
| `max_single_file_tokens` | 5,000 | usize | 单文件恢复 Token 上限 | 1,000 ~ 20,000 |
| `max_files_to_recover` | 5 | usize | 最大恢复文件数 | 1 ~ 20 |
| `max_skill_recovery_tokens` | 25,000 | usize | 技能恢复 Token 预算 | 5,000 ~ 100,000 |
| `max_session_memory_recovery_tokens` | 12,000 | usize | Session Memory 恢复 Token 预算 | 4,000 ~ 30,000 |
| `tracker_summary_chars` | 2,000 | usize | FileTracker/SkillTracker 摘要最大字符数 | 500 ~ 5,000 |

**阈值分配逻辑与预算约束**:

Compact 恢复总预算 = 文件 + 技能 + Session Memory = 50K + 25K + 12K = 87K tokens

这个分配遵循 **5:2.5:1.2** 的比例，原因如下：
- **文件 (50K, 57%)**: 文件内容是 Agent 执行任务的核心上下文，丢失后无法重建，分配最大预算
- **技能 (25K, 29%)**: 技能定义是 Agent 能力基础，但通常比文件短，分配中等预算
- **Session Memory (12K, 14%)**: 会话摘要已有 Compact 摘要覆盖，Session Memory 是补充，分配最小预算

**单文件上限 5K tokens 的约束**:
- 5 个文件 × 5K = 25K ≤ 50K 总预算，留有 25K 余量给大文件
- 单文件 5K ≈ 200 行代码，覆盖大多数源文件的核心部分

**与 token_budget 的关系**:
- 恢复总预算 87K 不应超过 token_budget 的 90%
- 默认 token_budget = 100K，恢复占比 87%，留 13K 给系统提示和新消息
- 若用户配置 token_budget = 200K，可按比例放大恢复预算

---

#### Layer 5: Auto Memory 提取 + 注入

**源文件**: `crates/agent/src/auto_memory/mod.rs`, `crates/agent/src/auto_memory/injector.rs`

| 阈值名 | 当前值 | 类型 | 说明 | 合理范围 |
|--------|--------|------|------|----------|
| `min_messages_for_extraction` | 15 | usize | 触发提取的最小消息数 | 5 ~ 50 |
| `extraction_cooldown_messages` | 5 | usize | 两次提取间的消息冷却数 | 2 ~ 20 |
| `max_memory_file_tokens` | 4,000 | usize | 单个记忆文件最大 Token 数 | 1,000 ~ 10,000 |
| `injection_max_tokens` | 4,000 | usize | 注入系统提示的最大 Token 预算 | 1,000 ~ 10,000 |

**阈值分配逻辑**:
- `min_messages_for_extraction`: 15 条消息 ≈ 7-8 轮对话，确保有足够素材提取用户偏好/项目信息
- `extraction_cooldown_messages`: 5 条消息冷却避免频繁 LLM 调用
- `max_memory_file_tokens`: 4K tokens 每个记忆文件，4 种类型总计 16K tokens 磁盘占用
- `injection_max_tokens`: 4K tokens 注入预算，4 种类型按优先级分配：User > Feedback > Project > Reference

**注入预算分配策略**:
```
总预算: 4,000 tokens
├── User (优先级1): 最多 1,500 tokens (37.5%)
├── Feedback (优先级2): 最多 1,000 tokens (25%)
├── Project (优先级3): 最多 1,000 tokens (25%)
└── Reference (优先级4): 最多 500 tokens (12.5%)
```
实际分配是贪心填充：按优先级顺序依次填入，直到总预算耗尽。

---

#### 全局: MemorySystemConfig

**源文件**: `crates/agent/src/memory_system/mod.rs`

| 阈值名 | 当前值 | 类型 | 说明 | 合理范围 |
|--------|--------|------|------|----------|
| `auto_memory_enabled` | true | bool | 是否启用自动记忆提取 | — |
| `compact_enabled` | true | bool | 是否启用 Compact | — |
| `compact_threshold` | 0.8 | f64 | Compact 触发比例 | 0.5 ~ 0.95 |
| `token_budget` | 100,000 | usize | Token 预算 | 20,000 ~ 500,000 |

> 注意: `compact_threshold` 与 Layer 4 的 `threshold_ratio` 语义相同，合并为一个配置项。

---

### 2.3 参数影响分析

每个参数的详细作用、过高/过低的影响、以及与其他参数的依赖关系。

#### 全局参数

| 参数 | 作用 | 过高影响 | 过低影响 | 依赖关系 |
|------|------|----------|----------|----------|
| `tokenBudget` | 整个记忆系统的 Token 预算上限，是所有恢复预算的"天花板" | 消耗大量内存；Compact 触发过晚导致上下文溢出 | 频繁 Compact，对话历史几乎无法保留 | 所有 Layer4 恢复预算之和不应超过此值的 95% |
| `autoMemoryEnabled` | L5 Auto Memory 总开关 | — | 跨会话记忆完全失效，每次新会话从零开始 | 控制 L5 全部行为 |
| `compactEnabled` | L4 Full Compact 总开关 | — | 对话历史无限增长，最终超出模型上下文窗口导致 API 错误 | 控制 L4 全部行为 |

#### Layer 1 参数

| 参数 | 作用 | 过高影响 | 过低影响 | 依赖关系 |
|------|------|----------|----------|----------|
| `maxResultSizeChars` | 单个工具结果超过此字符数则持久化到磁盘，否则保留在内存 | 大量内容留在内存中，历史消息 token 膨胀 | 频繁磁盘 I/O，小结果也被持久化，增加延迟 | 必须 ≤ `maxToolResultsPerMessageChars` |
| `maxToolResultsPerMessageChars` | 单轮对话中所有工具结果的总字符上限，超限则按大小降序持久化最大的结果 | 上下文窗口被工具结果占满，挤占用户/助手消息空间 | 过多结果被持久化，LLM 只能看到预览，丢失细节 | 应为 `maxResultSizeChars` 的 2~4 倍 |
| `previewSizeBytes` | 持久化后在历史中保留的预览字节数 | 预览过长，未达到压缩目的 | 预览太短，LLM 无法判断是否需要 `session_recall` | 独立参数 |
| `maxReplacementEntries` | 内容替换状态的最大条目数（LRU 淘汰旧条目） | 内存占用增大；HashMap 查找变慢 | 长会话中旧条目被淘汰，Prompt Cache 前缀可能失效 | 独立参数 |
| `cacheMaxPerSession` | ResponseCache 每会话最大缓存条目数 | 内存占用增大 | 缓存命中率下降，LLM 需要更频繁调用 `session_recall` | 独立参数 |
| `cacheMinItems` | 列表项数少于此值不触发缓存替换 | 几乎所有列表都被缓存，浪费缓存空间 | 短列表不被缓存，但短列表本身不占多少 token | 独立参数 |

#### Layer 2 参数

| 参数 | 作用 | 过高影响 | 过低影响 | 依赖关系 |
|------|------|----------|----------|----------|
| `enabled` | 是否启用时间触发 MicroCompact | — | 旧工具结果永远不被清理，历史持续膨胀 | 独立开关 |
| `gapThresholdMinutes` | 对话间歇超过此分钟数才触发清理 | 几乎不触发，旧结果长期驻留内存 | 用户短暂离开（如接电话）就触发清理，可能丢失有用上下文 | 独立参数 |
| `keepRecent` | 清理时保留最近 N 个工具结果不清理 | 保留过多，清理效果不明显 | 保留过少，LLM 可能丢失刚用过的工具结果上下文 | 独立参数 |

#### Layer 3 参数

| 参数 | 作用 | 过高影响 | 过低影响 | 依赖关系 |
|------|------|----------|----------|----------|
| `minimumMessageTokensToInit` | 首次 Session Memory 提取的 Token 门槛 | 首次提取延迟过长，早期对话信息未被捕获 | 提取过早，内容太少导致提取质量差，浪费 LLM 调用 | 应为 `minimumTokensBetweenUpdate` 的 2~3 倍 |
| `minimumTokensBetweenUpdate` | 两次增量提取间的 Token 增量门槛 | 增量提取间隔过长，新信息未被及时捕获 | 频繁调用 LLM 提取，增加延迟和 API 成本 | 应 ≤ `minimumMessageTokensToInit` |
| `toolCallsBetweenUpdates` | 两次提取间的工具调用次数门槛（辅助触发） | 纯工具调用场景下提取不及时 | 过于敏感，每次少量工具调用就触发提取 | 与 `minimumTokensBetweenUpdate` 互补 |
| `extractionWaitTimeoutMs` | Forked Agent 提取等待超时 | 主 Agent 长时间阻塞等待 | 提取未完成就放弃，浪费 LLM 调用 | 应 < `extractionStaleThresholdMs` |
| `extractionStaleThresholdMs` | 提取结果过期阈值，超过需重新提取 | 使用过时的 Session Memory，可能包含已不相关的信息 | 频繁重新提取，增加 LLM 调用 | 应 > `extractionWaitTimeoutMs` |
| `maxSectionLength` | 单个 Section 的最大字符长度 | Section 过长，总 token 超限 | Section 被过度截断，丢失关键信息 | 间接影响 `maxTotalSessionMemoryTokens` |
| `maxTotalSessionMemoryTokens` | Session Memory 总 Token 上限 | 占用过多上下文空间 | Session Memory 信息不足，Compact 后恢复丢失重要上下文 | 应 = L4 的 `maxSessionMemoryRecoveryTokens` |

#### Layer 4 参数

| 参数 | 作用 | 过高影响 | 过低影响 | 依赖关系 |
|------|------|----------|----------|----------|
| `compactThresholdRatio` | Compact 触发比例：当 token 使用量 ≥ ratio × tokenBudget 时触发 | Compact 触发过晚，可能超出模型上下文窗口 | Compact 触发过早，频繁压缩浪费 LLM 调用且丢失历史 | 与 `tokenBudget` 联合决定触发时机 |
| `keepRecentMessages` | Compact 后保留最近 N 条消息不压缩 | 保留过多，压缩效果差 | 保留过少，LLM 丢失当前任务上下文 | 独立参数 |
| `maxOutputTokens` | Compact 摘要的最大输出 Token 数 | 摘要过长，占用恢复预算 | 摘要过短，关键信息丢失 | 独立参数 |
| `maxFileRecoveryTokens` | Post-Compact 文件恢复的总 Token 预算 | 占用过多上下文，挤占新消息空间 | 文件恢复不足，Agent 丢失任务关键文件内容 | 与 `maxSkillRecoveryTokens` + `maxSessionMemoryRecoveryTokens` 之和不应超过 `tokenBudget` 的 95% |
| `maxSingleFileTokens` | 单个文件恢复的 Token 上限 | 单文件占用过多预算，其他文件无法恢复 | 大文件核心部分被截断 | `maxSingleFileTokens` × `maxFilesToRecover` 应 ≤ `maxFileRecoveryTokens` |
| `maxFilesToRecover` | 最大恢复文件数 | 恢复过多文件，总 token 超限 | 重要文件可能被排除 | 同上约束 |
| `maxSkillRecoveryTokens` | 技能恢复的 Token 预算 | 占用过多上下文 | Agent 丢失技能定义，无法调用技能 | 与文件/Session Memory 预算之和约束 |
| `maxSessionMemoryRecoveryTokens` | Session Memory 恢复的 Token 预算 | 占用过多上下文 | Compact 后 Session Memory 丢失 | 应 = L3 的 `maxTotalSessionMemoryTokens` |
| `trackerSummaryChars` | FileTracker/SkillTracker 中每个记录的摘要最大字符数 | 摘要过长，恢复时注入过多内容 | 摘要过短，LLM 无法判断文件/技能是否相关 | 独立参数 |

#### Layer 5 参数

| 参数 | 作用 | 过高影响 | 过低影响 | 依赖关系 |
|------|------|----------|----------|----------|
| `minMessagesForExtraction` | 触发 Auto Memory 提取的最小消息数 | 首次提取延迟，早期用户偏好未被捕获 | 提取过早，内容不足导致提取质量差 | 应 > `extractionCooldownMessages` |
| `extractionCooldownMessages` | 两次提取间的消息冷却数 | 新信息捕获延迟 | 频繁 LLM 调用，增加成本和延迟 | 应 < `minMessagesForExtraction` |
| `maxMemoryFileTokens` | 单个记忆文件（User/Feedback/Project/Reference）的最大 Token 数 | 记忆文件过大，注入时占用过多上下文 | 记忆信息被过度截断，丢失重要偏好/项目信息 | 应 ≥ `injectionMaxTokens` |
| `injectionMaxTokens` | 注入系统提示的最大 Token 预算 | 占用过多系统提示空间，挤占其他指令 | 注入的记忆信息不足，LLM 不知道用户偏好 | 应 ≤ 4 × `maxMemoryFileTokens`（4 种类型） |

---

## 3. 配置结构设计

### 3.1 config.json5 新增配置节

在 `Config` 结构体的 `memory` 字段下扩展，新增 `memorySystem` 子节：

```json5
{
  // ... 现有配置 ...

  "memory": {
    // 现有 vector 配置保持不变
    "vector": { ... },

    // 新增: 7 层记忆系统阈值配置
    "memorySystem": {
      // === 全局 ===
      "tokenBudget": 100000,        // Token 预算 (usize)
      "autoMemoryEnabled": true,    // 是否启用自动记忆提取
      "compactEnabled": true,       // 是否启用 Compact

      // === Layer 1: 工具结果持久化 ===
      "layer1": {
        "maxResultSizeChars": 50000,              // 单个工具结果持久化阈值
        "maxToolResultsPerMessageChars": 150000,  // 单消息工具结果总大小上限
        "previewSizeBytes": 2000,                 // 持久化预览大小
        "maxReplacementEntries": 1000,            // 替换状态最大条目数
        "cacheMaxPerSession": 10,                 // ResponseCache 每会话最大缓存
        "cacheMinItems": 5                        // ResponseCache 最小列表项数
      },

      // === Layer 2: 时间触发 MicroCompact ===
      "layer2": {
        "enabled": true,                // 是否启用
        "gapThresholdMinutes": 60,      // 对话间歇阈值(分钟)
        "keepRecent": 5                 // 保留最近工具结果数
      },

      // === Layer 3: Session Memory 提取 ===
      "layer3": {
        "minimumMessageTokensToInit": 10000,    // 首次提取 Token 门槛
        "minimumTokensBetweenUpdate": 5000,     // 提取间 Token 增量门槛
        "toolCallsBetweenUpdates": 3,           // 提取间工具调用次数门槛
        "extractionWaitTimeoutMs": 15000,       // 提取等待超时(ms)
        "extractionStaleThresholdMs": 60000,    // 提取结果过期阈值(ms)
        "maxSectionLength": 2000,               // 单 Section 最大长度
        "maxTotalSessionMemoryTokens": 12000    // Session Memory 总 Token 上限
      },

      // === Layer 4: Full Compact + 恢复 ===
      "layer4": {
        "compactThresholdRatio": 0.8,           // Compact 触发比例
        "keepRecentMessages": 2,                // 保留最近消息数
        "maxOutputTokens": 12000,               // Compact 摘要最大输出 tokens
        "maxFileRecoveryTokens": 50000,         // 文件恢复总预算
        "maxSingleFileTokens": 5000,            // 单文件恢复上限
        "maxFilesToRecover": 5,                 // 最大恢复文件数
        "maxSkillRecoveryTokens": 25000,        // 技能恢复预算
        "maxSessionMemoryRecoveryTokens": 12000, // Session Memory 恢复预算
        "trackerSummaryChars": 2000             // Tracker 摘要最大字符数
      },

      // === Layer 5: Auto Memory 提取 + 注入 ===
      "layer5": {
        "minMessagesForExtraction": 15,         // 触发提取最小消息数
        "extractionCooldownMessages": 5,        // 提取冷却消息数
        "maxMemoryFileTokens": 4000,            // 单记忆文件最大 tokens
        "injectionMaxTokens": 4000              // 注入系统提示最大 tokens
      }
    }
  }
}
```

### 3.2 Rust 配置结构定义

在 `crates/core/src/config.rs` 中新增：

```rust
/// 7 层记忆系统配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySystemConfig {
    /// Token 预算
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
    /// 是否启用自动记忆提取
    #[serde(default = "default_true_val")]
    pub auto_memory_enabled: bool,
    /// 是否启用 Compact
    #[serde(default = "default_true_val")]
    pub compact_enabled: bool,
    /// Layer 1 配置
    #[serde(default)]
    pub layer1: Layer1Config,
    /// Layer 2 配置
    #[serde(default)]
    pub layer2: Layer2Config,
    /// Layer 3 配置
    #[serde(default)]
    pub layer3: Layer3Config,
    /// Layer 4 配置
    #[serde(default)]
    pub layer4: Layer4Config,
    /// Layer 5 配置
    #[serde(default)]
    pub layer5: Layer5Config,
}

fn default_token_budget() -> usize { 100_000 }

impl Default for MemorySystemConfig {
    fn default() -> Self {
        Self {
            token_budget: 100_000,
            auto_memory_enabled: true,
            compact_enabled: true,
            layer1: Layer1Config::default(),
            layer2: Layer2Config::default(),
            layer3: Layer3Config::default(),
            layer4: Layer4Config::default(),
            layer5: Layer5Config::default(),
        }
    }
}

/// Layer 1: 工具结果持久化配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer1Config {
    #[serde(default = "default_l1_max_result_size")]
    pub max_result_size_chars: usize,
    #[serde(default = "default_l1_max_per_message")]
    pub max_tool_results_per_message_chars: usize,
    #[serde(default = "default_l1_preview_size")]
    pub preview_size_bytes: usize,
    #[serde(default = "default_l1_max_replacement")]
    pub max_replacement_entries: usize,
    #[serde(default = "default_l1_cache_max")]
    pub cache_max_per_session: usize,
    #[serde(default = "default_l1_cache_min_items")]
    pub cache_min_items: usize,
}
// ... 各 default 函数和 Default impl 省略，值同上表

/// Layer 2: 时间触发 MicroCompact 配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer2Config {
    #[serde(default = "default_true_val")]
    pub enabled: bool,
    #[serde(default = "default_l2_gap_threshold")]
    pub gap_threshold_minutes: u32,
    #[serde(default = "default_l2_keep_recent")]
    pub keep_recent: u32,
}

/// Layer 3: Session Memory 提取配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer3Config {
    #[serde(default = "default_l3_init_tokens")]
    pub minimum_message_tokens_to_init: usize,
    #[serde(default = "default_l3_update_tokens")]
    pub minimum_tokens_between_update: usize,
    #[serde(default = "default_l3_tool_calls")]
    pub tool_calls_between_updates: usize,
    #[serde(default = "default_l3_wait_timeout")]
    pub extraction_wait_timeout_ms: u64,
    #[serde(default = "default_l3_stale_threshold")]
    pub extraction_stale_threshold_ms: u64,
    #[serde(default = "default_l3_max_section")]
    pub max_section_length: usize,
    #[serde(default = "default_l3_max_total_tokens")]
    pub max_total_session_memory_tokens: usize,
}

/// Layer 4: Full Compact + 恢复配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer4Config {
    #[serde(default = "default_l4_threshold_ratio")]
    pub compact_threshold_ratio: f64,
    #[serde(default = "default_l4_keep_recent")]
    pub keep_recent_messages: usize,
    #[serde(default = "default_l4_max_output")]
    pub max_output_tokens: usize,
    #[serde(default = "default_l4_file_recovery")]
    pub max_file_recovery_tokens: usize,
    #[serde(default = "default_l4_single_file")]
    pub max_single_file_tokens: usize,
    #[serde(default = "default_l4_max_files")]
    pub max_files_to_recover: usize,
    #[serde(default = "default_l4_skill_recovery")]
    pub max_skill_recovery_tokens: usize,
    #[serde(default = "default_l4_session_memory_recovery")]
    pub max_session_memory_recovery_tokens: usize,
    #[serde(default = "default_l4_tracker_summary")]
    pub tracker_summary_chars: usize,
}

/// Layer 5: Auto Memory 提取 + 注入配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer5Config {
    #[serde(default = "default_l5_min_messages")]
    pub min_messages_for_extraction: usize,
    #[serde(default = "default_l5_cooldown")]
    pub extraction_cooldown_messages: usize,
    #[serde(default = "default_l5_max_file_tokens")]
    pub max_memory_file_tokens: usize,
    #[serde(default = "default_l5_injection_max")]
    pub injection_max_tokens: usize,
}
```

### 3.3 MemoryConfig 扩展

```rust
// 修改现有 MemoryConfig
pub struct MemoryConfig {
    #[serde(default)]
    pub vector: MemoryVectorConfig,
    /// 新增: 7 层记忆系统阈值配置
    #[serde(default)]
    pub memory_system: MemorySystemConfig,
}
```

---

## 4. 配置加载与校验

### 4.1 加载流程

```
config.json5 读取
    ↓
serde 反序列化 (缺失字段用 default 函数填充)
    ↓
MemorySystemConfig::validate() 校验
    ↓
传递给 MemorySystem::new()
    ↓
各层从 MemorySystemConfig 提取所需配置
```

### 4.2 校验规则

```rust
impl MemorySystemConfig {
    /// 校验配置合理性，返回警告列表（不阻断启动）
    pub fn validate(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        // 全局
        if self.token_budget < 20_000 {
            warnings.push("tokenBudget < 20000 可能导致频繁 Compact".into());
        }
        if self.token_budget > 500_000 {
            warnings.push("tokenBudget > 500000 可能导致内存压力".into());
        }

        // Layer 1
        if self.layer1.max_result_size_chars > self.layer1.max_tool_results_per_message_chars {
            warnings.push("layer1.maxResultSizeChars > maxToolResultsPerMessageChars 不合理".into());
        }

        // Layer 4: 恢复预算约束
        let total_recovery = self.layer4.max_file_recovery_tokens
            + self.layer4.max_skill_recovery_tokens
            + self.layer4.max_session_memory_recovery_tokens;
        let recovery_ratio = total_recovery as f64 / self.token_budget as f64;
        if recovery_ratio > 0.95 {
            warnings.push(format!(
                "Layer4 恢复总预算 {} 超过 tokenBudget 的 95% ({:.0}), 可能导致新消息空间不足",
                total_recovery, self.token_budget as f64 * 0.95
            ));
        }
        if self.layer4.max_single_file_tokens * self.layer4.max_files_to_recover
            > self.layer4.max_file_recovery_tokens {
            warnings.push("maxSingleFileTokens × maxFilesToRecover > maxFileRecoveryTokens, 部分文件可能被截断".into());
        }

        // Layer 4: 比例校验
        if self.layer4.compact_threshold_ratio < 0.5 || self.layer4.compact_threshold_ratio > 0.95 {
            warnings.push("compactThresholdRatio 建议在 [0.5, 0.95] 范围内".into());
        }

        // Layer 3 vs Layer 4: Session Memory 一致性
        if self.layer3.max_total_session_memory_tokens != self.layer4.max_session_memory_recovery_tokens {
            warnings.push("layer3.maxTotalSessionMemoryTokens != layer4.maxSessionMemoryRecoveryTokens, 建议保持一致".into());
        }

        // Layer 5
        if self.layer5.injection_max_tokens > self.layer5.max_memory_file_tokens * 4 {
            warnings.push("injectionMaxTokens 远大于 maxMemoryFileTokens, 可能注入过多内容".into());
        }

        warnings
    }
}
```

### 4.3 校验时机

- 启动时: `Config::load()` 后调用 `validate()`，警告通过 `tracing::warn!` 输出
- 运行时: 不做运行时校验，信任配置值（避免性能开销）
- `blockcell status` 命令: 显示当前配置和警告

### 4.4 配置文件自动更新机制

**核心需求**: 当 `config.json5` 中缺少 `memorySystem` 或其子字段时，系统不仅使用默认值运行，还要**将默认值写回配置文件**，使用户能直接看到并修改这些值。

#### 4.4.1 设计原则

1. **首次启动自动补全**: 当 `memorySystem` 节不存在时，写入完整的默认配置
2. **增量补全**: 当 `memorySystem` 存在但缺少子字段时，只补全缺失的子字段
3. **不覆盖用户值**: 已存在的配置值永远不被覆盖
4. **原子写入**: 先写临时文件再 rename，防止写入中断导致配置文件损坏
5. **保留注释**: json5 格式支持注释，写回时尽量保留用户注释

#### 4.4.2 实现方案

```rust
/// 配置文件自动更新器
pub struct ConfigAutoUpdater {
    /// 配置文件路径
    config_path: PathBuf,
}

impl ConfigAutoUpdater {
    /// 检查并补全缺失的 memorySystem 配置
    ///
    /// 返回值:
    /// - Ok(true): 配置文件已更新（写入了默认值）
    /// - Ok(false): 配置文件无需更新（所有字段已存在）
    /// - Err(e): 更新失败（不影响启动，仅记录警告）
    pub fn ensure_memory_system_defaults(&self) -> Result<bool, String> {
        // 1. 读取原始配置文件内容（保留注释和格式）
        let raw_content = std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("读取配置文件失败: {}", e))?;

        // 2. 解析为 json5 Value（保留结构）
        let mut root: serde_json::Value = json5::parse(&raw_content)
            .map_err(|e| format!("解析配置文件失败: {}", e))?;

        // 3. 检查 memory.memorySystem 是否存在
        let memory = root.pointer_mut("/memory");
        let memory_obj = match memory {
            Some(serde_json::Value::Object(m)) => m,
            _ => return Ok(false), // memory 节不存在，不强制创建
        };

        let mut updated = false;

        if !memory_obj.contains_key("memorySystem") {
            // memorySystem 整节缺失，写入完整默认配置
            let defaults = serde_json::to_value(MemorySystemConfig::default())
                .map_err(|e| format!("序列化默认配置失败: {}", e))?;
            memory_obj.insert("memorySystem".into(), defaults);
            updated = true;
        } else {
            // memorySystem 存在，增量补全缺失的子字段
            let ms = memory_obj.get_mut("memorySystem").unwrap();
            if let serde_json::Value::Object(ms_obj) = ms {
                updated = Self::fill_missing_fields(ms_obj);
            }
        }

        if updated {
            // 4. 序列化回 json5 格式
            let new_content = json5::to_string(&root)
                .map_err(|e| format!("序列化配置失败: {}", e))?;

            // 5. 原子写入：先写临时文件再 rename
            let tmp_path = self.config_path.with_extension("json5.tmp");
            std::fs::write(&tmp_path, &new_content)
                .map_err(|e| format!("写入临时文件失败: {}", e))?;
            std::fs::rename(&tmp_path, &self.config_path)
                .map_err(|e| format!("重命名配置文件失败: {}", e))?;

            tracing::info!(
                path = %self.config_path.display(),
                "已将默认 memorySystem 配置写入配置文件"
            );
        }

        Ok(updated)
    }

    /// 增量补全缺失的子字段
    ///
    /// 对比 MemorySystemConfig::default() 的 JSON 表示，
    /// 将缺失的键值对填入现有对象。不覆盖已存在的键。
    fn fill_missing_fields(existing: &mut serde_json::Map<String, serde_json::Value>) -> bool {
        let defaults = serde_json::to_value(MemorySystemConfig::default())
            .unwrap_or_default();

        if let serde_json::Value::Object(default_obj) = defaults {
            Self::merge_defaults(existing, &default_obj)
        } else {
            false
        }
    }

    /// 递归合并默认值（不覆盖已存在的键）
    fn merge_defaults(
        target: &mut serde_json::Map<String, serde_json::Value>,
        defaults: &serde_json::Map<String, serde_json::Value>,
    ) -> bool {
        let mut updated = false;

        for (key, default_value) in defaults {
            if !target.contains_key(key) {
                // 键不存在，填入默认值
                target.insert(key.clone(), default_value.clone());
                updated = true;
            } else if let (
                serde_json::Value::Object(target_obj),
                serde_json::Value::Object(default_obj),
            ) = (target.get_mut(key).unwrap(), default_value)
            {
                // 两边都是 Object，递归合并
                if Self::merge_defaults(target_obj, default_obj) {
                    updated = true;
                }
            }
        }

        updated
    }
}
```

#### 4.4.3 调用时机

```
Config::load()
    ↓
serde 反序列化 (缺失字段用 default 函数填充)
    ↓
ConfigAutoUpdater::ensure_memory_system_defaults()  ← 新增
    ↓
MemorySystemConfig::validate() 校验
    ↓
传递给 MemorySystem::new()
```

在 `Config::load()` 完成反序列化后、`validate()` 之前调用 `ensure_memory_system_defaults()`。

#### 4.4.4 更新失败处理

- 更新失败**不阻断启动**，仅记录 `tracing::warn!`
- 系统继续使用 serde 反序列化时填充的默认值运行
- 常见失败原因：配置文件权限不足、磁盘空间不足、json5 解析错误

#### 4.4.5 用户交互

- 首次写入时通过 `tracing::info!` 输出提示：
  ```
  [config] 已将默认 memorySystem 配置写入 ~/.blockcell/config.json5
  [config] 你可以通过修改 memory.memorySystem 节来调整记忆系统参数
  ```
- `blockcell status` 命令显示配置来源（用户配置 vs 默认值）

---

## 5. 配置传递路径

### 5.1 从 Config 到各层

```
Config.memory.memory_system (MemorySystemConfig)
    │
    ├──→ MemorySystem::new(config)           // 全局 + 开关
    │       ├── compact_threshold  ←── layer4.compact_threshold_ratio
    │       ├── token_budget       ←── token_budget
    │       ├── auto_memory_enabled ←── auto_memory_enabled
    │       └── compact_enabled    ←── compact_enabled
    │
    ├──→ ResponseCache::new(config.layer1)   // L1
    ├──→ TimeBasedMCConfig ←── config.layer2 // L2
    ├──→ SessionMemoryConfig ←── config.layer3 // L3
    ├──→ CompactConfig ←── config.layer4     // L4
    │       ├── FileTracker::new(config.layer4.tracker_summary_chars)
    │       └── SkillTracker::new(config.layer4.tracker_summary_chars)
    └──→ AutoMemoryConfig ←── config.layer5 // L5
            └── InjectionConfig::new(config.layer5.injection_max_tokens)
```

### 5.2 需要修改的文件清单

| 文件 | 修改内容 |
|------|----------|
| `crates/core/src/config.rs` | 新增 `MemorySystemConfig`, `Layer1Config` ~ `Layer5Config` 结构体；扩展 `MemoryConfig` |
| `crates/agent/src/memory_system/mod.rs` | `MemorySystemConfig` 从 core 引入，移除本地定义 |
| `crates/agent/src/response_cache.rs` | `ResponseCache::new()` 接受 L1 配置参数；移除硬编码常量 |
| `crates/agent/src/history_projector.rs` | `TimeBasedMCConfig` 从 core 引入 |
| `crates/agent/src/session_memory/extractor.rs` | `SessionMemoryConfig` 从 core 引入 |
| `crates/agent/src/session_memory/mod.rs` | 移除本地常量，从配置读取 |
| `crates/agent/src/compact/mod.rs` | `CompactConfig` 从 core 引入；移除恢复预算常量 |
| `crates/agent/src/compact/file_tracker.rs` | `FileTracker::new()` 接受 summary_chars 参数 |
| `crates/agent/src/compact/skill_tracker.rs` | `SkillTracker::new()` 接受 summary_chars 参数 |
| `crates/agent/src/auto_memory/mod.rs` | 移除本地常量，从配置读取 |
| `crates/agent/src/auto_memory/injector.rs` | `InjectionConfig` 从配置构建 |
| `crates/agent/src/runtime.rs` | 传递配置到 MemorySystem |

---

## 6. 向后兼容性

### 6.1 零破坏性变更

- 所有新增配置字段使用 `#[serde(default = "...")]` 注解
- 配置文件中不写 `memorySystem` 节时，全部使用默认值（与当前硬编码值完全一致）
- 现有 `MemoryConfig.vector` 配置不受影响

### 6.2 迁移路径

用户无需任何操作即可升级。如需调优，在 `config.json5` 中添加 `memory.memorySystem` 节即可。

---

## 7. 预设配置方案

为方便用户，提供几种典型场景的预设：

### 7.1 小模型 (8K 上下文)

```json5
{
  "memory": {
    "memorySystem": {
      "tokenBudget": 6000,
      "layer1": {
        "maxResultSizeChars": 10000,
        "maxToolResultsPerMessageChars": 30000
      },
      "layer3": {
        "minimumMessageTokensToInit": 3000,
        "minimumTokensBetweenUpdate": 1500,
        "maxTotalSessionMemoryTokens": 4000
      },
      "layer4": {
        "compactThresholdRatio": 0.7,
        "maxOutputTokens": 4000,
        "maxFileRecoveryTokens": 10000,
        "maxSingleFileTokens": 2000,
        "maxFilesToRecover": 3,
        "maxSkillRecoveryTokens": 5000,
        "maxSessionMemoryRecoveryTokens": 4000
      },
      "layer5": {
        "maxMemoryFileTokens": 2000,
        "injectionMaxTokens": 2000
      }
    }
  }
}
```

### 7.2 大模型 (200K 上下文)

```json5
{
  "memory": {
    "memorySystem": {
      "tokenBudget": 180000,
      "layer4": {
        "compactThresholdRatio": 0.85,
        "maxOutputTokens": 20000,
        "maxFileRecoveryTokens": 100000,
        "maxSingleFileTokens": 10000,
        "maxFilesToRecover": 10,
        "maxSkillRecoveryTokens": 50000,
        "maxSessionMemoryRecoveryTokens": 20000
      },
      "layer5": {
        "maxMemoryFileTokens": 8000,
        "injectionMaxTokens": 8000
      }
    }
  }
}
```

### 7.3 自动化/批处理场景

```json5
{
  "memory": {
    "memorySystem": {
      "layer2": {
        "gapThresholdMinutes": 15,
        "keepRecent": 3
      },
      "layer3": {
        "minimumMessageTokensToInit": 5000,
        "minimumTokensBetweenUpdate": 2000
      },
      "layer5": {
        "minMessagesForExtraction": 8,
        "extractionCooldownMessages": 3
      }
    }
  }
}
```

### 7.4 DeepSeek 1M 上下文场景

DeepSeek-V4 提供 1M (1,000,000) token 的超长上下文窗口，需要大幅调整记忆系统参数以充分利用这一优势。

**核心策略**:
- `tokenBudget` 设为 800K（留 200K 给系统提示和新消息）
- 恢复预算按 5:2.5:1.2 比例放大到 680K
- L1/L2/L3/L5 参数适度放大，充分利用长上下文
- `compactThresholdRatio` 提高到 0.9，减少不必要的 Compact

```json5
{
  "memory": {
    "memorySystem": {
      // === 全局 ===
      // 1M 上下文留 200K 给系统提示和新消息
      "tokenBudget": 800000,
      "autoMemoryEnabled": true,
      "compactEnabled": true,

      // === Layer 1: 工具结果持久化 ===
      // 长上下文下可容忍更大的工具结果
      "layer1": {
        "maxResultSizeChars": 100000,           // 10x: 允许更大的单文件内容
        "maxToolResultsPerMessageChars": 400000, // ~2.7x: 单轮可容纳更多工具输出
        "previewSizeBytes": 5000,               // 2.5x: 更长的预览帮助判断是否需要完整内容
        "maxReplacementEntries": 5000,          // 5x: 长会话需要更多替换记录
        "cacheMaxPerSession": 50,               // 5x: 更多缓存条目
        "cacheMinItems": 10                     // 2x: 更大的最小缓存阈值
      },

      // === Layer 2: 时间触发 MicroCompact ===
      // 长上下文下减少清理频率
      "layer2": {
        "enabled": true,
        "gapThresholdMinutes": 120,             // 2x: 更长的间隔，避免频繁清理
        "keepRecent": 20                        // 4x: 保留更多近期的工具结果
      },

      // === Layer 3: Session Memory 提取 ===
      // 长上下文下可提取更详细的摘要
      "layer3": {
        "minimumMessageTokensToInit": 20000,    // 2x: 等更多内容后再首次提取
        "minimumTokensBetweenUpdate": 10000,    // 2x: 增量提取间隔更大
        "toolCallsBetweenUpdates": 5,           // ~1.7x: 等更多工具调用后再提取
        "extractionWaitTimeoutMs": 30000,       // 2x: 更长的等待超时
        "extractionStaleThresholdMs": 120000,   // 2x: 更长的过期阈值
        "maxSectionLength": 5000,               // 2.5x: 更长的 Section
        "maxTotalSessionMemoryTokens": 50000    // ~4x: 更大的 Session Memory 总量
      },

      // === Layer 4: Full Compact + 恢复 ===
      // 1M 上下文下大幅放大恢复预算
      // 恢复总预算 = 400K + 200K + 80K = 680K (< 800K * 95%)
      "layer4": {
        "compactThresholdRatio": 0.9,           // 提高阈值，减少不必要的 Compact
        "keepRecentMessages": 5,                // 2.5x: 保留更多近期消息
        "maxOutputTokens": 50000,               // ~4x: 更长的 Compact 摘要
        "maxFileRecoveryTokens": 400000,        // 8x: 大幅放大文件恢复预算
        "maxSingleFileTokens": 20000,           // 4x: 单文件可恢复更多内容
        "maxFilesToRecover": 20,                // 4x: 可恢复更多文件
        "maxSkillRecoveryTokens": 200000,       // 8x: 大幅放大技能恢复预算
        "maxSessionMemoryRecoveryTokens": 80000, // ~6.7x: 大幅放大 Session Memory 恢复预算
        "trackerSummaryChars": 5000             // 2.5x: 更长的摘要
      },

      // === Layer 5: Auto Memory 提取 + 注入 ===
      // 长上下文下可注入更多记忆信息
      "layer5": {
        "minMessagesForExtraction": 20,         // ~1.3x: 等更多消息后再提取
        "extractionCooldownMessages": 8,         // 1.6x: 更长的冷却间隔
        "maxMemoryFileTokens": 16000,           // 4x: 更大的记忆文件
        "injectionMaxTokens": 16000             // 4x: 注入更多记忆信息
      }
    }
  }
}
```

**预算验证**:
- tokenBudget: 800,000
- 恢复总预算: 400K + 200K + 80K = 680K = 85% of 800K (< 95% ✓)
- 单文件约束: 20K × 20 = 400K (≤ 400K ✓)
- L3/L4 Session Memory 一致: 50K (L3) vs 80K (L4) — L4 恢复预算 > L3 总量，允许恢复时包含完整 Session Memory ✓

### 7.5 中等模型 (128K 上下文)

128K 上下文是当前主流模型的典型窗口大小（GPT-4、Claude Sonnet、DeepSeek-V3 等），适合大多数日常开发场景。

**核心策略**:
- `tokenBudget` 设为 100K（留 28K 给系统提示和新消息）
- 恢复预算按 5:2.5:1.2 比例分配：文件 50K + 技能 25K + Session Memory 12K = 87K
- L1/L2 保持默认值，128K 上下文下默认配置已足够
- `compactThresholdRatio` 保持 0.8，在 80K 时触发 Compact
- L3/L5 适度调整，在中等上下文下平衡提取频率和开销

```json5
{
  "memory": {
    "memorySystem": {
      // === 全局 ===
      // 128K 上下文留 28K 给系统提示和新消息
      "tokenBudget": 100000,
      "autoMemoryEnabled": true,
      "compactEnabled": true,

      // === Layer 1: 工具结果持久化 ===
      // 128K 上下文下默认值已合理，无需调整
      "layer1": {
        "maxResultSizeChars": 50000,
        "maxToolResultsPerMessageChars": 150000,
        "previewSizeBytes": 2000,
        "maxReplacementEntries": 1000,
        "cacheMaxPerSession": 10,
        "cacheMinItems": 5
      },

      // === Layer 2: 时间触发 MicroCompact ===
      // 保持默认，1 小时间歇清理合理
      "layer2": {
        "enabled": true,
        "gapThresholdMinutes": 60,
        "keepRecent": 5
      },

      // === Layer 3: Session Memory 提取 ===
      // 适度调整提取参数
      "layer3": {
        "minimumMessageTokensToInit": 10000,
        "minimumTokensBetweenUpdate": 5000,
        "toolCallsBetweenUpdates": 3,
        "extractionWaitTimeoutMs": 15000,
        "extractionStaleThresholdMs": 60000,
        "maxSectionLength": 2000,
        "maxTotalSessionMemoryTokens": 12000
      },

      // === Layer 4: Full Compact + 恢复 ===
      // 恢复总预算 = 50K + 25K + 12K = 87K (< 100K * 95%)
      "layer4": {
        "compactThresholdRatio": 0.8,
        "keepRecentMessages": 2,
        "maxOutputTokens": 12000,
        "maxFileRecoveryTokens": 50000,
        "maxSingleFileTokens": 5000,
        "maxFilesToRecover": 5,
        "maxSkillRecoveryTokens": 25000,
        "maxSessionMemoryRecoveryTokens": 12000,
        "trackerSummaryChars": 2000
      },

      // === Layer 5: Auto Memory 提取 + 注入 ===
      // 保持默认，15 条消息后提取合理
      "layer5": {
        "minMessagesForExtraction": 15,
        "extractionCooldownMessages": 5,
        "maxMemoryFileTokens": 4000,
        "injectionMaxTokens": 4000
      }
    }
  }
}
```

**预算验证**:
- tokenBudget: 100,000
- 恢复总预算: 50K + 25K + 12K = 87K = 87% of 100K (< 95% ✓)
- 单文件约束: 5K × 5 = 25K (≤ 50K ✓)
- L3/L4 Session Memory 一致: 12K (L3) = 12K (L4) ✓

**说明**: 此配置与默认值完全一致，列出所有字段是为了方便用户在此基础上微调。常见调整方向：
- **代码审查场景**: 提高 `layer4.maxFileRecoveryTokens` 到 80K，`maxFilesToRecover` 到 8，以便恢复更多代码文件
- **长对话场景**: 降低 `layer4.compactThresholdRatio` 到 0.7，更早触发 Compact 避免上下文溢出
- **轻量对话场景**: 提高 `layer4.compactThresholdRatio` 到 0.85，延迟 Compact 保留更多历史

---

## 8. 实现步骤

### Phase 1: 配置结构定义 (core crate)

1. 在 `crates/core/src/config.rs` 中定义 `MemorySystemConfig` 及各 `LayerXConfig`
2. 扩展 `MemoryConfig` 添加 `memory_system` 字段
3. 实现 `validate()` 方法
4. 添加单元测试验证默认值和校验逻辑

### Phase 2: 配置文件自动更新 (core crate)

5. 实现 `ConfigAutoUpdater` 及 `ensure_memory_system_defaults()` 方法
6. 实现 `merge_defaults()` 递归合并逻辑
7. 在 `Config::load()` 流程中集成自动更新调用
8. 添加单元测试：完整缺失、部分缺失、无需更新、写入失败等场景

### Phase 3: 配置传递 (agent crate)

9. 修改 `MemorySystem::new()` 接受 `MemorySystemConfig`（从 core 引入）
10. 修改 `runtime.rs` 将配置从 `Config` 传递到 `MemorySystem`
11. 移除 `memory_system/mod.rs` 中的本地 `MemorySystemConfig` 定义

### Phase 4: 各层接入配置

12. Layer 1: `ResponseCache` 接受 L1 配置
13. Layer 2: `TimeBasedMCConfig` 从 core 引入
14. Layer 3: `SessionMemoryConfig` 从 core 引入，移除本地常量
15. Layer 4: `CompactConfig` 从 core 引入，移除恢复预算常量
16. Layer 5: 移除 `auto_memory/mod.rs` 中的常量，从配置读取

### Phase 5: Tracker 参数化

17. `FileTracker::new(max_summary_chars)` 接受参数
18. `SkillTracker::new(max_summary_chars)` 接受参数

### Phase 6: 测试与文档

19. 更新所有受影响的单元测试
20. 添加配置集成测试
21. 更新 CLAUDE.md 文档

---

## 9. 风险与缓解

| 风险 | 缓解措施 |
|------|----------|
| 用户配置极端值导致系统崩溃 | `validate()` 输出警告；关键值硬性 clamp 到安全范围 |
| 配置项过多增加用户认知负担 | 提供预设方案；大部分用户只需调整 `tokenBudget` |
| 配置传递链过长 | 统一通过 `MemorySystemConfig` 传递，各层只提取自己需要的部分 |
| 恢复预算超限导致上下文溢出 | `validate()` 检查恢复总预算不超过 tokenBudget 的 95% |
| serde 默认值函数重复 | 使用 `#[serde(default)]` 配合 `Default` impl，减少重复 |

---

## 10. 附录: 当前硬编码阈值完整清单

### Layer 1 (`response_cache.rs`)

```rust
pub const DEFAULT_MAX_RESULT_SIZE_CHARS: usize = 50_000;       // L74
pub const MAX_TOOL_RESULTS_PER_MESSAGE_CHARS: usize = 150_000; // L76
pub const PREVIEW_SIZE_BYTES: usize = 2000;                    // L73
pub const MAX_REPLACEMENT_ENTRIES: usize = 1000;               // L381
// ResponseCacheInner.max_per_session: 10                       // L39
// is_cacheable: content.chars().count() > 800                  // L170
// maybe_cache_and_stub: items.len() < 5                        // L91
```

### Layer 2 (`history_projector.rs`)

```rust
// TimeBasedMCConfig::default()
//   enabled: true
//   gap_threshold_minutes: 60
//   keep_recent: 5
```

### Layer 3 (`session_memory/`)

```rust
// SessionMemoryConfig::default()
//   minimum_message_tokens_to_init: 10_000
//   minimum_tokens_between_update: 5_000
//   tool_calls_between_updates: 3
pub const EXTRACTION_WAIT_TIMEOUT_MS: u64 = 15_000;            // mod.rs:40
pub const EXTRACTION_STALE_THRESHOLD_MS: u64 = 60_000;         // mod.rs:41
pub const MAX_SECTION_LENGTH: usize = 2000;                    // mod.rs:44
pub const MAX_TOTAL_SESSION_MEMORY_TOKENS: usize = 12000;      // mod.rs:45
```

### Layer 4 (`compact/`)

```rust
pub const MAX_FILE_RECOVERY_TOKENS: usize = 50_000;            // mod.rs:39
pub const MAX_SINGLE_FILE_TOKENS: usize = 5_000;               // mod.rs:41
pub const MAX_SKILL_RECOVERY_TOKENS: usize = 25_000;           // mod.rs:43
pub const MAX_SESSION_MEMORY_RECOVERY_TOKENS: usize = 12_000;  // mod.rs:45
pub const MAX_FILES_TO_RECOVER: usize = 5;                     // mod.rs:47
// CompactConfig::default()
//   token_threshold: 100_000
//   threshold_ratio: 0.8
//   keep_recent_messages: 2
//   max_output_tokens: 12_000
// FileTracker.max_summary_chars: 2000                          // file_tracker.rs:37
// SkillTracker.max_summary_chars: 2000                         // skill_tracker.rs:36
```

### Layer 5 (`auto_memory/`)

```rust
pub const MIN_MESSAGES_FOR_EXTRACTION: usize = 15;             // mod.rs:31
pub const EXTRACTION_COOLDOWN_MESSAGES: usize = 5;             // mod.rs:32
pub const MAX_MEMORY_FILE_TOKENS: usize = 4000;                // mod.rs:33
// InjectionConfig::default()
//   max_tokens: MAX_MEMORY_FILE_TOKENS (= 4000)                // injector.rs:23
```

### 全局 (`memory_system/mod.rs`)

```rust
// MemorySystemConfig::default()
//   auto_memory_enabled: true
//   compact_enabled: true
//   compact_threshold: 0.8
//   token_budget: 100_000
```
