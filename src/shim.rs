use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use v4l::buffer::Type;
use v4l::format::FourCC;
use v4l::framesize::FrameSize;
use v4l::io::mmap::Stream;
use v4l::io::traits::{CaptureStream, Stream as StreamTrait};
use v4l::prelude::*;
use v4l::video::output::Parameters as OutputParameters;
use v4l::video::{Capture, Output};

use crate::compat::{
    native_capture_fps_from_intervals, DEFAULT_MAX_CAPTURE_HEIGHT, DEFAULT_MAX_CAPTURE_WIDTH,
    DEFAULT_TARGET_FPS,
};
use crate::error::{CamShimError, Result};
use crate::loopback_output::{LoopbackOutput, OUTPUT_BUFFERS};

const ENODEV: i32 = 19;
const CAPTURE_BUFFERS: u32 = 2;
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_millis(2);

/// Steady wall-clock pacing for loopback output (avoids bursty dup/drop).
struct FramePacer {
    frame_period: Duration,
    next_tick: Instant,
}

impl FramePacer {
    fn new(fps: u32) -> Self {
        let fps = fps.max(1);
        Self {
            frame_period: Duration::from_nanos(1_000_000_000 / u64::from(fps)),
            next_tick: Instant::now(),
        }
    }

    #[cfg(test)]
    fn frame_period(&self) -> Duration {
        self.frame_period
    }

    /// Wait until the next output tick. Uses wall-clock scheduling so a slow
    /// capture loop does not burst multiple frames to catch up.
    fn wait_for_tick(&mut self, running: &AtomicBool) -> bool {
        while running.load(Ordering::SeqCst) {
            let now = Instant::now();
            if now >= self.next_tick {
                self.next_tick = now + self.frame_period;
                return true;
            }

            let remaining = self.next_tick - now;
            thread::sleep(remaining.min(Duration::from_millis(2)));
        }

        false
    }
}

#[derive(Debug, Clone, Copy)]
struct CaptureLimits {
    max_width: u32,
    max_height: u32,
}

pub struct ShimConfig {
    pub source_path: String,
    pub target_path: String,
    pub target_fps: u32,
    /// Skip UVC modes wider than this when negotiating capture format.
    pub max_capture_width: u32,
    /// Skip UVC modes taller than this when negotiating capture format.
    pub max_capture_height: u32,
}

pub fn run_shim(config: ShimConfig) -> Result<()> {
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || {
            running.store(false, Ordering::SeqCst);
        })
        .map_err(|err| CamShimError::Io(std::io::Error::other(format!("ctrl-c handler: {err}"))))?;
    }

    run_shim_until(config, running, None)
}

pub fn run_shim_until(
    config: ShimConfig,
    running: Arc<AtomicBool>,
    heartbeat: Option<Arc<std::sync::atomic::AtomicU64>>,
) -> Result<()> {
    running.store(true, Ordering::SeqCst);

    let mut target = Device::with_path(&config.target_path)
        .map_err(|_| CamShimError::DeviceNotFound(config.target_path.clone()))?;

    let mut source = open_source_device(&config.source_path)?;
    let limits = CaptureLimits {
        max_width: config.max_capture_width,
        max_height: config.max_capture_height,
    };
    let source_format = configure_source(&mut source, limits)?;
    let capture_fps = native_capture_fps(&source, &source_format);
    let loopback_format = configure_target(&target, &source_format, config.target_fps)?;

    // v4l2loopback with exclusive_caps=1 only advertises Video Capture after a
    // producer attaches via STREAMON + mmap output. Raw write() does not count.
    let mut loopback_out = LoopbackOutput::open(&mut target, OUTPUT_BUFFERS)?;
    let mut cap_stream = Stream::with_buffers(&source, Type::VideoCapture, CAPTURE_BUFFERS)?;
    cap_stream.set_timeout(CAPTURE_DRAIN_TIMEOUT);
    let (buffer, meta) = CaptureStream::next(&mut cap_stream).map_err(CamShimError::Io)?;
    let frame = capture_frame_slice(buffer, meta)?;
    let mut latest_frame = frame.to_vec();
    loopback_out.prime(frame)?;

    if let Some(hb) = heartbeat.as_ref() {
        touch_heartbeat(hb);
    }

    tracing::info!(
        source = %config.source_path,
        target = %config.target_path,
        source_fourcc = %loopback_format.fourcc,
        target_fourcc = %loopback_format.fourcc,
        width = loopback_format.width,
        height = loopback_format.height,
        target_fps = config.target_fps,
        capture_fps = capture_fps.unwrap_or(0),
        max_capture_width = config.max_capture_width,
        max_capture_height = config.max_capture_height,
        "shim running"
    );

    while running.load(Ordering::SeqCst) {
        if let Err(err) = run_capture_session(
            &config,
            &loopback_format,
            &mut cap_stream,
            &mut loopback_out,
            &mut latest_frame,
            &running,
            heartbeat.as_ref(),
        ) {
            tracing::warn!(%err, "capture session error — retrying");
            pause_physical_capture(&mut cap_stream);
            thread::sleep(Duration::from_millis(100));
        }
    }

    tracing::info!("shim stopped");
    Ok(())
}

