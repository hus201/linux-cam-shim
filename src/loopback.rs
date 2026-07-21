use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;

use libc::{c_ulong, ioctl, kill, open, O_RDWR, SIGTERM};

use crate::compat::{kernel_card_label, kernel_card_label_bytes};
use crate::error::{CamShimError, Result};

const V4L2LOOPBACK_CTL_ADD: c_ulong = ioctl_iow::<V4l2LoopbackConfig>(b'~', 1);
const V4L2LOOPBACK_CTL_REMOVE: c_ulong = ioctl_iow::<u32>(b'~', 2);

#[repr(C)]
struct V4l2LoopbackConfig {
    output_nr: i32,
    unused: i32,
    card_label: [u8; 32],
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
    max_buffers: i32,
    max_openers: i32,
    debug: i32,
    announce_all_caps: i32,
}

pub struct LoopbackDevice {
    pub path: String,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct LoopbackDeviceInfo {
    pub path: String,
    pub index: u32,
    pub name: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CleanReport {
    pub removed: Vec<String>,
    pub failed: Vec<CleanFailure>,
    pub skipped: Vec<String>,
    pub force_releases: Vec<ForceReleaseEntry>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CleanFailure {
    pub path: String,
    pub reason: String,
    pub holders: Vec<DeviceHolder>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceHolder {
    pub pid: u32,
    pub label: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ForceReleaseEntry {
    pub device_path: String,
    pub releases: Vec<HolderRelease>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HolderRelease {
    pub holder: DeviceHolder,
    pub signal: String,
}

pub fn ensure_module_loaded() -> Result<()> {
    if fs::metadata("/sys/module/v4l2loopback").is_ok() {
        return Ok(());
    }

    let status = Command::new("modprobe")
        .args(["v4l2loopback", "exclusive_caps=1"])
        .status()?;

    if !status.success() {
        return Err(CamShimError::Io(io::Error::other(
            "failed to load v4l2loopback kernel module (is it installed?)",
        )));
    }

    Ok(())
}

pub fn create_device(label: &str, target_fps: u32) -> Result<LoopbackDevice> {
    ensure_module_loaded()?;

    if fs::metadata("/dev/v4l2loopback").is_ok() {
        if let Ok(device) = create_with_ioctl(label, target_fps) {
            return Ok(device);
        }
    }

    if ctl_supports_create() {
        if let Ok(device) = create_with_ctl(label, target_fps) {
            return Ok(device);
        }
    }

    if let Some(path) = find_loopback_by_label(label) {
        apply_target_fps(&path, target_fps);
        return Ok(LoopbackDevice {
            path,
            label: label.to_string(),
        });
    }

    if fs::metadata("/sys/module/v4l2loopback").is_ok() {
        return Err(CamShimError::Io(io::Error::other(
            "could not create a loopback device via /dev/v4l2loopback. \
             Your kernel module is v4l2loopback 0.15+, but apt's v4l2loopback-utils (0.12) \
             cannot create devices — cam-shim uses the control device directly instead.",
        )));
    }

    create_with_modprobe(label, target_fps)
}

fn create_with_ctl(label: &str, target_fps: u32) -> Result<LoopbackDevice> {
    let kernel_label = kernel_card_label(label);
    let output = Command::new("v4l2loopback-ctl")
        .args([
            "create",
            "--exclusive-caps",
            "1",
            "--name",
            &kernel_label,
            "--verbose",
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CamShimError::Io(io::Error::other(format!(
            "v4l2loopback-ctl create failed: {stderr}"
        ))));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout
        .lines()
        .find_map(parse_created_device_path)
        .ok_or_else(|| {
            CamShimError::Io(io::Error::other(format!(
                "could not parse loopback device path from v4l2loopback-ctl output: {stdout}"
            )))
        })?;

    apply_target_fps(&path, target_fps);

    Ok(LoopbackDevice {
        path,
        label: label.to_string(),
    })
}

/// Prefer `/dev/video10+` so physical UVC devices keep low numbers when they
/// reappear after replug. Does not remove or rename other apps' devices.
const PREFERRED_LOOPBACK_NR_START: i32 = 10;
const PREFERRED_LOOPBACK_NR_END: i32 = 63;

fn preferred_loopback_output_nr() -> i32 {
    for nr in PREFERRED_LOOPBACK_NR_START..=PREFERRED_LOOPBACK_NR_END {
        let sysfs = format!("/sys/class/video4linux/video{nr}");
        if fs::metadata(&sysfs).is_err() {
            return nr;
        }
    }
    -1
}

fn create_with_ioctl(label: &str, target_fps: u32) -> Result<LoopbackDevice> {
    let path = CString::new("/dev/v4l2loopback").map_err(|_| {
        CamShimError::Io(io::Error::other("invalid v4l2loopback control device path"))
    })?;

    let fd = unsafe { open(path.as_ptr(), O_RDWR) };
    if fd < 0 {
        return Err(CamShimError::Io(io::Error::other(format!(
            "could not open /dev/v4l2loopback: {}",
            io::Error::last_os_error()
        ))));
    }

    let mut config = V4l2LoopbackConfig {
        output_nr: preferred_loopback_output_nr(),
        unused: -1,
        card_label: kernel_card_label_bytes(label),
        min_width: 0,
        max_width: 8192,
        min_height: 0,
        max_height: 8192,
        max_buffers: 0,
        max_openers: 0,
        debug: 0,
        announce_all_caps: 0,
    };

    let result = unsafe { ioctl(fd, V4L2LOOPBACK_CTL_ADD, &mut config) };
    // If the preferred number was rejected, let the kernel pick any free slot.
    let result = if result < 0 && config.output_nr >= 0 {
        config.output_nr = -1;
        unsafe { ioctl(fd, V4L2LOOPBACK_CTL_ADD, &mut config) }
    } else {
        result
    };
    unsafe { libc::close(fd) };

    if result < 0 {
        return Err(CamShimError::Io(io::Error::last_os_error()));
    }

    // v4l2loopback 0.15+ returns the new device number as the ioctl result.
    let output_nr = if result > 0 {
        result as i32
    } else {
        config.output_nr
    };

    if output_nr < 0 {
        if let Some(path) = find_loopback_by_label(label) {
            apply_target_fps(&path, target_fps);
            return Ok(LoopbackDevice {
                path,
                label: label.to_string(),
            });
        }

        return Err(CamShimError::Io(io::Error::other(
            "loopback device was created but its /dev/video number could not be determined",
        )));
    }

    let device_path = format!("/dev/video{output_nr}");

    if fs::metadata(&device_path).is_err() {
        return Err(CamShimError::Io(io::Error::other(format!(
            "loopback reported /dev/video{output_nr} but the device node is missing"
        ))));
    }

    apply_target_fps(&device_path, target_fps);

    Ok(LoopbackDevice {
        path: device_path,
        label: label.to_string(),
    })
}

fn create_with_modprobe(label: &str, target_fps: u32) -> Result<LoopbackDevice> {
    let kernel_label = kernel_card_label(label);
    let status = Command::new("modprobe")
        .args([
            "v4l2loopback",
            "exclusive_caps=1",
            &format!("card_label={kernel_label}"),
            "devices=1",
        ])
        .status()?;

    if !status.success() {
        return Err(CamShimError::Io(io::Error::other(
            "failed to create loopback device via modprobe",
        )));
    }

    let path = find_latest_loopback_device()?.ok_or_else(|| {
        CamShimError::Io(io::Error::other(
            "loopback module loaded but no virtual /dev/video node found",
        ))
    })?;

    apply_target_fps(&path, target_fps);

    Ok(LoopbackDevice {
        path,
        label: label.to_string(),
    })
}

fn ctl_supports_create() -> bool {
    Command::new("v4l2loopback-ctl")
        .output()
        .ok()
        .map(|output| {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            text.contains("create")
        })
        .unwrap_or(false)
}

fn find_loopback_by_label(label: &str) -> Option<String> {
    let kernel_label = kernel_card_label(label);
    let mut matches = Vec::new();

    for entry in fs::read_dir("/sys/class/video4linux").ok()? {
        let entry = entry.ok()?;
        if !is_loopback_sysfs_node(&entry.path()) {
            continue;
        }

        let name_path = entry.path().join("name");
        let device_name = fs::read_to_string(name_path).ok()?;
        let device_name = device_name.trim();

        if device_name.starts_with(&kernel_label)
            || label.starts_with(device_name)
            || device_name.starts_with(label.strip_prefix("webcam: ").unwrap_or(label))
        {
            let node = format!("/dev/{}", entry.file_name().to_string_lossy());
            matches.push(node);
        }
    }

    matches.pop()
}

fn find_latest_loopback_device() -> Result<Option<String>> {
    let mut best: Option<(usize, String)> = None;

    for entry in fs::read_dir("/sys/class/video4linux")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if !name.starts_with("video") {
            continue;
        }

        if !is_loopback_sysfs_node(&entry.path()) {
            continue;
        }

        if let Some(index) = name.strip_prefix("video").and_then(|n| n.parse().ok()) {
            let node = format!("/dev/{name}");
            if best.as_ref().map(|(i, _)| index > *i).unwrap_or(true) {
                best = Some((index, node));
            }
        }
    }

    Ok(best.map(|(_, path)| path))
}

pub(crate) fn is_loopback_sysfs_node(class_path: &Path) -> bool {
    let target = match fs::read_link(class_path) {
        Ok(target) => target,
        Err(_) => return false,
    };

    target
        .to_string_lossy()
        .contains("devices/virtual/video4linux")
}

fn apply_target_fps(device_path: &str, target_fps: u32) {
    if Command::new("v4l2loopback-ctl")
        .args(["set-fps", &format!("{target_fps}/1"), device_path])
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }

    let Some(index) = device_path.strip_prefix("/dev/video") else {
        return;
    };
    if let Ok(video_nr) = index.parse::<i32>() {
        set_fps_via_sysfs(video_nr, target_fps);
    }
}

fn set_fps_via_sysfs(video_nr: i32, target_fps: u32) {
    let format = format!("@{target_fps}");
    let sysfs = format!("/sys/devices/virtual/video4linux/video{video_nr}/format");
    let _ = fs::write(sysfs, format);
}

fn parse_created_device_path(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("/dev/video") {
        return Some(trimmed.to_string());
    }

    trimmed
        .split_whitespace()
        .find(|part| part.starts_with("/dev/video"))
        .map(str::to_string)
}

const fn ioctl_iow<T>(ty: u8, nr: u8) -> c_ulong {
    const IOC_DIRSHIFT: u32 = 30;
    const IOC_TYPESHIFT: u32 = 8;
    const IOC_NRSHIFT: u32 = 0;
    const IOC_SIZESHIFT: u32 = 16;

    let size = std::mem::size_of::<T>() as u32;
    let dir = 1u32 << IOC_DIRSHIFT;
    let ty = (ty as u32) << IOC_TYPESHIFT;
    let nr = (nr as u32) << IOC_NRSHIFT;
    let sz = size << IOC_SIZESHIFT;
    (dir | ty | nr | sz) as c_ulong
}

pub fn list_loopback_devices() -> Result<Vec<LoopbackDeviceInfo>> {
    let mut devices = Vec::new();

    for entry in fs::read_dir("/sys/class/video4linux")? {
        let entry = entry?;
        if !is_loopback_sysfs_node(&entry.path()) {
            continue;
        }

        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(index) = file_name
            .strip_prefix("video")
            .and_then(|n| n.parse::<usize>().ok())
        else {
            continue;
        };

        let name = fs::read_to_string(entry.path().join("name"))
            .unwrap_or_default()
            .trim()
            .to_string();

        devices.push(LoopbackDeviceInfo {
            path: format!("/dev/{file_name}"),
            index: index as u32,
            name,
        });
    }

    devices.sort_by_key(|device| device.index);
    Ok(devices)
}

pub fn remove_loopback_device(index: u32) -> Result<()> {
    let path = CString::new("/dev/v4l2loopback").map_err(|_| {
        CamShimError::Io(io::Error::other("invalid v4l2loopback control device path"))
    })?;

    let fd = unsafe { open(path.as_ptr(), O_RDWR) };
    if fd < 0 {
        return Err(CamShimError::Io(io::Error::other(format!(
            "could not open /dev/v4l2loopback: {}",
            io::Error::last_os_error()
        ))));
    }

    // v4l2loopback-ctl passes the device number directly, not a pointer.
    let result = unsafe { ioctl(fd, V4L2LOOPBACK_CTL_REMOVE, index as libc::c_ulong) };
    unsafe { libc::close(fd) };

    if result < 0 {
        return Err(CamShimError::Io(io::Error::last_os_error()));
    }

    Ok(())
}

pub fn is_cam_shim_loopback(name: &str) -> bool {
    let name = name.trim();

    if name.contains("Linux Std")
        || name.contains("Linux Standardized")
        || name.contains("Standardized Camera")
    {
        return true;
    }

    // Early cam-shim builds wrote the full display label and the kernel truncated
    // it to 31 bytes, e.g. "webcam: Fantech Luminous C30 -".
    name.ends_with(" -") || name.ends_with(" - ")
}

pub fn clean_loopback_devices(all: bool, force: bool) -> Result<CleanReport> {
    if force {
        stop_cam_shim_processes();
    }

    let mut devices = list_loopback_devices()?;
    // Remove highest-numbered devices first — safer when multiple loopbacks exist.
    devices.sort_by_key(|device| std::cmp::Reverse(device.index));
    let mut report = CleanReport::default();
    let mut holder_map = build_video_device_holder_map();

    for device in devices {
        // Default: only our devices. Never touch OBS/other virtual cameras unless --all.
        if !all && !is_cam_shim_loopback(&device.name) {
            report
                .skipped
                .push(format!("{} ({})", device.path, device.name));
            continue;
        }

        if force {
            let releases = release_device_holders_with_map(&device.path, &mut holder_map);
            if !releases.is_empty() {
                report.force_releases.push(ForceReleaseEntry {
                    device_path: device.path.clone(),
                    releases,
                });
            }
        }

        let mut holders = holders_for_device(&holder_map, &device.path);
        let mut remove_result = remove_loopback_device(device.index);
        if force && matches!(&remove_result, Err(err) if is_busy_error(err)) {
            let more = release_device_holders_with_map(&device.path, &mut holder_map);
            if !more.is_empty() {
                if let Some(entry) = report
                    .force_releases
                    .iter_mut()
                    .find(|entry| entry.device_path == device.path)
                {
                    entry.releases.extend(more);
                } else {
                    report.force_releases.push(ForceReleaseEntry {
                        device_path: device.path.clone(),
                        releases: more,
                    });
                }
            }
            thread::sleep(Duration::from_millis(250));
            holders = holders_for_device(&holder_map, &device.path);
            remove_result = remove_loopback_device(device.index);
        }

        match remove_result {
            Ok(()) if loopback_sysfs_exists(device.index) => report.failed.push(CleanFailure {
                path: device.path.clone(),
                reason: "device still registered after remove".into(),
                holders: holders.clone(),
            }),
            Ok(()) => report
                .removed
                .push(format!("{} ({})", device.path, device.name)),
            Err(err) if is_busy_error(&err) => report.failed.push(CleanFailure {
                path: device.path.clone(),
                reason: busy_reason(&device.path, &holders),
                holders,
            }),
            Err(err) if is_gone_error(&err) && !loopback_sysfs_exists(device.index) => {
                report
                    .removed
                    .push(format!("{} ({}) [already gone]", device.path, device.name));
            }
            Err(err) => report.failed.push(CleanFailure {
                path: device.path.clone(),
                reason: err.to_string(),
                holders,
            }),
        }
    }

    Ok(report)
}

pub fn unload_loopback_module() -> Result<()> {
    let status = Command::new("modprobe")
        .args(["-r", "v4l2loopback"])
        .status()?;
    if !status.success() {
        return Err(CamShimError::Io(io::Error::other(
            "failed to unload v4l2loopback (is a virtual camera still open?)",
        )));
    }
    Ok(())
}

fn loopback_sysfs_exists(index: u32) -> bool {
    fs::metadata(format!("/sys/devices/virtual/video4linux/video{index}")).is_ok()
}

pub fn stop_cam_shim_processes() {
    let own_pid = std::process::id();
    let proc = match fs::read_dir("/proc") {
        Ok(proc) => proc,
        Err(_) => return,
    };

    for entry in proc.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == own_pid {
            continue;
        }

        let cmdline_path = entry.path().join("cmdline");
        let Ok(raw) = fs::read(cmdline_path) else {
            continue;
        };
        if !is_cam_shim_daemon_cmdline(&raw) {
            continue;
        }

        unsafe {
            kill(pid as i32, SIGTERM);
        }
    }

    thread::sleep(Duration::from_millis(250));
}

fn is_cam_shim_daemon_cmdline(raw: &[u8]) -> bool {
    let parts: Vec<&str> = raw
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::str::from_utf8(s).ok())
        .collect();
    if parts.is_empty() {
        return false;
    }

    let exe = parts[0];
    if !exe.ends_with("cam-shim") && !exe.contains("/cam-shim") {
        return false;
    }

    matches!(parts.get(1).copied(), Some("serve" | "fix" | "relay"))
}

pub fn find_device_holders(device_path: &str) -> Vec<u32> {
    list_device_holders(device_path)
        .into_iter()
        .map(|holder| holder.pid)
        .collect()
}

/// Walk `/proc` once and map each video device path to the processes holding it open.
pub fn build_video_device_holder_map() -> HashMap<String, Vec<DeviceHolder>> {
    let mut map: HashMap<String, Vec<DeviceHolder>> = HashMap::new();
    let proc = match fs::read_dir("/proc") {
        Ok(proc) => proc,
        Err(_) => return map,
    };

    for entry in proc.flatten() {
        let file_name = entry.file_name();
        let Ok(pid) = file_name.to_string_lossy().parse::<u32>() else {
            continue;
        };

        let fd_dir = entry.path().join("fd");
        let Ok(fds) = fs::read_dir(fd_dir) else {
            continue;
        };

        let label = process_label(pid);
        let mut seen_paths = HashSet::new();

        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };

            let Some(path_key) = video_device_path_key(&target) else {
                continue;
            };

            if !seen_paths.insert(path_key.clone()) {
                continue;
            }

            map.entry(path_key).or_default().push(DeviceHolder {
                pid,
                label: label.clone(),
            });
        }
    }

    for holders in map.values_mut() {
        holders.sort_by_key(|holder| holder.pid);
        holders.dedup_by_key(|holder| holder.pid);
    }

    map
}

/// Device paths held by a running cam-shim worker (avoid re-probing them from the CLI).
pub fn cam_shim_held_device_paths(map: &HashMap<String, Vec<DeviceHolder>>) -> HashSet<String> {
    map.iter()
        .filter(|(_, holders)| {
            holders
                .iter()
                .any(|holder| holder.label.contains("cam-shim"))
        })
        .map(|(path, _)| path.clone())
        .collect()
}

pub fn list_device_holders(device_path: &str) -> Vec<DeviceHolder> {
    let map = build_video_device_holder_map();
    holders_for_device(&map, device_path)
}

pub fn holders_for_device(
    map: &HashMap<String, Vec<DeviceHolder>>,
    device_path: &str,
) -> Vec<DeviceHolder> {
    if let Some(holders) = map.get(device_path) {
        return holders.clone();
    }

    let want = Path::new(device_path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let Some(want) = want else {
        return Vec::new();
    };

    for (path, holders) in map {
        if Path::new(path)
            .file_name()
            .is_some_and(|name| name.to_string_lossy() == want)
        {
            return holders.clone();
        }
    }

    Vec::new()
}

pub fn format_holder_list(holders: &[DeviceHolder]) -> String {
    if holders.is_empty() {
        return "none".into();
    }

    holders
        .iter()
        .map(|holder| format!("{} ({})", holder.label, holder.pid))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg_attr(not(test), allow(dead_code))]
fn fd_targets_device(link: &Path, device_path: &str) -> bool {
    let Some(key) = video_device_path_key(link) else {
        return false;
    };
    if key == device_path {
        return true;
    }

    if fs::canonicalize(device_path)
        .ok()
        .is_some_and(|canonical| canonical.to_string_lossy() == key)
    {
        return true;
    }

    let device_name = Path::new(device_path).file_name();
    let link_name = Path::new(&key).file_name();
    device_name.is_some() && device_name == link_name
}

fn video_device_path_key(link: &Path) -> Option<String> {
    let link = link.to_string_lossy();
    let link = link.strip_suffix(" (deleted)").unwrap_or(&link);

    if link.starts_with("/dev/video") {
        Some(link.to_string())
    } else {
        None
    }
}

/// Number of processes reading from a loopback device (capture side).
pub fn loopback_consumer_count(device_path: &str) -> Option<u32> {
    let name = Path::new(device_path).file_name()?.to_string_lossy();
    let sysfs = Path::new("/sys/class/video4linux")
        .join(&*name)
        .join("active_readers");
    if !sysfs.is_file() {
        return None;
    }
    fs::read_to_string(sysfs).ok()?.trim().parse().ok()
}

/// True when an external app is reading from the virtual camera.
pub fn loopback_has_consumers(device_path: &str) -> bool {
    if let Some(readers) = loopback_consumer_count(device_path) {
        return readers > 0;
    }

    let own_pid = std::process::id();
    find_device_holders(device_path)
        .into_iter()
        .any(|pid| pid != own_pid)
}

pub fn release_device_holders(device_path: &str) -> Vec<HolderRelease> {
    let mut holder_map = build_video_device_holder_map();
    release_device_holders_with_map(device_path, &mut holder_map)
}

fn release_device_holders_with_map(
    device_path: &str,
    holder_map: &mut HashMap<String, Vec<DeviceHolder>>,
) -> Vec<HolderRelease> {
    let own_pid = std::process::id();
    let mut releases = Vec::new();

    for pass in 0..2 {
        let signal = if pass == 0 { SIGTERM } else { libc::SIGKILL };
        let signal_name = if pass == 0 { "SIGTERM" } else { "SIGKILL" };
        let mut killed = false;

        for holder in holders_for_device(holder_map, device_path) {
            if holder.pid == own_pid {
                continue;
            }
            unsafe {
                kill(holder.pid as i32, signal);
            }
            releases.push(HolderRelease {
                holder,
                signal: signal_name.into(),
            });
            killed = true;
        }

        if killed {
            thread::sleep(Duration::from_millis(250));
            *holder_map = build_video_device_holder_map();
        }

        if holders_for_device(holder_map, device_path)
            .iter()
            .all(|holder| holder.pid == own_pid)
        {
            break;
        }
    }

    if Path::new(device_path).exists() && Command::new("fuser").arg("--version").output().is_ok() {
        for holder in holders_for_device(holder_map, device_path) {
            if holder.pid == own_pid {
                continue;
            }
            let _ = Command::new("fuser")
                .args(["-k", "-TERM", device_path])
                .status();
            releases.push(HolderRelease {
                holder,
                signal: "fuser SIGTERM".into(),
            });
            break;
        }
        thread::sleep(Duration::from_millis(250));
        *holder_map = build_video_device_holder_map();
    }

    releases
}

fn is_busy_error(err: &CamShimError) -> bool {
    matches!(
        err,
        CamShimError::Io(io_err) if io_err.raw_os_error() == Some(libc::EBUSY)
    )
}

fn is_gone_error(err: &CamShimError) -> bool {
    matches!(
        err,
        CamShimError::Io(io_err) if io_err.raw_os_error() == Some(libc::ENODEV)
    )
}

fn busy_reason(device_path: &str, holders: &[DeviceHolder]) -> String {
    if holders.is_empty() {
        return format!(
            "{device_path} is in use — close apps using it, or run \
             `cam-shim clean --force --reload` (module reload clears stuck loopbacks)"
        );
    }

    format!(
        "{device_path} is held open by {} — close those apps or run `cam-shim clean --force`",
        format_holder_list(holders)
    )
}

fn process_label(pid: u32) -> String {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|name| name.trim().to_string())
        .unwrap_or_default();

    let exe_base = fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|raw| {
            raw.split(|b| *b == 0)
                .find(|part| !part.is_empty())
                .and_then(|part| std::str::from_utf8(part).ok())
                .map(|exe| {
                    Path::new(exe)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| exe.to_string())
                })
        })
        .unwrap_or_default();

    pick_process_label(&comm, &exe_base)
}

