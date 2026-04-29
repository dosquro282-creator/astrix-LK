//! Pipeline telemetry: capture, convert, encode, send, network, decode, render.
//! Enable with ASTRIX_TELEMETRY=1. Output every 30 frames or 1 second.
//! Phase 4.1: encoder_type, encode_avg_us, frames_dropped for MFT diagnostics.
//! Phase 3.1: receiver stats (network min/max/avg, frame_delta, stall_count, recv_fps).
//! Phase 6: env var cached via OnceLock; output batched into single write.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use parking_lot::RwLock;

static TELEMETRY_ENABLED: OnceLock<bool> = OnceLock::new();

/// Check if ASTRIX_TELEMETRY=1. Cached on first call (process-lifetime).
pub fn is_telemetry_enabled() -> bool {
    *TELEMETRY_ENABLED.get_or_init(|| {
        std::env::var("ASTRIX_TELEMETRY")
            .map(|v| v == "1")
            .unwrap_or(false)
    })
}

/// Last measured duration per stage (microseconds). 0 = not yet measured.
#[derive(Default)]
pub struct PipelineTelemetry {
    pub capture_us: AtomicU64,
    pub convert_us: AtomicU64,
    pub encode_us: AtomicU64,
    /// Peak encode time since last telemetry print (microseconds).
    pub encode_max_us: AtomicU64,
    /// Sliding average encode time (microseconds). EMA: avg = avg*0.9 + us*0.1.
    pub encode_avg_us: AtomicU64,
    /// Frames dropped due to encode overshoot (late by >2 intervals). Reset at print.
    pub frames_dropped: AtomicU64,
    /// Encoder type: "MFT GPU (NVIDIA H.264 Encoder MFT, hardware)" or "MFT software (...)".
    encoder_type: RwLock<Option<String>>,
    pub send_us: AtomicU64,
    pub network_us: AtomicU64,
    pub decode_us: AtomicU64,
    pub render_us: AtomicU64,
    /// GUI: время отрисовки кадра (sync текстур + добавление Image в egui).
    pub gui_draw_us: AtomicU64,

    // Receiver-only: inter-frame and network jitter stats (reset at print).
    /// Last inter-frame interval (stream.next() to stream.next()) in µs.
    pub recv_frame_delta_us: AtomicU64,
    /// Last expected interval from sender timestamps (µs).
    pub recv_expected_us: AtomicU64,
    /// Min network jitter in window (frame_delta - expected).
    recv_network_min_us: AtomicU64,
    /// Max network jitter in window.
    recv_network_max_us: AtomicU64,
    /// EMA of network jitter.
    recv_network_avg_us: AtomicU64,
    /// STALL count (frame_delta > 100ms) in window. Reset at print.
    recv_stall_count: AtomicU64,
    /// Frames received in window (for FPS). Reset at print.
    recv_frame_count: AtomicU64,
    /// Pure stream.next() wait time (µs). Excludes local processing of previous frame.
    recv_wait_us: AtomicU64,
    /// Peak stream.next() wait in window (µs). Reset at print.
    recv_wait_max_us: AtomicU64,

    // Converter thread (viewer): narrows "2s rubber band" to recv vs queue vs convert/pacing vs UI.
    /// Peak coalesce depth after `recv`+`try_recv` (1 = no backlog). Reset at print.
    recv_coalesce_peak: AtomicU64,
    /// Sum of (depth−1): raw frames dropped without decode this window. Reset at print.
    recv_coalesced_frames: AtomicU64,
    /// Peak wall time one converter iteration: after coalesce → after `vf.insert` (µs). Reset at print.
    recv_conv_iter_peak_us: AtomicU64,
    /// Total `sleep` time in pacing branch this window (µs). Reset at print.
    recv_pacing_sleep_sum_us: AtomicU64,

    // Sender-only: queue buildup diagnostics for LONG_STALL/NACK/PLI investigation.
    /// Current outgoing queue depth (frames waiting to be sent).
    sender_outgoing_queue_cur: AtomicU64,
    /// Peak outgoing queue depth in window. Reset at print.
    sender_outgoing_queue_peak: AtomicU64,
    /// Average outgoing queue depth in window (EMA). Reset at print.
    sender_outgoing_queue_avg: AtomicU64,

    /// Current pacer queue depth (RTP packets buffered for pacing).
    sender_pacer_queue_cur: AtomicU64,
    /// Peak pacer queue depth in window. Reset at print.
    sender_pacer_queue_peak: AtomicU64,

