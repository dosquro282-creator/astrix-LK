# Diff Details

Date : 2026-03-11 21:58:06

Directory c:\\MyProjects\\Astrix

Total : 35 files,  2223 codes, 439 comments, 421 blanks, all 3083 lines

[Summary](results.md) / [Details](details.md) / [Diff Summary](diff.md) / Diff Details

## Files
| filename | language | code | comment | blank | total |
| :--- | :--- | ---: | ---: | ---: | ---: |
| [.claude/settings.local.json](/.claude/settings.local.json) | JSON | 16 | 0 | 0 | 16 |
| [antilag-bitrate-assessment.md](/antilag-bitrate-assessment.md) | Markdown | 110 | 0 | 66 | 176 |
| [antilag-wgc-encoder-flow.md](/antilag-wgc-encoder-flow.md) | Markdown | 89 | 0 | 49 | 138 |
| [antilag.md](/antilag.md) | Markdown | 136 | 0 | 61 | 197 |
| [client/astrix\_settings.json](/client/astrix_settings.json) | JSON | 1 | 0 | 0 | 1 |
| [client/nv12\_to\_rgba.hlsl](/client/nv12_to_rgba.hlsl) | HLSL | 69 | 11 | 12 | 92 |
| [client/src/app.rs](/client/src/app.rs) | Rust | 48 | 23 | 4 | 75 |
| [client/src/d3d11\_gl\_interop.rs](/client/src/d3d11_gl_interop.rs) | Rust | 206 | 89 | 36 | 331 |
| [client/src/d3d11\_nv12.rs](/client/src/d3d11_nv12.rs) | Rust | 8 | 4 | 0 | 12 |
| [client/src/d3d11\_rgba.rs](/client/src/d3d11_rgba.rs) | Rust | 590 | 73 | 41 | 704 |
| [client/src/lib.rs](/client/src/lib.rs) | Rust | 2 | 0 | 0 | 2 |
| [client/src/mft\_device.rs](/client/src/mft_device.rs) | Rust | 5 | 9 | 1 | 15 |
| [client/src/mft\_encoder.rs](/client/src/mft_encoder.rs) | Rust | 10 | 10 | 3 | 23 |
| [client/src/ui.rs](/client/src/ui.rs) | Rust | 205 | 48 | 12 | 265 |
| [client/src/voice.rs](/client/src/voice.rs) | Rust | 1 | 2 | 0 | 3 |
| [client/src/voice\_livekit.rs](/client/src/voice_livekit.rs) | Rust | 178 | 47 | 10 | 235 |
| [client/vendor/rust-sdks/libwebrtc/src/native/video\_frame.rs](/client/vendor/rust-sdks/libwebrtc/src/native/video_frame.rs) | Rust | 4 | 1 | 1 | 6 |
| [client/vendor/rust-sdks/libwebrtc/src/native/video\_source.rs](/client/vendor/rust-sdks/libwebrtc/src/native/video_source.rs) | Rust | 7 | 4 | 0 | 11 |
| [client/vendor/rust-sdks/libwebrtc/src/video\_frame.rs](/client/vendor/rust-sdks/libwebrtc/src/video_frame.rs) | Rust | 6 | 1 | 1 | 8 |
| [client/vendor/rust-sdks/libwebrtc/src/video\_source.rs](/client/vendor/rust-sdks/libwebrtc/src/video_source.rs) | Rust | 6 | 1 | 0 | 7 |
| [client/vendor/rust-sdks/webrtc-sys/build.rs](/client/vendor/rust-sdks/webrtc-sys/build.rs) | Rust | 1 | 0 | 0 | 1 |
| [client/vendor/rust-sdks/webrtc-sys/include/livekit/video\_frame\_buffer.h](/client/vendor/rust-sdks/webrtc-sys/include/livekit/video_frame_buffer.h) | C++ | 6 | 4 | 3 | 13 |
| [client/vendor/rust-sdks/webrtc-sys/include/livekit/video\_track.h](/client/vendor/rust-sdks/webrtc-sys/include/livekit/video_track.h) | C++ | 1 | 2 | 0 | 3 |
| [client/vendor/rust-sdks/webrtc-sys/src/mft/d3d11\_texture\_video\_frame\_buffer.cpp](/client/vendor/rust-sdks/webrtc-sys/src/mft/d3d11_texture_video_frame_buffer.cpp) | C++ | 133 | 27 | 26 | 186 |
| [client/vendor/rust-sdks/webrtc-sys/src/mft/d3d11\_texture\_video\_frame\_buffer.h](/client/vendor/rust-sdks/webrtc-sys/src/mft/d3d11_texture_video_frame_buffer.h) | C++ | 41 | 34 | 14 | 89 |
| [client/vendor/rust-sdks/webrtc-sys/src/mft/mft\_decoder\_factory.cpp](/client/vendor/rust-sdks/webrtc-sys/src/mft/mft_decoder_factory.cpp) | C++ | 15 | 0 | 1 | 16 |
| [client/vendor/rust-sdks/webrtc-sys/src/mft/mft\_h264\_decoder\_impl.cpp](/client/vendor/rust-sdks/webrtc-sys/src/mft/mft_h264_decoder_impl.cpp) | C++ | 80 | 29 | 5 | 114 |
| [client/vendor/rust-sdks/webrtc-sys/src/video\_decoder\_factory.cpp](/client/vendor/rust-sdks/webrtc-sys/src/video_decoder_factory.cpp) | C++ | 0 | 3 | 0 | 3 |
| [client/vendor/rust-sdks/webrtc-sys/src/video\_frame\_buffer.cpp](/client/vendor/rust-sdks/webrtc-sys/src/video_frame_buffer.cpp) | C++ | 47 | 0 | 11 | 58 |
| [client/vendor/rust-sdks/webrtc-sys/src/video\_frame\_buffer.rs](/client/vendor/rust-sdks/webrtc-sys/src/video_frame_buffer.rs) | Rust | 22 | 8 | 5 | 35 |
| [client/vendor/rust-sdks/webrtc-sys/src/video\_track.cpp](/client/vendor/rust-sdks/webrtc-sys/src/video_track.cpp) | C++ | 6 | 4 | 1 | 11 |
| [client/vendor/rust-sdks/webrtc-sys/src/video\_track.rs](/client/vendor/rust-sdks/webrtc-sys/src/video_track.rs) | Rust | 1 | 0 | 0 | 1 |
| [livekit.yaml](/livekit.yaml) | YAML | 4 | 2 | 0 | 6 |
| [new\_path\_deco.md](/new_path_deco.md) | Markdown | 116 | 0 | 56 | 172 |
| [server/internal/users/handlers.go](/server/internal/users/handlers.go) | Go | 53 | 3 | 2 | 58 |

[Summary](results.md) / [Details](details.md) / [Diff Summary](diff.md) / Diff Details