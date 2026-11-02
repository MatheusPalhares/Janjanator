use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tungstenite::protocol::WebSocket;
use tungstenite::{accept, Message};

pub const WIRE_TAG_CONTROL_JSON: u8 = 0x01;
pub const WIRE_TAG_VIDEO_BINARY: u8 = 0x02;
pub const WIRE_TAG_AUDIO_BINARY: u8 = 0x03;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParticipantInfo {
    pub identity: String,
    pub nickname: String,
    pub is_streaming: bool,
    pub stream_title: String,
    pub resolution_label: String,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub has_isolated_audio: bool,
    pub audio_pid: u32,
}

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

fn peek_binary_packet_sender(data: &[u8]) -> Option<&str> {
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

/// Envia uma mensagem WebSocket com controle estrito de `pending_flush` para impedir
/// que o `write_buffer` interno do `tungstenite` acumule múltiplos frames em conexões ZeroTier.
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

fn main() {
    println!("=============================================================");
    println!("  Janjanator - Docker SFU Relay & Hub v2 (60fps)             ");
    println!("  - Zero-Copy Binary Relay  : 0.0.0.0:7890 (Max 10 Users)     ");
    println!("  - HTTP Portal & Download  : 0.0.0.0:7888                    ");
    println!("=============================================================");

    thread::spawn(|| {
        if let Ok(bytes) = fs::read("/app/Janjanator.exe") {
            let _ = fs::create_dir_all("/dist");
            let _ = fs::remove_file("/dist/screenshare-win11.exe");
            loop {
                let needs_update = match fs::read("/dist/Janjanator.exe") {
                    Ok(existing) => existing != bytes,
                    Err(_) => true,
                };
                if needs_update {
                    if fs::write("/dist/Janjanator.exe", &bytes).is_ok() {
                        println!(
                            "[OK] Executável exportado para ./dist/Janjanator.exe ({:.2} MB)",
                            bytes.len() as f64 / (1024.0 * 1024.0)
                        );
                        break;
                    }
                } else {
                    break;
                }
                thread::sleep(Duration::from_secs(2));
            }
        }
    });

    let peers: Arc<Mutex<HashMap<String, ClientPeer>>> = Arc::new(Mutex::new(HashMap::new()));
    let peers_http = peers.clone();

    thread::spawn(move || {
        if let Ok(http_listener) = TcpListener::bind("0.0.0.0:7888") {
            for stream in http_listener.incoming().flatten() {
                let peers_ref = peers_http.clone();
                thread::spawn(move || {
                    handle_http_request(stream, &peers_ref);
                });
            }
        }
    });

    let listener = TcpListener::bind("0.0.0.0:7890").expect("Falha ao fazer bind em 0.0.0.0:7890");

    for stream in listener.incoming().flatten() {
        let _ = stream.set_nodelay(true);
        let peers_ref = peers.clone();

        thread::spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                handle_peer_connection(stream, peers_ref);
            }));
        });
    }
}

