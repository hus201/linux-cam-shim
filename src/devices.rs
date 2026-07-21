use std::fs;
use std::path::Path;

use v4l::capability::Flags as CapFlags;
use v4l::prelude::*;

use crate::error::Result;
use crate::loopback::is_loopback_sysfs_node;

#[derive(Debug, Clone)]
pub struct CameraIdentity {
    pub id_serial: String,
    pub nodes: Vec<String>,
}

/// Stable key for one physical camera (USB vendor/product/serial when available).
pub fn physical_camera_key(device_path: &str) -> String {
    physical_camera_key_with_name(device_path, "")
}

pub fn physical_camera_key_with_name(device_path: &str, card_name: &str) -> String {
    if let Some(key) = usb_device_sysfs_key(device_path) {
        return key;
    }
    if let Some(serial) = sysfs_usb_serial(device_path) {
        return format!("serial:{serial}");
    }
    if !card_name.is_empty() {
        return format!("name:{card_name}");
    }
    device_path.to_string()
}

pub fn device_id_serial(device_path: &str) -> Option<String> {
    sysfs_usb_serial(device_path)
}

pub fn camera_identity(source_device: &str) -> Result<CameraIdentity> {
    let id_serial = physical_camera_key(source_device);
    let nodes = related_video_nodes(&id_serial)?;
    Ok(CameraIdentity { id_serial, nodes })
}

/// Lowest-numbered `/dev/video*` capture node for one physical camera.
pub fn best_capture_path(source_device: &str) -> Result<String> {
    let identity = camera_identity(source_device)?;
    identity.nodes.into_iter().next().ok_or_else(|| {
        crate::error::CamShimError::DeviceNotFound(format!(
            "no capture node found for {source_device}"
        ))
    })
}

pub fn camera_serial_present(id_serial: &str) -> bool {
    related_video_nodes(id_serial).is_ok_and(|nodes| !nodes.is_empty())
}

#[derive(Debug, Default)]
pub struct RepairReport {
    pub ghosts_removed: Vec<String>,
}

pub fn repair_video_devices() -> Result<RepairReport> {
    let ghosts_removed = remove_ghost_device_nodes()?;
    Ok(RepairReport { ghosts_removed })
}

pub fn ghost_device_count() -> Result<usize> {
    Ok(count_ghost_device_nodes()?.len())
}

fn sysfs_video_exists(name: &str) -> bool {
    Path::new("/sys/class/video4linux").join(name).exists()
}

fn is_ghost_device_node(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };

    if !name.starts_with("video") || !name[5..].chars().all(|c| c.is_ascii_digit()) {
        return false;
    }

    !sysfs_video_exists(name)
}

fn count_ghost_device_nodes() -> Result<Vec<String>> {
    let mut ghosts = Vec::new();

    for entry in fs::read_dir("/dev")? {
        let entry = entry?;
        let path = entry.path();
        if is_ghost_device_node(&path) {
            ghosts.push(path.display().to_string());
        }
    }

    ghosts.sort();
    Ok(ghosts)
}

fn remove_ghost_device_nodes() -> Result<Vec<String>> {
    let mut removed = Vec::new();

    for path in count_ghost_device_nodes()? {
        fs::remove_file(&path)?;
        removed.push(path);
    }

    Ok(removed)
}

fn related_video_nodes(id_serial: &str) -> Result<Vec<String>> {
    let mut nodes = Vec::new();

    for entry in fs::read_dir("/sys/class/video4linux")? {
        let entry = entry?;
        if is_loopback_sysfs_node(&entry.path()) {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("video") {
            continue;
        }

        let node = format!("/dev/{name}");
        if !Path::new(&node).exists() {
            continue;
        }

        if physical_camera_key(&node) == id_serial && is_capture_device(&node) {
            nodes.push(node);
        }
    }

    nodes.sort();
    Ok(nodes)
}

fn sysfs_usb_serial(device_path: &str) -> Option<String> {
    usb_device_sysfs_key(device_path).and_then(|key| {
        key.strip_prefix("usb:")
            .and_then(|rest| rest.rsplit(':').next())
            .filter(|serial| *serial != "no-serial")
            .map(str::to_string)
    })
}

/// Walk from `video4linux/videoN/device` up to the USB device directory.
fn usb_device_sysfs_key(device_path: &str) -> Option<String> {
    let video_name = Path::new(device_path).file_name()?.to_string_lossy();
    let device_symlink = Path::new("/sys/class/video4linux")
        .join(&*video_name)
        .join("device");
    let mut path = fs::canonicalize(device_symlink).ok()?;

    loop {
        if path.join("idVendor").is_file() {
            let vendor = fs::read_to_string(path.join("idVendor"))
                .ok()?
                .trim()
                .to_string();
            let product = fs::read_to_string(path.join("idProduct"))
                .ok()?
                .trim()
                .to_string();
            let serial = fs::read_to_string(path.join("serial"))
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "no-serial".into());
            return Some(format!("usb:{vendor}:{product}:{serial}"));
        }

        let parent = path.parent()?.to_path_buf();
        if parent == path {
            break;
        }
        path = parent;
    }

    None
}

fn is_metadata_node(device_path: &str) -> bool {
    let Some(video_name) = Path::new(device_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };

    let name_path = format!("/sys/class/video4linux/{video_name}/name");
    fs::read_to_string(name_path)
        .ok()
        .is_some_and(|name| name.to_ascii_lowercase().contains("metadata"))
}

fn is_capture_device(device_path: &str) -> bool {
    if is_metadata_node(device_path) {
        return false;
    }

    Device::with_path(device_path)
        .and_then(|dev| dev.query_caps())
        .map(|caps| caps.capabilities.contains(CapFlags::VIDEO_CAPTURE))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_camera_key_falls_back_to_path() {
        assert_eq!(
            physical_camera_key("/dev/video99"),
            "/dev/video99"
        );
    }
}
