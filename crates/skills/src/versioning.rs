use crate::evolution::SkillLayout;
use blockcell_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// 技能版本信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillVersion {
    pub version: String,
    pub hash: String,
    pub created_at: i64,
    pub created_by: VersionSource,
    pub changelog: Option<String>,
    pub parent_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<SkillLayout>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

/// 版本来源
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum VersionSource {
    Manual,
    Evolution,
    Import,
}

/// 版本历史记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionHistory {
    pub skill_name: String,
    pub versions: Vec<SkillVersion>,
    pub current_version: String,
}

/// 版本管理器
pub struct VersionManager {
    skills_dir: PathBuf,
}

impl VersionManager {
    pub fn new(skills_dir: PathBuf) -> Self {
        Self { skills_dir }
    }

    /// 获取技能的版本历史
    pub fn get_history(&self, skill_name: &str) -> Result<VersionHistory> {
        let history_file = self.get_history_file_path(skill_name);

        if !history_file.exists() {
            // 如果没有历史文件，创建默认历史
            return Ok(VersionHistory {
                skill_name: skill_name.to_string(),
                versions: vec![],
                current_version: "v1".to_string(),
            });
        }

        let content = std::fs::read_to_string(&history_file)?;
        let history: VersionHistory = serde_json::from_str(&content)?;
        Ok(history)
    }

    /// 保存版本历史
    pub fn save_history(&self, history: &VersionHistory) -> Result<()> {
        let history_file = self.get_history_file_path(&history.skill_name);
        let content = serde_json::to_string_pretty(history)?;
        std::fs::write(&history_file, content)?;
        Ok(())
    }

    /// 创建新版本
    pub fn create_version(
        &self,
        skill_name: &str,
        source: VersionSource,
        changelog: Option<String>,
    ) -> Result<SkillVersion> {
        let mut history = self.get_history(skill_name)?;

        // 计算新版本号
        let version_num = history.versions.len() + 1;
        let version = format!("v{}", version_num);

        // 计算当前技能内容的 hash
        let hash = self.compute_skill_hash(skill_name)?;
        let skill_dir = self.skills_dir.join(skill_name);
        let (layout, source_path) = Self::detect_skill_layout_and_source_path(&skill_dir);

        let new_version = SkillVersion {
            version: version.clone(),
            hash,
            created_at: chrono::Utc::now().timestamp(),
            created_by: source,
            changelog,
            parent_version: Some(history.current_version.clone()),
            layout,
            source_path,
        };

        // 保存版本快照
        self.save_version_snapshot(skill_name, &new_version)?;

        // 更新历史
        history.versions.push(new_version.clone());
        history.current_version = version;
        self.save_history(&history)?;

        info!(
            skill = %skill_name,
            version = %new_version.version,
            "Created new skill version"
        );

        Ok(new_version)
    }

    /// 切换到指定版本
    pub fn switch_to_version(&self, skill_name: &str, version: &str) -> Result<()> {
        let mut history = self.get_history(skill_name)?;

        // 检查版本是否存在
        let target_version = history
            .versions
            .iter()
            .find(|v| v.version == version)
            .ok_or_else(|| Error::NotFound(format!("Version {} not found", version)))?;

        // 恢复版本快照
        self.restore_version_snapshot(skill_name, target_version)?;

        // 更新当前版本
        history.current_version = version.to_string();
        self.save_history(&history)?;

        info!(
            skill = %skill_name,
            version = %version,
            "Switched to version"
        );

        Ok(())
    }

