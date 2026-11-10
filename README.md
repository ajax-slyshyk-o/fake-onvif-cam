# fake-onvif-cam

A small cross-platform fake ONVIF camera emulator for testing NVR discovery and RTSP ingestion.

The emulator is written in Rust and uses an external `ffmpeg` binary for media. `ffmpeg` is not bundled with this project.

## What it does

- Responds to ONVIF WS-Discovery `Probe` requests on UDP multicast `239.255.255.250:3702`.
- Serves basic ONVIF Device and Media SOAP endpoints.
- Returns one H.264 RTSP stream URI from `GetStreamUri`.
- Starts `ffmpeg` as an H.264 RTP encoder using either a generated test pattern or a media file.
- Lets you choose the video bitrate, the encoder (software x264 or NVENC/QSV/AMF hardware) and its preset.
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

`--scale` works in both directions: use it to upscale a Full HD file to 4K as well as to downscale a large file. The scale filter does not preserve the aspect ratio, so keep the target in the same ratio as the source. Upscaling adds no detail, and the output bitrate is not changed by `--scale` (see below).

## Sub stream

Real cameras publish a high-resolution main stream and a low-resolution sub stream, and NVRs use the sub stream for live grids. Add `--sub-stream` (or `sub_stream = true` in TOML) to publish both:

```powershell
cargo run -- --advertise-host 192.168.1.50 --width 1920 --height 1080 --sub-stream
```

The camera then advertises two ONVIF profiles (`profile_1` main, `profile_2` sub) and serves two RTSP paths on the same RTSP port (`/live` and `/live_sub`). `GetStreamUri`, `GetProfile` and `GetVideoEncoderConfigurationOptions` follow the `ProfileToken` of the request. A single ffmpeg process decodes the source once and encodes both streams; an unknown RTSP path returns 404 when a sub stream is enabled.

| CLI | TOML | Default |
|---|---|---|
| `--sub-width`, `--sub-height` | `sub_width`, `sub_height` | 240 lines high, main aspect ratio (a missing side keeps the main aspect ratio) |
| `--sub-fps` | `sub_fps` | main fps |
| `--sub-bitrate` | `sub_bitrate` | `512` kbit/s |
| `--sub-rtsp-path` | `sub_rtsp_path` | `live_sub` |
| `--sub-rtp-port` | `sub_rtp_port` | `rtp_port + 1000` (RTCP is the next port) |
| `--sub-codec`, `--sub-preset` | `sub_codec`, `sub_preset` | `libx264`, codec default (`libx265` publishes H.265, see below) |
| `--sub-file` | `sub_file` | none: the sub stream is scaled down from the main video at runtime (see below) |

### Prepared sub video (`sub_file`)

By default the sub stream is the main video scaled down while streaming. With `sub_file` the sub stream is fed from a second, already small video instead, so nothing is scaled at runtime:

```toml
file = "kyiv1080.mp4"
sub_stream = true
sub_file = "kyiv-sub.mkv"   # relative paths are relative to the config file
```

- If the file does not exist it is created once at startup from the main video (scaled to the sub size, H.264, at the sub bitrate, no audio) and reused afterwards. Cameras that share one `sub_file` create it only once. If creating fails the camera falls back to runtime scaling.
- If it exists (for example a prepared 240p clip) its size is used for the sub stream unless `sub_width` / `sub_height` are set. Other sizes are scaled to the configured sub size.
- It needs `file`, and, like the other `sub_*` options, `sub_stream = true`. The overlay text is drawn on the sub video too, with the font scaled to the sub height.
- It is decoded in addition to the main video (a small extra decode); on 1080p30 it saved about 10% of ffmpeg's CPU time compared with scaling. The main decode and encode dominate the cost either way.

The sub stream uses software `libx264` by default even when the main stream uses a hardware encoder, so a camera does not take a second NVENC session (consumer GPUs limit concurrent sessions). `sub_*` options without `sub_stream = true` are rejected. Sub-stream UDP ports take part in the fleet port-collision check.

### H.265 (HEVC) sub stream

Set `sub_codec` (or `--sub-codec`) to an HEVC encoder to publish the sub stream as H.265 while the main stream stays H.264:

```toml
[[cameras]]
name = "Fake Camera 01"
sub_stream = true
sub_codec = "libx265"   # sub stream is H.265, main stream is H.264
sub_preset = "ultrafast"
```

The format follows the encoder name: `libx265` and any `hevc_*` encoder (`hevc_nvenc`, `hevc_qsv`, `hevc_amf`) are HEVC. For such a stream the RTSP SDP uses `a=rtpmap:96 H265/90000` (RFC 7798) and the ONVIF Media2 service reports `<tt:Encoding>H265</tt:Encoding>` for that profile.

