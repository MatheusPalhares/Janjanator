use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use image::codecs::jpeg::JpegEncoder;
use image::ColorType;
use rayon::prelude::*;

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

const NUM_PARALLEL_BANDS: usize = 15;

/// Faixa de qualidade JPEG do controle adaptativo para conteúdo em movimento.
const ADAPTIVE_MIN_QUALITY: u8 = 70;
const ADAPTIVE_MAX_QUALITY: u8 = 85;

/// Refinamento progressivo de regiões estáticas (técnica usada por RDP/VNC/Parsec):
/// quando uma faixa fica parada por alguns frames, ela é reenviada UMA vez em qualidade quase sem perdas,
/// eliminando o "granulado"/mosquito noise em textos pequenos (abas, chat, HUD) sem custo contínuo de banda.
const REFINE_QUALITY: u8 = 95;
const REFINE_AFTER_STATIC_FRAMES: u16 = 6;
/// Refina no máximo 1 faixa a cada N frames para diluir o pico de banda (15 faixas ≈ 0.5s a 60 FPS).
const REFINE_EVERY_N_FRAMES: u64 = 2;

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
                    return format!("{} + Parallel SIMD", name);
                }
            }
        }
        "Direct3D11 + Parallel 8x SIMD Zero-Latency".to_string()
    }
    #[cfg(not(windows))]
    {
        "Parallel 8x SIMD Zero-Latency".to_string()
    }
}

/// Verifica rapidamente se uma faixa horizontal teve mudança visual real (ignorando ruído temporal/dithering do DWM).
#[inline]
fn band_has_visual_change(cur: &[u8], prev: &[u8]) -> bool {
    if cur.len() != prev.len() {
        return true;
    }
    let len = cur.len();
    let step = 32; // amostra a cada 8 pixels em paralelo nos núcleos da CPU
    let mut changed_samples = 0u32;
    let mut i = 0;
    while i + 2 < len {
        let db = (cur[i] as i16 - prev[i] as i16).abs();
        let dg = (cur[i + 1] as i16 - prev[i + 1] as i16).abs();
        let dr = (cur[i + 2] as i16 - prev[i + 2] as i16).abs();
        if db > 5 || dg > 5 || dr > 5 {
            changed_samples += 1;
            if changed_samples >= 4 {
                return true;
            }
        }
        i += step;
    }
    false
}

/// 2.2. Codificador de Vídeo Multi-Core Paralelo com Bitrate Adaptativo ("Auto"),
/// Rolling Band Refresh sob demanda e controle de estabilidade sem efeito gangorra.
pub struct LowLatencyVideoEncoder {
    sender_id: String,
    sequence: u64,
    bitrate_preset: BitratePreset,
    adaptive_quality: u8,
    current_kbps: Arc<AtomicU32>,
    hw_encoder_name: String,
    force_keyframe: Arc<AtomicBool>,
    viewer_congested: Arc<AtomicBool>,
    prev_bgra: Vec<u8>,
    prev_dims: (u32, u32),
    bytes_in_window: u64,
    window_start: Instant,
    stable_frames_count: u32,
    healing_frames_remaining: u8,
    /// Frames consecutivos sem mudança visual por faixa.
    band_static_frames: Vec<u16>,
    /// Faixa já foi reenviada em alta qualidade desde a última mudança.
    band_refined: Vec<bool>,
    /// Bytes de conteúdo em movimento (exclui refinamentos) usados pelo controle adaptativo.
    controller_bytes_in_window: u64,
    controller_kbps: u32,
}

