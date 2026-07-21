use std::collections::HashMap;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use std::collections::HashSet;

use crate::hotplug::spawn_hotplug_monitor;

use crate::compat::DEFAULT_TARGET_FPS;
use crate::compat::{DEFAULT_MAX_CAPTURE_HEIGHT, DEFAULT_MAX_CAPTURE_WIDTH};
use crate::devices::{camera_identity, camera_serial_present, repair_video_devices};
use crate::error::Result;
use crate::loopback::{
    build_video_device_holder_map, cam_shim_held_device_paths, clean_loopback_devices,
    create_device_with_options, ensure_module_loaded, remove_loopback_device, CreateDeviceOptions,
};
use crate::probe::{scan_devices_with_options, ProbeDepth};
use crate::shim::{run_shim_until, ShimConfig};

const DEFAULT_POLL_SECS: u64 = 5;
const DEFAULT_MAX_FAILURES: u32 = 5;
const DEFAULT_QUARANTINE_SECS: u64 = 120;
const DEFAULT_BACKOFF_BASE_MS: u64 = 1_000;
const DEFAULT_MAX_BACKOFF_MS: u64 = 60_000;
const DEFAULT_STALE_FRAME_SECS: u64 = 10;
const DEFAULT_WATCHDOG_SECS: u64 = 30;
const STATE_DIR: &str = "/run/cam-shim";
const STATE_FILE: &str = "/run/cam-shim/state.json";

pub struct ServeConfig {
    pub target_fps: u32,
    pub poll_interval: Duration,
    pub max_failures: u32,
    pub quarantine_duration: Duration,
    pub backoff_base: Duration,
    pub max_backoff: Duration,
    pub stale_frame_timeout: Duration,
    pub watchdog_timeout: Duration,
    pub write_state_file: bool,
    pub hotplug: bool,
    pub max_capture_width: u32,
    pub max_capture_height: u32,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            target_fps: DEFAULT_TARGET_FPS,
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            max_failures: DEFAULT_MAX_FAILURES,
            quarantine_duration: Duration::from_secs(DEFAULT_QUARANTINE_SECS),
            backoff_base: Duration::from_millis(DEFAULT_BACKOFF_BASE_MS),
            max_backoff: Duration::from_millis(DEFAULT_MAX_BACKOFF_MS),
            stale_frame_timeout: Duration::from_secs(DEFAULT_STALE_FRAME_SECS),
            watchdog_timeout: Duration::from_secs(DEFAULT_WATCHDOG_SECS),
            write_state_file: true,
            hotplug: true,
            max_capture_width: DEFAULT_MAX_CAPTURE_WIDTH,
            max_capture_height: DEFAULT_MAX_CAPTURE_HEIGHT,
        }
    }
}

struct ShimCandidate {
    serial: String,
    source_path: String,
    label: String,
}

struct ManagedCamera {
    source_path: String,
    loopback_index: u32,
    loopback_path: String,
    stop: Arc<AtomicBool>,
    heartbeat: Arc<AtomicU64>,
    worker: JoinHandle<()>,
}

#[derive(Default)]
struct SerialState {
    consecutive_failures: u32,
    quarantined_until: Option<Instant>,
    retry_after: Option<Instant>,
}

impl SerialState {
    fn record_failure(&mut self, config: &ServeConfig) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let exp = self.consecutive_failures.saturating_sub(1).min(6);
        let backoff_ms = config
            .backoff_base
            .as_millis()
            .saturating_mul(1_u128 << exp)
            .min(config.max_backoff.as_millis());
        self.retry_after = Some(Instant::now() + Duration::from_millis(backoff_ms as u64));

        if self.consecutive_failures >= config.max_failures {
            self.quarantined_until = Some(Instant::now() + config.quarantine_duration);
            tracing::warn!(
                failures = self.consecutive_failures,
                quarantine_secs = config.quarantine_duration.as_secs(),
                "camera quarantined after repeated shim failures"
            );
        }
    }

    fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.quarantined_until = None;
        self.retry_after = None;
    }

    fn can_start(&self, now: Instant) -> bool {
        if self.quarantined_until.is_some_and(|until| now < until) {
            return false;
        }

        if self.retry_after.is_some_and(|after| now < after) {
            return false;
        }

        true
    }

    fn is_quarantined(&self, now: Instant) -> bool {
        self.quarantined_until.is_some_and(|until| now < until)
    }
}

struct Supervisor {
    managed: HashMap<String, ManagedCamera>,
    serial_states: HashMap<String, SerialState>,
    last_reconcile: Instant,
}

#[derive(Serialize)]
struct SupervisorSnapshot {
    updated_at_ms: u64,
    managed: Vec<ManagedSnapshot>,
    quarantined: Vec<String>,
}