fn handle_peer_connection(
    stream: TcpStream,
    peers_ref: Arc<Mutex<HashMap<String, ClientPeer>>>,
) {
    let Ok(mut ws) = accept(stream) else {
        return;
    };
    let _ = ws.get_ref().set_nonblocking(true);

    let (ctrl_tx, ctrl_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
    let (audio_tx, audio_rx) = crossbeam_channel::bounded::<Vec<u8>>(64);
    // Fila de vídeo de 1 único slot: impede fisicamente acumular 10 segundos de atraso no SFU
    let (video_tx, video_rx) = crossbeam_channel::bounded::<Vec<u8>>(1);
    let mut my_id: Option<String> = None;
    let mut pending_flush = false;

    loop {
        let mut active = false;

        // 1. Se o socket estava com flush pendente (WouldBlock), tentar descarregar antes de colocar mais dados no buffer do tungstenite
        if pending_flush {
            match ws.flush() {
                Ok(()) => {
                    pending_flush = false;
                    active = true;
                }
                Err(tungstenite::Error::Io(ref err))
                    if err.kind() == ErrorKind::WouldBlock
                        || err.kind() == ErrorKind::TimedOut
                        || err.kind() == ErrorKind::Interrupted =>
                {
                    // Socket TCP ainda enviando o pacote anterior; não empilhar novos pacotes
                }
                Err(_) => break,
            }
        }

        // 2. Só enviar novos pacotes quando o buffer do WebSocket estiver livre (!pending_flush)
        // Prioridade 1: Controle | Prioridade 2: Áudio (nunca picota!) | Prioridade 3: Vídeo (1 frame mais recente)
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

            if !pending_flush {
                if let Ok(bin) = video_rx.try_recv() {
                    active = true;
                    if !send_ws_gated(&mut ws, bin, &mut pending_flush) {
                        return;
                    }
                }
            }
        }

        // 3. Ler pacotes recebidos (Full-Duplex)
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
                                        // Se a fila de áudio do espectador encher por pico extremo, descarta o mais antigo
                                        if peer.audio_tx.try_send(data.clone()).is_err() {
                                            let _ = peer.audio_tx.try_send(data.clone());
                                        }
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
                                                // O link ZeroTier do espectador ainda não terminou o frame anterior:
                                                // substitui pelo frame mais novo (Zero-Lag) e avisa o Host para ajustar bitrate/keyframe!
                                                let _ = peer.video_rx.try_recv();
                                                let _ = peer.video_tx.try_send(data.clone());
                                                if peer.last_congestion_notify.elapsed()
                                                    >= Duration::from_millis(120)
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
                                    println!(
                                        "[JOIN] Usuário '{}' ({}) conectou.",
                                        participant.nickname, id
                                    );
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
                        println!("[LEAVE] Usuário '{}' desconectou.", id);
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
                        println!("[LEAVE] Usuário '{}' desconectou.", id);
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

fn handle_http_request(
    mut stream: TcpStream,
    peers: &Arc<Mutex<HashMap<String, ClientPeer>>>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 2048];
    let Ok(n) = stream.read(&mut buf) else {
        return;
    };
    if n == 0 {
        return;
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let first_line = req.lines().next().unwrap_or("");

    if first_line.starts_with("GET /download/Janjanator.exe")
        || first_line.starts_with("GET /download/screenshare-win11.exe")
    {
        if let Ok(exe_bytes) = fs::read("/app/Janjanator.exe")
            .or_else(|_| fs::read("/dist/Janjanator.exe"))
        {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"Janjanator.exe\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                exe_bytes.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&exe_bytes);
            return;
        }
    }

    let online_count = peers.lock().len();
    let html = format!(
        r#"<!DOCTYPE html>
<html lang="pt-BR">
<head>
<meta charset="utf-8"/>
<title>Janjanator - Servidor Ativo</title>
<style>
body {{ background:#0d1017; color:#e5e7eb; font-family:system-ui,sans-serif; display:flex; align-items:center; justify-content:center; height:100vh; margin:0; }}
.card {{ background:#161922; border:1px solid #374151; border-radius:12px; padding:32px; max-width:520px; text-align:center; box-shadow:0 10px 30px rgba(0,0,0,0.5); }}
h1 {{ color:#f87171; margin-top:0; }}
.badge {{ display:inline-block; background:#065f46; color:#34d399; padding:6px 14px; border-radius:999px; font-weight:600; font-size:14px; margin-bottom:18px; }}
.btn {{ display:inline-block; background:#dc2626; color:#fff; text-decoration:none; padding:12px 24px; border-radius:8px; font-weight:700; margin-top:16px; }}
.btn:hover {{ background:#b91c1c; }}
code {{ background:#1f2937; padding:3px 6px; border-radius:4px; color:#93c5fd; }}
</style>
</head>
<body>
<div class="card">
  <h1>⭐ Janjanator</h1>
  <div class="badge">● Servidor Docker SFU Online ({}/10 usuários na sala)</div>
  <p>Servidor LiveKit SFU (<code>:7880</code>) + Binary Media Relay 60fps (<code>:7890</code>) operacional.</p>
  <p>Baixe o executável portátil para Windows 11 abaixo ou pela aba <b>Releases</b> do GitHub:</p>
  <a class="btn" href="/download/Janjanator.exe">⬇ Baixar Janjanator.exe (Portátil)</a>
</div>
</body>
</html>"#,
        online_count
    );

    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        html.len(),
        html
    );
    let _ = stream.write_all(resp.as_bytes());
}
