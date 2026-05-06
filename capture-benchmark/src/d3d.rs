use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIFactory1, IDXGIOutput, DXGI_OUTPUT_DESC};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

// Feature level constants
const FL_11_1: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL(0x0000b100);
const FL_11_0: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL(0x0000b000);
const FL_10_1: D3D_FEATURE_LEVEL = D3D_FEATURE_LEVEL(0x0000a100);

pub struct D3D11Setup {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: IDXGIAdapter,
    pub output: Option<IDXGIOutput>,
    pub output_desc: Option<DXGI_OUTPUT_DESC>,
    pub gpu_name: String,
}

impl D3D11Setup {
    pub fn with_monitor(monitor_index: usize) -> anyhow::Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }

        let (device, context, adapter) = Self::create_device()?;
        let gpu_name = Self::get_adapter_name(&adapter)?;

        println!("[D3D] Created device on adapter: {}", gpu_name);

        let output = Self::find_output(&adapter, monitor_index)?;
        let output_desc = unsafe { output.GetDesc() }?;

        let display_name = Self::wide_to_string(&output_desc.DeviceName);
        let width = output_desc.DesktopCoordinates.right - output_desc.DesktopCoordinates.left;
        let height = output_desc.DesktopCoordinates.bottom - output_desc.DesktopCoordinates.top;

        println!(
            "[D3D] Monitor {}: {} {}x{}",
            monitor_index, display_name, width, height
        );

        Ok(Self {
            device,
            context,
            adapter,
            output: Some(output),
            output_desc: Some(output_desc),
            gpu_name,
        })
    }

    fn create_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext, IDXGIAdapter)> {
        unsafe {
            let factory: IDXGIFactory1 = windows::Win32::Graphics::Dxgi::CreateDXGIFactory1()?;
            let adapter1 = factory.EnumAdapters1(0)?;
            let adapter: IDXGIAdapter = adapter1.cast()?;

            let feature_levels = [FL_11_1, FL_11_0, FL_10_1];
            let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

            let mut device: Option<ID3D11Device> = None;
            let mut feature_level = D3D_FEATURE_LEVEL(0);
            let mut context: Option<ID3D11DeviceContext> = None;

            D3D11CreateDevice(
                Some(&adapter),
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                flags,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut feature_level),
                Some(&mut context),
            )?;

            let device = device.ok_or_else(|| anyhow::anyhow!("Failed to create D3D11 device"))?;
            let context =
                context.ok_or_else(|| anyhow::anyhow!("Failed to create D3D11 context"))?;

            Ok((device, context, adapter))
        }
    }

    fn find_output(adapter: &IDXGIAdapter, monitor_index: usize) -> anyhow::Result<IDXGIOutput> {
        unsafe {
            let mut i = 0u32;

            loop {
                match adapter.EnumOutputs(i) {
                    Ok(output) => {
                        if i as usize == monitor_index {
                            return Ok(output);
                        }
                    }
                    Err(_) => {
                        break;
                    }
                }
                i += 1;
            }

            Err(anyhow::anyhow!("Monitor index {} not found", monitor_index))
        }
    }

    fn get_adapter_name(adapter: &IDXGIAdapter) -> anyhow::Result<String> {
        unsafe {
            let desc = adapter.GetDesc()?;
            Ok(Self::wide_to_string(&desc.Description))
        }
    }

    fn wide_to_string(wide: &[u16]) -> String {
        let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        String::from_utf16_lossy(&wide[..len])
    }

    pub fn get_monitor_info(&self) -> String {
        if let Some(ref desc) = self.output_desc {
            let display_name = Self::wide_to_string(&desc.DeviceName);
            let width = desc.DesktopCoordinates.right - desc.DesktopCoordinates.left;
            let height = desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top;
            format!("{} {}x{}", display_name, width, height)
        } else {
            "Unknown".to_string()
        }
    }

    pub fn flush(&self) {
        unsafe {
            self.context.Flush();
        }
    }
}

pub fn get_all_monitors() -> anyhow::Result<Vec<String>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let (device, context, adapter) = D3D11Setup::create_device()?;
    let gpu_name = D3D11Setup::get_adapter_name(&adapter)?;
    let _ = (device, context);

    let mut monitors = Vec::new();
    let mut i = 0u32;

    loop {
        match D3D11Setup::find_output(&adapter, i as usize) {
            Ok(output) => {
                let desc = unsafe { output.GetDesc() }?;
                let display_name = D3D11Setup::wide_to_string(&desc.DeviceName);
                let width = desc.DesktopCoordinates.right - desc.DesktopCoordinates.left;
                let height = desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top;

                monitors.push(format!(
                    "[{}] {} {}x{} ({})",
                    i, display_name, width, height, gpu_name
                ));
            }
            Err(_) => {
                break;
            }
        }
        i += 1;
    }

    Ok(monitors)
}

pub fn get_monitor_rect(monitor_index: usize) -> anyhow::Result<(i32, i32, i32, i32)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }

    let (device, context, adapter) = D3D11Setup::create_device()?;
    let _ = (device, context);

    let output = D3D11Setup::find_output(&adapter, monitor_index)?;
    let desc = unsafe { output.GetDesc() }?;

    Ok((
        desc.DesktopCoordinates.left,
        desc.DesktopCoordinates.top,
        desc.DesktopCoordinates.right,
        desc.DesktopCoordinates.bottom,
    ))
}
