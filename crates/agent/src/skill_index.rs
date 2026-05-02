//! Skill Progressive Index — 渐进式 Skill 加载
//!
//! 核心思路:
//! - 启动时只加载 meta 信息 (name, category, description, tools)
//! - Agent 需要使用 Skill 时, 通过 skill_view 按需加载完整内容
//! - 减少系统提示词长度, 节省 token
//!
//! 参考 Hermes `agent_core/skill_index.py`

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Skill 索引条目 (轻量, 仅 meta)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillIndexEntry {
    /// Skill 名称
    pub name: String,
    /// 分类
    pub category: String,
    /// 描述 (从 meta.yaml 或 SKILL.md frontmatter 提取)
    pub description: String,
    /// 需要的工具列表
    pub tools: Vec<String>,
    /// 是否始终加载 (always: true)
    pub always: bool,
    /// Skill 目录路径
    pub path: PathBuf,
    /// 版本号 (从 meta.json 读取, 默认 "1.0.0")
    #[serde(default = "default_version")]
    pub version: String,
    /// 最后更新时间 (ISO 8601 格式)
    #[serde(default)]
    pub updated_at: Option<String>,
}

fn default_version() -> String {
    "1.0.0".to_string()
}

/// Skill Progressive Index
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillIndex {
    /// 所有已索引的 Skill
    entries: HashMap<String, SkillIndexEntry>,
    /// 已加载完整内容的 Skill (name → content)
    loaded: HashMap<String, String>,
}

