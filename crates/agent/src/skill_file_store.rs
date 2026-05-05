use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use blockcell_core::{Error, Paths, Result};
use blockcell_tools::SkillFileStoreOps;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::unified_security_scanner::scan_learned_skill_content;
use crate::write_guard::{WriteGuard, WriteGuardError, WriteGuardRAII, WriteTarget};

const SKILL_MD_CHAR_LIMIT: usize = 64_000;
const AUX_FILE_CHAR_LIMIT: usize = 128_000;
const SKILLS_PROMPT_SNAPSHOT_FILE: &str = ".skills_prompt_snapshot.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillFileMutation {
    pub skill_name: String,
    pub action: String,
    pub path: PathBuf,
    pub snapshot_ref: Option<String>,
    pub message: String,
}

#[derive(Debug)]
pub struct SkillFileStore {
    skills_dir: PathBuf,
    snapshots_dir: PathBuf,
    toggles_file: PathBuf,
    lock_path: PathBuf,
    /// Unified write guard for coordinated write protection across memory + skill files
    write_guard: Option<Arc<WriteGuard>>,
    write_lock: Mutex<()>,
}

impl SkillFileStore {
    pub fn open(paths: &Paths) -> Result<Self> {
        let skills_dir = paths.skills_dir();
        let snapshots_dir = skills_dir.join(".snapshots");
        fs::create_dir_all(&skills_dir)?;
        fs::create_dir_all(&snapshots_dir)?;
        Ok(Self {
            skills_dir,
            snapshots_dir,
            toggles_file: paths.toggles_file(),
            lock_path: paths.skills_dir().join(".skill_file_store.lockdir"),
            write_guard: None,
            write_lock: Mutex::new(()),
        })
    }

    /// Set the unified write guard for coordinated write protection
    pub fn set_write_guard(&mut self, guard: Arc<WriteGuard>) {
        self.write_guard = Some(guard);
    }

    /// Acquire the unified write guard for the given skill, if configured.
    /// Returns Ok(RAII guard) on success, Err if the target is already being written.
    /// If no write_guard is configured, returns Ok(None) (backward compat).
    fn acquire_write_guard(&self, skill_name: &str) -> Result<Option<WriteGuardRAII>> {
        let Some(ref guard) = self.write_guard else {
            return Ok(None);
        };
        let write_target = WriteTarget::Skill {
            category: String::new(),
            name: skill_name.to_string(),
        };
        guard
            .acquire(write_target)
            .map(Some)
            .map_err(|WriteGuardError { target }| {
                Error::Other(format!("concurrent write in progress for {target}"))
            })
    }

    pub fn view(&self, name: &str) -> Result<Value> {
        let skill_name = validate_skill_name(name)?;
        let skill_dir = self.skill_dir(&skill_name);
        let skill_md = skill_dir.join("SKILL.md");
        if !skill_md.exists() {
            return Err(Error::NotFound(format!("skill not found: {}", skill_name)));
        }
        let meta_yaml = skill_dir.join("meta.yaml");
        Ok(json!({
            "success": true,
            "name": skill_name,
            "dir": skill_dir.to_string_lossy(),
            "skillMd": skill_md.to_string_lossy(),
            "content": fs::read_to_string(&skill_md)?,
            "metaYaml": if meta_yaml.exists() { Some(fs::read_to_string(meta_yaml)?) } else { None },
            "files": list_skill_files(&skill_dir)?,
        }))
    }

