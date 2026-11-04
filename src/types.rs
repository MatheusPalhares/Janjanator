use std::io::{Cursor, Read, Write};
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use serde::{Deserialize, Serialize};

pub const APP_VERSION: &str = match option_env!("JANJANATOR_BUILD_VERSION") {
    Some(v) => v,
    None => "v0.6.8",
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionPreset {
    Res720p,
    Res1080p,
    Native,
}

impl ResolutionPreset {
    pub fn label(&self) -> &'static str {
        match self {
            ResolutionPreset::Res720p => "720p HD (1280×720)",
            ResolutionPreset::Res1080p => "1080p Full HD (1920×1080)",
            ResolutionPreset::Native => "Nativo (Resolução Original)",
        }
    }

    pub fn short_label(&self) -> &'static str {
        match self {
            ResolutionPreset::Res720p => "720p",
            ResolutionPreset::Res1080p => "1080p",
            ResolutionPreset::Native => "Nativo",
        }
    }

    pub fn target_dimensions(&self, src_w: u32, src_h: u32) -> (u32, u32) {
        let src_w = src_w.max(64);
        let src_h = src_h.max(64);
        match self {
            ResolutionPreset::Native => ((src_w & !1).max(64), (src_h & !1).max(64)),
            ResolutionPreset::Res720p => scale_preserving_aspect(src_w, src_h, 1280, 720),
            ResolutionPreset::Res1080p => scale_preserving_aspect(src_w, src_h, 1920, 1080),
        }
    }
}

