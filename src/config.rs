use crate::util;
use clap::Parser;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

const __VERSION: &str = concat!("\0", "Ver.:", env!("_GIT_VERSION"), "\0");
const _VERSION: &str = env!("_GIT_VERSION");

pub(crate) fn app_version() -> String {
    __VERSION
        .trim_matches('\0')
        .strip_prefix("Ver.:")
        .unwrap_or(_VERSION)
        .to_string()
}

static APP_VERSION: std::sync::LazyLock<String> = std::sync::LazyLock::new(app_version);

#[derive(Clone, Debug)]
pub enum MediaInput {
    TestPattern,
    File(PathBuf),
}

#[derive(Clone, Debug)]
pub struct Config {
    pub camera_name: String,
    pub uuid: String,
    pub http_addr: SocketAddr,
    pub advertise_host: String,
    pub rtsp_port: u16,
    pub rtsp_path: String,
    pub rtp_port: u16,
    pub ffmpeg_path: PathBuf,
    pub media_input: MediaInput,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: String,
    pub preset: Option<String>,
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
    pub overlay_text: Option<String>,
    pub overlay_font: Option<PathBuf>,
    pub overlay_font_size: u32,
    pub scale: bool,
    pub sub: Option<SubStream>,
    pub no_ffmpeg: bool,
    pub no_discovery: bool,
}

/// Optional second (sub) stream as configured. Unset dimensions and fps are derived from
/// the main stream in [`Config::streams`], because the main stream may still be replaced
/// by the probed file dimensions after the config is loaded.
#[derive(Clone, Debug)]
pub struct SubStream {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub bitrate_kbps: u32,
    pub rtsp_path: String,
    pub rtp_port: Option<u16>,
    pub codec: String,
    pub preset: Option<String>,
}

/// A fully resolved stream published by a camera.
#[derive(Clone, Debug)]
pub struct Stream {
    pub name: &'static str,
    pub rtsp_path: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub rtp_port: u16,
    pub codec: String,
    pub preset: Option<String>,
}

const SUB_DEFAULT_HEIGHT: u32 = 240;
const SUB_DEFAULT_BITRATE_KBPS: u32 = 512;
const SUB_DEFAULT_RTSP_PATH: &str = "live_sub";
const SUB_RTP_PORT_OFFSET: u16 = 1000;

impl Config {
    /// All streams of this camera; the main stream is always first.
    pub fn streams(&self) -> Vec<Stream> {
        let mut streams = vec![Stream {
            name: "main",
            rtsp_path: self.rtsp_path.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate_kbps,
            rtp_port: self.rtp_port,
            codec: self.codec.clone(),
            preset: self.preset.clone(),
        }];

        if let Some(sub) = &self.sub {
            let (width, height) = sub_dimensions(sub, self.width, self.height);
            streams.push(Stream {
                name: "sub",
                rtsp_path: sub.rtsp_path.clone(),
                width,
                height,
                fps: sub.fps.unwrap_or(self.fps),
                bitrate_kbps: sub.bitrate_kbps,
                rtp_port: sub.rtp_port.unwrap_or_else(|| {
                    self.rtp_port
                        .checked_add(SUB_RTP_PORT_OFFSET)
                        .unwrap_or_else(|| self.rtp_port.saturating_sub(SUB_RTP_PORT_OFFSET))
                }),
                codec: sub.codec.clone(),
                preset: sub.preset.clone(),
            });
        }

        streams
    }
}

/// Sub-stream size: explicit values win, a missing side keeps the main aspect ratio, and with
/// neither given the sub stream is 240 lines high. Results are even (required by H.264 4:2:0).
fn sub_dimensions(sub: &SubStream, main_width: u32, main_height: u32) -> (u32, u32) {
    let scaled = |value: u32, num: u32, den: u32| {
        (((u64::from(value) * u64::from(num) + u64::from(den) / 2) / u64::from(den)) as u32).max(2)
    };
    let (width, height) = match (sub.width, sub.height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, scaled(w, main_height, main_width)),
        (None, Some(h)) => (scaled(h, main_width, main_height), h),
        (None, None) => (
            scaled(SUB_DEFAULT_HEIGHT, main_width, main_height),
            SUB_DEFAULT_HEIGHT,
        ),
    };
    ((width & !1).max(2), (height & !1).max(2))
}

