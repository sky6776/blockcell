use async_trait::async_trait;
use blockcell_core::{Error, Result};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

use crate::{Tool, ToolContext, ToolSchema};

pub struct ExecLocalTool;

pub(crate) const ALLOWED_RUNNERS: &[&str] =
    &["python3", "python", "bash", "sh", "node", "php", "uv"];

pub(crate) fn validate_relative_skill_path(path: &str) -> Result<()> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(Error::Validation(
            "Missing required parameter: path".to_string(),
        ));
    }

    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return Err(Error::Validation(
            "`path` must be relative to the active skill directory".to_string(),
        ));
    }

    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::PermissionDenied(
            "`path` cannot escape the active skill directory".to_string(),
        ));
    }

    Ok(())
}

pub(crate) fn validate_runner(runner: &str) -> Result<()> {
    if ALLOWED_RUNNERS.contains(&runner) {
        Ok(())
    } else {
        Err(Error::PermissionDenied(format!(
            "Runner '{}' is not allowed for exec_local",
            runner
        )))
    }
}

/// Infer an appropriate runner from the script file extension when none is specified.
/// Returns `None` if the extension is unknown (caller should attempt direct execution).
pub(crate) fn infer_runner_from_extension(path: &str) -> Option<&'static str> {
    if path.ends_with(".py") {
        Some("python3")
    } else if path.ends_with(".sh") {
        Some("sh")
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        Some("node")
    } else if path.ends_with(".php") {
        Some("php")
    } else {
        None
    }
}

pub(crate) fn truncate_output(text: String, max_chars: usize, suffix: &str) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }

    match text.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}\n{}", &text[..idx], suffix),
        None => text,
    }
}

pub(crate) fn resolve_script_path(skill_dir: &Path, relative_path: &str) -> Result<PathBuf> {
    validate_relative_skill_path(relative_path)?;
    let joined = skill_dir.join(relative_path);
    let canonical_skill_dir = std::fs::canonicalize(skill_dir)?;
    let canonical_target = std::fs::canonicalize(&joined)
        .map_err(|_| Error::NotFound(format!("Local script '{}' not found", relative_path)))?;

    if !canonical_target.starts_with(&canonical_skill_dir) {
        return Err(Error::PermissionDenied(
            "Resolved script path is outside the active skill directory".to_string(),
        ));
    }

    // Strip \\?\ prefix on Windows for command-line compatibility.
    // canonicalize() adds this prefix on Windows for long path support,
    // but it causes issues when passed to subprocess via command line.
    #[cfg(windows)]
    {
        let path_str = canonical_target.to_string_lossy();
        if let Some(stripped) = path_str.strip_prefix("\\\\?\\") {
            return Ok(PathBuf::from(stripped));
        }
    }

    Ok(canonical_target)
}

