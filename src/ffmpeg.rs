use crate::config::{AudioStream, Config, MediaInput, Stream};
use std::io;
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Probe the first video stream of `file` and return `(width, height, fps)`.
/// `fps` is rounded to the nearest integer from the ffprobe rational (e.g. 30000/1001 → 30).
pub fn probe_file(ffmpeg_path: &Path, file: &Path) -> Result<(u32, u32, u32), String> {
    let probe = ffprobe_path(ffmpeg_path);
    let output = Command::new(&probe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .map_err(|e| format!("ffprobe failed to run: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffprobe exited with {}: {stderr}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("").trim();
    let parts: Vec<&str> = line.split(',').collect();
    if parts.len() != 3 {
        return Err(format!("unexpected ffprobe output: {line}"));
    }

    let width = parts[0]
        .parse::<u32>()
        .map_err(|_| format!("bad width from ffprobe: {}", parts[0]))?;
    let height = parts[1]
        .parse::<u32>()
        .map_err(|_| format!("bad height from ffprobe: {}", parts[1]))?;
    let fps = parse_fps_rational(parts[2])?;

    if width == 0 || height == 0 {
        return Err(format!(
            "ffprobe returned zero dimensions: {width}x{height}"
        ));
    }

    Ok((width, height, fps))
}

fn ffprobe_path(ffmpeg_path: &Path) -> std::path::PathBuf {
    let name = ffmpeg_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("ffprobe");
    let probe_name = name.replace("ffmpeg", "ffprobe");
    if probe_name == name {
        ffmpeg_path.with_file_name("ffprobe")
    } else {
        ffmpeg_path.with_file_name(probe_name)
    }
}

fn parse_fps_rational(s: &str) -> Result<u32, String> {
    let (num, den) = if let Some((n, d)) = s.split_once('/') {
        let num = n
            .parse::<u64>()
            .map_err(|_| format!("bad fps numerator: {n}"))?;
        let den = d
            .parse::<u64>()
            .map_err(|_| format!("bad fps denominator: {d}"))?;
        (num, den)
    } else {
        (s.parse::<u64>().map_err(|_| format!("bad fps: {s}"))?, 1)
    };

    if den == 0 {
        return Err("fps denominator is zero".to_string());
    }

    let fps = ((num + den / 2) / den) as u32;
    if fps == 0 {
        return Err("fps is zero".to_string());
    }
    Ok(fps)
}

pub struct FfmpegGuard {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for FfmpegGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn start(config: &Config) -> io::Result<Option<FfmpegGuard>> {
    if config.no_ffmpeg {
        println!("ffmpeg: disabled");
        return Ok(None);
    }

    let mut child = spawn_child(config).map_err(|err| {
        io::Error::other(format!(
            "failed to spawn `{}`: {err}",
            config.ffmpeg_path.display()
        ))
    })?;
    thread::sleep(Duration::from_millis(250));

    if let Some(status) = child.try_wait()? {
        return Err(io::Error::other(format!(
            "ffmpeg exited immediately with status {status}"
        )));
    }

    for stream in config.streams() {
        println!(
            "ffmpeg: {} stream RTP to 127.0.0.1:{} and RTCP to 127.0.0.1:{}",
            stream.name,
            stream.rtp_port,
            stream.rtp_port + 1
        );
    }

    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker_config = config.clone();
    let worker = thread::Builder::new()
        .name("ffmpeg-supervisor".to_string())
        .spawn(move || supervise(worker_config, child, worker_stop))?;

    Ok(Some(FfmpegGuard {
        stop,
        worker: Some(worker),
    }))
}

fn supervise(config: Config, mut child: Child, stop: Arc<AtomicBool>) {
    loop {
        while !stop.load(Ordering::Relaxed) {
            match child.try_wait() {
                Ok(Some(status)) => {
                    eprintln!("ffmpeg exited with status {status}; restarting in 1s");
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(500)),
                Err(err) => {
                    eprintln!("ffmpeg status check failed: {err}; restarting in 1s");
                    break;
                }
            }
        }

        if stop.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }

        thread::sleep(Duration::from_secs(1));
        child = match spawn_child(&config) {
            Ok(child) => child,
            Err(err) => {
                eprintln!("failed to restart ffmpeg: {err}; retrying in 3s");
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
    }
}

fn spawn_child(config: &Config) -> io::Result<Child> {
    if !config.cache_files.is_empty() {
        return spawn_cached(config);
    }

    let mut command = Command::new(&config.ffmpeg_path);
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-nostdin");

    match &config.media_input {
        MediaInput::TestPattern => {
            let source = format!(
                "testsrc2=size={}x{}:rate={}",
                config.width, config.height, config.fps
            );
            command
                .arg("-re")
                .arg("-f")
                .arg("lavfi")
                .arg("-i")
                .arg(source);
        }
        MediaInput::File(path) => {
            command
                .arg("-re")
                .arg("-stream_loop")
                .arg("-1")
                .arg("-i")
                .arg(path);
        }
    }

    // A prepared low-resolution video feeds the sub stream, so the main video is not scaled
    // down at runtime.
    let streams = config.streams();
    let sub_file = config.sub.as_ref().and_then(|sub| sub.file.as_ref());
    if let (Some(path), [_, _]) = (sub_file, streams.as_slice()) {
        command
            .arg("-re")
            .arg("-stream_loop")
            .arg("-1")
            .arg("-i")
            .arg(path);
    }

    // Audio comes from the main video when it has some, otherwise it is generated silence from
    // one more (real-time) input.
    let mut audio_input = 0;
    if let Some(audio) = config.audio {
        if !(config.audio_from_file && matches!(config.media_input, MediaInput::File(_))) {
            audio_input = 1 + usize::from(sub_file.is_some() && streams.len() == 2);
            command
                .arg("-re")
                .arg("-f")
                .arg("lavfi")
                .arg("-i")
                .arg(format!("anullsrc=r={}:cl=mono", audio.sample_rate()));
        }
    }

    add_video_outputs(&mut command, config, |command, stream, index| {
        add_output(command, stream, index as u32 + 1);
    })?;

    // One RTP output per audio track, each on its own port pair.
    for (index, stream) in streams.iter().enumerate() {
        if let Some(audio) = &stream.audio {
            add_audio_output(&mut command, audio_input, audio, 11 + index as u32);
        }
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    command.spawn()
}

/// Video filters, `-map`s and one output per stream. `output` adds the options and the
/// destination that follow each `-map` (it gets the stream and its index).
///
/// One process and one decode for all streams: the (overlaid) video is split and the sub stream
/// is scaled down from the copy, or fed from the prepared `sub_file` (second input).
pub(crate) fn add_video_outputs(
    command: &mut Command,
    config: &Config,
    mut output: impl FnMut(&mut Command, &Stream, usize),
) -> io::Result<()> {
    let streams = config.streams();
    let sub_file = config.sub.as_ref().and_then(|sub| sub.file.as_ref());
    let mut vfilters: Vec<String> = Vec::new();

    if config.scale && matches!(config.media_input, MediaInput::File(_)) {
        vfilters.push(format!("scale={}:{}", config.width, config.height));
        vfilters.push(format!("fps={}", config.fps));
    }

    if let Some(text) = &config.overlay_text {
        vfilters.push(drawtext_filter(
            text,
            config.overlay_font.as_deref(),
            config.overlay_font_size,
        ));
    }

    match streams.as_slice() {
        [main] => {
            command.arg("-map").arg("0:v:0");
            if !vfilters.is_empty() {
                command.arg("-vf").arg(vfilters.join(","));
            }
            output(command, main, 0);
        }
        [main, sub] => {
            let graph = if sub_file.is_some() {
                let sub_font_size = scaled_font_size(config.overlay_font_size, main, sub);
                let sub_overlay = config.overlay_text.as_deref().map(|text| {
                    drawtext_filter(text, config.overlay_font.as_deref(), sub_font_size)
                });
                separate_sub_graph(&vfilters, sub, sub_overlay.as_deref())
            } else {
                split_filter_graph(&vfilters, sub)
            };
            command.arg("-filter_complex").arg(graph);
            command.arg("-map").arg("[main]");
            output(command, main, 0);
            command.arg("-map").arg("[sub]");
            output(command, sub, 1);
        }
        _ => return Err(io::Error::other("unsupported number of streams")),
    }

    Ok(())
}

/// Command that stream-copies the cached videos (one file per stream, see `cache.rs`) to RTP:
/// no decode and no encode. The main file also carries the audio of the source, if any.
fn spawn_cached(config: &Config) -> io::Result<Child> {
    let mut command = Command::new(&config.ffmpeg_path);
    command.args(["-hide_banner", "-loglevel", "warning", "-nostdin"]);

    for file in &config.cache_files {
        command.args(["-re", "-stream_loop", "-1", "-i"]).arg(file);
    }

    // Audio comes from the cached main file when the source had some, otherwise silence.
    let mut audio_input = 0;
    if let Some(audio) = config.audio {
        if !config.audio_from_file {
            audio_input = config.cache_files.len();
            command
                .args(["-re", "-f", "lavfi", "-i"])
                .arg(format!("anullsrc=r={}:cl=mono", audio.sample_rate()));
        }
    }

    let streams = config.streams();
    for (index, stream) in streams.iter().enumerate() {
        command.arg("-map").arg(format!("{index}:v:0"));
        add_copy_output(&mut command, stream, index as u32 + 1);
    }

    for (index, stream) in streams.iter().enumerate() {
        if let Some(audio) = &stream.audio {
            add_audio_output(&mut command, audio_input, audio, 11 + index as u32);
        }
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    command.spawn()
}

/// Command-line arguments of the RTP output carrying one audio track.
fn audio_output_args(
    input_index: usize,
    audio: &AudioStream,
    ssrc: u32,
    local: &str,
) -> Vec<String> {
    let rate = audio.codec.sample_rate();
    vec![
        "-map".to_string(),
        format!("{input_index}:a:0"),
        "-vn".to_string(),
        "-af".to_string(),
        format!("aformat=sample_fmts=s16:sample_rates={rate}:channel_layouts=mono"),
        "-c:a".to_string(),
        audio.codec.encoder().to_string(),
        "-ar".to_string(),
        rate.to_string(),
        "-ac".to_string(),
        "1".to_string(),
        "-f".to_string(),
        "rtp".to_string(),
        "-ssrc".to_string(),
        ssrc.to_string(),
        format!(
            "rtp://127.0.0.1:{}?rtcpport={}&pkt_size=1200{local}",
            audio.rtp_port,
            audio.rtp_port + 1
        ),
    ]
}

fn add_audio_output(command: &mut Command, input_index: usize, audio: &AudioStream, ssrc: u32) {
    let local = free_local_port_pair()
        .map(|port| format!("&localport={}&localrtcpport={}", port, port + 1))
        .unwrap_or_default();
    command.args(audio_output_args(input_index, audio, ssrc, &local));
}

/// True when `file` has at least one audio stream.
pub fn has_audio(ffmpeg_path: &Path, file: &Path) -> bool {
    Command::new(ffprobe_path(ffmpeg_path))
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(file)
        .output()
        .map(|output| output.status.success() && !output.stdout.trim_ascii().is_empty())
        .unwrap_or(false)
}

fn split_filter_graph(vfilters: &[String], sub: &Stream) -> String {
    let prefix = if vfilters.is_empty() {
        String::new()
    } else {
        format!("{},", vfilters.join(","))
    };

    format!(
        "[0:v]{prefix}split=2[main][subraw];[subraw]scale={}:{},fps={}[sub]",
        sub.width, sub.height, sub.fps
    )
}

/// Filter graph with a separate input for the sub stream (`-i main -i sub_file`): no split and
/// no scaling of the main video. The scale below is a pass-through when the file already has
/// the sub-stream size.
fn separate_sub_graph(vfilters: &[String], sub: &Stream, sub_overlay: Option<&str>) -> String {
    let main_chain = if vfilters.is_empty() {
        "null".to_string()
    } else {
        vfilters.join(",")
    };
    let overlay = sub_overlay
        .map(|filter| format!(",{filter}"))
        .unwrap_or_default();

    format!(
        "[0:v]{main_chain}[main];[1:v]scale={}:{},fps={}{overlay}[sub]",
        sub.width, sub.height, sub.fps
    )
}

/// Overlay font size for the sub stream: the main size scaled by the height ratio.
fn scaled_font_size(font_size: u32, main: &Stream, sub: &Stream) -> u32 {
    let scaled = u64::from(font_size) * u64::from(sub.height) / u64::from(main.height.max(1));
    (scaled as u32).max(8)
}

/// Makes `sub_file` usable: creates it from the main video when it does not exist, then reads
/// its size so the sub stream is announced with the real dimensions. On failure the camera
/// falls back to scaling the main video at runtime.
pub fn prepare_sub_file(config: &mut Config) {
    let MediaInput::File(main) = config.media_input.clone() else {
        return;
    };
    let Some(path) = config.sub.as_ref().and_then(|sub| sub.file.clone()) else {
        return;
    };
    let Some(stream) = config.streams().get(1).cloned() else {
        return;
    };

    if !path.exists() {
        println!(
            "sub file {} is missing; creating it from {} ({}x{}, {}k)",
            path.display(),
            main.display(),
            stream.width,
            stream.height,
            stream.bitrate_kbps
        );
        if let Err(err) = create_sub_file(&config.ffmpeg_path, &main, &path, &stream) {
            eprintln!(
                "warning: could not create {}: {err}; scaling the main video instead",
                path.display()
            );
            drop_sub_file(config);
            return;
        }
    }

    match probe_file(&config.ffmpeg_path, &path) {
        Ok((width, height, _)) => {
            if let Some(sub) = config.sub.as_mut() {
                if sub.width.is_none() && sub.height.is_none() {
                    sub.width = Some(width);
                    sub.height = Some(height);
                }
            }
        }
        Err(err) => {
            eprintln!(
                "warning: could not probe {}: {err}; scaling the main video instead",
                path.display()
            );
            drop_sub_file(config);
        }
    }
}

fn drop_sub_file(config: &mut Config) {
    if let Some(sub) = config.sub.as_mut() {
        sub.file = None;
    }
}

/// Encodes a small copy of `main` to `target` (written to a `.part` file first, so an
/// interrupted run never leaves a truncated file behind).
fn create_sub_file(
    ffmpeg_path: &Path,
    main: &Path,
    target: &Path,
    stream: &Stream,
) -> Result<(), String> {
    if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;
    }

    let mut part_name = target.file_stem().unwrap_or_default().to_os_string();
    part_name.push(".part");
    if let Some(extension) = target.extension() {
        part_name.push(".");
        part_name.push(extension);
    }
    let part: PathBuf = target.with_file_name(part_name);

    let bitrate = format!("{}k", stream.bitrate_kbps);
    let status = Command::new(ffmpeg_path)
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(main)
        .arg("-vf")
        .arg(format!(
            "scale={}:{},fps={}",
            stream.width, stream.height, stream.fps
        ))
        .args([
            "-an", "-c:v", "libx264", "-preset", "veryfast", "-pix_fmt", "yuv420p",
        ])
        .args(["-b:v", &bitrate, "-maxrate", &bitrate, "-bufsize"])
        .arg(format!("{}k", stream.bitrate_kbps.saturating_mul(2)))
        .args(["-g", &stream.fps.to_string()])
        .arg(&part)
        .status()
        .map_err(|err| format!("ffmpeg failed to run: {err}"))?;

    if !status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(format!("ffmpeg exited with {status}"));
    }

    std::fs::rename(&part, target).map_err(|err| format!("cannot rename {}: {err}", part.display()))
}

/// Local source ports for ffmpeg's RTP/RTCP sockets are taken from this range, below the
/// OS ephemeral range (49152+ on Windows, 32768+ on Linux).
const LOCAL_PORT_BASE: u16 = 20000;
const LOCAL_PORT_PAIRS: u32 = 5000;

/// Finds a local UDP port `p` such that both `p` and `p + 1` are currently free.
///
/// ffmpeg binds the ports seconds after they are probed (encoder start-up), so the pair must
/// not come from the OS ephemeral range: the OS hands a just-released port to the next
/// ephemeral bind, and any process (another ffmpeg, a browser) can take it in that window.
/// Pairs are handed out from a process-wide counter instead, so pairs never overlap, whether
/// they belong to two streams of one ffmpeg or to cameras starting or restarting concurrently.
fn free_local_port_pair() -> Option<u16> {
    static NEXT: AtomicU32 = AtomicU32::new(0);

    for _ in 0..LOCAL_PORT_PAIRS {
        let n = NEXT.fetch_add(1, Ordering::Relaxed) % LOCAL_PORT_PAIRS;
        let port = LOCAL_PORT_BASE + (n * 2) as u16;
        if UdpSocket::bind(("127.0.0.1", port)).is_ok()
            && UdpSocket::bind(("127.0.0.1", port + 1)).is_ok()
        {
            return Some(port);
        }
    }
    None
}

/// Local RTP/RTCP source ports and the destination URL of a stream's RTP output.
///
/// Without explicit local ports ffmpeg binds an ephemeral RTP port and then RTCP at +1
/// blindly, which fails with "bind failed" (WSAEADDRINUSE on Windows) when many ffmpeg
/// processes start at once.
fn rtp_url(stream: &Stream) -> String {
    let local = free_local_port_pair()
        .map(|port| format!("&localport={}&localrtcpport={}", port, port + 1))
        .unwrap_or_default();
    format!(
        "rtp://127.0.0.1:{}?rtcpport={}&pkt_size=1200{local}",
        stream.rtp_port,
        stream.rtp_port + 1
    )
}

/// Encoder and rate control of one video stream.
pub(crate) fn add_video_encode(command: &mut Command, stream: &Stream) {
    let bitrate = format!("{}k", stream.bitrate_kbps);
    let bufsize = format!("{}k", stream.bitrate_kbps.saturating_mul(2));

    add_encoder_args(command, &stream.codec, stream.preset.as_deref());
    command
        .arg("-bf")
        .arg("0")
        .arg("-b:v")
        .arg(&bitrate)
        .arg("-maxrate")
        .arg(&bitrate)
        .arg("-bufsize")
        .arg(&bufsize)
        .arg("-g")
        .arg(stream.fps.to_string());
}

/// Output options for one stream: encoder, rate control and the RTP destination.
fn add_output(command: &mut Command, stream: &Stream, ssrc: u32) {
    command.arg("-an");
    add_video_encode(command, stream);
    command
        .arg("-f")
        .arg("rtp")
        .arg("-payload_type")
        .arg("96")
        .arg("-ssrc")
        .arg(ssrc.to_string())
        .arg(rtp_url(stream));
}

/// RTP output of an already encoded stream (from the cache): copied. The RTP muxer converts
/// MP4-style packets and puts the parameter sets in front of every keyframe, which the SDP
/// (without sprop-parameter-sets) relies on.
fn add_copy_output(command: &mut Command, stream: &Stream, ssrc: u32) {
    command
        .args(["-an", "-c:v", "copy"])
        .args(["-f", "rtp", "-payload_type", "96", "-ssrc"])
        .arg(ssrc.to_string())
        .arg(rtp_url(stream));
}

/// Encoder-specific arguments. The RTSP SDP carries no sprop-parameter-sets, so every
/// encoder must repeat SPS/PPS (VPS/SPS/PPS for HEVC) in-band on keyframes. Encoders without
/// a dedicated branch (e.g. `hevc_nvenc`) use the generic one, which relies on `dump_extra`.
fn add_encoder_args(command: &mut Command, codec: &str, preset: Option<&str>) {
    command.arg("-c:v").arg(codec);

    match codec {
        "libx264" => {
            command
                .arg("-profile:v")
                .arg("baseline")
                .arg("-preset")
                .arg(preset.unwrap_or("veryfast"))
                .arg("-tune")
                .arg("zerolatency")
                .arg("-x264-params")
                .arg("repeat-headers=1")
                .arg("-pix_fmt")
                .arg("yuv420p");
        }
        // HEVC: no baseline profile, and x265 needs its own parameter syntax to repeat
        // VPS/SPS/PPS on every keyframe. Closed GOPs (IDR instead of CRA) keep every
        // keyframe a clean random-access point for clients that join mid-stream.
        "libx265" => {
            command
                .arg("-profile:v")
                .arg("main")
                .arg("-preset")
                .arg(preset.unwrap_or("veryfast"))
                .arg("-tune")
                .arg("zerolatency")
                .arg("-x265-params")
                .arg("repeat-headers=1:open-gop=0")
                .arg("-pix_fmt")
                .arg("yuv420p");
        }
        "h264_nvenc" => {
            command
                .arg("-profile:v")
                .arg("baseline")
                .arg("-preset")
                .arg(preset.unwrap_or("p2"))
                .arg("-tune")
                .arg("ll")
                .arg("-pix_fmt")
                .arg("yuv420p")
                .arg("-bsf:v")
                .arg("dump_extra=freq=keyframe");
        }
        "h264_qsv" => {
            command
                .arg("-profile:v")
                .arg("baseline")
                .arg("-preset")
                .arg(preset.unwrap_or("veryfast"))
                .arg("-pix_fmt")
                .arg("nv12")
                .arg("-bsf:v")
                .arg("dump_extra=freq=keyframe");
        }
        "h264_amf" => {
            command
                .arg("-usage")
                .arg("lowlatency")
                .arg("-quality")
                .arg(preset.unwrap_or("speed"))
                .arg("-profile:v")
                .arg("constrained_baseline")
                .arg("-pix_fmt")
                .arg("yuv420p")
                .arg("-bsf:v")
                .arg("dump_extra=freq=keyframe");
        }
        _ => {
            if let Some(preset) = preset {
                command.arg("-preset").arg(preset);
            }
            command
                .arg("-pix_fmt")
                .arg("yuv420p")
                .arg("-bsf:v")
                .arg("dump_extra=freq=keyframe");
        }
    }
}

fn drawtext_filter(text: &str, font: Option<&std::path::Path>, font_size: u32) -> String {
    let mut filter = String::from("drawtext=");

    if let Some(font) = font {
        filter.push_str("fontfile='");
        filter.push_str(&escape_drawtext_value(&font.to_string_lossy()));
        filter.push_str("':");
    }

    filter.push_str(&format!(
        "fontcolor=white:fontsize={font_size}:x=20:y=20:box=1:boxcolor=black@0.55:boxborderw=8:expansion=none:text='{}'",
        escape_drawtext_value(text)
    ));

    filter
}

fn escape_drawtext_value(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());

    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\'' => escaped.push_str("\\'"),
            ':' => escaped.push_str("\\:"),
            ',' => escaped.push_str("\\,"),
            '[' => escaped.push_str("\\["),
            ']' => escaped.push_str("\\]"),
            '\r' | '\n' => escaped.push(' '),
            _ => escaped.push(ch),
        }
    }

    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub_stream() -> Stream {
        Stream {
            name: "sub",
            rtsp_path: "live_sub".to_string(),
            width: 426,
            height: 240,
            fps: 24,
            bitrate_kbps: 512,
            rtp_port: 6004,
            codec: "libx264".to_string(),
            preset: None,
            audio: None,
        }
    }

    #[test]
    fn audio_output_encodes_g711_and_g722_at_their_rates() {
        use crate::config::AudioCodec;
        let args = |codec| {
            audio_output_args(
                2,
                &AudioStream {
                    codec,
                    rtp_port: 8004,
                },
                11,
                "",
            )
            .join(" ")
        };
        assert_eq!(
            args(AudioCodec::Pcmu),
            "-map 2:a:0 -vn -af aformat=sample_fmts=s16:sample_rates=8000:channel_layouts=mono -c:a pcm_mulaw -ar 8000 -ac 1 -f rtp -ssrc 11 rtp://127.0.0.1:8004?rtcpport=8005&pkt_size=1200"
        );
        assert!(args(AudioCodec::Pcma).contains("-c:a pcm_alaw -ar 8000"));
        assert!(args(AudioCodec::G722).contains("-c:a g722 -ar 16000"));
    }

    fn encoder_args(codec: &str, preset: Option<&str>) -> Vec<String> {
        let mut command = Command::new("ffmpeg");
        add_encoder_args(&mut command, codec, preset);
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn libx265_repeats_headers_in_band() {
        let args = encoder_args("libx265", None).join(" ");
        assert!(args.starts_with("-c:v libx265 "), "{args}");
        assert!(args.contains("-profile:v main"), "{args}");
        assert!(args.contains("-preset veryfast"), "{args}");
        assert!(
            args.contains("-x265-params repeat-headers=1:open-gop=0"),
            "{args}"
        );
        assert!(!args.contains("baseline"), "{args}");
    }

    #[test]
    fn libx265_honours_preset() {
        let args = encoder_args("libx265", Some("ultrafast")).join(" ");
        assert!(args.contains("-preset ultrafast"), "{args}");
    }

    #[test]
    fn free_port_pairs_are_distinct_across_calls() {
        let pairs: Vec<u16> = (0..20).filter_map(|_| free_local_port_pair()).collect();
        assert_eq!(pairs.len(), 20);
        for (i, a) in pairs.iter().enumerate() {
            for b in &pairs[i + 1..] {
                assert!(
                    a.abs_diff(*b) >= 2,
                    "overlapping pairs {a} and {b}: {pairs:?}"
                );
            }
        }
    }

    #[test]
    fn filter_graph_splits_after_overlay() {
        let graph = split_filter_graph(
            &["scale=1280:720".to_string(), "drawtext=x".to_string()],
            &sub_stream(),
        );
        assert_eq!(
            graph,
            "[0:v]scale=1280:720,drawtext=x,split=2[main][subraw];[subraw]scale=426:240,fps=24[sub]"
        );
    }

    #[test]
    fn separate_sub_input_is_not_split_or_scaled_from_main() {
        let graph = separate_sub_graph(
            &["scale=1280:720".to_string(), "drawtext=x".to_string()],
            &sub_stream(),
            Some("drawtext=small"),
        );
        assert_eq!(
            graph,
            "[0:v]scale=1280:720,drawtext=x[main];[1:v]scale=426:240,fps=24,drawtext=small[sub]"
        );
        assert_eq!(
            separate_sub_graph(&[], &sub_stream(), None),
            "[0:v]null[main];[1:v]scale=426:240,fps=24[sub]"
        );
    }

    #[test]
    fn sub_overlay_font_follows_the_height_ratio() {
        let main = Stream {
            height: 1080,
            ..sub_stream()
        };
        let sub = sub_stream();
        assert_eq!(scaled_font_size(32, &main, &sub), 8);
        assert_eq!(scaled_font_size(96, &main, &sub), 21);
    }

    #[test]
    fn filter_graph_without_prefix_filters() {
        let graph = split_filter_graph(&[], &sub_stream());
        assert_eq!(
            graph,
            "[0:v]split=2[main][subraw];[subraw]scale=426:240,fps=24[sub]"
        );
    }
}