/// Raw sub-stream options, shared by the CLI and the TOML parser.
#[derive(Clone, Debug, Default)]
struct SubOptions {
    enabled: bool,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    bitrate: Option<u32>,
    rtsp_path: Option<String>,
    rtp_port: Option<u16>,
    codec: Option<String>,
    preset: Option<String>,
}

impl SubOptions {
    fn into_sub_stream(self) -> Result<Option<SubStream>, String> {
        let any_detail = self.width.is_some()
            || self.height.is_some()
            || self.fps.is_some()
            || self.bitrate.is_some()
            || self.rtsp_path.is_some()
            || self.rtp_port.is_some()
            || self.codec.is_some()
            || self.preset.is_some();
        if !self.enabled {
            return if any_detail {
                Err("sub_* options require sub_stream = true".to_string())
            } else {
                Ok(None)
            };
        }

        Ok(Some(SubStream {
            width: self.width,
            height: self.height,
            fps: self.fps,
            bitrate_kbps: self.bitrate.unwrap_or(SUB_DEFAULT_BITRATE_KBPS),
            rtsp_path: self
                .rtsp_path
                .map(|value| trim_rtsp_path(&value))
                .unwrap_or_else(|| SUB_DEFAULT_RTSP_PATH.to_string()),
            rtp_port: self
                .rtp_port
                .map(|port| parse_rtp_port(&port.to_string()))
                .transpose()?,
            // 240p is cheap in software, and keeps hardware encoder sessions (NVENC has a
            // per-GPU limit) for the main streams.
            codec: self.codec.unwrap_or_else(|| "libx264".to_string()),
            preset: self.preset,
        }))
    }
}

#[derive(Debug, Parser)]
#[command(version = APP_VERSION.as_str(), about, long_about = None)]
struct Cli {
    /// TOML config file containing one or more fake cameras.
    #[arg(long = "config")]
    config: Option<PathBuf>,

    /// Host or IP placed in ONVIF and RTSP URLs. Overrides `advertise_host` from the config file when both are given.
    #[arg(long = "advertise-host")]
    advertise_host: Option<String>,

    /// HTTP bind address.
    #[arg(long = "http", default_value = "0.0.0.0:8000")]
    http_addr: SocketAddr,

    /// RTSP service port.
    #[arg(long = "rtsp-port", default_value_t = 8554)]
    rtsp_port: u16,

    /// RTSP path.
    #[arg(long = "rtsp-path", default_value = "live", value_parser = parse_rtsp_path)]
    rtsp_path: String,

    /// Local RTP ingest port for ffmpeg. The following port is used for RTCP.
    #[arg(long = "rtp-port", default_value_t = 5004, value_parser = parse_rtp_port)]
    rtp_port: u16,

    /// Camera name exposed in ONVIF metadata.
    #[arg(long = "name", default_value = "Fake ONVIF Camera")]
    camera_name: String,

    /// Stable camera UUID.
    #[arg(long = "uuid", value_parser = parse_uuid)]
    uuid: Option<String>,

    /// ffmpeg executable.
    #[arg(long = "ffmpeg", default_value = "ffmpeg")]
    ffmpeg_path: PathBuf,

    /// Loop this media file instead of a generated test pattern.
    #[arg(long = "file")]
    file: Option<PathBuf>,

    /// Test-pattern width.
    #[arg(long = "width", default_value_t = 1280, value_parser = parse_non_zero_u32)]
    width: u32,

    /// Test-pattern height.
    #[arg(long = "height", default_value_t = 720, value_parser = parse_non_zero_u32)]
    height: u32,

    /// Test-pattern frame rate.
    #[arg(long = "fps", default_value_t = 25, value_parser = parse_non_zero_u32)]
    fps: u32,

    /// Target H.264 video bitrate in kbit/s.
    #[arg(long = "bitrate", default_value_t = 2500, value_parser = parse_non_zero_u32)]
    bitrate_kbps: u32,

    /// ffmpeg H.264 encoder: libx264, h264_nvenc, h264_qsv, h264_amf, ...
    #[arg(long = "codec", default_value = "libx264")]
    codec: String,

    /// Encoder preset. Defaults to a fast one suited to the chosen codec
    /// (libx264/h264_qsv: veryfast, h264_nvenc: p2, h264_amf: speed).
    #[arg(long = "preset")]
    preset: Option<String>,