    pub fn create(
        &self,
        name: &str,
        description: &str,
        content: &str,
    ) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let description = normalize_description(description)?;
        let body = normalize_skill_body(content)?;
        scan_learned_skill_content(&description)?;
        scan_learned_skill_content(&body)?;
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let skill_dir = self.skill_dir(&skill_name);
        if skill_dir.exists() {
            return Err(Error::Validation(format!(
                "skill already exists: {}",
                skill_name
            )));
        }
        fs::create_dir_all(&skill_dir)?;
        let skill_md = skill_dir.join("SKILL.md");
        let meta_yaml = skill_dir.join("meta.yaml");
        atomic_write(
            &skill_md,
            &render_skill_md(&skill_name, &description, &body),
        )?;
        atomic_write(&meta_yaml, &render_meta_yaml(&skill_name, &description))?;
        self.reenable_skill_if_disabled(&skill_name)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "create".to_string(),
            path: skill_md,
            snapshot_ref: None,
            message: "Skill created".to_string(),
        })
    }

    pub fn patch(&self, name: &str, old_text: &str, content: &str) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let old_text = old_text.trim();
        if old_text.is_empty() {
            return Err(Error::Validation("old_text cannot be empty".to_string()));
        }
        let content = normalize_skill_body(content)?;
        scan_learned_skill_content(&content)?;
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let skill_md = self.skill_dir(&skill_name).join("SKILL.md");
        let current = fs::read_to_string(&skill_md)
            .map_err(|err| Error::NotFound(format!("skill not found: {} ({})", skill_name, err)))?;
        let next = patch_skill_content(&current, old_text, &content)?;
        ensure_len("SKILL.md", &next, SKILL_MD_CHAR_LIMIT)?;
        let snapshot_ref = self.snapshot_before_write(&skill_name, &skill_md)?;
        atomic_write(&skill_md, &next)?;
        self.reenable_skill_if_disabled(&skill_name)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "patch".to_string(),
            path: skill_md,
            snapshot_ref,
            message: "Skill patched".to_string(),
        })
    }

    pub fn edit(&self, name: &str, content: &str) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let content = normalize_skill_body(content)?;
        scan_learned_skill_content(&content)?;
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let skill_md = self.skill_dir(&skill_name).join("SKILL.md");
        if !skill_md.exists() {
            return Err(Error::NotFound(format!("skill not found: {}", skill_name)));
        }
        let snapshot_ref = self.snapshot_before_write(&skill_name, &skill_md)?;
        atomic_write(&skill_md, &content)?;
        self.reenable_skill_if_disabled(&skill_name)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "edit".to_string(),
            path: skill_md,
            snapshot_ref,
            message: "Skill edited".to_string(),
        })
    }

    pub fn delete(&self, name: &str) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let skill_dir = self.skill_dir(&skill_name);
        if !skill_dir.exists() {
            return Err(Error::NotFound(format!("skill not found: {}", skill_name)));
        }
        let snapshot_ref = self.snapshot_skill_dir(&skill_name)?;
        fs::remove_dir_all(&skill_dir)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "delete".to_string(),
            path: skill_dir,
            snapshot_ref,
            message: "Skill deleted".to_string(),
        })
    }

    pub fn write_file(
        &self,
        name: &str,
        relative_path: &str,
        content: &str,
    ) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let relative_path = validate_skill_relative_path(relative_path)?;
        ensure_len(
            &relative_path.to_string_lossy(),
            content,
            AUX_FILE_CHAR_LIMIT,
        )?;
        scan_learned_skill_content(content)?;
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let skill_dir = self.skill_dir(&skill_name);
        if !skill_dir.exists() {
            return Err(Error::NotFound(format!("skill not found: {}", skill_name)));
        }
        let path = skill_dir.join(relative_path);
        let snapshot_ref = self.snapshot_before_write(&skill_name, &path)?;
        atomic_write(&path, content)?;
        self.reenable_skill_if_disabled(&skill_name)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "write_file".to_string(),
            path,
            snapshot_ref,
            message: "Skill file written".to_string(),
        })
    }

    pub fn restore_latest(&self, name: &str) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let Some(snapshot_path) = self.latest_snapshot_for(&skill_name)? else {
            return Err(Error::NotFound(format!(
                "no snapshot found for skill: {}",
                skill_name
            )));
        };
        let skill_dir = self.skill_dir(&skill_name);
        let current_snapshot = if skill_dir.exists() {
            self.snapshot_skill_dir(&skill_name)?
        } else {
            None
        };
        fs::create_dir_all(&skill_dir)?;
        if snapshot_path.is_dir() {
            copy_dir_recursive(&snapshot_path, &skill_dir)?;
        } else {
            let file_name = snapshot_path
                .file_name()
                .ok_or_else(|| Error::Other("invalid skill snapshot path".to_string()))?;
            fs::copy(&snapshot_path, skill_dir.join(file_name))?;
        }
        self.reenable_skill_if_disabled(&skill_name)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "restore_latest".to_string(),
            path: skill_dir,
            snapshot_ref: current_snapshot
                .or_else(|| Some(snapshot_path.to_string_lossy().to_string())),
            message: "Skill restored".to_string(),
        })
    }

    pub fn remove_file(&self, name: &str, relative_path: &str) -> Result<SkillFileMutation> {
        let skill_name = validate_skill_name(name)?;
        let relative_path = validate_skill_relative_path(relative_path)?;
        if relative_path == Path::new("SKILL.md") || relative_path == Path::new("meta.yaml") {
            return Err(Error::Validation(
                "use delete to remove the whole skill".to_string(),
            ));
        }
        let _wg = self.acquire_write_guard(&skill_name)?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| Error::Other("skill file write lock poisoned".to_string()))?;
        let _file_guard = FileWriteGuard::lock(&self.lock_path)?;
        let path = self.skill_dir(&skill_name).join(relative_path);
        if !path.exists() || !path.is_file() {
            return Err(Error::NotFound(format!(
                "skill file not found: {}",
                path.display()
            )));
        }
        let snapshot_ref = self.snapshot_before_write(&skill_name, &path)?;
        fs::remove_file(&path)?;
        self.invalidate_prompt_snapshot()?;
        Ok(SkillFileMutation {
            skill_name,
            action: "remove_file".to_string(),
            path,
            snapshot_ref,
            message: "Skill file removed".to_string(),
        })
    }

    fn skill_dir(&self, skill_name: &str) -> PathBuf {
        self.skills_dir.join(skill_name)
    }

    fn snapshot_before_write(
        &self,
        skill_name: &str,
        source_path: &Path,
    ) -> Result<Option<String>> {
        if !source_path.exists() {
            return Ok(None);
        }
        let snapshot_dir = self.new_snapshot_dir(skill_name)?;
        let snapshot_path = source_path
            .strip_prefix(self.skill_dir(skill_name))
            .map(|relative| snapshot_dir.join(relative))
            .unwrap_or_else(|_| {
                snapshot_dir.join(
                    source_path
                        .file_name()
                        .and_then(|v| v.to_str())
                        .unwrap_or("file"),
                )
            });
        if let Some(parent) = snapshot_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source_path, &snapshot_path)?;
        Ok(Some(snapshot_path.to_string_lossy().to_string()))
    }

    fn snapshot_skill_dir(&self, skill_name: &str) -> Result<Option<String>> {
        let source_dir = self.skill_dir(skill_name);
        if !source_dir.exists() {
            return Ok(None);
        }
        let snapshot_dir = self.new_snapshot_dir(skill_name)?;
        copy_dir_recursive(&source_dir, &snapshot_dir)?;
        Ok(Some(snapshot_dir.to_string_lossy().to_string()))
    }

    fn new_snapshot_dir(&self, skill_name: &str) -> Result<PathBuf> {
        let stamp = Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
        let dir = self
            .snapshots_dir
            .join(format!("{}_{}_{}", skill_name, stamp, Uuid::new_v4()));
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    fn latest_snapshot_for(&self, skill_name: &str) -> Result<Option<PathBuf>> {
        let prefix = format!("{skill_name}_");
        let mut latest: Option<PathBuf> = None;
        for entry in fs::read_dir(&self.snapshots_dir)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
                continue;
            };
            if !name.starts_with(&prefix) {
                continue;
            }
            if latest
                .as_ref()
                .and_then(|existing| existing.file_name())
                .and_then(|value| value.to_str())
                .map(|existing_name| name > existing_name)
                .unwrap_or(true)
            {
                latest = Some(path);
            }
        }
        Ok(latest)
    }

    fn reenable_skill_if_disabled(&self, skill_name: &str) -> Result<()> {
        if !self.toggles_file.exists() {
            return Ok(());
        }
        let mut toggles = fs::read_to_string(&self.toggles_file)
            .ok()
            .and_then(|content| serde_json::from_str::<Value>(&content).ok())
            .unwrap_or_else(|| json!({"skills": {}, "tools": {}}));
        if toggles
            .get("skills")
            .and_then(|value| value.as_object())
            .and_then(|skills| skills.get(skill_name))
            != Some(&Value::Bool(false))
        {
            return Ok(());
        }
        toggles["skills"][skill_name] = Value::Bool(true);
        atomic_write(&self.toggles_file, &serde_json::to_string_pretty(&toggles)?)
    }

    fn invalidate_prompt_snapshot(&self) -> Result<()> {
        let snapshot_path = self.skills_dir.join(SKILLS_PROMPT_SNAPSHOT_FILE);
        if snapshot_path.exists() {
            fs::remove_file(snapshot_path)?;
        }
        Ok(())
    }
}

