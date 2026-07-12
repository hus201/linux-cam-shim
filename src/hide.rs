use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use v4l::capability::Flags as CapFlags;
use v4l::prelude::*;

use crate::error::{CamShimError, Result};
use crate::loopback::is_loopback_sysfs_node;

const HIDDEN_DIR: &str = "/dev/cam-shim-hidden";
const DEFAULT_UDEV_RULE: &str = "/etc/udev/rules.d/99-cam-shim.rules";

#[derive(Debug, Clone)]
pub struct CameraIdentity {
    pub id_serial: String,
    pub id_path: String,
    pub nodes: Vec<String>,
}

/// USB serial from udev for a `/dev/video*` node (used to dedupe multi-node webcams).
pub fn device_id_serial(device_path: &str) -> Option<String> {
    udev_properties(device_path).ok().and_then(|props| {
        props
            .get("ID_SERIAL")
            .or_else(|| props.get("ID_SERIAL_SHORT"))
            .cloned()
    })
}

pub fn camera_identity(source_device: &str) -> Result<CameraIdentity> {
    match camera_identity_from_udev(source_device) {
        Ok(identity) => Ok(identity),
        Err(first_err) => {
            // Freshly restored/renamed nodes can briefly lack udev properties.
            let _ = Command::new("udevadm")
                .args(["settle", "--timeout=2"])
                .status();
            camera_identity_from_udev(source_device).map_err(|_| first_err)
        }
    }
}

fn camera_identity_from_udev(source_device: &str) -> Result<CameraIdentity> {
    let props = udev_properties(source_device)?;
    let id_serial = props
        .get("ID_SERIAL")
        .or_else(|| props.get("ID_SERIAL_SHORT"))
        .cloned()
        .ok_or_else(|| {
            CamShimError::Io(std::io::Error::other(format!(
                "could not read ID_SERIAL for {source_device} (is udev available?)"
            )))
        })?;

    let id_path = props.get("ID_PATH").cloned().unwrap_or_default();
    let nodes = related_video_nodes(&id_serial)?;

    Ok(CameraIdentity {
        id_serial,
        id_path,
        nodes,
    })
}

pub fn install_hide_rule(identity: &CameraIdentity, rule_path: &Path) -> Result<()> {
    write_udev_hide_rule(&identity.id_serial, rule_path)?;
    reload_udev_rules();
    Ok(())
}

pub fn write_hide_rule_for(identity: &CameraIdentity) -> Result<PathBuf> {
    let rule_path = udev_rule_path_for_serial(&identity.id_serial);
    write_udev_hide_rule(&identity.id_serial, &rule_path)?;
    Ok(rule_path)
}

pub fn activate_hide_rules() {
    reload_udev_rules();
}

pub fn visible_capture_path(identity: &CameraIdentity, fallback: &str) -> String {
    for node in &identity.nodes {
        if let Some(path) = resolve_node_path(node) {
            return path;
        }
    }

    resolve_device_path(fallback)
}

fn resolve_node_path(node: &str) -> Option<String> {
    let path = Path::new(node);
    if path.exists() {
        return Some(path.display().to_string());
    }

    if let Some(name) = path.file_name() {
        let hidden = Path::new(HIDDEN_DIR).join(name);
        if hidden.exists() {
            return Some(hidden.display().to_string());
        }
    }

    None
}

pub fn install_hide_rule_for(identity: &CameraIdentity) -> Result<PathBuf> {
    let rule_path = write_hide_rule_for(identity)?;
    activate_hide_rules();
    Ok(rule_path)
}

pub fn udev_rule_path_for_serial(id_serial: &str) -> PathBuf {
    let safe: String = id_serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    PathBuf::from(format!("/etc/udev/rules.d/99-cam-shim-{safe}.rules"))
}

pub fn remove_all_hide_rules() -> Result<()> {
    let rules_dir = Path::new("/etc/udev/rules.d");
    if !rules_dir.is_dir() {
        return Ok(());
    }

    for entry in fs::read_dir(rules_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("99-cam-shim") && name.ends_with(".rules") {
            fs::remove_file(entry.path())?;
        }
    }

    reload_udev_rules_only();
    Ok(())
}

