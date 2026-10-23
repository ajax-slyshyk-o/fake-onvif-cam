use crate::config::{Config, Stream};
use crate::onvif;
use crate::util;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

type Clients = Arc<Mutex<Vec<Client>>>;

pub struct RtspServerGuard {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl Drop for RtspServerGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[derive(Clone)]
struct Client {
    id: u64,
    playing: Arc<AtomicBool>,
    sink: ClientSink,
}

#[derive(Clone)]
enum ClientSink {
    Tcp {
        packet_tx: SyncSender<TcpPacket>,
        rtp_channel: u8,
        rtcp_channel: u8,
    },
    Udp {
        rtp_socket: Arc<UdpSocket>,
        rtcp_socket: Arc<UdpSocket>,
        rtp_peer: SocketAddr,
        rtcp_peer: SocketAddr,
    },
}

struct TcpPacket {
    channel: u8,
    data: Vec<u8>,
}

/// Per-stream relay state: the stream description, its RTP/RTCP ingest sockets (fed by
/// ffmpeg) and the RTSP clients currently subscribed to it.
struct StreamRelay {
    stream: Stream,
    clients: Clients,
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Arc<UdpSocket>,
}

struct RtspRequest {
    method: String,
    uri: String,
    headers: HashMap<String, String>,
}

impl RtspRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn cseq(&self) -> String {
        self.header("CSeq").unwrap_or("1").to_string()
    }
}

pub fn spawn(config: Arc<Config>) -> io::Result<RtspServerGuard> {
    let listener = TcpListener::bind(("0.0.0.0", config.rtsp_port))?;
    listener.set_nonblocking(true)?;

    let stop = Arc::new(AtomicBool::new(false));
    let mut relays = Vec::new();
    let mut workers = Vec::new();

    for stream in config.streams() {
        let rtp_socket = Arc::new(UdpSocket::bind(("127.0.0.1", stream.rtp_port))?);
        let rtcp_socket = Arc::new(UdpSocket::bind(("127.0.0.1", stream.rtp_port + 1))?);
        rtp_socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        rtcp_socket.set_read_timeout(Some(Duration::from_millis(500)))?;

        let clients: Clients = Arc::new(Mutex::new(Vec::new()));
        workers.push(spawn_relay_loop(
            &format!("rtp-relay-{}", stream.name),
            rtp_socket.clone(),
            clients.clone(),
            stop.clone(),
            false,
        )?);
        workers.push(spawn_relay_loop(
            &format!("rtcp-relay-{}", stream.name),
            rtcp_socket.clone(),
            clients.clone(),
            stop.clone(),
            true,
        )?);
        relays.push(Arc::new(StreamRelay {
            stream,
            clients,
            rtp_socket,
            rtcp_socket,
        }));
    }

    workers.push(spawn_accept_loop(
        listener,
        config,
        Arc::new(relays),
        stop.clone(),
    )?);

    Ok(RtspServerGuard { stop, workers })
}

fn spawn_accept_loop(
    listener: TcpListener,
    config: Arc<Config>,
    relays: Arc<Vec<Arc<StreamRelay>>>,
    stop: Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name("rtsp-accept".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        let config = config.clone();
                        let relays = relays.clone();

                        let _ = thread::Builder::new()
                            .name("rtsp-client".to_string())
                            .spawn(move || {
                                if let Err(err) = handle_client(stream, peer, config, relays) {
                                    eprintln!("rtsp client {peer} failed: {err}");
                                }
                            });
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(err) => {
                        eprintln!("rtsp accept failed: {err}");
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        })
}

fn spawn_relay_loop(
    name: &str,
    socket: Arc<UdpSocket>,
    clients: Clients,
    stop: Arc<AtomicBool>,
    is_rtcp: bool,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || relay_loop(socket, clients, stop, is_rtcp))
}

fn relay_loop(socket: Arc<UdpSocket>, clients: Clients, stop: Arc<AtomicBool>, is_rtcp: bool) {
    let mut buffer = vec![0_u8; 65_536];

    while !stop.load(Ordering::Relaxed) {
        let length = match socket.recv_from(&mut buffer) {
            Ok((length, _)) => length,
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(err) => {
                eprintln!("rtsp relay receive failed: {err}");
                continue;
            }
        };

        let failed_clients = broadcast_packet(&clients, &buffer[..length], is_rtcp);
        if !failed_clients.is_empty() {
            remove_clients(&clients, &failed_clients);
        }
    }
}

