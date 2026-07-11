use std::collections::BTreeSet;

use v4l::frameinterval::{FrameInterval, FrameIntervalEnum};
use v4l::fraction::Fraction;

pub const STANDARDIZED_SUFFIX: &str = " - Linux Standardized";
pub const DEFAULT_TARGET_FPS: u32 = 30;
pub const DEFAULT_MAX_CAPTURE_WIDTH: u32 = 1920;
pub const DEFAULT_MAX_CAPTURE_HEIGHT: u32 = 1080;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FpsRate {
    pub numerator: u32,
    pub denominator: u32,
}

impl FpsRate {
    pub fn fps(&self) -> f64 {
        if self.numerator == 0 || self.denominator == 0 {
            return 0.0;
        }
        self.numerator as f64 / self.denominator as f64
    }

    pub fn display(&self) -> String {
        if self.is_variable() {
            return "variable".into();
        }

        if self.denominator == 1 {
            format!("{} fps", self.numerator)
        } else {
            format!("{}/{}", self.numerator, self.denominator)
        }
    }

    pub fn is_variable(&self) -> bool {
        self.numerator == 0 && self.denominator == 0
    }

    pub fn is_standard(&self) -> bool {
        !self.is_variable()
            && ((self.numerator == 30 && self.denominator == 1)
                || (self.numerator == 60 && self.denominator == 1))
    }

    pub fn as_u32_fps(&self) -> u32 {
        if self.is_variable() || self.denominator == 0 {
            return 0;
        }
        ((self.numerator as f64 / self.denominator as f64).round() as u32).max(1)
    }
}

impl From<Fraction> for FpsRate {
    fn from(interval: Fraction) -> Self {
        interval_to_fps(interval)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatStatus {
    Compatible,
    NeedsShim,
}

#[derive(Debug, Clone)]
pub struct CompatReport {
    pub status: CompatStatus,
    pub advertised_fps: Vec<FpsRate>,
    pub issues: Vec<String>,
}

impl CompatReport {
    pub fn from_intervals(intervals: &[FrameInterval]) -> Self {
        let mut advertised_fps = BTreeSet::new();

        for interval in intervals {
            if let Some(rate) = fps_from_interval(interval) {
                advertised_fps.insert(rate);
            }
        }

        let advertised_fps: Vec<FpsRate> = advertised_fps.into_iter().collect();
        let mut issues = Vec::new();

        let has_variable = advertised_fps.iter().any(FpsRate::is_variable);
        let has_standard = advertised_fps.iter().any(FpsRate::is_standard);

        if has_variable {
            issues.push("reports variable frame rate".into());
        }

        if advertised_fps.is_empty() {
            issues.push("no frame intervals reported".into());
        } else if !has_standard {
            let native = advertised_fps
                .iter()
                .map(FpsRate::display)
                .collect::<Vec<_>>()
                .join(", ");
            issues.push(format!("no 30/60 fps modes (found: {native})"));
        }

        let status = if issues.is_empty() {
            CompatStatus::Compatible
        } else {
            CompatStatus::NeedsShim
        };

        Self {
            status,
            advertised_fps,
            issues,
        }
    }
}

/// Loopback fps metadata: keep the configured target when 30/60 is available,
/// otherwise advertise the camera's native rate (e.g. 25 fps PAL webcams).
pub fn loopback_fps_from_intervals(intervals: &[FrameInterval], requested: u32) -> u32 {
    let report = CompatReport::from_intervals(intervals);
    if report.advertised_fps.iter().any(FpsRate::is_standard) {
        return requested;
    }

    report
        .advertised_fps
        .iter()
        .filter(|rate| !rate.is_variable())
        .max_by(|a, b| {
            a.fps()
                .partial_cmp(&b.fps())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(FpsRate::as_u32_fps)
        .unwrap_or(requested)
}

pub fn standardized_label(original_name: &str) -> String {
    let trimmed = original_name.trim();
    if trimmed.is_empty() {
        return format!("Webcam{STANDARDIZED_SUFFIX}");
    }
    format!("{trimmed}{STANDARDIZED_SUFFIX}")
}

/// v4l2loopback `card_label` is limited to 31 bytes. Keep the standardized suffix visible.
pub fn kernel_card_label(display_label: &str) -> String {
    const MAX: usize = 31;
    const KERNEL_SUFFIX: &str = " - Linux Std";

    let mut label = display_label.trim().to_string();
    if let Some(stripped) = label.strip_prefix("webcam: ") {
        label = stripped.to_string();
    }

    if label.len() <= MAX {
        return label;
    }

    if label.contains("Linux Standardized") {
        let prefix_len = MAX.saturating_sub(KERNEL_SUFFIX.len());
        let prefix: String = label.chars().take(prefix_len).collect();
        return format!("{prefix}{KERNEL_SUFFIX}");
    }

    label.chars().take(MAX).collect()
}

pub fn kernel_card_label_bytes(display_label: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    let label = kernel_card_label(display_label);
    let bytes = label.as_bytes();
    let len = bytes.len().min(out.len() - 1);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

fn fps_from_interval(interval: &FrameInterval) -> Option<FpsRate> {
    match &interval.interval {
        FrameIntervalEnum::Discrete(fraction) => Some(interval_to_fps(*fraction)),
        FrameIntervalEnum::Stepwise(stepwise) => Some(interval_to_fps(stepwise.min)),
    }
}

fn interval_to_fps(interval: Fraction) -> FpsRate {
    if interval.numerator == 0 || interval.denominator == 0 {
        return FpsRate {
            numerator: 0,
            denominator: 0,
        };
    }

    FpsRate {
        numerator: interval.denominator,
        denominator: interval.numerator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use v4l::format::FourCC;

    #[test]
    fn standardized_label_uses_suffix() {
        assert_eq!(
            standardized_label("HD USB Camera"),
            "HD USB Camera - Linux Standardized"
        );
    }

    #[test]
    fn detects_non_standard_fps() {
        let report = CompatReport::from_intervals(&[FrameInterval {
            index: 0,
            fourcc: FourCC::new(b"YUYV"),
            width: 640,
            height: 480,
            typ: 0,
            interval: FrameIntervalEnum::Discrete(Fraction::new(1, 25)),
        }]);
        assert_eq!(report.status, CompatStatus::NeedsShim);
    }

    #[test]
    fn loopback_fps_uses_native_rate_when_not_standard() {
        let intervals = [FrameInterval {
            index: 0,
            fourcc: FourCC::new(b"MJPG"),
            width: 1920,
            height: 1080,
            typ: 0,
            interval: FrameIntervalEnum::Discrete(Fraction::new(1, 25)),
        }];
        assert_eq!(loopback_fps_from_intervals(&intervals, 30), 25);
    }

    #[test]
    fn loopback_fps_keeps_requested_when_standard_available() {
        let intervals = [
            FrameInterval {
                index: 0,
                fourcc: FourCC::new(b"MJPG"),
                width: 1280,
                height: 720,
                typ: 0,
                interval: FrameIntervalEnum::Discrete(Fraction::new(1, 25)),
            },
            FrameInterval {
                index: 1,
                fourcc: FourCC::new(b"MJPG"),
                width: 1280,
                height: 720,
                typ: 0,
                interval: FrameIntervalEnum::Discrete(Fraction::new(1, 30)),
            },
        ];
        assert_eq!(loopback_fps_from_intervals(&intervals, 30), 30);
    }
}