impl SkillFileStoreOps for SkillFileStore {
    fn view_skill_json(&self, name: &str) -> Result<Value> {
        self.view(name)
    }

    fn create_skill_json(&self, name: &str, description: &str, content: &str) -> Result<Value> {
        Ok(mutation_json(self.create(name, description, content)?))
    }

    fn edit_skill_json(&self, name: &str, content: &str) -> Result<Value> {
        Ok(mutation_json(self.edit(name, content)?))
    }

    fn patch_skill_json(&self, name: &str, old_text: &str, content: &str) -> Result<Value> {
        Ok(mutation_json(self.patch(name, old_text, content)?))
    }

    fn delete_skill_json(&self, name: &str) -> Result<Value> {
        Ok(mutation_json(self.delete(name)?))
    }

    fn write_skill_file_json(&self, name: &str, path: &str, content: &str) -> Result<Value> {
        Ok(mutation_json(self.write_file(name, path, content)?))
    }

    fn remove_skill_file_json(&self, name: &str, path: &str) -> Result<Value> {
        Ok(mutation_json(self.remove_file(name, path)?))
    }

    fn restore_latest_skill_json(&self, name: &str) -> Result<Value> {
        Ok(mutation_json(self.restore_latest(name)?))
    }
}

fn mutation_json(mutation: SkillFileMutation) -> Value {
    json!({
        "success": true,
        "skillName": mutation.skill_name,
        "action": mutation.action,
        "path": mutation.path.to_string_lossy(),
        "snapshotRef": mutation.snapshot_ref,
        "message": mutation.message,
    })
}

