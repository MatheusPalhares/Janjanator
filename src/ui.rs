use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use egui::{
    Align, Color32, ColorImage, Context, FontId, Frame, Key, Layout, Margin, RichText, Rounding,
    ScrollArea, Sense, Stroke, TextureHandle, TextureOptions, Vec2, ViewportCommand,
};

use crate::capture_audio::{AudioPlaybackEngine, ProcessAudioCaptureSession};
use crate::capture_video::{enumerate_monitors, enumerate_windows, VideoCaptureSession};
use crate::codec::{detect_hardware_video_encoder, LowLatencyAudioEncoder, LowLatencyVideoEncoder};
use crate::network::{NetworkEvent, RoomNetworkClient};
use crate::types::{
    BitratePreset, CaptureEvent, CaptureTarget, CaptureTargetKind, DecodedVideoFrame, FpsPreset,
    ParticipantInfo, ResolutionPreset,
};

pub struct ScreenShareApp {
    // Estado de Conexão (5.1)
    server_ip: String,
    nickname: String,
    my_identity: String,
    is_connected: bool,
    status_message: String,
    jwt_preview: String,
    livekit_sfu_online: bool,
    latency_ms: u64,
    hw_encoder_label: String,

    // Cliente de Rede e Sala (4.1 - 4.3)
    net_client: Option<RoomNetworkClient>,
    net_event_rx: Option<Receiver<NetworkEvent>>,
    participants: Vec<ParticipantInfo>,

    // Modal de Compartilhar Tela (5.2)
    show_share_modal: bool,
    modal_tab: CaptureTargetKind,
    available_monitors: Vec<CaptureTarget>,
    available_windows: Vec<CaptureTarget>,
    selected_target: Option<CaptureTarget>,
    share_window_audio: bool,
    selected_resolution: ResolutionPreset,
    selected_fps: FpsPreset,
    selected_bitrate: BitratePreset,

    // Estado de Transmissão Ativa (Host)
    is_publishing: bool,
    active_target_title: String,
    active_audio_pid: u32,
    target_minimized_warning: bool,
    video_capture: Option<VideoCaptureSession>,
    audio_capture: Option<ProcessAudioCaptureSession>,
    publish_stop_flag: Option<Arc<AtomicBool>>,
    force_keyframe_flag: Arc<AtomicBool>,
    local_preview_rx: Option<Receiver<DecodedVideoFrame>>,
    show_self_preview: bool,

    // Estado do Player de Visualização (Viewer - 5.3)
    watching_identity: Option<String>,
    watching_Local_volume_pct: u32,
    auto_pause_when_minimized: bool,
    was_paused_by_minimize: Option<String>,
    is_fullscreen: bool,
    video_texture: Option<TextureHandle>,
    last_frame_resolution: (u32, u32),
    last_frame_encoder: String,
    fps_counter_frames: u32,
    fps_counter_instant: Instant,
    measured_fps: u32,

    // Reprodutor de Áudio WASAPI do Viewer
    audio_player: Option<AudioPlaybackEngine>,
    audio_pcm_tx: Option<Sender<Vec<i16>>>,
}

