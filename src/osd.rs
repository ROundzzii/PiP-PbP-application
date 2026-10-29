//! Petite notification affichée par-dessus la fenêtre PiP (« On-Screen Display ») :
//! fenêtre Windows native, sombre, qui disparaît seule. Elle n'intercepte ni les
//! clics ni le focus.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Once;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

/// Durée d'affichage.
const DURATION_MS: u32 = 1500;
/// Notification affichée en ce moment (remplacée par la suivante).
static CURRENT: AtomicIsize = AtomicIsize::new(0);

/// Affiche `text` centré en haut de `anchor` (rectangle écran en pixels physiques).
/// `scale` = échelle de l'écran (1.0 à 100 %, 1.5 à 150 %…).
pub fn show(anchor: RECT, scale: f32, text: &str) {
    let previous = CURRENT.swap(0, Ordering::Relaxed);
    if previous != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(previous as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    // Thread dédié avec sa propre boucle de messages : indépendant de la fenêtre PiP.
    std::thread::spawn(move || unsafe { run(anchor, scale, &wide) });
}

fn font(scale: f32) -> HFONT {
    unsafe {
        CreateFontW(
            -(17.0 * scale).round() as i32,
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        )
    }
}

unsafe fn run(anchor: RECT, scale: f32, text: &[u16]) {
    unsafe {
        static REGISTER: Once = Once::new();
        let instance = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
        REGISTER.call_once(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(proc),
                hInstance: instance.unwrap_or_default(),
                lpszClassName: w!("PipPbpOsd"),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            RegisterClassW(&class);
        });

        // Taille du texte pour dimensionner la bulle.
        let mut size = SIZE::default();
        let dc = GetDC(None);
        let f = font(scale);
        let old = SelectObject(dc, f.into());
        let _ = GetTextExtentPoint32W(dc, &text[..text.len() - 1], &mut size);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        let _ = DeleteObject(f.into());

        let (pad_x, pad_y) = ((20.0 * scale) as i32, (11.0 * scale) as i32);
        let (w, h) = (size.cx + 2 * pad_x, size.cy + 2 * pad_y);
        let x = (anchor.left + anchor.right) / 2 - w / 2;
        let y = anchor.top + (24.0 * scale) as i32;

        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            w!("PipPbpOsd"),
            PCWSTR(text.as_ptr()),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            instance,
            None,
        ) else {
            return;
        };
        CURRENT.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as *const _,
            size_of_val(&corner) as u32,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(Some(hwnd), 1, DURATION_MS, None);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let background = CreateSolidBrush(COLORREF(0x0020_2020));
                FillRect(dc, &rect, background);
                let _ = DeleteObject(background.into());

                let mut text = [0u16; 256];
                let n = GetWindowTextW(hwnd, &mut text) as usize;
                let scale = windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) as f32 / 96.0;
                let f = font(scale.max(1.0));
                let old = SelectObject(dc, f.into());
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0x00FF_FFFF));
                DrawTextW(dc, &mut text[..n], &mut rect, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                SelectObject(dc, old);
                let _ = DeleteObject(f.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_TIMER => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = CURRENT.compare_exchange(hwnd.0 as isize, 0, Ordering::Relaxed, Ordering::Relaxed);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// --- Compteur permanent (latence) -------------------------------------------------------
// Petite fenêtre sombre collée au coin haut-gauche de la fenêtre PiP, qui la suit et se met
// à jour deux fois par seconde. Elle laisse passer les clics et ne prend pas le focus.

static INFO_TEXT: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
static INFO_HWND: AtomicIsize = AtomicIsize::new(0);
static INFO_ANCHOR: AtomicIsize = AtomicIsize::new(0);

pub fn set_info_text(text: &str) {
    *INFO_TEXT.lock().unwrap() = text.to_string();
}

/// Affiche le compteur sur la fenêtre `anchor` (HWND de la fenêtre PiP).
pub fn show_info(anchor: isize) {
    INFO_ANCHOR.store(anchor, Ordering::Relaxed);
    if INFO_HWND.load(Ordering::Relaxed) != 0 {
        return;
    }
    std::thread::spawn(|| unsafe { run_info() });
}

pub fn hide_info() {
    let hwnd = INFO_HWND.swap(0, Ordering::Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe fn run_info() {
    unsafe {
        static REGISTER: Once = Once::new();
        let instance = GetModuleHandleW(None).ok().map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
        REGISTER.call_once(|| {
            let class = WNDCLASSW {
                lpfnWndProc: Some(info_proc),
                hInstance: instance.unwrap_or_default(),
                lpszClassName: w!("PipPbpInfo"),
                ..Default::default()
            };
            RegisterClassW(&class);
        });
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            w!("PipPbpInfo"),
            w!(""),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            instance,
            None,
        ) else {
            return;
        };
        INFO_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 215, LWA_ALPHA);
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &corner as *const _ as *const _, size_of_val(&corner) as u32);
        place_info(hwnd);
        SetTimer(Some(hwnd), 1, 500, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            DispatchMessageW(&msg);
        }
    }
}

/// Taille selon le texte, position au coin haut-gauche de la fenêtre PiP (cachée si elle
/// est réduite ou invisible).
unsafe fn place_info(hwnd: HWND) {
    unsafe {
        let anchor = HWND(INFO_ANCHOR.load(Ordering::Relaxed) as *mut _);
        let mut r = RECT::default();
        if anchor.0.is_null() || !IsWindowVisible(anchor).as_bool() || IsIconic(anchor).as_bool() || GetWindowRect(anchor, &mut r).is_err() {
            let _ = ShowWindow(hwnd, SW_HIDE);
            return;
        }
        let scale = (windows::Win32::UI::HiDpi::GetDpiForWindow(anchor) as f32 / 96.0).max(1.0);
        let mut text: Vec<u16> = INFO_TEXT.lock().unwrap().encode_utf16().collect();
        let dc = GetDC(None);
        let f = info_font(scale);
        let old = SelectObject(dc, f.into());
        let mut calc = RECT::default();
        DrawTextW(dc, &mut text, &mut calc, DT_CALCRECT | DT_LEFT);
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        let _ = DeleteObject(f.into());
        let pad = (8.0 * scale) as i32;
        let margin = (10.0 * scale) as i32;
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            r.left + margin,
            r.top + margin,
            calc.right + 2 * pad,
            calc.bottom + 2 * pad,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

fn info_font(scale: f32) -> HFONT {
    unsafe {
        CreateFontW(
            -(13.0 * scale).round() as i32,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Consolas"),
        )
    }
}

unsafe extern "system" fn info_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                place_info(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let mut rect = RECT::default();
                let _ = GetClientRect(hwnd, &mut rect);
                let background = CreateSolidBrush(COLORREF(0x0018_1818));
                FillRect(dc, &rect, background);
                let _ = DeleteObject(background.into());
                let anchor = HWND(INFO_ANCHOR.load(Ordering::Relaxed) as *mut _);
                let scale = (windows::Win32::UI::HiDpi::GetDpiForWindow(anchor) as f32 / 96.0).max(1.0);
                let pad = (8.0 * scale) as i32;
                rect.left += pad;
                rect.top += pad;
                let f = info_font(scale);
                let old = SelectObject(dc, f.into());
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, COLORREF(0x0080_FF80));
                let mut text: Vec<u16> = INFO_TEXT.lock().unwrap().encode_utf16().collect();
                DrawTextW(dc, &mut text, &mut rect, DT_LEFT);
                SelectObject(dc, old);
                let _ = DeleteObject(f.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = INFO_HWND.compare_exchange(hwnd.0 as isize, 0, Ordering::Relaxed, Ordering::Relaxed);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
