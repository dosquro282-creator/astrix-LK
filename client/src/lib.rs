//! Astrix client library.

pub mod app;
pub mod bottom_panel;
pub mod channel_panel;
pub mod chat_panel;
pub mod components;
pub mod console_panel;
pub mod crypto;
pub mod deep_links;
pub mod denoise;
pub mod guild_panel;
pub mod member_panel;
pub mod net;
pub mod screen_encoder;
pub mod state;
pub mod telemetry;
pub mod theme;
pub mod todo_actions;
pub mod ui;
pub mod voice;
pub mod voice_livekit;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_gl_interop;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_i420;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_nv12;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_shared;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_rgba;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod dxgi_duplication;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod encoded_h264;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod gpu_device;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod mft_device;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod mft_encoder;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod nvenc_d11;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
mod nvenc_d11_bridge;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod windows_loopback;
