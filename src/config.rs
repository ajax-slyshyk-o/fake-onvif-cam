use crate::util;
use clap::Parser;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::net::{SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};

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
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub serial: String,
    pub overlay_text: Option<String>,
    pub overlay_font: Option<PathBuf>,
    pub overlay_font_size: u32,
    pub no_ffmpeg: bool,
    pub no_discovery: bool,
}

#[derive(Debug, Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// TOML config file containing one or more fake cameras.
    #[arg(long = "config")]
    config: Option<PathBuf>,

    /// Host or IP placed in ONVIF and RTSP URLs.
    #[arg(long = "advertise-host", default_value_t = default_advertise_host())]
    advertise_host: String,

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

    /// Do not launch ffmpeg.
    #[arg(long = "no-ffmpeg")]
    no_ffmpeg: bool,

    /// Disable WS-Discovery.
    #[arg(long = "no-discovery")]
    no_discovery: bool,
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
    manufacturer: Option<String>,
    model: Option<String>,
    firmware: Option<String>,
    serial: Option<String>,
    overlay_text: Option<String>,
    overlay_font: Option<PathBuf>,
    overlay_font_size: Option<u32>,
    no_overlay: Option<bool>,
    no_ffmpeg: Option<bool>,
    no_discovery: Option<bool>,
}

pub fn load_from_env() -> Result<Vec<Config>, String> {
    let cli = Cli::parse();

    if let Some(path) = cli.config.as_deref() {
        load_from_toml(path)
    } else {
        Ok(vec![cli.into()])
    }
}

impl From<Cli> for Config {
    fn from(cli: Cli) -> Self {
        let overlay_text = if cli.no_overlay {
            None
        } else {
            cli.overlay_text
                .clone()
                .or_else(|| Some(cli.camera_name.clone()))
        };

        Self {
            camera_name: cli.camera_name,
            uuid: cli.uuid.unwrap_or_else(util::make_uuid),
            http_addr: cli.http_addr,
            advertise_host: cli.advertise_host,
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
            no_ffmpeg: cli.no_ffmpeg,
            no_discovery: cli.no_discovery,
        }
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
            manufacturer: "Codex Labs".to_string(),
            model: "FakeCam Rust".to_string(),
            firmware: env!("CARGO_PKG_VERSION").to_string(),
            serial: "FAKE-ONVIF-001".to_string(),
            overlay_text: Some("Fake ONVIF Camera".to_string()),
            overlay_font: default_overlay_font(),
            overlay_font_size: 32,
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

        if !udp_ports.insert(config.rtp_port) {
            return Err(format!("duplicate UDP RTP port {}", config.rtp_port));
        }

        let rtcp_port = config.rtp_port + 1;
        if !udp_ports.insert(rtcp_port) {
            return Err(format!("duplicate UDP RTCP port {rtcp_port}"));
        }
    }

    Ok(())
}
