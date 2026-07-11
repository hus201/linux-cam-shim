use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use cam_shim::{
    activate_hide_rules, camera_identity, clean_loopback_devices, collect_status, create_device,
    default_shim_config, default_udev_rule_path, ensure_module_loaded, format_holder_list,
    hide_camera_now, list_device_holders, list_loopback_devices, print_doctor_report, print_status, probe_device_path,
    restore_all_hidden, run_doctor, run_shim, run_shim_until, run_supervisor, scan_devices,
    standardized_label, stop_cam_shim_processes, visible_capture_path, write_hide_rule_for,
    DoctorConfig, FixSession, ServeConfig, DEFAULT_MAX_CAPTURE_HEIGHT, DEFAULT_MAX_CAPTURE_WIDTH,
    DEFAULT_TARGET_FPS,
};
use cam_shim::unload_loopback_module;

#[derive(Parser)]
#[command(
    name = "cam-shim",
    about = "Linux webcam compatibility shim",
    long_about = "Detects incompatible UVC/V4L2 webcams and exposes a standardized virtual camera."
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List cameras and compatibility status
    Scan {
        /// Emit JSON instead of a human-readable table
        #[arg(long)]
        json: bool,
    },
    /// Create a virtual camera and start the fps shim for one device
    Fix {
        /// Source capture device (e.g. /dev/video2)
        #[arg(long)]
        device: String,

        /// Target fps for the standardized stream
        #[arg(long, default_value_t = DEFAULT_TARGET_FPS)]
        target_fps: u32,

        /// Skip hiding the physical camera
        #[arg(long)]
        no_hide: bool,

        /// Keep the virtual camera after exit (skip auto cleanup)
        #[arg(long)]
        no_cleanup: bool,
    },
    /// Run the capture → resample → loopback pipeline manually
    Relay {
        /// Source capture device
        source: String,

        /// Loopback output device
        target: String,

        #[arg(long, default_value_t = DEFAULT_TARGET_FPS)]
        target_fps: u32,
    },
    /// Run continuously — auto-detect incompatible cameras and start shims
    Serve {
        /// Target fps for standardized streams
        #[arg(long, default_value_t = DEFAULT_TARGET_FPS)]
        target_fps: u32,

        /// Fallback poll interval in seconds (safety net with udev hotplug)
        #[arg(long, default_value = "30")]
        poll_secs: u64,

        /// Do not hide physical camera nodes
        #[arg(long)]
        no_hide: bool,

        /// Disable udev hotplug and scan on a fixed interval only
        #[arg(long)]
        no_hotplug: bool,

        /// Failures before a camera is quarantined
        #[arg(long, default_value_t = 5)]
        max_failures: u32,

        /// Quarantine duration in seconds after repeated failures
        #[arg(long, default_value = "120")]
        quarantine_secs: u64,

        /// Restart backoff base in milliseconds
        #[arg(long, default_value = "1000")]
        backoff_ms: u64,

        /// Kill and restart a worker with no frames for this many seconds
        #[arg(long, default_value = "10")]
        stale_frame_secs: u64,

        /// Log an error if the reconcile loop stalls for this many seconds
        #[arg(long, default_value = "30")]
        watchdog_secs: u64,

        /// Do not write /run/cam-shim/state.json
        #[arg(long)]
        no_state_file: bool,

        /// Keep the physical camera open always (LED stays on even when idle)
        #[arg(long)]
        always_capture: bool,

        /// Maximum negotiated capture width (default: 1920)
        #[arg(long, default_value_t = DEFAULT_MAX_CAPTURE_WIDTH)]
        max_width: u32,

        /// Maximum negotiated capture height (default: 1080)
        #[arg(long, default_value_t = DEFAULT_MAX_CAPTURE_HEIGHT)]
        max_height: u32,
    },
    /// Load v4l2loopback and install udev helper rules (requires root)
    Install {
        /// Path for the generated udev rule
        #[arg(long, default_value = "/etc/udev/rules.d/99-cam-shim.rules")]
        udev_rule: PathBuf,
    },
    /// Show supervisor and camera runtime status (no root required)
    Status {
        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Diagnose and repair common cam-shim / V4L2 issues (requires root for fixes)
    Doctor {
        /// Report only — do not restore, clean, or reload anything
        #[arg(long)]
        check_only: bool,

        /// Unload and reload the v4l2loopback kernel module after cleanup
        #[arg(long)]
        reload: bool,

        /// Stop cam-shim serve/fix/relay before cleanup or module reload
        #[arg(long, short = 'f')]
        force: bool,

        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove virtual cameras and restore hidden physical cameras (requires root)
    Restore {
        /// Also remove orphan loopback devices
        #[arg(long)]
        loopback: bool,
    },
    /// Remove virtual cameras left behind by failed fix attempts (requires root)
    Clean {
        /// Remove every v4l2loopback device, not just cam-shim ones
        #[arg(long)]
        all: bool,

        /// Also remove the cam-shim udev hide rule and restore hidden cameras
        #[arg(long)]
        udev: bool,

        /// Unload and reload the v4l2loopback kernel module (fixes stale state)
        #[arg(long)]
        reload: bool,

        /// Stop cam-shim and other processes holding loopback devices open
        #[arg(long, short = 'f')]
        force: bool,

        /// List loopback devices only, do not remove anything
        #[arg(long)]
        dry_run: bool,

        /// Emit JSON
        #[arg(long)]
        json: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("cam_shim=info".parse()?))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Scan { json } => cmd_scan(json)?,
        Commands::Fix {
            device,
            target_fps,
            no_hide,
            no_cleanup,
        } => cmd_fix(&device, target_fps, no_hide, no_cleanup)?,
        Commands::Relay {
            source,
            target,
            target_fps,
        } => {
            let config = default_shim_config(source, target);
            let config = cam_shim::ShimConfig {
                target_fps,
                pause_when_idle: false,
                ..config
            };
            run_shim(config)?;
        }
        Commands::Serve {
            target_fps,
            poll_secs,
            no_hide,
            no_hotplug,
            max_failures,
            quarantine_secs,
            backoff_ms,
            stale_frame_secs,
            watchdog_secs,
            no_state_file,
            always_capture,
            max_width,
            max_height,
        } => {
            if max_width == 0 || max_height == 0 {
                anyhow::bail!("--max-width and --max-height must be greater than 0");
            }

            let config = ServeConfig {
                target_fps,
                poll_interval: std::time::Duration::from_secs(poll_secs),
                hide_physical: !no_hide,
                max_failures,
                quarantine_duration: std::time::Duration::from_secs(quarantine_secs),
                backoff_base: std::time::Duration::from_millis(backoff_ms),
                max_backoff: std::time::Duration::from_millis(60_000),
                stale_frame_timeout: std::time::Duration::from_secs(stale_frame_secs),
                watchdog_timeout: std::time::Duration::from_secs(watchdog_secs),
                write_state_file: !no_state_file,
                udev_hotplug: !no_hotplug,
                pause_when_idle: !always_capture,
                max_capture_width: max_width,
                max_capture_height: max_height,
            };
            run_supervisor(config)?;
        }
        Commands::Install { udev_rule } => cmd_install(&udev_rule)?,
        Commands::Status { json } => {
            let report = collect_status()?;
            print_status(&report, json)?;
        }
        Commands::Doctor {
            check_only,
            reload,
            force,
            json,
        } => {
            let report = run_doctor(DoctorConfig {
                check_only,
                reload_module: reload,
                force,
                json,
            })?;
            print_doctor_report(&report, json)?;
        }
        Commands::Restore { loopback } => cmd_restore(loopback)?,
        Commands::Clean {
            all,
            udev,
            reload,
            force,
            dry_run,
            json,
        } => cmd_clean(all, udev, reload, force, dry_run, json)?,
    }

    Ok(())
}

