# 意图分类器改进设计文档

> **状态**: 已实现  
> **分支**: `feature/support_openclaw_skill`  
> **创建日期**: 2026-04-12  
> **实现日期**: 2026-04-12

---

## 1. 背景与现状

### 1.1 现有架构

BlockCell 的意图分类系统由两层组成：

```
用户消息
  → IntentClassifier（规则匹配）→ IntentCategory[]
  → decide_interaction()       → InteractionMode（Chat / General / Skill）
  → IntentToolResolver         → 工具列表（注入 system prompt）
```

- **`crates/agent/src/intent.rs`** — 分类器（规则）+ 工具解析器（profile 映射）
- **`crates/agent/src/runtime.rs:3289`** — 每条消息处理时 `IntentClassifier::new()` 实例化
- **`crates/agent/src/context.rs`** — `InteractionMode` 定义

### 1.2 问题清单

| # | 问题 | 严重程度 |
|---|------|---------|
| P1 | 14 个 IntentCategory 中**只有 `Chat` 有匹配规则**，其余 13 个完全空缺 | 严重 |
| P2 | 每条消息都 `IntentClassifier::new()`，重复编译正则，浪费资源 | 中等 |
| P3 | 规则全部硬编码，无法从配置扩展 | 轻微 |
| P4 | 所有非 Chat 消息均落入 `Unknown`，意图路由精准性为零 | 严重 |

**实际效果**：
- "查一下茅台股价" → `Unknown`（期望 `Finance`）
- "帮我读 config.json" → `Unknown`（期望 `FileOps`）
- "发邮件给 xxx"    → `Unknown`（期望 `Communication`）

系统靠 `Unknown` 工具集的"大宽包"来规避规则缺失，失去了精准按需加载工具的设计价值。

---

## 2. 改进目标

1. **补全全部 13 个意图的匹配规则**，使分类器真正有效
2. **消除每次消息的重复初始化**，用 `OnceLock` 做全局单例
3. **新增全套单元测试**，覆盖所有意图的正例与负例
4. **为将来的 LLM 兜底预留扩展点**（本次不实现）

---

## 3. 意图规则设计

### 3.1 各意图的规则策略

每条规则包含：
- `keywords`：出现即匹配的关键词（中英文）
- `patterns`：正则表达式（复杂模式）
- `negative`：否定词（命中则跳过该规则）
- `priority`：优先级（越高越优先）

#### Chat（优先级 10）— 已有，仅补充关键词

当前仅靠正则，补充 `keywords`：
```
你好, hi, hello, hey, 谢谢, ok, 再见, bye
```

#### FileOps（优先级 60）

| 类型 | 示例 |
|------|------|
| keywords | 读文件, 写文件, 创建文件, 删除文件, 列目录, 列出文件, 重命名, 打开文件, 编辑文件 |
| keywords | read file, write file, create file, delete file, list dir, open file, edit file |
| patterns | 文件扩展名 `\.(rs|py|go|js|ts|json|toml|yaml|yml|md|txt|csv|sh|log)\b` |
| patterns | `(?i)(read|write|edit|create|delete|rename|copy|move)\s+(file|directory|folder|dir)` |
| patterns | `(?i)(cat|ls|mkdir|rm|cp|mv|touch|chmod)\s+` |

#### WebSearch（优先级 55）

| 类型 | 示例 |
|------|------|
| keywords | 搜索, 查一下, 查询, 找一找, 查找, 搜一搜, 百度, 谷歌, 网上找 |
| keywords | search, google, bing, look up, find out, browse |
| patterns | `(?i)(what\s+is|how\s+to|where\s+is|when\s+did)` |
| negative | 股价, 行情, price（避免与 Finance 冲突）|

#### Finance（优先级 65）

| 类型 | 示例 |
|------|------|
| keywords | 股价, 行情, 涨跌, K线, 市值, ETF, 基金, 期货, 股票, 买入, 卖出, 仓位, 盈亏, 止损, 市盈率, 分红 |
| keywords | stock, price, trading, portfolio, market, fund, futures, dividend, bull, bear, shares |
| patterns | `(?i)\b(A股|港股|美股|纳斯达克|道琼斯|上证|深证|沪深)\b` |
| patterns | `\d+(\.\d+)?\s*(元|美元|港元|点位)` |

#### Blockchain（优先级 65）

| 类型 | 示例 |
|------|------|
| keywords | 区块链, 链上, 钱包, 合约, NFT, 代币, 挖矿, gas费, 转账, DeFi, DAO, 公链 |
| keywords | blockchain, crypto, bitcoin, ethereum, solana, NFT, wallet, token, DeFi, gas |
| patterns | `0x[0-9a-fA-F]{40}` — 以太坊地址 |
| patterns | `(?i)\b(BTC|ETH|BNB|SOL|USDT|USDC)\b` |

