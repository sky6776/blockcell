# 日志系统改进设计文档

> 将日志从控制台输出改为文件输出，并添加 `/log` 斜杠命令进行动态控制

## 概述

### 问题

当前 BlockCell 日志系统仅在控制台输出，存在以下问题：
- 无法动态调整日志等级
- 无法按模块过滤日志

### 目标

1. 通过 `/log` 斜杠命令动态控制日志等级、模块过滤、控制台开关、文件开关
2. 支持配置文件持久化日志参数
3. 日志命令修改后自动同步到配置文件

---

## 架构

```text
┌─────────────────────────────────────────────────────────────┐
│                     tracing_subscriber::registry()           │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────┐  ┌───────────────┐  ┌──────────────────┐   │
│  │ EnvFilter   │  │ RollingFile   │  │ fmt::Layer       │   │
│  │ (reloadable)│  │ Layer         │  │ (stdout)         │   │
│  └─────────────┘  └───────────────┘  └──────────────────┘   │
│        │                 │                    │              │
│        v                 v                    v              │
│   等级过滤         ~/.blockcell/          stdout             │
│   模块过滤         workspace/logs/        (默认开启)         │
│                     agent.log.YYYY-MM-DD                     │
│                     (默认关闭)                                │
└─────────────────────────────────────────────────────────────┘
                              │
                              v
                    ┌─────────────────┐
                    │ LogController   │  ← 全局控制句柄
                    │ - set_level()   │
                    │ - set_console() │
                    │ - set_file()    │
                    │ - status()      │
                    └─────────────────┘
                              │
                              v
                    ┌─────────────────┐
                    │ config.json5    │  ← 配置持久化
                    │ log.level       │
                    │ log.consoleEnabled│
                    │ log.fileEnabled  │
                    └─────────────────┘
                              │
                              v
                    ┌─────────────────┐
                    │ /log 斜杠命令    │
                    │ /log debug      │
                    │ /log file on    │
                    │ /log console off│
                    │ (自动同步配置)   │
                    └─────────────────┘
```

---

## 默认值设置

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `log.level` | `info` | 日志等级 |
| `log.consoleEnabled` | `true` | 控制台输出默认开启 |
| `log.fileEnabled` | `false` | 文件输出默认关闭 |

**设计理由**：
- 控制台输出默认开启，方便开发调试和即时查看
- 文件输出默认关闭，避免不必要的磁盘写入
- 用户可通过 `/log file on` 或配置文件开启文件输出

---

## 核心组件

### 1. LogController（全局控制句柄）

**位置**: `crates/core/src/logging.rs`

```rust
use std::sync::{Arc, Mutex, OnceLock};
use tracing_subscriber::{reload, Registry, EnvFilter};

pub struct LogController {
    /// EnvFilter reload handle for dynamic level adjustment
    filter_handle: reload::Handle<EnvFilter, Registry>,
    /// Console output switch (default: true, independent from file)
    console_enabled: Arc<Mutex<bool>>,
    /// File output switch (default: false, independent from console)
    file_enabled: Arc<Mutex<bool>>,
    /// Current log file path
    current_file: Arc<Mutex<String>>,
}

impl LogController {
    /// Set global log level (trace/debug/info/warn/error/off)
    pub fn set_level(&self, level: &str) -> Result<(), String> { ... }
    
    /// Set module-specific filter (e.g., "blockcell_agent=trace")
    pub fn set_filter(&self, filter: &str) -> Result<(), String> { ... }
    
    /// Toggle console output (independent, default: on)
    pub fn set_console(&self, enabled: bool) { ... }
    
    /// Toggle file output (independent, default: off)
    pub fn set_file(&self, enabled: bool) { ... }
    
    /// Get current status
    pub fn status(&self) -> LogStatus { ... }
}

pub struct LogStatus {
    pub level: String,
    pub module_filters: Vec<String>,
    pub console_enabled: bool,  // 默认 true
    pub file_enabled: bool,     // 默认 false
    pub log_file: String,
}

/// Global singleton
pub static LOG_CONTROLLER: OnceLock<LogController> = OnceLock::new();

/// Initialize logging system
/// - level: log level from config
/// - console_enabled: whether to output to console (default: true)
/// - file_enabled: whether to output to file (default: false)
pub fn init_logging(
    logs_dir: &Path,
    level: &str,
    console_enabled: bool,
    file_enabled: bool,
) -> Result<(), String> { ... }
```

