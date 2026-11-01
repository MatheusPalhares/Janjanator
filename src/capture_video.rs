use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;

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
                process_name: "Explorer.exe".to_string(),
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
            format!("Monitor {} - Principal ({})", index, dev_name)
        } else {
            format!("Monitor {} ({})", index, dev_name)
        };

        monitors.push(CaptureTarget {
            id: hmonitor.0 as u64,
            title,
            process_name: "Desktop".to_string(),
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

    // Ignorar janelas utilitárias ocultas/toolwindows e janelas "cloaked" pelo DWM do Windows 11
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

/// Controlador de captura de vídeo em thread dedicada (Windows.Graphics.Capture D3D11 + Fallback GDI resiliente).
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
                run_capture_loop(target, resolution, fps, stop_clone, event_tx);
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
    last_w: u32,
    last_h: u32,
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
            CaptureTargetKind::Window => {
                interop.CreateForWindow(HWND(target.hwnd as *mut _))?
            }
            CaptureTargetKind::Monitor => {
                interop.CreateForMonitor(HMONITOR(target.hmonitor as *mut _))?
            }
        };

        let size = item.Size()?;
        let w = size.Width.max(64) as u32;
        let h = size.Height.max(64) as u32;

        let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_d3d_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
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
            last_w: w,
            last_h: h,
        })
    }

    unsafe fn try_grab_frame(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        let frame = self.frame_pool.TryGetNextFrame().ok()?;
        let content_size = frame.ContentSize().ok()?;
        let surface: IDirect3DSurface = frame.Surface().ok()?;
        let access: IDirect3DDxgiInterfaceAccess = surface.cast().ok()?;
        let texture: ID3D11Texture2D = access.GetInterface().ok()?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);

        if desc.Width == 0 || desc.Height == 0 {
            return None;
        }

        // Se a janela foi redimensionada, recriar o pool para o novo tamanho
        let cw = content_size.Width.max(64) as u32;
        let ch = content_size.Height.max(64) as u32;
        if cw != self.last_w || ch != self.last_h {
            self.last_w = cw;
            self.last_h = ch;
            let dxgi_device: IDXGIDevice = self.d3d_device.cast().ok()?;
            if let Ok(inspectable) = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) {
                if let Ok(winrt_dev) = inspectable
                    .cast::<windows::Graphics::DirectX::Direct3D11::IDirect3DDevice>()
                {
                    let _ = self.frame_pool.Recreate(
                        &winrt_dev,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        content_size,
                    );
                }
            }
        }

        let mut staging_desc = desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        staging_desc.MiscFlags = 0;

        let mut staging_tex: Option<ID3D11Texture2D> = None;
        self.d3d_device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging_tex))
            .ok()?;
        let staging_tex = staging_tex?;

        self.d3d_context.CopyResource(&staging_tex, &texture);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.d3d_context
            .Map(&staging_tex, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .ok()?;

        let width = desc.Width;
        let height = desc.Height;
        let row_pitch = mapped.RowPitch as usize;
        let dst_row_bytes = (width as usize) * 4;
        let mut bgra = vec![0u8; dst_row_bytes * (height as usize)];

        let src_ptr = mapped.pData as *const u8;
        for y in 0..(height as usize) {
            let src_row = std::slice::from_raw_parts(src_ptr.add(y * row_pitch), dst_row_bytes);
            let dst_row = &mut bgra[y * dst_row_bytes..(y + 1) * dst_row_bytes];
            dst_row.copy_from_slice(src_row);
        }

        self.d3d_context.Unmap(&staging_tex, 0);
        Some((width, height, bgra))
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
        let frame_interval = Duration::from_micros(1_000_000 / fps.max(1) as u64);
        let mut wgc_engine = WgcEngine::new(&target).ok();
        let mut was_minimized = false;
        let mut last_frame_cache: Option<(u32, u32, Vec<u8>)> = None;

        while !stop_flag.load(Ordering::Relaxed) {
            let start = Instant::now();

            // 6.1. Verificar estado da janela (fechada ou minimizada)
            if target.kind == CaptureTargetKind::Window {
                let hwnd = HWND(target.hwnd as *mut _);
                if !IsWindow(hwnd).as_bool() {
                    let _ = event_tx.send(CaptureEvent::TargetClosed(format!(
                        "A janela '{}' foi fechada.",
                        target.title
                    )));
                    break;
                }

                if IsIconic(hwnd).as_bool() {
                    if !was_minimized {
                        was_minimized = true;
                        let _ = event_tx.send(CaptureEvent::TargetMinimized);
                    }
                    thread::sleep(Duration::from_millis(100));
                    continue;
                } else if was_minimized {
                    was_minimized = false;
                    let _ = event_tx.send(CaptureEvent::TargetRestored);
                }
            }

            // 1.2. Tentar capturar via Windows.Graphics.Capture (D3D11)
            let mut captured = None;
            if let Some(ref mut engine) = wgc_engine {
                captured = engine.try_grab_frame();
            }

            // Se WGC não tiver um novo frame no pool ou falhar, usar fallback GDI/PrintWindow ou cache
            if captured.is_none() {
                if let Some(gdi_frame) = capture_fallback_gdi(&target) {
                    captured = Some(gdi_frame);
                } else if let Some(ref cached) = last_frame_cache {
                    captured = Some(cached.clone());
                }
            } else {
                last_frame_cache = captured.clone();
            }

            if let Some((src_w, src_h, src_bgra)) = captured {
                let (dst_w, dst_h) = resolution.target_dimensions(src_w, src_h);
                let final_bgra = if dst_w == src_w && dst_h == src_h {
                    src_bgra
                } else {
                    downscale_bgra(&src_bgra, src_w, src_h, dst_w, dst_h)
                };

                if event_tx
                    .try_send(CaptureEvent::FrameCaptured {
                        width: dst_w,
                        height: dst_h,
                        bgra: final_bgra,
                    })
                    .is_err()
                {
                    // Se o canal estiver cheio, descarta o frame antigo para manter zero-latency
                }
            }

            let elapsed = start.elapsed();
            if elapsed < frame_interval {
                thread::sleep(frame_interval - elapsed);
            }
        }
    }
}

