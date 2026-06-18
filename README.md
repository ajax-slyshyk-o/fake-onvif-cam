# fake-onvif-cam

A small cross-platform fake ONVIF camera emulator for testing NVR discovery and RTSP ingestion.

The emulator is written in Rust and uses an external `ffmpeg` binary for media. `ffmpeg` is not bundled with this project.

## What it does

- Responds to ONVIF WS-Discovery `Probe` requests on UDP multicast `239.255.255.250:3702`.
- Serves basic ONVIF Device and Media SOAP endpoints.
- Returns one H.264 RTSP stream URI from `GetStreamUri`.
- Starts `ffmpeg` as an H.264 RTP encoder using either a generated test pattern or a media file.
- Probes media files with `ffprobe` at startup and reports accurate resolution and frame rate in ONVIF metadata.
- Serves RTSP from Rust and relays ffmpeg RTP packets to RTSP clients.
- Restarts `ffmpeg` if the external encoder process exits.
- Serves a tiny JPEG snapshot endpoint for clients that call `GetSnapshotUri`.

This is intentionally an emulator, not a full ONVIF conformance implementation. It is aimed at NVR smoke testing: discovery, metadata lookup, profile enumeration, and RTSP connection.

## Requirements

- Rust toolchain
- `ffmpeg` and `ffprobe` available in `PATH`, or pass `--ffmpeg path/to/ffmpeg` (ffprobe is located in the same directory)

## Run

```powershell
cargo run -- --advertise-host 192.168.1.50
```

Replace `192.168.1.50` with the IP address that your NVR can reach.
Press `Ctrl+C` to stop the emulator and shut down the managed `ffmpeg` encoder processes.

By default the emulator uses:

- ONVIF HTTP: `0.0.0.0:8000`
- RTSP: `rtsp://<advertise-host>:8554/live`
- Local ffmpeg RTP ingest: `127.0.0.1:5004` and RTCP `127.0.0.1:5005`
- Camera name: `Fake ONVIF Camera`
- Overlay text: camera name
- Video: generated 1280×720 test pattern at 25 FPS

## Run with a media file

```powershell
cargo run -- --advertise-host 192.168.1.50 --name "Entrance 01" --file sample.mp4
```

The file is looped through `ffmpeg` and published as the RTSP stream. At startup `ffprobe` reads the file's actual resolution and frame rate, which are then reported in ONVIF metadata so the NVR sees the correct stream parameters.

The camera name is burned into the video by default.

## Scale to configured dimensions

By default, a media file is streamed as-is (pass-through encode, original dimensions).
Add `--scale` (or `scale = true` in TOML) to force the output to the dimensions set by `--width`, `--height`, and `--fps`:

```powershell
cargo run -- --advertise-host 192.168.1.50 --file 4k_clip.mp4 --width 1920 --height 1080 --fps 25 --scale
```

When `--scale` is active, ONVIF metadata advertises the configured target dimensions, not the source file's native dimensions.
When `--scale` is not set, `width`/`height`/`fps` are only used for the generated test pattern; for file input the real dimensions are probed and used.

## Run Multiple Cameras

Create a TOML config with one `[[cameras]]` table per fake camera:

```toml
[defaults]
advertise_host = "192.168.1.50"
ffmpeg = "ffmpeg"
# Fallback media file. A camera can override with its own file entry.
file = "sample.mp4"
fps = 25

[[cameras]]
name = "Entrance 01"
uuid = "00000000-0000-4550-98a8-98a80cab1b88"
http = "0.0.0.0:8000"
rtsp_port = 8554
rtsp_path = "live"
rtp_port = 5004
file = "entrance.mp4"
overlay_text = "Entrance 01"

[[cameras]]
name = "Warehouse 02"
uuid = "00000000-0000-4550-98a8-98a80cab1b89"
http = "0.0.0.0:8001"
rtsp_port = 8555
rtsp_path = "live"
rtp_port = 5010
file = "warehouse.mp4"
overlay_text = "Warehouse 02"
# Force this camera to a specific output resolution
scale = true
width = 1280
height = 720
fps = 15
```

Then run:

```powershell
cargo run -- --config cameras.toml
```

Each camera can set its own `file`. If omitted, it uses `[defaults].file`; if both are omitted, that camera uses the generated test pattern. Each camera needs unique HTTP, RTSP, RTP, and RTCP ports. The RTCP port is always `rtp_port + 1`.

All options available on the command line can also be set in the TOML `[defaults]` block or per `[[cameras]]` entry.

## Useful options

```text
--config <path>           TOML config file with one or more fake cameras.
--advertise-host <host>   Host or IP placed in ONVIF and RTSP URLs.
--http <addr:port>        HTTP bind address. Default: 0.0.0.0:8000
--rtsp-port <port>        RTSP service port. Default: 8554
--rtsp-path <path>        RTSP path. Default: live
--rtp-port <port>         Local RTP ingest port for ffmpeg. Default: 5004
--name <name>             Camera name exposed in scopes and metadata.
--uuid <uuid>             Stable camera UUID. Default: generated at startup.
--ffmpeg <path>           ffmpeg executable. Default: ffmpeg
--file <path>             Loop this media file instead of a generated test pattern.
--width <pixels>          Width for test pattern or scale target. Default: 1280
--height <pixels>         Height for test pattern or scale target. Default: 720
--fps <fps>               Frame rate for test pattern or scale target. Default: 25
--scale                   Scale and re-rate the file stream to --width/--height/--fps.
--overlay-text <text>     Text burned into the video. Default: camera name.
--overlay-font <path>     Font file used for the overlay. Default: a platform system font.
--overlay-font-size <px>  Overlay font size. Default: 32
--no-overlay              Disable video text overlay.
--no-ffmpeg               Start only ONVIF services and do not launch ffmpeg.
--no-discovery            Disable WS-Discovery listener.
```

## Firewall notes

NVRs normally discover ONVIF devices over UDP multicast and then connect back to HTTP and RTSP ports. Allow inbound traffic to:

- UDP `3702`
- TCP `8000`
- TCP `8554`

If discovery does not work, try adding the camera manually in the NVR using:

```text
http://<advertise-host>:8000/onvif/device_service
```

Then confirm the RTSP stream directly:

```powershell
ffplay rtsp://<advertise-host>:8554/live
```
