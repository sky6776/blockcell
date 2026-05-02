# Self-Improving 实现文档

> BlockCell Self-Improving 三子系统
>
> 参考: Hermes Agent Self-Improving (github.com/NousResearch/hermes-agent)

## Hermes 源码参考索引

| 功能 | Hermes 文件 | BlockCell 对应 |
|------|------------|---------------|
| Memory 存储 | `tools/memory_tool.py` | `crates/tools/src/memory.rs` |
| Skill 管理 | `tools/skill_manager_tool.py` | `crates/tools/src/skill_manage.rs` |
| Skill 安全扫描 | `tools/skills_guard.py` | `crates/tools/src/security_scan.rs` |
| 模糊匹配引擎 | `tools/fuzzy_match.py` | `crates/tools/src/fuzzy_match.rs` |
| Nudge 计数器 + 后台 Review | `run_agent.py` | `crates/agent/src/skill_nudge.rs` + `runtime.rs` |
| 系统提示词 | `agent/prompt_builder.py` | `crates/agent/src/context.rs` |
| Forked Agent | `run_agent.py:_spawn_background_review` | `crates/agent/src/forked/agent.rs` |
| 工具权限 | 内联在 `run_agent.py` | `crates/agent/src/forked/can_use_tool.rs` |
| Skill 互斥锁 | `agent_core/skill_mutex.py` | `crates/agent/src/skill_mutex.rs` |
| Memory 提取 | 内联在 prompt | `crates/agent/src/auto_memory/extractor.rs` |
| Skill 索引 | 内联 | `crates/agent/src/skill_index.rs` |

## 1. 背景与架构

### 1.1 Hermes Self-Improving 三子系统

BlockCell 实现了与 Hermes 对齐的三子系统闭环:

| 子系统 | 职责 | 实现 |
|--------|------|------|
| **Memory** | 记住用户偏好、环境、约定 | auto_memory (4 md 文件) + SQLite, MemoryInjector 注入系统提示词 |
| **Skill** | 记住工作流、步骤、常见陷阱 | skill_manage (7 个 action) + 渐进式加载 + SkillIndex |
| **Nudge Engine** | 定时触发 Review, 保证持续学习 | SkillNudgeEngine (双计数器 + 双阈值) + 后台 ForkedAgent |

核心差异: BlockCell 不仅匹配 Hermes 的 Layer 1 (Review) + Layer 2 (Patch), 还有独有的 Layer 3 (Evolve) — 从执行失败深度修复 Skill。

### 1.2 实现完成度

| 能力 | 状态 |
|------|------|
| Memory 存储 + 安全扫描 | ✅ |
| Memory 提取 (ForkedAgent + EXTRACTION_ENHANCEMENT) | ✅ |
| Memory 注入 (MemoryInjector + CacheSafeParams) | ✅ |
| Skill CRUD (create/patch/view/delete/edit/write_file/remove_file) | ✅ |
| Skill 修补 (fuzzy_find_and_replace 9 策略链) | ✅ |
| Skill 安全扫描 (16 类规则, 100+ 模式) | ✅ |
| Skill 渐进加载 (SkillIndex 轻量索引 + view 按需加载) | ✅ |
| Skill 深度进化 (EvolutionService Layer 3) | ✅ |
| Nudge Skill Review (双阈值 5/10 + 冷却) | ✅ |
| Nudge Memory Review (双阈值 3/6 + 冷却) | ✅ |
| Combined Review (Skill + Memory 联合) | ✅ |
| Skill 互斥锁 (SkillMutex, ForkedAgent 共享) | ✅ |
| Review 结果通知 (extract_review_summary, 5 种响应格式) | ✅ |
| Flush (压缩前保存, 仅 memory_upsert, 单轮) | ✅ |
| 系统提示词引导 (SKILL_GUIDANCE + Skill 索引) | ✅ |
| 原子写入 (atomic_write_text, temp file + rename) | ✅ |
| meta.json 生成 (create/patch/edit 后更新) | ✅ |
| Category 支持 (skills/{category}/{name}/) | ✅ |
| Clippy 零警告 (`cargo clippy -- -D warnings`) | ✅ |