fn cmd_scan(json: bool) -> anyhow::Result<()> {
    let devices = scan_devices()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&devices)?);
        return Ok(());
    }

    if devices.is_empty() {
        let hidden = cam_shim::hidden_camera_count().unwrap_or(0);
        let ghosts = cam_shim::ghost_device_count().unwrap_or(0);
        if hidden > 0 {
            println!("No visible V4L2 capture devices found.");
            println!("{hidden} camera node(s) are hidden under /dev/cam-shim-hidden/.");
            println!("Restore with: sudo cam-shim restore");
        } else if ghosts > 0 {
            println!("No V4L2 capture devices found.");
            println!("{ghosts} stale /dev/video* node(s) have no kernel device (ghost nodes).");
            println!("Repair with: sudo cam-shim restore");
        } else {
            println!("No V4L2 capture devices found.");
            println!("If your webcam is plugged in, try: unplug → wait 5s → replug");
            println!("Then run: sudo cam-shim restore --loopback");
        }
        return Ok(());
    }

    for device in devices {
        let status = if device.compatible {
            "compatible"
        } else if device.needs_shim {
            "needs shim"
        } else {
            "unknown"
        };

        println!("{} — {}", device.path, device.name);
        if device.hidden {
            println!("  status: hidden (cam-shim)");
        }
        println!("  driver: {}", device.driver);
        println!("  bus:    {}", device.bus);
        println!("  standardized name: {}", device.standardized_name);
        println!("  status: {status}");

        if !device.advertised_fps.is_empty() {
            println!("  fps:    {}", device.advertised_fps.join(", "));
        }

        for issue in &device.issues {
            println!("  issue:  {issue}");
        }

        println!();
    }

    Ok(())
}

