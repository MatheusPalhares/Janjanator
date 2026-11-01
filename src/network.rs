use std::collections::HashMap;
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
use tungstenite::{accept, connect, Message};

use crate::codec::{decode_audio_packet, decode_video_packet};
use crate::types::{
    DecodedVideoFrame, EncodedAudioPacket, EncodedVideoPacket, ParticipantInfo,
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
        exp: now + 86_400, // válido por 24 horas
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

/// Protocolo de sinalização e transporte de mídia da Sala SFU (até 10 usuários).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SfuWireMessage {
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
    VideoTrackPacket(EncodedVideoPacket),
    AudioTrackPacket(EncodedAudioPacket),
    Ping(u64),
    Pong(u64),
}

#[derive(Debug, Clone)]
pub enum NetworkEvent {
    Connected {
        server_endpoint: String,
        jwt_token: String,
        livekit_reachable: bool,
    },
    Disconnected(String),
    ParticipantsUpdated(Vec<ParticipantInfo>),
    RemoteVideoFrame(DecodedVideoFrame),
    RemoteAudioFrame {
        sender: String,
        source_pid: u32,
        samples: Vec<i16>,
    },
    KeyframeRequested,
    LatencyUpdatedMs(u64),
}

pub enum OutgoingCommand {
    UpdateLocalParticipant(ParticipantInfo),
    SubscribeToHost(Option<String>),
    PublishVideo(EncodedVideoPacket),
    PublishAudio(EncodedAudioPacket),
    Disconnect,
}

/// Cliente de rede para publicação (`LocalVideoTrack` / `LocalAudioTrack`) e consumo de tracks na sala.
pub struct RoomNetworkClient {
    cmd_tx: Sender<OutgoingCommand>,
    stop_flag: Arc<AtomicBool>,
}

