use crate::cli::OverlayMode;
use crate::foreground;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateSolidBrush, DeleteObject, FillRgn, GetDC, ReleaseDC,
    SetWindowRgn, HBRUSH, HDC, HRGN, RGN_DIFF,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, RegisterClassExW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, GWL_EXSTYLE, HTTRANSPARENT, HWND_TOPMOST,
    LWA_ALPHA, SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_SHOWNOACTIVATE, WM_NCHITTEST, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

const BORDER_WIDTH: i32 = 2;
const BORDER_COLOR: u32 = 0x0000FF00;

#[derive(Debug, Clone)]
pub struct OverlayStatus {
    pub mode: OverlayMode,
    pub created: bool,
    pub hwnd: Option<usize>,
    pub rect: (i32, i32, i32, i32),
    pub topmost: bool,
    pub noactivate: bool,
    pub clickthrough: bool,
    pub foreground_unchanged: bool,
}

impl OverlayStatus {
    pub fn off() -> Self {
        Self {
            mode: OverlayMode::Off,
            created: false,
            hwnd: None,
            rect: (0, 0, 0, 0),
            topmost: false,
            noactivate: false,
            clickthrough: false,
            foreground_unchanged: true,
        }
    }
}

pub struct CompatibilityOverlay {
    hwnd: HWND,
}

impl CompatibilityOverlay {
    pub fn create(
        mode: OverlayMode,
        monitor_rect: (i32, i32, i32, i32),
    ) -> (Option<Self>, OverlayStatus) {
        if mode == OverlayMode::Off {
            let status = OverlayStatus::off();
            print_status(&status);
            return (None, status);
        }

        let foreground_before = foreground::current_foreground_hwnd();
        let result = unsafe { create_overlay_window(mode, monitor_rect) };
        let foreground_after = foreground::current_foreground_hwnd();
        let foreground_unchanged = foreground_before == foreground_after;

        match result {
            Ok(hwnd) => {
                let exstyle = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
                let status = OverlayStatus {
                    mode,
                    created: true,
                    hwnd: Some(hwnd.0 as usize),
                    rect: overlay_rect(mode, monitor_rect),
                    topmost: (exstyle & WS_EX_TOPMOST.0) != 0,
                    noactivate: (exstyle & WS_EX_NOACTIVATE.0) != 0,
                    clickthrough: (exstyle & WS_EX_TRANSPARENT.0) != 0,
                    foreground_unchanged,
                };
                print_status(&status);
                (Some(Self { hwnd }), status)
            }
            Err(error) => {
                let status = OverlayStatus {
                    mode,
                    created: false,
                    hwnd: None,
                    rect: overlay_rect(mode, monitor_rect),
                    topmost: false,
                    noactivate: false,
                    clickthrough: false,
                    foreground_unchanged,
                };
                println!("[overlay] created=false error=\"{}\"", error);
                print_status(&status);
                (None, status)
            }
        }
    }
}

impl Drop for CompatibilityOverlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe fn create_overlay_window(
    mode: OverlayMode,
    monitor_rect: (i32, i32, i32, i32),
) -> anyhow::Result<HWND> {
    let class_name: Vec<u16> = "AstrixBenchCompatibilityOverlay\0".encode_utf16().collect();
    let hinstance = GetModuleHandleW(None)?;
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinstance.into(),
        hIcon: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
        hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
    };
    let _ = RegisterClassExW(&wc);

    let rect = overlay_rect(mode, monitor_rect);
    let width = rect.2 - rect.0;
    let height = rect.3 - rect.1;
    let window_name: Vec<u16> = format!("AstrixBenchOverlay-{}\0", mode.as_str())
        .encode_utf16()
        .collect();
    let ex_style =
        WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_LAYERED;

    let hwnd = CreateWindowExW(
        ex_style,
        PCWSTR(class_name.as_ptr()),
        PCWSTR(window_name.as_ptr()),
        WS_POPUP,
        rect.0,
        rect.1,
        width,
        height,
        HWND::default(),
        None,
        hinstance,
        None,
    )?;

    let alpha = match mode {
        OverlayMode::Transparent | OverlayMode::Tiny => 1,
        OverlayMode::VisibleBorder => 255,
        OverlayMode::Off => 0,
    };
    SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA)?;

    if mode == OverlayMode::VisibleBorder {
        apply_border_region(hwnd, width, height);
    }

    SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        rect.0,
        rect.1,
        width,
        height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    )?;
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

    if mode == OverlayMode::VisibleBorder {
        paint_border(hwnd, width, height);
    }

    Ok(hwnd)
}

fn overlay_rect(mode: OverlayMode, monitor_rect: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    match mode {
        OverlayMode::Tiny => (
            monitor_rect.0,
            monitor_rect.1,
            monitor_rect.0 + 8,
            monitor_rect.1 + 8,
        ),
        OverlayMode::Transparent | OverlayMode::VisibleBorder => monitor_rect,
        OverlayMode::Off => (0, 0, 0, 0),
    }
}

unsafe fn apply_border_region(hwnd: HWND, width: i32, height: i32) {
    let outer: HRGN = CreateRectRgn(0, 0, width, height);
    let inner: HRGN = CreateRectRgn(
        BORDER_WIDTH,
        BORDER_WIDTH,
        width - BORDER_WIDTH,
        height - BORDER_WIDTH,
    );
    let border_region: HRGN = CreateRectRgn(0, 0, 0, 0);
    let _ = CombineRgn(border_region, outer, inner, RGN_DIFF);
    let _ = DeleteObject::<HRGN>(outer);
    let _ = DeleteObject::<HRGN>(inner);
    let _ = SetWindowRgn(hwnd, border_region, true);
}

unsafe fn paint_border(hwnd: HWND, width: i32, height: i32) {
    let hdc: HDC = GetDC(hwnd);
    if hdc.is_invalid() {
        return;
    }
    let brush: HBRUSH = CreateSolidBrush(COLORREF(BORDER_COLOR));

    let regions = [
        CreateRectRgn(0, 0, width, BORDER_WIDTH),
        CreateRectRgn(0, height - BORDER_WIDTH, width, height),
        CreateRectRgn(0, BORDER_WIDTH, BORDER_WIDTH, height - BORDER_WIDTH),
        CreateRectRgn(
            width - BORDER_WIDTH,
            BORDER_WIDTH,
            width,
            height - BORDER_WIDTH,
        ),
    ];

    for region in regions {
        let _ = FillRgn(hdc, region, brush);
        let _ = DeleteObject::<HRGN>(region);
    }

    let _ = DeleteObject(brush);
    let _ = ReleaseDC(hwnd, hdc);
}

fn print_status(status: &OverlayStatus) {
    println!("[overlay] mode={}", status.mode.as_str());
    println!("[overlay] created={}", status.created);
    if let Some(hwnd) = status.hwnd {
        println!("[overlay] hwnd=0x{:X}", hwnd);
    }
    println!(
        "[overlay] rect=({},{} {}x{})",
        status.rect.0,
        status.rect.1,
        status.rect.2 - status.rect.0,
        status.rect.3 - status.rect.1
    );
    println!("[overlay] topmost={}", status.topmost);
    println!("[overlay] noactivate={}", status.noactivate);
    println!("[overlay] clickthrough={}", status.clickthrough);
    println!(
        "[overlay] foreground_unchanged={}",
        status.foreground_unchanged
    );
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}