fn broadcast_packet(clients: &Clients, packet: &[u8], is_rtcp: bool) -> Vec<u64> {
    let snapshot = match clients.lock() {
        Ok(clients) => clients.clone(),
        Err(_) => return Vec::new(),
    };
    let mut failed = Vec::new();

    for client in snapshot {
        if !client.playing.load(Ordering::Relaxed) {
            continue;
        }

        let result = match &client.sink {
            ClientSink::Tcp {
                packet_tx,
                rtp_channel,
                rtcp_channel,
            } => {
                let channel = if is_rtcp { *rtcp_channel } else { *rtp_channel };
                packet_tx
                    .try_send(TcpPacket {
                        channel,
                        data: packet.to_vec(),
                    })
                    .map_err(|err| match err {
                        TrySendError::Full(_) => io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "RTSP client packet queue full",
                        ),
                        TrySendError::Disconnected(_) => io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "RTSP client packet writer disconnected",
                        ),
                    })
            }
            ClientSink::Udp {
                rtp_socket,
                rtcp_socket,
                rtp_peer,
                rtcp_peer,
            } => {
                let socket = if is_rtcp { rtcp_socket } else { rtp_socket };
                let peer = if is_rtcp { rtcp_peer } else { rtp_peer };
                socket.send_to(packet, peer).map(|_| ())
            }
        };

        if result.is_err() {
            failed.push(client.id);
        }
    }

    failed
}

fn handle_client(
    stream: TcpStream,
    peer: SocketAddr,
    config: Arc<Config>,
    relays: Arc<Vec<Arc<StreamRelay>>>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(90)))?;
    stream.set_nodelay(true)?;

    let read_stream = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let mut reader = BufReader::new(read_stream);
    let session = util::make_uuid();
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    let playing = Arc::new(AtomicBool::new(false));
    // Stream this connection is subscribed to (index into `relays`), set by SETUP.
    let mut subscribed: Option<usize> = None;

    while let Some(request) = read_request(&mut reader)? {
        let cseq = request.cseq();

        match request.method.as_str() {
            "OPTIONS" => write_response(
                &writer,
                200,
                "OK",
                &cseq,
                &[(
                    "Public",
                    "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, GET_PARAMETER, TEARDOWN".to_string(),
                )],
                &[],
            )?,
            "DESCRIBE" => {
                let Some(index) = find_stream(&relays, &request.uri) else {
                    write_response(&writer, 404, "Not Found", &cseq, &[], &[])?;
                    continue;
                };
                let stream = &relays[index].stream;
                let body = sdp(&config, stream);
                write_response(
                    &writer,
                    200,
                    "OK",
                    &cseq,
                    &[
                        (
                            "Content-Base",
                            format!("{}/", onvif::rtsp_uri(&config, stream)),
                        ),
                        ("Content-Type", "application/sdp".to_string()),
                    ],
                    body.as_bytes(),
                )?;
            }
            "SETUP" => {
                let Some(index) = find_stream(&relays, &request.uri) else {
                    write_response(&writer, 404, "Not Found", &cseq, &[], &[])?;
                    continue;
                };
                let relay = &relays[index];
                let transport = request.header("Transport").unwrap_or("");
                let Some((sink, response_transport)) = setup_transport(
                    transport,
                    peer,
                    writer.clone(),
                    relay.rtp_socket.clone(),
                    relay.rtcp_socket.clone(),
                    &relay.stream,
                ) else {
                    write_response(&writer, 461, "Unsupported Transport", &cseq, &[], &[])?;
                    continue;
                };

                if let Some(previous) = subscribed.filter(|previous| *previous != index) {
                    remove_clients(&relays[previous].clients, &[client_id]);
                }
                subscribed = Some(index);
                replace_client(
                    &relay.clients,
                    Client {
                        id: client_id,
                        playing: playing.clone(),
                        sink,
                    },
                );
                write_response(
                    &writer,
                    200,
                    "OK",
                    &cseq,
                    &[
                        ("Transport", response_transport),
                        ("Session", format!("{session};timeout=60")),
                    ],
                    &[],
                )?;
            }
            "PLAY" => {
                playing.store(true, Ordering::Relaxed);
                let stream = &relays[subscribed.unwrap_or(0)].stream;
                write_response(
                    &writer,
                    200,
                    "OK",
                    &cseq,
                    &[
                        ("Session", session.clone()),
                        (
                            "RTP-Info",
                            format!(
                                "url={}/trackID=0;seq=0;rtptime=0",
                                onvif::rtsp_uri(&config, stream)
                            ),
                        ),
                    ],
                    &[],
                )?;
            }
            "PAUSE" => {
                playing.store(false, Ordering::Relaxed);
                write_response(
                    &writer,
                    200,
                    "OK",
                    &cseq,
                    &[("Session", session.clone())],
                    &[],
                )?;
            }
            "GET_PARAMETER" => write_response(
                &writer,
                200,
                "OK",
                &cseq,
                &[("Session", session.clone())],
                &[],
            )?,
            "TEARDOWN" => {
                playing.store(false, Ordering::Relaxed);
                write_response(
                    &writer,
                    200,
                    "OK",
                    &cseq,
                    &[("Session", session.clone())],
                    &[],
                )?;
                break;
            }
            _ => write_response(&writer, 501, "Not Implemented", &cseq, &[], &[])?,
        }
    }

    if let Some(index) = subscribed {
        remove_clients(&relays[index].clients, &[client_id]);
    }
    Ok(())
}

