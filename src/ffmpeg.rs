use crate::config::{Config, MediaInput, Stream};
use std::io;
use std::net::UdpSocket;
use std::path::Path;
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

    let mut child = spawn_child(config)?;
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

    // One process and one decode for all streams: the (overlaid) video is split and the
    // sub stream is scaled down from the copy.
    let streams = config.streams();
    match streams.as_slice() {
        [main] => {
            if !vfilters.is_empty() {
                command.arg("-vf").arg(vfilters.join(","));
            }
            add_output(&mut command, main, 1);
        }
        [main, sub] => {
            command
                .arg("-filter_complex")
                .arg(split_filter_graph(&vfilters, sub));
            command.arg("-map").arg("[main]");
            add_output(&mut command, main, 1);
            command.arg("-map").arg("[sub]");
            add_output(&mut command, sub, 2);
        }
        _ => return Err(io::Error::other("unsupported number of streams")),
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    command.spawn()
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

/// Output options for one stream: encoder, rate control and the RTP destination.
fn add_output(command: &mut Command, stream: &Stream, ssrc: u32) {
    // Without explicit local ports ffmpeg binds an ephemeral RTP port and then RTCP at +1
    // blindly, which fails with "bind failed" (WSAEADDRINUSE on Windows) when many ffmpeg
    // processes start at once.
    let local = free_local_port_pair()
        .map(|port| format!("&localport={}&localrtcpport={}", port, port + 1))
        .unwrap_or_default();
    let rtp_url = format!(
        "rtp://127.0.0.1:{}?rtcpport={}&pkt_size=1200{local}",
        stream.rtp_port,
        stream.rtp_port + 1
    );
    let bitrate = format!("{}k", stream.bitrate_kbps);
    let bufsize = format!("{}k", stream.bitrate_kbps.saturating_mul(2));

    command.arg("-an");
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
        .arg(stream.fps.to_string())
        .arg("-f")
        .arg("rtp")
        .arg("-payload_type")
        .arg("96")
        .arg("-ssrc")
        .arg(ssrc.to_string())
        .arg(rtp_url);
}

/// Encoder-specific arguments. The RTSP SDP carries no sprop-parameter-sets, so every
/// encoder must repeat SPS/PPS in-band on keyframes.
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
        }
    }

    #[test]
    fn free_port_pairs_are_distinct_across_calls() {
        let pairs: Vec<u16> = (0..20).filter_map(|_| free_local_port_pair()).collect();
        assert_eq!(pairs.len(), 20);
        for (i, a) in pairs.iter().enumerate() {
            for b in &pairs[i + 1..] {
                assert!(a.abs_diff(*b) >= 2, "overlapping pairs {a} and {b}: {pairs:?}");
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
    fn filter_graph_without_prefix_filters() {
        let graph = split_filter_graph(&[], &sub_stream());
        assert_eq!(
            graph,
            "[0:v]split=2[main][subraw];[subraw]scale=426:240,fps=24[sub]"
        );
    }
}