fn cmd_fix(device: &str, target_fps: u32, no_hide: bool, no_cleanup: bool) -> anyhow::Result<()> {
    let report = probe_device_path(device)?;

    if report.compatible {
        println!("{} is already compatible — no shim needed.", device);
        return Ok(());
    }

    let label = standardized_label(&report.name);
    println!("Creating virtual camera: {label}");

    let loopback = create_device(&label, target_fps)?;
    println!("Virtual camera ready at {}", loopback.path);

    let hide_rule = if !no_hide {
        let identity = camera_identity(device)?;
        let rule_path = write_hide_rule_for(&identity)?;
        Some((identity, rule_path))
    } else {
        None
    };

    let mut session = FixSession::new(
        loopback.path.clone(),
        hide_rule.as_ref().map(|(_, path)| path.clone()),
    )?;
    if no_cleanup {
        session.disable_cleanup();
    }

    let source_path = if let Some((identity, _)) = &hide_rule {
        visible_capture_path(identity, device)
    } else {
        device.to_string()
    };

    let config = default_shim_config(source_path.clone(), loopback.path.clone());
    let config = cam_shim::ShimConfig {
        target_fps,
        pause_when_idle: false,
        ..config
    };

    println!(
        "Starting shim {source_path} → {} @ {target_fps} fps (Ctrl+C stops and cleans up)",
        loopback.path
    );

    if let Some((identity, _)) = hide_rule {
        let pick_label = label.clone();
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        {
            let running = running.clone();
            ctrlc::set_handler(move || {
                running.store(false, std::sync::atomic::Ordering::SeqCst);
            })?;
        }

        run_shim_until(
            config,
            running,
            Some(move || {
                activate_hide_rules();
                match hide_camera_now(&identity) {
                    Ok(hidden) if hidden.is_empty() => {
                        println!("Warning: no physical camera nodes were hidden.");
                    }
                    Ok(hidden) => {
                        println!("Hidden physical camera node(s): {}", hidden.join(", "));
                    }
                    Err(err) => eprintln!("Warning: failed to hide physical camera: {err}"),
                }
                println!("Pick \"{pick_label}\" in your application's camera list.");
            }),
            None,
        )?;
    } else {
        run_shim(config)?;
    }
    Ok(())
}

