use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use v4l::capability::Flags as CapFlags;
use v4l::frameinterval::FrameInterval;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::compat::{standardized_label, CompatReport, CompatStatus};
use crate::devices::physical_camera_key_with_name;
use crate::error::{CamShimError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeDepth {
    /// Enumerate every format/size (used by `cam-shim scan`).
    Full,
    /// Current format only — used when discovering cameras for serve.
    Quick,
    /// Card name from sysfs only — used by status/doctor (never opens `/dev`).
    Sysfs,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceReport {
    pub path: String,
    pub name: String,
    pub driver: String,
    pub bus: String,
    pub standardized_name: String,
    pub compatible: bool,
    pub needs_shim: bool,
    pub advertised_fps: Vec<String>,
    pub issues: Vec<String>,
}

pub fn scan_devices() -> Result<Vec<DeviceReport>> {
    scan_devices_with_options(ProbeDepth::Full, &HashSet::new())
}

/// Fast scan for status/doctor — never opens capture devices (avoids UVC hangs).
pub fn scan_devices_sysfs() -> Result<Vec<DeviceReport>> {
    let reports = scan_devices_with_options(ProbeDepth::Sysfs, &HashSet::new())?;
    Ok(dedupe_by_physical_camera(reports))
}

/// Fast scan for serve — skips sysfs-only stubs for busy nodes.
pub fn scan_devices_quick(skip_paths: &HashSet<String>) -> Result<Vec<DeviceReport>> {
    scan_devices_with_options(ProbeDepth::Quick, skip_paths)
}

pub fn scan_devices_with_options(
    depth: ProbeDepth,
    skip_paths: &HashSet<String>,
) -> Result<Vec<DeviceReport>> {
    let mut reports = Vec::new();

    for path in list_video_nodes()? {
        let path_key = path.display().to_string();
        if depth == ProbeDepth::Sysfs {
            match probe_device_sysfs(&path, None) {
                Ok(report) => reports.push(report),
                Err(err) => tracing::debug!(device = %path.display(), %err, "skipping device"),
            }
            continue;
        }

        if skip_paths.contains(&path_key) {
            match probe_device_sysfs(&path, Some("in use by cam-shim (sysfs-only snapshot)")) {
                Ok(report) => reports.push(report),
                Err(err) => tracing::debug!(device = %path.display(), %err, "skipping busy device"),
            }
            continue;
        }

        match probe_device(&path, depth) {
            Ok(report) => reports.push(report),
            Err(err) => tracing::debug!(device = %path.display(), %err, "skipping device"),
        }
    }

    reports.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(reports)
}

pub fn probe_device_path(path: &str) -> Result<DeviceReport> {
    probe_device(Path::new(path), ProbeDepth::Full)
}

fn probe_device(path: &Path, depth: ProbeDepth) -> Result<DeviceReport> {
    let dev = Device::with_path(path)
        .map_err(|_| CamShimError::DeviceNotFound(path.display().to_string()))?;

    let caps = dev.query_caps()?;
    if !caps.capabilities.contains(CapFlags::VIDEO_CAPTURE) {
        return Err(CamShimError::NotCaptureDevice(path.display().to_string()));
    }

    let intervals = match depth {
        ProbeDepth::Full => collect_all_intervals(&dev)?,
        ProbeDepth::Quick => collect_current_intervals(&dev)?,
        ProbeDepth::Sysfs => {
            return Err(CamShimError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "sysfs probe does not open devices",
            )));
        }
    };

    let compat = CompatReport::from_intervals(&intervals);
    let name = caps.card.clone();

    Ok(DeviceReport {
        path: path.display().to_string(),
        name: name.clone(),
        driver: caps.driver,
        bus: caps.bus,
        standardized_name: standardized_label(&name),
        compatible: compat.status == CompatStatus::Compatible,
        needs_shim: compat.status == CompatStatus::NeedsShim,
        advertised_fps: compat
            .advertised_fps
            .iter()
            .map(|rate| rate.display())
            .collect(),
        issues: compat.issues,
    })
}

/// Read card name from sysfs without opening the device node.
fn probe_device_sysfs(path: &Path, note: Option<&str>) -> Result<DeviceReport> {
    let video_name = path
        .file_name()
        .ok_or_else(|| CamShimError::DeviceNotFound(path.display().to_string()))?
        .to_string_lossy();
    let name_path = format!("/sys/class/video4linux/{video_name}/name");
    let name = fs::read_to_string(&name_path)
        .map(|value| value.trim().to_string())
        .map_err(|err| {
            CamShimError::Io(std::io::Error::other(format!(
                "could not read {name_path}: {err}"
            )))
        })?;

    let standardized = is_standardized_device_name(&name);
    let mut issues = Vec::new();
    if let Some(note) = note {
        issues.push(note.into());
    } else if !standardized {
        issues.push("run cam-shim scan to verify fps compatibility".into());
    }

    Ok(DeviceReport {
        path: path.display().to_string(),
        name: name.clone(),
        driver: String::new(),
        bus: String::new(),
        standardized_name: standardized_label(&name),
        compatible: standardized,
        needs_shim: !standardized,
        advertised_fps: Vec::new(),
        issues,
    })
}