    /// Device manufacturer.
    #[arg(long = "manufacturer", default_value = "Codex Labs")]
    manufacturer: String,

    /// Device model.
    #[arg(long = "model", default_value = "FakeCam Rust")]
    model: String,

    /// Firmware version.
    #[arg(long = "firmware", default_value = env!("CARGO_PKG_VERSION"))]
    firmware: String,

    /// Serial number.
    #[arg(long = "serial", default_value = "FAKE-ONVIF-001")]
    serial: String,

    /// Text burned into the video. Defaults to the camera name.
    #[arg(long = "overlay-text")]
    overlay_text: Option<String>,

    /// Font file used for video text overlay.
    #[arg(long = "overlay-font")]
    overlay_font: Option<PathBuf>,

    /// Overlay font size in pixels.
    #[arg(
        long = "overlay-font-size",
        default_value_t = 32,
        value_parser = parse_non_zero_u32
    )]
    overlay_font_size: u32,

    /// Disable video text overlay.
    #[arg(long = "no-overlay")]
    no_overlay: bool,

    /// Scale and re-rate the video stream to the configured width, height, and fps.
    /// Has no effect for test patterns (they already use the configured size).
    #[arg(long = "scale")]
    scale: bool,

    /// Also publish a low-resolution sub stream (second ONVIF profile and RTSP path).
    #[arg(long = "sub-stream")]
    sub_stream: bool,

    /// Sub-stream width. Defaults to the main aspect ratio at 240 lines.
    #[arg(long = "sub-width", value_parser = parse_non_zero_u32)]
    sub_width: Option<u32>,

    /// Sub-stream height. Defaults to 240.
    #[arg(long = "sub-height", value_parser = parse_non_zero_u32)]
    sub_height: Option<u32>,

    /// Sub-stream frame rate. Defaults to the main frame rate.
    #[arg(long = "sub-fps", value_parser = parse_non_zero_u32)]
    sub_fps: Option<u32>,

    /// Sub-stream video bitrate in kbit/s. Defaults to 512.
    #[arg(long = "sub-bitrate", value_parser = parse_non_zero_u32)]
    sub_bitrate_kbps: Option<u32>,

    /// Sub-stream RTSP path. Defaults to live_sub.
    #[arg(long = "sub-rtsp-path")]
    sub_rtsp_path: Option<String>,

    /// Sub-stream local RTP ingest port (RTCP uses the next port). Defaults to rtp-port + 1000.
    #[arg(long = "sub-rtp-port", value_parser = parse_rtp_port)]
    sub_rtp_port: Option<u16>,

    /// Sub-stream encoder. Defaults to libx264.
    #[arg(long = "sub-codec")]
    sub_codec: Option<String>,

    /// Sub-stream encoder preset.
    #[arg(long = "sub-preset")]
    sub_preset: Option<String>,

    /// Do not launch ffmpeg.
    #[arg(long = "no-ffmpeg")]
    no_ffmpeg: bool,

    /// Disable WS-Discovery.
    #[arg(long = "no-discovery")]
    no_discovery: bool,

    /// Write a JSON description of the configured cameras to this file ("-" for stdout) and exit.
    #[arg(long = "export-cameras", value_name = "PATH")]
    export_cameras: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TomlConfig {
    #[serde(default)]
    defaults: TomlCamera,
    #[serde(default)]
    cameras: Vec<TomlCamera>,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct TomlCamera {
    name: Option<String>,
    uuid: Option<String>,
    http: Option<SocketAddr>,
    advertise_host: Option<String>,
    rtsp_port: Option<u16>,
    rtsp_path: Option<String>,
    rtp_port: Option<u16>,
    ffmpeg: Option<PathBuf>,
    file: Option<PathBuf>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<u32>,
    bitrate: Option<u32>,
    codec: Option<String>,
    preset: Option<String>,
    manufacturer: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    serial: Option<String>,
    overlay_text: Option<String>,
    overlay_font: Option<PathBuf>,
    overlay_font_size: Option<u32>,
    no_overlay: Option<bool>,
    scale: Option<bool>,
    sub_stream: Option<bool>,
    sub_width: Option<u32>,
    sub_height: Option<u32>,
    sub_fps: Option<u32>,
    sub_bitrate: Option<u32>,
    sub_rtsp_path: Option<String>,
    sub_rtp_port: Option<u16>,
    sub_codec: Option<String>,
    sub_preset: Option<String>,
    no_ffmpeg: Option<bool>,
    no_discovery: Option<bool>,
}

pub struct Startup {
    pub configs: Vec<Config>,
    pub export_cameras: Option<PathBuf>,
}

pub fn load_from_env() -> Result<Startup, String> {
    let cli = Cli::parse();
    let export_cameras = cli.export_cameras.clone();

    let configs = if let Some(path) = cli.config.as_deref() {
        let mut configs = load_from_toml(path)?;
        if let Some(host) = &cli.advertise_host {
            for config in &mut configs {
                config.advertise_host = host.clone();
            }
        }
        configs
    } else {
        vec![Config::try_from(cli)?]
    };

    Ok(Startup {
        configs,
        export_cameras,
    })
}

impl TryFrom<Cli> for Config {
    type Error = String;

    fn try_from(cli: Cli) -> Result<Self, String> {
        let sub = SubOptions {
            enabled: cli.sub_stream,
            width: cli.sub_width,
            height: cli.sub_height,
            fps: cli.sub_fps,
            bitrate: cli.sub_bitrate_kbps,
            rtsp_path: cli.sub_rtsp_path.clone(),
            rtp_port: cli.sub_rtp_port,
            codec: cli.sub_codec.clone(),
            preset: cli.sub_preset.clone(),
        }
        .into_sub_stream()?;

        let overlay_text = if cli.no_overlay {
            None
        } else {
            cli.overlay_text
                .clone()
                .or_else(|| Some(cli.camera_name.clone()))
        };

        Ok(Self {
            camera_name: cli.camera_name,
            uuid: cli.uuid.unwrap_or_else(util::make_uuid),
            http_addr: cli.http_addr,
            advertise_host: cli.advertise_host.unwrap_or_else(default_advertise_host),
            rtsp_port: cli.rtsp_port,
            rtsp_path: cli.rtsp_path,
            rtp_port: cli.rtp_port,
            ffmpeg_path: cli.ffmpeg_path,
            media_input: cli
                .file
                .map(MediaInput::File)
                .unwrap_or(MediaInput::TestPattern),
            width: cli.width,
            height: cli.height,
            fps: cli.fps,
            bitrate_kbps: cli.bitrate_kbps,
            codec: cli.codec,
            preset: cli.preset,
            manufacturer: cli.manufacturer,
            model: cli.model,
            firmware: cli.firmware,
            serial: cli.serial,
            overlay_text,
            overlay_font: if cli.no_overlay {
                None
            } else {
                cli.overlay_font.or_else(default_overlay_font)
            },
            overlay_font_size: cli.overlay_font_size,
            scale: cli.scale,
            sub,
            no_ffmpeg: cli.no_ffmpeg,
            no_discovery: cli.no_discovery,
        })
    }
}

pub fn load_from_toml(path: &Path) -> Result<Vec<Config>, String> {
    let text = fs::read_to_string(path)
        .map_err(|err| format!("failed to read config file {}: {err}", path.display()))?;
    let parsed: TomlConfig = toml::from_str(&text)
        .map_err(|err| format!("failed to parse config file {}: {err}", path.display()))?;

    if parsed.cameras.is_empty() {
        return Err("config file must contain at least one [[cameras]] entry".to_string());
    }

    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut configs = Vec::with_capacity(parsed.cameras.len());

    for (index, camera) in parsed.cameras.iter().enumerate() {
        configs.push(camera_from_toml(&parsed.defaults, camera, index, base_dir)?);
    }

    validate_fleet(&configs)?;
    Ok(configs)
}

fn camera_from_toml(
    defaults: &TomlCamera,
    camera: &TomlCamera,
    index: usize,
    base_dir: &Path,
) -> Result<Config, String> {
    let name = choose_string(&camera.name, &defaults.name, "Fake ONVIF Camera");
    let no_overlay = choose_copy(&camera.no_overlay, &defaults.no_overlay, false);
    let overlay_text = if no_overlay {
        None
    } else {
        choose_optional_string(&camera.overlay_text, &defaults.overlay_text)
            .or_else(|| Some(name.clone()))
    };

    let uuid = choose_optional_string(&camera.uuid, &defaults.uuid)
        .map(|value| parse_uuid(&value))
        .transpose()?
        .unwrap_or_else(util::make_uuid);
    let http_addr = choose_copy(
        &camera.http,
        &defaults.http,
        "0.0.0.0:8000"
            .parse()
            .expect("valid default socket address"),
    );
    let rtsp_path = choose_optional_string(&camera.rtsp_path, &defaults.rtsp_path)
        .map(|value| parse_rtsp_path(&value))
        .transpose()?
        .unwrap_or_else(|| "live".to_string());
    let rtp_port =
        parse_rtp_port(&choose_copy(&camera.rtp_port, &defaults.rtp_port, 5004).to_string())?;
    let width = validate_non_zero(
        choose_copy(&camera.width, &defaults.width, 1280),
        "width",
        index,
    )?;
    let height = validate_non_zero(
        choose_copy(&camera.height, &defaults.height, 720),
        "height",
        index,
    )?;
    let fps = validate_non_zero(choose_copy(&camera.fps, &defaults.fps, 25), "fps", index)?;
    let bitrate_kbps = validate_non_zero(
        choose_copy(&camera.bitrate, &defaults.bitrate, 2500),
        "bitrate",
        index,
    )?;
    let media_input = choose_optional_path(&camera.file, &defaults.file)
        .map(|path| MediaInput::File(resolve_config_path(base_dir, path)))
        .unwrap_or(MediaInput::TestPattern);
    let overlay_font = if no_overlay {
        None
    } else {
        choose_optional_path(&camera.overlay_font, &defaults.overlay_font)
            .map(|path| resolve_config_path(base_dir, path))
            .or_else(default_overlay_font)
    };

    Ok(Config {
        camera_name: name,
        uuid,
        http_addr,
        advertise_host: choose_string(
            &camera.advertise_host,
            &defaults.advertise_host,
            &default_advertise_host(),
        ),
        rtsp_port: choose_copy(&camera.rtsp_port, &defaults.rtsp_port, 8554),
        rtsp_path,
        rtp_port,
        ffmpeg_path: choose_optional_path(&camera.ffmpeg, &defaults.ffmpeg)
            .map(|path| resolve_command_path(base_dir, path))
            .unwrap_or_else(|| PathBuf::from("ffmpeg")),
        media_input,
        width,
        height,
        fps,
        bitrate_kbps,
        codec: choose_string(&camera.codec, &defaults.codec, "libx264"),
        preset: choose_optional_string(&camera.preset, &defaults.preset),
        manufacturer: choose_string(&camera.manufacturer, &defaults.manufacturer, "Codex Labs"),
        model: choose_string(&camera.model, &defaults.model, "FakeCam Rust"),
        firmware: choose_string(
            &camera.firmware,
            &defaults.firmware,
            env!("CARGO_PKG_VERSION"),
        ),
        serial: choose_string(&camera.serial, &defaults.serial, "FAKE-ONVIF-001"),
        overlay_text,
        overlay_font,
        overlay_font_size: validate_non_zero(
            choose_copy(&camera.overlay_font_size, &defaults.overlay_font_size, 32),
            "overlay_font_size",
            index,
        )?,
        scale: choose_copy(&camera.scale, &defaults.scale, false),
        sub: SubOptions {
            enabled: choose_copy(&camera.sub_stream, &defaults.sub_stream, false),
            width: camera.sub_width.or(defaults.sub_width),
            height: camera.sub_height.or(defaults.sub_height),
            fps: camera.sub_fps.or(defaults.sub_fps),
            bitrate: camera.sub_bitrate.or(defaults.sub_bitrate),
            rtsp_path: choose_optional_string(&camera.sub_rtsp_path, &defaults.sub_rtsp_path),
            rtp_port: camera.sub_rtp_port.or(defaults.sub_rtp_port),
            codec: choose_optional_string(&camera.sub_codec, &defaults.sub_codec),
            preset: choose_optional_string(&camera.sub_preset, &defaults.sub_preset),
        }
        .into_sub_stream()
        .map_err(|err| format!("camera {}: {err}", index + 1))?,
        no_ffmpeg: choose_copy(&camera.no_ffmpeg, &defaults.no_ffmpeg, false),
        no_discovery: choose_copy(&camera.no_discovery, &defaults.no_discovery, false),
    })
}

impl Default for Config {
    fn default() -> Self {
        Self {
            camera_name: "Fake ONVIF Camera".to_string(),
            uuid: util::make_uuid(),
            http_addr: "0.0.0.0:8000"
                .parse()
                .expect("valid default socket address"),
            advertise_host: default_advertise_host(),
            rtsp_port: 8554,
            rtsp_path: "live".to_string(),
            rtp_port: 5004,
            ffmpeg_path: PathBuf::from("ffmpeg"),
            media_input: MediaInput::TestPattern,
            width: 1280,
            height: 720,
            fps: 25,
            bitrate_kbps: 2500,
            codec: "libx264".to_string(),
            preset: None,
            manufacturer: "Codex Labs".to_string(),
            model: "FakeCam Rust".to_string(),
            firmware: env!("CARGO_PKG_VERSION").to_string(),
            serial: "FAKE-ONVIF-001".to_string(),
            overlay_text: Some("Fake ONVIF Camera".to_string()),
            overlay_font: default_overlay_font(),
            overlay_font_size: 32,
            scale: false,
            sub: None,
            no_ffmpeg: false,
            no_discovery: false,
        }
    }
}

fn parse_non_zero_u32(value: &str) -> Result<u32, String> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| format!("invalid positive integer: {value}"))?;

    if parsed == 0 {
        Err("value must be greater than zero".to_string())
    } else {
        Ok(parsed)
    }
}

