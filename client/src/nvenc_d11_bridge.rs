#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

#[cxx::bridge(namespace = "astrix_nvenc")]
pub mod ffi {
    extern "Rust" {
        fn nvenc_bridge_log_stderr(message: &str);
    }

    unsafe extern "C++" {
        include!("astrix/nvenc_d11_bridge.h");

        type NvencD3D11Session;

        fn nvenc_d3d11_create(
            d3d11_device: usize,
            width: u32,
            height: u32,
            fps: u32,
            bitrate: u32,
            texture_ptrs: Vec<usize>,
            gir_period_frames: u32,
            gir_duration_frames: u32,
        ) -> Result<UniquePtr<NvencD3D11Session>>;

        fn encoder_name(self: &NvencD3D11Session) -> String;
        fn is_async(self: &NvencD3D11Session) -> bool;
        fn input_ring_size(self: &NvencD3D11Session) -> u32;
        fn in_flight_count(self: &NvencD3D11Session) -> u32;
        fn collect(self: Pin<&mut NvencD3D11Session>, timeout_ms: u32) -> Result<Vec<u8>>;
        fn last_encode_time_us(self: &NvencD3D11Session) -> u64;
        fn last_submit_map_us(self: &NvencD3D11Session) -> u64;
        fn last_submit_encode_picture_us(self: &NvencD3D11Session) -> u64;
        fn last_submit_total_us(self: &NvencD3D11Session) -> u64;
        fn submit(
            self: Pin<&mut NvencD3D11Session>,
            texture_ptr: usize,
            force_idr: bool,
        ) -> Result<()>;
        fn set_bitrate(self: Pin<&mut NvencD3D11Session>, bitrate: u32) -> Result<()>;
    }
}

fn nvenc_bridge_log_stderr(message: &str) {
    crate::console_panel::log_error(message);
}