- `libx265` is configured with Main profile, `zerolatency` tune, `veryfast` default preset, and in-band VPS/SPS/PPS plus closed GOPs on every keyframe. `hevc_*` hardware encoders use the generic settings (`dump_extra` on keyframes) and are untested.
- ONVIF Media1 (`ver10`) cannot describe H.265, so, like real cameras, Media1 reports the HEVC sub profile as H.264 Baseline. A camera with an HEVC stream additionally serves Media2 (`ver20`) at `/onvif/media2_service`, announced in `GetServices`; there the sub profile is `H265` (Main profile) and the main profile `H264`. Media2 implements `GetProfiles`, `GetVideoSourceConfigurations`, `GetVideoEncoderConfigurations`, `GetVideoEncoderConfigurationOptions`, `GetStreamUri`, `GetSnapshotUri`, `GetServiceCapabilities` and `SetSynchronizationPoint`. Cameras without an HEVC stream do not offer Media2, so they behave as before. An NVR that only uses Media1 will see an H.264 profile and fail to decode the H.265 RTSP stream.
- HEVC is rejected for the main stream (`codec` / `--codec`) with a clear error; it is only wired up for sub streams for now.
- Check that your ffmpeg build has the encoder (`ffmpeg -encoders | findstr 265`). Clients and NVRs need HEVC decoding to view the sub stream.

## Bitrate, codec and preset

The encoder settings are independent of `--scale`:

- `--bitrate <kbit/s>` (`bitrate` in TOML) sets the target video bitrate. Default: `2500`. It is passed to ffmpeg as `-b:v` and `-maxrate`, with `-bufsize` at twice the value. Raise it together with the resolution or frame rate, for example about `16000` for 4K, otherwise the picture looks blocky. Resolution and fps do not change the bandwidth by themselves; the stream stays close to the configured bitrate (or below it for simple content).
- `--codec <encoder>` (`codec` in TOML) selects the ffmpeg H.264 encoder. Default: `libx264`.
- `--preset <name>` (`preset` in TOML) selects the encoder preset. If omitted, a fast default for the chosen codec is used.

