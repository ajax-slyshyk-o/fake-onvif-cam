use crate::config::{Config, MediaInput};
use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

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
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("ffmpeg exited immediately with status {status}"),
        ));
    }

    println!(
        "ffmpeg: sending RTP to 127.0.0.1:{} and RTCP to 127.0.0.1:{}",
        config.rtp_port,
        config.rtp_port + 1
    );

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
    let rtp_url = format!(
        "rtp://127.0.0.1:{}?rtcpport={}&pkt_size=1200",
        config.rtp_port,
        config.rtp_port + 1
    );
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

    if let Some(text) = &config.overlay_text {
        command.arg("-vf").arg(drawtext_filter(
            text,
            config.overlay_font.as_deref(),
            config.overlay_font_size,
        ));
    }

    let keyframe_interval = config.fps.to_string();
    command
        .arg("-an")
        .arg("-c:v")
        .arg("libx264")
        .arg("-profile:v")
        .arg("baseline")
        .arg("-preset")
        .arg("veryfast")
        .arg("-tune")
        .arg("zerolatency")
        .arg("-x264-params")
        .arg("repeat-headers=1")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-bf")
        .arg("0")
        .arg("-b:v")
        .arg("2500k")
        .arg("-maxrate")
        .arg("2500k")
        .arg("-bufsize")
        .arg("5000k")
        .arg("-g")
        .arg(keyframe_interval)
        .arg("-f")
        .arg("rtp")
        .arg("-payload_type")
        .arg("96")
        .arg("-ssrc")
        .arg("1")
        .arg(rtp_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    command.spawn()
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