/// Picks the stream addressed by an RTSP request URI. A single-stream camera keeps accepting
/// any path (as it always did); with several streams an unknown path is a 404, like a real camera.
fn find_stream(relays: &[Arc<StreamRelay>], uri: &str) -> Option<usize> {
    if relays.len() == 1 {
        return Some(0);
    }

    let path = uri_path(uri);
    relays.iter().position(|relay| {
        path == relay.stream.rtsp_path || path.starts_with(&format!("{}/", relay.stream.rtsp_path))
    })
}

/// Path part of an RTSP URI, without leading/trailing slashes, query or fragment.
fn uri_path(uri: &str) -> &str {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let path = rest.find('/').map_or("", |index| &rest[index..]);
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.trim_matches('/')
}

fn setup_transport(
    transport: &str,
    peer: SocketAddr,
    writer: Arc<Mutex<TcpStream>>,
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Arc<UdpSocket>,
    stream: &Stream,
) -> Option<(ClientSink, String)> {
    if transport.to_ascii_uppercase().contains("RTP/AVP/TCP")
        || transport.to_ascii_lowercase().contains("interleaved")
    {
        let (rtp_channel, rtcp_channel) = parse_interleaved(transport).unwrap_or((0, 1));
        let (packet_tx, packet_rx) = mpsc::sync_channel::<TcpPacket>(4096);
        let packet_writer = writer.clone();
        let _ = thread::Builder::new()
            .name("rtsp-tcp-writer".to_string())
            .spawn(move || {
                while let Ok(packet) = packet_rx.recv() {
                    if write_interleaved(&packet_writer, packet.channel, &packet.data).is_err() {
                        break;
                    }
                }
            });

        return Some((
            ClientSink::Tcp {
                packet_tx,
                rtp_channel,
                rtcp_channel,
            },
            format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}"),
        ));
    }

    let (client_rtp, client_rtcp) = parse_client_ports(transport)?;
    let rtp_peer = SocketAddr::new(peer.ip(), client_rtp);
    let rtcp_peer = SocketAddr::new(peer.ip(), client_rtcp);

    Some((
        ClientSink::Udp {
            rtp_socket,
            rtcp_socket,
            rtp_peer,
            rtcp_peer,
        },
        format!(
            "RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp};server_port={}-{}",
            stream.rtp_port,
            stream.rtp_port + 1
        ),
    ))
}

fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<RtspRequest>> {
    let mut request_line = String::new();

    loop {
        request_line.clear();
        let read = reader.read_line(&mut request_line)?;
        if read == 0 {
            return Ok(None);
        }

        if !request_line.trim().is_empty() {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing RTSP method"))?
        .to_string();
    let uri = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing RTSP URI"))?
        .to_string();

    let mut headers = HashMap::new();
    let mut content_length = 0_usize;

    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 || line == "\r\n" || line == "\n" {
            break;
        }

        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim().to_string();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name.trim().to_string(), value);
        }
    }

    if content_length > 0 {
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body)?;
    }

    Ok(Some(RtspRequest {
        method,
        uri,
        headers,
    }))
}

fn write_response(
    writer: &Arc<Mutex<TcpStream>>,
    status: u16,
    reason: &str,
    cseq: &str,
    headers: &[(&str, String)],
    body: &[u8],
) -> io::Result<()> {
    let mut response =
        format!("RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\nServer: fake-onvif-cam\r\n");

    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }

    response.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

    let mut stream = lock_stream(writer)?;
    stream.write_all(response.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_interleaved(writer: &Arc<Mutex<TcpStream>>, channel: u8, packet: &[u8]) -> io::Result<()> {
    if packet.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "RTP packet too large for RTSP interleaving",
        ));
    }

    let length = packet.len() as u16;
    let header = [b'$', channel, (length >> 8) as u8, length as u8];
    let mut stream = lock_stream(writer)?;
    stream.write_all(&header)?;
    stream.write_all(packet)
}