---

### 2. 配置文件结构

**位置**: `crates/core/src/config.rs`

```rust
/// 日志配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogConfig {
    /// 日志等级: trace, debug, info, warn, error, off。默认: info
    #[serde(default = "default_log_level")]
    pub level: String,
    /// 是否输出到文件。默认: false
    #[serde(default)]
    pub file_enabled: bool,
    /// 是否输出到控制台。默认: true
    #[serde(default = "default_true")]
    pub console_enabled: bool,
}
```

**配置文件示例** (`~/.blockcell/config.json5`):

```json
{
  "log": {
    "level": "info",
    "fileEnabled": false,
    "consoleEnabled": true
  }
}
```

---

### 3. 日志文件格式与匹配

**文件命名**: `tracing_appender::RollingFileAppender` 使用 `Rotation::DAILY` 创建：

```text
~/.blockcell/workspace/logs/
├── agent.log              ← 当前日志文件（当日）
├── agent.log.2026-04-16   ← 按日期滚动的日志
├── agent.log.2026-04-15
└── agent.log.2026-04-14
```

**文件匹配逻辑**:

```rust
/// Check if file is a log file (agent.log or agent.log.YYYY-MM-DD)
fn is_log_file(path: &std::path::Path) -> bool {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    file_name == "agent.log"
        || file_name.starts_with("agent.log.")
}
```

**重要**: 不能使用 `.log` 扩展名检查，因为 `agent.log.2026-04-16` 的扩展名是日期，不是 `log`。

---

### 4. 清理与清空逻辑

**位置**: `crates/core/src/logging.rs`

```rust
/// 清理所有日志文件，返回 (成功删除数, 删除的总大小)
pub fn clear_all_logs(logs_dir: &Path) -> (usize, u64) {
    // ...
    for entry in entries.flatten() {
        let path = entry.path();
        if is_log_file(&path) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            // 尝试删除
            if std::fs::remove_file(&path).is_ok() {
                count += 1;
                total_size += size;
            } else {
                // 如果删除失败（可能文件正在被写入），尝试清空内容
                if std::fs::write(&path, "").is_ok() {
                    count += 1;
                    total_size += size;
                }
            }
        }
    }
    (count, total_size)
}
```

**Windows 兼容**: 正在被写入的日志文件无法删除，此时清空文件内容作为备选方案。

---

## `/log` 斜杠命令

### 命令语法

```text
/log help              - 显示帮助信息
/log status            - 显示当前配置（包括文件统计）
/log trace             - 设置 TRACE 等级（最详细）
/log debug             - 设置 DEBUG 等级
/log info              - 设置 INFO 等级  
/log warn              - 设置 WARN 等级
/log error             - 设置 ERROR 等级
/log off               - 关闭所有日志
/log clear             - 清理所有日志文件
/log <模块>=<等级>      - 设置模块级别，如 /log blockcell_agent=trace
/log console on        - 开启控制台输出（默认）
/log console off       - 关闭控制台输出
/log file on           - 开启文件输出
/log file off          - 关闭文件输出（默认）
```

### 配置同步

所有 `/log` 命令执行后会自动同步更新配置文件：

```rust
/// 同步日志配置到配置文件
fn sync_config_to_file(level: Option<&str>, console_enabled: Option<bool>, file_enabled: Option<bool>) -> Result<(), String> {
    let paths = Paths::default();
    let config_path = paths.config_file();

    let mut config = Config::load(&config_path)?;
    
    if let Some(level) = level {
        config.log.level = level.to_string();
    }
    if let Some(console) = console_enabled {
        config.log.console_enabled = console;
    }
    if let Some(file) = file_enabled {
        config.log.file_enabled = file;
    }

    config.save(&config_path)?;
    Ok(())
}
```