impl RoomNetworkClient {
    pub fn connect(
        server_ip_input: &str,
        local_participant: ParticipantInfo,
        event_tx: Sender<NetworkEvent>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::bounded::<OutgoingCommand>(256);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let (host_str, port) = parse_server_endpoint(server_ip_input);

        thread::Builder::new()
            .name("sfu-network-client".to_string())
            .spawn(move || {
                run_network_worker(
                    host_str,
                    port,
                    local_participant,
                    cmd_rx,
                    event_tx,
                    stop_clone,
                );
            })
            .expect("Falha ao criar thread de rede SFU");

        Self { cmd_tx, stop_flag }
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

    pub fn publish_video(&self, packet: EncodedVideoPacket) {
        let _ = self.cmd_tx.try_send(OutgoingCommand::PublishVideo(packet));
    }

    pub fn publish_audio(&self, packet: EncodedAudioPacket) {
        let _ = self.cmd_tx.try_send(OutgoingCommand::PublishAudio(packet));
    }

    pub fn disconnect(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        let _ = self.cmd_tx.try_send(OutgoingCommand::Disconnect);
    }

    pub fn inner_sender(&self) -> Sender<OutgoingCommand> {
        self.cmd_tx.clone()
    }
}

impl Drop for RoomNetworkClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Interpreta entradas como:
/// - `127.0.0.1` ou `10.147.x.y` (ZeroTier/LAN) -> `(host, 7890)`
/// - `algo.gl.at.ply.gg:45123` ou `147.185.221.20:45123` (Playit.gg / Ngrok) -> `(host, 45123)`
fn parse_server_endpoint(input: &str) -> (String, u16) {
    let trimmed = input
        .trim()
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_start_matches("tcp://");

    // Remover eventual caminho de URL após '/'
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

/// Verifica se o LiveKit Server (`:7880`) está respondendo naquele IP (ou se é túnel customizado).
fn probe_livekit_port(host: &str, relay_port: u16) -> bool {
    if relay_port != DEFAULT_SFU_RELAY_PORT {
        // Quando conectado via túnel Playit.gg/Ngrok na porta customizada, o túnel aponta direto para o Hub
        return true;
    }
    let addr = format!("{}:7880", host);
    if let Ok(mut addrs) = std::net::ToSocketAddrs::to_socket_addrs(&addr) {
        if let Some(sock_addr) = addrs.next() {
            return TcpStream::connect_timeout(&sock_addr, Duration::from_millis(600)).is_ok();
        }
    }
    false
}

fn run_network_worker(
    host: String,
    port: u16,
    local_participant: ParticipantInfo,
    cmd_rx: Receiver<OutgoingCommand>,
    event_tx: Sender<NetworkEvent>,
    stop_flag: Arc<AtomicBool>,
) {
    let jwt_token = generate_livekit_jwt(
        &local_participant.identity,
        &local_participant.nickname,
        DEFAULT_ROOM_NAME,
    );

    let livekit_reachable = probe_livekit_port(&host, port);
    let ws_url = format!("ws://{}:{}/ws", host, port);

    // Tentar conectar ao servidor Docker SFU Hub (:7890 ou porta do Playit.gg).
    // Se for localhost na porta padrão e ainda não estiver aberto, inicia o mini-SFU embarcado.
    let mut ws_conn = connect_ws_nonblocking(&ws_url);
    if ws_conn.is_none()
        && port == DEFAULT_SFU_RELAY_PORT
        && (host == "127.0.0.1" || host.eq_ignore_ascii_case("localhost"))
    {
        ensure_embedded_sfu_running();
        thread::sleep(Duration::from_millis(120));
        ws_conn = connect_ws_nonblocking(&ws_url);
    }

    let Some(mut socket) = ws_conn else {
        let _ = event_tx.send(NetworkEvent::Disconnected(format!(
            "Não foi possível conectar em {}:{}. Verifique o túnel Playit.gg/ZeroTier e se o Docker está ativo (`docker-compose up -d`).",
            host, port
        )));
        return;
    };

    // Enviar JoinRoom com o JWT assinado
    let join_msg = SfuWireMessage::JoinRoom {
        jwt_token: jwt_token.clone(),
        participant: local_participant.clone(),
    };
    if let Ok(bin) = serde_json::to_vec(&join_msg) {
        let _ = socket.send(Message::Binary(bin));
    }

    let _ = event_tx.send(NetworkEvent::Connected {
        server_endpoint: format!("{}:{}", host, port),
        jwt_token,
        livekit_reachable,
    });

    let mut watched_host: Option<String> = None;
    let mut last_ping = Instant::now();
    let my_identity = local_participant.identity.clone();

    while !stop_flag.load(Ordering::Relaxed) {
        let mut did_work = false;

        // 1. Processar comandos de saída (Publicação de Vídeo/Áudio, Mudança de Estado)
        while let Ok(cmd) = cmd_rx.try_recv() {
            did_work = true;
            match cmd {
                OutgoingCommand::UpdateLocalParticipant(info) => {
                    if let Ok(bin) = serde_json::to_vec(&SfuWireMessage::UpdateParticipant(info)) {
                        let _ = socket.send(Message::Binary(bin));
                    }
                }
                OutgoingCommand::SubscribeToHost(target) => {
                    watched_host = target.clone();
                    let wire = match target {
                        Some(host_id) => SfuWireMessage::SubscribeStream {
                            viewer_identity: my_identity.clone(),
                            target_host_identity: host_id,
                        },
                        None => SfuWireMessage::UnsubscribeStream {
                            viewer_identity: my_identity.clone(),
                        },
                    };
                    if let Ok(bin) = serde_json::to_vec(&wire) {
                        let _ = socket.send(Message::Binary(bin));
                    }
                }
                OutgoingCommand::PublishVideo(packet) => {
                    if let Ok(bin) = serde_json::to_vec(&SfuWireMessage::VideoTrackPacket(packet)) {
                        let _ = socket.send(Message::Binary(bin));
                    }
                }
                OutgoingCommand::PublishAudio(packet) => {
                    if let Ok(bin) = serde_json::to_vec(&SfuWireMessage::AudioTrackPacket(packet)) {
                        let _ = socket.send(Message::Binary(bin));
                    }
                }
                OutgoingCommand::Disconnect => {
                    let _ = socket.close(None);
                    return;
                }
            }
        }

        // 2. Enviar Ping periódico de telemetria a cada 2s
        if last_ping.elapsed() >= Duration::from_secs(2) {
            last_ping = Instant::now();
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if let Ok(bin) = serde_json::to_vec(&SfuWireMessage::Ping(now_ms)) {
                let _ = socket.send(Message::Binary(bin));
            }
        }

        // 3. Ler pacotes recebidos da sala SFU
        match socket.read() {
            Ok(Message::Binary(data)) => {
                did_work = true;
                if let Ok(msg) = serde_json::from_slice::<SfuWireMessage>(&data) {
                    match msg {
                        SfuWireMessage::RoomState { participants } => {
                            let _ = event_tx.send(NetworkEvent::ParticipantsUpdated(participants));
                        }
                        SfuWireMessage::RequestKeyframe {
                            target_host_identity,
                        } => {
                            if target_host_identity == my_identity {
                                let _ = event_tx.send(NetworkEvent::KeyframeRequested);
                            }
                        }
                        SfuWireMessage::VideoTrackPacket(packet) => {
                            if watched_host.as_deref() == Some(packet.sender.as_str()) {
                                if let Some(decoded) = decode_video_packet(&packet) {
                                    let _ =
                                        event_tx.try_send(NetworkEvent::RemoteVideoFrame(decoded));
                                }
                            }
                        }
                        SfuWireMessage::AudioTrackPacket(packet) => {
                            if watched_host.as_deref() == Some(packet.sender.as_str()) {
                                if let Some(samples) = decode_audio_packet(&packet) {
                                    let _ = event_tx.try_send(NetworkEvent::RemoteAudioFrame {
                                        sender: packet.sender,
                                        source_pid: packet.source_pid,
                                        samples,
                                    });
                                }
                            }
                        }
                        SfuWireMessage::Pong(sent_ms) => {
                            let now_ms = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            let rtt = now_ms.saturating_sub(sent_ms);
                            let _ = event_tx.send(NetworkEvent::LatencyUpdatedMs(rtt));
                        }
                        _ => {}
                    }
                }
            }
            Ok(Message::Close(_)) => {
                let _ = event_tx.send(NetworkEvent::Disconnected(
                    "Conexão encerrada pelo servidor.".to_string(),
                ));
                break;
            }
            Err(tungstenite::Error::Io(ref err))
                if err.kind() == std::io::ErrorKind::WouldBlock
                    || err.kind() == std::io::ErrorKind::TimedOut => {}
            Err(err) => {
                let _ = event_tx.send(NetworkEvent::Disconnected(format!(
                    "Conexão perdida: {}",
                    err
                )));
                break;
            }
            _ => {}
        }

        if !did_work {
            thread::sleep(Duration::from_millis(2));
        }
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
    let _ = socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(4)));
    Some(socket)
}

static EMBEDDED_SFU_STARTED: AtomicBool = AtomicBool::new(false);

struct ClientPeer {
    info: ParticipantInfo,
    watching: Option<String>,
    tx: Sender<Vec<u8>>,
}

fn broadcast_room_state(peers: &Arc<Mutex<HashMap<String, ClientPeer>>>) {
    let map = peers.lock();
    let participants: Vec<ParticipantInfo> = map.values().map(|p| p.info.clone()).collect();
    if let Ok(bin) = serde_json::to_vec(&SfuWireMessage::RoomState { participants }) {
        for peer in map.values() {
            let _ = peer.tx.try_send(bin.clone());
        }
    }
}

/// Caso o usuário próprio seja o Host P2P ou o container SFU secundário não esteja na porta 7890,
/// sobe automaticamente um servidor SFU leve na porta 7890 capaz de atender até 10 amigos simultaneamente.
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
                    let Ok(mut ws) = accept(stream) else {
                        return;
                    };
                    let _ = ws
                        .get_ref()
                        .set_read_timeout(Some(Duration::from_millis(5)));

