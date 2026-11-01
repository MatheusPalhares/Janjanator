#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture_audio;
mod capture_video;
mod codec;
mod network;
mod types;
mod ui;

use eframe::egui;
use ui::ScreenShareApp;

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Configurar Per-Monitor DPI Awareness no Windows 11 para captura em resolução real (Nativo/1080p)
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Iniciar o relay SFU embarcado caso este PC seja usado como Host direto
    network::ensure_embedded_sfu_running();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ScreenShare Win11 Native — WGC + WASAPI PID Loopback + LiveKit SFU")
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([860.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "ScreenShare Win11 Native",
        native_options,
        Box::new(|cc| Ok(Box::new(ScreenShareApp::new(cc)))),
    )
}

