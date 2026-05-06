//! Raw frame types for CPU fallback path.
//!
//! This module contains `RawFrame` which holds raw RGBA pixel data in a `Vec<u8>`.
//! The `Vec<u8>` is only needed for the CPU fallback encoding path.
//!
//! For GPU-only production builds, use the D3D11 texture pipeline directly
//! (see `D3d11BgraToNv12`, `NvencD3d11Encoder`) which operates on GPU textures
//! without CPU memory allocations.

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// Raw RGBA frame from capture (e.g. WGC). Lock-free slot holds `Box<RawFrame>`.
///
/// **Note**: The `pixels` field contains raw RGBA pixel data as a `Vec<u8>`.
/// This is only used by the CPU fallback encoding path. GPU-only builds
/// should use D3D11 textures directly through the `d3d11_*` modules.
///
/// # Feature Gate
/// This struct is only available when the `cpu-fallback` feature is enabled.
/// In pure GPU mode, frame data stays on the GPU as D3D11 textures.
#[derive(Clone)]
#[cfg(feature = "cpu-fallback")]
pub struct RawFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[cfg(feature = "cpu-fallback")]
impl RawFrame {
    pub fn new(pixels: Vec<u8>, width: u32, height: u32) -> Self {
        Self {
            pixels,
            width,
            height,
        }
    }
}

/// SPSC ring buffer for RawFrame: WGC pushes, encoder pops.
/// Reduces skipped frames and FPS jitter when encoder briefly lags (~+16-33 ms latency).
///
/// This is used by the CPU fallback path only.
#[cfg(feature = "cpu-fallback")]
pub struct RawFrameRing {
    slots: [AtomicPtr<RawFrame>; RING_SIZE],
    write_idx: AtomicUsize,
    read_idx: AtomicUsize,
}

/// Ring buffer size: 3 slots allows WGC to capture ahead while encoder processes.
#[cfg(feature = "cpu-fallback")]
pub const RING_SIZE: usize = 3;

#[cfg(feature = "cpu-fallback")]
impl RawFrameRing {
    pub fn new() -> Self {
        Self {
            slots: std::array::from_fn(|_| AtomicPtr::new(std::ptr::null_mut())),
            write_idx: AtomicUsize::new(0),
            read_idx: AtomicUsize::new(0),
        }
    }

    /// Push a new frame into the ring buffer. Drops oldest frame if full.
    pub fn push(&self, ptr: *mut RawFrame) {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Acquire);
        if w.wrapping_sub(r) >= RING_SIZE {
            // Drop oldest
            let old = self.slots[r % RING_SIZE].swap(std::ptr::null_mut(), Ordering::AcqRel);
            if !old.is_null() {
                unsafe {
                    drop(Box::from_raw(old));
                }
            }
            self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        }
        let slot = w % RING_SIZE;
        let old = self.slots[slot].swap(ptr, Ordering::AcqRel);
        // Drop any previously unread frame (can happen when encoder lags at 60 fps).
        if !old.is_null() {
            unsafe {
                drop(Box::from_raw(old));
            }
        }
        self.write_idx.store(w.wrapping_add(1), Ordering::Release);
    }

    /// Pop a frame from the ring buffer. Returns None if empty.
    pub fn pop(&self) -> Option<*mut RawFrame> {
        let r = self.read_idx.load(Ordering::Acquire);
        let w = self.write_idx.load(Ordering::Acquire);
        if r >= w {
            return None;
        }
        let ptr = self.slots[r % RING_SIZE].swap(std::ptr::null_mut(), Ordering::AcqRel);
        self.read_idx.store(r.wrapping_add(1), Ordering::Release);
        if ptr.is_null() {
            None
        } else {
            Some(ptr)
        }
    }

    /// Drain and drop all frames in the ring.
    pub fn drain_drop(&self) {
        while let Some(ptr) = self.pop() {
            if !ptr.is_null() {
                unsafe {
                    drop(Box::from_raw(ptr));
                }
            }
        }
    }
}

#[cfg(feature = "cpu-fallback")]
impl Default for RawFrameRing {
    fn default() -> Self {
        Self::new()
    }
}