/// Fallback resiliente via PrintWindow / BitBlt para janelas ou monitores caso WGC não entregue frame estático.
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

            let ok = PrintWindow(hwnd, hdc_mem, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool();
            if !ok {
                let _ = BitBlt(
                    hdc_mem,
                    0,
                    0,
                    width as i32,
                    height as i32,
                    hdc_window,
                    0,
                    0,
                    SRCCOPY,
                );
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
    bmi.bmiHeader.biHeight = -(height as i32); // Top-down DIB
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

/// 1.3. Redimensionamento rápido de textura BGRA para 720p / 1080p / Nativo.
pub fn downscale_bgra(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Vec<u8> {
    let mut dst = vec![0u8; (dst_w as usize) * (dst_h as usize) * 4];
    let x_ratio = ((src_w as usize) << 16) / (dst_w as usize).max(1);
    let y_ratio = ((src_h as usize) << 16) / (dst_h as usize).max(1);

    let src_stride = (src_w as usize) * 4;
    let dst_stride = (dst_w as usize) * 4;

    for y in 0..(dst_h as usize) {
        let sy = ((y * y_ratio) >> 16).min((src_h as usize).saturating_sub(1));
        let src_row = &src[sy * src_stride..(sy + 1) * src_stride];
        let dst_row = &mut dst[y * dst_stride..(y + 1) * dst_stride];

        let mut sx_fp = 0usize;
        for x in 0..(dst_w as usize) {
            let sx = (sx_fp >> 16) * 4;
            let dx = x * 4;
            dst_row[dx..dx + 4].copy_from_slice(&src_row[sx..sx + 4]);
            sx_fp += x_ratio;
        }
    }
    dst
}

