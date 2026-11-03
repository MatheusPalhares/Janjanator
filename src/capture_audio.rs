use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

#[cfg(windows)]
use windows::{
    core::{Interface, GUID, HRESULT, PCWSTR, PROPVARIANT},
    Win32::Foundation::{CloseHandle, E_NOINTERFACE, HANDLE, S_OK},
    Win32::Media::Audio::{
        eConsole, eRender, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
        IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Vtbl,
        IAudioCaptureClient, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
        AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS,
        AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
        PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, WAVEFORMATEX,
    },
    Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
    },
    Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    },
    Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject},
};

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
/// 20ms frame at 48kHz stereo = 960 samples per channel = 1920 i16 samples
pub const AUDIO_FRAME_SAMPLES: usize = 960 * 2;

/// Executáveis do Discord (Stable, PTB, Canary e Development) excluídos da captura de tela inteira.
const DISCORD_PROCESS_NAMES: &[&str] = &[
    "discord.exe",
    "discordptb.exe",
    "discordcanary.exe",
    "discorddevelopment.exe",
];

/// Intervalo para verificar se o Discord foi aberto, fechado ou reiniciado durante a transmissão.
const DISCORD_RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// O que deve ser capturado pelo WASAPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCaptureTarget {
    /// Somente o áudio da árvore de processos do PID (compartilhamento de janela).
    ProcessTree(u32),
    /// Todo o áudio do Windows (compartilhamento de tela inteira).
    /// Com `exclude_discord`, a árvore de processos do Discord fica de fora (sem eco da call).
    Desktop { exclude_discord: bool },
}

impl AudioCaptureTarget {
    /// PID anunciado junto com os frames de áudio (0 = áudio do sistema).
    fn source_pid(&self) -> u32 {
        match self {
            AudioCaptureTarget::ProcessTree(pid) => *pid,
            AudioCaptureTarget::Desktop { .. } => 0,
        }
    }
}

/// Estrutura que gerencia a captura de áudio isolada por PID (Windows 11 WASAPI Process Loopback).
pub struct ProcessAudioCaptureSession {
    stop_flag: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl ProcessAudioCaptureSession {
    pub fn start(target: AudioCaptureTarget, pcm_tx: Sender<(u32, Vec<i16>)>) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = stop_flag.clone();

        let worker = thread::Builder::new()
            .name(format!("wasapi-pid-{}-loopback", target.source_pid()))
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    #[cfg(windows)]
                    unsafe {
                        run_process_loopback_capture(target, stop_clone, pcm_tx);
                    }
                }));
            })
            .expect("Falha ao iniciar thread WASAPI Process Loopback");

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

impl Drop for ProcessAudioCaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Implementação COM segura de `IActivateAudioInterfaceCompletionHandler` para `ActivateAudioInterfaceAsync`.
/// O `event_handle` só é fechado quando a última referência COM (`ref_count == 0`) é liberada pelo Windows.
#[cfg(windows)]
#[repr(C)]
struct CompletionHandlerCom {
    vtbl: *const IActivateAudioInterfaceCompletionHandler_Vtbl,
    ref_count: AtomicU32,
    event_handle: HANDLE,
}

#[cfg(windows)]
static COMPLETION_HANDLER_VTBL: IActivateAudioInterfaceCompletionHandler_Vtbl =
    IActivateAudioInterfaceCompletionHandler_Vtbl {
        base__: windows::core::IUnknown_Vtbl {
            QueryInterface: completion_query_interface,
            AddRef: completion_add_ref,
            Release: completion_release,
        },
        ActivateCompleted: completion_activate_completed,
    };

