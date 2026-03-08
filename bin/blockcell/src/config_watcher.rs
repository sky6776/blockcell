use anyhow::Result;
use blockcell_core::config::{validate_config_json5_str, Config};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info};

/// 配置文件监听器，支持热加载
pub struct ConfigWatcher {
    config_path: PathBuf,
    config: Arc<RwLock<Config>>,
}

impl ConfigWatcher {
    pub fn new(config_path: PathBuf, config: Arc<RwLock<Config>>) -> Self {
        Self {
            config_path,
            config,
        }
    }

    /// 启动配置文件监听
    pub async fn start(self) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel(10);
        let config_path = self.config_path.clone();

        // 创建文件监听器
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    if event.kind.is_modify() || event.kind.is_create() {
                        let _ = tx.blocking_send(());
                    }
                }
            })?;

        // 监听配置文件目录
        if let Some(parent) = config_path.parent() {
            watcher.watch(parent, RecursiveMode::NonRecursive)?;
        }

        info!(
            "📡 Config watcher started, monitoring: {}",
            config_path.display()
        );

        // 防抖：避免短时间内多次重载
        let mut last_reload = std::time::Instant::now();
        let debounce_duration = Duration::from_secs(1);

        loop {
            tokio::select! {
                Some(_) = rx.recv() => {
                    let now = std::time::Instant::now();
                    if now.duration_since(last_reload) < debounce_duration {
                        continue;
                    }
                    last_reload = now;

                    // 延迟一小段时间，确保文件写入完成
                    tokio::time::sleep(Duration::from_millis(200)).await;

                    match self.reload_config().await {
                        Ok(()) => {
                            info!("✅ Config reloaded successfully");
                        }
                        Err(e) => {
                            error!("❌ Failed to reload config: {}", e);
                        }
                    }
                }
            }
        }
    }

    /// 重新加载配置文件
    async fn reload_config(&self) -> Result<()> {
        let content = tokio::fs::read_to_string(&self.config_path).await?;
        let new_config: Config = validate_config_json5_str(&content)
            .map_err(|e| anyhow::anyhow!("Invalid JSON5 config: {}", e))?;

        // 验证通过，更新内存中的配置
        let mut config = self.config.write().await;
        *config = new_config;

        Ok(())
    }
}
