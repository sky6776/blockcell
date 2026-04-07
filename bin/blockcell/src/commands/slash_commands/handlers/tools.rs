//! # /tools 命令
//!
//! 列出所有注册工具。

use crate::commands::slash_commands::*;
use blockcell_core::CapabilityDescriptor;

/// 内置工具列表
const BUILTIN_TOOLS: &[(&str, &[(&str, &str)])] = &[
    (
        "📁 Filesystem",
        &[
            ("read_file", "Read files (text/Office/PDF)"),
            ("write_file", "Create and write files"),
            ("edit_file", "Precise file content editing"),
            ("list_dir", "Browse directory structure"),
            ("file_ops", "Delete/move/copy/compress/decompress/PDF"),
        ],
    ),
    (
        "⚡ Commands & System",
        &[
            ("exec", "Execute shell commands"),
            ("system_info", "Hardware/software/network detection"),
        ],
    ),
    (
        "🌐 Web & Browser",
        &[
            ("web_search", "Search engine queries"),
            ("web_fetch", "Fetch web page content"),
            ("browse", "CDP browser automation (35+ actions)"),
            ("http_request", "Generic HTTP/REST API calls"),
        ],
    ),
    (
        "🖥️ GUI Automation",
        &[("app_control", "macOS app control (System Events)")],
    ),
    (
        "🎨 Media",
        &[
            ("camera_capture", "Camera capture"),
            ("audio_transcribe", "Speech-to-text (Whisper/API)"),
            ("tts", "Text-to-speech"),
            ("ocr", "Image text recognition"),
            ("image_understand", "Multimodal image understanding"),
            ("video_process", "Video processing (ffmpeg)"),
            ("chart_generate", "Chart generation (matplotlib/plotly)"),
        ],
    ),
    (
        "📊 Data Processing",
        &[
            ("data_process", "CSV read/write/stats/query/transform"),
            ("office_write", "Generate PPTX/DOCX/XLSX documents"),
            ("knowledge_graph", "Knowledge graph operations"),
        ],
    ),
    (
        "📬 Communication",
        &[
            ("email", "Email send/receive (SMTP/IMAP)"),
            ("message", "Channel messaging (Telegram/Slack/Discord)"),
        ],
    ),
];

/// /tools 命令 - 列出所有注册工具
pub struct ToolsCommand;

#[async_trait::async_trait]
impl SlashCommand for ToolsCommand {
    fn name(&self) -> &str {
        "tools"
    }

    fn description(&self) -> &str {
        "List all registered tools"
    }

    async fn execute(&self, _args: &str, ctx: &CommandContext) -> CommandResult {
        let content = print_tools_status(&ctx.paths);
        CommandResult::Handled(CommandResponse::markdown(content))
    }
}

/// 打印工具状态
fn print_tools_status(paths: &blockcell_core::Paths) -> String {
    let total_tools: usize = BUILTIN_TOOLS.iter().map(|(_, items)| items.len()).sum();

    let mut content = String::new();
    content.push_str(&format!(
        "🔌 **Built-in tools** ({} total, {} categories)\n\n",
        total_tools,
        BUILTIN_TOOLS.len()
    ));

    for (category, items) in BUILTIN_TOOLS {
        content.push_str(&format!("**{}** ({})\n", category, items.len()));
        for (name, desc) in *items {
            content.push_str(&format!("- ✅ {} — {}\n", name, desc));
        }
        content.push('\n');
    }

    // 动态演化工具
    let cap_file = paths
        .workspace()
        .join("evolved_tools")
        .join("evolved_tools.json");
    if cap_file.exists() {
        if let Ok(file_content) = std::fs::read_to_string(&cap_file) {
            if let Ok(caps) = serde_json::from_str::<Vec<CapabilityDescriptor>>(&file_content) {
                if !caps.is_empty() {
                    let active = caps.iter().filter(|c| c.is_available()).count();
                    content.push_str(&format!(
                        "🧬 **Dynamic evolved tools** ({}, {} available)\n",
                        caps.len(),
                        active
                    ));
                    for cap in &caps {
                        let icon = match format!("{:?}", cap.status).as_str() {
                            "Active" => "✅",
                            "Available" | "Discovered" => "🟢",
                            "Loading" | "Evolving" => "⏳",
                            _ => "❌",
                        };
                        content.push_str(&format!(
                            "- {} {} v{} — {}\n",
                            icon, cap.id, cap.version, cap.description
                        ));
                    }
                    content.push('\n');
                }
            }
        }
    }

    // Core evolution 记录
    let evo_dir = paths.workspace().join("tool_evolution_records");
    if evo_dir.exists() {
        let mut evo_count = 0;
        let mut active_count = 0;
        if let Ok(entries) = std::fs::read_dir(&evo_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "json") {
                    evo_count += 1;
                    if let Ok(content) = std::fs::read_to_string(entry.path()) {
                        if content.contains("\"Active\"") {
                            active_count += 1;
                        }
                    }
                }
            }
        }
        if evo_count > 0 {
            content.push_str(&format!(
                "🧬 **Core evolution**: {} records ({} active)\n\n",
                evo_count, active_count
            ));
        }
    }

    content.push_str("💡 `/skills` view skills | `capability_evolve` tool to learn new tools\n");

    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tools_command() {
        let cmd = ToolsCommand;
        let ctx = CommandContext::test_context();

        let result = cmd.execute("", &ctx).await;
        assert!(matches!(result, CommandResult::Handled(_)));

        if let CommandResult::Handled(response) = result {
            assert!(response.content.contains("Built-in tools"));
        }
    }
}