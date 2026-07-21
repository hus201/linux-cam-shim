//! v4l2loopback output buffer lifecycle.
//!
//! v4l2loopback with `exclusive_caps=1` only exposes Video Capture to apps after
//! a producer attaches via STREAMON + mmap output. Consumers also reject empty
//! QBUF payloads (EINVAL). This module centralizes the rules:
//!
//! ```text
//!   Unprimed ──prime()──► Ready ──submit()──► Streaming
//!                                              │
//!                                              └── submit() ──► Streaming
//! ```
//!
//! Bootstrap uses mmap (STREAMON + QBUF). Steady streaming uses `write(2)` on a
//! second handle so we do not depend on OUTPUT DQBUF while buffers are full.

use std::io::{self, Write};
use std::time::Duration;

use libc;
use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::OutputStream;
use v4l::prelude::Device;

use crate::error::{CamShimError, Result};

pub const OUTPUT_BUFFERS: u32 = 2;
const OUTPUT_MMAP_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopbackOutputState {
    /// Output stream created; no filled buffers queued yet.
    Unprimed,
    /// Bootstrap complete — safe for consumers to attach.
    Ready,
    /// Capture session active; frames are submitted each tick.
    Streaming,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputEvent {
    Prime,
    Submit,
}

pub struct LoopbackOutput<'a> {
    stream: Stream<'a>,
    write_dev: Device,
    state: LoopbackOutputState,
}

impl<'a> LoopbackOutput<'a> {
    pub fn open(target: &'a mut Device, device_path: &str, buffer_count: u32) -> Result<Self> {
        let write_dev = Device::with_path(device_path)
            .map_err(|_| CamShimError::DeviceNotFound(device_path.to_string()))?;

        let mut stream = Stream::with_buffers(target, Type::VideoOutput, buffer_count)
            .map_err(CamShimError::Io)?;
        stream.set_timeout(OUTPUT_MMAP_TIMEOUT);

        Ok(Self {
            stream,
            write_dev,
            state: LoopbackOutputState::Unprimed,
        })
    }

    pub fn state(&self) -> LoopbackOutputState {
        self.state
    }

    /// Queue the first frame and pre-fill the next output buffer.
    pub fn prime(&mut self, frame: &[u8]) -> Result<()> {
        self.require_state(LoopbackOutputState::Unprimed, "prime")?;
        validate_frame(frame)?;

        self.queue_filled_buffer(frame)?;
        self.queue_filled_buffer(frame)?;

        self.state = LoopbackOutputState::Ready;
        tracing::debug!(state = ?self.state, "loopback primed for capture-side discovery");
        Ok(())
    }

    /// Push one frame while streaming.
    pub fn submit(&mut self, frame: &[u8]) -> Result<()> {
        self.require_state_any(
            &[LoopbackOutputState::Ready, LoopbackOutputState::Streaming],
            "submit",
        )?;
        validate_frame(frame)?;

        match self.write_dev.write_all(frame) {
            Ok(()) => {
                self.state = LoopbackOutputState::Streaming;
                Ok(())
            }
            Err(err) if is_write_backpressure(&err) => {
                tracing::trace!(%err, "loopback write skipped (backpressure)");
                Ok(())
            }
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => {
                tracing::debug!(%err, "loopback write busy — falling back to mmap output");
                self.submit_via_mmap(frame)
            }
            Err(err) => Err(CamShimError::Io(err)),
        }
    }

