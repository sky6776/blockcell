use crate::service::{EvolutionService, EvolutionServiceConfig};
use crate::versioning::{VersionManager, VersionSource};
use blockcell_core::{Paths, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub requires: SkillRequires,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub always: bool,
    /// Explicit tools this skill may use when activated.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Legacy compatibility field. Older skills stored visible tools here.
    /// New skills should use `tools`.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Output format hint (e.g. "markdown", "json", "table").
    #[serde(default)]
    pub output_format: Option<String>,
    /// Fallback strategy when the skill fails.
    #[serde(default)]
    pub fallback: Option<SkillFallback>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillRequires {
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillFallback {
    /// Strategy: "degrade" (use simpler approach), "skip" (inform user), "alternative" (use another skill).
    #[serde(default = "default_fallback_strategy")]
    pub strategy: String,
    /// Message to show user on fallback.
    #[serde(default)]
    pub message: Option<String>,
    /// Alternative skill name to try.
    #[serde(default)]
    pub alternative_skill: Option<String>,
}

fn default_fallback_strategy() -> String {
    "degrade".to_string()
}

impl SkillMeta {
    pub fn effective_tools(&self) -> Vec<String> {
        if !self.tools.is_empty() {
            self.tools.clone()
        } else {
            self.capabilities.clone()
        }
    }
}

#[derive(Debug, Clone)]
struct SkillDocCache {
    root_md: String,
    linked_docs: HashMap<PathBuf, String>,
    linked_sections: HashMap<String, String>,
    prompt_bundle: String,
    planning_bundle: String,
    summary_bundle: String,
}

impl SkillDocCache {
    fn load(skill_dir: &Path) -> Result<Option<Self>> {
        let root_path = skill_dir.join("SKILL.md");
        if !root_path.exists() {
            return Ok(None);
        }

        let root_md = std::fs::read_to_string(&root_path)?;
        let sections = parse_markdown_sections(&root_md);
        let has_reserved_sections = ["shared", "prompt", "planning", "summary"]
            .iter()
            .any(|anchor| find_section(&sections, anchor).is_some());

        let mut cache = Self {
            root_md: root_md.clone(),
            linked_docs: HashMap::new(),
            linked_sections: HashMap::new(),
            prompt_bundle: String::new(),
            planning_bundle: String::new(),
            summary_bundle: String::new(),
        };

        if !has_reserved_sections {
            let fallback = root_md.trim().to_string();
            cache.prompt_bundle = fallback.clone();
            cache.planning_bundle = fallback.clone();
            cache.summary_bundle = fallback;
            return Ok(Some(cache));
        }

        let shared = cache
            .compile_root_section(skill_dir, &root_md, &sections, "shared")?
            .unwrap_or_default();
        let prompt = cache
            .compile_root_section(skill_dir, &root_md, &sections, "prompt")?
            .unwrap_or_default();
        let planning = cache
            .compile_root_section(skill_dir, &root_md, &sections, "planning")?
            .unwrap_or_default();
        let summary = cache
            .compile_root_section(skill_dir, &root_md, &sections, "summary")?
            .unwrap_or_default();

        cache.prompt_bundle = join_markdown_parts(&[shared.as_str(), prompt.as_str()]);
        cache.planning_bundle = join_markdown_parts(&[shared.as_str(), planning.as_str()]);
        cache.summary_bundle = join_markdown_parts(&[shared.as_str(), summary.as_str()]);

        Ok(Some(cache))
    }

    fn compile_root_section(
        &mut self,
        skill_dir: &Path,
        root_md: &str,
        sections: &[MarkdownSection],
        anchor: &str,
    ) -> Result<Option<String>> {
        let Some(section) = find_section(sections, anchor) else {
            return Ok(None);
        };
        let raw_section = &root_md[section.start..section.end];
        let expanded = self.expand_root_links(skill_dir, raw_section)?;
        Ok(Some(expanded.trim().to_string()))
    }

    fn expand_root_links(&mut self, skill_dir: &Path, text: &str) -> Result<String> {
        let mut output = String::new();

        for line in text.split_inclusive('\n') {
            let local_links = extract_markdown_links(line)
                .into_iter()
                .filter(|link| is_local_markdown_target(&link.target))
                .collect::<Vec<_>>();

            if local_links.is_empty() {
                output.push_str(line);
                continue;
            }

            if local_links.len() == 1 && is_link_only_line(line, &local_links[0]) {
                let included = self.resolve_link_target(skill_dir, &local_links[0].target)?;
                if !included.is_empty() {
                    output.push_str(&included);
                    if !included.ends_with('\n') {
                        output.push('\n');
                    }
                }
                continue;
            }

            let mut last = 0usize;
            for link in local_links {
                output.push_str(&line[last..link.start]);
                output.push_str(&self.resolve_link_target(skill_dir, &link.target)?);
                last = link.end;
            }
            output.push_str(&line[last..]);
        }

        Ok(output)
    }

    fn resolve_link_target(&mut self, skill_dir: &Path, target: &str) -> Result<String> {
        let Some((relative_path, section_anchor)) = split_markdown_target(target) else {
            return Ok(target.to_string());
        };
        let canonical_path = resolve_markdown_path(skill_dir, relative_path)?;
        let doc_text = if let Some(existing) = self.linked_docs.get(&canonical_path) {
            existing.clone()
        } else {
            let content = std::fs::read_to_string(&canonical_path)?;
            self.linked_docs.insert(canonical_path.clone(), content.clone());
            content
        };

        if let Some(anchor) = section_anchor {
            let index_key = format!("{}#{}", canonical_path.display(), anchor);
            if let Some(existing) = self.linked_sections.get(&index_key) {
                return Ok(existing.clone());
            }

            let extracted = extract_section_by_anchor(&doc_text, anchor).ok_or_else(|| {
                blockcell_core::Error::Skill(format!(
                    "Skill markdown section '{}' not found in {}",
                    anchor,
                    canonical_path.display()
                ))
            })?;
            self.linked_sections
                .insert(index_key, extracted.clone());
            Ok(extracted)
        } else {
            Ok(doc_text)
        }
    }
}

#[derive(Debug, Clone)]
struct MarkdownSection {
    level: usize,
    explicit_anchor: Option<String>,
    slug_anchor: String,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct MarkdownLink {
    start: usize,
    end: usize,
    target: String,
}

fn join_markdown_parts(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn parse_markdown_sections(content: &str) -> Vec<MarkdownSection> {
    let mut sections = Vec::new();
    let mut offset = 0usize;

    for line in content.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some((level, title, explicit_anchor)) = parse_heading_line(trimmed) {
            sections.push(MarkdownSection {
                level,
                explicit_anchor,
                slug_anchor: slugify_anchor(&title),
                start: offset,
                end: content.len(),
            });
        }
        offset += line.len();
    }

    for index in 0..sections.len() {
        let level = sections[index].level;
        let next_start = sections[index + 1..]
            .iter()
            .find(|candidate| candidate.level <= level)
            .map(|candidate| candidate.start)
            .unwrap_or(content.len());
        sections[index].end = next_start;
    }

    sections
}

fn parse_heading_line(line: &str) -> Option<(usize, String, Option<String>)> {
    let level = line.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 {
        return None;
    }

    let remainder = line[level..].strip_prefix(' ')?;
    let mut title = remainder.trim().to_string();
    let mut explicit_anchor = None;

    if title.ends_with('}') {
        if let Some(start) = title.rfind("{#") {
            let anchor = title[start + 2..title.len() - 1].trim();
            if !anchor.is_empty() {
                explicit_anchor = Some(anchor.to_string());
                title = title[..start].trim().to_string();
            }
        }
    }

    Some((level, title, explicit_anchor))
}

fn slugify_anchor(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_was_dash = false;

    for ch in value.chars().flat_map(|ch| ch.to_lowercase()) {
        if ch.is_alphanumeric() {
            slug.push(ch);
            previous_was_dash = false;
        } else if !previous_was_dash {
            slug.push('-');
            previous_was_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

fn find_section<'a>(sections: &'a [MarkdownSection], anchor: &str) -> Option<&'a MarkdownSection> {
    sections
        .iter()
        .find(|section| section.explicit_anchor.as_deref() == Some(anchor))
        .or_else(|| {
            let slug = slugify_anchor(anchor);
            sections
                .iter()
                .find(|section| !slug.is_empty() && section.slug_anchor == slug)
        })
}

fn extract_section_by_anchor(content: &str, anchor: &str) -> Option<String> {
    let sections = parse_markdown_sections(content);
    let section = find_section(&sections, anchor)?;
    Some(content[section.start..section.end].trim().to_string())
}

fn extract_markdown_links(line: &str) -> Vec<MarkdownLink> {
    let mut links = Vec::new();
    let mut search_from = 0usize;

    while let Some(label_start_rel) = line[search_from..].find('[') {
        let label_start = search_from + label_start_rel;
        let Some(label_end_rel) = line[label_start + 1..].find("](") else {
            break;
        };
        let label_end = label_start + 1 + label_end_rel;
        let Some(target_end_rel) = line[label_end + 2..].find(')') else {
            break;
        };
        let target_end = label_end + 2 + target_end_rel;
        let target = &line[label_end + 2..target_end];
        links.push(MarkdownLink {
            start: label_start,
            end: target_end + 1,
            target: target.to_string(),
        });
        search_from = target_end + 1;
    }

    links
}

fn strip_markdown_list_prefix(line: &str) -> &str {
    let trimmed = line.trim();
    for prefix in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest.trim();
        }
    }

    let digit_count = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
    if digit_count > 0 {
        let rest = &trimmed[digit_count..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim();
        }
    }

    trimmed
}

fn is_link_only_line(line: &str, link: &MarkdownLink) -> bool {
    strip_markdown_list_prefix(line) == line[link.start..link.end].trim()
}

fn is_local_markdown_target(target: &str) -> bool {
    split_markdown_target(target).is_some()
}

fn split_markdown_target(target: &str) -> Option<(&str, Option<&str>)> {
    if target.trim().is_empty()
        || target.starts_with('#')
        || target.contains("://")
        || target.starts_with("mailto:")
    {
        return None;
    }

    let mut parts = target.splitn(2, '#');
    let relative_path = parts.next()?.trim();
    if relative_path.is_empty() || !relative_path.ends_with(".md") {
        return None;
    }

    let anchor = parts.next().map(str::trim).filter(|value| !value.is_empty());
    Some((relative_path, anchor))
}

fn resolve_markdown_path(skill_dir: &Path, relative_path: &str) -> Result<PathBuf> {
    let candidate = Path::new(relative_path);
    if candidate.is_absolute() {
        return Err(blockcell_core::Error::Skill(format!(
            "Skill markdown link '{}' must be relative",
            relative_path
        )));
    }

    let joined = skill_dir.join(candidate);
    let canonical_skill_dir = std::fs::canonicalize(skill_dir)?;
    let canonical_target = std::fs::canonicalize(&joined).map_err(|_| {
        blockcell_core::Error::Skill(format!(
            "Skill markdown link '{}' does not exist",
            relative_path
        ))
    })?;

    if !canonical_target.starts_with(&canonical_skill_dir) {
        return Err(blockcell_core::Error::Skill(format!(
            "Skill markdown link '{}' resolves outside the skill directory",
            relative_path
        )));
    }

    Ok(canonical_target)
}

fn strip_inline_markdown(line: &str) -> String {
    let mut output = String::new();
    let mut index = 0usize;

    while index < line.len() {
        let rest = &line[index..];
        let Some(label_start_rel) = rest.find('[') else {
            output.push_str(rest);
            break;
        };
        let label_start = index + label_start_rel;
        output.push_str(&line[index..label_start]);

        let Some(label_end_rel) = line[label_start + 1..].find("](") else {
            output.push_str(&line[label_start..]);
            break;
        };
        let label_end = label_start + 1 + label_end_rel;
        let Some(target_end_rel) = line[label_end + 2..].find(')') else {
            output.push_str(&line[label_start..]);
            break;
        };
        let target_end = label_end + 2 + target_end_rel;
        output.push_str(&line[label_start + 1..label_end]);
        index = target_end + 1;
    }

    output
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}...", text[..idx].trim_end()),
        None => text.to_string(),
    }
}

