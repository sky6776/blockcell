# BlockCell 7层记忆系统设计文档

---

## 目录

1. [概述](#一概述)
2. [架构设计](#二架构设计)
3. [实现状态](#三实现状态)
4. [Layer 1: 工具结果存储](#四layer-1-工具结果存储)
5. [Layer 2: 轻量压缩](#五layer-2-轻量压缩)
6. [Layer 3: 会话记忆](#六layer-3-会话记忆)
7. [Layer 4: 完整压缩](#七layer-4-完整压缩)
8. [Layer 5: 自动记忆提取](#八layer-5-自动记忆提取)
9. [Layer 6: 梦境机制](#九layer-6-梦境机制)
10. [Layer 7: Forked Agent](#十layer-7-forked-agent)
11. [各层协作机制](#十一各层协作机制)
12. [新旧逻辑对比](#十二新旧逻辑对比)
13. [未来 TODO](#十三未来-todo)

---

## 一、概述

### 1.1 7层记忆架构

BlockCell 的 7层记忆机制是一个精心设计的上下文管理系统，从基础的工具输出处理到高级的跨会话知识整合，形成了完整的记忆生命周期管理。

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          BlockCell 7层记忆架构                               │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Layer 7: Forked Agent (跨代理通信)                                         │
│           ↓ 支撑所有异步后台任务                                              │
│  Layer 6: Auto Dream (梦境机制)                                             │
│           ↓ 跨会话知识整合                                                    │
│  Layer 5: Extract Memories (自动记忆提取)                                    │
│           ↓ 每轮对话结束触发                                                  │
│  Layer 4: Full Compact (完整压缩)                                           │
│           ↓ LLM 语义压缩                                                      │
│  Layer 3: Session Memory (会话记忆)                                         │
│           ↓ 实时会话摘要                                                      │
│  Layer 2: Micro Compact (轻量压缩)                                          │
│           ↓ 时间触发清理                                                      │
│  Layer 1: Tool Result Storage (工具结果存储)                                │
│           ↓ 大输出持久化                                                      │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 1.2 核心设计原则

| 原则 | 说明 |
|------|------|
| **渐进式压缩** | 从简单截断到 LLM 语义压缩，逐层升级 |
| **异步非阻塞** | 后台任务使用 Forked Agent，不阻塞主流程 |
| **缓存优先** | 状态冻结、决策一致，保证 Prompt Cache 命中率 |
| **预算控制** | 每层都有明确的 Token 预算限制 |
| **安全隔离** | Forked Agent 限制工具权限，防止越权操作 |

### 1.3 设计理念

1. **零成本抽象**: Layer 1-2 同步执行，无 LLM 调用开销
2. **按需压缩**: 只在必要时触发更高级的压缩
3. **状态一致性**: 状态冻结模式确保缓存有效性
4. **分离关注点**: 每层独立职责，可单独优化

### 1.4 核心概念解释

#### 什么是 Token？

**Token（词元）** 是 LLM 处理文本的基本单位。简单理解：
- 1 个英文单词 ≈ 1-2 个 Token
- 1 个中文字 ≈ 2-3 个 Token
- LLM 有输入 Token 限制（如 Claude 约 200K Token）

**为什么关注 Token？**
- Token 越多 → API 成本越高 → 响应越慢
- 超过限制 → 无法处理 → 必须压缩

#### 什么是 Prompt Cache？

**Prompt Cache（提示缓存）** 是 LLM 提供商的缓存机制：
- 相同的输入前缀可以复用缓存
- 缓存命中 → 节省 90% 成本 → 响应更快
- 缓存失效 → 重新计算 → 成本增加

**类比**：就像网页缓存，访问过的内容下次加载更快。

#### 什么是缓存一致性？

**缓存一致性** 指多次请求中，可缓存部分的内容必须完全相同：
- 相同输入 → 相同输出 → 缓存命中
- 内容变化 → 缓存失效 → 成本增加

**状态冻结** 就是为保证缓存一致性而设计：一旦决定替换某工具结果，后续所有请求都用相同的替换内容。

#### 什么是 Forked Agent？

**Forked Agent（派生子代理）** 是从主 Agent 派生出的独立执行单元：

```
主 Agent (正在处理用户对话)
    │
    └─► Forked Agent (后台执行任务)
            ├─ 共享父 Agent 的缓存参数
            ├─ 限制工具权限（安全隔离）
            └─ 独立执行，不阻塞主流程
```

**类比**：主 Agent 是"前台接待员"，Forked Agent 是"后台专员"，前台忙于接待客人时，后台专员默默完成任务。

#### 什么是 LLM 语义压缩？

**LLM 语义压缩** 是用大语言模型对长对话生成摘要：
- 不是简单截断，而是理解语义后提炼精华
- 保留关键信息，丢弃冗余细节
- 类似"读书笔记"而非"原文摘抄"

**对比**：
| 方式 | 示例 |
|------|------|
| 截断 | "前面对话内容已删除..." |
| 语义压缩 | "用户之前要求修复登录 bug，已定位到 config.rs 的空指针问题..." |

---

## 二、架构设计

### 2.1 分层架构图

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                     BlockCell 7层记忆系统架构                                │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │ 已有系统 (保持不变)                                                   │   │
│  ├─────────────────────────────────────────────────────────────────────┤   │
│  │  MemoryStore (SQLite + FTS5)                                        │   │
│  │  ├─ long_term: fact, preference, project, task, policy, glossary   │   │
│  │  └─ short_term: note, snippet, contact, session_summary            │   │
│  │                                                                      │   │
│  │  GhostService (后台维护)                                             │   │
│  │  └─ 定时记忆清理和社区互动                                            │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
│                         ↑ 完全独立，互不干扰 ↑                               │
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │ 7层记忆系统 (完全替代旧的截断方式)                                     │   │
│  ├─────────────────────────────────────────────────────────────────────┤   │
│  │  Layer 7: ForkedAgent                                               │   │
│  │  └─ crates/agent/src/forked/                                        │   │
│  │                                                                      │   │
│  │  Layer 6: KnowledgeConsolidator                                     │   │
│  │  └─ crates/scheduler/src/consolidator.rs                            │   │
│  │                                                                      │   │
│  │  Layer 5: AutoMemoryExtractor                                       │   │
│  │  └─ crates/agent/src/auto_memory/                                   │   │
│  │                                                                      │   │
│  │  Layer 4: LLMCompactor (完整压缩，替代旧的截断方式)                   │   │
│  │  └─ crates/agent/src/compact/                                       │   │
│  │                                                                      │   │
│  │  Layer 3: SessionCache                                              │   │
│  │  └─ crates/agent/src/session_memory/                                │   │
│  │                                                                      │   │
│  │  Layer 2: TimeBasedMicroCompact                                     │   │
│  │  └─ crates/agent/src/history_projector.rs                           │   │
│  │                                                                      │   │
│  │  Layer 1: ToolResultStorage                                         │   │
│  │  └─ crates/agent/src/response_cache.rs                              │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 层级依赖关系

| Layer | 名称 | 依赖 | 执行方式 | 集成位置 |
|-------|------|------|----------|----------|
| **Layer 1** | 工具结果存储 | 无 | 同步 | `response_cache.rs` |
| **Layer 2** | 时间触发轻量压缩 | Layer 1 状态 | 同步 | `history_projector.rs` |
| **Layer 3** | 会话记忆 | Layer 7 | 异步 | `session_memory/` |
| **Layer 4** | 完整压缩 | Layer 3, 7 | 同步/异步 | `compact/` |
| **Layer 5** | 自动记忆提取 | Layer 7 | 异步 | `auto_memory/` |
| **Layer 6** | 梦境机制 | Layer 5, 7 | 异步 | `consolidator.rs` |
| **Layer 7** | Forked Agent | 无 | - | `forked/` |

### 2.3 Token 预算分配

| 层级 | 预算项 | 值 | 说明 |
|------|--------|-----|------|
| **Layer 1** | 预览大小 | 2KB | `PREVIEW_SIZE_BYTES` |
| **Layer 1** | 单工具上限 | 50KB | `DEFAULT_MAX_RESULT_SIZE_CHARS` |
| **Layer 1** | 消息预算 | 150KB | `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` |
| **Layer 3** | 单节限制 | 2000 tokens | 每个 Section 上限 |
| **Layer 3** | 总限制 | 12000 tokens | Session Memory 总上限 |
| **Layer 4** | 文件恢复预算 | 50,000 tokens | Post-Compact 恢复 |
| **Layer 4** | 单文件上限 | 5,000 tokens | 单个文件恢复上限 |
| **Layer 4** | 技能恢复预算 | 25,000 tokens | 技能恢复预算 |
| **Layer 4** | 最大文件数 | 5 | 恢复文件数量上限 |

---

## 三、实现状态

基于实际代码分析，各层实现状态如下：

| Layer | 模块位置 | 实现状态 | 说明 |
|-------|----------|----------|------|
| **Layer 1** | `response_cache.rs` | ✅ 已实现 | 工具结果缓存，使用 `session_recall` 工具检索 |
| **Layer 2** | `history_projector.rs` | ✅ 已实现 | 仅做历史分析，不执行截断（由 Layer 4 负责） |
| **Layer 3** | `session_memory/` | ✅ 已实现 | 10-Section 模板，Forked Agent 后台提取 |
| **Layer 4** | `compact/` | ✅ 已实现 | LLM 语义压缩，Post-Compact 恢复机制 |
| **Layer 5** | `auto_memory/` | ✅ 已实现 | 4种记忆类型，游标管理，注入器 |
| **Layer 6** | `consolidator.rs` + `dream_service.rs` | ✅ 已实现 | 三重门控，四阶段执行，后台服务 |
| **Layer 7** | `forked/` | ✅ 已实现 | CacheSafeParams, SubagentOverrides, CanUseToolFn |

### 关键实现文件

```
crates/agent/src/
├── response_cache.rs          # Layer 1: 工具结果缓存
├── history_projector.rs       # Layer 2: 历史分析（不截断）
├── session_memory/            # Layer 3: 会话记忆
│   ├── mod.rs
│   ├── template.rs            # 10-Section 模板
│   ├── extractor.rs           # 提取器
│   └── recovery.rs            # Post-Compact 恢复
├── compact/                   # Layer 4: 完整压缩
│   ├── mod.rs
│   ├── summary.rs             # 摘要生成
│   ├── recovery.rs            # 恢复机制
│   ├── hooks.rs               # Pre/Post Hooks
│   ├── file_tracker.rs        # 文件追踪
│   └── skill_tracker.rs       # 技能追踪
├── auto_memory/               # Layer 5: 自动记忆提取
│   ├── mod.rs
│   ├── memory_type.rs         # 4种记忆类型
│   ├── extractor.rs           # 提取器
│   ├── cursor.rs              # 游标管理
│   └── injector.rs            # 记忆注入器
├── forked/                    # Layer 7: Forked Agent 基础设施
│   ├── mod.rs
│   ├── agent.rs               # 子代理执行
│   ├── cache_safe.rs          # 缓存安全参数
│   ├── can_use_tool.rs        # 工具权限控制
│   └── context.rs             # 子代理上下文
└── memory_system/
    └── mod.rs                 # 记忆系统集成

crates/scheduler/src/
├── consolidator.rs            # Layer 6: 梦境整合逻辑
└── dream_service.rs           # Layer 6: 后台定时服务
```

---

## 四、Layer 1: 工具结果存储

### 4.1 核心思想

当工具输出过大时，持久化到磁盘，仅在对话中保留预览。这是最基础的上下文优化层，保证缓存一致性。

### 4.2 处理方式演进

#### 旧方式：简单截断

在 7 层记忆系统引入之前，大工具结果会被简单截断：

```
Turn 1:
  [user] "读取大文件"
  [assistant] "好的"
  [tool_result] "文件前 50KB 内容...
                 ... (content truncated)"  ← 直接截断
  
Turn 2:
  [user] "继续"
  [assistant] 无法恢复被截断的内容
             → 信息丢失
             → 用户需要重新读取
```

**问题**：
- 截断后的内容无法恢复
- 用户需要重新执行工具获取完整内容
- 无法通过工具 ID 检索原始结果

#### 新方式：持久化 + 预览 + 可检索

7 层记忆系统引入后，大工具结果持久化到磁盘，并支持后续检索：

```
Turn 1:
  [user] "读取大文件"
  [assistant] "好的"
  [tool_result] "<persisted-output>
                 文件太大 (500KB)，已保存到磁盘。
                 Tool ID: toolu_xxx
                 预览 (前 2KB):
                 ...
                 </persisted-output>"
  
Turn 2:
  [user] "给我完整内容"
  [assistant] 调用 session_recall 工具
             → 通过 Tool ID 检索原始内容
             → 无需重新读取文件
```

**改进对比**：

| 方面 | 旧方式（截断） | 新方式（持久化） |
|------|----------------|------------------|
| 内容恢复 | ❌ 不可恢复 | ✅ 可通过 `session_recall` 检索 |
| Token 消耗 | 截断后内容 | 仅预览 (2KB) |
| 重复执行 | 需要重新运行工具 | 直接检索磁盘 |
| Cache 命中 | 低（截断位置变化） | 高（状态冻结） |

### 4.3 触发机制

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          Layer 1 两层机制                                    │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  第一层: 单工具阈值检查                                                       │
│  ├─ 检查时机: 工具执行后                                                     │
│  ├─ 触发条件: 输出 > 50KB                                                   │
│  └─ 动作: 持久化到磁盘，返回预览                                              │
│                                                                             │
│  第二层: 消息级别预算                                                         │
│  ├─ 检查时机: Query 循环开始                                                 │
│  ├─ 触发条件: 单条消息工具结果总和 > 150KB                                    │
│  └─ 动作: 选择最大的结果持久化                                                │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 4.3 工作流程

1. **工具执行后检查**: 每个工具执行完成后，检查输出大小
2. **持久化决策**: 如果超过阈值，将内容写入磁盘
3. **预览生成**: 生成 2KB 的预览内容，保持可读性
4. **状态冻结**: 记录决策到 `ContentReplacementState`，确保一致性

### 4.4 关键数据结构

```rust
/// 持久化结果
pub struct PersistedToolResult {
    pub filepath: PathBuf,      // 存储路径
    pub original_size: usize,   // 原始大小
    pub is_json: bool,          // 是否 JSON
    pub preview: String,        // 预览内容
    pub has_more: bool,         // 是否有更多
}

/// 内容替换状态 (用于缓存一致性)
pub struct ContentReplacementState {
    pub seen_ids: HashSet<String>,              // 已处理的工具ID
    pub replacements: HashMap<String, String>,  // ID -> 替换内容
}
```

### 4.5 状态冻结原则详解

#### 4.5.1 核心思想

> **决策一旦做出，永不改变** —— 这是为了保证 Prompt Cache 的稳定性。

#### 4.5.2 背景：Prompt Cache 机制

```
┌─────────────────────────────────────────────────────────────┐
│                    LLM API 请求                              │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  System Prompt     ─┐                                       │
│  Tools 定义        ─┼── 可缓存前缀 (Cached Prefix)           │
│  消息 1..N-3      ─┤                                       │
│  消息 N-2         ─┘ ←── Cache 断点                          │
│  消息 N-1         ─── 新增内容                               │
│  消息 N           ─── 新增内容                               │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

Cache 命中条件：**相同输入 → 相同输出**。如果消息前缀发生变化，Cache 失效，需要重新计算。

#### 4.5.3 问题场景

```
Turn 1: 工具 A 输出 60KB → 超阈值 → 持久化 → 替换为预览
Turn 2: 工具 B 输出 30KB → 未超阈值 → 保持原样
Turn 3: 用户追问 → 需要重放消息历史
        → 工具 A 的内容是什么？预览还是原始？
```

如果决策不一致：

```
Turn 1 认为工具 A 已替换
Turn 3 认为工具 A 未替换
→ 消息前缀变化 → Cache 失效 → 成本增加
```

#### 4.5.4 状态冻结机制

```rust
/// 内容替换状态
pub struct ContentReplacementState {
    /// 已处理的工具 ID 集合
    /// 一旦 ID 进入此集合，其命运已确定
    pub seen_ids: HashSet<String>,
    
    /// ID → 替换后的内容
    /// 记录每个工具被替换成什么内容
    pub replacements: HashMap<String, String>,
}
```

#### 4.5.5 三条冻结规则

| 规则 | 说明 | 示例 |
|------|------|------|
| **已见 ID 命运确定** | 一旦 seen_ids 包含某 ID，永不重新决策 | tool-123 在 Turn 1 被标记为"替换" |
| **已替换永远替换** | replacements 中记录的内容永久不变 | tool-123 的替换内容是"预览 A"，永远如此 |
| **未替换永不替换** | 不在 replacements 中的 ID 保持原样 | tool-456 未被替换，后续所有 Turn 都保持原始内容 |

#### 4.5.6 实际流程示例

```
=== Turn 1 ===
工具 A (tool-aaa): 输出 80KB
  → 超过 50KB 阈值
  → 持久化到磁盘
  → seen_ids.insert("tool-aaa")
  → replacements.insert("tool-aaa", "预览内容...")
  
工具 B (tool-bbb): 输出 30KB
  → 未超阈值
  → seen_ids.insert("tool-bbb")
  → 不添加到 replacements（保持原始）

状态:
  seen_ids = {"tool-aaa", "tool-bbb"}
  replacements = {"tool-aaa": "预览内容..."}

=== Turn 2 ===
重放消息历史:
  tool-aaa ∈ seen_ids → 检查 replacements → 找到 → 使用预览
  tool-bbb ∈ seen_ids → 检查 replacements → 未找到 → 使用原始内容

工具 C (tool-ccc): 输出 60KB
  → 超过阈值
  → seen_ids.insert("tool-ccc")
  → replacements.insert("tool-ccc", "预览内容 C...")

状态:
  seen_ids = {"tool-aaa", "tool-bbb", "tool-ccc"}
  replacements = {"tool-aaa": "...", "tool-ccc": "..."}

=== Turn 3 ===
重放消息历史:
  tool-aaa → 使用 Turn 1 决定的预览（不变）
  tool-bbb → 使用原始内容（不变，因为当时未替换）
  tool-ccc → 使用 Turn 2 决定的预览（不变）
```

#### 4.5.7 设计对比

```
┌─────────────────────────────────────────────────────────────┐
│                  没有 state 冻结                             │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Turn 1: tool-aaa → 替换为预览                               │
│  Turn 2: 消息总预算超限 → 重新评估 tool-aaa                   │
│          → 工具 C 更大，优先替换 C                            │
│          → tool-aaa 改为"保留原始"  ← 决策改变！              │
│          → 消息前缀变化                                       │
│          → Prompt Cache 失效                                 │
│          → API 成本增加 ~30%                                  │
│                                                              │
└─────────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────────┐
│                  有 state 冻结                               │
├─────────────────────────────────────────────────────────────┤
│                                                              │
│  Turn 1: tool-aaa → 替换为预览 → seen_ids 记录               │
│  Turn 2: 消息总预算超限 → 检查 seen_ids                       │
│          → tool-aaa 已在 seen_ids → 查 replacements          │
│          → 使用已记录的预览（不变）                           │
│          → 消息前缀稳定                                       │
│          → Prompt Cache 命中                                  │
│          → API 成本节省 ~90%                                  │
│                                                              │
└─────────────────────────────────────────────────────────────┘
```

#### 4.5.8 代码实现关键点

```rust
// 处理工具结果时的决策逻辑
fn process_tool_result(
    tool_use_id: &str,
    content: &str,
    state: &ContentReplacementState,
) -> ProcessResult {
    // 1. 检查是否已处理过
    if state.seen_ids.contains(tool_use_id) {
        // 2. 命中 seen_ids，命运已确定
        if let Some(replacement) = state.replacements.get(tool_use_id) {
            // 已替换 → 返回已记录的替换内容（冻结）
            return ProcessResult::AlreadyReplaced(replacement.clone());
        } else {
            // 未替换 → 保持原始内容（冻结）
            return ProcessResult::KeepOriginal;
        }
    }
    
    // 3. 新 ID，做新决策
    if content.len() > THRESHOLD {
        let preview = generate_preview(content);
        // 4. 冻结决策
        state.seen_ids.insert(tool_use_id.to_string());
        state.replacements.insert(tool_use_id.to_string(), preview.clone());
        return ProcessResult::Replace(preview);
    } else {
        // 5. 冻结决策（不替换）
        state.seen_ids.insert(tool_use_id.to_string());
        return ProcessResult::KeepOriginal;
    }
}
```

#### 4.5.9 总结

| 原则 | 目的 |
|------|------|
| **决策冻结** | 保证相同 ID 在不同 Turn 的处理结果一致 |
| **前缀稳定** | 消息历史前缀不变 → Prompt Cache 命中 |
| **成本控制** | Cache 命中可节省 90% API 成本 |

> 这就是"状态冻结"的核心：**用确定性换取缓存效率**。

### 4.6 预览生成算法

```rust
fn generate_preview(content: &str, max_bytes: usize) -> (String, bool) {
    if content.len() <= max_bytes {
        return (content.to_string(), false);
    }
    
    // 在换行边界截断，保持可读性
    let truncated = &content[..max_bytes];
    let last_newline = truncated.rfind('\n');
    let cut_point = last_newline
        .filter(|&pos| pos > max_bytes / 2)
        .unwrap_or(max_bytes);
    
    (content[..cut_point].to_string(), true)
}
```

### 4.7 消息格式

```
<persisted-output>
Output too large (1.2MB). Full output saved to: /path/to/tool-results/uuid.json

Preview (first 2KB):
...内容...
...
</persisted-output>
```

---

## 五、Layer 2: 轻量压缩

### 5.1 核心思想

在不触发完整压缩的情况下，清理旧的工具结果。两种触发方式：

| 触发方式 | 触发条件 | 机制 |
|----------|----------|------|
| **时间触发** | 距上次 assistant > 60分钟 | 直接清理内容 |
| **缓存编辑** | 服务器缓存过期 | 使用 `cache_edits` API |

### 5.2 可压缩工具列表

```rust
const COMPACTABLE_TOOLS: &[&str] = &[
    "read_file", "shell", "grep", "glob",
    "web_search", "web_fetch", "file_edit", "file_write",
];
```

### 5.3 触发条件

```rust
pub struct TimeBasedMCConfig {
    pub enabled: bool,              // 默认 true
    pub gap_threshold_minutes: i64, // 默认 60 分钟
    pub keep_recent: usize,         // 保留最近 N 个，默认 3
}
```

### 5.4 工作流程

```
时间触发检查
    │
    ├─► 检查距上次 assistant 消息间隔
    │       └─ 如果 > 60分钟，继续
    │
    ├─► 收集可压缩的工具结果 ID
    │       └─ 过滤 COMPACTABLE_TOOLS
    │
    ├─► 保留最近 N 个
    │       └─ keep_recent = 3
    │
    └─► 清理其余内容
            └─ 替换为 "[Old tool result content cleared]"
```

### 5.5 与 Layer 1 的关系

Layer 2 依赖 Layer 1 的 `ContentReplacementState`：
- 清理时保留 `seenIds` 中的决策
- 不破坏已有的缓存一致性
- 只清理未被持久化的内容

---

## 六、Layer 3: 会话记忆

### 6.1 核心思想

维护一个实时更新的 Markdown 文件，包含当前会话的关键信息。使用 Forked Agent 后台提取，不中断主对话。

### 6.2 触发机制

```rust
pub struct SessionCacheConfig {
    pub minimum_tokens_to_init: usize,      // 默认 10,000
    pub minimum_tokens_between_update: usize, // 默认 5,000
    pub tool_calls_between_updates: usize,   // 默认 3
}

// 触发条件:
// 1. Token 阈值必须满足
// 2. Tool Calls 阈值可选满足
// 3. 安全条件: 最后一条消息无 tool_calls
```

### 6.3 10-Section 模板

```markdown
# Session Title
_简短描述性标题，5-10 词，信息密集_

# Current State
_当前正在做什么？待完成的任务。下一步行动。_

# Task specification
_用户要求做什么？设计决策和解释性上下文_

# Files and Functions
_重要文件有哪些？简述其内容和相关性_

# Workflow
_常用 bash 命令及执行顺序？如何解释输出？_

# Errors & Corrections
_遇到的错误及修复方法。用户纠正了什么？_

# Codebase and System Documentation
_重要系统组件有哪些？它们如何工作/组合？_

# Learnings
_什么有效？什么无效？应避免什么？_

# Key results
_用户要求的具体输出（答案、表格等）_

# Worklog
_尝试和完成的步骤，简洁记录_
```

### 6.4 Token 限制

- 单节限制: **2000 tokens**
- 总限制: **12000 tokens**

### 6.5 工作流程

```
Post-Sampling Hook
    │
    ├─► 检查触发条件
    │       ├─ Token 增量 > 5K
    │       └─ Tool Calls > 3 或自然断点
    │
    ├─► 启动 Forked Agent
    │       ├─ maxTurns: 1
    │       └─ 工具权限: 只能编辑 session-memory.md
    │
    └─► 后台更新文件
            └─ 不阻塞主流程
```

### 6.6 工具权限控制

Forked Agent 执行时的权限限制：

```rust
fn create_memory_can_use_tool(memory_path: &Path) -> CanUseToolFn {
    Box::new(move |tool: &Tool, input: &Value| {
        // 只允许 Edit 特定的 memory 文件
        if tool.name() == "file_edit" {
            if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
                if path == memory_path.to_str().unwrap() {
                    return ToolDecision::Allow;
                }
            }
        }
        ToolDecision::Deny("only file_edit on session-memory.md allowed".into())
    })
}
```

---

## 七、Layer 4: 完整压缩

### 7.1 核心思想

当上下文接近限制时，使用 LLM 生成对话摘要。支持多种触发方式和恢复机制。

### 7.2 压缩配置

```rust
pub struct LLMCompactConfig {
    pub token_threshold: usize,        // 默认 100,000
    pub keep_recent_messages: usize,   // 默认 2
    pub max_output_tokens: usize,      // 默认 12,000
}
```

### 7.3 触发方式

| 方式 | 触发条件 | 实现状态 | 说明 |
|------|----------|----------|------|
| **自动压缩** | Token 超过阈值 | ✅ 已实现 | 由 Agent 自动触发 |
| **手动压缩** | 用户请求 | ❌ 未实现 | 显式调用 `/compact` (待开发) |
| **部分压缩** | 选择特定消息 | ❌ 未实现 | 压缩用户选择的范围 (待开发) |

当前仅支持自动压缩，手动压缩和部分压缩为计划功能。

### 7.4 压缩结果结构

```rust
pub struct CompactionResult {
    pub boundary_marker: Message,       // 边界标记
    pub summary_messages: Vec<Message>, // 摘要消息
    pub attachments: Vec<Attachment>,   // 附件 (文件/技能恢复)
    pub hook_results: Vec<Message>,     // Hook 结果
    pub messages_to_keep: Vec<Message>, // 保留的消息
    pub pre_compact_token_count: usize,
    pub post_compact_token_count: usize,
}
```

### 7.5 Post-Compact 恢复机制

压缩完成后，需要恢复关键上下文，避免 AI "失忆"。恢复分为三部分：

```
┌─────────────────────────────────────────────────────────────┐
│                    Post-Compact 恢复机制                     │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  1. 文件恢复 (50,000 tokens)                                 │
│     └─ 最近读取的文件摘要                                    │
│                                                             │
│  2. 技能恢复 (25,000 tokens)                                 │
│     └─ 已加载的技能摘要                                      │
│                                                             │
│  3. Session Memory 恢复 (12,000 tokens)                     │
│     └─ 当前会话的 Layer 3 记忆                               │
│                                                             │
│  总预算: 87,000 tokens                                       │
└─────────────────────────────────────────────────────────────┘
```

#### 恢复预算配置

```rust
/// 文件恢复预算
pub const MAX_FILE_RECOVERY_TOKENS: usize = 50_000;
pub const MAX_SINGLE_FILE_TOKENS: usize = 5_000;   // 单文件上限
pub const MAX_FILES_TO_RECOVER: usize = 5;         // 最多 5 个文件

/// 技能恢复预算
pub const MAX_SKILL_RECOVERY_TOKENS: usize = 25_000;
pub const MAX_SINGLE_SKILL_TOKENS: usize = 5_000;   // 单技能上限

/// Session Memory 恢复预算
pub const MAX_SESSION_MEMORY_RECOVERY_TOKENS: usize = 12_000;
```

#### 1. 文件恢复

恢复最近读取的文件内容摘要，避免 AI 忘记已读取的代码：

```
## Files Previously Read

### src/runtime.rs
```
AgentRuntime 主循环实现
- 处理用户消息
- 调用工具
- 管理会话状态
```

### src/memory_system/mod.rs
```
7 层记忆系统集成
- Layer 1-7 状态管理
- 后台任务协调
```
```

**恢复流程**：
1. `FileTracker` 记录所有 `read_file` 调用
2. 按时间倒序排列
3. 截断到 Token 预算内
4. 生成恢复消息

#### 2. 技能恢复

恢复已加载的技能摘要，避免重新加载：

```
## Skills Previously Loaded

### code-review
```
代码审查技能
- 检查 CLAUDE.md 合规性
- 扫描安全漏洞
- 生成审查报告
```

### rust-router
```
Rust 错误诊断路由
- Layer 1: 编译错误
- Layer 3: 领域约束
- 推荐解决方案
```
```

**恢复流程**：
1. `SkillTracker` 记录所有加载的技能
2. 按最近使用排序
3. 截断到 Token 预算内
4. 生成恢复消息

#### 3. Session Memory 恢复

恢复当前会话的 Layer 3 记忆，这是最关键的恢复：

```
## Session Memory

# Session Title
_Implementing 7-layer memory system for BlockCell_

# Current State
_Implementing Post-Compact recovery mechanism_
- File recovery: ✅ Done
- Skill recovery: ✅ Done  
- Session Memory recovery: 🔄 In progress

# Task specification
_User wants detailed documentation for memory system_
- Design document at docs/analysis/blockcell-memory-system-design.md
- Target audience: developers new to the project

# Files and Functions
- crates/agent/src/compact/mod.rs - Main compact logic
- crates/agent/src/compact/recovery.rs - Recovery implementation

# Errors & Corrections
- Initial design used inline tool results, corrected to truncation
- Memory injection is to System Prompt, not separate

# Key results
- 7 layers: Tool Result Storage → Micro Compact → Session Memory → 
  Full Compact → Auto Memory → Dream → Forked Agent
```

**为什么恢复 Session Memory？**

| 没有恢复 | 有恢复 |
|----------|--------|
| AI 忘记当前任务目标 | AI 记得正在做什么 |
| AI 不知道已读取哪些文件 | AI 知道文件上下文 |
| AI 重复犯错 | AI 记得错误修正 |
| AI 丢失关键决策 | AI 保持决策连贯性 |

**恢复流程**：
1. 读取 `sessions/{session_id}/memory.md`
2. 截断到 12,000 tokens
3. 追加到恢复消息末尾

#### 完整压缩消息结构

压缩后的消息由**两部分**组成：LLM 生成的摘要 + 系统构建的恢复信息。

```
┌─────────────────────────────────────────────────────────────┐
│                  Compact 后的完整消息                        │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  Part 1: LLM 生成的摘要 (summary_message)                   │
│  ────────────────────────────────────────                   │
│  # Conversation Compacted                                   │
│                                                             │
│  ## Session Title                                           │
│  _实现 BlockCell 7 层记忆系统_                               │
│                                                             │
│  ## Current State                                           │
│  _正在实现 Post-Compact 恢复机制_                            │
│  - 文件恢复: ✅ 完成                                         │
│  - Session Memory 恢复: 🔄 进行中                            │
│                                                             │
│  ## Task specification                                      │
│  _用户要求详细解释恢复机制_                                   │
│                                                             │
│  ## Key decisions                                           │
│  - 使用 12,000 tokens 预算恢复 Session Memory                │
│  - 恢复消息追加在摘要之后                                     │
│                                                             │
│  ─────────────────────────────────────────                  │
│                                                             │
│  Part 2: 系统构建的恢复信息 (recovery_message)               │
│  ────────────────────────────────────────                   │
│  ---                                                        │
│                                                             │
│  ## Files Previously Read                                   │
│                                                             │
│  ### src/compact/mod.rs                                     │
│  ```                                                        │
│  Compact 核心模块                                            │
│  - should_compact(): 检查是否需要压缩                        │
│  - build_recovery_message(): 构建恢复消息                    │
│  ```                                                        │
│                                                             │
│  ### src/compact/recovery.rs                                │
│  ```                                                        │
│  恢复机制实现                                                 │
│  - FileRecoveryState: 文件恢复状态                           │
│  - SkillRecoveryState: 技能恢复状态                          │
│  ```                                                        │
│                                                             │
│  ## Skills Previously Loaded                                │
│                                                             │
│  ### rust-router                                            │
│  ```                                                        │
│  Rust 错误诊断路由技能                                        │
│  - Layer 1: 编译错误分析                                     │
│  - Layer 3: 领域约束分析                                     │
│  ```                                                        │
│                                                             │
│  ## Session Memory                                          │
│                                                             │
│  # Session Title                                            │
│  _实现 7 层记忆系统_                                         │
│                                                             │
│  # Current State                                            │
│  _Post-Compact 恢复机制实现中_                               │
│                                                             │
│  # Files and Functions                                      │
│  - crates/agent/src/compact/mod.rs                          │
│  - crates/agent/src/compact/recovery.rs                     │
│                                                             │
│  # Errors & Corrections                                     │
│  - Session Memory 恢复预算从 8K 增加到 12K                   │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

**消息生成代码**：

```rust
pub fn to_compact_message(&self) -> String {
    let mut message = String::new();

    // Part 1: LLM 生成的摘要
    if !self.summary_message.is_empty() {
        message.push_str("# Conversation Compacted\n\n");
        message.push_str(&self.summary_message);
    }

    // 分隔线
    if !self.recovery_message.is_empty() {
        message.push_str("\n\n---\n\n");
    }

    // Part 2: 系统构建的恢复信息
    message.push_str(&self.recovery_message);

    message
}
```

### 7.6 工作流程

```
Token 超限检测
    │
    ├─► 执行 Pre-Compact Hooks
    │       └─ 收集自定义压缩指令
    │
    ├─► 收集恢复数据
    │       ├─ FileTracker.get_recent_files()
    │       ├─ SkillTracker.get_recent_skills()
    │       └─ 读取 Session Memory 文件
    │
    ├─► 调用 LLM 生成摘要
    │       └─ Forked Agent (maxTurns: 1, 无工具权限)
    │
    ├─► 构建恢复消息
    │       ├─ 文件恢复 (≤50,000 tokens)
    │       ├─ 技能恢复 (≤25,000 tokens)
    │       └─ Session Memory 恢复 (≤12,000 tokens)
    │
    ├─► 清理状态缓存
    │       └─ readFileState.clear()
    │
    └─► 返回压缩结果
            ├─ 摘要消息
            └─ 恢复消息
```

---

## 八、Layer 5: 自动记忆提取

### 8.1 核心思想

在查询循环结束时，自动从对话中提取持久化记忆。使用 Forked Agent 在后台运行，用户无需主动调用。

### 8.2 四种记忆类型

| 类型 | 文件 | 触发条件 |
|------|------|----------|
| **user** | user.md | 了解用户角色、偏好、职责、知识时 |
| **project** | project.md | 了解谁在做什么、为什么、什么时候 |
| **feedback** | feedback.md | 用户纠正做法或确认非显而易见方法有效 |
| **reference** | reference.md | 了解外部系统的资源及其用途 |

#### 记忆类型详解

**1. User 记忆（用户画像）**

记录用户的角色、偏好、知识背景，帮助 AI 更好地理解用户需求。

```
示例场景：
用户说："我是数据科学家，正在分析日志系统"
AI 记录：
  - 角色: 数据科学家
  - 当前工作: 日志分析
  - 偏好: 倾向于技术性解释
```

**2. Project 记忆（项目上下文）**

记录项目的目标、进展、关键决策，保持跨会话的项目连贯性。

```
示例场景：
用户说："这个功能下周四发布"
AI 记录：
  - 事件: 功能发布
  - 时间: 下周四
  - 重要性: 高优先级
```

**3. Feedback 记忆（用户反馈）**

记录用户的纠正和指导，避免重复犯错。

```
示例场景：
用户说："不要用 try-catch，我们用 ? 操作符处理错误"
AI 记录：
  - 偏好: Rust 错误处理使用 ? 操作符
  - 避免: try-catch 模式
```

**4. Reference 记忆（外部资源）**

记录外部系统的位置和用途，方便后续查找。

```
示例场景：
用户说："错误日志在 Grafana 的 /d/api-latency 面板"
AI 记录：
  - 资源: Grafana 面板
  - 路径: /d/api-latency
  - 用途: API 延迟监控
```

### 8.3 存储路径

```
.blockcell/memory/
├── user.md          # 用户角色、偏好、知识背景
├── project.md       # 项目工作、目标、事件
├── feedback.md      # 用户纠正、工作指导
└── reference.md     # 外部系统资源指针
```

### 8.4 触发机制

```
每轮对话结束 (Post-Sampling Hook)
    │
    ├─► 检查主代理是否已写入记忆 (避免重复)
    │       └─ 如果已写入 → 推进游标，跳过
    │
    ├─► 扫描现有记忆文件
    │       └─ 作为提取上下文
    │
    ├─► 构建提取提示
    │       └─ 包含现有记忆
    │
    └─► Forked Agent 执行
            ├─ maxTurns: 5
            ├─ skipTranscript: true
            └─ 工具权限: 只读 + 写 memory 目录
```

### 8.5 工具权限矩阵

| 工具 | 权限 | 条件 |
|------|------|------|
| REPL | ✅ 允许 | 无限制 |
| Read/Grep/Glob | ✅ 允许 | 无限制 |
| Bash | ✅ 允许 | 仅只读命令 |
| Edit/Write | ✅ 允许 | 仅 memory 目录内 |
| 其他 | ❌ 禁止 | - |

### 8.6 记忆文件格式

```markdown
---
name: user_role
description: 用户是数据科学家，关注可观测性
type: user
---

用户在 [公司] 担任数据科学家角色，目前重点关注日志和可观测性系统。
主要技术栈：Python, SQL, Spark。
对前端代码较陌生。
```

### 8.7 记忆注入机制

Layer 5 不仅负责提取记忆，还负责在**新对话开始时**将持久化记忆注入到 System Prompt 中。

#### System Prompt 结构

两种来源的记忆都会被追加到 System Prompt 末尾：

```
┌─────────────────────────────────────────────────────────────┐
│                    System Prompt 完整结构                    │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  基础部分 (ContextBuilder 构建)                              │
│  │                                                          │
│  ├─► 角色定义 (硬编码)                                       │
│  │       └─ "You are blockcell, an AI assistant..."         │
│  │                                                          │
│  ├─► 用户配置文件                                            │
│  │       ├─ AGENTS.md → ## Agent Guidelines                 │
│  │       ├─ SOUL.md → ## Personality                        │
│  │       └─ USER.md → ## User Preferences                   │
│  │                                                          │
│  ├─► 工具规则                                                │
│  │       ├─ 硬编码规则（工具使用规范）                         │
│  │       └─ tool_prompt_rules（动态规则）                    │
│  │                                                          │
│  ├─► 运行时上下文                                            │
│  │       ├─ 当前时间                                         │
│  │       └─ 工作目录                                         │
│  │                                                          │
│  └─► 技能卡片 (如果激活技能)                                  │
│          └─ active_skill.prompt_md                          │
│                                                             │
│  ─────────────────────────────────────────                  │
│                                                             │
│  记忆部分 (Memory Sections)                                  │
│  │                                                          │
│  ├─► SQLite 记忆 (保留，通过语义搜索检索)                     │
│  │   │                                                      │
│  │   └─ ## Memory Brief (SQLite FTS5 Search)                │
│  │       ├─ long_term: fact, preference, project...         │
│  │       └─ short_term: note, snippet, contact...           │
│  │                                                          │
│  └─► Layer 5 持久化记忆 (新增，追加到末尾)                    │
│      │                                                      │
│      └─ # 持久化记忆                                        │
│          ├─ user.md: 用户画像                               │
│          ├─ project.md: 项目上下文                          │
│          ├─ feedback.md: 用户反馈                           │
│          └─ reference.md: 外部资源                          │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

#### 配置文件位置

基础部分的用户配置文件位于 BlockCell 配置目录：

```
~/.blockcell/
├── AGENTS.md     # Agent 指导原则（如何作为 AI 助手行事）
├── SOUL.md       # 个性化设定（性格、语气、风格）
├── USER.md       # 用户偏好（沟通方式、专业领域）
└── memory/       # Layer 5 持久化记忆目录
    ├── user.md
    ├── project.md
    ├── feedback.md
    └── reference.md
```

**区别**：
| 文件 | 用途 | 内容示例 |
|------|------|----------|
| `USER.md` | System Prompt 基础部分 | "用户喜欢简洁的回答" |
| `memory/user.md` | Layer 5 持久化记忆 | "用户是数据科学家，专注于日志分析" |

#### 注入流程

```
新对话开始
    │
    ├─► 初始化 MemoryInjector
    │       └─ 加载 memory/*.md 文件
    │
    ├─► ContextBuilder.build_system_prompt_for_mode_with_channel()
    │       │
    │       ├─► 构建基础部分
    │       │       ├─ 角色定义
    │       │       ├─ 工具定义
    │       │       └─ 时间、目录等上下文
    │       │
    │       ├─► 追加 SQLite 记忆
    │       │       └─ MemoryStore.generate_brief_for_query()
    │       │           └─ 语义搜索检索相关记忆
    │       │
    │       └─► 追加 Layer 5 持久化记忆
    │               └─ MemoryInjector.build_injection_content()
    │                   └─ 加载四种记忆文件
    │
    └─► 最终 System Prompt
            ├─ 基础部分
            ├─ SQLite 记忆 (Memory Brief)
            └─ Layer 5 持久化记忆
```

#### 注入示例

```markdown
---
# 持久化记忆

> 以下是系统自动提取并持久化的跨会话记忆：

## User (user.md)

用户在数据科学领域有丰富经验，偏好简洁的技术解释。
主要使用 Python 和 SQL，对 Rust 正在学习中。

## Project (project.md)

当前项目: BlockCell 多智能体框架
核心目标: 实现自进化的 AI Agent 系统
技术栈: Rust, Tokio, Axum, SQLite

## Feedback (feedback.md)

用户偏好:
- 使用 ? 操作符处理 Rust 错误
- 避免过度抽象，保持代码简洁
- 函数命名使用 snake_case

## Reference (reference.md)

关键资源:
- Grafana 面板: /d/api-latency (API 延迟监控)
- 日志系统: Elasticsearch 集群
- 文档站点: docs.blockcell.io

---
```

#### 为什么保留两种注入方式？

| 注入方式 | 特点 | 适用场景 |
|----------|------|----------|
| **SQLite 记忆** | 结构化存储，支持 FTS5 全文搜索 | 快速检索、语义查询 |
| **Layer 5 记忆** | Markdown 文件，人类可读 | 长期保存、跨会话上下文 |

**协同工作**：
- SQLite 记忆提供快速检索能力
- Layer 5 记忆提供稳定的跨会话上下文
- 两者互补，不冲突

### 9.1 核心思想

定期在后台整合记忆，跨会话提取知识。三个门控条件确保不会过度消耗资源。

### 9.2 三重门控机制

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                          Layer 6 门控机制                                    │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  Gate 1: 时间门控                                                           │
│  ├─ 条件: 距上次整合 > 24小时                                                │
│  └─ 目的: 避免频繁整合消耗资源                                               │
│                                                                             │
│  Gate 2: 会话门控                                                           │
│  ├─ 条件: 新会话数 > 5                                                      │
│  └─ 目的: 确保有足够的新材料值得整合                                         │
│                                                                             │
│  Gate 3: 锁门控                                                             │
│  ├─ 条件: 无其他进程正在整合                                                 │
│  └─ 目的: 防止并发冲突                                                      │
│                                                                             │
│  所有门控通过 → 执行整合                                                     │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 9.3 核心配置

```rust
pub struct DreamConfig {
    pub min_hours: u64,      // 默认 24
    pub min_sessions: u64,   // 默认 5
}

// 扫描节流间隔
const SESSION_SCAN_INTERVAL_MS: u64 = 10 * 60 * 1000; // 10 分钟
```

### 9.4 整合流程

#### 记忆类型说明

在理解整合流程前，需要明确两种记忆的区别：

| 记忆类型 | 来源 | 存储位置 | 生命周期 |
|----------|------|----------|----------|
| **Session Memory（会话记忆）** | Layer 3 生成 | `sessions/{session_id}/memory.md` | 单次会话，会话结束后可清理 |
| **持久化记忆（长期记忆）** | Layer 5 提取，Layer 6 整合 | `memory/{user,project,feedback,reference}.md` | 跨会话永久保存 |

```
┌─────────────────────────────────────────────────────────────┐
│                    记忆流动路径                              │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  用户对话 (Layer 0)                                          │
│       ↓                                                     │
│  Layer 3: 生成 Session Memory                               │
│       ├─ 存储到 sessions/{id}/memory.md                     │
│       └─ 包含：当前状态、任务、文件、错误、学习等            │
│                                                             │
│  Layer 6: 定期整合 (Dream)                                  │
│       ├─ 读取 Session Memory 文件                           │
│       ├─ 提取有价值的信号                                    │
│       └─ 合并到持久化记忆 (memory/*.md)                      │
│                                                             │
│  Layer 5: 下次对话时                                        │
│       └─ 注入持久化记忆到系统提示                            │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

#### 四阶段执行详解

**Phase 1 — Orient（定位）**

了解当前记忆状态，建立整合基线。

```bash
# Forked Agent 执行的操作
ls memory/              # 查看现有记忆文件
cat memory/user.md      # 读取用户画像
cat memory/project.md   # 读取项目上下文
```

**Phase 2 — Gather（收集信号）**

从 Session Memory 文件中提取有价值的信息片段（称为"信号"）。

```
sessions/
├── session-001/memory.md  → 提取信号：用户偏好、项目进展...
├── session-002/memory.md  → 提取信号：新发现、错误修复...
└── session-003/memory.md  → 提取信号：关键决策、学习成果...
```

信号重要性评分（1-10分）：
- 用户偏好纠正：8-10 分
- 项目关键决策：7-9 分
- 错误修复方案：6-8 分
- 一般性工作记录：3-5 分

**Phase 3 — Consolidate（整合）**

将收集的信号合并到持久化记忆文件中。

```markdown
# 整合操作示例

## Merge（合并）
原 user.md: "用户喜欢简洁的代码风格"
新信号: "用户偏好 Rust 的 ? 操作符处理错误"
→ 合并后: "用户喜欢简洁的代码风格，偏好 Rust 的 ? 操作符处理错误"

## Delete（删除）
原 project.md: "功能 A 计划下周发布"
新信号: "功能 A 已取消"
→ 删除过时信息

## Promote（升级）
Session Memory 中的信号: "用户是数据科学家，正在分析日志系统"
→ 升级到 user.md: 添加到用户画像
```

**Phase 4 — Prune（修剪）**

清理过期的 Session Memory 文件，保持存储整洁。

```bash
# 删除超过 7 天的 Session Memory
find sessions/ -name "memory.md" -mtime +7 -delete
```

#### 完整流程图

```
门控检查通过
    │
    ├─► Phase 1: Orient（定位）
    │       ├─ 读取 memory/ 目录现有内容
    │       └─ 建立当前记忆索引
    │
    ├─► Phase 2: Gather（收集信号）
    │       ├─ 扫描 sessions/*/memory.md 文件
    │       ├─ 按修改时间排序（最新优先）
    │       ├─ 提取各章节的有价值信息
    │       └─ 计算信号重要性分数
    │
    ├─► Phase 3: Consolidate（整合）
    │       ├─ Forked Agent 执行整合提示
    │       ├─ Merge: 合并相似记忆
    │       ├─ Delete: 删除过时记忆
    │       └─ Promote: 升级会话记忆为持久记忆
    │
    └─► Phase 4: Prune（修剪）
            ├─ 清理过期的 Session Memory
            └─ 优化记忆索引结构
```

#### 持久化记忆的使用

整合后的持久化记忆在下次对话时会被注入到系统提示中：

```
用户发起新对话
    │
    └─► Layer 5: MemoryInjector
            ├─ 读取 memory/user.md
            ├─ 读取 memory/project.md
            ├─ 读取 memory/feedback.md
            ├─ 读取 memory/reference.md
            └─ 注入到系统提示末尾
                    │
                    ↓
            ┌─────────────────────────────────┐
            │ System Prompt                    │
            │ ...                              │
            │ ---                              │
            │ # 持久化记忆                      │
            │ ## User (user.md)               │
            │ 用户偏好...                       │
            │ ## Project (project.md)         │
            │ 项目上下文...                     │
            │ ---                              │
            └─────────────────────────────────┘
```

### 9.5 锁机制

```rust
/// 整合锁
pub struct ConsolidationLock {
    pub holder_pid: Option<u32>,        // 持有者进程 ID
    pub acquired_at: Option<DateTime>,  // 获取时间
    pub last_consolidated_at: DateTime, // 上次整合时间
}

// 锁过期时间: 1 小时
// 进程检测: 防止僵尸锁
```

### 9.6 DreamTask 追踪

```rust
pub struct DreamTask {
    pub id: String,
    pub phase: DreamPhase,           // Starting | Updating | Completed | Failed
    pub sessions_reviewing: usize,   // 正在审查的会话数
    pub files_touched: Vec<String>,  // 被修改的文件
    pub turns: Vec<DreamTurn>,       // Agent 回复历史 (最多 30 条)
    pub abort_controller: AbortController,
}
```

---

## 十、Layer 7: Forked Agent

### 10.1 核心思想

Forked Agent 是实现多智能体协作的核心机制。

#### 为什么需要 Forked Agent？

在 AI Agent 运行过程中，很多任务适合"后台执行"，例如：

```
用户: "帮我分析一下最近的错误日志"
  │
  ├─► 主 Agent: 继续与用户对话，响应用户的新问题
  │
  └─► Forked Agent (后台): 执行日志分析任务
          ├─ 读取日志文件
          ├─ 分析错误模式
          └─ 生成报告（异步完成）
```

**核心问题**：如果直接在主流程中执行这些任务：
1. 用户等待时间长，体验差
2. 占用主对话的 Token 预算
3. 无法并发处理多个任务

**解决方案**：Forked Agent = "派生一个子代理独立执行"

```
┌─────────────────────────────────────────────────────────────┐
│                    Agent 执行模型                            │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│  主 Agent (前台)                                            │
│  ├─ 处理用户对话                                            │
│  ├─ 响应用户请求                                            │
│  └─ 可以随时创建 Forked Agent                               │
│                                                             │
│  Forked Agent 1          Forked Agent 2                     │
│  ├─ 后台提取记忆          ├─ 后台整合知识                    │
│  ├─ 不阻塞主流程          ├─ 独立 Token 预算                 │
│  └─ 完成后通知主 Agent    └─ 完成后写入文件                  │
│                                                             │
└─────────────────────────────────────────────────────────────┘
```

#### 设计哲学

**1. 缓存优先 (Cache-First Design)**
- 子代理必须与父代理共享相同的缓存关键参数
- 通过 Prompt Cache 复用，大幅降低 API 成本和延迟

**2. 状态隔离 (State Isolation)**
- 所有可变状态默认隔离，防止干扰父代理循环
- 子代理拥有独立的文件状态缓存、权限追踪、中止控制器

**3. 用量追踪 (Usage Tracking)**
- 跨整个查询循环累积用量指标
- 支持缓存命中率计算和成本分析

### 10.2 CacheSafeParams

必须与父请求完全一致的参数：

```rust
pub struct CacheSafeParams {
    pub system_prompt: Arc<SystemPrompt>,           // 系统提示
    pub user_context: HashMap<String, String>,      // 用户上下文
    pub system_context: HashMap<String, String>,    // 系统上下文
    pub tool_context: Arc<ToolContext>,             // 工具上下文
    pub fork_context_messages: Vec<Message>,        // 父对话消息
}
```

### 10.3 缓存键组成

| 组成部分 | 来源 | 说明 |
|----------|------|------|
| **System Prompt** | `cache_safe_params.system_prompt` | 系统提示的完整内容 |
| **Tools** | `cache_safe_params.tool_context.options.tools` | 工具定义列表 |
| **Model** | `cache_safe_params.tool_context.options.model` | 模型标识符 |
| **Messages Prefix** | `cache_safe_params.fork_context_messages` | 父代理的消息前缀 |
| **Thinking Config** | `tool_context.options.thinking_config` | 思维配置 |

### 10.4 状态隔离机制

```rust
pub fn create_subagent_context(
    parent_context: &ToolUseContext,
    overrides: Option<SubagentOverrides>,
) -> ToolUseContext {
    ToolUseContext {
        // 可变状态 - 克隆以保持隔离
        read_file_state: clone_file_state_cache(&parent_context.read_file_state),
        content_replacement_state: clone_content_replacement_state(
            &parent_context.content_replacement_state
        ),
        
        // 新建独立集合
        nested_memory_attachment_triggers: HashSet::new(),
        loaded_nested_memory_paths: HashSet::new(),
        tool_decisions: None,
        
        // AbortController - 新建子控制器
        abort_controller: create_child_abort_controller(&parent_context.abort_controller),
        
        // 状态访问 - 包装以避免权限提示
        get_app_state: wrap_app_state_for_subagent(&parent_context.get_app_state),
        set_app_state: Arc::new(|_| {}), // 默认 no-op
        
        // 继承不变的选项
        options: overrides.options.unwrap_or_else(|| parent_context.options.clone()),
        
        // 新的查询追踪链
        query_tracking: QueryTracking {
            chain_id: Uuid::new_v4(),
            depth: parent_context.query_tracking.depth + 1,
        },
        
        // 生成新的 agent ID
        agent_id: overrides.agent_id.unwrap_or_else(AgentId::new),
        
        ..parent_context.clone()
    }
}
```

### 10.5 共享选项

| 共享选项 | 默认值 | 用途 | 适用场景 |
|----------|--------|------|----------|
| `share_set_app_state` | `false` | 共享状态设置回调 | 交互式子代理 |
| `share_set_response_length` | `false` | 共享响应长度回调 | 子代理贡献父代理指标 |
| `share_abort_controller` | `false` | 共享中止控制器 | 子代理应随父代理一起中止 |

### 10.6 执行流程

```
┌─────────────────────────────────────────────────────────────────┐
│                     runForkedAgent 流程                          │
├─────────────────────────────────────────────────────────────────┤
│  1. 初始化                                                       │
│     ├─ outputMessages = []                                       │
│     └─ totalUsage = EMPTY_USAGE                                  │
│                                                                  │
│  2. 解析 CacheSafeParams                                         │
│     ├─ systemPrompt                                              │
│     ├─ userContext                                               │
│     ├─ toolContext                                               │
│     └─ forkContextMessages                                       │
│                                                                  │
│  3. 创建隔离上下文                                               │
│     createSubagentContext(toolUseContext, overrides)            │
│                                                                  │
│  4. 构建初始消息                                                 │
│     initialMessages = [...forkContextMessages, ...promptMessages]│
│                                                                  │
│  5. 查询循环                                                     │
│     for await (message of query({...}))                         │
│     ├─ 提取 stream_event 用量                                   │
│     ├─ 收集 outputMessages                                      │
│     └─ 调用 onMessage 回调                                      │
│                                                                  │
│  6. 清理资源                                                     │
│     ├─ readFileState.clear()                                    │
│     └─ initialMessages.clear()                                  │
│                                                                  │
│  7. 返回结果                                                     │
│     { messages: outputMessages, totalUsage }                    │
└─────────────────────────────────────────────────────────────────┘
```

---

## 十一、各层协作机制

### 11.1 触发时机汇总

| Layer | 触发时机 | 触发条件 | 执行方式 |
|-------|----------|----------|----------|
| **Layer 1** | 工具执行后 | 输出 > 50KB 或消息预算超限 | 同步 |
| **Layer 2** | Query 循环开始 | 时间间隔 > 60分钟 | 同步 |
| **Layer 3** | Post-Sampling | Token > 10K 初始化，之后增量 > 5K 且 Tool Calls > 3 | 异步 (Forked) |
| **Layer 4** | Token 超限 | 上下文接近模型限制 | 同步/异步 |
| **Layer 5** | Query 循环结束 | 每轮结束，间隔 1 轮 | 异步 (Forked) |
| **Layer 6** | 后台定时 | 时间 > 24h 且会话数 > 5 且获取锁 | 异步 (Forked) |
| **Layer 7** | 被动调用 | Layer 3/5/6 需要后台执行时 | - |

### 11.2 数据流分析

```
用户消息
    │
    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ Query Loop                                                                   │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  1. Pre-Sampling                                                            │
│     ├─ Layer 2: Micro Compact (时间触发检查)                                 │
│     └─ Layer 1: 预算检查 (applyToolResultBudget)                             │
│                                                                             │
│  2. LLM Sampling                                                            │
│     ├─ 构建请求 (含 Prompt Cache)                                           │
│     └─ 流式响应                                                             │
│                                                                             │
│  3. Tool Execution                                                          │
│     ├─ 执行工具                                                             │
│     └─ Layer 1: 大结果持久化 (processToolResultBlock)                        │
│                                                                             │
│  4. Post-Sampling Hooks                                                     │
│     ├─ Layer 3: Session Memory 提取 (maybeExtractSessionMemory)              │
│     ├─ Layer 5: 自动记忆提取 (executeExtractMemories)                        │
│     └─ Layer 4: Compact 检查 (maybeAutoCompact)                              │
│                                                                             │
│  5. Loop Check                                                              │
│     └─ 检查是否继续采样                                                      │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
    │
    ▼
响应完成
```

### 11.3 Layer 1 + Layer 2 协作

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    Layer 1 + Layer 2 协作                                    │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  工具执行后:                                                                 │
│  ├─ Layer 1: 检查单个工具输出                                               │
│  │   if output.size > threshold:                                           │
│  │     persist to disk                                                     │
│  │     replace with preview                                                │
│  │                                                                         │
│  ├─ Layer 1: 检查消息级别预算                                               │
│  │   if message.total_tool_results > 150KB:                               │
│  │     select largest results to persist                                  │
│  │     update ContentReplacementState                                     │
│  │                                                                         │
│  └─ Layer 2: 时间触发清理                                                   │
│      if time_since_last_assistant > 60min:                                 │
│        clear old tool results                                              │
│        preserve ContentReplacementState.seenIds                            │
│                                                                             │
│  关键: ContentReplacementState 在两层间共享，保证决策一致性                    │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 11.4 Layer 3 + Layer 4 协作

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    Layer 3 + Layer 4 协作                                    │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  正常对话流程:                                                               │
│  ├─ Layer 3: 持续更新 Session Memory                                        │
│  │   - 每 Token 增量 > 5K 检查一次                                          │
│  │   - Forked Agent 后台提取                                                │
│  │   - 更新 session-memory.md 文件                                          │
│  │                                                                         │
│  Token 超限触发压缩:                                                         │
│  ├─ Layer 4: 执行压缩                                                       │
│  │   1. 保存 FileTracker 状态                                               │
│  │   2. 调用 LLM 生成摘要                                                   │
│  │   3. 清除 readFileState                                                  │
│  │   4. 创建恢复消息                                                        │
│  │                                                                         │
│  └─ Layer 3: Session Memory 作为恢复依据                                    │
│      - 读取 "Current State" section                                         │
│      - 恢复工作上下文                                                       │
│      - 文件恢复消息 + Session Memory 组合                                   │
│                                                                             │
│  关键: Session Memory 是 Compact 后恢复工作状态的关键数据源                   │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 11.5 Layer 5 + Layer 6 协作

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    Layer 5 + Layer 6 协作                                    │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  对话结束:                                                                   │
│  └─ Layer 5: 自动记忆提取                                                   │
│      - 分析对话内容                                                         │
│      - 分类到 4 种记忆类型                                                  │
│      - 写入 .blockcell/memory/*.md                                          │
│                                                                             │
│  多次对话后:                                                                 │
│  └─ Layer 6: Auto Dream                                                     │
│      - 扫描所有记忆文件                                                     │
│      - 合并相似记忆                                                         │
│      - 删除过时记忆                                                         │
│      - 提取跨会话模式                                                       │
│                                                                             │
│  数据流:                                                                     │
│  Layer 5 输出 ────────────────────────────────────▶ Layer 6 输入            │
│  (单会话记忆文件)                                    (跨会话整合)            │
│                                                                             │
│  关键: Layer 5 提供原始材料，Layer 6 进行深加工                               │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 11.6 Layer 7 与所有层的协作

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                    Layer 7 作为基础设施                                       │
├─────────────────────────────────────────────────────────────────────────────┤
│                                                                             │
│  ┌─────────────────────────────────────────────────────────────────────┐   │
│  │ Layer 7: Forked Agent                                                │   │
│  │                                                                      │   │
│  │   提供的能力:                                                         │   │
│  │   ├─ 状态隔离 (克隆可变状态)                                         │   │
│  │   ├─ Prompt Cache 共享 (CacheSafeParams)                             │   │
│  │   ├─ 工具权限控制 (canUseTool 函数)                                  │   │
│  │   └─ 用量追踪 (totalUsage)                                           │   │
│  │                                                                      │   │
│  └─────────────────────────────────────────────────────────────────────┘   │
│                                    │                                        │
│          ┌─────────────────────────┼─────────────────────────┐             │
│          │                         │                         │             │
│          ▼                         ▼                         ▼             │
│  ┌───────────────┐         ┌───────────────┐         ┌───────────────┐    │
│  │ Layer 3       │         │ Layer 5       │         │ Layer 6       │    │
│  │ Session Memory│         │ Extract Mem   │         │ Auto Dream    │    │
│  │               │         │               │         │               │    │
│  │ forkLabel:    │         │ forkLabel:    │         │ forkLabel:    │    │
│  │ session_memory│         │ extract_mem   │         │ auto_dream    │    │
│  │               │         │               │         │               │    │
│  │ maxTurns: 1   │         │ maxTurns: 5   │         │ maxTurns: -   │    │
│  │               │         │               │         │               │    │
│  │ canUseTool:   │         │ canUseTool:   │         │ canUseTool:   │    │
│  │ 仅允许编辑    │         │ 只读 + 写     │         │ 只读 + 写     │    │
│  │ session-memory│         │ memory 目录   │         │ memory 目录   │    │
│  └───────────────┘         └───────────────┘         └───────────────┘    │
│                                                                             │
│  共享机制:                                                                   │
│  - 所有 Forked Agent 共享相同的 CacheSafeParams                             │
│  - 消息前缀相同 → Prompt Cache 命中                                          │
│  - 缓存命中率可达 98%                                                        │
│                                                                             │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 11.7 存储结构汇总

```
.blockcell/
├── sessions/
│   └── <session-id>/
│       ├── tool-results/           # Layer 1: 工具结果存储
│       │   ├── <tool-use-id>.json
│       │   └── <tool-use-id>.txt
│       ├── session-memory.md       # Layer 3: 会话记忆
│       └── transcript.jsonl        # 会话记录
│
├── memory/                         # Layer 5: 自动记忆
│   ├── user.md                     # 用户记忆
│   ├── project.md                  # 项目记忆
│   ├── feedback.md                 # 反馈记忆
│   └── reference.md                # 引用记忆
│
└── .dream_lock                     # Layer 6: 梦境锁文件
```

---

## 十二、新旧逻辑对比

### 12.1 核心变化

**已删除的旧逻辑**: `HistoryProjector.project()` 基于轮次截断的方式

**新逻辑**: Layer 4 (LLM 语义压缩)

### 12.2 详细对比

| 特性 | 旧逻辑 (HistoryProjector) | 新逻辑 (7层记忆系统) |
|------|---------------------------|----------------------|
| **压缩机制** | 规则裁剪/轮次截断 | LLM 语义压缩 |
| **LLM 调用** | 无 | 有 |
| **输出格式** | 截断消息列表 | 9-part structured summary |
| **恢复机制** | 无 | Post-Compact 恢复 |
| **信息完整性** | 可能丢失重要上下文 | 保留关键信息 |
| **缓存支持** | 破坏缓存 | 保持缓存一致性 |
| **Token 预算** | 无明确预算 | 每层有预算控制 |

### 12.3 架构变化

```
旧架构:
┌─────────────────────────────────────────────────────────────────┐
│  消息历史                                                        │
│  ├─ 轮次 1-N                                                     │
│  ├─ 轮次 N+1-M (可能被截断)                                       │
│  └─ 轮次 M+1-最新                                                │
└─────────────────────────────────────────────────────────────────┘

新架构:
┌─────────────────────────────────────────────────────────────────┐
│  Layer 1: 工具结果持久化 (减少消息大小)                           │
│  Layer 2: 时间触发清理 (无 LLM 成本)                             │
│  Layer 3: 实时会话摘要 (后台异步)                                 │
│  Layer 4: LLM 语义压缩 (保留关键信息)                             │
│  Layer 5: 自动记忆提取 (跨会话持久化)                             │
│  Layer 6: 跨会话整合 (知识融合)                                   │
│  Layer 7: Forked Agent (异步基础设施)                             │
└─────────────────────────────────────────────────────────────────┘
```

### 12.4 代码位置变化

| 功能 | 旧位置 | 新位置 |
|------|--------|--------|
| 上下文压缩 | `HistoryProjector.project()` | `compact/llm_compactor.rs` |
| 工具结果处理 | 内联处理 | `response_cache.rs` |
| 会话记忆 | 无 | `session_memory/` |
| 自动记忆 | 无 | `auto_memory/` |
| 梦境整合 | 无 | `consolidator.rs` |
| 后台任务 | Ghost 服务 | Forked Agent |

### 12.5 迁移影响

1. **API 兼容性**: 完全兼容，内部实现变化
2. **存储格式**: 新增 Markdown 文件，不修改 SQLite
3. **配置变化**: 新增 7 层相关配置项
4. **性能影响**: 
   - Layer 1-2: 无额外开销
   - Layer 3-6: 后台异步，不阻塞主流程
   - Layer 4: LLM 调用开销，但保留关键信息

---

## 十三、未来 TODO

### 1. Layer 1 增强

**支持更多工具类型的预览优化**

当前预览生成仅使用换行边界截断，对于不同类型的工具输出（如 JSON 数组、表格数据、代码块）缺乏针对性优化。需要根据内容类型选择最佳预览策略：
- JSON: 在对象边界截断，保持语法有效
- 表格: 保留完整行，添加省略标记
- 代码: 在函数/类边界截断，保持缩进

**智能预览生成 (基于内容类型)**

分析工具输出的 MIME 类型或结构特征，自动选择最佳预览策略。例如，`web_fetch` 返回的 HTML 可提取关键段落，`shell` 返回的日志可保留错误信息。

**压缩预览存储**

对于超大输出（如 10MB+ 的文件列表），预览本身也可能较大。需要考虑预览的压缩存储，如使用 gzip 压缩，或存储结构化摘要而非原始文本。

---

### 2. Layer 4 增强

**部分压缩优化 (选择最佳分割点)**

当前压缩是全量压缩，无法选择特定消息范围。需要支持部分压缩，让用户选择压缩哪些消息，或自动识别最佳分割点（如按任务边界分割）。

**多模型压缩支持**

不同模型有不同的上下文窗口和压缩偏好。需要支持根据当前使用的模型动态调整压缩策略，如针对小模型使用更激进的压缩。

**压缩质量评估指标**

当前缺乏压缩质量的可量化评估。需要引入指标如：
- 信息保留率（关键实体覆盖率）
- 摘要连贯性评分
- 恢复后任务成功率

---

### 3. Layer 5 增强

**记忆去重算法优化**

当前记忆提取可能产生重复或高度相似的条目。需要实现更智能的去重算法：
- 语义相似度检测（使用 embedding）
- 增量更新而非新增条目
- 合并相似记忆的自动化策略

**记忆重要性评分**

所有记忆同等对待，缺乏优先级。需要引入重要性评分机制：
- 访问频率（被引用次数）
- 时效性（最近更新时间）
- 相关性（与当前任务的关联度）

**团队记忆支持**

当前记忆系统仅支持单用户。团队记忆需要：
- 共享记忆目录
- 权限控制（读写分离）
- 记忆来源标识

---

### 4. Layer 5 MEMORY.md 索引文件

**功能说明**:

MEMORY.md 是记忆目录的索引文件，列出所有记忆文件的摘要。格式示例：

```markdown
- [User Role](user.md) — 数据科学家，关注可观测性
- [Feedback Testing](feedback.md) — 集成测试用真实数据库
```

**作用**:
- 快速预览所有记忆摘要，无需打开每个文件
- 注入系统提示时可用索引替代完整内容，节省 Token
- 帮助 Agent 快速定位需要的记忆

**实现要点**:
- 扫描记忆目录，读取每个文件的 Frontmatter 元数据
- 按修改时间排序，限制最大行数（建议 200 行）
- 可通过配置开关跳过索引生成

---

### 5. Layer 6 增强

**更智能的整合触发条件**

当前三重门控较为固定（24小时 + 5会话）。需要更灵活的触发条件：
- 基于记忆累积量触发（如新增 50 条记忆）
- 基于时间分布触发（如工作日不触发）
- 用户手动触发整合

**记忆生命周期管理**

记忆无限期保存，可能导致过时信息积累。需要：
- 记忆过期策略（如 90 天未访问降级）
- 自动归档机制
- 记忆版本历史

**跨项目知识迁移**

不同项目可能有相似的知识。需要支持：
- 项目间记忆共享
- 通用知识库（如编码规范）
- 项目特异性标记

---

### 6. Layer 7 增强

**子代理结果缓存**

子代理执行结果不被缓存，重复任务浪费资源。需要：
- 结果缓存机制（基于输入哈希）
- 缓存失效策略
- 缓存大小限制

**分布式执行支持**

当前子代理仅在本地执行。大规模任务可能需要：
- 远程执行节点
- 任务队列和调度
- 结果同步机制

**资源使用限制**

子代理可能消耗过多资源。需要：
- 最大执行时间限制
- Token 使用上限
- 工具调用次数限制

---

### 7. 监控与调试

**各层性能指标收集**

需要收集的关键指标：
- 每层触发频率
- 平均处理时间
- Token 消耗量
- 错误率

**Token 预算可视化**

提供可视化的预算使用情况：
- 当前各层 Token 占用
- 预算警告和建议
- 历史趋势图

**缓存命中率分析**

Prompt Cache 是降低成本的关键。需要：
- 缓存命中率统计
- 未命中原因分析
- 缓存优化建议

---

### 8. 存储优化

**记忆文件压缩存储**

大量记忆文件占用磁盘空间。需要：
- Markdown 文件压缩（gzip）
- 按时间分片存储
- 冷热数据分离

**过期记忆自动清理**

长期运行后记忆文件积累。需要：
- 自动清理策略（基于访问时间）
- 清理前备份机制
- 用户确认选项

**大规模记忆索引**

当记忆文件超过数千个时，扫描变慢。需要：
- 记忆索引数据库
- 全文搜索支持
- 分类标签系统

---

### 9. 配置增强

**动态预算调整**

当前预算是固定值。需要：
- 根据模型动态调整（不同模型不同预算）
- 根据对话复杂度调整
- 用户自定义预算模板

**用户自定义触发条件**

当前触发条件硬编码。需要：
- 配置文件支持自定义阈值
- 触发条件表达式（如 `tokens > 50000 && tool_calls > 10`）
- 预设配置模板

**分层开关控制**

用户可能只想启用部分层。需要：
- 每层独立的开关
- 依赖关系自动处理（如禁用 Layer 7 则 Layer 3/5/6 降级）
- 配置验证和提示

---

### 10. 文档与测试

**各层单元测试**

当前测试覆盖完善，136 个文件包含测试代码，核心逻辑均有单元测试覆盖。

**集成测试覆盖**

当前已实现丰富的集成测试（见 `crates/agent/tests/memory_integration_test.rs`）：
- 端到端场景测试：`test_full_memory_flow`、`test_full_layer_workflow`
- 多层协作测试：`test_layer1_layer2_interaction`、`test_layer3_layer4_recovery_interaction`、`test_layer5_layer7_forked_interaction`
- 触发链测试：`test_layer1_to_layer2_workflow` → `test_layer2_to_layer3_workflow` → `test_layer3_to_layer4_workflow`
- Compact 恢复测试：`test_compact_recovery_reinjection`、`test_compact_recovery_message_format`

待补充：
- 压力测试
- 性能回归测试

**性能基准测试**

缺乏性能基准数据。需要：
- 各层处理时间基准
- 内存使用基准
- Token 消耗基准
- 回归测试基准线

---

### 已知限制

1. **Windows 进程检测**: 在某些边缘情况下可能不准确，依赖 24 小时过期作为兜底
2. **缓存编辑 API**: 仅在特定模型上可用，需要条件检测
3. **记忆文件大小**: 无硬性限制，依赖用户定期清理
4. **后台任务超时**: 默认 60 秒，可能不适用于大型整合任务

---

## 附录: 关键实现文件

| 模块 | 文件路径 | 主要功能 |
|------|----------|----------|
| Layer 1 | `crates/agent/src/response_cache.rs` | 工具结果持久化 |
| Layer 2 | `crates/agent/src/history_projector.rs` | 时间触发清理 |
| Layer 3 | `crates/agent/src/session_memory/` | 会话记忆管理 |
| Layer 4 | `crates/agent/src/compact/` | LLM 语义压缩 |
| Layer 5 | `crates/agent/src/auto_memory/` | 自动记忆提取 |
| Layer 6 | `crates/scheduler/src/consolidator.rs` | 跨会话整合 |
| Layer 7 | `crates/agent/src/forked/` | 子代理执行框架 |
| 集成 | `crates/agent/src/memory_system/mod.rs` | 系统集成 |

---

> 文档版本: 2026-04-05
> 目标框架: BlockCell Rust 多智能体框架