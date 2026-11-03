use std::collections::HashMap;
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use crossbeam_channel::{Receiver, Sender};
use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tungstenite::protocol::WebSocket;
use tungstenite::{accept, Message};

use crate::codec::{decode_audio_packet, VideoStreamDecoder};
use crate::types::{
    DecodedVideoFrame, EncodedAudioPacket, EncodedVideoPacket, ParticipantInfo,
    WIRE_TAG_AUDIO_BINARY, WIRE_TAG_CONTROL_JSON, WIRE_TAG_VIDEO_BINARY,
};

pub const LIVEKIT_API_KEY: &str = "APIScreenSharePrivateKey";
pub const LIVEKIT_API_SECRET: &str = "SecretScreenShareKeyForFriends2026Win11Native!";
pub const DEFAULT_ROOM_NAME: &str = "sala-principal";
pub const DEFAULT_SFU_RELAY_PORT: u16 = 7890;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Serialize, Deserialize)]
struct LiveKitVideoGrant {
    #[serde(rename = "roomJoin")]
    room_join: bool,
    room: String,
    #[serde(rename = "canPublish")]
    can_publish: bool,
    #[serde(rename = "canSubscribe")]
    can_subscribe: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct LiveKitJwtClaims {
    iss: String,
    sub: String,
    name: String,
    iat: u64,
    exp: u64,
    nbf: u64,
    video: LiveKitVideoGrant,
}

/// 4.1. Gera localmente um Token JWT compatível com o LiveKit Server (`livekit.yaml`) usando HMAC-SHA256.
pub fn generate_livekit_jwt(identity: &str, nickname: &str, room: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let header_json = r#"{"alg":"HS256","typ":"JWT"}"#;
    let claims = LiveKitJwtClaims {
        iss: LIVEKIT_API_KEY.to_string(),
        sub: identity.to_string(),
        name: nickname.to_string(),
        iat: now,
        nbf: now.saturating_sub(10),
        exp: now + 86_400,
        video: LiveKitVideoGrant {
            room_join: true,
            room: room.to_string(),
            can_publish: true,
            can_subscribe: true,
        },
    };

    let claims_json = serde_json::to_string(&claims).unwrap_or_default();
    let header_b64 = URL_SAFE_NO_PAD.encode(header_json.as_bytes());
    let claims_b64 = URL_SAFE_NO_PAD.encode(claims_json.as_bytes());
    let signing_input = format!("{}.{}", header_b64, claims_b64);

    let mut mac = HmacSha256::new_from_slice(LIVEKIT_API_SECRET.as_bytes())
        .expect("HMAC aceita chaves de qualquer tamanho");
    mac.update(signing_input.as_bytes());
    let signature = mac.finalize().into_bytes();
    let sig_b64 = URL_SAFE_NO_PAD.encode(signature);

    format!("{}.{}", signing_input, sig_b64)
}

/// Mensagens de Controle da Sala (serializadas com prefixo `WIRE_TAG_CONTROL_JSON = 0x01`).
/// Os pacotes de Vídeo (`0x02`) e Áudio (`0x03`) trafegam em binário puro sem tocar em JSON!
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SfuControlMessage {
    JoinRoom {
        jwt_token: String,
        participant: ParticipantInfo,
    },
    UpdateParticipant(ParticipantInfo),
    RoomState {
        participants: Vec<ParticipantInfo>,
    },
    SubscribeStream {
        viewer_identity: String,
        target_host_identity: String,
    },
    UnsubscribeStream {
        viewer_identity: String,
    },
    RequestKeyframe {
        target_host_identity: String,
    },
    ViewerCongested {
        target_host_identity: String,
    },
    Ping(u64),
    Pong(u64),
}

impl SfuControlMessage {
    pub fn to_wire_bytes(&self) -> Option<Vec<u8>> {
        let json = serde_json::to_vec(self).ok()?;
        let mut out = Vec::with_capacity(1 + json.len());
        out.push(WIRE_TAG_CONTROL_JSON);
        out.extend_from_slice(&json);
        Some(out)
    }