fn pick_process_label(comm: &str, exe_base: &str) -> String {
    if exe_base.is_empty() {
        return if comm.is_empty() {
            "unknown".into()
        } else {
            comm.to_string()
        };
    }

    if comm.is_empty() {
        return exe_base.to_string();
    }

    // /proc/comm is limited to 15 bytes — prefer the executable basename when comm is truncated.
    if exe_base.starts_with(comm) || comm.len() >= 15 {
        return exe_base.to_string();
    }

    if comm == "python3" || comm == "python" || comm == "node" || comm == "sh" {
        return exe_base.to_string();
    }

    comm.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::standardized_label;
    use std::path::PathBuf;

    #[test]
    fn prefers_exe_basename_when_comm_truncated() {
        assert_eq!(
            pick_process_label("google-chrome", "google-chrome-stable"),
            "google-chrome-stable"
        );
        assert_eq!(pick_process_label("Discord", "Discord"), "Discord");
        assert_eq!(pick_process_label("python3", "guvcview"), "guvcview");
    }

    #[test]
    fn formats_holder_list_for_display() {
        let holders = vec![
            DeviceHolder {
                pid: 1234,
                label: "Discord".into(),
            },
            DeviceHolder {
                pid: 5678,
                label: "guvcview".into(),
            },
        ];
        assert_eq!(
            format_holder_list(&holders),
            "Discord (1234), guvcview (5678)"
        );
    }

    #[test]
    fn ctl_supports_create_false_for_apt_utils() {
        // When v4l2loopback-ctl lacks `create`, we rely on ioctl instead.
        let _ = ctl_supports_create();
    }

    #[test]
    fn detects_virtual_loopback_sysfs_path() {
        let class_dir = PathBuf::from("/sys/class/video4linux");
        let Ok(entries) = fs::read_dir(&class_dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(target) = fs::read_link(&path) else {
                continue;
            };

            let expected = target
                .to_string_lossy()
                .contains("devices/virtual/video4linux");
            assert_eq!(
                is_loopback_sysfs_node(&path),
                expected,
                "loopback detection mismatch for {}",
                path.display()
            );
        }
    }

    #[test]
    fn kernel_label_keeps_suffix() {
        let label = standardized_label("webcam: Fantech Luminous C30");
        let kernel = kernel_card_label(&label);
        assert!(kernel.contains("Linux Std"));
        assert!(kernel.len() <= 31);
    }

    #[test]
    fn detects_truncated_orphan_names() {
        assert!(is_cam_shim_loopback("webcam: Fantech Luminous C30 -"));
        assert!(is_cam_shim_loopback("Linux Standardized Camera"));
        assert!(!is_cam_shim_loopback("OBS Virtual Camera"));
        assert!(!is_cam_shim_loopback("Dummy video device (0x0000)"));
    }

    #[test]
    fn prefers_high_loopback_numbers() {
        let nr = preferred_loopback_output_nr();
        assert!(nr == -1 || nr >= PREFERRED_LOOPBACK_NR_START);
    }

    #[test]
    fn stop_list_excludes_clean_and_includes_serve() {
        assert!(is_cam_shim_daemon_cmdline(
            b"/usr/bin/cam-shim\0serve\0".as_slice()
        ));
        assert!(!is_cam_shim_daemon_cmdline(
            b"/usr/bin/cam-shim\0clean\0--force\0".as_slice()
        ));
    }

    #[test]
    fn matches_deleted_device_fds() {
        assert!(fd_targets_device(
            Path::new("/dev/video2 (deleted)"),
            "/dev/video2"
        ));
        assert!(!fd_targets_device(
            Path::new("/dev/video3 (deleted)"),
            "/dev/video2"
        ));
    }
}