## 2. 架构总览

### 2.1 三层渐进式系统

```
Layer 1: Skill Review (从成功创建)
  ↓ 触发: Nudge 计数器到阈值 (软 5 / 硬 10 次工具迭代)
  ↓ 执行: 后台 ForkedAgent (max 8 turns)
  ↓ 工具: skill_manage, list_skills, read_file, grep, glob
  ↓ 产出: 新建 Skill / 更新已有 Skill + Memory 保存

Layer 2: Skill Patch (从新坑修补)
  ↓ 触发: Agent 主动发现遗漏步骤或新 Pitfall
  ↓ 执行: 主循环内实时调用 skill_manage(patch)
  ↓ 工具: fuzzy_find_and_replace 9 策略模糊匹配
  ↓ 产出: patch Skill, 追加 Pitfalls

Layer 3: Skill Evolve (从失败深度修复)
  ↓ 触发: EvolutionService 收集错误, 超阈值后触发
  ↓ 执行: generate → audit → compile → deploy → observe
  ↓ 产出: 重新生成 + 金丝雀发布 (5 calls, <10% error)
```

### 2.2 Nudge Engine

```
SkillNudgeEngine {
    iterations_since_skill: u32   // 工具迭代 (LLM 调用 + 工具执行)
    turns_since_memory: u32       // 用户轮次 (仅真实用户消息, 排除 cron/system)
    skill_soft_threshold: 5       // Skill 软阈值
    skill_hard_threshold: 10      // Skill 硬阈值
    memory_soft_threshold: 3      // Memory 软阈值
    memory_hard_threshold: 6      // Memory 硬阈值
    min_nudge_interval_secs: 300  // 冷却时间
}
```

**计数器语义** (与 Hermes 完全一致):
- `record_iteration()`: 每次 LLM 调用 + 工具执行后 +1 → 影响 Skill
- `record_user_turn()`: 仅真实用户消息 (非 cron/system) → 影响 Memory
- `reset_skill()`: Agent 调用 skill_manage / skill_index 时重置
- `reset_memory()`: Agent 调用 memory_* / auto_memory 时重置

### 2.3 完整数据流

```
用户消息
  → process_message()
    → record_user_turn()            (仅非 cron/system)
    → check_memory_nudge()          (循环前检查)
      → 到阈值 → deferred_review_mode = Some(Memory)
    → LLM 循环:
      → record_iteration()           (每次迭代)
      → LLM 调用 → 工具执行
      → 如果 skill_manage 被调用 → reset_skill()
      → 如果 memory_* 被调用 → reset_memory()
      → check_skill_nudge()         (每次迭代后)
        → 到阈值 → deferred_review_mode = Some(Skill)
        → Memory 已设 → 升级为 Combined
  → 响应发送
    → spawn_review(mode, snapshot)   (延迟后台触发)
      → ForkedAgent (受限工具权限 + SkillMutex)
        → 审查对话, 创建/patch Skill, 保存 Memory
      → extract_review_summary()
        → 通知用户 "💾 Skill 'xxx' created · Memory updated"
      → refresh_skill_index_cache   (Skill/Combined 模式)
```

## 3. 核心组件

### 3.1 skill_manage 工具

**文件**: `crates/tools/src/skill_manage.rs` (主工具) + `crates/agent/src/forked/agent.rs` (ForkedAgent 实现)

| Action | 说明 | 安全措施 |
|--------|------|---------|
| `create` | 创建 Skill (SKILL.md + meta.json) | frontmatter 验证 + security_scan + 原子写入 |
| `patch` | 修补 Skill (fuzzy_find_and_replace) | 9 策略模糊匹配 + security_scan + 原子写入 + meta.json 更新 |
| `view` | 查看 Skill (content + meta + references + templates) | 按需加载 |
| `delete` | 删除 Skill 目录 | SkillMutex 检查 |
| `edit` | 完整替换 SKILL.md | security_scan + 原子写入 + meta.json 更新 |
| `write_file` | 添加 supporting 文件 | security_scan + 原子写入 |
| `remove_file` | 删除 supporting 文件 | 路径验证 |

