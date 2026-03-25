//! Screen share encoding abstraction: CPU vs hardware (NVENC, AMF, QuickSync).
//!
//! LiveKit's `NativeVideoSource` currently accepts only raw I420 frames; the actual
//! H264 encoding is done inside the WebRTC stack (OpenH264 by default). This module
//! provides a unified encoder abstraction that:
//! - **CPU path**: scales RGBA → I420 (libyuv) and returns I420 for `capture_frame()`.
//! - **HW path (stub)**: When LiveKit gains encoded-frame API, NVENC/AMF can return
//!   pre-encoded H264; until then HW implementations fall back to CPU pipeline.
//!
//! Lock-free frame swap (AtomicPtr<RawFrame>), warmup, and instrumentation remain
//! in the capture/encoder thread in voice_livekit.

use livekit::webrtc::prelude::VideoBuffer;


/// Raw RGBA frame from capture (e.g. WGC). Lock-free slot holds `Box<RawFrame>`.
#[derive(Clone)]
pub struct RawFrame {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl RawFrame {
    pub fn new(pixels: Vec<u8>, width: u32, height: u32) -> Self {
        Self { pixels, width, height }
    }
}

/// Timing for one encoded/prepared frame (for instrumentation and BOTTLENECK).
#[derive(Default, Clone, Copy)]
pub struct FrameTiming {
    pub scale_ns: u64,
    pub convert_ns: u64,
    pub total_ns: u64,
}

/// Output of the encoder. LiveKit currently only accepts raw I420 → we use `RawI420`.
/// When the SDK supports injecting encoded H264, add `EncodedH264(EncodedFrame)`.
pub enum EncoderOutput {
    /// I420 frame to push to NativeVideoSource::capture_frame (CPU or HW fallback).
    RawI420 {
        frame: livekit::webrtc::video_frame::VideoFrame<livekit::webrtc::video_frame::I420Buffer>,
        timing: FrameTiming,
    },
    /// Pre-encoded H264 (for future use when LiveKit accepts encoded frames).
    #[allow(dead_code)]
    EncodedH264(EncodedFrame),
}

/// Pre-encoded H264 NAL data (for future encoded-frame publishing).
#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub timestamp_us: i64,
    pub width: u32,
    pub height: u32,
}

/// Errors from encoder init or encode.
#[derive(Debug)]
pub enum EncoderError {
    Unsupported,
    Init(String),
    Encode(String),
}

/// Encoder that turns a raw RGBA frame into either I420 (for capture_frame) or
/// encoded H264 (when supported). Used by the screen encoder thread.
/// Buffer reuse: pass the buffer from the previous frame (after capture_frame + mem::replace)
/// as `returned_buffer`; the encoder fills it and returns it in the output (no per-frame alloc).
pub trait VideoEncoder: Send {
    /// Prepare or encode one frame. Target resolution must match preset.
    /// `returned_buffer`: buffer from the previous frame to reuse (caller does mem::replace after capture_frame).
    fn encode_frame(
        &mut self,
        raw: &RawFrame,
        target_width: u32,
        target_height: u32,
        timestamp_us: i64,
        returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer>,
    ) -> Result<EncoderOutput, EncoderError>;

    /// Human-readable name for logs (e.g. "cpu", "nvenc", "amf").
    fn name(&self) -> &'static str;
}

/// Persistent buffers for CPU encoder: reuse to avoid 120+ allocs/sec and cache thrash.
/// No mem::replace: caller passes buffer back; we fill it (no-scale) or use full_i420 + scale() (scale path).
pub struct CpuEncoderBuffers {
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    /// Full-size I420 for scale path: fill then I420Buffer::scale() (libyuv, SIMD).
    full_i420: livekit::webrtc::video_frame::I420Buffer,
}

impl CpuEncoderBuffers {
    fn new(src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Self {
        use livekit::webrtc::video_frame::I420Buffer;
        Self {
            src_w,
            src_h,
            dst_w,
            dst_h,
            full_i420: I420Buffer::new(src_w, src_h),
        }
    }

    fn ensure_size(&mut self, src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) {
        use livekit::webrtc::video_frame::I420Buffer;
        if self.src_w != src_w || self.src_h != src_h || self.dst_w != dst_w || self.dst_h != dst_h {
            self.src_w = src_w;
            self.src_h = src_h;
            self.dst_w = dst_w;
            self.dst_h = dst_h;
            self.full_i420 = I420Buffer::new(src_w, src_h);
        }
    }
}

/// CPU path: scale (libyuv) + RGBA→I420 with persistent buffers. H264 by LiveKit/OpenH264.
pub struct CpuH264Encoder {
    buffers: Option<CpuEncoderBuffers>,
}

impl CpuH264Encoder {
    pub fn new() -> Self {
        Self { buffers: None }
    }
}

impl Default for CpuH264Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoEncoder for CpuH264Encoder {
    fn encode_frame(
        &mut self,
        raw: &RawFrame,
        target_width: u32,
        target_height: u32,
        timestamp_us: i64,
        returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer>,
    ) -> Result<EncoderOutput, EncoderError> {
        let t_start = std::time::Instant::now();
        let (scale_ns, convert_ns, frame) = cpu_encode_frame_reuse(
            self.buffers.get_or_insert_with(|| {
                CpuEncoderBuffers::new(raw.width, raw.height, target_width, target_height)
            }),
            &raw.pixels,
            raw.width,
            raw.height,
            target_width,
            target_height,
            returned_buffer,
        )
        .map_err(EncoderError::Encode)?;
        let total_ns = t_start.elapsed().as_nanos() as u64;
        let timing = FrameTiming {
            scale_ns,
            convert_ns,
            total_ns,
        };
        let frame = livekit::webrtc::video_frame::VideoFrame {
            rotation: livekit::webrtc::video_frame::VideoRotation::VideoRotation0,
            timestamp_us,
            buffer: frame,
        };
        Ok(EncoderOutput::RawI420 { frame, timing })
    }