fn lock_stream(writer: &Arc<Mutex<TcpStream>>) -> io::Result<std::sync::MutexGuard<'_, TcpStream>> {
    writer
        .lock()
        .map_err(|_| io::Error::other("RTSP stream lock poisoned"))
}

fn replace_client(clients: &Clients, client: Client) {
    if let Ok(mut clients) = clients.lock() {
        clients.retain(|existing| existing.id != client.id);
        clients.push(client);
    }
}

fn remove_clients(clients: &Clients, ids: &[u64]) {
    if let Ok(mut clients) = clients.lock() {
        clients.retain(|client| !ids.contains(&client.id));
    }
}

fn parse_interleaved(transport: &str) -> Option<(u8, u8)> {
    for part in transport.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name.eq_ignore_ascii_case("interleaved") {
            let (rtp, rtcp) = value.split_once('-')?;
            return Some((rtp.parse().ok()?, rtcp.parse().ok()?));
        }
    }

    None
}

fn parse_client_ports(transport: &str) -> Option<(u16, u16)> {
    for part in transport.split(';') {
        let Some((name, value)) = part.trim().split_once('=') else {
            continue;
        };
        if name.eq_ignore_ascii_case("client_port") {
            let (rtp, rtcp) = value.split_once('-')?;
            return Some((rtp.parse().ok()?, rtcp.parse().ok()?));
        }
    }

    None
}

fn sdp(config: &Config, stream: &Stream) -> String {
    let name = config
        .camera_name
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string();

    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 0.0.0.0\r\n\
         s={name}\r\n\
         c=IN IP4 0.0.0.0\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         a=rtpmap:96 H264/90000\r\n\
         a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n\
         a=framerate:{}\r\n\
         a=control:trackID=0\r\n",
        stream.fps
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relay(path: &str, port: u16) -> Arc<StreamRelay> {
        Arc::new(StreamRelay {
            stream: Stream {
                name: "x",
                rtsp_path: path.to_string(),
                width: 2,
                height: 2,
                fps: 1,
                bitrate_kbps: 1,
                rtp_port: port,
                codec: String::new(),
                preset: None,
            },
            clients: Arc::new(Mutex::new(Vec::new())),
            rtp_socket: Arc::new(UdpSocket::bind(("127.0.0.1", 0)).unwrap()),
            rtcp_socket: Arc::new(UdpSocket::bind(("127.0.0.1", 0)).unwrap()),
        })
    }

    #[test]
    fn uri_path_strips_scheme_host_query_and_slashes() {
        assert_eq!(uri_path("rtsp://10.0.0.1:8554/live/"), "live");
        assert_eq!(
            uri_path("rtsp://10.0.0.1:8554/live_sub/trackID=0"),
            "live_sub/trackID=0"
        );
        assert_eq!(uri_path("rtsp://h/live?x=1"), "live");
        assert_eq!(uri_path("rtsp://h"), "");
    }

    #[test]
    fn routes_by_path_and_rejects_unknown_when_multi_stream() {
        let relays = [relay("live", 1), relay("live_sub", 3)];
        assert_eq!(find_stream(&relays, "rtsp://h:1/live"), Some(0));
        assert_eq!(find_stream(&relays, "rtsp://h:1/live/trackID=0"), Some(0));
        assert_eq!(find_stream(&relays, "rtsp://h:1/live_sub"), Some(1));
        assert_eq!(
            find_stream(&relays, "rtsp://h:1/live_sub/trackID=0"),
            Some(1)
        );
        assert_eq!(find_stream(&relays, "rtsp://h:1/other"), None);
    }

    #[test]
    fn transport_parsers_skip_parts_without_equals() {
        assert_eq!(
            parse_interleaved("RTP/AVP/TCP;unicast;interleaved=2-3"),
            Some((2, 3))
        );
        assert_eq!(
            parse_client_ports("RTP/AVP/UDP;unicast;client_port=5000-5001"),
            Some((5000, 5001))
        );
        assert_eq!(parse_client_ports("RTP/AVP/UDP;unicast"), None);
    }

    #[test]
    fn single_stream_accepts_any_path() {
        assert_eq!(
            find_stream(&[relay("live", 1)], "rtsp://h:1/whatever"),
            Some(0)
        );
    }
}
