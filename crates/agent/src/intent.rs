use blockcell_core::mcp_config::McpResolvedConfig;
use blockcell_core::{Config, Error, Result};
use blockcell_tools::ToolRegistry;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Intent categories for user messages.
/// Used to determine which tools, rules, and domain knowledge to load.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IntentCategory {
    /// 日常闲聊、问候、闲谈 — 不需要任何工具
    Chat,
    /// 文件/代码操作 — read_file, write_file, edit_file, list_dir, exec, file_ops
    FileOps,
    /// 网页/搜索 — web_search, web_fetch, browse
    WebSearch,
    /// 金融/行情/告警 — alert_rule, stream_subscribe, ...
    Finance,
    /// 区块链/链上资产相关请求
    Blockchain,
    /// 数据处理/可视化 — data_process, chart_generate, office_write
    DataAnalysis,
    /// 通信/邮件/消息 — email, message
    Communication,
    /// 系统/硬件/应用控制/Android — system_info, app_control, camera_capture, termux_api
    SystemControl,
    /// 日程/任务/记忆 — cron, memory_*, knowledge_graph, list_tasks
    Organization,
    /// IoT/设备控制类请求
    IoT,
    /// 媒体处理 — audio_transcribe, tts, ocr, image_understand, video_process
    Media,
    /// 开发/运维 — network_monitor, encrypt
    DevOps,
    /// 健康/生活类请求
    Lifestyle,
    /// 无法判断 — 加载核心工具集
    Unknown,
}

impl IntentCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            IntentCategory::Chat => "Chat",
            IntentCategory::FileOps => "FileOps",
            IntentCategory::WebSearch => "WebSearch",
            IntentCategory::Finance => "Finance",
            IntentCategory::Blockchain => "Blockchain",
            IntentCategory::DataAnalysis => "DataAnalysis",
            IntentCategory::Communication => "Communication",
            IntentCategory::SystemControl => "SystemControl",
            IntentCategory::Organization => "Organization",
            IntentCategory::IoT => "IoT",
            IntentCategory::Media => "Media",
            IntentCategory::DevOps => "DevOps",
            IntentCategory::Lifestyle => "Lifestyle",
            IntentCategory::Unknown => "Unknown",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim() {
            "Chat" => Some(IntentCategory::Chat),
            "FileOps" => Some(IntentCategory::FileOps),
            "WebSearch" => Some(IntentCategory::WebSearch),
            "Finance" => Some(IntentCategory::Finance),
            "Blockchain" => Some(IntentCategory::Blockchain),
            "DataAnalysis" => Some(IntentCategory::DataAnalysis),
            "Communication" => Some(IntentCategory::Communication),
            "SystemControl" => Some(IntentCategory::SystemControl),
            "Organization" => Some(IntentCategory::Organization),
            "IoT" => Some(IntentCategory::IoT),
            "Media" => Some(IntentCategory::Media),
            "DevOps" => Some(IntentCategory::DevOps),
            "Lifestyle" => Some(IntentCategory::Lifestyle),
            "Unknown" => Some(IntentCategory::Unknown),
            _ => None,
        }
    }
}

struct IntentRule {
    category: IntentCategory,
    /// 内置规则的关键词（静态字符串）
    keywords: Vec<&'static str>,
    /// 来自配置文件的动态关键词
    keywords_dyn: Vec<String>,
    patterns: Vec<Regex>,
    /// 内置规则的否定词（静态字符串）
    negative: Vec<&'static str>,
    /// 来自配置文件的动态否定词
    negative_dyn: Vec<String>,
    priority: u8,
}

impl Default for IntentRule {
    fn default() -> Self {
        Self {
            category: IntentCategory::Unknown,
            keywords: vec![],
            keywords_dyn: vec![],
            patterns: vec![],
            negative: vec![],
            negative_dyn: vec![],
            priority: 0,
        }
    }
}

pub struct IntentClassifier {
    rules: Vec<IntentRule>,
}

impl Default for IntentClassifier {
    fn default() -> Self {
        Self::new()
    }
}

static GLOBAL_CLASSIFIER: OnceLock<IntentClassifier> = OnceLock::new();