fn validate_skill_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed.len() > 80
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
        || trimmed.starts_with('.')
    {
        return Err(Error::Validation(
            "skill name must use lowercase letters, digits, '-' or '_'".to_string(),
        ));
    }
    Ok(trimmed.to_string())
}

fn validate_skill_relative_path(path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('~') {
        return Err(Error::Validation("invalid skill file path".to_string()));
    }
    let candidate = PathBuf::from(trimmed);
    let mut components = candidate.components().peekable();
    if components.peek().is_none() {
        return Err(Error::Validation("invalid skill file path".to_string()));
    }
    for component in candidate.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::Validation(
                "skill file path cannot contain traversal".to_string(),
            ));
        }
    }
    let first = candidate
        .components()
        .next()
        .and_then(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .unwrap_or_default();
    if !matches!(
        first,
        "references" | "templates" | "scripts" | "assets" | "SKILL.md"
    ) {
        return Err(Error::Validation(
            "skill files must live under references/, templates/, scripts/, assets/, or SKILL.md"
                .to_string(),
        ));
    }
    Ok(candidate)
}

fn normalize_description(description: &str) -> Result<String> {
    let text = description.trim();
    if text.is_empty() {
        return Err(Error::Validation("description cannot be empty".to_string()));
    }
    ensure_len("description", text, 500)?;
    Ok(text.to_string())
}

