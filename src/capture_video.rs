use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use rayon::prelude::*;

use crate::types::{CaptureEvent, CaptureTarget, CaptureTargetKind, ResolutionPreset};

#[cfg(windows)]
use windows::{
    core::Interface,
    Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem},
    Graphics::DirectX::Direct3D11::IDirect3DSurface,
    Graphics::DirectX::DirectXPixelFormat,
    Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE},
    Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
    Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
        D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    },
    Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
    Win32::Graphics::Dxgi::IDXGIDevice,
    Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR, MONITORINFOEXW,
        SRCCOPY,
    },
    Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
    Win32::System::ProcessStatus::K32GetModuleBaseNameW,
    Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ},
    Win32::System::WinRT::Direct3D11::{
        CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
    },
    Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
    Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED},
    Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClientRect, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
        GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
        GWL_EXSTYLE, PW_RENDERFULLCONTENT, WS_EX_TOOLWINDOW,
    },
};

/// Enumera todos os monitores conectados via Win32 `EnumDisplayMonitors`.
pub fn enumerate_monitors() -> Vec<CaptureTarget> {
    #[cfg(windows)]
    {
        let mut monitors: Vec<CaptureTarget> = Vec::new();
        unsafe {
            let ptr = &mut monitors as *mut Vec<CaptureTarget>;
            let _ = EnumDisplayMonitors(
                HDC::default(),
                None,
                Some(enum_monitors_proc),
                LPARAM(ptr as isize),
            );
        }
        if monitors.is_empty() {
            monitors.push(CaptureTarget {
                id: 1,
                title: "Monitor Principal (Windows 11)".to_string(),
                process_name: "Desktop".to_string(),
                hwnd: 0,
                hmonitor: 1,
                pid: 0,
                width: 1920,
                height: 1080,
                kind: CaptureTargetKind::Monitor,
            });
        }
        monitors
    }
    #[cfg(not(windows))]
    {
        vec![]
    }
}

#[cfg(windows)]
unsafe extern "system" fn enum_monitors_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _lprect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<CaptureTarget>);
    let mut mi = MONITORINFOEXW::default();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;

    if GetMonitorInfoW(hmonitor, &mut mi.monitorInfo).as_bool() {
        let rc = mi.monitorInfo.rcMonitor;
        let width = (rc.right - rc.left).max(1) as u32;
        let height = (rc.bottom - rc.top).max(1) as u32;
        let dev_name = String::from_utf16_lossy(
            &mi.szDevice
                .iter()
                .copied()
                .take_while(|&c| c != 0)
                .collect::<Vec<u16>>(),
        );
        let index = monitors.len() + 1;
        let is_primary = (mi.monitorInfo.dwFlags & 1) != 0;
        let title = if is_primary {
            format!("Monitor {} — Principal ({})", index, dev_name)
        } else {
            format!("Monitor {} ({})", index, dev_name)
        };

        monitors.push(CaptureTarget {
            id: hmonitor.0 as u64,
            title,
            process_name: "Monitor Físico".to_string(),
            hwnd: 0,
            hmonitor: hmonitor.0 as isize,
            pid: 0,
            width,
            height,
            kind: CaptureTargetKind::Monitor,
        });
    }
    TRUE
}

/// Enumera todas as janelas visíveis do Windows 11 via `EnumWindows`, obtendo `HWND`, `PID`, título e dimensões.
pub fn enumerate_windows() -> Vec<CaptureTarget> {
    #[cfg(windows)]
    {
        let mut windows_list: Vec<CaptureTarget> = Vec::new();
        unsafe {
            let ptr = &mut windows_list as *mut Vec<CaptureTarget>;
            let _ = EnumWindows(Some(enum_windows_proc), LPARAM(ptr as isize));
        }
        windows_list
    }
    #[cfg(not(windows))]
    {
        vec![]
    }
}

