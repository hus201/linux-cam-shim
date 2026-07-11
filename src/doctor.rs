use std::process::Command;

use serde::Serialize;

use crate::error::Result;
use crate::loopback::{
    clean_loopback_devices, ensure_module_loaded, stop_cam_shim_processes, unload_loopback_module,
};
use crate::runtime::{
    age_ms_since, collect_runtime_snapshot, heartbeat_age_secs, heartbeat_is_stale,
    RuntimeSnapshot, STATE_FILE, HEARTBEAT_STALE_SECS,
};
use crate::session::restore_all_hidden;

#[derive(Debug, Clone)]
pub struct DoctorConfig {
    pub check_only: bool,
    pub reload_module: bool,
    pub force: bool,
    pub json: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub before: RuntimeSnapshot,
    pub actions: DoctorActions,
    pub after: RuntimeSnapshot,
    pub issues: Vec<String>,
    pub recommendations: Vec<String>,
    pub v4l2_list_devices: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DoctorActions {
    pub stopped_daemons: bool,
    pub restored_hidden: Vec<String>,
    pub ghosts_removed: Vec<String>,
    pub stale_hidden_removed: Vec<String>,
    pub loopbacks_removed: Vec<String>,
    pub loopback_failures: Vec<String>,
    pub module_loaded: bool,
    pub module_reloaded: bool,
}

pub fn run_doctor(config: DoctorConfig) -> Result<DoctorReport> {
    let before = collect_runtime_snapshot()?;

    let mut actions = DoctorActions::default();
    let mut action_issues = Vec::new();

    if !config.check_only {
        if config.force && (before.serve_running || loopback_held_by_others(&before)) {
            stop_cam_shim_processes();
            actions.stopped_daemons = true;
        }

        let repair = restore_all_hidden()?;
        actions.restored_hidden = repair.restored;
        actions.ghosts_removed = repair.ghosts_removed;
        actions.stale_hidden_removed = repair.stale_hidden_removed;

        let clean = clean_loopback_devices(false, config.force)?;
        actions.loopbacks_removed = clean.removed;
        actions.loopback_failures = clean
            .failed
            .iter()
            .map(|failure| format!("{}: {}", failure.path, failure.reason))
            .collect();

        match ensure_module_loaded() {
            Ok(()) => actions.module_loaded = true,
            Err(err) => action_issues.push(format!("failed to load v4l2loopback: {err}")),
        }

        if config.reload_module {
            if config.force {
                stop_cam_shim_processes();
                actions.stopped_daemons = true;
            }

            match unload_loopback_module() {
                Ok(()) => match ensure_module_loaded() {
                    Ok(()) => actions.module_reloaded = true,
                    Err(err) => action_issues.push(format!("module reload load failed: {err}")),
                },
                Err(err) => action_issues.push(format!("module reload unload failed: {err}")),
            }
        }
    }

    let after = collect_runtime_snapshot()?;
    let mut issues = diagnose(&after);
    issues.extend(action_issues);
    issues.sort();
    issues.dedup();

    let recommendations = build_recommendations(&after, &issues);
    let v4l2_list_devices = v4l2_list_devices_output();
    let ok = issues.is_empty();

    Ok(DoctorReport {
        ok,
        before,
        actions,
        after,
        issues,
        recommendations,
        v4l2_list_devices,
    })
}

pub fn print_doctor_report(report: &DoctorReport, json: bool) -> Result<()> {
    if json {
        let body = serde_json::to_string_pretty(report).map_err(|err| {
            crate::error::CamShimError::Io(std::io::Error::other(format!(
                "failed to serialize doctor report: {err}"
            )))
        })?;
        println!("{body}");
        return Ok(());
    }

    println!("cam-shim doctor");
    println!("===============");
    println!();

    if report.actions.is_empty() {
        print_snapshot("System", &report.after);
    } else {
        print_snapshot("Before", &report.before);
        println!("Actions taken");
        println!("-------------");
        print_actions(&report.actions);
        println!();
        print_snapshot("After", &report.after);
    }

    if !report.issues.is_empty() {
        println!("Issues");
        println!("------");
        for issue in &report.issues {
            println!("  - {issue}");
        }
        println!();
    }

    if let Some(output) = &report.v4l2_list_devices {
        println!("v4l2-ctl --list-devices");
        println!("-----------------------");
        print!("{output}");
        if !output.ends_with('\n') {
            println!();
        }
        println!();
    } else {
        println!("v4l2-ctl not available — install v4l-utils for a device summary.");
        println!();
    }

    if !report.recommendations.is_empty() {
        println!("Recommendations");
        println!("---------------");
        for item in &report.recommendations {
            println!("  - {item}");
        }
        println!();
    }

    if report.ok {
        println!("Status: OK");
    } else {
        println!("Status: needs attention");
    }

    Ok(())
}

impl DoctorActions {
    fn is_empty(&self) -> bool {
        !self.stopped_daemons
            && self.restored_hidden.is_empty()
            && self.ghosts_removed.is_empty()
            && self.stale_hidden_removed.is_empty()
            && self.loopbacks_removed.is_empty()
            && self.loopback_failures.is_empty()
            && !self.module_loaded
            && !self.module_reloaded
    }
}

fn diagnose(snapshot: &RuntimeSnapshot) -> Vec<String> {
    let mut issues = Vec::new();

    if !snapshot.loopback_module_loaded {
        issues.push("v4l2loopback kernel module is not loaded".into());
    }
    if snapshot.hidden_cameras > 0 {
        issues.push(format!(
            "{} camera node(s) still hidden under /dev/cam-shim-hidden/",
            snapshot.hidden_cameras
        ));
    }
    if snapshot.ghost_nodes > 0 {
        issues.push(format!(
            "{} stale /dev/video* ghost node(s) remain",
            snapshot.ghost_nodes
        ));
    }
    if snapshot.visible_capture_devices == 0
        && snapshot.hidden_cameras == 0
        && snapshot.ghost_nodes == 0
    {
        issues.push("no visible V4L2 capture devices found".into());
    }

    for loopback in &snapshot.loopbacks {
        if !loopback.holders.is_empty() {
            let holders = loopback
                .holders
                .iter()
                .map(|holder| format!("{} ({})", holder.name, holder.pid))
                .collect::<Vec<_>>()
                .join(", ");
            issues.push(format!("{} is held open by {holders}", loopback.path));
        }
    }

    if let Some(state) = &snapshot.supervisor_state {
        if !state.quarantined.is_empty() {
            issues.push(format!(
                "quarantined camera serial(s): {}",
                state.quarantined.join(", ")
            ));
        }

        let state_fresh = age_ms_since(state.updated_at_ms) <= HEARTBEAT_STALE_SECS.saturating_mul(1000);
        if state_fresh {
            for camera in &state.managed {
                if camera.quarantined {
                    continue;
                }
                if let Some(age_secs) = heartbeat_age_secs(camera.last_heartbeat_ms) {
                    if heartbeat_is_stale(age_secs) && snapshot.serve_running {
                        issues.push(format!(
                            "shim for {} ({}) has stale heartbeat ({}s ago)",
                            camera.serial, camera.loopback_path, age_secs
                        ));
                    }
                }
            }
        }
    }

    issues
}

fn build_recommendations(snapshot: &RuntimeSnapshot, issues: &Vec<String>) -> Vec<String> {
    let mut out = Vec::new();

    if snapshot.hidden_cameras > 0 {
        out.push("Run: sudo cam-shim restore".into());
    }
    if snapshot.ghost_nodes > 0 {
        out.push("Run: sudo cam-shim restore (repairs ghost nodes)".into());
    }
    if !snapshot.loopback_module_loaded {
        out.push("Run: sudo modprobe v4l2loopback exclusive_caps=1".into());
    }
    if snapshot.needs_shim_devices > 0 && !snapshot.serve_running {
        out.push("Run: sudo cam-shim serve (or sudo systemctl start cam-shim)".into());
    }
    if issues.iter().any(|issue| issue.contains("held open")) {
        out.push(
            "Close apps using the virtual camera (Discord, guvcview), or run with --force".into(),
        );
    }
    if issues.iter().any(|issue| issue.contains("module reload")) {
        out.push(
            "Stop serve first: sudo systemctl stop cam-shim, then sudo cam-shim doctor --reload"
                .into(),
        );
    }
    if snapshot.visible_capture_devices == 0 {
        out.push("Unplug the webcam, wait 5 seconds, replug, then run doctor again".into());
    }

    out.sort();
    out.dedup();
    out
}

fn print_snapshot(title: &str, snapshot: &RuntimeSnapshot) {
    println!("{title}");
    println!("{}", "-".repeat(title.len()));
    println!(
        "  v4l2loopback: {}",
        if snapshot.loopback_module_loaded {
            "loaded"
        } else {
            "missing"
        }
    );
    println!(
        "  serve daemon: {}",
        if snapshot.serve_running {
            "running"
        } else {
            "not running"
        }
    );
    println!("  visible capture devices: {}", snapshot.visible_capture_devices);
    println!("  needs shim: {}", snapshot.needs_shim_devices);
    println!("  hidden cameras: {}", snapshot.hidden_cameras);
    println!("  ghost nodes: {}", snapshot.ghost_nodes);

    if snapshot.loopbacks.is_empty() {
        println!("  loopback devices: none");
    } else {
        println!("  loopback devices:");
        for loopback in &snapshot.loopbacks {
            let tag = if loopback.cam_shim { "cam-shim" } else { "other" };
            if loopback.holders.is_empty() {
                println!("    {} — {} [{tag}]", loopback.path, loopback.name);
            } else {
                let holders = loopback
                    .holders
                    .iter()
                    .map(|holder| format!("{} ({})", holder.name, holder.pid))
                    .collect::<Vec<_>>()
                    .join(", ");
                println!(
                    "    {} — {} [{tag}, held by {holders}]",
                    loopback.path, loopback.name
                );
            }
        }
    }

    if let Some(state) = &snapshot.supervisor_state {
        println!("  supervisor state: {}", STATE_FILE);
        println!("    updated: {} ms ago", age_ms_since(state.updated_at_ms));
        if state.managed.is_empty() {
            println!("    managed cameras: none");
        } else {
            println!("    managed cameras:");
            for camera in &state.managed {
                let status = if camera.quarantined {
                    "quarantined"
                } else {
                    "active"
                };
                println!(
                    "      {} → {} [{status}, failures={}]",
                    camera.serial, camera.loopback_path, camera.consecutive_failures
                );
            }
        }
    }

    println!();
}

fn print_actions(actions: &DoctorActions) {
    if actions.stopped_daemons {
        println!("  stopped cam-shim serve/fix/relay processes");
    }
    if !actions.restored_hidden.is_empty() {
        println!("  restored: {}", actions.restored_hidden.join(", "));
    }
    if !actions.ghosts_removed.is_empty() {
        println!("  removed ghost nodes: {}", actions.ghosts_removed.join(", "));
    }
    if !actions.stale_hidden_removed.is_empty() {
        println!(
            "  removed stale hidden nodes: {}",
            actions.stale_hidden_removed.join(", ")
        );
    }
    if !actions.loopbacks_removed.is_empty() {
        println!(
            "  removed orphan loopbacks: {}",
            actions.loopbacks_removed.join(", ")
        );
    }
    for failure in &actions.loopback_failures {
        println!("  loopback cleanup failed: {failure}");
    }
    if actions.module_loaded {
        println!("  ensured v4l2loopback is loaded");
    }
    if actions.module_reloaded {
        println!("  reloaded v4l2loopback kernel module");
    }
    if actions.is_empty() {
        println!("  (none)");
    }
}

fn loopback_held_by_others(snapshot: &RuntimeSnapshot) -> bool {
    let own_pid = std::process::id();
    snapshot.loopbacks.iter().any(|loopback| {
        loopback
            .holders
            .iter()
            .any(|holder| holder.pid != own_pid)
    })
}

fn v4l2_list_devices_output() -> Option<String> {
    let output = Command::new("v4l2-ctl").arg("--list-devices").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_actions_detected() {
        assert!(DoctorActions::default().is_empty());
    }
}