---

## CLI 命令：blockcell logs

**位置**: `bin/blockcell/src/commands/logs_cmd.rs`

```text
blockcell logs [N]            - 显示最近 N 行日志（默认 50）
blockcell logs --follow       - 实时跟踪日志（tail -f）
blockcell logs --filter=关键词 - 过滤包含关键词的日志
blockcell logs --clear        - 清理所有日志文件
```

---

## 文件修改清单

| 文件 | 操作 | 说明 |
|------|------|------|
| `crates/core/src/logging.rs` | 编辑 | LogController + SwitchableConsoleLayer + clear_all_logs |
| `crates/core/src/config.rs` | 编辑 | 添加 LogConfig 结构体 |
| `crates/core/src/paths.rs` | 编辑 | logs_dir() 返回 workspace/logs |
| `bin/blockcell/src/main.rs` | 编辑 | 使用配置参数初始化日志系统 |
| `bin/blockcell/src/commands/slash_commands/handlers/log.rs` | 新建 | /log 命令处理器 + 配置同步 |
| `bin/blockcell/src/commands/logs_cmd.rs` | 编辑 | 统一使用 is_log_file 函数 |

---

## 启动流程

**main.rs**:

```rust
// 加载配置
let config = blockcell_core::Config::load_or_default(&paths)?;

// 清理旧日志（保留 3 天）
logging::cleanup_old_logs(&logs_dir, 3);

// 使用配置中的日志参数初始化日志系统
logging::init_logging(
    &logs_dir,
    &config.log.level,        // 默认 "info"
    config.log.console_enabled, // 默认 true
    config.log.file_enabled,    // 默认 false
)?;
```

---

## 配置文件更新机制

当配置文件中缺少 log 相关字段时，启动时自动添加：

```rust
// 检查并添加缺失的 log 配置
if !raw.contains("consoleEnabled") {
    tracing::info!("Adding missing consoleEnabled field to log config (default: true)");
    needs_save = true;
}
if !raw.contains("fileEnabled") {
    tracing::info!("Adding missing fileEnabled field to log config (default: false)");
    needs_save = true;
}
if !raw.contains("level") {
    tracing::info!("Adding missing level field to log config (default: info)");
    needs_save = true;
}
```

---

## 测试计划

1. **单元测试**
   - LogController::set_level 正确性
   - is_log_file 文件名匹配逻辑
   - clear_all_logs 删除和清空逻辑

2. **集成测试**
   - `/log status` 输出格式和文件统计
   - `/log debug` 等级切换 + 配置同步
   - `/log console on/off` 控制台开关 + 配置同步
   - `/log file on/off` 文件开关 + 配置同步
   - `/log clear` 清空日志文件

3. **手动验证**
   - 启动 agent 模式，确认控制台日志可见（默认开启）
   - `/log file on` 后检查日志文件生成
   - 检查日志文件 `agent.log.YYYY-MM-DD` 格式
   - `/log console off` 后控制台无日志输出
   - 配置文件中验证 log 参数已更新

---

## 风险与缓解

| 风险 | 缓解措施 |
|------|---------|
| RollingFileAppender 文件名格式 | 使用 `is_log_file` 函数匹配 `agent.log.*` 格式 |
| Windows 文件占用无法删除 | clear_all_logs 清空内容作为备选 |
| 配置文件字段缺失 | 启动时自动添加默认值 |
| 配置修改未持久化 | /log 命令后自动同步到配置文件 |

---

## 实现状态

✅ 已完成：
- LogController 动态控制日志等级、控制台、文件
- 配置文件 LogConfig 结构体
- `/log` 斜杠命令 + 配置同步
- `blockcell logs` CLI 命令
- 日志文件匹配逻辑（agent.log.YYYY-MM-DD）
- 日志清空逻辑（删除失败时清空内容）

📋 后续扩展：
- WebUI 日志查看界面
- 按会话过滤日志（session ID 标记）