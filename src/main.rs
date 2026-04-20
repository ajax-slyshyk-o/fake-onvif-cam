mod config;
mod discovery;
mod ffmpeg;
mod http;
mod onvif;
mod rtsp;
mod util;

use std::process;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

fn main() {
    let configs = match config::load_from_env() {
        Ok(configs) => configs,
        Err(err) => {
            eprintln!("error: {err}");
            process::exit(2);
        }
    };
    let configs: Vec<_> = configs.into_iter().map(Arc::new).collect();

    println!("fake-onvif-cam {}", env!("CARGO_PKG_VERSION"));
    println!("cameras: {}", configs.len());

    let discovery_configs: Vec<_> = configs
        .iter()
        .filter(|config| !config.no_discovery)
        .cloned()
        .collect();

    if !discovery_configs.is_empty() {
        match discovery::spawn(discovery_configs) {
            Ok(()) => println!("ws-discovery: listening on udp://0.0.0.0:3702"),
            Err(err) => eprintln!("ws-discovery disabled: {err}"),
        }
    }

    let (fatal_tx, fatal_rx) = mpsc::channel();
    let mut rtsp_servers = Vec::new();
    let mut ffmpeg_processes = Vec::new();
    let mut http_threads = Vec::new();

    for config in &configs {
        println!();
        println!("camera: {}", config.camera_name);
        println!("uuid: {}", config.uuid);
        println!("device service: {}", onvif::device_xaddr(config));
        println!("media service: {}", onvif::media_xaddr(config));
        println!("rtsp stream: {}", onvif::rtsp_uri(config));

        match rtsp::spawn(config.clone()) {
            Ok(server) => {
                println!(
                    "rtsp: listening on rtsp://0.0.0.0:{}/{}",
                    config.rtsp_port, config.rtsp_path
                );
                rtsp_servers.push(server);
            }
            Err(err) => {
                eprintln!(
                    "failed to start rtsp service for {}: {err}",
                    config.camera_name
                );
                process::exit(1);
            }
        }

        match ffmpeg::start(config) {
            Ok(child) => ffmpeg_processes.push(child),
            Err(err) => {
                eprintln!("failed to start ffmpeg for {}: {err}", config.camera_name);
                eprintln!("start with --no-ffmpeg if you only want the ONVIF endpoints");
                process::exit(1);
            }
        }

        let http_config = config.clone();
        let camera_name = config.camera_name.clone();
        let fatal_tx = fatal_tx.clone();
        match thread::Builder::new()
            .name(format!("onvif-http-{}", camera_name))
            .spawn(move || {
                if let Err(err) = http::serve(http_config) {
                    let _ = fatal_tx.send(format!("http service for {camera_name} failed: {err}"));
                }
            }) {
            Ok(thread) => http_threads.push(thread),
            Err(err) => {
                eprintln!(
                    "failed to start http service for {}: {err}",
                    config.camera_name
                );
                process::exit(1);
            }
        }
    }

    drop(fatal_tx);

    if let Ok(message) = fatal_rx.recv() {
        eprintln!("{message}");
        process::exit(1);
    }
}