fn normalize_skill_body(content: &str) -> Result<String> {
    let text = content.trim();
    if text.is_empty() {
        return Err(Error::Validation(
            "skill content cannot be empty".to_string(),
        ));
    }
    ensure_len("SKILL.md", text, SKILL_MD_CHAR_LIMIT)?;
    Ok(text.to_string())
}

fn ensure_len(label: &str, text: &str, limit: usize) -> Result<()> {
    if text.chars().count() > limit {
        return Err(Error::Validation(format!(
            "{} exceeds {} characters",
            label, limit
        )));
    }
    Ok(())
}

fn render_skill_md(name: &str, description: &str, body: &str) -> String {
    format!(
        "# {}\n\n{}\n\n## Shared {{#shared}}\n\nUse this learned skill only when the current task directly matches its procedure.\n\n## Prompt {{#prompt}}\n\n{}\n",
        name, description, body
    )
}

fn render_meta_yaml(name: &str, description: &str) -> String {
    format!(
        "name: {}\ndescription: {}\nsource: blockcell\nuser_invocable: true\ndisable_model_invocation: false\ntools: []\n",
        yaml_scalar(name),
        yaml_scalar(description)
    )
}

fn yaml_scalar(value: &str) -> String {
    serde_yaml::to_string(value)
        .unwrap_or_else(|_| format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
        .trim()
        .trim_start_matches("---")
        .trim()
        .to_string()
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "tmp-{}",
        Uuid::new_v4().to_string().replace('-', "")
    ));
    write_file_durable(&tmp, content)?;
    fs::rename(tmp, path)?;
    sync_parent_dir(path)?;
    Ok(())
}

struct FileWriteGuard {
    path: PathBuf,
}