impl ScreenShareApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_dark_theme(&cc.egui_ctx);

        let random_suffix: u16 = rand::random::<u16>() % 9000 + 1000;
        let default_nick = std::env::var("USERNAME")
            .unwrap_or_else(|_| format!("User{}", random_suffix));
        let my_identity = format!("{}-{}", default_nick, random_suffix);

        let monitors = enumerate_monitors();
        let windows = enumerate_windows();
        let initial_target = monitors.first().cloned();

        Self {
            server_ip: "127.0.0.1".to_string(),
            nickname: default_nick,
            my_identity,
            is_connected: false,
            status_message: "Pronto para conectar ao servidor LiveKit SFU.".to_string(),
            jwt_preview: String::new(),
            livekit_sfu_online: false,
            latency_ms: 0,
            hw_encoder_label: detect_hardware_video_encoder(),

            net_client: None,
            net_event_rx: None,
            participants: Vec::new(),

            show_share_modal: false,
            modal_tab: CaptureTargetKind::Window,
            available_monitors: monitors,
            available_windows: windows,
            selected_target: initial_target,
            share_window_audio: true,
            selected_resolution: ResolutionPreset::Res1080p,
            selected_fps: FpsPreset::Fps60,
            selected_bitrate: BitratePreset::High12Mbps,

            is_publishing: false,
            active_target_title: String::new(),
            active_audio_pid: 0,
            target_minimized_warning: false,
            video_capture: None,
            audio_capture: None,
            publish_stop_flag: None,
            force_keyframe_flag: Arc::new(AtomicBool::new(true)),
            local_preview_rx: None,
            show_self_preview: true,

            watching_identity: None,
            watching_Local_volume_pct: 100,
            auto_pause_when_minimized: true,
            was_paused_by_minimize: None,
            is_fullscreen: false,
            video_texture: None,
            last_frame_resolution: (0, 0),
            last_frame_encoder: String::new(),
            fps_counter_frames: 0,
            fps_counter_instant: Instant::now(),
            measured_fps: 0,

            audio_player: None,
            audio_pcm_tx: None,
        }
    }

    fn connect_to_server(&mut self) {
        if self.nickname.trim().is_empty() {
            self.status_message = "Informe um Nickname válido.".to_string();
            return;
        }

        let (net_tx, net_rx) = crossbeam_channel::bounded::<NetworkEvent>(256);
        let local_info = self.build_local_participant_info();
        let client = RoomNetworkClient::connect(&self.server_ip, local_info, net_tx);

        // Iniciar engine WASAPI de reprodução de áudio para quando assistirmos alguém
        let (pcm_tx, pcm_rx) = crossbeam_channel::bounded::<Vec<i16>>(128);
        let player = AudioPlaybackEngine::start(pcm_rx, self.watching_Local_volume_pct);

        self.net_client = Some(client);
        self.net_event_rx = Some(net_rx);
        self.audio_player = Some(player);
        self.audio_pcm_tx = Some(pcm_tx);
        self.status_message = format!("Conectando a {}...", self.server_ip);
    }

    fn disconnect_from_server(&mut self) {
        self.stop_publishing();
        self.stop_watching();
        if let Some(mut client) = self.net_client.take() {
            client.disconnect();
        }
        if let Some(mut player) = self.audio_player.take() {
            player.stop();
        }
        self.audio_pcm_tx = None;
        self.net_event_rx = None;
        self.is_connected = false;
        self.participants.clear();
        self.status_message = "Desconectado da sala.".to_string();
    }

    fn build_local_participant_info(&self) -> ParticipantInfo {
        ParticipantInfo {
            identity: self.my_identity.clone(),
            nickname: self.nickname.trim().to_string(),
            is_streaming: self.is_publishing,
            stream_title: self.active_target_title.clone(),
            resolution_label: if self.is_publishing {
                self.selected_resolution.short_label().to_string()
            } else {
                String::new()
            },
            fps: if self.is_publishing {
                self.selected_fps.value()
            } else {
                0
            },
            bitrate_kbps: if self.is_publishing {
                self.selected_bitrate.kbps()
            } else {
                0
            },
            has_isolated_audio: self.is_publishing && self.share_window_audio,
            audio_pid: if self.is_publishing && self.share_window_audio {
                self.active_audio_pid
            } else {
                0
            },
        }
    }

    fn refresh_capture_targets(&mut self) {
        self.available_monitors = enumerate_monitors();
        self.available_windows = enumerate_windows();
        if self.selected_target.is_none() {
            self.selected_target = match self.modal_tab {
                CaptureTargetKind::Monitor => self.available_monitors.first().cloned(),
                CaptureTargetKind::Window => self.available_windows.first().cloned(),
            };
        }
    }

    fn start_publishing(&mut self) {
        let Some(target) = self.selected_target.clone() else {
            self.status_message = "Selecione um monitor ou janela para compartilhar.".to_string();
            return;
        };

        self.stop_publishing();

        let (cap_tx, cap_rx) = crossbeam_channel::bounded::<CaptureEvent>(4);
        let (preview_tx, preview_rx) = crossbeam_channel::bounded::<DecodedVideoFrame>(2);

        let video_session = VideoCaptureSession::start(
            target.clone(),
            self.selected_resolution,
            self.selected_fps.value(),
            cap_tx,
        );

        let audio_pid = if target.kind == CaptureTargetKind::Window {
            target.pid
        } else {
            0
        };

        let (audio_pcm_tx, audio_pcm_rx) = crossbeam_channel::bounded::<(u32, Vec<i16>)>(64);
        let audio_session = if self.share_window_audio {
            Some(ProcessAudioCaptureSession::start(audio_pid, audio_pcm_tx))
        } else {
            None
        };

        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();
        let force_idr = self.force_keyframe_flag.clone();
        force_idr.store(true, Ordering::SeqCst);

        let my_id = self.my_identity.clone();
        let bitrate = self.selected_bitrate;
        let net_client_ref = self.net_client.as_ref().map(|c| c. clone_sender());

        thread::Builder::new()
            .name("media-encoder-publisher".to_string())
            .spawn(move || {
                let mut v_encoder = LowLatencyVideoEncoder::new(my_id.clone(), bitrate, force_idr);
                let mut a_encoder = LowLatencyAudioEncoder::new(my_id.clone());

                while !stop_clone.load(Ordering::Relaxed) {
                    let mut active = false;

                    while let Ok((pid, pcm_samples)) = audio_pcm_rx.try_recv() {
                        active = true;
                        let packet = a_encoder.encode_pcm_frame(pid, &pcm_samples);
                        if let Some(ref tx) = net_client_ref {
                            let _ = tx.try_send(crate::network::OutgoingCommand::PublishAudio(packet));
                        }
                    }

                    while let Ok(ev) = cap_rx.try_recv() {
                        active = true;
                        match ev {
                            CaptureEvent::FrameCaptured {
                                width,
                                height,
                                bgra,
                            } => {
                                // Gerar preview local RGBA
                                let mut rgba = Vec::with_capacity(bgra.len());
                                for chunk in bgra.chunks_exact(4) {
                                    rgba.push(chunk[2]);
                                    rgba.push(chunk[1]);
                                    rgba.push(chunk[0]);
                                    rgba.push(255);
                                }
                                let _ = preview_tx.try_send(DecodedVideoFrame {
                                    sender: my_id.clone(),
                                    sequence: 0,
                                    width,
                                    height,
                                    hw_encoder_name: v_encoder.hw_encoder_name().to_string(),
                                    rgba,
                                });

                                if let Some(pkt) = v_encoder.encode_bgra_frame(width, height, &bgra)
                                {
                                    if let Some(ref tx) = net_client_ref {
                                        let _ = tx.try_send(
                                            crate::network::OutgoingCommand::PublishVideo(pkt),
                                        );
                                    }
                                }
                            }
                            CaptureEvent::TargetMinimized => {}
                            CaptureEvent::TargetRestored => {}
                            CaptureEvent::TargetClosed(_) => {
                                stop_clone.store(true, Ordering::SeqCst);
                                break;
                            }
                        }
                    }

                    if !active {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
            })
            .ok();

        self.video_capture = Some(video_session);
        self.audio_capture = audio_session;
        self.publish_stop_flag = Some(stop_flag);
        self.local_preview_rx = Some(preview_rx);
        self.is_publishing = true;
        self.active_target_title = target.title.clone();
        self.active_audio_pid = audio_pid;
        self.show_share_modal = false;

        let updated = self.build_local_participant_info();
        if let Some(ref client) = self.net_client {
            client.update_participant(updated);
        }

        self.status_message = format!(
            "Transmitindo '{}' em {} @ {} FPS ({})",
            target.title,
            self.selected_resolution.short_label(),
            self.selected_fps.value(),
            self.selected_bitrate.label()
        );
    }

    fn stop_publishing(&mut self) {
        if let Some(flag) = self.publish_stop_flag.take() {
            flag.store(true, Ordering::SeqCst);
        }
        if let Some(mut v) = self.video_capture.take() {
            v.stop();
        }
        if let Some(mut a) = self.audio_capture.take() {
            a.stop();
        }
        self.local_preview_rx = None;
        self.is_publishing = false;
        self.active_target_title.clear();
        self.active_audio_pid = 0;

        if let Some(ref client) = self.net_client {
            client.update_participant(self.build_local_participant_info());
        }
    }

    fn start_watching(&mut self, host_identity: String) {
        self.watching_identity = Some(host_identity.clone());
        if let Some(ref client) = self.net_client {
            client.set_watched_host(Some(host_identity));
        }
    }

    fn stop_watching(&mut self) {
        self.watching_identity = None;
        self.video_texture = None;
        if let Some(ref client) = self.net_client {
            client.set_watched_host(None);
        }
    }

    fn process_events(&mut self, ctx: &Context) {
        // Economizar banda quando minimizado (5.3)
        let is_minimized = ctx.input(|i| i.viewport().minimized.unwrap_or(false));
        if self.auto_pause_when_minimized {
            if is_minimized && self.watching_identity.is_some() {
                let current = self.watching_identity.clone();
                self.stop_watching();
                self.was_paused_by_minimize = current;
                self.status_message =
                    "Stream pausada automaticamente (janela minimizada para economizar banda)."
                        .to_string();
            } else if !is_minimized {
                if let Some(prev_host) = self.was_paused_by_minimize.take() {
                    self.start_watching(prev_host);
                }
            }
        }

        // Atalho ESC para sair da tela cheia
        if self.is_fullscreen && ctx.input(|i| i.key_pressed(Key::Escape)) {
            self.is_fullscreen = false;
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }

        // Eventos da rede
        let mut incoming_events = Vec::new();
        if let Some(ref rx) = self.net_event_rx {
            while let Ok(ev) = rx.try_recv() {
                incoming_events.push(ev);
            }
        }

        for ev in incoming_events {
            match ev {
                NetworkEvent::Connected {
                    server_endpoint,
                    jwt_token,
                    livekit_reachable,
                } => {
                    self.is_connected = true;
                    self.livekit_sfu_online = livekit_reachable;
                    self.jwt_preview = if jwt_token.len() > 36 {
                        format!("{}...{}", &jwt_token[..18], &jwt_token[jwt_token.len() - 12..])
                    } else {
                        jwt_token
                    };
                    self.status_message = format!("Conectado a {}", server_endpoint);
                }
                NetworkEvent::Disconnected(reason) => {
                    self.is_connected = false;
                    self.status_message = reason;
                }
                NetworkEvent::ParticipantsUpdated(list) => {
                    self.participants = list;
                }
                NetworkEvent::KeyframeRequested => {
                    self.force_keyframe_flag.store(true, Ordering::SeqCst);
                }
                NetworkEvent::LatencyUpdatedMs(rtt) => {
                    self.latency_ms = rtt;
                }
                NetworkEvent::RemoteVideoFrame(frame) => {
                    self.upload_video_frame(ctx, frame);
                }
                NetworkEvent::RemoteAudioFrame { samples, .. } => {
                    if let Some(ref pcm_tx) = self.audio_pcm_tx {
                        let _ = pcm_tx.try_send(samples);
                    }
                }
            }
        }

        // Se não estivermos assistindo outra pessoa mas estivermos transmitindo com Self-Preview ativo
        if self.watching_identity.is_none() && self.is_publishing && self.show_self_preview {
            let mut latest_preview = None;
            if let Some(ref rx) = self.local_preview_rx {
                while let Ok(frame) = rx.try_recv() {
                    latest_preview = Some(frame);
                }
            }
            if let Some(frame) = latest_preview {
                self.upload_video_frame(ctx, frame);
            }
        }
    }

    fn upload_video_frame(&mut self, ctx: &Context, frame: DecodedVideoFrame) {
        let size = [frame.width as usize, frame.height as usize];
        if frame.rgba.len() < size[0] * size[1] * 4 {
            return;
        }
        let color_image = ColorImage::from_rgba_unmultiplied(size, &frame.rgba);

        if let Some(ref mut tex) = self.video_texture {
            tex.set(color_image, TextureOptions::LINEAR);
        } else {
            self.video_texture =
                Some(ctx.load_texture("stream-video", color_image, TextureOptions::LINEAR));
        }

        self.last_frame_resolution = (frame.width, frame.height);
        self.last_frame_encoder = frame.hw_encoder_name;
        self.fps_counter_frames += 1;
        let elapsed = self.fps_counter_instant.elapsed();
        if elapsed >= Duration::from_secs(1) {
            self.measured_fps = self.fps_counter_frames;
            self.fps_counter_frames = 0;
            self.fps_counter_instant = Instant::now();
        }
    }
}

impl RoomNetworkClient {
    pub fn clone_sender(&self) -> Sender<crate::network::OutgoingCommand> {
        self.cmd_tx_clone()
    }
}

impl RoomNetworkClient {
    fn cmd_tx_clone(&self) -> Sender<crate::network::OutgoingCommand> {
        // Expor clone do sender interno para a thread de codificação
        self.inner_sender()
    }
}

impl eframe::App for ScreenShareApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.process_events(ctx);

        // Solicitar repaint contínuo quando houver stream ativa para manter 60 FPS fluidos
        if self.is_publishing || self.watching_identity.is_some() {
            ctx.request_repaint_after(Duration::from_millis(8));
        } else {
            ctx.request_repaint_after(Duration::from_millis(100));
        }

        if !self.is_connected {
            self.render_login_view(ctx);
            return;
        }

        if !self.is_fullscreen {
            self.render_top_bar(ctx);
            self.render_participants_sidebar(ctx);
        }

        self.render_player_central_view(ctx);

        if self.show_share_modal {
            self.render_share_modal(ctx);
        }
    }
}