    /// Current encoded frames pending (in encoder output queue).
    sender_encoded_pending_cur: AtomicU64,
    /// Peak encoded frames pending in window. Reset at print.
    sender_encoded_pending_peak: AtomicU64,

    /// Current RTP packets pending (queued for send).
    sender_rtp_pending_cur: AtomicU64,
    /// Peak RTP packets pending in window. Reset at print.
    sender_rtp_pending_peak: AtomicU64,
}

impl PipelineTelemetry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_capture(&self, us: u64) {
        self.capture_us.store(us, Ordering::Release);
    }
    pub fn set_convert(&self, us: u64) {
        self.convert_us.store(us, Ordering::Release);
    }
    pub fn set_encode(&self, us: u64) {
        self.encode_us.store(us, Ordering::Release);
        self.encode_max_us.fetch_max(us, Ordering::Relaxed);
        // EMA: avg = avg*0.9 + us*0.1
        let _: u64 = self
            .encode_avg_us
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                Some(if old == 0 { us } else { (old * 9 + us) / 10 })
            })
            .unwrap_or(0);
    }
    pub fn set_encoder_type(&self, s: &str) {
        *self.encoder_type.write() = Some(s.to_string());
    }
    pub fn add_frames_dropped(&self, n: u64) {
        self.frames_dropped.fetch_add(n, Ordering::Relaxed);
    }
    pub fn set_send(&self, us: u64) {
        self.send_us.store(us, Ordering::Release);
    }
    pub fn set_network(&self, us: u64) {
        self.network_us.store(us, Ordering::Release);
    }

    /// Receiver: increment frame count. Call for every frame.
    pub fn inc_recv_frame_count(&self) {
        self.recv_frame_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Receiver: update network jitter stats. Call when raw_frame_count > 1.
    /// `network_us` = frame_delta_us - expected_us (extra delay).
    /// `frame_delta_us` = actual interval between stream.next() returns.
    /// `expected_us` = sender's declared interval from timestamps.
    pub fn add_receiver_frame(&self, network_us: u64, frame_delta_us: u64, expected_us: u64) {
        self.network_us.store(network_us, Ordering::Release);
        self.recv_frame_delta_us
            .store(frame_delta_us, Ordering::Release);
        self.recv_expected_us.store(expected_us, Ordering::Release);
        // Min: update if first (0) or smaller
        let _: u64 = self
            .recv_network_min_us
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                Some(if old == 0 || network_us < old {
                    network_us
                } else {
                    old
                })
            })
            .unwrap_or(0);
        self.recv_network_max_us
            .fetch_max(network_us, Ordering::Relaxed);
        // EMA for avg
        let _: u64 = self
            .recv_network_avg_us
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                Some(if old == 0 {
                    network_us
                } else {
                    (old * 9 + network_us) / 10
                })
            })
            .unwrap_or(0);
        if frame_delta_us > 100_000 {
            self.recv_stall_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Receiver: record pure stream.next() wait time (excludes conversion).
    pub fn set_recv_wait(&self, us: u64) {
        self.recv_wait_us.store(us, Ordering::Release);
        self.recv_wait_max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Converter: `recv` coalesce depth (how many frames were waiting in the channel).
    pub fn record_recv_coalesce(&self, depth: u64) {
        self.recv_coalesce_peak.fetch_max(depth, Ordering::Relaxed);
        if depth > 1 {
            self.recv_coalesced_frames
                .fetch_add(depth - 1, Ordering::Relaxed);
        }
    }

    pub fn record_recv_conv_iter_peak(&self, us: u64) {
        self.recv_conv_iter_peak_us.fetch_max(us, Ordering::Relaxed);
    }

    pub fn add_recv_pacing_sleep_us(&self, us: u64) {
        self.recv_pacing_sleep_sum_us
            .fetch_add(us, Ordering::Relaxed);
    }

    /// Sender: record queue depths for buildup diagnostics.
    /// Call once per telemetry window with current values.
    pub fn record_sender_queue_depths(
        &self,
        outgoing_queue: u64,
        pacer_queue: u64,
        encoded_pending: u64,
        rtp_pending: u64,
    ) {
        self.sender_outgoing_queue_cur.store(outgoing_queue, Ordering::Release);
        self.sender_outgoing_queue_peak.fetch_max(outgoing_queue, Ordering::Relaxed);
        // EMA: avg = avg*0.9 + val*0.1
        let _: u64 = self
            .sender_outgoing_queue_avg
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |old| {
                Some(if old == 0 { outgoing_queue } else { (old * 9 + outgoing_queue) / 10 })
            })
            .unwrap_or(0);

        self.sender_pacer_queue_cur.store(pacer_queue, Ordering::Release);
        self.sender_pacer_queue_peak.fetch_max(pacer_queue, Ordering::Relaxed);

        self.sender_encoded_pending_cur.store(encoded_pending, Ordering::Release);
        self.sender_encoded_pending_peak.fetch_max(encoded_pending, Ordering::Relaxed);

        self.sender_rtp_pending_cur.store(rtp_pending, Ordering::Release);
        self.sender_rtp_pending_peak.fetch_max(rtp_pending, Ordering::Relaxed);
    }

    /// Reset receiver window stats (call at start of print for receiver).
    fn reset_receiver_window(&self) {
        self.recv_network_min_us.store(0, Ordering::Release);
        self.recv_network_max_us.store(0, Ordering::Release);
        self.recv_wait_max_us.store(0, Ordering::Release);
    }
    pub fn set_decode(&self, us: u64) {
        self.decode_us.store(us, Ordering::Release);
    }
    pub fn set_render(&self, us: u64) {
        self.render_us.store(us, Ordering::Release);
    }
    pub fn set_gui_draw(&self, us: u64) {
        self.gui_draw_us.store(us, Ordering::Release);
    }

    fn get_ms(v: u64) -> String {
        format!("{:.2} ms", v as f64 / 1000.0)
    }

    /// Print telemetry to stderr. Call periodically (e.g. every 30 frames or 1 second).
    /// Enable with ASTRIX_TELEMETRY=1. Uses cached env var check (OnceLock) and batches
    /// all output into a single write to avoid locking stderr 15+ times per call.
    pub fn print(&self, prefix: &str) {
        if !is_telemetry_enabled() {
            if prefix == "receiver" {
                self.recv_frame_count.swap(0, Ordering::AcqRel);
                self.recv_stall_count.swap(0, Ordering::AcqRel);
                self.reset_receiver_window();
                self.encode_max_us.swap(0, Ordering::AcqRel);
                self.frames_dropped.swap(0, Ordering::AcqRel);
                self.recv_coalesce_peak.swap(0, Ordering::AcqRel);
                self.recv_coalesced_frames.swap(0, Ordering::AcqRel);
                self.recv_conv_iter_peak_us.swap(0, Ordering::AcqRel);
                self.recv_pacing_sleep_sum_us.swap(0, Ordering::AcqRel);
            }
            if prefix == "sender" {
                self.sender_outgoing_queue_peak.swap(0, Ordering::AcqRel);
                self.sender_outgoing_queue_avg.swap(0, Ordering::AcqRel);
                self.sender_pacer_queue_peak.swap(0, Ordering::AcqRel);
                self.sender_encoded_pending_peak.swap(0, Ordering::AcqRel);
                self.sender_rtp_pending_peak.swap(0, Ordering::AcqRel);
            }
            return;
        }
        use std::fmt::Write;
        let mut buf = String::with_capacity(896);
        let _ = writeln!(buf, "[telemetry] {}:", prefix);
        if let Some(ref enc) = *self.encoder_type.read() {
            let _ = writeln!(buf, "  encoder:      {}", enc);
        }
        let _ = writeln!(
            buf,
            "  capture:      {}",
            Self::get_ms(self.capture_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  convert:      {}",
            Self::get_ms(self.convert_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  encode:       {}",
            Self::get_ms(self.encode_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  encode_avg:   {}",
            Self::get_ms(self.encode_avg_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  encode_peak:  {}",
            Self::get_ms(self.encode_max_us.swap(0, Ordering::AcqRel))
        );
        let _ = writeln!(
            buf,
            "  frames_drop:  {}",
            self.frames_dropped.swap(0, Ordering::AcqRel)
        );
        let _ = writeln!(
            buf,
            "  send:         {}",
            Self::get_ms(self.send_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  network:      {}",
            Self::get_ms(self.network_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  decode:       {}",
            Self::get_ms(self.decode_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  render:       {}",
            Self::get_ms(self.render_us.load(Ordering::Acquire))
        );
        let _ = writeln!(
            buf,
            "  gui_draw:     {}",
            Self::get_ms(self.gui_draw_us.load(Ordering::Acquire))
        );

        // Sender queue buildup diagnostics (LONG_STALL/NACK/PLI investigation).
        if prefix == "sender" {
            let out_cur = self.sender_outgoing_queue_cur.load(Ordering::Acquire);
            let out_peak = self.sender_outgoing_queue_peak.swap(0, Ordering::AcqRel);
            let out_avg = self.sender_outgoing_queue_avg.swap(0, Ordering::AcqRel);
            let pacer_cur = self.sender_pacer_queue_cur.load(Ordering::Acquire);
            let pacer_peak = self.sender_pacer_queue_peak.swap(0, Ordering::AcqRel);
            let enc_cur = self.sender_encoded_pending_cur.load(Ordering::Acquire);
            let enc_peak = self.sender_encoded_pending_peak.swap(0, Ordering::AcqRel);
            let rtp_cur = self.sender_rtp_pending_cur.load(Ordering::Acquire);
            let rtp_peak = self.sender_rtp_pending_peak.swap(0, Ordering::AcqRel);
            let _ = writeln!(buf, "  --- sender queue stats ---");
            let _ = writeln!(buf, "  out_queue:    {} (current), peak={}, avg={:.1}", out_cur, out_peak, out_avg as f64);
            let _ = writeln!(buf, "  pacer_queue:  {} (current), peak={}", pacer_cur, pacer_peak);
            let _ = writeln!(buf, "  encoded_pend: {} (current), peak={}", enc_cur, enc_peak);
            let _ = writeln!(buf, "  rtp_pending:  {} (current), peak={}", rtp_cur, rtp_peak);
        }

        if prefix == "receiver" {
            let elapsed_secs = 1.0;
            let frames = self.recv_frame_count.swap(0, Ordering::AcqRel);
            let stalls = self.recv_stall_count.swap(0, Ordering::AcqRel);
            let recv_fps = frames as f64 / elapsed_secs;
            let network_min = self.recv_network_min_us.load(Ordering::Acquire);
            let network_max = self.recv_network_max_us.load(Ordering::Acquire);
            let recv_wait_peak = self.recv_wait_max_us.load(Ordering::Acquire);
            self.reset_receiver_window();
            let _ = writeln!(buf, "  --- receiver stats (1s window) ---");
            let _ = writeln!(buf, "  recv_fps:     {:.1}", recv_fps);
            let _ = writeln!(
                buf,
                "  frame_delta:  {} (last)",
                Self::get_ms(self.recv_frame_delta_us.load(Ordering::Acquire))
            );
            let _ = writeln!(
                buf,
                "  expected:     {} (sender interval)",
                Self::get_ms(self.recv_expected_us.load(Ordering::Acquire))
            );
            let _ = writeln!(buf, "  network_min: {}", Self::get_ms(network_min));
            let _ = writeln!(buf, "  network_max: {}", Self::get_ms(network_max));
            let _ = writeln!(
                buf,
                "  network_avg: {}",
                Self::get_ms(self.recv_network_avg_us.load(Ordering::Acquire))
            );
            let _ = writeln!(
                buf,
                "  recv_wait:    {} (last stream.next)",
                Self::get_ms(self.recv_wait_us.load(Ordering::Acquire))
            );
            let _ = writeln!(
                buf,
                "  recv_wait_pk: {} (peak)",
                Self::get_ms(recv_wait_peak)
            );
            let _ = writeln!(buf, "  stall_count:  {}", stalls);
            let coalesce_pk = self.recv_coalesce_peak.swap(0, Ordering::AcqRel);
            let coalesced = self.recv_coalesced_frames.swap(0, Ordering::AcqRel);
            let conv_iter_pk = self.recv_conv_iter_peak_us.swap(0, Ordering::AcqRel);
            let pacing_sum = self.recv_pacing_sleep_sum_us.swap(0, Ordering::AcqRel);
            let _ = writeln!(
                buf,
                "  coalesce_pk: {} (max frames stacked in recv→conv channel)",
                coalesce_pk
            );
            let _ = writeln!(
                buf,
                "  coalesced_drop: {} (raw frames skipped, latest kept)",
                coalesced
            );
            let _ = writeln!(
                buf,
                "  conv_iter_pk: {} (peak recv→insert wall time)",
                Self::get_ms(conv_iter_pk)
            );
            let _ = writeln!(
                buf,
                "  pacing_sleep_sum: {} (converter sleep, 1s window)",
                Self::get_ms(pacing_sum)
            );
        }
        eprint!("{}", buf);
    }
}

/// RAII timer: records duration to telemetry on drop.
pub struct TelemetryTimer<'a> {
    start: Instant,
    target: &'a AtomicU64,
}

impl<'a> TelemetryTimer<'a> {
    pub fn new(target: &'a AtomicU64) -> Self {
        Self {
            start: Instant::now(),
            target,
        }
    }
}

impl Drop for TelemetryTimer<'_> {
    fn drop(&mut self) {
        self.target
            .store(self.start.elapsed().as_micros() as u64, Ordering::Release);
    }
}
