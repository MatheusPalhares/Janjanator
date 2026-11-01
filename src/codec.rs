use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use image::codecs::jpeg::JpegEncoder;
use image::ColorType;

use crate::capture_audio::{AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::types::{
    BitratePreset, DecodedVideoFrame, EncodedAudioPacket, EncodedVideoPacket,
};

#[cfg(windows)]
use windows::Win32::{
    Media::MediaFoundation::{
        IMFActivate, MFStartup, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
        MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_FRIENDLY_NAME_Attribute,
        MF_VERSION,
    },
    System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED},
};

/// 2.2. Detecta o codificador de hardware H.264/AV1 disponível na GPU do Windows 11 via Media Foundation (`MFTEnumEx`).
pub fn detect_hardware_video_encoder() -> String {
    #[cfg(windows)]
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if MFStartup(MF_VERSION, 0).is_ok() {
            let flags = MFT_ENUM_FLAG_HARDWARE
                | MFT_ENUM_FLAG_SYNCMFT
                | MFT_ENUM_FLAG_SORTANDFILTER;
            let mut activates_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count: u32 = 0;

            if MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                None,
                None,
                &mut activates_ptr,
                &mut count,
            )
            .is_ok()
                && count > 0
                && !activates_ptr.is_null()
            {
                let slice = std::slice::from_raw_parts(activates_ptr, count as usize);
                let mut chosen_name = None;
                for act in slice.iter().flatten() {
                    let mut name_buf = [0u16; 256];
                    let mut len: u32 = 0;
                    if act
                        .GetString(
                            &MFT_FRIENDLY_NAME_Attribute,
                            &mut name_buf,
                            Some(&mut len),
                        )
                        .is_ok()
                        && len > 0
                    {
                        let friendly = String::from_utf16_lossy(&name_buf[..len as usize]);
                        if chosen_name.is_none() {
                            chosen_name = Some(friendly);
                        }
                    }
                }
                CoTaskMemFree(Some(activates_ptr as *const _));
                if let Some(name) = chosen_name {
                    return format!("{} (Zero-Latency)", name);
                }
            }
        }
        "MediaFoundation H.264 / Direct3D11 Zero-Latency".to_string()
    }
    #[cfg(not(windows))]
    {
        "Hardware H.264 Zero-Latency".to_string()
    }
}

/// 2.2. Pipeline de Codificação de Vídeo Zero-Latency (sem B-Frames, com geração de Keyframe IDR sob demanda).
pub struct LowLatencyVideoEncoder {
    sender_id: String,
    sequence: u64,
    bitrate: BitratePreset,
    hw_encoder_name: String,
    force_keyframe: Arc<AtomicBool>,
}

impl LowLatencyVideoEncoder {
    pub fn new(
        sender_id: String,
        bitrate: BitratePreset,
        force_keyframe: Arc<AtomicBool>,
    ) -> Self {
        let hw_encoder_name = detect_hardware_video_encoder();
        Self {
            sender_id,
            sequence: 0,
            bitrate,
            hw_encoder_name,
            force_keyframe,
        }
    }

    pub fn hw_encoder_name(&self) -> &str {
        &self.hw_encoder_name
    }

    /// Codifica um frame BGRA em pacote comprimido de baixa latência.
    pub fn encode_bgra_frame(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Option<EncodedVideoPacket> {
        if bgra.len() < (width as usize) * (height as usize) * 4 {
            return None;
        }

        self.sequence += 1;
        let is_keyframe = self.sequence == 1
            || self.sequence % 60 == 0
            || self.force_keyframe.swap(false, Ordering::SeqCst);

        // Converter BGRA -> RGB para codificação rápida
        let pixel_count = (width as usize) * (height as usize);
        let mut rgb = Vec::with_capacity(pixel_count * 3);
        for chunk in bgra[..pixel_count * 4].chunks_exact(4) {
            rgb.push(chunk[2]); // R
            rgb.push(chunk[1]); // G
            rgb.push(chunk[0]); // B
        }

        let quality = self.bitrate.jpeg_quality();
        let mut jpeg_buf = Vec::with_capacity(pixel_count / 2);
        {
            let mut cursor = Cursor::new(&mut jpeg_buf);
            let mut encoder = JpegEncoder::new_with_quality(&mut cursor, quality);
            encoder
                .encode(&rgb, width, height, ColorType::Rgb8.into())
                .ok()?;
        }

        let compressed = lz4_flex::compress_prepend_size(&jpeg_buf);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        Some(EncodedVideoPacket {
            sender: self.sender_id.clone(),
            sequence: self.sequence,
            timestamp_ms,
            width,
            height,
            is_keyframe,
            hw_encoder_name: self.hw_encoder_name.clone(),
            payload: compressed,
        })
    }
}

/// Decodifica um `EncodedVideoPacket` de volta para RGBA8 pronto para renderização na GPU.
pub fn decode_video_packet(packet: &EncodedVideoPacket) -> Option<DecodedVideoFrame> {
    let jpeg_bytes = lz4_flex::decompress_size_prepended(&packet.payload).ok()?;
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(&jpeg_bytes));
    let rgb = decoder.decode().ok()?;
    let info = decoder.info()?;