fn validate_non_zero(value: u32, name: &str, camera_index: usize) -> Result<u32, String> {
    if value == 0 {
        Err(format!(
            "camera {} has invalid {name}: value must be greater than zero",
            camera_index + 1
        ))
    } else {
        Ok(value)
    }
}

fn parse_rtp_port(value: &str) -> Result<u16, String> {
    let parsed = value
        .parse::<u16>()
        .map_err(|_| format!("invalid UDP port: {value}"))?;

    if parsed == u16::MAX {
        Err("RTP port must leave room for the following RTCP port".to_string())
    } else {
        Ok(parsed)
    }
}

fn parse_rtsp_path(value: &str) -> Result<String, String> {
    Ok(trim_rtsp_path(value))
}

fn parse_uuid(value: &str) -> Result<String, String> {
    let uuid = normalize_uuid(value);

    if uuid.is_empty() {
        Err("UUID cannot be empty".to_string())
    } else {
        Ok(uuid)
    }
}

fn default_overlay_font() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "windows") {
        &[
            r"C:\Windows\Fonts\segoeui.ttf",
            r"C:\Windows\Fonts\arial.ttf",
            r"C:\Windows\Fonts\calibri.ttf",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Supplemental/Helvetica.ttf",
            "/System/Library/Fonts/SFNS.ttf",
        ]
    } else {
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation2/LiberationSans-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        ]
    };

    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn default_advertise_host() -> String {
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                let ip = addr.ip();
                if !ip.is_unspecified() {
                    return ip.to_string();
                }
            }
        }
    }

    "127.0.0.1".to_string()
}

