//! Process supervisor - manages running processes

use oxidepm_core::{constants, AppInfo, AppSpec, AppStatus, Error, HookEvent, Hooks, Result, RunState, Selector};
use oxidepm_db::Database;
use oxidepm_health::HealthMonitor;
use oxidepm_logs::{LogCapture, LogReader, RotationConfig};
use oxidepm_notify::{NotificationManager, NotifyConfig, ProcessEvent};
use oxidepm_runtime::get_runner;
use oxidepm_watch::{FileWatcher, WatchConfig};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};
use tokio::process::Child;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

/// Supervised process state
pub struct SupervisedProcess {
    pub spec: AppSpec,
    pub state: RunState,
    pub child: Option<Child>,
    #[allow(dead_code)]
    pub restart_count: u32,
    #[allow(dead_code)]
    pub last_restart: Option<Instant>,
    pub started_at: Option<Instant>,
    /// Health monitor for this process (if health checks are configured)
    pub health_monitor: Option<HealthMonitor>,
    /// Instance IDs for cluster children (parent only)
    pub cluster_instance_ids: Vec<u32>,
    /// Parent ID if this is a cluster instance
    #[allow(dead_code)]
    pub parent_id: Option<u32>,
}

/// Process supervisor
pub struct Supervisor {
    db: Database,
    processes: Arc<RwLock<HashMap<u32, SupervisedProcess>>>,
    shutdown_tx: broadcast::Sender<()>,
    system: Arc<RwLock<System>>,
    notifier: Arc<NotificationManager>,
}

impl Supervisor {
    /// Create a new supervisor
    pub async fn new(db: Database) -> Result<Self> {
        let (shutdown_tx, _) = broadcast::channel(16);

        // Load notification config
        let notify_config = NotifyConfig::load().unwrap_or_default();
        let notifier = Arc::new(NotificationManager::new(notify_config));

        let supervisor = Self {
            db,
            processes: Arc::new(RwLock::new(HashMap::new())),
            shutdown_tx,
            system: Arc::new(RwLock::new(System::new_all())),
            notifier,
        };

        // Start metrics collector
        supervisor.spawn_metrics_collector();

        Ok(supervisor)
    }

    /// Send a notification for a process event (non-blocking)
    fn notify_event(&self, event: ProcessEvent) {
        let notifier = Arc::clone(&self.notifier);
        tokio::spawn(async move {
            if let Err(e) = notifier.notify(&event).await {
                warn!("Failed to send notification: {}", e);
            }
        });
    }

    /// Start an application
    pub async fn start(&self, mut spec: AppSpec) -> Result<u32> {
        // Check if app already exists with this name
        if let Some(existing) = self.db.apps().get_by_name(&spec.name).await? {
            // Check if it's already running
            let processes = self.processes.read();
            if let Some(proc) = processes.get(&existing.id) {
                if proc.state.status.is_running() {
                    return Err(Error::AppAlreadyExists(spec.name));
                }
            }
            // Use existing ID
            spec.id = existing.id;
        } else {
            // Insert new app
            let id = self.db.apps().insert(&spec).await?;
            spec.id = id;
        }

        info!("Starting app: {} (id: {})", spec.name, spec.id);

        // Handle clustering: if instances > 1, spawn multiple processes
        if spec.instances > 1 && spec.instance_id.is_none() {
            return self.start_cluster(spec).await;
        }

        // Single instance or cluster child - start normally
        self.start_single(spec).await
    }

    /// Start a cluster of instances
    async fn start_cluster(&self, spec: AppSpec) -> Result<u32> {
        let instance_count = spec.instances;
        let base_name = spec.name.clone();
        let parent_id = spec.id;

        info!(
            "Starting cluster '{}' with {} instances",
            base_name, instance_count
        );

        let mut instance_ids = Vec::with_capacity(instance_count as usize);

        for i in 0..instance_count {
            // Calculate port for this instance
            let port = self.calculate_instance_port(&spec, i);

            // Create instance spec
            let instance_spec = spec.for_instance(i, port);

            // Start the instance
            match self.start_single(instance_spec).await {
                Ok(id) => {
                    instance_ids.push(id);
                    info!("Started instance {}-{} (id: {}, port: {:?})", base_name, i, id, port);
                }
                Err(e) => {
                    error!("Failed to start instance {}-{}: {}", base_name, i, e);
                    // Stop already started instances on failure
                    for started_id in &instance_ids {
                        let _ = self.stop(*started_id).await;
                    }
                    return Err(e);
                }
            }
        }

        // Create parent entry to track the cluster
        let parent_supervised = SupervisedProcess {
            spec: spec.clone(),
            state: RunState {
                app_id: parent_id,
                pid: None,
                status: AppStatus::Running,
                restarts: 0,
                uptime_secs: 0,
                cpu_percent: 0.0,
                memory_bytes: 0,
                last_exit_code: None,
                started_at: Some(chrono::Utc::now()),
                healthy: true,
                last_health_check: None,
                health_check_failures: 0,
                port: None,
                instance_id: None,
            },
            child: None,
            restart_count: 0,
            last_restart: None,
            started_at: Some(Instant::now()),
            health_monitor: None,
            cluster_instance_ids: instance_ids,
            parent_id: None,
        };

        self.processes.write().insert(parent_id, parent_supervised);

        info!(
            "Cluster '{}' started with {} instances",
            base_name, instance_count
        );
        Ok(parent_id)
    }

