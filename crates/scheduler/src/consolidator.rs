//! 梦境机制 - Layer 6 知识整合
//!
//! 后台跨会话知识整合，使用三重门控机制。
//!
//! ## 三重门控
//! 1. 时间门控：距上次整合 > 24 小时
//! 2. 会话门控：新会话数 > 5
//! 3. 锁门控：无其他进程正在整合
//!
//! ## 四阶段执行
//! 1. Orient - 定位现有内容
//! 2. Gather - 收集新信号
//! 3. Consolidate - 整合知识（使用 Forked Agent）
//! 4. Prune - 修剪索引

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::fs;
use serde::{Deserialize, Serialize};
use blockcell_core::types::ChatMessage;
use blockcell_agent::forked::{
    run_forked_agent, ForkedAgentParams, CacheSafeParams,
    create_dream_can_use_tool,
};
use blockcell_agent::memory_event;

/// 门控配置
pub const TIME_GATE_THRESHOLD_HOURS: u64 = 24;
pub const SESSION_GATE_THRESHOLD: usize = 5;
pub const LOCK_FILE_NAME: &str = ".dream_lock";
pub const DREAM_STATE_FILE: &str = ".dream_state.json";

/// Session Memory 过期阈值（天）
pub const SESSION_MEMORY_EXPIRY_DAYS: u64 = 7;
/// 每次处理的最大 session memory 文件数
pub const MAX_SESSIONS_TO_PROCESS: usize = 10;

/// Dream 执行统计数据
#[derive(Debug, Clone, Default)]
pub struct DreamStats {
    /// 创建的记忆数
    pub memories_created: usize,
    /// 更新的记忆数
    pub memories_updated: usize,
    /// 删除的记忆数
    pub memories_deleted: usize,
    /// 修剪的会话数
    pub sessions_pruned: usize,
    /// 处理的会话数
    pub sessions_processed: usize,
}

/// Memory 目录状态快照
#[derive(Debug, Clone, Default)]
struct MemoryDirState {
    /// 文件数量 (保留用于未来日志/指标)
    #[allow(dead_code)]
    file_count: usize,
    /// 总字节数 (保留用于未来日志/指标)
    #[allow(dead_code)]
    total_bytes: u64,
    /// 文件名 -> 修改时间映射
    file_mtimes: std::collections::HashMap<String, u64>,
}

/// 梦境状态
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DreamState {
    /// 上次整合时间戳
    pub last_consolidation_time: Option<u64>,
    /// 上次整合时的会话数
    pub last_session_count: usize,
    /// 当前会话数
    pub current_session_count: usize,
    /// 整合次数
    pub consolidation_count: usize,
    /// 是否正在整合
    pub is_consolidating: bool,
}

impl DreamState {
    /// 加载状态
    pub async fn load(config_dir: &Path) -> std::io::Result<Self> {
        let path = config_dir.join(DREAM_STATE_FILE);
        match fs::read_to_string(&path).await {
            Ok(content) => {
                match serde_json::from_str(&content) {
                    Ok(state) => Ok(state),
                    Err(e) => {
                        // JSON 解析失败，可能文件损坏，记录警告并使用默认值
                        tracing::warn!(
                            error = %e,
                            path = %path.display(),
                            "[dream] Failed to parse dream state file, using defaults (file may be corrupted)"
                        );
                        Ok(Self::default())
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self::default())
            }
            Err(e) => Err(e),
        }
    }

    /// 保存状态
    pub async fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        let path = config_dir.join(DREAM_STATE_FILE);
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&path, content).await
    }

    /// 增加会话计数
    pub fn increment_session_count(&mut self) {
        self.current_session_count += 1;
    }
}

/// 收集到的信号
#[derive(Debug, Clone)]
pub struct GatheredSignal {
    /// 信号标题（章节名）
    pub title: String,
    /// 信号内容
    pub content: String,
    /// 重要性分数 (0-10)
    pub importance: u8,
    /// 来源时间
    pub source_time: SystemTime,
}

/// 门控检查结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateCheckResult {
    /// 通过所有门控
    Passed,
    /// 时间门控未通过
    TimeGateFailed,
    /// 会话门控未通过
    SessionGateFailed,
    /// 锁门控未通过（有其他进程正在整合）
    LockGateFailed,
}