#[cfg(windows)]
unsafe extern "system" fn completion_query_interface(
    this: *mut core::ffi::c_void,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if ppv.is_null() || riid.is_null() {
        return E_NOINTERFACE;
    }
    let iid = &*riid;
    // Suportar IActivateAudioInterfaceCompletionHandler, IUnknown e IAgileObject (exigido pelo WASAPI Async)
    if *iid == IActivateAudioInterfaceCompletionHandler::IID
        || *iid == windows::core::IUnknown::IID
        || *iid == GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90)
    {
        *ppv = this;
        completion_add_ref(this);
        S_OK
    } else {
        *ppv = core::ptr::null_mut();
        E_NOINTERFACE
    }
}

#[cfg(windows)]
unsafe extern "system" fn completion_add_ref(this: *mut core::ffi::c_void) -> u32 {
    let obj = &*(this as *const CompletionHandlerCom);
    obj.ref_count.fetch_add(1, Ordering::Relaxed) + 1
}

#[cfg(windows)]
unsafe extern "system" fn completion_release(this: *mut core::ffi::c_void) -> u32 {
    let obj = &*(this as *const CompletionHandlerCom);
    let prev = obj.ref_count.fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        let boxed = Box::from_raw(this as *mut CompletionHandlerCom);
        if !boxed.event_handle.is_invalid() {
            let _ = CloseHandle(boxed.event_handle);
        }
    }
    prev - 1
}

#[cfg(windows)]
unsafe extern "system" fn completion_activate_completed(
    this: *mut core::ffi::c_void,
    _operation: *mut core::ffi::c_void,
) -> HRESULT {
    let obj = &*(this as *const CompletionHandlerCom);
    if !obj.event_handle.is_invalid() {
        let _ = SetEvent(obj.event_handle);
    }
    S_OK
}

/// Procura o processo raiz do Discord (o `Discord.exe` cujo pai não é outro processo do Discord).
/// Excluir a árvore desse PID remove a voz da call e os sons de notificação, que tocam nos processos filhos.
#[cfg(windows)]
pub fn find_discord_root_pid() -> Option<u32> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        // (pid, pid do pai) de cada processo do Discord
        let mut discord: Vec<(u32, u32)> = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase();
                if DISCORD_PROCESS_NAMES.contains(&name.as_str()) {
                    discord.push((entry.th32ProcessID, entry.th32ParentProcessID));
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);

        discord
            .iter()
            .find(|(_, parent)| !discord.iter().any(|(pid, _)| pid == parent))
            .map(|(pid, _)| *pid)
    }
}

/// 1.4. Ativa `IAudioClient` de Process Loopback para a árvore de processos do `target_pid` no Windows 11.
/// `mode` define se a árvore é a única capturada (INCLUDE) ou se tudo é capturado menos ela (EXCLUDE).
/// Usa `ManuallyDrop<PROPVARIANT>` para evitar que `PropVariantClear` tente dar `CoTaskMemFree`
/// no ponteiro da stack `&mut activation_params` (o que causava `STATUS_HEAP_CORRUPTION 0xC0000374`).
#[cfg(windows)]
unsafe fn activate_process_loopback_client(
    target_pid: u32,
    mode: PROCESS_LOOPBACK_MODE,
) -> Option<IAudioClient> {
    if target_pid == 0 {
        return activate_desktop_loopback_client();
    }

    let event = CreateEventW(None, true, false, None).ok()?;

    let mut activation_params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: windows::Win32::Media::Audio::AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: target_pid,
                ProcessLoopbackMode: mode,
            },
        },
    };

    #[repr(C)]
    struct RawPropVariantBlob {
        vt: u16,
        w_reserved1: u16,
        w_reserved2: u16,
        w_reserved3: u16,
        cb_size: u32,
        _pad: u32,
        p_blob_data: *mut u8,
    }

    let raw_prop = RawPropVariantBlob {
        vt: 65, // VT_BLOB
        w_reserved1: 0,
        w_reserved2: 0,
        w_reserved3: 0,
        cb_size: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
        _pad: 0,
        p_blob_data: &mut activation_params as *mut _ as *mut u8,
    };

    // CRÍTICO: Usar ManuallyDrop para impedir que o Drop do PROPVARIANT chame PropVariantClear/CoTaskMemFree na stack!
    let prop_variant: ManuallyDrop<PROPVARIANT> =
        ManuallyDrop::new(std::mem::transmute_copy(&raw_prop));

    let handler_box = Box::new(CompletionHandlerCom {
        vtbl: &COMPLETION_HANDLER_VTBL,
        ref_count: AtomicU32::new(1),
        event_handle: event,
    });
    let handler_ptr = Box::into_raw(handler_box);
    let handler: IActivateAudioInterfaceCompletionHandler = std::mem::transmute(handler_ptr);

    let device_id_utf16: Vec<u16> = "VAD\\Process_Loopback\0".encode_utf16().collect();

    let op_res: windows::core::Result<IActivateAudioInterfaceAsyncOperation> =
        ActivateAudioInterfaceAsync(
            PCWSTR(device_id_utf16.as_ptr()),
            &IAudioClient::IID,
            Some(&*prop_variant),
            &handler,
        );

    let operation = match op_res {
        Ok(op) => op,
        Err(_) => {
            return activate_desktop_loopback_client();
        }
    };

    WaitForSingleObject(event, 3000);

    let mut activate_hr = HRESULT::default();
    let mut activated_unk: Option<windows::core::IUnknown> = None;
    if operation
        .GetActivateResult(&mut activate_hr, &mut activated_unk)
        .is_ok()
        && activate_hr.is_ok()
    {
        if let Some(unk) = activated_unk {
            if let Ok(client) = unk.cast::<IAudioClient>() {
                return Some(client);
            }
        }
    }

    activate_desktop_loopback_client()
}

