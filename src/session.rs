use crate::error::Result;
use crate::devices::repair_video_devices;
use crate::loopback::remove_loopback_device;

pub struct FixSession {
    loopback_index: u32,
    loopback_path: String,
    cleanup_on_drop: bool,
}

impl FixSession {
    pub fn new(loopback_path: String) -> Result<Self> {
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
        let loopback_err = remove_loopback_device(self.loopback_index).err();
        let _ = repair_video_devices();

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

pub fn repair_devices() -> Result<crate::devices::RepairReport> {
    repair_video_devices()
}
