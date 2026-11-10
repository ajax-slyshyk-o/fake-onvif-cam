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
    /// ONVIF credentials; only exported for NVR provisioning, never checked.
    pub username: Option<String>,
    pub password: Option<String>,
    /// Disables the authentication that credentials otherwise turn on.
    pub no_auth: bool,
    /// ONVIF reports snapshots as unsupported and `/snapshot.jpg` is not served.
    pub no_snapshot: bool,
    /// Audio track of every stream. `None`: video only.
    pub audio: Option<AudioCodec>,
    /// The main video file has an audio track to use (set at startup); otherwise silence.
    pub audio_from_file: bool,
    /// Names of ONVIF tokens and default RTSP paths.
    pub naming: Naming,
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
    /// Prepared low-resolution video for this stream. Created from the main file when missing.
    pub file: Option<PathBuf>,
}

/// Video format a stream is published in (RTP payload, SDP and ONVIF encoding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoFormat {
    H264,
    H265,
}

impl VideoFormat {
    /// Format produced by an ffmpeg encoder name: `libx265`, `hevc_nvenc`, `hevc_qsv`, ...
    /// are HEVC, everything else is treated as H.264.
    pub fn of_encoder(codec: &str) -> Self {
        let codec = codec.to_ascii_lowercase();
        if codec.contains("265") || codec.contains("hevc") {
            VideoFormat::H265
        } else {
            VideoFormat::H264
        }
    }
}

/// How a camera names its ONVIF profiles, configurations and RTSP paths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Naming {
    /// `profile_1`, `video_encoder_config_1`, `/live`, `/live_sub`.
    #[default]
    Default,
    /// The names of logent's cameras: `cam-0-main`, `cam-0-venc-main`, `/cam-0_m`, `/cam-0_s`.
    Logent,
}

impl Naming {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "logent" => Ok(Self::Logent),
            other => Err(format!(
                "unknown naming \"{other}\" (use default or logent)"
            )),
        }
    }

    /// RTSP path of the main stream unless one is configured.
    fn main_rtsp_path(self) -> &'static str {
        match self {
            Self::Default => "live",
            Self::Logent => "cam-0_m",
        }
    }

    /// RTSP path of the sub stream unless one is configured.
    fn sub_rtsp_path(self) -> &'static str {
        match self {
            Self::Default => SUB_DEFAULT_RTSP_PATH,
            Self::Logent => "cam-0_s",
        }
    }
}

/// Audio codec of a camera's audio track. Only static RTP payload types (RFC 3551), so the SDP
/// needs no `fmtp`. AAC is left out on purpose: it needs another RTP packetization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodec {
    Pcmu,
    Pcma,
    G722,
}

impl AudioCodec {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pcmu" | "g711u" | "ulaw" => Ok(Self::Pcmu),
            "pcma" | "g711a" | "alaw" => Ok(Self::Pcma),
            "g722" => Ok(Self::G722),
            other => Err(format!(
                "unknown audio codec \"{other}\" (use pcmu, pcma or g722)"
            )),
        }
    }

    /// ffmpeg encoder name.
    pub fn encoder(self) -> &'static str {
        match self {
            Self::Pcmu => "pcm_mulaw",
            Self::Pcma => "pcm_alaw",
            Self::G722 => "g722",
        }
    }

    /// Sampling rate of the audio itself.
    pub fn sample_rate(self) -> u32 {
        match self {
            Self::G722 => 16_000,
            _ => 8_000,
        }
    }

    /// Static RTP payload type.
    pub fn payload_type(self) -> u8 {
        match self {
            Self::Pcmu => 0,
            Self::Pcma => 8,
            Self::G722 => 9,
        }
    }

    /// `a=rtpmap` value. G.722 is sampled at 16 kHz but its RTP clock is 8 kHz (RFC 3551).
    pub fn rtpmap(self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU/8000",
            Self::Pcma => "PCMA/8000",
            Self::G722 => "G722/8000",
        }
    }

    /// Encoding name in Media1 (`ver10`) audio encoder configurations. That schema only knows
    /// G711, G726 and AAC, so G.722 is reported as G711 there (Media2 names it correctly).
    pub fn onvif_encoding(self) -> &'static str {
        "G711"
    }

    /// Encoding name in Media2 (`ver20`), which can name G.722 and tells G.711 u-law and A-law apart.
    pub fn media2_encoding(self) -> &'static str {
        match self {
            Self::Pcmu => "PCMU",
            Self::Pcma => "PCMA",
            Self::G722 => "G722",
        }
    }

    /// Output bitrate in kbit/s (all three codecs are 64 kbit/s).
    pub fn bitrate_kbps(self) -> u32 {
        64
    }
}