#[cfg(windows)]
unsafe fn activate_desktop_loopback_client() -> Option<IAudioClient> {
    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
    let device = enumerator
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .ok()?;
    device.Activate::<IAudioClient>(CLSCTX_ALL, None).ok()
}

#[cfg(windows)]
unsafe fn run_process_loopback_capture(
    target: AudioCaptureTarget,
    stop_flag: Arc<AtomicBool>,
    pcm_tx: Sender<(u32, Vec<i16>)>,
) {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let source_pid = target.source_pid();
    let watch_discord = matches!(
        target,
        AudioCaptureTarget::Desktop {
            exclude_discord: true
        }
    );

    // Recria o cliente de áudio sempre que o Discord abre, fecha ou reinicia (PID raiz muda).
    while !stop_flag.load(Ordering::Relaxed) {
        let excluded_pid = if watch_discord {
            find_discord_root_pid()
        } else {
            None
        };

        let audio_client = match (target, excluded_pid) {
            (AudioCaptureTarget::ProcessTree(pid), _) => activate_process_loopback_client(
                pid,
                PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            ),
            (AudioCaptureTarget::Desktop { .. }, Some(discord_pid)) => {
                activate_process_loopback_client(
                    discord_pid,
                    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                )
            }
            (AudioCaptureTarget::Desktop { .. }, None) => activate_desktop_loopback_client(),
        };
        let Some(audio_client) = audio_client else {
            break;
        };

        let discord_changed = || watch_discord && find_discord_root_pid() != excluded_pid;
        let restart = capture_until_stopped(
            &audio_client,
            &stop_flag,
            &pcm_tx,
            source_pid,
            discord_changed,
        );
        let _ = audio_client.Stop();
        if !restart {
            break;
        }
    }

    CoUninitialize();
}