#[derive(Debug, Default)]
pub struct RestoreReport {
    pub restored: Vec<String>,
    pub ghosts_removed: Vec<String>,
    pub stale_hidden_removed: Vec<String>,
}

pub fn repair_video_devices() -> Result<RestoreReport> {
    let report = RestoreReport {
        restored: restore_hidden_cameras()?,
        ghosts_removed: remove_ghost_device_nodes()?,
        stale_hidden_removed: drop_stale_hidden_nodes()?,
    };
    if !report.restored.is_empty()
        || !report.ghosts_removed.is_empty()
        || !report.stale_hidden_removed.is_empty()
    {
        trigger_video_devices();
    }
    Ok(report)
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

fn is_loopback_video_name(name: &str) -> bool {
    is_loopback_sysfs_node(&Path::new("/sys/class/video4linux").join(name))
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

fn drop_stale_hidden_nodes() -> Result<Vec<String>> {
    let hidden_dir = Path::new(HIDDEN_DIR);
    if !hidden_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut removed = Vec::new();

    for entry in fs::read_dir(hidden_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("video") {
            continue;
        }

        // A loopback occupying videoN means the original physical node is gone from
        // that number (often re-enumerated higher). Drop the stale hidden node.
        if sysfs_video_exists(&name) && !is_loopback_video_name(&name) {
            continue;
        }

        fs::remove_file(entry.path())?;
        removed.push(entry.path().display().to_string());
    }

    if fs::read_dir(hidden_dir)?.next().is_none() {
        let _ = fs::remove_dir(hidden_dir);
    }

    Ok(removed)
}

fn trigger_video_devices() {
    let _ = Command::new("udevadm")
        .args(["trigger", "--subsystem-match=video4linux", "--action=add"])
        .status();
    // Do not block indefinitely — settle alone can take minutes on busy systems.
    let _ = Command::new("udevadm")
        .args(["settle", "--timeout=5"])
        .status();
}

pub fn camera_serial_present(id_serial: &str) -> bool {
    related_video_nodes(id_serial).is_ok_and(|nodes| !nodes.is_empty())
}

pub fn hide_camera_now(identity: &CameraIdentity) -> Result<Vec<String>> {
    fs::create_dir_all(HIDDEN_DIR)?;

    let mut hidden = Vec::new();
    for node in &identity.nodes {
        if !is_capture_device(node) {
            tracing::debug!(path = %node, "skipping non-capture video node");
            continue;
        }
        if hide_node(node)? {
            hidden.push(node.clone());
        }
    }

    Ok(hidden)
}

/// Return the path where a V4L2 node can be opened — normal `/dev` or hidden stash.
pub fn resolve_device_path(path: &str) -> String {
    let path = Path::new(path);
    if path.exists() {
        return path.display().to_string();
    }

    if let Some(name) = path.file_name() {
        let hidden = Path::new(HIDDEN_DIR).join(name);
        if hidden.exists() {
            return hidden.display().to_string();
        }
    }

    path.display().to_string()
}

pub fn hidden_camera_count() -> Result<usize> {
    Ok(hidden_video_names()?.len())
}

/// Basenames (`video0`, `video1`, …) currently under `/dev/cam-shim-hidden/`.
pub fn hidden_video_names() -> Result<Vec<String>> {
    let hidden_dir = Path::new(HIDDEN_DIR);
    if !hidden_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut names = Vec::new();
    for entry in fs::read_dir(hidden_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("video") && name[5..].chars().all(|c| c.is_ascii_digit()) {
            names.push(name.into_owned());
        }
    }
    names.sort();
    Ok(names)
}

pub fn restore_hidden_cameras() -> Result<Vec<String>> {
    let hidden_dir = Path::new(HIDDEN_DIR);
    if !hidden_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut restored = Vec::new();

    for entry in fs::read_dir(hidden_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("video") {
            continue;
        }

        let dest = Path::new("/dev").join(&*name);

        // Loopback can reuse a videoN name after the physical camera re-enumerated.
        // Only restore when sysfs still points at a real (non-loopback) device.
        if !sysfs_video_exists(&name) || is_loopback_video_name(&name) {
            tracing::warn!(
                name = %name,
                "skipping hidden node restore — no matching physical sysfs device"
            );
            continue;
        }

        if dest.exists() {
            if is_ghost_device_node(&dest) {
                fs::remove_file(&dest)?;
            } else {
                tracing::warn!(
                    path = %dest.display(),
                    "destination already exists — removing duplicate hidden node"
                );
                fs::remove_file(entry.path())?;
                continue;
            }
        }

        fs::rename(entry.path(), &dest)?;
        restored.push(dest.display().to_string());
    }

    if fs::read_dir(hidden_dir)?.next().is_none() {
        let _ = fs::remove_dir(hidden_dir);
    }

    Ok(restored)
}

pub fn remove_hide_rule(rule_path: &Path) -> Result<()> {
    if rule_path.exists() {
        fs::remove_file(rule_path)?;
    }
    reload_udev_rules();
    Ok(())
}

pub fn default_udev_rule_path() -> PathBuf {
    PathBuf::from(DEFAULT_UDEV_RULE)
}

pub fn teardown_hide(rule_path: &Path) -> Result<()> {
    remove_hide_rule(rule_path)?;
    let _ = repair_video_devices()?;
    Ok(())
}

fn hide_node(device_path: &str) -> Result<bool> {
    let path = Path::new(device_path);
    if !path.exists() {
        return Ok(false);
    }

    if !is_capture_device(device_path) {
        return Ok(false);
    }

    let file_name = path
        .file_name()
        .ok_or_else(|| CamShimError::Io(std::io::Error::other("invalid device path")))?;
    let hidden_path = Path::new(HIDDEN_DIR).join(file_name);

    fs::rename(path, &hidden_path)?;
    Ok(true)
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
        let resolved = resolve_node_path(&node).unwrap_or_else(|| node.clone());
        let props = match udev_properties(&resolved) {
            Ok(props) => props,
            Err(_) => continue,
        };

        let serial = props
            .get("ID_SERIAL")
            .or_else(|| props.get("ID_SERIAL_SHORT"));

        if serial == Some(&id_serial.to_string()) && is_capture_device(&resolved) {
            nodes.push(resolved);
        }
    }

    nodes.sort();
    Ok(nodes)
}

fn udev_properties(device_path: &str) -> Result<HashMap<String, String>> {
    let output = Command::new("udevadm")
        .args(["info", "-q", "property", "-n", device_path])
        .output()?;

    if !output.status.success() {
        return Err(CamShimError::Io(std::io::Error::other(format!(
            "udevadm failed for {device_path}: {}",
            String::from_utf8_lossy(&output.stderr)
        ))));
    }

    let mut props = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some((key, value)) = line.split_once('=') {
            props.insert(key.to_string(), value.to_string());
        }
    }

    Ok(props)
}