#[cfg(windows)]
unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }

    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return TRUE;
    }

    let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    if (ex_style & WS_EX_TOOLWINDOW.0) != 0 {
        return TRUE;
    }

    let mut cloaked: u32 = 0;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return TRUE;
    }

    let mut buf = vec![0u16; (length + 1) as usize];
    let read_len = GetWindowTextW(hwnd, &mut buf);
    if read_len <= 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buf[..read_len as usize])
        .trim()
        .to_string();
    if title.is_empty()
        || title == "Program Manager"
        || title == "Windows Input Experience"
        || title == "Settings"
    {
        return TRUE;
    }

    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width < 120 || height < 80 {
        return TRUE;
    }

    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == std::process::id() {
        return TRUE;
    }

    let process_name = query_process_name(pid);

    let list = &mut *(lparam.0 as *mut Vec<CaptureTarget>);
    list.push(CaptureTarget {
        id: hwnd.0 as u64,
        title,
        process_name,
        hwnd: hwnd.0 as isize,
        hmonitor: 0,
        pid,
        width,
        height,
        kind: CaptureTargetKind::Window,
    });

    TRUE
}

#[cfg(windows)]
unsafe fn query_process_name(pid: u32) -> String {
    if let Ok(handle) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) {
        let mut name_buf = [0u16; 260];
        let len = K32GetModuleBaseNameW(handle, None, &mut name_buf);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if len > 0 {
            return String::from_utf16_lossy(&name_buf[..len as usize]);
        }
    }
    format!("PID-{}", pid)
}

/// Controlador de captura de vídeo em thread dedicada (Windows.Graphics.Capture D3D11 Zero-Stall).
pub struct VideoCaptureSession {
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl VideoCaptureSession {
    pub fn start(
        target: CaptureTarget,
        resolution: ResolutionPreset,
        fps: u32,
        event_tx: Sender<CaptureEvent>,
    ) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();

        let worker = thread::Builder::new()
            .name("wgc-video-capture".to_string())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    run_capture_loop(target, resolution, fps, stop_clone, event_tx);
                }));
            })
            .expect("Falha ao criar thread de captura WGC");

        Self {
            stop_flag,
            worker: Some(worker),
        }
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for VideoCaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
struct WgcEngine {
    d3d_device: ID3D11Device,
    d3d_context: ID3D11DeviceContext,
    frame_pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    staging_tex: Option<ID3D11Texture2D>,
    staging_w: u32,
    staging_h: u32,
    pool_w: u32,
    pool_h: u32,
}

#[cfg(windows)]
impl WgcEngine {
    unsafe fn new(target: &CaptureTarget) -> windows::core::Result<Self> {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);

