//! Phase 3.5: WGL_NV_DX_interop2 — D3D11 RGBA texture ↔ OpenGL texture zero-copy sharing.
//!
//! Replaces the CPU readback path (D3D11 Map → Vec<u8> → egui::ColorImage) with direct
//! GPU sharing: the same VRAM used by the D3D11 compute shader output is bound as an
//! OpenGL texture and rendered by egui without any CPU involvement.
//!
//! Requirements:
//!   - WGL_NV_DX_interop2 extension (NVIDIA and AMD drivers on Windows 10+)
//!   - D3D11 RGBA texture created with D3D11_RESOURCE_MISC_SHARED (see d3d11_rgba.rs)
//!   - OpenGL context must be current on the calling thread (UI thread)
//!
//! Usage (UI thread, in eframe::App::update):
//!   1. D3d11GlInterop::try_new()        — init once, requires active GL context
//!   2. update_texture(key, handle, …)   — register or re-register shared texture
//!   3. lock_all()                        — transfer ownership to GL before egui renders
//!   At the start of the next eframe::App::update:
//!   4. unlock_all()                      — return ownership to D3D11

#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext};
use windows::Win32::Graphics::OpenGL::wglGetProcAddress;
use windows::core::{Interface, PCSTR};

// ─── GL / WGL constants ───────────────────────────────────────────────────────

const GL_TEXTURE_2D: u32 = 0x0DE1;

/// wglDXRegisterObjectNV access flags.
/// READ_WRITE is used instead of READ_ONLY because some NVIDIA driver versions do not
/// properly synchronize UAV-written textures when registered with READ_ONLY.
/// Semantically GL still only reads (the shader outputs alpha=1 opaque data), but
/// READ_WRITE forces the driver to do a full flush+wait on lock, preventing transparent
/// pixels caused by GL reading before the compute dispatch completes.
const WGL_ACCESS_READ_WRITE_NV: u32 = 0x0001;

// ─── WGL extension function types ────────────────────────────────────────────
// All WGL functions on Windows use stdcall calling convention.

type FnOpen     = unsafe extern "stdcall" fn(dx_device: *mut c_void) -> *mut c_void;
type FnClose    = unsafe extern "stdcall" fn(device: *mut c_void) -> i32;
type FnRegister = unsafe extern "stdcall" fn(
    device: *mut c_void,
    dx_object: *mut c_void,
    name: u32,
    object_type: u32,
    access: u32,
) -> *mut c_void;
type FnUnregister = unsafe extern "stdcall" fn(device: *mut c_void, object: *mut c_void) -> i32;
type FnLock       = unsafe extern "stdcall" fn(device: *mut c_void, count: i32, objects: *mut *mut c_void) -> i32;
type FnUnlock     = unsafe extern "stdcall" fn(device: *mut c_void, count: i32, objects: *mut *mut c_void) -> i32;

// ─── Global availability flag ─────────────────────────────────────────────────

/// Set to true after D3d11GlInterop::try_new() succeeds.
/// Voice thread reads this to decide whether to use zero-copy or CPU-readback path.
pub static GL_INTEROP_AVAILABLE: AtomicBool = AtomicBool::new(false);

// ─── Helper ──────────────────────────────────────────────────────────────────

unsafe fn load_wgl_fn<T: Copy>(name: &[u8]) -> Option<T> {
    debug_assert_eq!(name.last(), Some(&0u8), "name must be null-terminated");
    let proc = wglGetProcAddress(PCSTR(name.as_ptr()))?;
    Some(std::mem::transmute_copy::<_, T>(&proc))
}

// ─── Per-stream interop state ─────────────────────────────────────────────────

struct InteropEntry {
    /// Raw GL texture name (u32). Created by the caller before registration.
    gl_tex_id: u32,
    /// Opaque handle from wglDXRegisterObjectNV.
    registered: *mut c_void,
    /// Raw ID3D11Texture2D COM pointer (as usize) — used to detect when the compute
    /// converter recreated the RGBA output texture (e.g. on resolution change).
    shared_handle: usize,
    pub width: u32,
    pub height: u32,
}

// SAFETY: all WGL/D3D11 calls happen exclusively on the UI thread.
unsafe impl Send for InteropEntry {}

// ─── Main struct ──────────────────────────────────────────────────────────────

