use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use udev::{EventType, MonitorBuilder};

use crate::error::Result;

const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(400);

pub fn spawn_udev_monitor(
    shutdown: Arc<AtomicBool>,
    notify: Sender<()>,
) -> Result<JoinHandle<()>> {
    let handle = thread::Builder::new()
        .name("cam-shim-udev".into())
        .spawn(move || run_monitor(shutdown, notify))
        .map_err(|err| {
            crate::error::CamShimError::Io(std::io::Error::other(format!(
                "failed to spawn udev monitor thread: {err}"
            )))
        })?;

    Ok(handle)
}

fn run_monitor(shutdown: Arc<AtomicBool>, notify: Sender<()>) {
    let monitor = match MonitorBuilder::new()
        .and_then(|builder| builder.match_subsystem("video4linux"))
        .and_then(|builder| builder.listen())
    {
        Ok(monitor) => monitor,
        Err(err) => {
            tracing::error!(%err, "failed to start udev hotplug monitor");
            return;
        }
    };

    tracing::info!("udev hotplug monitor listening on video4linux");

    let socket = monitor.as_raw_fd();
    let mut last_notify = Instant::now()
        .checked_sub(HOTPLUG_DEBOUNCE)
        .unwrap_or_else(Instant::now);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        if !wait_readable(socket, Duration::from_secs(1)) {
            continue;
        }

        loop {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }

            let mut got_event = false;
            for event in monitor.iter() {
                got_event = true;
                if !matches!(
                    event.event_type(),
                    EventType::Add | EventType::Remove | EventType::Change
                ) {
                    continue;
                }

                let devpath = event.devpath().to_string_lossy();
                let name = event
                    .property_value("NAME")
                    .map(|value| value.to_string_lossy().into_owned());

                if !should_react_to_hotplug(&devpath, name.as_deref()) {
                    tracing::trace!(
                        action = ?event.event_type(),
                        devpath = %devpath,
                        ?name,
                        "ignored udev event"
                    );
                    continue;
                }

                let devnode = event
                    .devnode()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| devpath.to_string());

                if last_notify.elapsed() < HOTPLUG_DEBOUNCE {
                    tracing::trace!(devnode, "debounced udev hotplug event");
                    continue;
                }

                last_notify = Instant::now();
                tracing::debug!(
                    action = ?event.event_type(),
                    devnode,
                    "udev hotplug event"
                );

                if notify.send(()).is_err() {
                    return;
                }
            }

            if !got_event {
                break;
            }
        }
    }
}

pub(crate) fn should_react_to_hotplug(devpath: &str, device_name: Option<&str>) -> bool {
    if devpath.contains("/virtual/") {
        return false;
    }

    if let Some(name) = device_name {
        if name.contains("Linux Std") || name.contains("Linux Standardized") {
            return false;
        }
    }

    true
}

fn wait_readable(fd: libc::c_int, timeout: Duration) -> bool {
    use libc::{pollfd, POLLIN, POLLPRI};

    let mut pfd = pollfd {
        fd,
        events: POLLIN | POLLPRI,
        revents: 0,
    };

    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe { libc::poll(&mut pfd, 1, ms) > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_virtual_loopback_hotplug() {
        assert!(!should_react_to_hotplug(
            "/devices/virtual/video4linux/video4",
            Some("Fantech Luminous C3 - Linux Std")
        ));
    }

    #[test]
    fn reacts_to_physical_usb_hotplug() {
        assert!(should_react_to_hotplug(
            "/devices/pci0000:00/0000:00:14.0/usb1/1-8/1-8:1.0/video4linux/video0",
            Some("webcam: Fantech Luminous C30")
        ));
    }

    #[test]
    fn ignores_standardized_name_on_physical_path() {
        assert!(!should_react_to_hotplug(
            "/devices/pci0000:00/usb1/1-8/video4linux/video0",
            Some("webcam: Foo - Linux Standardized")
        ));
    }
}