/// The audio track of one stream: it has its own RTP/RTCP port pair.
#[derive(Clone, Copy, Debug)]
pub struct AudioStream {
    pub codec: AudioCodec,
    pub rtp_port: u16,
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
    pub audio: Option<AudioStream>,
}

impl Stream {
    pub fn format(&self) -> VideoFormat {
        VideoFormat::of_encoder(&self.codec)
    }
}

impl Config {
    fn audio_for(&self, video_rtp_port: u16) -> Option<AudioStream> {
        self.audio.map(|codec| AudioStream {
            codec,
            rtp_port: video_rtp_port
                .checked_add(AUDIO_RTP_PORT_OFFSET)
                .unwrap_or_else(|| video_rtp_port.saturating_sub(AUDIO_RTP_PORT_OFFSET)),
        })
    }

    /// Login required by ONVIF and RTSP: `Some` only when both credentials are set and
    /// authentication was not switched off.
    pub fn auth_credentials(&self) -> Option<(&str, &str)> {
        if self.no_auth {
            return None;
        }
        Some((self.username.as_deref()?, self.password.as_deref()?))
    }

    /// The prepared sub-stream file is derived from the main file, so it needs one.
    fn validate_sub_file(&self) -> Result<(), String> {
        let has_sub_file = self.sub.as_ref().is_some_and(|sub| sub.file.is_some());
        if has_sub_file && !matches!(self.media_input, MediaInput::File(_)) {
            return Err(
                "sub_file requires file (a main video to derive the sub video from)".to_string(),
            );
        }
        Ok(())
    }

    /// True when any stream of this camera is published as H.265.
    pub fn has_hevc(&self) -> bool {
        self.streams()
            .iter()
            .any(|stream| stream.format() == VideoFormat::H265)
    }
}

/// `username` and `password` only make sense together.
fn validate_credentials(
    username: &Option<String>,
    password: &Option<String>,
) -> Result<(), String> {
    if username.is_some() != password.is_some() {
        return Err("username and password must be set together".to_string());
    }
    Ok(())
}

/// HEVC is only wired up for sub streams for now: the main stream stays H.264.
fn validate_main_codec(codec: &str) -> Result<(), String> {
    if VideoFormat::of_encoder(codec) == VideoFormat::H265 {
        Err(format!(
            "codec \"{codec}\" is HEVC, which is only supported for the sub stream (use sub_codec / --sub-codec)"
        ))
    } else {
        Ok(())
    }
}

