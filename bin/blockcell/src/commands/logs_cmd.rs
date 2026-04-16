use blockcell_core::Paths;

/// Check if file is a log file (agent.log or agent.log.YYYY-MM-DD)
fn is_log_file(path: &std::path::Path) -> bool {
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    file_name == "agent.log"
        || file_name.starts_with("agent.log.")
        || file_name.ends_with(".jsonl")
}

/// Show recent agent logs.
pub async fn show(
    lines: usize,
    filter: Option<String>,
    session: Option<String>,
) -> anyhow::Result<()> {
    let paths = Paths::default();
    let logs_dir = paths.logs_dir();

    if !logs_dir.exists() {
        println!("(No logs. Logs are generated automatically when the agent runs.)");
        return Ok(());
    }

    // Find log files, sorted by modification time (newest first)
    let mut log_files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_log_file(&path) {
                log_files.push(path);
            }
        }
    }

    if log_files.is_empty() {
        println!("(No log files)");
        return Ok(());
    }

    log_files.sort_by(|a, b| {
        let ta = std::fs::metadata(a).and_then(|m| m.modified()).ok();
        let tb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
        tb.cmp(&ta)
    });

    // Read the most recent log file
    let log_file = &log_files[0];
    let content = std::fs::read_to_string(log_file)?;
    let all_lines: Vec<&str> = content.lines().collect();

    // Filter by session and/or keyword
    let filtered: Vec<&&str> = all_lines
        .iter()
        .filter(|line| {
            let sess_ok = session.as_deref().map(|s| line.contains(s)).unwrap_or(true);
            let filter_ok = filter
                .as_deref()
                .map(|f| line.to_lowercase().contains(&f.to_lowercase()))
                .unwrap_or(true);
            sess_ok && filter_ok
        })
        .collect();

    let start = filtered.len().saturating_sub(lines);
    let tail = &filtered[start..];

    println!(
        "📋 Logs: {} (last {} lines)",
        log_file.display(),
        tail.len()
    );
    println!();

    for line in tail {
        println!("{}", line);
    }

    if log_files.len() > 1 {
        println!();
        println!("({} log files total, showing latest)", log_files.len());
    }

    Ok(())
}

/// Follow logs in real-time (tail -f style).
pub async fn follow(filter: Option<String>, session: Option<String>) -> anyhow::Result<()> {
    let paths = Paths::default();
    let logs_dir = paths.logs_dir();

    if !logs_dir.exists() {
        println!("(No logs directory. Start the agent first.)");
        return Ok(());
    }

    // Find the most recent log file
    let mut log_files: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_log_file(&path) {
                log_files.push(path);
            }
        }
    }

    if log_files.is_empty() {
        println!("(No log files)");
        return Ok(());
    }

    log_files.sort_by(|a, b| {
        let ta = std::fs::metadata(a).and_then(|m| m.modified()).ok();
        let tb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
        tb.cmp(&ta)
    });

    let log_file = &log_files[0];
    println!("📋 Following logs: {} (Ctrl+C to exit)", log_file.display());
    println!();

    // Simple tail -f implementation
    let mut last_size = std::fs::metadata(log_file).map(|m| m.len()).unwrap_or(0);

    // Print last 10 lines first
    let content = std::fs::read_to_string(log_file)?;
    let all_lines: Vec<&str> = content.lines().collect();
    let start = all_lines.len().saturating_sub(10);
    for line in &all_lines[start..] {
        let sess_ok = session.as_deref().map(|s| line.contains(s)).unwrap_or(true);
        let filter_ok = filter
            .as_deref()
            .map(|f| line.to_lowercase().contains(&f.to_lowercase()))
            .unwrap_or(true);
        if sess_ok && filter_ok {
            println!("{}", line);
        }
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let current_size = std::fs::metadata(log_file).map(|m| m.len()).unwrap_or(0);
        if current_size > last_size {
            // Read new content
            let file = std::fs::File::open(log_file)?;
            use std::io::{Read, Seek, SeekFrom};
            let mut file = std::io::BufReader::new(file);
            file.seek(SeekFrom::Start(last_size))?;
            let mut new_content = String::new();
            file.read_to_string(&mut new_content)?;

            for line in new_content.lines() {
                let sess_ok = session.as_deref().map(|s| line.contains(s)).unwrap_or(true);
                let filter_ok = filter
                    .as_deref()
                    .map(|f| line.to_lowercase().contains(&f.to_lowercase()))
                    .unwrap_or(true);
                if sess_ok && filter_ok {
                    println!("{}", line);
                }
            }

            last_size = current_size;
        }
    }
}

/// Clear log files.
pub async fn clear(force: bool) -> anyhow::Result<()> {
    use blockcell_core::logging::clear_all_logs;

    let paths = Paths::default();
    let logs_dir = paths.logs_dir();

    if !logs_dir.exists() {
        println!("(No logs)");
        return Ok(())
    }

    if !force {
        print!("⚠ Clear all logs? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let (count, size) = clear_all_logs(&logs_dir);
    let size_mb = size as f64 / 1024.0 / 1024.0;
    println!("✓ Cleared {} log file(s) ({:.2} MB)", count, size_mb);
    Ok(())
}
