use std::path::Path;

use serde::Serialize;

use crate::error::Result;
use crate::loopback::loopback_consumer_count;
use crate::probe::DeviceReport;
use crate::runtime::{
    age_ms_since, collect_runtime_snapshot, heartbeat_age_secs, heartbeat_is_stale, ProcessHolder,
    RuntimeSnapshot, HEARTBEAT_STALE_SECS, STATE_FILE,
};

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub serve_running: bool,
    pub loopback_module_loaded: bool,
    pub state_file: &'static str,
    pub state_present: bool,
    pub state_age_ms: Option<u64>,
    pub ghost_nodes: usize,
    pub visible_capture_devices: usize,
    pub needs_shim_devices: usize,
    pub managed: Vec<ManagedStatus>,
    pub quarantined: Vec<String>,
    pub loopbacks: Vec<LoopbackStatus>,
    pub visible_cameras: Vec<CameraStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedStatus {
    pub serial: String,
    pub loopback_path: String,
    pub consecutive_failures: u32,
    pub quarantined: bool,
    pub heartbeat_age_secs: Option<u64>,
    pub heartbeat_stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LoopbackStatus {
    pub path: String,
    pub name: String,
    pub cam_shim: bool,
    pub active_readers: Option<u32>,
    pub holders: Vec<ProcessHolder>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CameraStatus {
    pub path: String,
    pub name: String,
    pub standardized_name: String,
    pub needs_shim: bool,
    pub compatible: bool,
}

pub fn collect_status() -> Result<StatusReport> {
    let snapshot = collect_runtime_snapshot()?;
    let visible_cameras = snapshot
        .devices
        .iter()
        .map(device_report_to_status)
        .collect();

    let state_age_ms = snapshot
        .supervisor_state
        .as_ref()
        .map(|state| age_ms_since(state.updated_at_ms));

    Ok(StatusReport {
        serve_running: snapshot.serve_running,
        loopback_module_loaded: snapshot.loopback_module_loaded,
        state_file: STATE_FILE,
        state_present: snapshot.supervisor_state.is_some(),
        state_age_ms,
        ghost_nodes: snapshot.ghost_nodes,
        visible_capture_devices: snapshot.visible_capture_devices,
        needs_shim_devices: snapshot.needs_shim_devices,
        managed: managed_status(&snapshot),
        quarantined: snapshot
            .supervisor_state
            .as_ref()
            .map(|state| state.quarantined.clone())
            .unwrap_or_default(),
        loopbacks: loopback_status(&snapshot),
        visible_cameras,
    })
}

fn device_report_to_status(device: &DeviceReport) -> CameraStatus {
    CameraStatus {
        path: device.path.clone(),
        name: device.name.clone(),
        standardized_name: device.standardized_name.clone(),
        needs_shim: device.needs_shim,
        compatible: device.compatible,
    }
}

pub fn print_status(report: &StatusReport, json: bool) -> Result<()> {
    if json {
        let body = serde_json::to_string_pretty(report).map_err(|err| {
            crate::error::CamShimError::Io(std::io::Error::other(format!(
                "failed to serialize status report: {err}"
            )))
        })?;
        println!("{body}");
        return Ok(());
    }

    println!("cam-shim status");
    println!("===============");
    println!();
    println!(
        "Serve:    {}",
        if report.serve_running {
            "running"
        } else {
            "not running"
        }
    );
    println!(
        "Module:   v4l2loopback {}",
        if report.loopback_module_loaded {
            "loaded"
        } else {
            "missing"
        }
    );
    print_state_file(report);
    println!(
        "Cameras:  {} visible, {} need shim, {} ghost node(s)",
        report.visible_capture_devices,
        report.needs_shim_devices,
        report.ghost_nodes
    );
    println!();

    print_managed(report);
    print_loopbacks(report);
    print_visible_cameras(report);
    print_quarantined(report);

    Ok(())
}

fn managed_status(snapshot: &RuntimeSnapshot) -> Vec<ManagedStatus> {
    let Some(state) = &snapshot.supervisor_state else {
        return Vec::new();
    };

    let state_age_ms = age_ms_since(state.updated_at_ms);
    let state_fresh = state_age_ms <= HEARTBEAT_STALE_SECS.saturating_mul(1000);

    state
        .managed
        .iter()
        .map(|camera| {
            let heartbeat_age_secs = heartbeat_age_secs(camera.last_heartbeat_ms);
            let heartbeat_stale = state_fresh
                && snapshot.serve_running
                && heartbeat_age_secs.is_some_and(heartbeat_is_stale);
            ManagedStatus {
                serial: camera.serial.clone(),
                loopback_path: camera.loopback_path.clone(),
                consecutive_failures: camera.consecutive_failures,
                quarantined: camera.quarantined,
                heartbeat_age_secs,
                heartbeat_stale,
            }
        })
        .collect()
}

fn loopback_status(snapshot: &RuntimeSnapshot) -> Vec<LoopbackStatus> {
    snapshot
        .loopbacks
        .iter()
        .map(|loopback| LoopbackStatus {
            path: loopback.path.clone(),
            name: loopback.name.clone(),
            cam_shim: loopback.cam_shim,
            active_readers: loopback_consumer_count(&loopback.path),
            holders: loopback.holders.clone(),
        })
        .collect()
}

fn print_state_file(report: &StatusReport) {
    if !report.state_present {
        println!(
            "State:    {STATE_FILE} (missing — serve not running or started with --no-state-file)"
        );
        return;
    }

    let age = report
        .state_age_ms
        .map(|ms| format!("{}s ago", ms / 1000))
        .unwrap_or_else(|| "unknown age".into());

    if report.serve_running {
        println!("State:    {STATE_FILE} (updated {age})");
    } else {
        println!("State:    {STATE_FILE} (stale, updated {age}; serve is not running)");
    }
}

fn print_managed(report: &StatusReport) {
    if report.managed.is_empty() {
        if report.serve_running {
            println!("Managed:  none (waiting for a compatible camera)");
        } else {
            println!("Managed:  none");
        }
        println!();
        return;
    }

    println!("Managed cameras ({})", report.managed.len());
    if !report.serve_running {
        println!("  (last session — serve is stopped)");
    }
    for camera in &report.managed {
        let status = if !report.serve_running {
            "stopped"
        } else if camera.quarantined {
            "quarantined"
        } else if camera.heartbeat_stale {
            "stale"
        } else {
            "active"
        };

        let heartbeat = match camera.heartbeat_age_secs {
            Some(0) => "just now".into(),
            Some(secs) => format!("{secs}s ago"),
            None => "none".into(),
        };

        let loopback = if Path::new(&camera.loopback_path).exists() {
            camera.loopback_path.clone()
        } else {
            format!("{} (missing)", camera.loopback_path)
        };

        println!("  {}", camera.serial);
        println!("    loopback:  {loopback}");
        println!("    heartbeat: {heartbeat}");
        println!("    failures:  {}", camera.consecutive_failures);
        println!("    status:    {status}");
    }
    println!();
}

fn print_loopbacks(report: &StatusReport) {
    if report.loopbacks.is_empty() {
        println!("Loopback: none");
        println!();
        return;
    }

    println!("Loopback devices");
    for loopback in &report.loopbacks {
        let tag = if loopback.cam_shim {
            "cam-shim"
        } else {
            "other"
        };
        let readers = loopback
            .active_readers
            .map(|count| format!(", readers: {count}"))
            .unwrap_or_default();

        if loopback.holders.is_empty() {
            println!("  {} — {} [{tag}{readers}]", loopback.path, loopback.name);
        } else {
            let holders = loopback
                .holders
                .iter()
                .map(|holder| format!("{} ({})", holder.name, holder.pid))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "  {} — {} [{tag}{readers}, held by {holders}]",
                loopback.path, loopback.name
            );
        }
    }
    println!();
}

fn print_visible_cameras(report: &StatusReport) {
    if report.visible_cameras.is_empty() {
        return;
    }

    println!("Visible capture devices");
    for camera in &report.visible_cameras {
        let flags = camera_flags(camera);
        println!("  {} — {} [{flags}]", camera.path, camera.name);
        if camera.needs_shim {
            println!("    standardized: {}", camera.standardized_name);
        }
    }
    println!();
}

fn print_quarantined(report: &StatusReport) {
    if report.quarantined.is_empty() {
        return;
    }

    println!("Quarantined serials: {}", report.quarantined.join(", "));
    println!();
}

fn camera_flags(camera: &CameraStatus) -> String {
    let mut flags = Vec::new();
    if camera.needs_shim {
        flags.push("needs shim");
    } else if camera.compatible {
        flags.push("compatible");
    }
    if flags.is_empty() {
        "unknown".into()
    } else {
        flags.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_flag_labels() {
        let camera = CameraStatus {
            path: "/dev/video0".into(),
            name: "Test".into(),
            standardized_name: "Test - Linux Standardized".into(),
            needs_shim: true,
            compatible: false,
        };
        assert_eq!(camera_flags(&camera), "needs shim");
    }
}
