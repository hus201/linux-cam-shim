pub mod compat;
pub mod doctor;
pub mod error;
pub mod hide;
pub mod hotplug;
pub mod loopback;
pub mod loopback_output;
pub mod probe;
pub mod runtime;
pub mod serve;
pub mod session;
pub mod shim;
pub mod status;

pub use compat::{
    kernel_card_label, kernel_card_label_bytes, loopback_fps_from_intervals, standardized_label,
    CompatReport, CompatStatus, DEFAULT_MAX_CAPTURE_HEIGHT, DEFAULT_MAX_CAPTURE_WIDTH,
    DEFAULT_TARGET_FPS,
};
pub use doctor::{print_doctor_report, run_doctor, DoctorConfig, DoctorReport};
pub use error::{CamShimError, Result};
pub use hide::{
    activate_hide_rules, camera_identity, camera_serial_present, default_udev_rule_path,
    ghost_device_count, hidden_camera_count, hidden_video_names, hide_camera_now,
    install_hide_rule, install_hide_rule_for, remove_all_hide_rules, repair_video_devices,
    resolve_device_path, restore_hidden_cameras, teardown_hide, udev_rule_path_for_serial,
    visible_capture_path, write_hide_rule_for, CameraIdentity, RestoreReport,
};
pub use loopback::{
    clean_loopback_devices, create_device, ensure_module_loaded, find_device_holders,
    format_holder_list, is_cam_shim_loopback, list_device_holders, list_loopback_devices,
    stop_cam_shim_processes, unload_loopback_module, CleanReport, DeviceHolder, LoopbackDeviceInfo,
};
pub use probe::{probe_device_path, scan_devices, DeviceReport};
pub use serve::{run_supervisor, ServeConfig};
pub use session::{remove_udev_hide_rule, restore_all_hidden, FixSession};
pub use shim::{default_shim_config, run_shim, run_shim_until, ShimConfig};
pub use status::{collect_status, print_status, StatusReport};