    /// 回滚到上一个版本
    pub fn rollback(&self, skill_name: &str) -> Result<()> {
        let history = self.get_history(skill_name)?;

        if history.versions.len() < 2 {
            return Err(Error::Other(format!(
                "No previous version to rollback to for skill '{}'",
                skill_name
            )));
        }

        // 取列表中的倒数第二个版本（比 parent_version 字段更可靠，
        // 因为 parent_version 可能指向已被 cleanup_old_versions 删除的版本）
        let prev_version = &history.versions[history.versions.len() - 2];
        let prev_version_str = prev_version.version.clone();
        let current_version_str = history.current_version.clone();

        self.switch_to_version(skill_name, &prev_version_str)?;

        warn!(
            skill = %skill_name,
            from = %current_version_str,
            to = %prev_version_str,
            "Rolled back skill version"
        );

        Ok(())
    }

    /// 列出所有版本
    pub fn list_versions(&self, skill_name: &str) -> Result<Vec<SkillVersion>> {
        let history = self.get_history(skill_name)?;
        Ok(history.versions)
    }

    /// 获取当前版本
    pub fn get_current_version(&self, skill_name: &str) -> Result<String> {
        let history = self.get_history(skill_name)?;
        Ok(history.current_version)
    }

    /// 删除旧版本（保留最近 N 个）
    pub fn cleanup_old_versions(&self, skill_name: &str, keep_count: usize) -> Result<()> {
        let mut history = self.get_history(skill_name)?;

        if history.versions.len() <= keep_count {
            return Ok(());
        }

        // 保留最近的 N 个版本
        let to_remove = history.versions.len() - keep_count;
        let removed_versions: Vec<_> = history.versions.drain(..to_remove).collect();

        // 删除版本快照
        for version in &removed_versions {
            let snapshot_dir = self.get_version_snapshot_dir(skill_name, &version.version);
            if snapshot_dir.exists() {
                std::fs::remove_dir_all(&snapshot_dir)?;
                debug!(
                    skill = %skill_name,
                    version = %version.version,
                    "Removed old version snapshot"
                );
            }
        }

        self.save_history(&history)?;

        info!(
            skill = %skill_name,
            removed = removed_versions.len(),
            "Cleaned up old versions"
        );

        Ok(())
    }

    /// 比较两个版本
    pub fn diff_versions(
        &self,
        skill_name: &str,
        version1: &str,
        version2: &str,
    ) -> Result<String> {
        let snapshot1 = self.get_version_snapshot_dir(skill_name, version1);
        let snapshot2 = self.get_version_snapshot_dir(skill_name, version2);

        let content1 = Self::read_snapshot_primary_script(&snapshot1)?;
        let content2 = Self::read_snapshot_primary_script(&snapshot2)?;

        // 简单的行级 diff
        let diff = self.compute_diff(&content1, &content2);
        Ok(diff)
    }

    // === 辅助方法 ===

    fn get_history_file_path(&self, skill_name: &str) -> PathBuf {
        self.skills_dir
            .join(skill_name)
            .join("version_history.json")
    }

    fn get_version_snapshot_dir(&self, skill_name: &str, version: &str) -> PathBuf {
        self.skills_dir
            .join(skill_name)
            .join("versions")
            .join(version)
    }