/// Inicializa e consome o `audio_client` até `stop_flag` ser acionado.
/// Retorna `true` quando `should_restart` pede para recriar o cliente (ex.: Discord reiniciou).
#[cfg(windows)]
unsafe fn capture_until_stopped(
    audio_client: &IAudioClient,
    stop_flag: &AtomicBool,
    pcm_tx: &Sender<(u32, Vec<i16>)>,
    source_pid: u32,
    should_restart: impl Fn() -> bool,
) -> bool {
    let wave_format = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: AUDIO_CHANNELS,
        nSamplesPerSec: AUDIO_SAMPLE_RATE,
        nAvgBytesPerSec: AUDIO_SAMPLE_RATE * (AUDIO_CHANNELS as u32) * 2,
        nBlockAlign: AUDIO_CHANNELS * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };

    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

    if audio_client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            1_000_000,
            0,
            &wave_format,
            None,
        )
        .is_err()
    {
        return false;
    }

    let Ok(capture_client) = audio_client.GetService::<IAudioCaptureClient>() else {
        return false;
    };

    if audio_client.Start().is_err() {
        return false;
    }

    let mut accumulator: Vec<i16> = Vec::with_capacity(AUDIO_FRAME_SAMPLES * 2);
    let mut last_restart_check = Instant::now();

    while !stop_flag.load(Ordering::Relaxed) {
        if last_restart_check.elapsed() >= DISCORD_RESCAN_INTERVAL {
            last_restart_check = Instant::now();
            if should_restart() {
                return true;
            }
        }

        let mut packet_length = capture_client.GetNextPacketSize().unwrap_or(0);
        if packet_length == 0 {
            thread::sleep(Duration::from_millis(2));
            continue;
        }

        while packet_length > 0 && !stop_flag.load(Ordering::Relaxed) {
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames_read: u32 = 0;
            let mut flags: u32 = 0;

            if capture_client
                .GetBuffer(
                    &mut data_ptr,
                    &mut num_frames_read,
                    &mut flags,
                    None,
                    None,
                )
                .is_ok()
            {
                let total_samples = (num_frames_read as usize) * (AUDIO_CHANNELS as usize);
                if (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0 || data_ptr.is_null() {
                    accumulator.extend(std::iter::repeat(0i16).take(total_samples));
                } else {
                    let samples_slice =
                        std::slice::from_raw_parts(data_ptr as *const i16, total_samples);
                    accumulator.extend_from_slice(samples_slice);
                }

                let _ = capture_client.ReleaseBuffer(num_frames_read);

                while accumulator.len() >= AUDIO_FRAME_SAMPLES {
                    let frame: Vec<i16> = accumulator.drain(..AUDIO_FRAME_SAMPLES).collect();
                    let _ = pcm_tx.try_send((source_pid, frame));
                }
            } else {
                break;
            }

            packet_length = capture_client.GetNextPacketSize().unwrap_or(0);
        }
    }

    false
}

/// 4.3. Reprodutor de Áudio WASAPI para o Espectador com controle de volume local em tempo real.
pub struct AudioPlaybackEngine {
    stop_flag: Arc<AtomicBool>,
    volume_pct: Arc<AtomicU32>,
    worker: Option<JoinHandle<()>>,
}

impl AudioPlaybackEngine {
    pub fn start(pcm_rx: Receiver<Vec<i16>>, initial_volume_pct: u32) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let volume_pct = Arc::new(AtomicU32::new(initial_volume_pct));
        let stop_clone = stop_flag.clone();
        let vol_clone = volume_pct.clone();