impl ScreenShareApp {
    /// 5.1. Tela de Conexão Minimalista
    fn render_login_view(&mut self, ctx: &Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.16);

                Frame::none()
                    .fill(Color32::from_rgb(22, 25, 34))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(55, 65, 85)))
                    .rounding(Rounding::same(12.0))
                    .inner_margin(Margin::same(28.0))
                    .show(ui, |ui| {
                        ui.set_max_width(440.0);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new("⚡ ScreenShare Win11 Native")
                                    .font(FontId::proportional(24.0))
                                    .strong()
                                    .color(Color32::from_rgb(96, 165, 250)),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(
                                    "Compartilhamento de Tela Ultra-Leve (WGC + WASAPI PID Loopback + LiveKit SFU)",
                                )
                                .size(12.5)
                                .color(Color32::from_rgb(156, 163, 175)),
                            );
                        });

                        ui.add_space(20.0);

                        ui.label(
                            RichText::new("Endereço do Servidor (Playit.gg, ZeroTier ou IP):")
                                .size(13.0)
                                .strong(),
                        );
                        ui.add_space(4.0);
                        ui.add_sized(
                            [400.0, 32.0],
                            egui::TextEdit::singleline(&mut self.server_ip).hint_text(
                                "Ex: algo.gl.at.ply.gg:PORTA, IP ZeroTier ou 127.0.0.1",
                            ),
                        );

                        ui.add_space(12.0);

                        ui.label(
                            RichText::new("Seu Nickname na Sala (Máx. 10 usuários):")
                                .size(13.0)
                                .strong(),
                        );
                        ui.add_space(4.0);
                        let enter_pressed = ui
                            .add_sized(
                                [400.0, 32.0],
                                egui::TextEdit::singleline(&mut self.nickname)
                                    .hint_text("Ex: Teteu"),
                            )
                            .lost_focus()
                            && ui.input(|i| i.key_pressed(Key::Enter));

                        ui.add_space(20.0);

                        let connect_btn = ui.add_sized(
                            [400.0, 40.0],
                            egui::Button::new(
                                RichText::new("🚀 Conectar à Sala")
                                    .size(15.0)
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(37, 99, 235))
                            .rounding(Rounding::same(8.0)),
                        );

                        if connect_btn.clicked() || enter_pressed {
                            self.connect_to_server();
                        }

                        ui.add_space(16.0);
                        ui.separator();
                        ui.add_space(8.0);

                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("GPU Encoder:")
                                    .size(11.5)
                                    .color(Color32::GRAY),
                            );
                            ui.label(
                                RichText::new(&self.hw_encoder_label)
                                    .size(11.5)
                                    .color(Color32::from_rgb(52, 211, 153)),
                            );
                        });

                        ui.add_space(4.0);
                        ui.label(
                            RichText::new(&self.status_message)
                                .size(12.0)
                                .color(Color32::from_rgb(209, 213, 219)),
                        );
                    });
            });
        });
    }

    /// 5.2. Barra Superior de Controles
    fn render_top_bar(&mut self, ctx: &Context) {
        egui::TopBottomPanel::top("top_control_bar")
            .frame(
                Frame::none()
                    .fill(Color32::from_rgb(18, 21, 29))
                    .inner_margin(Margin::symmetric(16.0, 10.0))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(40, 46, 62))),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("⚡ ScreenShare Win11")
                            .size(16.0)
                            .strong()
                            .color(Color32::from_rgb(96, 165, 250)),
                    );

                    ui.separator();

                    let status_color = if self.livekit_sfu_online {
                        Color32::from_rgb(52, 211, 153)
                    } else {
                        Color32::from_rgb(251, 191, 36)
                    };
                    ui.label(
                        RichText::new(format!(
                            "● Sala Ativa ({}/10) | Ping: {} ms",
                            self.participants.len().max(1),
                            self.latency_ms
                        ))
                        .size(12.5)
                        .color(status_color),
                    );

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("Sair").color(Color32::from_rgb(248, 113, 113)),
                                )
                                .fill(Color32::from_rgb(45, 25, 30)),
                            )
                            .clicked()
                        {
                            self.disconnect_from_server();
                        }

                        ui.add_space(8.0);

                        if self.is_publishing {
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("⏹ Parar Compartilhamento")
                                            .strong()
                                            .color(Color32::WHITE),
                                    )
                                    .fill(Color32::from_rgb(220, 38, 38)),
                                )
                                .clicked()
                            {
                                self.stop_publishing();
                            }
                        } else if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("🖥 Compartilhar Tela / Janela")
                                        .strong()
                                        .color(Color32::WHITE),
                                )
                                .fill(Color32::from_rgb(37, 99, 235)),
                            )
                            .clicked()
                        {
                            self.refresh_capture_targets();
                            self.show_share_modal = true;
                        }
                    });
                });
            });
    }

    /// 5.2. Lista lateral de usuários conectados na sala (até 10 usuários)
    fn render_participants_sidebar(&mut self, ctx: &Context) {
        egui::SidePanel::left("participants_panel")
            .resizable(false)
            .exact_width(275.0)
            .frame(
                Frame::none()
                    .fill(Color32::from_rgb(16, 19, 26))
                    .inner_margin(Margin::same(12.0))
                    .stroke(Stroke::new(1.0_f32, Color32::from_rgb(38, 43, 58))),
            )
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "👥 USUÁRIOS NA SALA ({}/10)",
                        self.participants.len().max(1)
                    ))
                    .size(12.0)
                    .strong()
                    .color(Color32::from_rgb(156, 163, 175)),
                );
                ui.add_space(8.0);

                let list_snapshot = if self.participants.is_empty() {
                    vec![self.build_local_participant_info()]
                } else {
                    self.participants.clone()
                };

                ScrollArea::vertical().show(ui, |ui| {
                    for p in list_snapshot {
                        let is_me = p.identity == self.my_identity;
                        let is_watching_this =
                            self.watching_identity.as_deref() == Some(p.identity.as_str());

                        Frame::none()
                            .fill(if is_watching_this {
                                Color32::from_rgb(30, 41, 59)
                            } else {
                                Color32::from_rgb(24, 28, 38)
                            })
                            .rounding(Rounding::same(8.0))
                            .inner_margin(Margin::same(10.0))
                            .stroke(Stroke::new(
                                1.0_f32,
                                if p.is_streaming {
                                    Color32::from_rgb(59, 130, 246)
                                } else {
                                    Color32::from_rgb(42, 48, 64)
                                },
                            ))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(if is_me {
                                            format!("{} (Você)", p.nickname)
                                        } else {
                                            p.nickname.clone()
                                        })
                                        .strong()
                                        .color(Color32::WHITE),
                                    );

                                    if p.is_streaming {
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new("● AO VIVO")
                                                        .size(10.5)
                                                        .strong()
                                                        .color(Color32::from_rgb(248, 113, 113)),
                                                );
                                            },
                                        );
                                    }
                                });

                                if p.is_streaming {
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(format!("📺 {}", p.stream_title))
                                            .size(11.5)
                                            .color(Color32::from_rgb(209, 213, 219)),
                                    );
                                    let audio_tag = if p.has_isolated_audio {
                                        if p.audio_pid > 0 {
                                            format!("🔊 Áudio PID {}", p.audio_pid)
                                        } else {
                                            "🔊 Áudio Ativo".to_string()
                                        }
                                    } else {
                                        "🔇 Sem Áudio".to_string()
                                    };
                                    ui.label(
                                        RichText::new(format!(
                                            "{} • {} FPS • {} Mbps • {}",
                                            p.resolution_label,
                                            p.fps,
                                            p.bitrate_kbps / 1000,
                                            audio_tag
                                        ))
                                        .size(11.0)
                                        .color(Color32::from_rgb(96, 165, 250)),
                                    );

                                    ui.add_space(6.0);
                                    if is_watching_this {
                                        if ui
                                            .add_sized(
                                                [ui.available_width(), 26.0],
                                                egui::Button::new(
                                                    RichText::new("⏹ Parar de Assistir")
                                                        .size(12.0)
                                                        .color(Color32::WHITE),
                                                )
                                                .fill(Color32::from_rgb(185, 28, 28)),
                                            )
                                            .clicked()
                                        {
                                            self.stop_watching();
                                        }
                                    } else if ui
                                        .add_sized(
                                            [ui.available_width(), 26.0],
                                            egui::Button::new(
                                                RichText::new("▶ Assistir Transmissão")
                                                    .size(12.0)
                                                    .strong()
                                                    .color(Color32::WHITE),
                                            )
                                            .fill(Color32::from_rgb(37, 99, 235)),
                                        )
                                        .clicked()
                                    {
                                        self.start_watching(p.identity.clone());
                                    }
                                }
                            });

                        ui.add_space(8.0);
                    }
                });

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    ui.label(
                        RichText::new(&self.status_message)
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                    ui.separator();
                    ui.checkbox(
                        &mut self.auto_pause_when_minimized,
                        RichText::new("Pausar stream ao minimizar").size(11.5),
                    );
                    if self.is_publishing {
                        ui.checkbox(
                            &mut self.show_self_preview,
                            RichText::new("Mostrar prévia da minha tela").size(11.5),
                        );
                    }
                });
            });
    }

    /// 5.3. Player de Visualização Central (Duplo clique para Tela Cheia, Volume Local, Parar de Assistir)
    fn render_player_central_view(&mut self, ctx: &Context) {
        egui::CentralPanel::default()
            .frame(
                Frame::none()
                    .fill(Color32::from_rgb(10, 12, 16))
                    .inner_margin(Margin::same(if self.is_fullscreen { 0.0 } else { 12.0 })),
            )
            .show(ctx, |ui| {
                let has_active_video = self.video_texture.is_some()
                    && (self.watching_identity.is_some()
                        || (self.is_publishing && self.show_self_preview));

                if has_active_video {
                    // Barra de telemetria e controles do Player no topo
                    ui.horizontal(|ui| {
                        let (w, h) = self.last_frame_resolution;
                        ui.label(
                            RichText::new(format!(
                                "🟢 {}x{} @ {} FPS | Codec: {}",
                                w, h, self.measured_fps, self.last_frame_encoder
                            ))
                            .size(12.0)
                            .color(Color32::from_rgb(52, 211, 153)),
                        );

                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .button(if self.is_fullscreen {
                                    "🗗 Sair da Tela Cheia (ESC)"
                                } else {
                                    "⛶ Tela Cheia (Duplo Clique)"
                                })
                                .clicked()
                            {
                                self.is_fullscreen = !self.is_fullscreen;
                                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(
                                    self.is_fullscreen,
                                ));
                            }

                            if self.watching_identity.is_some() {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            RichText::new("⏹ Parar de Assistir")
                                                .color(Color32::WHITE),
                                        )
                                        .fill(Color32::from_rgb(185, 28, 28)),
                                    )
                                    .clicked()
                                {
                                    self.stop_watching();
                                }

                                ui.add_space(10.0);

                                let mut vol = self.watching_Local_volume_pct;
                                if ui
                                    .add(
                                        egui::Slider::new(&mut vol, 0..=200)
                                            .text("🔊 Volume")
                                            .suffix("%"),
                                    )
                                    .changed()
                                {
                                    self.watching_Local_volume_pct = vol;
                                    if let Some(ref player) = self.audio_player {
                                        player.set_volume_percent(vol);
                                    }
                                }
                            }
                        });
                    });

                    ui.add_space(6.0);

                    if let Some(ref tex) = self.video_texture {
                        let avail = ui.available_size();
                        let tex_size = tex.size_vec2();
                        let aspect = if tex_size.y > 0.0 {
                            tex_size.x / tex_size.y
                        } else {
                            16.0 / 9.0
                        };

                        let mut draw_w = avail.x;
                        let mut draw_h = draw_w / aspect;
                        if draw_h > avail.y {
                            draw_h = avail.y;
                            draw_w = draw_h * aspect;
                        }

                        ui.centered_and_justified(|ui| {
                            let response = ui.add(
                                egui::Image::new((tex.id(), Vec2::new(draw_w, draw_h)))
                                    .sense(Sense::click()),
                            );
                            if response.double_clicked() {
                                self.is_fullscreen = !self.is_fullscreen;
                                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(
                                    self.is_fullscreen,
                                ));
                            }
                        });
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.vertical_centered(|ui| {
                            ui.add_space(ui.available_height() * 0.35);
                            ui.label(
                                RichText::new("📺 Nenhuma transmissão selecionada")
                                    .size(20.0)
                                    .strong()
                                    .color(Color32::from_rgb(148, 163, 184)),
                            );
                            ui.add_space(8.0);
                            ui.label(
                                RichText::new(
                                    "Clique em 'Compartilhar Tela / Janela' no topo para transmitir com áudio isolado por PID,\nou clique em 'Assistir Transmissão' ao lado de um amigo conectado.",
                                )
                                .size(13.5)
                                .color(Color32::GRAY),
                            );
                        });
                    });
                }
            });
    }

    /// 5.2. Modal de "Compartilhar Tela" (Monitores / Janelas, Áudio por PID, 720p/1080p/Nativo, 30/60 FPS, Bitrate)
    fn render_share_modal(&mut self, ctx: &Context) {
        let mut open = self.show_share_modal;
        egui::Window::new("🖥 Configurar Compartilhamento de Tela e Áudio")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([580.0, 470.0])
            .show(ctx, |ui| {
                // Abas Monitores vs Janelas Abertas
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            self.modal_tab == CaptureTargetKind::Window,
                            RichText::new("🪟 Janelas Abertas (Áudio Isolado por PID)")
                                .size(13.5)
                                .strong(),
                        )
                        .clicked()
                    {
                        self.modal_tab = CaptureTargetKind::Window;
                        self.selected_target = self.available_windows.first().cloned();
                    }

                    ui.add_space(8.0);

                    if ui
                        .selectable_label(
                            self.modal_tab == CaptureTargetKind::Monitor,
                            RichText::new("🖥 Monitores Inteiros")
                                .size(13.5)
                                .strong(),
                        )
                        .clicked()
                    {
                        self.modal_tab = CaptureTargetKind::Monitor;
                        self.selected_target = self.available_monitors.first().cloned();
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("🔄 Atualizar Lista").clicked() {
                            self.refresh_capture_targets();
                        }
                    });
                });

                ui.separator();

                // Lista selecionável de Monitores ou Janelas
                Frame::none()
                    .fill(Color32::from_rgb(15, 18, 25))
                    .rounding(Rounding::same(6.0))
                    .inner_margin(Margin::same(8.0))
                    .show(ui, |ui| {
                        ScrollArea::vertical()
                            .max_height(190.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                let targets = match self.modal_tab {
                                    CaptureTargetKind::Monitor => &self.available_monitors,
                                    CaptureTargetKind::Window => &self.available_windows,
                                };

                                for target in targets {
                                    let is_selected = self
                                        .selected_target
                                        .as_ref()
                                        .map(|t| t.id == target.id && t.kind == target.kind)
                                        .unwrap_or(false);

                                    if ui
                                        .selectable_label(is_selected, target.display_label())
                                        .clicked()
                                    {
                                        self.selected_target = Some(target.clone());
                                    }
                                }
                            });
                    });

                ui.add_space(10.0);

                // Checkbox de Áudio Isolado por Janela (WASAPI Process Loopback)
                Frame::none()
                    .fill(Color32::from_rgb(22, 30, 46))
                    .rounding(Rounding::same(6.0))
                    .inner_margin(Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.checkbox(
                            &mut self.share_window_audio,
                            RichText::new(
                                "🔊 Compartilhar áudio desta janela (WASAPI Process Loopback Isolado)",
                            )
                            .strong()
                            .color(Color32::from_rgb(147, 197, 253)),
                        );
                        if let Some(ref t) = self.selected_target {
                            if t.kind == CaptureTargetKind::Window {
                                ui.label(
                                    RichText::new(format!(
                                        "Captura exclusivamente a árvore de áudio do processo '{}' (PID: {}), ignorando Discord e sons do desktop.",
                                        t.process_name, t.pid
                                    ))
                                    .size(11.5)
                                    .color(Color32::from_rgb(156, 163, 175)),
                                );
                            } else {
                                ui.label(
                                    RichText::new(
                                        "Modo Monitor selecionado: capturará o loopback do monitor.",
                                    )
                                    .size(11.5)
                                    .color(Color32::from_rgb(156, 163, 175)),
                                );
                            }
                        }
                    });

                ui.add_space(12.0);

                // Dropdowns de Qualidade: Resolução, Taxa de Quadros e Bitrate
                egui::Grid::new("quality_settings_grid")
                    .num_columns(2)
                    .spacing([16.0, 10.0])
                    .show(ui, |ui| {
                        ui.label(RichText::new("Resolução da Stream:").strong());
                        egui::ComboBox::from_id_salt("combo_resolution")
                            .width(260.0)
                            .selected_text(self.selected_resolution.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.selected_resolution,
                                    ResolutionPreset::Res720p,
                                    ResolutionPreset::Res720p.label(),
                                );
                                ui.selectable_value(
                                    &mut self.selected_resolution,
                                    ResolutionPreset::Res1080p,
                                    ResolutionPreset::Res1080p.label(),
                                );
                                ui.selectable_value(
                                    &mut self.selected_resolution,
                                    ResolutionPreset::Native,
                                    ResolutionPreset::Native.label(),
                                );
                            });
                        ui.end_row();

                        ui.label(RichText::new("Taxa de Quadros (FPS):").strong());
                        egui::ComboBox::from_id_salt("combo_fps")
                            .width(260.0)
                            .selected_text(self.selected_fps.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.selected_fps,
                                    FpsPreset::Fps30,
                                    FpsPreset::Fps30.label(),
                                );
                                ui.selectable_value(
                                    &mut self.selected_fps,
                                    FpsPreset::Fps60,
                                    FpsPreset::Fps60.label(),
                                );
                            });
                        ui.end_row();

                        ui.label(RichText::new("Bitrate Alvo (Codificador):").strong());
                        egui::ComboBox::from_id_salt("combo_bitrate")
                            .width(260.0)
                            .selected_text(self.selected_bitrate.label())
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.selected_bitrate,
                                    BitratePreset::Medium6Mbps,
                                    BitratePreset::Medium6Mbps.label(),
                                );
                                ui.selectable_value(
                                    &mut self.selected_bitrate,
                                    BitratePreset::High12Mbps,
                                    BitratePreset::High12Mbps.label(),
                                );
                                ui.selectable_value(
                                    &mut self.selected_bitrate,
                                    BitratePreset::Ultra25Mbps,
                                    BitratePreset::Ultra25Mbps.label(),
                                );
                            });
                        ui.end_row();
                    });

                ui.add_space(16.0);
                ui.separator();
                ui.add_space(8.0);

                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [260.0, 36.0],
                            egui::Button::new(
                                RichText::new("🔴 Iniciar Transmissão Agora")
                                    .size(14.0)
                                    .strong()
                                    .color(Color32::WHITE),
                            )
                            .fill(Color32::from_rgb(37, 99, 235)),
                        )
                        .clicked()
                    {
                        self.start_publishing();
                    }

                    if ui
                        .add_sized([120.0, 36.0], egui::Button::new("Cancelar"))
                        .clicked()
                    {
                        self.show_share_modal = false;
                    }
                });
            });

        if !open {
            self.show_share_modal = false;
        }
    }
}

fn configure_dark_theme(ctx: &Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = Color32::from_rgb(18, 21, 29);
    visuals.panel_fill = Color32::from_rgb(13, 16, 23);
    ctx.set_visuals(visuals);
}

