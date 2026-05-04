# 统一学习框架设计与冲突治理整合文档

> 版本：v1.1  
> 日期：2026-05-03  

---

## 1. 总结

Ghost Native Learning 和 Skill 自进化不应该二选一。两者的学习对象不同：

- **Ghost Native Learning** 更适合声明性知识：用户偏好、事实、环境、长期记忆、会话边界总结。
- **Skill 自进化** 更适合程序性知识：工作流、操作步骤、修复经验、工具使用模式和可复用技能。

真正的问题不是功能重复，而是原实现缺少统一仲裁层：

- 两个系统都可能触发后台 review。
- 两个系统都可能接近 memory/skill 写入。
- 两个系统各自维护安全扫描、并发锁和触发策略。

因此推荐方向是：**保留两套学习价值，但用统一学习协调器管理触发、权限、写入、安全扫描、节流和去重。**

---

## 2. 设计目标

| 目标 | 优先级 | 说明 |
| --- | :---: | --- |
| 消除技能文件竞态写入 | P0 | 避免多个后台任务同时修改 `SKILL.md` 导致覆盖或丢失 |
| 消除 memory 与 skill 的放大循环 | P0 | 防止 Ghost 写 memory、Skill 从 memory 生成 skill、Ghost 再学习 skill 变化的循环 |
| 消除重复 LLM 调用 | P1 | 同一回合不应触发多个独立后台 review |
| 统一安全扫描 | P1 | Memory、Skill、auto memory 使用同一套安全规则 |
| 统一并发写保护 | P1 | MemoryFileStore、SkillFileStore、SkillMutex 不再各自为政 |
| 保持显式调用链 | P2 | `runtime.rs` 中仍保持可追踪、可调试的显式调用 |
| 渐进迁移 | P2 | 先解决 P0/P1 风险，再逐步收敛配置和检索层 |

---

## 3. 核心冲突点

### 3.1 写入权限重叠

Ghost background review 曾允许使用 `skill_manage`，导致 Memory Review 也可能创建或修改 `SKILL.md`。这会和 Skill Review 同时写技能文件，产生覆盖风险。

治理原则：

- Memory/Ghost Review 只允许 `memory_manage`、`session_search` 和只读的 `skill_view`。
- Skill Review 才允许 `skill_manage`。
- Combined Review 是唯一允许同时处理 memory 和 skill 的路径。

### 3.2 触发机制重叠

Ghost 的学习边界和 Skill 的 nudge 阈值独立触发。同一用户回合可能同时启动 Memory Review 和 Skill Review。

治理原则：

- 由 `LearningCoordinator` 在同一处做统一决策。
- 同一回合最多形成一个动作：`Skip`、`MemoryReview`、`SkillReview`、`CombinedReview` 或 `PreCompressFlush`。

### 3.3 放大循环

风险链路：

```text
Ghost 写入流程性知识到 memory
→ Skill 自进化从 memory 中抽取 skill
→ skill 变化又被 Ghost 观察并写回 memory
→ 重复放大
```

治理原则：

- Memory 只保存声明性知识。
- Skill 只保存程序性知识。
- auto memory prompt 明确声明 memory-only：不创建、不 patch、不请求 skill，也不把 workflow 写入 memory。

### 3.4 安全扫描不一致

原先存在多套扫描逻辑：

- `MemoryFileStore` 内联字符串黑名单。
- `SkillFileStore` 另一套内联规则。
- `auto_memory/scanner.rs` 独立 scanner。
- `blockcell-tools::security_scan` 已有更完整的 16 类安全扫描。

治理原则：

- 引入 `UnifiedSecurityScanner`。
- 所有学习写入统一走 `blockcell-tools::security_scan`。
- Memory 和 agent 生成的 skill 均按 `AgentCreated` 语义严格扫描。

### 3.5 并发保护不统一

原先存在多套锁：

- `SkillMutex`
- `MemoryFileStore` lockdir
- `SkillFileStore` lockdir

这些锁只能保护各自文件，不能表达“当前 learning write 是同一临界区”。

治理原则：

- 引入 `WriteGuard`。
- Memory 和 Skill 文件写入共享统一保护入口。
- 后续稳定后可逐步替代旧 `SkillMutex`。

---

## 4. 统一框架架构

推荐四层边界：

```text
runtime.rs
  ↓ 显式调用
LearningCoordinator
  ├─ 决策：Memory / Skill / Combined / Skip / PreCompressFlush
  ├─ 节流：LearningThrottle
  ├─ 去重：LearningDedup
  └─ 审计：Review Ledger / GhostLedger
      ↓
Learning Channels
  ├─ MemoryLearningChannel
  ├─ SkillLearningChannel
  └─ CombinedLearningChannel
      ↓
Shared Infrastructure
  ├─ UnifiedSecurityScanner
  ├─ WriteGuard
  ├─ MemoryFileStore / SkillFileStore
  └─ RecallEngine
```