fn write_udev_hide_rule(id_serial: &str, rule_path: &Path) -> Result<()> {
    let rule = format!(
        r#"# Generated by cam-shim — hides the physical camera from app enumeration.
SUBSYSTEM!="video4linux", GOTO="cam_shim_end"
ENV{{ID_SERIAL}}!="{id_serial}", GOTO="cam_shim_end"
ATTR{{name}}=="*Linux Standardized*", GOTO="cam_shim_end"
ATTR{{name}}=="*Linux Std*", GOTO="cam_shim_end"

ACTION=="add", RUN+="/bin/sh -c 'D=$env{{DEVNAME}}; [ -e \"$D\" ] || exit 0; /usr/bin/v4l2-ctl -d \"$D\" --all 2>/dev/null | grep -q \"Video Capture\" || exit 0; /bin/mkdir -p {HIDDEN_DIR}; /bin/mv -f \"$D\" {HIDDEN_DIR}/'"

LABEL="cam_shim_end"
"#
    );

    fs::write(rule_path, rule)?;
    Ok(())
}

fn reload_udev_rules_only() {
    let _ = Command::new("udevadm")
        .args(["control", "--reload-rules"])
        .status();
}

fn reload_udev_rules() {
    reload_udev_rules_only();
    let _ = Command::new("udevadm").args(["trigger"]).status();
}

fn is_capture_device(device_path: &str) -> bool {
    Device::with_path(device_path)
        .and_then(|dev| dev.query_caps())
        .map(|caps| caps.capabilities.contains(CapFlags::VIDEO_CAPTURE))
        .unwrap_or(false)
}
