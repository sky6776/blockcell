use crate::job::{CronJob, ScheduleKind};
use blockcell_core::system_event::{DeliveryPolicy, EventPriority, SystemEvent};
use blockcell_core::{InboundMessage, Paths, Result};
use blockcell_tools::EventEmitterHandle;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::SystemTime;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info};

#[derive(Debug, Serialize, Deserialize)]
pub struct JobStore {
    pub version: u32,
    pub jobs: Vec<CronJob>,
}

impl Default for JobStore {
    fn default() -> Self {
        Self {
            version: 1,
            jobs: Vec::new(),
        }
    }
}

pub struct CronService {
    paths: Paths,
    jobs: Arc<RwLock<Vec<CronJob>>>,
    inbound_tx: mpsc::Sender<InboundMessage>,
    agent_id: Option<String>,
    event_emitter: Arc<StdMutex<Option<EventEmitterHandle>>>,
    /// Last known modification time of cron_jobs.json file.
    /// Used to skip unnecessary disk reads when file hasn't changed.
    last_file_mtime: Arc<RwLock<Option<SystemTime>>>,
    /// Flag to track if in-memory state has changes that need to be saved.
    has_unsaved_changes: Arc<RwLock<bool>>,
    /// Tick interval in seconds for checking due jobs.
    tick_interval_secs: u64,
    /// Default timezone for jobs without a specified timezone or with invalid timezone.
    default_timezone: Option<Tz>,
}

fn apply_route_agent_id(metadata: &mut serde_json::Value, agent_id: Option<&str>) {
    if let Some(agent_id) = agent_id.map(str::trim).filter(|id| !id.is_empty()) {
        if !metadata.is_object() {
            *metadata = serde_json::json!({});
        }
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert("route_agent_id".to_string(), serde_json::json!(agent_id));
        }
    }
}

/// Parse a timezone string (e.g., "Asia/Shanghai") into a Tz.
/// Returns None and logs a warning if the timezone string is invalid.
fn parse_timezone(tz_str: &str) -> Option<Tz> {
    match tz_str.parse::<Tz>() {
        Ok(tz) => Some(tz),
        Err(e) => {
            tracing::warn!(tz = %tz_str, error = %e, "Invalid timezone string");
            None
        }
    }
}

impl CronService {
    /// Create a new CronService with default tick interval (1 second).
    pub fn new(paths: Paths, inbound_tx: mpsc::Sender<InboundMessage>) -> Self {
        Self::new_with_options(paths, inbound_tx, None, None, None)
    }

    /// Create a new CronService with optional agent_id (uses default tick interval).
    pub fn new_with_agent(
        paths: Paths,
        inbound_tx: mpsc::Sender<InboundMessage>,
        agent_id: Option<String>,
    ) -> Self {
        Self::new_with_options(paths, inbound_tx, agent_id, None, None)
    }