    fn primary_skill_file(skill_dir: &Path) -> Option<PathBuf> {
        for filename in &["SKILL.rhai", "SKILL.py", "SKILL.md"] {
            let path = skill_dir.join(filename);
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    fn read_snapshot_primary_script(snapshot_dir: &Path) -> Result<String> {
        if let Some(source_path) = Self::snapshot_source_path(snapshot_dir) {
            let source_file = snapshot_dir.join(source_path);
            if source_file.exists() {
                return Ok(std::fs::read_to_string(source_file)?);
            }
        }

        let file_path = Self::primary_skill_file(snapshot_dir).ok_or_else(|| {
            Error::NotFound(format!(
                "No skill script found in snapshot: {}",
                snapshot_dir.display()
            ))
        })?;
        Ok(std::fs::read_to_string(file_path)?)
    }

    fn compute_skill_hash(&self, skill_name: &str) -> Result<String> {
        let skill_dir = self.skills_dir.join(skill_name);
        let Some(skill_file) = Self::skill_primary_file(&skill_dir) else {
            return Ok("empty".to_string());
        };

        let content = std::fs::read_to_string(&skill_file)?;
        let hash = format!("{:x}", md5::compute(content.as_bytes()));
        Ok(hash)
    }

    fn skill_primary_file(skill_dir: &Path) -> Option<PathBuf> {
        if let Some(source_path) = Self::detect_skill_layout_and_source_path(skill_dir).1 {
            let source_file = skill_dir.join(source_path);
            if source_file.exists() {
                return Some(source_file);
            }
        }

        Self::primary_skill_file(skill_dir)
    }

    fn detect_skill_layout_and_source_path(
        skill_dir: &Path,
    ) -> (Option<SkillLayout>, Option<String>) {
        let has_skill_md = skill_dir.join("SKILL.md").exists();

        if skill_dir.join("SKILL.rhai").exists() {
            return (
                Some(SkillLayout::RhaiOrchestration),
                Some("SKILL.rhai".to_string()),
            );
        }

        if skill_dir.join("SKILL.py").exists() {
            let layout = if has_skill_md {
                SkillLayout::Hybrid
            } else {
                SkillLayout::LocalScript
            };
            return (Some(layout), Some("SKILL.py".to_string()));
        }

        if let Some(legacy_py_path) = Self::first_legacy_python_script(skill_dir) {
            let rel = legacy_py_path
                .strip_prefix(skill_dir)
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| legacy_py_path.display().to_string());
            // Normalize path separators to Unix format for cross-platform consistency
            let rel = rel.replace('\\', "/");
            let layout = if has_skill_md {
                SkillLayout::Hybrid
            } else {
                SkillLayout::LocalScript
            };
            return (Some(layout), Some(rel));
        }

        if let Some(local_script_path) = Self::first_local_script_asset(skill_dir) {
            let rel = local_script_path
                .strip_prefix(skill_dir)
                .ok()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| local_script_path.display().to_string());
            // Normalize path separators to Unix format for cross-platform consistency
            let rel = rel.replace('\\', "/");
            let layout = if has_skill_md {
                SkillLayout::Hybrid
            } else {
                SkillLayout::LocalScript
            };
            return (Some(layout), Some(rel));
        }

        if has_skill_md {
            return (Some(SkillLayout::PromptTool), Some("SKILL.md".to_string()));
        }

        (None, None)
    }

    fn snapshot_source_path(snapshot_dir: &Path) -> Option<String> {
        let version_path = snapshot_dir.join("version.json");
        let content = std::fs::read_to_string(version_path).ok()?;
        let version: SkillVersion = serde_json::from_str(&content).ok()?;
        version.source_path
    }