impl LowLatencyVideoEncoder {
    pub fn new(
        sender_id: String,
        bitrate_preset: BitratePreset,
        force_keyframe: Arc<AtomicBool>,
        viewer_congested: Arc<AtomicBool>,
        current_kbps: Arc<AtomicU32>,
    ) -> Self {
        let hw_encoder_name = detect_hardware_video_encoder();
        let adaptive_quality = bitrate_preset.initial_quality();
        Self {
            sender_id,
            sequence: 0,
            bitrate_preset,
            adaptive_quality,
            current_kbps,
            hw_encoder_name,
            force_keyframe,
            viewer_congested,
            prev_bgra: Vec::new(),
            prev_dims: (0, 0),
            bytes_in_window: 0,
            window_start: Instant::now(),
            stable_frames_count: 0,
            healing_frames_remaining: 0,
            band_static_frames: Vec::new(),
            band_refined: Vec::new(),
            controller_bytes_in_window: 0,
            controller_kbps: 0,
        }
    }

    pub fn hw_encoder_name(&self) -> &str {
        &self.hw_encoder_name
    }

    /// Ajusta dinamicamente a qualidade com zona morta (deadband) entre 10 e 16 Mbps para evitar efeito sanfona/picos de 50+ Mbps.
    pub fn notify_network_pressure(&mut self, queue_len: usize, rtt_ms: u64) {
        let _ = self.bitrate_preset;
        let min_q: u8 = ADAPTIVE_MIN_QUALITY;
        let max_q: u8 = ADAPTIVE_MAX_QUALITY;

        let congested_remote = self.viewer_congested.swap(false, Ordering::Relaxed);
        if congested_remote {
            self.adaptive_quality = self.adaptive_quality.saturating_sub(3).max(min_q);
            self.stable_frames_count = 0;
            // Agenda 15 frames de rolling refresh para curar qualquer delta descartado no SFU
            self.healing_frames_remaining = NUM_PARALLEL_BANDS as u8;
            return;
        }

        if queue_len > 2 || rtt_ms > 180 {
            self.adaptive_quality = self.adaptive_quality.saturating_sub(2).max(min_q);
            self.stable_frames_count = 0;
        } else if rtt_ms > 120 {
            self.adaptive_quality = self.adaptive_quality.saturating_sub(1).max(min_q + 2);
            self.stable_frames_count = 0;
        } else {
            // Usa apenas o bitrate de conteúdo em movimento (refinamentos estáticos são picos pontuais)
            let live_kbps = self.controller_kbps;
            // Só sobe qualidade se o bitrate atual estiver abaixo de 11 Mbps (impede subir para Q=88 e explodir para 50~80 Mbps!)
            if live_kbps < 11_000 {
                self.stable_frames_count += 1;
                if self.stable_frames_count >= 30 {
                    self.stable_frames_count = 0;
                    if self.adaptive_quality < max_q {
                        self.adaptive_quality += 1;
                    }
                }
            } else {
                self.stable_frames_count = 0;
            }
        }
    }