fn concise_markdown_excerpt(text: &str, max_chars: usize) -> String {
    let mut excerpt = String::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !excerpt.is_empty() {
                break;
            }
            continue;
        }
        if trimmed.starts_with('#') || trimmed == "---" {
            continue;
        }

        let cleaned = strip_inline_markdown(strip_markdown_list_prefix(trimmed))
            .replace('`', "")
            .trim()
            .to_string();
        if cleaned.is_empty() {
            continue;
        }

        if !excerpt.is_empty() {
            excerpt.push(' ');
        }
        excerpt.push_str(&cleaned);

        if excerpt.chars().count() >= max_chars {
            break;
        }
    }

    truncate_chars(excerpt.trim(), max_chars)
}

fn skill_supports_local_exec(skill: &Skill, manual: &str) -> bool {
    if skill.path.join("SKILL.py").exists() || skill.path.join("SKILL.rhai").exists() {
        return true;
    }

    if skill_dir_contains_local_script(&skill.path) {
        return true;
    }

    let manual_lower = manual.to_lowercase();
    [
        "exec_local",
        "本地脚本",
        "local script",
        "scripts/",
        ".py",
        ".sh",
        ".php",
        ".js",
        "python ",
        "bash ",
        "sh ",
        "./",
    ]
        .iter()
        .any(|needle| manual_lower.contains(needle))
}