impl SkillIndex {
    /// 创建空索引
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            loaded: HashMap::new(),
        }
    }

    /// 从 skills 目录构建索引
    pub fn build_from_dir(skills_dir: &Path) -> Self {
        let mut index = Self::new();

        if !skills_dir.exists() {
            return index;
        }

        // 遍历 skills_dir 下的子目录
        // 每个子目录可能是:
        //   1. 一个 category 目录 (包含多个 skill 子目录)
        //   2. 一个无 category 的 skill 目录 (直接包含 SKILL.md)
        if let Ok(entries) = std::fs::read_dir(skills_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let dir_name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("unknown")
                    .to_string();

                // 判断: 如果该目录直接包含 SKILL.md, 则它是一个无 category 的 skill
                if path.join("SKILL.md").exists() {
                    let entry = Self::build_entry(&dir_name, "", &path);
                    index.entries.insert(dir_name.clone(), entry);
                } else {
                    // 该目录是一个 category, 遍历其下的 skill 子目录
                    if let Ok(skill_entries) = std::fs::read_dir(&path) {
                        for skill_entry in skill_entries.flatten() {
                            let skill_path = skill_entry.path();
                            if !skill_path.is_dir() {
                                continue;
                            }
                            // 只索引包含 SKILL.md 的目录 (跳过 .git, __pycache__ 等非 skill 目录)
                            if !skill_path.join("SKILL.md").exists() {
                                continue;
                            }
                            let skill_name = skill_path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let entry = Self::build_entry(&skill_name, &dir_name, &skill_path);
                            index.entries.insert(skill_name.clone(), entry);
                        }
                    }
                }
            }
        }

        tracing::info!(
            skill_count = index.entries.len(),
            "[SkillIndex] Built index from {}",
            skills_dir.display()
        );

        index
    }

    /// 从单个 Skill 目录构建索引条目
    fn build_entry(name: &str, category: &str, path: &Path) -> SkillIndexEntry {
        let mut description = String::new();
        let mut tools = Vec::new();
        let mut always = false;
        let mut version = default_version();
        let mut updated_at: Option<String> = None;

        // 1. 尝试从 meta.json 读取
        let meta_path = path.join("meta.json");
        if meta_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&meta_path) {
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(desc) = meta.get("description").and_then(|v| v.as_str()) {
                        description = desc.to_string();
                    }
                    if let Some(t) = meta.get("tools").and_then(|v| v.as_array()) {
                        tools = t
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect();
                    }
                    if let Some(a) = meta.get("always").and_then(|v| v.as_bool()) {
                        always = a;
                    }
                    if let Some(v) = meta.get("version").and_then(|v| v.as_str()) {
                        version = v.to_string();
                    }
                    if let Some(u) = meta.get("updated_at").and_then(|v| v.as_str()) {
                        updated_at = Some(u.to_string());
                    }
                }
            }
        }

        // 2. 回退到 SKILL.md frontmatter
        if description.is_empty() {
            let skill_md = path.join("SKILL.md");
            if let Ok(content) = std::fs::read_to_string(&skill_md) {
                description = Self::extract_description(&content);
                if tools.is_empty() {
                    tools = Self::extract_tools(&content);
                }
                if !always {
                    always = Self::extract_always(&content);
                }
                if version == default_version() {
                    version = Self::extract_version(&content);
                }
            }
        }

        // 3. 从文件修改时间获取 updated_at (如果 meta.json 和 frontmatter 都没有)
        if updated_at.is_none() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                if let Ok(metadata) = std::fs::metadata(&skill_md) {
                    if let Ok(modified) = metadata.modified() {
                        let dt: DateTime<Utc> = modified.into();
                        updated_at = Some(dt.to_rfc3339());
                    }
                }
            }
        }

        // 4. 最终回退
        if description.is_empty() {
            description = format!("Skill: {}", name);
        }

        SkillIndexEntry {
            name: name.to_string(),
            category: category.to_string(),
            description,
            tools,
            always,
            path: path.to_path_buf(),
            version,
            updated_at,
        }
    }

    /// 从 SKILL.md 内容提取描述
    fn extract_description(content: &str) -> String {
        let trimmed = content.trim();

        // 尝试从 frontmatter 提取
        if let Some(rest) = trimmed.strip_prefix("---") {
            if let Some(end_idx) = rest.find("---") {
                let frontmatter = &rest[..end_idx];
                for line in frontmatter.lines() {
                    if let Some(val) = line.strip_prefix("description:") {
                        return val.trim().trim_matches('"').trim_matches('\'').to_string();
                    }
                }
            }
        }

        // 回退到第一个标题
        for line in content.lines() {
            let line = line.trim();
            if let Some(heading) = line.strip_prefix("# ") {
                return heading.to_string();
            }
        }

        String::new()
    }

    /// 从 SKILL.md 内容提取工具列表
    fn extract_tools(content: &str) -> Vec<String> {
        let trimmed = content.trim();

        if let Some(rest) = trimmed.strip_prefix("---") {
            if let Some(end_idx) = rest.find("---") {
                let frontmatter = &rest[..end_idx];
                for line in frontmatter.lines() {
                    if let Some(val) = line.strip_prefix("tools:") {
                        let val = val.trim();
                        if val.starts_with('[') && val.ends_with(']') {
                            return val[1..val.len() - 1]
                                .split(',')
                                .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                                .filter(|s| !s.is_empty())
                                .collect();
                        }
                    }
                }
            }
        }

        Vec::new()
    }

    /// 从 SKILL.md 内容提取 always 标志
    fn extract_always(content: &str) -> bool {
        let trimmed = content.trim();

        if let Some(rest) = trimmed.strip_prefix("---") {
            if let Some(end_idx) = rest.find("---") {
                let frontmatter = &rest[..end_idx];
                for line in frontmatter.lines() {
                    if let Some(val) = line.strip_prefix("always:") {
                        return val.trim() == "true";
                    }
                }
            }
        }

        false
    }

    /// 从 SKILL.md frontmatter 提取版本号
    fn extract_version(content: &str) -> String {
        let trimmed = content.trim();

        if let Some(rest) = trimmed.strip_prefix("---") {
            if let Some(end_idx) = rest.find("---") {
                let frontmatter = &rest[..end_idx];
                for line in frontmatter.lines() {
                    if let Some(val) = line.strip_prefix("version:") {
                        let v = val.trim().trim_matches('"').trim_matches('\'');
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }

        default_version()
    }

    /// 获取所有 Skill 的索引条目
    pub fn entries(&self) -> &HashMap<String, SkillIndexEntry> {
        &self.entries
    }

    /// 获取 always=true 的 Skill (启动时加载)
    pub fn always_skills(&self) -> Vec<&SkillIndexEntry> {
        self.entries.values().filter(|e| e.always).collect()
    }

    /// 按需加载 Skill 完整内容
    pub fn load_skill(&mut self, name: &str) -> Option<String> {
        // 检查是否已加载
        if let Some(content) = self.loaded.get(name) {
            return Some(content.clone());
        }

        // 从索引查找路径
        let entry = self.entries.get(name)?;
        let skill_md = entry.path.join("SKILL.md");

        if !skill_md.exists() {
            return None;
        }

        let content = std::fs::read_to_string(&skill_md).ok()?;
        self.loaded.insert(name.to_string(), content.clone());

        tracing::debug!(
            skill_name = name,
            content_len = content.len(),
            "[SkillIndex] Loaded skill content"
        );

        Some(content)
    }

    /// 卸载 Skill 内容 (释放内存)
    pub fn unload_skill(&mut self, name: &str) {
        self.loaded.remove(name);
    }

    /// 检查 Skill 是否已加载
    pub fn is_loaded(&self, name: &str) -> bool {
        self.loaded.contains_key(name)
    }

    /// 生成系统提示词中的 Skill 索引摘要
    ///
    /// 格式:
    /// ```text
    /// ## Available Skills (index only, use skill_view to load)
    /// - flask-k8s-deploy [devops]: Deploy Flask apps to Kubernetes (tools: exec, write_file)
    /// - rust-debug [software-development]: Debug Rust compilation errors (always loaded)
    /// ```
    pub fn to_prompt_summary(&self) -> String {
        if self.entries.is_empty() {
            return String::new();
        }

        let mut summary = String::from(
            "## Available Skills (index only, use `skill_view` to load full content)\n",
        );

        let mut entries: Vec<_> = self.entries.values().collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));

        for entry in &entries {
            let tools_str = if entry.tools.is_empty() {
                String::new()
            } else {
                format!(" (tools: {})", entry.tools.join(", "))
            };
            let always_str = if entry.always { " [always loaded]" } else { "" };
            summary.push_str(&format!(
                "- {} [{}]: {}{}{}\n",
                entry.name, entry.category, entry.description, tools_str, always_str
            ));
        }

        summary
    }

    /// 根据当前活跃工具列表匹配 Skill
    ///
    /// 返回工具列表与 active_tools 有交集的 Skill (按交集大小降序排列)
    /// 参考 Hermes `skill_index.py` 的 `match_by_tools()`
    pub fn match_by_tools(&self, active_tools: &[String]) -> Vec<&SkillIndexEntry> {
        if active_tools.is_empty() || self.entries.is_empty() {
            return Vec::new();
        }

        let active_set: HashSet<&str> = active_tools.iter().map(|s| s.as_str()).collect();

        let mut matched: Vec<(usize, &SkillIndexEntry)> = self
            .entries
            .values()
            .filter_map(|entry| {
                if entry.tools.is_empty() {
                    return None;
                }
                let overlap = entry
                    .tools
                    .iter()
                    .filter(|t| active_set.contains(t.as_str()))
                    .count();
                if overlap > 0 {
                    Some((overlap, entry))
                } else {
                    None
                }
            })
            .collect();

        // 按交集大小降序排列 (交集越大越相关)
        matched.sort_by(|a, b| b.0.cmp(&a.0));

        matched.into_iter().map(|(_, entry)| entry).collect()
    }

    /// 模糊搜索 Skill
    ///
    /// 基于字符重叠度 + 子串匹配的轻量级模糊搜索
    /// 参考 Hermes `skill_index.py` 的 `fuzzy_search()`
    ///
    /// 搜索范围: name, description, category
    /// 评分: 子串完全匹配 > 字符重叠度
    pub fn fuzzy_search(&self, query: &str) -> Vec<&SkillIndexEntry> {
        if query.is_empty() || self.entries.is_empty() {
            return Vec::new();
        }

        let query_lower = query.to_lowercase();
        let query_chars: HashSet<char> = query_lower.chars().collect();

        let mut scored: Vec<(i64, &SkillIndexEntry)> = self
            .entries
            .values()
            .filter_map(|entry| {
                let mut score: i64 = 0;

                // 1. 名称子串匹配 (权重最高)
                let name_lower = entry.name.to_lowercase();
                if name_lower == query_lower {
                    score += 1000; // 完全匹配
                } else if name_lower.contains(&query_lower) {
                    score += 500; // 子串匹配
                } else {
                    // 名称字符重叠度
                    let name_chars: HashSet<char> = name_lower.chars().collect();
                    let overlap = name_chars.intersection(&query_chars).count();
                    score += (overlap as i64) * 10;
                }

                // 2. 描述子串匹配
                let desc_lower = entry.description.to_lowercase();
                if desc_lower.contains(&query_lower) {
                    score += 200;
                } else {
                    // 描述字符重叠度
                    let desc_chars: HashSet<char> = desc_lower.chars().collect();
                    let overlap = desc_chars.intersection(&query_chars).count();
                    score += (overlap as i64) * 3;
                }

                // 3. 分类匹配
                let cat_lower = entry.category.to_lowercase();
                if cat_lower.contains(&query_lower) {
                    score += 100;
                }

                // 阈值: 至少要有一定相关性
                if score > 0 {
                    Some((score, entry))
                } else {
                    None
                }
            })
            .collect();

        // 按分数降序排列
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        scored.into_iter().map(|(_, entry)| entry).collect()
    }

    /// 刷新索引 (重新扫描 skills 目录)
    pub fn refresh(&mut self, skills_dir: &Path) {
        let new_index = Self::build_from_dir(skills_dir);
        self.entries = new_index.entries;
        // 保留已加载的内容 (如果 skill 仍然存在)
        self.loaded
            .retain(|name, _| self.entries.contains_key(name));
    }
}