    pub fn from_wire_bytes(data: &[u8]) -> Option<Self> {
        if data.is_empty() {
            return None;
        }
        if data[0] == WIRE_TAG_CONTROL_JSON {
            serde_json::from_slice(&data[1..]).ok()
        } else {
            serde_json::from_slice(data).ok()
        }
    }
}

/// Extrai rapidamente o `sender` de um pacote binário de Vídeo (`0x02`) ou Áudio (`0x03`) sem deserializar o payload.
pub fn peek_binary_packet_sender(data: &[u8]) -> Option<&str> {
    if data.len() < 4 {
        return None;
    }
    let tag = data[0];
    if tag != WIRE_TAG_VIDEO_BINARY && tag != WIRE_TAG_AUDIO_BINARY {
        return None;
    }
    let sender_len = u16::from_le_bytes([data[1], data[2]]) as usize;
    if data.len() < 3 + sender_len {
        return None;
    }
    std::str::from_utf8(&data[3..3 + sender_len]).ok()
}

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    Connected {
        server_endpoint: String,
        jwt_token: String,
        livekit_reachable: bool,
    },
    Reconnecting(String),
    Disconnected(String),
    ParticipantsUpdated(Vec<ParticipantInfo>),
    RemoteVideoFrame(DecodedVideoFrame),
    RemoteAudioFrame {
        samples: Vec<i16>,
    },
    KeyframeRequested,
    ViewerCongested,
    LatencyUpdatedMs(u64),
}

pub enum OutgoingCommand {
    UpdateLocalParticipant(ParticipantInfo),
    SubscribeToHost(Option<String>),
    PublishVideo(EncodedVideoPacket),
    PublishAudio(EncodedAudioPacket),
    Disconnect,
}

#[derive(Clone)]
pub struct MediaPublisherHandle {
    cmd_tx: Sender<OutgoingCommand>,
    audio_tx: Sender<Vec<u8>>,
    video_tx: Sender<Vec<u8>>,
    socket_busy: Arc<AtomicBool>,
}

impl MediaPublisherHandle {
    pub fn is_video_busy(&self) -> bool {
        // Permite codificar o próximo frame na CPU em paralelo enquanto o socket TCP envia o frame anterior!
        self.video_tx.is_full()
    }

    pub fn queue_len(&self) -> usize {
        self.video_tx.len() + usize::from(self.socket_busy.load(Ordering::Relaxed))
    }

    pub fn send_audio(&self, pkt: EncodedAudioPacket) {
        let _ = self.audio_tx.try_send(pkt.to_wire_bytes());
    }

    pub fn send_video(&self, pkt: EncodedVideoPacket) -> bool {
        self.video_tx.try_send(pkt.to_wire_bytes()).is_ok()
    }

