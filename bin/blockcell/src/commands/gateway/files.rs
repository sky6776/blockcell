use super::*;
// ---------------------------------------------------------------------------
// P2: File management endpoints
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct FileListQuery {
    #[serde(default = "default_file_path")]
    path: String,
    #[serde(default)]
    agent: Option<String>,
}

fn default_file_path() -> String {
    ".".to_string()
}

/// GET /v1/files — list directory contents
pub(super) async fn handle_files_list(
    State(state): State<GatewayState>,
    Query(params): Query<FileListQuery>,
) -> impl IntoResponse {
    let agent_id = match resolve_requested_agent_id(&state.config, params.agent.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(err) => return Json(serde_json::json!({ "error": err })),
    };
    let workspace = state.paths.for_agent(&agent_id).workspace();
    let target = if params.path == "." || params.path.is_empty() {
        workspace.to_path_buf()
    } else {
        workspace.join(&params.path)
    };

    // Security: ensure path is within workspace
    let canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            if !target.exists() {
                return Json(serde_json::json!({ "error": "Path not found" }));
            }
            target.clone()
        }
    };
    let ws_canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if !canonical.starts_with(&ws_canonical) {
        return Json(serde_json::json!({ "error": "Access denied: path outside workspace" }));
    }

    if !target.is_dir() {
        return Json(serde_json::json!({ "error": "Not a directory" }));
    }

    let mut entries = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&target) {
        for entry in dir.flatten() {
            let meta = entry.metadata().ok();
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta.as_ref().and_then(|m| m.modified().ok()).map(|t| {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.to_rfc3339()
            });

            // Relative path from workspace
            let rel_path = entry
                .path()
                .strip_prefix(&workspace)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| name.clone());

            let ext = entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_lowercase())
                .unwrap_or_default();

            let file_type = if is_dir {
                "directory".to_string()
            } else {
                match ext.as_str() {
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" => "image",
                    "mp3" | "wav" | "m4a" | "flac" | "ogg" => "audio",
                    "mp4" | "mkv" | "webm" | "avi" => "video",
                    "pdf" => "pdf",
                    "json" | "jsonl" => "json",
                    "md" | "txt" | "log" | "csv" | "yaml" | "yml" | "toml" | "xml" | "html"
                    | "css" | "js" | "ts" | "py" | "rs" | "sh" | "rhai" => "text",
                    "xlsx" | "xls" | "docx" | "pptx" => "office",
                    "zip" | "tar" | "gz" | "tgz" => "archive",
                    "db" | "sqlite" => "database",
                    _ => "file",
                }
                .to_string()
            };

            entries.push(serde_json::json!({
                "name": name,
                "path": rel_path,
                "is_dir": is_dir,
                "size": size,
                "type": file_type,
                "modified": modified,
            }));
        }
    }

    // Sort: directories first, then by name
    entries.sort_by(|a, b| {
        let a_dir = a.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false);
        let b_dir = b.get("is_dir").and_then(|v| v.as_bool()).unwrap_or(false);
        match (b_dir, a_dir) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => {
                let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
                a_name.cmp(b_name)
            }
        }
    });

    let count = entries.len();
    Json(serde_json::json!({
        "path": params.path,
        "entries": entries,
        "count": count,
    }))
}

#[derive(Deserialize)]
pub(super) struct FileContentQuery {
    path: String,
    #[serde(default)]
    agent: Option<String>,
}

/// GET /v1/files/content — read file content
pub(super) async fn handle_files_content(
    State(state): State<GatewayState>,
    Query(params): Query<FileContentQuery>,
) -> Response {
    let agent_id = match resolve_requested_agent_id(&state.config, params.agent.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };
    let workspace = state.paths.for_agent(&agent_id).workspace();
    let target = workspace.join(&params.path);

    // Security check
    let canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };
    let ws_canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if !canonical.starts_with(&ws_canonical) {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }

    if !target.is_file() {
        return (StatusCode::NOT_FOUND, "Not a file").into_response();
    }

    let ext = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    // For binary files (images, etc.), return base64 encoded
    let is_binary = matches!(
        ext.as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "svg"
            | "mp3"
            | "wav"
            | "m4a"
            | "mp4"
            | "mkv"
            | "webm"
            | "pdf"
            | "xlsx"
            | "xls"
            | "docx"
            | "pptx"
            | "zip"
            | "tar"
            | "gz"
            | "db"
            | "sqlite"
    );

    let mime_type = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "json" | "jsonl" => "application/json",
        "html" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        _ => {
            if is_binary {
                "application/octet-stream"
            } else {
                "text/plain"
            }
        }
    };

    if is_binary {
        match std::fs::read(&target) {
            Ok(bytes) => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                Json(serde_json::json!({
                    "path": params.path,
                    "encoding": "base64",
                    "mime_type": mime_type,
                    "size": bytes.len(),
                    "content": b64,
                }))
                .into_response()
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Read error: {}", e),
            )
                .into_response(),
        }
    } else {
        match std::fs::read_to_string(&target) {
            Ok(content) => Json(serde_json::json!({
                "path": params.path,
                "encoding": "utf-8",
                "mime_type": mime_type,
                "size": content.len(),
                "content": content,
            }))
            .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Read error: {}", e),
            )
                .into_response(),
        }
    }
}

