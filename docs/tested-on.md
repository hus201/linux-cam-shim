# Tested on

Hardware validation performed so far. Your mileage may vary on other cameras, kernels, and desktop apps.

## Fantech Luminous C30 (2026-07-22)

| Item | Details |
|------|---------|
| **Camera** | Fantech Luminous C30 (`usb:1d6d:0105`, UVC) |
| **Host** | Linux 7.0.0-28-generic, x86_64 |
| **Capture** | MJPEG 1920×1080 @ ~25 fps (native; camera advertises 20/25 fps) |
| **Loopback** | v4l2loopback in-tree, `exclusive_caps=1`, `/dev/video10` |
| **Virtual name** | `Fantech Luminous C30 - Shim` (validated before rename; legacy label was `… - Linux Std`) |
| **Soak test** | `sudo ./scripts/soak.sh --start-serve --iterations 5 --hold-secs 2` — **PASS** |
| **systemd** | `cam-shim.service` enabled after soak pass |

### Soak notes

- Steady ~25 fps on `/dev/video10` across 5 open/close cycles
- Brief `dropped buffers` on cycles 2–3 after reopen (consumer catch-up); cleared by cycle 4
- No stale heartbeats or worker crashes

### Commands used

```bash
cargo build --release
sudo ./scripts/soak.sh --start-serve \
  --cam-shim ./target/release/cam-shim \
  --iterations 5 --hold-secs 2 --verbose
sudo install -m 755 target/release/cam-shim /usr/bin/cam-shim
sudo systemctl enable --now cam-shim
```

## Contributing a report

If you validate cam-shim on your hardware, add a dated subsection here with camera model, kernel, capture format/fps, soak result, and any app notes (Discord, OBS, etc.).
