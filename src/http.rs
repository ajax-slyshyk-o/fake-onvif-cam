use crate::config::Config;
use crate::onvif;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const SNAPSHOT_JPEG: &[u8] = &[
    0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10, 0x4a, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x01, 0x00, 0x48,
    0x00, 0x48, 0x00, 0x00, 0xff, 0xdb, 0x00, 0x43, 0x00, 0x03, 0x02, 0x02, 0x03, 0x02, 0x02, 0x03,
    0x03, 0x03, 0x03, 0x04, 0x03, 0x03, 0x04, 0x05, 0x08, 0x05, 0x05, 0x04, 0x04, 0x05, 0x0a, 0x07,
    0x07, 0x06, 0x08, 0x0c, 0x0a, 0x0c, 0x0c, 0x0b, 0x0a, 0x0b, 0x0b, 0x0d, 0x0e, 0x12, 0x10, 0x0d,
    0x0e, 0x11, 0x0e, 0x0b, 0x0b, 0x10, 0x16, 0x10, 0x11, 0x13, 0x14, 0x15, 0x15, 0x15, 0x0c, 0x0f,
    0x17, 0x18, 0x16, 0x14, 0x18, 0x12, 0x14, 0x15, 0x14, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xff, 0xc4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xff, 0xc4, 0x00, 0x14,
    0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0xff, 0xda, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3f, 0x00, 0x37, 0xff, 0xd9,
];

struct Request {
    method: String,
    path: String,
    body: String,
}

pub fn serve(config: Arc<Config>) -> io::Result<()> {
    let listener = TcpListener::bind(config.http_addr)?;
    println!("http: listening on http://{}", config.http_addr);

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("http accept failed: {err}");
                continue;
            }
        };

        let config = config.clone();
        thread::Builder::new()
            .name("onvif-http-client".to_string())
            .spawn(move || {
                if let Err(err) = handle_client(stream, config) {
                    eprintln!("http client failed: {err}");
                }
            })?;
    }

    Ok(())
}

fn handle_client(mut stream: TcpStream, config: Arc<Config>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = read_request(&stream)?;
    let path = request.path.split('?').next().unwrap_or(&request.path);

    let response = match (request.method.as_str(), path) {
        ("GET", "/") => text_response(200, status_text(200), landing_text(&config)),
        ("GET", "/snapshot.jpg") => binary_response(200, "image/jpeg", SNAPSHOT_JPEG),
        ("POST", "/onvif/device_service") | ("POST", "/onvif/media_service") => {
            soap_response_for(&request.body, &config)
        }
        ("GET", "/onvif/device_service") | ("GET", "/onvif/media_service") => {
            text_response(200, status_text(200), landing_text(&config))
        }
        _ => text_response(404, status_text(404), "not found\n".to_string()),
    };

    stream.write_all(&response)?;
    stream.flush()?;
    Ok(())
}

fn read_request(stream: &TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    if request_line.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty request"));
    }

    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing path"))?
        .to_string();

    let mut content_length = 0_usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }

    let mut body_bytes = vec![0_u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_bytes)?;
    }

    Ok(Request {
        method,
        path,
        body: String::from_utf8_lossy(&body_bytes).to_string(),
    })
}

fn soap_response_for(body: &str, config: &Config) -> Vec<u8> {
    let operation = onvif::detect_operation(body);
    let xml = onvif::soap_response(operation.as_deref(), config);
    binary_response(200, "application/soap+xml; charset=utf-8", xml.as_bytes())
}

fn landing_text(config: &Config) -> String {
    format!(
        "fake-onvif-cam\n\nDevice service: {}\nMedia service: {}\nRTSP stream: {}\nSnapshot: {}\n",
        onvif::device_xaddr(config),
        onvif::media_xaddr(config),
        onvif::rtsp_uri(config),
        onvif::snapshot_uri(config)
    )
}

fn text_response(status: u16, reason: &str, body: String) -> Vec<u8> {
    binary_response_with_reason(status, reason, "text/plain; charset=utf-8", body.as_bytes())
}

fn binary_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    binary_response_with_reason(status, status_text(status), content_type, body)
}

fn binary_response_with_reason(
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        _ => "OK",
    }
}
