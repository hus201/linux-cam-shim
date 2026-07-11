# linux-cam-shim

Linux webcam compatibility shim. Detects UVC/V4L2 cameras that advertise non-standard frame rates (e.g. 25 fps only) and exposes a virtual **Linux Standardized** camera via [v4l2loopback](https://github.com/umlaeute/v4l2loopback).

## Naming

| Thing | Name |
|-------|------|
| Project / repo | `linux-cam-shim` |
| CLI binary | `cam-shim` |
| Virtual device label | `{Original Name} - Linux Standardized` |

## Problem

Some inexpensive UVC webcams advertise frame rates like **25 fps** (common in PAL regions). Browsers and WebRTC apps often expect **30/60 fps** and may crash or fail negotiation. A virtual camera with normalized frame rate fixes this.

## Requirements

- Linux with V4L2
- Rust toolchain
- [`v4l2loopback`](https://github.com/umlaeute/v4l2loopback) kernel module (for `fix` / `serve`)
- Root for `fix`, `serve`, and `install` (loopback + udev)
- Optional: `v4l2loopback-utils` (`v4l2loopback-ctl`) — not required on module ≥ 0.15 with `/dev/v4l2loopback`

## Build

```bash
cargo build --release
```

Binary: `target/release/cam-shim`

### Build a `.deb` package

```bash
./packaging/build-deb.sh
```

Install:

```bash
sudo dpkg -i target/debian/cam-shim_*_amd64.deb
sudo systemctl enable --now cam-shim
```

**v4l2loopback:** Your kernel may already include it (`modprobe v4l2loopback`). Do **not** install `v4l2loopback-dkms` from Ubuntu on kernel 7.x unless you need it — it often fails to build and is unnecessary when the in-tree module is present.

Requires `cargo-deb` (the build script installs it automatically if missing).

## Usage

### Scan cameras (no root)

```bash
cam-shim scan
cam-shim scan --json
```

### Run continuously (recommended)

The **`serve`** command runs as a supervisor: it uses **udev hotplug** for instant camera detection, with a periodic fallback poll as a safety net.

```bash
sudo cam-shim serve
sudo cam-shim serve --target-fps 30 --poll-secs 30
sudo cam-shim serve --no-hide   # keep physical camera visible
sudo cam-shim serve --no-hotplug --poll-secs 2   # polling only
sudo cam-shim serve --always-capture            # keep physical camera LED on when idle
sudo cam-shim serve --max-width 1280 --max-height 720   # cap UVC negotiation
```

The supervisor includes:

- **Udev hotplug** — reacts immediately when cameras are plugged or unplugged
- **Fallback poll** — safety reconcile every 30s if an event is missed
- **Idle pause** — physical camera LED off when no app is using the virtual camera

- **Startup self-check** — repair ghost nodes, restore hidden cameras, remove orphan loopbacks
- **Worker health** — restarts shims that stop producing frames
- **Exponential backoff** — avoids crash/restart storms after failures
- **Circuit breaker** — quarantines a camera after 5 failures (120s default)
- **Watchdog** — logs if the reconcile loop stalls
- **State file** — `/run/cam-shim/state.json` for observability

Tuning flags: `--max-failures`, `--quarantine-secs`, `--backoff-ms`, `--stale-frame-secs`, `--watchdog-secs`, `--no-state-file`, `--max-width`, `--max-height`

**Install as a systemd service** (starts on boot):

```bash
./packaging/build-deb.sh
sudo dpkg -i target/debian/cam-shim_*_amd64.deb
sudo systemctl enable --now cam-shim
sudo systemctl status cam-shim
journalctl -u cam-shim -f
```

Or install manually without the `.deb`:

```bash
sudo cam-shim install
sudo cp packaging/cam-shim.service /etc/systemd/system/
sudo systemctl enable --now cam-shim
```

When a compatible camera is plugged in, pick **`Your Camera - Linux Standardized`** in your app's camera list.

### Fix one camera manually

Creates a virtual device, hides **all nodes** for that USB camera (e.g. `/dev/video0` and `/dev/video3`), and runs the fps shim. **Ctrl+C** removes the virtual camera and restores the physical one.

```bash
sudo cam-shim fix --device /dev/video2
sudo cam-shim fix --device /dev/video2 --target-fps 30
sudo cam-shim fix --device /dev/video2 --no-cleanup   # keep virtual cam after exit
```

Resolution is chosen automatically at the **highest MJPEG size at or below 1920×1080** (capped to reduce driver quirks on cheap UVC cams).

### Check runtime status

```bash
cam-shim status
cam-shim status --json
```

Shows whether `serve` is running, managed cameras, loopback paths, heartbeats, and quarantined serials. No root required.

### Diagnose and repair

One-shot health check and recovery:

```bash
sudo cam-shim doctor              # restore hidden, clean orphans, ensure module
sudo cam-shim doctor --check-only # report only, no changes
sudo cam-shim doctor --force --reload   # stop daemons, clean, reload module
```

### Clean up failed fix attempts

Remove orphan virtual cameras created by earlier failed `fix` runs:

```bash
sudo cam-shim clean
sudo cam-shim clean --dry-run    # preview only
sudo cam-shim clean --all        # remove every loopback device
sudo cam-shim clean --udev       # also remove udev hide rules
sudo cam-shim clean --reload     # unload/reload module (after manual /dev edits or stale ghosts)
sudo cam-shim clean --force      # stop cam-shim/other apps holding the virtual camera, then remove
sudo cam-shim clean --force --reload
```

Do **not** delete `/dev/video*` files by hand — that leaves ghost sysfs entries. Use `cam-shim clean` instead.

### Restore hidden cameras

If `scan` says no devices but your webcam is plugged in, it may be hidden from a previous `fix` or `serve`:

```bash
sudo cam-shim restore                 # move cameras back from /dev/cam-shim-hidden/
sudo cam-shim restore --loopback      # also remove leftover virtual cameras
```

### Run shim only (manual loopback)

If you already have a loopback device:

```bash
sudo cam-shim relay /dev/video2 /dev/video10 --target-fps 30
```

### Install helpers

```bash
sudo cam-shim install
```

## How it works

1. **Scan** — enumerate V4L2 devices via sysfs, query formats and frame intervals.
2. **Compat check** — flag devices missing 30/60 fps or reporting variable frame rate.
3. **Shim** — capture MJPEG from the physical device, duplicate/drop frames to target fps, write to v4l2loopback.
4. **Hide** — per-camera udev rules move physical nodes to `/dev/cam-shim-hidden/` so apps prefer the standardized virtual camera.
5. **Serve** — supervisor loop watches for plug/unplug and manages shims automatically.

## Troubleshooting

### Quick checks

```bash
cam-shim scan                     # list visible cameras and compat status
cam-shim status                   # serve, loopbacks, heartbeats, quarantined serials
sudo cam-shim doctor --check-only # full system report without changes
journalctl -u cam-shim -f         # if running via systemd
```

Enable verbose shim logs when debugging stream errors:

```bash
RUST_LOG=cam_shim::shim=debug sudo cam-shim serve
```

### One-shot recovery

When things are in a bad state (hidden camera, orphan loopback, stale module):

```bash
# Close Discord, guvcview, and other apps using the virtual camera first.
sudo cam-shim doctor --force --reload
sudo systemctl restart cam-shim    # if installed as a service
```

Or step by step:

```bash
sudo cam-shim restore --loopback   # unhide physical nodes, remove orphan loopbacks
sudo cam-shim clean --force        # if loopbacks are still held open
sudo cam-shim clean --force --reload
sudo cam-shim serve                # or: sudo systemctl start cam-shim
```

### Common symptoms

| Symptom | Likely cause | What to do |
|--------|----------------|------------|
| `scan` finds no cameras | Physical nodes hidden | `sudo cam-shim restore` |
| Virtual cam missing in app list | Loopback not primed yet, or module not loaded | Wait ~1s after plug-in; `sudo modprobe v4l2loopback exclusive_caps=1`; check `cam-shim status` |
| Only the physical camera shows up | `serve` not running, or camera is compatible | `cam-shim scan`; start `sudo cam-shim serve` |
| Camera works once, fails on reopen | App left loopback open, or pause/resume bug | Close the app fully; run soak test (below); check logs for `EINVAL` |
| `clean` does nothing / device busy | Discord, guvcview, or browser holding `/dev/video*` | Close those apps, or `sudo cam-shim clean --force` |
| Ghost `/dev/video* (deleted)` nodes | Manual deletion of device nodes | `sudo cam-shim restore`; never delete `/dev/video*` by hand |
| Physical LED stays on when idle | App still reading the virtual cam | Check `cam-shim status` for active readers; close the app |
| Camera quarantined after failures | Repeated shim crashes | `cam-shim status` for quarantined serials; fix underlying error, wait 120s, or restart serve |
| `v4l2loopback-dkms` build fails on kernel 7.x | DKMS package not needed | Use the in-tree module: `modprobe v4l2loopback`; do not install `v4l2loopback-dkms` |

### App notes

- **Discord / Chrome / Firefox** — pick **`Your Camera - Linux Standardized`**, not the raw physical device. The first open after plug-in may take about a second while the loopback producer attaches.
- **guvcview** — close it before `cam-shim clean`; it often keeps loopback nodes open after the window closes.
- **Close apps before cleanup** — `clean --force` can terminate holders, but graceful close avoids stale buffers and EINVAL on the next open.

### Regression testing

After code changes, run the lifecycle soak test (requires `v4l-utils`, `python3`, and a compatible webcam):

```bash
cargo build --release
sudo cam-shim serve &   # or use systemd
./scripts/soak.sh --iterations 50

# Or let the script start serve:
sudo ./scripts/soak.sh --start-serve --iterations 100
```

The script repeatedly opens and closes the virtual camera to exercise pause/resume and catch EINVAL or worker crashes.

## Project status

`scan`, `fix`, and `serve` are usable with v4l2loopback and root. `serve` + systemd is the intended always-on workflow.

## License

MIT