fn run_capture_session(
    config: &ShimConfig,
    loopback_format: &v4l::format::Format,
    cap_stream: &mut Stream<'_>,
    loopback_out: &mut LoopbackOutput<'_>,
    latest_frame: &mut Vec<u8>,
    running: &Arc<AtomicBool>,
    heartbeat: Option<&Arc<std::sync::atomic::AtomicU64>>,
) -> Result<()> {
    let mut pacer = FramePacer::new(config.target_fps);

    while pacer.wait_for_tick(running) {
        drain_capture_frames(cap_stream, latest_frame)?;
        submit_loopback_frame(
            loopback_out,
            latest_frame,
            &config.target_path,
            loopback_format,
        )?;
        if let Some(hb) = heartbeat {
            touch_heartbeat(hb);
        }
    }

    Ok(())
}

/// Read every frame currently queued on the capture device, keeping the newest.
fn drain_capture_frames(cap_stream: &mut Stream<'_>, latest_frame: &mut Vec<u8>) -> Result<()> {
    loop {
        match CaptureStream::next(cap_stream) {
            Ok((buffer, meta)) => {
                let frame = capture_frame_slice(buffer, meta)?;
                latest_frame.clear();
                latest_frame.extend_from_slice(frame);
            }
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => break,
            Err(err) => return Err(CamShimError::Io(err)),
        }
    }

    if latest_frame.is_empty() {
        return Err(CamShimError::Io(std::io::Error::other(
            "no capture frame available for paced output",
        )));
    }

    Ok(())
}

fn submit_loopback_frame(
    loopback_out: &mut LoopbackOutput<'_>,
    frame: &[u8],
    target_path: &str,
    loopback_format: &v4l::format::Format,
) -> Result<()> {
    match loopback_out.submit(frame) {
        Ok(()) => Ok(()),
        Err(err) if LoopbackOutput::is_queue_timeout(&err) => {
            tracing::debug!(
                %err,
                target = target_path,
                fourcc = %loopback_format.fourcc,
                "loopback submit skipped (output backpressure)"
            );
            Ok(())
        }
        Err(err) => Err(CamShimError::Io(std::io::Error::other(format!(
            "failed writing {} bytes to {} as {}: {err}",
            frame.len(),
            target_path,
            loopback_format.fourcc
        )))),
    }
}

fn pause_physical_capture(stream: &mut Stream<'_>) {
    if let Err(err) = stream.stop() {
        tracing::debug!(%err, "capture stream stop (may already be off)");
    }
}

/// Validate capture metadata and return a slice into the mmap buffer.
fn capture_frame_slice<'a>(buffer: &'a [u8], meta: &v4l::buffer::Metadata) -> Result<&'a [u8]> {
    let len = meta.bytesused as usize;
    if len == 0 || len > buffer.len() {
        return Err(CamShimError::Io(std::io::Error::other(
            "received empty frame from physical camera",
        )));
    }
    Ok(&buffer[..len])
}

fn open_source_device(path: &str) -> Result<Device> {
    const ATTEMPTS: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_millis(200);

    let mut last_err = None;
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            thread::sleep(RETRY_DELAY);
        }
        match Device::with_path(path) {
            Ok(dev) => return Ok(dev),
            Err(err) => last_err = Some(err),
        }
    }

    let err = last_err.unwrap_or_else(|| std::io::Error::other("unknown open error"));
    Err(CamShimError::Io(std::io::Error::other(format!(
        "could not open source device {path}: {err}"
    ))))
}