        let mut d3d_device: Option<ID3D11Device> = None;
        let mut d3d_context: Option<ID3D11DeviceContext> = None;
        let feature_levels = [D3D_FEATURE_LEVEL_11_0];

        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut d3d_device),
            None,
            Some(&mut d3d_context),
        )?;

        let d3d_device = d3d_device.unwrap();
        let d3d_context = d3d_context.unwrap();
        let dxgi_device: IDXGIDevice = d3d_device.cast()?;
        let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)?;
        let winrt_d3d_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice =
            inspectable.cast()?;

        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;

        let item: GraphicsCaptureItem = match target.kind {
            CaptureTargetKind::Window => interop.CreateForWindow(HWND(target.hwnd as *mut _))?,
            CaptureTargetKind::Monitor => {
                interop.CreateForMonitor(HMONITOR(target.hmonitor as *mut _))?
            }
        };

        let mut size = item.Size()?;
        size.Width = size.Width.max(64);
        size.Height = size.Height.max(64);
        let w = size.Width as u32;
        let h = size.Height as u32;

        // Buffer de 3 frames para permitir captura contínua a 60+ FPS sem stalls de GPU
        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            3,
            size,
        )?;

        let session = frame_pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(true);
        session.StartCapture()?;

        Ok(Self {
            d3d_device,
            d3d_context,
            frame_pool,
            session,
            staging_tex: None,
            staging_w: 0,
            staging_h: 0,
            pool_w: w,
            pool_h: h,
        })
    }

    /// Drena o pool e pega sempre o frame mais recente da GPU reutilizando a textura Staging D3D11.
    unsafe fn try_grab_latest_frame(&mut self, out_bgra: &mut Vec<u8>) -> Option<(u32, u32)> {
        let mut latest_frame = self.frame_pool.TryGetNextFrame().ok()?;
        // Se houver mais frames acumulados no pool, descartar os antigos e ficar com o mais novo (Zero-Lag)
        while let Ok(newer) = self.frame_pool.TryGetNextFrame() {
            let _ = latest_frame.Close();
            latest_frame = newer;
        }

        let content_size = latest_frame.ContentSize().ok()?;
        let surface: IDirect3DSurface = latest_frame.Surface().ok()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().ok()?;
        let texture: ID3D11Texture2D = access.GetInterface().ok()?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);

        if desc.Width < 16 || desc.Height < 16 {
            let _ = latest_frame.Close();
            return None;
        }

        // Reutilizar textura Staging na GPU (só recria se a resolução da textura mudou)
        if self.staging_tex.is_none()
            || self.staging_w != desc.Width
            || self.staging_h != desc.Height
        {
            let mut staging_desc = desc;
            staging_desc.Usage = D3D11_USAGE_STAGING;
            staging_desc.BindFlags = 0;
            staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            staging_desc.MiscFlags = 0;

            let mut new_tex: Option<ID3D11Texture2D> = None;
            if self
                .d3d_device
                .CreateTexture2D(&staging_desc, None, Some(&mut new_tex))
                .is_err()
            {
                let _ = latest_frame.Close();
                return None;
            }
            self.staging_tex = new_tex;
            self.staging_w = desc.Width;
            self.staging_h = desc.Height;
        }

        let Some(ref staging_tex) = self.staging_tex else {
            let _ = latest_frame.Close();
            return None;
        };

        self.d3d_context.CopyResource(staging_tex, &texture);
        drop(texture);
        drop(access);
        drop(surface);
        let _ = latest_frame.Close();

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if self
            .d3d_context
            .Map(staging_tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .is_err()
        {
            return None;
        }

        let width = desc.Width;
        let height = desc.Height;
        let row_pitch = mapped.RowPitch as usize;
        let dst_row_bytes = (width as usize) * 4;
        let total_bytes = dst_row_bytes * (height as usize);

        if mapped.pData.is_null() || row_pitch < dst_row_bytes {
            self.d3d_context.Unmap(staging_tex, 0);
            return None;
        }

        if out_bgra.len() != total_bytes {
            out_bgra.resize(total_bytes, 0);
        }

        let src_ptr = mapped.pData as *const u8;
        if row_pitch == dst_row_bytes {
            let src_slice = std::slice::from_raw_parts(src_ptr, total_bytes);
            out_bgra.copy_from_slice(src_slice);
        } else {
            for y in 0..(height as usize) {
                let src_row = std::slice::from_raw_parts(src_ptr.add(y * row_pitch), dst_row_bytes);
                let dst_row = &mut out_bgra[y * dst_row_bytes..(y + 1) * dst_row_bytes];
                dst_row.copy_from_slice(src_row);
            }
        }

        self.d3d_context.Unmap(staging_tex, 0);

        // Recriar o frame pool se a janela mudou de tamanho (APÓS copiar e liberar a superfície D3D11 do frame atual!)
        let mut safe_size = content_size;
        safe_size.Width = safe_size.Width.max(64);
        safe_size.Height = safe_size.Height.max(64);
        let cw = safe_size.Width as u32;
        let ch = safe_size.Height as u32;
        if cw != self.pool_w || ch != self.pool_h {
            self.pool_w = cw;
            self.pool_h = ch;
            if let Ok(dxgi_device) = self.d3d_device.cast::<IDXGIDevice>() {
                if let Ok(inspectable) = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) {
                    if let Ok(winrt_dev) = inspectable
                        .cast::<windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>()
                    {
                        let _ = self.frame_pool.Recreate(
                            &winrt_dev,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            3,
                            safe_size,
                        );
                    }
                }
            }
        }

        Some((width, height))
    }
}

#[cfg(windows)]
impl Drop for WgcEngine {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}

