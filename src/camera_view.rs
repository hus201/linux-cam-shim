use std::collections::HashMap;

use serde::Serialize;

use crate::compat::{kernel_card_label, standardized_label};
use crate::devices::physical_camera_key_with_name;
use crate::error::Result;
use crate::loopback::{is_cam_shim_loopback, list_loopback_devices, LoopbackDeviceInfo};
use crate::probe::{scan_devices, DeviceReport};
use crate::runtime::{cam_shim_serve_running, read_supervisor_state, RuntimeSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    Physical,
    VirtualCamShim,
    VirtualOther,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecommendedDevice {
    pub path: String,
    pub name: String,
    pub for_physical: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceView {
    pub path: String,
    pub name: String,
    pub role: DeviceRole,
    pub tags: Vec<String>,
    pub paired_with: Option<String>,
    pub use_in_apps: bool,
    pub needs_shim: bool,
    pub compatible: bool,
    pub standardized_name: Option<String>,
    pub driver: Option<String>,
    pub bus: Option<String>,
    pub advertised_fps: Vec<String>,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanReport {
    pub serve_running: bool,
    pub devices: Vec<DeviceView>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recommended_devices: Vec<RecommendedDevice>,
}

pub fn collect_scan_report() -> Result<ScanReport> {
    let physical = scan_devices()?;
    let loopbacks = list_loopback_devices()?;
    let serve_running = cam_shim_serve_running();
    let supervisor = read_supervisor_state();

    Ok(build_device_views(
        &physical,
        &loopbacks,
        serve_running,
        supervisor.as_ref().map(|state| &state.managed[..]),
    ))
}

pub fn device_views_from_snapshot(snapshot: &RuntimeSnapshot) -> ScanReport {
    let managed = snapshot
        .supervisor_state
        .as_ref()
        .map(|state| state.managed.as_slice());

    let loopbacks: Vec<LoopbackDeviceInfo> = snapshot
        .loopbacks
        .iter()
        .map(|loopback| LoopbackDeviceInfo {
            path: loopback.path.clone(),
            index: video_index_from_path(&loopback.path).unwrap_or(0),
            name: loopback.name.clone(),
        })
        .collect();

    build_device_views(
        &snapshot.devices,
        &loopbacks,
        snapshot.serve_running,
        managed,
    )
}

fn build_device_views(
    physical: &[DeviceReport],
    loopbacks: &[LoopbackDeviceInfo],
    serve_running: bool,
    managed: Option<&[crate::runtime::ManagedCameraSnapshot]>,
) -> ScanReport {
    let mut serial_to_loopback: HashMap<String, String> = HashMap::new();
    if let Some(managed) = managed {
        for camera in managed {
            if !camera.quarantined {
                serial_to_loopback.insert(camera.serial.clone(), camera.loopback_path.clone());
            }
        }
    }

    let mut physical_to_virtual: HashMap<String, String> = HashMap::new();
    let mut virtual_to_physical: HashMap<String, String> = HashMap::new();

    for device in physical {
        let key = physical_camera_key_with_name(&device.path, &device.name);
        if let Some(loopback_path) = serial_to_loopback.get(&key) {
            physical_to_virtual.insert(device.path.clone(), loopback_path.clone());
            virtual_to_physical.insert(loopback_path.clone(), device.path.clone());
            continue;
        }

        for loopback in loopbacks {
            if virtual_matches_physical(&device.name, &loopback.name) {
                physical_to_virtual.insert(device.path.clone(), loopback.path.clone());
                virtual_to_physical.insert(loopback.path.clone(), device.path.clone());
                break;
            }
        }
    }

    let mut devices = Vec::new();

    for device in physical {
        let paired_with = physical_to_virtual.get(&device.path).cloned();
        let use_in_apps = false;
        devices.push(physical_device_view(device, paired_with, use_in_apps));
    }

    for loopback in loopbacks {
        let paired_with = virtual_to_physical.get(&loopback.path).cloned();
        let cam_shim = is_cam_shim_loopback(&loopback.name);
        let use_in_apps = serve_running
            && cam_shim
            && paired_with
                .as_ref()
                .is_some_and(|path| physical_needs_shim(physical, path));
        devices.push(virtual_device_view(
            loopback,
            paired_with,
            use_in_apps,
            cam_shim,
        ));
    }

    devices.sort_by(|a, b| a.path.cmp(&b.path));

    let recommended_devices = if serve_running {
        devices
            .iter()
            .filter(|device| device.use_in_apps)
            .filter_map(|device| {
                let for_physical = device.paired_with.clone()?;
                Some(RecommendedDevice {
                    path: device.path.clone(),
                    name: device.name.clone(),
                    for_physical,
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    ScanReport {
        serve_running,
        devices,
        recommended_devices,
    }
}

fn physical_device_view(
    device: &DeviceReport,
    paired_with: Option<String>,
    use_in_apps: bool,
) -> DeviceView {
    let tags = physical_tags(device);
    DeviceView {
        path: device.path.clone(),
        name: device.name.clone(),
        role: DeviceRole::Physical,
        tags,
        paired_with,
        use_in_apps,
        needs_shim: device.needs_shim,
        compatible: device.compatible,
        standardized_name: Some(device.standardized_name.clone()),
        driver: Some(device.driver.clone()),
        bus: Some(device.bus.clone()),
        advertised_fps: device.advertised_fps.clone(),
        issues: device.issues.clone(),
    }
}

fn virtual_device_view(
    loopback: &LoopbackDeviceInfo,
    paired_with: Option<String>,
    use_in_apps: bool,
    cam_shim: bool,
) -> DeviceView {
    let role = if cam_shim {
        DeviceRole::VirtualCamShim
    } else {
        DeviceRole::VirtualOther
    };
    let tags = virtual_tags(cam_shim, use_in_apps);
    DeviceView {
        path: loopback.path.clone(),
        name: loopback.name.clone(),
        role,
        tags,
        paired_with,
        use_in_apps,
        needs_shim: false,
        compatible: false,
        standardized_name: None,
        driver: None,
        bus: None,
        advertised_fps: Vec::new(),
        issues: Vec::new(),
    }
}

fn physical_tags(device: &DeviceReport) -> Vec<String> {
    let mut tags = vec!["physical".into()];
    if device.needs_shim {
        tags.push("needs shim".into());
    } else if device.compatible {
        tags.push("compatible".into());
    }
    tags
}

fn virtual_tags(cam_shim: bool, use_in_apps: bool) -> Vec<String> {
    let mut tags = vec!["virtual".into()];
    if use_in_apps {
        tags.push("use this".into());
    } else if cam_shim {
        tags.push("cam-shim".into());
    } else {
        tags.push("other".into());
    }
    tags
}

fn physical_needs_shim(physical: &[DeviceReport], path: &str) -> bool {
    physical
        .iter()
        .find(|device| device.path == path)
        .is_some_and(|device| device.needs_shim)
}

pub fn virtual_matches_physical(physical_name: &str, virtual_name: &str) -> bool {
    if !is_cam_shim_loopback(virtual_name) {
        return false;
    }

    let expected = standardized_label(physical_name);
    if virtual_name == expected {
        return true;
    }

    let kernel = kernel_card_label(&expected);
    if virtual_name == kernel {
        return true;
    }

    let phys = normalize_card_name(physical_name);
    let virt = normalize_card_name(virtual_name);

    if virt.starts_with(&phys) {
        return true;
    }

    for suffix in [" - Linux Standardized", " - Linux Std"] {
        if let Some(base) = virt.strip_suffix(suffix) {
            if base == phys || phys.starts_with(base) || base.starts_with(&phys) {
                return true;
            }
        }
    }

    if virt.ends_with(" -") {
        let prefix = virt.trim_end_matches(" -").trim();
        if !prefix.is_empty() && (phys.starts_with(prefix) || prefix.starts_with(&phys)) {
            return true;
        }
    }

    false
}

fn normalize_card_name(name: &str) -> String {
    name.trim()
        .strip_prefix("webcam: ")
        .unwrap_or(name.trim())
        .to_string()
}

fn video_index_from_path(path: &str) -> Option<u32> {
    let name = path.rsplit('/').next()?;
    name.strip_prefix("video")?.parse().ok()
}

pub fn role_label(role: DeviceRole) -> &'static str {
    match role {
        DeviceRole::Physical => "physical",
        DeviceRole::VirtualCamShim => "virtual_cam_shim",
        DeviceRole::VirtualOther => "virtual_other",
    }
}

pub fn format_device_line(device: &DeviceView) -> String {
    let tags = device.tags.join(", ");
    format!("{}  {}  [{tags}]", device.path, device.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::ManagedCameraSnapshot;

    fn sample_physical(path: &str, name: &str, needs_shim: bool) -> DeviceReport {
        DeviceReport {
            path: path.into(),
            name: name.into(),
            driver: "uvcvideo".into(),
            bus: "usb".into(),
            standardized_name: standardized_label(name),
            compatible: !needs_shim,
            needs_shim,
            advertised_fps: vec!["25 fps".into()],
            issues: Vec::new(),
        }
    }

    fn sample_loopback(path: &str, name: &str) -> LoopbackDeviceInfo {
        LoopbackDeviceInfo {
            path: path.into(),
            index: video_index_from_path(path).unwrap_or(0),
            name: name.into(),
        }
    }

    #[test]
    fn virtual_name_matching_handles_kernel_truncation() {
        assert!(virtual_matches_physical(
            "Fantech Luminous C30",
            "Fantech Luminous C30 - Linux Std"
        ));
        assert!(virtual_matches_physical(
            "Fantech Luminous C30",
            "webcam: Fantech Luminous C30 -"
        ));
        assert!(!virtual_matches_physical(
            "Fantech Luminous C30",
            "OBS Virtual Camera"
        ));
    }

    #[test]
    fn build_pairs_physical_with_cam_shim_loopback() {
        let physical = vec![sample_physical(
            "/dev/video0",
            "Fantech Luminous C30",
            true,
        )];
        let loopbacks = vec![sample_loopback(
            "/dev/video10",
            "Fantech Luminous C30 - Linux Std",
        )];

        let report = build_device_views(&physical, &loopbacks, true, None);
        assert_eq!(report.devices.len(), 2);

        let physical = report
            .devices
            .iter()
            .find(|device| device.path == "/dev/video0")
            .expect("physical device");
        assert_eq!(physical.paired_with.as_deref(), Some("/dev/video10"));
        assert!(!physical.use_in_apps);

        let virtual_dev = report
            .devices
            .iter()
            .find(|device| device.path == "/dev/video10")
            .expect("virtual device");
        assert_eq!(virtual_dev.paired_with.as_deref(), Some("/dev/video0"));
        assert!(virtual_dev.use_in_apps);
        assert_eq!(virtual_dev.tags, vec!["virtual", "use this"]);

        assert_eq!(report.recommended_devices.len(), 1);
        assert_eq!(report.recommended_devices[0].path, "/dev/video10");
        assert_eq!(report.recommended_devices[0].for_physical, "/dev/video0");
    }

    #[test]
    fn recommended_devices_empty_when_serve_not_running() {
        let physical = vec![sample_physical("/dev/video0", "Cam", true)];
        let loopbacks = vec![sample_loopback("/dev/video10", "Cam - Linux Std")];

        let report = build_device_views(&physical, &loopbacks, false, None);
        assert!(report.recommended_devices.is_empty());
        assert!(!report.devices[1].use_in_apps);
        assert_eq!(report.devices[1].tags, vec!["virtual", "cam-shim"]);
    }

    #[test]
    fn managed_state_pairs_by_serial() {
        let physical = vec![sample_physical("/dev/video0", "Cam", true)];
        let loopbacks = vec![sample_loopback("/dev/video10", "Other Name")];
        let managed = vec![ManagedCameraSnapshot {
            serial: physical_camera_key_with_name("/dev/video0", "Cam"),
            loopback_path: "/dev/video10".into(),
            ..Default::default()
        }];

        let report = build_device_views(&physical, &loopbacks, true, Some(&managed));
        assert_eq!(
            report.devices[0].paired_with.as_deref(),
            Some("/dev/video10")
        );
    }
}
