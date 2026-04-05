#![cfg(all(target_os = "windows", feature = "wgc-capture"))]

use thiserror::Error;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
    DXGI_OUTPUT_DESC,
};
use xcap::Monitor;

use crate::gpu_device::{GpuDevice, GpuDeviceError};

#[derive(Error, Debug)]
pub enum DxgiDuplicationError {
    #[error("No monitors found")]
    NoMonitors,
    #[error("Monitor index {0} out of range (max {1})")]
    InvalidMonitorIndex(usize, usize),
    #[error("DXGI output not found for monitor {0} ({1})")]
    OutputNotFound(usize, String),
    #[error("DXGI duplication access lost")]
    AccessLost,
    #[error("Monitor enumeration failed: {0}")]
    Monitor(String),
    #[error("GPU device error: {0}")]
    GpuDevice(#[from] GpuDeviceError),
    #[error("Windows API error: {0}")]
    Win(#[from] windows::core::Error),
}

#[derive(Debug, Clone)]
pub struct OutputSelection {
    pub monitor_idx: usize,
    pub monitor_name: String,
    pub adapter_idx: u32,
    pub adapter_name: String,
    pub output_idx: u32,
    pub output_name: String,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub width: u32,
    pub height: u32,
}

struct SelectedOutput {
    output: IDXGIOutput1,
    selection: OutputSelection,
}

struct OutputCandidate {
    adapter_idx: u32,
    adapter_name: String,
    output_idx: u32,
    output: IDXGIOutput1,
    output_desc: DXGI_OUTPUT_DESC,
}

pub struct DxgiDuplicationCapture {
    duplication: IDXGIOutputDuplication,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub selection: OutputSelection,
}

pub struct AcquiredDesktopFrame {
    duplication: IDXGIOutputDuplication,
    resource: Option<IDXGIResource>,
    released: bool,
    pub info: DXGI_OUTDUPL_FRAME_INFO,
    pub dirty_rects: Vec<RECT>,
    pub move_rects: Vec<DXGI_OUTDUPL_MOVE_RECT>,
}

impl AcquiredDesktopFrame {
    pub fn texture(&self) -> Result<ID3D11Texture2D, windows::core::Error> {
        let resource = self
            .resource
            .as_ref()
            .ok_or_else(|| windows::core::Error::from(DXGI_ERROR_INVALID_CALL))?;
        resource.cast()
    }

    pub fn release(&mut self) -> Result<(), windows::core::Error> {
        if !self.released {
            unsafe { self.duplication.ReleaseFrame()? };
            self.resource = None;
            self.released = true;
        }
        Ok(())
    }
}

impl Drop for AcquiredDesktopFrame {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

impl DxgiDuplicationCapture {
    pub fn new(screen_index: Option<usize>) -> Result<Self, DxgiDuplicationError> {
        let selected = select_output(screen_index)?;
        let gpu = GpuDevice::create_for_adapter_idx(selected.selection.adapter_idx)?;
        let duplication = unsafe { selected.output.DuplicateOutput(&gpu.device)? };

        eprintln!(
            "[voice][screen] DXGI capturing monitor {} '{}' via adapter {} '{}' / output {} '{}' ({}x{} at {},{})",
            selected.selection.monitor_idx,
            selected.selection.monitor_name,
            selected.selection.adapter_idx,
            selected.selection.adapter_name,
            selected.selection.output_idx,
            selected.selection.output_name,
            selected.selection.width,
            selected.selection.height,
            selected.selection.desktop_x,
            selected.selection.desktop_y,
        );

        Ok(Self {
            duplication,
            device: gpu.device,
            context: gpu.context,
            selection: selected.selection,
        })
    }

    pub fn acquire_next_frame(
        &self,
        timeout_ms: u32,
    ) -> Result<Option<AcquiredDesktopFrame>, DxgiDuplicationError> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {
                let (dirty_rects, move_rects) =
                    collect_frame_metadata(&self.duplication, &info)?;
                Ok(Some(AcquiredDesktopFrame {
                    duplication: self.duplication.clone(),
                    resource,
                    released: false,
                    info,
                    dirty_rects,
                    move_rects,
                }))
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => Err(DxgiDuplicationError::AccessLost),
            Err(e) => Err(e.into()),
        }
    }
}

fn collect_frame_metadata(
    duplication: &IDXGIOutputDuplication,
    info: &DXGI_OUTDUPL_FRAME_INFO,
) -> Result<(Vec<RECT>, Vec<DXGI_OUTDUPL_MOVE_RECT>), windows::core::Error> {
    let metadata_capacity = info.TotalMetadataBufferSize;
    if metadata_capacity == 0 {
        return Ok((Vec::new(), Vec::new()));
    }

    let move_rect_capacity =
        (metadata_capacity as usize) / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>();
    let mut move_rects = vec![DXGI_OUTDUPL_MOVE_RECT::default(); move_rect_capacity];
    let mut move_rect_bytes = 0u32;
    unsafe {
        duplication.GetFrameMoveRects(
            metadata_capacity,
            move_rects.as_mut_ptr(),
            &mut move_rect_bytes,
        )?;
    }
    move_rects.truncate(
        (move_rect_bytes as usize) / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>(),
    );

    let dirty_rect_capacity = (metadata_capacity as usize) / std::mem::size_of::<RECT>();
    let mut dirty_rects = vec![RECT::default(); dirty_rect_capacity];
    let mut dirty_rect_bytes = 0u32;
    unsafe {
        duplication.GetFrameDirtyRects(
            metadata_capacity,
            dirty_rects.as_mut_ptr(),
            &mut dirty_rect_bytes,
        )?;
    }
    dirty_rects.truncate((dirty_rect_bytes as usize) / std::mem::size_of::<RECT>());

    Ok((dirty_rects, move_rects))
}

impl OutputCandidate {
    fn into_selected(self, monitor_idx: usize, monitor_name: String) -> SelectedOutput {
        let rect = self.output_desc.DesktopCoordinates;
        SelectedOutput {
            output: self.output,
            selection: OutputSelection {
                monitor_idx,
                monitor_name,
                adapter_idx: self.adapter_idx,
                adapter_name: self.adapter_name,
                output_idx: self.output_idx,
                output_name: wide_to_string(&self.output_desc.DeviceName),
                desktop_x: rect.left,
                desktop_y: rect.top,
                width: (rect.right - rect.left).max(0) as u32,
                height: (rect.bottom - rect.top).max(0) as u32,
            },
        }
    }
}

fn select_output(screen_index: Option<usize>) -> Result<SelectedOutput, DxgiDuplicationError> {
    let monitors = Monitor::all().map_err(|e| DxgiDuplicationError::Monitor(e.to_string()))?;
    if monitors.is_empty() {
        return Err(DxgiDuplicationError::NoMonitors);
    }

    let idx = screen_index.unwrap_or(0);
    if idx >= monitors.len() {
        return Err(DxgiDuplicationError::InvalidMonitorIndex(
            idx,
            monitors.len() - 1,
        ));
    }

    let target = &monitors[idx];
    let target_name = target.name().to_string();
    let target_x = target.x();
    let target_y = target.y();
    let target_w = target.width();
    let target_h = target.height();

    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut outputs = Vec::new();
    let mut adapter_idx = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(adapter) => adapter,
            Err(_) => break,
        };
        let adapter_desc = unsafe { adapter.GetDesc1()? };
        let adapter_name = wide_to_string(&adapter_desc.Description);

        let mut output_idx = 0u32;
        loop {
            let output: IDXGIOutput = match unsafe { adapter.EnumOutputs(output_idx) } {
                Ok(output) => output,
                Err(_) => break,
            };
            let output1: IDXGIOutput1 = output.cast()?;
            let output_desc = unsafe { output.GetDesc()? };
            outputs.push(OutputCandidate {
                adapter_idx,
                adapter_name: adapter_name.clone(),
                output_idx,
                output: output1,
                output_desc,
            });
            output_idx += 1;
        }

        adapter_idx += 1;
    }

    if let Some(pos) = outputs.iter().position(|candidate| {
        let rect = candidate.output_desc.DesktopCoordinates;
        candidate.output_desc.AttachedToDesktop.as_bool()
            && rect.left == target_x
            && rect.top == target_y
            && (rect.right - rect.left) as u32 == target_w
            && (rect.bottom - rect.top) as u32 == target_h
    }) {
        return Ok(outputs
            .swap_remove(pos)
            .into_selected(idx, target_name.clone()));
    }

    let attached_positions: Vec<usize> = outputs
        .iter()
        .enumerate()
        .filter_map(|(pos, candidate)| {
            candidate
                .output_desc
                .AttachedToDesktop
                .as_bool()
                .then_some(pos)
        })
        .collect();
    if idx < attached_positions.len() {
        return Ok(outputs
            .swap_remove(attached_positions[idx])
            .into_selected(idx, target_name.clone()));
    }

    Err(DxgiDuplicationError::OutputNotFound(
        idx,
        format!(
            "{} {}x{} at {},{}",
            target_name, target_w, target_h, target_x, target_y
        ),
    ))
}

fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end]).trim().to_string()
}