#### DataAnalysis（优先级 60）

| 类型 | 示例 |
|------|------|
| keywords | 数据分析, 图表, 可视化, 统计, 报表, 画图, 折线图, 柱状图, 饼图, 散点图, Excel |
| keywords | analyze, chart, graph, plot, visualize, statistics, report, csv, excel |
| patterns | `(?i)(数据|data)\s*(处理|分析|清洗|转换|导出)` |

#### Communication（优先级 60）

| 类型 | 示例 |
|------|------|
| keywords | 发邮件, 发消息, 发短信, 通知, 提醒我, 群发, 回复消息 |
| keywords | send email, send message, notify, remind, email to, message to |
| patterns | `(?i)(发送|send)\s*(邮件|email|消息|message|通知|notification)` |
| patterns | `[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}` — 邮件地址 |

#### SystemControl（优先级 60）

| 类型 | 示例 |
|------|------|
| keywords | 系统信息, CPU, 内存, 磁盘, 进程, 截图, 相机, 拍照, 打开应用, 关闭应用 |
| keywords | system info, cpu usage, memory, disk space, process, screenshot, camera |
| patterns | `(?i)(打开|关闭|重启|安装|卸载)\s*(应用|软件|程序|app)` |

#### Organization（优先级 55）

| 类型 | 示例 |
|------|------|
| keywords | 定时, 提醒, 日程, 任务, 计划, 待办, cron, 记住, 记录, 备忘 |
| keywords | remind, schedule, task, todo, cron, remember, note, calendar |
| patterns | `(?i)(设置|创建|添加)\s*(提醒|任务|日程|闹钟)` |
| patterns | `\d+\s*(分钟|小时|天|周)\s*(后|内)` — 时间描述 |

#### IoT（优先级 65）

| 类型 | 示例 |
|------|------|
| keywords | IoT, 智能家居, 传感器, 设备控制, MQTT, 温度计, 湿度, 灯光, 开关 |
| keywords | iot, smart home, sensor, mqtt, temperature, humidity, switch, thermostat |
| patterns | `(?i)(打开|关闭|调节)\s*(灯|空调|窗帘|风扇|暖气)` |

#### Media（优先级 60）

| 类型 | 示例 |
|------|------|
| keywords | 语音转文字, 文字转语音, OCR, 识图, 图片理解, 视频处理, 音频, 转写 |
| keywords | transcribe, tts, text to speech, ocr, image recognition, video process, audio |
| patterns | `(?i)(识别|提取|转换)\s*(图片|图像|音频|视频|文字)` |
| patterns | `\.(mp3|mp4|wav|avi|jpg|jpeg|png|gif|pdf)\b` |

#### DevOps（优先级 60）

| 类型 | 示例 |
|------|------|
| keywords | 部署, 运维, 监控, 网络, 端口, HTTP, API, 加密, 解密, 哈希, 证书 |
| keywords | deploy, devops, network, port, http request, api, encrypt, decrypt, hash, certificate |
| patterns | `(?i)(GET|POST|PUT|DELETE|PATCH)\s+https?://` |
| patterns | `https?://[^\s]+` — URL |
| patterns | `(?i)(ping|curl|wget|nmap|ssh)\s+` |

#### Lifestyle（优先级 50）

| 类型 | 示例 |
|------|------|
| keywords | 健康, 运动, 饮食, 卡路里, 跑步, 睡眠, 天气, 菜谱, 旅游, 生活 |
| keywords | health, exercise, diet, calories, sleep, weather, recipe, travel, lifestyle |
| patterns | `(?i)(今天|明天|后天)\s*(天气|气温|下雨)` |

### 3.2 优先级规划

```
IoT / Finance / Blockchain  65   — 高度专业，词汇明确
FileOps / DataAnalysis / Communication / SystemControl / Media / DevOps  60
WebSearch / Organization    55
Lifestyle                   50
Chat                        10   — 最后兜底
```

高优先级意图的词汇越具体，越不容易误匹配。

---

## 4. IntentClassifier 单例化

### 4.1 当前问题

```rust
// runtime.rs:3289 — 每条消息都重建，重复编译正则
let classifier = crate::intent::IntentClassifier::new();
```

正则编译是一次性开销，不应每条消息都执行。

### 4.2 改进方案

使用标准库 `std::sync::OnceLock` 实现全局单例：