/// GET /v1/files/download — download a file
pub(super) async fn handle_files_download(
    State(state): State<GatewayState>,
    Query(params): Query<FileContentQuery>,
) -> Response {
    let agent_id = match resolve_requested_agent_id(&state.config, params.agent.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };
    let workspace = state.paths.for_agent(&agent_id).workspace();
    let target = workspace.join(&params.path);

    let canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };
    let ws_canonical = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    if !canonical.starts_with(&ws_canonical) {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }

    match std::fs::read(&target) {
        Ok(bytes) => {
            let filename = target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download");
            let headers = [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", filename),
                ),
            ];
            (headers, bytes).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Read error: {}", e),
        )
            .into_response(),
    }
}

/// GET /v1/files/serve — serve a file inline with proper Content-Type (for <img>/<audio> tags)
/// Supports both workspace-relative paths and absolute paths within ~/.blockcell/
pub(super) async fn handle_files_serve(
    State(state): State<GatewayState>,
    Query(params): Query<FileContentQuery>,
) -> Response {
    let base_dir = state.paths.base.clone();
    let agent_id = match resolve_requested_agent_id(&state.config, params.agent.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(err) => return (StatusCode::BAD_REQUEST, err).into_response(),
    };
    let workspace = state.paths.for_agent(&agent_id).workspace();

    // Determine target: absolute path or workspace-relative
    let target = if params.path.starts_with('/') {
        std::path::PathBuf::from(&params.path)
    } else {
        workspace.join(&params.path)
    };

    // Canonicalize for security check
    let canonical = match target.canonicalize() {
        Ok(p) => p,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };

    // Security: file must be within ~/.blockcell/ base directory
    let base_canonical = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());
    if !canonical.starts_with(&base_canonical) {
        return (
            StatusCode::FORBIDDEN,
            "Access denied: file outside allowed directory",
        )
            .into_response();
    }

    if !target.is_file() {
        return (StatusCode::NOT_FOUND, "Not a file").into_response();
    }

    let ext = target
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_lowercase())
        .unwrap_or_default();

    let content_type = match ext.as_str() {
        // Images
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "heic" | "heif" => "image/heic",
        "tiff" | "tif" => "image/tiff",
        // Audio
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" | "aac" => "audio/aac",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "opus" => "audio/opus",
        "weba" => "audio/webm",
        // Video
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        // Other
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    };

    match std::fs::read(&target) {
        Ok(bytes) => {
            let headers = [
                (header::CONTENT_TYPE, content_type.to_string()),
                (header::CACHE_CONTROL, "public, max-age=3600".to_string()),
            ];
            (headers, bytes).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Read error: {}", e),
        )
            .into_response(),
    }
}

/// POST /v1/files/upload — upload a file to workspace
pub(super) async fn handle_files_upload(
    State(state): State<GatewayState>,
    Query(agent): Query<AgentScopedQuery>,
    Json(req): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_id = match resolve_requested_agent_id(&state.config, agent.agent.as_deref()) {
        Ok(agent_id) => agent_id,
        Err(err) => return Json(serde_json::json!({ "error": err })),
    };
    let path = req.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let content = req.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let encoding = req
        .get("encoding")
        .and_then(|v| v.as_str())
        .unwrap_or("utf-8");

    let rel = match validate_workspace_relative_path(path) {
        Ok(p) => p,
        Err(e) => return Json(serde_json::json!({ "error": e })),
    };

    let workspace = state.paths.for_agent(&agent_id).workspace();
    let target = workspace.join(&rel);
    let path_echo = rel.to_string_lossy().to_string();
    let content = content.to_string();
    let encoding = encoding.to_string();

    let result = tokio::task::spawn_blocking(move || {
        if let Some(parent) = target.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return Err(format!("{}", e));
            }
        }

        if encoding == "base64" {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(content.as_bytes())
                .map_err(|e| format!("Base64 decode error: {}", e))?;
            std::fs::write(&target, bytes).map_err(|e| format!("{}", e))?;
        } else {
            std::fs::write(&target, content).map_err(|e| format!("{}", e))?;
        }
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(_)) => Json(serde_json::json!({ "status": "uploaded", "path": path_echo })),
        Ok(Err(e)) => Json(serde_json::json!({ "error": e })),
        Err(e) => Json(serde_json::json!({ "error": format!("{}", e) })),
    }
}