### 4.1 LearningCoordinator

`LearningCoordinator` 是统一学习仲裁层，负责：

- 合并 Ghost policy 与 Skill nudge 的触发结果。
- 对同一回合进行节流和去重。
- 选择最终学习动作。
- 保持 Runtime 中的调用链显式可读。

核心动作模型：

```rust
pub enum LearningAction {
    Skip,
    MemoryReview { trigger: MemoryTrigger, context: ReviewContext },
    SkillReview { trigger: SkillTrigger, context: ReviewContext },
    CombinedReview {
        memory_trigger: MemoryTrigger,
        skill_trigger: SkillTrigger,
        context: ReviewContext,
    },
    PreCompressFlush { context: ReviewContext },
}
```

### 4.2 LearningThrottle

用于防止后台 review 同时过多：

- 限制最大并发 review 数。
- 限制最小 review 间隔。
- 防止短时间重复触发。

### 4.3 LearningDedup

用于避免重复学习同一模式：

- 基于用户意图、助手结果、工具调用数量、复杂度等生成 fingerprint。
- 在去重窗口内跳过相似学习请求。

### 4.4 WriteGuard

统一 Memory/Skill 写入临界区：

- 进程内保护：避免同一 Runtime 内并发写同一目标。
- lockdir 保护：降低跨进程写入冲突风险。
- `MemoryFileStore` 和 `SkillFileStore` 均通过该入口写入。

### 4.5 UnifiedSecurityScanner

统一扫描入口：

- `scan_memory_content`
- `scan_skill_content`
- `scan_skill_dir`

Memory、Skill、auto memory 写入都使用同一规则集，减少“一个路径拦截、另一路径放过”的安全差异。

### 4.6 RecallEngine

后续可将 GhostRecall 与 SkillIndex 合并为统一检索预算层，但输出仍建议分区：

- memory context 保持在 memory block。
- skill summary 保持在 skill summary 区域。

这样既统一预算和排序，又避免 prompt 语义混淆。

---

## 5. 决策矩阵

| Ghost 决策 | Skill Nudge | Memory Nudge | 最终动作 |
| --- | :---: | :---: | --- |
| Ignore | NoNudge | NoNudge | Skip |
| Ignore | Soft/Hard | NoNudge | SkillReview |
| Ignore | NoNudge | Soft/Hard | MemoryReview |
| Ignore | Soft/Hard | Soft/Hard | CombinedReview |
| ReviewAfterResponse | NoNudge | NoNudge | MemoryReview |
| ReviewAfterResponse | Soft/Hard | NoNudge | CombinedReview |
| ReviewAfterResponse | NoNudge | Soft/Hard | MemoryReview |
| ReviewAfterResponse | Soft/Hard | Soft/Hard | CombinedReview |
| ForceBoundaryReview | 任意 | 任意 | CombinedReview 或强制 MemoryReview |
| PreCompress | N/A | N/A | PreCompressFlush |
| SessionEnd | N/A | N/A | MemoryReview |
| SessionRotate | N/A | N/A | MemoryReview |

---

## 6. 已实现改进记录

截至 2026-05-03，已实现或修复：

- 新增 `LearningCoordinator`，统一处理 Ghost learning 与 Skill nudge 的部分决策。
- 新增 `LearningThrottle`，限制 review 并发与触发频率。
- 新增 `LearningDedup`，降低同类学习重复触发。
- 修复 `LearningCoordinator::evaluate_nudge` 自锁问题：原逻辑持有 `nudge_engine` mutex 后又二次 lock reset，导致测试挂起。
- 新增 `UnifiedSecurityScanner`，封装 `blockcell_tools::security_scan`。
- `MemoryFileStore` 改为调用统一安全扫描器。
- `SkillFileStore` 改为调用统一安全扫描器。
- `auto_memory::extractor` 改为调用统一安全扫描器，扫描失败时回滚写入。
- auto memory prompt 明确 memory-only，不创建、不 patch、不请求 skill，不把 workflow 写入 memory。
- `MemoryFileStore` 和 `SkillFileStore` 接入 `WriteGuard`，并保留未配置 guard 时的兼容路径。
- 修复 Windows 测试环境下目录 fsync 的 `PermissionDenied` 问题：目录同步改为 best-effort。
- `MemoryFileStore::restore_latest` 改为读取 snapshot 后走原子写入，避免 Windows 上直接 `fs::copy` 覆盖现有文件失败。

---

## 7. 仍建议继续推进的事项

### 7.1 UnifiedLearningConfig

将 `GhostLearningConfig` 和 `SelfImproveConfig` 映射到统一 `learning` 配置入口，同时保留旧字段兼容。

目标：