    pub fn try_send(&self, cmd: OutgoingCommand) -> Result<(), ()> {
        match cmd {
            OutgoingCommand::PublishAudio(pkt) => {
                self.send_audio(pkt);
                Ok(())
            }
            OutgoingCommand::PublishVideo(pkt) => {
                if self.send_video(pkt) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            other => self.cmd_tx.try_send(other).map_err(|_| ()),
        }
    }
}

/// Cliente de rede Full-Duplex com filas separadas de Áudio/Vídeo, Zero-Bufferbloat e thread de decodificação isolada.
pub struct RoomNetworkClient {
    cmd_tx: Sender<OutgoingCommand>,
    pub_handle: MediaPublisherHandle,
    stop_flag: Arc<AtomicBool>,
}

impl RoomNetworkClient {
    pub fn connect(
        server_ip_input: &str,
        local_participant: ParticipantInfo,
        event_tx: Sender<NetworkEvent>,
        audio_pcm_tx: Sender<Vec<i16>>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<OutgoingCommand>(32);
        let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
        // Fila de saída de vídeo com 3 slots (~50ms) para que o encoder 60 FPS nunca perca o frame par enquanto a thread de rede consome o ímpar!
        let (video_tx, video_rx) = crossbeam_channel::bounded::<Vec<u8>>(3);
        let socket_busy = Arc::new(AtomicBool::new(false));
        let socket_busy_clone = socket_busy.clone();

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let (host_str, port) = parse_server_endpoint(server_ip_input);

        thread::Builder::new()
            .name("sfu-network-client".to_string())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    run_network_worker(
                        host_str,
                        port,
                        local_participant,
                        cmd_rx,
                        audio_rx,
                        video_rx,
                        socket_busy_clone,
                        event_tx,
                        audio_pcm_tx,
                        stop_clone,
                    );
                }));
            })
            .expect("Falha ao criar thread de rede SFU");

        let pub_handle = MediaPublisherHandle {
            cmd_tx: cmd_tx.clone(),
            audio_tx,
            video_tx,
            socket_busy,
        };

        Self {
            cmd_tx,
            pub_handle,
            stop_flag,
        }
    }

    pub fn update_participant(&self, info: ParticipantInfo) {
        let _ = self
            .cmd_tx
            .try_send(OutgoingCommand::UpdateLocalParticipant(info));
    }

    pub fn set_watched_host(&self, target_identity: Option<String>) {
        let _ = self
            .cmd_tx
            .try_send(OutgoingCommand::SubscribeToHost(target_identity));
    }

    pub fn disconnect(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.try_send(OutgoingCommand::Disconnect);
    }

    pub fn inner_sender(&self) -> MediaPublisherHandle {
        self.pub_handle.clone()
    }
}

impl Drop for RoomNetworkClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

fn parse_server_endpoint(input: &str) -> (String, u16) {
    let trimmed = input
        .trim()
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("tcp://");

    let host_port = trimmed.split('/').next().unwrap_or("").trim();

    if host_port.is_empty() {
        return ("127.0.0.1".to_string(), DEFAULT_SFU_RELAY_PORT);
    }

    if let Some((host, port_str)) = host_port.rsplit_once(':') {
        if !host.is_empty() {
            if let Ok(custom_port) = port_str.trim().parse::<u16>() {
                if custom_port > 0 {
                    return (host.trim().to_string(), custom_port);
                }
            }
            return (host.trim().to_string(), DEFAULT_SFU_RELAY_PORT);
        }
    }

    (host_port.to_string(), DEFAULT_SFU_RELAY_PORT)
}

fn probe_livekit_port(host: &str, relay_port: u16) -> bool {
    if relay_port != DEFAULT_SFU_RELAY_PORT {
        return true;
    }
    let addr = format!("{}:7880", host);
    if let Ok(mut addrs) = std::net::ToSocketAddrs::to_socket_addrs(&addr) {
        if let Some(sock_addr) = addrs.next() {
            return TcpStream::connect_timeout(&sock_addr, Duration::from_millis(500)).is_ok();
        }
    }
    false
}

