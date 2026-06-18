# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**fake-onvif-cam** is a Rust-based ONVIF camera emulator for testing NVR (Network Video Recorder) discovery and RTSP ingestion. It simulates real ONVIF cameras by serving SOAP endpoints, responding to WS-Discovery probes, and publishing H.264 streams via ffmpeg → RTP → RTSP.

## Commands

```powershell
# Build
cargo build
cargo build --release

# Run (single camera, auto-detects host IP)
cargo run -- --advertise-host 192.168.1.50

# Run from config file (multi-camera)
cargo run -- --config cameras.toml

# Run without ffmpeg (ONVIF endpoints only, no video)
cargo run -- --advertise-host 192.168.1.50 --no-ffmpeg

# Tests
cargo test

# Lint
cargo clippy
```

There is no test suite yet — `cargo test` runs only doctests.

## Architecture

The app is single-binary with no async runtime. All concurrency is done with OS threads and `Arc<Mutex<>>`.

**Entry point:** `main.rs` — loads config, spawns one set of threads per camera, then blocks waiting for Ctrl+C.

**Per-camera thread topology:**
```
main thread
├── discovery thread (shared, UDP multicast 239.255.255.250:3702)
└── per camera:
    ├── HTTP thread     — ONVIF SOAP on TCP (default :8000)
    ├── RTSP accept     — clients connect on TCP (default :8554)
    │   └── per client thread
    ├── RTP relay       — receives UDP from ffmpeg (:5004), broadcasts to RTSP clients
    ├── RTCP relay      — same for RTCP (:5005)
    └── ffmpeg process  — encodes video → RTP, supervised with auto-restart
```

**Module responsibilities:**

| File | Responsibility |
|------|----------------|
| `config.rs` | CLI (`clap`) + TOML parsing, fleet validation, host auto-detection |
| `discovery.rs` | WS-Discovery: parse UDP Probe, reply with ProbeMatch SOAP |
| `onvif.rs` | All ONVIF SOAP XML generation (17+ operations for Device + Media services) |
| `http.rs` | Minimal HTTP server: routes `/`, `/snapshot.jpg`, `/onvif/device_service`, `/onvif/media_service` |
| `rtsp.rs` | Custom RTSP server + RTP/RTCP relay; supports both TCP-interleaved and UDP-unicast transport |
| `ffmpeg.rs` | Spawns/supervises ffmpeg subprocess; builds drawtext filter for overlay; handles platform fonts |
| `util.rs` | UUID v4 generation, XML escaping, UTC timestamp helpers |

## Key Design Decisions

- **No async.** Each connection/service gets its own OS thread. The RTSP client list (`Arc<Mutex<Vec<Client>>>`) is shared across the relay and accept threads.
- **No streaming library.** RTP/RTCP framing is hand-rolled in `rtsp.rs`.
- **ffmpeg as encoder.** Video never passes through Rust — ffmpeg writes RTP packets to a local UDP socket, the relay picks them up and forwards to RTSP clients.
- **No ONVIF authentication.** All SOAP endpoints are unauthenticated (by design for testing).
- **Snapshot is a stub.** `/snapshot.jpg` returns a hardcoded 1×1 JPEG byte array, not a real frame.

## Configuration File (cameras.toml)

```toml
[defaults]
advertise_host = "10.10.26.172"
ffmpeg = "d:/bin/ffmpeg.exe"   # absolute path or omit to use PATH
file = "d:/video/clip.mp4"     # omit for test pattern (testsrc2)
width = 3840
height = 2160
fps = 25

[[cameras]]
name = "Fake Camera 01"
uuid = "00000000-0000-4550-98a8-98a80cab1b88"  # stable UUID for NVR persistence
http = "0.0.0.0:8000"
rtsp_port = 8554
rtp_port = 5004   # RTCP uses rtp_port+1 automatically

[[cameras]]
name = "Fake Camera 02"
uuid = "00000000-0000-4550-98a8-98a80cab1b89"
http = "0.0.0.0:8001"
rtsp_port = 8555
rtp_port = 5006
```

Fleet validation rejects duplicate ports or UUIDs across cameras.

## Runtime Requirements

- `ffmpeg` in PATH (or set `--ffmpeg <path>` / `ffmpeg` key in TOML)
- Firewall: inbound UDP 3702 (WS-Discovery), inbound TCP per camera for HTTP and RTSP

## Version

Version is embedded at build time from `git describe` via the `git-version` crate (see `build.rs`). Release builds use `-C opt-level=z`, LTO, and stripped debug info.