fn configure_source(source: &mut Device, limits: CaptureLimits) -> Result<v4l::format::Format> {
    let fourcc = pick_capture_fourcc(source)?;
    let resolutions = pick_sizes_sorted(source, fourcc, limits)?;

    if resolutions.is_empty() {
        return try_fallback_resolutions(source, fourcc, limits);
    }

    let mut last_err = None;
    let preferred = resolutions.first().copied();
    for (width, height) in &resolutions {
        let mut format = Capture::format(source)?;
        format.fourcc = fourcc;
        format.width = *width;
        format.height = *height;

        match Capture::set_format(source, &format) {
            Ok(format) => {
                if Some((*width, *height)) != preferred {
                    tracing::info!(
                        %fourcc,
                        width = *width,
                        height = *height,
                        "using fallback capture resolution"
                    );
                }
                return Ok(format);
            }
            Err(err) if err.raw_os_error() == Some(ENODEV) => {
                return Err(format_error("source", &format, err));
            }
            Err(err) => {
                tracing::debug!(
                    %fourcc,
                    width = *width,
                    height = *height,
                    %err,
                    "resolution not usable, trying next"
                );
                last_err = Some(err);
            }
        }
    }

    let (width, height) = preferred.unwrap_or((0, 0));
    let format = v4l::format::Format {
        width,
        height,
        fourcc,
        ..Capture::format(source)?
    };
    Err(format_error(
        "source",
        &format,
        last_err.unwrap_or_else(|| std::io::Error::other("no usable capture resolution")),
    ))
}

fn pick_sizes_sorted(
    source: &Device,
    fourcc: FourCC,
    limits: CaptureLimits,
) -> Result<Vec<(u32, u32)>> {
    let sizes = Capture::enum_framesizes(source, fourcc)?;
    let mut resolutions: Vec<(u32, u32)> = discrete_sizes(&sizes)
        .into_iter()
        .filter(|(width, height)| resolution_within_cap(*width, *height, limits))
        .collect();

    let total = discrete_sizes(&sizes).len();
    if resolutions.len() < total {
        tracing::debug!(
            %fourcc,
            kept = resolutions.len(),
            total,
            max_width = limits.max_width,
            max_height = limits.max_height,
            "skipping capture modes above negotiation cap"
        );
    }

    resolutions.sort_by(|a, b| (b.0 * b.1).cmp(&(a.0 * a.1)).then_with(|| b.0.cmp(&a.0)));
    Ok(resolutions)
}

fn resolution_within_cap(width: u32, height: u32, limits: CaptureLimits) -> bool {
    width <= limits.max_width && height <= limits.max_height
}

fn try_fallback_resolutions(
    source: &mut Device,
    fourcc: FourCC,
    limits: CaptureLimits,
) -> Result<v4l::format::Format> {
    const FALLBACKS: &[(u32, u32)] = &[(1920, 1080), (1280, 720), (1280, 960), (640, 480)];

    let mut last_err = None;
    for (width, height) in FALLBACKS {
        if !resolution_within_cap(*width, *height, limits) {
            continue;
        }

        let mut format = Capture::format(source)?;
        format.fourcc = fourcc;
        format.width = *width;
        format.height = *height;

        match Capture::set_format(source, &format) {
            Ok(format) => {
                tracing::info!(
                    %fourcc,
                    width = *width,
                    height = *height,
                    "using fallback capture resolution"
                );
                return Ok(format);
            }
            Err(err) => {
                tracing::debug!(
                    %fourcc,
                    width = *width,
                    height = *height,
                    %err,
                    "fallback resolution not usable, trying next"
                );
                last_err = Some(err);
            }
        }
    }

    Err(format_error(
        "source",
        &v4l::format::Format {
            width: limits.max_width,
            height: limits.max_height,
            fourcc,
            ..Capture::format(source)?
        },
        last_err.unwrap_or_else(|| {
            std::io::Error::other(format!(
                "no capture mode at or below {}x{}",
                limits.max_width, limits.max_height
            ))
        }),
    ))
}

fn configure_target(
    target: &Device,
    source_format: &v4l::format::Format,
    loopback_fps: u32,
) -> Result<v4l::format::Format> {
    let mut format = Output::format(target).map_err(|err| {
        CamShimError::Io(std::io::Error::other(format!(
            "could not read loopback output format: {err}"
        )))
    })?;

    format.width = source_format.width;
    format.height = source_format.height;
    format.fourcc = source_format.fourcc;

    let format = Output::set_format(target, &format)
        .map_err(|err| format_error("loopback", &format, err))?;

    let params = OutputParameters::with_fps(loopback_fps);
    Output::set_params(target, &params).map_err(|err| {
        CamShimError::Io(std::io::Error::other(format!(
            "could not set loopback output fps to {loopback_fps}: {err}"
        )))
    })?;

    Ok(format)
}

