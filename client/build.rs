use std::env;
use std::path::PathBuf;

fn main() {
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    if env::var("CARGO_FEATURE_WGC_CAPTURE").is_err() {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let nvcodec_include = manifest_dir
        .join("vendor")
        .join("rust-sdks")
        .join("webrtc-sys")
        .join("src")
        .join("nvidia")
        .join("NvCodec")
        .join("include");

    cxx_build::bridge("src/nvenc_d11_bridge.rs")
        .file("src/nvenc_d11_bridge.cpp")
        .include("include")
        .include(&nvcodec_include)
        .flag("/std:c++20")
        .flag("/EHsc")
        .warnings(false)
        .compile("astrix-nvenc-d11");

    println!("cargo:rerun-if-changed=src/nvenc_d11_bridge.rs");
    println!("cargo:rerun-if-changed=src/nvenc_d11_bridge.cpp");
    println!("cargo:rerun-if-changed=include/astrix/nvenc_d11_bridge.h");
}