fn is_standardized_device_name(name: &str) -> bool {
    name.contains("Linux Standardized")
}

fn collect_current_intervals(dev: &Device) -> Result<Vec<FrameInterval>> {
    let format = Capture::format(dev)?;
    Capture::enum_frameintervals(dev, format.fourcc, format.width, format.height)
        .map_err(CamShimError::Io)
}

fn collect_all_intervals(dev: &Device) -> Result<Vec<FrameInterval>> {
    let mut intervals = Vec::new();
    for format in dev.enum_formats()? {
        for size in dev.enum_framesizes(format.fourcc)? {
            for discrete in size.size.to_discrete() {
                if let Ok(device_intervals) =
                    dev.enum_frameintervals(format.fourcc, discrete.width, discrete.height)
                {
                    intervals.extend(device_intervals);
                }
            }
        }
    }
    Ok(intervals)
}

fn list_video_nodes() -> Result<Vec<PathBuf>> {
    let mut nodes = Vec::new();

    for entry in fs::read_dir("/sys/class/video4linux")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !name.starts_with("video") || !name[5..].chars().all(|c| c.is_ascii_digit()) {
            continue;
        }

        if crate::loopback::is_loopback_sysfs_node(&entry.path()) {
            continue;
        }

        if let Some(path) = resolve_device_node(&name) {
            nodes.push(path);
        }
    }

    nodes.sort_by_key(|path| video_index(path).unwrap_or(usize::MAX));
    Ok(nodes)
}

fn resolve_device_node(video_name: &str) -> Option<PathBuf> {
    let normal = Path::new("/dev").join(video_name);
    if normal.exists() {
        return Some(normal);
    }

    None
}

fn video_index(path: &Path) -> Option<usize> {
    let name = path.file_name()?.to_string_lossy();
    name.strip_prefix("video")?.parse().ok()
}

/// UVC webcams often expose multiple `/dev/video*` nodes for one physical device.
pub fn dedupe_by_physical_camera(reports: Vec<DeviceReport>) -> Vec<DeviceReport> {
    use std::collections::HashMap;

    let mut best: HashMap<String, DeviceReport> = HashMap::new();

    for report in reports {
        let key = physical_camera_key_with_name(&report.path, &report.name);
        let report_rank = node_rank(&report);

        best.entry(key)
            .and_modify(|existing| {
                if report_rank < node_rank(existing) {
                    *existing = report.clone();
                }
            })
            .or_insert(report);
    }

    let mut out: Vec<_> = best.into_values().collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Prefer capture nodes over metadata; then lowest `/dev/videoN` index.
fn node_rank(report: &DeviceReport) -> (usize, usize) {
    let metadata = report.name.to_ascii_lowercase().contains("metadata");
    let index = video_index(Path::new(&report.path)).unwrap_or(usize::MAX);
    (metadata as usize, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standardized_name_detection() {
        assert!(is_standardized_device_name(
            "webcam: Fantech Luminous C30 - Linux Standardized"
        ));
        assert!(!is_standardized_device_name("webcam: Fantech Luminous C30"));
    }

    #[test]
    fn node_rank_prefers_capture_over_metadata() {
        let capture = DeviceReport {
            path: "/dev/video2".into(),
            name: "webcam: Example".into(),
            driver: String::new(),
            bus: String::new(),
            standardized_name: String::new(),
            compatible: false,
            needs_shim: true,
            advertised_fps: Vec::new(),
            issues: Vec::new(),
        };
        let metadata = DeviceReport {
            name: "webcam: Example Metadata".into(),
            path: "/dev/video1".into(),
            ..capture.clone()
        };
        assert!(node_rank(&capture) < node_rank(&metadata));
    }

    #[test]
    fn dedupe_keeps_lowest_video_index_per_path_key() {
        let reports = vec![
            DeviceReport {
                path: "/dev/video2".into(),
                name: "Cam".into(),
                driver: String::new(),
                bus: String::new(),
                standardized_name: "Cam".into(),
                compatible: false,
                needs_shim: true,
                advertised_fps: Vec::new(),
                issues: Vec::new(),
            },
            DeviceReport {
                path: "/dev/video1".into(),
                name: "Cam".into(),
                driver: String::new(),
                bus: String::new(),
                standardized_name: "Cam".into(),
                compatible: false,
                needs_shim: true,
                advertised_fps: Vec::new(),
                issues: Vec::new(),
            },
        ];

        let deduped = dedupe_by_physical_camera(reports);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].path, "/dev/video1");
    }
}