const SUB_DEFAULT_HEIGHT: u32 = 240;
const SUB_DEFAULT_BITRATE_KBPS: u32 = 512;
const SUB_DEFAULT_RTSP_PATH: &str = "live_sub";
const SUB_RTP_PORT_OFFSET: u16 = 1000;
/// The audio track of a stream listens `AUDIO_RTP_PORT_OFFSET` above the stream's video port.
const AUDIO_RTP_PORT_OFFSET: u16 = 2000;

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
            audio: self.audio_for(self.rtp_port),
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
                audio: None,
            });
            let sub_stream = streams.last_mut().expect("just pushed");
            sub_stream.audio = self.audio_for(sub_stream.rtp_port);
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
    file: Option<PathBuf>,
    naming: Naming,
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
            || self.preset.is_some()
            || self.file.is_some();
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
                .unwrap_or_else(|| self.naming.sub_rtsp_path().to_string()),
            rtp_port: self
                .rtp_port
                .map(|port| parse_rtp_port(&port.to_string()))
                .transpose()?,
            // 240p is cheap in software, and keeps hardware encoder sessions (NVENC has a
            // per-GPU limit) for the main streams.
            codec: self.codec.unwrap_or_else(|| "libx264".to_string()),
            preset: self.preset,
            file: self.file,
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
    #[arg(long = "rtsp-path", value_parser = parse_rtsp_path)]
    rtsp_path: Option<String>,

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

    /// ONVIF/RTSP username. With `--password` it also turns authentication on.
    #[arg(long = "username")]
    username: Option<String>,

    /// ONVIF/RTSP password. With `--username` it also turns authentication on.
    #[arg(long = "password")]
    password: Option<String>,

    /// Export the credentials but do not require a login (ONVIF and RTSP stay open).
    #[arg(long = "no-auth")]
    no_auth: bool,

    /// Behave like a camera without snapshots: `GetSnapshotUri` is not supported and
    /// `/snapshot.jpg` returns 404.
    #[arg(long = "no-snapshot")]
    no_snapshot: bool,

    /// Add an audio track to every stream: pcmu, pcma or g722. It comes from the --file video
    /// when that has audio, otherwise it is silence. Off by default.
    #[arg(long = "audio", value_parser = AudioCodec::parse)]
    audio: Option<AudioCodec>,

    /// Names of ONVIF profiles/tokens and default RTSP paths: `default` (profile_1, /live, ...)
    /// or `logent` (cam-0-main, /cam-0_m, ...). An explicit --rtsp-path always wins.
    #[arg(long = "naming", value_parser = Naming::parse)]
    naming: Option<Naming>,

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

    /// Prepared low-resolution video for the sub stream, used instead of scaling the main video
    /// at runtime. Created from the main file when it does not exist. Requires --file.
    #[arg(long = "sub-file")]
    sub_file: Option<PathBuf>,

    /// Sub-stream encoder. Defaults to libx264. HEVC encoders (libx265, hevc_nvenc, ...) publish
    /// the sub stream as H.265.
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
    /// Expands this `[[cameras]]` entry into `count` cameras (see [`expand_cameras`]).
    count: Option<u32>,
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
    username: Option<String>,
    password: Option<String>,
    auth: Option<bool>,
    snapshot: Option<bool>,
    audio: Option<String>,
    naming: Option<String>,
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
    sub_file: Option<PathBuf>,
    no_ffmpeg: Option<bool>,
    no_discovery: Option<bool>,
}

fn ffmpeg_runs(path: &std::path::Path) -> bool {
    std::process::Command::new(path)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
        // If the ffmpeg from the config file can't run, fall back to `--ffmpeg`.
        for config in &mut configs {
            if config.no_ffmpeg || ffmpeg_runs(&config.ffmpeg_path) {
                continue;
            }
            eprintln!(
                "warning: ffmpeg `{}` from config can't run; using `{}`",
                config.ffmpeg_path.display(),
                cli.ffmpeg_path.display()
            );
            config.ffmpeg_path = cli.ffmpeg_path.clone();
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
        validate_main_codec(&cli.codec)?;
        let naming = cli.naming.unwrap_or_default();
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
            file: cli.sub_file.clone(),
            naming: cli.naming.unwrap_or_default(),
        }
        .into_sub_stream()?;

        let overlay_text = if cli.no_overlay {
            None
        } else {
            cli.overlay_text
                .clone()
                .or_else(|| Some(cli.camera_name.clone()))
        };

        validate_credentials(&cli.username, &cli.password)?;

        let config = Self {
            camera_name: cli.camera_name,
            uuid: cli.uuid.unwrap_or_else(util::make_uuid),
            http_addr: cli.http_addr,
            advertise_host: cli.advertise_host.unwrap_or_else(default_advertise_host),
            rtsp_port: cli.rtsp_port,
            rtsp_path: cli
                .rtsp_path
                .unwrap_or_else(|| naming.main_rtsp_path().to_string()),
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
            username: cli.username,
            password: cli.password,
            no_auth: cli.no_auth,
            no_snapshot: cli.no_snapshot,
            audio: cli.audio,
            audio_from_file: false,
            naming,
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
        };
        config.validate_sub_file()?;
        Ok(config)
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
    let cameras = expand_cameras(&parsed.defaults, &parsed.cameras)?;
    let mut configs = Vec::with_capacity(cameras.len());

    for (index, camera) in cameras.iter().enumerate() {
        configs.push(camera_from_toml(&parsed.defaults, camera, index, base_dir)?);
    }

    validate_fleet(&configs)?;
    Ok(configs)
}

