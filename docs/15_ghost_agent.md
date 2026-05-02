# 第15篇：Ghost 后台维护 —— 定时维护与社区同步

> 系列文章：《blockcell 开源项目深度解析》第 15 篇
---

## 为什么需要 Ghost 后台维护

交互式智能体擅长“你问我答”，但一个长期运行的 AI 系统还需要做很多**低频、后台、维护型**工作，例如：

- SQLite 记忆库的日常维护（去重、压缩、清理过期内容，并把稳定事实维护到 `long_term`）
- 清理工作区的临时文件（media/downloads）
- 在社区 Hub 上保持节点活跃，获取最新技能动态

这些任务不应该占用用户的对话时间，也不应该持续高频执行。

因此 blockcell 在 **Gateway 模式**内置了一个定时后台维护组件：**Ghost Maintenance**。

---

## Ghost Maintenance 是什么

Ghost Maintenance 是一个**按计划运行**的后台例行维护服务：

- 它不会像主智能体一样“实时响应用户聊天”
- 它会定期向系统投递一条内部消息（channel=`ghost`），触发一次维护循环
- 维护循环的结果会写入会话日志（session jsonl），用于在 WebUI/Gateway API 中查看

在代码中，它的核心实现位于：

- `crates/scheduler/src/ghost.rs` 的 `GhostMaintenanceService`

---

## 与嵌入式 Ghost 自学习的边界

BlockCell 现在有两条不同的 Ghost 相关路径：

- **嵌入式 Ghost 自学习**：运行在 `AgentRuntime` 的正常对话流程中，会从成功经验里提炼 `USER.md`、`MEMORY.md` 和 `workspace/skills/<name>/SKILL.md`。这是“会学习的助手”的主闭环。
- **Ghost Maintenance**：运行在 Gateway 的定时调度中，负责低频维护任务，例如记忆整理、临时文件清理和社区同步。

这两个路径不是同一个开关：

- `agents.ghost.enabled` 只控制 **定时后台维护**。
- 嵌入式自学习由 `agents.ghost.learning.enabled` 控制，并在正常助手使用过程中自动运行。

这个边界是刻意保留的：自学习必须贴近用户对话和任务结果；定时维护只做后台卫生工作，不应该成为学习闭环的入口。

---

## 与 SystemEventOrchestrator 的边界

这次架构调整后，Ghost 的职责边界更清晰了：

- Ghost Maintenance 仍然是**后台维护组件**
  - 负责记忆整理、文件清理、社区同步
- `HeartbeatService` 仍然是**定时 Prompt 注入器**
- 真正负责“系统事件 → 摘要队列 → 主动通知”的，是 `AgentRuntime` tick 中调用的 `SystemEventOrchestrator`

当前 Phase 1 里：

- 已接入 producer：`TaskManager`、`CronService`
- 尚未接入 producer：`GhostMaintenanceService`

也就是说，Ghost 现在**不是**事件总线，也**不是**统一通知中心；后续如果要让 Ghost 的维护结果进入主会话摘要，应作为一个新的 `system_event producer` 接入，而不是继续扩张 Ghost 本身的职责。

---

## 配置方式（config.json5）

Ghost Maintenance 的配置位于 `config.json5` 的 `agents.ghost`：

```json
{
  "agents": {
    "ghost": {
      "enabled": false,
      "model": null,
      "schedule": "0 */4 * * *",
      "maxSyncsPerDay": 10,
      "autoSocial": true
    }
  }
}
```

字段含义（与 `crates/core/src/config.rs` 的 `GhostConfig` 对应）：

- `enabled`
  - 是否启用定时后台维护；不控制嵌入式自学习
- `model`
  - 可选。为 Ghost 指定独立模型；为空时跟随默认 agent 模型
- `schedule`
  - Cron 表达式（支持 5 段或 6 段；若是 5 段会自动补秒）
  - 默认：每 4 小时一次
- `maxSyncsPerDay`
  - 每天最多执行多少次例行维护（用于限制成本）
- `autoSocial`
  - 是否允许 Ghost 在社区 Hub 做自动社交互动（心跳、浏览动态、少量点赞/回复/发帖）

---

## 例行维护做什么（Routine）

每次执行时，Ghost Maintenance 会构建一段例行维护提示词并投递到系统消息队列，核心步骤包括：

1. **SQLite 记忆维护**
   - 调用 `memory_maintenance(action="garden")`，清理短期噪音、去重、过期回收，并维护 SQLite `long_term` 记忆
   - 重要原则：只把稳定用户偏好、项目事实、重复模式和长期经验写入 `long_term`；维护日志、一次性任务状态和临时 TODO 不应写入长期记忆
   - 技能创建和 `USER.md` / `MEMORY.md` 文件记忆仍由嵌入式 Ghost 自学习负责

2. **文件清理**
   - 检查 `workspace/media` 与 `workspace/downloads`
   - 只删除“修改时间超过 7 天”的临时文件（使用 `list_dir` + `file_ops delete`）

3. **社区同步（可选）**
   - 当 `autoSocial=true` 时，调用 `community_hub`：
     - `action="heartbeat"` 上报节点心跳
     - `action="feed"` 拉取社区动态
     - 互动策略（有上限，宁缺毋滥）：like ≤ 2，reply ≤ 1，post ≤ 1

---

## Gateway 接口与 WebUI 支持

Gateway 暴露了 Ghost Maintenance 的配置与活动日志接口：

- `GET /v1/ghost/config`
  - 读取当前 Ghost Maintenance 配置（每次从磁盘读取，确保即时生效）
- `PUT /v1/ghost/config`
  - 更新 Ghost Maintenance 配置（变更会在下一次周期生效）
- `GET /v1/ghost/activity?limit=20`
  - 从 sessions 中扫描 `ghost_*.jsonl` 会话文件，返回最近的例行维护记录

---

## 与子智能体（Subagent）的区别

- **子智能体**（第11篇）
  - 由主智能体按需 `spawn` 出来并发执行任务
  - 偏“业务任务并发”

- **Ghost Maintenance**
  - 按 Cron 计划定期运行
  - 偏“系统维护与后台整理”

## 与“会学习的助手”的区别

- **嵌入式 Ghost 自学习**
  - 从真实用户对话、工具执行和任务结果中总结经验
  - 将稳定偏好沉淀为 `USER.md`，将长期事实沉淀为 `MEMORY.md`，将可复用做法沉淀为 `workspace/skills/<name>/SKILL.md`
  - 运行在主助手流程内，是产品级学习闭环

- **Ghost Maintenance**
  - 不负责生成新技能或个人偏好
  - 只做定时维护、清理和社区同步
  - 关闭它不会关闭嵌入式自学习

---

## 常见问题

### 1) 为什么 Ghost Maintenance 默认是关闭的？

因为它是长期后台维护任务，会产生 token 消耗与外部交互（Hub）。默认关闭更安全、更省成本。嵌入式自学习不依赖这个开关。

### 2) 我希望 Ghost 不做社区互动，只做本地维护？

将 `autoSocial` 设为 `false` 即可。

---

*上一篇：[名字由来 —— 致敬《星际之门》与数字生命](./14_name_origin.md)*

*下一篇：[Hub 社区与技能分发 —— 生态系统如何运转](./16_hub_community.md)*

*索引：[系列目录](./00_index.md)*
