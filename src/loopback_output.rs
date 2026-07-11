//! v4l2loopback output buffer lifecycle.
//!
//! v4l2loopback with `exclusive_caps=1` only exposes Video Capture to apps after
//! a producer attaches via STREAMON + mmap output. Consumers also reject empty
//! QBUF payloads (EINVAL). This module centralizes the rules:
//!
//! ```text
//!   Unprimed ──prime()──► Ready ──submit()──► Streaming
//!                            ▲                    │
//!                            └── hold_last_frame()┘
//! ```
//!
//! `hold_last_frame()` re-arms like `prime()`. While idle (no consumer), refresh
//! the cached frame at a low keepalive rate so Video Capture stays visible.

use std::io;
use std::time::Duration;

use v4l::buffer::Type;
use v4l::io::mmap::Stream;
use v4l::io::traits::OutputStream;
use v4l::prelude::Device;

use crate::error::{CamShimError, Result};

pub const OUTPUT_BUFFERS: u32 = 2;
const OUTPUT_QUEUE_TIMEOUT: Duration = Duration::from_millis(500);

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
    Pause,
}

pub struct LoopbackOutput<'a> {
    stream: Stream<'a>,
    state: LoopbackOutputState,
    last_frame: Option<Vec<u8>>,
}

impl<'a> LoopbackOutput<'a> {
    pub fn open(target: &'a mut Device, buffer_count: u32) -> Result<Self> {
        let mut stream = Stream::with_buffers(target, Type::VideoOutput, buffer_count)
            .map_err(CamShimError::Io)?;
        stream.set_timeout(OUTPUT_QUEUE_TIMEOUT);
        Ok(Self {
            stream,
            state: LoopbackOutputState::Unprimed,
            last_frame: None,
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

        remember_frame(&mut self.last_frame, frame);
        self.state = LoopbackOutputState::Ready;
        tracing::debug!(state = ?self.state, "loopback primed for capture-side discovery");
        Ok(())
    }

    /// Push one frame while a capture session is active (or the first frame after resume).
    pub fn submit(&mut self, frame: &[u8]) -> Result<()> {
        self.queue_frame(frame, true)
    }

    /// Queue the cached last frame without copying it again (idle keepalive / resume).
    pub fn submit_cached(&mut self) -> Result<()> {
        self.require_state_any(
            &[LoopbackOutputState::Ready, LoopbackOutputState::Streaming],
            "submit",
        )?;
        let Some(frame) = self.last_frame.as_ref() else {
            return Ok(());
        };
        validate_frame(frame)?;
        queue_filled_buffer(&mut self.stream, frame)?;
        self.state = LoopbackOutputState::Streaming;
        Ok(())
    }

    /// Re-queue two filled buffers (same as bootstrap) so capture side reappears.
    pub fn rearm_idle(&mut self) -> Result<()> {
        let Some(frame) = self.last_frame.as_ref() else {
            if self.state != LoopbackOutputState::Unprimed {
                self.state = LoopbackOutputState::Ready;
            }
            return Ok(());
        };

        validate_frame(frame)?;
        queue_filled_buffer(&mut self.stream, frame)?;
        queue_filled_buffer(&mut self.stream, frame)?;
        self.state = LoopbackOutputState::Ready;
        Ok(())
    }

    /// Re-queue the last real frame before pausing physical capture.
    pub fn hold_last_frame(&mut self) -> Result<()> {
        match self.state {
            LoopbackOutputState::Unprimed => Ok(()),
            LoopbackOutputState::Ready | LoopbackOutputState::Streaming => {
                self.rearm_idle()?;
                tracing::debug!(state = ?self.state, "loopback held last frame on capture pause");
                Ok(())
            }
        }
    }

    fn queue_frame(&mut self, frame: &[u8], remember: bool) -> Result<()> {
        self.require_state_any(
            &[LoopbackOutputState::Ready, LoopbackOutputState::Streaming],
            "submit",
        )?;
        validate_frame(frame)?;

        self.queue_filled_buffer(frame)?;
        if remember {
            remember_frame(&mut self.last_frame, frame);
        }
        self.state = LoopbackOutputState::Streaming;
        Ok(())
    }

    fn queue_filled_buffer(&mut self, frame: &[u8]) -> Result<()> {
        queue_filled_buffer(&mut self.stream, frame)
    }

    pub fn is_queue_timeout(err: &CamShimError) -> bool {
        matches!(err, CamShimError::Io(io_err) if io_err.kind() == io::ErrorKind::TimedOut)
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

fn queue_filled_buffer(stream: &mut Stream<'_>, frame: &[u8]) -> Result<()> {
    let (buf, meta) = OutputStream::next(stream).map_err(CamShimError::Io)?;
    write_frame_into_buffer(buf, meta, frame).map_err(CamShimError::Io)
}

fn remember_frame(slot: &mut Option<Vec<u8>>, frame: &[u8]) {
    match slot {
        Some(existing) => {
            existing.clear();
            existing.extend_from_slice(frame);
        }
        None => {
            *slot = Some(frame.to_vec());
        }
    }
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
        (Streaming, OutputEvent::Pause) => Some(Ready),
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
        assert_eq!(next_state(Streaming, OutputEvent::Pause), Some(Ready));
        assert_eq!(next_state(Unprimed, OutputEvent::Submit), None);
        assert_eq!(next_state(Ready, OutputEvent::Pause), None);
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
}