/// Expands `[[cameras]]` entries with `count = N` into N cameras, so a group of identical cameras
/// is one block instead of N.
///
/// - `{n}` is the camera's number in the file (1, 2, ... across all groups), `{i}` its number
///   inside the group. A format may follow: `{n:02}` (zero padded), `{n:04x}` (hex). They work in
///   `name`, `uuid`, `serial` and `overlay_text`, for single cameras too. Other braces stay as they are.
/// - `http`, `rtsp_port` and `rtp_port` are the values of the group's first camera; the next
///   cameras get +1, +1 and +2 (RTP and RTCP). Without them a camera continues after the
///   previous one, starting at 8000 / 8554 / 5004.
/// - In a group (`count > 1`) a `name` without `{n}` / `{i}` gets ` {n:02}` appended, `uuid` must
///   contain a placeholder (every camera needs its own), and a missing `name`, `uuid` or `serial`
///   gets a numbered default.
fn expand_cameras(
    defaults: &TomlCamera,
    cameras: &[TomlCamera],
) -> Result<Vec<TomlCamera>, String> {
    if defaults.count.is_some() {
        return Err("count is not allowed in [defaults]".to_string());
    }

    let default_http = defaults.http;
    let mut next_http = default_http.map_or(8000, |addr| addr.port());
    let mut next_rtsp = 8554_u16;
    let mut next_rtp = 5004_u16;
    let mut expanded: Vec<TomlCamera> = Vec::new();

    for (group, camera) in cameras.iter().enumerate() {
        let count = camera.count.unwrap_or(1);
        if count == 0 {
            return Err(format!("[[cameras]] entry {}: count must be at least 1", group + 1));
        }
        let grouped = count > 1;

        let http = camera.http.or(defaults.http);
        let ip = http.map_or_else(|| "0.0.0.0".parse().expect("valid ip"), |addr| addr.ip());
        let first_http = http.map_or(next_http, |addr| addr.port());
        let first_rtsp = camera.rtsp_port.or(defaults.rtsp_port).unwrap_or(next_rtsp);
        let first_rtp = camera.rtp_port.or(defaults.rtp_port).unwrap_or(next_rtp);

        for i in 0..count {
            let n = expanded.len() + 1;
            let ports = |first: u16, step: u32| {
                u32::from(first)
                    .checked_add(i * step)
                    .and_then(|port| u16::try_from(port).ok())
                    .ok_or_else(|| {
                        format!(
                            "[[cameras]] entry {}: ports run out of range at camera {n}",
                            group + 1
                        )
                    })
            };

            let mut resolved = camera.clone();
            resolved.count = None;
            resolved.http = Some(SocketAddr::new(ip, ports(first_http, 1)?));
            resolved.rtsp_port = Some(ports(first_rtsp, 1)?);
            resolved.rtp_port = Some(ports(first_rtp, 2)?);

            let mut name = camera.name.clone().or_else(|| defaults.name.clone());
            if grouped {
                name = Some(match name {
                    Some(name) if has_placeholder(&name) => name,
                    Some(name) => format!("{name} {{n:02}}"),
                    None => "Fake ONVIF Camera {n:02}".to_string(),
                });
            }
            resolved.name = name.map(|value| expand_template(&value, n, i as usize + 1));

            let uuid = camera.uuid.clone().or_else(|| defaults.uuid.clone());
            resolved.uuid = match uuid {
                Some(value) if grouped && !has_placeholder(&value) => {
                    return Err(format!(
                        "[[cameras]] entry {}: uuid \"{value}\" is the same for all {count} cameras; add {{n}}, e.g. \"...{{n:04x}}\"",
                        group + 1
                    ));
                }
                Some(value) => Some(expand_template(&value, n, i as usize + 1)),
                None if grouped => Some(expand_template(
                    "00000000-0000-4550-98a8-98a80cab{n:04x}",
                    n,
                    i as usize + 1,
                )),
                None => None,
            };

            let serial = camera.serial.clone().or_else(|| defaults.serial.clone());
            resolved.serial = match serial {
                Some(value) => Some(expand_template(&value, n, i as usize + 1)),
                None if grouped => Some(expand_template("FAKE-ONVIF-{n:03}", n, i as usize + 1)),
                None => None,
            };

            resolved.overlay_text = camera
                .overlay_text
                .clone()
                .or_else(|| defaults.overlay_text.clone())
                .map(|value| expand_template(&value, n, i as usize + 1));

            expanded.push(resolved);
        }

        next_http = ports_after(first_http, count, 1);
        next_rtsp = ports_after(first_rtsp, count, 1);
        next_rtp = ports_after(first_rtp, count, 2);
    }

    Ok(expanded)
}

