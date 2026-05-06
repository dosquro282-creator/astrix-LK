use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateSolidBrush, DeleteObject, FillRgn, GetDC, ReleaseDC,
    SelectObject, SetWindowRgn, HBRUSH, HDC, HPEN, HRGN, PS_SOLID, RGN_DIFF,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, RegisterClassExW, SetWindowPos, ShowWindow, HWND_TOPMOST, MSG, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOW, WM_DESTROY, WNDCLASSEXW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

const BORDER_WIDTH: i32 = 6;
const BORDER_COLOR: u32 = 0x00FFFF00; // Yellow (0x00BBGGRR format)

static QUIT_FLAG: AtomicBool = AtomicBool::new(false);

pub struct BorderOverlay {
    hwnd: HWND,
    hdc: HDC,
}

impl BorderOverlay {
    pub fn new() -> Self {
        unsafe {
            // Register window class
            let class_name: Vec<u16> = "BorderOverlayClass\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(None).unwrap();

            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: windows::Win32::UI::WindowsAndMessaging::WNDCLASS_STYLES(0),
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance.into(),
                hIcon: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
                hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR::default(),
                hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                hIconSm: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
            };
            let _ = RegisterClassExW(&wc);

            // Create hidden border window initially
            let window_name: Vec<u16> = "BorderOverlay\0".encode_utf16().collect();
            let ex_style = WS_EX_TOPMOST | WS_EX_TOOLWINDOW;
            let style = WS_POPUP;

            let hinstance_win: windows::Win32::Foundation::HINSTANCE = hinstance.into();

            let hwnd = CreateWindowExW(
                ex_style,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(window_name.as_ptr()),
                style,
                0,
                0,
                100,
                100,
                HWND::default(),
                None,
                hinstance_win,
                None,
            )
            .unwrap();

            // Get DC for painting
            let hdc = GetDC(hwnd);

            Self { hwnd, hdc }
        }
    }

    pub fn show_border(&self, left: i32, top: i32, right: i32, bottom: i32) {
        unsafe {
            let width = right - left;
            let height = bottom - top;

            // Create border region: outer rectangle minus inner rectangle
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

            // Set the window region (creates the border shape)
            let _ = SetWindowRgn(self.hwnd, border_region, true);

            // Move and resize window
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                left,
                top,
                width,
                height,
                SWP_NOACTIVATE,
            );

            // Show window
            ShowWindow(self.hwnd, SW_SHOW);
            SetWindowPos(
                self.hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );

            // Paint the border
            self.paint_border(width, height);
        }
    }

    pub fn hide_border(&self, _left: i32, _top: i32, _right: i32, _bottom: i32) {
        unsafe {
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    unsafe fn paint_border(&self, width: i32, height: i32) {
        let brush: HBRUSH = CreateSolidBrush(windows::Win32::Foundation::COLORREF(BORDER_COLOR));
        let pen: HPEN = windows::Win32::Graphics::Gdi::CreatePen(
            PS_SOLID,
            BORDER_WIDTH,
            windows::Win32::Foundation::COLORREF(BORDER_COLOR),
        );

        if !pen.is_invalid() {
            let _old_brush = SelectObject(self.hdc, brush);
            let _old_pen = SelectObject(self.hdc, pen);

            // Draw filled rectangles for each border edge
            // Top border
            let top_rgn: HRGN = CreateRectRgn(0, 0, width, BORDER_WIDTH);
            FillRgn(self.hdc, top_rgn, brush);
            let _ = DeleteObject::<HRGN>(top_rgn);

            // Bottom border
            let bottom_rgn: HRGN = CreateRectRgn(0, height - BORDER_WIDTH, width, height);
            FillRgn(self.hdc, bottom_rgn, brush);
            let _ = DeleteObject::<HRGN>(bottom_rgn);

            // Left border
            let left_rgn: HRGN =
                CreateRectRgn(0, BORDER_WIDTH, BORDER_WIDTH, height - BORDER_WIDTH);
            FillRgn(self.hdc, left_rgn, brush);
            let _ = DeleteObject::<HRGN>(left_rgn);

            // Right border
            let right_rgn: HRGN = CreateRectRgn(
                width - BORDER_WIDTH,
                BORDER_WIDTH,
                width,
                height - BORDER_WIDTH,
            );
            FillRgn(self.hdc, right_rgn, brush);
            let _ = DeleteObject::<HRGN>(right_rgn);

            let _ = DeleteObject(pen);
        }

        let _ = DeleteObject(brush);
    }
}

impl Default for BorderOverlay {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BorderOverlay {
    fn drop(&mut self) {
        unsafe {
            if !self.hdc.is_invalid() {
                let _ = ReleaseDC(self.hwnd, self.hdc);
            }
            windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

// Window procedure for message handling
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    match msg {
        WM_DESTROY => {
            QUIT_FLAG.store(true, Ordering::SeqCst);
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
        }
        _ => {}
    }
    windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Run the message pump - call this in a separate thread to handle window messages
pub fn run_message_pump() {
    unsafe {
        let mut msg = MSG::default();
        use windows::Win32::UI::WindowsAndMessaging::PM_REMOVE;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage,
        };

        while !QUIT_FLAG.load(Ordering::SeqCst) {
            // Use PM_REMOVE to remove messages from queue
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_DESTROY {
                    return;
                }
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            // Small sleep to prevent busy-waiting
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}