fn native_capture_fps(source: &Device, source_format: &v4l::format::Format) -> Option<u32> {
    let intervals = Capture::enum_frameintervals(
        source,
        source_format.fourcc,
        source_format.width,
        source_format.height,
    )
    .map_err(|err| {
        tracing::debug!(
            %err,
            fourcc = %source_format.fourcc,
            width = source_format.width,
            height = source_format.height,
            "could not read capture frame intervals"
        );
    })
    .ok()?;

    native_capture_fps_from_intervals(&intervals)
}

fn pick_capture_fourcc(source: &Device) -> Result<FourCC> {
    let current = Capture::format(source)?;
    if is_compressed(current.fourcc) {
        return Ok(current.fourcc);
    }

    if Capture::enum_framesizes(source, FourCC::new(b"MJPG")).is_ok_and(|s| !s.is_empty()) {
        return Ok(FourCC::new(b"MJPG"));
    }

    for format in Capture::enum_formats(source)? {
        if is_compressed(format.fourcc) {
            return Ok(format.fourcc);
        }
    }

    Ok(current.fourcc)
}

fn discrete_sizes(sizes: &[FrameSize]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for size in sizes {
        match &size.size {
            v4l::framesize::FrameSizeEnum::Discrete(discrete) => {
                out.push((discrete.width, discrete.height));
            }
            v4l::framesize::FrameSizeEnum::Stepwise(stepwise) => {
                for width in (stepwise.min_width..=stepwise.max_width)
                    .step_by(stepwise.step_width.max(1) as usize)
                {
                    for height in (stepwise.min_height..=stepwise.max_height)
                        .step_by(stepwise.step_height.max(1) as usize)
                    {
                        out.push((width, height));
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn is_compressed(fourcc: FourCC) -> bool {
    matches!(
        &fourcc.repr,
        b"MJPG" | b"JPEG" | b"H264" | b"HEVC" | b"MPEG" | b"MPG1" | b"MPG2" | b"MPG4"
    )
}

fn format_error(device: &str, format: &v4l::format::Format, err: std::io::Error) -> CamShimError {
    CamShimError::Io(std::io::Error::other(format!(
        "could not set {device} format to {} {}x{}: {err}",
        format.fourcc, format.width, format.height
    )))
}

fn touch_heartbeat(hb: &std::sync::atomic::AtomicU64) {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    hb.store(ms, Ordering::Relaxed);
}

pub fn default_shim_config(source_path: String, target_path: String) -> ShimConfig {
    ShimConfig {
        source_path,
        target_path,
        target_fps: DEFAULT_TARGET_FPS,
        max_capture_width: DEFAULT_MAX_CAPTURE_WIDTH,
        max_capture_height: DEFAULT_MAX_CAPTURE_HEIGHT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_compressed_fourcc() {
        assert!(is_compressed(FourCC::new(b"MJPG")));
        assert!(!is_compressed(FourCC::new(b"YUYV")));
    }

    #[test]
    fn picks_largest_resolution_first() {
        let mut resolutions = [(640, 480), (1920, 1080), (1280, 720)];
        resolutions.sort_by(|a, b| (b.0 * b.1).cmp(&(a.0 * a.1)).then_with(|| b.0.cmp(&a.0)));
        assert_eq!(resolutions[0], (1920, 1080));
    }

    #[test]
    fn caps_resolution_at_1080p() {
        let limits = CaptureLimits {
            max_width: DEFAULT_MAX_CAPTURE_WIDTH,
            max_height: DEFAULT_MAX_CAPTURE_HEIGHT,
        };
        assert!(resolution_within_cap(1920, 1080, limits));
        assert!(resolution_within_cap(1280, 720, limits));
        assert!(!resolution_within_cap(2560, 1440, limits));
        assert!(!resolution_within_cap(2304, 1296, limits));

        let mut resolutions = vec![(2560, 1440), (1920, 1080), (1280, 720)];
        resolutions.retain(|(w, h)| resolution_within_cap(*w, *h, limits));
        resolutions.sort_by_key(|b| std::cmp::Reverse(b.0 * b.1));
        assert_eq!(resolutions[0], (1920, 1080));
    }

    #[test]
    fn frame_pacer_period_for_30fps() {
        let pacer = FramePacer::new(30);
        assert_eq!(pacer.frame_period(), Duration::from_nanos(33_333_333));
    }

    #[test]
    fn frame_pacer_period_clamps_zero_fps() {
        let pacer = FramePacer::new(0);
        assert_eq!(pacer.frame_period(), Duration::from_nanos(1_000_000_000));
    }
}
