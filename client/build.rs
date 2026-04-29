use std::env;
use std::path::PathBuf;

fn main() {
    // Set Windows subsystem to GUI (no console window) in release builds
    if cfg!(target_os = "windows") && !cfg!(debug_assertions) {
        println!("cargo:rustc-link-arg=/SUBSYSTEM:WINDOWS");
        println!("cargo:rustc-link-arg=/ENTRY:mainCRTStartup");
    }

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

    cxx_build::bridge("src/bin/nvenc_d11_bridge.rs")
        .file("src/nvenc_d11_bridge.cpp")
        .include("include")
        .include(&nvcodec_include)
        .flag("/std:c++20")
        .flag("/EHsc")
        .flag("/MD")
        .warnings(false)
        .compile("astrix_nvenc_d11_probe");
    println!("cargo:rustc-link-lib=static=astrix_nvenc_d11_probe");

    println!("cargo:rerun-if-changed=src/nvenc_d11_bridge.rs");
    println!("cargo:rerun-if-changed=src/nvenc_d11_bridge.cpp");
    println!("cargo:rerun-if-changed=include/astrix/nvenc_d11_bridge.h");
    println!("cargo:rerun-if-changed=src/bin/nvenc_d11_bridge.rs");
}