fn skill_dir_contains_local_script(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if skill_dir_contains_local_script(&path) {
                return true;
            }
            continue;
        }

        if path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "py" | "sh" | "php" | "js" | "ts" | "rb"))
        {
            return true;
        }
    }

    false
}

fn skill_local_exec_entrypoints(skill: &Skill) -> Vec<String> {
    let mut entrypoints = Vec::new();

    for candidate in ["SKILL.py", "SKILL.rhai"] {
        if skill.path.join(candidate).exists() {
            entrypoints.push(candidate.to_string());
        }
    }

    for directory in ["scripts", "bin"] {
        if skill_dir_contains_local_script(&skill.path.join(directory)) {
            entrypoints.push(format!("{}/", directory));
        }
    }

    let Ok(entries) = std::fs::read_dir(&skill.path) else {
        entrypoints.sort();
        entrypoints.dedup();
        return entrypoints;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let file_name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
        if matches!(
            file_name,
            "SKILL.md" | "SKILL.py" | "SKILL.rhai" | "meta.yaml" | "meta.json" | "CHANGELOG.md"
        ) {
            continue;
        }

        let ext_ok = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| matches!(ext, "py" | "sh" | "php" | "js" | "ts" | "rb"));
        let no_ext_exec = path.extension().is_none() && looks_executable(&path);
        if ext_ok || no_ext_exec {
            if let Ok(rel_path) = path.strip_prefix(&skill.path) {
                entrypoints.push(rel_path.display().to_string());
            }
        }
    }

    entrypoints.sort();
    entrypoints.dedup();
    entrypoints
}

fn skill_execution_layout(skill: &Skill, manual: &str) -> String {
    let has_rhai = skill.path.join("SKILL.rhai").exists();
    let supports_local_exec = skill_supports_local_exec(skill, manual);

    match (has_rhai, supports_local_exec) {
        (true, true) => "RhaiOrchestration + LocalScript".to_string(),
        (true, false) => "RhaiOrchestration".to_string(),
        (false, true) => "PromptTool + LocalScript".to_string(),
        (false, false) => "PromptTool".to_string(),
    }
}

