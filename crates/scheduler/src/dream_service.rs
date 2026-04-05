//! 梦境服务 - Layer 6 定期触发
//!
//! 后台定期检查门控条件，执行跨会话知识整合。
//!
//! ## 触发机制
//! - 定期检查（默认每 10 分钟）
//! - 检查三重门控：时间、会话数、锁
//! - 所有门控通过时执行 Dream

use crate::consolidator::{DreamConsolidator, GateCheckResult};
use blockcell_core::Paths;
use blockcell_providers::ProviderPool;
use std::sync::Arc;
use tokio::sync::broadcast;

/// 默认检查间隔（秒）
///
/// Dream Service 默认每 10 分钟检查一次门控条件。
/// 此值可通过 `DreamServiceConfig::check_interval_secs` 覆盖。
pub const DEFAULT_CHECK_INTERVAL_SECS: u64 = 10 * 60; // 10 分钟

/// Dream 服务配置
#[derive(Clone)]
pub struct DreamServiceConfig {
    /// 是否启用
    pub enabled: bool,
    /// 检查间隔（秒）
    pub check_interval_secs: u64,
    /// Provider 池（用于 Forked Agent LLM 调用）
    pub provider_pool: Option<Arc<ProviderPool>>,
}

impl std::fmt::Debug for DreamServiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DreamServiceConfig")
            .field("enabled", &self.enabled)
            .field("check_interval_secs", &self.check_interval_secs)
            .field("provider_pool", &self.provider_pool.is_some())
            .finish()
    }
}

impl Default for DreamServiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_interval_secs: DEFAULT_CHECK_INTERVAL_SECS,
            provider_pool: None,
        }
    }
}

/// 梦境服务
pub struct DreamService {
    config: DreamServiceConfig,
    paths: Paths,
}

impl DreamService {
    /// 创建梦境服务
    pub fn new(config: DreamServiceConfig, paths: Paths) -> Self {
        Self { config, paths }
    }

    /// 设置 Provider 池
    pub fn with_provider_pool(mut self, pool: Arc<ProviderPool>) -> Self {
        self.config.provider_pool = Some(pool);
        self
    }

    /// 运行梦境服务主循环
    pub async fn run_loop(self, mut shutdown: broadcast::Receiver<()>) {
        if !self.config.enabled {
            tracing::info!("[dream] DreamService disabled");
            return;
        }

        // 启动时检查 provider_pool 配置
        if self.config.provider_pool.is_none() {
            tracing::warn!(
                concat!(
                    "[dream] ⚠️  DreamService started WITHOUT provider_pool! ",
                    "Dream consolidation will be skipped. ",
                    "To enable dream consolidation, configure a provider in your config."
                )
            );
        } else {
            tracing::info!(
                check_interval_secs = self.config.check_interval_secs,
                has_provider_pool = true,
                "[dream] DreamService started"
            );
        }

        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(
            self.config.check_interval_secs,
        ));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    self.check_and_run().await;
                }
                _ = shutdown.recv() => {
                    tracing::info!("[dream] DreamService shutting down");
                    break;
                }
            }
        }
    }

    /// 检查门控并执行梦境
    async fn check_and_run(&self) {
        // 使用 paths.base 作为配置目录
        let config_dir = self.paths.base.clone();

        // 创建 DreamConsolidator
        let consolidator = match DreamConsolidator::new(&config_dir).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "[dream] Failed to create DreamConsolidator");
                return;
            }
        };

        // 检查门控
        let gate_result = consolidator.should_dream();

        match gate_result {
            GateCheckResult::Passed => {
                tracing::info!("[dream] All gates passed, starting consolidation");

                // 检查 provider_pool
                let provider_pool = match &self.config.provider_pool {
                    Some(p) => p.clone(),
                    None => {
                        tracing::warn!("[dream] No provider pool configured, skipping dream");
                        return;
                    }
                };

                // 设置 provider_pool 并执行
                let mut consolidator = consolidator.with_provider_pool(provider_pool);

                match consolidator.dream().await {
                    Ok(()) => {
                        tracing::info!("[dream] Consolidation completed successfully");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "[dream] Consolidation failed");
                    }
                }
            }
            GateCheckResult::TimeGateFailed => {
                tracing::debug!("[dream] Time gate not passed, skipping");
            }
            GateCheckResult::SessionGateFailed => {
                tracing::debug!("[dream] Session gate not passed, skipping");
            }
            GateCheckResult::LockGateFailed => {
                tracing::debug!("[dream] Lock gate not passed (another process is consolidating)");
            }
        }
    }

    /// 手动触发梦境（用于测试或管理命令）
    pub async fn trigger_dream(paths: &Paths, provider_pool: Arc<ProviderPool>) -> Result<(), String> {
        let config_dir = paths.base.clone();

        let consolidator = DreamConsolidator::new(&config_dir)
            .await
            .map_err(|e| format!("Failed to create DreamConsolidator: {}", e))?;

        let mut consolidator = consolidator.with_provider_pool(provider_pool);

        consolidator
            .dream()
            .await
            .map_err(|e| format!("Dream failed: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_check_interval_constant() {
        // 验证常量值为 10 分钟
        assert_eq!(DEFAULT_CHECK_INTERVAL_SECS, 10 * 60);
    }

    #[test]
    fn test_dream_service_config_default() {
        let config = DreamServiceConfig::default();
        assert!(config.enabled);
        assert_eq!(config.check_interval_secs, DEFAULT_CHECK_INTERVAL_SECS);
        assert!(config.provider_pool.is_none());
    }

    #[test]
    fn test_dream_service_config_debug() {
        let config = DreamServiceConfig::default();
        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("enabled: true"));
        assert!(debug_str.contains("check_interval_secs: 600"));
    }

    #[test]
    fn test_dream_service_new() {
        let config = DreamServiceConfig::default();
        let paths = Paths::new();
        let service = DreamService::new(config, paths.clone());

        // 验证服务创建成功
        assert!(service.config.enabled);
    }

    #[test]
    fn test_dream_service_disabled() {
        let config = DreamServiceConfig {
            enabled: false,
            ..Default::default()
        };
        let paths = Paths::new();
        let service = DreamService::new(config, paths);

        assert!(!service.config.enabled);
    }

    #[test]
    fn test_dream_service_custom_interval() {
        let config = DreamServiceConfig {
            check_interval_secs: 5 * 60, // 5 分钟
            ..Default::default()
        };
        let paths = Paths::new();
        let service = DreamService::new(config, paths);

        assert_eq!(service.config.check_interval_secs, 300);
    }
}