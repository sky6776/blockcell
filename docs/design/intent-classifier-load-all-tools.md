# 意图分类器全量加载工具设计文档

## 背景

BlockCell 使用意图分类器（Intent Classifier）来决定加载哪些工具。当前设计：
- 用户消息 → IntentClassifier.classify() → IntentCategory[]
- 意图类别 → IntentToolResolver.resolve_tool_names() → 允许的工具列表

但在分析 Claude Code 和 OpenClaw 的实现后，发现两者都**不使用意图分类器**：
- Claude Code：通过白名单/黑名单 + 权限规则控制工具可用性
- OpenClaw：通过多层策略管道过滤工具

本文档设计一种可选方案：让用户可以选择是否使用意图分类器。

## Claude Code / OpenClaw 对比分析

### Claude Code 工具选择机制

| 机制 | 说明 |
|------|------|
| 无意图分类 | 把所有工具给 LLM，让 LLM 自己决定 |
| 安全分类器 | Auto Mode 下用 LLM 评估工具调用的安全性 |
| 权限规则系统 | 用户配置 `alwaysAllow/alwaysDeny` 规则 |

### OpenClaw 工具选择机制

| 机制 | 说明 |
|------|------|
| 无意图分类 | 依赖 LLM 自身理解用户意图 |
| 多层工具过滤 | profile → global → agent → group 策略管道 |
| Tool Profiles | minimal/coding/messaging/full 预定义工具集 |

### 共性

- **无意图分类**：让 LLM 自己判断，通过权限控制安全边界
- **工具全量提供**：LLM 可看到所有允许的工具定义
- **Prompt Caching**：现代 LLM 支持缓存，后续请求几乎零开销

## Token 和延时分析

### Token 消耗估算

假设 BlockCell 有 **50 个工具**：

| 组成部分 | 每工具估算 | 50工具总计 |
|---------|-----------|-----------|
| name | ~10 tokens | ~500 |
| description | ~30-50 tokens | ~1,500-2,500 |
| parameters schema | ~50-100 tokens | ~2,500-5,000 |
| **总计** | ~90-160 tokens | **~4,500-8,000 tokens** |

### 与意图分类器的对比

| 方案 | 工具定义 tokens | 总 tokens |
|------|----------------|-----------|
| **意图分类后加载** | ~1,500-2,500 (10-15工具) | **~1,500-2,500** |
| **全量加载** | ~4,500-8,000 (50工具) | **~4,500-8,000** |

**差异**: 全量加载多消耗约 **3,000-5,500 tokens**

### Prompt Caching 的关键作用

现代 LLM (如 Claude) 支持 **Prompt Caching**：

| 场景 | 无缓存 | 有缓存 |
|------|--------|--------|
| **首次请求** | 8,000 tokens + ~300ms 延时 | 同上 |
| **后续请求** | 8,000 tokens + ~300ms 延时 | ~800 tokens (仅新消息) + ~50ms 延时 |

**结论**: 如果工具定义被缓存，后续交互几乎零额外开销！

## 配置项设计

### 新增配置项

在 `IntentRouterConfig` 新增 `load_all_tools` 字段：

```json5
{
  "intentRouter": {
    "enabled": true,            // 是否启用意图分类（默认 true）
    "loadAllTools": false,      // 当 enabled=false 时，是否全量加载（默认 false）
    "defaultProfile": "default",
    "profiles": { ... }
  }
}
```

### 行为矩阵

| enabled | loadAllTools | 行为 |
|---------|--------------|------|
| true | * | 意图分类 + 按意图加载工具（当前默认） |
| false | false | 不分类 + 走 Unknown profile |
| false | true | 不分类 + 全量加载所有工具 |

### 配置文件自动更新

当配置文件缺少 `loadAllTools` 字段时，自动添加并置为 `false`：

```rust
// 在 Config::load_or_default 中检测并更新
if raw.contains("intentRouter") && !raw.contains("loadAllTools") {
    tracing::info!("Adding missing loadAllTools field to intentRouter config");
    needs_save = true;
}
```

## 实现细节

