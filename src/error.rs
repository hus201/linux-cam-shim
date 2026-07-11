use thiserror::Error;

#[derive(Debug, Error)]
pub enum CamShimError {
    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("device is not a video capture source: {0}")]
    NotCaptureDevice(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, CamShimError>;