#[cfg(unix)]
fn looks_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn looks_executable(_path: &Path) -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkillCard {
    pub name: String,
    pub description: String,
    pub execution_layout: String,
    pub when_to_use: String,
    pub outputs: String,
    pub allowed_tools: Vec<String>,
    pub local_exec_entrypoints: Vec<String>,
    pub supports_local_exec: bool,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub meta: SkillMeta,
    pub available: bool,
    pub unavailable_reason: Option<String>,
    pub current_version: Option<String>,
    /// Root SKILL.md and compiled phase bundles cached at load time.
    cached_docs: Option<SkillDocCache>,
}

impl Skill {
    /// Check if this skill has a SKILL.rhai orchestration script.
    pub fn has_rhai(&self) -> bool {
        self.path.join("SKILL.rhai").exists()
    }

    /// Check if this skill has a SKILL.md prompt file.
    pub fn has_md(&self) -> bool {
        self.path.join("SKILL.md").exists()
    }

    /// Return the SKILL.md content, using the in-memory cache populated at load time.
    pub fn load_md(&self) -> Option<String> {
        if let Some(cached_docs) = &self.cached_docs {
            return Some(cached_docs.root_md.clone());
        }
        // Fallback: read from disk (e.g. if skill was constructed outside SkillManager)
        let md_path = self.path.join("SKILL.md");
        std::fs::read_to_string(md_path).ok()
    }

    pub fn load_prompt_bundle(&self) -> Option<String> {
        self.cached_docs
            .as_ref()
            .map(|cached_docs| cached_docs.prompt_bundle.clone())
            .or_else(|| self.load_md())
    }

    pub fn load_planning_bundle(&self) -> Option<String> {
        self.cached_docs
            .as_ref()
            .map(|cached_docs| cached_docs.planning_bundle.clone())
            .or_else(|| self.load_md())
    }

    pub fn load_summary_bundle(&self) -> Option<String> {
        self.cached_docs
            .as_ref()
            .map(|cached_docs| cached_docs.summary_bundle.clone())
            .or_else(|| self.load_md())
    }

    /// Load the SKILL.rhai script content.
    pub fn load_rhai(&self) -> Option<String> {
        let rhai_path = self.path.join("SKILL.rhai");
        std::fs::read_to_string(rhai_path).ok()
    }

    /// Get the tests directory path.
    pub fn tests_dir(&self) -> PathBuf {
        self.path.join("tests")
    }

    /// Load test fixtures from the tests/ directory.
    pub fn load_test_fixtures(&self) -> Vec<SkillTestFixture> {
        let tests_dir = self.tests_dir();
        if !tests_dir.exists() {
            return vec![];
        }
        let mut fixtures = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&tests_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "json") {
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        if let Ok(fixture) = serde_json::from_str::<SkillTestFixture>(&content) {
                            fixtures.push(fixture);
                        }
                    }
                }
            }
        }
        fixtures
    }
}

/// A test fixture for shadow testing a skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillTestFixture {
    /// Test case name.
    pub name: String,
    /// Simulated user input.
    pub input: String,
    /// Expected output (substring match or JSON schema).
    #[serde(default)]
    pub expected_output: Option<String>,
    /// Expected tool calls (in order).
    #[serde(default)]
    pub expected_tools: Vec<String>,
    /// Context variables to inject into Rhai scope.
    #[serde(default)]
    pub context: serde_json::Value,
    /// Optional tool parameters used by some fixtures.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Optional constraint hints used by some fixtures.
    #[serde(default)]
    pub constraints: serde_json::Value,
}

pub struct SkillManager {
    skills: HashMap<String, Skill>,
    version_manager: Option<VersionManager>,
    evolution_service: Option<EvolutionService>,
    /// Known available capability IDs (synced from CapabilityRegistry)
    available_capabilities: std::collections::HashSet<String>,
}