impl FileWriteGuard {
    fn lock(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::create_dir(path) {
                Ok(()) => {
                    sync_parent_dir(path)?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(Error::Other(format!(
                            "timed out waiting for skill file lock: {}",
                            path.display()
                        )));
                    }
                    // Yield the CPU without blocking the tokio worker thread.
                    std::hint::spin_loop();
                    std::thread::yield_now();
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

impl Drop for FileWriteGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
        let _ = sync_parent_dir(&self.path);
    }
}

fn write_file_durable(path: &Path, content: &str) -> Result<()> {
    let mut file = File::create(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

fn sync_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        match File::open(parent) {
            Ok(dir) => dir.sync_all()?,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn patch_skill_content(current: &str, old_text: &str, replacement: &str) -> Result<String> {
    let count = current.matches(old_text).count();
    if count == 1 {
        return Ok(current.replacen(old_text, replacement, 1));
    }
    if count > 1 {
        return Err(patch_match_error(
            "old_text is ambiguous",
            old_text,
            current,
            current
                .match_indices(old_text)
                .map(|(start, text)| (start, start + text.len()))
                .collect(),
        ));
    }

    let candidates = fuzzy_patch_candidates(current, old_text);
    let (start, end) = match candidates.as_slice() {
        [only] => (only.start, only.end),
        [best, second, ..] if best.score - second.score >= 0.20 && best.score >= 0.92 => {
            (best.start, best.end)
        }
        [] => {
            return Err(patch_match_error(
                "old_text did not match any unique location",
                old_text,
                current,
                Vec::new(),
            ));
        }
        _ => {
            return Err(patch_match_error(
                "old_text fuzzy match is ambiguous",
                old_text,
                current,
                candidates
                    .iter()
                    .map(|candidate| (candidate.start, candidate.end))
                    .collect(),
            ));
        }
    };
    let mut next = String::with_capacity(current.len() - (end - start) + replacement.len());
    next.push_str(&current[..start]);
    next.push_str(replacement);
    next.push_str(&current[end..]);
    Ok(next)
}

#[derive(Debug, Clone)]
struct PatchCandidate {
    start: usize,
    end: usize,
    score: f64,
}

fn fuzzy_patch_candidates(haystack: &str, needle: &str) -> Vec<PatchCandidate> {
    let needle_tokens = tokenize_patch_match_text(needle);
    if needle_tokens.len() < 4 {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for (start, end) in fuzzy_patch_segments(haystack) {
        let span_tokens = tokenize_patch_match_text(&haystack[start..end]);
        if span_tokens.is_empty() {
            continue;
        }
        let score = patch_token_similarity(&needle_tokens, &span_tokens);
        if score >= 0.72 {
            candidates.push(PatchCandidate { start, end, score });
        }
    }

    candidates.sort_by(|left, right| right.score.total_cmp(&left.score));
    candidates
}

fn patch_match_error(
    reason: &str,
    old_text: &str,
    current: &str,
    candidates: Vec<(usize, usize)>,
) -> Error {
    let preview = truncate_chars(current, 700);
    let candidate_previews = candidates
        .into_iter()
        .take(5)
        .map(|(start, end)| truncate_chars(current[start..end].trim(), 180))
        .collect::<Vec<_>>();
    Error::Validation(
        serde_json::json!({
            "error": reason,
            "old_text": old_text,
            "hint": "Use skill_view, then retry with a longer unique old_text that includes surrounding context.",
            "file_preview": preview,
            "candidates": candidate_previews,
        })
        .to_string(),
    )
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let truncated = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        format!("{truncated}...")
    } else {
        truncated
    }
}

fn fuzzy_patch_segments(text: &str) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let end = idx + ch.len_utf8();
            if end > start {
                segments.push((start, end));
            }
            start = end;
        }
    }
    if start < text.len() {
        segments.push((start, text.len()));
    }
    segments
        .into_iter()
        .map(|(start, end)| trim_span(text, start, end))
        .filter(|(start, end)| end > start)
        .collect()
}

fn trim_span(text: &str, mut start: usize, mut end: usize) -> (usize, usize) {
    while start < end {
        let Some(ch) = text[start..end].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        start += ch.len_utf8();
    }
    while start < end {
        let Some(ch) = text[start..end].chars().next_back() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        end -= ch.len_utf8();
    }
    (start, end)
}

fn tokenize_patch_match_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.push(ch.to_ascii_lowercase());
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn patch_token_similarity(left: &[String], right: &[String]) -> f64 {
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let mut dp = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for i in 0..left.len() {
        for (j, right_token) in right.iter().enumerate() {
            dp[i + 1][j + 1] = if &left[i] == right_token {
                dp[i][j] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let common = dp[left.len()][right.len()] as f64;
    common / left.len().max(right.len()) as f64
}

fn list_skill_files(skill_dir: &Path) -> Result<Vec<String>> {
    let mut files = Vec::new();
    collect_files(skill_dir, skill_dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect_files(root: &Path, dir: &Path, files: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                files.push(rel.to_string_lossy().to_string());
            }
        }
    }
    Ok(())
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();

        // Check for symbolic links to prevent symlink attacks
        // Use symlink_metadata to detect symlinks without following them
        if let Ok(meta) = fs::symlink_metadata(&source_path) {
            if meta.file_type().is_symlink() {
                tracing::warn!(
                    path = %source_path.display(),
                    "Skipping symbolic link in skill directory to prevent path traversal"
                );
                continue;
            }
        }

        let dest_path = dest.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir_recursive(&source_path, &dest_path)?;
        } else {
            fs::copy(source_path, dest_path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(name: &str) -> Paths {
        let base = std::env::temp_dir().join(format!(
            "blockcell-skill-file-store-{}-{}",
            name,
            Uuid::new_v4()
        ));
        Paths::with_base(base)
    }

    #[test]
    fn skill_file_store_creates_and_views_prompt_skill() {
        let paths = test_paths("create");
        let store = SkillFileStore::open(&paths).unwrap();
        let result = store
            .create(
                "release_checklist",
                "Release verification checklist",
                "Confirm rollback plan before release verification.",
            )
            .unwrap();
        assert_eq!(result.action, "create");
        let view = store.view("release_checklist").unwrap();
        assert!(view["content"]
            .as_str()
            .unwrap()
            .contains("Confirm rollback plan"));
        assert!(paths
            .skills_dir()
            .join("release_checklist")
            .join("meta.yaml")
            .exists());
    }

    #[test]
    fn skill_file_store_patches_and_snapshots_skill_md() {
        let paths = test_paths("patch");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create("deploy_docs", "Deploy docs", "Write concise deploy docs.")
            .unwrap();
        let result = store
            .patch(
                "deploy_docs",
                "Write concise deploy docs.",
                "Write concise deploy docs with rollback steps.",
            )
            .unwrap();
        assert!(result.snapshot_ref.is_some());
        let view = store.view("deploy_docs").unwrap();
        assert!(view["content"].as_str().unwrap().contains("rollback steps"));
    }

    #[test]
    fn skill_file_store_patch_accepts_minor_fuzzy_mismatch() {
        let paths = test_paths("patch-fuzzy");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create(
                "deploy_docs",
                "Deploy docs",
                "Before release, verify the rollback checklist and smoke tests.",
            )
            .unwrap();

        let result = store
            .patch(
                "deploy_docs",
                "Before release verify rollback checklist and smoke tests",
                "Before release, verify the rollback checklist, smoke tests, and owner handoff.",
            )
            .unwrap();

        assert_eq!(result.action, "patch");
        let view = store.view("deploy_docs").unwrap();
        let content = view["content"].as_str().unwrap();
        assert!(content.contains("owner handoff"));
        assert!(!content.contains("smoke tests.\n"));
    }

    #[test]
    fn skill_file_store_patch_failure_returns_preview_and_hint() {
        let paths = test_paths("patch-preview");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create(
                "deploy_docs",
                "Deploy docs",
                "First check rollback. Then run smoke tests. Finally notify owners.",
            )
            .unwrap();

        let err = store
            .patch("deploy_docs", "nonexistent rollback section", "replacement")
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("file_preview"));
        assert!(text.contains("First check rollback"));
        assert!(text.contains("hint"));
    }

    #[test]
    fn skill_file_store_patch_ambiguous_fuzzy_match_returns_candidates() {
        let paths = test_paths("patch-ambiguous");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create(
                "deploy_docs",
                "Deploy docs",
                "Before release, verify rollback checklist. Before deploy, verify rollback checklist.",
            )
            .unwrap();

        let err = store
            .patch(
                "deploy_docs",
                "Before release verify rollback checklist",
                "Before release, verify rollback checklist and owner handoff.",
            )
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("ambiguous"));
        assert!(text.contains("candidates"));
        assert!(text.contains("Before release"));
        assert!(text.contains("Before deploy"));
    }

    #[test]
    fn skill_file_store_mutations_invalidate_skill_prompt_snapshot() {
        let paths = test_paths("invalidate-snapshot");
        let store = SkillFileStore::open(&paths).unwrap();
        std::fs::write(
            paths.skills_dir().join(SKILLS_PROMPT_SNAPSHOT_FILE),
            "stale snapshot",
        )
        .expect("write stale snapshot");

        store
            .create("deploy_docs", "Deploy docs", "Write deploy docs.")
            .unwrap();

        assert!(!paths
            .skills_dir()
            .join(SKILLS_PROMPT_SNAPSHOT_FILE)
            .exists());
    }

    #[test]
    fn skill_file_store_restore_latest_reverts_skill_md() {
        let paths = test_paths("restore-latest");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create("deploy_docs", "Deploy docs", "Write concise deploy docs.")
            .unwrap();
        store
            .patch(
                "deploy_docs",
                "Write concise deploy docs.",
                "Write verbose deploy docs without rollback steps.",
            )
            .unwrap();

        let result = store.restore_latest("deploy_docs").unwrap();

        assert_eq!(result.action, "restore_latest");
        let view = store.view("deploy_docs").unwrap();
        let content = view["content"].as_str().unwrap();
        assert!(content.contains("Write concise deploy docs."));
        assert!(!content.contains("verbose deploy docs"));
        assert!(paths
            .skills_dir()
            .join("deploy_docs")
            .join("meta.yaml")
            .exists());
    }

    #[test]
    fn skill_file_store_restore_latest_preserves_relative_auxiliary_path() {
        let paths = test_paths("restore-aux");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create("research_flow", "Research flow", "Collect evidence.")
            .unwrap();
        store
            .write_file("research_flow", "references/checklist.md", "first version")
            .unwrap();
        store
            .write_file("research_flow", "references/checklist.md", "second version")
            .unwrap();

        store.restore_latest("research_flow").unwrap();

        let restored = fs::read_to_string(
            paths
                .skills_dir()
                .join("research_flow")
                .join("references/checklist.md"),
        )
        .unwrap();
        assert_eq!(restored, "first version");
    }

    #[test]
    fn skill_file_store_patch_reenables_disabled_learned_skill() {
        let paths = test_paths("patch-reenable");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create("deploy_docs", "Deploy docs", "Write concise deploy docs.")
            .unwrap();
        let toggles = paths.toggles_file();
        fs::write(
            &toggles,
            serde_json::to_string_pretty(&json!({"skills": {"deploy_docs": false}, "tools": {}}))
                .unwrap(),
        )
        .unwrap();

        store
            .patch(
                "deploy_docs",
                "Write concise deploy docs.",
                "Write concise deploy docs with rollback steps.",
            )
            .unwrap();

        let toggles_json: Value =
            serde_json::from_str(&fs::read_to_string(toggles).unwrap()).unwrap();
        assert_eq!(toggles_json["skills"]["deploy_docs"], json!(true));
    }

    #[test]
    fn skill_file_store_edits_skill_md_with_snapshot() {
        let paths = test_paths("edit");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create("triage_notes", "Triage notes", "Check logs first.")
            .unwrap();

        let result = store
            .edit(
                "triage_notes",
                "# triage_notes\n\nUpdated complete skill instructions.\n",
            )
            .unwrap();

        assert_eq!(result.action, "edit");
        assert!(result.snapshot_ref.is_some());
        let view = store.view("triage_notes").unwrap();
        assert!(view["content"]
            .as_str()
            .unwrap()
            .contains("Updated complete skill instructions"));
    }

    #[test]
    fn skill_file_store_writes_removes_and_deletes_skill_files() {
        let paths = test_paths("files");
        let store = SkillFileStore::open(&paths).unwrap();
        store
            .create("research_flow", "Research flow", "Collect evidence.")
            .unwrap();

        let write = store
            .write_file("research_flow", "references/checklist.md", "# Checklist\n")
            .unwrap();
        assert_eq!(write.action, "write_file");
        assert!(write.path.exists());

        let remove = store
            .remove_file("research_flow", "references/checklist.md")
            .unwrap();
        assert_eq!(remove.action, "remove_file");
        assert!(remove.snapshot_ref.is_some());
        assert!(!remove.path.exists());

        let delete = store.delete("research_flow").unwrap();
        assert_eq!(delete.action, "delete");
        assert!(delete.snapshot_ref.is_some());
        assert!(!paths.skills_dir().join("research_flow").exists());
    }

    #[test]
    fn skill_file_store_rejects_path_traversal() {
        let paths = test_paths("traversal");
        let store = SkillFileStore::open(&paths).unwrap();
        store.create("safe_skill", "Safe", "Do safe work.").unwrap();
        let err = store
            .write_file("safe_skill", "../evil.txt", "bad")
            .unwrap_err();
        assert!(format!("{}", err).contains("traversal"));
    }
}
