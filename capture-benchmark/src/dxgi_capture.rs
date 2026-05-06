use std::time::{Duration, Instant};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE,
};
use windows::Win32::Graphics::Dxgi::{
    Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC},
    IDXGIDevice, IDXGIKeyedMutex, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    IDXGIResource1, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_SHARED_RESOURCE_READ,
    DXGI_SHARED_RESOURCE_WRITE,
};

use crate::cli::{BenchConfig, ConvertTest, DeviceMode};
use crate::foreground::{ForegroundBucket, ForegroundTracker, ScreenRect};
use crate::stats::{foreground_bucket_fps, BenchSummary, FrameMetric, Percentiles, StatsCollector};

const KEY_CAPTURE: u64 = 0;
const KEY_MEDIA: u64 = 1;

#[derive(Debug, Clone)]
pub struct FrameResult {
    pub acquire_wait_us: u64,
    pub get_resource_us: u64,
    pub copy_submit_us: u64,
    pub acquire_to_release_us: u64,
    pub release_frame_us: u64,
    pub total_capture_stage_us: u64,
    pub copy_ready_delay_us: Option<u64>,
    pub shared_open_us: Option<u64>,
    pub shared_sync_wait_us: Option<u64>,
    pub convert_submit_us: Option<u64>,
    pub convert_ready_delay_us: Option<u64>,
    pub accumulated: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SharedPath {
    Disabled,
    NtHandleKeyedMutex,
    LegacyKeyedMutex,
    Failed,
}

impl SharedPath {
    pub fn as_str(self) -> &'static str {
        match self {
            SharedPath::Disabled => "disabled",
            SharedPath::NtHandleKeyedMutex => "nt_handle_keyed_mutex",
            SharedPath::LegacyKeyedMutex => "legacy_keyed_mutex",
            SharedPath::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SharedSlotState {
    Free,
    CaptureWritten,
    MediaOpened,
    MediaBusy,
    Dropped,
}

#[allow(dead_code)]
pub struct SharedRingSlot {
    pub capture_texture: ID3D11Texture2D,
    pub shared_handle: HANDLE,
    pub shared_handle_needs_close: bool,
    pub media_texture: Option<ID3D11Texture2D>,
    pub keyed_mutex_capture: Option<IDXGIKeyedMutex>,
    pub keyed_mutex_media: Option<IDXGIKeyedMutex>,
    pub state: SharedSlotState,
    pub index: u64,
}

/// Ring buffer for shared textures in separated mode.
///
/// Phase 1 keeps media-side resources optional. The next phase will populate
/// them by opening each shared handle on the media device.
#[allow(dead_code)]
pub struct SharedTextureRing {
    pub slots: Vec<SharedRingSlot>,
    pub shared_path: SharedPath,
    pub shared_create_handle_us: Vec<u64>,
    pub shared_open_us: Vec<u64>,
    pub media_open_failed_count: usize,
    pub current_index: usize,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub format: DXGI_FORMAT,
}

impl SharedTextureRing {
    pub fn new(
        capture_device: &ID3D11Device,
        media_device: &ID3D11Device,
        size: usize,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> anyhow::Result<Self> {
        match Self::new_with_path(
            SharedPath::NtHandleKeyedMutex,
            capture_device,
            media_device,
            size,
            width,
            height,
            format,
        ) {
            Ok(ring) => Ok(ring),
            Err(nt_error) => {
                println!(
                    "[DXGI] NT shared handle path unavailable: {}. Trying legacy shared handle path.",
                    nt_error
                );
                Self::new_with_path(
                    SharedPath::LegacyKeyedMutex,
                    capture_device,
                    media_device,
                    size,
                    width,
                    height,
                    format,
                )
                .map_err(|legacy_error| {
                    anyhow::anyhow!(
                        "ERROR: separated mode requested but cross-device shared texture path is not available.\n\
                         Try --device-mode single, or use an explicit invalid-skip flag if intentionally measuring capture-only timing.\n\
                         NT path error: {}\nLegacy path error: {}",
                        nt_error,
                        legacy_error
                    )
                })
            }
        }
    }

    fn new_with_path(
        shared_path: SharedPath,
        capture_device: &ID3D11Device,
        media_device: &ID3D11Device,
        size: usize,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> anyhow::Result<Self> {
        let mut slots = Vec::with_capacity(size);
        let mut shared_create_handle_us = Vec::with_capacity(size);
        let mut shared_open_us = Vec::with_capacity(size);
        let misc_flags = match shared_path {
            SharedPath::NtHandleKeyedMutex => {
                (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
                    as u32
            }
            SharedPath::LegacyKeyedMutex => D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
            SharedPath::Disabled | SharedPath::Failed => {
                return Err(anyhow::anyhow!(
                    "unsupported shared path: {:?}",
                    shared_path
                ));
            }
        };

        for i in 0..size {
            let (slot, create_handle_us, open_us) = match shared_path {
                SharedPath::NtHandleKeyedMutex => Self::create_nt_slot(
                    capture_device,
                    media_device,
                    width,
                    height,
                    format,
                    misc_flags,
                    i,
                )?,
                SharedPath::LegacyKeyedMutex => Self::create_legacy_slot(
                    capture_device,
                    media_device,
                    width,
                    height,
                    format,
                    misc_flags,
                    i,
                )?,
                SharedPath::Disabled | SharedPath::Failed => unreachable!(),
            };
            slots.push(slot);
            shared_create_handle_us.push(create_handle_us);
            shared_open_us.push(open_us);
        }

        Ok(Self {
            slots,
            shared_path,
            shared_create_handle_us,
            shared_open_us,
            media_open_failed_count: 0,
            current_index: 0,
            size,
            width,
            height,
            format,
        })
    }

    fn create_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        misc_flags: u32,
        slot_index: usize,
    ) -> anyhow::Result<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE(0),
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: misc_flags,
        };

        unsafe {
            let mut tex: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            tex.ok_or_else(|| anyhow::anyhow!("failed to create shared texture {}", slot_index))
        }
    }

    fn create_nt_slot(
        capture_device: &ID3D11Device,
        media_device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        misc_flags: u32,
        slot_index: usize,
    ) -> anyhow::Result<(SharedRingSlot, u64, u64)> {
        unsafe {
            let media_device1: ID3D11Device1 = media_device.cast()?;
            let capture_texture = Self::create_texture(
                capture_device,
                width,
                height,
                format,
                misc_flags,
                slot_index,
            )?;
            let keyed_mutex_capture: IDXGIKeyedMutex = capture_texture.cast()?;
            let dxgi_resource1: IDXGIResource1 = capture_texture.cast()?;
            let shared_access = DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0;

            let create_handle_start = Instant::now();
            let shared_handle =
                dxgi_resource1.CreateSharedHandle(None, shared_access, PCWSTR::null())?;
            let create_handle_us = create_handle_start.elapsed().as_micros() as u64;

            let open_start = Instant::now();
            let media_texture: ID3D11Texture2D =
                media_device1.OpenSharedResource1(shared_handle)?;
            let open_us = open_start.elapsed().as_micros() as u64;
            let keyed_mutex_media: IDXGIKeyedMutex = media_texture.cast()?;

            Ok((
                SharedRingSlot {
                    capture_texture,
                    shared_handle,
                    shared_handle_needs_close: true,
                    media_texture: Some(media_texture),
                    keyed_mutex_capture: Some(keyed_mutex_capture),
                    keyed_mutex_media: Some(keyed_mutex_media),
                    state: SharedSlotState::MediaOpened,
                    index: 0,
                },
                create_handle_us,
                open_us,
            ))
        }
    }

    fn create_legacy_slot(
        capture_device: &ID3D11Device,
        media_device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        misc_flags: u32,
        slot_index: usize,
    ) -> anyhow::Result<(SharedRingSlot, u64, u64)> {
        unsafe {
            let capture_texture = Self::create_texture(
                capture_device,
                width,
                height,
                format,
                misc_flags,
                slot_index,
            )?;
            let keyed_mutex_capture: IDXGIKeyedMutex = capture_texture.cast()?;
            let dxgi_resource: IDXGIResource = capture_texture.cast()?;

            let create_handle_start = Instant::now();
            let shared_handle = dxgi_resource.GetSharedHandle()?;
            let create_handle_us = create_handle_start.elapsed().as_micros() as u64;

            let open_start = Instant::now();
            let mut media_texture: Option<ID3D11Texture2D> = None;
            media_device.OpenSharedResource(shared_handle, &mut media_texture)?;
            let media_texture = media_texture
                .ok_or_else(|| anyhow::anyhow!("legacy OpenSharedResource returned no texture"))?;
            let open_us = open_start.elapsed().as_micros() as u64;
            let keyed_mutex_media: IDXGIKeyedMutex = media_texture.cast()?;

            Ok((
                SharedRingSlot {
                    capture_texture,
                    shared_handle,
                    shared_handle_needs_close: false,
                    media_texture: Some(media_texture),
                    keyed_mutex_capture: Some(keyed_mutex_capture),
                    keyed_mutex_media: Some(keyed_mutex_media),
                    state: SharedSlotState::MediaOpened,
                    index: 0,
                },
                create_handle_us,
                open_us,
            ))
        }
    }

    /// Get next slot for writing (overwrites oldest if full)
    pub fn next_slot(&mut self) -> usize {
        let slot = self.current_index;
        self.current_index = (self.current_index + 1) % self.size;
        slot
    }

    /// Check if a slot is ready (for reading)
    #[allow(dead_code)]
    pub fn is_ready(&self, slot: usize, expected_index: u64) -> bool {
        self.slots[slot].index >= expected_index
    }

    /// Mark slot as filled
    #[allow(dead_code)]
    pub fn mark_filled(&mut self, slot: usize, index: u64) {
        if let Some(slot) = self.slots.get_mut(slot) {
            slot.index = index;
            slot.state = SharedSlotState::CaptureWritten;
        }
    }
}

impl Drop for SharedTextureRing {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if slot.shared_handle_needs_close && !slot.shared_handle.is_invalid() {
                unsafe {
                    let _ = CloseHandle(slot.shared_handle);
                }
                slot.shared_handle = HANDLE::default();
            }
        }
    }
}

pub struct DeviceSetup {
    pub capture_device: ID3D11Device,
    pub capture_context: ID3D11DeviceContext,
    pub media_device: Option<ID3D11Device>,
    pub media_context: Option<ID3D11DeviceContext>,
    pub adapter: windows::Win32::Graphics::Dxgi::IDXGIAdapter,
    pub adapter_luid: i64,
    pub capture_luid: i64,
    pub media_luid: Option<i64>,
}

impl DeviceSetup {
    pub fn new(monitor_index: usize, device_mode: DeviceMode) -> anyhow::Result<Self> {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }

