use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::hide::{remove_hide_rule, repair_video_devices, teardown_hide};
use crate::loopback::remove_loopback_device;

pub struct FixSession {
    loopback_index: u32,
    loopback_path: String,
    udev_rule_path: Option<PathBuf>,
    cleanup_on_drop: bool,
}

impl FixSession {
    pub fn new(loopback_path: String, udev_rule_path: Option<PathBuf>) -> Result<Self> {
        let loopback_index = loopback_path
            .strip_prefix("/dev/video")
            .and_then(|n| n.parse().ok())
            .ok_or_else(|| {
                crate::error::CamShimError::Io(std::io::Error::other(format!(
                    "invalid loopback path: {loopback_path}"
                )))
            })?;

        Ok(Self {
            loopback_index,
            loopback_path,
            udev_rule_path,
            cleanup_on_drop: true,
        })
    }

    pub fn loopback_path(&self) -> &str {
        &self.loopback_path
    }

    pub fn disable_cleanup(&mut self) {
        self.cleanup_on_drop = false;
    }

    pub fn cleanup(&mut self) -> Result<()> {
        let mut loopback_err = None;
        let mut hide_err = None;

        if let Err(err) = remove_loopback_device(self.loopback_index) {
            loopback_err = Some(err);
        }

        if let Some(rule_path) = self.udev_rule_path.take() {
            if let Err(err) = teardown_hide(&rule_path) {
                hide_err = Some(err);
            }
        } else {
            let _ = repair_video_devices();
        }

        if let Some(err) = hide_err {
            return Err(err);
        }
        if let Some(err) = loopback_err {
            return Err(err);
        }

        Ok(())
    }
}

impl Drop for FixSession {
    fn drop(&mut self) {
        if self.cleanup_on_drop {
            let _ = self.cleanup();
        }
    }
}

pub fn remove_udev_hide_rule(rule_path: &Path) -> Result<()> {
    remove_hide_rule(rule_path)
}

pub fn restore_all_hidden() -> Result<crate::hide::RestoreReport> {
    crate::hide::remove_all_hide_rules()?;
    crate::hide::repair_video_devices()
}