fn scale_preserving_aspect(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w <= max_w && src_h <= max_h {
        return ((src_w & !1).max(64), (src_h & !1).max(64));
    }
    let ratio_w = max_w as f64 / src_w as f64;
    let ratio_h = max_h as f64 / src_h as f64;
    let ratio = ratio_w.min(ratio_h);
    let out_w = ((src_w as f64 * ratio).round() as u32) & !1;
    let out_h = ((src_h as f64 * ratio).round() as u32) & !1;
    (out_w.max(64), out_h.max(64))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FpsPreset {
    Fps30,
    Fps60,
}

impl FpsPreset {
    pub fn value(&self) -> u32 {
        match self {
            FpsPreset::Fps30 => 30,
            FpsPreset::Fps60 => 60,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            FpsPreset::Fps30 => "30 FPS (Estável)",
            FpsPreset::Fps60 => "60 FPS (Ultra Fluido)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BitratePreset {
    AutoAdaptive,
}

impl BitratePreset {
    pub fn bps(&self) -> u32 {
        16_000_000
    }

    pub fn kbps(&self) -> u32 {
        self.bps() / 1000
    }

    pub fn initial_quality(&self) -> u8 {
        80
    }

    pub fn label(&self) -> &'static str {
        "⚡ Automático (Máximo Estável)"
    }

    pub fn short_label(&self) -> &'static str {
        "Auto Máx"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureTargetKind {
    Monitor,
    Window,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureTarget {
    pub id: u64,
    pub title: String,
    pub process_name: String,
    pub hwnd: isize,
    pub hmonitor: isize,
    pub pid: u32,
    pub width: u32,
    pub height: u32,
    pub kind: CaptureTargetKind,
}

impl CaptureTarget {
    pub fn display_label(&self) -> String {
        match self.kind {
            CaptureTargetKind::Monitor => {
                format!("🖥 {} ({}×{})", self.title, self.width, self.height)
            }
            CaptureTargetKind::Window => {
                format!(
                    "🪟 {} [{} | PID {}] ({}×{})",
                    self.title, self.process_name, self.pid, self.width, self.height
                )
            }
        }
    }
}

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

pub const WIRE_TAG_CONTROL_JSON: u8 = 0x01;
pub const WIRE_TAG_VIDEO_BINARY: u8 = 0x02;
pub const WIRE_TAG_AUDIO_BINARY: u8 = 0x03;

#[derive(Debug, Clone)]
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

impl EncodedVideoPacket {
    /// Serialização binária ultra-rápida (Zero-Copy overhead, sem JSON).
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let sender_bytes = self.sender.as_bytes();
        let enc_bytes = self.hw_encoder_name.as_bytes();
        let mut buf = Vec::with_capacity(32 + sender_bytes.len() + enc_bytes.len() + self.payload.len());

        let _ = buf.write_u8(WIRE_TAG_VIDEO_BINARY);
        let _ = buf.write_u16::<LittleEndian>(sender_bytes.len() as u16);
        let _ = buf.write_all(sender_bytes);
        let _ = buf.write_u64::<LittleEndian>(self.sequence);
        let _ = buf.write_u64::<LittleEndian>(self.timestamp_ms);
        let _ = buf.write_u32::<LittleEndian>(self.width);
        let _ = buf.write_u32::<LittleEndian>(self.height);
        let _ = buf.write_u8(u8::from(self.is_keyframe));
        let _ = buf.write_u16::<LittleEndian>(enc_bytes.len() as u16);
        let _ = buf.write_all(enc_bytes);
        let _ = buf.write_u32::<LittleEndian>(self.payload.len() as u32);
        let _ = buf.write_all(&self.payload);
        buf
    }

    pub fn from_wire_bytes(data: &[u8]) -> Option<Self> {
        if data.first().copied()? != WIRE_TAG_VIDEO_BINARY {
            return None;
        }
        let mut cur = Cursor::new(&data[1..]);
        let sender_len = cur.read_u16::<LittleEndian>().ok()? as usize;
        let mut sender_buf = vec![0u8; sender_len];
        cur.read_exact(&mut sender_buf).ok()?;
        let sender = String::from_utf8(sender_buf).ok()?;

        let sequence = cur.read_u64::<LittleEndian>().ok()?;
        let timestamp_ms = cur.read_u64::<LittleEndian>().ok()?;
        let width = cur.read_u32::<LittleEndian>().ok()?;
        let height = cur.read_u32::<LittleEndian>().ok()?;
        let is_keyframe = cur.read_u8().ok()? != 0;

        let enc_len = cur.read_u16::<LittleEndian>().ok()? as usize;
        let mut enc_buf = vec![0u8; enc_len];
        cur.read_exact(&mut enc_buf).ok()?;
        let hw_encoder_name = String::from_utf8(enc_buf).ok()?;

        let payload_len = cur.read_u32::<LittleEndian>().ok()? as usize;
        let pos = cur.position() as usize + 1;
        if pos + payload_len > data.len() {
            return None;
        }
        let payload = data[pos..pos + payload_len].to_vec();

        Some(Self {
            sender,
            sequence,
            timestamp_ms,
            width,
            height,
            is_keyframe,
            hw_encoder_name,
            payload,
        })
    }
}

#[derive(Debug, Clone)]
pub struct EncodedAudioPacket {
    pub sender: String,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub source_pid: u32,
    pub payload: Vec<u8>,
}

impl EncodedAudioPacket {
    pub fn to_wire_bytes(&self) -> Vec<u8> {
        let sender_bytes = self.sender.as_bytes();
        let mut buf = Vec::with_capacity(32 + sender_bytes.len() + self.payload.len());

        let _ = buf.write_u8(WIRE_TAG_AUDIO_BINARY);
        let _ = buf.write_u16::<LittleEndian>(sender_bytes.len() as u16);
        let _ = buf.write_all(sender_bytes);
        let _ = buf.write_u64::<LittleEndian>(self.sequence);
        let _ = buf.write_u64::<LittleEndian>(self.timestamp_ms);
        let _ = buf.write_u32::<LittleEndian>(self.sample_rate);
        let _ = buf.write_u16::<LittleEndian>(self.channels);
        let _ = buf.write_u32::<LittleEndian>(self.source_pid);
        let _ = buf.write_u32::<LittleEndian>(self.payload.len() as u32);
        let _ = buf.write_all(&self.payload);
        buf
    }

    pub fn from_wire_bytes(data: &[u8]) -> Option<Self> {
        if data.first().copied()? != WIRE_TAG_AUDIO_BINARY {
            return None;
        }
        let mut cur = Cursor::new(&data[1..]);
        let sender_len = cur.read_u16::<LittleEndian>().ok()? as usize;
        let mut sender_buf = vec![0u8; sender_len];
        cur.read_exact(&mut sender_buf).ok()?;
        let sender = String::from_utf8(sender_buf).ok()?;

        let sequence = cur.read_u64::<LittleEndian>().ok()?;
        let timestamp_ms = cur.read_u64::<LittleEndian>().ok()?;
        let sample_rate = cur.read_u32::<LittleEndian>().ok()?;
        let channels = cur.read_u16::<LittleEndian>().ok()?;
        let source_pid = cur.read_u32::<LittleEndian>().ok()?;

        let payload_len = cur.read_u32::<LittleEndian>().ok()? as usize;
        let pos = cur.position() as usize + 1;
        if pos + payload_len > data.len() {
            return None;
        }
        let payload = data[pos..pos + payload_len].to_vec();

        Some(Self {
            sender,
            sequence,
            timestamp_ms,
            sample_rate,
            channels,
            source_pid,
            payload,
        })
    }
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub hw_encoder_name: String,
    pub rgba: Vec<u8>,
}

#[derive(Debug, Clone)]
pub enum CaptureEvent {
    FrameCaptured {
        width: u32,
        height: u32,
        bgra: Vec<u8>,
    },
    TargetMinimized,
    TargetRestored,
    TargetClosed,
}