    /// Create a new CronService with all options.
    pub fn new_with_options(
        paths: Paths,
        inbound_tx: mpsc::Sender<InboundMessage>,
        agent_id: Option<String>,
        tick_interval_secs: Option<u64>,
        default_timezone: Option<&str>,
    ) -> Self {
        // Parse default timezone string to Tz
        let default_tz = default_timezone.and_then(|tz_str| {
            match tz_str.parse::<Tz>() {
                Ok(tz) => {
                    tracing::info!(default_timezone = %tz_str, "CronService using default timezone");
                    Some(tz)
                }
                Err(e) => {
                    tracing::warn!(
                        default_timezone = %tz_str,
                        error = %e,
                        "Invalid default timezone string, falling back to UTC"
                    );
                    None
                }
            }
        });

        Self {
            paths,
            jobs: Arc::new(RwLock::new(Vec::new())),
            inbound_tx,
            agent_id: agent_id
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty()),
            event_emitter: Arc::new(StdMutex::new(None)),
            last_file_mtime: Arc::new(RwLock::new(None)),
            has_unsaved_changes: Arc::new(RwLock::new(false)),
            tick_interval_secs: tick_interval_secs.unwrap_or(1),
            default_timezone: default_tz,
        }
    }

    pub fn set_event_emitter(&self, emitter: EventEmitterHandle) {
        let mut slot = self
            .event_emitter
            .lock()
            .expect("cron service event emitter lock poisoned");
        *slot = Some(emitter);
    }

    pub async fn load(&self) -> Result<()> {
        let path = self.paths.cron_jobs_file();
        if !path.exists() {
            return Ok(());
        }

        let content = tokio::fs::read_to_string(&path).await?;
        let store: JobStore = serde_json::from_str(&content)?;

        let mut jobs = self.jobs.write().await;
        // Keep overdue one-time jobs in memory so the next tick can execute them.
        // Dropping them here makes At jobs impossible to fire because every execution
        // happens after crossing `at_ms`.
        *jobs = store.jobs;

        debug!(count = jobs.len(), "Loaded cron jobs");
        Ok(())
    }

    pub async fn save(&self) -> Result<()> {
        // Check if there are unsaved changes
        if !*self.has_unsaved_changes.read().await {
            debug!("No unsaved changes, skipping cron save");
            return Ok(());
        }

        let path = self.paths.cron_jobs_file();

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let jobs = self.jobs.read().await;
        let store = JobStore {
            version: 1,
            jobs: jobs.clone(),
        };

        let content = serde_json::to_string_pretty(&store)?;
        tokio::fs::write(&path, content).await?;

        // Update the recorded modification time (our own write)
        let mtime = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        *self.last_file_mtime.write().await = mtime;

        // Clear the unsaved changes flag
        *self.has_unsaved_changes.write().await = false;

        debug!("Cron jobs saved to disk");
        Ok(())
    }

    pub async fn add_job(&self, job: CronJob) -> Result<()> {
        let mut jobs = self.jobs.write().await;
        // Check for duplicate ID
        if jobs.iter().any(|j| j.id == job.id) {
            tracing::warn!(job_id = %job.id, "Duplicate job ID, replacing existing job");
            jobs.retain(|j| j.id != job.id);
        }
        jobs.push(job);
        drop(jobs);
        *self.has_unsaved_changes.write().await = true;
        self.save().await
    }

    pub async fn remove_job(&self, id: &str) -> Result<bool> {
        let mut jobs = self.jobs.write().await;
        let len_before = jobs.len();
        jobs.retain(|j| j.id != id);
        let removed = jobs.len() < len_before;
        drop(jobs);

        if removed {
            *self.has_unsaved_changes.write().await = true;
            self.save().await?;
        }
        Ok(removed)
    }

    pub async fn list_jobs(&self) -> Vec<CronJob> {
        self.jobs.read().await.clone()
    }

    /// Update the enabled state of a job by ID prefix. Returns the job name if found.
    pub async fn update_job_enabled(
        &self,
        id_prefix: &str,
        enabled: bool,
    ) -> Result<Option<String>> {
        let mut jobs = self.jobs.write().await;
        let matching: Vec<usize> = jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| j.id.starts_with(id_prefix))
            .map(|(i, _)| i)
            .collect();

        match matching.len() {
            0 => Ok(None),
            1 => {
                let job = &mut jobs[matching[0]];
                job.enabled = enabled;
                job.updated_at_ms = chrono::Utc::now().timestamp_millis();
                let name = job.name.clone();
                drop(jobs);
                *self.has_unsaved_changes.write().await = true;
                self.save().await?;
                Ok(Some(name))
            }
            _ => {
                // Multiple matches — return Err with disambiguation hint
                let names: Vec<String> = matching
                    .iter()
                    .map(|&i| {
                        format!(
                            "{} ({})",
                            &jobs[i].id.chars().take(8).collect::<String>(),
                            jobs[i].name
                        )
                    })
                    .collect();
                Err(blockcell_core::Error::Other(format!(
                    "Multiple jobs match '{}': {}",
                    id_prefix,
                    names.join(", ")
                )))
            }
        }
    }

    /// Reload from disk while preserving in-memory execution state (next_run_at_ms /
    /// last_run_at_ms) for jobs that have already been initialized this session.
    /// This avoids the old `load()` bug where a full replace would clobber in-memory
    /// scheduling state and could cause jobs to re-fire or never fire.
    ///
    /// Returns `true` if the file was actually read (had changes), `false` if skipped.
    async fn merge_load(&self) -> Result<bool> {
        let path = self.paths.cron_jobs_file();
        if !path.exists() {
            return Ok(false);
        }

        // Check file modification time to skip unnecessary reads
        let current_mtime = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|m| m.modified().ok());

        let last_mtime = *self.last_file_mtime.read().await;

        // If file hasn't changed, skip the read
        if let (Some(current), Some(last)) = (current_mtime, last_mtime) {
            if current == last {
                debug!("Cron jobs file unchanged, skipping reload");
                return Ok(false);
            }
        }

        // File has changed (or no previous mtime), read it
        let content = tokio::fs::read_to_string(&path).await?;
        let store: JobStore = serde_json::from_str(&content)?;

        let mut mem_jobs = self.jobs.write().await;
        // Capture execution state for existing jobs by ID.
        let mem_state: std::collections::HashMap<String, (Option<i64>, Option<i64>)> = mem_jobs
            .iter()
            .map(|j| {
                (
                    j.id.clone(),
                    (j.state.next_run_at_ms, j.state.last_run_at_ms),
                )
            })
            .collect();

        let mut new_jobs = store.jobs;

        // Replace with disk state, restoring in-memory scheduling state where present.
        for job in new_jobs.iter_mut() {
            if let Some((next_run, last_run)) = mem_state.get(&job.id) {
                if next_run.is_some() {
                    job.state.next_run_at_ms = *next_run;
                }
                if last_run.is_some() {
                    job.state.last_run_at_ms = *last_run;
                }
            }
        }
        *mem_jobs = new_jobs;

        // Update the recorded modification time
        *self.last_file_mtime.write().await = current_mtime;

        debug!(
            count = mem_jobs.len(),
            "Loaded cron jobs (merged with in-memory state)"
        );
        Ok(true)
    }

    /// Pick up any new jobs written to disk (e.g. by CronTool) since the last load.
    /// Also updates existing jobs if they were modified on disk (detected by updated_at_ms).
    /// Called just before save() to close the race window.
    async fn sync_new_from_disk(
        &self,
        known_ids: &std::collections::HashSet<String>,
    ) -> Result<bool> {
        let path = self.paths.cron_jobs_file();
        if !path.exists() {
            return Ok(false);
        }
        let content = tokio::fs::read_to_string(&path).await?;
        let store: JobStore = serde_json::from_str(&content)?;

        let mut mem_jobs = self.jobs.write().await;
        let mut changed = false;

        for disk_job in store.jobs {
            // Check if this is a new job
            if !known_ids.contains(&disk_job.id)
                && !mem_jobs.iter().any(|job| job.id == disk_job.id)
            {
                debug!(job_id = %disk_job.id, "Picked up new cron job from disk");
                mem_jobs.push(disk_job);
                changed = true;
            } else {
                // Check if existing job was modified on disk (by updated_at_ms)
                if let Some(mem_job) = mem_jobs.iter_mut().find(|j| j.id == disk_job.id) {
                    // If disk job is newer, update the in-memory copy
                    // but preserve execution state (next_run_at_ms, last_run_at_ms)
                    // unless the schedule itself changed
                    if disk_job.updated_at_ms > mem_job.updated_at_ms {
                        let schedule_changed = mem_job.schedule.kind != disk_job.schedule.kind
                            || mem_job.schedule.at_ms != disk_job.schedule.at_ms
                            || mem_job.schedule.every_ms != disk_job.schedule.every_ms
                            || mem_job.schedule.expr != disk_job.schedule.expr
                            || mem_job.schedule.tz != disk_job.schedule.tz;

                        debug!(
                            job_id = %disk_job.id,
                            schedule_changed = schedule_changed,
                            "Detected modified job on disk, updating in-memory state"
                        );

                        // Preserve execution state unless schedule changed
                        let preserved_next_run = if schedule_changed {
                            // Schedule changed: reset next_run to force recalculation
                            None
                        } else {
                            mem_job.state.next_run_at_ms
                        };
                        let preserved_last_run = mem_job.state.last_run_at_ms;

                        // Update from disk
                        *mem_job = disk_job;

                        // Restore preserved state
                        mem_job.state.next_run_at_ms = preserved_next_run;
                        mem_job.state.last_run_at_ms = preserved_last_run;

                        changed = true;
                    }
                }
            }
        }
        Ok(changed)
    }

    pub async fn run_tick(&self) -> Result<()> {
        // Reload from disk, merging in-memory execution state for already-initialized jobs.
        // New jobs added by CronTool (disk-only) are picked up; existing job scheduling
        // state (next_run_at_ms / last_run_at_ms) is preserved to avoid double-firing.
        let _file_changed = match self.merge_load().await {
            Ok(changed) => changed,
            Err(e) => {
                error!(error = %e.to_string(), "Failed to reload cron jobs from disk");
                false
            }
        };

        let now_ms = chrono::Utc::now().timestamp_millis();
        let mut jobs = self.jobs.write().await;
        let known_ids: std::collections::HashSet<String> =
            jobs.iter().map(|job| job.id.clone()).collect();
        let mut jobs_to_run = Vec::new();
        let mut state_changed = false;

        for job in jobs.iter_mut() {
            if !job.enabled {
                continue;
            }

            // Guard: skip one-time (At) jobs that have already fired
            if job.schedule.kind == ScheduleKind::At && job.state.last_run_at_ms.is_some() {
                job.enabled = false;
                state_changed = true;
                continue;
            }

            // Parse timezone for this job
            let tz: Option<Tz> = job.schedule.tz.as_ref().and_then(|tz_str| {
                let parsed = parse_timezone(tz_str);
                if parsed.is_none() {
                    tracing::warn!(
                        job_id = %job.id,
                        tz = %tz_str,
                        default_tz = ?self.default_timezone,
                        "Invalid timezone string, falling back to default timezone or UTC"
                    );
                }
                parsed
            }).or(self.default_timezone);

            let should_run = match &job.state.next_run_at_ms {
                Some(next) => *next <= now_ms,
                None => self.calculate_next_run(job, now_ms, tz.as_ref()),
            };

            if should_run {
                jobs_to_run.push(job.clone());

                // Update state
                job.state.last_run_at_ms = Some(now_ms);
                state_changed = true;

                // Calculate next run with timezone support
                match job.schedule.kind {
                    ScheduleKind::At => {
                        // One-time job: disable immediately
                        job.state.next_run_at_ms = None;
                        job.enabled = false;
                    }
                    ScheduleKind::Every => {
                        if let Some(every_ms) = job.schedule.every_ms {
                            job.state.next_run_at_ms = Some(now_ms + every_ms);
                        }
                    }
                    ScheduleKind::Cron => {
                        // Calculate next cron time with timezone support
                        if let Some(expr) = &job.schedule.expr {
                            if let Some(next_ms) = self.calculate_next_cron_run_ms(expr, tz.as_ref()) {
                                job.state.next_run_at_ms = Some(next_ms);
                            }
                        }
                    }
                }
            }
        }

        // Handle delete_after_run
        let delete_ids: Vec<String> = jobs
            .iter()
            .filter(|j| j.delete_after_run && j.state.last_run_at_ms.is_some())
            .map(|j| j.id.clone())
            .collect();
        if !delete_ids.is_empty() {
            jobs.retain(|j| !delete_ids.contains(&j.id));
            state_changed = true;
            info!(count = delete_ids.len(), "Deleted completed one-time jobs");
        }

        drop(jobs);

        // Mark state as changed if any modifications occurred
        if state_changed {
            *self.has_unsaved_changes.write().await = true;
        }

        // Always sync new jobs from disk to close the mtime race window.
        // CronTool may have written new jobs between merge_load and now; without this,
        // save() would overwrite those new jobs. This also catches cases where mtime
        // granularity (1-2s on most filesystems) caused merge_load to skip a changed file.
        match self.sync_new_from_disk(&known_ids).await {
            Ok(added) => {
                if added {
                    *self.has_unsaved_changes.write().await = true;
                }
            }
            Err(e) => {
                error!(error = %e.to_string(), "Failed to sync new cron jobs from disk");
            }
        }

        // Save state changes to disk BEFORE executing jobs
        // This ensures the next tick won't re-fire disabled/deleted jobs
        if state_changed || *self.has_unsaved_changes.read().await {
            self.save().await?;
        }

        // Execute jobs - spawn for parallel execution to avoid blocking
        let inbound_tx = self.inbound_tx.clone();
        let event_emitter = self.event_emitter.clone();
        let agent_id = self.agent_id.clone();

        for job in jobs_to_run {
            let inbound_tx = inbound_tx.clone();
            let event_emitter = event_emitter.clone();
            let agent_id = agent_id.clone();

            tokio::spawn(async move {
                Self::execute_job_internal(&job, inbound_tx, event_emitter, agent_id).await;
            });
        }
        Ok(())
    }

    /// Internal execute function that can be called from spawned tasks
    async fn execute_job_internal(
        job: &CronJob,
        inbound_tx: mpsc::Sender<InboundMessage>,
        event_emitter: Arc<StdMutex<Option<EventEmitterHandle>>>,
        agent_id: Option<String>,
    ) {
        debug!(job_id = %job.id, job_name = %job.name, kind = %job.payload.kind, "Executing cron job");

        // Emit start event
        if let Some(emitter) = event_emitter.lock().ok().and_then(|e| e.clone()) {
            let mut event = SystemEvent::new_main_session(
                "cron.job_started",
                "cron",
                EventPriority::Normal,
                "定时任务开始执行",
                format!("定时任务 {} 已开始执行", job.name),
            );
            event.delivery = DeliveryPolicy::default();
            event.details = serde_json::json!({
                "job_id": job.id.clone(),
                "job_name": job.name.clone(),
                "payload_kind": job.payload.kind.clone(),
                "deliver": job.payload.deliver,
                "deliver_channel": job.payload.channel.clone(),
                "deliver_to": job.payload.to.clone(),
            });
            emitter.emit(event);
        }

        let (content, metadata) = match job.payload.kind.as_str() {
            "reminder" => {
                let content = job.payload.message.clone();
                let metadata = serde_json::json!({
                    "job_id": job.id,
                    "job_name": job.name,
                    "reminder": true,
                    "reminder_message": job.payload.message,
                    "deliver": job.payload.deliver,
                    "deliver_channel": job.payload.channel,
                    "deliver_to": job.payload.to,
                });
                (content, metadata)
            }
            "script" => {
                let skill_name = job.payload.skill_name.as_deref().unwrap_or("unknown");
                let content = job.payload.message.clone();
                let metadata = serde_json::json!({
                    "job_id": job.id,
                    "job_name": job.name,
                    "skill_name": skill_name,
                    "forced_skill_name": skill_name,
                    "skill_run_mode": "cron",
                    "deliver": job.payload.deliver,
                    "deliver_channel": job.payload.channel,
                    "deliver_to": job.payload.to,
                });
                (content, metadata)
            }
            "agent" => {
                let content = job.payload.message.clone();
                let metadata = serde_json::json!({
                    "job_id": job.id,
                    "job_name": job.name,
                    "cron_agent": true,
                    "deliver": job.payload.deliver,
                    "deliver_channel": job.payload.channel,
                    "deliver_to": job.payload.to,
                });
                (content, metadata)
            }
            _ => {
                error!(job_id = %job.id, kind = %job.payload.kind, "Unknown cron payload kind");
                return;
            }
        };

        let (msg_channel, msg_chat_id) = ("cron".to_string(), job.id.clone());

        let mut metadata = metadata;
        apply_route_agent_id(&mut metadata, agent_id.as_deref());

        let msg = InboundMessage {
            channel: msg_channel,
            account_id: None,
            sender_id: "cron".to_string(),
            chat_id: msg_chat_id,
            content,
            media: vec![],
            metadata,
            timestamp_ms: chrono::Utc::now().timestamp_millis(),
        };

        if let Err(e) = inbound_tx.send(msg).await {
            error!(error = %e, "Failed to send cron job message");

            // Emit failure event
            if let Some(emitter) = event_emitter.lock().ok().and_then(|e| e.clone()) {
                let mut event = SystemEvent::new_main_session(
                    "cron.job_failed",
                    "cron",
                    EventPriority::Critical,
                    "定时任务派发失败",
                    format!("定时任务 {} 派发失败：{}", job.name, e),
                );
                event.delivery = DeliveryPolicy::critical();
                event.details = serde_json::json!({
                    "job_id": job.id.clone(),
                    "job_name": job.name.clone(),
                    "error": e.to_string(),
                });
                emitter.emit(event);
            }
        } else {
            // Emit completion event
            if let Some(emitter) = event_emitter.lock().ok().and_then(|e| e.clone()) {
                let mut event = SystemEvent::new_main_session(
                    "cron.job_completed",
                    "cron",
                    EventPriority::Normal,
                    "定时任务已派发",
                    format!("定时任务 {} 已成功派发", job.name),
                );
                event.delivery = DeliveryPolicy::default();
                event.details = serde_json::json!({
                    "job_id": job.id.clone(),
                    "job_name": job.name.clone(),
                });
                emitter.emit(event);
            }
        }
    }

    /// Execute a cron job (wrapper for testing and internal use)
    #[allow(dead_code)]
    async fn execute_job(&self, job: &CronJob) {
        Self::execute_job_internal(
            job,
            self.inbound_tx.clone(),
            self.event_emitter.clone(),
            self.agent_id.clone(),
        ).await;
    }

    /// Calculate the next cron run time with timezone support.
    /// Returns the next run time as milliseconds since epoch.
    fn calculate_next_cron_run_ms(&self, expr: &str, tz: Option<&Tz>) -> Option<i64> {
        match expr.parse::<cron::Schedule>() {
            Ok(schedule) => match tz {
                Some(tz_ref) => schedule.upcoming(*tz_ref).next().map(|dt| dt.timestamp_millis()),
                None => schedule.upcoming(chrono::Utc).next().map(|dt| dt.timestamp_millis()),
            },
            Err(e) => {
                tracing::error!(expr = %expr, error = %e, "Invalid cron expression");
                None
            }
        }
    }

    fn calculate_next_run(&self, job: &mut CronJob, now_ms: i64, tz: Option<&Tz>) -> bool {
        match job.schedule.kind {
            ScheduleKind::At => {
                if let Some(at_ms) = job.schedule.at_ms {
                    job.state.next_run_at_ms = Some(at_ms);
                    at_ms <= now_ms
                } else {
                    false
                }
            }
            ScheduleKind::Every => {
                if let Some(every_ms) = job.schedule.every_ms {
                    // Set next_run_at_ms for the NEXT scheduled run (after potential immediate execution).
                    // This is stored now so that after immediate execution, the next run time is already set.
                    // run_immediately controls whether THIS tick should trigger execution:
                    // - true: return true to trigger immediate execution, next_run_at_ms is already set for next cycle
                    // - false: return false, job will wait until next_run_at_ms
                    job.state.next_run_at_ms = Some(now_ms + every_ms);
                    job.schedule.run_immediately
                } else {
                    false
                }
            }
            ScheduleKind::Cron => {
                if let Some(expr) = &job.schedule.expr {
                    if let Some(next_ms) = self.calculate_next_cron_run_ms(expr, tz) {
                        job.state.next_run_at_ms = Some(next_ms);
                        debug!(
                            job_id = %job.id,
                            next_run_ms = next_ms,
                            tz = job.schedule.tz.as_deref().unwrap_or("UTC"),
                            "Cron job initialized, waiting for first scheduled time"
                        );
                    }
                }
                false
            }
        }
    }

    pub async fn run_loop(self: Arc<Self>, mut shutdown: tokio::sync::broadcast::Receiver<()>) {
        info!(
            tick_interval_secs = self.tick_interval_secs,
            "CronService started"
        );

        // Use configurable tick interval
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(self.tick_interval_secs));

        // Skip accumulated ticks when the service was paused/blocked
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Skip the first immediate tick (tokio interval returns immediately on first tick)
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.run_tick().await {
                        error!(error = %e.to_string(), "Cron tick failed");
                    }
                }
                _ = shutdown.recv() => {
                    info!("CronService shutting down");

                    // Save any unsaved state before shutting down
                    if *self.has_unsaved_changes.read().await {
                        if let Err(e) = self.save().await {
                            error!(error = %e.to_string(), "Failed to save cron state on shutdown");
                        }
                    }
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[derive(Clone, Default)]
    struct RecordingEmitter {
        events: Arc<StdMutex<Vec<SystemEvent>>>,
    }

    impl RecordingEmitter {
        fn handle(&self) -> EventEmitterHandle {
            Arc::new(self.clone())
        }

        fn kinds(&self) -> Vec<String> {
            self.events
                .lock()
                .expect("recording emitter lock poisoned")
                .iter()
                .map(|event| event.kind.clone())
                .collect()
        }

        fn priorities(&self) -> Vec<EventPriority> {
            self.events
                .lock()
                .expect("recording emitter lock poisoned")
                .iter()
                .map(|event| event.priority)
                .collect()
        }
    }

    impl blockcell_tools::SystemEventEmitter for RecordingEmitter {
        fn emit(&self, event: SystemEvent) {
            self.events
                .lock()
                .expect("recording emitter lock poisoned")
                .push(event);
        }
    }

    fn test_job() -> CronJob {
        let now_ms = Utc::now().timestamp_millis();
        CronJob {
            id: "job-1".to_string(),
            name: "daily sync".to_string(),
            enabled: true,
            schedule: crate::job::JobSchedule {
                kind: ScheduleKind::Every,
                at_ms: None,
                every_ms: Some(60_000),
                expr: None,
                tz: None,
                run_immediately: false,
            },
            payload: crate::job::JobPayload {
                kind: "reminder".to_string(),
                message: "sync status".to_string(),
                deliver: false,
                channel: None,
                to: None,
                script_kind: None,
                skill_name: None,
            },
            state: crate::job::JobState::default(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            delete_after_run: false,
        }
    }

    fn test_agent_job() -> CronJob {
        let now_ms = Utc::now().timestamp_millis();
        CronJob {
            id: "job-agent-1".to_string(),
            name: "news digest".to_string(),
            enabled: true,
            schedule: crate::job::JobSchedule {
                kind: ScheduleKind::Every,
                at_ms: None,
                every_ms: Some(60_000),
                expr: None,
                tz: None,
                run_immediately: false,
            },
            payload: crate::job::JobPayload {
                kind: "agent".to_string(),
                message: "请搜索美国伊朗最新新闻并整理摘要".to_string(),
                deliver: true,
                channel: Some("telegram".to_string()),
                to: Some("12345".to_string()),
                script_kind: None,
                skill_name: None,
            },
            state: crate::job::JobState::default(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            delete_after_run: false,
        }
    }

    fn test_due_at_job() -> CronJob {
        let now_ms = Utc::now().timestamp_millis();
        CronJob {
            id: "job-due-at-1".to_string(),
            name: "bedtime reminder".to_string(),
            enabled: true,
            schedule: crate::job::JobSchedule {
                kind: ScheduleKind::At,
                at_ms: Some(now_ms - 1_000),
                every_ms: None,
                expr: None,
                tz: None,
                run_immediately: false,
            },
            payload: crate::job::JobPayload {
                kind: "reminder".to_string(),
                message: "time to sleep".to_string(),
                deliver: true,
                channel: Some("ws".to_string()),
                to: Some("ws:test-reminder".to_string()),
                script_kind: None,
                skill_name: None,
            },
            state: crate::job::JobState::default(),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            delete_after_run: true,
        }
    }

    #[test]
    fn test_apply_route_agent_id_inserts_metadata() {
        let mut metadata = serde_json::json!({"job_id":"1"});
        apply_route_agent_id(&mut metadata, Some("ops"));
        assert_eq!(
            metadata.get("route_agent_id").and_then(|v| v.as_str()),
            Some("ops")
        );
    }

    #[test]
    fn test_apply_route_agent_id_skips_empty_agent() {
        let mut metadata = serde_json::json!({"job_id":"1"});
        apply_route_agent_id(&mut metadata, Some("   "));
        assert!(metadata.get("route_agent_id").is_none());
    }

    #[tokio::test]
    async fn test_cron_event_execute_job_emits_started_and_completed() {
        let paths = Paths::with_base(
            std::env::temp_dir().join(format!("blockcell-cron-service-{}", uuid::Uuid::new_v4())),
        );
        let (tx, mut rx) = mpsc::channel(1);
        let service = CronService::new(paths, tx);
        let emitter = RecordingEmitter::default();
        service.set_event_emitter(emitter.handle());

        service.execute_job(&test_job()).await;

        let message = rx.recv().await.expect("receive cron inbound message");
        assert_eq!(message.sender_id, "cron");
        assert_eq!(
            emitter.kinds(),
            vec![
                "cron.job_started".to_string(),
                "cron.job_completed".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_cron_event_execute_job_emits_failed_on_send_error() {
        let paths = Paths::with_base(
            std::env::temp_dir().join(format!("blockcell-cron-service-{}", uuid::Uuid::new_v4())),
        );
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let service = CronService::new(paths, tx);
        let emitter = RecordingEmitter::default();
        service.set_event_emitter(emitter.handle());

        service.execute_job(&test_job()).await;

        assert_eq!(
            emitter.kinds(),
            vec![
                "cron.job_started".to_string(),
                "cron.job_failed".to_string(),
            ]
        );
        assert_eq!(
            emitter.priorities().last().copied(),
            Some(EventPriority::Critical)
        );
    }

    #[tokio::test]
    async fn test_execute_agent_job_sends_plain_cron_message_without_fast_path_flags() {
        let paths = Paths::with_base(
            std::env::temp_dir().join(format!("blockcell-cron-service-{}", uuid::Uuid::new_v4())),
        );
        let (tx, mut rx) = mpsc::channel(1);
        let service = CronService::new(paths, tx);

        service.execute_job(&test_agent_job()).await;

        let message = rx.recv().await.expect("receive cron inbound message");
        assert_eq!(message.channel, "cron");
        assert_eq!(message.content, "请搜索美国伊朗最新新闻并整理摘要");
        assert_eq!(
            message.metadata.get("cron_agent").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(message.metadata.get("reminder").is_none());
        assert!(message.metadata.get("skill_script").is_none());
    }

    #[tokio::test]
    async fn test_run_tick_executes_due_at_job_loaded_from_disk() {
        let paths = Paths::with_base(
            std::env::temp_dir().join(format!("blockcell-cron-service-{}", uuid::Uuid::new_v4())),
        );
        tokio::fs::create_dir_all(paths.cron_dir())
            .await
            .expect("create cron dir");
        let store = JobStore {
            version: 1,
            jobs: vec![test_due_at_job()],
        };
        let content = serde_json::to_string_pretty(&store).expect("serialize cron store");
        tokio::fs::write(paths.cron_jobs_file(), content)
            .await
            .expect("write cron store");

        let (tx, mut rx) = mpsc::channel(1);
        let service = CronService::new(paths, tx);

        service.run_tick().await.expect("run tick");

        let message = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("cron message should be sent")
            .expect("receive cron inbound message");
        assert_eq!(message.content, "time to sleep");
        assert_eq!(
            message.metadata.get("reminder").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[tokio::test]
    async fn test_run_tick_does_not_readd_delete_after_run_job_from_disk() {
        let paths = Paths::with_base(
            std::env::temp_dir().join(format!("blockcell-cron-service-{}", uuid::Uuid::new_v4())),
        );
        tokio::fs::create_dir_all(paths.cron_dir())
            .await
            .expect("create cron dir");
        let store = JobStore {
            version: 1,
            jobs: vec![test_due_at_job()],
        };
        let content = serde_json::to_string_pretty(&store).expect("serialize cron store");
        tokio::fs::write(paths.cron_jobs_file(), content)
            .await
            .expect("write cron store");

        let (tx, mut rx) = mpsc::channel(4);
        let service = CronService::new(paths.clone(), tx);

        service.run_tick().await.expect("run first tick");
        let first = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv())
            .await
            .expect("first cron message should be sent")
            .expect("receive first cron inbound message");
        assert_eq!(first.content, "time to sleep");

        service.run_tick().await.expect("run second tick");
        let second = tokio::time::timeout(tokio::time::Duration::from_millis(200), rx.recv()).await;
        assert!(
            second.is_err(),
            "delete_after_run job should not fire twice"
        );

        let saved = tokio::fs::read_to_string(paths.cron_jobs_file())
            .await
            .expect("read saved cron store");
        let saved: JobStore = serde_json::from_str(&saved).expect("parse saved cron store");
        assert!(
            saved.jobs.is_empty(),
            "delete_after_run job should be removed from disk"
        );
    }
}