    fn submit_via_mmap(&mut self, frame: &[u8]) -> Result<()> {
        match self.queue_filled_buffer(frame) {
            Ok(()) => {
                self.state = LoopbackOutputState::Streaming;
                Ok(())
            }
            Err(err) if Self::is_mmap_backpressure(&err) => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn queue_filled_buffer(&mut self, frame: &[u8]) -> Result<()> {
        queue_filled_buffer(&mut self.stream, frame)
    }

    pub fn is_backpressure(err: &CamShimError) -> bool {
        match err {
            CamShimError::Io(io_err) => {
                is_write_backpressure(io_err) || is_mmap_backpressure(io_err)
            }
            _ => false,
        }
    }

    pub fn is_mmap_backpressure(err: &CamShimError) -> bool {
        match err {
            CamShimError::Io(io_err) => is_mmap_backpressure(io_err),
            _ => false,
        }
    }

    #[allow(dead_code)]
    pub fn is_queue_timeout(err: &CamShimError) -> bool {
        Self::is_mmap_backpressure(err)
    }

    fn require_state(&self, expected: LoopbackOutputState, action: &str) -> Result<()> {
        if self.state == expected {
            return Ok(());
        }
        Err(invalid_state(action, self.state, &[expected]))
    }

    fn require_state_any(&self, expected: &[LoopbackOutputState], action: &str) -> Result<()> {
        if expected.contains(&self.state) {
            return Ok(());
        }
        Err(invalid_state(action, self.state, expected))
    }
}

fn is_write_backpressure(err: &io::Error) -> bool {
    err.kind() == io::ErrorKind::WouldBlock
        || matches!(err.raw_os_error(), Some(libc::EAGAIN))
}

fn is_mmap_backpressure(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::TimedOut {
        return true;
    }

    matches!(
        err.raw_os_error(),
        Some(libc::EAGAIN) | Some(libc::EBUSY) | Some(libc::EINVAL)
    )
}

fn queue_filled_buffer(stream: &mut Stream<'_>, frame: &[u8]) -> Result<()> {
    let (buf, meta) = OutputStream::next(stream).map_err(CamShimError::Io)?;
    write_frame_into_buffer(buf, meta, frame).map_err(CamShimError::Io)
}

fn validate_frame(frame: &[u8]) -> Result<()> {
    if frame.is_empty() {
        return Err(CamShimError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing to queue empty loopback output frame",
        )));
    }
    Ok(())
}

fn write_frame_into_buffer(
    buf: &mut [u8],
    meta: &mut v4l::buffer::Metadata,
    frame: &[u8],
) -> io::Result<()> {
    if frame.len() > buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "frame is {} bytes but loopback buffer is only {} bytes",
                frame.len(),
                buf.len()
            ),
        ));
    }
    buf[..frame.len()].copy_from_slice(frame);
    meta.bytesused = frame.len() as u32;
    meta.field = 0;
    Ok(())
}

fn invalid_state(
    action: &str,
    current: LoopbackOutputState,
    allowed: &[LoopbackOutputState],
) -> CamShimError {
    let allowed = allowed
        .iter()
        .map(|state| format!("{state:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    CamShimError::Io(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("loopback output cannot {action} in state {current:?} (allowed: {allowed})"),
    ))
}

#[cfg(test)]
fn next_state(current: LoopbackOutputState, event: OutputEvent) -> Option<LoopbackOutputState> {
    use LoopbackOutputState::*;
    match (current, event) {
        (Unprimed, OutputEvent::Prime) => Some(Ready),
        (Ready, OutputEvent::Submit) => Some(Streaming),
        (Streaming, OutputEvent::Submit) => Some(Streaming),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_transition_table() {
        use LoopbackOutputState::*;
        assert_eq!(next_state(Unprimed, OutputEvent::Prime), Some(Ready));
        assert_eq!(next_state(Ready, OutputEvent::Submit), Some(Streaming));
        assert_eq!(next_state(Streaming, OutputEvent::Submit), Some(Streaming));
        assert_eq!(next_state(Unprimed, OutputEvent::Submit), None);
    }

    #[test]
    fn rejects_empty_frame() {
        assert!(validate_frame(&[]).is_err());
        assert!(validate_frame(&[0u8; 64]).is_ok());
    }

    #[test]
    fn write_frame_sets_bytesused() {
        let mut buf = [0u8; 128];
        let mut meta = v4l::buffer::Metadata::default();
        write_frame_into_buffer(&mut buf, &mut meta, &[1, 2, 3, 4]).unwrap();
        assert_eq!(meta.bytesused, 4);
        assert_eq!(&buf[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn write_frame_rejects_oversized_payload() {
        let mut buf = [0u8; 8];
        let mut meta = v4l::buffer::Metadata::default();
        assert!(write_frame_into_buffer(&mut buf, &mut meta, &[0; 16]).is_err());
    }

    #[test]
    fn detects_write_backpressure() {
        assert!(is_write_backpressure(&io::Error::from(io::ErrorKind::WouldBlock)));
        assert!(is_write_backpressure(&io::Error::from_raw_os_error(libc::EAGAIN)));
        assert!(!is_write_backpressure(&io::Error::from_raw_os_error(libc::EBUSY)));
    }

    #[test]
    fn detects_mmap_backpressure() {
        assert!(is_mmap_backpressure(&io::Error::from(io::ErrorKind::TimedOut)));
        assert!(is_mmap_backpressure(&io::Error::from_raw_os_error(libc::EINVAL)));
        assert!(!is_mmap_backpressure(&io::Error::from_raw_os_error(libc::ENODEV)));
    }
}