#[async_trait]
impl Tool for ExecLocalTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "exec_local",
            description:
                "Execute a local script or executable inside the active skill directory only. The `path` must be RELATIVE (e.g. `scripts/run.py`), never absolute. If the skill manual shows `{baseDir}/scripts/...`, strip the `{baseDir}/` prefix and pass only the relative portion.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "RELATIVE path to the script inside the active skill directory (e.g. `scripts/run.py`). Must NOT be absolute. The tool auto-infers the interpreter from the file extension (.py→python3, .sh→sh, .js→node)."
                    },
                    "runner": {
                        "type": "string",
                        "description": "Optional interpreter override. Allowed: python3, python, bash, sh, node, php, uv. Auto-inferred from extension when omitted."
                    },
                    "args": {
                        "type": "array",
                        "description": "Arguments passed to the script or executable.",
                        "items": {
                            "type": "string"
                        }
                    },
                    "cwd_mode": {
                        "type": "string",
                        "description": "Working directory mode. Only `skill` is supported.",
                        "enum": ["skill"]
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<()> {
        let path = params
            .get("path")
            .and_then(|value| value.as_str())
            .ok_or_else(|| Error::Validation("Missing required parameter: path".to_string()))?;
        validate_relative_skill_path(path)?;

        if let Some(runner) = params.get("runner").and_then(|value| value.as_str()) {
            validate_runner(runner)?;
        }

        if let Some(args) = params.get("args").and_then(|value| value.as_array()) {
            if args.iter().any(|value| value.as_str().is_none()) {
                return Err(Error::Validation(
                    "`args` must be an array of strings".to_string(),
                ));
            }
        }

        if let Some(cwd_mode) = params.get("cwd_mode").and_then(|value| value.as_str()) {
            if cwd_mode != "skill" {
                return Err(Error::Validation(
                    "`cwd_mode` only supports `skill`".to_string(),
                ));
            }
        }

        Ok(())
    }

    async fn execute(&self, ctx: ToolContext, params: Value) -> Result<Value> {
        let skill_dir = ctx.active_skill_dir.ok_or_else(|| {
            // Use Tool error (not PermissionDenied) to avoid Permanent classification.
            // Hint tells the LLM to use activate_skill first.
            Error::Tool(
                "exec_local requires an active skill context. \
                Use the `activate_skill` tool first, e.g. \
                activate_skill({skill_name: \"<skill-name>\", goal: \"<goal>\"})"
                    .to_string(),
            )
        })?;
        let relative_path = params["path"]
            .as_str()
            .ok_or_else(|| Error::Validation("Missing required parameter: path".to_string()))?;
        let resolved_path = resolve_script_path(&skill_dir, relative_path)?;
        let explicit_runner = params.get("runner").and_then(|value| value.as_str());
        if let Some(runner) = explicit_runner {
            validate_runner(runner)?;
        }
        // Auto-infer runner from file extension when not explicitly specified (Windows compat)
        let effective_runner =
            explicit_runner.or_else(|| infer_runner_from_extension(relative_path));
        let args = params
            .get("args")
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|value| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    Error::Validation("`args` must be an array of strings".to_string())
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let cwd_mode = params
            .get("cwd_mode")
            .and_then(|value| value.as_str())
            .unwrap_or("skill");
        if cwd_mode != "skill" {
            return Err(Error::Validation(
                "`cwd_mode` only supports `skill`".to_string(),
            ));
        }

        let timeout_secs = ctx.config.tools.exec.timeout as u64;
        let max_output_chars = 10_000usize;

        let mut command = if let Some(runner) = effective_runner {
            let mut command = Command::new(runner);
            // For `uv`, use `uv run -- python3 -X utf8 <script>` to:
            // 1. Enable uv's dependency management
            // 2. Force UTF-8 mode (uv doesn't pass PYTHONIOENCODING to subprocess)
            if runner == "uv" {
                command.arg("run");
                command.arg("--");
                command.arg("python3");
                command.arg("-X");
                command.arg("utf8");
            } else if runner == "python3" || runner == "python" {
                // On Windows, python3 needs -X utf8 flag to handle UTF-8 output properly
                // (MSYS2/MinGW python3 ignores PYTHONIOENCODING env var)
                #[cfg(windows)]
                {
                    command.arg("-X");
                    command.arg("utf8");
                }
            }
            command.arg(&resolved_path);
            command
        } else {
            Command::new(&resolved_path)
        };
        command
            .args(&args)
            .current_dir(&skill_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1");

        let output = timeout(Duration::from_secs(timeout_secs), command.output())
            .await
            .map_err(|_| {
                Error::Timeout(format!(
                    "Local script timed out after {} seconds",
                    timeout_secs
                ))
            })?
            .map_err(|error| Error::Tool(format!("Failed to execute local script: {}", error)))?;

        let stdout = truncate_output(
            String::from_utf8_lossy(&output.stdout).to_string(),
            max_output_chars,
            "... (stdout truncated)",
        );
        let stderr = truncate_output(
            String::from_utf8_lossy(&output.stderr).to_string(),
            max_output_chars,
            "... (stderr truncated)",
        );

        // Build command_parts for logging/output: `runner [run -- python3 -X utf8] script [args...]`
        let command_parts: Vec<String> = if effective_runner == Some("uv") {
            vec![
                "uv".to_string(),
                "run".to_string(),
                "--".to_string(),
                "python3".to_string(),
                "-X".to_string(),
                "utf8".to_string(),
                resolved_path.display().to_string(),
            ]
            .into_iter()
            .chain(args.iter().cloned())
            .collect()
        } else if effective_runner == Some("python3") || effective_runner == Some("python") {
            // On Windows, python3 needs -X utf8 flag to handle UTF-8 output properly
            std::iter::once(effective_runner.map(str::to_string))
                .flatten()
                .chain(if cfg!(windows) {
                    vec!["-X".to_string(), "utf8".to_string()]
                } else {
                    vec![]
                })
                .chain(Some(resolved_path.display().to_string()))
                .chain(args.iter().cloned())
                .collect()
        } else {
            std::iter::once(effective_runner.map(str::to_string))
                .flatten()
                .chain(if effective_runner.is_some() {
                    Some(resolved_path.display().to_string())
                } else {
                    None
                })
                .chain(args.iter().cloned())
                .collect()
        };

        Ok(json!({
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
            "command": command_parts.join(" "),
            "resolved_path": resolved_path.display().to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blockcell_core::Config;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn temp_skill_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{}-{}", prefix, uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("create temp skill dir");
        dir
    }

    fn tool_context(skill_dir: PathBuf) -> ToolContext {
        ToolContext {
            workspace: std::env::temp_dir(),
            builtin_skills_dir: None,
            active_skill_dir: Some(skill_dir),
            session_key: "cli:test".to_string(),
            channel: "cli".to_string(),
            account_id: None,
            sender_id: None,
            chat_id: "chat-1".to_string(),
            config: Config::default(),
            permissions: blockcell_core::types::PermissionSet::new(),
            task_manager: None,
            memory_store: None,
            outbound_tx: None,
            spawn_handle: None,
            capability_registry: None,
            core_evolution: None,
            event_emitter: None,
            channel_contacts_file: None,
            response_cache: None,
        }
    }

    #[tokio::test]
    async fn test_exec_local_runs_skill_relative_script() {
        let skill_dir = temp_skill_dir("blockcell-exec-local");
        let scripts_dir = skill_dir.join("scripts");
        fs::create_dir_all(&scripts_dir).expect("create scripts dir");
        let script_path = scripts_dir.join("hello.sh");
        fs::write(&script_path, "#!/bin/sh\necho \"hello $1\"\n").expect("write script");
        #[cfg(unix)]
        {
            let mut perms = fs::metadata(&script_path)
                .expect("script metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script_path, perms).expect("set script perms");
        }

        let tool = ExecLocalTool;
        let result = tool
            .execute(
                tool_context(skill_dir.clone()),
                json!({
                    "path": "scripts/hello.sh",
                    "runner": "sh",
                    "args": ["world"],
                    "cwd_mode": "skill"
                }),
            )
            .await
            .expect("exec_local should succeed");
        let expected_path = script_path.canonicalize().expect("canonical path");

        assert_eq!(result["exit_code"].as_i64(), Some(0));
        assert!(result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("hello world"));
        assert_eq!(
            result["resolved_path"].as_str(),
            Some(expected_path.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn test_exec_local_runs_top_level_skill_py() {
        let skill_dir = temp_skill_dir("blockcell-exec-local-skill-py");
        let script_path = skill_dir.join("SKILL.py");
        fs::write(
            &script_path,
            "import sys\nprint('py:' + '-'.join(sys.argv[1:]))\n",
        )
        .expect("write skill py");

        let tool = ExecLocalTool;
        let result = tool
            .execute(
                tool_context(skill_dir.clone()),
                json!({
                    "path": "SKILL.py",
                    "runner": "python3",
                    "args": ["demo", "local"],
                    "cwd_mode": "skill"
                }),
            )
            .await
            .expect("exec_local should succeed for SKILL.py");

        let expected_path = script_path.canonicalize().expect("canonical path");

        assert_eq!(result["exit_code"].as_i64(), Some(0));
        assert!(result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("py:demo-local"));
        assert_eq!(
            result["resolved_path"].as_str(),
            Some(expected_path.to_string_lossy().as_ref())
        );
    }

    #[tokio::test]
    async fn test_exec_local_blocks_parent_path_escape() {
        let skill_dir = temp_skill_dir("blockcell-exec-local-escape");
        let outside_path = skill_dir
            .parent()
            .expect("skill dir parent")
            .join("outside.sh");
        fs::write(&outside_path, "#!/bin/sh\necho escape\n").expect("write outside script");

        let tool = ExecLocalTool;
        let result = tool
            .execute(
                tool_context(skill_dir),
                json!({
                    "path": "../outside.sh",
                    "runner": "sh"
                }),
            )
            .await;

        assert!(result.is_err());
        assert!(format!("{}", result.expect_err("should fail")).contains("skill directory"));
    }

    #[test]
    fn test_exec_local_allows_whitelisted_runners_only() {
        let tool = ExecLocalTool;

        assert!(tool
            .validate(&json!({
                "path": "scripts/run.py",
                "runner": "python3"
            }))
            .is_ok());
        assert!(tool
            .validate(&json!({
                "path": "scripts/run.py",
                "runner": "perl"
            }))
            .is_err());
    }
}
