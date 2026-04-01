#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::collections::VecDeque;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::sync::Arc;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use std::time::Duration;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use parking_lot::Mutex;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows::Win32::Foundation::CloseHandle;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVE_FORMAT_PCM,
};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemAlloc, CoUninitialize,
    StructuredStorage::{
        PropVariantClear, PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
    },
    BLOB, COINIT_MULTITHREADED,
};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows::Win32::System::Threading::GetCurrentProcessId;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows::Win32::System::Variant::VT_BLOB;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
use windows_core::{implement, Interface, HRESULT};

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
const SAMPLE_RATE: u32 = 48_000;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
const SAMPLES_PER_10MS: usize = 480;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
const PROCESS_LOOPBACK_CAPTURE_SAMPLE_RATE: u32 = SAMPLE_RATE;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
const PROCESS_LOOPBACK_CAPTURE_CHANNELS: u16 = 2;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
const PROCESS_LOOPBACK_CAPTURE_BITS_PER_SAMPLE: u16 = 16;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[derive(Clone, Copy, Debug)]
pub enum LoopbackTarget {
    ExcludeCurrentProcessTree,
    IncludeProcessTree(u32),
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
impl LoopbackTarget {
    fn activation_params(self) -> AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        match self {
            Self::ExcludeCurrentProcessTree => AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: unsafe { GetCurrentProcessId() },
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
            Self::IncludeProcessTree(process_id) => AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: process_id,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        }
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn process_entry_exe_name(entry: &PROCESSENTRY32W) -> String {
    let len = entry
        .szExeFile
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..len]).to_ascii_lowercase()
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub fn sibling_current_image_process_ids() -> Vec<u32> {
    let current_pid = unsafe { GetCurrentProcessId() };
    let Some(current_exe_name) = std::env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
    }) else {
        return Vec::new();
    };

    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("[voice][screen][audio] sibling Astrix enumeration failed: {err:?}");
            return Vec::new();
        }
    };

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut process_ids = Vec::new();
    let mut next = unsafe { Process32FirstW(snapshot, &mut entry) };
    while next.is_ok() {
        if entry.th32ProcessID != 0
            && entry.th32ProcessID != current_pid
            && process_entry_exe_name(&entry) == current_exe_name
        {
            process_ids.push(entry.th32ProcessID);
        }
        next = unsafe { Process32NextW(snapshot, &mut entry) };
    }

    unsafe {
        let _ = CloseHandle(snapshot);
    }
    process_ids.sort_unstable();
    process_ids.dedup();
    process_ids
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[derive(Clone, Copy, Debug)]
enum SampleKind {
    I16,
    I32,
    F32,
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[derive(Clone, Copy, Debug)]
struct MixFormat {
    sample_rate: u32,
    channels: usize,
    bits_per_sample: u16,
    valid_bits_per_sample: u16,
    sample_kind: SampleKind,
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
struct ComGuard {
    initialized: bool,
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
impl ComGuard {
    fn new() -> Result<Self, String> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            Ok(Self { initialized: true })
        } else {
            Err(format!("CoInitializeEx failed: {:?}", result))
        }
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    sender: Mutex<Option<std::sync::mpsc::SyncSender<Result<IAudioClient, String>>>>,
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
impl ActivationHandler {
    fn new(
        sender: std::sync::mpsc::SyncSender<Result<IAudioClient, String>>,
    ) -> IActivateAudioInterfaceCompletionHandler {
        Self {
            sender: Mutex::new(Some(sender)),
        }
        .into()
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        activateoperation: windows_core::Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows_core::Result<()> {
        let mut status = HRESULT(0);
        let mut interface: Option<windows_core::IUnknown> = None;
        let result = {
            unsafe {
                activateoperation
                    .ok()?
                    .GetActivateResult(&mut status, &mut interface)
            }?;
            if status.is_ok() {
                match interface {
                    Some(interface) => interface
                        .cast::<IAudioClient>()
                        .map_err(|e| format!("IAudioClient cast failed: {e:?}")),
                    None => Err("ActivateAudioInterfaceAsync returned no interface".to_string()),
                }
            } else {
                Err(format!(
                    "ActivateAudioInterfaceAsync result failed: 0x{:08X}",
                    status.0 as u32
                ))
            }
        };
        if let Some(sender) = self.sender.lock().take() {
            let _ = sender.send(result);
        }
        Ok(())
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn activate_loopback_audio_client(target: LoopbackTarget) -> Result<IAudioClient, String> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: target.activation_params(),
        },
    };
    let params_size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
    let params_blob = unsafe { CoTaskMemAlloc(params_size) as *mut u8 };
    if params_blob.is_null() {
        return Err(format!(
            "CoTaskMemAlloc failed for AUDIOCLIENT_ACTIVATION_PARAMS ({} bytes)",
            params_size
        ));
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&params as *const AUDIOCLIENT_ACTIVATION_PARAMS).cast::<u8>(),
            params_blob,
            params_size,
        );
    }
    let mut activation_prop = PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: params_size as u32,
                        pBlobData: params_blob,
                    },
                },
            }),
        },
    };
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let handler = ActivationHandler::new(tx);
    let result = (|| {
        let operation = unsafe {
            ActivateAudioInterfaceAsync(
                VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
                &IAudioClient::IID,
                Some(&activation_prop),
                &handler,
            )
            .map_err(|e| format!("ActivateAudioInterfaceAsync failed: {e:?}"))?
        };
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|e| format!("ActivateAudioInterfaceAsync timed out: {e}"))?;
        drop(operation);
        result
    })();
    unsafe {
        let _ = PropVariantClear(&mut activation_prop);
    }
    result
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn process_loopback_mix_format() -> MixFormat {
    MixFormat {
        sample_rate: PROCESS_LOOPBACK_CAPTURE_SAMPLE_RATE,
        channels: PROCESS_LOOPBACK_CAPTURE_CHANNELS as usize,
        bits_per_sample: PROCESS_LOOPBACK_CAPTURE_BITS_PER_SAMPLE,
        valid_bits_per_sample: PROCESS_LOOPBACK_CAPTURE_BITS_PER_SAMPLE,
        sample_kind: SampleKind::I16,
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn process_loopback_wave_format() -> WAVEFORMATEX {
    let block_align =
        PROCESS_LOOPBACK_CAPTURE_CHANNELS * (PROCESS_LOOPBACK_CAPTURE_BITS_PER_SAMPLE / 8);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: PROCESS_LOOPBACK_CAPTURE_CHANNELS,
        nSamplesPerSec: PROCESS_LOOPBACK_CAPTURE_SAMPLE_RATE,
        nAvgBytesPerSec: PROCESS_LOOPBACK_CAPTURE_SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: PROCESS_LOOPBACK_CAPTURE_BITS_PER_SAMPLE,
        cbSize: 0,
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn resample_linear(samples: &[i16], target_len: usize) -> Vec<i16> {
    if samples.is_empty() {
        return vec![0; target_len];
    }
    if target_len == 0 {
        return Vec::new();
    }
    let last = samples.len().saturating_sub(1);
    (0..target_len)
        .map(|i| {
            let src_f = (i as f64) * (last as f64) / (target_len.saturating_sub(1).max(1) as f64);
            let idx = src_f as usize;
            let frac = src_f - idx as f64;
            let a = samples.get(idx).copied().unwrap_or(0) as f64;
            let b = samples.get(idx + 1).copied().unwrap_or(0) as f64;
            (a + (b - a) * frac).clamp(-32768.0, 32767.0) as i16
        })
        .collect()
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn push_resampled_chunk_into_ring(output_ring: &Arc<Mutex<VecDeque<i16>>>, resampled: Vec<i16>) {
    let mut ring = output_ring.lock();
    let max_len = SAMPLES_PER_10MS * 24;
    let ring_len = ring.len();
    if ring_len + resampled.len() > max_len {
        let to_drop = (ring_len + resampled.len()) - max_len;
        ring.drain(..to_drop.min(ring_len));
    }
    ring.extend(resampled);
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn flush_source_ring_to_output(
    source_ring: &mut VecDeque<i16>,
    source_10ms_len: usize,
    output_ring: &Arc<Mutex<VecDeque<i16>>>,
) {
    while source_ring.len() >= source_10ms_len {
        let chunk: Vec<i16> = source_ring.drain(..source_10ms_len).collect();
        let resampled = if source_10ms_len == SAMPLES_PER_10MS {
            chunk
        } else {
            resample_linear(&chunk, SAMPLES_PER_10MS)
        };
        push_resampled_chunk_into_ring(output_ring, resampled);
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn build_default_output_loopback_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    output_ring: Arc<Mutex<VecDeque<i16>>>,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::Sample + Send + 'static,
    f32: cpal::FromSample<T>,
{
    use cpal::traits::DeviceTrait;

    let channels = config.channels.max(1) as usize;
    let source_10ms_len = (config.sample_rate.0 / 100).max(1) as usize;
    let max_source_len = source_10ms_len * 48;
    let ring_for_stream = Arc::clone(&output_ring);
    let err_fn = |e| eprintln!("[voice][screen][audio] output loopback stream error: {e}");
    let mut source_ring = VecDeque::<i16>::with_capacity(source_10ms_len * 32);

    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks(channels) {
                    let sum: f32 = frame.iter().map(|sample| sample.to_sample::<f32>()).sum();
                    let mono = (sum / frame.len().max(1) as f32).clamp(-1.0, 1.0);
                    source_ring.push_back((mono * 32767.0) as i16);
                }
                while source_ring.len() > max_source_len {
                    source_ring.pop_front();
                }
                flush_source_ring_to_output(&mut source_ring, source_10ms_len, &ring_for_stream);
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("build_input_stream(loopback fallback) failed: {e}"))
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn capture_default_output_to_ring(
    stop: Arc<AtomicBool>,
    output_ring: Arc<Mutex<VecDeque<i16>>>,
) -> Result<(), String> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device for loopback fallback".to_string())?;
    let device_name = device
        .name()
        .unwrap_or_else(|_| "default output".to_string());
    let supported = device
        .default_output_config()
        .map_err(|e| format!("default_output_config(loopback fallback) failed: {e}"))?;
    let sample_format = supported.sample_format();
    let config = supported.config();

    eprintln!(
        "[voice][screen][audio] fallback: default output loopback on '{}' ({} Hz, {} ch, {})",
        device_name, config.sample_rate.0, config.channels, sample_format
    );

    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            build_default_output_loopback_stream::<f32>(&device, &config, output_ring)?
        }
        cpal::SampleFormat::I16 => {
            build_default_output_loopback_stream::<i16>(&device, &config, output_ring)?
        }
        cpal::SampleFormat::U16 => {
            build_default_output_loopback_stream::<u16>(&device, &config, output_ring)?
        }
        cpal::SampleFormat::I32 => {
            build_default_output_loopback_stream::<i32>(&device, &config, output_ring)?
        }
        cpal::SampleFormat::U32 => {
            build_default_output_loopback_stream::<u32>(&device, &config, output_ring)?
        }
        cpal::SampleFormat::F64 => {
            build_default_output_loopback_stream::<f64>(&device, &config, output_ring)?
        }
        other => {
            return Err(format!(
                "unsupported output sample format for loopback fallback: {other}"
            ));
        }
    };

    stream
        .play()
        .map_err(|e| format!("play(loopback fallback) failed: {e}"))?;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
fn convert_buffer_to_mono(
    format: MixFormat,
    data: *const u8,
    frames: usize,
    silent: bool,
) -> Vec<i16> {
    if silent || data.is_null() || frames == 0 {
        return vec![0; frames];
    }
    let channels = format.channels.max(1);
    match format.sample_kind {
        SampleKind::I16 => {
            let samples = unsafe {
                std::slice::from_raw_parts(data as *const i16, frames.saturating_mul(channels))
            };
            let mut out = Vec::with_capacity(frames);
            for frame in samples.chunks(channels) {
                let sum: i32 = frame.iter().map(|&sample| sample as i32).sum();
                out.push((sum / channels as i32).clamp(-32768, 32767) as i16);
            }
            out
        }
        SampleKind::I32 => {
            let samples = unsafe {
                std::slice::from_raw_parts(data as *const i32, frames.saturating_mul(channels))
            };
            let shift = format.valid_bits_per_sample.saturating_sub(16).min(16) as u32;
            let mut out = Vec::with_capacity(frames);
            for frame in samples.chunks(channels) {
                let sum: i64 = frame
                    .iter()
                    .map(|&sample| ((sample >> shift) as i64).clamp(-32768, 32767))
                    .sum();
                out.push((sum / channels as i64).clamp(-32768, 32767) as i16);
            }
            out
        }
        SampleKind::F32 => {
            let samples = unsafe {
                std::slice::from_raw_parts(data as *const f32, frames.saturating_mul(channels))
            };
            let mut out = Vec::with_capacity(frames);
            for frame in samples.chunks(channels) {
                let sum: f32 = frame.iter().copied().sum();
                let mono = (sum / channels as f32).clamp(-1.0, 1.0);
                out.push((mono * 32767.0) as i16);
            }
            out
        }
    }
}

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub fn capture_loopback_to_ring(
    target: LoopbackTarget,
    stop: Arc<AtomicBool>,
    output_ring: Arc<Mutex<VecDeque<i16>>>,
) -> Result<(), String> {
    let _com = ComGuard::new()?;
    let audio_client = match activate_loopback_audio_client(target) {
        Ok(client) => {
            eprintln!(
                "[voice][screen][audio] process loopback activated for {:?}",
                target
            );
            client
        }
        Err(err) => {
            eprintln!(
                "[voice][screen][audio] process loopback unavailable for {:?}: {}",
                target, err
            );
            match target {
                LoopbackTarget::ExcludeCurrentProcessTree => {
                    eprintln!(
                        "[voice][screen][audio] process exclusion fallback: capturing default output without Astrix exclusion"
                    );
                    return capture_default_output_to_ring(stop, output_ring);
                }
                LoopbackTarget::IncludeProcessTree(process_id) => {
                    return Err(format!(
                        "process loopback unavailable for process {}: {}",
                        process_id, err
                    ));
                }
            }
        }
    };
    let mut wave_format = process_loopback_wave_format();
    let mix_format = process_loopback_mix_format();
    eprintln!(
        "[voice][screen][audio] process loopback format: {} Hz, {} ch, {}-bit PCM",
        mix_format.sample_rate, mix_format.channels, mix_format.bits_per_sample
    );
    unsafe {
        audio_client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                0,
                0,
                &mut wave_format,
                None,
            )
            .map_err(|e| format!("IAudioClient::Initialize failed: {e:?}"))?;
    }
    let capture_client = unsafe {
        audio_client
            .GetService::<IAudioCaptureClient>()
            .map_err(|e| format!("IAudioClient::GetService<IAudioCaptureClient> failed: {e:?}"))?
    };
    unsafe {
        audio_client
            .Start()
            .map_err(|e| format!("IAudioClient::Start failed: {e:?}"))?;
    }
    let source_10ms_len = (mix_format.sample_rate / 100).max(1) as usize;
    let mut source_ring = VecDeque::<i16>::with_capacity(source_10ms_len * 32);
    while !stop.load(Ordering::Relaxed) {
        let mut had_packet = false;
        loop {
            let frames_available = unsafe {
                capture_client
                    .GetNextPacketSize()
                    .map_err(|e| format!("IAudioCaptureClient::GetNextPacketSize failed: {e:?}"))?
            };
            if frames_available == 0 {
                break;
            }
            had_packet = true;
            let mut buffer = std::ptr::null_mut();
            let mut packet_frames = 0u32;
            let mut flags = 0u32;
            unsafe {
                capture_client
                    .GetBuffer(&mut buffer, &mut packet_frames, &mut flags, None, None)
                    .map_err(|e| format!("IAudioCaptureClient::GetBuffer failed: {e:?}"))?;
            }
            let mono = convert_buffer_to_mono(
                mix_format,
                buffer,
                packet_frames as usize,
                flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0,
            );
            unsafe {
                capture_client
                    .ReleaseBuffer(packet_frames)
                    .map_err(|e| format!("IAudioCaptureClient::ReleaseBuffer failed: {e:?}"))?;
            }
            source_ring.extend(mono);
            flush_source_ring_to_output(&mut source_ring, source_10ms_len, &output_ring);
        }
        if !had_packet {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    unsafe {
        let _ = audio_client.Stop();
    }
    Ok(())
}