    fn first_legacy_python_script(skill_dir: &Path) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        let scripts_dir = skill_dir.join("scripts");
        if scripts_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&scripts_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().is_some_and(|e| e == "py") {
                        candidates.push(path);
                    }
                }
            }
        }

        if candidates.is_empty() {
            if let Ok(entries) = std::fs::read_dir(skill_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file()
                        && path.file_name().and_then(|n| n.to_str()) != Some("SKILL.py")
                        && path.extension().is_some_and(|e| e == "py")
                    {
                        candidates.push(path);
                    }
                }
            }
        }

        candidates.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
        candidates.into_iter().next()
    }

    fn first_local_script_asset(skill_dir: &Path) -> Option<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        let allowed_extensions = ["sh", "bash", "zsh", "js", "php", "rb"];
        let scan_dir = |dir: &Path, candidates: &mut Vec<PathBuf>| {
            if !dir.is_dir() {
                return;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let ext_ok = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| allowed_extensions.contains(&ext));
                    let no_ext_exec = path.extension().is_none() && Self::looks_executable(&path);
                    if ext_ok || no_ext_exec {
                        candidates.push(path);
                    }
                }
            }
        };

        scan_dir(&skill_dir.join("scripts"), &mut candidates);
        scan_dir(&skill_dir.join("bin"), &mut candidates);

        if candidates.is_empty() {
            if let Ok(entries) = std::fs::read_dir(skill_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_file() {
                        continue;
                    }
                    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if matches!(
                        file_name,
                        "SKILL.md" | "SKILL.py" | "SKILL.rhai" | "meta.yaml" | "meta.json"
                    ) {
                        continue;
                    }
                    let ext_ok = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| allowed_extensions.contains(&ext));
                    let no_ext_exec = path.extension().is_none() && Self::looks_executable(&path);
                    if ext_ok || no_ext_exec {
                        candidates.push(path);
                    }
                }
            }
        }

        candidates.sort_by(|a, b| a.to_string_lossy().cmp(&b.to_string_lossy()));
        candidates.into_iter().next()
    }

    fn copy_dir_contents(src: &Path, dst: &Path, excluded_names: &[&str]) -> Result<()> {
        std::fs::create_dir_all(dst)?;

        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if excluded_names.iter().any(|excluded| *excluded == name_str) {
                continue;
            }

            Self::copy_path_recursive(&entry.path(), &dst.join(name))?;
        }

        Ok(())
    }

    fn copy_path_recursive(src: &Path, dst: &Path) -> Result<()> {
        if src.is_dir() {
            std::fs::create_dir_all(dst)?;
            for entry in std::fs::read_dir(src)? {
                let entry = entry?;
                Self::copy_path_recursive(&entry.path(), &dst.join(entry.file_name()))?;
            }
            return Ok(());
        }

        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst)?;
        Ok(())
    }

    fn clear_skill_asset_tree(skill_dir: &Path) -> Result<()> {
        if !skill_dir.exists() {
            std::fs::create_dir_all(skill_dir)?;
            return Ok(());
        }

        for entry in std::fs::read_dir(skill_dir)? {
            let entry = entry?;
            let file_name = entry.file_name();
            if matches!(
                file_name.to_str(),
                Some("versions") | Some("version_history.json")
            ) {
                continue;
            }

            let path = entry.path();
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
        }

        Ok(())
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

    fn save_version_snapshot(&self, skill_name: &str, version: &SkillVersion) -> Result<()> {
        let snapshot_dir = self.get_version_snapshot_dir(skill_name, &version.version);
        std::fs::create_dir_all(&snapshot_dir)?;

        let skill_dir = self.skills_dir.join(skill_name);

        Self::copy_dir_contents(
            &skill_dir,
            &snapshot_dir,
            &["versions", "version_history.json", "version.json"],
        )?;

        // 保存版本元数据
        let version_meta = serde_json::to_string_pretty(version)?;
        std::fs::write(snapshot_dir.join("version.json"), version_meta)?;

        Ok(())
    }

    fn restore_version_snapshot(&self, skill_name: &str, version: &SkillVersion) -> Result<()> {
        let snapshot_dir = self.get_version_snapshot_dir(skill_name, &version.version);

        if !snapshot_dir.exists() {
            return Err(Error::NotFound(format!(
                "Version snapshot not found: {}",
                version.version
            )));
        }

        let skill_dir = self.skills_dir.join(skill_name);

        Self::clear_skill_asset_tree(&skill_dir)?;
        Self::copy_dir_contents(&snapshot_dir, &skill_dir, &["version.json"])?;

        Ok(())
    }

    fn compute_diff(&self, content1: &str, content2: &str) -> String {
        let lines1: Vec<&str> = content1.lines().collect();
        let lines2: Vec<&str> = content2.lines().collect();

        let mut diff = String::new();
        diff.push_str("--- version 1\n");
        diff.push_str("+++ version 2\n");

        let max_len = lines1.len().max(lines2.len());
        for i in 0..max_len {
            let line1 = lines1.get(i);
            let line2 = lines2.get(i);

            match (line1, line2) {
                (Some(l1), Some(l2)) if l1 == l2 => {
                    diff.push_str(&format!("  {}\n", l1));
                }
                (Some(l1), Some(l2)) => {
                    diff.push_str(&format!("- {}\n", l1));
                    diff.push_str(&format!("+ {}\n", l2));
                }
                (Some(l1), None) => {
                    diff.push_str(&format!("- {}\n", l1));
                }
                (None, Some(l2)) => {
                    diff.push_str(&format!("+ {}\n", l2));
                }
                (None, None) => break,
            }
        }

        diff
    }

    /// 导出版本到文件
    pub fn export_version(
        &self,
        skill_name: &str,
        version: &str,
        output_path: &Path,
    ) -> Result<()> {
        let snapshot_dir = self.get_version_snapshot_dir(skill_name, version);

        if !snapshot_dir.exists() {
            return Err(Error::NotFound(format!("Version {} not found", version)));
        }

        // 创建 tar.gz 归档
        let file = std::fs::File::create(output_path)?;
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);

        tar.append_dir_all(skill_name, &snapshot_dir)?;
        tar.finish()?;

        info!(
            skill = %skill_name,
            version = %version,
            output = %output_path.display(),
            "Exported version"
        );

        Ok(())
    }

    /// 导入版本
    pub fn import_version(&self, skill_name: &str, archive_path: &Path) -> Result<SkillVersion> {
        let file = std::fs::File::open(archive_path)?;
        let dec = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(dec);

        // 解压到临时目录（使用纳秒时间戳避免并行测试冲突）
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!(
            "skill_import_{}_{}",
            std::process::id(),
            now_ns
        ));
        std::fs::create_dir_all(&temp_dir)?;
        archive.unpack(&temp_dir)?;

        // 读取版本元数据
        let version_meta_path = temp_dir.join(skill_name).join("version.json");
        let version_meta_content = std::fs::read_to_string(&version_meta_path)?;
        let mut version: SkillVersion = serde_json::from_str(&version_meta_content)?;

        if version.layout.is_none() || version.source_path.is_none() {
            let (layout, source_path) =
                Self::detect_skill_layout_and_source_path(&temp_dir.join(skill_name));
            if version.layout.is_none() {
                version.layout = layout;
            }
            if version.source_path.is_none() {
                version.source_path = source_path;
            }
        }

        // 修改版本号和来源
        let mut history = self.get_history(skill_name)?;
        let version_num = history.versions.len() + 1;
        version.version = format!("v{}", version_num);
        version.created_by = VersionSource::Import;
        version.created_at = chrono::Utc::now().timestamp();

        // 复制到版本目录
        let snapshot_dir = self.get_version_snapshot_dir(skill_name, &version.version);
        std::fs::create_dir_all(&snapshot_dir)?;

        Self::copy_dir_contents(&temp_dir.join(skill_name), &snapshot_dir, &[])?;

        // 更新历史
        history.versions.push(version.clone());
        self.save_history(&history)?;

        // 清理临时目录
        let _ = std::fs::remove_dir_all(&temp_dir);

        info!(
            skill = %skill_name,
            version = %version.version,
            "Imported version"
        );

        Ok(version)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_skills_dir(tag: &str) -> PathBuf {
        let mut root = std::env::temp_dir();
        let now_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        root.push(format!(
            "blockcell_versioning_{}_{}_{}",
            tag,
            std::process::id(),
            now_ns
        ));
        std::fs::create_dir_all(&root).expect("create temp skills dir");
        root
    }

    #[test]
    fn test_version_source() {
        let source = VersionSource::Evolution;
        assert_eq!(source, VersionSource::Evolution);
    }

    #[test]
    fn test_create_version_hash_and_snapshot_for_python_skill() {
        let skills_dir = temp_skills_dir("py_hash_snapshot");
        let skill_name = "py_skill_hash";
        let skill_dir = skills_dir.join(skill_name);
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        std::fs::write(
            skill_dir.join("SKILL.py"),
            "print('hello from python skill')\n",
        )
        .expect("write SKILL.py");

        let vm = VersionManager::new(skills_dir.clone());
        let version = vm
            .create_version(skill_name, VersionSource::Manual, None)
            .expect("create version");

        assert_ne!(version.hash, "empty");
        assert!(
            skills_dir
                .join(skill_name)
                .join("versions")
                .join("v1")
                .join("SKILL.py")
                .exists(),
            "python snapshot should exist"
        );

        let _ = std::fs::remove_dir_all(skills_dir);
    }

    #[test]
    fn test_diff_versions_supports_python_skills() {
        let skills_dir = temp_skills_dir("py_diff");
        let skill_name = "py_skill_diff";
        let skill_dir = skills_dir.join(skill_name);
        std::fs::create_dir_all(&skill_dir).expect("create skill dir");
        std::fs::write(skill_dir.join("SKILL.py"), "print('v1')\n").expect("write v1");

        let vm = VersionManager::new(skills_dir.clone());
        vm.create_version(skill_name, VersionSource::Manual, None)
            .expect("create v1");

        std::fs::write(skill_dir.join("SKILL.py"), "print('v2')\n").expect("write v2");
        vm.create_version(skill_name, VersionSource::Manual, None)
            .expect("create v2");

        let diff = vm
            .diff_versions(skill_name, "v1", "v2")
            .expect("diff versions");
        assert!(diff.contains("print('v1')"));
        assert!(diff.contains("print('v2')"));

        let _ = std::fs::remove_dir_all(skills_dir);
    }

    #[test]
    fn test_version_snapshot_restores_nested_assets_and_metadata() {
        let skills_dir = temp_skills_dir("restore_tree");
        let skill_name = "restore_skill";
        let skill_dir = skills_dir.join(skill_name);
        std::fs::create_dir_all(skill_dir.join("scripts")).expect("create scripts dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# restore skill\n").expect("write SKILL.md");
        std::fs::write(skill_dir.join("scripts/run.sh"), "#!/bin/sh\necho run\n")
            .expect("write run script");

        let vm = VersionManager::new(skills_dir.clone());
        let version = vm
            .create_version(skill_name, VersionSource::Manual, None)
            .expect("create version");

        assert_eq!(version.layout, Some(SkillLayout::Hybrid));
        assert_eq!(version.source_path.as_deref(), Some("scripts/run.sh"));
        assert!(skill_dir.join("versions/v1/scripts/run.sh").exists());

        std::fs::write(skill_dir.join("SKILL.md"), "# mutated\n").expect("mutate SKILL.md");
        std::fs::write(
            skill_dir.join("scripts/extra.sh"),
            "#!/bin/sh\necho extra\n",
        )
        .expect("write extra script");
        std::fs::create_dir_all(skill_dir.join("bin")).expect("create bin dir");
        std::fs::write(skill_dir.join("bin/helper.sh"), "#!/bin/sh\necho helper\n")
            .expect("write helper script");

        vm.switch_to_version(skill_name, "v1").expect("restore v1");

        assert_eq!(
            std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap(),
            "# restore skill\n"
        );
        assert!(skill_dir.join("scripts/run.sh").exists());
        assert!(!skill_dir.join("scripts/extra.sh").exists());
        assert!(!skill_dir.join("bin/helper.sh").exists());

        let _ = std::fs::remove_dir_all(skills_dir);
    }

    #[test]
    fn test_import_version_preserves_nested_assets() {
        let skills_dir = temp_skills_dir("import_tree");
        let skill_name = "import_skill";
        let skill_dir = skills_dir.join(skill_name);
        std::fs::create_dir_all(skill_dir.join("bin")).expect("create bin dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# import skill\n").expect("write SKILL.md");
        std::fs::write(skill_dir.join("bin/run.sh"), "#!/bin/sh\necho run\n")
            .expect("write run script");

        let vm = VersionManager::new(skills_dir.clone());
        vm.create_version(skill_name, VersionSource::Manual, None)
            .expect("create version");

        let archive_path = skills_dir.join("import_skill.tar.gz");
        vm.export_version(skill_name, "v1", &archive_path)
            .expect("export version");

        let imported = vm
            .import_version(skill_name, &archive_path)
            .expect("import version");

        assert_eq!(imported.version, "v2");
        assert!(skill_dir.join("versions/v2/bin/run.sh").exists());
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("versions/v2/bin/run.sh")).unwrap(),
            "#!/bin/sh\necho run\n"
        );

        let _ = std::fs::remove_dir_all(skills_dir);
    }

    #[test]
    fn test_version_snapshot_preserves_manual_assets() {
        let skills_dir = temp_skills_dir("manual_tree");
        let skill_name = "manual_skill";
        let skill_dir = skills_dir.join(skill_name);
        std::fs::create_dir_all(skill_dir.join("manual")).expect("create manual dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# manual skill\n").expect("write SKILL.md");
        std::fs::write(
            skill_dir.join("manual/evolution.md"),
            "## history\n- initial note\n",
        )
        .expect("write evolution manual");

        let vm = VersionManager::new(skills_dir.clone());
        vm.create_version(skill_name, VersionSource::Manual, None)
            .expect("create version");

        std::fs::write(
            skill_dir.join("manual/evolution.md"),
            "## history\n- mutated note\n",
        )
        .expect("mutate evolution manual");

        vm.switch_to_version(skill_name, "v1").expect("restore v1");
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("manual/evolution.md")).unwrap(),
            "## history\n- initial note\n"
        );

        let archive_path = skills_dir.join("manual_skill.tar.gz");
        vm.export_version(skill_name, "v1", &archive_path)
            .expect("export version");
        let imported = vm
            .import_version(skill_name, &archive_path)
            .expect("import version");

        assert_eq!(imported.version, "v2");
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("versions/v2/manual/evolution.md")).unwrap(),
            "## history\n- initial note\n"
        );

        let _ = std::fs::remove_dir_all(skills_dir);
    }

    #[test]
    fn test_version_snapshot_preserves_hybrid_python_and_scripts_assets() {
        let skills_dir = temp_skills_dir("hybrid_python_tree");
        let skill_name = "hybrid_skill";
        let skill_dir = skills_dir.join(skill_name);
        std::fs::create_dir_all(skill_dir.join("scripts")).expect("create scripts dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# hybrid skill\n").expect("write SKILL.md");
        std::fs::write(skill_dir.join("SKILL.py"), "print('hybrid')\n").expect("write SKILL.py");
        std::fs::write(
            skill_dir.join("scripts/helper.sh"),
            "#!/bin/sh\necho helper\n",
        )
        .expect("write helper script");

        let vm = VersionManager::new(skills_dir.clone());
        let version = vm
            .create_version(skill_name, VersionSource::Manual, None)
            .expect("create version");

        assert_eq!(version.layout, Some(SkillLayout::Hybrid));
        assert_eq!(version.source_path.as_deref(), Some("SKILL.py"));
        assert!(skill_dir.join("versions/v1/SKILL.py").exists());
        assert!(skill_dir.join("versions/v1/scripts/helper.sh").exists());

        std::fs::write(
            skill_dir.join("scripts/helper.sh"),
            "#!/bin/sh\necho mutated\n",
        )
        .expect("mutate helper script");
        vm.switch_to_version(skill_name, "v1").expect("restore v1");

        assert_eq!(
            std::fs::read_to_string(skill_dir.join("SKILL.py")).unwrap(),
            "print('hybrid')\n"
        );
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("scripts/helper.sh")).unwrap(),
            "#!/bin/sh\necho helper\n"
        );

        let _ = std::fs::remove_dir_all(skills_dir);
    }
}