        let (primary_device, primary_context, adapter) = create_d3d11_device(None)?;
        let adapter_desc = unsafe { adapter.GetDesc()? };
        let adapter_luid = ((adapter_desc.AdapterLuid.HighPart as i64) << 32)
            | (adapter_desc.AdapterLuid.LowPart as i64);

        println!("[D3D] Primary device adapter LUID: {}", adapter_luid);

        match device_mode {
            DeviceMode::Single => {
                // Single device for both capture and media
                println!("[D3D] Device mode: SINGLE (capture + media on same device)");
                Ok(Self {
                    capture_device: primary_device,
                    capture_context: primary_context,
                    media_device: None,
                    media_context: None,
                    adapter,
                    adapter_luid,
                    capture_luid: adapter_luid,
                    media_luid: None,
                })
            }
            DeviceMode::Separated => {
                // Create separate capture and media devices
                println!("[D3D] Device mode: SEPARATED (capture + media devices)");

                // Media device - create a new one on the same adapter
                let (media_device, media_context, media_adapter) =
                    create_d3d11_device(Some(&adapter))?;
                let media_desc = unsafe { media_adapter.GetDesc()? };
                let media_luid = ((media_desc.AdapterLuid.HighPart as i64) << 32)
                    | (media_desc.AdapterLuid.LowPart as i64);

                println!("[D3D] Capture device LUID: {}", adapter_luid);
                println!("[D3D] Media device LUID: {}", media_luid);

                if adapter_luid != media_luid {
                    println!("[D3D] WARNING: Capture and media devices are on different adapters!");
                }

                Ok(Self {
                    capture_device: primary_device,
                    capture_context: primary_context,
                    media_device: Some(media_device),
                    media_context: Some(media_context),
                    adapter,
                    adapter_luid,
                    capture_luid: adapter_luid,
                    media_luid: Some(media_luid),
                })
            }
        }
    }