/// 三重门控检查
pub fn check_gates(state: &DreamState, _config_dir: &Path) -> GateCheckResult {
    // 1. 检查锁门控
    if state.is_consolidating {
        return GateCheckResult::LockGateFailed;
    }

    // 2. 检查时间门控
    if let Some(last_time) = state.last_consolidation_time {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let hours_since_last = (now - last_time) / 3600;

        if hours_since_last < TIME_GATE_THRESHOLD_HOURS {
            return GateCheckResult::TimeGateFailed;
        }
    } else {
        // 从未整合过，时间门控通过
    }

    // 3. 检查会话门控
    let new_sessions = state.current_session_count.saturating_sub(state.last_session_count);
    if new_sessions < SESSION_GATE_THRESHOLD {
        return GateCheckResult::SessionGateFailed;
    }

    GateCheckResult::Passed
}

/// 梦境执行器
pub struct DreamConsolidator {
    /// 配置目录
    config_dir: PathBuf,
    /// 当前状态
    state: DreamState,
    /// Provider 池（用于 Forked Agent LLM 调用）
    provider_pool: Option<Arc<blockcell_providers::ProviderPool>>,
}

impl DreamConsolidator {
    /// 创建执行器
    pub async fn new(config_dir: &Path) -> std::io::Result<Self> {
        let state = DreamState::load(config_dir).await?;
        Ok(Self {
            config_dir: config_dir.to_path_buf(),
            state,
            provider_pool: None,
        })
    }

    /// 设置 Provider 池
    ///
    /// 必须在调用 `dream()` 之前设置，否则 Forked Agent 无法执行 LLM 调用
    pub fn with_provider_pool(mut self, provider_pool: Arc<blockcell_providers::ProviderPool>) -> Self {
        self.provider_pool = Some(provider_pool);
        self
    }

    /// 检查是否应该执行梦境
    pub fn should_dream(&self) -> GateCheckResult {
        check_gates(&self.state, &self.config_dir)
    }

    /// 执行梦境整合
    pub async fn dream(&mut self) -> Result<(), DreamError> {
        // 获取锁
        self.acquire_lock().await?;

        // 记录 Layer 6 dream_started 事件
        let sessions_count = self.state.current_session_count;
        let hours_since_last = self.state.last_consolidation_time
            .map(|t| {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (now.saturating_sub(t)) / 3600
            })
            .unwrap_or(24);
        memory_event!(layer6, dream_started, sessions_count, hours_since_last);

        // 标记开始
        self.state.is_consolidating = true;
        if let Err(e) = self.state.save(&self.config_dir).await {
            // 保存失败，重置状态并释放锁
            self.state.is_consolidating = false;
            let _ = self.release_lock().await;
            return Err(DreamError::Io(e));
        }

        let start_time = Instant::now();

        // 在 consolidate 前扫描 memory 目录
        let memory_dir = self.config_dir.join("memory");
        let pre_memory_state = self.scan_memory_dir(&memory_dir).await;

        // 执行四阶段，收集统计
        let mut stats = DreamStats::default();
        let result = async {
            self.orient().await?;
            let signals = self.gather().await?;
            self.consolidate(&signals).await?;
            // 在 consolidate 后计算 memory 变化
            let post_memory_state = self.scan_memory_dir(&memory_dir).await;
            stats = self.compute_memory_diff(&pre_memory_state, &post_memory_state);
            // prune 返回修剪统计
            let prune_stats = self.prune().await?;
            stats.sessions_pruned = prune_stats.sessions_pruned;
            stats.sessions_processed = prune_stats.sessions_processed;
            Ok::<(), DreamError>(())
        }
        .await;

        // 无论成功或失败，都要更新状态并释放锁
        self.state.is_consolidating = false;
        self.state.last_consolidation_time = Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        self.state.last_session_count = self.state.current_session_count;

        // 只有成功时才增加计数
        if result.is_ok() {
            self.state.consolidation_count += 1;
        }

        // 保存最终状态（失败时记录警告但继续）
        if let Err(e) = self.state.save(&self.config_dir).await {
            tracing::warn!(
                error = %e,
                "[dream] Failed to save final state"
            );
        }

        // 释放锁（失败时记录警告但继续）
        if let Err(e) = self.release_lock().await {
            tracing::warn!(
                error = %e,
                "[dream] Failed to release lock"
            );
        }

        let elapsed = start_time.elapsed();
        match &result {
            Ok(()) => {
                // 记录 Layer 6 dream_finished 事件（成功，传递实际统计数据）
                memory_event!(
                    layer6, dream_finished,
                    stats.memories_created,
                    stats.memories_updated,
                    stats.memories_deleted,
                    stats.sessions_pruned,
                    stats.sessions_processed
                );
                tracing::info!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    consolidation_count = self.state.consolidation_count,
                    memories_created = stats.memories_created,
                    memories_updated = stats.memories_updated,
                    sessions_pruned = stats.sessions_pruned,
                    "[dream] consolidation completed"
                );
            }
            Err(e) => {
                tracing::error!(
                    elapsed_ms = elapsed.as_millis() as u64,
                    error = %e,
                    "[dream] consolidation failed"
                );
            }
        }

