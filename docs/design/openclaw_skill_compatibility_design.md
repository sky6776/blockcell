# OpenClaw Skill 兼容性设计文档

> 版本: 2.0
> 日期: 2026-04-11
> 状态: 设计评审
> 作者: Claude

---

## 目录

1. [概述](#1-概述)
2. [OpenClaw Skill 系统深度分析](#2-openclaw-skill-系统深度分析)
3. [BlockCell Skill 系统深度分析](#3-blockcell-skill-系统深度分析)
4. [两系统对比分析](#4-两系统对比分析)
5. [兼容性设计方案](#5-兼容性设计方案)
6. [SkillSource 枚举设计](#6-skillsource-枚举设计)
7. [解析阶段](#7-解析阶段)
8. [加载阶段](#8-加载阶段)
9. [执行阶段](#9-执行阶段)
10. [Prompt 注入阶段](#10-prompt-注入阶段)
11. [自进化与版本管理](#11-自进化与版本管理)
12. [测试计划](#12-测试计划)
13. [实现优先级](#13-实现优先级)
14. [附录](#附录)

---

## 1. 概述

### 1.1 背景

BlockCell 当前仅支持原生 BlockCell skill 格式 (`meta.yaml`/`meta.json` + `SKILL.md` 锚点区块)。为了扩展技能生态，需要支持 OpenClaw skill 格式，使 BlockCell 能够加载和运行 OpenClaw 技能市场中的技能。

### 1.2 目标

- 完整理解 OpenClaw 和 BlockCell 两套 skill 系统的架构差异
- 支持 OpenClaw skill 的 YAML frontmatter 格式解析
- 支持 OpenClaw 的脚本执行模型（通过 exec 工具执行外部脚本）
- 支持斜杠命令 (`/stock`, `/portfolio` 等)
- 支持 `metadata.openclaw.requires` 依赖检查
- 支持 `{baseDir}` 占位符替换
- 非 BlockCell skill 不触发自进化系统

### 1.3 非目标

- ZeroClaw skill 支持（暂不考虑）
- Claude Code skill 支持（暂不考虑）
- OpenClaw Hub API 集成（后续迭代）
- OpenClaw 沙箱系统（Docker-based sandbox）的移植

---

## 2. OpenClaw Skill 系统深度分析

### 2.1 技术栈

OpenClaw 使用 TypeScript/Node.js 构建，基于 `@mariozechner/pi-coding-agent` 上游库扩展。

### 2.2 Skill 定义格式

OpenClaw skill 是一个目录，核心文件为 `SKILL.md`，使用 YAML frontmatter + Markdown 正文格式。

**SKILL.md 示例：**

```yaml
---
name: github
description: "GitHub operations via `gh` CLI: issues, PRs, CI runs, code review..."
metadata:
  {
    "openclaw":
      {
        "emoji": "🐙",
        "requires": { "bins": ["gh"] },
        "install":
          [
            { "id": "brew", "kind": "brew", "formula": "gh", "bins": ["gh"] },
          ],
      },
  }
---

# GitHub Skill

[Markdown 正文 - 指导 LLM 如何使用 gh CLI]
```

**关键源文件：**
- `src/agents/skills/types.ts` — 类型定义
- `src/agents/skills/frontmatter.ts` — frontmatter 解析
- `src/agents/skills/local-loader.ts` — 安全加载
- `src/agents/skills/workspace.ts` — 多源发现与合并

### 2.3 Frontmatter 完整字段

**顶层字段（字符串解析后转换）：**

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `name` | string | 目录名 | 技能名称（必填） |
| `description` | string | — | 技能描述（必填） |
| `homepage` | string | — | 主页 URL |
| `user-invocable` | bool | true | 用户是否可直接调用 |
| `disable-model-invocation` | bool | false | 是否禁止模型自动调用 |

**`metadata.openclaw` 块（OpenClawSkillMetadata）：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `always` | bool | 是否始终加载到 prompt |
| `skillKey` | string | 技能唯一标识 |
| `primaryEnv` | string | 主要环境变量名 |
| `emoji` | string | 显示图标 |
| `homepage` | string | 主页 URL |
| `os` | string[] | 支持的操作系统 (`darwin`, `linux`, `win32`) |
| `requires.bins` | string[] | 必需的二进制程序 |
| `requires.anyBins` | string[] | 任一存在即可的二进制程序 |
| `requires.env` | string[] | 必需的环境变量 |
| `requires.config` | string[] | 必需的配置路径 |
| `install` | SkillInstallSpec[] | 安装说明 |

**SkillInstallSpec（安装规格）：**

```typescript
type SkillInstallSpec = {
  id?: string;
  kind: "brew" | "node" | "go" | "uv" | "download";
  label?: string;
  bins?: string[];
  os?: string[];
  formula?: string;   // brew
  package?: string;    // node/uv
  module?: string;     // go
  url?: string;        // download
  archive?: string;
  extract?: boolean;
  stripComponents?: number;
  targetDir?: string;
};
```

**SkillCommandSpec（命令规格）：**

```typescript
type SkillCommandSpec = {
  name: string;
  skillName: string;
  description: string;
  dispatch?: {
    kind: "tool";
    toolName: string;
    argMode?: "raw";
  };
  promptTemplate?: string;
  sourceFilePath?: string;
};
```

**SkillInvocationPolicy（调用策略）：**

```typescript
type SkillInvocationPolicy = {
  userInvocable: boolean;          // 默认 true
  disableModelInvocation: boolean; // 默认 false
};
```

### 2.4 脚本/工具类型

OpenClaw skill 中的脚本类型：

| 类型 | 位置 | 执行方式 |
|------|------|----------|
| Python 脚本 | `scripts/*.py` | 通过 exec 工具调用 |
| Shell 脚本 | `scripts/*.sh` | 通过 exec 工具调用 |
| Lobster 流程 | `examples/*.lobster` | TaskFlow 引擎 |
| CLI 命令 | SKILL.md 正文中描述 | 通过 exec 工具调用 |

**核心要点：OpenClaw 没有内置的"技能脚本引擎"。** 技能本质上是 Markdown 指令文档，指导 LLM 使用 exec 工具来执行脚本。脚本执行完全依赖 exec 工具（bash-tools）。

### 2.5 Exec 工具参数 Schema

OpenClaw 的 exec 工具是脚本执行的核心通道：

```typescript
// bash-tools.exec-runtime.ts
const execSchema = {
  command: string,        // 必填：Shell 命令
  workdir?: string,       // 工作目录
  env?: Record<string, string>,  // 环境变量
  yieldMs?: number,       // 后台化等待时间（毫秒）
  background?: boolean,   // 是否后台运行
  timeout?: number,       // 超时（秒）
  pty?: boolean,          // 是否使用伪终端
  elevated?: boolean,     // 是否提权运行
  host?: "auto" | "sandbox" | "gateway" | "node",  // 执行目标
  security?: "deny" | "allowlist" | "full",         // 安全模式
  ask?: "off" | "on-miss" | "always",               // 审批模式
  node?: string,          // 节点 ID（host=node 时）
};
```

**执行结果格式：**

```typescript
{
  status: "completed" | "failed",
  exitCode: number | null,
  durationMs: number,
  aggregated: string,  // stdout + stderr 合并输出
  timedOut?: boolean,
  cwd?: string,
}
```

### 2.6 内置工具列表

OpenClaw 通过 `createOpenClawTools()` 注册的核心工具：

| 工具名 | 用途 | 源文件 |
|--------|------|--------|
| `canvas` | 结构化内容/UI 画布 | `tools/canvas-tool.ts` |
| `nodes` | 节点执行编排 | `tools/nodes-tool.ts` |
| `cron` | 定时任务调度 | `tools/cron-tool.ts` |
| `message` | 发送消息到渠道 | `tools/message-tool.ts` |
| `tts` | 文本转语音 | `tools/tts-tool.ts` |
| `image_generate` | 图片生成 | `tools/image-generate-tool.ts` |
| `music_generate` | 音乐生成 | `tools/music-generate-tool.ts` |
| `video_generate` | 视频生成 | `tools/video-generate-tool.ts` |
| `gateway` | 网关交互 | `tools/gateway-tool.ts` |
| `agents_list` | 列出 Agent | `tools/agents-list-tool.ts` |
| `update_plan` | 更新计划（条件启用） | `tools/update-plan-tool.ts` |
| `sessions_list` | 列出会话 | `tools/sessions-list-tool.ts` |
| `sessions_history` | 会话历史 | `tools/sessions-history-tool.ts` |
| `sessions_send` | 发送会话消息 | `tools/sessions-send-tool.ts` |
| `sessions_spawn` | 创建子会话 | `tools/sessions-spawn-tool.ts` |
| `sessions_yield` | 会话让出 | `tools/sessions-yield-tool.ts` |
| `session_status` | 会话状态 | `tools/session-status-tool.ts` |
| `subagents` | 子 Agent 管理 | `tools/subagents-tool.ts` |
| `web_search` | 网页搜索 | `tools/web-tools.ts` |
| `web_fetch` | 网页抓取 | `tools/web-tools.ts` |
| `image` | 图片处理 | `tools/image-tool.ts` |
| `pdf` | PDF 处理 | `tools/pdf-tool.ts` |

此外还支持插件工具（plugin tools）动态注册。

### 2.7 Skill 生命周期

```text
发现 → 加载 → 过滤 → Prompt 注入 → 运行时执行
```

**发现（多源优先级，后者覆盖前者）：**
1. `extra dirs`（配置额外目录）
2. `bundled`（内置技能目录）
3. `managed`（`~/.openclaw/skills`）
4. `personal agents`（`~/.agents/skills`）
5. `project agents`（`<workspace>/.agents/skills`）
6. `workspace`（`<workspace>/skills`）

**加载安全机制：**
- 不跟随符号链接
- 文件大小上限（256KB）
- 路径逃逸检查（realpath 验证）
- 每个源最多 200 个技能
- Prompt 中最多 150 个技能，30,000 字符

**Prompt 注入格式：**

```xml
<available_skills>
  <skill>
    <name>github</name>
    <description>GitHub operations via gh CLI...</description>
    <location>/path/to/skills/github</location>
  </skill>
</available_skills>
```

超出预算时自动切换为紧凑格式（仅 name + location）。

### 2.8 沙箱系统

OpenClaw 有完整的 Docker-based 沙箱：
- `Dockerfile.sandbox` — 基础沙箱
- `Dockerfile.sandbox-browser` — 带浏览器的沙箱
- `sandbox-tool-policy.ts` — 沙箱工具策略
- 支持 `host` 参数选择执行目标（auto/sandbox/gateway/node）

### 2.9 Skill 过滤与配置

通过 `src/agents/skills/config.ts` 控制技能是否加载：
- 配置级别：`skills.entries[skillKey].enabled`
- 内置技能白名单：`skills.allowBundled`
- 运行时资格：OS 匹配、bins/env/config 检查

---

## 3. BlockCell Skill 系统深度分析

### 3.1 技术栈

BlockCell 使用 Rust 构建，Rhai 作为内嵌脚本引擎，Tokio 作为异步运行时。

### 3.2 Skill 定义格式

BlockCell skill 是一个目录，包含以下文件组合：

| 文件 | 用途 | 必需 |
|------|------|------|
| `meta.yaml` 或 `meta.json` | 元数据定义 | 推荐 |
| `SKILL.md` | Prompt 文档（锚点区块格式） | 推荐 |
| `SKILL.rhai` | Rhai 编排脚本 | 可选 |
| `SKILL.py` | Python 入口脚本 | 可选 |
| `scripts/*` | 辅助脚本资产 | 可选 |

**关键源文件：**
- `crates/skills/src/manager.rs` — 技能管理器、元数据加载、可用性检查
- `crates/skills/src/engine.rs` — Rhai 引擎配置与限制
- `crates/skills/src/dispatcher.rs` — Rhai 脚本工具调用分发
- `crates/skills/src/evolution.rs` — 自进化类型定义
- `crates/skills/src/versioning.rs` — 版本管理
- `crates/tools/src/exec_skill_script.rs` — 技能脚本执行桥接
- `crates/tools/src/exec_local.rs` — 进程脚本执行
- `crates/tools/src/skills.rs` — 技能列表与资产检测
- `crates/agent/src/context.rs` — ActiveSkillContext 定义
- `crates/agent/src/runtime.rs` — Agent 运行时集成

### 3.3 SkillMeta 完整字段

```rust
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub requires: SkillRequires,      // { bins: Vec<String>, env: Vec<String> }
    pub permissions: Vec<String>,
    pub always: bool,
    pub tools: Vec<String>,           // 主要：允许使用的工具列表
    pub capabilities: Vec<String>,    // 遗留兼容字段
    pub output_format: Option<String>,// "markdown" | "json" | "table"
    pub fallback: Option<SkillFallback>,
}

pub struct SkillRequires {
    pub bins: Vec<String>,  // 必需的二进制程序
    pub env: Vec<String>,   // 必需的环境变量
}

pub struct SkillFallback {
    pub strategy: String,              // "degrade" | "skip" | "alternative"
    pub message: Option<String>,
    pub alternative_skill: Option<String>,
}
```

工具列表解析逻辑：
```rust
impl SkillMeta {
    pub fn effective_tools(&self) -> Vec<String> {
        if !self.tools.is_empty() { self.tools.clone() }
        else { self.capabilities.clone() }
    }
}
```

### 3.4 SKILL.md 格式（锚点区块）

BlockCell 使用 Markdown 锚点区块（而非 YAML frontmatter）：

```markdown
## Shared {#shared}
通用指令...

## Prompt {#prompt}
激活时注入的 prompt...

## Planning {#planning}
规划阶段的指令...

## Summary {#summary}
总结阶段的指令...
```

**Bundle 构建规则：**
- `prompt_bundle = shared + prompt`
- `planning_bundle = shared + planning`
- `summary_bundle = shared + summary`
- 如果没有保留区块，整个文档作为所有 bundle

支持本地 Markdown 链接展开（限制在技能目录内）。

### 3.5 脚本/工具类型

BlockCell 支持两种脚本执行路径：

**路径一：Rhai 进程内执行（`SKILL.rhai`）**

| 特性 | 值 |
|------|-----|
| 运行时 | Rhai 嵌入式脚本引擎 |
| 执行方式 | 进程内（in-process） |
| 最大操作数 | 100,000 |
| 超时 | 30 秒 |
| 最大字符串 | 1,000,000 字符 |
| 最大数组 | 10,000 元素 |
| 最大 Map | 10,000 条目 |
| 调用栈深度 | 64 |
| 表达式深度 | 64 |

**Rhai 可用的注册函数（dispatcher.rs）：**

| 函数 | 说明 |
|------|------|
| `call_tool(name, params)` | 调用任意注册工具 |
| `call_tool_json(name, json)` | JSON 参数调用工具 |
| `set_output(value)` | 设置输出值 |
| `set_output_json(json)` | JSON 格式设置输出 |
| `log(msg)` / `log_warn(msg)` | 日志 |
| `to_json(val)` / `from_json(str)` | JSON 转换 |
| `exec(command)` | 执行命令（快捷方式） |
| `web_search(query)` | 网页搜索 |
| `web_fetch(url)` | 网页抓取 |
| `read_file(path)` | 读取文件 |
| `write_file(path, content)` | 写入文件 |
| `http_request(url)` | HTTP GET |
| `message(content)` | 发送消息 |
| `sleep_ms(ms)` | 休眠（上限 10 秒） |
| `timestamp()` | 当前时间戳 |
| `is_map/is_string/is_array/is_error` | 类型检查 |
| `str_sub/str_truncate/str_lines` | 字符串操作 |
| `arr_join/len` | 数组操作 |

**Rhai 执行结果：**
```json
{
  "runtime": "rhai",
  "path": "SKILL.rhai",
  "resolved_path": "/abs/path/to/SKILL.rhai",
  "success": true,
  "output": { ... },
  "error": null,
  "tool_calls": [
    { "tool_name": "web_fetch", "params": {...}, "result": {...}, "success": true }
  ]
}
```

**路径二：进程执行（非 .rhai 文件）**

| 特性 | 值 |
|------|-----|
| 运行时 | 操作系统进程 |
| 允许的 Runner | `python3`, `bash`, `sh`, `node`, `php` |
| 工作目录 | 技能目录 |
| stdin | null |
| stdout/stderr | piped，上限 10,000 字符 |
| 超时 | 配置文件中 `tools.exec.timeout` |

**进程执行结果：**
```json
{
  "runtime": "process",
  "path": "scripts/analyze.py",
  "resolved_path": "/abs/path/to/scripts/analyze.py",
  "success": true,
  "exit_code": 0,
  "stdout": "...",
  "stderr": "",
  "command": "python3 /abs/path/to/scripts/analyze.py"
}
```

### 3.6 exec_skill_script 工具（桥接层）

这是 BlockCell 技能脚本执行的核心桥接工具：

**参数 Schema：**

```json
{
  "path": "string (必填) - 技能目录内的相对路径",
  "runner": "string (可选) - 解释器: python3/bash/sh/node/php",
  "args": ["string"] ,
  "cwd_mode": "skill (唯一选项)",
  "user_input": "string (可选) - Rhai user_input 值",
  "context": "object (可选) - Rhai 上下文变量"
}
```

**安全机制：**
1. 必须在活跃技能作用域内（`ctx.active_skill_dir` 必须存在）
2. 路径必须是相对路径，不能包含 `..`
3. canonicalize 验证最终路径在技能目录内
4. Rhai 内禁止嵌套调用 `exec_skill_script`

**运行时选择：**
```rust
fn resolve_runtime(path: &str) -> ScriptRuntime {
    if path.ends_with(".rhai") { ScriptRuntime::Rhai }
    else { ScriptRuntime::Process }
}
```

### 3.7 资产检测

`list_skills` 工具扫描技能目录时识别的文件类型：

| 扩展名 | 类型 |
|--------|------|
| `.rhai` | Rhai 脚本 |
| `.py` | Python 脚本 |
| `.sh` | Shell 脚本 |
| `.php` | PHP 脚本 |
| `.js` | JavaScript 脚本 |
| `.ts` | TypeScript 脚本 |
| `.rb` | Ruby 脚本 |
| Unix 可执行文件 | 任意可执行文件 |

### 3.8 Skill 生命周期

```text
发现 → 加载元数据 → 可用性检查 → 构建 SkillCard → Prompt 注入 → 激活 → 执行
```

**发现（两层，workspace 覆盖 builtin）：**
1. `builtin_skills_dir`（内置技能）
2. `~/.blockcell/workspace/skills`（用户技能，优先级更高）

**可用性检查：**
- 必需二进制程序存在（`which` 检查）
- 必需环境变量存在
- 引用的工具存在（除非是内置工具或包含 `__`）

**激活机制：**
- Agent 运行时暴露 `activate_skill` 合成工具
- 模型选择技能后，`ActiveSkillContext` 被创建
- 工具列表限制为技能声明的 `effective_tools()`
- `ToolContext.active_skill_dir` 被设置，使 `exec_skill_script` 可用

```rust
pub struct ActiveSkillContext {
    pub name: String,
    pub prompt_md: String,
    pub inject_prompt_md: bool,
    pub tools: Vec<String>,
    pub fallback_message: Option<String>,
    // 工程评审 Issue 4：新增 source 字段，用于运行时区分技能来源
    // 自进化屏蔽（第 11.2 节）依赖此字段
    pub source: SkillSource,
}
```

### 3.9 自进化系统

BlockCell 独有的自进化机制：
- 错误追踪 → 阈值触发 → 生成修复 → 审计 → 编译检查 → 测试 → 观察 → 滚动发布
- 进化记录存储在 `~/.blockcell/workspace/evolution_records/*.json`
- 版本快照存储在 `versions/vN/` 目录

### 3.10 内置工具列表（核心工具）

BlockCell 通过 `ToolRegistry::with_defaults()` 注册的核心工具：

```rust
GLOBAL_CORE_TOOL_NAMES: [
    "memory_query", "memory_upsert", "memory_forget",
    "spawn", "list_tasks", "agent_status",
    "list_skills", "cron", "toggle_manage",
    "web_fetch"
]
```

此外还有大量非核心工具：文件操作、网络、通讯、数据处理、浏览器控制、系统信息等 50+ 工具。

---

## 4. 两系统对比分析

### 4.1 架构对比总览

| 维度 | OpenClaw | BlockCell |
|------|----------|-----------|
| 语言 | TypeScript/Node.js | Rust |
| Skill 格式 | YAML frontmatter + Markdown | meta.yaml/json + 锚点区块 Markdown |
| 脚本引擎 | 无内置引擎，依赖 exec 工具 | Rhai 内嵌引擎 + 进程执行 |
| 脚本执行模型 | LLM 决定调用 exec → 外部进程 | Rhai 进程内 或 外部进程 |
| 工具注册 | TypeScript 函数 + JSON Schema | Rust Tool trait + serde Schema |
| 沙箱 | Docker-based sandbox | 无（Rhai 资源限制） |
| 自进化 | 无 | 有（错误追踪→修复→审计→发布） |
| 技能发现 | 6 层优先级（extra→workspace） | 2 层（builtin→user） |
| 技能激活 | 始终注入 prompt 或按需 | activate_skill 合成工具 |
| 版本管理 | 无内置 | 有（vN/ 快照） |
| 斜杠命令 | SkillCommandSpec 定义 | SlashCommand trait |

### 4.2 Skill 元数据对比

| 字段 | OpenClaw | BlockCell | 兼容性 |
|------|----------|-----------|--------|
| name | frontmatter `name` | meta.yaml `name` | 直接映射 |
| description | frontmatter `description` | meta.yaml `description` | 直接映射 |
| requires.bins | `metadata.openclaw.requires.bins` | `requires.bins` | 直接映射 |
| requires.env | `metadata.openclaw.requires.env` | `requires.env` | 直接映射 |
| requires.anyBins | `metadata.openclaw.requires.anyBins` | 无 | 需扩展 |
| requires.config | `metadata.openclaw.requires.config` | 无 | 需扩展 |
| always | `metadata.openclaw.always` | `always` | 直接映射 |
| tools | 无（LLM 自行决定） | `tools` 列表 | OpenClaw 无此概念 |
| permissions | 无 | `permissions` | BlockCell 独有 |
| output_format | 无 | `output_format` | BlockCell 独有 |
| fallback | 无 | `fallback` | BlockCell 独有 |
| emoji | `metadata.openclaw.emoji` | 无 | 可扩展 |
| os | `metadata.openclaw.os` | 无 | 需扩展 |
| install | `metadata.openclaw.install` | 无 | 需扩展 |
| user-invocable | frontmatter 字段 | 无（默认可调用） | 需扩展 |
| disable-model-invocation | frontmatter 字段 | 无 | 需扩展 |
| commands | SkillCommandSpec[] | 无 | 需扩展 |

### 4.3 脚本执行模型对比

| 维度 | OpenClaw | BlockCell |
|------|----------|-----------|
| 执行触发 | LLM 在对话中决定调用 exec | Rhai: exec_skill_script 工具; Process: exec_skill_script 工具 |
| 脚本发现 | LLM 读取 SKILL.md 中的指令 | meta.yaml 中声明 或 资产扫描 |
| 参数传递 | exec 工具的 command 字段 | Rhai: context/user_input; Process: args |
| 环境变量 | exec 工具的 env 字段 | 继承进程环境 |
| 工作目录 | exec 工具的 workdir 字段 | 技能目录（固定） |
| 超时控制 | exec 工具的 timeout 字段 | Rhai: 30s; Process: 配置文件 |
| 后台执行 | exec 工具的 background 字段 | 无 |
| 安全模型 | sandbox + security 字段 + ask 审批 | 路径限制 + Rhai 资源限制 |
| 占位符 | `{baseDir}` → 技能目录 | 无 |

### 4.4 关键差异总结

1. **脚本执行哲学不同**：OpenClaw 让 LLM 自主决定何时执行脚本（通过 exec 工具），BlockCell 通过 Rhai 引擎或 exec_skill_script 工具执行
2. **工具约束不同**：BlockCell 通过 `tools` 列表限制技能可用工具，OpenClaw 无此限制
3. **Prompt 格式不同**：OpenClaw 用 YAML frontmatter，BlockCell 用锚点区块
4. **沙箱模型不同**：OpenClaw 有 Docker 沙箱，BlockCell 依赖 Rhai 资源限制
5. **自进化**：BlockCell 独有，OpenClaw 无此机制

---

## 5. 兼容性设计方案

### 5.1 设计原则

1. **最小侵入**：不修改现有 BlockCell skill 的加载和执行路径
2. **格式自动检测**：根据 SKILL.md 是否包含 YAML frontmatter 自动判断来源
3. **语义映射**：将 OpenClaw 字段映射到 BlockCell 的 SkillMeta
4. **执行桥接**：OpenClaw 脚本通过 BlockCell 的 exec_skill_script 工具执行
5. **安全优先**：OpenClaw skill 不触发自进化系统

### 5.2 整体架构

```text
OpenClaw SKILL.md (YAML frontmatter)
        │
        ▼
┌─────────────────────┐
│  格式检测器          │  检测 YAML frontmatter vs 锚点区块
│  (detect_format)     │
└─────────┬───────────┘
          │
    ┌─────┴─────┐
    ▼           ▼
OpenClaw     BlockCell
Parser       Parser (现有)
    │           │
    ▼           ▼
┌─────────────────────┐
│  统一 SkillMeta      │  两种格式映射到同一结构
└─────────┬───────────┘
          │
          ▼
┌─────────────────────┐
│  可用性检查          │  bins/env/os 检查
│  (availability)      │
└─────────┬───────────┘
          │
          ▼
┌─────────────────────┐
│  SkillCard 构建      │  prompt bundle + 工具列表
└─────────┬───────────┘
          │
          ▼
┌─────────────────────┐
│  Prompt 注入         │  注入到 Agent system prompt
└─────────┬───────────┘
          │
          ▼
┌─────────────────────┐
│  运行时执行          │  exec_skill_script 桥接
└─────────────────────┘
```

### 5.3 兼容性矩阵

| OpenClaw 特性 | BlockCell 支持策略 | 优先级 |
|---------------|-------------------|--------|
| YAML frontmatter 解析 | 新增解析器 | P0 |
| `requires.bins` 检查 | 直接映射 | P0 |
| `requires.env` 检查 | 直接映射 | P0 |
| `requires.anyBins` 检查 | 扩展 SkillRequires | P1 |
| `requires.config` 检查 | 扩展 SkillRequires | P2 |
| `metadata.openclaw.os` 过滤 | 新增 OS 检查 | P1 |
| `metadata.openclaw.always` | 直接映射 | P0 |
| `metadata.openclaw.emoji` | 扩展 SkillMeta | P2 |
| `{baseDir}` 占位符 | 新增替换逻辑 | P0 |
| SkillCommandSpec 斜杠命令 | 注册到 SlashCommand | P1 |
| install 规格 | 提示用户手动安装 | P2 |
| Docker sandbox | 不支持（安全降级） | 不实现 |
| user-invocable | 扩展 SkillMeta | P1 |
| disable-model-invocation | 扩展 SkillMeta | P1 |

---

## 6. SkillSource 枚举设计

### 6.1 来源标识

为区分不同来源的技能，引入 `SkillSource` 枚举：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSource {
    /// BlockCell 原生格式 (meta.yaml + 锚点区块 SKILL.md)
    BlockCell,
    /// OpenClaw 格式 (YAML frontmatter SKILL.md)
    OpenClaw,
}
```

### 6.2 SkillMeta 扩展

```rust
pub struct SkillMeta {
    // --- 现有字段 ---
    pub name: String,
    pub description: String,
    pub requires: SkillRequires,
    pub permissions: Vec<String>,
    pub always: bool,
    pub tools: Vec<String>,
    pub capabilities: Vec<String>,
    pub output_format: Option<String>,
    pub fallback: Option<SkillFallback>,

    // --- 新增字段 ---
    pub source: SkillSource,
    pub emoji: Option<String>,
    pub os: Option<Vec<String>>,
    pub user_invocable: bool,           // 默认 true
    pub disable_model_invocation: bool, // 默认 false
    pub commands: Vec<SkillCommandSpec>,
    pub install: Vec<SkillInstallSpec>,
}
```

### 6.3 SkillRequires 扩展

```rust
pub struct SkillRequires {
    pub bins: Vec<String>,
    pub env: Vec<String>,
    // --- 新增 ---
    pub any_bins: Vec<String>,   // OpenClaw anyBins
    pub config: Vec<String>,     // OpenClaw config paths
}
```

### 6.4 SkillCommandSpec（新增）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCommandSpec {
    pub name: String,           // 命令名 (如 "stock")
    pub skill_name: String,     // 所属技能名
    pub description: String,    // 命令描述
    pub dispatch: Option<SkillCommandDispatch>,
    pub prompt_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCommandDispatch {
    pub kind: String,           // "tool"
    pub tool_name: String,      // 目标工具名
    pub arg_mode: Option<String>, // "raw"
}
```

### 6.5 SkillInstallSpec（新增）

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInstallSpec {
    pub id: Option<String>,
    pub kind: String,           // "brew" | "node" | "go" | "uv" | "download"
    pub label: Option<String>,
    pub bins: Vec<String>,
    pub os: Option<Vec<String>>,
    pub formula: Option<String>,
    pub package: Option<String>,
    pub module: Option<String>,
    pub url: Option<String>,
}
```

---

## 7. 解析阶段

### 7.1 格式检测

在 `crates/skills/src/manager.rs` 中新增格式检测逻辑：

```rust
/// 检测技能目录的格式来源
///
/// 优先级规则（工程评审 Issue 1 决定）：
/// 1. 有 meta.yaml 或 meta.json → BlockCell（优先，避免误判）
/// 2. SKILL.md 以 "---" 开头 → OpenClaw
/// 3. 默认 → BlockCell
///
/// 性能优化：只读取 SKILL.md 前 8 字节，不读取整个文件
fn detect_skill_format(skill_dir: &Path) -> SkillSource {
    // 优先检查 BlockCell 标志文件（避免 meta.yaml + frontmatter SKILL.md 共存时误判）
    if skill_dir.join("meta.yaml").exists() || skill_dir.join("meta.json").exists() {
        return SkillSource::BlockCell;
    }

    // 检查 SKILL.md 是否使用 YAML frontmatter
    let skill_md = skill_dir.join("SKILL.md");
    if skill_md.exists() {
        // 只读前 8 字节，避免读取整个大文件
        if let Ok(file) = std::fs::File::open(&skill_md) {
            use std::io::Read;
            let mut buf = [0u8; 8];
            let mut reader = std::io::BufReader::new(file);
            if let Ok(n) = reader.read(&mut buf) {
                let prefix = &buf[..n];
                if prefix.starts_with(b"---\n") || prefix.starts_with(b"---\r\n") {
                    return SkillSource::OpenClaw;
                }
            }
        }
    }

    SkillSource::BlockCell // 默认
}
```

### 7.2 OpenClaw Frontmatter 解析

新增 `crates/skills/src/openclaw_parser.rs`：

```rust
use serde::Deserialize;

#[derive(Deserialize)]
struct OpenClawFrontmatter {
    name: Option<String>,
    description: Option<String>,
    homepage: Option<String>,
    #[serde(rename = "user-invocable")]
    user_invocable: Option<bool>,
    #[serde(rename = "disable-model-invocation")]
    disable_model_invocation: Option<bool>,
    metadata: Option<OpenClawMetadataWrapper>,
}

#[derive(Deserialize)]
struct OpenClawMetadataWrapper {
    openclaw: Option<OpenClawSkillMetadata>,
}

#[derive(Deserialize)]
struct OpenClawSkillMetadata {
    always: Option<bool>,
    emoji: Option<String>,
    os: Option<Vec<String>>,
    requires: Option<OpenClawRequires>,
    install: Option<Vec<OpenClawInstallSpec>>,
}

#[derive(Deserialize)]
struct OpenClawRequires {
    bins: Option<Vec<String>>,
    #[serde(rename = "anyBins")]
    any_bins: Option<Vec<String>>,
    env: Option<Vec<String>>,
    config: Option<Vec<String>>,
}

/// CQ-6 修复：补充缺失的 OpenClawInstallSpec 反序列化结构体
#[derive(Deserialize)]
struct OpenClawInstallSpec {
    id: Option<String>,
    kind: String,           // "brew" | "node" | "go" | "uv" | "download"
    label: Option<String>,
    bins: Option<Vec<String>>,
    os: Option<Vec<String>>,
    formula: Option<String>,   // brew
    package: Option<String>,   // node/uv
    module: Option<String>,    // go
    url: Option<String>,       // download
}

/// 将反序列化结构体映射到输出侧的 SkillInstallSpec
fn map_install_spec(spec: &OpenClawInstallSpec) -> SkillInstallSpec {
    SkillInstallSpec {
        id: spec.id.clone(),
        kind: spec.kind.clone(),
        label: spec.label.clone(),
        bins: spec.bins.clone().unwrap_or_default(),
        os: spec.os.clone(),
        formula: spec.formula.clone(),
        package: spec.package.clone(),
        module: spec.module.clone(),
        url: spec.url.clone(),
    }
}

/// 解析 SKILL.md 的 YAML frontmatter，返回 SkillMeta + prompt 正文
pub fn parse_openclaw_skill(
    skill_dir: &Path,
    content: &str,
) -> Result<(SkillMeta, String), SkillError> {
    // 1. 提取 frontmatter (两个 "---" 之间的内容)
    let (yaml_str, body) = extract_frontmatter(content)?;

    // 2. 解析 YAML
    let fm: OpenClawFrontmatter = serde_yaml::from_str(&yaml_str)?;

    // 3. 映射到 SkillMeta
    let oc = fm.metadata.and_then(|m| m.openclaw);
    let requires = oc.as_ref().and_then(|o| o.requires.as_ref());

    let meta = SkillMeta {
        name: fm.name.unwrap_or_else(|| dir_name(skill_dir)),
        description: fm.description.unwrap_or_default(),
        source: SkillSource::OpenClaw,
        requires: SkillRequires {
            bins: requires.and_then(|r| r.bins.clone()).unwrap_or_default(),
            env: requires.and_then(|r| r.env.clone()).unwrap_or_default(),
            any_bins: requires.and_then(|r| r.any_bins.clone()).unwrap_or_default(),
            config: requires.and_then(|r| r.config.clone()).unwrap_or_default(),
        },
        always: oc.as_ref().and_then(|o| o.always).unwrap_or(false),
        emoji: oc.as_ref().and_then(|o| o.emoji.clone()),
        os: oc.as_ref().and_then(|o| o.os.clone()),
        user_invocable: fm.user_invocable.unwrap_or(true),
        disable_model_invocation: fm.disable_model_invocation.unwrap_or(false),
        // 工具列表由 infer_tools_for_openclaw() 按需推断（工程评审 Issue 3 决定）
        tools: infer_tools_for_openclaw(skill_dir, &body),
        install: oc.as_ref()
            .and_then(|o| o.install.as_ref())
            .map(|specs| specs.iter().map(map_install_spec).collect())
            .unwrap_or_default(),
        // 其他字段使用默认值
        ..Default::default()
    };

    // 4. 处理 body 中的 {baseDir} 占位符
    let base_dir = skill_dir.to_string_lossy();
    let body = body.replace("{baseDir}", &base_dir);

    Ok((meta, body))
}

fn extract_frontmatter(content: &str) -> Result<(String, String), SkillError> {
    if !content.starts_with("---") {
        return Err(SkillError::ParseError("Missing frontmatter".into()));
    }
    let rest = &content[3..];
    let end = rest.find("\n---")
        .ok_or_else(|| SkillError::ParseError("Unclosed frontmatter".into()))?;
    let yaml = rest[..end].trim().to_string();
    let body = rest[end + 4..].trim().to_string();
    Ok((yaml, body))
}
```

### 7.3 `{baseDir}` 占位符替换

OpenClaw 技能中常用 `{baseDir}` 引用技能目录路径。在解析阶段统一替换：

```rust
// 在 parse_openclaw_skill 中已包含
let body = body.replace("{baseDir}", &skill_dir.to_string_lossy());
```

这确保 SKILL.md 中类似以下指令能正确工作：

```markdown
Run the analysis script:
python3 {baseDir}/scripts/analyze.py --input data.csv
```

---

## 8. 加载阶段

### 8.1 统一加载流程

修改 `SkillManager::load_skill()` 以支持双格式（注意：现有入口函数是 `load_skill()`，不是 `load_skill_dir()`）：

```rust
/// 修改现有 load_skill 方法，在加载前先检测格式
fn load_skill(&self, skill_dir: &std::path::Path) -> Result<Option<Skill>> {
    let source = detect_skill_format(skill_dir);

    match source {
        SkillSource::OpenClaw => self.load_openclaw_skill(skill_dir),
        SkillSource::BlockCell => self.load_blockcell_skill(skill_dir), // 现有逻辑
    }
}

fn load_openclaw_skill(&self, skill_dir: &Path) -> Result<Option<Skill>> {
    let skill_md_path = skill_dir.join("SKILL.md");

    // 文件大小检查（对齐 OpenClaw 的 256KB 限制）
    let file_meta = std::fs::metadata(&skill_md_path)?;
    if file_meta.len() > 256 * 1024 {
        tracing::warn!(
            path = %skill_md_path.display(),
            size = file_meta.len(),
            "SKILL.md exceeds 256KB limit, skipping"
        );
        return Ok(None);
    }

    let content = std::fs::read_to_string(&skill_md_path)?;

    // 1. 解析 frontmatter + body
    let (meta, prompt_body) = parse_openclaw_skill(skill_dir, &content)?;

    // 2. OS 过滤
    if let Some(ref os_list) = meta.os {
        let current_os = std::env::consts::OS; // "windows", "linux", "macos"
        let mapped = match current_os {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        if !os_list.iter().any(|o| o == mapped) {
            return Err(SkillError::UnsupportedOS(current_os.to_string()).into());
        }
    }

    // 3. 可用性检查 (bins, env, anyBins)
    // 注意：现有 check_availability 返回 (bool, Option<String>)
    // 对 OpenClaw skill 使用新的 check_openclaw_availability 返回 Result
    check_openclaw_availability(&meta)?;

    // 4. 构建 Skill（source 已在 meta.source 中，不重复存储）
    Ok(Some(Skill {
        meta,
        prompt_md: prompt_body,
        dir: skill_dir.to_path_buf(),
    }))
}
```

### 8.2 可用性检查扩展

现有 `check_availability` 返回 `(bool, Option<String>)`，为避免破坏现有 BlockCell skill 的加载路径，新增独立函数处理 OpenClaw 的扩展检查：

```rust
/// OpenClaw 专用可用性检查（新增，不修改现有 check_availability 签名）
///
/// 依赖新增 crate: shellexpand（用于 ~ 路径展开）
/// 需在 crates/skills/Cargo.toml 中添加: shellexpand = "3"
fn check_openclaw_availability(meta: &SkillMeta) -> Result<(), SkillError> {
    // 检查必需的 bins
    for bin in &meta.requires.bins {
        if which::which(bin).is_err() {
            return Err(SkillError::MissingBinary(bin.clone()));
        }
    }

    // 检查 anyBins (任一存在即可)
    if !meta.requires.any_bins.is_empty() {
        let any_found = meta.requires.any_bins.iter()
            .any(|bin| which::which(bin).is_ok());
        if !any_found {
            return Err(SkillError::MissingAnyBinary(
                meta.requires.any_bins.clone()
            ));
        }
    }

    // 检查必需的环境变量
    for env_var in &meta.requires.env {
        if std::env::var(env_var).is_err() {
            return Err(SkillError::MissingEnvVar(env_var.clone()));
        }
    }

    // 检查 config 路径（需要 shellexpand crate）
    for config_path in &meta.requires.config {
        let expanded = shellexpand::tilde(config_path);
        if !Path::new(expanded.as_ref()).exists() {
            return Err(SkillError::MissingConfig(config_path.clone()));
        }
    }

    Ok(())
}
```

### 8.3 安装提示

当可用性检查失败且有 install 规格时，生成安装提示：

```rust
fn generate_install_hint(meta: &SkillMeta, error: &SkillError) -> String {
    if meta.install.is_empty() {
        return format!("Skill '{}' is unavailable: {}", meta.name, error);
    }

    let mut hint = format!(
        "Skill '{}' is unavailable: {}\n\nInstall options:\n",
        meta.name, error
    );

    for spec in &meta.install {
        match spec.kind.as_str() {
            "brew" => {
                if let Some(ref formula) = spec.formula {
                    hint.push_str(&format!("  brew install {}\n", formula));
                }
            }
            "node" => {
                if let Some(ref package) = spec.package {
                    hint.push_str(&format!("  npm install -g {}\n", package));
                }
            }
            "go" => {
                if let Some(ref module) = spec.module {
                    hint.push_str(&format!("  go install {}\n", module));
                }
            }
            "uv" => {
                if let Some(ref package) = spec.package {
                    hint.push_str(&format!("  uv tool install {}\n", package));
                }
            }
            _ => {}
        }
    }

    hint
}
```

---

## 9. 执行阶段

### 9.1 OpenClaw 脚本执行桥接

OpenClaw skill 的脚本执行完全依赖 LLM 阅读 SKILL.md 后决定调用 exec 工具。在 BlockCell 中，这映射为两条路径：

**路径 A：LLM 直接调用 exec_skill_script**

当 OpenClaw SKILL.md 中指示 LLM 执行脚本时（如 `python3 {baseDir}/scripts/analyze.py`），LLM 会调用 BlockCell 的 `exec_skill_script` 工具：

```rust
// 已有工具，无需修改核心逻辑
// OpenClaw skill 激活时，tools 列表包含 "exec_skill_script"
// LLM 根据 SKILL.md 指令决定调用
```

**路径 B：LLM 调用 exec_local（通用命令执行）**

部分 OpenClaw skill 指示 LLM 调用 CLI 工具（如 `gh pr list`），这些通过 BlockCell 的 `exec_local` 工具执行：

```rust
// exec_local 已存在，支持任意命令执行
// OpenClaw skill 默认添加 exec_local 到工具列表
// 因为 OpenClaw 技能的核心执行模型就是通过 exec 工具调用外部命令
```

### 9.2 工具列表按需推断

OpenClaw skill 没有显式的 `tools` 列表。需要根据 SKILL.md 内容和技能目录结构按需推断，同时默认添加 `exec_local`（OpenClaw 的核心执行模型依赖 exec 工具调用外部命令）：

```rust
/// 根据技能目录结构和 SKILL.md 内容按需推断工具列表
///
/// OpenClaw 技能默认包含 exec_local，因为其核心执行模型
/// 就是通过 exec 工具调用外部 CLI 命令。
fn infer_tools_for_openclaw(skill_dir: &Path, skill_body: &str) -> Vec<String> {
    let mut tools = vec![];

    // OpenClaw 技能默认添加 exec_local（核心执行通道）
    tools.push("exec_local".to_string());

    // 如果有脚本文件，添加 exec_skill_script（安全：路径限制在技能目录内）
    let has_scripts = skill_dir.join("scripts").is_dir()
        || skill_dir.join("SKILL.rhai").exists()
        || skill_dir.join("SKILL.py").exists();
    if has_scripts {
        tools.push("exec_skill_script".to_string());
    }

    // 按需推断：扫描 SKILL.md 正文关键词
    let body_lower = skill_body.to_lowercase();
    if body_lower.contains("web_fetch") || body_lower.contains("fetch") {
        tools.push("web_fetch".to_string());
    }
    if body_lower.contains("web_search") || body_lower.contains("search") {
        tools.push("web_search".to_string());
    }
    if body_lower.contains("read_file") || body_lower.contains("read file") {
        tools.push("read_file".to_string());
    }
    if body_lower.contains("write_file") || body_lower.contains("write file") {
        tools.push("write_file".to_string());
    }

    tools
}
```

### 9.3 环境变量注入

OpenClaw exec 工具支持 `env` 参数传递环境变量。BlockCell 的 `exec_local` 和 `exec_skill_script` 需要支持类似功能：

```rust
// exec_local 已支持 env 参数
// exec_skill_script 的进程执行路径也已支持环境变量继承
// 无需额外修改
```

### 9.4 工作目录处理

| 场景 | OpenClaw 行为 | BlockCell 映射 |
|------|--------------|----------------|
| 默认 | 项目工作目录 | `exec_local` 默认 cwd |
| 技能脚本 | `{baseDir}` 指向技能目录 | `exec_skill_script` 的 `cwd_mode: "skill"` |
| 自定义 | exec 的 `workdir` 参数 | `exec_local` 的 `cwd` 参数 |

### 9.5 超时与后台执行

| 特性 | OpenClaw | BlockCell 当前 | 需要扩展 |
|------|----------|---------------|----------|
| 超时 | exec `timeout` 参数 | exec_local 配置级超时 | 可选：支持参数级超时 |
| 后台执行 | exec `background` 参数 | 无 | P2：后续迭代 |
| PTY | exec `pty` 参数 | 无 | 不实现 |

### 9.6 斜杠命令桥接

OpenClaw 的 SkillCommandSpec 需要注册为 BlockCell 的斜杠命令。

**前置改造（工程评审 Issue 2 决定）：**

当前 `SLASH_COMMAND_HANDLER` 是 `once_cell::Lazy<Arc<SlashCommandHandler>>` 全局静态实例，初始化后不可变。OpenClaw 技能是运行时动态加载的，命令需要动态注册/注销。需要改为 `RwLock`：

```rust
// registry.rs — 改造全局 handler 支持动态注册
pub static SLASH_COMMAND_HANDLER: once_cell::sync::Lazy<Arc<RwLock<SlashCommandHandler>>> =
    once_cell::sync::Lazy::new(|| Arc::new(RwLock::new(create_default_handler())));
```

**注册逻辑：**

```rust
/// 将 OpenClaw skill 的 commands 注册为斜杠命令
fn register_openclaw_commands(skill: &LoadedSkill) {
    let mut handler = SLASH_COMMAND_HANDLER.write().unwrap();
    for cmd in &skill.meta.commands {
        let openclaw_cmd = OpenClawCommandHandler {
            skill_name: skill.meta.name.clone(),
            command: cmd.clone(),
            skill_dir: skill.dir.clone(),
        };
        handler.register(openclaw_cmd);
    }
}

/// 技能卸载时注销对应的斜杠命令
fn unregister_openclaw_commands(skill_name: &str, commands: &[SkillCommandSpec]) {
    let mut handler = SLASH_COMMAND_HANDLER.write().unwrap();
    for cmd in commands {
        handler.unregister(&cmd.name);
    }
}

struct OpenClawCommandHandler {
    skill_name: String,
    command: SkillCommandSpec,
    skill_dir: PathBuf,
}

#[async_trait]
impl SlashCommand for OpenClawCommandHandler {
    fn name(&self) -> &str { &self.command.name }
    fn description(&self) -> &str { &self.command.description }
    fn accepts_args(&self) -> bool { true }

    async fn execute(&self, args: &str, ctx: &CommandContext) -> CommandResult {
        match &self.command.dispatch {
            Some(dispatch) if dispatch.kind == "tool" => {
                // 分发到指定工具
                CommandResult::ForwardToRuntime
            }
            _ => {
                // 使用 prompt_template 或转发到 Agent
                CommandResult::ForwardToRuntime
            }
        }
    }
}
```

**注意：** 所有读取 `SLASH_COMMAND_HANDLER` 的地方需要从 `handler.xxx()` 改为 `handler.read().unwrap().xxx()`。影响范围：Gateway WebSocket handler、CLI 命令分发、Channel adapter。

---

## 10. Prompt 注入阶段

### 10.1 OpenClaw Prompt 注入格式

OpenClaw 使用 XML 标签格式注入技能信息到 system prompt：

```xml
<available_skills>
  <skill>
    <name>github</name>
    <description>GitHub operations via gh CLI</description>
    <location>/path/to/skills/github</location>
  </skill>
</available_skills>
```

### 10.2 BlockCell 当前注入方式

BlockCell 通过 `activate_skill` 合成工具让 LLM 选择技能，激活后将 `prompt_md` 注入到对话上下文。

### 10.3 兼容方案

对于 OpenClaw skill，保持 BlockCell 的激活机制不变，但在 prompt 注入时做以下适配：

```rust
fn build_prompt_for_openclaw_skill(skill: &LoadedSkill) -> String {
    let mut prompt = String::new();

    // 1. 技能描述头
    if let Some(ref emoji) = skill.meta.emoji {
        prompt.push_str(&format!("{} ", emoji));
    }
    prompt.push_str(&format!("# Skill: {}\n\n", skill.meta.name));

    // 2. 技能目录信息（OpenClaw skill 依赖 {baseDir}）
    prompt.push_str(&format!(
        "Skill directory: {}\n\n",
        skill.dir.display()
    ));

    // 3. 可用工具提示
    prompt.push_str("Available tools for this skill:\n");
    for tool in &skill.meta.tools {
        prompt.push_str(&format!("- {}\n", tool));
    }
    prompt.push_str("\n");

    // 4. SKILL.md 正文（已替换 {baseDir}）
    prompt.push_str(&skill.prompt_md);

    prompt
}
```

### 10.4 调用策略控制

根据 OpenClaw 的 `user-invocable` 和 `disable-model-invocation` 字段控制技能可见性：

| user_invocable | disable_model_invocation | 行为 |
|----------------|--------------------------|------|
| true | false | 用户和模型都可调用（默认） |
| true | true | 仅用户可通过斜杠命令调用 |
| false | false | 仅模型可自动调用 |
| false | true | 不可调用（配置错误） |

```rust
fn should_show_in_skill_list(meta: &SkillMeta) -> bool {
    meta.user_invocable
}

fn should_allow_model_activation(meta: &SkillMeta) -> bool {
    !meta.disable_model_invocation
}
```

---

## 11. 自进化与版本管理

### 11.1 原则

OpenClaw skill 不触发 BlockCell 的自进化系统。原因：

1. OpenClaw skill 来自外部生态，自动修改可能破坏兼容性
2. 版本管理由 OpenClaw Hub 或用户手动控制
3. 避免对第三方技能产生意外副作用

### 11.2 实现

```rust
fn should_evolve(skill: &LoadedSkill) -> bool {
    match skill.meta.source {
        SkillSource::BlockCell => true,   // 原生技能可自进化
        SkillSource::OpenClaw => false,   // OpenClaw 技能不自进化
    }
}
```

### 11.3 版本快照

OpenClaw skill 不创建 `versions/vN/` 快照。但会记录加载时的元数据用于审计：

```rust
fn log_skill_load(skill: &LoadedSkill) {
    tracing::info!(
        skill_name = %skill.meta.name,
        source = ?skill.meta.source,
        dir = %skill.dir.display(),
        "Skill loaded"
    );
}
```

---

## 12. 测试计划

### 12.1 单元测试

| 测试 | 描述 | 文件 |
|------|------|------|
| `test_detect_openclaw_format` | YAML frontmatter 格式检测 | `skills/src/manager.rs` |
| `test_detect_blockcell_format` | meta.yaml 格式检测 | `skills/src/manager.rs` |
| `test_detect_format_priority` | meta.yaml + frontmatter SKILL.md 共存时优先识别为 BlockCell（Issue 1 回归） | `skills/src/manager.rs` |
| `test_parse_openclaw_frontmatter` | 完整 frontmatter 解析 | `skills/src/openclaw_parser.rs` |
| `test_parse_minimal_frontmatter` | 最小 frontmatter（仅 name+description） | `skills/src/openclaw_parser.rs` |
| `test_parse_malformed_frontmatter` | 畸形 frontmatter：无效 YAML、未闭合 `---`、空 frontmatter | `skills/src/openclaw_parser.rs` |
| `test_basedir_replacement` | `{baseDir}` 占位符替换 | `skills/src/openclaw_parser.rs` |
| `test_basedir_path_traversal` | `{baseDir}/../../etc/passwd` 路径穿越安全验证 | `skills/src/openclaw_parser.rs` |
| `test_basedir_windows_backslash` | Windows 下 `{baseDir}` 替换后反斜杠在 shell 命令中的行为 | `skills/src/openclaw_parser.rs` |
| `test_os_filtering` | OS 过滤逻辑 | `skills/src/manager.rs` |
| `test_any_bins_check` | anyBins 可用性检查 | `skills/src/manager.rs` |
| `test_config_path_check` | config 路径检查 | `skills/src/manager.rs` |
| `test_install_hint_generation` | 安装提示生成 | `skills/src/manager.rs` |
| `test_tool_inference` | 工具列表按需推断（验证包含 exec_local） | `skills/src/manager.rs` |
| `test_invocation_policy` | 调用策略控制 | `skills/src/manager.rs` |
| `test_oversized_skill_md` | 超过 256KB 的 SKILL.md 应被跳过 | `skills/src/manager.rs` |
| `test_source_propagation` | SkillSource 从 SkillMeta → Skill → ActiveSkillContext 正确传播（Issue 4 回归） | `skills/src/manager.rs` |

### 12.2 集成测试

| 测试 | 描述 |
|------|------|
| `test_load_openclaw_github_skill` | 加载 OpenClaw github 技能目录 |
| `test_load_openclaw_stock_skill` | 加载带 Python 脚本的 stock 技能 |
| `test_openclaw_skill_activation` | 激活 OpenClaw 技能并验证 prompt 注入 |
| `test_openclaw_script_execution` | 通过 exec_skill_script 执行 OpenClaw 脚本 |
| `test_openclaw_slash_command` | OpenClaw 斜杠命令注册与执行 |
| `test_openclaw_command_unregister` | 技能卸载时斜杠命令正确注销（Issue 2 回归） |
| `test_mixed_skill_loading` | 同时加载 BlockCell 和 OpenClaw 技能 |
| `test_openclaw_no_evolution` | 验证 OpenClaw 技能不触发自进化 |

### 12.3 测试数据

在 `tests/fixtures/` 下创建测试用 OpenClaw skill 目录：

```text
tests/fixtures/
├── openclaw_minimal/
│   └── SKILL.md              # 最小 frontmatter（仅 name + description）
├── openclaw_full/
│   ├── SKILL.md              # 完整 frontmatter + requires + install
│   └── scripts/
│       └── test.py           # 测试脚本
├── openclaw_commands/
│   └── SKILL.md              # 带 commands 定义
├── openclaw_malformed/
│   ├── unclosed.md           # 未闭合 frontmatter（只有开头 ---）
│   ├── invalid_yaml.md       # 无效 YAML 内容
│   └── empty_frontmatter.md  # 空 frontmatter（---\n---）
├── openclaw_oversized/
│   └── SKILL.md              # 超过 256KB 的 SKILL.md
├── openclaw_with_meta/
│   ├── meta.yaml             # BlockCell 元数据（优先级测试）
│   └── SKILL.md              # 同时有 YAML frontmatter
└── openclaw_path_traversal/
    └── SKILL.md              # 包含 {baseDir}/../../etc/passwd 的指令
```

---

## 13. 实现优先级

### 13.1 Phase 1（P0 - 核心兼容）

目标：能加载和运行基本的 OpenClaw skill。

| 任务 | 涉及文件 | 工作量 |
|------|----------|--------|
| 格式检测 `detect_skill_format()` | `skills/src/manager.rs` | 小 |
| OpenClaw frontmatter 解析器 | `skills/src/openclaw_parser.rs`（新建） | 中 |
| `SkillMeta` 扩展 `source` 字段 | `skills/src/manager.rs` | 小 |
| `ActiveSkillContext` 扩展 `source` 字段 | `agent/src/context.rs` | 小 |
| `SkillRequires` 扩展 `any_bins`/`config` | `skills/src/manager.rs` | 小 |
| `{baseDir}` 占位符替换 | `skills/src/openclaw_parser.rs` | 小 |
| 可用性检查扩展（anyBins, config） | `skills/src/manager.rs` | 小 |
| 工具列表按需推断 | `skills/src/manager.rs` | 小 |
| 自进化屏蔽（OpenClaw skill 不进化） | `skills/src/evolution.rs` | 小 |
| 新增依赖 `shellexpand` | `skills/Cargo.toml` | 小 |
| 单元测试（含边界测试） | `skills/src/tests/` | 中 |

预计工作量：3-5 天

### 13.2 Phase 2（P1 - 增强功能）

目标：支持 OpenClaw 的高级特性。

| 任务 | 涉及文件 | 工作量 |
|------|----------|--------|
| OS 过滤 | `skills/src/manager.rs` | 小 |
| `user-invocable` / `disable-model-invocation` | `skills/src/manager.rs`, `agent/src/runtime.rs` | 中 |
| `SLASH_COMMAND_HANDLER` 改用 `RwLock` | `commands/slash_commands/registry.rs` | 中 |
| SkillCommandSpec 斜杠命令动态注册/注销 | `skills/src/manager.rs`, `commands/slash_commands/` | 中 |
| 安装提示生成 | `skills/src/manager.rs` | 小 |
| emoji 显示支持 | `skills/src/manager.rs`, WebUI | 小 |
| OpenClaw 兼容开关（config `openclaw_enabled`） | `skills/src/manager.rs`, `core/src/config.rs` | 小 |
| 集成测试 | `tests/` | 中 |

预计工作量：3-5 天

### 13.3 Phase 3（P2 - 生态集成）

目标：与 OpenClaw 生态更深度集成。

| 任务 | 涉及文件 | 工作量 |
|------|----------|--------|
| OpenClaw Hub API 集成（技能搜索/下载） | `skills/src/hub.rs`（新建） | 大 |
| 多源技能发现（6 层优先级） | `skills/src/manager.rs` | 中 |
| 参数级超时支持 | `tools/src/exec_local.rs` | 小 |
| 后台执行支持 | `tools/src/exec_local.rs` | 中 |
| WebUI 技能市场页面 | `webui/src/` | 大 |

预计工作量：2-3 周

---

## 附录

### A. OpenClaw 关键源文件索引

| 文件 | 用途 |
|------|------|
| `src/agents/skills/types.ts` | Skill 类型定义（SkillMeta, SkillCommandSpec 等） |
| `src/agents/skills/frontmatter.ts` | YAML frontmatter 解析 |
| `src/agents/skills/local-loader.ts` | 安全加载（符号链接检查、大小限制） |
| `src/agents/skills/workspace.ts` | 多源发现与合并 |
| `src/agents/skills/config.ts` | 技能配置与过滤 |
| `src/agents/skills/prompt-builder.ts` | Prompt 注入构建 |
| `src/tools/bash-tools.exec-runtime.ts` | exec 工具实现 |
| `src/tools/canvas-tool.ts` | Canvas 工具 |
| `src/tools/web-tools.ts` | web_search / web_fetch |

### B. BlockCell 关键源文件索引

| 文件 | 用途 |
|------|------|
| `crates/skills/src/manager.rs` | 技能管理器、元数据加载、可用性检查 |
| `crates/skills/src/engine.rs` | Rhai 引擎配置与限制 |
| `crates/skills/src/dispatcher.rs` | Rhai 脚本工具调用分发 |
| `crates/skills/src/evolution.rs` | 自进化类型定义 |
| `crates/skills/src/versioning.rs` | 版本管理 |
| `crates/tools/src/exec_skill_script.rs` | 技能脚本执行桥接 |
| `crates/tools/src/exec_local.rs` | 进程脚本执行 |
| `crates/tools/src/skills.rs` | 技能列表与资产检测 |
| `crates/agent/src/context.rs` | ActiveSkillContext 定义 |
| `crates/agent/src/runtime.rs` | Agent 运行时集成 |

### C. 需新增/修改的文件清单

| 操作 | 文件 | 说明 |
|------|------|------|
| 新建 | `crates/skills/src/openclaw_parser.rs` | OpenClaw frontmatter 解析器 |
| 修改 | `crates/skills/src/manager.rs` | 格式检测、SkillMeta 扩展、可用性检查扩展 |
| 修改 | `crates/skills/src/evolution.rs` | 自进化屏蔽 OpenClaw skill |
| 修改 | `crates/skills/src/lib.rs` | 导出新模块 |
| 修改 | `crates/skills/Cargo.toml` | 新增依赖 `shellexpand = "3"`, `serde_yaml` |
| 修改 | `crates/agent/src/context.rs` | ActiveSkillContext 新增 `source` 字段 |
| 修改 | `crates/agent/src/runtime.rs` | 调用策略控制、source 传播 |
| 修改 | `bin/blockcell/src/commands/slash_commands/registry.rs` | 改用 `RwLock`，支持动态注册/注销 |
| 新建 | `tests/fixtures/openclaw_*/` | 测试数据（7 个目录） |

### D. 术语表

| 术语 | 说明 |
|------|------|
| Frontmatter | Markdown 文件开头的 YAML 元数据块（`---` 包围） |
| 锚点区块 | BlockCell SKILL.md 中用 `{#id}` 标记的 Markdown 区块 |
| exec 工具 | OpenClaw 中执行外部命令的核心工具 |
| exec_skill_script | BlockCell 中执行技能脚本的桥接工具 |
| exec_local | BlockCell 中执行本地命令的工具 |
| Rhai | BlockCell 内嵌的轻量级脚本引擎 |
| SkillCard | BlockCell 中技能的展示卡片（名称+描述+工具列表） |
| ActiveSkillContext | BlockCell 中技能激活后的运行时上下文 |
| `{baseDir}` | OpenClaw 中指向技能目录的占位符 |

---

## 14. 性能优化建议

以下为工程评审中发现的性能优化点，均为低优先级，不阻塞实现：

| 问题 | 影响 | 建议 | 优先级 |
|------|------|------|--------|
| `detect_skill_format` 原实现读取整个文件 | 大型 SKILL.md（接近 256KB）浪费 IO | 已修复：改用 BufReader 只读前 8 字节 | 已完成 |
| `check_openclaw_availability` 串行调用 `which::which` | 多个 bins 时加载时间累积 | 冷路径（启动时一次），暂不优化；如需优化可并行检查 | P3 |
| 无格式检测缓存 | 同一目录被多层扫描时重复读取 | 当前 BlockCell 只有 2 层发现，影响有限；如扩展到 6 层可加 HashMap 缓存 | P3 |

---

## 15. 工程评审报告

> 评审日期: 2026-04-11
> 评审范围: 架构、代码质量、测试覆盖、性能

### 15.1 架构评审

| Issue | 问题 | 决定 | 状态 |
|-------|------|------|------|
| Issue 1 | 格式检测优先级：meta.yaml + frontmatter SKILL.md 共存时误判为 OpenClaw | 先检查 meta.yaml/json，再检查 SKILL.md frontmatter | 已修复 |
| Issue 2 | `SLASH_COMMAND_HANDLER` 是静态不可变实例，无法动态注册 OpenClaw 斜杠命令 | 改用 `Lazy<Arc<RwLock<SlashCommandHandler>>>`，支持动态注册/注销 | 已修复 |
| Issue 3 | OpenClaw 技能需要 `exec_local` 执行外部 CLI 命令 | 默认添加 `exec_local` 到 OpenClaw skill 工具列表（OpenClaw 核心执行模型依赖 exec） | 已修复 |
| Issue 4 | `ActiveSkillContext` 缺少 `source` 字段，运行时无法区分技能来源，自进化屏蔽失效 | 扩展 `ActiveSkillContext` 新增 `source: SkillSource` 字段 | 已修复 |

### 15.2 代码质量评审

| Issue | 问题 | 状态 |
|-------|------|------|
| CQ-1 | `detect_skill_format` 读取整个文件只为检查前 4 字节 | 已修复（改用 BufReader） |
| CQ-2 | `LoadedSkill` 的 `source` 字段与 `meta.source` 重复 | 已修复（移除顶层 source，仅保留 meta.source） |
| CQ-3 | `load_skill_dir` 函数名与现有代码 `load_skill` 不匹配 | 已修复（对齐为 `load_skill`） |
| CQ-4 | `check_availability` 签名与现有代码不匹配 | 已修复（新增独立函数 `check_openclaw_availability`） |
| CQ-5 | 使用 `shellexpand` 但未声明依赖 | 已修复（在文件清单和 Phase 1 任务中标注） |
| CQ-6 | `OpenClawInstallSpec` 反序列化结构体缺失 | 已修复（补充结构体定义和 `map_install_spec` 函数） |

### 15.3 测试覆盖评审

新增 7 个测试用例（原 18 个 → 现 25 个）：

| 新增测试 | 覆盖风险 |
|----------|----------|
| `test_detect_format_priority` | Issue 1 回归 |
| `test_parse_malformed_frontmatter` | 解析器健壮性 |
| `test_basedir_path_traversal` | 安全边界 |
| `test_basedir_windows_backslash` | 跨平台兼容 |
| `test_oversized_skill_md` | 资源限制 |
| `test_source_propagation` | Issue 4 回归 |
| `test_openclaw_command_unregister` | Issue 2 回归 |

新增 4 个测试 fixture 目录（原 3 个 → 现 7 个）。

### 15.4 性能评审

3 个低优先级优化点已记录在第 14 节，不阻塞实现。

### 15.5 评审结论

**状态: DONE**

设计文档经过评审后已修正 4 个架构问题、6 个代码质量问题，补充 7 个缺失测试用例和 4 个测试 fixture。主要改进：

1. 格式检测逻辑更安全（meta.yaml 优先）
2. 工具列表默认包含 `exec_local`（OpenClaw 核心执行模型）
3. 斜杠命令支持动态注册/注销（RwLock）
4. `ActiveSkillContext` 携带 `source` 字段（自进化屏蔽可靠）
5. 伪代码与现有代码对齐（函数名、签名、依赖）
6. 测试覆盖所有评审发现的风险点

文档已准备好进入实现阶段。

---

> 文档结束