| Codec | Runs on | Default preset | Notes |
|-------|---------|----------------|-------|
| `libx264` | CPU | `veryfast` | Use `ultrafast` to cut CPU load further. |
| `libx265` | CPU | `veryfast` | HEVC, sub stream only (`sub_codec`). See [H.265 sub stream](#h265-hevc-sub-stream). |
| `h264_nvenc` | NVIDIA GPU | `p2` | Presets `p1` (fastest) to `p7`. Consumer GPUs limit the number of simultaneous NVENC sessions (depends on the driver). |
| `h264_qsv` | Intel GPU | `veryfast` | Needs the Intel iGPU to be enabled. |
| `h264_amf` | AMD GPU | `speed` | `-quality` is set from the preset: `speed`, `balanced` or `quality`. |
| other | | none | `--preset` is passed through if set. |

The RTSP SDP carries no `sprop-parameter-sets`, so every encoder is configured to repeat SPS/PPS in the stream on each keyframe. Make sure the chosen encoder exists in your ffmpeg build (`ffmpeg -encoders`).

```powershell
cargo run -- --advertise-host 192.168.1.50 --file clip.mp4 --codec h264_nvenc --bitrate 8000
```

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
cargo run -- --config configs/cameras.toml
```

Each camera can set its own `file`. If omitted, it uses `[defaults].file`; if both are omitted, that camera uses the generated test pattern. Each camera needs unique HTTP, RTSP, RTP, and RTCP ports. The RTCP port is always `rtp_port + 1`.

All options available on the command line can also be set in the TOML `[defaults]` block or per `[[cameras]]` entry.

### CPU load with many cameras

Every camera runs its own ffmpeg encode, so the load grows linearly with the camera count. Sixteen 1080p30 `libx264 veryfast` streams can saturate a desktop CPU, and 4K is about four times heavier per stream. To reduce it, lower the resolution and fps (with `scale = true`), use `preset = "ultrafast"`, or move cameras to a hardware encoder. `codec` and `preset` can be set per camera, so encoders can be mixed.

Ready-made examples in the repository (`configs/`):

| File | Contents |
|------|----------|
| `configs/cameras-8.toml` | 8 cameras, `libx264` |
| `configs/cameras-16.toml` | 16 cameras, `libx264` |
| `configs/cameras-16-hw.toml` | 16 cameras at 4000 kbit/s: 01-08 on `h264_nvenc` (GPU), 09-16 on `libx264 ultrafast` (CPU), to stay under the NVENC session limit |

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
--bitrate <kbit/s>        Target video bitrate. Default: 2500
--codec <encoder>         ffmpeg H.264 encoder: libx264, h264_nvenc, h264_qsv, h264_amf, ... Default: libx264
--preset <name>           Encoder preset. Default depends on the codec (libx264: veryfast, h264_nvenc: p2).
--sub-stream              Also publish a low-resolution sub stream (see Sub stream).
--sub-codec <encoder>     Sub-stream encoder. Default: libx264. HEVC encoders (libx265, ...) publish H.265.
--overlay-text <text>     Text burned into the video. Default: camera name.
--overlay-font <path>     Font file used for the overlay. Default: a platform system font.
--overlay-font-size <px>  Overlay font size. Default: 32
--no-overlay              Disable video text overlay.
--no-ffmpeg               Start only ONVIF services and do not launch ffmpeg.
--no-discovery            Disable WS-Discovery listener.
--export-cameras <path>   Write the configured cameras as JSON ("-" for stdout) and exit.
```

## Export the camera list

`--export-cameras` prints the resolved camera list (name, UUID, advertise host, HTTP port, plus `username`/`password` when configured) as JSON and exits without starting any services. Other tools can use it to register the fake cameras elsewhere:

```powershell
cargo run -- --config configs/cameras.toml --export-cameras cameras.json
```

`username` / `password` (CLI `--username`/`--password`, or TOML `[defaults]` with per-camera override) are also exported for provisioning. See [Authentication](#authentication).

```json
{
  "cameras": [
    { "name": "Fake Camera 01", "uuid": "00000000-0000-4550-98a8-98a80cab1b88", "advertise_host": "192.168.1.50", "http_port": 8000 }
  ]
}
```

UUIDs that were not set in the config or with `--uuid` are generated on every run, so the exported UUIDs would not match a later run. Set them explicitly when you export.

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

## Authentication

Set `username` and `password` (CLI `--username`/`--password`, or TOML `[defaults]` with per-camera override; both are required together) and the camera asks for a login, like a real one:

- ONVIF SOAP (`/onvif/*_service`): WS-Security `UsernameToken` (password digest or text) in the SOAP header, or HTTP Digest. Failures get `401` with a `WWW-Authenticate: Digest` challenge and a `ter:NotAuthorized` fault.
- RTSP: Digest on every request except `OPTIONS`, e.g. `rtsp://admin:admin123@host:8554/live`.
- Open without a login: `GetServices`, `GetServiceCapabilities`, `GetCapabilities`, `GetDeviceInformation`, `GetSystemDateAndTime`, `GetScopes`, `GetWsdlUrl`. The device `GetServiceCapabilities` reports `UsernameToken`/`HttpDigest` as `true`.
- `--no-auth` (TOML `auth = false`) keeps the credentials for `--export-cameras` but does not require a login.
- Without credentials nothing is checked, as before.

This is a login check for NVR testing, not security: nonces are stateless, there is no TLS, and the password is stored in plain text.

## Cameras without snapshots

`--no-snapshot` (TOML `snapshot = false`) makes a camera behave like one that has no snapshot support: `GetSnapshotUri` (Media and Media2) returns `ter:ActionNotSupported`, the media capabilities report `SnapshotUri="false"`, and `/snapshot.jpg` returns 404. Use it to check that the NVR copes with a missing snapshot. By default the snapshot is the 1x1 stub image.

## Extra ONVIF services

Besides Device and Media (and Media2 for H.265), every camera answers four small services at the paths a real camera uses, and lists them in `GetServices` and `GetCapabilities`:

| Path | Service | What it does |
|------|---------|--------------|
| `/onvif/imaging_service` | Imaging | capabilities, default `GetImagingSettings` |
| `/onvif/deviceIO_service` | DeviceIO | capabilities: 1 video source, no audio, relays or inputs |
| `/onvif/analytics_service` | Analytics | capabilities only (no rules, no modules) |
| `/onvif/event_service` | Events | capabilities, `GetEventProperties` (empty topic set), pull-point subscriptions |

The Events service accepts `CreatePullPointSubscription`, `PullMessages`, `Renew` and `Unsubscribe`. There are never any events: `PullMessages` waits for the requested `Timeout` (at most 5 s) and returns an empty batch, so an NVR keeps its subscription alive without being flooded. Any other operation of these services returns `ter:ActionNotSupported`. They share the authentication of the other services.

## Audio

Cameras are video-only by default. `audio = "pcmu" | "pcma" | "g722"` (CLI `--audio`, TOML `[defaults]` with per-camera override) adds an audio track to every stream of the camera:

- **Source:** the audio of the `file` video when it has one (checked with `ffprobe` at startup), otherwise generated silence. Audio is mono and re-encoded at 8 kHz (G.711) or 16 kHz (G.722), 64 kbit/s.
- **RTSP:** the SDP gets an `m=audio` section (`trackID=1`) with the static payload types 0 (PCMU), 8 (PCMA) or 9 (G.722); a client sets up `trackID=0` for video and `trackID=1` for audio, over UDP or TCP-interleaved (default channels 0-1 and 2-3).
- **Ports:** each stream listens for its audio on its video RTP port + 2000 (and the next port for RTCP), for example 7004 for the main stream of a camera with `rtp_port = 5004`. These take part in the fleet port check.
- **ONVIF:** profiles get an `AudioSourceConfiguration` and `AudioEncoderConfiguration` (shared by all profiles), `GetAudioSources`, `GetAudioSourceConfigurations` and `GetAudioEncoderConfigurations` answer, the scopes include `type/audio_encoder` and DeviceIO reports one audio source. Media1 can only say `G711`, so G.722 is reported as G711 there (Media2 says `G722`).
- AAC is not supported: it needs a different RTP packetization.
