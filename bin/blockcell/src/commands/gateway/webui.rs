use super::*;
// ---------------------------------------------------------------------------
// Embedded WebUI static files
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "../../webui/dist"]
struct WebUiAssets;

pub(super) async fn handle_webui_static(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    // Try the exact path first, then fall back to index.html for SPA routing
    let file_path = if path.is_empty() { "index.html" } else { path };

    match WebUiAssets::get(file_path) {
        Some(content) => {
            let mime = mime_guess::from_path(file_path)
                .first_or_octet_stream()
                .to_string();
            let mut body: Vec<u8> = content.data.into();

            // Runtime injection: make WebUI load /env.js before the main bundle.
            // This allows changing backend address via config.json5 without rebuilding dist.
            if file_path == "index.html" {
                let html = String::from_utf8_lossy(&body);
                let injected = inject_env_js_into_index_html(&html);
                body = injected.into_bytes();
            }
            // index.html must never be cached: a stale index.html that references
            // old hashed JS/CSS bundle filenames causes a blank page after rebuild.
            // Hashed assets (/assets/*.js, /assets/*.css) are safe to cache forever.
            let cache_control = if file_path == "index.html" {
                "no-store, no-cache, must-revalidate"
            } else if file_path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "public, max-age=3600"
            };
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime),
                    (header::CACHE_CONTROL, cache_control.to_string()),
                ],
                body,
            )
                .into_response()
        }
        None => {
            // SPA fallback: serve index.html for any unknown route
            match WebUiAssets::get("index.html") {
                Some(content) => {
                    let mut body: Vec<u8> = content.data.into();
                    let html = String::from_utf8_lossy(&body);
                    let injected = inject_env_js_into_index_html(&html);
                    body = injected.into_bytes();
                    (
                        StatusCode::OK,
                        [
                            (header::CONTENT_TYPE, "text/html".to_string()),
                            (
                                header::CACHE_CONTROL,
                                "no-store, no-cache, must-revalidate".to_string(),
                            ),
                        ],
                        body,
                    )
                        .into_response()
                }
                None => (StatusCode::NOT_FOUND, "Not Found").into_response(),
            }
        }
    }
}

fn inject_env_js_into_index_html(html: &str) -> String {
    let tag = "<script src=\"/env.js\"></script>";
    if html.contains(tag) {
        return html.to_string();
    }
    if let Some(idx) = html.find("</head>") {
        let mut out = String::with_capacity(html.len() + tag.len() + 1);
        out.push_str(&html[..idx]);
        out.push_str(tag);
        out.push_str(&html[idx..]);
        return out;
    }
    format!("{}{}", tag, html)
}

pub(super) async fn handle_webui_env_js(config: Config) -> impl IntoResponse {
    let api_port = config.gateway.port;
    let public_base = config.gateway.public_api_base.clone().unwrap_or_default();

    // JS runs in browser, can compute hostname dynamically.
    // If publicApiBase is provided, use it as-is.
    let js = if !public_base.trim().is_empty() {
        format!(
            "window.BLOCKCELL_API_BASE = {};\nwindow.BLOCKCELL_WS_URL = (window.BLOCKCELL_API_BASE.startsWith('https://') ? 'wss://' : 'ws://') + window.BLOCKCELL_API_BASE.replace(/^https?:\\/\\//, '') + '/v1/ws';\n",
            serde_json::to_string(&public_base).unwrap_or_else(|_| "\"\"".to_string())
        )
    } else {
        format!(
            "(function(){{\n  var proto = window.location.protocol;\n  var host = window.location.hostname;\n  var apiPort = {};\n  window.BLOCKCELL_API_BASE = proto + '//' + host + ':' + apiPort;\n  var wsProto = (proto === 'https:') ? 'wss://' : 'ws://';\n  window.BLOCKCELL_WS_URL = wsProto + host + ':' + apiPort + '/v1/ws';\n}})();\n",
            api_port
        )
    };

    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                "application/javascript; charset=utf-8".to_string(),
            ),
            (
                header::CACHE_CONTROL,
                "no-store, no-cache, must-revalidate".to_string(),
            ),
        ],
        js,
    )
}
