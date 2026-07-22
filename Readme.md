# linux-cam-shim

**Research prototype — [linux consumer labs](https://github.com/linux-consumer-labs)**

A userland compatibility layer for UVC webcams that break browsers and video calls on Linux.

> **Early stage (v0.3)** — Core relay, hotplug, scan/status UX, stable loopback indices, and YUYV/uncompressed capture are in place, but this is **not** a stability guarantee yet. `serve` and `fix` require **root** and load kernel modules. Compatibility varies by camera, kernel, and desktop apps. Test on a non-critical system first; keep `cam-shim restore` and `cam-shim doctor` handy if something goes wrong.

## Problem

Everyday users expect a webcam to work when they plug it in. On Linux, some inexpensive UVC devices advertise frame rates like **25 fps** (common in PAL regions) while browsers and WebRTC stacks often expect **30/60 fps**. Negotiation fails, apps crash, or the call starts without video — and the fix usually means understanding V4L2, loopback modules, frame-rate metadata, and which `/dev/video*` node each app actually opened.

That is consumer friction: the hardware works, the kernel sees it, but the stack between plug-in and Google Meet does not bridge the gap predictably.

## Others

Other operating systems treat this as a platform problem, not a user homework assignment.

**macOS** enforces compatibility upstream in the stack Apple controls. Built-in cameras and commonly used externals are validated so frame rates and formats are predictable before FaceTime, Chrome, or Zoom open a picker — a "25 fps only" UVC quirk rarely becomes an app-level failure for typical Mac users. Generic USB webcams can still work, but the failure mode this repo targets is handled before apps negotiate. The invariant is *curated hardware and stack*, not a userland shim.

**Windows** routes almost all camera access through **Windows Media Foundation (WMF)** — a user-mode media pipeline between the kernel driver and applications. Apps negotiate with WMF's advertised capabilities, not raw USB descriptors. Under the hood, a typical UVC path looks like this:

1. The inbox **USB Video Class (UVC) driver** talks to the hardware and exposes raw streams.
2. **DevProxy** marshals frames and commands from the kernel driver into user mode.
3. Optional **Device MFTs** (Media Foundation Transforms) — vendor or inbox "Platform DMFT" plugins — can adjust formats, frame rates, and processing before apps see data.
4. The **Device Transform Manager (DTM)** inside the media source handles **media-type negotiation** (resolution, pixel format, fps) across that chain.
5. Applications consume frames through **Source Reader**, WinRT **`MediaCapture`**, or legacy DirectShow — all backed by the same WMF media source.

So even when the sensor only outputs 25 fps, the OS has a defined place to normalize or translate what apps are offered. Chrome and Teams do not each re-implement UVC parsing and hope for the best.

**Linux workarounds** — there is no equivalent platform layer. The kernel's UVC driver exposes what the hardware advertises; PipeWire, browsers, and desktop apps negotiate directly against `/dev/video*`. When that breaks, users patch it themselves:

- **`v4l2loopback` + `ffmpeg` / GStreamer** — load the loopback module, capture from the physical camera, re-encode or retime frames, write to a virtual `/dev/videoN`. Works, but you maintain the pipeline, module options, and device paths by hand.
- **OBS Virtual Camera** — point OBS at the physical webcam, enable its virtual output, select "OBS Virtual Camera" in the meeting app. Common and GUI-friendly, but heavy (full broadcast stack for a compatibility fix) and easy to leave running with wrong settings.
- **Pick a different app or browser** — sometimes one stack tolerates 25 fps while another crashes; inconsistent and not a fix for the household.
- **Replace the hardware** — buy a webcam that advertises 30/60 fps. Reliable, but punishes the user for a software negotiation gap.

These are all valid — and all fragile: nothing detects incompatible cameras on plug-in, nothing pairs a physical device with its virtual stand-in, and recovery after a failed `fix` or ghost `/dev/video*` node is still terminal-first. That gap is what motivated this prototype.

## What-if

*What if Linux could close that gap with a small userland layer instead of asking every user to become a V4L2 debugger?*

This prototype tests that hypothesis on top of V4L2 and [v4l2loopback](https://github.com/umlaeute/v4l2loopback) — no kernel fork. See [How it works](#how-it-works) for scan, shim, and hotplug management. Limits today: root, hardware variance, early-stage stability. This repo collects the evidence.

## Quick reference

| Thing | Name |
|-------|------|
| Project / repo | `linux-cam-shim` |
| CLI binary | `cam-shim` |
| Virtual device label | `{Original Name} - Shim` |

## Requirements

- Linux with V4L2
- Rust toolchain
- [`v4l2loopback`](https://github.com/umlaeute/v4l2loopback) kernel module (for `fix` / `serve`)
- Root for `fix`, `serve`, and `install` (loopback module)
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
sudo cam-shim serve
```

The `.deb` installs a systemd unit but does **not** enable or start it. Use `sudo cam-shim serve` first to validate your camera; enable the service only once that looks solid:

```bash
sudo systemctl enable --now cam-shim
```

**v4l2loopback:** Your kernel may already include it (`modprobe v4l2loopback`). Do **not** install `v4l2loopback-dkms` from Ubuntu on kernel 7.x unless you need it — it often fails to build and is unnecessary when the in-tree module is present.

Requires `cargo-deb` (the build script installs it automatically if missing).

## Usage

### Scan cameras (no root)

Lists physical and virtual cameras with roles and pairing hints:

```bash
cam-shim scan
cam-shim scan --json
```

Example human output:

```text
/dev/video0  Fantech C30  [physical, needs shim]
  paired with: /dev/video10
  expected virtual name: Fantech C30 - Shim

/dev/video10  Fantech C30 - Shim  [virtual, use this]
  paired with: /dev/video0
```

When `serve` is running, JSON includes `recommended_devices` — pick those in your app, **not** the physical device.

### Run continuously (recommended)

The **`serve`** command runs as a supervisor: **netlink hotplug** for instant camera detection, with a periodic fallback poll as a safety net.

```bash
sudo cam-shim serve
sudo cam-shim serve --target-fps 30
sudo cam-shim serve --no-hotplug --poll-secs 2   # polling only
sudo cam-shim serve --max-width 1280 --max-height 720   # cap UVC negotiation
```

The supervisor includes:

- **Netlink hotplug** — reacts within ~200ms when cameras are plugged or unplugged
- **Hotplug settle** — retries discovery for up to 2s after plug-in while sysfs/V4L2 comes up
- **Fallback poll** — safety reconcile every 5s by default if an event is missed (`--poll-secs` to tune)
- **Always capture** — physical camera stays open while a shim worker is running
- **Direct relay** — capture frames forwarded to the loopback at the camera's native rate; loopback fps metadata set to `target_fps`
- **Stable loopback index** — same USB camera keeps the same `/dev/video10+` across replug and reboot (`/var/lib/cam-shim/devices.json`)
- **Startup self-check** — repair ghost nodes and remove orphan loopbacks
- **Worker health** — restarts shims that stop producing frames
- **Exponential backoff** — avoids crash/restart storms after failures
- **Circuit breaker** — quarantines a camera after 5 failures (120s default)
- **Watchdog** — logs if the reconcile loop stalls
- **State file** — `/run/cam-shim/state.json` for observability (runtime only)

Tuning flags: `--no-hotplug`, `--max-failures`, `--quarantine-secs`, `--backoff-ms`, `--stale-frame-secs`, `--watchdog-secs`, `--no-state-file`, `--max-width`, `--max-height`

**Optional: systemd** (unit ships with the `.deb`, disabled by default — enable only after `serve` works for you):

```bash
sudo systemctl enable --now cam-shim
sudo systemctl status cam-shim
journalctl -u cam-shim -f
```

Or copy the unit manually without the `.deb`:

```bash
sudo cam-shim install
sudo cp packaging/cam-shim.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now cam-shim
```

When a compatible camera is plugged in, pick **`Your Camera - Shim`** in your app's camera list (not the raw physical device).

### Fix one camera manually

Creates a virtual device and runs the fps shim. **Ctrl+C** removes the virtual camera.

```bash
sudo cam-shim fix --device /dev/video2
sudo cam-shim fix --device /dev/video2 --target-fps 30
sudo cam-shim fix --device /dev/video2 --no-cleanup   # keep virtual cam after exit
```

Resolution is chosen automatically at the **highest mode at or below 1920×1080** — MJPEG when available, otherwise YUYV or other uncompressed formats.

### Check runtime status

```bash
cam-shim status
cam-shim status --json
```

Shows whether `serve` is running, managed cameras, loopback paths, heartbeats, quarantined serials, and a unified camera list with pairing. No root required.

### Diagnose and repair

One-shot health check and recovery:

```bash
sudo cam-shim doctor              # repair ghost nodes, clean orphans, ensure module
sudo cam-shim doctor --check-only # report only, no changes
sudo cam-shim doctor --force --reload   # stop daemons, clean, reload module
```

### Clean up failed fix attempts

Remove orphan virtual cameras created by earlier failed `fix` runs:

```bash
sudo cam-shim clean
sudo cam-shim clean --dry-run    # preview only
sudo cam-shim clean --all        # remove every loopback device
sudo cam-shim clean --reload     # unload/reload module (after manual /dev edits or stale ghosts)
sudo cam-shim clean --force      # stop cam-shim/other apps holding the virtual camera, then remove
sudo cam-shim clean --force --reload
```

Do **not** delete `/dev/video*` files by hand — that leaves ghost sysfs entries. Use `cam-shim clean` instead.

### Restore / repair device nodes

Repair stale ghost `/dev/video*` nodes and optionally remove orphan loopbacks:

```bash
sudo cam-shim restore                 # remove ghost device nodes
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

1. **Scan** — enumerate physical and virtual V4L2 devices; flag compat issues; pair physical cameras with their standardized loopback.
2. **Compat check** — flag devices missing 30/60 fps or reporting variable frame rate.
3. **Shim** — capture MJPEG or uncompressed YUV (YUYV, NV12, …) from the physical device at native rate; relay frames to v4l2loopback mmap output with `target_fps` metadata on the virtual device.
4. **Serve** — supervisor reacts to hotplug (with settle retry) and manages shims automatically.

Both the physical camera and the virtual **Shim** device stay visible. Pick the Shim device in your app.

Virtual cameras are created at `/dev/video10+` when possible so low numbers stay available for physical webcams. The same camera gets the same loopback index every time via `/var/lib/cam-shim/devices.json`. `clean` / `restore --loopback` only remove cam-shim devices — other apps' virtual cameras (OBS, etc.) are left alone unless you pass `clean --all`.

## Troubleshooting

### Quick checks

```bash
cam-shim scan                     # cameras, roles, recommended virtual device
cam-shim status                   # serve, loopbacks, heartbeats, quarantined serials
sudo cam-shim doctor --check-only # full system report without changes
journalctl -u cam-shim -f         # if running via systemd
```

Enable verbose shim logs when debugging stream errors:

```bash
RUST_LOG=cam_shim::shim=debug sudo cam-shim serve
```

### One-shot recovery

When things are in a bad state (orphan loopback, stale module, ghost nodes):

```bash
# Close Discord, guvcview, and other apps using the virtual camera first.
sudo cam-shim doctor --force --reload
sudo systemctl restart cam-shim    # if installed as a service
```

Or step by step:

```bash
sudo cam-shim restore --loopback   # remove leftover loopbacks first, then repair ghost nodes
sudo cam-shim clean --force        # if loopbacks are still held open
sudo cam-shim clean --force --reload
sudo cam-shim serve                # or: sudo systemctl start cam-shim
```

### Common symptoms

| Symptom | Likely cause | What to do |
|--------|----------------|------------|
| `scan` finds no cameras | Ghost nodes or unplugged camera | `sudo cam-shim restore` |
| Virtual cam missing in app list | Loopback not primed yet, or module not loaded | Wait ~1s after plug-in; `sudo cam-shim install` or start `serve`; check `cam-shim status` |
| Only the physical camera shows up | `serve` not running, or camera is compatible | `cam-shim scan`; start `sudo cam-shim serve` |
| Plug-in not detected for ~2s | Hotplug settle or slow sysfs | Normal — settle retries up to 2s; fallback poll every 5s |
| Camera works once, fails on reopen | App left loopback open | Close the app fully; run soak test (below); check logs for `EINVAL` |
| Virtual cam moved to a new `/dev/videoN` | Registry lost or first sighting | Should stabilize after first shim; check `/var/lib/cam-shim/devices.json` |
| Physical camera moved off `/dev/video0` | Another virtual cam claimed a low number while the camera was unplugged | Unplug/replug the camera; cam-shim uses `video10+` so it does not take low numbers. Do not use `clean --all` unless you intend to remove other apps' virtual cams |
| `clean` skips a device / busy | Not a cam-shim device, or an app holds our virtual cam | Expected for OBS/others; for ours: `sudo cam-shim clean --force` or `--force --reload` |
| Ghost `/dev/video* (deleted)` nodes | Manual deletion of device nodes | `sudo cam-shim restore`; never delete `/dev/video*` by hand |
| Physical LED stays on | `serve` keeps the camera open while a shim is running | Expected; stop `serve` or unplug the camera to turn it off |
| Camera quarantined after failures | Repeated shim crashes | `cam-shim status` for quarantined serials; fix underlying error, wait 120s, or restart serve |
| `v4l2loopback-dkms` build fails on kernel 7.x | DKMS package not needed | Use the in-tree module: `modprobe v4l2loopback`; do not install `v4l2loopback-dkms` |

### App notes

- **Discord / Chrome / Firefox** — pick **`Your Camera - Shim`**, not the raw physical device. The first open after plug-in may take about a second while the loopback producer attaches.
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

The script repeatedly opens and closes the virtual camera to catch EINVAL or worker crashes on reopen.

See [docs/tested-on.md](docs/tested-on.md) for hardware validation reports.

## Project status

**Early stage (v0.3)** — working toward a reliable “plug in → run serve → pick `{camera} - Shim`” flow. Not ready to call stable yet; run the soak test on your hardware before trusting it daily.

### v0.3 highlights

- Scan/status UX — physical vs virtual pairing, `recommended_devices`
- Stable loopback index — `/var/lib/cam-shim/devices.json` (survives reboot)
- Direct capture→loopback relay + loopback fps metadata at `target_fps`
- Netlink hotplug with 2s settle retry
- YUYV / NV12 / uncompressed relay when MJPEG is unavailable

| Area | Status |
|------|--------|
| UVC scan + compat detection | Done |
| MJPEG + YUYV/uncompressed relay | Done |
| Netlink hotplug + settle retry + fallback poll | Done |
| Scan/status UX (physical vs virtual, recommendations) | Done |
| Stable loopback index (`/var/lib/cam-shim/devices.json`) | Done |
| Always-on capture (no idle pause) | Done |
| PipeWire / Flatpak portal polish | Not yet |
| Rootless operation | Not yet |
| Soak test in CI | Manual (`scripts/soak.sh`) |

The systemd unit ships with the `.deb` but stays **disabled by default** until you validate your camera with `sudo cam-shim serve`.

## License

All [linux consumer labs](https://github.com/linux-consumer-labs) repositories are licensed under the [MIT License](LICENSE).
