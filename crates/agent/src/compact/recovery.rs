//! Compact 恢复机制
//!
//! Post-Compact 阶段恢复文件和技能状态。

use crate::token::estimate_tokens;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 文件恢复状态
#[derive(Debug, Clone)]
pub struct FileRecoveryState {
    /// 文件路径
    pub path: PathBuf,
    /// 最后读取的内容摘要
    pub content_summary: String,
    /// Token 估算
    pub estimated_tokens: usize,
    /// 是否被修改
    pub was_modified: bool,
}

/// 技能恢复状态
#[derive(Debug, Clone)]
pub struct SkillRecoveryState {
    /// 技能名称
    pub name: String,
    /// 技能内容摘要
    pub content_summary: String,
    /// Token 估算
    pub estimated_tokens: usize,
}

/// Compact 恢复上下文
#[derive(Debug)]
pub struct CompactRecoveryContext {
    /// 文件恢复列表
    pub files: Vec<FileRecoveryState>,
    /// 技能恢复列表
    pub skills: Vec<SkillRecoveryState>,
    /// Session Memory 内容
    pub session_memory: Option<String>,
    /// 总恢复 Token 数
    pub total_recovery_tokens: usize,
}

impl CompactRecoveryContext {
    /// 创建空恢复上下文
    pub fn empty() -> Self {
        Self {
            files: Vec::new(),
            skills: Vec::new(),
            session_memory: None,
            total_recovery_tokens: 0,
        }
    }

    /// 添加文件恢复
    pub fn add_file(&mut self, file: FileRecoveryState) {
        self.total_recovery_tokens += file.estimated_tokens;
        self.files.push(file);
    }

    /// 添加技能恢复
    pub fn add_skill(&mut self, skill: SkillRecoveryState) {
        self.total_recovery_tokens += skill.estimated_tokens;
        self.skills.push(skill);
    }

    /// 设置 Session Memory
    pub fn set_session_memory(&mut self, content: String) {
        self.total_recovery_tokens += estimate_tokens(&content);
        self.session_memory = Some(content);
    }

    /// 检查预算是否超限
    pub fn is_within_budget(&self, max_file_tokens: usize, max_skill_tokens: usize) -> bool {
        let file_tokens: usize = self.files.iter().map(|f| f.estimated_tokens).sum();
        let skill_tokens: usize = self.skills.iter().map(|s| s.estimated_tokens).sum();

        file_tokens <= max_file_tokens && skill_tokens <= max_skill_tokens
    }

    /// 按优先级截断文件列表
    pub fn truncate_files_to_budget(&mut self, max_tokens: usize) {
        // 按重要性排序：修改过的文件优先
        self.files
            .sort_by(|a, b| b.was_modified.cmp(&a.was_modified));

        let mut used_tokens = 0;
        self.files.retain(|f| {
            if used_tokens + f.estimated_tokens <= max_tokens {
                used_tokens += f.estimated_tokens;
                true
            } else {
                false
            }
        });

        self.total_recovery_tokens = used_tokens
            + self
                .skills
                .iter()
                .map(|s| s.estimated_tokens)
                .sum::<usize>()
            + self
                .session_memory
                .as_ref()
                .map(|m| estimate_tokens(m))
                .unwrap_or(0);
    }
}

/// 创建恢复上下文
///
/// 从 FileStateCache 和 Session Memory 构建恢复信息
///
/// ## 参数
/// - `_workspace_dir`: 工作目录（预留，用于从磁盘加载状态）
/// - `_session_id`: 会话 ID（预留，用于从磁盘加载状态）
/// - `read_files`: 已读取的文件内容映射
/// - `loaded_skills`: 已加载的技能内容映射
/// - `session_memory_content`: Session Memory 内容
pub async fn create_recovery_context(
    _workspace_dir: &Path,
    _session_id: &str,
    read_files: HashMap<PathBuf, String>,
    loaded_skills: HashMap<String, String>,
    session_memory_content: Option<String>,
) -> CompactRecoveryContext {
    use super::{
        MAX_FILES_TO_RECOVER, MAX_FILE_RECOVERY_TOKENS, MAX_SINGLE_FILE_TOKENS,
        MAX_SKILL_RECOVERY_TOKENS,
    };

    let mut ctx = CompactRecoveryContext::empty();

    // 处理文件
    let mut file_states: Vec<FileRecoveryState> = read_files
        .iter()
        .map(|(path, content)| {
            // 检查文件是否被修改（简化判断：检查文件大小变化）
            let estimated_tokens = estimate_tokens(content);
            FileRecoveryState {
                path: path.clone(),
                content_summary: generate_file_summary(content, MAX_SINGLE_FILE_TOKENS),
                estimated_tokens: estimated_tokens.min(MAX_SINGLE_FILE_TOKENS),
                was_modified: false, // 需要更精确的检测
            }
        })
        .collect();

    // 按重要性排序，保留最多 MAX_FILES_TO_RECOVER 个
    file_states.sort_by(|a, b| b.estimated_tokens.cmp(&a.estimated_tokens));
    file_states.truncate(MAX_FILES_TO_RECOVER);

    for file in file_states {
        ctx.add_file(file);
    }

    // 处理技能
    for (name, content) in loaded_skills {
        let estimated_tokens = estimate_tokens(&content);
        ctx.add_skill(SkillRecoveryState {
            name,
            content_summary: generate_skill_summary(&content),
            estimated_tokens,
        });
    }

    // 处理 Session Memory
    if let Some(content) = session_memory_content {
        ctx.set_session_memory(content);
    }

    // 截断到预算
    if !ctx.is_within_budget(MAX_FILE_RECOVERY_TOKENS, MAX_SKILL_RECOVERY_TOKENS) {
        ctx.truncate_files_to_budget(MAX_FILE_RECOVERY_TOKENS);
    }

    ctx
}