impl SkillManager {
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
            version_manager: None,
            evolution_service: None,
            available_capabilities: std::collections::HashSet::new(),
        }
    }

    /// Sync available capability IDs from the CapabilityRegistry.
    /// Called periodically from the runtime tick to keep skill availability up to date.
    pub fn sync_capabilities(&mut self, capability_ids: Vec<String>) {
        self.available_capabilities = capability_ids.into_iter().collect();
    }

    /// Get the list of missing capabilities across all skills.
    /// Returns (skill_name, missing_capability_id) pairs.
    /// Filters out capability IDs that match built-in tool names — those are
    /// already available as tools and should not trigger capability evolution.
    pub fn get_missing_capabilities(&self) -> Vec<(String, String)> {
        let mut missing = Vec::new();
        for skill in self.skills.values() {
            for tool_name in skill.meta.effective_tools() {
                if !self.available_capabilities.contains(&tool_name)
                    && !crate::service::is_builtin_tool(&tool_name)
                    && !tool_name.contains("__")
                {
                    missing.push((skill.name.clone(), tool_name));
                }
            }
        }
        missing
    }

    pub fn with_versioning(mut self, skills_dir: PathBuf) -> Self {
        self.version_manager = Some(VersionManager::new(skills_dir));
        self
    }

    pub fn with_evolution(mut self, skills_dir: PathBuf, config: EvolutionServiceConfig) -> Self {
        self.evolution_service = Some(EvolutionService::new(skills_dir, config));
        self
    }

    pub fn evolution_service(&self) -> Option<&EvolutionService> {
        self.evolution_service.as_ref()
    }

    pub fn evolution_service_mut(&mut self) -> Option<&mut EvolutionService> {
        self.evolution_service.as_mut()
    }

    pub fn load_from_paths(&mut self, paths: &Paths) -> Result<()> {
        // Load built-in skills first (lower priority)
        let builtin_dir = paths.builtin_skills_dir();
        if builtin_dir.exists() {
            debug!(path = %builtin_dir.display(), "Loading built-in skills");
            self.scan_directory_with_priority(&builtin_dir, false)?;
        }

        // Load workspace skills (higher priority, can override built-in)
        let workspace_dir = paths.skills_dir();
        if workspace_dir.exists() {
            debug!(path = %workspace_dir.display(), "Loading workspace skills");
            self.scan_directory_with_priority(&workspace_dir, true)?;
        }

        Ok(())
    }

    /// Re-scan skill directories and pick up any newly created or modified skills.
    /// Returns the names of newly discovered skills (not previously loaded).
    pub fn reload_skills(&mut self, paths: &Paths) -> Result<Vec<String>> {
        let before: std::collections::HashSet<String> = self.skills.keys().cloned().collect();
        self.load_from_paths(paths)?;
        let new_skills: Vec<String> = self
            .skills
            .keys()
            .filter(|k| !before.contains(*k))
            .cloned()
            .collect();
        if !new_skills.is_empty() {
            info!(new_skills = ?new_skills, "Hot-reloaded new skills");
        }
        Ok(new_skills)
    }

    fn scan_directory_with_priority(&mut self, dir: &PathBuf, is_workspace: bool) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.is_dir() {
                if let Some(skill) = self.load_skill(&path)? {
                    let skill_name = skill.name.clone();

                    // Workspace skills override built-in skills
                    if is_workspace || !self.skills.contains_key(&skill_name) {
                        let source = if is_workspace {
                            "workspace"
                        } else {
                            "built-in"
                        };
                        debug!(
                            name = %skill_name,
                            available = skill.available,
                            source = source,
                            "Loaded skill"
                        );
                        self.skills.insert(skill_name, skill);
                    }
                }
            }
        }

        Ok(())
    }

    fn load_skill(&self, skill_dir: &std::path::Path) -> Result<Option<Skill>> {
        let name = skill_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        // Try to load meta from meta.yaml or meta.json
        let meta = self.load_meta(skill_dir)?;

        // Check availability
        let (available, reason) = self.check_availability(&meta);

        // 获取当前版本
        let current_version = if let Some(vm) = &self.version_manager {
            vm.get_current_version(&name).ok()
        } else {
            None
        };

        let cached_docs = SkillDocCache::load(skill_dir)?;
        Ok(Some(Skill {
            name: if meta.name.is_empty() {
                name
            } else {
                meta.name.clone()
            },
            path: skill_dir.to_path_buf(),
            meta,
            available,
            unavailable_reason: reason,
            current_version,
            cached_docs,
        }))
    }

    fn load_meta(&self, skill_dir: &std::path::Path) -> Result<SkillMeta> {
        // Try meta.yaml first
        let yaml_path = skill_dir.join("meta.yaml");
        if yaml_path.exists() {
            let content = std::fs::read_to_string(&yaml_path)?;
            return Ok(serde_yaml::from_str(&content)?);
        }

        // Try meta.json
        let json_path = skill_dir.join("meta.json");
        if json_path.exists() {
            let content = std::fs::read_to_string(&json_path)?;
            return Ok(serde_json::from_str(&content)?);
        }

        // Return default meta
        Ok(SkillMeta::default())
    }

    fn check_availability(&self, meta: &SkillMeta) -> (bool, Option<String>) {
        // Check required binaries
        for bin in &meta.requires.bins {
            if which::which(bin).is_err() {
                return (false, Some(format!("Missing binary: {}", bin)));
            }
        }

        // Check required environment variables
        for env_var in &meta.requires.env {
            if std::env::var(env_var).is_err() {
                return (false, Some(format!("Missing env var: {}", env_var)));
            }
        }

        // Check required tools / legacy capabilities from the registry.
        // Built-in tool ids are always available here; evolved capabilities must exist.
        for tool_name in meta.effective_tools() {
            if tool_name.contains("__") {
                continue;
            }
            if !crate::service::is_builtin_tool(&tool_name)
                && !self.available_capabilities.contains(&tool_name)
            {
                return (false, Some(format!("Missing capability: {}", tool_name)));
            }
        }

        (true, None)
    }

    pub fn get_summary_xml(&self) -> String {
        let mut xml = String::from("<skills>\n");

        for skill in self.skills.values() {
            xml.push_str(&format!("  <skill available=\"{}\">\n", skill.available));
            xml.push_str(&format!("    <name>{}</name>\n", skill.name));
            xml.push_str(&format!(
                "    <description>{}</description>\n",
                skill.meta.description
            ));
            xml.push_str(&format!(
                "    <location>{}/SKILL.md</location>\n",
                skill.path.display()
            ));

            if !skill.available {
                if let Some(reason) = &skill.unavailable_reason {
                    xml.push_str(&format!("    <requires>{}</requires>\n", reason));
                }
            }

            xml.push_str("  </skill>\n");
        }

        xml.push_str("</skills>");
        xml
    }

    pub fn get_always_skills(&self) -> Vec<&Skill> {
        self.skills
            .values()
            .filter(|s| s.meta.always && s.available)
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn build_skill_card(skill: &Skill) -> SkillCard {
        let cached_docs = skill
            .cached_docs
            .clone()
            .or_else(|| SkillDocCache::load(&skill.path).ok().flatten());
        let manual = cached_docs
            .as_ref()
            .map(|docs| docs.root_md.clone())
            .or_else(|| skill.load_md())
            .unwrap_or_default();
        let prompt_bundle = cached_docs
            .as_ref()
            .map(|docs| docs.prompt_bundle.clone())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| manual.clone());
        let summary_bundle = cached_docs
            .as_ref()
            .map(|docs| docs.summary_bundle.clone())
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| manual.clone());
        let shared_section = extract_section_by_anchor(&manual, "shared").unwrap_or_default();
        let prompt_section = extract_section_by_anchor(&manual, "prompt").unwrap_or_default();
        let summary_section = extract_section_by_anchor(&manual, "summary").unwrap_or_default();
        let when_to_use_source = join_markdown_parts(&[shared_section.as_str(), prompt_section.as_str()]);
        let when_to_use = concise_markdown_excerpt(
            if when_to_use_source.trim().is_empty() {
                &prompt_bundle
            } else {
                &when_to_use_source
            },
            220,
        );
        let outputs = concise_markdown_excerpt(
            if summary_section.trim().is_empty() {
                &summary_bundle
            } else {
                &summary_section
            },
            220,
        );

        SkillCard {
            name: skill.name.clone(),
            description: skill.meta.description.clone(),
            execution_layout: skill_execution_layout(skill, &manual),
            when_to_use: if when_to_use.is_empty() {
                concise_markdown_excerpt(&manual, 220)
            } else {
                when_to_use
            },
            outputs: if outputs.is_empty() {
                skill.meta
                    .output_format
                    .clone()
                    .unwrap_or_else(|| "Answer the user's request directly.".to_string())
            } else {
                outputs
            },
            allowed_tools: skill.meta.effective_tools(),
            local_exec_entrypoints: skill_local_exec_entrypoints(skill),
            supports_local_exec: skill_supports_local_exec(skill, &manual),
        }
    }

    pub fn list_enabled_skills<'a>(
        &'a self,
        disabled_skills: &HashSet<String>,
    ) -> Vec<&'a Skill> {
        self.skills
            .values()
            .filter(|s| s.available && !disabled_skills.contains(&s.name))
            .collect()
    }

    pub fn list_enabled_skill_cards(&self, disabled_skills: &HashSet<String>) -> Vec<SkillCard> {
        self.list_enabled_skills(disabled_skills)
            .into_iter()
            .map(Self::build_skill_card)
            .collect()
    }

    /// List all available skills.
    pub fn list_available(&self) -> Vec<&Skill> {
        self.skills.values().filter(|s| s.available).collect()
    }

    // === 版本管理方法 ===

    /// 创建技能的新版本
    pub fn create_version(
        &self,
        skill_name: &str,
        source: VersionSource,
        changelog: Option<String>,
    ) -> Result<()> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.create_version(skill_name, source, changelog)?;
        info!(skill = %skill_name, "Created new skill version");
        Ok(())
    }

    /// 切换到指定版本
    pub fn switch_version(&self, skill_name: &str, version: &str) -> Result<()> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.switch_to_version(skill_name, version)?;
        Ok(())
    }

    /// 回滚到上一个版本
    pub fn rollback_version(&self, skill_name: &str) -> Result<()> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.rollback(skill_name)?;
        Ok(())
    }

    /// 列出技能的所有版本
    pub fn list_versions(&self, skill_name: &str) -> Result<Vec<crate::versioning::SkillVersion>> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.list_versions(skill_name)
    }

    /// 清理旧版本
    pub fn cleanup_old_versions(&self, skill_name: &str, keep_count: usize) -> Result<()> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.cleanup_old_versions(skill_name, keep_count)?;
        Ok(())
    }

    /// 比较两个版本
    pub fn diff_versions(
        &self,
        skill_name: &str,
        version1: &str,
        version2: &str,
    ) -> Result<String> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.diff_versions(skill_name, version1, version2)
    }

    /// 导出版本
    pub fn export_version(
        &self,
        skill_name: &str,
        version: &str,
        output_path: &std::path::Path,
    ) -> Result<()> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.export_version(skill_name, version, output_path)?;
        Ok(())
    }

    /// 导入版本
    pub fn import_version(&self, skill_name: &str, archive_path: &std::path::Path) -> Result<()> {
        let vm = self.version_manager.as_ref().ok_or_else(|| {
            blockcell_core::Error::Other("Version manager not initialized".to_string())
        })?;

        vm.import_version(skill_name, archive_path)?;
        Ok(())
    }
}

