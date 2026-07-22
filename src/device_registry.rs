use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Persistent loopback index assignments (survives reboot).
pub const PERSISTENT_DEVICES_FILE: &str = "/var/lib/cam-shim/devices.json";
/// Legacy runtime path — read for migration, no longer written.
pub const RUNTIME_DEVICES_FILE: &str = "/run/cam-shim/devices.json";
/// Primary registry path (persistent).
pub const DEVICES_FILE: &str = PERSISTENT_DEVICES_FILE;

const PERSISTENT_STATE_DIR: &str = "/var/lib/cam-shim";

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
    let persistent = read_registry_file(PERSISTENT_DEVICES_FILE)?.unwrap_or_default();
    let runtime = read_registry_file(RUNTIME_DEVICES_FILE)?.unwrap_or_default();
    let merged = merge_registries(persistent, &runtime);

    if Path::new(RUNTIME_DEVICES_FILE).exists() {
        migrate_runtime_registry(&merged)?;
    }

    Ok(merged)
}

pub fn write_device_registry(registry: &DeviceRegistry) -> Result<()> {
    fs::create_dir_all(PERSISTENT_STATE_DIR)?;
    write_registry_file(PERSISTENT_DEVICES_FILE, registry)
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

fn read_registry_file(path: &str) -> Result<Option<DeviceRegistry>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|err| {
            crate::error::CamShimError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("failed to parse {path}: {err}"),
            ))
        })
}

fn write_registry_file(path: &str, registry: &DeviceRegistry) -> Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        fs::create_dir_all(dir)?;
    }

    let body = serde_json::to_string_pretty(registry).map_err(|err| {
        crate::error::CamShimError::Io(std::io::Error::other(format!(
            "failed to serialize device registry: {err}"
        )))
    })?;
    fs::write(path, body)?;
    Ok(())
}

fn merge_registries(
    mut primary: DeviceRegistry,
    secondary: &DeviceRegistry,
) -> DeviceRegistry {
    for (key, assignment) in &secondary.assignments {
        primary
            .assignments
            .entry(key.clone())
            .or_insert_with(|| assignment.clone());
    }
    primary.updated_at_ms = primary.updated_at_ms.max(secondary.updated_at_ms);
    primary
}

fn migrate_runtime_registry(merged: &DeviceRegistry) -> Result<()> {
    if fs::metadata(RUNTIME_DEVICES_FILE).is_err() {
        return Ok(());
    }

    let persistent_only = read_registry_file(PERSISTENT_DEVICES_FILE)?.unwrap_or_default();
    if merged.assignments != persistent_only.assignments {
        write_device_registry(merged)?;
        tracing::info!(
            path = PERSISTENT_DEVICES_FILE,
            assignments = merged.assignments.len(),
            "migrated loopback index registry from /run to /var/lib"
        );
    }

    let _ = fs::remove_file(RUNTIME_DEVICES_FILE);
    Ok(())
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
                label: "Cam - Shim".into(),
            },
        );

        let json = serde_json::to_string(&registry).expect("serialize");
        let parsed: DeviceRegistry = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, registry);
    }

    #[test]
    fn merge_prefers_persistent_assignments() {
        let persistent = DeviceRegistry {
            updated_at_ms: 2,
            assignments: HashMap::from([(
                "serial:A".into(),
                LoopbackAssignment {
                    loopback_index: 10,
                    label: "A".into(),
                },
            )]),
        };
        let runtime = DeviceRegistry {
            updated_at_ms: 1,
            assignments: HashMap::from([
                (
                    "serial:A".into(),
                    LoopbackAssignment {
                        loopback_index: 11,
                        label: "A-old".into(),
                    },
                ),
                (
                    "serial:B".into(),
                    LoopbackAssignment {
                        loopback_index: 12,
                        label: "B".into(),
                    },
                ),
            ]),
        };

        let merged = merge_registries(persistent, &runtime);
        assert_eq!(merged.assignments["serial:A"].loopback_index, 10);
        assert_eq!(merged.assignments["serial:B"].loopback_index, 12);
        assert_eq!(merged.updated_at_ms, 2);
    }
}