    pub fn is_same_device(&self) -> bool {
        self.media_device.is_none()
    }
}

fn create_d3d11_device(
    adapter: Option<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
) -> anyhow::Result<(
    ID3D11Device,
    ID3D11DeviceContext,
    windows::Win32::Graphics::Dxgi::IDXGIAdapter,
)> {
    unsafe {
        let factory: windows::Win32::Graphics::Dxgi::IDXGIFactory1 =
            windows::Win32::Graphics::Dxgi::CreateDXGIFactory1()?;

        let actual_adapter = if let Some(a) = adapter {
            a.clone()
        } else {
            let adapter1 = factory.EnumAdapters1(0)?;
            adapter1.cast()?
        };

        const FL_11_1: windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL =
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL(0x0000b100);
        const FL_11_0: windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL =
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL(0x0000b000);

        let feature_levels = [FL_11_1, FL_11_0];
        let flags = windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_BGRA_SUPPORT;

        let mut device: Option<ID3D11Device> = None;
        let mut feature_level = windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL(0);
        let mut context: Option<ID3D11DeviceContext> = None;

        windows::Win32::Graphics::Direct3D11::D3D11CreateDevice(
            Some(&actual_adapter),
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN,
            None,
            flags,
            Some(&feature_levels),
            windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut feature_level),
            Some(&mut context),
        )?;

        Ok((
            device.ok_or_else(|| anyhow::anyhow!("Failed to create D3D11 device"))?,
            context.ok_or_else(|| anyhow::anyhow!("Failed to create D3D11 context"))?,
            actual_adapter,
        ))
    }
}

/// Apply GPU thread priority to a D3D11 device
pub fn apply_gpu_thread_priority(
    device: &ID3D11Device,
    role: &str,
    priority: Option<i8>,
) -> Option<i8> {
    unsafe {
        let dxgi_device: Result<IDXGIDevice, _> = device.cast();

        if let Ok(dxgi_dev) = dxgi_device {
            let before = dxgi_dev.GetGPUThreadPriority().unwrap_or(-1);

            match priority {
                Some(p) => {
                    if dxgi_dev.SetGPUThreadPriority(p as i32).is_ok() {
                        let after = dxgi_dev.GetGPUThreadPriority().unwrap_or(-1);
                        println!(
                            "[priority][gpu] role={} requested={} before={} after={} ok",
                            role, p, before, after
                        );
                        Some(after as i8)
                    } else {
                        println!("[priority][gpu] role={} SetGPUThreadPriority failed", role);
                        None
                    }
                }
                None => {
                    println!("[priority][gpu] role={} skipped (priority=off)", role);
                    None
                }
            }
        } else {
            println!("[priority][gpu] role={} failed to get IDXGIDevice", role);
            None
        }
    }
}

pub struct DxgiCapture {
    device_setup: DeviceSetup,
    duplication: IDXGIOutputDuplication,
    desc: DXGI_OUTDUPL_DESC,
    ring_textures: Vec<ID3D11Texture2D>,
    shared_ring: Option<SharedTextureRing>,
    ring_index: usize,
    media_validation_texture: Option<ID3D11Texture2D>,
    output_width: u32,
    output_height: u32,
    output_format: DXGI_FORMAT,
    config: BenchConfig,
    fence_index: u64,
    gpu_name: String,
}

