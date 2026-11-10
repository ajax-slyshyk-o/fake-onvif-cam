use crate::auth;
use crate::config::{Config, Stream, VideoFormat};
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

/// Packets queued per TCP client before it counts as too slow (about 5 s of a 4 Mbit/s stream).
const CLIENT_QUEUE_PACKETS: usize = 2048;

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
    /// Set while this client cannot keep up (its TCP queue overflowed): video packets are
    /// dropped until the next keyframe, so what it receives next is decodable again.
    lagging: Arc<AtomicBool>,
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

/// One RTP/RTCP track of a stream (video or audio): its ingest sockets, fed by ffmpeg, and the
/// RTSP clients subscribed to it.
#[derive(Clone)]
struct TrackRelay {
    clients: Clients,
    rtp_socket: Arc<UdpSocket>,
    rtcp_socket: Arc<UdpSocket>,
    rtp_port: u16,
}

/// Per-stream relay state: the stream description, its RTP/RTCP ingest sockets (fed by
/// ffmpeg) and the RTSP clients currently subscribed to it.
struct StreamRelay {
    stream: Stream,
    video: TrackRelay,
    audio: Option<TrackRelay>,
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

/// The ingest side of one ffmpeg process: the UDP sockets it publishes RTP/RTCP to, the relay
/// threads reading them and the subscribed RTSP clients of every track. Cameras that share one
/// ffmpeg process share one `Source`, so a client of any of them gets the same packets.
pub struct Source {
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    /// One entry per stream (main, then sub): the video track and the optional audio track.
    tracks: Vec<(TrackRelay, Option<TrackRelay>)>,
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// Binds the ingest ports of `config`'s streams (the RTP ports ffmpeg sends to) and starts
/// relaying them.
pub fn spawn_source(config: &Config) -> io::Result<Source> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    let mut tracks = Vec::new();

    for stream in config.streams() {
        let video = spawn_track(
            stream.name,
            stream.rtp_port,
            Some(stream.format()),
            &stop,
            &mut workers,
        )?;
        let audio = stream
            .audio
            .map(|audio| {
                spawn_track(
                    &format!("{}-audio", stream.name),
                    audio.rtp_port,
                    None,
                    &stop,
                    &mut workers,
                )
            })
            .transpose()?;
        tracks.push((video, audio));
    }

    Ok(Source {
        stop,
        workers,
        tracks,
    })
}

/// Starts the RTSP server of one camera. Its streams are fed by `source`, which must have the
/// same stream layout (one video track per stream, audio where the stream has audio).
pub fn spawn(config: Arc<Config>, source: &Source) -> io::Result<RtspServerGuard> {
    let streams = config.streams();
    if streams.len() != source.tracks.len() {
        return Err(io::Error::other(format!(
            "camera {} has {} streams but its ffmpeg source has {}",
            config.camera_name,
            streams.len(),
            source.tracks.len()
        )));
    }

    let listener = TcpListener::bind(("0.0.0.0", config.rtsp_port))?;
    listener.set_nonblocking(true)?;

    let stop = Arc::new(AtomicBool::new(false));
    let relays: Vec<_> = streams
        .into_iter()
        .zip(&source.tracks)
        .map(|(stream, (video, audio))| {
            Arc::new(StreamRelay {
                audio: audio.clone().filter(|_| stream.audio.is_some()),
                video: video.clone(),
                stream,
            })
        })
        .collect();

    let workers = vec![spawn_accept_loop(
        listener,
        config,
        Arc::new(relays),
        stop.clone(),
    )?];

    Ok(RtspServerGuard { stop, workers })
}

/// Binds the ingest sockets of one track and starts its RTP and RTCP relay threads.
fn spawn_track(
    name: &str,
    rtp_port: u16,
    format: Option<VideoFormat>,
    stop: &Arc<AtomicBool>,
    workers: &mut Vec<JoinHandle<()>>,
) -> io::Result<TrackRelay> {
    let rtp_socket = Arc::new(UdpSocket::bind(("127.0.0.1", rtp_port))?);
    let rtcp_socket = Arc::new(UdpSocket::bind(("127.0.0.1", rtp_port + 1))?);
    rtp_socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    rtcp_socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    let clients: Clients = Arc::new(Mutex::new(Vec::new()));
    workers.push(spawn_relay_loop(
        &format!("rtp-relay-{name}"),
        rtp_socket.clone(),
        clients.clone(),
        stop.clone(),
        false,
        format,
    )?);
    workers.push(spawn_relay_loop(
        &format!("rtcp-relay-{name}"),
        rtcp_socket.clone(),
        clients.clone(),
        stop.clone(),
        true,
        format,
    )?);

    Ok(TrackRelay {
        clients,
        rtp_socket,
        rtcp_socket,
        rtp_port,
    })
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
    format: Option<VideoFormat>,
) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.to_string())
        .spawn(move || relay_loop(socket, clients, stop, is_rtcp, format))
}