**主工具 vs ForkedAgent**:
- 主工具: 完整 Tool trait + ToolContext, 注册到工具注册表
- ForkedAgent: 内联在 `execute_forked_tool()` 中, 使用相同的 fuzzy_match、security_scan、frontmatter 验证、原子写入、meta.json 生成

### 3.2 fuzzy_match 引擎

**文件**: `crates/tools/src/fuzzy_match.rs`

9 策略链 (与 Hermes 逐策略一致):

```
exact → line_trimmed → whitespace_norm → indent_flex → escape_norm
    → trim_boundary → unicode_norm → block_anchor → context_aware
```

返回: `(new_content, match_count, strategy_name)`

### 3.3 安全扫描

**文件**: `crates/tools/src/security_scan.rs`

16 类规则, 100+ 模式:
- PERSISTENCE: crontab, .bashrc, authorized_keys, systemd
- OBFUSCATION: base64_decode, eval(), exec()
- NETWORK_ATTACK: reverse shell, tunnel, bind
- SUPPLY_CHAIN: curl_pipe_shell, unpinned deps
- PRIVILEGE_ESCALATION: sudo, setuid
- CREDENTIAL_EXPOSURE: hardcoded keys, tokens
- JAILBREAK: DAN, developer mode
- ZERO_WIDTH_UNICODE: 15 种不可见字符

`scan_skill_content()` 在 create/edit/patch/write_file 前调用, 不通过则拒绝。

### 3.4 Skill 索引

**文件**: `crates/agent/src/skill_index.rs`

- `build_from_dir()`: 扫描 skills 目录 (支持 category 子目录)
- `build_entry()`: 4 级降级 (meta.json → SKILL.md frontmatter → 文件 mtime → 默认)
- `to_prompt_summary()`: 生成系统提示词中的轻量摘要
- 支持 `{skills_dir}/{category}/{name}/` 结构

### 3.5 Skill 互斥锁

**文件**: `crates/agent/src/skill_mutex.rs`

- `SkillMutex`: `Arc<RwLock<HashSet<String>>>` (同步, 无 tokio 依赖)
- `acquire(skill_name) -> Result<SkillGuard>`: 获取锁
- `can_modify(skill_name) -> bool`: 检查是否可修改
- `SkillGuard`: RAII Drop 自动释放
- ForkedAgent 通过 `with_skill_mutex()` 共享主 Agent 的互斥锁

### 3.6 后台 Review Agent

**文件**: `crates/agent/src/runtime.rs` (`spawn_review()`) + `crates/agent/src/forked/agent.rs`

**Review 模式**:

| 模式 | 工具 | 提示词 (与 Hermes 逐字一致) |
|------|------|---------------------------|
| `Skill` | skill_manage, list_skills, read_file, grep, glob | SKILL_REVIEW_PROMPT |
| `Memory` | memory_upsert, memory_query, memory_forget, read_file, grep, glob | MEMORY_REVIEW_PROMPT |
| `Combined` | 以上全部 | COMBINED_REVIEW_PROMPT |

**工具权限** (`crates/agent/src/forked/can_use_tool.rs`):

| CanUseToolFn | 创建函数 | 允许的工具 |
|---|---|---|
| Skill Review | `create_skill_review_can_use_tool()` | skill_manage, list_skills, read_file, grep, glob |
| Memory Review | `create_memory_review_can_use_tool()` | memory_upsert, memory_query, memory_forget, read_file, grep, glob |
| Combined | `create_combined_review_can_use_tool()` | 以上全部 |
| Flush | `create_flush_can_use_tool()` | 仅 memory_upsert |

**对齐 Hermes 的关键点**:
- Review 提示词完全一致 (逐字匹配)
- 继承主 Agent 的 system prompt (`build_system_prompt()`)
- 共享 memory_store (`Arc::clone` + `with_memory_store()`)
- 共享 SkillMutex (`with_skill_mutex()`)
- 受限于特制工具权限 (CanUseToolFn)
- 后台 fire-and-forget (`tokio::spawn`)
- 完成后刷新 Skill 索引缓存 (`Arc<RwLock>` 共享)
- 提取操作摘要并通知用户 (`extract_review_summary`)

