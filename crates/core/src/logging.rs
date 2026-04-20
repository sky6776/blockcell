//! # 日志系统
//!
//! 提供可动态控制的日志输出系统：
//! - 控制台输出（可开关，默认开启）
//! - 文件输出（可开关，默认开启，按日期滚动）
//! - 日志等级动态调整（trace/debug/info/warn/error/off）
//! - 模块过滤（如 blockcell_agent=trace）

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use tracing::Subscriber;
use tracing_appender::rolling::RollingFileAppender;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{
    layer::{Context, Layer},
    reload, EnvFilter, Registry,
};

/// 全局日志控制器单例
pub static LOG_CONTROLLER: OnceLock<LogController> = OnceLock::new();

/// 日志控制器
pub struct LogController {
    /// EnvFilter reload handle
    filter_handle: reload::Handle<EnvFilter, Registry>,
    /// Console output switch (default: true)
    console_enabled: Arc<Mutex<bool>>,
    /// File output switch (default: true)
    file_enabled: Arc<Mutex<bool>>,
    /// Current log file path
    current_file: Arc<Mutex<String>>,
}

/// 日志状态
pub struct LogStatus {
    pub level: String,
    pub module_filters: Vec<String>,
    pub console_enabled: bool,
    pub file_enabled: bool,
    pub log_file: String,
}

impl LogController {
    /// 设置全局日志等级
    pub fn set_level(&self, level: &str) -> Result<(), String> {
        let new_filter = match level {
            "trace" => EnvFilter::new("trace"),
            "debug" => EnvFilter::new("debug"),
            "info" => EnvFilter::new("info"),
            "warn" => EnvFilter::new("warn"),
            "error" => EnvFilter::new("error"),
            "off" => EnvFilter::new("off"),
            other => return Err(format!("Unknown log level: {}", other)),
        };

        self.filter_handle
            .reload(new_filter)
            .map_err(|e| format!("Failed to reload filter: {}", e))?;

        Ok(())
    }

    /// 设置模块过滤
    pub fn set_filter(&self, filter: &str) -> Result<(), String> {
        let new_filter = EnvFilter::new(filter);

        self.filter_handle
            .reload(new_filter)
            .map_err(|e| format!("Failed to reload filter: {}", e))?;

        Ok(())
    }

    /// 切换控制台输出（独立控制，不影响文件）
    pub fn set_console(&self, enabled: bool) {
        *self.console_enabled.lock().unwrap() = enabled;
    }

    /// 切换文件输出（独立控制，不影响控制台）
    pub fn set_file(&self, enabled: bool) {
        *self.file_enabled.lock().unwrap() = enabled;
    }

    /// 获取当前状态
    pub fn status(&self) -> LogStatus {
        let current_filter = self
            .filter_handle
            .with_current(|f| f.to_string())
            .unwrap_or_default();

        let parts: Vec<&str> = current_filter.split(',').collect();
        let level = parts
            .first()
            .map(|s| s.split('=').next().unwrap_or("info"))
            .unwrap_or("info")
            .to_string();

        let module_filters = parts
            .iter()
            .filter(|p| p.contains('='))
            .map(|s| s.to_string())
            .collect();

        LogStatus {
            level,
            module_filters,
            console_enabled: *self.console_enabled.lock().unwrap(),
            file_enabled: *self.file_enabled.lock().unwrap(),
            log_file: self.current_file.lock().unwrap().clone(),
        }
    }
}

/// 可开关的控制台输出层
pub struct SwitchableConsoleLayer {
    enabled: Arc<Mutex<bool>>,
}

impl SwitchableConsoleLayer {
    pub fn new(enabled: Arc<Mutex<bool>>) -> Self {
        Self { enabled }
    }
}

impl<S> Layer<S> for SwitchableConsoleLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if !*self.enabled.lock().unwrap() {
            return;
        }

        let mut stdout = std::io::stdout().lock();

        let now = chrono::Local::now();
        let timestamp = now.format("%Y-%m-%d %H:%M:%S%.3f");

        let level = event.metadata().level();
        let module = event.metadata().module_path().unwrap_or("unknown");

        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);

        if visitor.fields.is_empty() {
            let _ = writeln!(
                stdout,
                "{} [{}] {}: {}",
                timestamp, level, module, visitor.message
            );
        } else {
            let _ = writeln!(
                stdout,
                "{} [{}] {}: {} | {}",
                timestamp, level, module, visitor.message, visitor.fields
            );
        }
    }
}

/// 可开关的文件输出层
pub struct SwitchableFileLayer {
    enabled: Arc<Mutex<bool>>,
    writer: Arc<Mutex<RollingFileAppender>>,
}