fn cmd_restore(clean_loopback: bool) -> anyhow::Result<()> {
    let report = restore_all_hidden()?;
    println!("Removed cam-shim udev rules.");

    if !report.restored.is_empty() {
        println!("Restored: {}", report.restored.join(", "));
    }
    if !report.ghosts_removed.is_empty() {
        println!("Removed ghost nodes: {}", report.ghosts_removed.join(", "));
    }
    if !report.stale_hidden_removed.is_empty() {
        println!(
            "Removed stale hidden nodes: {}",
            report.stale_hidden_removed.join(", ")
        );
    }

    if report.restored.is_empty()
        && report.ghosts_removed.is_empty()
        && report.stale_hidden_removed.is_empty()
    {
        let hidden = cam_shim::hidden_camera_count()?;
        if hidden > 0 {
            println!("Warning: {hidden} camera node(s) still hidden under /dev/cam-shim-hidden/.");
        } else {
            println!("Device nodes repaired. If the camera still does not appear, unplug and replug it.");
        }
    }

    if clean_loopback {
        let report = clean_loopback_devices(true, true)?;
        for path in &report.removed {
            println!("Removed loopback {path}");
        }
        for failure in &report.failed {
            println!("Failed to remove {}: {}", failure.path, failure.reason);
        }
    }

    Ok(())
}

fn cmd_install(_udev_rule: &PathBuf) -> anyhow::Result<()> {
    ensure_module_loaded()?;
    println!("Loaded v4l2loopback module.");
    println!();
    println!("Run as a continuous daemon:");
    println!("  sudo cam-shim serve");
    println!();
    println!("Or install the systemd unit:");
    println!("  sudo cp packaging/cam-shim.service /etc/systemd/system/");
    println!("  sudo systemctl enable --now cam-shim");
    Ok(())
}

fn cmd_clean(
    all: bool,
    udev: bool,
    reload: bool,
    force: bool,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<()> {
    let rule_path = default_udev_rule_path();
    let devices = list_loopback_devices()?;

    if dry_run {
        if json {
            println!("{}", serde_json::to_string_pretty(&devices)?);
            return Ok(());
        }

        if devices.is_empty() {
            println!("No v4l2loopback devices found.");
        } else {
            println!("v4l2loopback devices:");
            for device in &devices {
                let tag = if cam_shim::is_cam_shim_loopback(&device.name) {
                    "cam-shim"
                } else {
                    "other"
                };
                let holders = list_device_holders(&device.path);
                if holders.is_empty() {
                    println!("  {} — {} [{tag}]", device.path, device.name);
                } else {
                    println!(
                        "  {} — {} [{tag}, held by {}]",
                        device.path,
                        device.name,
                        format_holder_list(&holders)
                    );
                }
            }
        }

        if udev && rule_path.exists() {
            println!("Would remove udev rule at {}", rule_path.display());
        }

        return Ok(());
    }

    let report = clean_loopback_devices(all, force)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if force && !report.force_releases.is_empty() {
            println!("Force-released device holders:");
            for entry in &report.force_releases {
                println!("  {}:", entry.device_path);
                for release in &entry.releases {
                    println!(
                        "    {} ({}) — {}",
                        release.holder.label, release.holder.pid, release.signal
                    );
                }
            }
            println!();
        }

        for path in &report.removed {
            println!("Removed {path}");
        }
        for failure in &report.failed {
            println!("Failed to remove {}: {}", failure.path, failure.reason);
            if !failure.holders.is_empty() {
                println!("  still held by: {}", format_holder_list(&failure.holders));
            }
        }
        for path in &report.skipped {
            println!("Skipped {path}");
        }

        if report.removed.is_empty() && report.failed.is_empty() {
            println!("No matching loopback devices to remove.");
        }
    }

    if udev {
        restore_all_hidden()?;
        if !json {
            println!("Removed udev rules and restored hidden cameras.");
        }
    }

    if reload {
        if force {
            stop_cam_shim_processes();
        }

        match unload_loopback_module() {
            Ok(()) => {
                ensure_module_loaded()?;
                if !json {
                    println!("Reloaded v4l2loopback kernel module.");
                }
            }
            Err(err) if force => {
                if !json {
                    eprintln!("Warning: module reload failed after force cleanup: {err}");
                }
            }
            Err(err) => return Err(err.into()),
        }
    }

    Ok(())
}