### 1. IntentRouterConfig 结构体修改

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentRouterConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 当 enabled=false 时，是否全量加载所有可用工具。
    #[serde(default)]
    pub load_all_tools: bool,
    // ... 其他字段不变
}
```

### 2. resolve_tool_names 修改

```rust
pub fn resolve_tool_names(...) -> Option<Vec<String>> {
    let router = ...;

    // 1. 先判断 enabled
    if !router.enabled {
        // 2. enabled=false 时，判断 load_all_tools
        if router.load_all_tools {
            // 全量加载：返回所有可用工具（扣除 deny_tools）
            if let Some(available) = available_tools {
                let mut result: Vec<String> = available.iter().cloned().collect();
                // 应用 deny_tools 过滤
                ...
                return Some(result);
            }
        }
        // load_all_tools=false: 走 Unknown profile
    }

    // enabled=true: 意图分类流程（原有逻辑）
    ...
}
```

### 3. resolve_effective_tool_names 修改

```rust
fn resolve_effective_tool_names(...) -> Vec<String> {
    // 1. 先检查 intent_router.enabled
    let router_enabled = config.intent_router
        .as_ref()
        .map(|r| r.enabled)
        .unwrap_or(true);

    if !router_enabled {
        // 2. enabled=false 时，检查 load_all_tools
        let load_all = config.intent_router
            .as_ref()
            .map(|r| r.load_all_tools)
            .unwrap_or(false);

        if load_all {
            // 全量加载模式：返回所有可用工具
            let mut tool_names: Vec<String> = available_tools.iter().cloned().collect();
            // 应用 napcat 过滤
            ...
            return tool_names;
        }
    }

    // enabled=true 或 load_all_tools=false: 原有意图分类逻辑
    ...
}
```

## 使用示例

### 场景 1：保持默认（意图分类）

```json5
{
  "intentRouter": {
    "enabled": true,
    "loadAllTools": false
  }
}
```

行为：意图分类 + 按意图加载工具（节省 tokens）

### 场景 2：全量加载所有工具

```json5
{
  "intentRouter": {
    "enabled": false,
    "loadAllTools": true
  }
}
```

行为：不分类 + 全量加载所有工具（让 LLM 自己选择）

### 场景 3：走 Unknown profile

```json5
{
  "intentRouter": {
    "enabled": false,
    "loadAllTools": false
  }
}
```

行为：不分类 + 走 Unknown profile（由配置决定工具）

## 修改文件列表

| 文件 | 修改内容 |
|------|----------|
| `crates/core/src/config.rs` | 新增 `load_all_tools` 字段 + 配置文件自动更新 |
| `crates/agent/src/intent.rs` | 修改 `resolve_tool_names` 支持 loadAllTools |
| `crates/agent/src/runtime.rs` | 修改 `resolve_effective_tool_names` 支持 loadAllTools |

## 验证方法

### 单元测试

```rust
#[test]
fn test_load_all_tools_returns_all_tools() {
    let raw = r#"{"intentRouter": {"enabled": false, "loadAllTools": true}}"#;
    let config: Config = serde_json::from_str(raw).unwrap();
    let resolver = IntentToolResolver::new(&config);
    let available: HashSet<String> = ["read_file", "write_file", "exec"].iter().cloned().collect();
    let tools = resolver.resolve_tool_names(None, &[IntentCategory::Unknown], Some(&available));
    assert_eq!(tools.unwrap().len(), 3);
}

#[test]
fn test_load_all_tools_false_follows_unknown_profile() {
    // 验证 loadAllTools=false 时走 Unknown profile
}
```

### 手动验证

```bash
# 修改 ~/.blockcell/config.json5
# 设置 "enabled": false, "loadAllTools": true

# 运行 agent
cargo run -p blockcell -- agent

# 发送消息，观察日志中工具数量
RUST_LOG=debug cargo run -p blockcell -- agent
```

## 结论

通过新增 `loadAllTools` 配置项，用户可以灵活选择：

1. **意图分类模式**（默认）：节省 tokens，适合低成本场景
2. **全量加载模式**：让 LLM 自己选择，适合支持 Prompt Caching 的场景
3. **Unknown profile 模式**：由配置决定工具，适合精细化控制

这种设计兼顾了灵活性、兼容性和易用性。