fn run_network_worker(
    host: String,
    port: u16,
    mut current_participant: ParticipantInfo,
    cmd_rx: Receiver<OutgoingCommand>,
    audio_rx: Receiver<Vec<u8>>,
    video_rx: Receiver<Vec<u8>>,
    socket_busy: Arc<AtomicBool>,
    event_tx: Sender<NetworkEvent>,
    audio_pcm_tx: Sender<Vec<i16>>,
    stop_flag: Arc<AtomicBool>,
) {
    let jwt_token = generate_livekit_jwt(
        &current_participant.identity,
        &current_participant.nickname,
        DEFAULT_ROOM_NAME,
    );

    let livekit_reachable = probe_livekit_port(&host, port);
    let ws_url = format!("ws://{}:{}/ws", host, port);

    // Iniciar thread dedicada de decodificação de vídeo para NUNCA bloquear o socket TCP
    let (vdec_tx, vdec_rx) = crossbeam_channel::bounded::<EncodedVideoPacket>(32);
    let event_tx_vdec = event_tx.clone();
    let stop_vdec = stop_flag.clone();
    thread::Builder::new()
        .name("video-decoder-worker".to_string())
        .spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let mut decoder = VideoStreamDecoder::new();
                while !stop_vdec.load(Ordering::Relaxed) {
                    let Ok(mut pkt) = vdec_rx.recv_timeout(Duration::from_millis(50)) else {
                        continue;
                    };
                    // Só fundir frames intermediários se houver atraso real acumulado (>3 frames na fila);
                    // em rajadas normais de 2 pacotes do TCP/ZeroTier, emite cada frame individualmente para manter 60 FPS!
                    while vdec_rx.len() > 3 {
                        let Ok(next_pkt) = vdec_rx.try_recv() else {
                            break;
                        };
                        if next_pkt.is_keyframe {
                            pkt = next_pkt;
                        } else {
                            let _ = decoder.decode_packet(&pkt);
                            pkt = next_pkt;
                        }
                    }
                    if let Some(frame) = decoder.decode_packet(&pkt) {
                        let _ = event_tx_vdec.try_send(NetworkEvent::RemoteVideoFrame(frame));
                    }
                }
            }));
        })
        .ok();

    let mut watched_host: Option<String> = None;
    let my_identity = current_participant.identity.clone();
    let mut first_connect = true;

    // Loop com reconexão automática transparente caso a conexão oscile durante stream dupla
    while !stop_flag.load(Ordering::Relaxed) {
        socket_busy.store(false, Ordering::Relaxed);
        let mut ws_conn = connect_ws_nonblocking(&ws_url);
        if ws_conn.is_none()
            && port == DEFAULT_SFU_RELAY_PORT
            && (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost"))
        {
            ensure_embedded_sfu_running();
            thread::sleep(Duration::from_millis(100));
            ws_conn = connect_ws_nonblocking(&ws_url);
        }

        let Some(mut socket) = ws_conn else {
            if first_connect {
                let _ = event_tx.send(NetworkEvent::Disconnected(format!(
                    "Não foi possível conectar em {}:{}. Verifique o IP ZeroTier/Playit e o Docker.",
                    host, port
                )));
                return;
            } else {
                let _ = event_tx.send(NetworkEvent::Reconnecting(format!(
                    "Reconectando a {}:{}...",
                    host, port
                )));
                thread::sleep(Duration::from_millis(400));
                continue;
            }
        };

        // Enviar JoinRoom
        if let Some(bin) = (SfuControlMessage::JoinRoom {
            jwt_token: jwt_token.clone(),
            participant: current_participant.clone(),
        })
        .to_wire_bytes()
        {
            let _ = socket.send(Message::Binary(bin));
        }

        // Se já estávamos assistindo alguém antes de uma oscilação, re-assinar automaticamente
        if let Some(ref target_host) = watched_host {
            if let Some(bin) = (SfuControlMessage::SubscribeStream {
                viewer_identity: my_identity.clone(),
                target_host_identity: target_host.clone(),
            })
            .to_wire_bytes()
            {
                let _ = socket.send(Message::Binary(bin));
            }
        }

        if first_connect {
            first_connect = false;
            let _ = event_tx.send(NetworkEvent::Connected {
                server_endpoint: format!("{}:{}", host, port),
                jwt_token: jwt_token.clone(),
                livekit_reachable,
            });
        }

        let mut last_ping = Instant::now();
        let mut connection_alive = true;
        let mut pending_flush = false;

        while connection_alive && !stop_flag.load(Ordering::Relaxed) {
            let mut did_work = false;

            // 1. Verificar se o write_buffer do WebSocket terminou de descarregar no socket TCP
            if pending_flush {
                match socket.flush() {
                    Ok(()) => {
                        pending_flush = false;
                        socket_busy.store(false, Ordering::Relaxed);
                        did_work = true;
                    }
                    Err(tungstenite::Error::Io(ref err))
                        if err.kind() == ErrorKind::WouldBlock
                            || err.kind() == ErrorKind::TimedOut
                            || err.kind() == ErrorKind::Interrupted =>
                    {
                        socket_busy.store(true, Ordering::Relaxed);
                    }
                    Err(_) => {
                        connection_alive = false;
                        break;
                    }
                }
            }

            // 2. Comandos de Controle
            while !pending_flush {
                let Ok(cmd) = cmd_rx.try_recv() else {
                    break;
                };
                did_work = true;

                match cmd {
                    OutgoingCommand::UpdateLocalParticipant(info) => {
                        current_participant = info.clone();
                        if let Some(bin) =
                            SfuControlMessage::UpdateParticipant(info).to_wire_bytes()
                        {
                            if !send_ws_gated(&mut socket, bin, &mut pending_flush) {
                                connection_alive = false;
                                break;
                            }
                        }
                    }
                    OutgoingCommand::SubscribeToHost(target) => {
                        watched_host = target.clone();
                        let ctrl = match target {
                            Some(host_id) => SfuControlMessage::SubscribeStream {
                                viewer_identity: my_identity.clone(),
                                target_host_identity: host_id,
                            },
                            None => SfuControlMessage::UnsubscribeStream {
                                viewer_identity: my_identity.clone(),
                            },
                        };
                        if let Some(bin) = ctrl.to_wire_bytes() {
                            if !send_ws_gated(&mut socket, bin, &mut pending_flush) {
                                connection_alive = false;
                                break;
                            }
                        }
                    }
                    OutgoingCommand::PublishVideo(packet) => {
                        let bin = packet.to_wire_bytes();
                        if !send_ws_gated(&mut socket, bin, &mut pending_flush) {
                            connection_alive = false;
                            break;
                        }
                    }
                    OutgoingCommand::PublishAudio(packet) => {
                        let bin = packet.to_wire_bytes();
                        if !send_ws_gated(&mut socket, bin, &mut pending_flush) {
                            connection_alive = false;
                            break;
                        }
                    }
                    OutgoingCommand::Disconnect => {
                        let _ = socket.close(None);
                        return;
                    }
                }
            }

            if !connection_alive {
                break;
            }

            // 3. Prioridade Estrita para Áudio de Saída (nunca espera atrás de vídeo)
            while !pending_flush {
                let Ok(audio_bin) = audio_rx.try_recv() else {
                    break;
                };
                did_work = true;
                if !send_ws_gated(&mut socket, audio_bin, &mut pending_flush) {
                    connection_alive = false;
                    break;
                }
            }

            if !connection_alive {
                break;
            }

            // 4. Vídeo de Saída (descarrega todos os frames prontos enquanto o socket TCP aceitar sem bloquear)
            while !pending_flush {
                let Ok(video_bin) = video_rx.try_recv() else {
                    break;
                };
                did_work = true;
                if !send_ws_gated(&mut socket, video_bin, &mut pending_flush) {
                    connection_alive = false;
                    break;
                }
            }

            socket_busy.store(pending_flush, Ordering::Relaxed);

            // 5. Ping periódico a cada 2 segundos
            if !pending_flush && last_ping.elapsed() >= Duration::from_secs(2) {
                last_ping = Instant::now();
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                if let Some(bin) = SfuControlMessage::Ping(now_ms).to_wire_bytes() {
                    let _ = send_ws_gated(&mut socket, bin, &mut pending_flush);
                }
            }

            // 6. Drenar todos os pacotes recebidos disponíveis no socket não-bloqueante
            for _ in 0..24 {
                match socket.read() {
                    Ok(Message::Binary(data)) => {
                        did_work = true;
                        if data.is_empty() {
                            continue;
                        }
                        match data[0] {
                            WIRE_TAG_VIDEO_BINARY => {
                                if let Some(pkt) = EncodedVideoPacket::from_wire_bytes(&data) {
                                    if watched_host.as_deref() == Some(pkt.sender.as_str()) {
                                        let _ = vdec_tx.try_send(pkt);
                                    }
                                }
                            }
                            WIRE_TAG_AUDIO_BINARY => {
                                if let Some(pkt) = EncodedAudioPacket::from_wire_bytes(&data) {
                                    if watched_host.as_deref() == Some(pkt.sender.as_str()) {
                                        if let Some(samples) = decode_audio_packet(&pkt) {
                                            // Envia direto da thread de rede para a thread de áudio WASAPI (sem passar pela GUI!)
                                            let _ = audio_pcm_tx.try_send(samples);
                                        }
                                    }
                                }
                            }
                            _ => {
                                if let Some(msg) = SfuControlMessage::from_wire_bytes(&data) {
                                    match msg {
                                        SfuControlMessage::RoomState { participants } => {
                                            let _ = event_tx.send(
                                                NetworkEvent::ParticipantsUpdated(participants),
                                            );
                                        }
                                        SfuControlMessage::RequestKeyframe {
                                            target_host_identity,
                                        } => {
                                            if target_host_identity == my_identity {
                                                let _ =
                                                    event_tx.send(NetworkEvent::KeyframeRequested);
                                            }
                                        }
                                        SfuControlMessage::ViewerCongested {
                                            target_host_identity,
                                        } => {
                                            if target_host_identity == my_identity {
                                                let _ =
                                                    event_tx.send(NetworkEvent::ViewerCongested);
                                            }
                                        }
                                        SfuControlMessage::Pong(sent_ms) => {
                                            let now_ms = SystemTime::now()
                                                .duration_since(UNIX_EPOCH)
                                                .unwrap_or_default()
                                                .as_millis()
                                                as u64;
                                            let rtt = now_ms.saturating_sub(sent_ms);
                                            let _ =
                                                event_tx.send(NetworkEvent::LatencyUpdatedMs(rtt));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        connection_alive = false;
                        break;
                    }
                    Err(tungstenite::Error::Io(ref err))
                        if err.kind() == ErrorKind::WouldBlock
                            || err.kind() == ErrorKind::TimedOut
                            || err.kind() == ErrorKind::Interrupted =>
                    {
                        break;
                    }
                    Err(_) => {
                        connection_alive = false;
                        break;
                    }
                    _ => {}
                }
            }

            if !did_work {
                // Evitar thread::sleep(1ms) no Windows (que dorme 15.6ms e derruba o FPS para 30~40!)
                thread::yield_now();
            }
        }
    }
}

/// Envia uma mensagem binária com controle estrito de `pending_flush` para impedir bufferbloat.
fn send_ws_gated(
    socket: &mut WebSocket<TcpStream>,
    data: Vec<u8>,
    pending_flush: &mut bool,
) -> bool {
    match socket.send(Message::Binary(data)) {
        Ok(()) => {
            *pending_flush = false;
            true
        }
        Err(tungstenite::Error::Io(ref err))
            if err.kind() == ErrorKind::WouldBlock
                || err.kind() == ErrorKind::TimedOut
                || err.kind() == ErrorKind::Interrupted =>
        {
            *pending_flush = true;
            true
        }
        Err(_) => false,
    }
}

fn connect_ws_nonblocking(ws_url: &str) -> Option<WebSocket<TcpStream>> {
    let parsed = url::Url::parse(ws_url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port().unwrap_or(DEFAULT_SFU_RELAY_PORT);
    let addr_str = format!("{}:{}", host, port);
    let mut addrs = std::net::ToSocketAddrs::to_socket_addrs(&addr_str).ok()?;
    let sock_addr = addrs.next()?;

    let tcp = TcpStream::connect_timeout(&sock_addr, Duration::from_millis(3500)).ok()?;
    let _ = tcp.set_nodelay(true);
    let (socket, _resp) = tungstenite::client::client(ws_url, tcp).ok()?;
    let _ = socket.get_ref().set_nonblocking(true);
    Some(socket)
}

struct ClientPeer {
    info: ParticipantInfo,
    watching: Option<String>,
    ctrl_tx: Sender<Vec<u8>>,
    audio_tx: Sender<Vec<u8>>,
    video_tx: Sender<Vec<u8>>,
    video_rx: Receiver<Vec<u8>>,
    last_congestion_notify: Instant,
}

fn broadcast_room_state(peers: &Arc<Mutex<HashMap<String, ClientPeer>>>) {
    let map = peers.lock();
    let participants: Vec<ParticipantInfo> = map.values().map(|p| p.info.clone()).collect();
    if let Some(bin) = (SfuControlMessage::RoomState { participants }).to_wire_bytes() {
        for peer in map.values() {
            let _ = peer.ctrl_tx.try_send(bin.clone());
        }
    }
}

static EMBEDDED_SFU_STARTED: AtomicBool = AtomicBool::new(false);

pub fn ensure_embedded_sfu_running() {
    if EMBEDDED_SFU_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }

    thread::Builder::new()
        .name("embedded-sfu-relay".to_string())
        .spawn(move || {
            let bind_addr = format!("0.0.0.0:{}", DEFAULT_SFU_RELAY_PORT);
            let Ok(listener) = TcpListener::bind(&bind_addr) else {
                return;
            };

            let peers: Arc<Mutex<HashMap<String, ClientPeer>>> =
                Arc::new(Mutex::new(HashMap::new()));

            for stream in listener.incoming().flatten() {
                let _ = stream.set_nodelay(true);
                let peers_ref = peers.clone();

                thread::spawn(move || {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                        handle_embedded_peer(stream, peers_ref);
                    }));
                });
            }
        })
        .ok();
}

fn handle_embedded_peer(
    stream: TcpStream,
    peers_ref: Arc<Mutex<HashMap<String, ClientPeer>>>,
) {
    let Ok(mut ws) = accept(stream) else {
        return;
    };
    let _ = ws.get_ref().set_nonblocking(true);

    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
    let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<u8>>(96);
    let (video_tx, video_rx) = crossbeam_channel::bounded::<Vec<u8>>(4);
    let mut my_id: Option<String> = None;
    let mut pending_flush = false;

    loop {
        let mut active = false;

        if pending_flush {
            match ws.flush() {
                Ok(()) => {
                    pending_flush = false;
                    active = true;
                }
                Err(tungstenite::Error::Io(ref err))
                    if err.kind() == ErrorKind::WouldBlock
                        || err.kind() == ErrorKind::TimedOut
                        || err.kind() == ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }

        if !pending_flush {
            while !pending_flush {
                let Ok(bin) = ctrl_rx.try_recv() else {
                    break;
                };
                active = true;
                if !send_ws_gated(&mut ws, bin, &mut pending_flush) {
                    return;
                }
            }

            while !pending_flush {
                let Ok(bin) = audio_rx.try_recv() else {
                    break;
                };
                active = true;
                if !send_ws_gated(&mut ws, bin, &mut pending_flush) {
                    return;
                }
            }

            while !pending_flush {
                let Ok(bin) = video_rx.try_recv() else {
                    break;
                };
                active = true;
                if !send_ws_gated(&mut ws, bin, &mut pending_flush) {
                    return;
                }
            }
        }

        for _ in 0..16 {
            match ws.read() {
                Ok(Message::Binary(data)) => {
                    active = true;
                    if data.is_empty() {
                        continue;
                    }
                    match data[0] {
                        WIRE_TAG_AUDIO_BINARY => {
                            if let Some(sender_id) = peek_binary_packet_sender(&data) {
                                let map = peers_ref.lock();
                                for peer in map.values() {
                                    if peer.watching.as_deref() == Some(sender_id) {
                                        let _ = peer.audio_tx.try_send(data.clone());
                                    }
                                }
                            }
                        }
                        WIRE_TAG_VIDEO_BINARY => {
                            if let Some(sender_id) = peek_binary_packet_sender(&data) {
                                let mut notify_congested_host: Option<Sender<Vec<u8>>> = None;
                                {
                                    let mut map = peers_ref.lock();
                                    let host_ctrl_tx =
                                        map.get(sender_id).map(|h| h.ctrl_tx.clone());
                                    for peer in map.values_mut() {
                                        if peer.watching.as_deref() == Some(sender_id) {
                                            if peer.video_tx.try_send(data.clone()).is_err() {
                                                let _ = peer.video_rx.try_recv();
                                                let _ = peer.video_tx.try_send(data.clone());
                                                if peer.last_congestion_notify.elapsed()
                                                    >= Duration::from_millis(350)
                                                {
                                                    peer.last_congestion_notify = Instant::now();
                                                    notify_congested_host = host_ctrl_tx.clone();
                                                }
                                            }
                                        }
                                    }
                                }
                                if let Some(tx) = notify_congested_host {
                                    if let Some(msg) = (SfuControlMessage::ViewerCongested {
                                        target_host_identity: sender_id.to_string(),
                                    })
                                    .to_wire_bytes()
                                    {
                                        let _ = tx.try_send(msg);
                                    }
                                }
                            }
                        }
                        _ => {
                            let Some(msg) = SfuControlMessage::from_wire_bytes(&data) else {
                                continue;
                            };
                            match msg {
                                SfuControlMessage::JoinRoom { participant, .. } => {
                                    let id = participant.identity.clone();
                                    my_id = Some(id.clone());
                                    {
                                        let mut map = peers_ref.lock();
                                        let prev_watching =
                                            map.get(&id).and_then(|p| p.watching.clone());
                                        if map.len() < 10 || map.contains_key(&id) {
                                            map.insert(
                                                id,
                                                ClientPeer {
                                                    info: participant,
                                                    watching: prev_watching,
                                                    ctrl_tx: ctrl_tx.clone(),
                                                    audio_tx: audio_tx.clone(),
                                                    video_tx: video_tx.clone(),
                                                    video_rx: video_rx.clone(),
                                                    last_congestion_notify: Instant::now(),
                                                },
                                            );
                                        }
                                    }
                                    broadcast_room_state(&peers_ref);
                                }
                                SfuControlMessage::UpdateParticipant(info) => {
                                    {
                                        let mut map = peers_ref.lock();
                                        if let Some(peer) = map.get_mut(&info.identity) {
                                            peer.info = info;
                                        }
                                    }
                                    broadcast_room_state(&peers_ref);
                                }
                                SfuControlMessage::SubscribeStream {
                                    viewer_identity,
                                    target_host_identity,
                                } => {
                                    let mut host_tx = None;
                                    {
                                        let mut map = peers_ref.lock();
                                        if let Some(viewer) = map.get_mut(&viewer_identity) {
                                            viewer.watching = Some(target_host_identity.clone());
                                        }
                                        if let Some(host) = map.get(&target_host_identity) {
                                            host_tx = Some(host.ctrl_tx.clone());
                                        }
                                    }
                                    if let Some(tx) = host_tx {
                                        if let Some(req) = (SfuControlMessage::RequestKeyframe {
                                            target_host_identity,
                                        })
                                        .to_wire_bytes()
                                        {
                                            let _ = tx.try_send(req);
                                        }
                                    }
                                }
                                SfuControlMessage::UnsubscribeStream { viewer_identity } => {
                                    let mut map = peers_ref.lock();
                                    if let Some(viewer) = map.get_mut(&viewer_identity) {
                                        viewer.watching = None;
                                    }
                                }
                                SfuControlMessage::Ping(ts) => {
                                    if let Some(pong) = SfuControlMessage::Pong(ts).to_wire_bytes()
                                    {
                                        let _ = ctrl_tx.try_send(pong);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    if let Some(id) = my_id.take() {
                        peers_ref.lock().remove(&id);
                        broadcast_room_state(&peers_ref);
                    }
                    return;
                }
                Err(tungstenite::Error::Io(ref e))
                    if e.kind() == ErrorKind::WouldBlock
                        || e.kind() == ErrorKind::TimedOut
                        || e.kind() == ErrorKind::Interrupted =>
                {
                    break;
                }
                Err(_) => {
                    if let Some(id) = my_id.take() {
                        peers_ref.lock().remove(&id);
                        broadcast_room_state(&peers_ref);
                    }
                    return;
                }
                _ => {}
            }
        }

        if !active {
            thread::sleep(Duration::from_micros(400));
        }
    }
}