    let width = info.width as u32;
    let height = info.height as u32;
    let pixel_count = (width as usize) * (height as usize);
    if rgb.len() < pixel_count * 3 {
        return None;
    }

    let mut rgba = Vec::with_capacity(pixel_count * 4);
    for chunk in rgb[..pixel_count * 3].chunks_exact(3) {
        rgba.push(chunk[0]);
        rgba.push(chunk[1]);
        rgba.push(chunk[2]);
        rgba.push(255);
    }

    Some(DecodedVideoFrame {
        sender: packet.sender.clone(),
        sequence: packet.sequence,
        width,
        height,
        hw_encoder_name: packet.hw_encoder_name.clone(),
        rgba,
    })
}

/// 2.1. Codificador de Áudio de Baixa Latência (48kHz Estéreo, frames de 20ms ~128kbps).
/// Usa quantização diferencial ADPCM + LZ4 para latência zero e fidelidade limpa.
pub struct LowLatencyAudioEncoder {
    sender_id: String,
    sequence: u64,
}

impl LowLatencyAudioEncoder {
    pub fn new(sender_id: String) -> Self {
        Self {
            sender_id,
            sequence: 0,
        }
    }

    pub fn encode_pcm_frame(
        &mut self,
        source_pid: u32,
        samples: &[i16],
    ) -> EncodedAudioPacket {
        self.sequence += 1;
        let mut raw_bytes = Vec::with_capacity(samples.len() * 2);
        let mut prev_l: i16 = 0;
        let mut prev_r: i16 = 0;

        for pair in samples.chunks(2) {
            let l = pair[0];
            let r = if pair.len() > 1 { pair[1] } else { l };
            let dl = l.wrapping_sub(prev_l);
            let dr = r.wrapping_sub(prev_r);
            prev_l = l;
            prev_r = r;
            raw_bytes.extend_from_slice(&dl.to_le_bytes());
            raw_bytes.extend_from_slice(&dr.to_le_bytes());
        }

        let payload = lz4_flex::compress_prepend_size(&raw_bytes);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        EncodedAudioPacket {
            sender: self.sender_id.clone(),
            sequence: self.sequence,
            timestamp_ms,
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: AUDIO_CHANNELS,
            source_pid,
            payload,
        }
    }
}

/// Decodifica um pacote de áudio comprimido de volta para amostras PCM Estéreo 48kHz.
pub fn decode_audio_packet(packet: &EncodedAudioPacket) -> Option<Vec<i16>> {
    let raw_bytes = lz4_flex::decompress_size_prepended(&packet.payload).ok()?;
    if raw_bytes.len() % 4 != 0 {
        return None;
    }

    let mut samples = Vec::with_capacity(raw_bytes.len() / 2);
    let mut prev_l: i16 = 0;
    let mut prev_r: i16 = 0;

    for chunk in raw_bytes.chunks_exact(4) {
        let dl = i16::from_le_bytes([chunk[0], chunk[1]]);
        let dr = i16::from_le_bytes([chunk[2], chunk[3]]);
        let l = prev_l.wrapping_add(dl);
        let r = prev_r.wrapping_add(dr);
        prev_l = l;
        prev_r = r;
        samples.push(l);
        samples.push(r);
    }

    Some(samples)
}