#[derive(Serialize)]
struct ManagedSnapshot {
    serial: String,
    loopback_path: String,
    consecutive_failures: u32,
    quarantined: bool,
    last_heartbeat_ms: u64,
}

pub fn run_supervisor(config: ServeConfig) -> Result<()> {
    ensure_module_loaded()?;
    startup_self_check()?;

    if config.write_state_file {
        let _ = fs::remove_file(STATE_FILE);
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    {
        let shutdown = shutdown.clone();
        ctrlc::set_handler(move || {
            shutdown.store(true, Ordering::SeqCst);
        })
        .map_err(|err| {
            crate::error::CamShimError::Io(std::io::Error::other(format!("ctrl-c handler: {err}")))
        })?;
    }

    let last_reconcile_ms = Arc::new(AtomicU64::new(now_unix_ms()));
    spawn_watchdog(
        shutdown.clone(),
        last_reconcile_ms.clone(),
        config.watchdog_timeout,
    );

    let mut supervisor = Supervisor {
        managed: HashMap::new(),
        serial_states: HashMap::new(),
        last_reconcile: Instant::now(),
    };

    tracing::info!(
        target_fps = config.target_fps,
        poll_ms = config.poll_interval.as_millis(),
        hotplug = config.hotplug,
        max_failures = config.max_failures,
        quarantine_secs = config.quarantine_duration.as_secs(),
        stale_frame_secs = config.stale_frame_timeout.as_secs(),
        "cam-shim supervisor started"
    );

    let (hotplug_tx, hotplug_rx) = mpsc::channel();
    let hotplug_handle = if config.hotplug {
        match spawn_hotplug_monitor(shutdown.clone(), hotplug_tx) {
            Ok(handle) => Some(handle),
            Err(err) => {
                tracing::warn!(%err, "netlink hotplug disabled — using polling only");
                None
            }
        }
    } else {
        None
    };

    let mut hotplug_active = hotplug_handle.is_some();

    supervisor.reconcile(&config)?;
    last_reconcile_ms.store(now_unix_ms(), Ordering::Relaxed);

    while !shutdown.load(Ordering::SeqCst) {
        if hotplug_active {
            match hotplug_rx.recv_timeout(config.poll_interval) {
                Ok(()) => tracing::debug!("reconciling after netlink hotplug event"),
                Err(RecvTimeoutError::Timeout) => tracing::trace!("reconciling on fallback poll"),
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::warn!("hotplug monitor stopped — continuing with fallback polling");
                    hotplug_active = false;
                    continue;
                }
            }
        } else {
            thread::sleep(config.poll_interval);
        }

        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        supervisor.reconcile(&config)?;
        last_reconcile_ms.store(now_unix_ms(), Ordering::Relaxed);
    }

    if let Some(handle) = hotplug_handle {
        let _ = handle.join();
    }

    tracing::info!("cam-shim supervisor shutting down");
    for (_, camera) in supervisor.managed.drain() {
        stop_managed(camera);
    }
    let _ = fs::remove_file(STATE_FILE);

    Ok(())
}

impl Supervisor {
    fn reconcile(&mut self, config: &ServeConfig) -> Result<()> {
        self.last_reconcile = Instant::now();
        let now = Instant::now();

        self.reap_dead_workers(config, now);
        self.reap_stale_workers(config, now);

        let skip_paths = cam_shim_held_device_paths(&build_video_device_holder_map());
        let candidates = discover_candidates(&skip_paths)?;
        let active_serials: std::collections::HashSet<_> =
            candidates.iter().map(|c| c.serial.clone()).collect();

        let stale: Vec<String> = self
            .managed
            .keys()
            .filter(|serial| !active_serials.contains(*serial) && !camera_serial_present(serial))
            .cloned()
            .collect();

        for serial in stale {
            if let Some(camera) = self.managed.remove(&serial) {
                tracing::info!(%serial, "camera unplugged — stopping shim");
                stop_managed(camera);
                self.serial_states.remove(&serial);
            }
        }

        for candidate in &candidates {
            let restart = match self.managed.get(&candidate.serial) {
                Some(camera) if managed_source_stale(camera, candidate) => {
                    tracing::info!(
                        serial = %candidate.serial,
                        old = %camera.source_path,
                        new = %candidate.source_path,
                        "camera path changed — restarting shim"
                    );
                    true
                }
                Some(_) => continue,
                None => false,
            };

            if restart {
                if let Some(camera) = self.managed.remove(&candidate.serial) {
                    stop_managed(camera);
                }
                // Restore/replug should not inherit prior backoff/quarantine.
                self.serial_states
                    .entry(candidate.serial.clone())
                    .or_default()
                    .record_success();
            }

            let state = self
                .serial_states
                .entry(candidate.serial.clone())
                .or_default();

            if !state.can_start(now) {
                if state.is_quarantined(now) {
                    tracing::warn!(serial = %candidate.serial, "skipping quarantined camera");
                } else {
                    tracing::info!(serial = %candidate.serial, "waiting for restart backoff");
                }
                continue;
            }

            match start_managed(candidate, config) {
                Ok(camera) => {
                    tracing::info!(
                        serial = %candidate.serial,
                        source = %candidate.source_path,
                        target = %camera.loopback_path,
                        "shim started"
                    );
                    state.record_success();
                    self.managed.insert(candidate.serial.clone(), camera);
                }
                Err(err) => {
                    tracing::warn!(
                        serial = %candidate.serial,
                        source = %candidate.source_path,
                        %err,
                        "failed to start shim"
                    );
                    state.record_failure(config);
                }
            }
        }

        if config.write_state_file {
            let _ = write_state_snapshot(self, now);
        }

        Ok(())
    }