                    let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(256);
                    let mut my_id: Option<String> = None;

                    loop {
                        while let Ok(bin) = out_rx.try_recv() {
                            if ws.send(Message::Binary(bin)).is_err() {
                                break;
                            }
                        }

                        match ws.read() {
                            Ok(Message::Binary(data)) => {
                                let Ok(msg) = serde_json::from_slice::<SfuWireMessage>(&data)
                                else {
                                    continue;
                                };
                                match msg {
                                    SfuWireMessage::JoinRoom { participant, .. } => {
                                        let id = participant.identity.clone();
                                        my_id = Some(id.clone());
                                        {
                                            let mut map = peers_ref.lock();
                                            if map.len() < 10 {
                                                map.insert(
                                                    id,
                                                    ClientPeer {
                                                        info: participant,
                                                        watching: None,
                                                        tx: out_tx.clone(),
                                                    },
                                                );
                                            }
                                        }
                                        broadcast_room_state(&peers_ref);
                                    }
                                    SfuWireMessage::UpdateParticipant(info) => {
                                        {
                                            let mut map = peers_ref.lock();
                                            if let Some(peer) = map.get_mut(&info.identity) {
                                                peer.info = info;
                                            }
                                        }
                                        broadcast_room_state(&peers_ref);
                                    }
                                    SfuWireMessage::SubscribeStream {
                                        viewer_identity,
                                        target_host_identity,
                                    } => {
                                        let mut host_tx = None;
                                        {
                                            let mut map = peers_ref.lock();
                                            if let Some(viewer) = map.get_mut(&viewer_identity) {
                                                viewer.watching =
                                                    Some(target_host_identity.clone());
                                            }
                                            if let Some(host) = map.get(&target_host_identity) {
                                                host_tx = Some(host.tx.clone());
                                            }
                                        }
                                        if let Some(tx) = host_tx {
                                            if let Ok(req) = serde_json::to_vec(
                                                &SfuWireMessage::RequestKeyframe {
                                                    target_host_identity,
                                                },
                                            ) {
                                                let _ = tx.try_send(req);
                                            }
                                        }
                                    }
                                    SfuWireMessage::UnsubscribeStream { viewer_identity } => {
                                        let mut map = peers_ref.lock();
                                        if let Some(viewer) = map.get_mut(&viewer_identity) {
                                            viewer.watching = None;
                                        }
                                    }
                                    SfuWireMessage::VideoTrackPacket(ref pkt) => {
                                        let sender_id = pkt.sender.clone();
                                        let map = peers_ref.lock();
                                        for peer in map.values() {
                                            if peer.watching.as_deref() == Some(sender_id.as_str())
                                            {
                                                let _ = peer.tx.try_send(data.clone());
                                            }
                                        }
                                    }
                                    SfuWireMessage::AudioTrackPacket(ref pkt) => {
                                        let sender_id = pkt.sender.clone();
                                        let map = peers_ref.lock();
                                        for peer in map.values() {
                                            if peer.watching.as_deref() == Some(sender_id.as_str())
                                            {
                                                let _ = peer.tx.try_send(data.clone());
                                            }
                                        }
                                    }
                                    SfuWireMessage::Ping(ts) => {
                                        if let Ok(pong) =
                                            serde_json::to_vec(&SfuWireMessage::Pong(ts))
                                        {
                                            let _ = out_tx.try_send(pong);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Ok(Message::Close(_)) => break,
                            Err(tungstenite::Error::Io(ref e))
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::TimedOut =>
                            {
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(_) => break,
                            _ => {}
                        }
                    }

                    if let Some(id) = my_id {
                        peers_ref.lock().remove(&id);
                        broadcast_room_state(&peers_ref);
                    }
                });
            }
        })
        .ok();
}