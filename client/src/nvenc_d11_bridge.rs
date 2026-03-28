#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

#[cxx::bridge(namespace = "astrix_nvenc")]
pub mod ffi {
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
        ) -> Result<UniquePtr<NvencD3D11Session>>;

        fn encoder_name(self: &NvencD3D11Session) -> String;
        fn is_async(self: &NvencD3D11Session) -> bool;
        fn input_ring_size(self: &NvencD3D11Session) -> u32;
        fn in_flight_count(self: &NvencD3D11Session) -> u32;
        fn collect(self: Pin<&mut NvencD3D11Session>, timeout_ms: u32) -> Result<Vec<u8>>;
        fn last_encode_time_us(self: &NvencD3D11Session) -> u64;
        fn submit(
            self: Pin<&mut NvencD3D11Session>,
            texture_ptr: usize,
            force_idr: bool,
        ) -> Result<()>;
        fn set_bitrate(self: Pin<&mut NvencD3D11Session>, bitrate: u32) -> Result<()>;
    }
}