        result
    }

    /// 获取锁
    ///
    /// 使用原子 rename 操作避免 TOCTOU 竞争条件。
    /// 锁文件格式: `PID:TIMESTAMP`
    ///
    /// ## 算法
    /// 1. 先创建临时锁文件（带唯一标识）
    /// 2. 检查现有锁是否过期
    /// 3. 如果过期，尝试原子 rename（只有一个进程会成功）
    /// 4. 如果 rename 失败，说明另一个进程已获取锁
    async fn acquire_lock(&self) -> Result<(), DreamError> {
        use std::process;

        let lock_path = self.config_dir.join(LOCK_FILE_NAME);
        let temp_lock_path = self.config_dir.join(format!("{}.tmp.{}", LOCK_FILE_NAME, process::id()));
        let current_pid = process::id();
        let max_retries = 3;

        for attempt in 0..max_retries {
            // 1. 先创建临时锁文件（每个进程有自己的临时文件，无竞争）
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let lock_content = format!("{}:{}", current_pid, timestamp);

            // 确保配置目录存在
            if let Some(parent) = lock_path.parent() {
                fs::create_dir_all(parent).await?;
            }

            // 写入临时文件
            fs::write(&temp_lock_path, &lock_content).await?;

            // 2. 检查现有锁是否存在且有效
            match fs::try_exists(&lock_path).await {
                Ok(true) => {
                    // 锁文件存在，检查是否过期
                    match self.check_lock_validity(&lock_path).await {
                        Ok(true) => {
                            // 锁仍然有效，清理临时文件并返回
                            tracing::debug!(
                                attempt,
                                "[dream] Lock is held by another process"
                            );
                            let _ = fs::remove_file(&temp_lock_path).await;
                            return Err(DreamError::LockAcquired);
                        }
                        Ok(false) => {
                            // 锁已过期，尝试原子替换
                            // rename 在大多数平台上是原子的
                            match fs::rename(&temp_lock_path, &lock_path).await {
                                Ok(()) => {
                                    tracing::debug!(
                                        pid = current_pid,
                                        attempt,
                                        "[dream] Lock acquired (replaced stale lock)"
                                    );
                                    return Ok(());
                                }
                                Err(e) => {
                                    // rename 失败，可能另一个进程已获取锁
                                    tracing::warn!(
                                        error = %e,
                                        attempt,
                                        "[dream] Failed to replace stale lock, retrying"
                                    );
                                    let _ = fs::remove_file(&temp_lock_path).await;
                                    // 继续重试
                                }
                            }
                        }
                        Err(e) => {
                            // 无法读取锁文件，尝试替换
                            tracing::warn!(
                                error = %e,
                                "[dream] Cannot read lock file, attempting to replace"
                            );
                            match fs::rename(&temp_lock_path, &lock_path).await {
                                Ok(()) => {
                                    tracing::debug!(
                                        pid = current_pid,
                                        "[dream] Lock acquired (replaced corrupted lock)"
                                    );
                                    return Ok(());
                                }
                                Err(_e) => {
                                    let _ = fs::remove_file(&temp_lock_path).await;
                                }
                            }
                        }
                    }
                }
                Ok(false) => {
                    // 锁文件不存在，尝试创建
                    match fs::rename(&temp_lock_path, &lock_path).await {
                        Ok(()) => {
                            tracing::debug!(
                                pid = current_pid,
                                attempt,
                                "[dream] Lock acquired (new lock)"
                            );
                            return Ok(());
                        }
                        Err(e) => {
                            // rename 失败（可能另一个进程同时创建）
                            tracing::warn!(
                                error = %e,
                                attempt,
                                "[dream] Failed to create lock, retrying"
                            );
                            let _ = fs::remove_file(&temp_lock_path).await;
                            // 继续重试
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "[dream] Cannot check lock existence"
                    );
                    let _ = fs::remove_file(&temp_lock_path).await;
                    return Err(e.into());
                }
            }
        }

        // 重试次数耗尽
        tracing::error!(
            attempts = max_retries,
            "[dream] Failed to acquire lock after max retries"
        );
        // 清理临时文件
        let _ = fs::remove_file(&temp_lock_path).await;
        Err(DreamError::LockAcquired)
    }

    /// 检查锁的有效性
    ///
    /// 返回 Ok(true) 表示锁仍有效（进程存活且未过期）
    /// 返回 Ok(false) 表示锁已失效（进程已死或过期）
    async fn check_lock_validity(&self, lock_path: &Path) -> Result<bool, DreamError> {
        let content = fs::read_to_string(lock_path).await?;

        // 解析 PID:TIMESTAMP
        let parts: Vec<&str> = content.split(':').collect();
        if parts.len() != 2 {
            // 格式错误，锁无效
            return Ok(false);
        }

        // 检查时间戳是否过期
        let timestamp: u64 = parts[1].parse().unwrap_or(0);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let age_hours = (now - timestamp) / 3600;

        if age_hours >= TIME_GATE_THRESHOLD_HOURS {
            // 锁已过期
            tracing::debug!(
                age_hours,
                "[dream] Lock expired"
            );
            return Ok(false);
        }

        // 检查持有锁的进程是否仍在运行
        let pid: u32 = parts[0].parse().unwrap_or(0);
        if pid == 0 {
            return Ok(false);
        }

        // 跨平台进程存活检查
        let process_alive = self.is_process_alive(pid);

        Ok(process_alive)
    }

    /// 检查进程是否存活
    #[cfg(unix)]
    fn is_process_alive(&self, pid: u32) -> bool {
        // Unix: 使用 kill(pid, 0) 检查进程是否存在
        // ESRCH 表示进程不存在
        unsafe {
            let result = libc::kill(pid as i32, 0);
            result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }

    /// 检查进程是否存活
    #[cfg(windows)]
    fn is_process_alive(&self, pid: u32) -> bool {
        // Windows: 尝试打开进程
        use winapi::um::processthreadsapi::OpenProcess;
        use winapi::um::winnt::PROCESS_QUERY_INFORMATION;
        use winapi::um::processthreadsapi::GetExitCodeProcess;
        use winapi::um::handleapi::CloseHandle;

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }

            let mut exit_code: u32 = 0;
            let result = GetExitCodeProcess(handle, &mut exit_code);
            CloseHandle(handle);

            // STILL_ACTIVE (259) 表示进程仍在运行
            //
            // 已知限制：如果进程恰好以退出码 259 结束，会被误判为仍在运行。
            // 这在现实中极其罕见，因为：
            // 1. 259 不是常见的错误码
            // 2. 大多数程序使用 0 表示成功，非零值表示错误
            // 3. 即使发生误判，锁也会在 TIME_GATE_THRESHOLD_HOURS 小时后过期
            //
            // 如果需要更精确的检测，可以使用 WaitForSingleObject 等待 0 毫秒，
            // 但那会增加代码复杂性。
            result != 0 && exit_code == 259
        }
    }

    /// 检查进程是否存活 (非 Unix 非 Windows 平台的保守实现)
    #[cfg(not(any(unix, windows)))]
    fn is_process_alive(&self, _pid: u32) -> bool {
        // 保守策略：假设进程存活
        true
    }

    /// 释放锁
    async fn release_lock(&self) -> Result<(), DreamError> {
        let lock_path = self.config_dir.join(LOCK_FILE_NAME);
        if fs::try_exists(&lock_path).await? {
            fs::remove_file(&lock_path).await?;
        }
        Ok(())
    }

    /// 阶段 1: 定位现有内容
    async fn orient(&self) -> Result<(), DreamError> {
        tracing::debug!("[dream] Phase 1: Orienting");

        // 读取现有记忆文件，建立索引
        let memory_dir = self.config_dir.join("memory");
        if !fs::try_exists(&memory_dir).await? {
            fs::create_dir_all(&memory_dir).await?;
        }

        Ok(())
    }

    /// 阶段 2: 收集新信号
    ///
    /// 从 session memory 文件中收集信息，提取需要整合的信号。
    /// 优先级：最新的会话 > 旧的会话
    async fn gather(&self) -> Result<Vec<GatheredSignal>, DreamError> {
        tracing::debug!("[dream] Phase 2: Gathering signals");

        let mut signals = Vec::new();
        let sessions_dir = self.config_dir.join("sessions");

        if !fs::try_exists(&sessions_dir).await? {
            return Ok(signals);
        }

        // 收集所有 session memory 文件及其修改时间
        let mut session_files: Vec<(PathBuf, SystemTime)> = Vec::new();
        let mut entries = fs::read_dir(&sessions_dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let memory_file = entry.path().join("memory.md");
            if fs::try_exists(&memory_file).await? {
                if let Ok(metadata) = fs::metadata(&memory_file).await {
                    if let Ok(modified) = metadata.modified() {
                        session_files.push((memory_file, modified));
                    }
                }
            }
        }

        // 按修改时间降序排序（最新的优先）
        session_files.sort_by(|a, b| b.1.cmp(&a.1));

        // 限制处理数量
        let files_to_process = session_files.iter().take(MAX_SESSIONS_TO_PROCESS);

        for (memory_file, modified_time) in files_to_process {
            match fs::read_to_string(memory_file).await {
                Ok(content) => {
                    // 提取信号
                    let signal = self.extract_signals_from_memory(&content, *modified_time);
                    if !signal.is_empty() {
                        tracing::trace!(
                            path = %memory_file.display(),
                            signal_count = signal.len(),
                            "extracted signals from session memory"
                        );
                        signals.extend(signal);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        path = %memory_file.display(),
                        error = %e,
                        "failed to read session memory"
                    );
                }
            }
        }

        tracing::info!(
            total_signals = signals.len(),
            "[dream] Phase 2: Gathered {} signals",
            signals.len()
        );

        Ok(signals)
    }

    /// 从 session memory 内容中提取信号
    fn extract_signals_from_memory(&self, content: &str, modified_time: SystemTime) -> Vec<GatheredSignal> {
        let mut signals = Vec::new();

        // 按章节分割
        for section in content.split("\n## ") {
            let section = section.trim();
            if section.is_empty() {
                continue;
            }

            // 提取章节标题
            let title_end = section.find('\n').unwrap_or(section.len());
            let title = section[..title_end].trim();

            // 提取章节内容（跳过标题行和换行符）
            let section_content = if title_end < section.len() {
                // 有换行符，从换行符后开始提取
                section[title_end..].trim()
            } else {
                // 没有换行符，只有标题，无内容
                ""
            };

            if !section_content.is_empty() && section_content != format!("*{}*", title).as_str() {
                // 计算内容的重要性分数
                let importance = self.calculate_signal_importance(title, section_content);

                if importance > 0 {
                    signals.push(GatheredSignal {
                        title: title.to_string(),
                        content: section_content.to_string(),
                        importance,
                        source_time: modified_time,
                    });
                }
            }
        }

        signals
    }

    /// 计算信号的重要性分数 (0-10)
    fn calculate_signal_importance(&self, title: &str, content: &str) -> u8 {
        // 高重要性章节
        let high_priority = ["Current State", "Errors & Corrections", "User Request"];
        // 中重要性章节
        let medium_priority = ["Key Files", "Pending Tasks", "Important Context"];
        // 低重要性章节
        let low_priority = ["Work Log", "Session Info"];

        if high_priority.iter().any(|t| title.contains(t)) {
            8
        } else if medium_priority.iter().any(|t| title.contains(t)) {
            5
        } else if low_priority.iter().any(|t| title.contains(t)) {
            2
        } else {
            // 根据内容长度和关键词判断
            let content_len = content.len();
            if content_len > 500 {
                4
            } else if content_len > 200 {
                3
            } else {
                1
            }
        }
    }

    /// 阶段 3: 整合知识
    async fn consolidate(&self, signals: &[GatheredSignal]) -> Result<(), DreamError> {
        tracing::debug!(
            signal_count = signals.len(),
            "[dream] Phase 3: Consolidating knowledge"
        );

        // 检查 provider_pool
        let provider_pool = self.provider_pool.as_ref()
            .ok_or(DreamError::NoProviderPool)?;

        // 构建整合提示（包含收集的信号）
        let memory_dir = self.config_dir.join("memory");
        let prompt = self.build_consolidation_prompt(&memory_dir, signals);

        // 创建工具权限检查
        let can_use_tool = create_dream_can_use_tool(&memory_dir);

        // 创建 CacheSafeParams（使用默认系统提示）
        let cache_safe_params = CacheSafeParams::default();

        // 运行 Forked Agent 进行整合
        // 使用 Builder 模式构建参数
        let params = ForkedAgentParams::builder()
            .provider_pool(provider_pool.clone())
            .prompt_messages(vec![ChatMessage::user(&prompt)])
            .cache_safe_params(cache_safe_params)
            .can_use_tool(can_use_tool)
            .query_source("auto_dream")
            .fork_label("auto_dream")
            .max_turns(10)
            .skip_transcript(true)
            .build()
            .map_err(|e| DreamError::ConsolidationFailed(format!("Failed to build params: {}", e)))?;

        let result = run_forked_agent(params).await;

        match result {
            Ok(agent_result) => {
                tracing::info!(
                    input_tokens = agent_result.total_usage.input_tokens,
                    output_tokens = agent_result.total_usage.output_tokens,
                    cache_hit_rate = agent_result.total_usage.cache_hit_rate(),
                    "[dream] Forked Agent completed"
                );
                Ok(())
            }
            Err(e) => {
                tracing::error!(error = %e, "[dream] Forked Agent failed");
                Err(DreamError::ConsolidationFailed(format!("{}", e)))
            }
        }
    }

    /// 构建整合提示
    fn build_consolidation_prompt(&self, memory_dir: &Path, signals: &[GatheredSignal]) -> String {
        // 按重要性排序信号
        let mut sorted_signals = signals.to_vec();
        sorted_signals.sort_by(|a, b| b.importance.cmp(&a.importance));

        // 构建信号摘要
        let signals_section = if sorted_signals.is_empty() {
            "无新信号需要整合。\n".to_string()
        } else {
            let mut section = String::new();
            section.push_str("以下是从最近会话中收集的新信号（按重要性排序）：\n\n");

            for signal in sorted_signals.iter().take(20) { // 限制最多20个信号
                section.push_str(&format!(
                    "### {} (重要性: {}/10)\n{}\n\n",
                    signal.title, signal.importance, signal.content
                ));
            }

            section
        };

        format!(
            r#"# Dream: Memory Consolidation

## 任务
对记忆文件进行回顾、整理、更新和索引优化。

## 记忆目录
{}

## 收集的新信号
{}

## 执行阶段

### Phase 1 — Orient (定位)
- `ls` 记忆目录查看现有内容
- 读取入口文件理解当前索引
- 浏览现有主题文件避免重复创建

### Phase 2 — Gather recent signal (收集新信号)
优先级排序：
1. Daily logs（日志流）
2. 已过时的记忆（需要修正）
3. Transcript search（特定上下文搜索）

### Phase 3 — Consolidate (整合)
- 合并新信号到现有主题文件
- 将相对日期转换为绝对日期
- 删除被证伪的事实
- 更新过时信息

### Phase 4 — Prune and index (修剪和索引)
- 更新入口文件（保持 < 100 行, < 25KB）
- 移除过时指针
- 添加新指针
- 优化索引结构

## 工具限制
- Bash: 仅限只读命令 (ls, find, grep, cat, stat, wc, head, tail)
- Edit/Write: 仅限记忆目录内

## 注意事项
- 不要删除现有记忆，除非确认过时
- 合并相似条目
- 保持信息密度
"#,
            memory_dir.display(),
            signals_section
        )
    }

    /// 阶段 4: 修剪索引
    async fn prune(&self) -> Result<DreamStats, DreamError> {
        tracing::debug!("[dream] Phase 4: Pruning indexes");

        // 清理过期的 session memory 文件
        self.prune_expired_session_memories().await
    }

    /// 清理过期的 session memory 文件
    async fn prune_expired_session_memories(&self) -> Result<DreamStats, DreamError> {
        let sessions_dir = self.config_dir.join("sessions");

        if !fs::try_exists(&sessions_dir).await? {
            return Ok(DreamStats::default());
        }

        let expiry_threshold = SESSION_MEMORY_EXPIRY_DAYS * 24 * 3600; // 转换为秒
        let active_threshold = 3600; // 1小时内更新视为活跃会话
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let mut entries = fs::read_dir(&sessions_dir).await?;
        let mut pruned_count = 0;
        let mut skipped_active = 0;

        while let Some(entry) = entries.next_entry().await? {
            let session_dir = entry.path();

            // 检查是否为活跃会话
            if self.is_session_active(&session_dir, now, active_threshold).await? {
                skipped_active += 1;
                continue;
            }

            // 检查目录修改时间
            if let Ok(metadata) = fs::metadata(&session_dir).await {
                if let Ok(modified) = metadata.modified() {
                    let modified_secs = modified
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);

                    // 如果超过过期阈值，删除整个目录
                    if now - modified_secs > expiry_threshold {
                        tracing::trace!(
                            path = %session_dir.display(),
                            age_days = (now - modified_secs) / (24 * 3600),
                            "pruning expired session memory"
                        );
                        fs::remove_dir_all(&session_dir).await?;
                        pruned_count += 1;
                    }
                }
            }
        }

        tracing::info!(
            pruned_count,
            skipped_active,
            "[dream] Phase 4: Pruned {} expired session memories ({} active sessions skipped)",
            pruned_count,
            skipped_active
        );

        Ok(DreamStats {
            sessions_pruned: pruned_count,
            sessions_processed: pruned_count + skipped_active,
            ..Default::default()
        })
    }

    /// 检查会话是否仍在活跃运行
    ///
    /// 通过检查 `.active` 文件是否存在且最近更新来判断。
    /// 如果文件不存在或超过阈值时间未更新，则视为非活跃。
    async fn is_session_active(
        &self,
        session_dir: &Path,
        now: u64,
        active_threshold_secs: u64,
    ) -> Result<bool, DreamError> {
        let active_file = session_dir.join(".active");

        // 如果 .active 文件不存在，会话非活跃
        if !fs::try_exists(&active_file).await? {
            return Ok(false);
        }

        // 检查文件修改时间
        match fs::metadata(&active_file).await {
            Ok(metadata) => {
                match metadata.modified() {
                    Ok(modified) => {
                        let modified_secs = modified
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);

                        // 如果最近有更新，视为活跃
                        let is_active = now.saturating_sub(modified_secs) < active_threshold_secs;
                        Ok(is_active)
                    }
                    Err(_) => Ok(false),
                }
            }
            Err(_) => Ok(false),
        }
    }

    /// 扫描 memory 目录，获取文件状态
    ///
    /// 返回 (文件数量, 总字节数, 文件修改时间映射)
    async fn scan_memory_dir(&self, memory_dir: &Path) -> MemoryDirState {
        let mut file_count = 0;
        let mut total_bytes = 0u64;
        let mut file_mtimes: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

        match fs::try_exists(memory_dir).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(path = %memory_dir.display(), "Memory directory does not exist");
                return MemoryDirState::default();
            }
            Err(e) => {
                tracing::debug!(path = %memory_dir.display(), error = %e, "Failed to check memory directory existence");
                return MemoryDirState::default();
            }
        }

        match fs::read_dir(memory_dir).await {
            Ok(mut entries) => {
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        file_count += 1;
                        if let Ok(metadata) = fs::metadata(&path).await {
                            total_bytes += metadata.len();
                            if let Ok(modified) = metadata.modified() {
                                let mtime = modified
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0);
                                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                    file_mtimes.insert(name.to_string(), mtime);
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::debug!(path = %memory_dir.display(), error = %e, "Failed to read memory directory");
            }
        }

        MemoryDirState {
            file_count,
            total_bytes,
            file_mtimes,
        }
    }

    /// 计算前后 memory 目录的差异
    fn compute_memory_diff(&self, pre: &MemoryDirState, post: &MemoryDirState) -> DreamStats {
        let mut created = 0;
        let mut updated = 0;
        let mut deleted = 0;

        // 检查新增和更新
        for (name, post_mtime) in &post.file_mtimes {
            match pre.file_mtimes.get(name) {
                Some(pre_mtime) => {
                    // 文件已存在，检查是否更新
                    if post_mtime > pre_mtime {
                        updated += 1;
                    }
                }
                None => {
                    // 新文件
                    created += 1;
                }
            }
        }

        // 检查删除
        for name in pre.file_mtimes.keys() {
            if !post.file_mtimes.contains_key(name) {
                deleted += 1;
            }
        }

        DreamStats {
            memories_created: created,
            memories_updated: updated,
            memories_deleted: deleted,
            ..Default::default()
        }
    }

    /// 增加会话计数
    pub fn increment_session_count(&mut self) {
        self.state.increment_session_count();
    }

    /// 获取当前状态
    pub fn state(&self) -> &DreamState {
        &self.state
    }
}