fn normalize_uuid(value: &str) -> String {
    value
        .trim()
        .strip_prefix("urn:uuid:")
        .or_else(|| value.trim().strip_prefix("uuid:"))
        .unwrap_or_else(|| value.trim())
        .to_string()
}

fn trim_rtsp_path(value: &str) -> String {
    let trimmed = value.trim().trim_matches('/');

    if trimmed.is_empty() {
        "live".to_string()
    } else {
        trimmed.to_string()
    }
}

fn choose_copy<T: Copy>(camera: &Option<T>, defaults: &Option<T>, fallback: T) -> T {
    camera.or(*defaults).unwrap_or(fallback)
}

fn choose_string(camera: &Option<String>, defaults: &Option<String>, fallback: &str) -> String {
    choose_optional_string(camera, defaults).unwrap_or_else(|| fallback.to_string())
}

fn choose_optional_string(camera: &Option<String>, defaults: &Option<String>) -> Option<String> {
    camera.clone().or_else(|| defaults.clone())
}

fn choose_optional_path(camera: &Option<PathBuf>, defaults: &Option<PathBuf>) -> Option<PathBuf> {
    camera.clone().or_else(|| defaults.clone())
}

fn resolve_config_path(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    }
}

fn resolve_command_path(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() || path.components().count() == 1 {
        path
    } else {
        base_dir.join(path)
    }
}