/// 生成文件摘要
///
/// max_tokens: 单文件 token 上限 (设计文档: 5,000 tokens)
fn generate_file_summary(content: &str, max_tokens: usize) -> String {
    // 粗略估算: 1 token ≈ 4 字符
    let max_chars = max_tokens * 4;
    let preview = content.chars().take(max_chars).collect::<String>();
    if content.len() > max_chars {
        format!(
            "{}\n[... content truncated at {} tokens ...]",
            preview, max_tokens
        )
    } else {
        preview
    }
}

/// 生成技能摘要（前 500 字符）
fn generate_skill_summary(content: &str) -> String {
    let preview = content.chars().take(500).collect::<String>();
    if content.len() > 500 {
        format!("{}\n[... skill content truncated ...]", preview)
    } else {
        preview
    }
}

/// 生成恢复消息
///
/// 用于 Post-Compact 阶段恢复上下文
pub fn generate_recovery_message(ctx: &CompactRecoveryContext) -> String {
    let mut message = String::new();

    message.push_str("## Compact Recovery\n\n");
    message.push_str("The conversation has been compacted. Here's the recovery context:\n\n");

    // 文件恢复
    if !ctx.files.is_empty() {
        message.push_str("### Files Recently Read\n\n");
        for file in &ctx.files {
            message.push_str(&format!("**{}**\n", file.path.display()));
            message.push_str("```text\n");
            message.push_str(&file.content_summary);
            message.push_str("\n```\n\n");
        }
    }

    // 技能恢复
    if !ctx.skills.is_empty() {
        message.push_str("### Loaded Skills\n\n");
        for skill in &ctx.skills {
            message.push_str(&format!("**{}**\n", skill.name));
            message.push_str("```markdown\n");
            message.push_str(&skill.content_summary);
            message.push_str("\n```\n\n");
        }
    }

    // Session Memory 恢复
    if let Some(memory) = &ctx.session_memory {
        message.push_str("### Session Memory\n\n");
        message.push_str("```markdown\n");
        message.push_str(memory);
        message.push_str("\n```\n\n");
    }

    message.push_str(&format!(
        "*Recovery tokens used: ~{}*\n",
        ctx.total_recovery_tokens
    ));

    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recovery_context_empty() {
        let ctx = CompactRecoveryContext::empty();
        assert!(ctx.files.is_empty());
        assert!(ctx.skills.is_empty());
        assert!(ctx.session_memory.is_none());
        assert_eq!(ctx.total_recovery_tokens, 0);
    }

    #[test]
    fn test_recovery_context_add_file() {
        let mut ctx = CompactRecoveryContext::empty();
        ctx.add_file(FileRecoveryState {
            path: PathBuf::from("/test.txt"),
            content_summary: "test content".to_string(),
            estimated_tokens: 100,
            was_modified: false,
        });

        assert_eq!(ctx.files.len(), 1);
        assert_eq!(ctx.total_recovery_tokens, 100);
    }

    #[test]
    fn test_recovery_context_budget_check() {
        let mut ctx = CompactRecoveryContext::empty();
        ctx.add_file(FileRecoveryState {
            path: PathBuf::from("/test.txt"),
            content_summary: "test".to_string(),
            estimated_tokens: 60_000,
            was_modified: false,
        });

        // 超过默认文件预算 50_000
        assert!(!ctx.is_within_budget(50_000, 25_000));
    }

    #[test]
    fn test_recovery_context_truncate() {
        let mut ctx = CompactRecoveryContext::empty();

        // 添加多个文件
        for i in 0..10 {
            ctx.add_file(FileRecoveryState {
                path: PathBuf::from(format!("/file{}.txt", i)),
                content_summary: "content".to_string(),
                estimated_tokens: 10_000,
                was_modified: i < 3, // 前3个被修改
            });
        }

        // 截断到 30_000 预算
        ctx.truncate_files_to_budget(30_000);

        // 应保留约 3 个文件
        assert!(ctx.files.len() <= 3);
    }

    #[test]
    fn test_generate_file_summary() {
        // 测试长内容截断
        let long_content = "x".repeat(30_000); // 超过 5000 tokens * 4 chars
        let summary = generate_file_summary(&long_content, 5_000);
        assert!(summary.contains("[... content truncated at 5000 tokens ...]"));

        // 测试短内容不截断
        let short_content = "short";
        let summary = generate_file_summary(short_content, 5_000);
        assert_eq!(summary, "short");
    }

    #[test]
    fn test_generate_recovery_message() {
        let mut ctx = CompactRecoveryContext::empty();
        ctx.add_file(FileRecoveryState {
            path: PathBuf::from("/src/main.rs"),
            content_summary: "fn main() {}".to_string(),
            estimated_tokens: 10,
            was_modified: true,
        });
        ctx.set_session_memory("# Current State\nWorking\n".to_string());

        let msg = generate_recovery_message(&ctx);
        assert!(msg.contains("## Compact Recovery"));
        assert!(msg.contains("main.rs"));
        assert!(msg.contains("Session Memory"));
    }
}
