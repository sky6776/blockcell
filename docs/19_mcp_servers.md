# 第19篇：MCP Server 集成 —— blockcell 的独立 MCP 子系统

> 系列文章：《blockcell 开源项目深度解析》第 19 篇

## MCP 是什么

**MCP（Model Context Protocol）** 是让 AI 助手按统一协议发现并调用外部工具/数据源的标准接口。

在 blockcell 里，MCP 适合承载：

- GitHub / GitLab 等平台集成
- SQLite / PostgreSQL / MySQL 等数据库访问
- Filesystem、Puppeteer 等外部工具集
- 任何遵循 MCP 协议的自定义 server

## 当前架构

现在的 blockcell 不再把 MCP 配置塞进 `config.json5` 的 `mcpServers` 字段，而是改成**独立 MCP 配置层**：

- `~/.blockcell/mcp.json`：全局 MCP 元配置
- `~/.blockcell/mcp.d/*.json`：按 server 拆分的独立文件

同时，MCP 与多 agent 的关系也更清晰：

- **MCP 独立**：server 定义属于基础设施层
- **agent 绑定权限视图**：agent 只声明允许访问哪些 MCP servers/tools
- **运行时共享**：同一进程内，MCP server 由共享管理器统一启动与复用

## 快速开始

### 方式一：CLI 快捷添加（推荐）

```bash
# 添加 GitHub MCP
blockcell mcp add github

# 添加 SQLite MCP
blockcell mcp add sqlite --db-path /tmp/test.db

# 查看当前 MCP 配置
blockcell mcp list
```

### 方式二：直接编辑文件

例如创建 `~/.blockcell/mcp.d/github.json`：

```json
{
  "name": "github",
  "command": "npx",
  "args": ["-y", "@modelcontextprotocol/server-github"],
  "env": {
    "GITHUB_PERSONAL_ACCESS_TOKEN": "${env:GITHUB_PERSONAL_ACCESS_TOKEN}"
  },
  "enabled": true,
  "autoStart": true
}
```

或者创建 `~/.blockcell/mcp.d/sqlite.json`：

```json
{
  "name": "sqlite",
  "command": "uvx",
  "args": ["mcp-server-sqlite", "--db-path", "/tmp/test.db"],
  "enabled": true,
  "autoStart": true
}
```

修改后重启：

```bash
blockcell agent
# 或
blockcell gateway
```

## 配置字段

每个 `mcp.d/<name>.json` 支持如下字段：

| 字段 | 类型 | 说明 |
|------|------|------|
| `name` | string | server 逻辑名称，也是工具名前缀 |
| `command` | string | 启动命令，如 `npx`、`uvx` |
| `args` | array | 启动参数 |
| `env` | object | 额外环境变量 |
| `cwd` | string/null | 工作目录 |
| `enabled` | bool | 是否启用 |
| `autoStart` | bool | 启动 blockcell 时是否自动启动 |
| `startupTimeoutSecs` | integer | 启动/握手超时 |
| `callTimeoutSecs` | integer | 工具调用超时 |

## 工具命名规则

MCP 工具在 blockcell 内统一命名为：

```text
<serverName>__<toolName>
```

例如：

- `github__list_issues`
- `sqlite__query`
- `filesystem__read_file`

## 与多 agent 的关系

这是这次重构里最重要的边界：

- **MCP 不是 agent 自有配置**
- **agent 只是绑定 MCP 权限视图**

这意味着：

- MCP server 定义是全局的
- agent 通过 `allowedMcpServers` / `allowedMcpTools` 控制可见性
- subagent 默认不自动继承 MCP

## CLI 管理命令

```bash
blockcell mcp list
blockcell mcp show github
blockcell mcp add github
blockcell mcp add sqlite --db-path /tmp/app.db
blockcell mcp add custom --raw --name custom --command uvx --arg my-mcp-server
blockcell mcp enable github
blockcell mcp disable github
blockcell mcp remove github
blockcell mcp edit github
```

## 工作原理

blockcell 内部通过共享 `McpManager` 管理 MCP：

1. 读取 `mcp.json` 与 `mcp.d/*.json`
2. 合并为运行时 `McpResolvedConfig`
3. 自动启动 `enabled && autoStart` 的 server
4. 获取 `tools/list`
5. 根据 agent 的 MCP 权限视图，将可见工具注入该 agent 的 `ToolRegistry`
6. 真正执行时，通过 `tools/call` 转发给目标 MCP server

## 故障排查

### 1. `blockcell mcp list` 看不到新 server

确认文件是否写到了：

- `~/.blockcell/mcp.json`
- `~/.blockcell/mcp.d/<name>.json`

并检查 JSON 是否合法。

### 2. 配置已写入但工具列表里没有 MCP 工具

MCP 变更默认需要重启后生效：

```bash
blockcell agent
# 或
blockcell gateway
```

### 3. server 启动失败

先单独手工验证命令：

```bash
uvx mcp-server-sqlite --db-path /tmp/test.db
npx -y @modelcontextprotocol/server-github
```

### 4. agent 看不到某个 MCP 工具

检查该 agent 的：

- `allowedMcpServers`
- `allowedMcpTools`

若未授权，工具不会进入该 agent 的可见 registry。

## 建议

- 高频、底层、平台无关能力继续做成内置工具
- 第三方平台/数据库/专业系统接入优先走 MCP
- 新手优先用 `blockcell mcp add <template>`
- 复杂场景再直接编辑 `mcp.d/*.json`