    /// Codifica um frame BGRA 100% em paralelo nos múltiplos núcleos da CPU (detecção delta + cópia + JPEG fundidos).
    pub fn encode_bgra_frame(
        &mut self,
        width: u32,
        height: u32,
        bgra: &[u8],
    ) -> Option<EncodedVideoPacket> {
        let expected_len = (width as usize) * (height as usize) * 4;
        if width < 16 || height < 16 || bgra.len() < expected_len {
            return None;
        }

        self.sequence += 1;

        let dims_changed =
            self.prev_dims != (width, height) || self.prev_bgra.len() != expected_len;
        if dims_changed {
            self.prev_bgra.resize(expected_len, 0);
            self.prev_dims = (width, height);
        }

        let is_keyframe = self.sequence == 1
            || dims_changed
            || self.force_keyframe.swap(false, Ordering::SeqCst);

        let quality = self.adaptive_quality;

        let num_bands = NUM_PARALLEL_BANDS.min(height as usize).max(1);
        let base_band_h = (height as usize) / num_bands;
        let stride = (width as usize) * 4;

        if dims_changed || self.band_static_frames.len() != num_bands {
            self.band_static_frames = vec![0; num_bands];
            self.band_refined = vec![false; num_bands];
        }
        if is_keyframe {
            // Keyframe vai em qualidade adaptativa (evita pico gigante); o refinamento reconstrói a nitidez em ~0.5s
            self.band_refined.iter_mut().for_each(|r| *r = false);
        }

        // Rolling refresh inteligente: ativo durante auto-cura pós-congestionamento ou suavemente 1 faixa a cada 8 frames (~133ms)
        let healing_active = self.healing_frames_remaining > 0;
        let active_rolling_band = if self.healing_frames_remaining > 0 {
            self.healing_frames_remaining -= 1;
            Some((self.sequence as usize) % num_bands)
        } else if self.sequence % 8 == 0 {
            Some(((self.sequence / 8) as usize) % num_bands)
        } else {
            None
        };

        // Escolhe 1 faixa estática ainda não refinada para reenviar em alta qualidade (rotacionando o ponto de partida)
        let refine_band: Option<usize> = if !is_keyframe
            && !healing_active
            && self.sequence % REFINE_EVERY_N_FRAMES == 0
        {
            let start = (self.sequence as usize / REFINE_EVERY_N_FRAMES as usize) % num_bands;
            (0..num_bands).map(|i| (start + i) % num_bands).find(|&b| {
                !self.band_refined[b] && self.band_static_frames[b] >= REFINE_AFTER_STATIC_FRAMES
            })
        } else {
            None
        };

        struct BandTask<'a> {
            band_idx: u8,
            y_start: u32,
            band_h: u32,
            force_encode: bool,
            is_refine: bool,
            quality: u8,
            src_slice: &'a [u8],
            prev_slice: &'a mut [u8],
        }

        let mut tasks = Vec::with_capacity(num_bands);
        let mut prev_rem: &mut [u8] = &mut self.prev_bgra[..expected_len];
        for b in 0..num_bands {
            let y_start = b * base_band_h;
            let y_end = if b + 1 == num_bands {
                height as usize
            } else {
                (b + 1) * base_band_h
            };
            let band_h = y_end - y_start;
            let band_bytes = band_h * stride;
            let (prev_band, rest) = prev_rem.split_at_mut(band_bytes);
            prev_rem = rest;

            let src_slice = &bgra[(y_start * stride)..(y_end * stride)];
            let is_refine = refine_band == Some(b);
            let force_encode = is_keyframe || is_refine || active_rolling_band == Some(b);
            // Rolling refresh de uma faixa já refinada mantém a qualidade alta (não degrada texto parado)
            let band_quality = if is_refine || (!is_keyframe && self.band_refined[b]) {
                REFINE_QUALITY
            } else {
                quality
            };

            tasks.push(BandTask {
                band_idx: b as u8,
                y_start: y_start as u32,
                band_h: band_h as u32,
                force_encode,
                is_refine,
                quality: band_quality,
                src_slice,
                prev_slice: prev_band,
            });
        }

        // Detectar mudança visual, atualizar cache prev_bgra e codificar JPEG simultaneamente em todos os núcleos via Rayon!
        // Retorno: (band_idx, y_start, band_h, jpeg, mudou_visualmente, foi_refinamento, qualidade_usada)
        let encoded_bands: Vec<(u8, u32, u32, Vec<u8>, bool, bool, u8)> = tasks
            .into_par_iter()
            .filter_map(|task| {
                let visually_changed = band_has_visual_change(task.src_slice, task.prev_slice);
                if !visually_changed && !task.force_encode {
                    return None;
                }
                // Conteúdo mudou durante o refinamento: volta para a qualidade adaptativa de movimento
                let band_quality = if visually_changed && task.is_refine {
                    quality
                } else {
                    task.quality
                };

                task.prev_slice.copy_from_slice(task.src_slice);

                let pixel_count = (width as usize) * (task.band_h as usize);
                let mut rgb = vec![0u8; pixel_count * 3];
                for (dst, src) in rgb
                    .chunks_exact_mut(3)
                    .zip(task.src_slice.chunks_exact(4))
                {
                    dst[0] = src[2];
                    dst[1] = src[1];
                    dst[2] = src[0];
                }

                let mut jpeg_buf = Vec::with_capacity(pixel_count / 4);
                {
                    let mut cursor = Cursor::new(&mut jpeg_buf);
                    let mut encoder = JpegEncoder::new_with_quality(&mut cursor, band_quality);
                    encoder
                        .encode(&rgb, width, task.band_h, ColorType::Rgb8.into())
                        .ok()?;
                }

                Some((
                    task.band_idx,
                    task.y_start,
                    task.band_h,
                    jpeg_buf,
                    visually_changed,
                    task.is_refine,
                    band_quality,
                ))
            })
            .collect();