**超出 Hermes 的增强**:
- Review prompt 前附加 Skill 索引摘要 (减少 LLM 工具调用)
- 双阈值 (软/硬) + 冷却机制
- Combined Review: 一次后台任务同时处理 Skill + Memory
- SkillMutex 保护: 防止 Review Agent 与主 Agent 并发修改同一 Skill

### 3.7 系统提示词注入

**文件**: `crates/agent/src/context.rs`

系统提示词注入顺序:
1. `SKILL_GUIDANCE`: Skill 管理引导 + Memory/Skill 边界规则 (非 chat 模式)
2. Skill 索引: `## Available Skills` 轻量列表 (来自 SkillIndex)
3. `EXTRACTION_ENHANCEMENT`: Memory 提取收敛规则 (注入到提取 prompt)

### 3.8 extract_review_summary

**文件**: `crates/agent/src/runtime.rs`

识别 5 种工具响应格式:
1. `{"success": true, "message": "Skill 'xxx' created"}` — skill_manage 操作
2. `{"target": "memory", "success": true}` — Hermes 格式 memory
3. `{"status": "saved", ...}` — memory_upsert 操作
4. `{"action": "delete", "deleted": true}` — memory_forget 删除
5. `{"action": "batch_delete", "deleted_count": N}` — memory_forget 批量

### 3.9 Flush (上下文压缩前保存)

**文件**: `crates/agent/src/runtime.rs` (`flush_memories()`)

- 触发: 上下文压缩前
- 执行: ForkedAgent, 单轮 (`max_turns=1`)
- 工具: `create_flush_can_use_tool()` (仅 `memory_upsert`)
- 提示词: 与 Hermes 完全一致

### 3.10 内存适配器

**文件**: `crates/agent/src/memory_adapter.rs`

`MemoryStoreAdapter` 实现 `MemoryStoreOps` trait, 桥接 `blockcell_storage::memory::MemoryStore` 到 `blockcell_tools::MemoryStoreOps`:
- `upsert_json()` → `MemoryService::upsert()`
- `query_json()` → `MemoryStore::query()`
- `soft_delete()` / `batch_soft_delete_json()` → 直接委托

## 4. 文件清单

### 4.1 核心实现文件

| 文件 | 说明 |
|------|------|
| `crates/tools/src/skill_manage.rs` | skill_manage 工具 (7 action, frontmatter 提取, 原子写入) |
| `crates/tools/src/fuzzy_match.rs` | 模糊查找替换引擎 (9 策略链) |
| `crates/tools/src/security_scan.rs` | Skill + Memory 安全扫描 (16 类规则) |
| `crates/tools/src/memory.rs` | memory_upsert/query/forget 工具 |
| `crates/agent/src/skill_nudge.rs` | SkillNudgeEngine (双计数器 + 双阈值) |
| `crates/agent/src/skill_mutex.rs` | Skill 操作互斥锁 |
| `crates/agent/src/skill_index.rs` | Skill 轻量索引 |
| `crates/agent/src/forked/agent.rs` | ForkedAgent 执行核心 + 工具执行器 |
| `crates/agent/src/forked/can_use_tool.rs` | 工具权限 (5 种 CanUseToolFn) |
| `crates/agent/src/forked/cache_safe.rs` | CacheSafeParams 前缀缓存 |
| `crates/agent/src/forked/context.rs` | 子代理上下文 |
| `crates/agent/src/context.rs` | 系统提示词构建 (SKILL_GUIDANCE + 索引注入) |
| `crates/agent/src/auto_memory/extractor.rs` | Memory 提取 (EXTRACTION_ENHANCEMENT) |
| `crates/agent/src/auto_memory/scanner.rs` | Memory 安全扫描 |
| `crates/agent/src/runtime.rs` | AgentRuntime (Nudge 集成, spawn_review, flush_memories) |
| `crates/agent/src/memory_adapter.rs` | MemoryStoreOps 适配器 |
| `crates/agent/src/capability_adapter.rs` | Provider/Registry/Evolution 适配器 |
| `crates/core/src/config.rs` | SelfImproveConfig (Nudge + Review 配置) |

