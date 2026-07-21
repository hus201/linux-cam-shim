use std::collections::HashMap;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;

pub const DEVICES_FILE: &str = "/run/cam-shim/devices.json";
const STATE_DIR: &str = "/run/cam-shim";

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceRegistry {
    pub updated_at_ms: u64,
    pub assignments: HashMap<String, LoopbackAssignment>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopbackAssignment {
    pub loopback_index: u32,
    pub label: String,
}

pub fn read_device_registry() -> Result<DeviceRegistry> {
    let raw = match fs::read_to_string(DEVICES_FILE) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DeviceRegistry::default());
        }
        Err(err) => return Err(err.into()),
    };

    serde_json::from_str(&raw).map_err(|err| {
        crate::error::CamShimError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("failed to parse {DEVICES_FILE}: {err}"),
        ))
    })
}

pub fn write_device_registry(registry: &DeviceRegistry) -> Result<()> {
    fs::create_dir_all(STATE_DIR)?;
    let body = serde_json::to_string_pretty(registry).map_err(|err| {
        crate::error::CamShimError::Io(std::io::Error::other(format!(
            "failed to serialize device registry: {err}"
        )))
    })?;
    fs::write(DEVICES_FILE, body)?;
    Ok(())
}

pub fn lookup_loopback_index(camera_key: &str) -> Option<u32> {
    read_device_registry()
        .ok()
        .and_then(|registry| registry.assignments.get(camera_key).map(|a| a.loopback_index))
}

pub fn assign_loopback_index(camera_key: &str, loopback_index: u32, label: &str) -> Result<()> {
    let mut registry = read_device_registry().unwrap_or_default();
    registry.updated_at_ms = unix_now_ms();
    registry.assignments.insert(
        camera_key.to_string(),
        LoopbackAssignment {
            loopback_index,
            label: label.to_string(),
        },
    );
    write_device_registry(&registry)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrip_serialization() {
        let mut registry = DeviceRegistry::default();
        registry.updated_at_ms = 1;
        registry.assignments.insert(
            "serial:ABC".into(),
            LoopbackAssignment {
                loopback_index: 10,
                label: "Cam - Linux Standardized".into(),
            },
        );

        let json = serde_json::to_string(&registry).expect("serialize");
        let parsed: DeviceRegistry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, registry);
    }

    #[test]
    fn lookup_missing_key_returns_none() {
        let registry = DeviceRegistry::default();
        assert!(registry.assignments.get("missing").is_none());
    }
}