    fn name(&self) -> &'static str {
        "cpu"
    }
}

/// Box-filter scale RGBA (src) → RGBA (dst). Reused buffer for ARGBScale-before-I420 path.
/// Public for xcap I420 path (scale-first then convert).
#[inline(never)]
pub fn box_scale_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
) {
    let dst_w = dst_w as usize;
    let dst_h = dst_h as usize;
    let src_w = src_w as usize;
    let src_h = src_h as usize;
    let src_stride = src_w * 4;
    for dy in 0..dst_h {
        let sy0 = (dy * src_h) / dst_h;
        let sy1 = ((dy + 1) * src_h).min(src_h).max(sy0 + 1);
        for dx in 0..dst_w {
            let sx0 = (dx * src_w) / dst_w;
            let sx1 = ((dx + 1) * src_w).min(src_w).max(sx0 + 1);
            let mut r = 0u32;
            let mut g = 0u32;
            let mut b = 0u32;
            let mut a = 0u32;
            let mut n = 0u32;
            for sy in sy0..sy1 {
                let row = &src[sy * src_stride..];
                for sx in sx0..sx1 {
                    let i = sx * 4;
                    r += row[i] as u32;
                    g += row[i + 1] as u32;
                    b += row[i + 2] as u32;
                    a += row[i + 3] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            let out_off = (dy * dst_w + dx) * 4;
            dst[out_off] = (r / n) as u8;
            dst[out_off + 1] = (g / n) as u8;
            dst[out_off + 2] = (b / n) as u8;
            dst[out_off + 3] = (a / n) as u8;
        }
    }
}

/// CPU encode with persistent buffers: caller passes buffer back; we fill it (no-scale) or use full_i420 + scale() (scale path).
fn cpu_encode_frame_reuse(
    buffers: &mut CpuEncoderBuffers,
    rgba: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
    returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer>,
) -> Result<(u64, u64, livekit::webrtc::video_frame::I420Buffer), String> {
    use livekit::webrtc::native::yuv_helper;
    use livekit::webrtc::video_frame::I420Buffer;

    if rgba.len() < (src_w * src_h * 4) as usize {
        return Err("rgba buffer too small".into());
    }

    buffers.ensure_size(src_w, src_h, dst_w, dst_h);

    let t_convert = std::time::Instant::now();
    let (scale_ns, convert_ns, out) = if src_w == dst_w && src_h == dst_h {
        // No scale: fill returned buffer (reuse) or allocate once.
        let mut to_fill = match &returned_buffer {
            Some(b) if b.width() == src_w && b.height() == src_h => returned_buffer.unwrap(),
            _ => I420Buffer::new(src_w, src_h),
        };
        let (y, u, v) = to_fill.data_mut();
        yuv_helper::abgr_to_i420(
            rgba,
            src_w * 4,
            y,
            src_w,
            u,
            (src_w + 1) / 2,
            v,
            (src_w + 1) / 2,
            src_w as i32,
            src_h as i32,
        );
        let convert_ns = t_convert.elapsed().as_nanos() as u64;
        (0u64, convert_ns, to_fill)
    } else {
        // Scale path: RGBA → full_i420 (libyuv) → I420Buffer::scale() (libyuv, SIMD). One alloc per frame for scale() result.
        let (y, u, v) = buffers.full_i420.data_mut();
        yuv_helper::abgr_to_i420(
            rgba,
            src_w * 4,
            y,
            src_w,
            u,
            (src_w + 1) / 2,
            v,
            (src_w + 1) / 2,
            src_w as i32,
            src_h as i32,
        );
        let convert_ns = t_convert.elapsed().as_nanos() as u64;
        let t_scale = std::time::Instant::now();
        let scaled = buffers.full_i420.scale(dst_w as i32, dst_h as i32);
        let scale_ns = t_scale.elapsed().as_nanos() as u64;
        (scale_ns, convert_ns, scaled)
    };

    Ok((scale_ns, convert_ns, out))
}

// ─── Hardware encoder stubs ─────────────────────────────────────────────────
// When LiveKit supports publishing pre-encoded H264, implement real encode here.
// Until then, HW encoders fall back to CPU pipeline so the same I420 path is used
// (and if the LiveKit webrtc build uses NVENC internally, it may still use GPU).

/// NVIDIA NVENC. Fallback: use CPU pipeline until encoded-frame API exists.
#[cfg(target_os = "windows")]
pub struct NvencH264Encoder {
    fallback: CpuH264Encoder,
}

#[cfg(target_os = "windows")]
impl NvencH264Encoder {
    /// Returns Some when NVIDIA GPU with NVENC is detected; until then use CPU.
    pub fn try_new() -> Option<Self> {
        // TODO: detect NVIDIA GPU via DXGI or nvapi; check NVENC support.
        // For now no detection → None → select_screen_encoder() uses CPU.
        None
    }
}

#[cfg(target_os = "windows")]
impl VideoEncoder for NvencH264Encoder {
    fn encode_frame(
        &mut self,
        raw: &RawFrame,
        target_width: u32,
        target_height: u32,
        timestamp_us: i64,
        returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer>,
    ) -> Result<EncoderOutput, EncoderError> {
        self.fallback.encode_frame(raw, target_width, target_height, timestamp_us, returned_buffer)
    }

    fn name(&self) -> &'static str {
        "nvenc"
    }
}

/// AMD AMF. Fallback: use CPU pipeline until encoded-frame API exists.
#[cfg(target_os = "windows")]
pub struct AmfH264Encoder {
    fallback: CpuH264Encoder,
}

#[cfg(target_os = "windows")]
impl AmfH264Encoder {
    /// Returns Some when AMD GPU with AMF is detected.
    pub fn try_new() -> Option<Self> {
        // TODO: detect AMD GPU; check AMF support.
        None
    }
}

#[cfg(target_os = "windows")]
impl VideoEncoder for AmfH264Encoder {
    fn encode_frame(
        &mut self,
        raw: &RawFrame,
        target_width: u32,
        target_height: u32,
        timestamp_us: i64,
        returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer>,
    ) -> Result<EncoderOutput, EncoderError> {
        self.fallback.encode_frame(raw, target_width, target_height, timestamp_us, returned_buffer)
    }

    fn name(&self) -> &'static str {
        "amf"
    }
}

