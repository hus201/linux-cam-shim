//! Kernel uevent hotplug via netlink (`NETLINK_KOBJECT_UEVENT`).
//!
//! Reacts to physical `video4linux` add/remove/change events without libudev.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::Result;

const NETLINK_KOBJECT_UEVENT: i32 = 15;
const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(200);
const UEVENT_BUFFER_SIZE: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Uevent {
    action: String,
    devpath: String,
    subsystem: String,
    devname: Option<String>,
    name: Option<String>,
}

pub fn spawn_hotplug_monitor(shutdown: Arc<AtomicBool>, notify: Sender<()>) -> Result<JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("cam-shim-hotplug".into())
        .spawn(move || run_monitor(shutdown, notify))
        .map_err(|err| {
            crate::error::CamShimError::Io(io::Error::other(format!(
                "failed to spawn netlink hotplug thread: {err}"
            )))
        })?;

    Ok(handle)
}

fn run_monitor(shutdown: Arc<AtomicBool>, notify: Sender<()>) {
    let socket = match open_uevent_socket() {
        Ok(socket) => socket,
        Err(err) => {
            tracing::error!(%err, "failed to open netlink uevent socket");
            return;
        }
    };

    tracing::info!("netlink hotplug monitor listening for video4linux uevents");

    let fd = socket.as_raw_fd();
    let mut last_notify = Instant::now()
        .checked_sub(HOTPLUG_DEBOUNCE)
        .unwrap_or_else(Instant::now);

    let mut buffer = vec![0u8; UEVENT_BUFFER_SIZE];

    while !shutdown.load(Ordering::SeqCst) {
        if !wait_readable(fd, Duration::from_secs(1)) {
            continue;
        }

        loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }

            let received = match recv_datagram(fd, &mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    tracing::warn!(%err, "netlink uevent read failed");
                    break;
                }
            };

            let Some(event) = parse_uevent(&buffer[..received]) else {
                continue;
            };

            if !should_react_to_uevent(&event) {
                tracing::trace!(
                    action = %event.action,
                    devpath = %event.devpath,
                    subsystem = %event.subsystem,
                    ?event.name,
                    "ignored uevent"
                );
                continue;
            }

            let devnode = event
                .devname
                .clone()
                .unwrap_or_else(|| event.devpath.clone());

            if last_notify.elapsed() < HOTPLUG_DEBOUNCE {
                tracing::trace!(devnode, "debounced netlink hotplug event");
                continue;
            }

            last_notify = Instant::now();
            tracing::debug!(
                action = %event.action,
                devnode,
                "netlink hotplug event"
            );

            if notify.send(()).is_err() {
                return;
            }
        }
    }
}

fn open_uevent_socket() -> io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            NETLINK_KOBJECT_UEVENT,
        );
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let rcvbuf: libc::c_int = UEVENT_BUFFER_SIZE as libc::c_int;
        if libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &rcvbuf as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        ) < 0
        {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }

        let mut addr: libc::sockaddr_nl = std::mem::zeroed();
        addr.nl_family = libc::AF_NETLINK as u16;
        addr.nl_pid = 0;
        addr.nl_groups = 1;

        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        ) < 0
        {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }

        Ok(OwnedFd::from_raw_fd(fd))
    }
}

fn recv_datagram(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    let received = unsafe {
        libc::recv(
            fd,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
            libc::MSG_DONTWAIT,
        )
    };

    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(received as usize)
}

fn wait_readable(fd: RawFd, timeout: Duration) -> bool {
    use libc::{pollfd, POLLIN, POLLPRI};

    let mut pfd = pollfd {
        fd,
        events: POLLIN | POLLPRI,
        revents: 0,
    };

    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

fn parse_uevent(buf: &[u8]) -> Option<Uevent> {
    if buf.is_empty() {
        return None;
    }

    let mut props = HashMap::new();

    for chunk in buf.split(|&byte| byte == 0) {
        if chunk.is_empty() {
            continue;
        }

        let line = std::str::from_utf8(chunk).ok()?;
        if let Some((key, value)) = line.split_once('=') {
            props.insert(key.to_string(), value.to_string());
            continue;
        }

        if let Some((action, devpath)) = line.split_once('@') {
            props.insert("ACTION".into(), action.to_string());
            props.insert("DEVPATH".into(), devpath.to_string());
        }
    }

    let action = props.remove("ACTION")?;
    let devpath = props.remove("DEVPATH")?;
    let subsystem = props.remove("SUBSYSTEM").unwrap_or_default();

    Some(Uevent {
        action,
        devpath,
        subsystem,
        devname: props.remove("DEVNAME"),
        name: props.remove("NAME"),
    })
}

fn should_react_to_uevent(event: &Uevent) -> bool {
    if event.subsystem != "video4linux" {
        return false;
    }

    if !matches!(event.action.as_str(), "add" | "remove" | "change") {
        return false;
    }

    if event.devpath.contains("/virtual/") {
        return false;
    }

    if let Some(name) = event.name.as_deref() {
        if name.contains("Linux Std") || name.contains("Linux Standardized") {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event(devpath: &str, name: Option<&str>) -> Uevent {
        Uevent {
            action: "add".into(),
            devpath: devpath.into(),
            subsystem: "video4linux".into(),
            devname: Some("/dev/video0".into()),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn ignores_virtual_loopback_hotplug() {
        assert!(!should_react_to_uevent(&sample_event(
            "/devices/virtual/video4linux/video4",
            Some("Fantech Luminous C3 - Linux Std")
        )));
    }

    #[test]
    fn reacts_to_physical_usb_hotplug() {
        assert!(should_react_to_uevent(&sample_event(
            "/devices/pci0000:00/0000:00:14.0/usb1/1-8/1-8:1.0/video4linux/video0",
            Some("webcam: Fantech Luminous C30")
        )));
    }

    #[test]
    fn ignores_standardized_name_on_physical_path() {
        assert!(!should_react_to_uevent(&sample_event(
            "/devices/pci0000:00/usb1/1-8/video4linux/video0",
            Some("webcam: Foo - Linux Standardized")
        )));
    }

    #[test]
    fn ignores_non_video_subsystem() {
        let mut event = sample_event("/devices/usb1/video4linux/video0", None);
        event.subsystem = "usb".into();
        assert!(!should_react_to_uevent(&event));
    }

    #[test]
    fn parse_uevent_message() {
        let raw = b"add@/devices/pci0000:00/usb1/1-8/video4linux/video0\0ACTION=add\0DEVPATH=/devices/pci0000:00/usb1/1-8/video4linux/video0\0SUBSYSTEM=video4linux\0DEVNAME=/dev/video0\0NAME=webcam: Example\0";
        let event = parse_uevent(raw).expect("uevent");
        assert_eq!(event.action, "add");
        assert_eq!(event.subsystem, "video4linux");
        assert_eq!(event.devname.as_deref(), Some("/dev/video0"));
        assert_eq!(event.name.as_deref(), Some("webcam: Example"));
    }
}
