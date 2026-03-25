//! Astrix client library.

pub mod app;
pub mod bottom_panel;
pub mod channel_panel;
pub mod chat_panel;
pub mod components;
pub mod guild_panel;
pub mod member_panel;
pub mod state;
pub mod theme;
pub mod ui;
pub mod net;
pub mod crypto;
pub mod voice;
pub mod voice_livekit;
pub mod screen_encoder;
pub mod telemetry;

#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_i420;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_rgba;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_nv12;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod gpu_device;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod mft_encoder;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod mft_device;
#[cfg(all(target_os = "windows", feature = "wgc-capture"))]
pub mod d3d11_gl_interop;