fn relay_loop(
    socket: Arc<UdpSocket>,
    clients: Clients,
    stop: Arc<AtomicBool>,
    is_rtcp: bool,
    format: Option<VideoFormat>,
) {
    let mut buffer = vec![0_u8; 65_536];
    let port = socket.local_addr().map_or(0, |addr| addr.port());

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

        let failed_clients = broadcast_packet(&clients, &buffer[..length], is_rtcp, port, format);
        if !failed_clients.is_empty() {
            remove_clients(&clients, &failed_clients);
        }
    }
}

/// Sends one packet from ffmpeg to every playing client and returns the clients that are gone.
///
/// A TCP client that reads too slowly (its queue is full) is not dropped, because it would just
/// sit on an open connection without data until the NVR's watchdog notices. Like a real camera
/// under congestion it loses packets instead: for video everything up to the next keyframe, so
/// that what arrives afterwards decodes again.
fn broadcast_packet(
    clients: &Clients,
    packet: &[u8],
    is_rtcp: bool,
    track_port: u16,
    format: Option<VideoFormat>,
) -> Vec<u64> {
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
                let video = format.filter(|_| !is_rtcp);
                let lagging = client.lagging.load(Ordering::Relaxed);
                if lagging && !video.is_some_and(|format| starts_keyframe(format, packet)) {
                    continue;
                }

                let channel = if is_rtcp { *rtcp_channel } else { *rtp_channel };
                match packet_tx.try_send(TcpPacket {
                    channel,
                    data: packet.to_vec(),
                }) {
                    Ok(()) => {
                        if lagging {
                            client.lagging.store(false, Ordering::Relaxed);
                        }
                        Ok(())
                    }
                    Err(TrySendError::Full(_)) => {
                        if video.is_some() && !client.lagging.swap(true, Ordering::Relaxed) {
                            eprintln!(
                                "rtsp: client {} of track port {track_port} is not reading fast enough; dropping video until the next keyframe",
                                client.id
                            );
                        }
                        Ok(())
                    }
                    Err(TrySendError::Disconnected(_)) => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "RTSP client packet writer disconnected",
                    )),
                }
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

        if let Err(err) = result {
            eprintln!(
                "rtsp: dropping client {} of track port {track_port}: {err}",
                client.id
            );
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
    let lagging = Arc::new(AtomicBool::new(false));
    // Stream this connection is subscribed to (index into `relays`), set by SETUP, and which
    // of its tracks were set up.
    let mut subscribed: Option<usize> = None;
    let mut video_setup = false;
    let mut audio_setup = false;

    while let Some(request) = read_request(&mut reader)? {
        let cseq = request.cseq();

        // Everything except OPTIONS needs a login when the camera has credentials.
        if request.method != "OPTIONS"
            && !auth::check_rtsp(&config, request.header("Authorization"), &request.method)
        {
            write_response(
                &writer,
                401,
                "Unauthorized",
                &cseq,
                &[("WWW-Authenticate", auth::digest_challenge())],
                &[],
            )?;
            continue;
        }

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
                // `trackID=1` is the audio track; anything else is the video track.
                let wants_audio = is_audio_track(&request.uri);
                let track = if wants_audio {
                    let Some(audio) = &relay.audio else {
                        write_response(&writer, 404, "Not Found", &cseq, &[], &[])?;
                        continue;
                    };
                    audio
                } else {
                    &relay.video
                };
                let transport = request.header("Transport").unwrap_or("");
                let default_channels = if wants_audio { (2, 3) } else { (0, 1) };
                let Some((sink, response_transport)) =
                    setup_transport(transport, peer, writer.clone(), track, default_channels)
                else {
                    write_response(&writer, 461, "Unsupported Transport", &cseq, &[], &[])?;
                    continue;
                };

                if let Some(previous) = subscribed.filter(|previous| *previous != index) {
                    remove_stream_clients(&relays[previous], client_id);
                    video_setup = false;
                    audio_setup = false;
                }
                subscribed = Some(index);
                if wants_audio {
                    audio_setup = true;
                } else {
                    video_setup = true;
                }
                replace_client(
                    &track.clients,
                    Client {
                        id: client_id,
                        playing: playing.clone(),
                        lagging: lagging.clone(),
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
                let base = onvif::rtsp_uri(&config, stream);
                let mut rtp_info = Vec::new();
                if video_setup || !audio_setup {
                    rtp_info.push(format!("url={base}/trackID=0;seq=0;rtptime=0"));
                }
                if audio_setup {
                    rtp_info.push(format!("url={base}/trackID=1;seq=0;rtptime=0"));
                }
                write_response(
                    &writer,
                    200,
                    "OK",
                    &cseq,
                    &[
                        ("Session", session.clone()),
                        ("RTP-Info", rtp_info.join(",")),
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
        remove_stream_clients(&relays[index], client_id);
    }
    Ok(())
}

/// Removes a connection from every track of a stream.
fn remove_stream_clients(relay: &StreamRelay, client_id: u64) {
    remove_clients(&relay.video.clients, &[client_id]);
    if let Some(audio) = &relay.audio {
        remove_clients(&audio.clients, &[client_id]);
    }
}

/// True when the request URI addresses the audio track (`.../trackID=1`).
fn is_audio_track(uri: &str) -> bool {
    uri_path(uri).rsplit('/').next() == Some("trackID=1")
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
    track: &TrackRelay,
    default_channels: (u8, u8),
) -> Option<(ClientSink, String)> {
    if transport.to_ascii_uppercase().contains("RTP/AVP/TCP")
        || transport.to_ascii_lowercase().contains("interleaved")
    {
        let (rtp_channel, rtcp_channel) = parse_interleaved(transport).unwrap_or(default_channels);
        let (packet_tx, packet_rx) = mpsc::sync_channel::<TcpPacket>(CLIENT_QUEUE_PACKETS);
        let packet_writer = writer.clone();
        let _ = thread::Builder::new()
            .name("rtsp-tcp-writer".to_string())
            .spawn(move || {
                while let Ok(packet) = packet_rx.recv() {
                    if let Err(err) =
                        write_interleaved(&packet_writer, packet.channel, &packet.data)
                    {
                        eprintln!("rtsp: writing to client {peer} failed: {err}");
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
            rtp_socket: track.rtp_socket.clone(),
            rtcp_socket: track.rtcp_socket.clone(),
            rtp_peer,
            rtcp_peer,
        },
        format!(
            "RTP/AVP;unicast;client_port={client_rtp}-{client_rtcp};server_port={}-{}",
            track.rtp_port,
            track.rtp_port + 1
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

/// True when `packet` (an RTP packet of a video stream) begins a keyframe: an IDR picture or
/// the parameter sets sent in front of one (also inside an aggregation packet or as the first
/// fragment of a fragmented NAL unit).
fn starts_keyframe(format: VideoFormat, packet: &[u8]) -> bool {
    let Some(&first) = packet.first() else {
        return false;
    };
    let csrc = usize::from(first & 0x0f) * 4;
    let mut start = 12 + csrc;
    if first & 0x10 != 0 {
        // header extension: 16-bit profile, 16-bit length in 32-bit words
        let Some(length) = packet.get(start + 2..start + 4) else {
            return false;
        };
        start += 4 + usize::from(u16::from_be_bytes([length[0], length[1]])) * 4;
    }
    let Some(payload) = packet.get(start..).filter(|payload| payload.len() >= 2) else {
        return false;
    };

    match format {
        VideoFormat::H264 => {
            let key = |nal: u8| matches!(nal & 0x1f, 5 | 7);
            match payload[0] & 0x1f {
                // STAP-A: 16-bit size and NAL unit, repeated
                24 => aggregated_units(&payload[1..], |unit| key(unit[0])),
                // FU-A: start bit in the FU header, NAL type in its low bits
                28 => payload[1] & 0x80 != 0 && key(payload[1]),
                nal => key(nal),
            }
        }
        VideoFormat::H265 => {
            // VPS/SPS/PPS (32 to 34) and IRAP pictures (16 to 21)
            let key = |nal: u8| matches!(nal, 16..=21 | 32..=34);
            match (payload[0] >> 1) & 0x3f {
                // aggregation packet: 16-bit size and NAL unit (with its 2-byte header)
                48 => aggregated_units(&payload[2..], |unit| {
                    unit.len() >= 2 && key((unit[0] >> 1) & 0x3f)
                }),
                // FU: start bit in the FU header, NAL type in its low bits
                49 => payload.len() >= 3 && payload[2] & 0x80 != 0 && key(payload[2] & 0x3f),
                nal => key(nal),
            }
        }
    }
}

/// Whether any unit of an aggregation packet body (`size, unit` pairs) satisfies `matches`.
fn aggregated_units(mut body: &[u8], matches: impl Fn(&[u8]) -> bool) -> bool {
    while body.len() > 2 {
        let size = usize::from(u16::from_be_bytes([body[0], body[1]]));
        let Some(unit) = body.get(2..2 + size).filter(|unit| !unit.is_empty()) else {
            return false;
        };
        if matches(unit) {
            return true;
        }
        body = &body[2 + size..];
    }
    false
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

    // No sprop-* parameter sets in either format: the encoders repeat them in-band.
    let codec_lines = match stream.format() {
        VideoFormat::H264 => {
            "a=rtpmap:96 H264/90000\r\n\
             a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n"
        }
        VideoFormat::H265 => "a=rtpmap:96 H265/90000\r\n",
    };

    let audio_lines = stream
        .audio
        .map(|audio| {
            format!(
                "m=audio 0 RTP/AVP {pt}\r\n\
                 a=rtpmap:{pt} {}\r\n\
                 a=control:trackID=1\r\n",
                audio.codec.rtpmap(),
                pt = audio.codec.payload_type()
            )
        })
        .unwrap_or_default();

    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 0.0.0.0\r\n\
         s={name}\r\n\
         c=IN IP4 0.0.0.0\r\n\
         t=0 0\r\n\
         a=control:*\r\n\
         m=video 0 RTP/AVP 96\r\n\
         {codec_lines}\
         a=framerate:{}\r\n\
         a=control:trackID=0\r\n\
         {audio_lines}",
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
                audio: None,
            },
            video: TrackRelay {
                clients: Arc::new(Mutex::new(Vec::new())),
                rtp_socket: Arc::new(UdpSocket::bind(("127.0.0.1", 0)).unwrap()),
                rtcp_socket: Arc::new(UdpSocket::bind(("127.0.0.1", 0)).unwrap()),
                rtp_port: port,
            },
            audio: None,
        })
    }

    #[test]
    fn sdp_adds_the_audio_track_with_static_payload_types() {
        use crate::config::{AudioCodec, AudioStream};
        let sdp_for = |codec| {
            let stream = Stream {
                audio: Some(AudioStream {
                    codec,
                    rtp_port: 8004,
                }),
                ..relay("live", 5004).stream.clone()
            };
            sdp(&Config::default(), &stream)
        };

        let pcmu = sdp_for(AudioCodec::Pcmu);
        assert!(
            pcmu.contains("m=audio 0 RTP/AVP 0\r\na=rtpmap:0 PCMU/8000\r\na=control:trackID=1\r\n")
        );
        assert!(sdp_for(AudioCodec::Pcma).contains("m=audio 0 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000"));
        assert!(sdp_for(AudioCodec::G722).contains("m=audio 0 RTP/AVP 9\r\na=rtpmap:9 G722/8000"));

        let video_only = sdp(&Config::default(), &relay("live", 5004).stream);
        assert!(!video_only.contains("m=audio"));
    }

    #[test]
    fn audio_track_is_addressed_by_track_id_one() {
        assert!(is_audio_track("rtsp://h:8554/live/trackID=1"));
        assert!(is_audio_track("rtsp://h:8554/live_sub/trackID=1"));
        assert!(!is_audio_track("rtsp://h:8554/live/trackID=0"));
        assert!(!is_audio_track("rtsp://h:8554/live"));
    }

    #[test]
    fn sdp_describes_h264_and_h265_streams() {
        let config = Config::default();
        let mut stream = relay("live", 1).stream.clone();

        let h264 = sdp(&config, &stream);
        assert!(h264.contains("a=rtpmap:96 H264/90000\r\n"));
        assert!(h264.contains("a=fmtp:96 packetization-mode=1;profile-level-id=42e01f\r\n"));

        stream.codec = "libx265".to_string();
        let h265 = sdp(&config, &stream);
        assert!(h265.contains("a=rtpmap:96 H265/90000\r\n"));
        assert!(!h265.contains("H264"));
        assert!(!h265.contains("packetization-mode"));
        assert!(h265.contains("a=framerate:1\r\na=control:trackID=0\r\n"));
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

    fn rtp(payload: &[u8]) -> Vec<u8> {
        let mut packet = vec![0x80, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        packet.extend_from_slice(payload);
        packet
    }

    #[test]
    fn finds_h264_keyframe_starts() {
        let h264 = VideoFormat::H264;
        assert!(starts_keyframe(h264, &rtp(&[0x65, 1, 2]))); // IDR slice
        assert!(starts_keyframe(h264, &rtp(&[0x67, 1, 2]))); // SPS
        assert!(!starts_keyframe(h264, &rtp(&[0x41, 1, 2]))); // P slice
        assert!(starts_keyframe(h264, &rtp(&[0x7c, 0x85, 1]))); // FU-A start of IDR
        assert!(!starts_keyframe(h264, &rtp(&[0x7c, 0x05, 1]))); // FU-A continuation of IDR
        assert!(!starts_keyframe(h264, &rtp(&[0x5c, 0x81, 1]))); // FU-A start of a P slice
                                                                 // STAP-A with SPS and PPS
        assert!(starts_keyframe(
            h264,
            &rtp(&[24, 0, 2, 0x67, 1, 0, 2, 0x68, 2])
        ));
        // STAP-A with only a P slice
        assert!(!starts_keyframe(h264, &rtp(&[24, 0, 2, 0x41, 1])));
        assert!(!starts_keyframe(h264, &[]) && !starts_keyframe(h264, &rtp(&[])));
    }

    #[test]
    fn finds_h265_keyframe_starts() {
        let h265 = VideoFormat::H265;
        assert!(starts_keyframe(h265, &rtp(&[0x40, 1, 1]))); // VPS (32)
        assert!(starts_keyframe(h265, &rtp(&[0x26, 1, 1]))); // IDR_W_RADL (19)
        assert!(!starts_keyframe(h265, &rtp(&[0x02, 1, 1]))); // TRAIL_R (1)
        assert!(starts_keyframe(h265, &rtp(&[0x62, 1, 0x93]))); // FU start of IDR (19)
        assert!(!starts_keyframe(h265, &rtp(&[0x62, 1, 0x13]))); // FU continuation
                                                                 // aggregation packet with a VPS
        assert!(starts_keyframe(h265, &rtp(&[0x60, 1, 0, 3, 0x40, 1, 9])));
    }

    #[test]
    fn rtp_header_with_csrc_and_extension_is_skipped() {
        let mut packet = vec![0x91, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 1];
        packet.extend_from_slice(&[0, 0, 0, 0]); // one CSRC
        packet.extend_from_slice(&[0xbe, 0xde, 0, 1, 9, 9, 9, 9]); // extension, one word
        packet.extend_from_slice(&[0x65, 1]);
        assert!(starts_keyframe(VideoFormat::H264, &packet));
    }

    fn tcp_client(id: u64, capacity: usize) -> (Client, mpsc::Receiver<TcpPacket>) {
        let (packet_tx, packet_rx) = mpsc::sync_channel(capacity);
        let client = Client {
            id,
            playing: Arc::new(AtomicBool::new(true)),
            lagging: Arc::new(AtomicBool::new(false)),
            sink: ClientSink::Tcp {
                packet_tx,
                rtp_channel: 0,
                rtcp_channel: 1,
            },
        };
        (client, packet_rx)
    }

    #[test]
    fn a_client_that_cannot_keep_up_keeps_its_connection_and_resumes_at_a_keyframe() {
        let clients: Clients = Arc::new(Mutex::new(Vec::new()));
        let (client, rx) = tcp_client(7, 2);
        let lagging = client.lagging.clone();
        clients.lock().unwrap().push(client);
        let send = |payload: &[u8]| {
            broadcast_packet(
                &clients,
                &rtp(payload),
                false,
                5004,
                Some(VideoFormat::H264),
            )
        };
        let queued = || std::iter::from_fn(|| rx.try_recv().ok()).count();

        assert!(send(&[0x65, 1]).is_empty());
        assert!(send(&[0x41, 1]).is_empty());
        // the queue (2) is full: the packet is lost, the client stays and starts lagging
        assert!(send(&[0x41, 2]).is_empty());
        assert!(lagging.load(Ordering::Relaxed));
        assert_eq!(clients.lock().unwrap().len(), 1);

        // the client reads again, but P slices are still skipped until a keyframe
        assert_eq!(queued(), 2);
        assert!(send(&[0x41, 3]).is_empty());
        assert_eq!(queued(), 0);
        assert!(lagging.load(Ordering::Relaxed));

        // keyframe: delivery resumes
        assert!(send(&[0x67, 1]).is_empty());
        assert!(!lagging.load(Ordering::Relaxed));
        assert!(send(&[0x41, 4]).is_empty());
        assert_eq!(queued(), 2);

        // a client whose writer is gone is reported
        drop(rx);
        assert_eq!(send(&[0x41, 5]), vec![7]);
    }

    #[test]
    fn audio_and_rtcp_overflow_only_drop_the_packet() {
        let clients: Clients = Arc::new(Mutex::new(Vec::new()));
        let (client, _rx) = tcp_client(1, 1);
        let lagging = client.lagging.clone();
        clients.lock().unwrap().push(client);

        assert!(broadcast_packet(&clients, &rtp(&[1]), false, 7004, None).is_empty());
        assert!(broadcast_packet(&clients, &rtp(&[2]), false, 7004, None).is_empty());
        assert!(
            broadcast_packet(&clients, &[0x80, 200], true, 7005, Some(VideoFormat::H264))
                .is_empty()
        );
        assert!(!lagging.load(Ordering::Relaxed), "only video lags");
    }
}