/// Manages WGL_NV_DX_interop2 objects for one or more video streams.
/// Created and used exclusively on the UI thread (where the OpenGL context is current).
pub struct D3d11GlInterop {
    /// Handle from wglDXOpenDeviceNV — represents the D3D11↔GL interop channel.
    interop_device: *mut c_void,
    /// D3D11 immediate context — used to call Flush() before wglDXLockObjectsNV so
    /// that all pending compute commands are in the GPU queue before WGL synchronises.
    d3d11_context: ID3D11DeviceContext,
    /// Per-stream registered GL/D3D11 texture pairs.
    entries: HashMap<i64, InteropEntry>,
    /// Objects currently locked for GL access (held between lock_all / unlock_all).
    locked_objects: Vec<*mut c_void>,

    fn_close:      FnClose,
    fn_register:   FnRegister,
    fn_unregister: FnUnregister,
    fn_lock:       FnLock,
    fn_unlock:     FnUnlock,
}

// SAFETY: D3d11GlInterop is only used from the UI thread.
unsafe impl Send for D3d11GlInterop {}

impl D3d11GlInterop {
    /// Initialize WGL_NV_DX_interop2 using the provided D3D11 device.
    ///
    /// **Must be called from the UI thread with the OpenGL context current.**
    ///
    /// `d3d11_device` must be the same device used by the compute shader (`D3d11Nv12ToRgba`).
    /// The device's immediate context is stored so `lock_all()` can call `Flush()` before
    /// `wglDXLockObjectsNV`, ensuring compute-shader commands are in the GPU queue first.
    ///
    /// Returns `Err` if the extension is unavailable (Intel older drivers, some VMs).
    pub fn try_new(d3d11_device: ID3D11Device) -> Result<Self, String> {
        let fn_open = unsafe {
            load_wgl_fn::<FnOpen>(b"wglDXOpenDeviceNV\0")
                .ok_or("WGL_NV_DX_interop2 unavailable: wglDXOpenDeviceNV not found")?
        };
        let fn_close = unsafe {
            load_wgl_fn::<FnClose>(b"wglDXCloseDeviceNV\0")
                .ok_or("wglDXCloseDeviceNV not found")?
        };
        let fn_register = unsafe {
            load_wgl_fn::<FnRegister>(b"wglDXRegisterObjectNV\0")
                .ok_or("wglDXRegisterObjectNV not found")?
        };
        let fn_unregister = unsafe {
            load_wgl_fn::<FnUnregister>(b"wglDXUnregisterObjectNV\0")
                .ok_or("wglDXUnregisterObjectNV not found")?
        };
        let fn_lock = unsafe {
            load_wgl_fn::<FnLock>(b"wglDXLockObjectsNV\0")
                .ok_or("wglDXLockObjectsNV not found")?
        };
        let fn_unlock = unsafe {
            load_wgl_fn::<FnUnlock>(b"wglDXUnlockObjectsNV\0")
                .ok_or("wglDXUnlockObjectsNV not found")?
        };

        let d3d11_context = unsafe {
            d3d11_device
                .GetImmediateContext()
                .map_err(|e| format!("GetImmediateContext: {:?}", e))?
        };

        // Open the WGL interop device — binds the D3D11 device to the current GL context.
        // Must be the same device as the compute shader so wglDXLockObjectsNV flushes
        // and waits for pending NV12→RGBA GPU work before GL reads the texture.
        let interop_device = unsafe { fn_open(d3d11_device.as_raw() as *mut c_void) };
        if interop_device.is_null() {
            return Err(
                "wglDXOpenDeviceNV returned null — extension may not support this D3D11 device"
                    .into(),
            );
        }

        eprintln!("[Phase 3.5] WGL_NV_DX_interop2 initialized successfully");

        Ok(Self {
            interop_device,
            d3d11_context,
            entries: HashMap::new(),
            locked_objects: Vec::new(),
            fn_close,
            fn_register,
            fn_unregister,
            fn_lock,
            fn_unlock,
        })
    }