/// 梦境错误类型
#[derive(Debug, thiserror::Error)]
pub enum DreamError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Lock already acquired by another process")]
    LockAcquired,

    #[error("Consolidation failed: {0}")]
    ConsolidationFailed(String),

    #[error("No provider pool configured - call with_provider_pool() before dream()")]
    NoProviderPool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dream_state_default() {
        let state = DreamState::default();
        assert!(state.last_consolidation_time.is_none());
        assert_eq!(state.current_session_count, 0);
        assert!(!state.is_consolidating);
    }

    #[test]
    fn test_dream_state_increment() {
        let mut state = DreamState::default();
        state.increment_session_count();
        assert_eq!(state.current_session_count, 1);
    }

    #[test]
    fn test_check_gates_time_failed() {
        let state = DreamState {
            last_consolidation_time: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            ),
            last_session_count: 0,
            current_session_count: 10,
            consolidation_count: 1,
            is_consolidating: false,
        };

        let result = check_gates(&state, Path::new("/config"));
        assert_eq!(result, GateCheckResult::TimeGateFailed);
    }

    #[test]
    fn test_check_gates_session_failed() {
        let state = DreamState {
            last_consolidation_time: Some(0), // 很久以前
            last_session_count: 0,
            current_session_count: 3, // 少于阈值 5
            consolidation_count: 1,
            is_consolidating: false,
        };

        let result = check_gates(&state, Path::new("/config"));
        assert_eq!(result, GateCheckResult::SessionGateFailed);
    }

    #[test]
    fn test_check_gates_lock_failed() {
        let state = DreamState {
            last_consolidation_time: Some(0),
            last_session_count: 0,
            current_session_count: 10,
            consolidation_count: 1,
            is_consolidating: true, // 正在整合
        };

        let result = check_gates(&state, Path::new("/config"));
        assert_eq!(result, GateCheckResult::LockGateFailed);
    }

    #[test]
    fn test_check_gates_passed() {
        let state = DreamState {
            last_consolidation_time: Some(0), // 很久以前
            last_session_count: 0,
            current_session_count: 10, // 超过阈值 5
            consolidation_count: 1,
            is_consolidating: false,
        };

        let result = check_gates(&state, Path::new("/config"));
        assert_eq!(result, GateCheckResult::Passed);
    }

    // ========== 核心路径测试 ==========

    #[test]
    fn test_gathered_signal_creation() {
        use std::time::SystemTime;

        let signal = GatheredSignal {
            title: "User Preferences".to_string(),
            content: "User prefers dark mode".to_string(),
            importance: 8,
            source_time: SystemTime::now(),
        };

        assert_eq!(signal.title, "User Preferences");
        assert_eq!(signal.importance, 8);
    }

    #[test]
    fn test_dream_state_serialization() {
        let state = DreamState {
            last_consolidation_time: Some(1234567890),
            last_session_count: 5,
            current_session_count: 10,
            consolidation_count: 3,
            is_consolidating: false,
        };

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: DreamState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.last_consolidation_time, Some(1234567890));
        assert_eq!(deserialized.current_session_count, 10);
    }

    #[test]
    fn test_gate_check_result_variants() {
        // 确保所有变体都能正确创建和比较
        assert_eq!(GateCheckResult::TimeGateFailed, GateCheckResult::TimeGateFailed);
        assert_eq!(GateCheckResult::SessionGateFailed, GateCheckResult::SessionGateFailed);
        assert_eq!(GateCheckResult::LockGateFailed, GateCheckResult::LockGateFailed);
        assert_eq!(GateCheckResult::Passed, GateCheckResult::Passed);
    }

    #[test]
    fn test_dream_config_defaults() {
        assert_eq!(TIME_GATE_THRESHOLD_HOURS, 24);
        assert_eq!(SESSION_GATE_THRESHOLD, 5);
        assert_eq!(SESSION_MEMORY_EXPIRY_DAYS, 7);
        assert_eq!(MAX_SESSIONS_TO_PROCESS, 10);
    }
}