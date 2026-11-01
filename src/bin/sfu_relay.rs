use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tungstenite::{accept, Message};

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
pub struct EncodedVideoPacket {
    pub sender: String,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub width: u32,
    pub height: u32,
    pub is_keyframe: bool,
    pub hw_encoder_name: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncodedAudioPacket {
    pub sender: String,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub source_pid: u32,
    pub payload: Vec<u8>,
}

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

fn main() {
    println!("=============================================================");
    println!("  ScreenShare Win11 Native - Docker SFU Relay & Hub Server   ");
    println!("  - Media & Signaling Relay: 0.0.0.0:7890 (Max 10 Users)     ");
    println!("  - HTTP Portal & Download : 0.0.0.0:7888                    ");
    println!("=============================================================");

    // Copiar executável compilado para o volume /dist do Host se existir
    if let Ok(bytes) = fs::read("/app/screenshare-win11.exe") {
        let _ = fs::create_dir_all("/dist");
        if fs::write("/dist/screenshare-win11.exe", &bytes).is_ok() {
            println!(
                "[OK] Executável Windows 11 exportado para ./dist/screenshare-win11.exe ({:.2} MB)",
                bytes.len() as f64 / (1024.0 * 1024.0)
            );
        }
    }

    let peers: Arc<Mutex<HashMap<String, ClientPeer>>> = Arc::new(Mutex::new(HashMap::new()));
    let peers_http = peers.clone();

    // Thread HTTP Portal (:7888) para Healthcheck e Download direto do .exe pelos amigos
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

    // Servidor Principal WebSocket SFU (:7890)
    let listener = TcpListener::bind("0.0.0.0:7890").expect("Falha ao fazer bind em 0.0.0.0:7890");

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
                        let Ok(msg) = serde_json::from_slice::<SfuWireMessage>(&data) else {
                            continue;
                        };
                        match msg {
                            SfuWireMessage::JoinRoom { participant, .. } => {
                                let id = participant.identity.clone();
                                println!(
                                    "[JOIN] Usuário '{}' ({}) entrou na sala.",
                                    participant.nickname, id
                                );
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
                                        viewer.watching = Some(target_host_identity.clone());
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
                                    if peer.watching.as_deref() == Some(sender_id.as_str()) {
                                        let _ = peer.tx.try_send(data.clone());
                                    }
                                }
                            }
                            SfuWireMessage::AudioTrackPacket(ref pkt) => {
                                let sender_id = pkt.sender.clone();
                                let map = peers_ref.lock();
                                for peer in map.values() {
                                    if peer.watching.as_deref() == Some(sender_id.as_str()) {
                                        let _ = peer.tx.try_send(data.clone());
                                    }
                                }
                            }
                            SfuWireMessage::Ping(ts) => {
                                if let Ok(pong) = serde_json::to_vec(&SfuWireMessage::Pong(ts)) {
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
                println!("[LEAVE] Usuário '{}' saiu da sala.", id);
                peers_ref.lock().remove(&id);
                broadcast_room_state(&peers_ref);
            }
        });
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

    if first_line.starts_with("GET /download/screenshare-win11.exe") {
        if let Ok(exe_bytes) = fs::read("/dist/screenshare-win11.exe")
            .or_else(|_| fs::read("/app/screenshare-win11.exe"))
        {
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"screenshare-win11.exe\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
<title>ScreenShare Win11 Native - Servidor Ativo</title>
<style>
body {{ background:#0d1017; color:#e5e7eb; font-family:system-ui,sans-serif; display:flex; align-items:center; justify-content:center; height:100vh; margin:0; }}
.card {{ background:#161922; border:1px solid #374151; border-radius:12px; padding:32px; max-width:520px; text-align:center; box-shadow:0 10px 30px rgba(0,0,0,0.5); }}
h1 {{ color:#60a5fa; margin-top:0; }}
.badge {{ display:inline-block; background:#065f46; color:#34d399; padding:6px 14px; border-radius:999px; font-weight:600; font-size:14px; margin-bottom:18px; }}
.btn {{ display:inline-block; background:#2563eb; color:#fff; text-decoration:none; padding:12px 24px; border-radius:8px; font-weight:700; margin-top:16px; }}
.btn:hover {{ background:#1d4ed8; }}
code {{ background:#1f2937; padding:3px 6px; border-radius:4px; color:#93c5fd; }}
</style>
</head>
<body>
<div class="card">
  <h1>⚡ ScreenShare Win11 Native</h1>
  <div class="badge">● Servidor Docker SFU Online ({}/10 usuários na sala)</div>
  <p>Servidor LiveKit SFU (<code>:7880</code>) + Media Relay (<code>:7890</code>) operacional.</p>
  <p>Baixe o executável portátil para Windows 11 abaixo ou pela aba <b>Releases</b> do GitHub:</p>
  <a class="btn" href="/download/screenshare-win11.exe">⬇ Baixar screenshare-win11.exe (Portátil)</a>
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