fn validate_fleet(configs: &[Config]) -> Result<(), String> {
    let mut tcp_ports = HashSet::new();
    let mut udp_ports = HashSet::new();
    let mut uuids = HashSet::new();

    for config in configs {
        if !uuids.insert(config.uuid.clone()) {
            return Err(format!("duplicate camera uuid: {}", config.uuid));
        }

        let http_port = config.http_addr.port();
        if !tcp_ports.insert(http_port) {
            return Err(format!(
                "duplicate TCP port {http_port}; each fake camera needs unique HTTP and RTSP ports"
            ));
        }

        if !tcp_ports.insert(config.rtsp_port) {
            return Err(format!(
                "duplicate TCP port {}; each fake camera needs unique HTTP and RTSP ports",
                config.rtsp_port
            ));
        }

        let mut paths = HashSet::new();
        for stream in config.streams() {
            if !paths.insert(stream.rtsp_path.clone()) {
                return Err(format!(
                    "camera {}: {} stream reuses RTSP path \"{}\"",
                    config.camera_name, stream.name, stream.rtsp_path
                ));
            }

            if !udp_ports.insert(stream.rtp_port) {
                return Err(format!(
                    "duplicate UDP RTP port {} ({} stream of {})",
                    stream.rtp_port, stream.name, config.camera_name
                ));
            }

            let rtcp_port = stream.rtp_port + 1;
            if !udp_ports.insert(rtcp_port) {
                return Err(format!(
                    "duplicate UDP RTCP port {rtcp_port} ({} stream of {})",
                    stream.name, config.camera_name
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(width: Option<u32>, height: Option<u32>) -> SubStream {
        SubOptions {
            enabled: true,
            width,
            height,
            ..SubOptions::default()
        }
        .into_sub_stream()
        .unwrap()
        .unwrap()
    }

    #[test]
    fn sub_defaults_to_240_lines_with_main_aspect() {
        assert_eq!(sub_dimensions(&sub(None, None), 1920, 1080), (426, 240));
        assert_eq!(sub_dimensions(&sub(None, None), 1280, 720), (426, 240));
        assert_eq!(sub_dimensions(&sub(None, None), 640, 480), (320, 240));
    }

    #[test]
    fn sub_derives_missing_side_and_keeps_dimensions_even() {
        assert_eq!(
            sub_dimensions(&sub(Some(640), None), 1920, 1080),
            (640, 360)
        );
        assert_eq!(
            sub_dimensions(&sub(None, Some(360)), 1920, 1080),
            (640, 360)
        );
        assert_eq!(
            sub_dimensions(&sub(Some(641), Some(361)), 1920, 1080),
            (640, 360)
        );
    }

    #[test]
    fn streams_lists_main_then_sub_with_defaults() {
        let config = Config {
            sub: Some(sub(None, None)),
            ..Config::default()
        };
        let streams = config.streams();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].rtsp_path, "live");
        assert_eq!(streams[1].rtsp_path, "live_sub");
        assert_eq!(streams[1].rtp_port, 6004);
        assert_eq!(streams[1].fps, config.fps);
        assert_eq!(streams[1].bitrate_kbps, 512);
        assert_eq!(streams[1].codec, "libx264");
    }

    #[test]
    fn sub_options_without_enable_are_rejected() {
        let options = SubOptions {
            width: Some(320),
            ..SubOptions::default()
        };
        assert!(options.into_sub_stream().is_err());
    }

    #[test]
    fn fleet_rejects_colliding_sub_ports_and_paths() {
        let mut a = Config::default();
        a.uuid = "a".to_string();
        a.sub = Some(sub(None, None));
        let mut b = Config::default();
        b.uuid = "b".to_string();
        b.http_addr = "0.0.0.0:8001".parse().unwrap();
        b.rtsp_port = 8555;
        b.rtp_port = 6004; // collides with a's sub stream
        assert!(validate_fleet(&[a.clone(), b]).is_err());

        let mut same_path = a;
        same_path.sub.as_mut().unwrap().rtsp_path = "live".to_string();
        assert!(validate_fleet(&[same_path]).is_err());
    }
}