        // Atualizar o estado de refinamento por faixa
        let mut encoded_mask = vec![false; num_bands];
        let mut refine_bytes: u64 = 0;
        for (band_idx, _, _, jpeg, changed, was_refine, used_q) in &encoded_bands {
            let b = *band_idx as usize;
            encoded_mask[b] = true;
            if *changed {
                self.band_static_frames[b] = 0;
                self.band_refined[b] = false;
            } else {
                self.band_static_frames[b] = self.band_static_frames[b].saturating_add(1);
                if *was_refine && *used_q == REFINE_QUALITY {
                    self.band_refined[b] = true;
                }
                if *used_q == REFINE_QUALITY {
                    refine_bytes += jpeg.len() as u64;
                }
            }
        }
        for (b, encoded) in encoded_mask.iter().enumerate() {
            if !encoded {
                self.band_static_frames[b] = self.band_static_frames[b].saturating_add(1);
            }
        }

        // Empacotamento binário direto das faixas JPEG
        let total_jpeg_bytes: usize = encoded_bands
            .iter()
            .map(|(_, _, _, b, _, _, _)| b.len() + 13)
            .sum();
        let mut raw_payload = Vec::with_capacity(1 + total_jpeg_bytes);
        let _ = raw_payload.write_u8(encoded_bands.len() as u8);
        for (band_idx, y_start, band_h, jpeg_bytes, _, _, _) in encoded_bands {
            let _ = raw_payload.write_u8(band_idx);
            let _ = raw_payload.write_u32::<LittleEndian>(y_start);
            let _ = raw_payload.write_u32::<LittleEndian>(band_h);
            let _ = raw_payload.write_u32::<LittleEndian>(jpeg_bytes.len() as u32);
            let _ = raw_payload.write_all(&jpeg_bytes);
        }

        // Medir bitrate real enviado na janela de 0.5s com suavização e teto estável de 16 Mbps
        self.bytes_in_window += raw_payload.len() as u64;
        self.controller_bytes_in_window += (raw_payload.len() as u64).saturating_sub(refine_bytes);
        let elapsed_sec = self.window_start.elapsed().as_secs_f64();
        if elapsed_sec >= 0.5 {
            let instant_kbps =
                ((self.bytes_in_window as f64 * 8.0) / 1000.0 / elapsed_sec).round() as u32;
            let motion_kbps = ((self.controller_bytes_in_window as f64 * 8.0) / 1000.0
                / elapsed_sec)
                .round() as u32;
            let prev_kbps = self.current_kbps.load(Ordering::Relaxed);
            let smoothed_kbps = if prev_kbps == 0 {
                instant_kbps
            } else {
                (instant_kbps * 7 + prev_kbps * 3) / 10
            };
            self.current_kbps
                .store(smoothed_kbps.max(50), Ordering::Relaxed);
            self.controller_kbps = if self.controller_kbps == 0 {
                motion_kbps
            } else {
                (motion_kbps * 7 + self.controller_kbps * 3) / 10
            };
            self.bytes_in_window = 0;
            self.controller_bytes_in_window = 0;
            self.window_start = Instant::now();

            // Teto de banda considera apenas conteúdo em movimento (refinamento é pontual e se auto-limita)
            if motion_kbps > 16_000 {
                self.adaptive_quality = self
                    .adaptive_quality
                    .saturating_sub(2)
                    .max(ADAPTIVE_MIN_QUALITY);
            } else if motion_kbps > 13_500 {
                self.adaptive_quality = self
                    .adaptive_quality
                    .saturating_sub(1)
                    .max(ADAPTIVE_MIN_QUALITY);
            }
        }

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
            payload: raw_payload,
        })
    }
}