    fn reap_dead_workers(&mut self, config: &ServeConfig, _now: Instant) {
        let dead: Vec<String> = self
            .managed
            .iter()
            .filter(|(_, camera)| camera.worker.is_finished())
            .map(|(serial, _)| serial.clone())
            .collect();

        for serial in dead {
            if let Some(camera) = self.managed.remove(&serial) {
                tracing::warn!(%serial, "shim worker exited — cleaning up");
                stop_managed(camera);
                self.serial_states
                    .entry(serial)
                    .or_default()
                    .record_failure(config);
            }
        }
    }

    fn reap_stale_workers(&mut self, config: &ServeConfig, _now: Instant) {
        let stale_timeout = config.stale_frame_timeout;
        let stale: Vec<String> = self
            .managed
            .iter()
            .filter(|(_, camera)| {
                !camera.worker.is_finished()
                    && heartbeat_age(camera.heartbeat.load(Ordering::Relaxed)) > stale_timeout
            })
            .map(|(serial, _)| serial.clone())
            .collect();

        for serial in stale {
            if let Some(camera) = self.managed.remove(&serial) {
                tracing::warn!(
                    %serial,
                    stale_secs = stale_timeout.as_secs(),
                    "shim worker stale — restarting"
                );
                stop_managed(camera);
                self.serial_states
                    .entry(serial)
                    .or_default()
                    .record_failure(config);
            }
        }
    }
}

fn startup_self_check() -> Result<()> {
    tracing::info!("supervisor startup self-check");

    let repair = repair_video_devices()?;
    if !repair.ghosts_removed.is_empty() {
        tracing::info!(ghosts = ?repair.ghosts_removed, "removed ghost device nodes");
    }

    let clean = clean_loopback_devices(false, false)?;
    if !clean.removed.is_empty() {
        tracing::info!(removed = ?clean.removed, "removed orphan loopback devices");
    }

    Ok(())
}

fn spawn_watchdog(shutdown: Arc<AtomicBool>, last_reconcile_ms: Arc<AtomicU64>, timeout: Duration) {
    thread::spawn(move || {
        let check_interval = Duration::from_secs(5);
        while !shutdown.load(Ordering::SeqCst) {
            thread::sleep(check_interval);
            let last = last_reconcile_ms.load(Ordering::Relaxed);
            if last == 0 {
                continue;
            }
            let age = Duration::from_millis(now_unix_ms().saturating_sub(last));
            if age > timeout {
                tracing::error!(
                    stale_secs = age.as_secs(),
                    limit_secs = timeout.as_secs(),
                    "supervisor reconcile loop appears stalled"
                );
            }
        }
    });
}

fn write_state_snapshot(supervisor: &Supervisor, now: Instant) -> Result<()> {
    fs::create_dir_all(STATE_DIR)?;

    let managed = supervisor
        .managed
        .iter()
        .map(|(serial, camera)| {
            let state = supervisor.serial_states.get(serial);
            ManagedSnapshot {
                serial: serial.clone(),
                loopback_path: camera.loopback_path.clone(),
                consecutive_failures: state.map(|s| s.consecutive_failures).unwrap_or(0),
                quarantined: state.is_some_and(|s| s.is_quarantined(now)),
                last_heartbeat_ms: camera.heartbeat.load(Ordering::Relaxed),
            }
        })
        .collect();

    let quarantined = supervisor
        .serial_states
        .iter()
        .filter(|(_, state)| state.is_quarantined(now))
        .map(|(serial, _)| serial.clone())
        .collect();

    let snapshot = SupervisorSnapshot {
        updated_at_ms: now_unix_ms(),
        managed,
        quarantined,
    };

    let json = serde_json::to_string_pretty(&snapshot).map_err(|err| {
        crate::error::CamShimError::Io(std::io::Error::other(format!(
            "failed to serialize supervisor state: {err}"
        )))
    })?;
    fs::write(STATE_FILE, json)?;
    Ok(())
}