impl SwitchableFileLayer {
    pub fn new(enabled: Arc<Mutex<bool>>, writer: RollingFileAppender) -> Self {
        Self {
            enabled,
            writer: Arc::new(Mutex::new(writer)),
        }
    }
}

impl<S> Layer<S> for SwitchableFileLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        if !*self.enabled.lock().unwrap() {
            return;
        }

        let now = chrono::Local::now();
        let timestamp = now.format("%Y-%m-%d %H:%M:%S%.3f");

        let level = event.metadata().level();
        let module = event.metadata().module_path().unwrap_or("unknown");

        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);

        let mut writer = self.writer.lock().unwrap();
        if visitor.fields.is_empty() {
            let _ = writeln!(
                writer,
                "{} [{}] {}: {}",
                timestamp, level, module, visitor.message
            );
        } else {
            let _ = writeln!(
                writer,
                "{} [{}] {}: {} | {}",
                timestamp, level, module, visitor.message, visitor.fields
            );
        }
    }
}

/// 消息访问器
struct MessageVisitor {
    message: String,
    /// 非消息字段，格式: key1=val1, key2=val2
    fields: String,
}

impl MessageVisitor {
    fn new() -> Self {
        Self {
            message: String::new(),
            fields: String::new(),
        }
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
        } else {
            if !self.fields.is_empty() {
                self.fields.push_str(", ");
            }
            self.fields.push_str(&format!("{}={:?}", field.name(), value));
        }
    }
}

/// 初始化日志系统
/// 参数：
/// - logs_dir: 日志目录路径
/// - level: 日志等级 (trace/debug/info/warn/error/off)
/// - console_enabled: 是否输出到控制台
/// - file_enabled: 是否输出到文件
pub fn init_logging(
    logs_dir: &Path,
    level: &str,
    console_enabled: bool,
    file_enabled: bool,
) -> Result<(), String> {
    use tracing_subscriber::prelude::*;

    if let Err(e) = std::fs::create_dir_all(logs_dir) {
        return Err(format!("Failed to create logs directory: {}", e));
    }

    let file_appender = RollingFileAppender::new(Rotation::DAILY, logs_dir, "agent.log");

    let filter = EnvFilter::new(level);
    let (filter_layer, filter_handle) = reload::Layer::new(filter);

    let console_enabled_flag = Arc::new(Mutex::new(console_enabled));
    let file_enabled_flag = Arc::new(Mutex::new(file_enabled));

    let console_layer = SwitchableConsoleLayer::new(console_enabled_flag.clone());
    let file_layer = SwitchableFileLayer::new(file_enabled_flag.clone(), file_appender);

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(console_layer)
        .with(file_layer)
        .init();

    let controller = LogController {
        filter_handle,
        console_enabled: console_enabled_flag,
        file_enabled: file_enabled_flag,
        current_file: Arc::new(Mutex::new(logs_dir.join("agent.log").display().to_string())),
    };

    LOG_CONTROLLER
        .set(controller)
        .map_err(|_| "Log controller already initialized")?;

    Ok(())
}

/// 清理旧日志文件（超过 retention_days 天）
pub fn cleanup_old_logs(logs_dir: &Path, retention_days: u64) {
    let cutoff = SystemTime::now() - Duration::from_secs(retention_days * 86400);

    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // 匹配 agent.log 或 agent.log.YYYY-MM-DD 格式
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_log_file = file_name == "agent.log" || file_name.starts_with("agent.log.");

            if path.is_file() && is_log_file {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(time) = metadata.modified() {
                        if time < cutoff {
                            let _ = std::fs::remove_file(&path);
                        }
                    }
                }
            }
        }
    }
}

/// 清理所有日志文件，返回 (成功删除数, 删除的总大小)
pub fn clear_all_logs(logs_dir: &Path) -> (usize, u64) {
    let mut count = 0;
    let mut total_size = 0u64;

    if let Ok(entries) = std::fs::read_dir(logs_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            // 匹配 agent.log 或 agent.log.YYYY-MM-DD 格式
            let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let is_log_file = file_name == "agent.log" || file_name.starts_with("agent.log.");

            if path.is_file() && is_log_file {
                // 先获取文件大小
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                // 尝试删除
                if std::fs::remove_file(&path).is_ok() {
                    count += 1;
                    total_size += size;
                } else {
                    // 如果删除失败（可能文件正在被写入），尝试清空内容
                    if std::fs::write(&path, "").is_ok() {
                        count += 1;
                        total_size += size;
                    }
                }
            }
        }
    }

    (count, total_size)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    #[test]
    fn test_logs_dir_path() {
        let dir = PathBuf::from("/tmp/logs");
        assert!(dir.ends_with("logs"));
    }
}