fn run_capture_loop(
    target: CaptureTarget,
    resolution: ResolutionPreset,
    fps: u32,
    stop_flag: Arc<AtomicBool>,
    event_tx: Sender<CaptureEvent>,
) {
    #[cfg(windows)]
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let target_fps = fps.clamp(15, 120);
        let frame_interval = Duration::from_nanos(1_000_000_000 / target_fps as u64);
        let mut wgc_engine = WgcEngine::new(&target).ok();
        let mut was_minimized = false;

        let mut raw_bgra_buf: Vec<u8> = Vec::new();
        let mut last_scaled_cache: Option<(u32, u32, Vec<u8>)> = None;
        let mut next_tick = Instant::now();

        while !stop_flag.load(Ordering::Relaxed) {
            // 6.1. Verificar estado da janela (fechada ou minimizada)
            if target.kind == CaptureTargetKind::Window {
                let hwnd = HWND(target.hwnd as *mut _);
                if !IsWindow(hwnd).as_bool() {
                    let _ = event_tx.send(CaptureEvent::TargetClosed);
                    break;
                }

                if IsIconic(hwnd).as_bool() {
                    if !was_minimized {
                        was_minimized = true;
                        let _ = event_tx.send(CaptureEvent::TargetMinimized);
                    }
                    thread::sleep(Duration::from_millis(80));
                    next_tick = Instant::now();
                    continue;
                } else if was_minimized {
                    was_minimized = false;
                    let _ = event_tx.send(CaptureEvent::TargetRestored);
                }
            }

            let mut got_new_gpu_frame = false;
            let mut raw_dims = (0u32, 0u32);

            if let Some(ref mut engine) = wgc_engine {
                // Tentar pegar o frame imediatamente; poll curto de 450us para não roubar CPU do encoder
                let poll_deadline = Instant::now() + Duration::from_micros(450);
                loop {
                    if let Some((w, h)) = engine.try_grab_latest_frame(&mut raw_bgra_buf) {
                        got_new_gpu_frame = true;
                        raw_dims = (w, h);
                        break;
                    }
                    if Instant::now() >= poll_deadline {
                        break;
                    }
                    thread::yield_now();
                }
            } else if let Some((w, h, gdi_data)) = capture_fallback_gdi(&target) {
                raw_bgra_buf = gdi_data;
                got_new_gpu_frame = true;
                raw_dims = (w, h);
            }

            if got_new_gpu_frame {
                let (src_w, src_h) = raw_dims;
                let (dst_w, dst_h) = resolution.target_dimensions(src_w, src_h);
                let final_bgra = if dst_w == src_w && dst_h == src_h {
                    raw_bgra_buf.clone()
                } else {
                    downscale_bgra_parallel(&raw_bgra_buf, src_w, src_h, dst_w, dst_h)
                };

                last_scaled_cache = Some((dst_w, dst_h, final_bgra.clone()));

                let _ = event_tx.try_send(CaptureEvent::FrameCaptured {
                    width: dst_w,
                    height: dst_h,
                    bgra: final_bgra,
                });
            } else if let Some((dst_w, dst_h, ref cached)) = last_scaled_cache {
                // Mantém cadência exata de 30/60 FPS mesmo quando o DWM não redesenhou a janela no tick
                let _ = event_tx.try_send(CaptureEvent::FrameCaptured {
                    width: dst_w,
                    height: dst_h,
                    bgra: cached.clone(),
                });
            }

            next_tick += frame_interval;
            let now = Instant::now();
            if next_tick > now {
                let wait = next_tick - now;
                // Com timeBeginPeriod(1) ativo em main.rs, o sleep do Windows tem precisão de ~1ms.
                // Dormir até faltar 2ms libera 100% dos núcleos para o codificador Rayon manter 60 FPS cravados!
                if wait > Duration::from_millis(3) {
                    thread::sleep(wait - Duration::from_millis(2));
                }
                while Instant::now() + Duration::from_micros(250) < next_tick {
                    thread::yield_now();
                }
                while Instant::now() < next_tick {
                    std::hint::spin_loop();
                }
            } else if now - next_tick > frame_interval {
                next_tick = now;
            }
        }
    }
}