impl Default for SkillIndex {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_skills_dir() -> PathBuf {
        let mut dir = std::env::temp_dir();
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        dir.push(format!(
            "blockcell_skill_index_test_{}_{}",
            std::process::id(),
            now_ns
        ));
        dir
    }

    fn create_test_skill(dir: &Path, category: &str, name: &str, content: &str) {
        let skill_dir = dir.join(category).join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn test_empty_index() {
        let index = SkillIndex::new();
        assert!(index.entries().is_empty());
    }

    #[test]
    fn test_build_from_dir() {
        let dir = temp_skills_dir();
        create_test_skill(
            &dir,
            "devops",
            "flask-deploy",
            "---\nname: flask-deploy\ndescription: Deploy Flask apps\ntools: [exec, write_file]\n---\n\n# Flask Deploy",
        );
        create_test_skill(&dir, "general", "hello", "# Hello Skill\n\nSimple greeting");

        let index = SkillIndex::build_from_dir(&dir);
        assert_eq!(index.entries().len(), 2);
        assert!(index.entries().contains_key("flask-deploy"));
        assert!(index.entries().contains_key("hello"));

        let flask = index.entries().get("flask-deploy").unwrap();
        assert_eq!(flask.category, "devops");
        assert_eq!(flask.description, "Deploy Flask apps");
        assert_eq!(flask.tools, vec!["exec", "write_file"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_always_skills() {
        let dir = temp_skills_dir();
        create_test_skill(
            &dir,
            "general",
            "always-skill",
            "---\nname: always-skill\ndescription: Always loaded\nalways: true\n---\n\n# Always",
        );
        create_test_skill(
            &dir,
            "general",
            "normal-skill",
            "---\nname: normal-skill\ndescription: Normal\n---\n\n# Normal",
        );

        let index = SkillIndex::build_from_dir(&dir);
        let always = index.always_skills();
        assert_eq!(always.len(), 1);
        assert_eq!(always[0].name, "always-skill");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_skill() {
        let dir = temp_skills_dir();
        create_test_skill(
            &dir,
            "general",
            "load-test",
            "# Load Test\n\nFull content here",
        );

        let mut index = SkillIndex::build_from_dir(&dir);
        assert!(!index.is_loaded("load-test"));

        let content = index.load_skill("load-test").unwrap();
        assert!(content.contains("Full content here"));
        assert!(index.is_loaded("load-test"));

        // 再次加载应返回缓存
        let content2 = index.load_skill("load-test").unwrap();
        assert_eq!(content, content2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unload_skill() {
        let dir = temp_skills_dir();
        create_test_skill(&dir, "general", "unload-test", "# Unload Test");

        let mut index = SkillIndex::build_from_dir(&dir);
        index.load_skill("unload-test").unwrap();
        assert!(index.is_loaded("unload-test"));

        index.unload_skill("unload-test");
        assert!(!index.is_loaded("unload-test"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_to_prompt_summary() {
        let dir = temp_skills_dir();
        create_test_skill(
            &dir,
            "devops",
            "deploy",
            "---\ndescription: Deploy apps\ntools: [exec]\n---\n\n# Deploy",
        );

        let index = SkillIndex::build_from_dir(&dir);
        let summary = index.to_prompt_summary();
        assert!(summary.contains("Available Skills"));
        assert!(summary.contains("deploy"));
        assert!(summary.contains("devops"));
        assert!(summary.contains("skill_view"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_to_prompt_summary_empty() {
        let index = SkillIndex::new();
        let summary = index.to_prompt_summary();
        assert!(summary.is_empty());
    }

    #[test]
    fn test_refresh() {
        let dir = temp_skills_dir();
        create_test_skill(&dir, "general", "skill1", "# Skill 1");

        let mut index = SkillIndex::build_from_dir(&dir);
        assert_eq!(index.entries().len(), 1);

        // 添加新 skill
        create_test_skill(&dir, "general", "skill2", "# Skill 2");
        index.refresh(&dir);
        assert_eq!(index.entries().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_nonexistent_dir() {
        let index = SkillIndex::build_from_dir(Path::new("/nonexistent/path"));
        assert!(index.entries().is_empty());
    }

    #[test]
    fn test_extract_description_from_heading() {
        let content = "# My Great Skill\n\nSome content";
        let desc = SkillIndex::extract_description(content);
        assert_eq!(desc, "My Great Skill");
    }

    #[test]
    fn test_extract_description_from_frontmatter() {
        let content = "---\ndescription: A test skill\n---\n\n# Title";
        let desc = SkillIndex::extract_description(content);
        assert_eq!(desc, "A test skill");
    }

    #[test]
    fn test_match_by_tools() {
        let dir = temp_skills_dir();
        create_test_skill(
            &dir,
            "devops",
            "deploy",
            "---\ndescription: Deploy apps\ntools: [exec, write_file]\n---\n\n# Deploy",
        );
        create_test_skill(
            &dir,
            "general",
            "hello",
            "---\ndescription: Hello\ntools: [read_file]\n---\n\n# Hello",
        );
        create_test_skill(
            &dir,
            "general",
            "no-tools",
            "---\ndescription: No tools\n---\n\n# No Tools",
        );

        let index = SkillIndex::build_from_dir(&dir);

        // 匹配 exec 工具
        let matched = index.match_by_tools(&["exec".to_string()]);
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].name, "deploy");

        // 匹配多个工具 (交集大的排前面)
        let matched = index.match_by_tools(&[
            "exec".to_string(),
            "write_file".to_string(),
            "read_file".to_string(),
        ]);
        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].name, "deploy"); // 交集=2, 排前面
        assert_eq!(matched[1].name, "hello"); // 交集=1

        // 空工具列表
        let matched = index.match_by_tools(&[]);
        assert!(matched.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fuzzy_search() {
        let dir = temp_skills_dir();
        create_test_skill(
            &dir,
            "devops",
            "flask-deploy",
            "---\ndescription: Deploy Flask apps to Kubernetes\ntools: [exec, write_file]\n---\n\n# Flask Deploy",
        );
        create_test_skill(
            &dir,
            "general",
            "rust-debug",
            "---\ndescription: Debug Rust compilation errors\ntools: [exec, read_file]\n---\n\n# Rust Debug",
        );
        create_test_skill(&dir, "general", "hello", "# Hello World");

        let index = SkillIndex::build_from_dir(&dir);

        // 按名称精确匹配
        let results = index.fuzzy_search("flask-deploy");
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "flask-deploy");

        // 按描述搜索
        let results = index.fuzzy_search("kubernetes");
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "flask-deploy");

        // 按名称模糊搜索
        let results = index.fuzzy_search("rust");
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "rust-debug");

        // 空查询
        let results = index.fuzzy_search("");
        assert!(results.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_version_and_updated_at() {
        let dir = temp_skills_dir();
        let skill_dir = dir.join("general").join("versioned-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        // 写 meta.json 带版本号
        std::fs::write(
            skill_dir.join("meta.json"),
            r#"{"description":"Versioned skill","version":"2.1.0","updated_at":"2025-01-15T10:30:00Z"}"#,
        ).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Versioned Skill").unwrap();

        let index = SkillIndex::build_from_dir(&dir);
        let entry = index.entries().get("versioned-skill").unwrap();
        assert_eq!(entry.version, "2.1.0");
        assert_eq!(entry.updated_at.as_deref(), Some("2025-01-15T10:30:00Z"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_extract_version_from_frontmatter() {
        let content = "---\nversion: 3.0.1\n---\n\n# Title";
        let version = SkillIndex::extract_version(content);
        assert_eq!(version, "3.0.1");
    }
}