```rust
// intent.rs
use std::sync::OnceLock;

static GLOBAL_CLASSIFIER: OnceLock<IntentClassifier> = OnceLock::new();

impl IntentClassifier {
    pub fn global() -> &'static IntentClassifier {
        GLOBAL_CLASSIFIER.get_or_init(Self::new)
    }
}
```

```rust
// runtime.rs:3289 — 改为
let classifier = crate::intent::IntentClassifier::global();
```

---

## 5. 测试设计

每个意图至少覆盖：
- **3 个正例**（应该匹配该意图）
- **2 个负例**（不应该匹配，检验不误匹配）
- **1 个边界用例**（中英混合或模糊表述）

关键测试用例示例：

```rust
// Finance
assert_eq!(classify("查一下茅台股价"),  [Finance]);
assert_eq!(classify("BTC今天涨了多少"), [Finance]);
// FileOps
assert_eq!(classify("帮我读一下 config.json5"), [FileOps]);
assert_eq!(classify("列出当前目录的文件"),      [FileOps]);
// 负例：Chat 不应触发 FileOps
assert_ne!(classify("谢谢"),  [FileOps]);
```

---

## 6. 实现总结

### 变更文件

| 文件 | 变更内容 |
|------|---------|
| `crates/agent/src/intent.rs` | 补全 13 个意图规则；添加 `OnceLock` 单例；新增 13 个测试用例；更新旧测试 |
| `crates/agent/src/runtime.rs` | 将 `IntentClassifier::new()` 改为 `IntentClassifier::global()` |

### 实现的关键改动

1. **补全了所有 13 个意图的规则**（Finance / Blockchain / FileOps / WebSearch / DataAnalysis / Communication / SystemControl / Organization / IoT / Media / DevOps / Lifestyle）
2. **OnceLock 全局单例**：`IntentClassifier::global()` 只在首次调用时初始化，后续复用
3. **Runtime 使用单例**：消除了每条消息的正则重复编译
4. **全套测试覆盖**：每个意图有 2-3 个正例和至少 1 个负例，并有单例指针一致性测试

---

## 7. 配置文件自定义意图规则

### 7.1 设计目标

内置规则覆盖通用场景，用户可通过 `config.json5` 叠加自定义规则，无需修改代码。两者是**互补关系**：

- 内置规则（代码中）：全局通用，优先级固定
- 配置规则（config.json5 中）：用户私有扩展，按需追加

### 7.2 新增数据结构

**`crates/core/src/config.rs`** 中新增：

```rust
pub struct IntentRuleConfig {
    pub category: String,      // 意图类别名（如 "Finance"）
    pub keywords: Vec<String>, // 关键词（大小写不敏感）
    pub patterns: Vec<String>, // 正则表达式
    pub negative: Vec<String>, // 否定词
    pub priority: u8,          // 默认 60
}
```

`IntentRouterConfig` 新增字段：

```rust
pub intent_rules: Vec<IntentRuleConfig>,
```

### 7.3 内部实现

`IntentRule` 结构添加动态字段（避免改动所有内置规则）：

```rust
struct IntentRule {
    keywords: Vec<&'static str>,  // 内置
    keywords_dyn: Vec<String>,    // 配置文件
    negative: Vec<&'static str>,  // 内置
    negative_dyn: Vec<String>,    // 配置文件
    // ...
}
```

`rule_matches` 同时检查静态 + 动态两组关键词。

新增 `IntentClassifier::with_extra_rules(extra: &[IntentRuleConfig]) -> Self`：

- 先调用 `Self::new()` 初始化所有内置规则
- 再将配置规则编译追加进去（正则无效时 warn 跳过）

### 7.4 Runtime 调度逻辑

```text
config.intent_rules 为空？
  → 是：使用 IntentClassifier::global()（零开销，静态单例）
  → 否：使用 IntentClassifier::with_extra_rules(...)（含配置规则的实例）
```

### 7.5 用户配置示例

```json5
{
  "intentRouter": {
    "intentRules": [
      {
        "category": "Finance",
        "keywords": ["我的持仓", "组合收益", "期权策略"],
        "priority": 70
      },
      {
        "category": "IoT",
        "keywords": ["热水器", "扫地机器人"],
        "patterns": ["(?i)打开.*(热水|扫地)"],
        "priority": 65
      },
      {
        "category": "WebSearch",
        "keywords": ["搜索"],
        // 搜索金融相关内容时，应排除 WebSearch 路由
        "negative": ["股价", "行情", "stock price"],
        "priority": 55
      }
    ]
  }
}
```

## 8. 不在本次范围内

- LLM 语义兜底分类（规则匹配仍返回 Unknown 时调用小模型）
- 多意图同优先级冲突解决策略优化

这些作为后续 issue 追踪。