impl IntentClassifier {
    /// 返回全局单例，避免每条消息重复编译正则。
    pub fn global() -> &'static IntentClassifier {
        GLOBAL_CLASSIFIER.get_or_init(Self::new)
    }

    pub fn new() -> Self {
        let rules = vec![
            // ── Chat (highest priority) ──
            IntentRule {
                category: IntentCategory::Chat,
                keywords: vec![],
                patterns: vec![
                    Regex::new(r"(?i)^(你好|hi|hello|hey|嗨|早安|晚安|早上好|下午好|晚上好|good\s*(morning|afternoon|evening))[\s!！。.？?~～]*$").unwrap(),
                    Regex::new(r"(?i)^(谢谢|感谢|辛苦了|好的|明白了|知道了|ok|okay|got\s*it|thanks|thank\s*you)[\s!！。.？?~～]*$").unwrap(),
                    Regex::new(r"(?i)^(再见|拜拜|bye|goodbye|see\s*you)[\s!！。.？?~～]*$").unwrap(),
                    Regex::new(r"(?i)^(你是谁|who\s*are\s*you|你能做什么|what\s*can\s*you\s*do|帮助|help)[\s？?]*$").unwrap(),
                    Regex::new(r"(?i)^(哈哈|嘿嘿|呵呵|lol|haha|😂|👍|🙏|❤️|😊)[\s!！。.？?~～]*$").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 10,
            },
            // ── Finance (priority 65) ──
            IntentRule {
                category: IntentCategory::Finance,
                keywords: vec![
                    "股价", "行情", "涨跌", "k线", "市值", "etf", "基金", "期货",
                    "股票", "买入", "卖出", "仓位", "盈亏", "止损", "市盈率", "分红",
                    "stock", "trading", "portfolio", "market cap", "fund", "futures",
                    "dividend", "bull market", "bear market", "shares",
                    "a股", "港股", "美股", "纳斯达克", "道琼斯", "上证", "深证", "沪深",
                ],
                patterns: vec![
                    Regex::new(r"(?i)\b(stock\s*price|market\s*cap|p/e\s*ratio|pe\s*ratio)\b").unwrap(),
                    Regex::new(r"\d+(\.\d+)?\s*(元|美元|港元|点位)").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 65,
            },
            // ── Blockchain (priority 65) ──
            IntentRule {
                category: IntentCategory::Blockchain,
                keywords: vec![
                    "区块链", "链上", "钱包", "合约", "nft", "代币", "挖矿", "gas费",
                    "转账", "defi", "dao", "公链", "私钥", "助记词",
                    "blockchain", "crypto", "bitcoin", "ethereum", "solana",
                    "wallet", "token", "mining",
                ],
                patterns: vec![
                    Regex::new(r"0x[0-9a-fA-F]{40}").unwrap(),
                    Regex::new(r"(?i)\b(BTC|ETH|BNB|SOL|USDT|USDC|MATIC|AVAX)\b").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 65,
            },
            // ── FileOps (priority 60) ──
            IntentRule {
                category: IntentCategory::FileOps,
                keywords: vec![
                    "读文件", "写文件", "创建文件", "删除文件", "列目录", "列出文件",
                    "重命名", "打开文件", "编辑文件", "复制文件", "移动文件",
                    "read file", "write file", "create file", "delete file",
                    "list dir", "open file", "edit file", "rename file",
                ],
                patterns: vec![
                    Regex::new(r"(?i)\.(rs|py|go|js|ts|json|toml|yaml|yml|md|txt|csv|sh|log|conf|cfg|ini)\b").unwrap(),
                    Regex::new(r"(?i)(read|write|edit|create|delete|rename|copy|move)\s+(file|directory|folder|dir)").unwrap(),
                    Regex::new(r"(?i)\b(cat|ls|mkdir|rm|cp|mv|touch|chmod)\s+").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 60,
            },
            // ── WebSearch (priority 55) ──
            IntentRule {
                category: IntentCategory::WebSearch,
                keywords: vec![
                    "搜索", "查一下", "查询", "找一找", "查找", "搜一搜", "百度", "谷歌", "网上找",
                    "search", "google", "bing", "look up", "find out", "browse",
                ],
                patterns: vec![
                    Regex::new(r"(?i)\b(what\s+is|how\s+to|where\s+is|when\s+did|who\s+is)\b").unwrap(),
                    Regex::new(r"(?i)(网上|网页|互联网|internet|web)\s*(搜|找|查|看)").unwrap(),
                ],
                negative: vec!["股价", "行情", "stock price"],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 55,
            },
            // ── DataAnalysis (priority 60) ──
            IntentRule {
                category: IntentCategory::DataAnalysis,
                keywords: vec![
                    "数据分析", "图表", "可视化", "统计", "报表", "画图",
                    "折线图", "柱状图", "饼图", "散点图",
                    "analyze", "chart", "graph", "plot", "visualize",
                    "statistics", "report", "dashboard",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(数据|data)\s*(处理|分析|清洗|转换|导出|挖掘)").unwrap(),
                    Regex::new(r"(?i)(生成|绘制|画)\s*(图|表|报告)").unwrap(),
                    Regex::new(r"(?i)\.(csv|xlsx|xls|parquet)\b").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 60,
            },
            // ── Communication (priority 60) ──
            IntentRule {
                category: IntentCategory::Communication,
                keywords: vec![
                    "发邮件", "发消息", "发短信", "通知", "群发", "回复消息", "发送邮件",
                    "send email", "send message", "notify", "email to",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(发送|send)\s*(邮件|email|消息|message|通知|notification)").unwrap(),
                    Regex::new(r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 60,
            },
            // ── SystemControl (priority 60) ──
            IntentRule {
                category: IntentCategory::SystemControl,
                keywords: vec![
                    "系统信息", "cpu", "内存", "磁盘", "进程", "截图", "相机", "拍照",
                    "打开应用", "关闭应用", "系统状态",
                    "system info", "cpu usage", "disk space",
                    "process", "screenshot", "camera",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(打开|关闭|重启|安装|卸载)\s*(应用|软件|程序|app)").unwrap(),
                    Regex::new(r"(?i)(系统|system)\s*(负载|使用率|状态|监控)").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 60,
            },
            // ── Organization (priority 55) ──
            IntentRule {
                category: IntentCategory::Organization,
                keywords: vec![
                    "定时", "提醒", "日程", "任务", "计划", "待办", "cron", "记住",
                    "记录", "备忘", "记事",
                    "remind me", "schedule task", "todo list", "calendar event",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(设置|创建|添加)\s*(提醒|任务|日程|闹钟)").unwrap(),
                    Regex::new(r"\d+\s*(分钟|小时|天|周)\s*(后|内|提醒)").unwrap(),
                    Regex::new(r"(?i)(every|每)\s*(day|天|hour|小时|week|周)").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 55,
            },
            // ── IoT (priority 65) ──
            IntentRule {
                category: IntentCategory::IoT,
                keywords: vec![
                    "iot", "智能家居", "传感器", "设备控制", "mqtt",
                    "温度计", "湿度", "灯光",
                    "smart home", "sensor", "temperature", "humidity",
                    "thermostat", "zigbee",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(打开|关闭|调节)\s*(灯|空调|窗帘|风扇|暖气|热水器)").unwrap(),
                    Regex::new(r"(?i)\b(mqtt|zigbee|z-wave|homeassistant|home\s*assistant)\b").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 65,
            },
            // ── Media (priority 60) ──
            IntentRule {
                category: IntentCategory::Media,
                keywords: vec![
                    "语音转文字", "文字转语音", "ocr", "识图", "图片理解",
                    "视频处理", "音频", "转写", "字幕",
                    "transcribe", "tts", "text to speech", "image recognition",
                    "video process", "audio",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(识别|提取|转换)\s*(图片|图像|音频|视频|文字|语音)").unwrap(),
                    Regex::new(r"(?i)\.(mp3|mp4|wav|avi|mkv|jpg|jpeg|png|gif|webp)\b").unwrap(),
                    Regex::new(r"(?i)(语音|voice|audio)\s*(识别|转文|to\s*text)").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 60,
            },
            // ── DevOps (priority 60) ──
            IntentRule {
                category: IntentCategory::DevOps,
                keywords: vec![
                    "部署", "运维", "监控", "端口", "加密", "解密",
                    "哈希", "证书", "ssh", "docker", "kubernetes", "k8s",
                    "deploy", "devops", "encrypt", "decrypt",
                    "hash", "certificate", "firewall",
                ],
                patterns: vec![
                    Regex::new(r"(?i)\b(GET|POST|PUT|DELETE|PATCH)\s+https?://").unwrap(),
                    Regex::new(r"(?i)\b(ping|curl|wget|nmap|ssh|scp)\s+").unwrap(),
                    Regex::new(r"(?i)\b(docker|kubectl|helm)\s+(run|build|push|deploy|apply)").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 60,
            },
            // ── Lifestyle (priority 50) ──
            IntentRule {
                category: IntentCategory::Lifestyle,
                keywords: vec![
                    "健康", "运动", "饮食", "卡路里", "跑步", "睡眠",
                    "天气", "菜谱", "旅游", "生活", "体重", "减肥",
                    "health", "exercise", "diet", "calories", "sleep",
                    "weather", "recipe", "travel",
                ],
                patterns: vec![
                    Regex::new(r"(?i)(今天|明天|后天)\s*(天气|气温|下雨|温度)").unwrap(),
                    Regex::new(r"(?i)(推荐|建议)\s*(菜|食谱|运动|健身)").unwrap(),
                ],
                negative: vec![],
                keywords_dyn: vec![],
                negative_dyn: vec![],
                priority: 50,
            },
        ];

        Self { rules }
    }

    /// 在内置规则基础上，叠加来自配置文件的自定义规则，返回新的分类器实例。
    /// 当 `extra_rules` 为空时等价于 `Self::new()`。
    ///
    /// 配置规则始终在内置规则之后追加，优先级由配置中的 priority 字段决定。
    /// 如果配置文件中出现重复 category，只会记录第一次，忽略后续重复项并发出警告。
    pub fn with_extra_rules(extra_rules: &[blockcell_core::config::IntentRuleConfig]) -> Self {
        let mut classifier = Self::new();
        // 用 String 而非 &IntentCategory 引用，避免 borrow 冲突
        let mut seen_extra_categories: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for rule_cfg in extra_rules {
            let Some(category) = IntentCategory::from_name(&rule_cfg.category) else {
                tracing::warn!(
                    category = %rule_cfg.category,
                    "intentRouter.intentRules 中包含未知的意图类别，已跳过"
                );
                continue;
            };
            // 配置文件出现重复 category 时，只记录第一次并警告
            if !seen_extra_categories.insert(rule_cfg.category.clone()) {
                tracing::warn!(
                    category = %rule_cfg.category,
                    "intentRouter.intentRules 中重复的 category 已跳过"
                );
                continue;
            }
            let mut patterns = Vec::new();
            for pat in &rule_cfg.patterns {
                match Regex::new(pat) {
                    Ok(re) => patterns.push(re),
                    Err(e) => {
                        tracing::warn!(
                            pattern = %pat,
                            error = %e,
                            "intentRouter.intentRules 中的正则表达式无效，已跳过"
                        );
                    }
                }
            }
            // 预先 lowercase，避免每次匹配时重复分配
            let keywords_dyn: Vec<String> =
                rule_cfg.keywords.iter().map(|s| s.to_lowercase()).collect();
            let negative_dyn: Vec<String> =
                rule_cfg.negative.iter().map(|s| s.to_lowercase()).collect();
            classifier.rules.push(IntentRule {
                category,
                keywords: vec![],
                keywords_dyn,
                patterns,
                negative: vec![],
                negative_dyn,
                priority: rule_cfg.priority,
            });
        }
        classifier
    }

    /// Classify user input into one or more intent categories.
    /// Returns up to 2 categories, sorted by priority.
    pub fn classify(&self, input: &str) -> Vec<IntentCategory> {
        let input_lower = input.to_lowercase();
        let mut matches: Vec<(IntentCategory, u8)> = Vec::new();

        for rule in &self.rules {
            if self.rule_matches(rule, input, &input_lower) {
                matches.push((rule.category.clone(), rule.priority));
            }
        }

        if matches.is_empty() {
            return vec![IntentCategory::Unknown];
        }

        // Sort by priority descending; use category name as secondary key for determinism
        matches.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));
        matches.dedup_by(|a, b| a.0 == b.0);

        // If Chat is the only match, return it alone
        if matches.len() == 1 && matches[0].0 == IntentCategory::Chat {
            return vec![IntentCategory::Chat];
        }

        // If Chat is matched alongside other intents, drop Chat
        matches.retain(|m| m.0 != IntentCategory::Chat);

        if matches.is_empty() {
            return vec![IntentCategory::Unknown];
        }

        // Take top 2
        matches.into_iter().take(2).map(|(c, _)| c).collect()
    }

    fn rule_matches(&self, rule: &IntentRule, input: &str, input_lower: &str) -> bool {
        // Check negative keywords first (static + pre-lowercased dynamic)
        for neg in rule
            .negative
            .iter()
            .copied()
            .chain(rule.negative_dyn.iter().map(String::as_str))
        {
            if input_lower.contains(&neg.to_lowercase()) {
                return false;
            }
        }

        // Check regex patterns
        for pattern in &rule.patterns {
            if pattern.is_match(input) {
                return true;
            }
        }

        // Check keywords (static + pre-lowercased dynamic — avoid double-lowercasing)
        for keyword in &rule.keywords {
            if input_lower.contains(&keyword.to_lowercase()) {
                return true;
            }
        }
        for keyword in rule.keywords_dyn.iter() {
            if input_lower.contains(keyword.as_str()) {
                return true;
            }
        }

        false
    }
}

pub struct IntentToolResolver<'a> {
    config: &'a Config,
}

impl<'a> IntentToolResolver<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    pub fn resolve_tool_names(
        &self,
        agent_id: Option<&str>,
        intents: &[IntentCategory],
        available_tools: Option<&HashSet<String>>,
    ) -> Option<Vec<String>> {
        let default_router;
        let router = if let Some(router) = self.config.intent_router.as_ref() {
            router
        } else {
            default_router = blockcell_core::config::IntentRouterConfig::default();
            &default_router
        };

        let profile_id = self.config.resolve_intent_profile_id(agent_id)?;
        let profile = router.profiles.get(&profile_id)?;

        let effective_intents: Vec<IntentCategory> = if router.enabled {
            intents.to_vec()
        } else {
            vec![IntentCategory::Unknown]
        };

        let mut tools = HashSet::new();
        for intent in &effective_intents {
            if let Some(entry) = profile.intent_tools.get(intent.as_str()) {
                if entry.inherit_base() {
                    for tool in &profile.core_tools {
                        tools.insert(tool.clone());
                    }
                }
                for tool in entry.tools() {
                    tools.insert(tool.clone());
                }
            } else {
                for tool in &profile.core_tools {
                    tools.insert(tool.clone());
                }
            }
        }

        for tool in &profile.deny_tools {
            tools.remove(tool);
        }

        if let Some(available_tools) = available_tools {
            tools.retain(|tool| available_tools.contains(tool));
        }

        let mut result: Vec<String> = tools.into_iter().collect();
        result.sort();
        Some(result)
    }

    pub fn validate(&self, registry: &ToolRegistry) -> Result<()> {
        self.validate_with_mcp(registry, None)
    }

    pub fn validate_with_mcp(
        &self,
        registry: &ToolRegistry,
        mcp: Option<&McpResolvedConfig>,
    ) -> Result<()> {
        let default_router;
        let router = if let Some(router) = self.config.intent_router.as_ref() {
            router
        } else {
            default_router = blockcell_core::config::IntentRouterConfig::default();
            &default_router
        };

        let default_profile = router.default_profile.trim();
        if default_profile.is_empty() {
            return Err(Error::Config(
                "intentRouter.defaultProfile must not be empty".to_string(),
            ));
        }
        if !router.profiles.contains_key(default_profile) {
            return Err(Error::Config(format!(
                "intentRouter.defaultProfile '{}' does not exist",
                default_profile
            )));
        }

        for (agent_id, profile_id) in &router.agent_profiles {
            if !router.profiles.contains_key(profile_id) {
                return Err(Error::Config(format!(
                    "intentRouter.agentProfiles.{} references missing profile '{}'",
                    agent_id, profile_id
                )));
            }
        }

        for agent in &self.config.agents.list {
            if let Some(profile_id) = agent.intent_profile.as_deref() {
                let profile_id = profile_id.trim();
                if !profile_id.is_empty() && !router.profiles.contains_key(profile_id) {
                    return Err(Error::Config(format!(
                        "agents.list[{}].intentProfile references missing profile '{}'",
                        agent.id, profile_id
                    )));
                }
            }
        }

        let registered: HashSet<String> = registry.tool_names().into_iter().collect();
        for (profile_name, profile) in &router.profiles {
            if !profile
                .intent_tools
                .contains_key(IntentCategory::Unknown.as_str())
            {
                return Err(Error::Config(format!(
                    "intentRouter.profiles.{} must configure Unknown intent",
                    profile_name
                )));
            }

            for intent_name in profile.intent_tools.keys() {
                if IntentCategory::from_name(intent_name).is_none() {
                    return Err(Error::Config(format!(
                        "intentRouter.profiles.{}.intentTools contains invalid intent '{}'",
                        profile_name, intent_name
                    )));
                }
            }

            for tool in &profile.core_tools {
                ensure_known_tool(self.config, mcp, profile_name, tool, &registered)?;
            }
            for tool in &profile.deny_tools {
                ensure_known_tool(self.config, mcp, profile_name, tool, &registered)?;
            }
            for entry in profile.intent_tools.values() {
                for tool in entry.tools() {
                    ensure_known_tool(self.config, mcp, profile_name, tool, &registered)?;
                }
            }
        }

        Ok(())
    }
}

fn ensure_known_tool(
    config: &Config,
    mcp: Option<&McpResolvedConfig>,
    profile_name: &str,
    tool_name: &str,
    registered: &HashSet<String>,
) -> Result<()> {
    if registered.contains(tool_name) || declared_mcp_tool(config, mcp, tool_name, registered) {
        Ok(())
    } else if is_optional_feature_tool(tool_name) {
        // Optional feature tools (e.g., napcat_*) may not be registered if the feature is disabled
        Ok(())
    } else {
        Err(Error::Config(format!(
            "intentRouter.profiles.{} references unknown tool '{}'",
            profile_name, tool_name
        )))
    }
}

/// Check if a tool is from an optional feature that may not be enabled.
/// These tools are allowed to be missing from the registry.
fn is_optional_feature_tool(tool_name: &str) -> bool {
    tool_name.starts_with("napcat_")
}

fn declared_mcp_tool(
    _config: &Config,
    mcp: Option<&McpResolvedConfig>,
    tool_name: &str,
    registered: &HashSet<String>,
) -> bool {
    let Some((server_name, tool_suffix)) = tool_name.split_once("__") else {
        return false;
    };

    let server_name = server_name.trim();
    let tool_suffix = tool_suffix.trim();
    if server_name.is_empty() || tool_suffix.is_empty() {
        return false;
    }

    let Some(mcp) = mcp else {
        return false;
    };
    let Some(server) = mcp.servers.get(server_name) else {
        return false;
    };
    if !server.enabled {
        return false;
    }

    let server_prefix = format!("{}__", server_name);
    let discovered_server_tools = registered
        .iter()
        .filter(|name| name.starts_with(&server_prefix))
        .count();

    if discovered_server_tools == 0 {
        true
    } else {
        registered.contains(tool_name)
    }
}

/// Check if the intents should show skills list.
pub fn needs_skills_list(intents: &[IntentCategory]) -> bool {
    !intents.iter().any(|i| matches!(i, IntentCategory::Chat))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::Config;
    use blockcell_tools::ToolRegistry;

    #[test]
    fn test_chat_classification() {
        let classifier = IntentClassifier::new();
        assert_eq!(classifier.classify("你好"), vec![IntentCategory::Chat]);
        assert_eq!(classifier.classify("hello"), vec![IntentCategory::Chat]);
        assert_eq!(classifier.classify("Hi!"), vec![IntentCategory::Chat]);
        assert_eq!(classifier.classify("谢谢"), vec![IntentCategory::Chat]);
        assert_eq!(classifier.classify("再见"), vec![IntentCategory::Chat]);
        assert_eq!(classifier.classify("你是谁?"), vec![IntentCategory::Chat]);
    }

    #[test]
    fn test_non_chat_classification_falls_back_to_unknown() {
        let classifier = IntentClassifier::new();
        // 现在这些有了明确意图规则，不再是 Unknown
        assert_eq!(
            classifier.classify("查一下茅台股价"),
            vec![IntentCategory::Finance]
        );
        assert_eq!(
            classifier.classify("0x1234567890abcdef1234567890abcdef12345678 这个地址安全吗"),
            vec![IntentCategory::Blockchain]
        );
        assert_eq!(
            classifier.classify("帮我读一下 config.json5"),
            vec![IntentCategory::FileOps]
        );
        // 完全模糊的输入仍然是 Unknown
        assert_eq!(
            classifier.classify("帮我做一件复杂的事情"),
            vec![IntentCategory::Unknown]
        );
    }

    #[test]
    fn test_finance_classification() {
        let c = IntentClassifier::new();
        assert_eq!(c.classify("查一下茅台股价"), vec![IntentCategory::Finance]);
        assert_eq!(
            c.classify("我想了解一下A股行情"),
            vec![IntentCategory::Finance]
        );
        assert_eq!(c.classify("介绍一下ETF基金"), vec![IntentCategory::Finance]);
        // 负例：普通问候不应触发 Finance
        assert_ne!(c.classify("你好"), vec![IntentCategory::Finance]);
        assert_ne!(c.classify("再见"), vec![IntentCategory::Finance]);
    }

    #[test]
    fn test_blockchain_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("0x742d35Cc6634C0532925a3b844Bc454e4438f44e 这个地址安全吗"),
            vec![IntentCategory::Blockchain]
        );
        assert_eq!(
            c.classify("钱包里的ETH怎么转账"),
            vec![IntentCategory::Blockchain]
        );
        assert_eq!(
            c.classify("区块链上的NFT怎么铸造"),
            vec![IntentCategory::Blockchain]
        );
        // 负例
        assert_ne!(c.classify("你好"), vec![IntentCategory::Blockchain]);
    }

    #[test]
    fn test_fileops_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("帮我读一下 config.json5"),
            vec![IntentCategory::FileOps]
        );
        assert_eq!(
            c.classify("列出当前目录的文件"),
            vec![IntentCategory::FileOps]
        );
        assert_eq!(
            c.classify("edit the README.md file"),
            vec![IntentCategory::FileOps]
        );
        assert_eq!(
            c.classify("创建一个新的 main.rs 文件"),
            vec![IntentCategory::FileOps]
        );
        // 负例：URL 中的文件扩展名（无空格边界）不应触发 FileOps
        assert_ne!(
            c.classify("打开 https://example.com/main.rs 看看"),
            vec![IntentCategory::FileOps]
        );
        // 负例："rs" 作为单词一部分不触发
        assert_ne!(
            c.classify("我喜欢 rust 和 python"),
            vec![IntentCategory::FileOps]
        );
        // 负例
        assert_ne!(c.classify("谢谢"), vec![IntentCategory::FileOps]);
    }

    #[test]
    fn test_websearch_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("搜索一下量子计算"),
            vec![IntentCategory::WebSearch]
        );
        assert_eq!(
            c.classify("google how to learn rust"),
            vec![IntentCategory::WebSearch]
        );
        assert_eq!(
            c.classify("what is a neural network"),
            vec![IntentCategory::WebSearch]
        );
        // 股价搜索应该命中 Finance negative 过滤，不触发 WebSearch
        let result = c.classify("搜索一下股价行情");
        assert!(!result.contains(&IntentCategory::WebSearch));
    }

    #[test]
    fn test_dataanalysis_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("帮我做一个数据分析"),
            vec![IntentCategory::DataAnalysis]
        );
        assert_eq!(
            c.classify("画一个折线图"),
            vec![IntentCategory::DataAnalysis]
        );
        assert_eq!(
            c.classify("分析这个 data.csv 文件"),
            vec![IntentCategory::DataAnalysis]
        );
        // 负例：闲聊中的图表字样不应触发
        assert_ne!(
            c.classify("今天天气真好"),
            vec![IntentCategory::DataAnalysis]
        );
    }

    #[test]
    fn test_communication_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("发邮件给 boss@company.com"),
            vec![IntentCategory::Communication]
        );
        assert_eq!(
            c.classify("帮我发一条消息通知团队"),
            vec![IntentCategory::Communication]
        );
        // 负例：提到邮件地址但不是要发邮件
        assert_ne!(
            c.classify("我的邮箱是 user@domain.com，记得联系我"),
            vec![IntentCategory::Communication]
        );
    }

    #[test]
    fn test_systemcontrol_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("查看当前CPU使用率"),
            vec![IntentCategory::SystemControl]
        );
        assert_eq!(
            c.classify("打开微信应用"),
            vec![IntentCategory::SystemControl]
        );
        assert_eq!(
            c.classify("系统负载状态怎么样"),
            vec![IntentCategory::SystemControl]
        );
        // 负例：内存作为日常用语不应触发
        assert_ne!(
            c.classify("我最近记忆力不太好"),
            vec![IntentCategory::SystemControl]
        );
    }

    #[test]
    fn test_organization_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("30分钟后提醒我开会"),
            vec![IntentCategory::Organization]
        );
        assert_eq!(
            c.classify("添加一个每天的提醒"),
            vec![IntentCategory::Organization]
        );
        assert_eq!(
            c.classify("设置一个早上8点的日程"),
            vec![IntentCategory::Organization]
        );
        // 负例：cargo task 是构建命令，不是任务管理
        assert_ne!(
            c.classify("cargo task build"),
            vec![IntentCategory::Organization]
        );
    }

    #[test]
    fn test_iot_classification() {
        let c = IntentClassifier::new();
        assert_eq!(c.classify("打开客厅的灯"), vec![IntentCategory::IoT]);
        assert_eq!(c.classify("调节空调温度"), vec![IntentCategory::IoT]);
        assert_eq!(c.classify("关闭窗帘"), vec![IntentCategory::IoT]);
        // 负例：问温度是 Lifestyle，不是 IoT
        assert_ne!(c.classify("今天温度多少度"), vec![IntentCategory::IoT]);
    }

    #[test]
    fn test_media_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("帮我识别这张图片里的文字"),
            vec![IntentCategory::Media]
        );
        assert_eq!(
            c.classify("把这段音频转成文字"),
            vec![IntentCategory::Media]
        );
        assert_eq!(
            c.classify("处理一下这个 video.mp4 文件"),
            vec![IntentCategory::Media]
        );
        // 负例：提到 mp4 但只是闲聊不应触发
        assert_ne!(
            c.classify("我下载了一个 video.mp4 视频"),
            vec![IntentCategory::Media]
        );
    }

    #[test]
    fn test_devops_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("POST https://api.example.com/users"),
            vec![IntentCategory::DevOps]
        );
        assert_eq!(c.classify("ping 192.168.1.1"), vec![IntentCategory::DevOps]);
        assert_eq!(
            c.classify("docker build -t myapp ."),
            vec![IntentCategory::DevOps]
        );
        // 负例：问 curl 命令是什么不应触发 DevOps（是问问题，不是用工具）
        assert_ne!(c.classify("curl 命令是什么"), vec![IntentCategory::DevOps]);
    }

    #[test]
    fn test_lifestyle_classification() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("今天天气怎么样"),
            vec![IntentCategory::Lifestyle]
        );
        assert_eq!(
            c.classify("推荐一个健康的菜谱"),
            vec![IntentCategory::Lifestyle]
        );
        assert_eq!(
            c.classify("明天温度多少度"),
            vec![IntentCategory::Lifestyle]
        );
        // 负例：问内存价格不应触发 Lifestyle
        assert_ne!(
            c.classify("内存价格最近怎么样"),
            vec![IntentCategory::Lifestyle]
        );
    }

    #[test]
    fn test_unknown_for_truly_ambiguous_input() {
        let c = IntentClassifier::new();
        assert_eq!(
            c.classify("帮我做一件复杂的事情"),
            vec![IntentCategory::Unknown]
        );
        assert_eq!(
            c.classify("balabala xyzzy quux"),
            vec![IntentCategory::Unknown]
        );
    }

    #[test]
    fn test_global_singleton_is_same_instance() {
        let a = IntentClassifier::global();
        let b = IntentClassifier::global();
        // 同一个 static 引用，地址应该相同
        assert!(std::ptr::eq(a, b));
    }

    #[test]
    fn test_with_extra_rules_adds_and_deduplicates() {
        // 测试追加配置规则时：内置规则保留；重复 extra category 只添加第一个并警告
        let extra = vec![
            blockcell_core::config::IntentRuleConfig {
                category: "Finance".to_string(),
                keywords: vec!["狗狗币".to_string(), "屎币".to_string()],
                patterns: vec![],
                negative: vec![],
                priority: 70,
            },
            blockcell_core::config::IntentRuleConfig {
                category: "Finance".to_string(), // 重复 category，跳过
                keywords: vec!["其他币".to_string()],
                patterns: vec![],
                negative: vec![],
                priority: 70,
            },
            blockcell_core::config::IntentRuleConfig {
                category: "IoT".to_string(),
                keywords: vec!["热水器".to_string()],
                patterns: vec![],
                negative: vec![],
                priority: 65,
            },
        ];
        let c = IntentClassifier::with_extra_rules(&extra);
        // 第一个 Finance extra 规则中的两个关键词都应该匹配
        assert_eq!(
            c.classify("查一下狗狗币价格"),
            vec![IntentCategory::Finance]
        );
        assert_eq!(c.classify("查一下屎币"), vec![IntentCategory::Finance]);
        // 第二个 Finance extra 被跳过，所以"其他币"不触发 Finance（只有内置关键词"股价"等会触发）
        assert_ne!(c.classify("查一下其他币"), vec![IntentCategory::Finance]);
        assert_eq!(c.classify("打开热水器"), vec![IntentCategory::IoT]);
    }

    #[test]
    fn test_intent_router_resolves_chat_without_base_tools() {
        let config = Config::default();
        let resolver = IntentToolResolver::new(&config);

        let tools = resolver
            .resolve_tool_names(None, &[IntentCategory::Chat], None)
            .expect("config router tools");

        assert!(tools.is_empty());
    }

    #[test]
    fn test_intent_router_resolves_agent_profile_and_applies_deny_list() {
        let raw = r#"{
  "agents": {
    "list": [
      { "id": "ops", "enabled": true, "intentProfile": "ops" }
    ]
  },
  "intentRouter": {
    "enabled": true,
    "defaultProfile": "default",
    "profiles": {
      "default": {
        "coreTools": ["read_file", "message"],
        "intentTools": {
          "Chat": { "inheritBase": false, "tools": [] },
          "Unknown": ["browse"]
        }
      },
      "ops": {
        "coreTools": ["read_file", "exec", "email"],
        "intentTools": {
          "DevOps": ["network_monitor", "http_request"],
          "Unknown": ["http_request"]
        },
        "denyTools": ["email"]
      }
    }
  }
}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let resolver = IntentToolResolver::new(&config);

        let tools = resolver
            .resolve_tool_names(Some("ops"), &[IntentCategory::DevOps], None)
            .expect("config router tools");

        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"exec".to_string()));
        assert!(tools.contains(&"network_monitor".to_string()));
        assert!(!tools.contains(&"email".to_string()));
    }

    #[test]
    fn test_intent_router_uses_default_router_for_missing_config() {
        let config: Config = serde_json::from_str("{}").unwrap();
        let resolver = IntentToolResolver::new(&config);
        let tools = resolver
            .resolve_tool_names(None, &[IntentCategory::Unknown], None)
            .expect("default intent router tools");

        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"browse".to_string()));
    }

    #[test]
    fn test_intent_router_validation_rejects_invalid_tools() {
        let raw = r#"{
  "intentRouter": {
    "enabled": true,
    "defaultProfile": "default",
    "profiles": {
      "default": {
        "coreTools": ["read_file"],
        "intentTools": {
          "Chat": { "inheritBase": false, "tools": [] },
          "Unknown": ["definitely_missing_tool"]
        }
      }
    }
  }
}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let resolver = IntentToolResolver::new(&config);
        let registry = ToolRegistry::with_defaults();

        assert!(resolver.validate(&registry).is_err());
    }

    #[test]
    fn test_intent_router_validation_accepts_declared_mcp_tool_prefix() {
        let raw = r#"{
  "intentRouter": {
    "enabled": true,
    "defaultProfile": "default",
    "profiles": {
      "default": {
        "coreTools": ["read_file"],
        "intentTools": {
          "Chat": { "inheritBase": false, "tools": [] },
          "Unknown": ["github__search_repositories"]
        }
      }
    }
  }
}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let resolver = IntentToolResolver::new(&config);
        let registry = ToolRegistry::with_defaults();
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "github".to_string(),
            blockcell_core::mcp_config::McpServerConfig {
                command: "npx".to_string(),
                args: vec![
                    "-y".to_string(),
                    "@modelcontextprotocol/server-github".to_string(),
                ],
                env: std::collections::HashMap::new(),
                cwd: None,
                enabled: true,
                auto_start: true,
                startup_timeout_secs: 20,
                call_timeout_secs: 60,
            },
        );
        let mcp = blockcell_core::mcp_config::McpResolvedConfig {
            defaults: blockcell_core::mcp_config::McpDefaultsConfig::default(),
            servers,
        };

        assert!(resolver.validate_with_mcp(&registry, Some(&mcp)).is_ok());
    }

    #[test]
    fn test_disabled_intent_router_falls_back_to_unknown_profile_tools() {
        let raw = r#"{
  "intentRouter": {
    "enabled": false,
    "defaultProfile": "default",
    "profiles": {
      "default": {
        "coreTools": ["read_file"],
        "intentTools": {
          "Chat": { "inheritBase": false, "tools": [] },
          "Unknown": ["browse"]
        }
      }
    }
  }
}"#;
        let config: Config = serde_json::from_str(raw).unwrap();
        let resolver = IntentToolResolver::new(&config);

        let tools = resolver
            .resolve_tool_names(None, &[IntentCategory::Chat], None)
            .expect("disabled router still resolves config tools");

        assert!(tools.contains(&"read_file".to_string()));
        assert!(tools.contains(&"browse".to_string()));
    }
}