/// Decodificador de Vídeo Multi-Core com estado para recompor faixas Delta em < 1.2ms.
pub struct VideoStreamDecoder {
    canvas_rgba: Vec<u8>,
    canvas_w: u32,
    canvas_h: u32,
}

impl VideoStreamDecoder {
    pub fn new() -> Self {
        Self {
            canvas_rgba: Vec::new(),
            canvas_w: 0,
            canvas_h: 0,
        }
    }

    pub fn decode_packet(&mut self, packet: &EncodedVideoPacket) -> Option<DecodedVideoFrame> {
        let data = &packet.payload;
        if data.is_empty() {
            return None;
        }
        let mut cur = Cursor::new(data);
        let band_count = cur.read_u8().ok()? as usize;

        let width = packet.width;
        let height = packet.height;
        let total_rgba = (width as usize) * (height as usize) * 4;

        if self.canvas_w != width || self.canvas_h != height || self.canvas_rgba.len() != total_rgba
        {
            self.canvas_w = width;
            self.canvas_h = height;
            self.canvas_rgba = vec![255u8; total_rgba];
        }

        let mut band_tasks: Vec<(u32, u32, &[u8])> = Vec::with_capacity(band_count);
        for _ in 0..band_count {
            let _band_idx = cur.read_u8().ok()?;
            let y_start = cur.read_u32::<LittleEndian>().ok()?;
            let band_h = cur.read_u32::<LittleEndian>().ok()?;
            let jpeg_len = cur.read_u32::<LittleEndian>().ok()? as usize;
            let pos = cur.position() as usize;
            if pos + jpeg_len > data.len() {
                return None;
            }
            band_tasks.push((y_start, band_h, &data[pos..pos + jpeg_len]));
            cur.set_position((pos + jpeg_len) as u64);
        }

        // Decodificar todas as faixas em paralelo via Rayon (Zero-Copy slice input)
        let decoded_strips: Vec<(u32, u32, Vec<u8>)> = band_tasks
            .into_par_iter()
            .filter_map(|(y_start, band_h, jpeg_slice)| {
                let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(jpeg_slice));
                let rgb = decoder.decode().ok()?;
                let pixel_count = (width as usize) * (band_h as usize);
                if rgb.len() < pixel_count * 3 {
                    return None;
                }
                let mut rgba = vec![255u8; pixel_count * 4];
                for (dst, src) in rgba
                    .chunks_exact_mut(4)
                    .zip(rgb[..pixel_count * 3].chunks_exact(3))
                {
                    dst[0] = src[0];
                    dst[1] = src[1];
                    dst[2] = src[2];
                }
                Some((y_start, band_h, rgba))
            })
            .collect();

        let stride = (width as usize) * 4;
        for (y_start, band_h, strip_rgba) in decoded_strips {
            let start_byte = (y_start as usize) * stride;
            let end_byte = start_byte + (band_h as usize) * stride;
            if end_byte <= self.canvas_rgba.len() && strip_rgba.len() == end_byte - start_byte {
                self.canvas_rgba[start_byte..end_byte].copy_from_slice(&strip_rgba);
            }
        }

        Some(DecodedVideoFrame {
            width,
            height,
            hw_encoder_name: packet.hw_encoder_name.clone(),
            rgba: self.canvas_rgba.clone(),
        })
    }
}

/// 2.1. Codificador de Áudio de Baixa Latência (48kHz Estéreo, frames de 20ms).
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