    /// Register (or re-register on pointer change) a D3D11 RGBA texture as a GL texture.
    ///
    /// `key`: stream key (positive = camera, negative = screen share).
    /// `shared_handle`: raw `ID3D11Texture2D*` COM pointer (as usize).
    ///   Must be the **same** pointer the compute shader writes through (not one opened
    ///   via OpenSharedResource) so wglDXLockObjectsNV sees those exact GPU commands.
    ///   The texture must have D3D11_RESOURCE_MISC_SHARED so WGL can access its VRAM.
    /// `gl_tex_id`: GL texture name created by the caller (via glow::create_texture).
    ///              Must have texture parameters already set (filter, wrap).
    /// `width`, `height`: frame dimensions.
    pub fn update_texture(
        &mut self,
        key: i64,
        shared_handle: usize,
        gl_tex_id: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        // If pointer unchanged (same resolution), just update dimensions and return.
        if let Some(entry) = self.entries.get_mut(&key) {
            if entry.shared_handle == shared_handle {
                entry.width = width;
                entry.height = height;
                return Ok(());
            }
            // Texture was recreated (e.g. resolution change): unregister old object.
            eprintln!("[Phase 3.5] update_texture key={key}: handle changed 0x{:x}→0x{shared_handle:x}, re-registering", entry.shared_handle);
            unsafe { (self.fn_unregister)(self.interop_device, entry.registered) };
            self.entries.remove(&key);
        }

        // Register the original D3D11Texture2D COM pointer directly with WGL.
        // WGL_ACCESS_READ_WRITE_NV: required for UAV-written textures; ensures
        // wglDXLockObjectsNV does a full GPU flush+wait even on compute outputs.
        let d3d11_ptr = shared_handle as *mut c_void;
        let registered = unsafe {
            (self.fn_register)(
                self.interop_device,
                d3d11_ptr,
                gl_tex_id,
                GL_TEXTURE_2D,
                WGL_ACCESS_READ_WRITE_NV,
            )
        };
        if registered.is_null() {
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            return Err(format!(
                "wglDXRegisterObjectNV FAILED key={key} gl_tex={gl_tex_id} ptr=0x{shared_handle:x} GetLastError={err:?}"
            ));
        }
        self.entries.insert(
            key,
            InteropEntry {
                gl_tex_id,
                registered,
                shared_handle,
                width,
                height,
            },
        );

        Ok(())
    }

    /// Returns the GL texture ID and dimensions for a registered stream key, if any.
    pub fn get_gl_tex(&self, key: i64) -> Option<(u32, u32, u32)> {
        self.entries.get(&key).map(|e| (e.gl_tex_id, e.width, e.height))
    }

    /// Lock all registered interop objects for GL access.
    ///
    /// **Call from the UI thread at the end of `eframe::App::update()`.**
    /// Transfers texture ownership to OpenGL; wglDXLockObjectsNV waits for all pending
    /// D3D11 GPU work on the device to complete before returning.
    /// After this call, D3D11 commands that reference these textures will stall until
    /// `unlock_all()` is called.
    pub fn lock_all(&mut self) {
        if self.entries.is_empty() {
            return;
        }

        // Flush any remaining D3D11 commands on this thread before locking.
        // The decode thread already calls Flush() after CopyResource, but this
        // ensures any UI-thread device operations are also submitted.
        unsafe { self.d3d11_context.Flush() };

        let mut handles: Vec<*mut c_void> =
            self.entries.values().map(|e| e.registered).collect();
        let ok = unsafe {
            (self.fn_lock)(self.interop_device, handles.len() as i32, handles.as_mut_ptr())
        };
        if ok == 0 {
            let err = unsafe { windows::Win32::Foundation::GetLastError() };
            eprintln!("[Phase 3.5] wglDXLockObjectsNV FAILED count={} GetLastError={err:?}", handles.len());
        }
        self.locked_objects = handles;
    }

    /// Unlock all interop objects — return ownership to D3D11.
    ///
    /// **Call at the start of `eframe::App::update()` so D3D11 can write the next frame.**
    pub fn unlock_all(&mut self) {
        if self.locked_objects.is_empty() {
            return;
        }
        unsafe {
            (self.fn_unlock)(
                self.interop_device,
                self.locked_objects.len() as i32,
                self.locked_objects.as_mut_ptr(),
            )
        };
        self.locked_objects.clear();
    }

    /// Remove entries whose keys are no longer active (stream ended).
    /// Unregisters the WGL objects. Caller should also delete the corresponding GL textures.
    pub fn remove_keys(&mut self, keys_to_remove: &[i64]) {
        for key in keys_to_remove {
            if let Some(entry) = self.entries.remove(key) {
                unsafe { (self.fn_unregister)(self.interop_device, entry.registered) };
            }
        }
    }

    /// Unregister all entries (call on voice leave or app shutdown).
    pub fn clear(&mut self) {
        for entry in self.entries.values() {
            unsafe { (self.fn_unregister)(self.interop_device, entry.registered) };
        }
        self.entries.clear();
    }
}

impl Drop for D3d11GlInterop {
    fn drop(&mut self) {
        if !self.locked_objects.is_empty() {
            unsafe {
                (self.fn_unlock)(
                    self.interop_device,
                    self.locked_objects.len() as i32,
                    self.locked_objects.as_mut_ptr(),
                )
            };
            self.locked_objects.clear();
        }
        self.clear();
        unsafe { (self.fn_close)(self.interop_device) };
    }
}