    /// Calculate port for a cluster instance
    fn calculate_instance_port(&self, spec: &AppSpec, instance_index: u32) -> Option<u16> {
        // Priority 1: Use port_range if specified
        if let Some((start, end)) = spec.port_range {
            let port = start + instance_index as u16;
            if port <= end {
                return Some(port);
            }
            warn!(
                "Port range exhausted for instance {}, using increment from base",
                instance_index
            );
        }

        // Priority 2: Increment from base port
        if let Some(base_port) = spec.port {
            return Some(base_port + instance_index as u16);
        }

        // No port management configured
        None
    }

    /// Start a single process (internal)
    async fn start_single(&self, mut spec: AppSpec) -> Result<u32> {
        // For cluster instances, we need a new ID
        if spec.instance_id.is_some() {
            let id = self.db.apps().insert(&spec).await?;
            spec.id = id;
        }

        // Apply startup delay if configured
        if let Some(delay_ms) = spec.startup_delay_ms {
            if delay_ms > 0 {
                info!("Waiting {}ms before starting {}...", delay_ms, spec.name);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }

        // Get appropriate runner
        let runner = get_runner(spec.mode);

        // Prepare (build if needed)
        info!("Preparing {} ({})...", spec.name, spec.mode);
        let prepare_result = runner.prepare(&spec).await?;

        if !prepare_result.success {
            error!("Prepare failed for {}: {}", spec.name, prepare_result.output);
            return Err(Error::BuildFailed(prepare_result.output));
        }

        info!("Prepare successful for {}", spec.name);

        // Start process
        let running = runner.start(&spec).await?;
        let pid = running.pid;

        info!("Started {} with PID {}", spec.name, pid);

        // Set up log capture
        oxidepm_logs::ensure_log_dir()?;
        let log_capture = LogCapture::new(&spec.name, RotationConfig::default())?;

        // Take ownership of child's stdout/stderr
        let mut child = running.child;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Spawn log capture tasks
        log_capture.spawn_capture(stdout, stderr);

        // Set up health monitor if configured
        let health_monitor = spec.health_check.as_ref().map(|hc| HealthMonitor::new(hc.clone()));

        // Create supervised process
        let supervised = SupervisedProcess {
            spec: spec.clone(),
            state: RunState {
                app_id: spec.id,
                pid: Some(pid),
                status: AppStatus::Running,
                restarts: 0,
                uptime_secs: 0,
                cpu_percent: 0.0,
                memory_bytes: 0,
                last_exit_code: None,
                started_at: Some(chrono::Utc::now()),
                healthy: true,
                last_health_check: None,
                health_check_failures: 0,
                port: spec.port,
                instance_id: spec.instance_id,
            },
            child: Some(child),
            restart_count: 0,
            last_restart: None,
            started_at: Some(Instant::now()),
            health_monitor,
            cluster_instance_ids: Vec::new(),
            parent_id: None,
        };

        // Track process
        self.processes.write().insert(spec.id, supervised);

        // Send notification for process started
        self.notify_event(ProcessEvent::Started {
            name: spec.name.clone(),
            id: spec.id,
        });

        // Run on_start hook if configured
        self.run_hook(
            &spec.hooks,
            HookEvent::Start,
            spec.id,
            &spec.name,
            Some(pid),
            None,
        );

        // Spawn supervision task
        self.spawn_supervision_task(spec.id);

        // Spawn health check task if configured
        if spec.health_check.is_some() {
            self.spawn_health_check_task(spec.id);
        }

        // Set up watch if enabled
        if spec.watch {
            self.spawn_watch_task(spec.id);
        }

        Ok(spec.id)
    }

    /// Stop an application
    pub async fn stop(&self, id: u32) -> Result<bool> {
        // Extract what we need without holding the lock across await
        let (name, kill_timeout_ms, child, pid, hooks) = {
            let mut processes = self.processes.write();

            if let Some(proc) = processes.get_mut(&id) {
                if !proc.state.status.is_running() {
                    return Ok(false);
                }

                info!("Stopping app {} (id: {})", proc.spec.name, id);
                proc.state.status = AppStatus::Stopping;

                let child = proc.child.take();
                let pid = proc.state.pid;
                let name = proc.spec.name.clone();
                let timeout = proc.spec.kill_timeout_ms;
                let hooks = proc.spec.hooks.clone();
                (name, timeout, child, pid, hooks)
            } else {
                return Ok(false);
            }
        };

        if let Some(mut child) = child {
            // Send SIGTERM
            #[cfg(unix)]
            {
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid as NixPid;

                if let Some(pid) = pid {
                    let _ = kill(NixPid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }

            // Wait with timeout
            let timeout = Duration::from_millis(kill_timeout_ms);
            let kill_result = tokio::time::timeout(timeout, child.wait()).await;

            let exit_code = match kill_result {
                Ok(Ok(status)) => {
                    debug!("Process exited with code: {:?}", status.code());
                    status.code()
                }
                Ok(Err(e)) => {
                    warn!("Error waiting for process: {}", e);
                    None
                }
                Err(_) => {
                    // Timeout, send SIGKILL
                    warn!("Process didn't stop gracefully, sending SIGKILL");
                    let _ = child.kill().await;
                    None
                }
            };

            // Update state after async operations complete
            {
                let mut processes = self.processes.write();
                if let Some(proc) = processes.get_mut(&id) {
                    proc.state.last_exit_code = exit_code;
                    proc.state.status = AppStatus::Stopped;
                    proc.state.pid = None;
                    proc.started_at = None;
                }
            }
        }

        // Get exit code for hook
        let exit_code = {
            let processes = self.processes.read();
            processes.get(&id).and_then(|p| p.state.last_exit_code)
        };

        // Send notification for process stopped
        self.notify_event(ProcessEvent::Stopped {
            name: name.clone(),
            id,
            exit_code,
        });

        // Run on_stop hook if configured
        self.run_hook(&hooks, HookEvent::Stop, id, &name, pid, exit_code);

        info!("Stopped app {}", name);
        Ok(true)
    }

    /// Restart an application
    pub async fn restart(&self, id: u32) -> Result<bool> {
        // Get the spec first
        let spec = {
            let processes = self.processes.read();
            processes.get(&id).map(|p| p.spec.clone())
        };

        if let Some(spec) = spec {
            // Run on_restart hook if configured (before stop/start)
            self.run_hook(&spec.hooks, HookEvent::Restart, id, &spec.name, None, None);

            self.stop(id).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.start(spec).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Delete an application
    pub async fn delete(&self, id: u32) -> Result<bool> {
        // Stop first if running
        self.stop(id).await?;

        // Remove from processes
        self.processes.write().remove(&id);

        // Delete from database
        self.db.apps().delete(id).await?;

        info!("Deleted app (id: {})", id);
        Ok(true)
    }

    /// Get status of all apps
    pub async fn status(&self) -> Result<Vec<AppInfo>> {
        let apps = self.db.apps().get_all().await?;
        let processes = self.processes.read();

        let mut result = Vec::new();
        for spec in apps {
            let state = if let Some(proc) = processes.get(&spec.id) {
                proc.state.clone()
            } else {
                RunState::new(spec.id)
            };
            result.push(AppInfo::new(spec, state));
        }

        Ok(result)
    }

    /// Get info for a single app
    pub async fn show(&self, selector: &Selector) -> Result<Option<AppInfo>> {
        let spec = match selector {
            Selector::All => return Ok(None),
            Selector::ById(id) => self.db.apps().get_by_id(*id).await?,
            Selector::ByName(name) => self.db.apps().get_by_name(name).await?,
            Selector::ByTag(tag) => {
                // For tags, return the first matching app
                let apps = self.db.apps().get_all().await?;
                apps.into_iter().find(|app| app.tags.contains(tag))
            }
        };

        if let Some(spec) = spec {
            let processes = self.processes.read();
            let state = if let Some(proc) = processes.get(&spec.id) {
                proc.state.clone()
            } else {
                RunState::new(spec.id)
            };
            Ok(Some(AppInfo::new(spec, state)))
        } else {
            Ok(None)
        }
    }

    /// Get logs for an app
    pub async fn logs(
        &self,
        selector: &Selector,
        lines: usize,
        stdout: bool,
        stderr: bool,
    ) -> Result<Vec<String>> {
        let spec = match selector {
            Selector::All => return Err(Error::InvalidSelector("Cannot get logs for 'all'".into())),
            Selector::ById(id) => self.db.apps().get_by_id(*id).await?,
            Selector::ByName(name) => self.db.apps().get_by_name(name).await?,
            Selector::ByTag(tag) => {
                // For tags, return logs from first matching app
                let apps = self.db.apps().get_all().await?;
                apps.into_iter().find(|app| app.tags.contains(tag))
            }
        };

        let spec = spec.ok_or_else(|| Error::AppNotFound(selector.to_string()))?;

        let mut all_lines = Vec::new();

        if stdout || !stderr {
            let reader = LogReader::new(oxidepm_logs::stdout_path(&spec.name));
            all_lines.extend(reader.tail(lines)?);
        }

        if stderr || !stdout {
            let reader = LogReader::new(oxidepm_logs::stderr_path(&spec.name));
            all_lines.extend(reader.tail(lines)?);
        }

        // Sort by timestamp if we have both
        if stdout && stderr {
            all_lines.sort();
        }

        // Limit to requested lines
        if all_lines.len() > lines {
            let skip_count = all_lines.len() - lines;
            all_lines = all_lines.into_iter().skip(skip_count).collect();
        }

        Ok(all_lines)
    }

    /// Save current process list
    pub async fn save(&self) -> Result<usize> {
        let apps = self.db.apps().get_all().await?;
        let path = constants::saved_path();

        let json = serde_json::to_string_pretty(&apps)?;
        std::fs::write(&path, json)?;

        info!("Saved {} apps to {}", apps.len(), path.display());
        Ok(apps.len())
    }

    /// Resurrect saved processes
    pub async fn resurrect(&self) -> Result<usize> {
        let path = constants::saved_path();
        if !path.exists() {
            return Ok(0);
        }

        let content = std::fs::read_to_string(&path)?;
        let apps: Vec<AppSpec> = serde_json::from_str(&content)?;

        let mut count = 0;
        for spec in apps {
            // Check if already in database
            if self.db.apps().get_by_name(&spec.name).await?.is_none() {
                // Insert into database
                let mut new_spec = spec.clone();
                new_spec.id = self.db.apps().insert(&spec).await?;

                // Start the process
                if let Err(e) = self.start(new_spec).await {
                    warn!("Failed to resurrect {}: {}", spec.name, e);
                } else {
                    count += 1;
                }
            }
        }

        info!("Resurrected {} processes", count);
        Ok(count)
    }

    /// Resolve selector to app IDs
    pub async fn resolve_selector(&self, selector: &Selector) -> Result<Vec<u32>> {
        match selector {
            Selector::All => {
                let apps = self.db.apps().get_all().await?;
                Ok(apps.into_iter().map(|a| a.id).collect())
            }
            Selector::ById(id) => {
                if self.db.apps().get_by_id(*id).await?.is_some() {
                    Ok(vec![*id])
                } else {
                    Err(Error::AppNotFound(id.to_string()))
                }
            }
            Selector::ByName(name) => {
                if let Some(app) = self.db.apps().get_by_name(name).await? {
                    Ok(vec![app.id])
                } else {
                    Err(Error::AppNotFound(name.clone()))
                }
            }
            Selector::ByTag(tag) => {
                let apps = self.db.apps().get_all().await?;
                let matching: Vec<u32> = apps
                    .into_iter()
                    .filter(|app| app.tags.contains(tag))
                    .map(|app| app.id)
                    .collect();
                if matching.is_empty() {
                    Err(Error::AppNotFound(format!("@{}", tag)))
                } else {
                    Ok(matching)
                }
            }
        }
    }

    /// Graceful reload - start new instance, wait for healthy, then stop old
    pub async fn reload(&self, id: u32) -> Result<bool> {
        // Get the current spec
        let (spec, is_cluster) = {
            let processes = self.processes.read();
            match processes.get(&id) {
                Some(proc) => {
                    let is_cluster = !proc.cluster_instance_ids.is_empty();
                    (proc.spec.clone(), is_cluster)
                }
                None => return Ok(false),
            }
        };

        info!("Starting graceful reload for {} (id: {})", spec.name, id);

        // Handle cluster reload differently
        if is_cluster {
            return self.reload_cluster(id).await;
        }

        // Single instance reload
        self.reload_single(id, spec).await
    }

    /// Reload a single instance with zero-downtime
    async fn reload_single(&self, old_id: u32, spec: AppSpec) -> Result<bool> {
        // Create a temporary spec with a new name for the new instance
        let mut new_spec = spec.clone();
        new_spec.name = format!("{}-reload", spec.name);
        new_spec.id = 0; // Will get new ID

        // Start the new instance
        info!("Starting new instance for reload: {}", new_spec.name);
        let new_id = match self.start_single(new_spec.clone()).await {
            Ok(id) => id,
            Err(e) => {
                error!("Failed to start new instance for reload: {}", e);
                return Err(e);
            }
        };

        // Wait for health check if configured
        if spec.health_check.is_some() {
            info!("Waiting for new instance to become healthy...");
            if !self.wait_for_healthy(new_id, Duration::from_secs(30)).await {
                error!("New instance failed health check, aborting reload");
                let _ = self.stop(new_id).await;
                let _ = self.delete(new_id).await;
                return Err(Error::HealthCheckFailed);
            }
            info!("New instance is healthy");
        } else {
            // No health check, wait a brief moment for startup
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Stop the old instance
        info!("Stopping old instance (id: {})", old_id);
        self.stop(old_id).await?;

        // Rename new instance to original name
        {
            let mut processes = self.processes.write();
            if let Some(proc) = processes.get_mut(&new_id) {
                proc.spec.name = spec.name.clone();
            }
        }

        // Update database
        self.db.apps().delete(old_id).await?;

        info!("Graceful reload completed for {}", spec.name);
        Ok(true)
    }

    /// Reload a cluster with rolling restart
    async fn reload_cluster(&self, parent_id: u32) -> Result<bool> {
        let (spec, instance_ids) = {
            let processes = self.processes.read();
            match processes.get(&parent_id) {
                Some(proc) => (proc.spec.clone(), proc.cluster_instance_ids.clone()),
                None => return Ok(false),
            }
        };

        info!(
            "Starting rolling reload for cluster '{}' ({} instances)",
            spec.name,
            instance_ids.len()
        );

        // Reload each instance one at a time
        for (i, old_instance_id) in instance_ids.iter().enumerate() {
            // Get the instance spec
            let _instance_spec = {
                let processes = self.processes.read();
                match processes.get(old_instance_id) {
                    Some(proc) => proc.spec.clone(),
                    None => continue,
                }
            };

            info!(
                "Reloading instance {} of {} (id: {})",
                i + 1,
                instance_ids.len(),
                old_instance_id
            );

            // Calculate port for new instance
            let port = self.calculate_instance_port(&spec, i as u32);
            let mut new_instance_spec = spec.for_instance(i as u32, port);
            new_instance_spec.name = format!("{}-{}-reload", spec.name, i);
            new_instance_spec.id = 0;

            // Start new instance
            let new_id = match self.start_single(new_instance_spec).await {
                Ok(id) => id,
                Err(e) => {
                    error!("Failed to start new instance {}: {}", i, e);
                    continue;
                }
            };

            // Wait for health
            if spec.health_check.is_some() {
                if !self.wait_for_healthy(new_id, Duration::from_secs(30)).await {
                    error!("Instance {} failed health check, skipping", i);
                    let _ = self.stop(new_id).await;
                    let _ = self.delete(new_id).await;
                    continue;
                }
            } else {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }

            // Stop old instance
            self.stop(*old_instance_id).await?;
            self.delete(*old_instance_id).await?;

            // Update the new instance name
            {
                let mut processes = self.processes.write();
                if let Some(proc) = processes.get_mut(&new_id) {
                    proc.spec.name = format!("{}-{}", spec.name, i);
                }

                // Update parent's cluster instance IDs
                if let Some(parent) = processes.get_mut(&parent_id) {
                    if let Some(pos) = parent.cluster_instance_ids.iter().position(|x| x == old_instance_id) {
                        parent.cluster_instance_ids[pos] = new_id;
                    }
                }
            }

            info!("Instance {} reloaded successfully", i);
        }

        info!("Rolling reload completed for cluster '{}'", spec.name);
        Ok(true)
    }

    /// Wait for an instance to become healthy
    async fn wait_for_healthy(&self, app_id: u32, timeout: Duration) -> bool {
        let start = Instant::now();
        let check_interval = Duration::from_millis(500);

        // Get the health check config
        let health_config = {
            let processes = self.processes.read();
            if let Some(proc) = processes.get(&app_id) {
                proc.spec.health_check.clone()
            } else {
                return false;
            }
        };

        // No health check configured, assume healthy
        let health_config = match health_config {
            Some(c) => c,
            None => return true,
        };

        let mut monitor = HealthMonitor::new(health_config);

        while start.elapsed() < timeout {
            // Perform health check outside the lock
            let result = monitor.check().await;

            // Update state and check result
            {
                let mut processes = self.processes.write();
                if let Some(proc) = processes.get_mut(&app_id) {
                    proc.state.healthy = result.healthy;
                    proc.state.last_health_check = Some(chrono::Utc::now());

                    if result.healthy {
                        return true;
                    }
                } else {
                    // Process not found
                    return false;
                }
            }

            tokio::time::sleep(check_interval).await;
        }

        false
    }

    /// Spawn health check task for an app
    fn spawn_health_check_task(&self, app_id: u32) {
        let processes = Arc::clone(&self.processes);
        let notifier = Arc::clone(&self.notifier);
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            // Get initial interval and health check config
            let (interval, health_config) = {
                let procs = processes.read();
                match procs.get(&app_id) {
                    Some(proc) => {
                        let interval = proc
                            .health_monitor
                            .as_ref()
                            .map(|m| m.interval())
                            .unwrap_or(Duration::from_secs(30));
                        let config = proc.spec.health_check.clone();
                        (interval, config)
                    }
                    None => return,
                }
            };

            // Create our own health checker (doesn't need to be in the process struct)
            let health_config = match health_config {
                Some(c) => c,
                None => return, // No health check configured
            };
            let mut monitor = HealthMonitor::new(health_config);

            // Wait for process to start before first health check
            tokio::time::sleep(Duration::from_secs(5)).await;

            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    _ = tokio::time::sleep(interval) => {
                        // First check if we should continue (without holding lock across await)
                        let should_check = {
                            let procs = processes.read();
                            if let Some(proc) = procs.get(&app_id) {
                                proc.state.status.is_running()
                            } else {
                                false
                            }
                        };

                        if !should_check {
                            break;
                        }

                        // Perform health check outside the lock
                        let result = monitor.check().await;
                        let is_unhealthy = monitor.is_unhealthy();

                        // Update state inside the lock
                        {
                            let mut procs = processes.write();
                            if let Some(proc) = procs.get_mut(&app_id) {
                                proc.state.healthy = result.healthy;
                                proc.state.last_health_check = Some(chrono::Utc::now());

                                if result.healthy {
                                    proc.state.health_check_failures = 0;
                                    debug!("Health check passed for app {}", app_id);
                                } else {
                                    proc.state.health_check_failures += 1;
                                    warn!(
                                        "Health check failed for app {} ({} consecutive failures): {:?}",
                                        app_id,
                                        proc.state.health_check_failures,
                                        result.message
                                    );

                                    // Check if we should mark as unhealthy
                                    if is_unhealthy {
                                        warn!("App {} marked as unhealthy", app_id);
                                        proc.state.status = AppStatus::Errored;

                                        // Send health check failure notification
                                        let name = proc.spec.name.clone();
                                        let endpoint = proc.spec.health_check
                                            .as_ref()
                                            .and_then(|hc| hc.http_url.clone())
                                            .unwrap_or_else(|| "unknown".to_string());
                                        let notifier_clone = Arc::clone(&notifier);
                                        tokio::spawn(async move {
                                            let event = ProcessEvent::HealthCheckFailed {
                                                name,
                                                id: app_id,
                                                endpoint,
                                            };
                                            if let Err(e) = notifier_clone.notify(&event).await {
                                                warn!("Failed to send health check notification: {}", e);
                                            }
                                        });

                                        // Run on_error hook if configured
                                        if let Some(error_script) = proc.spec.hooks.on_error.clone() {
                                            let hook_name = proc.spec.name.clone();
                                            let pid = proc.state.pid;
                                            tokio::spawn(async move {
                                                let result = run_hook_script(
                                                    &error_script,
                                                    app_id,
                                                    &hook_name,
                                                    "error",
                                                    pid,
                                                    None,
                                                ).await;
                                                match result {
                                                    Ok(output) => {
                                                        if !output.is_empty() {
                                                            debug!("Error hook output for {}: {}", hook_name, output);
                                                        }
                                                        info!("Error hook completed successfully for {}", hook_name);
                                                    }
                                                    Err(e) => {
                                                        error!("Error hook failed for {}: {}", hook_name, e);
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Spawn supervision task for an app
    fn spawn_supervision_task(&self, app_id: u32) {
        let processes = Arc::clone(&self.processes);
        let notifier = Arc::clone(&self.notifier);
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        break;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        let mut procs = processes.write();
                        if let Some(proc) = procs.get_mut(&app_id) {
                            if let Some(child) = &mut proc.child {
                                // Check if process has exited
                                match child.try_wait() {
                                    Ok(Some(status)) => {
                                        // Process exited
                                        let exit_code = status.code();
                                        proc.state.last_exit_code = exit_code;
                                        proc.state.pid = None;
                                        proc.child = None;

                                        if proc.state.status == AppStatus::Stopping {
                                            proc.state.status = AppStatus::Stopped;
                                        } else {
                                            // Unexpected exit (crash)
                                            warn!("Process {} exited unexpectedly", app_id);
                                            proc.state.status = AppStatus::Errored;

                                            // Send crash notification
                                            let name = proc.spec.name.clone();
                                            let error = exit_code
                                                .map(|c| format!("Exit code {}", c))
                                                .unwrap_or_else(|| "Unknown error".to_string());
                                            let notifier_clone = Arc::clone(&notifier);
                                            tokio::spawn(async move {
                                                let event = ProcessEvent::Crashed {
                                                    name,
                                                    id: app_id,
                                                    error,
                                                };
                                                if let Err(e) = notifier_clone.notify(&event).await {
                                                    warn!("Failed to send crash notification: {}", e);
                                                }
                                            });

                                            // Run on_crash hook if configured
                                            if let Some(crash_script) = proc.spec.hooks.on_crash.clone() {
                                                let hook_name = proc.spec.name.clone();
                                                tokio::spawn(async move {
                                                    let result = run_hook_script(
                                                        &crash_script,
                                                        app_id,
                                                        &hook_name,
                                                        "crash",
                                                        None,
                                                        exit_code,
                                                    ).await;
                                                    match result {
                                                        Ok(output) => {
                                                            if !output.is_empty() {
                                                                debug!("Crash hook output for {}: {}", hook_name, output);
                                                            }
                                                            info!("Crash hook completed successfully for {}", hook_name);
                                                        }
                                                        Err(e) => {
                                                            error!("Crash hook failed for {}: {}", hook_name, e);
                                                        }
                                                    }
                                                });
                                            }

                                            // TODO: Handle restart logic here
                                        }
                                    }
                                    Ok(None) => {
                                        // Still running, update uptime
                                        if let Some(started) = proc.started_at {
                                            proc.state.uptime_secs = started.elapsed().as_secs();
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Error checking process status: {}", e);
                                    }
                                }
                            }
                        } else {
                            // Process removed, exit task
                            break;
                        }
                    }
                }
            }
        });
    }

    /// Spawn watch task for an app
    fn spawn_watch_task(&self, app_id: u32) {
        let processes = Arc::clone(&self.processes);

        tokio::spawn(async move {
            // Get app spec
            let spec = {
                let procs = processes.read();
                procs.get(&app_id).map(|p| p.spec.clone())
            };

            let spec = match spec {
                Some(s) => s,
                None => return,
            };

            // Create watcher
            let config = WatchConfig {
                ignore: spec.ignore_patterns.clone(),
                debounce_ms: 200,
            };

            let mut watcher = match FileWatcher::new(config) {
                Ok(w) => w,
                Err(e) => {
                    warn!("Failed to create watcher for {}: {}", app_id, e);
                    return;
                }
            };

            if let Err(e) = watcher.watch(&spec.cwd) {
                warn!("Failed to watch directory for {}: {}", app_id, e);
                return;
            }

            info!("Watch mode active for {} in {}", spec.name, spec.cwd.display());

            loop {
                // Check if still running
                {
                    let procs = processes.read();
                    if !procs.contains_key(&app_id) {
                        break;
                    }
                }

                // Wait for changes
                if let Some(event) = watcher.wait(Duration::from_secs(1)) {
                    info!("File change detected for {}: {:?}", spec.name, event.paths);
                    // Restart logic would go here
                }
            }
        });
    }

    /// Spawn metrics collector task with limit enforcement
    fn spawn_metrics_collector(&self) {
        let processes = Arc::clone(&self.processes);
        let system = Arc::clone(&self.system);
        let notifier = Arc::clone(&self.notifier);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(2));
            // Track which processes have already been notified/scheduled for restart
            let mut memory_limit_notified: std::collections::HashSet<u32> = std::collections::HashSet::new();
            let mut pending_restarts: std::collections::HashSet<u32> = std::collections::HashSet::new();

            loop {
                interval.tick().await;

                // Refresh system info
                {
                    let mut sys = system.write();
                    sys.refresh_all();
                }

                // Collect processes that need restart due to limits
                let mut restart_needed: Vec<(u32, String, String)> = Vec::new();

                // Update process metrics and check limits
                {
                    let mut procs = processes.write();
                    let sys = system.read();

                    for (app_id, proc) in procs.iter_mut() {
                        if let Some(pid) = proc.state.pid {
                            if let Some(process) = sys.process(Pid::from(pid as usize)) {
                                proc.state.cpu_percent = process.cpu_usage();
                                proc.state.memory_bytes = process.memory();
                            }
                        }

                        // Skip if not running or already pending restart
                        if !proc.state.status.is_running() || pending_restarts.contains(app_id) {
                            continue;
                        }

                        // Check memory limit - enforce restart if exceeded
                        if let Some(limit_mb) = proc.spec.max_memory_mb {
                            let memory_mb = proc.state.memory_bytes / (1024 * 1024);
                            if memory_mb > limit_mb {
                                warn!(
                                    "Process {} (id: {}) exceeded memory limit: {}MB > {}MB, scheduling restart",
                                    proc.spec.name, app_id, memory_mb, limit_mb
                                );

                                // Send notification if not already sent
                                if !memory_limit_notified.contains(app_id) {
                                    memory_limit_notified.insert(*app_id);
                                    let name = proc.spec.name.clone();
                                    let id = *app_id;
                                    let notifier_clone = Arc::clone(&notifier);
                                    tokio::spawn(async move {
                                        let event = ProcessEvent::MemoryLimit {
                                            name,
                                            id,
                                            memory_mb,
                                            limit_mb,
                                        };
                                        if let Err(e) = notifier_clone.notify(&event).await {
                                            warn!("Failed to send memory limit notification: {}", e);
                                        }
                                    });
                                }

                                restart_needed.push((*app_id, proc.spec.name.clone(), "memory_limit".to_string()));
                                pending_restarts.insert(*app_id);
                            } else if memory_mb < limit_mb {
                                // Reset notification flag when memory is back under limit
                                memory_limit_notified.remove(app_id);
                            }
                        }

                        // Check max uptime limit - enforce restart if exceeded
                        if let Some(max_uptime) = proc.spec.max_uptime_secs {
                            if proc.state.uptime_secs >= max_uptime {
                                warn!(
                                    "Process {} (id: {}) exceeded max uptime: {}s >= {}s, scheduling restart",
                                    proc.spec.name, app_id, proc.state.uptime_secs, max_uptime
                                );
                                restart_needed.push((*app_id, proc.spec.name.clone(), "max_uptime".to_string()));
                                pending_restarts.insert(*app_id);
                            }
                        }
                    }
                }

                // Handle restarts outside of the lock
                for (app_id, name, reason) in restart_needed {
                    info!(
                        "Restarting process {} (id: {}) due to {} limit exceeded",
                        name, app_id, reason
                    );

                    // Get the spec and child for restart
                    let spec_and_child = {
                        let mut procs = processes.write();
                        if let Some(proc) = procs.get_mut(&app_id) {
                            // Mark as stopping
                            proc.state.status = AppStatus::Stopping;
                            let child = proc.child.take();
                            let spec = proc.spec.clone();
                            Some((spec, child, proc.state.pid))
                        } else {
                            None
                        }
                    };

                    if let Some((spec, child, pid)) = spec_and_child {
                        // Run on_restart hook if configured (for auto-restart scenarios)
                        if let Some(restart_script) = spec.hooks.on_restart.clone() {
                            let hook_name = spec.name.clone();
                            tokio::spawn(async move {
                                let result = run_hook_script(
                                    &restart_script,
                                    app_id,
                                    &hook_name,
                                    "restart",
                                    pid,
                                    None,
                                ).await;
                                match result {
                                    Ok(output) => {
                                        if !output.is_empty() {
                                            debug!("Restart hook output for {}: {}", hook_name, output);
                                        }
                                        info!("Restart hook completed successfully for {}", hook_name);
                                    }
                                    Err(e) => {
                                        error!("Restart hook failed for {}: {}", hook_name, e);
                                    }
                                }
                            });
                        }

                        // Stop the current process
                        if let Some(mut child) = child {
                            // Send SIGTERM first
                            #[cfg(unix)]
                            {
                                use nix::sys::signal::{kill, Signal};
                                use nix::unistd::Pid as NixPid;

                                if let Some(pid) = pid {
                                    let _ = kill(NixPid::from_raw(pid as i32), Signal::SIGTERM);
                                }
                            }

                            // Wait with timeout then kill
                            let timeout = Duration::from_millis(spec.kill_timeout_ms);
                            let kill_result = tokio::time::timeout(timeout, child.wait()).await;

                            if kill_result.is_err() {
                                warn!("Process didn't stop gracefully, sending SIGKILL");
                                let _ = child.kill().await;
                            }
                        }

                        // Update state to stopped
                        {
                            let mut procs = processes.write();
                            if let Some(proc) = procs.get_mut(&app_id) {
                                proc.state.status = AppStatus::Stopped;
                                proc.state.pid = None;
                                proc.started_at = None;
                            }
                        }

                        // Clear from pending restarts so it can be started again
                        pending_restarts.remove(&app_id);
                        memory_limit_notified.remove(&app_id);

                        // Note: The actual restart will be handled by the supervision task
                        // which watches for process exits. We've stopped the process,
                        // so the supervision task will detect this and restart if auto-restart is enabled.
                        info!(
                            "Process {} (id: {}) stopped for {} restart",
                            spec.name, app_id, reason
                        );
                    }
                }
            }
        });
    }

    /// Run a hook script asynchronously
    ///
    /// Executes the hook script in a background task with environment variables:
    /// - OPM_APP_ID: The application ID
    /// - OPM_APP_NAME: The application name
    /// - OPM_EVENT: The event type (start, stop, restart, crash, error)
    /// - OPM_PID: The process ID (if available)
    /// - OPM_EXIT_CODE: The exit code (if available, for crash/stop events)
    fn run_hook(
        &self,
        hooks: &Hooks,
        event: HookEvent,
        app_id: u32,
        app_name: &str,
        pid: Option<u32>,
        exit_code: Option<i32>,
    ) {
        if let Some(script) = hooks.get(event) {
            let script = script.to_string();
            let app_name = app_name.to_string();
            let event_name = event.to_string();

            info!(
                "Running {} hook for {} (id: {}): {}",
                event_name, app_name, app_id, script
            );

            tokio::spawn(async move {
                let result = run_hook_script(&script, app_id, &app_name, &event_name, pid, exit_code).await;
                match result {
                    Ok(output) => {
                        if !output.is_empty() {
                            debug!("Hook output for {} ({}): {}", app_name, event_name, output);
                        }
                        info!("Hook {} completed successfully for {}", event_name, app_name);
                    }
                    Err(e) => {
                        error!("Hook {} failed for {}: {}", event_name, app_name, e);
                    }
                }
            });
        }
    }
}

/// Execute a hook script with environment variables
///
/// The script is run through the shell (sh -c) with the following environment variables:
/// - OPM_APP_ID: The application ID
/// - OPM_APP_NAME: The application name
/// - OPM_EVENT: The event type (start, stop, restart, crash, error)
/// - OPM_PID: The process ID (if available)
/// - OPM_EXIT_CODE: The exit code (if available)
///
/// Hook output is logged to a separate hook log file.
async fn run_hook_script(
    script: &str,
    app_id: u32,
    app_name: &str,
    event: &str,
    pid: Option<u32>,
    exit_code: Option<i32>,
) -> std::result::Result<String, String> {
    use std::process::Stdio;
    use tokio::process::Command;

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(script);

    // Set environment variables
    cmd.env("OPM_APP_ID", app_id.to_string());
    cmd.env("OPM_APP_NAME", app_name);
    cmd.env("OPM_EVENT", event);

    if let Some(pid) = pid {
        cmd.env("OPM_PID", pid.to_string());
    }

    if let Some(code) = exit_code {
        cmd.env("OPM_EXIT_CODE", code.to_string());
    }

    // Capture output
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Run the command with a timeout
    let timeout = Duration::from_secs(30);
    let result = tokio::time::timeout(timeout, cmd.output()).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Log hook output to a separate file
            if let Err(e) = log_hook_output(app_name, event, &stdout, &stderr) {
                warn!("Failed to log hook output: {}", e);
            }

            if output.status.success() {
                Ok(stdout.trim().to_string())
            } else {
                Err(format!(
                    "Hook exited with code {:?}: {}",
                    output.status.code(),
                    stderr.trim()
                ))
            }
        }
        Ok(Err(e)) => Err(format!("Failed to execute hook: {}", e)),
        Err(_) => Err("Hook timed out after 30 seconds".to_string()),
    }
}

/// Log hook output to a separate log file
fn log_hook_output(
    app_name: &str,
    event: &str,
    stdout: &str,
    stderr: &str,
) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    let log_dir = constants::logs_dir();
    if !log_dir.exists() {
        std::fs::create_dir_all(&log_dir)?;
    }

    let log_path = log_dir.join(format!("{}-hooks.log", app_name));
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let timestamp = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC");
    writeln!(file, "[{}] Event: {}", timestamp, event)?;

    if !stdout.is_empty() {
        writeln!(file, "  stdout: {}", stdout.trim())?;
    }
    if !stderr.is_empty() {
        writeln!(file, "  stderr: {}", stderr.trim())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxidepm_core::Hooks;
    

    #[tokio::test]
    async fn test_run_hook_script_simple() {
        let result = run_hook_script(
            "echo 'hello world'",
            1,
            "test-app",
            "start",
            Some(1234),
            None,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn test_run_hook_script_with_env_vars() {
        let result = run_hook_script(
            "echo $OPM_APP_ID $OPM_APP_NAME $OPM_EVENT",
            42,
            "my-app",
            "stop",
            None,
            None,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "42 my-app stop");
    }

    #[tokio::test]
    async fn test_run_hook_script_with_exit_code() {
        let result = run_hook_script(
            "echo $OPM_EXIT_CODE",
            1,
            "crashed-app",
            "crash",
            Some(5678),
            Some(137),
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "137");
    }

    #[tokio::test]
    async fn test_run_hook_script_with_pid() {
        let result = run_hook_script(
            "echo $OPM_PID",
            1,
            "app",
            "start",
            Some(9999),
            None,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "9999");
    }

    #[tokio::test]
    async fn test_run_hook_script_failure() {
        let result = run_hook_script(
            "exit 1",
            1,
            "failing-app",
            "start",
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Hook exited with code"));
    }

    #[tokio::test]
    async fn test_run_hook_script_command_not_found() {
        let result = run_hook_script(
            "/nonexistent/command",
            1,
            "app",
            "start",
            None,
            None,
        )
        .await;

        assert!(result.is_err());
    }

    #[test]
    fn test_hooks_default() {
        let hooks = Hooks::default();
        assert!(hooks.is_empty());
    }

    #[test]
    fn test_hooks_get_event() {
        let hooks = Hooks {
            on_start: Some("start.sh".to_string()),
            on_stop: Some("stop.sh".to_string()),
            on_restart: None,
            on_crash: Some("crash.sh".to_string()),
            on_error: None,
        };

        assert_eq!(hooks.get(HookEvent::Start), Some("start.sh"));
        assert_eq!(hooks.get(HookEvent::Stop), Some("stop.sh"));
        assert_eq!(hooks.get(HookEvent::Restart), None);
        assert_eq!(hooks.get(HookEvent::Crash), Some("crash.sh"));
        assert_eq!(hooks.get(HookEvent::Error), None);
    }

    #[test]
    fn test_hook_event_display() {
        assert_eq!(HookEvent::Start.to_string(), "start");
        assert_eq!(HookEvent::Stop.to_string(), "stop");
        assert_eq!(HookEvent::Restart.to_string(), "restart");
        assert_eq!(HookEvent::Crash.to_string(), "crash");
        assert_eq!(HookEvent::Error.to_string(), "error");
    }

    #[tokio::test]
    async fn test_run_hook_script_multiline_output() {
        let result = run_hook_script(
            "echo 'line1'; echo 'line2'; echo 'line3'",
            1,
            "app",
            "start",
            None,
            None,
        )
        .await;

        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("line1"));
        assert!(output.contains("line2"));
        assert!(output.contains("line3"));
    }

    #[tokio::test]
    async fn test_run_hook_script_stderr() {
        let result = run_hook_script(
            "echo 'error' >&2; exit 1",
            1,
            "app",
            "start",
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("error") || err.contains("Hook exited"));
    }
}