fn discover_candidates(skip_paths: &HashSet<String>) -> Result<Vec<ShimCandidate>> {
    let mut by_serial: HashMap<String, ShimCandidate> = HashMap::new();

    // Always fully probe free devices. Held paths are already sysfs-only via
    // skip_paths — Quick mode only inspected the current format and often
    // mis-labeled NeedsShim cameras as Compatible after the first shim started.
    for report in scan_devices_with_options(ProbeDepth::Full, skip_paths)? {
        if !report.needs_shim {
            tracing::debug!(
                path = %report.path,
                name = %report.name,
                "skipping compatible camera"
            );
            continue;
        }

        let identity = match camera_identity(&report.path) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(path = %report.path, %err, "skipping device without serial");
                continue;
            }
        };

        if by_serial.contains_key(&identity.id_serial) {
            continue;
        }

        by_serial.insert(
            identity.id_serial.clone(),
            ShimCandidate {
                serial: identity.id_serial,
                source_path: report.path,
                label: report.standardized_name,
            },
        );
    }

    Ok(by_serial.into_values().collect())
}

fn managed_source_stale(camera: &ManagedCamera, candidate: &ShimCandidate) -> bool {
    if !std::path::Path::new(&camera.source_path).exists() {
        return true;
    }

    let managed_name = std::path::Path::new(&camera.source_path).file_name();
    let candidate_name = std::path::Path::new(&candidate.source_path).file_name();
    managed_name != candidate_name
}

fn start_managed(candidate: &ShimCandidate, config: &ServeConfig) -> Result<ManagedCamera> {
    let loopback = create_device_with_options(
        &candidate.label,
        config.target_fps,
        CreateDeviceOptions::for_camera(&candidate.serial),
    )?;
    let loopback_index = loopback
        .path
        .strip_prefix("/dev/video")
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| {
            crate::error::CamShimError::Io(std::io::Error::other(format!(
                "invalid loopback path: {}",
                loopback.path
            )))
        })?;

    tracing::info!(
        serial = %candidate.serial,
        source = %candidate.source_path,
        loopback = %loopback.path,
        label = %candidate.label,
        "shim started"
    );

    let source_path = candidate.source_path.clone();
    let shim_config = ShimConfig {
        source_path: source_path.clone(),
        target_path: loopback.path.clone(),
        target_fps: config.target_fps,
        max_capture_width: config.max_capture_width,
        max_capture_height: config.max_capture_height,
    };

    let stop = Arc::new(AtomicBool::new(true));
    let worker_stop = stop.clone();
    let heartbeat = Arc::new(AtomicU64::new(now_unix_ms()));
    let worker_heartbeat = heartbeat.clone();

    let worker = thread::spawn(move || {
        if let Err(err) = run_shim_until(shim_config, worker_stop, Some(worker_heartbeat))
        {
            tracing::error!(%err, "shim worker exited with error");
        }
    });

    Ok(ManagedCamera {
        source_path,
        loopback_index,
        loopback_path: loopback.path,
        stop,
        heartbeat,
        worker,
    })
}

fn stop_managed(camera: ManagedCamera) {
    camera.stop.store(false, Ordering::SeqCst);
    let _ = camera.worker.join();

    if let Err(err) = remove_loopback_device(camera.loopback_index) {
        tracing::warn!(
            path = %camera.loopback_path,
            %err,
            "failed to remove loopback device"
        );
    }

    let _ = repair_video_devices();
}

fn heartbeat_age(last_ms: u64) -> Duration {
    if last_ms == 0 {
        return Duration::MAX;
    }
    Duration::from_millis(now_unix_ms().saturating_sub(last_ms))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_failures() {
        let config = ServeConfig::default();
        let mut state = SerialState::default();

        state.record_failure(&config);
        assert_eq!(state.consecutive_failures, 1);
        assert!(state.retry_after.is_some());

        for _ in 0..config.max_failures {
            state.record_failure(&config);
        }
        assert!(state.is_quarantined(Instant::now()));
    }

    #[test]
    fn success_clears_failure_state() {
        let config = ServeConfig::default();
        let mut state = SerialState::default();
        state.record_failure(&config);
        state.record_success();
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.can_start(Instant::now()));
    }
}