/// Intel QuickSync. Fallback: use CPU pipeline until encoded-frame API exists.
#[cfg(target_os = "windows")]
pub struct QsvH264Encoder {
    fallback: CpuH264Encoder,
}

#[cfg(target_os = "windows")]
impl QsvH264Encoder {
    /// Returns Some when Intel GPU with QuickSync is detected.
    pub fn try_new() -> Option<Self> {
        // TODO: detect Intel GPU; check QSV support.
        None
    }
}

#[cfg(target_os = "windows")]
impl VideoEncoder for QsvH264Encoder {
    fn encode_frame(
        &mut self,
        raw: &RawFrame,
        target_width: u32,
        target_height: u32,
        timestamp_us: i64,
        returned_buffer: Option<livekit::webrtc::video_frame::I420Buffer>,
    ) -> Result<EncoderOutput, EncoderError> {
        self.fallback.encode_frame(raw, target_width, target_height, timestamp_us, returned_buffer)
    }

    fn name(&self) -> &'static str {
        "qsv"
    }
}

/// Select best available encoder: try NVENC (NVIDIA) → AMF (AMD) → QSV (Intel) → CPU.
/// Preserves cross-GPU/CPU compatibility and fallback.
#[cfg(target_os = "windows")]
pub fn select_screen_encoder() -> Box<dyn VideoEncoder> {
    if let Some(enc) = NvencH264Encoder::try_new() {
        eprintln!("[voice][screen] encoder: nvenc (NVIDIA, CPU fallback for capture_frame path)");
        return Box::new(enc);
    }
    if let Some(enc) = AmfH264Encoder::try_new() {
        eprintln!("[voice][screen] encoder: amf (AMD, CPU fallback)");
        return Box::new(enc);
    }
    if let Some(enc) = QsvH264Encoder::try_new() {
        eprintln!("[voice][screen] encoder: qsv (Intel, CPU fallback)");
        return Box::new(enc);
    }
    eprintln!("[voice][screen] encoder: cpu");
    Box::new(CpuH264Encoder::new())
}

/// Non-Windows: CPU only.
#[cfg(not(target_os = "windows"))]
pub fn select_screen_encoder() -> Box<dyn VideoEncoder> {
    eprintln!("[voice][screen] encoder: cpu (no HW encoder on this platform)");
    Box::new(CpuH264Encoder::new())
}