        let worker = thread::Builder::new()
            .name("wasapi-audio-player".to_string())
            .spawn(move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    #[cfg(windows)]
                    unsafe {
                        run_audio_playback_loop(pcm_rx, stop_clone, vol_clone);
                    }
                }));
            })
            .expect("Falha ao criar thread de reprodução WASAPI");

        Self {
            stop_flag,
            volume_pct,
            worker: Some(worker),
        }
    }

    pub fn set_volume_percent(&self, pct: u32) {
        self.volume_pct.store(pct.min(200), Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioPlaybackEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
unsafe fn run_audio_playback_loop(
    pcm_rx: Receiver<Vec<i16>>,
    stop_flag: Arc<AtomicBool>,
    volume_pct: Arc<AtomicU32>,
) {
    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

    let Ok(enumerator) =
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
    else {
        CoUninitialize();
        return;
    };

    let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) else {
        CoUninitialize();
        return;
    };

    let Ok(audio_client) = device.Activate::<IAudioClient>(CLSCTX_ALL, None) else {
        CoUninitialize();
        return;
    };

    let wave_format = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: AUDIO_CHANNELS,
        nSamplesPerSec: AUDIO_SAMPLE_RATE,
        nAvgBytesPerSec: AUDIO_SAMPLE_RATE * (AUDIO_CHANNELS as u32) * 2,
        nBlockAlign: AUDIO_CHANNELS * 2,
        wBitsPerSample: 16,
        cbSize: 0,
    };

    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    // Buffer compartilhado de 200ms (2_000_000 hns) para absorver bursts de TCP no ZeroTier sem picotar
    if audio_client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            2_000_000,
            0,
            &wave_format,
            None,
        )
        .is_err()
    {
        CoUninitialize();
        return;
    }

    let buffer_frame_count = audio_client.GetBufferSize().unwrap_or(9600);
    let Ok(render_client) = audio_client.GetService::<IAudioRenderClient>() else {
        CoUninitialize();
        return;
    };

    if audio_client.Start().is_err() {
        CoUninitialize();
        return;
    };

    while !stop_flag.load(Ordering::Relaxed) {
        let Ok(samples) = pcm_rx.recv_timeout(Duration::from_millis(25)) else {
            continue;
        };

        // Só descarta 1 pacote se o atraso acumulado passar de ~440ms (22 pacotes de 20ms).
        // NUNCA descartar em rajadas normais de 3~8 pacotes do ZeroTier (isso causava o efeito chirping/picotado!).
        if pcm_rx.len() > 22 {
            let _ = pcm_rx.try_recv();
        }

        let vol = volume_pct.load(Ordering::Relaxed) as f32 / 100.0;
        let ch = AUDIO_CHANNELS as usize;

        // Colchão Anti-Jitter de 40ms (1920 frames estéreo): se o buffer de hardware secou (início da stream
        // ou após silêncio longo), injeta 40ms de colchão silencioso antes do pacote atual para que o ponteiro
        // de leitura da placa de som fique sempre 40~60ms atrás dos pacotes que chegam via ZeroTier.
        let initial_padding = audio_client.GetCurrentPadding().unwrap_or(0);
        if initial_padding == 0 {
            let cushion_frames = 1920u32.min(buffer_frame_count / 3);
            if cushion_frames > 0 {
                if let Ok(buf_ptr) = render_client.GetBuffer(cushion_frames) {
                    let cushion_slice = std::slice::from_raw_parts_mut(
                        buf_ptr as *mut i16,
                        (cushion_frames as usize) * ch,
                    );
                    cushion_slice.fill(0);
                    let _ = render_client.ReleaseBuffer(cushion_frames, 0);
                }
            }
        }

        let total_frames = (samples.len() / ch) as u32;
        let mut written_frames = 0u32;
        let mut retries = 0u8;

        while written_frames < total_frames && retries < 12 && !stop_flag.load(Ordering::Relaxed) {
            let padding = audio_client.GetCurrentPadding().unwrap_or(0);
            let available = buffer_frame_count.saturating_sub(padding);
            let remaining = total_frames - written_frames;
            let count = remaining.min(available);

            if count > 0 {
                if let Ok(buf_ptr) = render_client.GetBuffer(count) {
                    let out_slice =
                        std::slice::from_raw_parts_mut(buf_ptr as *mut i16, (count as usize) * ch);
                    let src_start = (written_frames as usize) * ch;
                    let src_end = src_start + (count as usize) * ch;
                    for (dst, &src) in out_slice.iter_mut().zip(samples[src_start..src_end].iter())
                    {
                        *dst = (src as f32 * vol).clamp(-32768.0, 32767.0) as i16;
                    }
                    let _ = render_client.ReleaseBuffer(count, 0);
                }
                written_frames += count;
            } else {
                retries += 1;
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    let _ = audio_client.Stop();
    CoUninitialize();
}