- 用户能直接理解 memory learning 与 skill learning 的关系。
- 后续统一 throttle、dedup、review 策略配置。

### 7.2 Review Ledger

给 Memory、Skill、Combined Review 建立统一 ledger。

建议记录：

- trigger
- dedup key
- 是否被 throttle
- 写入目标
- 安全扫描结果
- review 输出摘要
- 成功/失败状态

价值：

- 排查“为什么学了/没学”更容易。
- 方便统计学习质量和回滚。

### 7.3 RecallEngine

统一 GhostRecall 与 SkillIndex 的检索预算。

原则：

- 预算、排序、去重可以统一。
- 输出位置仍分区，避免 memory 与 skill 语义混杂。

### 7.4 删除旧 scanner 与 SkillMutex

当前方向已经明确：

- `auto_memory/scanner.rs` 可标记 deprecated，待调用点稳定后删除。
- `skill_mutex.rs` 可逐步由 `WriteGuard` 完全替代。

---

## 8. 分阶段迁移路线

### Phase 1：紧急风险修复

目标：消除技能文件竞态写入和放大循环。

内容：

- Ghost Background Review 移除 `skill_manage`。
- Ghost Review 只保留 `memory_manage`、`session_search`、只读 `skill_view`。
- auto memory prompt 明确 memory-only。

### Phase 2：统一并发保护

目标：消除跨系统写入竞态。

内容：

- 引入 `WriteGuard`。
- MemoryFileStore、SkillFileStore 写入通过 `WriteGuard`。
- 后续逐步替代 SkillMutex。

### Phase 3：统一学习协调器

目标：统一决策、节流和去重。

内容：

- `LearningCoordinator` 整合 `GhostLearningPolicy` 与 `SkillNudgeEngine`。
- 引入 `LearningThrottle` 和 `LearningDedup`。
- Runtime 中学习相关调用改为通过 coordinator 仲裁。

### Phase 4：统一安全扫描

目标：所有学习写入使用同一安全标准。

内容：

- 引入 `UnifiedSecurityScanner`。
- Memory、Skill、auto memory 写入全部接入。
- 旧 scanner 标记迁移方向。

### Phase 5：统一配置

目标：降低用户理解成本。

内容：

- 新增 `UnifiedLearningConfig`。
- 旧配置字段向后兼容映射。
- 更新配置文档和默认配置示例。

### Phase 6：统一检索

目标：统一 memory 与 skill 的召回预算和排序。

内容：

- 引入 `RecallEngine`。
- 合并 GhostRecall 与 SkillIndex 的预算管理。
- 输出仍保持 memory/skill 分区。

---

## 9. 风险与缓解

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| Ghost 移除 `skill_manage` 后无法创建技能 | 低 | Ghost 本应只负责声明性记忆，技能创建由 Skill Review 负责 |
| WriteGuard 引入死锁 | 高 | 使用 try-acquire、超时和 RAII，避免嵌套锁 |
| LearningCoordinator 逻辑复杂 | 中 | evaluate 与 execute 分离，重点补单元测试 |
| 统一安全扫描过严 | 中 | 按内容来源映射 TrustLevel |
| 配置迁移影响旧用户 | 高 | 保留旧键兼容映射，增加迁移警告 |
| RecallEngine 预算不足 | 低 | 保留可配置 token budget，输出分区 |

---

## 10. 验证记录

冲突治理实现阶段已通过：

- `cargo check -p blockcell-agent --message-format=short`
- `cargo clippy -p blockcell-agent -- -D warnings`
- `cargo test -p blockcell-agent learning_coordinator --lib -- --nocapture`
- `cargo test -p blockcell-agent unified_security_scanner --lib -- --nocapture`
- `cargo test -p blockcell-agent file_store --lib -- --nocapture`
- `cargo test -p blockcell-agent auto_memory::extractor::tests::test_build_extraction_prompt --lib -- --nocapture`

完整 `cargo test -p blockcell-agent --lib -- --nocapture` 已不再挂起，当时剩余 2 个失败：

- `runtime::tests::test_prompt_skill_can_still_use_exec_local_inside_skill_scope_for_compat`
- `runtime::tests::test_skill_executor_uses_manual_not_file_type_to_choose_skill_script`

失败原因均为 Windows 环境找不到 `sh`，错误为 `Failed to execute local script: program not found`，与 Ghost/Skill 学习冲突治理无关。

---

## 11. 最终建议

当前最优路线不是把 Ghost 与 Skill 立即重写成单一 Learning 系统，而是先用 coordinator、guard、scanner 和权限边界解决真实冲突。

原因：

- 两套系统的学习对象不同，强行合并会增加风险。
- 当前 P0/P1 问题来自协调缺失，不是抽象不足。
- 渐进迁移能更快稳定后台学习行为，并保留现有能力。

后续可继续推进配置和检索层统一，但执行通道不必急于完全重写。