impl Default for SkillManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_skill_dir(prefix: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{}-{}-{}", prefix, std::process::id(), nonce));
        fs::create_dir_all(&dir).expect("create temp skill dir");
        dir
    }

    #[test]
    fn test_skill_meta_prefers_tools_over_capabilities() {
        let meta: SkillMeta = serde_yaml::from_str(
            r#"
name: stock_analysis
tools:
  - finance_api
  - chart_generate
capabilities:
  - web_fetch
"#,
        )
        .expect("meta should parse");

        assert_eq!(
            meta.effective_tools(),
            vec!["finance_api".to_string(), "chart_generate".to_string()]
        );
    }

    #[test]
    fn test_skill_meta_falls_back_to_capabilities_when_tools_missing() {
        let meta: SkillMeta = serde_yaml::from_str(
            r#"
name: weather
capabilities:
  - web_fetch
  - web_search
"#,
        )
        .expect("legacy meta should parse");

        assert_eq!(
            meta.effective_tools(),
            vec!["web_fetch".to_string(), "web_search".to_string()]
        );
    }

    #[test]
    fn test_skill_manager_lists_enabled_skills_without_free_text_matching() {
        let mut manager = SkillManager::new();
        manager.skills = HashMap::from([
            (
                "disabled_skill".to_string(),
                Skill {
                    name: "disabled_skill".to_string(),
                    path: PathBuf::from("/tmp/disabled_skill"),
                    meta: SkillMeta {
                        name: "disabled_skill".to_string(),
                        ..SkillMeta::default()
                    },
                    available: true,
                    unavailable_reason: None,
                    current_version: None,
                    cached_docs: None,
                },
            ),
            (
                "active_skill".to_string(),
                Skill {
                    name: "active_skill".to_string(),
                    path: PathBuf::from("/tmp/active_skill"),
                    meta: SkillMeta {
                        name: "active_skill".to_string(),
                        ..SkillMeta::default()
                    },
                    available: true,
                    unavailable_reason: None,
                    current_version: None,
                    cached_docs: None,
                },
            ),
        ]);

        let disabled_skills = HashSet::from(["disabled_skill".to_string()]);
        let listed = manager.list_enabled_skills(&disabled_skills);

        assert_eq!(
            listed
                .into_iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["active_skill"]
        );
    }

    #[test]
    fn test_skill_meta_serializes_without_legacy_fields() {
        let meta: SkillMeta = serde_yaml::from_str(
            r#"
name: demo
description: minimal
permissions:
  - network
requires:
  bins: ["python3"]
fallback:
  strategy: degrade
  message: failed
"#,
        )
        .expect("meta should parse");

        assert_eq!(meta.name, "demo");
        assert_eq!(meta.description, "minimal");
        assert_eq!(meta.permissions, vec!["network"]);
        assert_eq!(meta.requires.bins, vec!["python3"]);
        assert_eq!(
            meta.fallback
                .as_ref()
                .and_then(|fallback| fallback.message.as_deref()),
            Some("failed")
        );

        let value = serde_json::to_value(&meta).expect("serialize skill meta");
        let object = value
            .as_object()
            .expect("skill meta should serialize to an object");
        let keys = object.keys().cloned().collect::<std::collections::HashSet<_>>();
        let expected = std::collections::HashSet::from([
            "name".to_string(),
            "description".to_string(),
            "requires".to_string(),
            "permissions".to_string(),
            "always".to_string(),
            "tools".to_string(),
            "capabilities".to_string(),
            "output_format".to_string(),
            "fallback".to_string(),
        ]);
        assert_eq!(keys, expected);
    }

    #[test]
    fn test_skill_doc_bundles_expand_root_markdown_links_in_order() {
        let skill_dir = temp_skill_dir("blockcell-skill-doc-bundles");
        fs::create_dir_all(skill_dir.join("manual")).expect("create manual dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Demo Skill

## Shared {#shared}
Shared preface.
- [Shared rules](manual/shared.md#rules)

## Prompt {#prompt}
Prompt rules.

## Planning {#planning}
Planning intro.
- [Build argv](manual/planning.md#build-argv)
Planning tail.

## Summary {#summary}
Summary intro.
- [Final answer](manual/summary.md#final-answer)
"#,
        )
        .expect("write root skill md");
        fs::write(
            skill_dir.join("manual/shared.md"),
            r#"## Shared rules {#rules}
Use cached auth token.
- [Do not expand](nested.md#ignored)
"#,
        )
        .expect("write shared child md");
        fs::write(
            skill_dir.join("manual/planning.md"),
            r#"## Build argv {#build-argv}
argv[0] must be the action.

### Flags
Keep deterministic ordering.

## Other {#other}
ignore me
"#,
        )
        .expect("write planning child md");
        fs::write(
            skill_dir.join("manual/summary.md"),
            r#"## Final answer {#final-answer}
Summarize in Chinese with concise bullets.
"#,
        )
        .expect("write summary child md");

        let manager = SkillManager::new();
        let skill = manager
            .load_skill(&skill_dir)
            .expect("load skill result")
            .expect("skill should load");

        let prompt_bundle = skill
            .load_prompt_bundle()
            .expect("prompt bundle should be cached");
        let planning_bundle = skill
            .load_planning_bundle()
            .expect("planning bundle should be cached");
        let summary_bundle = skill
            .load_summary_bundle()
            .expect("summary bundle should be cached");

        assert!(prompt_bundle.contains("Shared preface."));
        assert!(prompt_bundle.contains("Use cached auth token."));
        assert!(prompt_bundle.contains("Prompt rules."));
        assert!(!prompt_bundle.contains("argv[0] must be the action."));

        let shared_index = planning_bundle.find("Shared preface.").expect("shared in planning");
        let child_index = planning_bundle
            .find("argv[0] must be the action.")
            .expect("planning child in bundle");
        let tail_index = planning_bundle
            .find("Planning tail.")
            .expect("planning tail in bundle");
        assert!(shared_index < child_index);
        assert!(child_index < tail_index);
        assert!(planning_bundle.contains("Keep deterministic ordering."));
        assert!(planning_bundle.contains("[Do not expand](nested.md#ignored)"));
        assert!(!planning_bundle.contains("ignore me"));

        assert!(summary_bundle.contains("Summary intro."));
        assert!(summary_bundle.contains("Summarize in Chinese with concise bullets."));
        assert!(!summary_bundle.contains("Planning intro."));
    }

    #[test]
    fn test_skill_doc_bundles_reject_parent_path_escape() {
        let skill_dir = temp_skill_dir("blockcell-skill-doc-escape");
        let outside_file = skill_dir
            .parent()
            .expect("temp skill dir should have parent")
            .join("secret.md");
        fs::write(&outside_file, "## Hack {#hack}\nsecret\n").expect("write outside file");
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Escape Demo

## Planning {#planning}
- [Oops](../secret.md#hack)
"#,
        )
        .expect("write root skill md");

        let manager = SkillManager::new();
        let err = manager
            .load_skill(&skill_dir)
            .expect_err("parent escape should fail");

        assert!(format!("{}", err).contains("outside"));
    }

    #[test]
    fn test_skill_card_is_built_from_meta_and_skill_doc() {
        let skill_dir = temp_skill_dir("blockcell-skill-card");
        fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: weather
description: 查询天气
tools:
  - web_fetch
"#,
        )
        .expect("write meta");
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Weather

## Shared {#shared}
适合查询城市天气、未来天气和天气趋势。

## Prompt {#prompt}
先确认用户想查哪个城市、哪一天。

## Summary {#summary}
输出天气概览、温度和降雨提示。
"#,
        )
        .expect("write skill md");
        fs::write(skill_dir.join("weather.py"), "print('ok')").expect("write local script");

        let skill = Skill {
            name: "weather".to_string(),
            path: skill_dir,
            meta: SkillMeta {
                name: "weather".to_string(),
                description: "查询天气".to_string(),
                tools: vec!["web_fetch".to_string()],
                ..SkillMeta::default()
            },
            available: true,
            unavailable_reason: None,
            current_version: None,
            cached_docs: None,
        };

        let card = SkillManager::build_skill_card(&skill);
        assert_eq!(card.name, "weather");
        assert_eq!(card.description, "查询天气");
        assert_eq!(card.execution_layout, "PromptTool + LocalScript");
        assert!(card.when_to_use.contains("适合查询城市天气"));
        assert!(card.outputs.contains("输出天气概览"));
        assert_eq!(card.allowed_tools, vec!["web_fetch"]);
        assert!(card.local_exec_entrypoints.iter().any(|entry| entry == "weather.py"));
        assert!(card.supports_local_exec);
    }

    #[test]
    fn test_skill_cards_are_short_and_do_not_inline_full_skill_md() {
        let skill_dir = temp_skill_dir("blockcell-skill-card-short");
        fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: ppt
description: 生成 PPT 页面
"#,
        )
        .expect("write meta");
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "# PPT\n\n## Shared {{#shared}}\n适合把信息整理成演示页面。\n\n## Prompt {{#prompt}}\n{}\n",
                "VERY_DETAILED_INTERNAL_STEP ".repeat(80)
            ),
        )
        .expect("write skill md");

        let mut manager = SkillManager::new();
        manager.skills = HashMap::from([(
            "ppt".to_string(),
            Skill {
                name: "ppt".to_string(),
                path: skill_dir,
                meta: SkillMeta {
                    name: "ppt".to_string(),
                    description: "生成 PPT 页面".to_string(),
                    ..SkillMeta::default()
                },
                available: true,
                unavailable_reason: None,
                current_version: None,
                cached_docs: None,
            },
        )]);

        let cards = manager.list_enabled_skill_cards(&HashSet::new());
        assert_eq!(cards.len(), 1);
        assert!(cards[0].when_to_use.len() <= 240);
        assert!(!cards[0]
            .when_to_use
            .contains("VERY_DETAILED_INTERNAL_STEP VERY_DETAILED_INTERNAL_STEP VERY_DETAILED_INTERNAL_STEP VERY_DETAILED_INTERNAL_STEP VERY_DETAILED_INTERNAL_STEP"));
    }

    #[test]
    fn test_skill_card_detects_exec_local_from_manual_and_nested_script() {
        let skill_dir = temp_skill_dir("blockcell-skill-card-exec-local");
        fs::create_dir_all(skill_dir.join("scripts")).expect("create scripts dir");
        fs::write(
            skill_dir.join("meta.yaml"),
            r#"
name: local_demo
description: 本地脚本 demo
"#,
        )
        .expect("write meta");
        fs::write(
            skill_dir.join("SKILL.md"),
            r#"# Local Demo

## Shared {#shared}
适合执行 skill 目录内的本地脚本。

## Prompt {#prompt}
如果要运行本地脚本，使用 exec_local。
"#,
        )
        .expect("write skill md");
        fs::write(skill_dir.join("scripts/hello.sh"), "#!/bin/sh\necho ok\n")
            .expect("write script");

        let skill = Skill {
            name: "local_demo".to_string(),
            path: skill_dir,
            meta: SkillMeta {
                name: "local_demo".to_string(),
                description: "本地脚本 demo".to_string(),
                ..SkillMeta::default()
            },
            available: true,
            unavailable_reason: None,
            current_version: None,
            cached_docs: None,
        };

        let card = SkillManager::build_skill_card(&skill);
        assert!(card.supports_local_exec);
        assert_eq!(card.execution_layout, "PromptTool + LocalScript");
        assert!(card
            .local_exec_entrypoints
            .iter()
            .any(|entry| entry == "scripts/"));
    }
}