impl DxgiCapture {
    pub fn new(config: &BenchConfig) -> anyhow::Result<Self> {
        let device_setup = DeviceSetup::new(config.monitor_index, config.device_mode)?;

        // Get adapter name
        let gpu_name = {
            let desc = unsafe { device_setup.adapter.GetDesc()? };
            let len = desc
                .Description
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(desc.Description.len());
            String::from_utf16_lossy(&desc.Description[..len])
        };

        // Create output for duplication
        let output = d3d_find_output(&device_setup.adapter, config.monitor_index)?;
        let output1: IDXGIOutput1 = output.cast()?;
        let duplication = unsafe { output1.DuplicateOutput(&device_setup.capture_device) }?;
        let desc = unsafe { duplication.GetDesc() };

        let output_width = desc.ModeDesc.Width;
        let output_height = desc.ModeDesc.Height;
        let output_format = desc.ModeDesc.Format;

        println!("[DXGI] Output format: {:?}", output_format);
        println!("[DXGI] Output size: {}x{}", output_width, output_height);

        // Create ring textures based on device mode
        let ring_textures;
        let shared_ring;

        match config.device_mode {
            DeviceMode::Single => {
                // Single device: create local ring textures
                ring_textures = Self::create_ring_textures(
                    &device_setup.capture_device,
                    output_width,
                    output_height,
                    output_format,
                    config.ring_size,
                )?;
                shared_ring = None;
                println!(
                    "[DXGI] Created {} local ring textures (single mode)",
                    ring_textures.len()
                );
            }
            DeviceMode::Separated => {
                if config.convert_test == ConvertTest::BgraToNv12 {
                    return Err(anyhow::anyhow!(
                        "ERROR: --convert-test bgra-to-nv12 is not valid in separated mode until the media-side NV12 conversion path uses slot.media_texture."
                    ));
                }

                // Separated mode: create shared texture ring
                ring_textures = Vec::new();
                let media_device = device_setup.media_device.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "ERROR: separated mode requested but media D3D11 device is not available."
                    )
                })?;
                shared_ring = Some(SharedTextureRing::new(
                    &device_setup.capture_device,
                    media_device,
                    config.ring_size,
                    output_width,
                    output_height,
                    output_format,
                )?);
                println!(
                    "[DXGI] Created shared texture ring with {} slots (separated mode)",
                    config.ring_size
                );
            }
        }

        // Apply GPU priorities
        let _ = apply_gpu_thread_priority(
            &device_setup.capture_device,
            "capture",
            config.gpu_priority_capture.0,
        );
        if let Some(ref media_dev) = device_setup.media_device {
            let _ = apply_gpu_thread_priority(media_dev, "media", config.gpu_priority_media.0);
        }

        Ok(Self {
            device_setup,
            duplication,
            desc,
            ring_textures,
            shared_ring,
            ring_index: 0,
            media_validation_texture: None,
            output_width,
            output_height,
            output_format,
            config: config.clone(),
            fence_index: 0,
            gpu_name,
        })
    }

    fn create_ring_textures(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        count: usize,
    ) -> anyhow::Result<Vec<ID3D11Texture2D>> {
        let texture_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE(0),
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut textures = Vec::new();
        for _ in 0..count {
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&texture_desc, None, Some(&mut tex)) }?;
            let texture = tex.ok_or_else(|| anyhow::anyhow!("Failed to create texture"))?;
            textures.push(texture);
        }

        Ok(textures)
    }

    pub fn gpu_name(&self) -> &str {
        &self.gpu_name
    }

    pub fn monitor_info(&self) -> String {
        format!(
            "{} {}x{}",
            self.gpu_name, self.output_width, self.output_height
        )
    }

    pub fn width(&self) -> u32 {
        self.output_width
    }

    pub fn height(&self) -> u32 {
        self.output_height
    }

    fn get_next_ring_texture(&mut self) -> Option<ID3D11Texture2D> {
        if self.ring_textures.is_empty() {
            return None;
        }
        let tex = self.ring_textures[self.ring_index].clone();
        self.ring_index = (self.ring_index + 1) % self.ring_textures.len();
        Some(tex)
    }

    fn wait_budget_ms(ready_wait_budget_us: u64) -> u32 {
        if ready_wait_budget_us == 0 {
            0
        } else {
            let rounded_ms = ready_wait_budget_us.saturating_add(999) / 1000;
            rounded_ms.min(u32::MAX as u64) as u32
        }
    }

    fn ensure_media_validation_texture(&mut self) -> anyhow::Result<ID3D11Texture2D> {
        if let Some(ref texture) = self.media_validation_texture {
            return Ok(texture.clone());
        }

        let media_device = self.device_setup.media_device.as_ref().ok_or_else(|| {
            anyhow::anyhow!("separated media validation requested without a media device")
        })?;
        let texture = SharedTextureRing::create_texture(
            media_device,
            self.output_width,
            self.output_height,
            self.output_format,
            0,
            0,
        )?;
        self.media_validation_texture = Some(texture.clone());
        Ok(texture)
    }

    fn dxgi_success_metric(
        frame_index: usize,
        timestamp_us: i64,
        frame_result: &FrameResult,
        warmup: bool,
    ) -> FrameMetric {
        let mut metric = FrameMetric::success(
            frame_index,
            timestamp_us,
            frame_result.acquire_wait_us,
            frame_result.get_resource_us,
            frame_result.copy_submit_us,
            frame_result.acquire_to_release_us,
            frame_result.release_frame_us,
            frame_result.total_capture_stage_us,
            frame_result.accumulated,
            warmup,
        );
        metric.copy_ready_delay_us = frame_result.copy_ready_delay_us;
        metric.shared_open_us = frame_result.shared_open_us;
        metric.shared_sync_wait_us = frame_result.shared_sync_wait_us;
        metric.convert_submit_us = frame_result.convert_submit_us;
        metric.convert_ready_delay_us = frame_result.convert_ready_delay_us;
        metric
    }

    pub fn run(&mut self, config: &BenchConfig) -> anyhow::Result<BenchSummary> {
        let mut collector = StatsCollector::new();
        let start_time = Instant::now();
        let captured_monitor_rect =
            ScreenRect::from_tuple(crate::d3d::get_monitor_rect(config.monitor_index)?);
        let mut foreground_tracker = ForegroundTracker::new(captured_monitor_rect);
        foreground_tracker.log_startup();

        let mut successful_frames = 0usize;
        let mut timeouts = 0usize;
        let mut access_lost = 0usize;
        let mut duplicated_errors = 0usize;
        let mut total_accumulated: u64 = 0;
        let mut max_accumulated: u32 = 0;
        let mut frame_index = 0usize;

        // Track frame gaps
        let mut last_acquired_time: Option<Instant> = None;
        let mut longest_acquired_gap_ms: f64 = 0.0;
        let mut last_produced_time: Option<Instant> = None;
        let mut longest_produced_gap_ms: f64 = 0.0;
        let mut steady_gap_values_ms: Vec<f64> = Vec::new();
        let mut first_frame_delay_ms: Option<f64> = None;
        let mut first_steady_frame_time_ms: Option<f64> = None;
        let mut startup_long_gap_count = 0usize;
        let mut startup_gap_ms = 0.0;
        let mut last_startup_frame_time = start_time;

        println!("[DXGI] Device mode: {:?}", config.device_mode);
        println!("[DXGI] Convert test: {:?}", config.convert_test);
        println!(
            "[DXGI] Ready wait budget: {}us",
            config.ready_wait_budget_us
        );
        println!("[DXGI] Starting warmup ({} frames)...", config.warmup);

        // Log device map
        self.log_device_map();

        let warmup_start = Instant::now();
        while frame_index < config.warmup {
            let result = self.acquire_and_release_frame(config);
            let current_time = Instant::now();
            let timestamp_us = start_time.elapsed().as_micros() as i64;
            let bucket = foreground_tracker.current_bucket_name().to_string();

            let frame_metric = match result {
                Ok(r) => {
                    if first_frame_delay_ms.is_none() {
                        first_frame_delay_ms =
                            Some(current_time.duration_since(start_time).as_secs_f64() * 1000.0);
                    }
                    let gap_ms = current_time
                        .duration_since(last_startup_frame_time)
                        .as_secs_f64()
                        * 1000.0;
                    if gap_ms > startup_gap_ms {
                        startup_gap_ms = gap_ms;
                    }
                    if gap_ms > 100.0 {
                        startup_long_gap_count += 1;
                    }
                    last_startup_frame_time = current_time;
                    Self::dxgi_success_metric(frame_index, timestamp_us, &r, true)
                        .with_foreground_bucket(&bucket)
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("timeout") {
                        timeouts += 1;
                        FrameMetric::timeout(frame_index, timestamp_us, true)
                            .with_foreground_bucket(&bucket)
                    } else {
                        if err_str.contains("access_lost") {
                            access_lost += 1;
                        } else {
                            duplicated_errors += 1;
                        }
                        FrameMetric::error_metric(frame_index, timestamp_us, err_str, true)
                            .with_foreground_bucket(&bucket)
                    }
                }
            };
            collector.add_metric(frame_metric);
            frame_index += 1;
            let _ = foreground_tracker.sample();
        }
        let warmup_elapsed_ms = warmup_start.elapsed().as_millis() as u64;

        if let Some(duration_sec) = config.duration_sec {
            println!(
                "[DXGI] Warmup complete. Starting benchmark ({:.1}s duration)...",
                duration_sec
            );
        } else {
            println!(
                "[DXGI] Warmup complete. Starting benchmark ({} frames)...",
                config.frames
            );
        }

        let benchmark_start = Instant::now();
        let duration_limit = config.duration_sec.map(Duration::from_secs_f64);
        while duration_limit
            .map(|limit| benchmark_start.elapsed() < limit)
            .unwrap_or(successful_frames < config.frames)
        {
            let result = self.acquire_and_release_frame(config);
            let current_time = Instant::now();
            let timestamp_us = start_time.elapsed().as_micros() as i64;
            let mut foreground_logged = false;
            let bucket = foreground_tracker.current_bucket();
            let bucket_name = bucket.as_str().to_string();

            // Track frame gap for acquired frames
            if let Some(last_time) = last_acquired_time {
                let gap_ms = current_time.duration_since(last_time).as_secs_f64() * 1000.0;
                if gap_ms > longest_acquired_gap_ms {
                    longest_acquired_gap_ms = gap_ms;
                }
            }
            last_acquired_time = Some(current_time);

            match result {
                Ok(frame_result) => {
                    if first_frame_delay_ms.is_none() {
                        first_frame_delay_ms =
                            Some(current_time.duration_since(start_time).as_secs_f64() * 1000.0);
                    }
                    if first_steady_frame_time_ms.is_none() {
                        first_steady_frame_time_ms =
                            Some(current_time.duration_since(start_time).as_secs_f64() * 1000.0);
                        let startup_to_steady_gap_ms = current_time
                            .duration_since(last_startup_frame_time)
                            .as_secs_f64()
                            * 1000.0;
                        if startup_to_steady_gap_ms > startup_gap_ms {
                            startup_gap_ms = startup_to_steady_gap_ms;
                        }
                        if startup_to_steady_gap_ms > 100.0 {
                            startup_long_gap_count += 1;
                        }
                    }
                    successful_frames += 1;
                    foreground_tracker.record_successful_frame(bucket);
                    total_accumulated += frame_result.accumulated as u64;
                    if frame_result.accumulated > max_accumulated {
                        max_accumulated = frame_result.accumulated;
                    }

                    // Track produced frame gap
                    if let Some(previous_produced_time) = last_produced_time {
                        let produced_gap_ms = current_time
                            .duration_since(previous_produced_time)
                            .as_secs_f64()
                            * 1000.0;
                        if produced_gap_ms > longest_produced_gap_ms {
                            longest_produced_gap_ms = produced_gap_ms;
                        }
                        steady_gap_values_ms.push(produced_gap_ms);
                        foreground_tracker.record_gap(bucket, produced_gap_ms);
                        if produced_gap_ms > 100.0 {
                            foreground_tracker.log_long_gap(produced_gap_ms, frame_index);
                            foreground_logged = true;
                        }
                    }
                    last_produced_time = Some(current_time);

                    let frame_metric =
                        Self::dxgi_success_metric(frame_index, timestamp_us, &frame_result, false)
                            .with_foreground_bucket(&bucket_name);
                    collector.add_metric(frame_metric);
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("timeout") {
                        timeouts += 1;
                    } else if err_str.contains("access_lost") {
                        access_lost += 1;
                    } else {
                        duplicated_errors += 1;
                    }

                    let frame_metric =
                        FrameMetric::error_metric(frame_index, timestamp_us, err_str, false)
                            .with_foreground_bucket(&bucket_name);
                    collector.add_metric(frame_metric);
                }
            }

            frame_index += 1;

            // Periodic summary
            if config.summary_every > 0 && frame_index % config.summary_every == 0 {
                println!(
                    "[DXGI] Progress: {}/{} frames, successful={}, timeouts={}, dropped={}",
                    frame_index,
                    config.frames + config.warmup,
                    successful_frames,
                    timeouts,
                    collector.frames_dropped()
                );
                foreground_tracker.log_periodic(frame_index);
                foreground_logged = true;
            }

            if !foreground_logged {
                let _ = foreground_tracker.sample();
            }
        }

        let elapsed = start_time.elapsed();
        let elapsed_ms = elapsed.as_millis() as u64;
        let benchmark_elapsed_ms = benchmark_start.elapsed().as_millis() as u64;
        let captured_fps = if benchmark_elapsed_ms > 0 {
            successful_frames as f64 / (benchmark_elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        let steady_state_fps = captured_fps;

        let effective_source_fps = if total_accumulated > 0 && benchmark_elapsed_ms > 0 {
            Some(total_accumulated as f64 / (benchmark_elapsed_ms as f64 / 1000.0))
        } else {
            None
        };
        let foreground_summary = foreground_tracker.summary();
        let p95_gap_ms = percentile_f64(&steady_gap_values_ms, 0.95);
        let p99_gap_ms = percentile_f64(&steady_gap_values_ms, 0.99);
        let long_gap_count = steady_gap_values_ms
            .iter()
            .filter(|gap| **gap > 100.0)
            .count();
        let foreground_game_fps = foreground_bucket_fps(
            &foreground_summary.buckets,
            ForegroundBucket::GameForegroundOnCapturedMonitor,
        );
        let foreground_other_fps = foreground_bucket_fps(
            &foreground_summary.buckets,
            ForegroundBucket::OtherForegroundOnOtherMonitor,
        );

        // Compute all percentiles
        let main_metrics = collector.get_main_metrics();

        let acquire_wait_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.acquire_wait_us)
            .collect();
        let get_resource_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.get_resource_us)
            .collect();
        let copy_submit_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.copy_submit_us)
            .collect();
        let acquire_to_release_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.acquire_to_release_us)
            .collect();
        let release_frame_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.release_frame_us)
            .collect();
        let total_capture_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.total_capture_stage_us)
            .collect();
        let copy_ready_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.copy_ready_delay_us)
            .collect();
        let shared_open_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.shared_open_us)
            .collect();
        let shared_sync_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.shared_sync_wait_us)
            .collect();
        let convert_submit_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.convert_submit_us)
            .collect();
        let convert_ready_values: Vec<u64> = main_metrics
            .iter()
            .filter_map(|m| m.convert_ready_delay_us)
            .collect();
        let frame_age_values: Vec<u64> = main_metrics
            .iter()
            .map(|m| start_time.elapsed().as_micros() as u64 - m.timestamp_us as u64)
            .collect();

        let acquire_wait_stats = Percentiles::compute(&acquire_wait_values);
        let get_resource_stats = Percentiles::compute(&get_resource_values);
        let copy_submit_stats = Percentiles::compute(&copy_submit_values);
        let acquire_to_release_stats = Percentiles::compute(&acquire_to_release_values);
        let release_frame_stats = Percentiles::compute(&release_frame_values);
        let total_capture_stats = Percentiles::compute(&total_capture_values);
        let copy_ready_stats = Percentiles::compute(&copy_ready_values);
        let shared_open_stats = Percentiles::compute(&shared_open_values);
        let shared_sync_stats = Percentiles::compute(&shared_sync_values);
        let convert_submit_stats = Percentiles::compute(&convert_submit_values);
        let convert_ready_stats = Percentiles::compute(&convert_ready_values);
        let frame_age_stats = Percentiles::compute(&frame_age_values);
        let media_actual_used_count = if config.device_mode == DeviceMode::Separated {
            main_metrics
                .iter()
                .filter(|m| m.convert_submit_us.is_some())
                .count()
        } else {
            0
        };
        let shared_create_stats = self
            .shared_ring
            .as_ref()
            .map(|ring| Percentiles::compute(&ring.shared_create_handle_us));
        let media_open_failed_count = self
            .shared_ring
            .as_ref()
            .map(|ring| ring.media_open_failed_count)
            .unwrap_or(0);
        let shared_path = self
            .shared_ring
            .as_ref()
            .map(|ring| ring.shared_path.as_str().to_string())
            .unwrap_or_else(|| SharedPath::Disabled.as_str().to_string());
        let same_adapter_luid = self
            .device_setup
            .media_luid
            .map(|media_luid| media_luid == self.device_setup.capture_luid);
        let separated_path_valid = config.device_mode == DeviceMode::Separated
            && self
                .shared_ring
                .as_ref()
                .map(|ring| {
                    ring.shared_path != SharedPath::Disabled
                        && ring.shared_path != SharedPath::Failed
                        && ring.media_open_failed_count == 0
                        && ring.slots.iter().any(|slot| slot.media_texture.is_some())
                })
                .unwrap_or(false)
            && media_actual_used_count > 0;

        if config.flush_each_frame {
            unsafe {
                self.device_setup.capture_context.Flush();
            }
        }

        let summary = BenchSummary {
            backend: "DXGI".to_string(),
            monitor_info: self.monitor_info(),
            gpu_name: self.gpu_name().to_string(),
            frames_requested: config.frames,
            warmup_frames: config.warmup,
            successful_frames,
            timeouts,
            access_lost,
            duplicated_errors,
            elapsed_ms,
            captured_fps,
            steady_state_fps,
            effective_source_fps,
            accumulated_frames_total: total_accumulated,
            accumulated_frames_max: max_accumulated,
            // Legacy
            acquire_wait_us: Some(acquire_wait_stats.clone()),
            callback_gap_us: None,
            copy_us: None,
            held_frame_us: None,
            // Extended
            get_resource_us: Some(get_resource_stats),
            copy_submit_us: Some(copy_submit_stats),
            acquire_to_release_us: Some(acquire_to_release_stats),
            release_frame_us: Some(release_frame_stats),
            total_capture_stage_us: Some(total_capture_stats),
            copy_ready_delay_us: Some(copy_ready_stats),
            copy_ready_timeout_count: collector.copy_ready_timeout_count,
            dropped_gpu_not_ready_count: collector.dropped_gpu_not_ready_count,
            shared_create_handle_us: shared_create_stats,
            shared_open_us: Some(shared_open_stats),
            shared_sync_wait_us: Some(shared_sync_stats),
            shared_busy_drop_count: collector.shared_busy_drop_count,
            media_actual_used_count,
            media_open_failed_count,
            separated_path_valid,
            same_adapter_luid,
            shared_path,
            convert_submit_us: Some(convert_submit_stats),
            convert_ready_delay_us: Some(convert_ready_stats),
            convert_timeout_count: collector.convert_timeout_count,
            convert_dropped_not_ready_count: collector.convert_dropped_not_ready_count,
            frames_attempted: collector.frames_attempted(),
            frames_acquired: collector.frames_acquired(),
            frames_dropped: collector.frames_dropped(),
            frame_age_us: Some(frame_age_stats),
            longest_gap_between_acquired_ms: Some(longest_acquired_gap_ms),
            longest_gap_between_produced_ms: Some(longest_produced_gap_ms),
            p95_gap_ms,
            p99_gap_ms,
            long_gap_count,
            warmup_elapsed_ms,
            benchmark_elapsed_ms,
            startup_long_gap_count,
            first_frame_delay_ms,
            first_steady_frame_time_ms,
            startup_gap_ms,
            foreground_buckets: foreground_summary.buckets,
            foreground_game_fps,
            foreground_other_fps,
            callback_gap_histogram: None,
            device_mode: format!("{:?}", config.device_mode),
            cpu_priority_mode: format!("{:?}", config.cpu_priority),
            gpu_priority_capture: format!("{:?}", config.gpu_priority_capture),
            gpu_priority_media: format!("{:?}", config.gpu_priority_media),
            ready_wait_budget_us: config.ready_wait_budget_us,
            percent_time_foreground_on_captured_monitor: foreground_summary
                .percent_time_foreground_on_captured_monitor,
            long_gaps_while_game_foreground: foreground_summary.long_gaps_while_game_foreground,
            long_gaps_while_other_foreground: foreground_summary.long_gaps_while_other_foreground,
            foreground_exe_most_common: foreground_summary.foreground_exe_most_common,
            foreground_title_most_common: foreground_summary.foreground_title_most_common,
            overlay_mode: config.overlay_mode.as_str().to_string(),
            overlay_created: config.overlay_created,
            foreground_unchanged: config.overlay_foreground_unchanged,
        };

        if let Some(csv_path) = &config.csv_path {
            crate::stats::write_summary_csv(&summary, csv_path, config)?;
            println!("[DXGI] Summary CSV written to {}", csv_path);
        }

        Ok(summary)
    }

    fn acquire_and_release_frame(&mut self, config: &BenchConfig) -> anyhow::Result<FrameResult> {
        let total_start = Instant::now();

        // AcquireNextFrame
        let acquire_start = Instant::now();
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut desktop_resource: Option<IDXGIResource> = None;

        unsafe {
            self.duplication.AcquireNextFrame(
                config.timeout_ms,
                &mut frame_info,
                &mut desktop_resource,
            )
        }
        .map_err(|e| {
            let code = e.code().0 as u32;
            if code == 0x887A0007 || code == 0x887A0005 {
                anyhow::anyhow!("timeout: AcquireNextFrame timed out")
            } else if code == 0x887A0027 {
                anyhow::anyhow!("access_lost: Desktop Duplication access lost")
            } else {
                anyhow::anyhow!("AcquireNextFrame failed: {:x}", code)
            }
        })?;

        let acquire_wait_us = acquire_start.elapsed().as_micros() as u64;

        // GetResource
        let get_resource_start = Instant::now();
        let src_tex = if let Some(ref resource) = desktop_resource {
            resource.cast::<ID3D11Texture2D>().ok()
        } else {
            None
        };
        let get_resource_us = get_resource_start.elapsed().as_micros() as u64;

        // Copy to ring/shared texture
        let copy_start = Instant::now();
        let mut shared_open_us = None;
        let mut shared_sync_wait_us = None;
        let mut selected_shared_slot = None;
        let mut pre_release_error = None;

        if let Some(ref tex) = src_tex {
            match config.device_mode {
                DeviceMode::Single => {
                    if config.copy_mode == crate::cli::CopyMode::Copy {
                        if let Some(dst_tex) = self.get_next_ring_texture() {
                            unsafe {
                                self.device_setup
                                    .capture_context
                                    .CopyResource(&dst_tex, tex);
                            }
                        }
                    }
                }
                DeviceMode::Separated => {
                    // Copy to shared ring texture
                    if let Some(ref mut ring) = self.shared_ring {
                        let slot = ring.next_slot();
                        selected_shared_slot = Some(slot);
                        shared_open_us = ring.shared_open_us.get(slot).copied();

                        let wait_ms = Self::wait_budget_ms(config.ready_wait_budget_us);
                        let sync_start = Instant::now();
                        let copy_result = (|| -> anyhow::Result<()> {
                            let ring_slot = ring.slots.get_mut(slot).ok_or_else(|| {
                                anyhow::anyhow!("shared ring slot {} is missing", slot)
                            })?;
                            let keyed_mutex_capture =
                                ring_slot.keyed_mutex_capture.as_ref().ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "shared ring slot {} has no capture keyed mutex",
                                        slot
                                    )
                                })?;

                            unsafe {
                                keyed_mutex_capture.AcquireSync(KEY_CAPTURE, wait_ms)?;
                                self.device_setup
                                    .capture_context
                                    .CopyResource(&ring_slot.capture_texture, tex);
                                keyed_mutex_capture.ReleaseSync(KEY_MEDIA)?;
                            }

                            ring_slot.index = self.fence_index;
                            ring_slot.state = SharedSlotState::CaptureWritten;
                            Ok(())
                        })();

                        shared_sync_wait_us = Some(sync_start.elapsed().as_micros() as u64);
                        if let Err(error) = copy_result {
                            if let Some(ring_slot) = ring.slots.get_mut(slot) {
                                ring_slot.state = SharedSlotState::Dropped;
                            }
                            pre_release_error = Some(error);
                        }
                    }
                }
            }
        }

        let copy_submit_us = copy_start.elapsed().as_micros() as u64;

        // Calculate acquire_to_release time before ReleaseFrame
        let acquire_to_release_start = Instant::now();

        // DROP resource BEFORE ReleaseFrame - this is critical for separated mode
        drop(desktop_resource);

        // ReleaseFrame - must happen BEFORE any convert/media processing
        let release_start = Instant::now();
        unsafe {
            let _ = self.duplication.ReleaseFrame();
        }
        let release_frame_us = release_start.elapsed().as_micros() as u64;

        let acquire_to_release_us = acquire_to_release_start.elapsed().as_micros() as u64;

        if let Some(error) = pre_release_error {
            return Err(error);
        }

        // Media-side validation/conversion happens HERE, after ReleaseFrame.
        let mut convert_submit_us = None;
        let mut convert_ready_delay_us = None;

        if let Some(slot) = selected_shared_slot {
            let media_local_texture = self.ensure_media_validation_texture()?;
            let media_context = self
                .device_setup
                .media_context
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("separated mode has no media context"))?
                .clone();
            let wait_ms = Self::wait_budget_ms(config.ready_wait_budget_us);

            let ring = self
                .shared_ring
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("separated mode has no shared ring"))?;
            let ring_slot = ring
                .slots
                .get_mut(slot)
                .ok_or_else(|| anyhow::anyhow!("shared ring slot {} is missing", slot))?;
            let media_texture = ring_slot
                .media_texture
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("shared ring slot {} has no media texture", slot))?;
            let keyed_mutex_media = ring_slot.keyed_mutex_media.as_ref().ok_or_else(|| {
                anyhow::anyhow!("shared ring slot {} has no media keyed mutex", slot)
            })?;

            let wait_start = Instant::now();
            unsafe {
                keyed_mutex_media.AcquireSync(KEY_MEDIA, wait_ms)?;
            }
            let media_wait_us = wait_start.elapsed().as_micros() as u64;
            shared_sync_wait_us = Some(
                shared_sync_wait_us
                    .unwrap_or(0)
                    .saturating_add(media_wait_us),
            );

            ring_slot.state = SharedSlotState::MediaBusy;
            let convert_start = Instant::now();
            unsafe {
                media_context.CopyResource(&media_local_texture, media_texture);
                keyed_mutex_media.ReleaseSync(KEY_CAPTURE)?;
            }
            convert_submit_us = Some(convert_start.elapsed().as_micros() as u64);
            convert_ready_delay_us = Some(media_wait_us);
            ring_slot.state = SharedSlotState::Free;
        }

        let total_capture_stage_us = total_start.elapsed().as_micros() as u64;
        self.fence_index += 1;

        Ok(FrameResult {
            acquire_wait_us,
            get_resource_us,
            copy_submit_us,
            acquire_to_release_us,
            release_frame_us,
            total_capture_stage_us,
            copy_ready_delay_us: None,
            shared_open_us,
            shared_sync_wait_us,
            convert_submit_us,
            convert_ready_delay_us,
            accumulated: frame_info.AccumulatedFrames,
        })
    }

    fn log_device_map(&self) {
        println!("\n[device-map]");
        println!("  device_mode={:?}", self.config.device_mode);
        println!("  backend=dxgi");
        println!("  gpu_name={}", self.gpu_name);
        println!(
            "  capture_device_ptr={:?}",
            std::ptr::addr_of!(self.device_setup.capture_device)
        );
        println!("  capture_adapter_luid={}", self.device_setup.capture_luid);

        if let Some(ref media_dev) = self.device_setup.media_device {
            println!("  media_device_ptr={:?}", std::ptr::addr_of!(media_dev));
        } else {
            println!("  media_device_ptr=None");
        }
        if let Some(media_luid) = self.device_setup.media_luid {
            println!("  media_adapter_luid={}", media_luid);
            println!(
                "  same_adapter_luid={}",
                media_luid == self.device_setup.capture_luid
            );
            if media_luid != self.device_setup.capture_luid {
                println!(
                    "  WARNING: capture and media devices use different adapter LUIDs; cross-adapter sharing may be slow or unavailable."
                );
            }
        } else {
            println!("  media_adapter_luid=None");
            println!("  same_adapter_luid=None");
        }

        println!(
            "  same_device_capture_media={}",
            self.device_setup.is_same_device()
        );
        println!(
            "  shared_texture_path={}",
            self.config.device_mode == DeviceMode::Separated
        );
        let shared_path = self
            .shared_ring
            .as_ref()
            .map(|ring| ring.shared_path.as_str())
            .unwrap_or_else(|| SharedPath::Disabled.as_str());
        println!("  shared_path={}", shared_path);
        println!(
            "  open_shared_status={}",
            if self
                .shared_ring
                .as_ref()
                .map(|ring| ring.media_open_failed_count == 0 && !ring.slots.is_empty())
                .unwrap_or(false)
            {
                "ok"
            } else {
                "disabled"
            }
        );
        println!("  ring_size={}", self.config.ring_size);
        println!("  convert_test={:?}", self.config.convert_test);
        println!(
            "  ready_wait_budget_us={}",
            self.config.ready_wait_budget_us
        );
        println!(
            "  gpu_priority_capture={:?}",
            self.config.gpu_priority_capture
        );
        println!("  gpu_priority_media={:?}", self.config.gpu_priority_media);
        println!("  cpu_priority={:?}", self.config.cpu_priority);
        println!();
    }

    pub fn release(&mut self) {
        unsafe {
            let _ = self.duplication.ReleaseFrame();
        }
    }
}

fn d3d_find_output(
    adapter: &windows::Win32::Graphics::Dxgi::IDXGIAdapter,
    monitor_index: usize,
) -> anyhow::Result<windows::Win32::Graphics::Dxgi::IDXGIOutput> {
    unsafe {
        let mut i = 0u32;
        loop {
            match adapter.EnumOutputs(i) {
                Ok(output) => {
                    if i as usize == monitor_index {
                        return Ok(output);
                    }
                }
                Err(_) => {
                    break;
                }
            }
            i += 1;
        }
        Err(anyhow::anyhow!("Monitor index {} not found", monitor_index))
    }
}

fn percentile_f64(values: &[f64], pct: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((sorted.len() as f64) * pct) as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub trait CaptureBenchmark {
    fn name(&self) -> &'static str;
    fn run(&mut self, config: &BenchConfig) -> anyhow::Result<BenchSummary>;
}

impl CaptureBenchmark for DxgiCapture {
    fn name(&self) -> &'static str {
        "DXGI"
    }

    fn run(&mut self, config: &BenchConfig) -> anyhow::Result<BenchSummary> {
        self.run(config)
    }
}