/// First port after a run of `count` cameras that started at `first` (saturating, so an
/// overflow surfaces as the "ports run out of range" error of the next camera).
fn ports_after(first: u16, count: u32, step: u32) -> u16 {
    u16::try_from(u32::from(first) + count * step).unwrap_or(u16::MAX)
}

fn has_placeholder(template: &str) -> bool {
    expand_template(template, 1, 1) != expand_template(template, 2, 2)
}

/// Replaces `{n}` / `{i}` (with an optional `:0W` / `:0Wx` style format) in `template`.
/// Anything that is not a valid placeholder is kept as it is.
fn expand_template(template: &str, n: usize, i: usize) -> String {
    let mut out = String::new();
    let mut rest = template;

    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push('{');
            rest = after;
            continue;
        };

        match format_placeholder(&after[..end], n, i) {
            Some(text) => {
                out.push_str(&text);
                rest = &after[end + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }

    out.push_str(rest);
    out
}

/// `n`, `i`, `n:02`, `n:x`, `i:04x`: variable, then optional `0` padding, width and `x` (hex).
fn format_placeholder(inner: &str, n: usize, i: usize) -> Option<String> {
    let (variable, spec) = inner.split_once(':').unwrap_or((inner, ""));
    let value = match variable {
        "n" => n,
        "i" => i,
        _ => return None,
    };

    let (spec, hex) = match spec.strip_suffix('x') {
        Some(spec) => (spec, true),
        None => (spec, false),
    };
    let zero = spec.starts_with('0');
    let width: usize = if spec.is_empty() { 0 } else { spec.parse().ok()? };

    Some(match (hex, zero) {
        (true, true) => format!("{value:0width$x}"),
        (true, false) => format!("{value:width$x}"),
        (false, true) => format!("{value:0width$}"),
        (false, false) => format!("{value:width$}"),
    })
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
    let naming = choose_optional_string(&camera.naming, &defaults.naming)
        .map(|value| Naming::parse(&value))
        .transpose()
        .map_err(|err| format!("camera {}: {err}", index + 1))?
        .unwrap_or_default();
    let rtsp_path = choose_optional_string(&camera.rtsp_path, &defaults.rtsp_path)
        .map(|value| parse_rtsp_path(&value))
        .transpose()?
        .unwrap_or_else(|| naming.main_rtsp_path().to_string());
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
    let codec = choose_string(&camera.codec, &defaults.codec, "libx264");
    validate_main_codec(&codec).map_err(|err| format!("camera {}: {err}", index + 1))?;
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

    let username = choose_optional_string(&camera.username, &defaults.username);
    let password = choose_optional_string(&camera.password, &defaults.password);
    validate_credentials(&username, &password)
        .map_err(|err| format!("camera {}: {err}", index + 1))?;

    let config = Config {
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
        codec,
        preset: choose_optional_string(&camera.preset, &defaults.preset),
        manufacturer: choose_string(&camera.manufacturer, &defaults.manufacturer, "Codex Labs"),
        model: choose_string(&camera.model, &defaults.model, "FakeCam Rust"),
        firmware: choose_string(
            &camera.firmware,
            &defaults.firmware,
            env!("CARGO_PKG_VERSION"),
        ),
        serial: choose_string(&camera.serial, &defaults.serial, "FAKE-ONVIF-001"),
        username,
        password,
        no_auth: !choose_copy(&camera.auth, &defaults.auth, true),
        no_snapshot: !choose_copy(&camera.snapshot, &defaults.snapshot, true),
        audio: choose_optional_string(&camera.audio, &defaults.audio)
            .map(|value| AudioCodec::parse(&value))
            .transpose()
            .map_err(|err| format!("camera {}: {err}", index + 1))?,
        audio_from_file: false,
        naming,
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
            file: choose_optional_path(&camera.sub_file, &defaults.sub_file)
                .map(|path| resolve_config_path(base_dir, path)),
            naming,
        }
        .into_sub_stream()
        .map_err(|err| format!("camera {}: {err}", index + 1))?,
        no_ffmpeg: choose_copy(&camera.no_ffmpeg, &defaults.no_ffmpeg, false),
        no_discovery: choose_copy(&camera.no_discovery, &defaults.no_discovery, false),
    };
    config
        .validate_sub_file()
        .map_err(|err| format!("camera {}: {err}", index + 1))?;
    Ok(config)
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
            username: None,
            password: None,
            no_auth: false,
            no_snapshot: false,
            audio: None,
            audio_from_file: false,
            naming: Naming::Default,
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

            if let Some(audio) = &stream.audio {
                for port in [audio.rtp_port, audio.rtp_port + 1] {
                    if !udp_ports.insert(port) {
                        return Err(format!(
                            "duplicate UDP audio port {port} ({} stream of {}); audio uses the video port + {AUDIO_RTP_PORT_OFFSET}",
                            stream.name, config.camera_name
                        ));
                    }
                }
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
    fn encoder_names_map_to_video_format() {
        for codec in ["libx265", "hevc_nvenc", "hevc_qsv", "hevc_amf", "LIBX265"] {
            assert_eq!(VideoFormat::of_encoder(codec), VideoFormat::H265, "{codec}");
        }
        for codec in ["libx264", "h264_nvenc", "h264_qsv", "h264_amf"] {
            assert_eq!(VideoFormat::of_encoder(codec), VideoFormat::H264, "{codec}");
        }
    }

    #[test]
    fn hevc_is_allowed_for_sub_but_not_main() {
        let hevc_sub = SubOptions {
            enabled: true,
            codec: Some("libx265".to_string()),
            ..SubOptions::default()
        }
        .into_sub_stream()
        .unwrap()
        .unwrap();
        let config = Config {
            sub: Some(hevc_sub),
            ..Config::default()
        };
        let streams = config.streams();
        assert_eq!(streams[0].format(), VideoFormat::H264);
        assert_eq!(streams[1].format(), VideoFormat::H265);

        assert!(validate_main_codec("libx264").is_ok());
        assert!(validate_main_codec("libx265").is_err());
        assert!(validate_main_codec("hevc_nvenc").is_err());
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

    #[test]
    fn sub_file_needs_sub_stream_and_a_main_file() {
        let toml = |body: &str| {
            let config: TomlConfig = toml::from_str(body).unwrap();
            camera_from_toml(&config.defaults, &config.cameras[0], 0, Path::new("/cfg"))
        };

        let ok =
            toml("[[cameras]]\nfile = \"main.mp4\"\nsub_stream = true\nsub_file = \"sub.mkv\"\n")
                .unwrap();
        assert_eq!(
            ok.sub.unwrap().file,
            Some(PathBuf::from("/cfg/sub.mkv")),
            "relative to the config file"
        );

        let no_sub = toml("[[cameras]]\nfile = \"main.mp4\"\nsub_file = \"sub.mkv\"\n");
        assert!(no_sub.unwrap_err().contains("sub_stream"));

        let no_main = toml("[[cameras]]\nsub_stream = true\nsub_file = \"sub.mkv\"\n");
        assert!(no_main.unwrap_err().contains("requires file"));
    }

    #[test]
    fn audio_codec_names_and_properties() {
        assert_eq!(AudioCodec::parse("PCMU"), Ok(AudioCodec::Pcmu));
        assert_eq!(AudioCodec::parse("g711a"), Ok(AudioCodec::Pcma));
        assert_eq!(AudioCodec::parse("g722"), Ok(AudioCodec::G722));
        assert!(AudioCodec::parse("aac")
            .unwrap_err()
            .contains("pcmu, pcma or g722"));

        assert_eq!(AudioCodec::G722.sample_rate(), 16_000);
        assert_eq!(
            AudioCodec::G722.rtpmap(),
            "G722/8000",
            "RTP clock stays 8 kHz"
        );
        assert_eq!(
            [AudioCodec::Pcmu, AudioCodec::Pcma, AudioCodec::G722].map(AudioCodec::payload_type),
            [0, 8, 9]
        );
    }

    #[test]
    fn audio_gets_its_own_port_pair_per_stream() {
        let config = Config {
            audio: Some(AudioCodec::Pcmu),
            rtp_port: 5004,
            sub: Some(sub(None, None)),
            ..Config::default()
        };
        let streams = config.streams();
        assert_eq!(streams[0].audio.unwrap().rtp_port, 7004);
        assert_eq!(
            streams[1].audio.unwrap().rtp_port,
            8004,
            "sub video is 6004"
        );
        assert!(Config::default().streams()[0].audio.is_none());
    }

    #[test]
    fn audio_ports_take_part_in_the_fleet_check() {
        let audio = |http: u16, rtsp: u16, rtp: u16| Config {
            audio: Some(AudioCodec::Pcma),
            http_addr: format!("0.0.0.0:{http}").parse().unwrap(),
            rtsp_port: rtsp,
            rtp_port: rtp,
            uuid: format!("{http}"),
            ..Config::default()
        };
        // The audio port of the first camera (5004 + 2000) is the video port of the second.
        let err = validate_fleet(&[audio(8000, 8554, 5004), audio(8001, 8555, 7004)]).unwrap_err();
        assert!(err.contains("7004"), "{err}");
        validate_fleet(&[audio(8000, 8554, 5004), audio(8001, 8555, 5006)]).unwrap();
    }

    #[test]
    fn audio_in_toml() {
        let parse = |body: &str| {
            let config: TomlConfig = toml::from_str(body).unwrap();
            camera_from_toml(&config.defaults, &config.cameras[0], 0, Path::new("/cfg"))
        };
        let config = parse("[defaults]\naudio = \"g722\"\n[[cameras]]\n").unwrap();
        assert_eq!(config.audio, Some(AudioCodec::G722));
        assert_eq!(parse("[[cameras]]\n").unwrap().audio, None);
        assert!(parse("[[cameras]]\naudio = \"aac\"\n")
            .unwrap_err()
            .contains("camera 1"));
    }

    #[test]
    fn naming_sets_default_rtsp_paths_but_explicit_paths_win() {
        let parse = |body: &str| {
            let config: TomlConfig = toml::from_str(body).unwrap();
            camera_from_toml(&config.defaults, &config.cameras[0], 0, Path::new("/cfg")).unwrap()
        };

        let plain = parse("[[cameras]]\nsub_stream = true\n");
        assert_eq!(plain.naming, Naming::Default);
        assert_eq!(plain.streams()[0].rtsp_path, "live");
        assert_eq!(plain.streams()[1].rtsp_path, "live_sub");

        let logent = parse("[defaults]\nnaming = \"logent\"\n[[cameras]]\nsub_stream = true\n");
        assert_eq!(logent.streams()[0].rtsp_path, "cam-0_m");
        assert_eq!(logent.streams()[1].rtsp_path, "cam-0_s");

        let explicit = parse(
            "[[cameras]]\nnaming = \"logent\"\nrtsp_path = \"main\"\nsub_stream = true\nsub_rtsp_path = \"low\"\n",
        );
        assert_eq!(explicit.streams()[0].rtsp_path, "main");
        assert_eq!(explicit.streams()[1].rtsp_path, "low");
    }

    #[test]
    fn naming_parse_rejects_unknown_schemes() {
        assert_eq!(Naming::parse("Logent"), Ok(Naming::Logent));
        assert!(Naming::parse("onvif")
            .unwrap_err()
            .contains("default or logent"));
    }

    fn expand(body: &str) -> Result<Vec<TomlCamera>, String> {
        let config: TomlConfig = toml::from_str(body).unwrap();
        expand_cameras(&config.defaults, &config.cameras)
    }

    #[test]
    fn templates_support_padding_and_hex() {
        assert_eq!(expand_template("Cam {n} / {i}", 3, 1), "Cam 3 / 1");
        assert_eq!(expand_template("{n:02}-{n:03}", 7, 1), "07-007");
        assert_eq!(expand_template("{n:x} {n:04x} {n:4}", 255, 1), "ff 00ff  255");
        assert_eq!(expand_template("{x} {n:q} { {n} {n", 1, 1), "{x} {n:q} { 1 {n");
        assert!(has_placeholder("a{i}"));
        assert!(!has_placeholder("a-b"));
    }

    #[test]
    fn group_expands_names_serials_uuids_and_ports() {
        let cameras = expand(
            "[[cameras]]\ncount = 3\nname = \"Cam {n:02}\"\nuuid = \"u-{n}\"\nserial = \"S{i}\"\n\
             http = \"0.0.0.0:8100\"\nrtsp_port = 8600\nrtp_port = 6000\n",
        )
        .unwrap();

        assert_eq!(cameras.len(), 3);
        let names: Vec<_> = cameras.iter().map(|c| c.name.clone().unwrap()).collect();
        assert_eq!(names, ["Cam 01", "Cam 02", "Cam 03"]);
        assert_eq!(cameras[2].uuid.as_deref(), Some("u-3"));
        assert_eq!(cameras[2].serial.as_deref(), Some("S3"));
        assert_eq!(cameras[2].http.unwrap().port(), 8102);
        assert_eq!(cameras[2].rtsp_port, Some(8602));
        assert_eq!(cameras[2].rtp_port, Some(6004));
        assert!(cameras.iter().all(|c| c.count.is_none()));
    }

    #[test]
    fn groups_continue_ports_and_numbering() {
        let cameras = expand(
            "[[cameras]]\nname = \"Solo\"\n\
             [[cameras]]\ncount = 2\nname = \"Grp\"\n\
             [[cameras]]\ncount = 2\nhttp = \"0.0.0.0:9000\"\nname = \"Far {n}\"\nuuid = \"f{n}\"\n",
        )
        .unwrap();

        let http: Vec<_> = cameras.iter().map(|c| c.http.unwrap().port()).collect();
        assert_eq!(http, [8000, 8001, 8002, 9000, 9001]);
        let rtsp: Vec<_> = cameras.iter().map(|c| c.rtsp_port.unwrap()).collect();
        assert_eq!(rtsp, [8554, 8555, 8556, 8557, 8558]);
        let rtp: Vec<_> = cameras.iter().map(|c| c.rtp_port.unwrap()).collect();
        assert_eq!(rtp, [5004, 5006, 5008, 5010, 5012]);

        // a name without a placeholder gets the number appended in a group, not for a single camera
        assert_eq!(cameras[0].name.as_deref(), Some("Solo"));
        assert_eq!(cameras[1].name.as_deref(), Some("Grp 02"));
        assert_eq!(cameras[4].name.as_deref(), Some("Far 5"));
        // defaults for a group without uuid / serial
        assert_eq!(
            cameras[1].uuid.as_deref(),
            Some("00000000-0000-4550-98a8-98a80cab0002")
        );
        assert_eq!(cameras[2].serial.as_deref(), Some("FAKE-ONVIF-003"));
        assert_eq!(cameras[0].uuid, None, "a single camera keeps the generated uuid");
    }

    #[test]
    fn group_errors() {
        let same_uuid = expand("[[cameras]]\ncount = 2\nuuid = \"fixed\"\n").unwrap_err();
        assert!(same_uuid.contains("same for all 2"), "{same_uuid}");
        assert!(expand("[[cameras]]\ncount = 0\n").is_err());
        assert!(expand("[defaults]\ncount = 2\n[[cameras]]\n").is_err());
        assert!(expand("[[cameras]]\ncount = 3\nrtp_port = 65532\n")
            .unwrap_err()
            .contains("out of range"));
    }

    #[test]
    fn defaults_are_templated_too() {
        let cameras = expand(
            "[defaults]\noverlay_text = \"cam {n}\"\nserial = \"SN{n}\"\n[[cameras]]\ncount = 2\n",
        )
        .unwrap();
        assert_eq!(cameras[1].overlay_text.as_deref(), Some("cam 2"));
        assert_eq!(cameras[1].serial.as_deref(), Some("SN2"));
    }
}
