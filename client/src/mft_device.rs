//! Shared D3D11 device management for MFT hardware decoder (Phase 1.8).
//!
//! Call `init_for_mft_decode()` before starting a LiveKit session.
//! This passes the shared `ID3D11Device*` to the C++ MFT decoder factory
//! so each new `MftH264DecoderImpl` gets a D3D11 device and can use the
//! DXVA2/D3D11 hardware decode path (zero-copy GPU).
//!
//! Note: on Windows, Rust's `std::env::set_var` calls `SetEnvironmentVariableW`
//! which does NOT update the CRT environment table read by C++'s `std::getenv`.
//! The C++ `VideoDecoderFactory` defaults to MFT regardless of the env var;
//! `ASTRIX_DECODE_PATH=cpu` set before process launch is the only way to opt out.
//!
//! The `GpuDevice` is kept alive in a global `Mutex` so the raw pointer
//! remains valid for the lifetime of the decoder.

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::ffi::c_void;

use parking_lot::Mutex;
use windows::core::Interface;

use crate::gpu_device::GpuDevice;

/// Keeps the GpuDevice alive so `ID3D11Device*` stays valid for C++ side.
static SHARED_GPU: Mutex<Option<GpuDevice>> = Mutex::new(None);

extern "C" {
    /// Set the `ID3D11Device*` used by the MFT hardware decoder factory.
    /// Implemented in `mft_decoder_factory.cpp`. C++ AddRefs the pointer.
    /// Pass null to clear (disable hardware path).
    fn webrtc_mft_set_d3d11_device(device_ptr: *mut c_void);
}

/// Initialize the shared D3D11 device for MFT hardware decode.
///
/// - Creates a `GpuDevice` (same selection logic as MFT encoder: best adapter).
/// - Passes the raw `ID3D11Device*` to C++ via `webrtc_mft_set_d3d11_device`.
/// - Stores the `GpuDevice` in a global so the pointer stays valid.
///
/// Safe to call multiple times: on subsequent calls the existing device is
/// re-registered (in case C++ global was reset between sessions).
pub fn init_for_mft_decode() {
    let mut guard = SHARED_GPU.lock();

    if let Some(ref gpu) = *guard {
        // Already have a device — re-register it (idempotent).
        let raw = unsafe { gpu.device.as_raw() as *mut c_void };
        unsafe { webrtc_mft_set_d3d11_device(raw) };
        eprintln!("[mft_device] Re-registered existing D3D11 device for hardware decode");
        return;
    }

    match GpuDevice::select_best() {
        Ok(gpu) => {
            let raw = unsafe { gpu.device.as_raw() as *mut c_void };
            unsafe { webrtc_mft_set_d3d11_device(raw) };
            *guard = Some(gpu);
            eprintln!("[mft_device] Shared D3D11 device initialized for hardware decode");
        }
        Err(e) => {
            eprintln!(
                "[mft_device] GpuDevice::select_best failed: {:?} — MFT will use software decode",
                e
            );
            // C++ will use nullptr device → software MFT path.
        }
    }
}

/// Get a clone of the shared D3D11 device for use on the decode thread.
///
/// Returns `None` if `init_for_mft_decode()` has not been called yet.
/// The returned clone keeps the underlying GPU resource alive.
pub fn get_shared_device() -> Option<windows::Win32::Graphics::Direct3D11::ID3D11Device> {
    let guard = SHARED_GPU.lock();
    guard.as_ref().map(|g| g.device.clone())
}

/// Clear the shared D3D11 device (call when session ends, optional).
/// C++ will fall back to software decode until re-initialized.
pub fn clear_mft_device() {
    let mut guard = SHARED_GPU.lock();
    unsafe { webrtc_mft_set_d3d11_device(std::ptr::null_mut()) };
    *guard = None;
    eprintln!("[mft_device] Shared D3D11 device cleared");
}