/// Fallback via BitBlt/PrintWindow usado APENAS se WGC não estiver disponível.
#[cfg(windows)]
unsafe fn capture_fallback_gdi(target: &CaptureTarget) -> Option<(u32, u32, Vec<u8>)> {
    match target.kind {
        CaptureTargetKind::Window => {
            let hwnd = HWND(target.hwnd as *mut _);
            let mut rect = RECT::default();
            if GetClientRect(hwnd, &mut rect).is_err() {
                return None;
            }
            let width = (rect.right - rect.left).max(0) as u32;
            let height = (rect.bottom - rect.top).max(0) as u32;
            if width < 32 || height < 32 {
                return None;
            }

            let hdc_window = GetDC(hwnd);
            if hdc_window.is_invalid() {
                return None;
            }
            let hdc_mem = CreateCompatibleDC(hdc_window);
            let hbm = CreateCompatibleBitmap(hdc_window, width as i32, height as i32);
            let old_obj = SelectObject(hdc_mem, hbm);

            // Tentar BitBlt rápido primeiro; se falhar, PrintWindow
            if BitBlt(
                hdc_mem,
                0,
                0,
                width as i32,
                height as i32,
                hdc_window,
                0,
                0,
                SRCCOPY,
            )
            .is_err()
            {
                let _ = PrintWindow(hwnd, hdc_mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT));
            }

            let bgra = extract_dib_bits(hdc_mem, hbm, width, height);

            SelectObject(hdc_mem, old_obj);
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(hwnd, hdc_window);

            bgra.map(|data| (width, height, data))
        }
        CaptureTargetKind::Monitor => {
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let hmon = HMONITOR(target.hmonitor as *mut _);
            let (left, top, width, height) =
                if GetMonitorInfoW(hmon, &mut mi.monitorInfo).as_bool() {
                    let rc = mi.monitorInfo.rcMonitor;
                    (
                        rc.left,
                        rc.top,
                        (rc.right - rc.left).max(64) as u32,
                        (rc.bottom - rc.top).max(64) as u32,
                    )
                } else {
                    (0, 0, target.width.max(64), target.height.max(64))
                };

            let hdc_screen = GetDC(HWND::default());
            if hdc_screen.is_invalid() {
                return None;
            }
            let hdc_mem = CreateCompatibleDC(hdc_screen);
            let hbm = CreateCompatibleBitmap(hdc_screen, width as i32, height as i32);
            let old_obj = SelectObject(hdc_mem, hbm);

            let _ = BitBlt(
                hdc_mem,
                0,
                0,
                width as i32,
                height as i32,
                hdc_screen,
                left,
                top,
                SRCCOPY,
            );

            let bgra = extract_dib_bits(hdc_mem, hbm, width, height);

            SelectObject(hdc_mem, old_obj);
            let _ = DeleteObject(hbm);
            let _ = DeleteDC(hdc_mem);
            ReleaseDC(HWND::default(), hdc_screen);

            bgra.map(|data| (width, height, data))
        }
    }
}

#[cfg(windows)]
unsafe fn extract_dib_bits(
    hdc: HDC,
    hbm: windows::Win32::Graphics::Gdi::HBITMAP,
    width: u32,
    height: u32,
) -> Option<Vec<u8>> {
    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = width as i32;
    bmi.bmiHeader.biHeight = -(height as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;

    let mut buf = vec![0u8; (width as usize) * (height as usize) * 4];
    let lines = GetDIBits(
        hdc,
        hbm,
        0,
        height,
        Some(buf.as_mut_ptr() as *mut _),
        &mut bmi,
        DIB_RGB_COLORS,
    );
    if lines == 0 {
        None
    } else {
        Some(buf)
    }
}

/// 1.3. Redimensionamento paralelo multi-core (`rayon`) com verificação segura de limites (< 1ms para 1080p).
pub fn downscale_bgra_parallel(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let src_w = src_w.max(1) as usize;
    let src_h = src_h.max(1) as usize;
    let dst_w = dst_w.max(1) as usize;
    let dst_h = dst_h.max(1) as usize;
    let src_stride = src_w * 4;
    let dst_stride = dst_w * 4;

    if src.len() < src_stride * src_h {
        return vec![0u8; dst_stride * dst_h];
    }

    let mut dst = vec![0u8; dst_stride * dst_h];
    let x_ratio = (src_w << 16) / dst_w;
    let y_ratio = (src_h << 16) / dst_h;
    let max_sx = (src_w - 1) * 4;

    dst.par_chunks_mut(dst_stride)
        .enumerate()
        .for_each(|(y, dst_row)| {
            let sy = ((y * y_ratio) >> 16).min(src_h - 1);
            let src_row = &src[sy * src_stride..(sy + 1) * src_stride];
            let mut sx_fp = 0usize;
            for x in 0..dst_w {
                let sx = ((sx_fp >> 16) * 4).min(max_sx);
                let dx = x * 4;
                dst_row[dx..dx + 4].copy_from_slice(&src_row[sx..sx + 4]);
                sx_fp += x_ratio;
            }
        });

    dst
}
