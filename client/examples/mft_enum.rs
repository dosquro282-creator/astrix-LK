//! Phase 0.3: Enumerate MFT H.264 encoders via MFTEnumEx.
//! Verifies MFVideoFormat_NV12 input is available on target machine.
//!
//! Run: cargo run --example mft_enum

#![cfg(windows)]

use std::ptr;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SYNCMFT,
    MFT_REGISTER_TYPE_INFO,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    println!("=== MFT H.264 Encoder Enumeration (Phase 0.3) ===\n");

    let input_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_NV12.into(),
    };
    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video.into(),
        guidSubtype: MFVideoFormat_H264.into(),
    };

    // Try hardware first
    println!("--- Hardware MFT encoders (NV12 -> H.264) ---");
    let count = unsafe { enumerate_mfts(MFT_ENUM_FLAG_HARDWARE, &input_type, &output_type) }?;
    println!("Found {} hardware encoder(s)\n", count);

    // Fallback: software MFT
    println!("--- Software MFT encoders (NV12 -> H.264) ---");
    let count_sw = unsafe { enumerate_mfts(MFT_ENUM_FLAG_SYNCMFT, &input_type, &output_type) }?;
    println!("Found {} software encoder(s)\n", count_sw);

    if count > 0 || count_sw > 0 {
        println!("OK: MFVideoFormat_NV12 input is available on this machine.");
    } else {
        println!("WARNING: No NV12->H.264 MFT encoders found.");
    }

    Ok(())
}

unsafe fn enumerate_mfts(
    flags: windows::Win32::Media::MediaFoundation::MFT_ENUM_FLAG,
    input_type: &MFT_REGISTER_TYPE_INFO,
    output_type: &MFT_REGISTER_TYPE_INFO,
) -> Result<u32, windows::core::Error> {
    let mut activates: *mut Option<IMFActivate> = ptr::null_mut();
    let mut count: u32 = 0;

    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        Some(input_type as *const _),
        Some(output_type as *const _),
        &mut activates,
        &mut count,
    )?;

    if count == 0 {
        return Ok(0);
    }

    let activates_slice = std::slice::from_raw_parts(activates, count as usize);
    for (i, act_opt) in activates_slice.iter().enumerate() {
        if let Some(act) = act_opt {
            let name = get_friendly_name(act).unwrap_or_else(|_| "?".to_string());
            let is_hw = (flags.0 & MFT_ENUM_FLAG_HARDWARE.0) != 0;
            println!(
                "  [{}] {} ({})",
                i,
                name,
                if is_hw { "hardware" } else { "software" }
            );
        }
    }

    // Free activates (Release each, then free the array)
    for act_opt in activates_slice {
        if let Some(act) = act_opt {
            let _ = act.ShutdownObject();
        }
    }
    CoTaskMemFree(Some(activates as *const _ as *mut _));

    Ok(count)
}

fn get_friendly_name(activate: &IMFActivate) -> Result<String, windows::core::Error> {
    use windows::Win32::Media::MediaFoundation::MFT_FRIENDLY_NAME_Attribute;

    unsafe {
        let mut buf = [0u16; 256];
        let attrs: &windows::Win32::Media::MediaFoundation::IMFAttributes = activate;
        attrs.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None)?;
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(String::from_utf16_lossy(&buf[..len]))
    }
}
