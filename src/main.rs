#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture_audio;
mod capture_video;
mod codec;
mod network;
mod types;
mod ui;

use std::sync::Arc;
use eframe::egui;
use ui::ScreenShareApp;

const JANJANATOR_ICON_RGBA: &[u8] = include_bytes!("../img/janjanator_icon_64.rgba");

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

    let icon_data = egui::IconData {
        rgba: JANJANATOR_ICON_RGBA.to_vec(),
        width: 64,
        height: 64,
    };

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Janjanator")
            .with_icon(Arc::new(icon_data))
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([860.0, 540.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Janjanator",
        native_options,
        Box::new(|cc| Ok(Box::new(ScreenShareApp::new(cc)))),
    )
}
