pub mod camera_view;
pub mod compat;
pub mod device_registry;
pub mod devices;
pub mod doctor;
pub mod error;
pub mod hotplug;
pub mod loopback;
pub mod loopback_output;
pub mod probe;
pub mod runtime;
pub mod serve;
pub mod session;
pub mod shim;
pub mod status;

pub use camera_view::{
    collect_scan_report, device_views_from_snapshot, format_device_line, role_label, DeviceRole,
    DeviceView, RecommendedDevice, ScanReport,
};
pub use compat::{
    kernel_card_label, kernel_card_label_bytes, loopback_fps_from_intervals,
    native_capture_fps_from_intervals, standardized_label, CompatReport, CompatStatus,
    DEFAULT_MAX_CAPTURE_HEIGHT, DEFAULT_MAX_CAPTURE_WIDTH, DEFAULT_TARGET_FPS,
};
pub use devices::{
    camera_identity, camera_serial_present, device_id_serial, ghost_device_count,
    physical_camera_key, physical_camera_key_with_name, repair_video_devices, CameraIdentity,
    RepairReport,
};
pub use device_registry::{
    assign_loopback_index, lookup_loopback_index, read_device_registry, DeviceRegistry,
    LoopbackAssignment, DEVICES_FILE, PERSISTENT_DEVICES_FILE, RUNTIME_DEVICES_FILE,
};
pub use doctor::{print_doctor_report, run_doctor, DoctorConfig, DoctorReport};
pub use error::{CamShimError, Result};
pub use loopback::{
    clean_loopback_devices, create_device, create_device_with_options, ensure_module_loaded,
    find_device_holders, format_holder_list, is_cam_shim_loopback, list_device_holders,
    list_loopback_devices, stop_cam_shim_processes, unload_loopback_module, CleanReport,
    CreateDeviceOptions, DeviceHolder, LoopbackDeviceInfo,
};
pub use probe::{probe_device_path, scan_devices, DeviceReport};
pub use serve::{run_supervisor, ServeConfig};
pub use session::{repair_devices, FixSession};
pub use shim::{default_shim_config, run_shim, run_shim_until, ShimConfig};
pub use status::{collect_status, print_status, StatusReport};
