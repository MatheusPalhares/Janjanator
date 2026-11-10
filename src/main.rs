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

#[cfg(windows)]
#[link(name = "winmm")]
extern "system" {
    fn timeBeginPeriod(uPeriod: u32) -> u32;
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Inicializar o provedor criptográfico Ring para o Rustls (TLS/WSS dos Túneis Cloudflare)
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Limitar o pool global do Rayon a 4 threads dedicadas: codifica/decodifica 1080p em < 1.2ms
    // sem acordar e girar (spin-wait) todas as 16 threads de CPUs Ryzen 7 a cada tick de 60 FPS!
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .thread_name(|i| format!("janjan-simd-{}", i))
        .build_global();

    // Ativar precisão de timer de 1ms no kernel do Windows (evita o sleep padrão de 15.6ms que derruba 60 FPS para 30~40 FPS)
    #[cfg(windows)]
    unsafe {
        let _ = timeBeginPeriod(1);
    }

    // Limpar binário antigo (.exe.old / .exe.new) com retentativas caso o processo anterior ainda esteja encerrando no kernel
    if let Ok(exe_path) = std::env::current_exe() {
        std::thread::spawn(move || {
            let old_path = exe_path.with_extension("exe.old");
            let new_path = exe_path.with_extension("exe.new");
            for _ in 0..8 {
                let _ = std::fs::remove_file(&new_path);
                if !old_path.exists() || std::fs::remove_file(&old_path).is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        });
    }

    // Configurar Per-Monitor DPI Awareness no Windows 11 para captura em resolução real (Nativo/1080p)
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let icon_data = egui::IconData {
        rgba: JANJANATOR_ICON_RGBA.to_vec(),
        width: 64,
        height: 64,
    };

    let window_title = format!("Janjanator {}", types::APP_VERSION);
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title)
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