### 4.2 设计文档

| 文件 | 说明 |
|------|------|
| `docs/design/self-improving-design.md` | 本文档 — 架构和实现概览 |

## 5. 配置

`~/.blockcell/config.json5`:

```json5
{
  "selfImprove": {
    "nudge": {
      "enabled": true,
      "skill_soft_threshold": 5,
      "skill_hard_threshold": 10,
      "memory_soft_threshold": 3,
      "memory_hard_threshold": 6,
      "min_nudge_interval_secs": 300
    },
    "review": {
      "enabled": true,
      "max_rounds": 8
    }
  }
}
```

## 6. 安全

| 风险 | 防范 |
|------|------|
| Memory 注入攻击 | `scan_memory_content()` 写入前检查 (20+ 威胁模式) |
| Skill 代码注入 | `scan_skill_content()` 写入前检查 (16 类, 100+ 模式), 失败拒绝 |
| Review Agent 权限逃逸 | CanUseToolFn 严格限制 (5/6/1 工具) |
| Skill 并发修改 | SkillMutex (`Arc<RwLock>`), ForkedAgent 共享主 Agent 互斥锁 |
| 路径遍历 | 多层防护: `..` `/` `\` `\0` + VALID_SKILL_NAME_RE |
| 文件损坏 | 原子写入 (`atomic_write_text`: temp file + rename) |
| 无意义 Skill 创建 | Review prompt: "If nothing is worth saving, just say 'Nothing to save.'" |

## 7. 与 Hermes 对比

| 特性 | Hermes | BlockCell | 对齐 |
|------|--------|-----------|------|
| Memory 存储 | 2 md 文件 | 4 md + SQLite FTS5 | ✅+ |
| Memory Nudge | 10 回合 | 双阈值 3/6 + 300s 冷却 | ✅+ |
| Skill Nudge | 10 迭代 | 双阈值 5/10 + 300s 冷却 | ✅+ |
| Review 提示词 | 3 种 | 完全一致 | ✅ |
| Combined Review | 两个 flag | ReviewMode::Combined | ✅ |
| ForkedAgent | daemon 线程, max 8 | tokio::spawn, max 8 | ✅ |
| Review 工具权限 | 内联 | CanUseToolFn 类型化 | ✅ |
| 结果通知 | _safe_print + callback | extract_review_summary + outbound_tx | ✅ |
| System prompt | 默认 constructor | 显式 build_system_prompt() | ✅ |
| Memory store 共享 | 直接赋值 | Arc::clone + with_memory_store() | ✅ |
| Flush | 单次 API | ForkedAgent, 1 轮 | ✅ |
| Create | frontmatter + security + meta | + 原子写入 + category | ✅+ |
| Patch | fuzzy_find_and_replace 9 策略 | 完全一致 | ✅ |
| View | content + refs + templates + meta | 完全一致 | ✅ |
| Security scan | skills_guard.py | 16 类, 100+ 模式 | ✅+ |
| SkillMutex | 无 | 同步 RwLock | BlockCell 独有 |
| 索引刷新 | compress 时 invalidate | 每轮重建 + Review 后立即刷新 | ✅+ |
| Nudge 语义 | turns/iters | 完全一致 + 排除 cron/system | ✅ |
| Evolve (Layer 3) | 无 | EvolutionService | BlockCell 独有 |
| Category | 无 | {category}/{name}/ | BlockCell 独有 |
| 原子写入 | 直接 | atomic_write_text | ✅+ |
| Frontmatter 验证 | 内联 | validate_skill_frontmatter | ✅ |
| 名称验证 | 隐式 | VALID_SKILL_NAME_RE | ✅ |
| Clippy | N/A (Python) | `-D warnings` 零警告 | ✅ |
