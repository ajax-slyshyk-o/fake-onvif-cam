use crate::config::Config;
use crate::onvif;
use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::Arc;
use std::thread;

const WS_DISCOVERY_ADDR: &str = "0.0.0.0:3702";
const WS_DISCOVERY_MULTICAST: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);

pub fn spawn(configs: Vec<Arc<Config>>) -> io::Result<()> {
    let socket = UdpSocket::bind(WS_DISCOVERY_ADDR)?;
    socket.join_multicast_v4(&WS_DISCOVERY_MULTICAST, &Ipv4Addr::UNSPECIFIED)?;

    thread::Builder::new()
        .name("ws-discovery".to_string())
        .spawn(move || run(socket, configs))?;

    Ok(())
}

fn run(socket: UdpSocket, configs: Vec<Arc<Config>>) {
    let mut buffer = [0_u8; 16 * 1024];

    loop {
        let (length, peer) = match socket.recv_from(&mut buffer) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("ws-discovery receive failed: {err}");
                continue;
            }
        };

        let request = String::from_utf8_lossy(&buffer[..length]);
        if !looks_like_probe(&request) {
            continue;
        }

        let relates_to =
            extract_tag_text(&request, "MessageID").unwrap_or_else(|| "uuid:unknown".to_string());

        for config in &configs {
            let response = onvif::discovery_probe_match(config, &relates_to);

            if let Err(err) = socket.send_to(response.as_bytes(), peer) {
                eprintln!(
                    "ws-discovery response for {} to {peer} failed: {err}",
                    config.camera_name
                );
            }
        }
    }
}

fn looks_like_probe(request: &str) -> bool {
    request.contains(":Probe") || request.contains("<Probe")
}

fn extract_tag_text(xml: &str, local_name: &str) -> Option<String> {
    let needle = format!(":{local_name}>");
    if let Some(end) = xml.find(&needle) {
        let before_end = &xml[..end];
        let start = before_end.rfind('>')?;
        return Some(before_end[start + 1..].trim().to_string());
    }

    let plain_open = format!("<{local_name}>");
    let plain_close = format!("</{local_name}>");
    let start = xml.find(&plain_open)? + plain_open.len();
    let end = xml[start..].find(&plain_close)? + start;
    Some(xml[start..end].trim().to_string())
}
