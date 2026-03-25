//! Phase 2: GPU adapter selection for MFT H.264 encoder.
//!
//! Selects best D3D11 device (prefer discrete GPU with largest VRAM).
//! Env var `ASTRIX_GPU_ADAPTER=N` for manual selection by DXGI index.

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::env;

use thiserror::Error;
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_9_1, D3D_FEATURE_LEVEL_9_2,
    D3D_FEATURE_LEVEL_9_3, D3D_FEATURE_LEVEL_10_0, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
    D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_FLAG,
    D3D11_SDK_VERSION, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ADAPTER_DESC1, IDXGIAdapter,
    IDXGIAdapter1, IDXGIDevice, IDXGIFactory1,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// D3D11_CREATE_DEVICE_VIDEO_SUPPORT = 0x800 — required for ID3D11VideoProcessor (Phase 3).
const D3D11_CREATE_DEVICE_VIDEO_SUPPORT: D3D11_CREATE_DEVICE_FLAG =
    D3D11_CREATE_DEVICE_FLAG(0x800);

/// Threshold (bytes) above which we consider adapter "discrete" (512 MB).
const DISCRETE_VRAM_THRESHOLD: u64 = 512 * 1024 * 1024;

#[derive(Error, Debug)]
pub enum GpuDeviceError {
    #[error("No suitable GPU adapter found (all SOFTWARE or empty)")]
    NoAdapter,
    #[error("Adapter index {0} out of range (max {1})")]
    InvalidAdapterIndex(u32, u32),
    #[error("D3D11 device creation failed: {0}")]
    DeviceCreation(#[from] windows::core::Error),
    #[error("Feature level 11.0 not satisfied")]
    FeatureLevelNotSatisfied,
}

/// Info about a DXGI adapter (for UI / logs).
#[derive(Debug, Clone)]
pub struct AdapterInfo {
    pub name: String,
    pub adapter_idx: u32,
    pub dedicated_video_memory: u64,
    pub shared_system_memory: u64,
    pub software: bool,
    /// `true` if DedicatedVideoMemory > 512 MB.
    pub is_discrete: bool,
}

/// D3D11 device + context on selected GPU adapter.
pub struct GpuDevice {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter_name: String,
    pub adapter_idx: u32,
    pub is_discrete: bool,
}

impl GpuDevice {
    /// Select best adapter: discrete GPU with largest VRAM.
    /// Env var `ASTRIX_GPU_ADAPTER=N` — use adapter at index N.
    pub fn select_best() -> Result<Self, GpuDeviceError> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let adapters = Self::enumerate();

        if adapters.is_empty() {
            return Err(GpuDeviceError::NoAdapter);
        }

        let idx = if let Ok(s) = env::var("ASTRIX_GPU_ADAPTER") {
            let n: u32 = s.parse().map_err(|_| {
                GpuDeviceError::InvalidAdapterIndex(0, (adapters.len() as u32).saturating_sub(1))
            })?;
            if n as usize >= adapters.len() {
                return Err(GpuDeviceError::InvalidAdapterIndex(
                    n,
                    (adapters.len() as u32).saturating_sub(1),
                ));
            }
            eprintln!("[gpu_device] ASTRIX_GPU_ADAPTER={} (manual override)", n);
            n
        } else {
            let best_idx = adapters
                .iter()
                .enumerate()
                .filter(|(_, a)| !a.software)
                .max_by_key(|(_, a)| {
                    // Sort: discrete first (by VRAM desc), then integrated
                    let discrete = a.is_discrete as u32;
                    let vram = a.dedicated_video_memory;
                    (discrete, vram)
                })
                .map(|(i, _)| i as u32)
                .ok_or(GpuDeviceError::NoAdapter)?;
            best_idx
        };

        let info = &adapters[idx as usize];
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
        let adapter1: IDXGIAdapter1 = unsafe { factory.EnumAdapters1(idx)? };
        let adapter: IDXGIAdapter = adapter1.cast()?;

