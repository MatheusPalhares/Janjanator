use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResolutionPreset {
    Res720p,
    Res1080p,
    Native,
}

impl ResolutionPreset {
    pub fn label(&self) -> &'static str {
        match self {
            ResolutionPreset::Res720p => "720p (1280x720 - HD)",
            ResolutionPreset::Res1080p => "1080p (1920x1080 - Full HD)",
            ResolutionPreset::Native => "Nativo (Resolução da Fonte)",
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
            ResolutionPreset::Native => {
                // Ensure even dimensions for hardware H.264 encoders
                ((src_w & !1).max(64), (src_h & !1).max(64))
            }
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
            FpsPreset::Fps30 => "30 FPS",
            FpsPreset::Fps60 => "60 FPS (Fluido)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BitratePreset {
    Medium6Mbps,
    High12Mbps,
    Ultra25Mbps,
}

impl BitratePreset {
    pub fn bps(&self) -> u32 {
        match self {
            BitratePreset::Medium6Mbps => 6_000_000,
            BitratePreset::High12Mbps => 12_000_000,
            BitratePreset::Ultra25Mbps => 25_000_000,
        }
    }

    pub fn kbps(&self) -> u32 {
        self.bps() / 1000
    }

    pub fn jpeg_quality(&self) -> u8 {
        match self {
            BitratePreset::Medium6Mbps => 72,
            BitratePreset::High12Mbps => 85,
            BitratePreset::Ultra25Mbps => 94,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            BitratePreset::Medium6Mbps => "Médio (6 Mbps)",
            BitratePreset::High12Mbps => "Alto (12 Mbps)",
            BitratePreset::Ultra25Mbps => "Ultra (25 Mbps)",
        }
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
                format!("🖥 {} ({}x{})", self.title, self.width, self.height)
            }
            CaptureTargetKind::Window => {
                format!(
                    "🪟 {} [PID: {}] ({}x{})",
                    self.title, self.pid, self.width, self.height
                )
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub target: CaptureTarget,
    pub share_window_audio: bool,
    pub resolution: ResolutionPreset,
    pub fps: FpsPreset,
    pub bitrate: BitratePreset,
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

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub sender: String,
    pub sequence: u64,
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
    TargetClosed(String),
}

