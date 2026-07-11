use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::hide::{ghost_device_count, hidden_camera_count};
use crate::loopback::{
    build_video_device_holder_map, is_cam_shim_loopback, list_loopback_devices,
};
use crate::probe::{scan_devices_sysfs, DeviceReport};

pub const STATE_FILE: &str = "/run/cam-shim/state.json";
pub const HEARTBEAT_STALE_SECS: u64 = 30;

#[derive(Debug, Clone, Default, Serialize)]
pub struct RuntimeSnapshot {
    pub loopback_module_loaded: bool,
    pub serve_running: bool,
    pub hidden_cameras: usize,
    pub ghost_nodes: usize,
    pub visible_capture_devices: usize,
    pub needs_shim_devices: usize,
    pub loopbacks: Vec<LoopbackSnapshot>,
    pub supervisor_state: Option<SupervisorStateSnapshot>,
    pub devices: Vec<DeviceReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoopbackSnapshot {
    pub path: String,
    pub name: String,
    pub cam_shim: bool,
    pub holders: Vec<ProcessHolder>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessHolder {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SupervisorStateSnapshot {
    pub updated_at_ms: u64,
    pub managed: Vec<ManagedCameraSnapshot>,
    pub quarantined: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ManagedCameraSnapshot {
    pub serial: String,
    pub loopback_path: String,
    pub consecutive_failures: u32,
    pub quarantined: bool,
    pub last_heartbeat_ms: u64,
}

pub fn collect_runtime_snapshot() -> Result<RuntimeSnapshot> {
    let holder_map = build_video_device_holder_map();
    let devices = scan_devices_sysfs()?;
    let visible_capture_devices = devices.len();
    let needs_shim_devices = devices.iter().filter(|d| d.needs_shim).count();

    let loopbacks = list_loopback_devices()?
        .into_iter()
        .map(|device| {
            let cam_shim = is_cam_shim_loopback(&device.name);
            let holders = holder_map
                .get(&device.path)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|holder| ProcessHolder {
                    pid: holder.pid,
                    name: holder.label,
                })
                .collect();
            LoopbackSnapshot {
                path: device.path,
                name: device.name,
                cam_shim,
                holders,
            }
        })
        .collect();

    Ok(RuntimeSnapshot {
        loopback_module_loaded: fs::metadata("/sys/module/v4l2loopback").is_ok(),
        serve_running: cam_shim_serve_running(),
        hidden_cameras: hidden_camera_count().unwrap_or(0),
        ghost_nodes: ghost_device_count().unwrap_or(0),
        visible_capture_devices,
        needs_shim_devices,
        loopbacks,
        supervisor_state: read_supervisor_state(),
        devices,
    })
}

pub fn cam_shim_serve_running() -> bool {
    let proc = match fs::read_dir("/proc") {
        Ok(proc) => proc,
        Err(_) => return false,
    };

    let own_pid = std::process::id();
    for entry in proc.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == own_pid {
            continue;
        }

        let Ok(raw) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };

        let parts: Vec<&str> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok())
            .collect();

        let Some(exe) = parts.first() else {
            continue;
        };
        if !exe.ends_with("cam-shim") && !exe.contains("/cam-shim") {
            continue;
        }
        if parts.get(1) == Some(&"serve") {
            return true;
        }
    }

    false
}

pub fn read_supervisor_state() -> Option<SupervisorStateSnapshot> {
    let raw = fs::read_to_string(STATE_FILE).ok()?;
    serde_json::from_str(&raw).ok()
}

pub fn process_name(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|_| format!("pid-{pid}"))
}

pub fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn age_ms_since(timestamp_ms: u64) -> u64 {
    unix_now_ms().saturating_sub(timestamp_ms)
}

pub fn heartbeat_age_secs(last_heartbeat_ms: u64) -> Option<u64> {
    if last_heartbeat_ms == 0 {
        return None;
    }
    Some(age_ms_since(last_heartbeat_ms) / 1000)
}

pub fn heartbeat_is_stale(age_secs: u64) -> bool {
    age_secs > HEARTBEAT_STALE_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_heartbeat_threshold() {
        assert!(!heartbeat_is_stale(10));
        assert!(heartbeat_is_stale(45));
    }
}