        let feature_levels = [
            D3D_FEATURE_LEVEL_11_1,
            D3D_FEATURE_LEVEL_11_0,
            D3D_FEATURE_LEVEL_10_1,
            D3D_FEATURE_LEVEL_10_0,
            D3D_FEATURE_LEVEL_9_3,
            D3D_FEATURE_LEVEL_9_2,
            D3D_FEATURE_LEVEL_9_1,
        ];

        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;

        let mut device = None;
        let mut feature_level = D3D_FEATURE_LEVEL::default();
        let mut context = None;

        unsafe {
            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )?;
        }

        let device = device.ok_or(GpuDeviceError::DeviceCreation(
            windows::core::Error::from(windows::core::HRESULT(-1)),
        ))?;
        let context = context.ok_or(GpuDeviceError::DeviceCreation(
            windows::core::Error::from(windows::core::HRESULT(-1)),
        ))?;

        if feature_level.0 < D3D_FEATURE_LEVEL_11_0.0 {
            return Err(GpuDeviceError::FeatureLevelNotSatisfied);
        }

        let vram_mb = info.dedicated_video_memory / (1024 * 1024);
        let kind = if info.is_discrete {
            "discrete"
        } else {
            "integrated"
        };
        eprintln!(
            "[gpu_device] Selected GPU: {} ({}; {} MB VRAM)",
            info.name.trim_end_matches('\0'),
            kind,
            vram_mb
        );

        Ok(Self {
            device,
            context,
            adapter_name: info.name.clone(),
            adapter_idx: info.adapter_idx,
            is_discrete: info.is_discrete,
        })
    }

    /// Enumerate all DXGI adapters (for UI / logs).
    pub fn enumerate() -> Vec<AdapterInfo> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        let mut result = Vec::new();
        let mut i = 0u32;

        loop {
            let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(i) } {
                Ok(a) => a,
                Err(_) => break,
            };

            let desc = match unsafe { adapter.GetDesc1() } {
                Ok(d) => d,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };

            let name = String::from_utf16_lossy(
                &desc.Description
                    [..desc.Description.iter().position(|&c| c == 0).unwrap_or(0)],
            )
            .trim_end_matches('\0')
            .to_string();

            let software = (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0;
            let dedicated = desc.DedicatedVideoMemory as u64;
            let shared = desc.SharedSystemMemory as u64;
            let is_discrete = dedicated > DISCRETE_VRAM_THRESHOLD;

            result.push(AdapterInfo {
                name: if name.is_empty() {
                    format!("Adapter {}", i)
                } else {
                    name
                },
                adapter_idx: i,
                dedicated_video_memory: dedicated,
                shared_system_memory: shared,
                software,
                is_discrete,
            });

            i += 1;
        }

        result
    }

    /// Get adapter info from an existing D3D11 device (Phase 2.6: WGC reuse).
    /// Use to check if WGC's device matches our preferred adapter.
    pub fn adapter_info_from_device(
        device: &ID3D11Device,
    ) -> Result<AdapterInfo, GpuDeviceError> {
        let dxgi_device: IDXGIDevice = device.cast().map_err(GpuDeviceError::DeviceCreation)?;
        let adapter: IDXGIAdapter1 = unsafe { dxgi_device.GetAdapter()?.cast()? };
        let desc: DXGI_ADAPTER_DESC1 = unsafe { adapter.GetDesc1()? };

        let name = String::from_utf16_lossy(
            &desc.Description[..desc.Description.iter().position(|&c| c == 0).unwrap_or(0)],
        )
        .trim_end_matches('\0')
        .to_string();

        let software = (desc.Flags & (DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32)) != 0;
        let dedicated = desc.DedicatedVideoMemory as u64;
        let shared = desc.SharedSystemMemory as u64;
        let is_discrete = dedicated > DISCRETE_VRAM_THRESHOLD;

        // Adapter index unknown from device alone; use 0 as placeholder.
        Ok(AdapterInfo {
            name: if name.is_empty() {
                "Unknown adapter".to_string()
            } else {
                name
            },
            adapter_idx: 0,
            dedicated_video_memory: dedicated,
            shared_system_memory: shared,
            software,
            is_discrete,
        })
    }
}
